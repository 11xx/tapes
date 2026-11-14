//! Item pages over one recording, in physical record order.
//!
//! Positions are gaps: `forwards(page.start)` includes the page's first item,
//! `before(page.start)` excludes it. Keep `page.end` to follow appends. Positions
//! contain no transcript text and survive process restarts and append-only growth.
//! File positions validate the inode and 64 bytes before their offset; internal
//! record positions also validate that record. Edits elsewhere are not pinned.
//! Use a captured recording when the whole history must remain immutable.
//!
//! Claude records normalize independently. Codex runtime mirrors remain separate
//! occurrences, labelled `runtime`; Pi exposes the physical tree log, including
//! abandoned branches, with `parent_id`. Neither view claims the prefix-dependent
//! canonical projection of `show`. Supplied JSON/ZIP exports are unsupported.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use agent_tapes_discovery::{Discovery, NativeSession, NativeStore};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use sha2::{Digest, Sha256};

use crate::model::{Session, Turn, TurnKind};

/// Native stores configured in the environment, for callers selecting a recording.
pub fn native_stores() -> Vec<NativeStore> {
    Discovery::from_env().stores().to_vec()
}

pub const PAGE_SCHEMA: &str = "tapes-recording-page/1";
/// Maximum source bytes retained by one file page. Verification rereads these
/// bytes once; position anchors add at most 128 bytes per returned boundary.
pub const MAX_SCAN_BYTES: usize = 64 * 1024 * 1024;
const BLOCK: usize = 8192;
const MAX_POSITION_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    User,
    Assistant,
    Reasoning,
    ToolCall,
    ToolResult,
    Control,
    Ambient,
    Notice,
    Unknown,
}

impl std::str::FromStr for Kind {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        serde_json::from_value(Value::String(value.to_owned())).context("invalid recording kind")
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub items: usize,
    /// Maximum sum of serialized Item sizes. Envelope/positions outside items
    /// are additional. An individually oversized item refuses, without clipping.
    pub bytes: usize,
    /// Maximum source bytes inspected, independent of the returned item budget.
    /// A filter can reach this before filling a page; resume from the boundary.
    pub scan_bytes: usize,
    /// Empty admits every kind. Filtering precedes normalization and skips
    /// deserializing unwanted native payloads where the format separates them.
    pub kinds: Vec<Kind>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            items: 40,
            bytes: 1024 * 1024,
            scan_bytes: 8 * 1024 * 1024,
            kinds: Vec::new(),
        }
    }
}

impl Limits {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.items > 0 && self.bytes > 0,
            "item and byte budgets must be positive"
        );
        ensure!(
            (1..=MAX_SCAN_BYTES).contains(&self.scan_bytes),
            "scan bytes must be between 1 and {MAX_SCAN_BYTES}"
        );
        Ok(())
    }
    pub(crate) fn wants(&self, kind: Kind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }
}

/// An opaque, versioned gap coordinate. Store or transmit it verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Position(String);
impl Position {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(
            value.len() <= MAX_POSITION_BYTES,
            "oversized recording position"
        );
        let position = Self(value.to_owned());
        let _: Cursor = position.decode()?;
        Ok(position)
    }
    pub(crate) fn encode(cursor: &Cursor) -> Result<Self> {
        Ok(Self(URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor)?)))
    }
    pub(crate) fn decode(&self) -> Result<Cursor> {
        ensure!(
            self.0.len() <= MAX_POSITION_BYTES,
            "oversized recording position"
        );
        let cursor: Cursor = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(&self.0)
                .context("invalid recording position")?,
        )?;
        ensure!(
            cursor.version == 1,
            "unsupported recording position version"
        );
        Ok(cursor)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Direction<'a> {
    Tail,
    Before(&'a Position),
    Forwards(&'a Position),
}

/// Complete tool fields, independent of legacy event preview limits.
#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    pub name: Option<String>,
    pub call_id: Option<String>,
    pub status: Option<String>,
    pub arguments: Option<Value>,
    pub output: Option<Value>,
}

pub(crate) fn complete_tool(
    turn: &Turn,
    content: &Value,
    harness: &str,
    kind: Kind,
) -> Option<Tool> {
    let event = turn.tool.as_ref()?;
    let field = |object: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).filter(|v| !v.is_null()).cloned())
    };
    let (arguments, output) = match harness {
        "claude" => (field(content, &["input"]), field(content, &["content"])),
        "pi" => (field(content, &["arguments"]), field(content, &["content"])),
        "opencode" => (
            field(&content["state"], &["input"]),
            field(
                &content["state"],
                if event.status.as_deref() == Some("error") {
                    &["error", "output", "content"]
                } else {
                    &["content", "output", "error"]
                },
            ),
        ),
        "codex" if content.get("item").is_some() => (
            Some(content["item"].clone()),
            (kind == Kind::ToolResult).then(|| content["item"].clone()),
        ),
        "codex" => {
            let args = field(content, &["arguments", "input"]);
            let args = if content["type"] == "function_call" {
                args.map(|arg| {
                    arg.as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(arg)
                })
            } else {
                args
            };
            (args, field(content, &["output"]))
        }
        _ => (None, None),
    };
    Some(Tool {
        name: event.name.clone(),
        call_id: event.call_id.clone(),
        status: event.status.clone(),
        arguments,
        output,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub position: Position,
    pub end: Position,
    pub kind: Kind,
    pub turn: Turn,
    /// Complete native content of this part, including tool arguments/results.
    /// Normalized descriptor and tool-preview bounds do not clip this field.
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub representation: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stop {
    End,
    Items,
    Bytes,
    ScanBytes,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub schema: &'static str,
    pub recording: String,
    pub harness: String,
    /// Recorded parent/fork identity or native recording path, when available.
    pub parents: Vec<Parent>,
    pub items: Vec<Item>,
    pub start: Position,
    pub end: Position,
    pub stop: Stop,
    pub item_bytes: usize,
    /// Actual file bytes read, including verification and cursor anchors.
    /// OpenCode reports transport bytes, not SQLite's internal page reads.
    pub bytes_read: usize,
    pub records_read: usize,
    pub malformed_records: usize,
    /// Bytes after the newest newline are deferred until a later read.
    pub pending_bytes: u64,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Parent {
    pub relation: String,
    pub reference: String,
}

/// A selected native recording. This value holds no reader, cache or global state.
#[derive(Clone, Debug)]
pub struct Recording {
    pub(crate) id: String,
    pub(crate) harness: String,
    pub(crate) path: Option<PathBuf>,
    pub(crate) store: Option<NativeStore>,
    pub(crate) child: Option<String>,
}

impl Recording {
    /// Resolve using discovery's bounded identity opening, without reading a
    /// normalized transcript. Retain this descriptor for subsequent pages.
    pub fn resolve(stores: &[NativeStore], query: &str) -> Result<Self> {
        let native = Discovery::new(stores.iter().cloned()).resolve(query)?;
        Self::from_native(&native, stores)
    }
    pub fn from_native(native: &NativeSession, stores: &[NativeStore]) -> Result<Self> {
        let store = stores
            .iter()
            .find(|s| s.coordinate() == native.store_coordinate())
            .cloned();
        ensure!(
            native.locator().is_some() || store.is_some(),
            "native recording has no selected store"
        );
        Ok(Self {
            id: native.id().to_owned(),
            harness: native.harness().as_str().to_owned(),
            path: native.locator().map(Path::to_path_buf),
            store,
            child: None,
        })
    }
    /// Use an already resolved file-backed normalized session without locating it again.
    pub fn from_session(session: &Session) -> Result<Self> {
        ensure!(
            matches!(session.harness(), "claude" | "codex" | "pi"),
            "use native discovery for database recordings; supplied exports cannot be paged"
        );
        let path =
            crate::backend::session_file_path(session).context("session has no recording file")?;
        Ok(Self {
            id: session.id.clone(),
            harness: session.harness().to_owned(),
            path: Some(path.to_owned()),
            store: None,
            child: None,
        })
    }
    /// Select a Claude child recording under its parent; no parent transcript is read.
    pub fn child(&self, reference: &str) -> Result<Self> {
        ensure!(
            self.harness == "claude" && self.child.is_none(),
            "child-qualified pages require a Claude parent"
        );
        ensure!(
            !reference.is_empty()
                && reference
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid child reference"
        );
        let path = self
            .path
            .as_ref()
            .context("parent has no recording file")?
            .with_extension("")
            .join("subagents")
            .join(format!("agent-{reference}.jsonl"));
        Ok(Self {
            path: Some(path),
            child: Some(reference.to_owned()),
            ..self.clone()
        })
    }
    pub fn tail(&self, limits: &Limits) -> Result<Page> {
        self.page(Direction::Tail, limits)
    }
    pub fn before(&self, position: &Position, limits: &Limits) -> Result<Page> {
        self.page(Direction::Before(position), limits)
    }
    pub fn forwards(&self, position: &Position, limits: &Limits) -> Result<Page> {
        self.page(Direction::Forwards(position), limits)
    }
    pub fn page(&self, direction: Direction<'_>, limits: &Limits) -> Result<Page> {
        limits.validate()?;
        if self.harness == "opencode" {
            return crate::backend::opencode::OpenCodeBackend::from_store(
                self.store.clone().context("missing OpenCode store")?,
            )
            .recording_page(&self.id, direction, limits);
        }
        file_page(self, direction, limits)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Cursor {
    pub version: u8,
    pub source: String,
    pub device: u64,
    pub inode: u64,
    pub offset: u64,
    pub part: usize,
    pub anchor: String,
    pub record: Option<String>,
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct Window {
    file: File,
    metadata: Metadata,
    source: String,
    lo: u64,
    data: VecDeque<u8>,
    budget: usize,
    bytes_read: usize,
}

#[derive(Debug)]
struct ScanLimit;
impl std::fmt::Display for ScanLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "record or unfinished suffix exceeds scan budget; increase scan bytes"
        )
    }
}
impl std::error::Error for ScanLimit {}

impl Window {
    fn new(path: &Path, recording: &Recording, budget: usize) -> Result<Self> {
        let file = crate::backend::open_regular_file(path)?;
        let metadata = file.metadata()?;
        let source = digest(
            format!(
                "{}\0{}\0{}\0{}",
                recording.harness,
                recording.id,
                path.display(),
                recording.child.as_deref().unwrap_or("")
            )
            .as_bytes(),
        );
        Ok(Self {
            file,
            metadata,
            source,
            lo: 0,
            data: VecDeque::new(),
            budget,
            bytes_read: 0,
        })
    }
    fn hi(&self) -> u64 {
        self.lo + self.data.len() as u64
    }
    fn byte(&mut self, offset: u64) -> Result<u8> {
        ensure!(offset < self.metadata.len(), "position outside recording");
        if self.data.is_empty() {
            self.lo = offset;
        }
        if offset < self.lo || offset >= self.hi() {
            let remaining = self.budget.saturating_sub(self.data.len());
            if remaining == 0 {
                return Err(ScanLimit.into());
            }
            let (lo, hi) = if offset < self.lo {
                (self.lo.saturating_sub(BLOCK.min(remaining) as u64), self.lo)
            } else {
                (
                    self.hi(),
                    (self.hi() + BLOCK.min(remaining) as u64).min(self.metadata.len()),
                )
            };
            let mut data = vec![0; (hi - lo) as usize];
            self.file.seek(SeekFrom::Start(lo))?;
            self.file.read_exact(&mut data)?;
            self.bytes_read += data.len();
            if lo < self.lo {
                for byte in data.into_iter().rev() {
                    self.data.push_front(byte);
                }
                self.lo = lo;
            } else {
                self.data.extend(data);
            }
        }
        Ok(self.data[(offset - self.lo) as usize])
    }
    fn slice(&self, lo: u64, hi: u64) -> Vec<u8> {
        self.data
            .range((lo - self.lo) as usize..(hi - self.lo) as usize)
            .copied()
            .collect()
    }
    fn line_before(&mut self, end: u64) -> Result<(u64, Vec<u8>)> {
        ensure!(
            end > 0 && self.byte(end - 1)? == b'\n',
            "position is not a record boundary"
        );
        let mut start = end - 1;
        while start > 0 && self.byte(start - 1)? != b'\n' {
            start -= 1;
        }
        Ok((start, self.slice(start, end)))
    }
    fn line_at(&mut self, start: u64) -> Result<Option<(u64, Vec<u8>)>> {
        let mut end = start;
        while end < self.metadata.len() {
            if self.byte(end)? == b'\n' {
                return Ok(Some((end + 1, self.slice(start, end + 1))));
            }
            end += 1;
        }
        Ok(None)
    }
    fn anchor(&mut self, offset: u64) -> Result<String> {
        let lo = offset.saturating_sub(64);
        let mut bytes = vec![0; (offset - lo) as usize];
        self.file.seek(SeekFrom::Start(lo))?;
        self.file.read_exact(&mut bytes)?;
        self.bytes_read += bytes.len();
        Ok(digest(&bytes))
    }
    fn position(&mut self, offset: u64, part: usize, record: Option<&[u8]>) -> Result<Position> {
        Position::encode(&Cursor {
            version: 1,
            source: self.source.clone(),
            device: self.metadata.dev(),
            inode: self.metadata.ino(),
            offset,
            part,
            anchor: self.anchor(offset)?,
            record: (part > 0).then(|| digest(record.unwrap_or_default())),
        })
    }
    fn resume(&mut self, position: &Position) -> Result<Cursor> {
        let cursor = position.decode()?;
        ensure!(
            cursor.source == self.source
                && cursor.device == self.metadata.dev()
                && cursor.inode == self.metadata.ino()
                && cursor.offset <= self.metadata.len(),
            "recording replaced, truncated, or position belongs to another source"
        );
        ensure!(
            cursor.anchor == self.anchor(cursor.offset)?,
            "recording changed at position"
        );
        ensure!(
            cursor.offset == 0 || self.byte(cursor.offset - 1)? == b'\n',
            "position is not a record boundary"
        );
        if cursor.part > 0 {
            let (_, line) = self
                .line_at(cursor.offset)?
                .context("recording truncated at internal position")?;
            ensure!(
                cursor.record.as_deref() == Some(&digest(&line)),
                "recording changed inside positioned record"
            );
        }
        Ok(cursor)
    }
    fn verify(&mut self, path: &Path) -> Result<()> {
        let after = self.file.metadata()?;
        let named = std::fs::metadata(path)?;
        ensure!(
            named.dev() == self.metadata.dev()
                && named.ino() == self.metadata.ino()
                && after.len() >= self.metadata.len(),
            "recording replaced or truncated during read"
        );
        if after.len() == self.metadata.len() {
            ensure!(
                after.mtime() == self.metadata.mtime()
                    && after.mtime_nsec() == self.metadata.mtime_nsec()
                    && after.ctime() == self.metadata.ctime()
                    && after.ctime_nsec() == self.metadata.ctime_nsec(),
                "recording rewritten during read"
            );
        }
        self.file.seek(SeekFrom::Start(self.lo))?;
        let mut buffer = [0; BLOCK];
        let (front, back) = self.data.as_slices();
        for expected in front.chunks(BLOCK).chain(back.chunks(BLOCK)) {
            self.file.read_exact(&mut buffer[..expected.len()])?;
            self.bytes_read += expected.len();
            ensure!(
                expected == &buffer[..expected.len()],
                "recording rewritten during read"
            );
        }
        Ok(())
    }
}

fn file_page(recording: &Recording, direction: Direction<'_>, limits: &Limits) -> Result<Page> {
    let path = recording.path.as_ref().context("recording has no file")?;
    let mut window = Window::new(path, recording, limits.scan_bytes)?;
    let mut parents = Vec::new();
    // Read only the identity header, not the transcript opening window.
    let header = read_header(
        &mut window.file,
        window.metadata.len(),
        &mut window.bytes_read,
    )?;
    let mut origins = Vec::new();
    if let Some(raw) = header.as_deref() {
        let outer = object(raw)?;
        let mut fields = serde_json::Map::new();
        for key in [
            "type",
            "sessionId",
            "agentId",
            "id",
            "parentSession",
            "payload",
        ] {
            if let Some(value) = outer.get(key) {
                fields.insert(key.to_owned(), serde_json::from_str(value.get())?);
            }
        }
        let value = Value::Object(fields);
        crate::backend::validate_native_identity(&value, &recording.harness, &recording.id)?;
        if let Some(origin) = value
            .pointer("/payload/forked_from_id")
            .and_then(Value::as_str)
        {
            origins.push(origin.to_owned());
        }
        if let Some(child) = &recording.child {
            ensure!(
                value["agentId"].as_str().is_none_or(|agent| agent == child),
                "recording names another child"
            );
        }
        parent_links(&mut parents, &value, &recording.harness);
    }
    if recording.child.is_some() {
        parents.push(Parent {
            relation: "child-of".to_owned(),
            reference: recording.id.clone(),
        });
    }
    let backwards = !matches!(direction, Direction::Forwards(_));
    let mut pending_bytes = 0;
    let (mut offset, mut part) = match direction {
        Direction::Tail => {
            let mut end = window.metadata.len();
            while end > 0 && window.byte(end - 1)? != b'\n' {
                end -= 1;
            }
            pending_bytes = window.metadata.len() - end;
            (end, 0)
        }
        Direction::Before(p) | Direction::Forwards(p) => {
            let cursor = window.resume(p)?;
            (cursor.offset, cursor.part)
        }
    };
    let mut start = match direction {
        Direction::Before(p) | Direction::Forwards(p) => p.clone(),
        Direction::Tail => window.position(offset, part, None)?,
    };
    let mut end = start.clone();
    let mut items = Vec::new();
    let mut item_bytes: usize = 0;
    let mut records_read = 0;
    let mut malformed_records = 0;
    let mut stop = Stop::End;
    loop {
        if items.len() == limits.items {
            stop = Stop::Items;
            break;
        }
        if backwards && offset == 0 && part == 0 {
            break;
        }
        let internal = part > 0;
        let read = if backwards && !internal {
            window
                .line_before(offset)
                .map(|(lo, line)| Some((lo, offset, line)))
        } else {
            window
                .line_at(offset)
                .map(|line| line.map(|(hi, line)| (offset, hi, line)))
        };
        let (lo, hi, line) = match read {
            Ok(Some(record)) => record,
            Ok(None) => {
                pending_bytes = window.metadata.len() - offset;
                break;
            }
            Err(error) if error.downcast_ref::<ScanLimit>().is_some() && records_read > 0 => {
                stop = Stop::ScanBytes;
                break;
            }
            Err(error) => return Err(error),
        };
        records_read += 1;
        let candidates = match fragments(
            &line,
            &recording.harness,
            recording.child.as_deref(),
            &recording.id,
            &origins,
        ) {
            Ok(candidates) => candidates,
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                malformed_records += 1;
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        let count = candidates.len();
        ensure!(!internal || part <= count, "position part outside record");
        let indices: Vec<usize> = if backwards {
            (0..if internal { part } else { count }).rev().collect()
        } else {
            (part..count).collect()
        };
        let mut full = false;
        for index in indices {
            let fragment = &candidates[index];
            if !fragment.might_match(limits) {
                continue;
            }
            let before = window.position(lo, index, Some(&line))?;
            let after = if index + 1 == count {
                window.position(hi, 0, None)?
            } else {
                window.position(lo, index + 1, Some(&line))?
            };
            let Some(item) = fragment.materialize(before, after, &recording.harness)? else {
                continue;
            };
            if !limits.wants(item.kind) {
                continue;
            }
            let size = serde_json::to_vec(&item)?.len();
            if size > limits.bytes && items.is_empty() {
                bail!("item requires {size} bytes; increase item byte budget ({}); content was not clipped", limits.bytes);
            }
            if item_bytes.saturating_add(size) > limits.bytes {
                stop = Stop::Bytes;
                full = true;
                break;
            }
            item_bytes += size;
            if backwards {
                start = item.position.clone();
            } else {
                end = item.end.clone();
            }
            items.push(item);
            if items.len() == limits.items {
                stop = Stop::Items;
                full = true;
                break;
            }
        }
        if full {
            break;
        }
        offset = if backwards { lo } else { hi };
        part = 0;
        let boundary = window.position(offset, 0, None)?;
        if backwards {
            start = boundary;
        } else {
            end = boundary;
        }
    }
    if backwards {
        items.reverse();
    }
    window.verify(path)?;
    let notes = match recording.harness.as_str() {
        "codex" => vec!["Physical record occurrences; runtime tool mirrors remain separately labelled runtime.".to_owned()],
        "pi" => vec!["Physical tree log, including abandoned branches; parent_id links entries. Active-branch selection requires a whole-recording ancestry pass.".to_owned()],
        _ => Vec::new(),
    };
    Ok(Page {
        schema: PAGE_SCHEMA,
        recording: recording.child.as_ref().map_or_else(
            || recording.id.clone(),
            |child| format!("{}:{child}", recording.id),
        ),
        harness: recording.harness.clone(),
        parents,
        items,
        start,
        end,
        stop,
        item_bytes,
        bytes_read: window.bytes_read,
        records_read,
        malformed_records,
        pending_bytes,
        notes,
    })
}

fn read_header(
    file: &mut File,
    length: u64,
    bytes_read: &mut usize,
) -> Result<Option<Box<RawValue>>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    let mut buffer = [0; BLOCK];
    while bytes.len() < 1024 * 1024 && (bytes.len() as u64) < length {
        let take = buffer.len().min((length - bytes.len() as u64) as usize);
        file.read_exact(&mut buffer[..take])?;
        *bytes_read += take;
        if let Some(newline) = memchr::memchr(b'\n', &buffer[..take]) {
            bytes.extend_from_slice(&buffer[..newline]);
            return serde_json::from_slice(&bytes)
                .map(Some)
                .context("invalid recording identity header");
        }
        bytes.extend_from_slice(&buffer[..take]);
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    ensure!(
        bytes.len() < 1024 * 1024,
        "recording identity header exceeds 1MiB"
    );
    // An uncommitted first line cannot establish identity or yield an item.
    Ok(None)
}

fn parent_links(parents: &mut Vec<Parent>, value: &Value, harness: &str) {
    let links = match harness {
        "codex" if value["type"] == "session_meta" => vec![
            ("forked-from", value.pointer("/payload/forked_from_id")),
            (
                "child-of",
                value.pointer("/payload/source/subagent/thread_spawn/parent_thread_id"),
            ),
            ("child-of", value.pointer("/payload/parent_thread_id")),
        ],
        "pi" if value["type"] == "session" => vec![("forked-from", value.get("parentSession"))],
        _ => Vec::new(),
    };
    for (relation, reference) in links {
        if let Some(reference) = reference.and_then(Value::as_str) {
            if !parents
                .iter()
                .any(|p| p.relation == relation && p.reference == reference)
            {
                parents.push(Parent {
                    relation: relation.to_owned(),
                    reference: reference.to_owned(),
                });
            }
        }
    }
}

// Borrowed raw values let a prose-only page inspect tags without allocating
// skipped tool strings, payload trees or normalized turns.
type Object<'a> = BTreeMap<&'a str, &'a RawValue>;
fn object(raw: &RawValue) -> Result<Object<'_>> {
    Ok(serde_json::from_str(raw.get())?)
}
fn string<'a>(obj: &Object<'a>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| serde_json::from_str(v.get()).ok())
}
pub(crate) fn turn_kind(kind: TurnKind) -> Kind {
    match kind {
        TurnKind::Operator => Kind::User,
        TurnKind::Assistant => Kind::Assistant,
        TurnKind::Reasoning => Kind::Reasoning,
        TurnKind::Control => Kind::Control,
        TurnKind::Ambient => Kind::Ambient,
        TurnKind::Notice => Kind::Notice,
        TurnKind::Unknown => Kind::Unknown,
        TurnKind::Tool => Kind::ToolCall,
    }
}

struct Fragment<'a> {
    kind: Kind,
    envelope: &'a RawValue,
    content: &'a RawValue,
    block: bool,
    parent_id: Option<String>,
    runtime: bool,
    message_kind: Option<TurnKind>,
}
impl Fragment<'_> {
    fn might_match(&self, limits: &Limits) -> bool {
        limits.wants(self.kind)
            || self.kind == Kind::User
                && [Kind::Control, Kind::Ambient, Kind::Notice, Kind::Unknown]
                    .into_iter()
                    .any(|kind| limits.wants(kind))
    }
    fn materialize(
        &self,
        position: Position,
        end: Position,
        harness: &str,
    ) -> Result<Option<Item>> {
        let content: Value = serde_json::from_str(self.content.get())?;
        let envelope = if self.block {
            let outer = object(self.envelope)?;
            let mut fields = serde_json::Map::new();
            for (key, raw) in &outer {
                if *key != "message" && *key != "content" && *key != "toolUseResult" {
                    fields.insert((*key).to_owned(), serde_json::from_str(raw.get())?);
                }
            }
            if let Some(message) = outer.get("message") {
                let mut message_fields = serde_json::Map::new();
                for (key, raw) in object(message)? {
                    if key != "content" {
                        message_fields.insert(key.to_owned(), serde_json::from_str(raw.get())?);
                    }
                }
                message_fields.insert("content".to_owned(), serde_json::json!([content]));
                fields.insert("message".to_owned(), Value::Object(message_fields));
            } else {
                fields.insert("content".to_owned(), serde_json::json!([content]));
            }
            Value::Object(fields)
        } else {
            serde_json::from_str(self.envelope.get())?
        };
        let turns = match harness {
            "claude" => crate::backend::claude::parse_turns(&envelope),
            "codex" => crate::backend::codex::parse_turns(&envelope, true),
            "pi" => crate::backend::pi::parse_turns(&envelope),
            _ => bail!("unsupported recording harness"),
        };
        let Some(mut turn) = turns.into_iter().next() else {
            return Ok(None);
        };
        // Complete native content is authoritative even when derived descriptors
        // in Turn apply their own preview bounds.
        let call_id = turn.tool.as_ref().and_then(|tool| tool.call_id.clone());
        turn.record_ref = None;
        if turn.role == crate::model::Role::User {
            if let Some(kind) = self.message_kind {
                turn.kind = kind;
            }
        }
        let kind = if turn.kind == TurnKind::Tool {
            self.kind
        } else {
            turn_kind(turn.kind)
        };
        let tool = complete_tool(&turn, &content, harness, kind);
        Ok(Some(Item {
            position,
            end,
            kind,
            turn,
            content,
            call_id,
            tool,
            parent_id: self.parent_id.clone(),
            representation: self.runtime.then(|| "runtime".to_owned()),
        }))
    }
}

fn fragments<'a>(
    line: &'a [u8],
    harness: &str,
    child: Option<&str>,
    id: &str,
    origins: &[String],
) -> Result<Vec<Fragment<'a>>> {
    let raw: &RawValue = serde_json::from_slice(line)?;
    let outer = object(raw)?;
    let typ = string(&outer, "type").unwrap_or_default();
    // Identity checks deserialize only identity-bearing fields, never content.
    let mut identity = serde_json::Map::new();
    for key in ["type", "sessionId", "agentId", "id"] {
        if let Some(value) = outer.get(key) {
            identity.insert(key.to_owned(), serde_json::from_str(value.get())?);
        }
    }
    if typ == "session_meta" {
        identity.insert(
            "payload".to_owned(),
            serde_json::from_str(
                outer
                    .get("payload")
                    .context("missing session header")?
                    .get(),
            )?,
        );
    }
    let check = Value::Object(identity);
    let inherited = typ == "session_meta"
        && check["payload"]["id"]
            .as_str()
            .is_some_and(|native| origins.iter().any(|origin| origin == native));
    if !inherited {
        crate::backend::validate_native_identity(&check, harness, id)?;
    }
    if let Some(child) = child {
        ensure!(
            check["agentId"].as_str().is_none_or(|agent| agent == child),
            "recording names another child"
        );
    }
    let parent_id = string(
        &outer,
        if harness == "pi" {
            "parentId"
        } else {
            "parentUuid"
        },
    );
    let make = |kind, content, block, runtime| Fragment {
        kind,
        envelope: raw,
        content,
        block,
        parent_id: parent_id.clone(),
        runtime,
        message_kind: None,
    };
    if harness == "codex" {
        let Some(payload) = outer.get("payload") else {
            return Ok(Vec::new());
        };
        let payload_obj = object(payload)?;
        let subtype = string(&payload_obj, "type").unwrap_or_default();
        if typ == "event_msg" && matches!(subtype.as_str(), "item_started" | "item_completed") {
            let Some(item) = payload_obj.get("item") else {
                return Ok(Vec::new());
            };
            let item_obj = object(item)?;
            let Some(kind) = string(&item_obj, "type") else {
                return Ok(Vec::new());
            };
            if !matches!(
                kind.as_str(),
                "CommandExecution"
                    | "FileChange"
                    | "McpToolCall"
                    | "ImageView"
                    | "Extension"
                    | "WebSearch"
                    | "CollabAgentToolCall"
                    | "SubAgentActivity"
            ) {
                return Ok(Vec::new());
            }
            return Ok(vec![make(
                if subtype == "item_started" {
                    Kind::ToolCall
                } else {
                    Kind::ToolResult
                },
                payload,
                false,
                true,
            )]);
        }
        if typ != "response_item" {
            return Ok(Vec::new());
        }
        let kind = match subtype.as_str() {
            "message" => match string(&payload_obj, "role").as_deref() {
                Some("assistant") => Kind::Assistant,
                Some("user") => Kind::User,
                Some("system" | "developer") => Kind::Ambient,
                _ => return Ok(Vec::new()),
            },
            "reasoning" => Kind::Reasoning,
            "function_call" | "custom_tool_call" | "web_search_call" => Kind::ToolCall,
            "function_call_output" | "custom_tool_call_output" => Kind::ToolResult,
            _ => return Ok(Vec::new()),
        };
        return Ok(vec![make(kind, payload, false, false)]);
    }
    if harness == "claude"
        && ((typ == "system" && string(&outer, "subtype").as_deref() == Some("compact_boundary"))
            || typ == "attachment")
    {
        return Ok(vec![make(
            if typ == "attachment" {
                Kind::Ambient
            } else {
                Kind::Notice
            },
            raw,
            false,
            false,
        )]);
    }
    if harness == "claude" && typ == "api-request-blob" {
        return Ok(Vec::new());
    }
    let custom = harness == "pi" && typ == "custom_message";
    if harness == "pi" && typ != "message" && !custom {
        return Ok(Vec::new());
    }
    let message = if custom {
        outer.clone()
    } else {
        match outer.get("message") {
            Some(m) => object(m)?,
            None => return Ok(Vec::new()),
        }
    };
    let role = if custom {
        "user".to_owned()
    } else {
        string(&message, "role").unwrap_or_default()
    };
    let base_kind = match role.as_str() {
        "user" => Kind::User,
        "assistant" => Kind::Assistant,
        "system" | "developer" => Kind::Ambient,
        "toolResult" => Kind::ToolResult,
        _ => return Ok(Vec::new()),
    };
    let Some(content) = message.get("content") else {
        return Ok(Vec::new());
    };
    if role == "toolResult" {
        return Ok(vec![make(Kind::ToolResult, outer["message"], false, false)]);
    }
    if content.get().starts_with('"') {
        return Ok(vec![make(base_kind, content, false, false)]);
    }
    if !content.get().starts_with('[') {
        return Ok(Vec::new());
    }
    let blocks: Vec<&RawValue> = serde_json::from_str(content.get())?;
    let message_kind = if harness == "claude" && role == "user" {
        let mut fields = serde_json::Map::new();
        for key in [
            "origin",
            "promptSource",
            "isMeta",
            "interruptedMessageId",
            "isCompactSummary",
            "isSidechain",
            "agentId",
            "parentUuid",
        ] {
            if let Some(value) = outer.get(key) {
                fields.insert(key.to_owned(), serde_json::from_str(value.get())?);
            }
        }
        let text = match blocks.as_slice() {
            [block] => {
                let block = object(block)?;
                if string(&block, "type").as_deref() == Some("text") {
                    string(&block, "text")
                } else {
                    None
                }
            }
            _ => None,
        };
        Some(crate::backend::claude::claude_user_kind(
            &Value::Object(fields),
            text.as_deref(),
        ))
    } else {
        None
    };
    let mut result = Vec::new();
    for block in blocks {
        let block_obj = object(block)?;
        let kind = match string(&block_obj, "type").as_deref() {
            Some("thinking") => Kind::Reasoning,
            Some("tool_use" | "toolCall") => Kind::ToolCall,
            Some("tool_result") => Kind::ToolResult,
            _ => base_kind,
        };
        let mut fragment = make(kind, block, true, false);
        if kind == Kind::User {
            if let Some(kind) = message_kind {
                fragment.kind = turn_kind(kind);
            }
        }
        fragment.message_kind = message_kind;
        result.push(fragment);
    }
    Ok(result)
}
