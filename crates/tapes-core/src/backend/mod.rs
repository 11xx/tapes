use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::event::{self, EventTranscript};
use crate::lineage::Lineage;
use crate::model::{
    Accounting, AccountingBasis, AccountingCoverage, ByteSpan, Cost, ReadEvidence, ReadGap,
    ReadRange, ReadRangeKind, RecordRef, Session, SourceBound, TerminalObservation,
    TextTailEvidence, Tokens, TrailingRecord, Transcript, TranscriptEvidence, Truncation, Turn,
};
use crate::scope::Scope;
use tapes_discovery::{
    CandidatePage as NativeCandidatePage, Discovery, DiscoveryError, Harness as NativeHarness,
    IdentityRecord, IdentitySource, NativeSession, NativeStore,
};

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;

/// How much of a recording's end a transcript read takes unless the caller
/// sets another bound.
pub const DEFAULT_READ_BYTES: u64 = 4 * 1024 * 1024;
/// The narrowest read bound a caller may set: the opening probe's own size.
pub const MIN_READ_BYTES: u64 = HEAD_PROBE_BYTES;
/// The widest read bound a caller may set. The window is held in memory
/// whole, so it stays finite.
pub const MAX_READ_BYTES: u64 = 1024 * 1024 * 1024;
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
/// The longest single record a whole-recording read decodes. A longer record
/// is skipped to its end and reported as a gap, so one runaway line cannot
/// hold the file in memory.
pub const FULL_RECORD_BYTES: u64 = 64 * 1024 * 1024;

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
    /// The order the caller presents. A backend holding its whole candidate
    /// set applies it before `limit` and continuation.
    pub sort: crate::ListSort,
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
            sort: crate::ListSort::Newest,
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
            sort: crate::ListSort::Newest,
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
        let since_matches = self.since.is_none_or(|since| {
            session
                .last_activity_at
                .is_some_and(|activity| activity >= since)
        });
        let until_matches = self.until.is_none_or(|until| {
            session
                .last_activity_at
                .is_some_and(|activity| activity < until)
        });
        model_matches && directory_matches && since_matches && until_matches
    }
}

/// What a listing found, and how hard it looked. `scanned` is candidates
/// inspected, not sessions returned: a caller reading an empty scoped listing
/// needs to know whether the store was exhausted or the search stopped.
#[derive(Debug, Default)]
pub struct Listing {
    pub sessions: Vec<Session>,
    /// Artifact-native evidence discovered alongside supplied sessions. Most
    /// installed backends never populate this collection.
    pub artifacts: Vec<crate::content::ArtifactReference>,
    /// Diagnostics for individual candidates that could not be normalized.
    /// These use the same vocabulary as the public listing's `unavailable`
    /// field, while a command or store failure still names the whole harness.
    pub unavailable: Vec<String>,
    /// The session ids `unavailable` describes, for the diagnostics that name
    /// one. A harness whose stores answer the same ids keeps a later store's
    /// projection from standing in for a session an earlier store holds and
    /// could not read.
    pub unavailable_ids: Vec<String>,
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
            artifacts: Vec::new(),
            unavailable: Vec::new(),
            unavailable_ids: Vec::new(),
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

    fn history_page(
        &self,
        _session: &Session,
        _cursor: Option<&str>,
        _bytes: usize,
    ) -> Result<crate::history::Page> {
        anyhow::bail!("{} does not support file history pages", self.harness())
    }
    /// Read model observations without decoding transcript turns when the backend supports it.
    fn metadata_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> Result<crate::history::Page> {
        self.history_page(session, cursor, bytes)
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
    /// Locate one supplied-source occurrence without treating its native id
    /// as globally unique. Installed stores have no occurrence coordinate.
    fn locate_occurrence(&self, _occurrence: &str) -> Result<Option<Session>> {
        Ok(None)
    }
    /// Whether an exact lookup failure must remain terminal instead of
    /// falling through to prefix enumeration. Supplied sources use this to
    /// preserve occurrence ambiguity and identity-coverage failures.
    fn terminal_exact_failure(&self) -> bool {
        false
    }
    /// Read a transcript for a session already resolved by this backend.
    /// Implementations must use the supplied normalized session rather than
    /// locating it again; the transcript read may still need to open the
    /// underlying record to collect turns.
    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript>;
    /// Read the whole recording in order, handing each normalized turn to
    /// `turn` as it is produced, so memory follows one record rather than the
    /// file. The default refuses: a backend without a streamed reader must not
    /// answer with a bounded one.
    ///
    /// `replay` is an earlier read of the same session. Given one, the read
    /// covers exactly the source that read observed and hands over the same
    /// turns with the same record references, so a consumer can stream a
    /// recording twice and join the passes; a source that can no longer be
    /// read that way refuses.
    fn stream_transcript(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let _ = (session, replay, turn);
        anyhow::bail!(
            "{} sessions cannot be read whole; their reader keeps a bounded tail",
            self.harness()
        )
    }
    /// Whether another installed store of this harness answers the same
    /// session ids with its own projection of them, so listing and resolution
    /// must pick one store's answer per id.
    fn shares_session_ids(&self) -> bool {
        false
    }
    /// The store this backend reads, where its harness has more than one, as
    /// a diagnostic names it.
    fn store(&self) -> Option<String> {
        None
    }
    /// Which turn kinds this harness's records can evidence, and the kind its
    /// reader gives a user turn that carries no evidence of its own.
    fn kinds(&self) -> crate::model::KindDeclaration;
    /// Whether this session can be streamed twice with the second read
    /// handing over the first read's turns. A consumer that joins two passes
    /// asks before the first, so a source that cannot be replayed refuses
    /// without being read. The default can be.
    fn replayable(&self, session: &Session) -> Result<()> {
        let _ = session;
        Ok(())
    }
    /// The session as every record of the recording `read` covered states it:
    /// counters, accounting, recorded usage detail, model, and activity range
    /// folded from the whole recording rather than its bounded tail. The read
    /// is replayed as `stream_transcript` replays it, so the session and the
    /// turns of `read` describe the same records. The default refuses, as
    /// `stream_transcript` does.
    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        let _ = (session, read);
        anyhow::bail!(
            "{} sessions cannot be read whole; their reader keeps a bounded tail",
            self.harness()
        )
    }
    /// Read one backend's accounting observations against the same bounded or
    /// pinned source evidence a caller already used. Ordinary usage never
    /// calls this hook, so a backend does not retain a per-observation
    /// collection unless the caller explicitly asks for one.
    fn usage_observations(
        &self,
        session: &Session,
        read: Option<&crate::model::ReadEvidence>,
        options: crate::usage::UsageObservationOptions,
    ) -> Result<crate::usage::UsageObservationResult> {
        let _ = (session, read, options);
        anyhow::bail!(
            "{} sessions do not support usage observations",
            self.harness()
        )
    }
    /// Every relationship the whole recording names, read record by record so
    /// memory follows the references kept rather than the file. The default
    /// refuses: a bounded lineage read is not an answer to a whole one.
    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        let _ = session;
        anyhow::bail!(
            "{} sessions cannot be read whole; their reader keeps a bounded tail",
            self.harness()
        )
    }
    /// Read a parent-qualified child's whole recording in order, handing each
    /// turn to `turn` as it is produced, and answer with the child session its
    /// records state. Every record carrying a native parent identity is
    /// checked against `parent`.
    fn stream_child_transcript(
        &self,
        parent: &Session,
        reference: &str,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedChild> {
        let _ = (parent, reference, turn);
        anyhow::bail!(
            "{} does not support whole child-qualified transcript reads",
            self.harness()
        )
    }
    /// Project tool events after parsing the whole source read but before its
    /// turn window is applied. Paged backends override this so the supplied
    /// tail also bounds how many source pages are fetched.
    fn events(&self, session: &Session, tail: usize) -> Result<EventTranscript> {
        self.events_with_options(session, tail, event::EventOptions::default())
    }
    /// Project tool events as `events` does, honoring the caller's options. A
    /// backend with its own `events` read overrides this instead, so an
    /// option is never silently dropped on one projection path.
    fn events_with_options(
        &self,
        session: &Session,
        tail: usize,
        options: event::EventOptions,
    ) -> Result<EventTranscript> {
        Ok(event::project_with(
            self.transcript(session, usize::MAX)?,
            tail,
            options,
        ))
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

impl IdentityRecord for Session {
    fn id(&self) -> &str {
        &self.id
    }

    fn harness(&self) -> &str {
        self.harness()
    }

    fn store_coordinate(&self) -> Option<&str> {
        self.locator()
    }
}

/// The core's rich backend adapted to the discovery crate's shared selector.
pub(crate) struct BackendIdentitySource<'a>(pub &'a dyn Backend);

impl IdentitySource<Session> for BackendIdentitySource<'_> {
    fn harness(&self) -> &str {
        self.0.harness()
    }

    fn locate_exact(&self, query: &str) -> std::result::Result<Option<Session>, String> {
        self.0.locate(query).map_err(|error| format!("{error:#}"))
    }

    fn candidates(&self, limit: usize) -> NativeCandidatePage<Session, String> {
        let mut query = Query::unscoped(limit);
        query.ceiling = limit;
        match self.0.list(&query) {
            Ok(listing) => {
                let failures = listing.unavailable;
                let complete = !listing.scan_truncated
                    && listing.sessions.len() < limit
                    && listing.unavailable_ids.is_empty()
                    && failures.is_empty();
                NativeCandidatePage {
                    records: listing.sessions,
                    scanned: listing.scanned,
                    visited_entries: listing.scanned,
                    complete,
                    failures,
                    unreadable_ids: listing.unavailable_ids,
                }
            }
            Err(error) => NativeCandidatePage {
                records: Vec::new(),
                scanned: 0,
                visited_entries: 0,
                complete: false,
                failures: vec![format!("{error:#}")],
                unreadable_ids: Vec::new(),
            },
        }
    }

    fn shares_session_ids(&self) -> bool {
        self.0.shares_session_ids()
    }

    fn terminal_exact_failure(&self) -> bool {
        self.0.terminal_exact_failure()
    }

    fn available(&self) -> bool {
        self.0.available()
    }
}

pub(crate) fn default_native_store(harness: NativeHarness) -> Option<NativeStore> {
    Discovery::from_env()
        .stores()
        .iter()
        .find(|store| store.harness() == harness)
        .cloned()
}

/// Keep every native identity decision in discovery and use the normalized
/// parser only to enrich the selected native record.
pub(crate) fn enrich_native_session(
    native: &NativeSession,
    mut session: Session,
) -> Result<Session> {
    if session.harness() != native.harness().as_str() {
        anyhow::bail!(
            "{} native identity {} was normalized as {}",
            native.harness(),
            native.id(),
            session.harness()
        );
    }
    session.id = native.id().to_owned();
    let native_path = native.locator().map(Path::to_path_buf);
    let locator = native_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| native.store_coordinate().to_owned());
    session.source.location = Some(crate::model::SourceLocation {
        locator,
        native_path,
        member: None,
        container: None,
    });
    Ok(session)
}

/// Resolve a session's file using the selected native path when discovery
/// supplied one, while preserving the existing locator semantics for inputs.
pub(crate) fn session_file_path(session: &Session) -> Option<&Path> {
    let location = session.source.location.as_ref()?;
    Some(
        location
            .native_path
            .as_deref()
            .unwrap_or_else(|| Path::new(&location.locator)),
    )
}

fn is_candidate_page_limit(error: &DiscoveryError) -> bool {
    matches!(
        error,
        DiscoveryError::BoundExhausted {
            bound: "candidate enumeration limit",
            ..
        }
    )
}

fn merge_discovery_coverage(
    listing: &mut Listing,
    failures: Vec<DiscoveryError>,
    unreadable_ids: Vec<String>,
    complete: bool,
    intentional_result_limit: bool,
) {
    let candidate_limit_only = intentional_result_limit
        && !complete
        && !failures.is_empty()
        && failures.iter().all(is_candidate_page_limit)
        && unreadable_ids.is_empty();
    for failure in failures {
        if intentional_result_limit && is_candidate_page_limit(&failure) {
            continue;
        }
        listing.unavailable.push(failure.to_string());
    }
    listing.unavailable_ids.extend(unreadable_ids);
    if !complete && !candidate_limit_only {
        listing.scan_truncated = true;
    }
}

pub(crate) fn list_discovered_files(
    store: &NativeStore,
    query: &Query,
    probe: impl Fn(&Path) -> Option<PathBuf>,
    parse: impl Fn(&NativeSession, &Path) -> Result<Session>,
) -> Listing {
    if query.limit == 0 {
        return Listing::default();
    }
    let intentional_result_limit =
        query.scope.is_none() && !query.has_filters() && query.ceiling > query.limit;
    let page = store.candidates(query.ceiling);
    let mut by_path = HashMap::with_capacity(page.records.len());
    let mut files = Vec::with_capacity(page.records.len());
    let mut no_path = Vec::new();
    for native in page.records {
        let Some(path) = native.locator().map(Path::to_path_buf) else {
            no_path.push(native.id().to_owned());
            continue;
        };
        files.push(path.clone());
        by_path.insert(path, native);
    }
    let parse_errors = std::cell::RefCell::new(Vec::<(String, String)>::new());
    let mut listing = list_files(files, query, probe, |path| {
        let native = by_path.get(path)?;
        match parse(native, path) {
            Ok(session) => match enrich_native_session(native, session) {
                Ok(session) => Some(session),
                Err(error) => {
                    parse_errors
                        .borrow_mut()
                        .push((native.id().to_owned(), format!("{}", error)));
                    None
                }
            },
            Err(error) => {
                parse_errors
                    .borrow_mut()
                    .push((native.id().to_owned(), format!("{error:#}")));
                None
            }
        }
    });
    for (id, error) in parse_errors.into_inner() {
        listing
            .unavailable
            .push(format!("native session {id}: {error}"));
        listing.unavailable_ids.push(id);
    }
    for id in no_path {
        listing
            .unavailable
            .push(format!("native session {id}: file locator is missing"));
        listing.unavailable_ids.push(id);
    }
    merge_discovery_coverage(
        &mut listing,
        page.failures,
        page.unreadable_ids,
        page.complete,
        intentional_result_limit,
    );
    listing
}

pub(crate) fn list_discovered_files_with_search(
    store: &NativeStore,
    query: &Query,
    needle: &str,
    tail: usize,
    read_bytes: u64,
    probe: impl Fn(&Path) -> Option<PathBuf>,
    parse: impl Fn(&NativeSession, &Path) -> Result<ParsedFile> + Sync,
) -> Listing {
    if query.limit == 0 {
        return Listing::default();
    }
    let page = store.candidates(query.ceiling);
    let mut by_path = HashMap::with_capacity(page.records.len());
    let mut files = Vec::with_capacity(page.records.len());
    let mut no_path = Vec::new();
    for native in page.records {
        let Some(path) = native.locator().map(Path::to_path_buf) else {
            no_path.push(native.id().to_owned());
            continue;
        };
        files.push(path.clone());
        by_path.insert(path, native);
    }
    let parse_errors = std::sync::Mutex::new(Vec::<(String, String)>::new());
    let mut listing =
        list_files_with_search(files, query, needle, tail, read_bytes, probe, |path| {
            let native = by_path.get(path)?;
            let parsed = match parse(native, path) {
                Ok(parsed) => parsed,
                Err(error) => {
                    parse_errors
                        .lock()
                        .expect("parse diagnostic mutex poisoned")
                        .push((native.id().to_owned(), format!("{error:#}")));
                    return None;
                }
            };
            match enrich_native_session(native, parsed.session) {
                Ok(session) => Some(ParsedFile {
                    session,
                    turns: parsed.turns,
                    truncated: parsed.truncated,
                }),
                Err(error) => {
                    parse_errors
                        .lock()
                        .expect("parse diagnostic mutex poisoned")
                        .push((native.id().to_owned(), error.to_string()));
                    None
                }
            }
        });
    for (id, error) in parse_errors
        .into_inner()
        .expect("parse diagnostic mutex poisoned")
    {
        listing
            .unavailable
            .push(format!("native session {id}: {error}"));
        listing.unavailable_ids.push(id);
    }
    for id in no_path {
        listing
            .unavailable
            .push(format!("native session {id}: file locator is missing"));
        listing.unavailable_ids.push(id);
    }
    merge_discovery_coverage(
        &mut listing,
        page.failures,
        page.unreadable_ids,
        page.complete,
        false,
    );
    listing
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
            SourceBound::FileTail { .. }
                | SourceBound::RecordPage { .. }
                | SourceBound::InputCoverage { .. }
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
    backends_reading(DEFAULT_READ_BYTES)
}

/// The installed backends, each reading at most `read_bytes` from the end of a
/// file-backed recording.
pub fn backends_with_read_bytes(read_bytes: u64) -> Result<Vec<Box<dyn Backend>>> {
    if !(MIN_READ_BYTES..=MAX_READ_BYTES).contains(&read_bytes) {
        anyhow::bail!(
            "read bytes must be between {} and {}",
            crate::byte_size::ByteSize::new(MIN_READ_BYTES),
            crate::byte_size::ByteSize::new(MAX_READ_BYTES)
        );
    }
    Ok(backends_reading(read_bytes))
}

fn backends_reading(read_bytes: u64) -> Vec<Box<dyn Backend>> {
    let discovery = Discovery::from_env();
    let store = |harness| {
        discovery
            .stores()
            .iter()
            .find(|store| store.harness() == harness)
            .cloned()
    };
    let mut backends: Vec<Box<dyn Backend>> = vec![
        Box::new(
            claude::ClaudeBackend::from_store(store(NativeHarness::Claude))
                .with_read_bytes(read_bytes),
        ),
        Box::new(
            codex::CodexBackend::from_store(store(NativeHarness::Codex))
                .with_read_bytes(read_bytes),
        ),
    ];
    backends.extend(
        discovery
            .stores()
            .iter()
            .filter(|store| store.harness() == NativeHarness::OpenCode)
            .cloned()
            .map(opencode::OpenCodeBackend::from_store)
            .map(|backend| Box::new(backend) as Box<dyn Backend>),
    );
    backends.push(Box::new(
        pi::PiBackend::from_store(store(NativeHarness::Pi)).with_read_bytes(read_bytes),
    ));
    backends
}

pub(crate) struct Jsonl {
    pub values: Vec<Value>,
    /// Absolute byte span for each decoded value, aligned with `values`.
    pub spans: Vec<ByteSpan>,
    pub skipped: usize,
    pub truncated: bool,
    pub source_length: u64,
    pub read_start: u64,
    pub read_end: u64,
    pub configured_bound: u64,
    pub source_revision: String,
    pub gaps: Vec<ReadGap>,
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
    pub head_spans: Vec<ByteSpan>,
    pub head_read_end: u64,
    pub head_gaps: Vec<ReadGap>,
    pub tail: Jsonl,
}

/// The bounded opening probe's decoded records, successful spans, physical
/// read end, and parsing gaps.
#[derive(Default)]
pub(crate) struct HeadJsonl {
    pub values: Vec<Value>,
    pub spans: Vec<ByteSpan>,
    pub read_end: u64,
    pub gaps: Vec<ReadGap>,
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

pub(crate) fn read_recording(path: &Path, read_bytes: u64) -> Result<Recording> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file.metadata()?;
    let tail = read_jsonl_from(&mut file, &metadata, read_bytes)?;
    let head = if tail.truncated {
        head_jsonl_from(&mut file, tail.source_length)?
    } else {
        HeadJsonl::default()
    };
    let after = file.metadata()?;
    if !same_source(&metadata, &after) {
        anyhow::bail!("recording source changed during the read; restart without a cursor");
    }
    Ok(Recording {
        head: head.values,
        head_spans: head.spans,
        head_read_end: head.read_end,
        head_gaps: head.gaps,
        tail,
    })
}

pub(crate) fn read_jsonl(path: &Path, read_bytes: u64) -> Result<Jsonl> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let read = read_jsonl_from(&mut file, &metadata, read_bytes)?;
    let after = file.metadata()?;
    if !same_source(&metadata, &after) {
        anyhow::bail!("recording source changed during the read; restart without a cursor");
    }
    Ok(read)
}

/// What a whole-recording read's `source_length` counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamCoordinates {
    /// Bytes of a recording file.
    FileBytes,
    /// An OpenCode session's message rows, oldest first.
    OpenCodeMessages,
}

impl StreamCoordinates {
    pub fn domain(self) -> &'static str {
        match self {
            Self::FileBytes => "file-byte-range",
            Self::OpenCodeMessages => "opencode-message",
        }
    }

    /// The source length in words, for human output.
    pub fn describe(self, length: u64) -> String {
        match self {
            Self::FileBytes => format!("source length {length} bytes"),
            Self::OpenCodeMessages => format!("{length} messages"),
        }
    }
}

/// What a whole-recording read established beside the turns it streamed.
pub struct StreamedTranscript {
    pub coordinates: StreamCoordinates,
    /// The source length observed when the read opened. The read stops there;
    /// anything appended later belongs to the next read.
    pub source_length: u64,
    /// Bounds the reader reached inside the records it read, such as turn
    /// text a store projection cut. A whole read reaches no record-count bound.
    pub source_bounds: Vec<SourceBound>,
    /// The source revision observed when the read opened, which every record
    /// reference names. A replay of this read names it too.
    pub source_revision: Option<String>,
    /// Internal hash of the observed file prefix used by same-process replays.
    pub(crate) source_prefix_sha256: Option<[u8; 32]>,
    /// Records that could not be decoded: malformed, or longer than
    /// [`FULL_RECORD_BYTES`]. Each is also a gap.
    pub skipped: usize,
    pub gaps: Vec<ReadGap>,
    pub trailing_record: Option<TrailingRecord>,
    /// The newest terminal observation the recording holds, where the harness
    /// records one.
    pub terminal: Option<TerminalObservation>,
    pub notes: Vec<String>,
    /// The records that produced no turn, where the reader counts them.
    pub unmapped: Option<crate::model::UnmappedRecords>,
    /// The kinds the harness's records can evidence, set by whoever knows
    /// which backend streamed the read.
    pub kinds: Option<crate::model::KindDeclaration>,
}

impl StreamedTranscript {
    /// Where a replay of this file read stops, and the revision it reports.
    pub(crate) fn pin(&self) -> Result<ReadPin<'_>> {
        let revision = self
            .source_revision
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("the earlier read names no file revision to replay"))?;
        Ok(ReadPin {
            length: self.source_length,
            revision,
            prefix_sha256: self.source_prefix_sha256.as_ref(),
        })
    }

    /// The read evidence of a whole-recording read: one range from the start
    /// of the source to the length observed at open, in the read's own
    /// coordinates, marked by the `full` projection option. Record spans are
    /// not collected here; a streamed turn's `record_ref` carries its own.
    pub fn read_evidence(&self, producer: Option<String>) -> ReadEvidence {
        ReadEvidence {
            source_length: self.source_length,
            configured_bound: self.source_length,
            coordinate_domain: self.coordinates.domain().to_owned(),
            source_revision: self.source_revision.clone(),
            source_prefix_sha256: self.source_prefix_sha256,
            producer,
            projection: crate::model::SESSION_SCHEMA.to_owned(),
            projection_options: vec!["full".to_owned()],
            observed_at: Utc::now(),
            ranges: vec![ReadRange {
                kind: ReadRangeKind::Tail,
                span: ByteSpan {
                    start: 0,
                    end: self.source_length,
                },
            }],
            records: Vec::new(),
            context_records: Vec::new(),
            gaps: self.gaps.clone(),
            unmapped: self.unmapped.clone(),
            record_sha256: Vec::new(),
            reader: Some(crate::reader::identity()),
        }
    }

    /// The transcript a whole-recording read closes with: its read evidence,
    /// source bounds, terminal observation, trailing record, and notes around
    /// the `turns` a consumer kept, under the turn `window` it applied.
    pub fn transcript(
        &self,
        session: Session,
        turns: Vec<Turn>,
        window: Option<crate::model::TurnWindow>,
    ) -> Transcript {
        let mut notes = skipped_records_note(self.skipped)
            .into_iter()
            .collect::<Vec<_>>();
        notes.extend(self.notes.iter().cloned());
        let read = self.read_evidence(session.source.producer.clone());
        let mut transcript = Transcript::new(
            session,
            turns,
            Truncation {
                window,
                source: self.source_bounds.clone(),
            },
            self.trailing_record.clone(),
            notes,
        );
        transcript.read = Some(read);
        transcript.terminal = self.terminal.clone();
        transcript.kinds = self.kinds.clone();
        transcript
    }
}

/// Distinct native types an unmapped-record tally names before it counts the
/// rest together, so a recording of arbitrary type strings stays bounded.
const MAX_UNMAPPED_TYPES: usize = 64;
const OTHER_UNMAPPED_TYPES: &str = "(other types)";

/// Counts the records a read decoded and represented as no turn.
#[derive(Default)]
pub(crate) struct UnmappedTally(crate::model::UnmappedRecords);

impl UnmappedTally {
    /// Count one record of `native_type`, left out on purpose when `declined`.
    pub(crate) fn add(&mut self, native_type: String, declined: bool) {
        let counts = if declined {
            &mut self.0.declined
        } else {
            &mut self.0.unrecognized
        };
        let key = if counts.len() < MAX_UNMAPPED_TYPES || counts.contains_key(&native_type) {
            native_type
        } else {
            OTHER_UNMAPPED_TYPES.to_owned()
        };
        *counts.entry(key).or_default() += 1;
    }

    pub(crate) fn finish(self) -> crate::model::UnmappedRecords {
        self.0
    }
}

/// A child's whole-recording read: the child session its records state, and
/// what the read established beside the turns it streamed.
pub struct StreamedChild {
    pub session: Session,
    pub read: StreamedTranscript,
}

/// The note a whole-recording read carries for the records it could not
/// decode, or nothing when it decoded every one.
pub fn skipped_records_note(skipped: usize) -> Option<String> {
    let noun = if skipped == 1 { "record" } else { "records" };
    (skipped > 0).then(|| {
        format!(
            "Skipped {skipped} unreadable {noun}: malformed, or longer than the {} record bound.",
            crate::byte_size::ByteSize::new(FULL_RECORD_BYTES)
        )
    })
}

/// The earliest and latest `timestamp` a stream of records carries.
#[derive(Default)]
pub(crate) struct ActivityRange(Option<(DateTime<Utc>, DateTime<Utc>)>);

impl ActivityRange {
    pub(crate) fn observe(&mut self, value: &Value) {
        if let Some(stamp) = timestamp(&value["timestamp"]) {
            self.0 = Some(match self.0 {
                Some((earliest, latest)) => (earliest.min(stamp), latest.max(stamp)),
                None => (stamp, stamp),
            });
        }
    }

    /// Set the session's start and newest activity to the range observed. A
    /// whole read reaches the first record, so the start is never uncertain.
    pub(crate) fn apply(self, session: &mut Session) {
        session.started_at = self.0.map(|(earliest, _)| earliest);
        session.last_activity_at = self.0.map(|(_, latest)| latest);
        session.start_uncertain = false;
    }
}

pub(crate) struct StreamedJsonl {
    pub source_length: u64,
    /// The revision observed at open, or the pinned one on a replay.
    pub revision: String,
    /// The SHA-256 of the bytes a replay is required to repeat.
    pub prefix_sha256: [u8; 32],
    pub skipped: usize,
    pub gaps: Vec<ReadGap>,
    /// The final decoded record and whether it produced turns, which is all a
    /// trailing-record judgment needs from a stream.
    pub last: Option<(Value, bool)>,
}

impl StreamedJsonl {
    /// Where a later pass over the same file stops, and the revision it
    /// reports.
    pub fn pin(&self) -> ReadPin<'_> {
        ReadPin {
            length: self.source_length,
            revision: &self.revision,
            prefix_sha256: Some(&self.prefix_sha256),
        }
    }
}

/// What an earlier pass over a file observed at open: its length, revision,
/// and content hash. A pass pinned to it reads those bytes and names that
/// revision, so every pass hands over identical records.
#[derive(Clone, Copy)]
pub(crate) struct ReadPin<'a> {
    length: u64,
    revision: &'a str,
    prefix_sha256: Option<&'a [u8; 32]>,
}

struct DigestReader<R> {
    inner: R,
    digest: Sha256,
}

impl<R> DigestReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
        }
    }
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.digest.update(&buffer[..read]);
        Ok(read)
    }
}

/// Decode every record of a JSONL recording in order, handing each to
/// `record` with its absolute span and the source revision. `record` answers
/// whether the record produced turns. Unpinned, the read stops at the length
/// the file had when it was opened; pinned, at the length the earlier pass
/// observed, reporting that pass's revision. A file replaced, shortened, or
/// changed within the pinned prefix refuses; bytes appended after that prefix
/// are outside this replay and remain allowed.
pub(crate) fn stream_jsonl(
    path: &Path,
    pin: Option<ReadPin<'_>>,
    record: impl FnMut(&Value, ByteSpan, &str) -> Result<bool>,
) -> Result<StreamedJsonl> {
    stream_jsonl_within(path, FULL_RECORD_BYTES, pin, record, |_| {})
}

/// Decode a JSONL recording while notifying an observer when malformed or
/// oversized records create a source gap. The ordinary reader keeps its
/// existing callback contract; accounting observers opt into this form so a
/// gap can invalidate model context before later observations.
pub(crate) fn stream_jsonl_with_gaps(
    path: &Path,
    pin: Option<ReadPin<'_>>,
    record: impl FnMut(&Value, ByteSpan, &str) -> Result<bool>,
    gap: impl FnMut(&ReadGap),
) -> Result<StreamedJsonl> {
    stream_jsonl_within(path, FULL_RECORD_BYTES, pin, record, gap)
}

/// The pin a whole-recording read starts from: none for a first read, the
/// earlier read's for a replay.
pub(crate) fn replay_pin(replay: Option<&StreamedTranscript>) -> Result<Option<ReadPin<'_>>> {
    replay.map(StreamedTranscript::pin).transpose()
}

fn stream_jsonl_within(
    path: &Path,
    record_bytes: u64,
    pin: Option<ReadPin<'_>>,
    mut record: impl FnMut(&Value, ByteSpan, &str) -> Result<bool>,
    mut gap: impl FnMut(&ReadGap),
) -> Result<StreamedJsonl> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let source_length = pin.map_or(metadata.len(), |pin| pin.length);
    if let Some(pin) = pin {
        if metadata.len() < pin.length
            || !pin
                .revision
                .starts_with(&format!("stat:{}:{}:", metadata.dev(), metadata.ino()))
        {
            anyhow::bail!("recording source was replaced or shortened between reads");
        }
    }
    let revision = pin.map_or_else(|| stat_revision(&metadata), |pin| pin.revision.to_owned());
    let mut reader =
        BufReader::with_capacity(256 * 1024, DigestReader::new(file).take(source_length));
    let mut streamed = StreamedJsonl {
        source_length,
        revision,
        prefix_sha256: [0; 32],
        skipped: 0,
        gaps: Vec::new(),
        last: None,
    };
    let mut line = Vec::new();
    let mut offset = 0;
    loop {
        line.clear();
        let read = (&mut reader)
            .take(record_bytes + 1)
            .read_until(b'\n', &mut line)? as u64;
        if read == 0 {
            break;
        }
        let mut span = ByteSpan {
            start: offset,
            end: offset + read,
        };
        if read > record_bytes && line.last() != Some(&b'\n') {
            span.end += skip_line(&mut reader)?;
            streamed.skipped += 1;
            let read_gap = ReadGap {
                span,
                reason: "oversized-record".to_owned(),
            };
            gap(&read_gap);
            streamed.gaps.push(read_gap);
        } else {
            let content = line[..].strip_suffix(b"\n").unwrap_or(&line[..]);
            if !content.iter().all(u8::is_ascii_whitespace) {
                match serde_json::from_slice::<Value>(content) {
                    Ok(value) => {
                        let produced = record(&value, span, &streamed.revision)?;
                        streamed.last = Some((value, produced));
                    }
                    Err(_) => {
                        streamed.skipped += 1;
                        let read_gap = ReadGap {
                            span,
                            reason: "malformed-record".to_owned(),
                        };
                        gap(&read_gap);
                        streamed.gaps.push(read_gap);
                    }
                }
            }
        }
        offset = span.end;
        // A long record grows the line buffer; give that memory back rather
        // than carry the largest record's size through the rest of the file.
        if line.capacity() as u64 > HEAD_PROBE_MAX_BYTES {
            line = Vec::new();
        }
    }
    streamed.prefix_sha256 = reader.get_ref().get_ref().digest.clone().finalize().into();
    if let Some(expected) = pin.and_then(|pin| pin.prefix_sha256) {
        if expected != &streamed.prefix_sha256 {
            anyhow::bail!("recording source changed between pinned reads");
        }
    }
    let after = reader.get_ref().get_ref().inner.metadata()?;
    if after.dev() != metadata.dev() || after.ino() != metadata.ino() || after.len() < source_length
    {
        anyhow::bail!("recording source was replaced or shortened during the read");
    }
    Ok(streamed)
}

/// Consume through the next newline, or to the end, returning the bytes
/// consumed.
fn skip_line(reader: &mut impl BufRead) -> Result<u64> {
    let mut consumed = 0;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(consumed);
        }
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            reader.consume(newline + 1);
            return Ok(consumed + newline as u64 + 1);
        }
        let length = buffer.len();
        reader.consume(length);
        consumed += length as u64;
    }
}

/// The trailing record of a streamed read: the final record, when it produced
/// no turn and its kind is one the harness names.
pub(crate) fn streamed_trailing_record(
    last: Option<&(Value, bool)>,
    known_kind: impl Fn(&Value) -> Option<&'static str>,
) -> Option<TrailingRecord> {
    let (value, produced) = last?;
    if *produced {
        return None;
    }
    Some(TrailingRecord {
        kind: known_kind(value)?.to_owned(),
        timestamp: timestamp(&value["timestamp"]),
    })
}

fn read_jsonl_from(
    file: &mut File,
    metadata: &std::fs::Metadata,
    configured_bound: u64,
) -> Result<Jsonl> {
    read_jsonl_from_length(file, metadata, configured_bound, metadata.len())
}

fn read_jsonl_from_length(
    file: &mut File,
    metadata: &std::fs::Metadata,
    configured_bound: u64,
    source_length: u64,
) -> Result<Jsonl> {
    let truncated = source_length > configured_bound;
    let read_start = source_length.saturating_sub(configured_bound);
    let read_end = source_length;
    let aligned = if read_start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(read_start - 1))?;
        let mut preceding = [0];
        file.read_exact(&mut preceding)?;
        preceding[0] == b'\n'
    };
    file.seek(SeekFrom::Start(read_start))?;
    let mut bytes = Vec::with_capacity((read_end - read_start) as usize);
    file.take(read_end - read_start).read_to_end(&mut bytes)?;

    let normalized_start = if truncated && !aligned {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |newline| newline + 1)
    } else {
        0
    };
    let (values, spans, skipped, mut gaps) = parse_jsonl_records(
        &bytes[normalized_start..],
        read_start + normalized_start as u64,
    );
    if truncated {
        if read_start > 0 {
            gaps.push(ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: read_start,
                },
                reason: "outside-configured-tail-bound".to_owned(),
            });
        }
        if normalized_start > 0 {
            gaps.push(ReadGap {
                span: ByteSpan {
                    start: read_start,
                    end: read_start + normalized_start as u64,
                },
                reason: "discarded-partial-record".to_owned(),
            });
        }
    }
    Ok(Jsonl {
        values,
        spans,
        skipped,
        truncated,
        source_length,
        read_start,
        read_end,
        configured_bound,
        source_revision: stat_revision(metadata),
        gaps,
    })
}

/// Reopen a bounded recording at the exact revision and source length named
/// by an earlier transcript read. A changed or appended source is refused so
/// usage totals and an opt-in observation suffix cannot silently mix reads.
pub(crate) fn read_recording_at(path: &Path, evidence: &ReadEvidence) -> Result<Recording> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    let expected = evidence
        .source_revision
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("the earlier read names no source revision to replay"))?;
    if metadata.len() != evidence.source_length || stat_revision(&metadata) != expected {
        anyhow::bail!("recording source changed between bounded usage reads");
    }
    let tail = read_jsonl_from_length(
        &mut file,
        &metadata,
        evidence.configured_bound,
        evidence.source_length,
    )?;
    let head = if tail.truncated {
        head_jsonl_from(&mut file, tail.source_length)?
    } else {
        HeadJsonl::default()
    };
    let after = file.metadata()?;
    if !same_source(&metadata, &after) {
        anyhow::bail!("recording source changed during bounded usage replay");
    }
    Ok(Recording {
        head: head.values,
        head_spans: head.spans,
        head_read_end: head.read_end,
        head_gaps: head.gaps,
        tail,
    })
}

fn parse_jsonl_records(
    bytes: &[u8],
    base: u64,
) -> (Vec<Value>, Vec<ByteSpan>, usize, Vec<ReadGap>) {
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
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        if content.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice(content) {
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

fn same_source(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn stat_revision(metadata: &std::fs::Metadata) -> String {
    format!(
        "stat:{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

/// A negative result is safe only for plain ASCII JSON. JSON permits any
/// character to be written as a `\u` escape, and non-ASCII case folding can
/// change the characters a search sees, so those inputs stay candidates for
/// the normal parser. A raw hit is only a reason to parse; it is not a match.
fn raw_tail_may_contain(path: &Path, needle: &str, read_bytes: u64) -> bool {
    if needle.is_empty() || !needle.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return true;
    }
    let Ok(mut file) = File::open(path) else {
        return true;
    };
    let Ok(len) = file.metadata().map(|metadata| metadata.len()) else {
        return true;
    };
    let start = len.saturating_sub(read_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return true;
    }
    let mut bytes = Vec::new();
    if file.take(read_bytes).read_to_end(&mut bytes).is_err() {
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
    let Ok(metadata) = file.metadata() else {
        return Vec::new();
    };
    let Ok(head) = head_jsonl_from(&mut file, metadata.len()) else {
        return Vec::new();
    };
    head.values
}

pub(crate) fn head_jsonl_from(file: &mut File, source_length: u64) -> Result<HeadJsonl> {
    file.seek(SeekFrom::Start(0))?;
    // The probe grows only while it holds no complete line, so a store of
    // ordinary files costs one small read each and a file whose first record
    // outgrows the probe costs one larger read rather than a wrong answer.
    let mut bytes = Vec::new();
    let mut window = HEAD_PROBE_BYTES;
    loop {
        let wanted = window - bytes.len() as u64;
        (&mut *file).take(wanted).read_to_end(&mut bytes)?;
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
            .map_or(&[][..], |newline| &bytes[..=newline])
    } else {
        &bytes
    };
    let (values, spans, _, gaps) = parse_jsonl_records(complete, 0);
    Ok(HeadJsonl {
        values,
        spans,
        read_end: bytes.len().min(source_length as usize) as u64,
        gaps,
    })
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
    read_bytes: u64,
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
                                .map_or(true, |metadata| metadata.len() > read_bytes);
                            if !oversized && !raw_tail_may_contain(path, &needle, read_bytes) {
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
                                    parsed.session.harness(),
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
/// older one would misidentify the store's actual ending. A recording with no
/// normalized turns may still have a verified final record.
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
    if last_turn.is_some_and(|last_turn| last_turn + 1 >= values.len()) {
        return None;
    }
    let value = values.last().copied()?;
    let kind = known_kind(value)?.to_owned();
    Some(TrailingRecord {
        kind,
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
                bytes: read.configured_bound,
            })
            .into_iter()
            .collect(),
    }
}

pub(crate) fn read_evidence(read: &Jsonl) -> ReadEvidence {
    let mut ranges = Vec::with_capacity(2);
    if read.read_start > 0 {
        ranges.push(ReadRange {
            kind: ReadRangeKind::Alignment,
            span: ByteSpan {
                start: read.read_start - 1,
                end: read.read_start,
            },
        });
    }
    ranges.push(ReadRange {
        kind: ReadRangeKind::Tail,
        span: ByteSpan {
            start: read.read_start,
            end: read.read_end,
        },
    });
    ReadEvidence {
        source_length: read.source_length,
        configured_bound: read.configured_bound,
        coordinate_domain: "file-byte-range".to_owned(),
        source_revision: Some(read.source_revision.clone()),
        source_prefix_sha256: None,
        producer: None,
        projection: crate::model::SESSION_SCHEMA.to_owned(),
        projection_options: Vec::new(),
        observed_at: Utc::now(),
        ranges,
        records: read.spans.clone(),
        context_records: Vec::new(),
        gaps: read.gaps.clone(),
        unmapped: None,
        record_sha256: Vec::new(),
        reader: Some(crate::reader::identity()),
    }
}

pub(crate) fn recording_evidence(recording: &Recording) -> ReadEvidence {
    let mut ranges = Vec::with_capacity(3);
    if recording.head_read_end > 0 {
        ranges.push(ReadRange {
            kind: ReadRangeKind::Head,
            span: ByteSpan {
                start: 0,
                end: recording.head_read_end,
            },
        });
    }
    if recording.tail.read_start > 0 {
        ranges.push(ReadRange {
            kind: ReadRangeKind::Alignment,
            span: ByteSpan {
                start: recording.tail.read_start - 1,
                end: recording.tail.read_start,
            },
        });
    }
    ranges.push(ReadRange {
        kind: ReadRangeKind::Tail,
        span: ByteSpan {
            start: recording.tail.read_start,
            end: recording.tail.read_end,
        },
    });
    let records = recording.tail.spans.clone();
    let context_records = recording.head_spans.clone();
    let covered_head = [ByteSpan {
        start: 0,
        end: recording.head_read_end,
    }];
    ReadEvidence {
        source_length: recording.tail.source_length,
        configured_bound: recording.tail.configured_bound,
        coordinate_domain: "file-byte-range".to_owned(),
        source_revision: Some(recording.tail.source_revision.clone()),
        source_prefix_sha256: None,
        producer: None,
        projection: crate::model::SESSION_SCHEMA.to_owned(),
        projection_options: Vec::new(),
        observed_at: Utc::now(),
        ranges,
        records,
        context_records,
        unmapped: None,
        record_sha256: Vec::new(),
        reader: Some(crate::reader::identity()),
        gaps: subtract_covered_ranges(
            &recording
                .head_gaps
                .iter()
                .chain(&recording.tail.gaps)
                .cloned()
                .collect::<Vec<_>>(),
            &covered_head,
            &recording.head_spans,
        ),
    }
}

/// Remove bytes from genuine unread-region gaps. Parsing failures remain
/// unless a successful record span proves the same partial record whole.
pub(crate) fn subtract_covered_ranges(
    gaps: &[ReadGap],
    covered: &[ByteSpan],
    successful_records: &[ByteSpan],
) -> Vec<ReadGap> {
    let mut normalized = Vec::new();
    for gap in gaps {
        if gap.reason == "discarded-partial-record"
            && successful_records
                .iter()
                .any(|record| record.start <= gap.span.start && record.end >= gap.span.end)
        {
            continue;
        }
        if gap.reason != "outside-configured-tail-bound" {
            normalized.push(gap.clone());
            continue;
        }
        let mut remainder = vec![gap.span];
        for cover in covered {
            remainder = remainder
                .into_iter()
                .flat_map(|span| {
                    if span.end <= cover.start || span.start >= cover.end {
                        return vec![span];
                    }
                    let mut pieces = Vec::with_capacity(2);
                    if span.start < cover.start {
                        pieces.push(ByteSpan {
                            start: span.start,
                            end: cover.start,
                        });
                    }
                    if span.end > cover.end {
                        pieces.push(ByteSpan {
                            start: cover.end,
                            end: span.end,
                        });
                    }
                    pieces
                })
                .collect();
        }
        normalized.extend(
            remainder
                .into_iter()
                .filter(|span| !span.is_empty())
                .map(|span| ReadGap {
                    span,
                    reason: gap.reason.clone(),
                }),
        );
    }
    normalized.sort_by(|left, right| {
        (left.span.start, left.span.end, &left.reason).cmp(&(
            right.span.start,
            right.span.end,
            &right.reason,
        ))
    });
    normalized.dedup_by(|left, right| left.span == right.span && left.reason == right.reason);
    normalized
}

pub(crate) fn terminal_from_values(
    values: &[Value],
    parse: impl Fn(&Value) -> Option<TerminalObservation>,
) -> Option<TerminalObservation> {
    values.iter().rev().find_map(parse)
}

pub(crate) fn attach_record_refs(
    turns: &mut [Turn],
    domain: &str,
    revision: Option<&str>,
    span: Option<ByteSpan>,
) {
    for (part_index, turn) in turns.iter_mut().enumerate() {
        let reference = RecordRef {
            domain: domain.to_owned(),
            revision: revision.map(str::to_owned),
            span,
            native_id: turn.native_id.clone(),
            pointer: None,
            part_index,
            content_part_index: None,
        };
        turn.record_ref = Some(reference.clone());
        for (content_part_index, part) in turn.parts.iter_mut().enumerate() {
            part.set_record_ref_part(reference.clone(), content_part_index);
        }
        if let Some(tool) = turn.tool.as_mut() {
            for invocation in &mut tool.invocations {
                invocation.record_ref = Some(reference.clone());
            }
            for artifact in &mut tool.artifact_references {
                artifact.source = Some(reference.clone());
            }
            for consumption in &mut tool.artifact_consumptions {
                consumption.reference.source = Some(reference.clone());
            }
        }
    }
}

pub(crate) fn transcript(
    session: Session,
    turns: Vec<Turn>,
    tail: usize,
    read: &Jsonl,
    trailing_record: Option<TrailingRecord>,
    notes: Vec<String>,
) -> Transcript {
    transcript_with_facts(
        session,
        turns,
        tail,
        TranscriptFacts {
            read_evidence: read_evidence(read),
            terminal: None,
            read,
        },
        trailing_record,
        notes,
    )
}

pub(crate) fn transcript_from_recording(
    session: Session,
    turns: Vec<Turn>,
    tail: usize,
    recording: &Recording,
    terminal: Option<TerminalObservation>,
    trailing_record: Option<TrailingRecord>,
    notes: Vec<String>,
) -> Transcript {
    transcript_with_facts(
        session,
        turns,
        tail,
        TranscriptFacts {
            read_evidence: recording_evidence(recording),
            terminal,
            read: &recording.tail,
        },
        trailing_record,
        notes,
    )
}

fn transcript_with_facts(
    session: Session,
    mut turns: Vec<Turn>,
    tail: usize,
    facts: TranscriptFacts<'_>,
    trailing_record: Option<TrailingRecord>,
    mut notes: Vec<String>,
) -> Transcript {
    let TranscriptFacts {
        read_evidence,
        terminal,
        read,
    } = facts;
    let mut read_evidence = read_evidence;
    read_evidence.producer = session.source.producer.clone();
    let total = turns.len();
    let text_count_in_read = turns.iter().filter(|turn| turn.kind.in_exchange()).count();
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
                bytes: read.configured_bound,
            })
            .into_iter()
            .collect(),
    };
    let text_count_returned = turns
        .iter()
        .filter(|turn| {
            matches!(
                turn.kind,
                crate::model::TurnKind::Operator | crate::model::TurnKind::Assistant
            )
        })
        .count();
    let empty_reason = if text_count_returned > 0 {
        None
    } else if tail == 0 {
        Some(crate::model::EmptyTextTailReason::ZeroRequestedTail)
    } else if text_count_in_read == 0 && read.truncated {
        Some(crate::model::EmptyTextTailReason::NoOperatorAssistantTextInRead)
    } else if text_count_in_read == 0 {
        Some(crate::model::EmptyTextTailReason::EmptyCompleteProjection)
    } else {
        Some(crate::model::EmptyTextTailReason::NoOperatorAssistantTextInRead)
    };

    Transcript::with_evidence(
        session,
        turns,
        truncation,
        TranscriptEvidence {
            read: Some(read_evidence),
            terminal,
            text_tail: Some(TextTailEvidence {
                requested: tail,
                returned: text_count_returned,
                empty_reason,
            }),
        },
        trailing_record,
        notes,
    )
}

struct TranscriptFacts<'a> {
    read_evidence: ReadEvidence,
    terminal: Option<TerminalObservation>,
    read: &'a Jsonl,
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn a_streamed_read_reports_malformed_and_oversized_records_and_keeps_the_rest() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/stream-jsonl-{}.jsonl",
            std::process::id()
        ));
        fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
        let long = format!("{{\"text\":\"{}\"}}", "x".repeat(64));
        fs::write(
            &path,
            format!("{{\"n\":1}}\nnot json\n{long}\n\n{{\"n\":2}}"),
        )
        .unwrap();
        let mut seen = Vec::new();
        let read = stream_jsonl_within(
            &path,
            32,
            None,
            |value, span, _| {
                seen.push((value["n"].as_u64(), span));
                Ok(value["n"] == 2)
            },
            |_| {},
        )
        .unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(
            seen.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        assert_eq!(seen[1].1.end, read.source_length);
        assert_eq!(read.skipped, 2);
        assert_eq!(
            read.gaps
                .iter()
                .map(|gap| (gap.reason.as_str(), gap.span.start, gap.span.end))
                .collect::<Vec<_>>(),
            [("malformed-record", 8, 17), ("oversized-record", 17, 93)]
        );
        assert!(read.last.is_some_and(|(_, produced)| produced));
    }

    /// A replay pinned to an earlier pass reads the bytes that pass read and
    /// names its revision, however the file grew since; a replaced file
    /// refuses rather than replaying different records.
    #[test]
    fn a_pinned_replay_repeats_the_earlier_pass_over_a_grown_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/stream-jsonl-pin-{}.jsonl",
            std::process::id()
        ));
        fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
        fs::write(&path, "{\"n\":1}\n{\"n\":2}\n").unwrap();
        let spans = |pin: Option<ReadPin<'_>>| {
            let mut seen = Vec::new();
            let read = stream_jsonl(&path, pin, |value, span, revision| {
                seen.push((value["n"].as_u64(), span, revision.to_owned()));
                Ok(true)
            });
            (read, seen)
        };
        let (first, observed) = spans(None);
        let first = first.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"n\":3}\n")
            .unwrap();
        let (replay, replayed) = spans(Some(first.pin()));
        assert_eq!(replayed, observed);
        assert_eq!(replay.unwrap().revision, first.revision);
        assert_ne!(stat_revision(&fs::metadata(&path).unwrap()), first.revision);

        // The original stays linked, so the replacement cannot reuse its inode.
        let kept = path.with_extension("kept");
        let replacement = path.with_extension("replacement");
        fs::hard_link(&path, &kept).unwrap();
        fs::write(&replacement, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n").unwrap();
        fs::rename(&replacement, &path).unwrap();
        let (replaced, _) = spans(Some(first.pin()));
        fs::remove_file(&path).unwrap();
        fs::remove_file(&kept).unwrap();
        assert!(replaced.is_err());
    }

    #[test]
    fn raw_prefilter_skips_a_definite_miss_but_not_a_hit() {
        let path = std::env::temp_dir().join(format!("tapes-raw-prefilter-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        fs::write(&path, br#"{"text":"other"}"#).unwrap();
        assert!(!raw_tail_may_contain(&path, "needle", DEFAULT_READ_BYTES));

        fs::write(&path, br#"{"text":"NEEDLE"}"#).unwrap();
        assert!(raw_tail_may_contain(&path, "needle", DEFAULT_READ_BYTES));

        fs::write(&path, br#"{"text":"\u006e\u0065\u0065\u0064\u006c\u0065"}"#).unwrap();
        assert!(raw_tail_may_contain(&path, "needle", DEFAULT_READ_BYTES));

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
            DEFAULT_READ_BYTES,
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
