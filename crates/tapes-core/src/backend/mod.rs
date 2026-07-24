use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::{Session, Transcript, Turn};

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;

const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;

pub trait Backend {
    fn harness(&self) -> &'static str;
    fn available(&self) -> bool;
    fn list(&self, limit: usize) -> Result<Vec<Session>>;
    fn transcript(&self, id: &str, tail: usize) -> Result<Transcript>;
}

pub fn backends() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(claude::ClaudeBackend::default()),
        Box::new(codex::CodexBackend::default()),
        Box::new(opencode::OpenCodeBackend::default()),
        Box::new(pi::PiBackend::default()),
    ]
}

pub(crate) struct Jsonl {
    pub values: Vec<Value>,
    pub skipped: usize,
    pub truncated: bool,
}

pub(crate) fn read_jsonl(path: &Path) -> Result<Jsonl> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .len();
    let truncated = len > MAX_TRANSCRIPT_BYTES;
    let start = len.saturating_sub(MAX_TRANSCRIPT_BYTES);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("failed to seek {}", path.display()))?;

    let mut bytes = Vec::with_capacity((len - start) as usize);
    file.take(MAX_TRANSCRIPT_BYTES)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let bytes = if truncated {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(&[][..], |newline| &bytes[newline + 1..])
    } else {
        &bytes
    };

    let mut values = Vec::new();
    let mut skipped = 0;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice(line) {
            Ok(value) => values.push(value),
            Err(_) => skipped += 1,
        }
    }

    Ok(Jsonl {
        values,
        skipped,
        truncated,
    })
}

pub(crate) fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

pub(crate) fn time_range(values: &[Value]) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let mut timestamps = values
        .iter()
        .filter_map(|value| timestamp(&value["timestamp"]));
    let first = timestamps.next()?;
    Some(
        timestamps.fold((first, first), |(earliest, latest), value| {
            (earliest.min(value), latest.max(value))
        }),
    )
}

pub(crate) fn transcript(
    session: Session,
    mut turns: Vec<Turn>,
    tail: usize,
    read: &Jsonl,
    mut notes: Vec<String>,
) -> Transcript {
    let tail_truncated = turns.len() > tail;
    if tail_truncated {
        turns.drain(..turns.len() - tail);
    }
    if read.skipped > 0 {
        let noun = if read.skipped == 1 { "line" } else { "lines" };
        notes.push(format!("Skipped {} unparseable {noun}.", read.skipped));
    }

    Transcript {
        session,
        turns,
        truncated: read.truncated || tail_truncated,
        notes,
    }
}

pub(crate) fn home_path(parts: &[&str]) -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).map(|mut path| {
        path.extend(parts);
        path
    })
}

pub(crate) fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: &Path, files: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, files);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(root, &mut files);
    files.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    files.reverse();
    files
}

pub(crate) fn session_file(root: &Path, id: &str) -> Option<PathBuf> {
    matching_session_file(jsonl_files(root), id)
}

pub(crate) fn matching_session_file(
    files: impl IntoIterator<Item = PathBuf>,
    id: &str,
) -> Option<PathBuf> {
    files.into_iter().find(|path| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| {
                stem == id
                    || stem
                        .strip_suffix(id)
                        .is_some_and(|prefix| prefix.ends_with('-'))
            })
    })
}
