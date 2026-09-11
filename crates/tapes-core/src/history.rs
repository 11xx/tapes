//! Stateless backward pages of normalized file-backed session evidence.
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{self, Backend};
use crate::model::{Model, Session, Turn};
use crate::Selection;

pub const DEFAULT_BYTES: usize = 64 * 1024;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_CURSOR_BYTES: usize = 16 * 1024;
const PROVENANCE_BYTES: u64 = 64 * 1024;
const MAX_PAGES: usize = 32;
const MAX_RESULTS: usize = 100;
const EXCERPT_CHARS: usize = 600;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageProjection {
    Transcript,
    Models,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadContext {
    None,
    OperatorProvenance,
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
    pub context_bytes: usize,
    pub skipped_records: usize,
    pub skipped_fragment_bytes: usize,
    pub turns: Vec<Turn>,
    pub models: Vec<ModelObservation>,
    pub next_cursor: Option<String>,
}

pub fn page(selection: Selection<'_>, cursor: Option<&str>, bytes: usize) -> Result<Page> {
    let backends = backend::backends();
    let resolved = selection.resolve(&backends)?;
    backends[resolved.backend_index].history_page(&resolved.session, cursor, bytes)
}

pub(crate) fn read_file(
    session: &Session,
    path: &Path,
    cursor: Option<&str>,
    bytes: usize,
    context: ReadContext,
    normalize: impl FnOnce(&[Value], &[Value], &[Value]) -> (Vec<Turn>, Vec<ModelObservation>),
) -> Result<Page> {
    if !(1024..=MAX_BYTES).contains(&bytes) {
        bail!("page bytes must be between 1024 and {MAX_BYTES}");
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
    let (values, skipped_records) = parse_records(&data[start_index..finish]);
    let (newer_records, context_bytes) = read_context(&mut file, end, state.size, context)?;
    let opening = if context == ReadContext::OperatorProvenance {
        backend::head_jsonl(path)
    } else {
        Vec::new()
    };
    let (mut turns, models) = normalize(&values, &opening, &newer_records);
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
    Ok(Page {
        schema: "tapes-page/1",
        session: session.clone(),
        start,
        end,
        source_bytes: state.size,
        bytes_read: data.len(),
        alignment_bytes,
        context_bytes,
        skipped_records,
        skipped_fragment_bytes,
        turns,
        models,
        next_cursor,
    })
}

fn parse_records(bytes: &[u8]) -> (Vec<Value>, usize) {
    let mut values = Vec::new();
    let mut skipped = 0;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        match serde_json::from_slice(line) {
            Ok(value) => values.push(value),
            Err(_) => skipped += 1,
        }
    }
    (values, skipped)
}

fn read_context(
    file: &mut File,
    end: u64,
    size: u64,
    context: ReadContext,
) -> Result<(Vec<Value>, usize)> {
    if context == ReadContext::None {
        return Ok((Vec::new(), 0));
    }
    let bytes = (size - end).min(PROVENANCE_BYTES) as usize;
    let mut newer = vec![0; bytes];
    file.seek(SeekFrom::Start(end))?;
    file.read_exact(&mut newer)?;
    let complete = if end + bytes as u64 == size {
        newer.len()
    } else {
        newer
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |i| i + 1)
    };
    Ok((parse_records(&newer[..complete]).0, bytes))
}

#[derive(Default)]
struct ReadProgress {
    pages_read: usize,
    bytes_read: usize,
    alignment_bytes: usize,
    context_bytes: usize,
    skipped_records: usize,
    skipped_fragment_bytes: usize,
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
        progress.context_bytes += page.context_bytes;
        progress.skipped_records += page.skipped_records;
        progress.skipped_fragment_bytes += page.skipped_fragment_bytes;
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
    pub context_bytes: usize,
    pub skipped_records: usize,
    pub skipped_fragment_bytes: usize,
    pub matches: Vec<SearchMatch>,
    pub matches_truncated: bool,
    pub next_cursor: Option<String>,
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
    Ok(Search {
        schema: "tapes-history-search/1",
        session: resolved.session,
        pages_read: progress.pages_read,
        bytes_read: progress.bytes_read,
        alignment_bytes: progress.alignment_bytes,
        context_bytes: progress.context_bytes,
        skipped_records: progress.skipped_records,
        skipped_fragment_bytes: progress.skipped_fragment_bytes,
        matches,
        matches_truncated,
        next_cursor: progress.next_cursor,
    })
}

#[derive(Debug, Serialize)]
pub struct MetadataHistory {
    pub schema: &'static str,
    pub session: Session,
    pub pages_read: usize,
    pub bytes_read: usize,
    pub alignment_bytes: usize,
    pub context_bytes: usize,
    pub skipped_records: usize,
    pub skipped_fragment_bytes: usize,
    pub observations: Vec<ModelObservation>,
    pub observations_truncated: bool,
    pub next_cursor: Option<String>,
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
    Ok(MetadataHistory {
        schema: "tapes-metadata-history/1",
        session: resolved.session,
        pages_read: progress.pages_read,
        bytes_read: progress.bytes_read,
        alignment_bytes: progress.alignment_bytes,
        context_bytes: progress.context_bytes,
        skipped_records: progress.skipped_records,
        skipped_fragment_bytes: progress.skipped_fragment_bytes,
        observations,
        observations_truncated,
        next_cursor: progress.next_cursor,
    })
}
