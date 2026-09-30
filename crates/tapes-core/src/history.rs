//! Stateless backward pages of normalized file-backed session evidence.
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{self, Backend};
use chrono::Utc;

use crate::content::{self, ContentInventory};
use crate::model::{
    ByteSpan, Model, ReadEvidence, ReadGap, ReadRange, ReadRangeKind, Session, Turn,
};
use crate::Selection;

pub const DEFAULT_BYTES: usize = 64 * 1024;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const PAGE_SCHEMA: &str = "tapes-page/7";
pub const HISTORY_SEARCH_SCHEMA: &str = "tapes-history-search/7";
pub const METADATA_HISTORY_SCHEMA: &str = "tapes-metadata-history/7";
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
struct Cursor {
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

impl Cursor {
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
    pub content: Option<ContentInventory>,
    pub turns: Vec<Turn>,
    pub models: Vec<ModelObservation>,
    pub next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

pub fn page(selection: Selection<'_>, cursor: Option<&str>, bytes: usize) -> Result<Page> {
    let backends = backend::backends();
    let resolved = selection.resolve(&backends)?;
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
    let mut state = Cursor::at_end(session, path, &metadata).resume(cursor)?;
    let end = state.end;
    let window_start = end.saturating_sub(bytes as u64);
    let mut aligned = window_start == 0;
    let alignment_bytes = usize::from(window_start > 0);
    if window_start > 0 {
        file.seek(SeekFrom::Start(window_start - 1))?;
        let mut preceding = [0];
        file.read_exact(&mut preceding)?;
        aligned = preceding[0] == b'\n';
    }
    file.seek(SeekFrom::Start(window_start))?;
    let mut data = vec![0; (end - window_start) as usize];
    file.read_exact(&mut data)?;
    let mut finish = data.len();
    let mut skipped_fragment_bytes = 0;
    if state.discard_suffix {
        if let Some(newline) = data.iter().rposition(|b| *b == b'\n') {
            finish = newline + 1;
            skipped_fragment_bytes += data.len() - finish;
            state.discard_suffix = false;
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
        state.discard_suffix = true;
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
    let revision = cursor_revision(&state);
    let (mut turns, models) = normalize(&values, &spans, &revision);
    for (ordinal, turn) in turns.iter_mut().enumerate() {
        turn.ordinal = ordinal;
    }
    let after = std::fs::metadata(path)?;
    if !state.matches_file(&after) {
        bail!("history source changed during the page read; restart without --cursor");
    }
    state.end = start;
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
    let read = ReadEvidence {
        source_length: state.size,
        configured_bound: bytes as u64,
        coordinate_domain: "file-byte-range".to_owned(),
        source_revision: Some(revision),
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
        source_bytes: state.size,
        bytes_read: data.len(),
        alignment_bytes,
        skipped_records,
        skipped_fragment_bytes,
        read,
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

fn cursor_revision(cursor: &Cursor) -> String {
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
    pub page_start: u64,
    pub page_end: u64,
    pub ordinal: usize,
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
    if !(1..=MAX_PAGES).contains(&pages) {
        bail!("history search pages must be between 1 and {MAX_PAGES}");
    }
    if needle.is_empty() {
        bail!("history search text must not be empty");
    }
    let backends = backend::backends();
    let resolved = selection.resolve(&backends)?;
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
    if !(1..=MAX_PAGES).contains(&pages) {
        bail!("metadata pages must be between 1 and {MAX_PAGES}");
    }
    let backends = backend::backends();
    let resolved = selection.resolve(&backends)?;
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
