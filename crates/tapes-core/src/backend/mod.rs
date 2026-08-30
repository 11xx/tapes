use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::{Session, TrailingRecord, Transcript, Turn};
use crate::scope::Scope;

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;

const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;
/// Enough of a file's opening to carry any harness's session header, and
/// small enough that probing a whole store stays cheap.
const HEAD_PROBE_BYTES: u64 = 64 * 1024;

/// What a listing asks for.
///
/// The scope and metadata filters travel with the query rather than filtering
/// the result in a caller, because a limit applied before any of them answers
/// a different question: "the newest sessions, of which these happen to
/// match" instead of "the newest matching sessions".
pub struct Query<'a> {
    pub scope: Option<&'a Scope>,
    /// Sessions to return, per harness.
    pub limit: usize,
    /// Candidates a backend may inspect before it gives up. Bounds the search
    /// for a project whose sessions are all old, or absent.
    pub ceiling: usize,
    /// Lowercase substring of the full model identity, when filtering.
    pub model: Option<String>,
    /// Lowercase substring of a session's directory path, when filtering.
    pub directory: Option<String>,
}

impl<'a> Query<'a> {
    pub fn unscoped(limit: usize) -> Self {
        Self {
            scope: None,
            limit,
            ceiling: usize::MAX,
            model: None,
            directory: None,
        }
    }

    pub fn unscoped_with_filters(
        limit: usize,
        model: Option<&str>,
        directory: Option<&str>,
    ) -> Self {
        Self {
            model: model.map(str::to_lowercase),
            directory: directory.map(str::to_lowercase),
            ..Self::unscoped(limit)
        }
    }

    pub(crate) fn scoped_with_filters(
        scope: Option<&'a Scope>,
        limit: usize,
        ceiling: usize,
        model: Option<&str>,
        directory: Option<&str>,
    ) -> Self {
        Self {
            scope,
            limit,
            ceiling,
            model: model.map(str::to_lowercase),
            directory: directory.map(str::to_lowercase),
        }
    }

    pub(crate) fn has_filters(&self) -> bool {
        self.model.is_some() || self.directory.is_some()
    }

    pub(crate) fn matches(&self, session: &Session) -> bool {
        let model_matches = self.model.as_ref().is_none_or(|needle| {
            session
                .model
                .as_ref()
                .is_some_and(|model| model.identity().to_lowercase().contains(needle))
        });
        let directory_matches = self.directory.as_ref().is_none_or(|needle| {
            session.directory.as_deref().is_some_and(|directory| {
                directory.to_string_lossy().to_lowercase().contains(needle)
            })
        });
        model_matches && directory_matches
    }
}

/// What a listing found, and how hard it looked. `scanned` is candidates
/// inspected, not sessions returned: a caller reading an empty scoped listing
/// needs to know whether the store was exhausted or the search stopped.
#[derive(Debug, Default)]
pub struct Listing {
    pub sessions: Vec<Session>,
    /// Diagnostics for individual candidates that could not be normalized.
    /// These use the same vocabulary as the public listing's `unavailable`
    /// field, while a command or store failure still names the whole harness.
    pub unavailable: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
}

impl Listing {
    pub fn from_sessions(sessions: Vec<Session>) -> Self {
        Self {
            scanned: sessions.len(),
            sessions,
            unavailable: Vec::new(),
            scan_truncated: false,
        }
    }
}

pub trait Backend {
    fn harness(&self) -> &'static str;
    /// Report whether listing is likely to work for this backend.
    ///
    /// This is an advisory listing hint, not a precondition for other
    /// methods. Exact-id resolution deliberately skips it, and callers may
    /// still invoke `locate` or `transcript` when it returns `false` so those
    /// methods can report their own absence or failure.
    fn available(&self) -> bool;
    fn list(&self, query: &Query) -> Result<Listing>;
    /// Locate one session by its exact id without enumerating the store.
    /// `Ok(None)` means this backend does not hold it. Resolution calls this
    /// before it calls `list`, so an exact id never pays for a listing.
    fn locate(&self, id: &str) -> Result<Option<Session>>;
    /// Read a transcript for a session already resolved by this backend.
    /// Implementations must use the supplied normalized session rather than
    /// locating it again; the transcript read may still need to open the
    /// underlying record to collect turns.
    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript>;
}

pub fn backends() -> Vec<Box<dyn Backend>> {
    let mut backends: Vec<Box<dyn Backend>> = vec![
        Box::new(claude::ClaudeBackend::default()),
        Box::new(codex::CodexBackend::default()),
    ];
    backends.extend(
        opencode::OpenCodeBackend::defaults()
            .into_iter()
            .map(|backend| Box::new(backend) as Box<dyn Backend>),
    );
    backends.push(Box::new(pi::PiBackend::default()));
    backends
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

/// Parse the opening of a JSONL file. Every harness writes what it knows
/// about a session at the top, and `read_jsonl` reads the *end* of a file, so
/// a large transcript loses its own header without this.
pub(crate) fn head_jsonl(path: &Path) -> Vec<Value> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file.take(HEAD_PROBE_BYTES).read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    // A read that filled the window stopped somewhere inside a line, so the
    // remainder after the last newline is a fragment. A shorter read reached
    // the end of the file, where a final line without a trailing newline is
    // whole.
    let complete = if bytes.len() as u64 == HEAD_PROBE_BYTES {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(&[][..], |newline| &bytes[..newline])
    } else {
        &bytes
    };
    complete
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

/// The working directory a session recorded, read from the file's opening
/// alone. `pick` is the harness's own answer to "where is the cwd on this
/// line", tried against each line in turn.
pub(crate) fn head_directory(
    path: &Path,
    pick: impl Fn(&Value) -> Option<&str>,
) -> Option<PathBuf> {
    head_jsonl(path).iter().find_map(pick).map(PathBuf::from)
}

/// Walk mtime-ordered candidates newest first, keeping those the query accepts
/// for scope and metadata, until the limit is filled or the ceiling is reached.
///
/// `probe` answers "which directory was this recorded in" cheaply, and is only
/// called when a scope needs the answer. It must answer with the same rule
/// `parse` uses, so that a candidate it places outside the scope is one the
/// full read would place there too — otherwise skipping would lose sessions.
/// A candidate it cannot place at all is parsed and judged on what the full
/// read reports, so a probe that misses costs time and never a session.
pub(crate) fn list_files(
    files: Vec<PathBuf>,
    query: &Query,
    probe: impl Fn(&Path) -> Option<PathBuf>,
    parse: impl Fn(&Path) -> Option<Session>,
) -> Listing {
    let mut listing = Listing::default();
    for path in files {
        if listing.sessions.len() >= query.limit {
            return listing;
        }
        if listing.scanned >= query.ceiling {
            listing.scan_truncated = true;
            return listing;
        }
        listing.scanned += 1;
        if let Some(scope) = query.scope {
            if probe(&path).is_some_and(|directory| !scope.contains(&directory)) {
                continue;
            }
        }
        let Some(session) = parse(&path) else {
            continue;
        };
        let placed = query.scope.is_none_or(|scope| {
            session
                .directory
                .as_deref()
                .is_some_and(|directory| scope.contains(directory))
        });
        if placed && query.matches(&session) {
            listing.sessions.push(session);
        }
    }
    listing
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

/// Report the final verified record after the last record that rendered a
/// turn. A newer unrecognized record suppresses an older candidate: naming the
/// older one would misidentify the store's actual ending.
pub(crate) fn trailing_record<'a, I, R, K>(
    values: I,
    renders_turn: R,
    known_kind: K,
) -> Option<TrailingRecord>
where
    I: IntoIterator<Item = &'a Value>,
    R: Fn(&Value) -> bool,
    K: Fn(&Value) -> Option<&'static str>,
{
    let values = values.into_iter().collect::<Vec<_>>();
    let last_turn = values.iter().rposition(|value| renders_turn(value))?;
    let value = values.get(last_turn + 1..)?.last().copied()?;
    Some(TrailingRecord {
        kind: known_kind(value)?.to_owned(),
        timestamp: timestamp(&value["timestamp"]),
    })
}

pub(crate) fn transcript(
    session: Session,
    mut turns: Vec<Turn>,
    tail: usize,
    read: &Jsonl,
    trailing_record: Option<TrailingRecord>,
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
        trailing_record,
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
                        .and_then(|prefix| prefix.chars().next_back())
                        .is_some_and(|separator| !separator.is_alphanumeric())
            })
    })
}
