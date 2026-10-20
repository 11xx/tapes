//! The `tapes-attribution-evidence/1` format: a JSON Lines file in which any
//! tool states what it knows about which sessions caused which work.
//!
//! The first line is a header naming the schema and the producer. Every later
//! line is a record whose `kind` says what it states:
//!
//! - `lineage`: a session launched another;
//! - `binding`: a session's consumption served a piece of work.
//!
//! A reader keeps what is valid, counts what is not, and ignores what it does
//! not know: an unknown field is ignored, an unknown `kind` is counted by
//! kind, a malformed line is counted with its line number and a fixed reason
//! and never its text. Identical records count once. A file whose first line
//! is not a header of this schema is refused, naming the file. Reads are
//! bounded: a line longer than [`MAX_LINE_BYTES`], or more than [`MAX_LINES`]
//! lines after the header, stops the read of that file and is reported as a
//! truncation.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::backend::open_regular_file;

pub const SCHEMA: &str = "tapes-attribution-evidence/1";

/// The longest line a file may hold. A binding that names a thousand commits
/// is about 70 KiB.
pub const MAX_LINE_BYTES: usize = 256 * 1024;

/// The most lines, blank ones included, read after the header. It matches the
/// most members a supplied input may hold.
pub const MAX_LINES: usize = 10_000;

/// The most `*.jsonl` files a directory may hold.
pub const MAX_DIRECTORY_FILES: usize = 1_000;

/// The longest informational string kept; a longer one is cut.
const MAX_TEXT_CHARS: usize = 512;

/// The longest unknown `kind` kept as a key of the unknown-kind counts.
const MAX_KIND_CHARS: usize = 64;

/// A session by harness and that harness's own session id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct SessionRef {
    pub harness: String,
    pub id: String,
}

/// How a producer stands behind a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// The producer itself launched the child and recorded both sessions.
    Bound,
    /// The relation is asserted without such a record.
    Declared,
}

/// A session launched another.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Lineage {
    pub parent: SessionRef,
    pub child: SessionRef,
    pub tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// The work a binding names.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Work {
    /// Full object names, lowercase.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
}

/// The part of a session that served the work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Span {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// A session's consumption served a piece of work.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Binding {
    pub session: SessionRef,
    pub work: Work,
    pub tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

/// Why a line is malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Malformed {
    /// The line is not a JSON object.
    NotAnObject,
    /// A required field is absent, empty or not of its type.
    MissingField,
    UnknownTier,
    ChildIsParent,
    /// An instant is not RFC 3339.
    InvalidInstant,
    /// `commits` is empty or holds a value that is not a full object name.
    InvalidCommits,
    /// A span's `from` is later than its `to`.
    SpanReversed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MalformedLine {
    pub line: usize,
    pub reason: Malformed,
}

/// Why a read of one file stopped before its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TruncationReason {
    /// The line is longer than [`MAX_LINE_BYTES`].
    LineTooLong,
    /// The file holds more than [`MAX_LINES`] lines after the header.
    TooManyLines,
}

/// Where a read of one file stopped: the first line it did not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Truncation {
    pub line: usize,
    pub reason: TruncationReason,
}

/// How many records of one kind a file held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct KindCount {
    /// Valid records, duplicates included.
    pub read: usize,
    /// Distinct valid records.
    pub used: usize,
}

/// What a read of one file found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileReport {
    /// The file as the caller named it, or the directory the caller named
    /// joined with the file's name.
    pub path: String,
    pub producer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written_at: Option<DateTime<Utc>>,
    pub lineage: KindCount,
    pub binding: KindCount,
    pub malformed: Vec<MalformedLine>,
    /// Records of a kind this reader does not know, by kind.
    pub unknown_kinds: BTreeMap<String, usize>,
    /// Records identical to an earlier one in the file.
    pub duplicates: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Truncation>,
}

/// One file read: its report and the distinct records it holds in file order.
#[derive(Clone, Debug)]
pub struct EvidenceFile {
    pub report: FileReport,
    pub lineage: Vec<Lineage>,
    pub bindings: Vec<Binding>,
}

/// Why a path named as evidence is refused.
#[derive(Debug)]
pub enum EvidenceError {
    /// The path does not exist.
    Missing { path: String },
    /// The path or a file under it could not be read.
    Unreadable {
        path: String,
        kind: std::io::ErrorKind,
    },
    /// The first line is not a header of this schema.
    NotEvidence { path: String, why: HeaderFault },
    /// A directory holds more evidence files than are read.
    TooManyFiles { path: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderFault {
    Empty,
    TooLong,
    NotAnObject,
    WrongSchema,
    NoProducer,
    InvalidWrittenAt,
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { path } => write!(f, "evidence path {path} does not exist"),
            Self::Unreadable { path, kind } => {
                write!(f, "evidence path {path} could not be read ({kind:?})")
            }
            Self::NotEvidence { path, why } => write!(
                f,
                "{path} is not a {SCHEMA} file: its first line {}",
                match why {
                    HeaderFault::Empty => "is missing",
                    HeaderFault::TooLong => "is longer than the line bound",
                    HeaderFault::NotAnObject => "is not a JSON object",
                    HeaderFault::WrongSchema => "does not name the schema",
                    HeaderFault::NoProducer => "names no producer",
                    HeaderFault::InvalidWrittenAt => "has a written_at that is not RFC 3339",
                }
            ),
            Self::TooManyFiles { path } => write!(
                f,
                "evidence directory {path} holds more than {MAX_DIRECTORY_FILES} .jsonl files"
            ),
        }
    }
}

impl std::error::Error for EvidenceError {}

/// Read each path named: a file is one evidence file, a directory is every
/// `*.jsonl` file directly inside it in name order. The files come back in
/// the order the paths were given.
pub fn read_paths(paths: &[PathBuf]) -> Result<Vec<EvidenceFile>, EvidenceError> {
    let mut files = Vec::new();
    for path in paths {
        let shown = path.display().to_string();
        let metadata = std::fs::metadata(path).map_err(|error| io_error(&shown, &error))?;
        if metadata.is_dir() {
            for member in directory_files(path, &shown)? {
                files.push(read_file(&member)?);
            }
        } else {
            files.push(read_file(path)?);
        }
    }
    Ok(files)
}

fn io_error(path: &str, error: &std::io::Error) -> EvidenceError {
    if error.kind() == std::io::ErrorKind::NotFound {
        EvidenceError::Missing {
            path: path.to_owned(),
        }
    } else {
        EvidenceError::Unreadable {
            path: path.to_owned(),
            kind: error.kind(),
        }
    }
}

fn directory_files(directory: &Path, shown: &str) -> Result<Vec<PathBuf>, EvidenceError> {
    let entries = std::fs::read_dir(directory).map_err(|error| io_error(shown, &error))?;
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error(shown, &error))?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && path.is_file()
        {
            found.push(path);
            if found.len() > MAX_DIRECTORY_FILES {
                return Err(EvidenceError::TooManyFiles {
                    path: shown.to_owned(),
                });
            }
        }
    }
    found.sort();
    Ok(found)
}

enum Next {
    End,
    Line,
    TooLong,
}

/// One line without its newline into `line`. A line longer than the bound is
/// not read to its end.
fn next_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<Next> {
    line.clear();
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', line)?;
    if read == 0 {
        return Ok(Next::End);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        return Ok(Next::Line);
    }
    Ok(if line.len() > MAX_LINE_BYTES {
        Next::TooLong
    } else {
        Next::Line
    })
}

/// Read one evidence file.
pub fn read_file(path: &Path) -> Result<EvidenceFile, EvidenceError> {
    let shown = path.display().to_string();
    let file = open_regular_file(path).map_err(|error| io_error(&shown, &error))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let refuse = |why| EvidenceError::NotEvidence {
        path: shown.clone(),
        why,
    };
    match next_line(&mut reader, &mut line).map_err(|error| io_error(&shown, &error))? {
        Next::End => return Err(refuse(HeaderFault::Empty)),
        Next::TooLong => return Err(refuse(HeaderFault::TooLong)),
        Next::Line => {}
    }
    let (producer, written_at) = parse_header(&line).map_err(refuse)?;
    let mut read = EvidenceFile {
        report: FileReport {
            path: shown.clone(),
            producer,
            written_at,
            lineage: KindCount::default(),
            binding: KindCount::default(),
            malformed: Vec::new(),
            unknown_kinds: BTreeMap::new(),
            duplicates: 0,
            truncated: None,
        },
        lineage: Vec::new(),
        bindings: Vec::new(),
    };
    let mut pairs: HashSet<(SessionRef, SessionRef)> = HashSet::new();
    let mut bindings: HashSet<Binding> = HashSet::new();
    let mut number = 1usize;
    let mut after_header = 0usize;
    loop {
        number += 1;
        match next_line(&mut reader, &mut line).map_err(|error| io_error(&shown, &error))? {
            Next::End => break,
            Next::TooLong => {
                read.report.truncated = Some(Truncation {
                    line: number,
                    reason: TruncationReason::LineTooLong,
                });
                break;
            }
            Next::Line => {}
        }
        if after_header >= MAX_LINES {
            read.report.truncated = Some(Truncation {
                line: number,
                reason: TruncationReason::TooManyLines,
            });
            break;
        }
        after_header += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse_record(&line) {
            Ok(Parsed::Lineage(record)) => {
                read.report.lineage.read += 1;
                if pairs.insert((record.parent.clone(), record.child.clone())) {
                    read.report.lineage.used += 1;
                    read.lineage.push(record);
                } else {
                    read.report.duplicates += 1;
                }
            }
            Ok(Parsed::Binding(record)) => {
                read.report.binding.read += 1;
                if bindings.insert(record.clone()) {
                    read.report.binding.used += 1;
                    read.bindings.push(record);
                } else {
                    read.report.duplicates += 1;
                }
            }
            Ok(Parsed::Unknown(kind)) => {
                *read.report.unknown_kinds.entry(kind).or_default() += 1;
            }
            Err(reason) => read.report.malformed.push(MalformedLine {
                line: number,
                reason,
            }),
        }
    }
    Ok(read)
}

fn parse_header(line: &[u8]) -> Result<(String, Option<DateTime<Utc>>), HeaderFault> {
    let Ok(Value::Object(header)) = serde_json::from_slice::<Value>(line) else {
        return Err(HeaderFault::NotAnObject);
    };
    if header.get("schema").and_then(Value::as_str) != Some(SCHEMA) {
        return Err(HeaderFault::WrongSchema);
    }
    let producer = header
        .get("producer")
        .and_then(Value::as_str)
        .filter(|producer| !producer.trim().is_empty())
        .ok_or(HeaderFault::NoProducer)?;
    let written_at = match header.get("written_at") {
        None | Some(Value::Null) => None,
        Some(value) => Some(instant(value).ok_or(HeaderFault::InvalidWrittenAt)?),
    };
    Ok((text(producer), written_at))
}

enum Parsed {
    Lineage(Lineage),
    Binding(Binding),
    Unknown(String),
}

fn parse_record(line: &[u8]) -> Result<Parsed, Malformed> {
    let Ok(Value::Object(record)) = serde_json::from_slice::<Value>(line) else {
        return Err(Malformed::NotAnObject);
    };
    let kind = record
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(Malformed::MissingField)?;
    match kind {
        "lineage" => parse_lineage(&record).map(Parsed::Lineage),
        "binding" => parse_binding(&record).map(Parsed::Binding),
        other => Ok(Parsed::Unknown(
            other.chars().take(MAX_KIND_CHARS).collect(),
        )),
    }
}

fn parse_lineage(record: &Map<String, Value>) -> Result<Lineage, Malformed> {
    let parent = session_ref(record.get("parent")).ok_or(Malformed::MissingField)?;
    let child = session_ref(record.get("child")).ok_or(Malformed::MissingField)?;
    let tier = parse_tier(record)?;
    if child == parent {
        return Err(Malformed::ChildIsParent);
    }
    Ok(Lineage {
        parent,
        child,
        tier,
        at: optional_instant(record.get("at"))?,
        label: optional_text(record.get("label")),
        subject: optional_text(record.get("subject")),
    })
}

fn parse_binding(record: &Map<String, Value>) -> Result<Binding, Malformed> {
    let session = session_ref(record.get("session")).ok_or(Malformed::MissingField)?;
    let Some(Value::Object(work)) = record.get("work") else {
        return Err(Malformed::MissingField);
    };
    let commits = match work.get("commits") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(commits)) => parse_commits(commits)?,
        Some(_) => return Err(Malformed::InvalidCommits),
    };
    let label = optional_text(work.get("label"));
    if commits.is_empty() && label.is_none() {
        return Err(Malformed::MissingField);
    }
    let tier = parse_tier(record)?;
    let span = match record.get("span") {
        None | Some(Value::Null) => None,
        Some(Value::Object(span)) => {
            let edge = |name| match span.get(name) {
                None | Some(Value::Null) => Err(Malformed::MissingField),
                Some(value) => instant(value).ok_or(Malformed::InvalidInstant),
            };
            let (from, to) = (edge("from")?, edge("to")?);
            if from > to {
                return Err(Malformed::SpanReversed);
            }
            Some(Span { from, to })
        }
        Some(_) => return Err(Malformed::MissingField),
    };
    Ok(Binding {
        session,
        work: Work {
            commits,
            label,
            repository: optional_text(work.get("repository")),
        },
        tier,
        role: optional_text(record.get("role")),
        span,
    })
}

fn parse_commits(commits: &[Value]) -> Result<Vec<String>, Malformed> {
    if commits.is_empty() {
        return Err(Malformed::InvalidCommits);
    }
    commits
        .iter()
        .map(|commit| {
            commit
                .as_str()
                .filter(|name| {
                    matches!(name.len(), 40 | 64)
                        && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .map(str::to_ascii_lowercase)
                .ok_or(Malformed::InvalidCommits)
        })
        .collect()
}

fn parse_tier(record: &Map<String, Value>) -> Result<Tier, Malformed> {
    match record.get("tier").and_then(Value::as_str) {
        None => Err(Malformed::MissingField),
        Some("bound") => Ok(Tier::Bound),
        Some("declared") => Ok(Tier::Declared),
        Some(_) => Err(Malformed::UnknownTier),
    }
}

fn session_ref(value: Option<&Value>) -> Option<SessionRef> {
    let Value::Object(reference) = value? else {
        return None;
    };
    let field = |name| {
        reference
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Some(SessionRef {
        harness: field("harness")?,
        id: field("id")?,
    })
}

fn instant(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|instant| instant.with_timezone(&Utc))
}

fn optional_instant(value: Option<&Value>) -> Result<Option<DateTime<Utc>>, Malformed> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => instant(value).map(Some).ok_or(Malformed::InvalidInstant),
    }
}

/// An informational string: kept when non-empty, cut when long, absent when
/// it is anything else.
fn optional_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(text)
}

fn text(value: &str) -> String {
    value.chars().take(MAX_TEXT_CHARS).collect()
}
