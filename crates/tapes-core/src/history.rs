//! Stateless backward pages of normalized file-backed session evidence.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend;
use crate::model::{Model, Session, Turn};
use crate::Selection;

pub const DEFAULT_BYTES: usize = 64 * 1024;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    session: String,
    store: String,
    device: u64,
    inode: u64,
    size: u64,
    modified: i64,
    nanos: i64,
    end: u64,
    discard_suffix: bool,
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
    normalize: impl FnOnce(&[Value], &[Value]) -> (Vec<Turn>, Vec<ModelObservation>),
) -> Result<Page> {
    if !(1024..=MAX_BYTES).contains(&bytes) {
        bail!("page bytes must be between 1024 and {MAX_BYTES}");
    }
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    let mut state = Cursor {
        session: session.id.clone(),
        store: path.display().to_string(),
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified: metadata.mtime(),
        nanos: metadata.mtime_nsec(),
        end: metadata.len(),
        discard_suffix: false,
    };
    if let Some(cursor) = cursor {
        if cursor.len() > 16384 {
            bail!("invalid oversized history cursor");
        }
        let parsed: Cursor = serde_json::from_str(cursor).context("invalid history cursor")?;
        let mut expected = state.clone();
        expected.end = parsed.end;
        expected.discard_suffix = parsed.discard_suffix;
        if parsed != expected || parsed.end > metadata.len() {
            bail!("history source changed or cursor belongs to another recording; restart without --cursor");
        }
        state = parsed;
    }
    let end = state.end;
    let window_start = end.saturating_sub(bytes as u64);
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
    let start_index = if window_start == 0 {
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
    let mut values = Vec::new();
    let mut skipped_records = 0;
    for line in data[start_index..finish]
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        match serde_json::from_slice(line) {
            Ok(value) => values.push(value),
            Err(_) => skipped_records += 1,
        }
    }
    let opening = backend::head_jsonl(path);
    let (mut turns, models) = normalize(&values, &opening);
    for (ordinal, turn) in turns.iter_mut().enumerate() {
        turn.ordinal = ordinal;
    }
    let after = std::fs::metadata(path)?;
    if after.dev() != state.device
        || after.ino() != state.inode
        || after.len() != state.size
        || after.mtime() != state.modified
        || after.mtime_nsec() != state.nanos
    {
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
        skipped_records,
        skipped_fragment_bytes,
        turns,
        models,
        next_cursor,
    })
}

#[derive(Debug, Serialize)]
pub struct SearchMatch {
    pub page_start: u64,
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
    if !(1..=32).contains(&pages) {
        bail!("history search pages must be between 1 and 32");
    }
    if needle.is_empty() {
        bail!("history search text must not be empty");
    }
    let backends = backend::backends();
    let resolved = selection.resolve(&backends)?;
    let backend = &backends[resolved.backend_index];
    let mut result = Search {
        schema: "tapes-history-search/1",
        session: resolved.session.clone(),
        pages_read: 0,
        bytes_read: 0,
        skipped_records: 0,
        skipped_fragment_bytes: 0,
        matches: vec![],
        matches_truncated: false,
        next_cursor: cursor.map(str::to_owned),
    };
    let needle = needle.to_lowercase();
    for _ in 0..pages {
        let page = backend.history_page(&resolved.session, result.next_cursor.as_deref(), bytes)?;
        result.pages_read += 1;
        result.bytes_read += page.bytes_read;
        result.skipped_records += page.skipped_records;
        result.skipped_fragment_bytes += page.skipped_fragment_bytes;
        for turn in page.turns {
            if turn.text.to_lowercase().contains(&needle) {
                if result.matches.len() < 100 {
                    result.matches.push(SearchMatch {
                        page_start: page.start,
                        ordinal: turn.ordinal,
                        text: turn.text.chars().take(600).collect(),
                        text_truncated: turn.text.chars().count() > 600,
                    });
                } else {
                    result.matches_truncated = true;
                }
            }
        }
        result.next_cursor = page.next_cursor;
        if result.next_cursor.is_none() {
            break;
        }
    }
    Ok(result)
}

#[derive(Debug, Serialize)]
pub struct MetadataHistory {
    pub schema: &'static str,
    pub session: Session,
    pub pages_read: usize,
    pub bytes_read: usize,
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
    if !(1..=32).contains(&pages) {
        bail!("metadata pages must be between 1 and 32");
    }
    let backends = backend::backends();
    let resolved = selection.resolve(&backends)?;
    let backend = &backends[resolved.backend_index];
    let mut result = MetadataHistory {
        schema: "tapes-metadata-history/1",
        session: resolved.session.clone(),
        pages_read: 0,
        bytes_read: 0,
        skipped_records: 0,
        skipped_fragment_bytes: 0,
        observations: vec![],
        observations_truncated: false,
        next_cursor: cursor.map(str::to_owned),
    };
    for _ in 0..pages {
        let page = backend.history_page(&resolved.session, result.next_cursor.as_deref(), bytes)?;
        result.pages_read += 1;
        result.bytes_read += page.bytes_read;
        result.skipped_records += page.skipped_records;
        result.skipped_fragment_bytes += page.skipped_fragment_bytes;
        for observation in page.models.into_iter().rev() {
            if result.observations.len() < 100 {
                result.observations.push(observation);
            } else {
                result.observations_truncated = true;
            }
        }
        result.next_cursor = page.next_cursor;
        if result.next_cursor.is_none() {
            break;
        }
    }
    Ok(result)
}
