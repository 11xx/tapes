use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::event::{self, EventTranscript};
use crate::lineage::Lineage;
use crate::model::{
    Accounting, AccountingBasis, AccountingCoverage, Cost, Session, SourceBound, Tokens,
    TrailingRecord, Transcript, Truncation, Turn,
};
use crate::scope::Scope;

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;

const MAX_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024;
/// Keep a content search from monopolizing a large machine while still
/// allowing independent file reads to overlap.
const MAX_SEARCH_WORKERS: usize = 8;
/// Enough of a file's opening to carry any harness's session header, and
/// small enough that probing a whole store stays cheap.
const HEAD_PROBE_BYTES: u64 = 64 * 1024;
/// How far the head probe will grow when the first line alone is longer than
/// the probe, as a Claude record carrying a large pasted prompt can be. A
/// first line longer than this leaves the opening empty.
const HEAD_PROBE_MAX_BYTES: u64 = 1024 * 1024;

/// Accumulates only counters the source actually wrote. A zero is retained as
/// a present value, while a missing field remains absent in the result.
#[derive(Default)]
pub(crate) struct TokenTotals {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

impl TokenTotals {
    pub(crate) fn add(
        &mut self,
        input: Option<u64>,
        output: Option<u64>,
        reasoning: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
    ) {
        add_counter(&mut self.input, input);
        add_counter(&mut self.output, output);
        add_counter(&mut self.reasoning, reasoning);
        add_counter(&mut self.cache_read, cache_read);
        add_counter(&mut self.cache_write, cache_write);
    }

    pub(crate) fn finish(self) -> Option<Tokens> {
        let has_counter = self.input.is_some()
            || self.output.is_some()
            || self.reasoning.is_some()
            || self.cache_read.is_some()
            || self.cache_write.is_some();
        has_counter.then_some(Tokens {
            input: self.input,
            output: self.output,
            reasoning: self.reasoning,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
        })
    }
}

fn add_counter(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some((*total).unwrap_or_default().saturating_add(value));
    }
}

pub(crate) fn accounting_for(
    tokens: Option<&Tokens>,
    cost: Option<&Cost>,
    basis: AccountingBasis,
    coverage: AccountingCoverage,
) -> Option<Accounting> {
    (tokens.is_some() || cost.is_some()).then_some(Accounting { basis, coverage })
}

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
    /// Lower bound for the session's newest recorded activity, inclusive.
    pub since: Option<DateTime<Utc>>,
    /// Upper bound for the session's newest recorded activity, exclusive.
    pub until: Option<DateTime<Utc>>,
}

impl<'a> Query<'a> {
    pub fn unscoped(limit: usize) -> Self {
        Self {
            scope: None,
            limit,
            ceiling: usize::MAX,
            model: None,
            directory: None,
            since: None,
            until: None,
        }
    }

    pub fn unscoped_with_filters(
        limit: usize,
        model: Option<&str>,
        directory: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            model: model.map(str::to_lowercase),
            directory: directory.map(str::to_lowercase),
            since,
            until,
            ..Self::unscoped(limit)
        }
    }

    pub(crate) fn scoped_with_filters(
        scope: Option<&'a Scope>,
        limit: usize,
        ceiling: usize,
        model: Option<&str>,
        directory: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            scope,
            limit,
            ceiling,
            model: model.map(str::to_lowercase),
            directory: directory.map(str::to_lowercase),
            since,
            until,
        }
    }

    pub(crate) fn has_filters(&self) -> bool {
        self.model.is_some()
            || self.directory.is_some()
            || self.since.is_some()
            || self.until.is_some()
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
        let since_matches = self
            .since
            .is_none_or(|since| session.last_activity_at >= since);
        let until_matches = self
            .until
            .is_none_or(|until| session.last_activity_at < until);
        model_matches && directory_matches && since_matches && until_matches
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
    /// Candidates whose bounded content search could not be answered, plus
    /// diagnostics from a search stage that had to fall back.
    pub unsearched: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
}

impl Listing {
    pub fn from_sessions(sessions: Vec<Session>) -> Self {
        Self {
            scanned: sessions.len(),
            sessions,
            unavailable: Vec::new(),
            unsearched: Vec::new(),
            scan_truncated: false,
        }
    }
}

pub trait Backend {
    fn child_transcript(&self, _parent: &Session, _reference: &str) -> Result<Transcript> {
        anyhow::bail!(
            "{} does not support child-qualified transcript reads",
            self.harness()
        )
    }
    fn harness(&self) -> &'static str;
    /// Report whether listing is likely to work for this backend.
    ///
    /// This is an advisory listing hint, not a precondition for other
    /// methods. Exact-id resolution deliberately skips it, and callers may
    /// still invoke `locate` or `transcript` when it returns `false` so those
    /// methods can report their own absence or failure.
    fn available(&self) -> bool;
    fn list(&self, query: &Query) -> Result<Listing>;
    /// Enumerate title evidence with bounded candidate discovery and explicit gaps.
    fn list_titles(&self, query: &Query) -> Result<Listing> {
        self.list(query)
    }
    /// List sessions after a bounded content search. File-backed backends can
    /// override this to search while their existing candidate parse is open;
    /// the default reuses the backend's bounded transcript path.
    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        Ok(filter_listing_search(self, self.list(query)?, needle, tail))
    }
    /// Locate one session by its exact id without enumerating the store.
    /// `Ok(None)` means this backend does not hold it. Resolution calls this
    /// before it calls `list`, so an exact id never pays for a listing.
    fn locate(&self, id: &str) -> Result<Option<Session>>;
    /// Read a transcript for a session already resolved by this backend.
    /// Implementations must use the supplied normalized session rather than
    /// locating it again; the transcript read may still need to open the
    /// underlying record to collect turns.
    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript>;
    /// Project tool events after parsing the whole source read but before its
    /// turn window is applied. Paged backends override this so the supplied
    /// tail also bounds how many source pages are fetched.
    fn events(&self, session: &Session, tail: usize) -> Result<EventTranscript> {
        Ok(event::project(self.transcript(session, usize::MAX)?, tail))
    }
    /// The relationships this session's store records for it. The default is
    /// the answer for a harness that records none: a backend reports only
    /// what a record names, and never reads a child's turns.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let _ = session;
        Ok(Lineage::default())
    }
    /// Search only the bounded tail requested by a listing. The default keeps
    /// this path aligned with each backend's existing transcript reader, so a
    /// backend cannot accidentally grow a second unbounded parser for search.
    fn search(&self, session: &Session, needle: &str, tail: usize) -> Result<bool> {
        search_turns(&self.transcript(session, tail)?, needle, tail)
    }
}

/// Whether the searched tail contains the needle. A hit anywhere in the read
/// is a match. A miss is a non-match only when the read covered the tail it
/// was asked to search: fewer turns than that behind a bound that withheld
/// turns means the answer is unknown, and an unknown reported as a miss would
/// hide exactly the sessions a search exists to find. A bound that only cut
/// text within turns the read did reach leaves the turn count whole.
pub(crate) fn search_turns(transcript: &Transcript, needle: &str, tail: usize) -> Result<bool> {
    let needle = needle.to_lowercase();
    if transcript
        .turns
        .iter()
        .any(|turn| turn.text.to_lowercase().contains(&needle))
    {
        return Ok(true);
    }
    let turns_withheld = transcript.truncation.source.iter().any(|bound| {
        matches!(
            bound,
            SourceBound::FileTail { .. } | SourceBound::RecordPage { .. }
        )
    });
    if transcript.turns.len() < tail && turns_withheld {
        anyhow::bail!(
            "the bounded read reached {} of the last {tail} turns, so a miss is not a non-match",
            transcript.turns.len()
        );
    }
    Ok(false)
}

pub(crate) fn filter_listing_search<B: Backend + ?Sized>(
    backend: &B,
    mut listing: Listing,
    needle: &str,
    tail: usize,
) -> Listing {
    let mut matching = Vec::new();
    for session in listing.sessions {
        match backend.search(&session, needle, tail) {
            Ok(true) => matching.push(session),
            Ok(false) => {}
            Err(error) => listing.unsearched.push(format!(
                "{} session {}: {error:#}",
                backend.harness(),
                session.id
            )),
        }
    }
    listing.sessions = matching;
    listing
}

pub(crate) fn filter_listing_search_parallel<B: Backend + Sync + ?Sized>(
    backend: &B,
    mut listing: Listing,
    needle: &str,
    tail: usize,
) -> Listing {
    if listing.sessions.is_empty() {
        return listing;
    }
    let worker_count = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_SEARCH_WORKERS)
        .min(listing.sessions.len());
    let chunk_size = listing.sessions.len().div_ceil(worker_count);
    let sessions = std::mem::take(&mut listing.sessions);
    let searched = std::thread::scope(|scope| {
        let handles = sessions
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(|| {
                    chunk
                        .iter()
                        .map(|session| (session, backend.search(session, needle, tail)))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("content search worker panicked"))
            .collect::<Vec<_>>()
    });
    for (session, result) in searched {
        match result {
            Ok(true) => listing.sessions.push(session.clone()),
            Ok(false) => {}
            Err(error) => listing.unsearched.push(format!(
                "{} session {}: {error:#}",
                backend.harness(),
                session.id
            )),
        }
    }
    listing
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

pub(crate) struct ParsedFile {
    pub session: Session,
    pub turns: Vec<Turn>,
    /// Whether the read kept only the file's tail, so turns older than the
    /// ones here exist and were not read.
    pub truncated: bool,
}

/// What a bounded content search concluded about one file.
enum SearchOutcome {
    Matched(Box<Session>),
    Miss,
    /// The read reached fewer turns than it was asked to search and the file
    /// holds older ones, so a miss is unknown rather than a non-match. The
    /// session travels with the diagnostic so the listing's filters decide
    /// whether the candidate was asked about at all.
    Unsearched(Box<Session>, String),
}

/// A JSONL recording seen through two bounded windows. The tail holds the
/// newest content and decides truncation. The head holds what every harness
/// writes first, its session header, which a bounded tail loses on a large
/// file. Neither window grows with the file.
pub(crate) struct Recording {
    pub head: Vec<Value>,
    pub tail: Jsonl,
}

impl Recording {
    /// The file's first records: the head probe when the tail is truncated,
    /// the tail itself when it is the whole file. Header facts read from here
    /// follow the same rule a scoped listing's cheap probe uses, so a session
    /// the probe places outside a scope is one the full parse places there
    /// too, and skipping it can never lose it.
    pub fn opening(&self) -> &[Value] {
        if self.tail.truncated {
            &self.head
        } else {
            &self.tail.values
        }
    }

    /// The earliest and latest recorded timestamps across both windows. The
    /// start comes from the head whenever its records carry one, so a
    /// truncated tail never advances a session's start to its first retained
    /// record. The end comes from the tail, which is the end of the file.
    pub fn time_range(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match (time_range(&self.head), time_range(&self.tail.values)) {
            (Some((head_start, head_end)), Some((tail_start, tail_end))) => {
                Some((head_start.min(tail_start), head_end.max(tail_end)))
            }
            (head, tail) => head.or(tail),
        }
    }

    /// Whether the start is only the earliest record reached: the file is
    /// past the tail bound and its opening carried no timestamp, so the
    /// recorded start is behind the bound and unknown.
    pub fn start_uncertain(&self) -> bool {
        self.tail.truncated && time_range(&self.head).is_none()
    }
}

pub(crate) fn read_recording(path: &Path) -> Result<Recording> {
    let tail = read_jsonl(path)?;
    let head = if tail.truncated {
        head_jsonl(path)
    } else {
        Vec::new()
    };
    Ok(Recording { head, tail })
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

/// A negative result is safe only for plain ASCII JSON. JSON permits any
/// character to be written as a `\u` escape, and non-ASCII case folding can
/// change the characters a search sees, so those inputs stay candidates for
/// the normal parser. A raw hit is only a reason to parse; it is not a match.
fn raw_tail_may_contain(path: &Path, needle: &str) -> bool {
    if needle.is_empty() || !needle.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return true;
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return true;
    };
    let start = len.saturating_sub(MAX_TRANSCRIPT_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return true;
    }
    let mut bytes = Vec::new();
    if file
        .take(MAX_TRANSCRIPT_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return true;
    }
    if bytes.iter().any(|byte| *byte >= 0x80) || bytes.windows(2).any(|window| window == b"\\u") {
        return true;
    }
    bytes.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle.bytes())
            .all(|(byte, expected)| byte.eq_ignore_ascii_case(&expected))
    })
}

/// Parse the opening of a JSONL file. Every harness writes what it knows
/// about a session at the top, and `read_jsonl` reads the *end* of a file, so
/// a large transcript loses its own header without this.
pub(crate) fn head_jsonl(path: &Path) -> Vec<Value> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    // The probe grows only while it holds no complete line, so a store of
    // ordinary files costs one small read each and a file whose first record
    // outgrows the probe costs one larger read rather than a wrong answer.
    let mut bytes = Vec::new();
    let mut window = HEAD_PROBE_BYTES;
    loop {
        let wanted = window - bytes.len() as u64;
        if (&mut file).take(wanted).read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        let filled = bytes.len() as u64 == window;
        if !filled || bytes.contains(&b'\n') || window >= HEAD_PROBE_MAX_BYTES {
            break;
        }
        window = HEAD_PROBE_MAX_BYTES;
    }
    // A read that filled the window stopped somewhere inside a line, so the
    // remainder after the last newline is a fragment. A shorter read reached
    // the end of the file, where a final line without a trailing newline is
    // whole.
    let complete = if bytes.len() as u64 == window {
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
    list_files_inner(files, query, probe, |path| {
        parse(path).map(|session| ParsedFile {
            session,
            turns: Vec::new(),
            truncated: false,
        })
    })
}

/// Search file-backed candidates with a conservative raw prefilter, then retain
/// only candidates whose final normalized turns contain the needle. The file
/// parser already caps the bytes it reads, and `tail` caps the turns considered
/// by the content search. A file past the parser's bound is always parsed,
/// because only the parse can tell whether the retained tail holds enough
/// turns for a miss to mean anything; when it does not, the candidate is
/// reported as unsearched by the same rule `search_turns` applies. Worker
/// reads preserve candidate order before the caller applies scope and result
/// limits.
pub(crate) fn list_files_with_search(
    files: Vec<PathBuf>,
    query: &Query,
    needle: &str,
    tail: usize,
    probe: impl Fn(&Path) -> Option<PathBuf>,
    parse: impl Fn(&Path) -> Option<ParsedFile> + Sync,
) -> Listing {
    let needle = needle.to_lowercase();
    if query.limit == 0 {
        return Listing::default();
    }
    let candidate_count = files.len().min(query.ceiling);
    let scan_truncated = files.len() > candidate_count;
    let candidates = files
        .into_iter()
        .take(candidate_count)
        .filter(|path| {
            query
                .scope
                .is_none_or(|scope| probe(path).is_none_or(|directory| scope.contains(&directory)))
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Listing {
            scanned: candidate_count,
            scan_truncated,
            ..Listing::default()
        };
    }
    let worker_count = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_SEARCH_WORKERS)
        .min(candidates.len());
    let chunk_size = candidates.len().div_ceil(worker_count);
    let parsed = std::thread::scope(|scope| {
        let handles = candidates
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(|| {
                    chunk
                        .iter()
                        .map(|path| {
                            let oversized = fs::metadata(path)
                                .map_or(true, |metadata| metadata.len() > MAX_TRANSCRIPT_BYTES);
                            if !oversized && !raw_tail_may_contain(path, &needle) {
                                return SearchOutcome::Miss;
                            }
                            let Some(parsed) = parse(path) else {
                                return SearchOutcome::Miss;
                            };
                            let matched = parsed
                                .turns
                                .iter()
                                .rev()
                                .take(tail)
                                .any(|turn| turn.text.to_lowercase().contains(&needle));
                            if matched {
                                SearchOutcome::Matched(Box::new(parsed.session))
                            } else if parsed.truncated && parsed.turns.len() < tail {
                                let diagnostic = format!(
                                    "{} session {}: the bounded read reached {} of the last {tail} \
                                     turns, so a miss is not a non-match",
                                    parsed.session.harness,
                                    parsed.session.id,
                                    parsed.turns.len()
                                );
                                SearchOutcome::Unsearched(Box::new(parsed.session), diagnostic)
                            } else {
                                SearchOutcome::Miss
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("file search worker panicked"))
            .collect::<Vec<_>>()
    });
    let mut listing = Listing {
        scanned: candidate_count,
        scan_truncated,
        ..Listing::default()
    };
    // Scope and metadata filters decide which candidates the listing was
    // asked about; a match outside them is not returned, and an unsearched
    // candidate outside them is not reported either.
    for outcome in parsed {
        let (session, diagnostic) = match outcome {
            SearchOutcome::Matched(session) => (*session, None),
            SearchOutcome::Miss => continue,
            SearchOutcome::Unsearched(session, diagnostic) => (*session, Some(diagnostic)),
        };
        let placed = query.scope.is_none_or(|scope| {
            session
                .directory
                .as_deref()
                .is_some_and(|directory| scope.contains(directory))
        });
        if !placed || !query.matches(&session) {
            continue;
        }
        match diagnostic {
            Some(diagnostic) => listing.unsearched.push(diagnostic),
            None if listing.sessions.len() < query.limit => listing.sessions.push(session),
            None => {}
        }
    }
    listing
}

fn list_files_inner(
    files: Vec<PathBuf>,
    query: &Query,
    probe: impl Fn(&Path) -> Option<PathBuf>,
    parse: impl Fn(&Path) -> Option<ParsedFile>,
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
        let Some(parsed) = parse(&path) else {
            continue;
        };
        let session = parsed.session;
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
pub(crate) fn trailing_record<'a, I, K>(
    values: I,
    last_turn: Option<usize>,
    known_kind: K,
) -> Option<TrailingRecord>
where
    I: IntoIterator<Item = &'a Value>,
    K: Fn(&Value) -> Option<&'static str>,
{
    let values = values.into_iter().collect::<Vec<_>>();
    let last_turn = last_turn?;
    let value = values.get(last_turn + 1..)?.last().copied()?;
    Some(TrailingRecord {
        kind: known_kind(value)?.to_owned(),
        timestamp: timestamp(&value["timestamp"]),
    })
}

/// What a bounded file read leaves unread, in the transcript's own terms.
pub(crate) fn read_bounds(read: &Jsonl) -> Truncation {
    Truncation {
        window: None,
        source: read
            .truncated
            .then_some(SourceBound::FileTail {
                bytes: MAX_TRANSCRIPT_BYTES,
            })
            .into_iter()
            .collect(),
    }
}

pub(crate) fn transcript(
    session: Session,
    mut turns: Vec<Turn>,
    tail: usize,
    read: &Jsonl,
    trailing_record: Option<TrailingRecord>,
    mut notes: Vec<String>,
) -> Transcript {
    let total = turns.len();
    for (ordinal, turn) in turns.iter_mut().enumerate() {
        turn.ordinal = ordinal;
    }
    if total > tail {
        turns.drain(..total - tail);
    }
    if read.skipped > 0 {
        let noun = if read.skipped == 1 { "line" } else { "lines" };
        notes.push(format!("Skipped {} unparseable {noun}.", read.skipped));
    }
    let truncation = Truncation {
        window: Truncation::window(turns.len(), total, tail),
        source: read
            .truncated
            .then_some(SourceBound::FileTail {
                bytes: MAX_TRANSCRIPT_BYTES,
            })
            .into_iter()
            .collect(),
    };

    Transcript::new(session, turns, truncation, trailing_record, notes)
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn raw_prefilter_skips_a_definite_miss_but_not_a_hit() {
        let path = std::env::temp_dir().join(format!("tapes-raw-prefilter-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        fs::write(&path, br#"{"text":"other"}"#).unwrap();
        assert!(!raw_tail_may_contain(&path, "needle"));

        fs::write(&path, br#"{"text":"NEEDLE"}"#).unwrap();
        assert!(raw_tail_may_contain(&path, "needle"));

        fs::write(&path, br#"{"text":"\u006e\u0065\u0065\u0064\u006c\u0065"}"#).unwrap();
        assert!(raw_tail_may_contain(&path, "needle"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn list_search_does_not_parse_a_raw_definite_miss() {
        let path =
            std::env::temp_dir().join(format!("tapes-raw-prefilter-list-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        fs::write(&path, br#"{"text":"other"}"#).unwrap();
        let parsed = AtomicBool::new(false);
        let listing = list_files_with_search(
            vec![path.clone()],
            &Query::unscoped(10),
            "needle",
            32,
            |_| None,
            |_| {
                parsed.store(true, Ordering::Relaxed);
                None
            },
        );

        assert_eq!(listing.scanned, 1);
        assert!(listing.sessions.is_empty());
        assert!(!parsed.load(Ordering::Relaxed));
        fs::remove_file(path).unwrap();
    }
}
