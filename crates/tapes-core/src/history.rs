//! Stateless backward pages of normalized file-backed session evidence.
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::backend::{self, Backend};
use chrono::Utc;

use crate::content::{self, ContentInventory};
use crate::model::{
    ByteSpan, Model, ReadEvidence, ReadGap, ReadRange, ReadRangeKind, RecordRef, Session, Turn,
    TurnKind,
};
use crate::Selection;

pub const DEFAULT_BYTES: usize = 64 * 1024;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const PAGE_SCHEMA: &str = "tapes-page/11";
pub const HISTORY_SEARCH_SCHEMA: &str = "tapes-history-search/10";
pub const METADATA_HISTORY_SCHEMA: &str = "tapes-metadata-history/10";
const MAX_CURSOR_BYTES: usize = 16 * 1024;
const MAX_PAGES: usize = 32;
const MAX_RESULTS: usize = 100;
const EXCERPT_CHARS: usize = 600;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageProjection {
    Transcript,
    Models,
}

impl PageProjection {
    fn options(self) -> Vec<String> {
        match self {
            Self::Transcript => vec!["transcript".to_owned()],
            Self::Models => vec!["models-only".to_owned()],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StrictCursor {
    session: String,
    store: String,
    device: u64,
    inode: u64,
    size: u64,
    modified: i64,
    nanos: i64,
    changed: i64,
    changed_nanos: i64,
    end: u64,
    discard_suffix: bool,
}

impl StrictCursor {
    fn at_end(session: &Session, path: &Path, metadata: &Metadata) -> Self {
        Self {
            session: session.id.clone(),
            store: path.display().to_string(),
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified: metadata.mtime(),
            nanos: metadata.mtime_nsec(),
            changed: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
            end: metadata.len(),
            discard_suffix: false,
        }
    }

    fn resume(self, cursor: Option<&str>) -> Result<Self> {
        let Some(cursor) = cursor else {
            return Ok(self);
        };
        if cursor.len() > MAX_CURSOR_BYTES {
            bail!("invalid oversized history cursor");
        }
        let parsed: Self = serde_json::from_str(cursor).context("invalid history cursor")?;
        let mut expected = self;
        expected.end = parsed.end;
        expected.discard_suffix = parsed.discard_suffix;
        if parsed != expected || parsed.end > expected.size {
            bail!("history source changed or cursor belongs to another recording; restart without --cursor");
        }
        Ok(parsed)
    }

    fn matches_file(&self, metadata: &Metadata) -> bool {
        metadata.dev() == self.device
            && metadata.ino() == self.inode
            && metadata.len() == self.size
            && metadata.mtime() == self.modified
            && metadata.mtime_nsec() == self.nanos
            && metadata.ctime() == self.changed
            && metadata.ctime_nsec() == self.changed_nanos
    }
}

const PIN_REQUEST: &str = "pin";
const MAX_PIN_BYTES: u64 = 256 * MAX_BYTES as u64;

/// Initiate a pinned walk, or continue the mode carried by an existing cursor.
pub fn walk_cursor(pin: bool, cursor: Option<&str>) -> Result<Option<&str>> {
    if pin && cursor.is_some() {
        bail!("--pin cannot be used with --cursor; restart with --pin to pin a recording");
    }
    if !pin && cursor == Some(PIN_REQUEST) {
        bail!("invalid history cursor");
    }
    Ok(if pin { Some(PIN_REQUEST) } else { cursor })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum Cursor {
    Strict(StrictCursor),
    Pinned(PinnedCursor),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PinnedCursor {
    session: String,
    store: String,
    device: u64,
    inode: u64,
    end: u64,
    chunk: u64,
    digests: String,
    prefix_sha256: String,
    revision: String,
    next_end: u64,
    discard_suffix: bool,
}

#[derive(Debug, Serialize)]
pub struct PinEvidence {
    pub chunk_bytes: u64,
    pub chunks: usize,
    pub pinned_bytes: u64,
    pub verification_bytes: usize,
    pub verified: Vec<ByteSpan>,
}

fn chunk_size(end: u64, bytes: usize) -> Result<u64> {
    if end > MAX_PIN_BYTES {
        bail!("--pin supports prefixes up to 1GiB; page a captured copy with --native-store");
    }
    Ok((bytes as u64).max(end.div_ceil(256)))
}

fn chunk_span(end: u64, chunk: u64, index: u64) -> ByteSpan {
    ByteSpan {
        start: end.saturating_sub((index + 1) * chunk),
        end: end.saturating_sub(index * chunk),
    }
}

impl PinnedCursor {
    fn capture(file: &mut File, state: &StrictCursor, bytes: usize) -> Result<(Self, Vec<u8>)> {
        let chunk = chunk_size(state.size, bytes)?;
        let count = state.size.div_ceil(chunk) as usize;
        let mut hashes = vec![[0u8; 32]; count];
        let mut whole = Sha256::new();
        let mut partial = Sha256::new();
        let mut tail = Vec::with_capacity(chunk as usize);
        let mut buffer = vec![0; 256 * 1024];
        let mut offset = 0;
        file.seek(SeekFrom::Start(0))?;
        while offset < state.size {
            let index = (state.size - 1 - offset) / chunk;
            let span = chunk_span(state.size, chunk, index);
            let n = buffer.len().min((span.end - offset) as usize);
            file.read_exact(&mut buffer[..n])?;
            whole.update(&buffer[..n]);
            partial.update(&buffer[..n]);
            if index == 0 {
                tail.extend_from_slice(&buffer[..n]);
            }
            offset += n as u64;
            if offset == span.end {
                hashes[index as usize] = partial.finalize_reset().into();
            }
        }
        let prefix = crate::model::SourcePrefixDigest::sha256(whole.finalize().into(), state.size);
        Ok((
            Self {
                session: state.session.clone(),
                store: state.store.clone(),
                device: state.device,
                inode: state.inode,
                end: state.size,
                chunk,
                digests: STANDARD.encode(hashes.concat()),
                prefix_sha256: prefix.value,
                revision: cursor_revision(state),
                next_end: state.size,
                discard_suffix: false,
            },
            tail,
        ))
    }

    fn matches_file(&self, metadata: &Metadata) -> bool {
        metadata.dev() == self.device && metadata.ino() == self.inode && metadata.len() >= self.end
    }

    fn validate(&self, state: &StrictCursor) -> Result<Vec<u8>> {
        let hashes = STANDARD
            .decode(&self.digests)
            .context("invalid pin digests")?;
        if self.session != state.session
            || self.store != state.store
            || self.device != state.device
            || self.inode != state.inode
            || state.size < self.end
        {
            bail!("history source changed or cursor belongs to another recording; restart without --cursor");
        }
        if self.end > MAX_PIN_BYTES
            || self.chunk == 0
            || self.chunk > MAX_BYTES as u64
            || self.end.div_ceil(self.chunk) > 256
            || self.next_end > self.end
            || hashes.len() != self.end.div_ceil(self.chunk) as usize * 32
            || self.prefix_sha256.len() != 64
        {
            bail!("invalid pinned history cursor");
        }
        Ok(hashes)
    }
}

#[derive(Debug, Serialize)]
pub struct ModelObservation {
    pub model: Model,
    pub timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub schema: &'static str,
    pub session: Session,
    pub start: u64,
    pub end: u64,
    pub source_bytes: u64,
    pub bytes_read: usize,
    pub alignment_bytes: usize,
    pub skipped_records: usize,
    pub skipped_fragment_bytes: usize,
    pub read: ReadEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<PinEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    pub turns: Vec<Turn>,
    pub models: Vec<ModelObservation>,
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

pub fn page(selection: Selection<'_>, cursor: Option<&str>, bytes: usize) -> Result<Page> {
    page_with_backends(&backend::backends(), selection, cursor, bytes)
}

/// Read history using only the supplied backends.
pub fn page_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection<'_>,
    cursor: Option<&str>,
    bytes: usize,
) -> Result<Page> {
    let resolved = selection.resolve(backends)?;
    let mut page =
        backends[resolved.backend_index].history_page(&resolved.session, cursor, bytes)?;
    page.notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    Ok(page)
}

pub(crate) fn read_file(
    session: &Session,
    path: &Path,
    cursor: Option<&str>,
    bytes: usize,
    projection: PageProjection,
    normalize: impl FnOnce(&[Value], &[ByteSpan], &str) -> (Vec<Turn>, Vec<ModelObservation>),
) -> Result<Page> {
    if !(1024..=MAX_BYTES).contains(&bytes) {
        bail!(
            "page bytes must be between 1KiB and {}",
            crate::byte_size::ByteSize::new(MAX_BYTES as u64)
        );
    }
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    let current = StrictCursor::at_end(session, path, &metadata);
    let mut cached_tail = None;
    let mut state = if cursor == Some(PIN_REQUEST) {
        let (pin, tail) = PinnedCursor::capture(&mut file, &current, bytes)?;
        cached_tail = Some(tail);
        Cursor::Pinned(pin)
    } else if let Some(raw) = cursor {
        if raw.len() > MAX_CURSOR_BYTES {
            bail!("invalid oversized history cursor");
        }
        let mut value: Value = serde_json::from_str(raw).context("invalid history cursor")?;
        if value.get("mode").is_none() {
            value
                .as_object_mut()
                .context("invalid history cursor")?
                .insert("mode".into(), Value::String("strict".into()));
        }
        let parsed: Cursor = serde_json::from_value(value).context("invalid history cursor")?;
        match &parsed {
            Cursor::Strict(_) => {
                current.clone().resume(Some(raw))?;
            }
            Cursor::Pinned(pin) => {
                pin.validate(&current)?;
            }
        }
        parsed
    } else {
        Cursor::Strict(current)
    };
    let (end, discard_suffix) = match &state {
        Cursor::Strict(s) => (s.end, s.discard_suffix),
        Cursor::Pinned(s) => (s.next_end, s.discard_suffix),
    };
    let window_start = end.saturating_sub(bytes as u64);
    let alignment_bytes = usize::from(window_start > 0);
    let read_start = window_start.saturating_sub(1);
    let mut pin_evidence = None;
    let (data, aligned) = match &state {
        Cursor::Strict(_) => {
            file.seek(SeekFrom::Start(read_start))?;
            let mut raw = vec![0; (end - read_start) as usize];
            file.read_exact(&mut raw)?;
            let aligned = window_start == 0 || raw[0] == b'\n';
            (raw[alignment_bytes..].to_vec(), aligned)
        }
        Cursor::Pinned(pin) => {
            let hashes = pin.validate(&StrictCursor::at_end(session, path, &metadata))?;
            let mut raw = Vec::new();
            let mut verified = Vec::new();
            let mut verification_bytes = 0;
            if end > read_start {
                let first = (pin.end - end) / pin.chunk;
                let last = (pin.end - 1 - read_start) / pin.chunk;
                for index in (first..=last).rev() {
                    let span = chunk_span(pin.end, pin.chunk, index);
                    let data = if index == 0 && cached_tail.is_some() {
                        cached_tail.take().unwrap()
                    } else {
                        file.seek(SeekFrom::Start(span.start))?;
                        let mut data = vec![0; (span.end - span.start) as usize];
                        file.read_exact(&mut data)?;
                        data
                    };
                    let digest: [u8; 32] = Sha256::digest(&data).into();
                    if digest.as_slice() != &hashes[index as usize * 32..(index as usize + 1) * 32]
                    {
                        bail!("history source changed inside the pinned prefix (bytes {}..{}); restart without --cursor, or page a captured copy with --native-store", span.start, span.end);
                    }
                    verification_bytes += data.len();
                    let lo = read_start.max(span.start) - span.start;
                    let hi = end.min(span.end) - span.start;
                    raw.extend_from_slice(&data[lo as usize..hi as usize]);
                    verified.push(span);
                }
            }
            let aligned = window_start == 0 || raw[0] == b'\n';
            pin_evidence = Some(PinEvidence {
                chunk_bytes: pin.chunk,
                chunks: hashes.len() / 32,
                pinned_bytes: pin.end,
                verification_bytes,
                verified,
            });
            (raw[alignment_bytes..].to_vec(), aligned)
        }
    };
    let mut discard_suffix = discard_suffix;
    let mut finish = data.len();
    let mut skipped_fragment_bytes = 0;
    if discard_suffix {
        if let Some(newline) = data.iter().rposition(|b| *b == b'\n') {
            finish = newline + 1;
            skipped_fragment_bytes += data.len() - finish;
            discard_suffix = false;
        } else {
            finish = 0;
            skipped_fragment_bytes += data.len();
        }
    }
    let start_index = if aligned {
        0
    } else {
        data[..finish]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(finish, |i| i + 1)
    };
    let mut start = window_start + start_index as u64;
    if start_index == finish && window_start > 0 && finish == data.len() {
        // No whole record fits. Advance through its bytes explicitly without
        // inventing a normalized record from either fragment.
        skipped_fragment_bytes = data.len();
        discard_suffix = true;
        start = window_start;
    }
    let (values, spans, skipped_records, mut gaps) = parse_records(
        &data[start_index..finish],
        window_start + start_index as u64,
    );
    if start_index > 0 {
        gaps.push(ReadGap {
            span: ByteSpan {
                start: window_start,
                end: window_start + start_index as u64,
            },
            reason: "discarded-partial-record".to_owned(),
        });
    }
    if finish < data.len() {
        gaps.push(ReadGap {
            span: ByteSpan {
                start: window_start + finish as u64,
                end,
            },
            reason: "discarded-partial-suffix".to_owned(),
        });
    }
    let revision = match &state {
        Cursor::Strict(s) => cursor_revision(s),
        Cursor::Pinned(s) => s.revision.clone(),
    };
    let (mut turns, models) = normalize(&values, &spans, &revision);
    for (ordinal, turn) in turns.iter_mut().enumerate() {
        turn.ordinal = ordinal;
        turn.establish_origin(session.launch);
    }
    let after = std::fs::metadata(path)?;
    if !match &state {
        Cursor::Strict(s) => s.matches_file(&after),
        Cursor::Pinned(s) => s.matches_file(&after),
    } {
        bail!("history source changed during the page read; restart without --cursor");
    }
    let (source_length, source_prefix_digest) = match &mut state {
        Cursor::Strict(s) => {
            s.end = start;
            s.discard_suffix = discard_suffix;
            (s.size, None)
        }
        Cursor::Pinned(s) => {
            s.next_end = start;
            s.discard_suffix = discard_suffix;
            (
                s.end,
                Some(crate::model::SourcePrefixDigest {
                    algorithm: crate::model::DigestAlgorithm::Sha256,
                    value: s.prefix_sha256.clone(),
                    span: ByteSpan {
                        start: 0,
                        end: s.end,
                    },
                }),
            )
        }
    };
    let next_cursor = (start > 0)
        .then(|| serde_json::to_string(&state))
        .transpose()?;
    let mut ranges = Vec::with_capacity(2);
    if window_start > 0 {
        ranges.push(ReadRange {
            kind: ReadRangeKind::Alignment,
            span: ByteSpan {
                start: window_start - 1,
                end: window_start,
            },
        });
    }
    ranges.push(ReadRange {
        kind: ReadRangeKind::Tail,
        span: ByteSpan {
            start: window_start,
            end,
        },
    });
    if let Some(pin) = &pin_evidence {
        ranges.extend(pin.verified.iter().map(|span| ReadRange {
            kind: ReadRangeKind::Verification,
            span: *span,
        }));
    }
    let read = ReadEvidence {
        source_length,
        configured_bound: bytes as u64,
        coordinate_domain: "file-byte-range".to_owned(),
        source_revision: Some(revision),
        source_prefix_digest,
        source_prefix_sha256: None,
        producer: session.source.producer.clone(),
        projection: PAGE_SCHEMA.to_owned(),
        projection_options: projection.options(),
        observed_at: Utc::now(),
        records: spans,
        ranges,
        context_records: Vec::new(),
        gaps,
        unmapped: None,
        record_sha256: Vec::new(),
        reader: Some(crate::reader::identity()),
    };
    Ok(Page {
        schema: PAGE_SCHEMA,
        session: session.clone(),
        start,
        end,
        source_bytes: source_length,
        bytes_read: data.len(),
        alignment_bytes,
        skipped_records,
        skipped_fragment_bytes,
        read,
        pin: pin_evidence,
        content: content::inventory(&turns),
        turns,
        models,
        next_cursor,
        notes: Vec::new(),
    })
}

fn parse_records(bytes: &[u8], base: u64) -> (Vec<Value>, Vec<ByteSpan>, usize, Vec<ReadGap>) {
    let mut values = Vec::new();
    let mut spans = Vec::new();
    let mut gaps = Vec::new();
    let mut skipped = 0;
    let mut offset = 0;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let span = ByteSpan {
            start: base + offset,
            end: base + offset + line.len() as u64,
        };
        offset += line.len() as u64;
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice(line) {
            Ok(value) => {
                values.push(value);
                spans.push(span);
            }
            Err(_) => {
                skipped += 1;
                gaps.push(ReadGap {
                    span,
                    reason: "malformed-record".to_owned(),
                });
            }
        }
    }
    (values, spans, skipped, gaps)
}

fn cursor_revision(cursor: &StrictCursor) -> String {
    format!(
        "stat:{}:{}:{}:{}:{}:{}:{}",
        cursor.device,
        cursor.inode,
        cursor.size,
        cursor.modified,
        cursor.nanos,
        cursor.changed,
        cursor.changed_nanos
    )
}

#[derive(Default)]
struct ReadProgress {
    pages_read: usize,
    bytes_read: usize,
    alignment_bytes: usize,
    skipped_records: usize,
    skipped_fragment_bytes: usize,
    reads: Vec<ReadEvidence>,
    content: ContentInventory,
    next_cursor: Option<String>,
}

fn visit_pages(
    backend: &dyn Backend,
    session: &Session,
    cursor: Option<&str>,
    bytes: usize,
    pages: usize,
    projection: PageProjection,
    mut visit: impl FnMut(Page),
) -> Result<ReadProgress> {
    let mut progress = ReadProgress {
        next_cursor: cursor.map(str::to_owned),
        ..ReadProgress::default()
    };
    for _ in 0..pages {
        let cursor = progress.next_cursor.as_deref();
        let page = match projection {
            PageProjection::Transcript => backend.history_page(session, cursor, bytes)?,
            PageProjection::Models => backend.metadata_page(session, cursor, bytes)?,
        };
        progress.pages_read += 1;
        progress.bytes_read += page.bytes_read;
        progress.alignment_bytes += page.alignment_bytes;
        progress.skipped_records += page.skipped_records;
        progress.skipped_fragment_bytes += page.skipped_fragment_bytes;
        progress.reads.push(page.read.clone());
        if let Some(content) = page.content.as_ref() {
            progress.content.merge(content);
        }
        progress.next_cursor = page.next_cursor.clone();
        visit(page);
        if progress.next_cursor.is_none() {
            break;
        }
    }
    Ok(progress)
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    /// Byte bounds of the page containing the match.
    pub page_start: u64,
    pub page_end: u64,
    /// Zero-based position within this page, not the full session.
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    pub kind: TurnKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<chrono::DateTime<Utc>>,
    pub text: String,
    pub text_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct Search {
    pub schema: &'static str,
    pub session: Session,
    pub pages_read: usize,
    pub bytes_read: usize,
    pub alignment_bytes: usize,
    pub skipped_records: usize,
    pub skipped_fragment_bytes: usize,
    pub reads: Vec<ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    pub matches: Vec<SearchMatch>,
    pub matches_truncated: bool,
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

pub fn search(
    selection: Selection<'_>,
    cursor: Option<&str>,
    bytes: usize,
    pages: usize,
    needle: &str,
) -> Result<Search> {
    search_with_backends(
        &backend::backends(),
        selection,
        cursor,
        bytes,
        pages,
        needle,
    )
}

/// Read history using only the supplied backends.
pub fn search_with_backends(
    backends: &[Box<dyn Backend>],

    selection: Selection<'_>,
    cursor: Option<&str>,
    bytes: usize,
    pages: usize,
    needle: &str,
) -> Result<Search> {
    if !(1..=MAX_PAGES).contains(&pages) {
        bail!("history search pages must be between 1 and {MAX_PAGES}");
    }
    if needle.is_empty() {
        bail!("history search text must not be empty");
    }
    let resolved = selection.resolve(backends)?;
    let backend = &backends[resolved.backend_index];
    let mut matches = Vec::new();
    let mut matches_truncated = false;
    let needle = needle.to_lowercase();
    let progress = visit_pages(
        backend.as_ref(),
        &resolved.session,
        cursor,
        bytes,
        pages,
        PageProjection::Transcript,
        |page| {
            for turn in page.turns {
                if turn.text.to_lowercase().contains(&needle) {
                    if matches.len() < MAX_RESULTS {
                        matches.push(SearchMatch {
                            page_start: page.start,
                            page_end: page.end,
                            ordinal: turn.ordinal,
                            record_ref: turn.record_ref,
                            native_id: turn.native_id,
                            kind: turn.kind,
                            ts: turn.ts,
                            text: turn.text.chars().take(EXCERPT_CHARS).collect(),
                            text_truncated: turn.text.chars().count() > EXCERPT_CHARS,
                        });
                    } else {
                        matches_truncated = true;
                    }
                }
            }
        },
    )?;
    let notes = resolved.diagnostics.latest_warnings(&resolved.session);
    Ok(Search {
        schema: HISTORY_SEARCH_SCHEMA,
        session: resolved.session,
        pages_read: progress.pages_read,
        bytes_read: progress.bytes_read,
        alignment_bytes: progress.alignment_bytes,
        skipped_records: progress.skipped_records,
        skipped_fragment_bytes: progress.skipped_fragment_bytes,
        reads: progress.reads,
        content: (!progress.content.is_empty()).then_some(progress.content),
        matches,
        matches_truncated,
        next_cursor: progress.next_cursor,
        notes,
    })
}

#[derive(Debug, Serialize)]
pub struct MetadataHistory {
    pub schema: &'static str,
    pub session: Session,
    pub pages_read: usize,
    pub bytes_read: usize,
    pub alignment_bytes: usize,
    pub skipped_records: usize,
    pub skipped_fragment_bytes: usize,
    pub reads: Vec<ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    pub observations: Vec<ModelObservation>,
    pub observations_truncated: bool,
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Recorded model observations in reverse record order, without asserting a current model.
pub fn metadata(
    selection: Selection<'_>,
    cursor: Option<&str>,
    bytes: usize,
    pages: usize,
) -> Result<MetadataHistory> {
    metadata_with_backends(&backend::backends(), selection, cursor, bytes, pages)
}

/// Read history using only the supplied backends.
pub fn metadata_with_backends(
    backends: &[Box<dyn Backend>],

    selection: Selection<'_>,
    cursor: Option<&str>,
    bytes: usize,
    pages: usize,
) -> Result<MetadataHistory> {
    if !(1..=MAX_PAGES).contains(&pages) {
        bail!("metadata pages must be between 1 and {MAX_PAGES}");
    }
    let resolved = selection.resolve(backends)?;
    let backend = &backends[resolved.backend_index];
    let mut observations = Vec::new();
    let mut observations_truncated = false;
    let progress = visit_pages(
        backend.as_ref(),
        &resolved.session,
        cursor,
        bytes,
        pages,
        PageProjection::Models,
        |page| {
            for observation in page.models.into_iter().rev() {
                if observations.len() < MAX_RESULTS {
                    observations.push(observation);
                } else {
                    observations_truncated = true;
                }
            }
        },
    )?;
    let notes = resolved.diagnostics.latest_warnings(&resolved.session);
    Ok(MetadataHistory {
        schema: METADATA_HISTORY_SCHEMA,
        session: resolved.session,
        pages_read: progress.pages_read,
        bytes_read: progress.bytes_read,
        alignment_bytes: progress.alignment_bytes,
        skipped_records: progress.skipped_records,
        skipped_fragment_bytes: progress.skipped_fragment_bytes,
        reads: progress.reads,
        content: (!progress.content.is_empty()).then_some(progress.content),
        observations,
        observations_truncated,
        next_cursor: progress.next_cursor,
        notes,
    })
}

#[cfg(test)]
mod pinned_tests {
    use super::*;

    #[test]
    fn chunk_bounds_and_cursor_capacity() {
        assert_eq!(chunk_size(0, 1024).unwrap(), 1024);
        assert_eq!(chunk_size(MAX_PIN_BYTES, 1024).unwrap(), MAX_BYTES as u64);
        assert!(chunk_size(MAX_PIN_BYTES + 1, MAX_BYTES).is_err());
        assert_eq!(chunk_span(2500, 1024, 2), ByteSpan { start: 0, end: 452 });
        let pin = Cursor::Pinned(PinnedCursor {
            session: "fixture-session".into(),
            store: "/fixtures/recording.jsonl".into(),
            device: 1,
            inode: 2,
            end: MAX_PIN_BYTES,
            chunk: MAX_BYTES as u64,
            digests: STANDARD.encode(vec![0u8; 256 * 32]),
            prefix_sha256: "0".repeat(64),
            revision: "stat:1:2:1073741824:0:0:0:0".into(),
            next_end: MAX_PIN_BYTES,
            discard_suffix: false,
        });
        let encoded = serde_json::to_string(&pin).unwrap();
        eprintln!(
            "256-chunk cursor: {} bytes (limit {})",
            encoded.len(),
            MAX_CURSOR_BYTES
        );
        assert!(encoded.len() < MAX_CURSOR_BYTES);
    }
}
