//! Item pages over one recording, in physical record order.
//!
//! Positions are gaps: `forwards(page.start)` includes the page's first item,
//! `before(page.start)` excludes it. Keep `page.end` to follow appends. Positions
//! contain no transcript text and survive process restarts and append-only growth.
//! File positions validate the inode and preceding record tail digest; internal
//! record positions also validate their record. Edits elsewhere are not pinned.
//! Use a captured recording when the whole history must remain immutable.
//!
//! Claude records normalize independently. Codex runtime mirrors remain separate
//! occurrences, labelled `runtime`; Pi exposes the physical tree log, including
//! abandoned branches, with `parent_id`. Neither view claims the prefix-dependent
//! canonical projection of `show`. Supplied JSON/ZIP exports are unsupported.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_tapes_discovery::{Discovery, NativeSession, NativeStore};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use sha2::{Digest, Sha256};

use crate::model::{Session, TurnKind};

/// Native stores configured in the environment, for callers selecting a recording.
pub fn native_stores() -> Vec<NativeStore> {
    Discovery::from_env().stores().to_vec()
}

pub const PAGE_SCHEMA: &str = "tapes-recording-page/1";
/// Maximum source bytes retained by one file page. Verification rereads these
/// bytes once. Preceding-record anchors consume the same source scan budget.
pub const MAX_SCAN_BYTES: usize = 64 * 1024 * 1024;
const BLOCK: usize = 8192;
const ANCHOR_BYTES: u64 = 4096;
const MAX_POSITION_BYTES: usize = 4096;

/// Consumers retain a wildcard arm when matching item kinds.
/// ```compile_fail
/// use agent_tapes_core::recording::Kind;
/// fn closed(kind: Kind) { match kind {
/// Kind::User | Kind::Assistant | Kind::Reasoning | Kind::ToolCall |
/// Kind::ToolResult | Kind::Control | Kind::Ambient | Kind::Notice | Kind::Unknown => ()
/// } }
/// ```
#[non_exhaustive]
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
    #[serde(other)]
    Unknown,
}

impl std::str::FromStr for Kind {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        Ok(match value {
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "reasoning" => Self::Reasoning,
            "tool-call" => Self::ToolCall,
            "tool-result" => Self::ToolResult,
            "control" => Self::Control,
            "ambient" => Self::Ambient,
            "notice" => Self::Notice,
            "unknown" => Self::Unknown,
            _ => bail!("invalid recording kind: {value}"),
        })
    }
}

/// Extendable page options, constructed with Default and the with_* builders.
/// ```compile_fail
/// use agent_tapes_core::recording::Limits;
/// let limits = Limits { items: 1, bytes: 1024, scan_bytes: 8192, kinds: vec![] };
/// ```
#[non_exhaustive]
#[derive(Clone, Debug)]
pub struct Limits {
    pub items: usize,
    /// Native content once plus serialized metadata, measured by a counting
    /// writer. Normalized text/tool projections share that payload budget;
    /// duplicate wire projections and the page envelope are additional.
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
    pub fn with_items(mut self, items: usize) -> Self {
        self.items = items;
        self
    }
    pub fn with_bytes(mut self, bytes: usize) -> Self {
        self.bytes = bytes;
        self
    }
    pub fn with_scan_bytes(mut self, scan_bytes: usize) -> Self {
        self.scan_bytes = scan_bytes;
        self
    }
    pub fn with_kinds(mut self, kinds: Vec<Kind>) -> Self {
        self.kinds = kinds;
        self
    }
    pub fn validate(&self) -> Result<()> {
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
        let position = Self(value.to_owned());
        let _: Cursor = position.decode()?;
        Ok(position)
    }
    pub(crate) fn encode(cursor: &Cursor) -> Result<Self> {
        Ok(Self(URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor)?)))
    }
    pub(crate) fn decode(&self) -> Result<Cursor> {
        self.decode_checked().map_err(|error| {
            Refusal::InvalidPosition {
                reason: error.to_string(),
            }
            .into()
        })
    }
    fn decode_checked(&self) -> Result<Cursor> {
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

mod item;
pub use item::{Item, Payload, Status, Tool};

/// Structured reader refusals, also serialized by the CLI JSON error response.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Refusal {
    Oversized {
        needed: usize,
        start: Position,
        end: Position,
    },
    ScanBudget {
        limit: usize,
        /// Sufficient scan budget when the native record's size is known.
        #[serde(skip_serializing_if = "Option::is_none")]
        needed: Option<usize>,
    },
    Changed {
        reason: String,
    },
    Replaced {
        reason: String,
    },
    Unsupported {
        store: String,
    },
    InvalidPosition {
        reason: String,
    },
    InvalidInput {
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oversized { needed, .. } => write!(
                f,
                "item requires {needed} budget bytes; content was not clipped"
            ),
            Self::ScanBudget { limit, needed } => {
                write!(f, "scan budget exhausted at {limit} bytes")?;
                if let Some(needed) = needed {
                    write!(f, "; requires {needed} bytes")?;
                }
                Ok(())
            }
            Self::Changed { reason } => write!(f, "positioned content changed: {reason}"),
            Self::Replaced { reason } => write!(f, "recording replaced or truncated: {reason}"),
            Self::Unsupported { store } => write!(f, "recording positions unsupported for {store}"),
            Self::InvalidPosition { reason } => write!(f, "invalid recording position: {reason}"),
            Self::InvalidInput { reason } => write!(f, "invalid recording input: {reason}"),
            Self::Unavailable { reason } => write!(f, "recording unavailable: {reason}"),
        }
    }
}
impl std::error::Error for Refusal {}
fn unavailable(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<Refusal>().is_some() {
        return error;
    }
    let reason = error.to_string();
    error.context(Refusal::Unavailable { reason })
}
fn changed(reason: &str) -> anyhow::Error {
    Refusal::Changed {
        reason: reason.to_owned(),
    }
    .into()
}
fn replaced(reason: &str) -> anyhow::Error {
    Refusal::Replaced {
        reason: reason.to_owned(),
    }
    .into()
}

/// Consumers retain a wildcard arm when matching page stops.
/// ```compile_fail
/// use agent_tapes_core::recording::Stop;
/// fn closed(stop: Stop) { match stop {
/// Stop::End | Stop::Items | Stop::Bytes | Stop::ScanBytes | Stop::Pending | Stop::Unknown => ()
/// } }
/// ```
#[non_exhaustive]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stop {
    End,
    Items,
    Bytes,
    ScanBytes,
    Pending,
    #[serde(other)]
    Unknown,
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
    /// Why this page ended. Followers check pending/pending_bytes for whether
    /// input is unfinished, even when an item/byte/scan limit ended the page.
    pub stop: Stop,
    pub item_bytes: usize,
    /// Actual file bytes read, including verification and cursor anchors.
    /// OpenCode reports transport bytes, not SQLite's internal page reads.
    pub bytes_read: usize,
    pub records_read: usize,
    pub malformed_records: usize,
    /// Bytes after the newest newline. A nonzero value tells a follower to
    /// wait for a source change after consuming the available page progress.
    pub pending_bytes: u64,
    /// Native pending part ID. Some value tells a follower to wait for a
    /// source change after consuming progress; files use pending_bytes.
    pub pending: Option<String>,
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
        let native = Discovery::new(stores.iter().cloned())
            .resolve(query)
            .map_err(|error| match error {
                agent_tapes_discovery::ResolveError::InvalidQuery
                | agent_tapes_discovery::ResolveError::Ambiguous { .. } => {
                    anyhow::Error::new(error).context(Refusal::InvalidInput {
                        reason: "invalid or ambiguous recording query".to_owned(),
                    })
                }
                _ => unavailable(error.into()),
            })?;
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
        if !matches!(session.harness(), "claude" | "codex" | "pi") {
            return Err(Refusal::Unsupported {
                store: session.harness().to_owned(),
            }
            .into());
        }
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
        limits.validate().map_err(|error| {
            let reason = error.to_string();
            error.context(Refusal::InvalidInput { reason })
        })?;
        (|| {
            if self.harness == "opencode" {
                return crate::backend::opencode::OpenCodeBackend::from_store(
                    self.store.clone().context("missing OpenCode store")?,
                )
                .recording_page(&self.id, direction, limits);
            }
            file_page(self, direction, limits)
        })()
        .map_err(unavailable)
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
    anchors: HashMap<u64, String>,
}

impl Window {
    fn new(path: &Path, recording: &Recording, budget: usize) -> Result<Self> {
        let file = crate::backend::open_regular_file(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                replaced("recording file disappeared")
            } else {
                error.into()
            }
        })?;
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
            anchors: HashMap::new(),
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
                return Err(Refusal::ScanBudget {
                    limit: self.budget,
                    needed: None,
                }
                .into());
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
        if let Some(anchor) = self.anchors.get(&offset) {
            return Ok(anchor.clone());
        }
        let mut hash = Sha256::new();
        if offset > 0 {
            if self.byte(offset - 1)? != b'\n' {
                return Err(changed("position is not a record boundary"));
            }
            let mut start = offset - 1;
            let lower = offset.saturating_sub(ANCHOR_BYTES);
            while start > lower && self.byte(start - 1)? != b'\n' {
                start -= 1;
            }
            let from = (start - self.lo) as usize;
            let to = (offset - self.lo) as usize;
            let (front, back) = self.data.as_slices();
            if from < front.len() {
                hash.update(&front[from..to.min(front.len())]);
            }
            if to > front.len() {
                hash.update(&back[from.saturating_sub(front.len())..to - front.len()]);
            }
        }
        let anchor = format!("{:x}", hash.finalize());
        self.anchors.insert(offset, anchor.clone());
        Ok(anchor)
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
        if cursor.source != self.source
            || cursor.device != self.metadata.dev()
            || cursor.inode != self.metadata.ino()
            || cursor.offset > self.metadata.len()
        {
            return Err(replaced(
                "position belongs to another source or its file changed",
            ));
        }
        if cursor.anchor != self.anchor(cursor.offset)? {
            return Err(changed("preceding record tail digest differs"));
        }
        ensure!(
            cursor.offset == 0 || self.byte(cursor.offset - 1)? == b'\n',
            "position is not a record boundary"
        );
        if cursor.part > 0 {
            let (_, line) = self
                .line_at(cursor.offset)?
                .ok_or_else(|| changed("recording truncated at internal position"))?;
            if cursor.record.as_deref() != Some(&digest(&line)) {
                return Err(changed("record digest differs"));
            }
        }
        Ok(cursor)
    }
    fn verify(&mut self, path: &Path) -> Result<()> {
        let after = self.file.metadata()?;
        let named = std::fs::metadata(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                replaced("recording file disappeared during read")
            } else {
                error.into()
            }
        })?;
        if named.dev() != self.metadata.dev()
            || named.ino() != self.metadata.ino()
            || after.len() < self.metadata.len()
        {
            return Err(replaced("source changed during read"));
        }
        if after.len() == self.metadata.len()
            && (after.mtime() != self.metadata.mtime()
                || after.mtime_nsec() != self.metadata.mtime_nsec()
                || after.ctime() != self.metadata.ctime()
                || after.ctime_nsec() != self.metadata.ctime_nsec())
        {
            return Err(changed("source rewritten during read"));
        }
        self.file.seek(SeekFrom::Start(self.lo))?;
        let mut buffer = [0; BLOCK];
        let (front, back) = self.data.as_slices();
        for expected in front.chunks(BLOCK).chain(back.chunks(BLOCK)) {
            self.file.read_exact(&mut buffer[..expected.len()])?;
            self.bytes_read += expected.len();
            if expected != &buffer[..expected.len()] {
                return Err(changed("source rewritten during read"));
            }
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
            Err(error)
                if matches!(
                    error.downcast_ref::<Refusal>(),
                    Some(Refusal::ScanBudget { .. })
                ) && records_read > 0 =>
            {
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
            let positions = (|| -> Result<_> {
                let before = window.position(lo, index, Some(&line))?;
                let after = if index + 1 == count {
                    window.position(hi, 0, None)?
                } else {
                    window.position(lo, index + 1, Some(&line))?
                };
                Ok((before, after))
            })();
            let (before, after) = match positions {
                Ok(positions) => positions,
                Err(error)
                    if !items.is_empty()
                        && matches!(
                            error.downcast_ref::<Refusal>(),
                            Some(Refusal::ScanBudget { .. })
                        ) =>
                {
                    stop = Stop::ScanBytes;
                    full = true;
                    break;
                }
                Err(error) => return Err(error),
            };
            let Some(item) = fragment.materialize(before, after, &recording.harness)? else {
                continue;
            };
            if !limits.wants(item.kind) {
                continue;
            }
            let size = item.budget_bytes()?;
            if size > limits.bytes && items.is_empty() {
                return Err(Refusal::Oversized {
                    needed: size,
                    start: item.start,
                    end: item.end,
                }
                .into());
            }
            if item_bytes.saturating_add(size) > limits.bytes {
                stop = Stop::Bytes;
                full = true;
                break;
            }
            item_bytes += size;
            if backwards {
                start = item.start.clone();
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
        let boundary = match window.position(offset, 0, None) {
            Ok(position) => position,
            Err(error)
                if !items.is_empty()
                    && matches!(
                        error.downcast_ref::<Refusal>(),
                        Some(Refusal::ScanBudget { .. })
                    ) =>
            {
                stop = Stop::ScanBytes;
                break;
            }
            Err(error) => return Err(error),
        };
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
    if pending_bytes > 0 && matches!(stop, Stop::End) {
        stop = Stop::Pending;
    }
    Ok(Page {
        schema: PAGE_SCHEMA,
        recording: recording.child.as_ref().map_or_else(
            || recording.id.clone(),
            |child| format!("{}::{child}", recording.id),
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
        pending: None,
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
    fn materialize(&self, start: Position, end: Position, harness: &str) -> Result<Option<Item>> {
        let content: Arc<Value> = Arc::new(serde_json::from_str(self.content.get())?);
        let meta = metadata(self.envelope)?;
        let message_id = match harness {
            "claude" if meta["type"] == "user" => meta["uuid"].as_str(),
            "claude" => meta["message"]["id"].as_str(),
            "codex" if meta["type"] == "response_item" && meta["payload"]["type"] == "message" => {
                meta["payload"]["id"].as_str()
            }
            "pi" => meta["id"].as_str(),
            _ => None,
        }
        .map(str::to_owned);
        let mut item = if matches!(self.kind, Kind::ToolCall | Kind::ToolResult) {
            let (tool, call_id) = Tool::native(&content, harness, self.kind);
            let native_id = match harness {
                "claude" => meta["uuid"].as_str(),
                "codex" if self.runtime => content["item"]["id"].as_str(),
                "codex" => content["id"].as_str(),
                _ => meta["id"].as_str(),
            }
            .map(str::to_owned);
            Item {
                start,
                end,
                kind: self.kind,
                role: crate::model::Role::Tool,
                text: String::new(),
                ts: crate::backend::timestamp(&meta["timestamp"]),
                native_id,
                request_turn_id: meta["payload"]["turn_id"]
                    .as_str()
                    .or_else(|| {
                        meta.pointer("/payload/context/turn_id")
                            .and_then(Value::as_str)
                    })
                    .map(str::to_owned),
                model: recorded_model(&meta, harness),
                origin: None,
                call_id,
                tool: Some(tool),
                status: None,
                parent_id: None,
                message_id: None,
                representation: None,
                content,
            }
        } else {
            let envelope = if self.block {
                let mut envelope = meta.clone();
                if harness == "codex" {
                    envelope["payload"]["content"] = serde_json::json!([&*content]);
                } else if envelope["type"] == "custom_message" {
                    envelope["content"] = serde_json::json!([&*content]);
                } else {
                    envelope["message"]["content"] = serde_json::json!([&*content]);
                }
                envelope
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
            if turn.role == crate::model::Role::User {
                if let Some(kind) = self.message_kind {
                    turn.kind = kind;
                }
            }
            if turn.model.is_none() {
                turn.model = recorded_model(&meta, harness);
            }
            let kind = turn_kind(turn.kind);
            Item::from_turn(start, end, kind, turn, content)
        };
        item.parent_id = self.parent_id.clone();
        item.message_id = message_id;
        item.representation = self.runtime.then(|| "runtime".to_owned());
        Ok(Some(item))
    }
}

fn metadata(raw: &RawValue) -> Result<Value> {
    let mut fields = serde_json::Map::new();
    for (key, value) in object(raw)? {
        if matches!(
            key,
            "content" | "toolUseResult" | "arguments" | "input" | "output" | "item"
        ) {
            continue;
        }
        fields.insert(
            key.to_owned(),
            if matches!(key, "message" | "payload") {
                metadata(value)?
            } else {
                serde_json::from_str(value.get())?
            },
        );
    }
    Ok(Value::Object(fields))
}
fn recorded_model(meta: &Value, harness: &str) -> Option<crate::model::Model> {
    let id = meta["message"]["model"].as_str()?;
    if id == "<synthetic>" {
        return None;
    }
    Some(crate::model::Model {
        id: id.to_owned(),
        variant: (harness == "claude")
            .then(|| {
                meta["perTurnEffort"]
                    .as_str()
                    .or_else(|| meta["effort"].as_str())
                    .filter(|effort| !effort.is_empty())
                    .map(str::to_owned)
            })
            .flatten(),
    })
}

fn fragments<'a>(
    line: &'a [u8],
    harness: &str,
    child: Option<&str>,
    id: &str,
    origins: &[String],
) -> Result<Vec<Fragment<'a>>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
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
        if subtype == "message" {
            if let Some(content) = payload_obj.get("content") {
                let blocks: Vec<&RawValue> = serde_json::from_str(content.get())?;
                let message_kind = if kind == Kind::User {
                    Some(crate::backend::codex::codex_user_kind(
                        &serde_json::from_str(payload.get())?,
                    ))
                } else {
                    None
                };
                let mut fragments = Vec::new();
                for block in blocks {
                    let mut fragment = make(kind, block, true, false);
                    fragment.message_kind = message_kind;
                    fragments.push(fragment);
                }
                return Ok(fragments);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn api_store_refuses_positions_without_starting_a_process() {
        let r = Recording {
            id: "fixture".to_owned(),
            harness: "opencode".to_owned(),
            path: None,
            store: Some(NativeStore::opencode_v2("must-not-execute")),
            child: None,
        };
        assert!(matches!(
            r.tail(&Limits::default())
                .unwrap_err()
                .downcast_ref::<Refusal>(),
            Some(Refusal::Unsupported { .. })
        ));
    }
}
