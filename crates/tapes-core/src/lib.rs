use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use agent_tapes_discovery::{resolve_with_sources, IdentitySource, SharedStorePrecedence};
use backend::{Backend, BackendIdentitySource, Listing, Query};
use model::{Session, Transcript, Truncation};
use scope::Scope;

pub use agent_tapes_discovery::ResolveError;

pub use event::{
    Bounded, EventKind, EventRecord, EventTranscript, Incomplete, PairCounts, PairRef, ToolEvent,
    EVENTS_SCHEMA,
};

pub mod backend;
pub mod brief;
pub mod bundle;
pub mod byte_size;
pub mod child;
pub mod content;
pub mod endings;
pub mod event;
pub mod evidence;
pub mod history;
pub mod input;
pub mod lineage;
pub mod model;
pub mod reader;
pub mod scope;
pub mod stats;
pub mod stats_summary;
pub mod title;
pub mod usage;

pub const LIST_SCHEMA: &str = "tapes-list/6";
pub const EXPORT_MANIFEST_SCHEMA: &str = "tapes-export-manifest/5";
pub const USAGE_SUMMARY_SCHEMA: &str = "tapes-usage-summary/4";
/// Number of normalized turns a `list --search` query inspects per session.
/// Keeping this fixed makes the listing's cost predictable for callers.
pub const LIST_SEARCH_TAIL: usize = 32;
pub(crate) const DEFAULT_LIST_LIMIT: usize = 20;
/// Candidates a scoped listing may inspect per harness before it reports that
/// it stopped looking. Well above any store seen in practice, so it bounds a
/// pathological one without truncating a real search.
const SCAN_CEILING: usize = 5_000;
/// How many of the newest candidates `--latest` considers before choosing by
/// recorded activity. Small enough to stay cheap, wide enough to absorb a
/// store whose file times disagree with its transcripts.
const LATEST_WINDOW: usize = 5;
/// An export is a rescue: take the whole session the bounded read allows,
/// not the window `show` defaults to.
const EXPORT_TAIL: usize = usize::MAX;

/// Keep the turns an export's selection allows; no selection keeps every turn
/// the bounded read reached.
fn project_export(transcript: Transcript, turns: Option<model::TurnSelection>) -> Transcript {
    match turns {
        Some(selection) => transcript.project(selection, EXPORT_TAIL),
        None => transcript,
    }
}

pub type ActivityTimestamp = DateTime<Utc>;

pub fn parse_activity_timestamp(value: &str) -> std::result::Result<ActivityTimestamp, String> {
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Ok(timestamp.with_timezone(&Utc));
    }

    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|midnight| Utc.from_utc_datetime(&midnight))
        .ok_or_else(|| {
            format!("{value:?} is not an RFC 3339 timestamp with an offset or a YYYY-MM-DD date")
        })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListSort {
    #[default]
    Newest,
    Oldest,
}

/// Metadata and content predicates applied while each backend gathers its
/// candidates. Bounds use the session's newest recorded activity.
#[derive(Clone, Copy, Debug, Default)]
pub struct ListFilters<'a> {
    pub model: Option<&'a str>,
    pub directory: Option<&'a str>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub search: Option<&'a str>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivityWindow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct SessionList {
    pub schema: &'static str,
    pub sort: ListSort,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityWindow>,
    pub sessions: Vec<Session>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<crate::content::ArtifactReference>,
    /// Harnesses that could not be read at all.
    pub unavailable: Vec<String>,
    /// Sessions a readable harness could not normalize, each named with its
    /// id and the diagnostic. Kept apart from `unavailable`, because "this
    /// store is gone" and "one row in it is corrupt" are different facts and
    /// a reader that conflates them mis-states both.
    pub unreadable: Vec<String>,
    /// Sessions for which a requested bounded content search could not be
    /// answered, plus diagnostics from a search-stage fallback. A read
    /// failure is not a non-match.
    pub unsearched: Vec<String>,
    /// Candidate sessions inspected to produce this list, across every
    /// harness. A scoped listing reads more than it returns.
    pub scanned: usize,
    /// A backend stopped at its scan ceiling with candidates left. The listing
    /// is a view, not the set — "I stopped looking" is not "it is not there".
    pub scan_truncated: bool,
    /// Inspected candidates a scoped listing excluded because their recorded
    /// directory no longer resolves. Absent when nothing was excluded so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unplaced: Option<Unplaced>,
}

/// The wire form of a listing, so a serialized list can be read back. The
/// schema member is validated rather than carried, because a list this reader
/// did not produce must not be answered as one it did.
#[derive(Deserialize)]
struct SerializedList {
    schema: String,
    sort: ListSort,
    #[serde(default)]
    activity: Option<ActivityWindow>,
    sessions: Vec<Session>,
    #[serde(default)]
    artifacts: Vec<crate::content::ArtifactReference>,
    #[serde(default)]
    unavailable: Vec<String>,
    #[serde(default)]
    unreadable: Vec<String>,
    #[serde(default)]
    unsearched: Vec<String>,
    scanned: usize,
    #[serde(default)]
    scan_truncated: bool,
    #[serde(default)]
    unplaced: Option<Unplaced>,
}

impl<'de> Deserialize<'de> for SessionList {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let serialized = SerializedList::deserialize(deserializer)?;
        if serialized.schema != LIST_SCHEMA {
            return Err(serde::de::Error::custom(format!(
                "unsupported schema: {}",
                serialized.schema
            )));
        }
        Ok(Self {
            schema: LIST_SCHEMA,
            sort: serialized.sort,
            activity: serialized.activity,
            sessions: serialized.sessions,
            artifacts: serialized.artifacts,
            unavailable: serialized.unavailable,
            unreadable: serialized.unreadable,
            unsearched: serialized.unsearched,
            scanned: serialized.scanned,
            scan_truncated: serialized.scan_truncated,
            unplaced: serialized.unplaced,
        })
    }
}

/// Recorded directories a scoped listing could not place. A session recorded
/// in a removed directory — a deleted per-change worktree, most often — may
/// belong to the project, but nothing left on disk proves which repository it
/// was, so it is excluded and counted rather than guessed at from its path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unplaced {
    /// Distinct directories excluded this way among the inspected candidates.
    pub directories: usize,
    /// The first of them in relevance order; ordering never proves membership.
    pub examples: Vec<std::path::PathBuf>,
}

const MAX_UNPLACED_EXAMPLES: usize = 8;

impl Unplaced {
    pub(crate) fn from_directories(directories: Vec<std::path::PathBuf>) -> Option<Self> {
        (!directories.is_empty()).then(|| Self {
            directories: directories.len(),
            examples: directories
                .into_iter()
                .take(MAX_UNPLACED_EXAMPLES)
                .collect(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SelectionDiagnostics {
    /// Rows or stores a bounded candidate scan could not read. Unavailable
    /// harnesses stay separate: their absence does not make a readable choice
    /// uncertain.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
    /// Scoped candidates whose recorded directories no longer resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unplaced: Option<Unplaced>,
}

impl SelectionDiagnostics {
    fn from_listed(listed: &Listed) -> Self {
        Self {
            unreadable: listed.unreadable.clone(),
            unplaced: Unplaced::from_directories(listed.unplaced.clone()),
        }
    }

    /// The warning a latest selection carries when the bounded candidate scan
    /// did not establish that its readable choice is the newest activity.
    pub fn latest_warnings(&self, session: &Session) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.unreadable.is_empty() {
            warnings.push(format!(
                "--latest picked {} session {}, but newer activity may be hidden because unreadable rows or stores were omitted from the bounded candidate scan: {}",
                session.harness(),
                session.id,
                self.unreadable.join("; ")
            ));
        }
        if let Some(unplaced) = &self.unplaced {
            let noun = if unplaced.directories == 1 {
                "directory"
            } else {
                "directories"
            };
            warnings.push(format!(
                "--latest picked {} session {}, but newer activity may be hidden because the bounded scoped scan excluded {} unresolved {noun}; membership cannot be proven",
                session.harness(),
                session.id,
                unplaced.directories
            ));
        }
        warnings
    }
}

#[derive(Debug)]
pub struct ResolvedSession {
    pub backend_index: usize,
    pub session: Session,
    pub diagnostics: SelectionDiagnostics,
}

/// Where a command looks: one project, or every session on the machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Where<'a> {
    /// The project containing the current directory.
    Here,
    /// The project containing a named path.
    Project(&'a Path),
    #[default]
    Global,
}

impl Where<'_> {
    pub fn resolve(&self) -> Result<Option<Scope>> {
        match self {
            Self::Here => Scope::here().map(Some),
            Self::Project(path) => Scope::at(path).map(Some),
            Self::Global => Ok(None),
        }
    }
}

pub fn list(harness: Option<&str>, within: Where, limit: Option<usize>) -> Result<SessionList> {
    list_with_filters(harness, within, limit, None, None)
}

pub fn list_with_filters(
    harness: Option<&str>,
    within: Where,
    limit: Option<usize>,
    model: Option<&str>,
    directory: Option<&str>,
) -> Result<SessionList> {
    list_with_filters_and_search(harness, within, limit, model, directory, None)
}

pub fn list_with_filters_and_search(
    harness: Option<&str>,
    within: Where,
    limit: Option<usize>,
    model: Option<&str>,
    directory: Option<&str>,
    search: Option<&str>,
) -> Result<SessionList> {
    list_with_options(
        harness,
        within,
        limit,
        ListFilters {
            model,
            directory,
            search,
            ..ListFilters::default()
        },
        ListSort::Newest,
    )
}

pub fn list_with_options(
    harness: Option<&str>,
    within: Where,
    limit: Option<usize>,
    filters: ListFilters<'_>,
    sort: ListSort,
) -> Result<SessionList> {
    let scope = within.resolve()?;
    list_with_backends_options(
        &backend::backends(),
        harness,
        scope.as_ref(),
        limit.unwrap_or(DEFAULT_LIST_LIMIT),
        &filters,
        sort,
    )
}

pub fn list_with_backends(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
) -> Result<SessionList> {
    list_with_backends_options(
        backends,
        harness,
        scope,
        limit,
        &ListFilters::default(),
        ListSort::Newest,
    )
}

pub fn list_with_backends_filtered(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
    model: Option<&str>,
    directory: Option<&str>,
) -> Result<SessionList> {
    list_with_backends_options(
        backends,
        harness,
        scope,
        limit,
        &ListFilters {
            model,
            directory,
            ..ListFilters::default()
        },
        ListSort::Newest,
    )
}

pub fn list_with_backends_filtered_and_search(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
    model: Option<&str>,
    directory: Option<&str>,
    search: Option<&str>,
) -> Result<SessionList> {
    list_with_backends_options(
        backends,
        harness,
        scope,
        limit,
        &ListFilters {
            model,
            directory,
            search,
            ..ListFilters::default()
        },
        ListSort::Newest,
    )
}

pub fn list_with_backends_options(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
    filters: &ListFilters<'_>,
    sort: ListSort,
) -> Result<SessionList> {
    let listed = list_scoped(backends, harness, scope, limit, filters, sort)?;
    Ok(SessionList {
        schema: LIST_SCHEMA,
        sort,
        activity: (filters.since.is_some() || filters.until.is_some()).then_some(ActivityWindow {
            since: filters.since,
            until: filters.until,
        }),
        sessions: listed.sessions,
        artifacts: listed.artifacts,
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
        unplaced: Unplaced::from_directories(listed.unplaced),
    })
}

pub(crate) struct Listed {
    pub(crate) sessions: Vec<Session>,
    pub(crate) artifacts: Vec<crate::content::ArtifactReference>,
    /// Which backend each session came from, positionally — kept so a
    /// selection can go straight to its transcript without resolving the id
    /// against every store again.
    pub(crate) origins: Vec<usize>,
    pub(crate) unavailable: Vec<String>,
    pub(crate) unreadable: Vec<String>,
    pub(crate) unsearched: Vec<String>,
    pub(crate) scanned: usize,
    pub(crate) scan_truncated: bool,
    /// Recorded directories the scope excluded because they no longer resolve.
    pub(crate) unplaced: Vec<std::path::PathBuf>,
}

pub(crate) fn list_scoped(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
    filters: &ListFilters<'_>,
    sort: ListSort,
) -> Result<Listed> {
    if let Some(harness) = harness {
        if !backends.iter().any(|backend| backend.harness() == harness) {
            return Err(anyhow!("unknown harness: {harness}"));
        }
    }

    if filters
        .since
        .zip(filters.until)
        .is_some_and(|(since, until)| since >= until)
    {
        return Err(anyhow!("activity window requires since before until"));
    }

    // Content search is a filter, so the backend must inspect candidates until
    // it has found the requested result set rather than spending the limit on
    // sessions whose recent turns do not match. A backend walks its store
    // newest first, so the oldest sessions are known only once every candidate
    // has been seen: an oldest-first listing likewise inspects them all and
    // applies the limit after the sort.
    let candidate_limit = if (filters.search.is_some() || sort == ListSort::Oldest) && limit > 0 {
        usize::MAX
    } else {
        limit
    };
    let mut query = Query::scoped_with_filters(
        scope,
        candidate_limit,
        if scope.is_some() {
            SCAN_CEILING
        } else {
            usize::MAX
        },
        filters.model,
        filters.directory,
        filters.since,
        filters.until,
    );
    query.sort = sort;
    let mut found: Vec<(Session, usize)> = Vec::new();
    let mut precedence = SharedStorePrecedence::default();
    // The earliest store of each harness whose stores answer the same ids
    // that could not be listed at all, and the name its diagnostics use.
    let mut failed_stores = HashMap::<&'static str, String>::new();
    let mut available_harnesses = HashSet::new();
    let mut unavailable_harnesses = Vec::new();
    let mut unreadable_sessions = Vec::new();
    let mut unsearched_sessions = Vec::new();
    let mut artifacts = Vec::new();
    let mut scanned = 0;
    let mut scan_truncated = false;
    let mut selected_harness_counts = HashMap::<String, usize>::new();
    if filters.search.is_some() {
        for backend in backends.iter().filter(|backend| {
            harness.is_none_or(|name| backend.harness() == name) && backend.available()
        }) {
            *selected_harness_counts
                .entry(backend.harness().to_owned())
                .or_default() += 1;
        }
    }
    for (index, backend) in backends
        .iter()
        .enumerate()
        .filter(|(_, backend)| harness.is_none_or(|name| backend.harness() == name))
    {
        if !backend.available() {
            unavailable_harnesses.push(backend.harness());
            continue;
        }
        // Some harnesses expose the same session through more than one
        // projection. Search obeys the same first-source rule as resolution:
        // a later projection cannot turn an earlier projection's non-match
        // into a match for the same global id.
        if filters.search.is_some()
            && selected_harness_counts
                .get(backend.harness())
                .is_some_and(|count| *count > 1)
        {
            match backend.list(&query) {
                Ok(listing) => {
                    if listing.scan_truncated
                        || !listing.unavailable.is_empty()
                        || !listing.unavailable_ids.is_empty()
                    {
                        let detail = if listing.unavailable.is_empty() {
                            "candidate listing stopped before coverage was complete".to_owned()
                        } else {
                            listing.unavailable.join("; ")
                        };
                        unsearched_sessions.push(format!(
                            "{} search could not reconcile duplicate projections: {detail}",
                            backend.harness()
                        ));
                    }
                    for session in &listing.sessions {
                        precedence.record_listed(
                            backend.harness(),
                            backend.shares_session_ids(),
                            index,
                            &session.id,
                        );
                    }
                    for id in &listing.unavailable_ids {
                        precedence.record_unreadable(
                            backend.harness(),
                            backend.shares_session_ids(),
                            index,
                            id,
                        );
                    }
                }
                Err(error) => {
                    unsearched_sessions.push(format!(
                        "{} search could not reconcile duplicate projections: {error:#}",
                        backend.harness()
                    ));
                    precedence.record_store_failure(
                        backend.harness(),
                        backend.shares_session_ids(),
                        index,
                    );
                }
            }
        }
        let listing = if let Some(needle) = filters.search {
            backend.list_with_search(&query, needle, LIST_SEARCH_TAIL)
        } else {
            backend.list(&query)
        };
        match listing {
            Ok(Listing {
                sessions,
                artifacts: discovered_artifacts,
                unavailable,
                unavailable_ids,
                unsearched,
                scanned: inspected,
                scan_truncated: truncated,
            }) => {
                available_harnesses.insert(backend.harness().to_owned());
                for id in &unavailable_ids {
                    precedence.record_unreadable(
                        backend.harness(),
                        backend.shares_session_ids(),
                        index,
                        id,
                    );
                }
                unreadable_sessions.extend(unavailable);
                unsearched_sessions.extend(unsearched);
                scanned += inspected;
                scan_truncated |= truncated;
                found.extend(sessions.into_iter().map(|session| (session, index)));
                artifacts.extend(discovered_artifacts);
            }
            Err(error) => {
                if backend.harness() == "input" {
                    return Err(error);
                }
                if filters.search.is_some() {
                    unsearched_sessions
                        .push(format!("{} search failed: {error:#}", backend.harness()));
                }
                if backend.shares_session_ids() {
                    let store = backend
                        .store()
                        .unwrap_or_else(|| backend.harness().to_owned());
                    unreadable_sessions.push(format!(
                        "{} store {store} could not be listed: {error:#}",
                        backend.harness()
                    ));
                    precedence.record_store_failure(backend.harness(), true, index);
                    failed_stores.entry(backend.harness()).or_insert(store);
                }
                unavailable_harnesses.push(backend.harness());
            }
        }
    }
    // Stable OpenCode and opencode2 can expose the same global session id from
    // different projections. The first store wins, preserving one session row
    // and its transcript origin instead of inventing ambiguity, and a session
    // it reported unreadable stays unreadable rather than being listed from
    // the later store's projection of it.
    let mut withheld = HashMap::<&'static str, usize>::new();
    found.retain(|(session, origin)| {
        let backend = backends[*origin].as_ref();
        let behind_failure = precedence.has_prior_store_failure(
            backend.harness(),
            backend.shares_session_ids(),
            *origin,
        );
        if behind_failure {
            *withheld.entry(backend.harness()).or_default() += 1;
            return false;
        }
        precedence.admits(
            backend.harness(),
            backend.shares_session_ids(),
            *origin,
            &session.id,
        )
    });
    for (harness, store) in &failed_stores {
        if let Some(count) = withheld.get(harness) {
            let noun = if *count == 1 { "session" } else { "sessions" };
            unreadable_sessions.push(format!(
                "{harness}: withheld {count} {noun} another store listed, because {store} may hold them and could not be read"
            ));
        }
    }
    found.sort_by(|(left, _), (right, _)| compare_sessions(left, right, sort));

    // A harness may have more than one installed store, such as stable
    // OpenCode and opencode2. Keep the public limit per harness, not per store.
    let mut returned = HashMap::<String, usize>::new();
    let (sessions, origins) = found
        .into_iter()
        .filter_map(|(session, origin)| {
            let count = returned.entry(session.harness().to_owned()).or_default();
            if *count >= limit {
                return None;
            }
            *count += 1;
            Some((session, origin))
        })
        .unzip();
    let mut unavailable = Vec::new();
    for harness in unavailable_harnesses {
        if available_harnesses.contains(harness) || unavailable.iter().any(|name| name == harness) {
            continue;
        }
        unavailable.push(harness.to_owned());
    }
    Ok(Listed {
        sessions,
        artifacts,
        origins,
        unavailable,
        unreadable: unreadable_sessions,
        unsearched: unsearched_sessions,
        scanned,
        scan_truncated,
        unplaced: scope.map(Scope::unresolved_ranked).unwrap_or_default(),
    })
}

pub(crate) fn compare_sessions(left: &Session, right: &Session, sort: ListSort) -> Ordering {
    let activity = compare_activity(left.last_activity_at, right.last_activity_at, sort);
    activity
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.harness().cmp(right.harness()))
}

fn compare_activity(
    left: Option<DateTime<Utc>>,
    right: Option<DateTime<Utc>>,
    sort: ListSort,
) -> Ordering {
    match (left, right, sort) {
        (Some(left), Some(right), ListSort::Newest) => right.cmp(&left),
        (Some(left), Some(right), ListSort::Oldest) => left.cmp(&right),
        (Some(_), None, ListSort::Newest) => Ordering::Less,
        (None, Some(_), ListSort::Newest) => Ordering::Greater,
        (Some(_), None, ListSort::Oldest) => Ordering::Less,
        (None, Some(_), ListSort::Oldest) => Ordering::Greater,
        (None, None, _) => Ordering::Equal,
    }
}

/// The most recent session in scope, with the sessions named in `exclude`
/// passed over.
///
/// Excluding is the caller's job because it is the only party that can do it.
/// An agent asking this question from inside its own live session is usually
/// the newest session in scope, and nothing observable in a store separates
/// "the session asking" from "the session that just died" — two stores can be
/// identical and differ only in who invoked. So the tool reports the latest
/// and takes the caller's word for what to skip, rather than inferring it from
/// recency and silently discarding the crash it exists to recover.
///
/// "Most recent" is judged over the newest [`LATEST_WINDOW`] candidates each
/// store offers, and settled among them by recorded activity. What "newest"
/// means before that is the store's own answer, not this one's: file
/// modification time for the file-backed harnesses, and the API's ordering
/// within a single page for opencode. Establishing it independently would mean
/// reading every session, which the bounded-read invariant rules out — so a
/// transcript whose file time was disturbed by a restore or a copy, or an
/// opencode session sitting beyond the page the API returns, can fall outside
/// the window. A listing reports the page bound through `scan_truncated`;
/// `list` plus an explicit id remains exact.
pub fn latest_with_backends(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    exclude: &[String],
) -> Result<ResolvedSession> {
    // A store is walked in modification order, which is a proxy for recency
    // and not the same as it: a transcript restored or touched after a newer
    // one sorts ahead of it. Taking a small window and choosing by recorded
    // activity costs a few extra parses and removes that skew.
    let filters = ListFilters::default();
    let listed = list_scoped(
        backends,
        harness,
        scope,
        exclude.len() + LATEST_WINDOW,
        &filters,
        ListSort::Newest,
    )?;
    let scoped = if scope.is_some() {
        "in this project"
    } else {
        "on this machine"
    };
    if listed
        .sessions
        .iter()
        .any(|session| session.last_activity_at.is_none())
    {
        anyhow::bail!(
            "--latest cannot choose the newest session {scoped}: at least one candidate has no recorded activity timestamp"
        );
    }
    let diagnostics = SelectionDiagnostics::from_listed(&listed);
    listed
        .sessions
        .into_iter()
        .zip(listed.origins)
        .filter(|(session, _)| !exclude.contains(&session.id))
        .max_by(
            |(left, _), (right, _)| match (left.last_activity_at, right.last_activity_at) {
                (Some(left), Some(right)) => left.cmp(&right),
                (Some(_), None) => Ordering::Greater,
                (None, Some(_)) => Ordering::Less,
                (None, None) => Ordering::Equal,
            },
        )
        .map(|(session, backend_index)| ResolvedSession {
            backend_index,
            session,
            diagnostics: diagnostics.clone(),
        })
        .ok_or_else(|| {
            let mut message = format!("no session was found {scoped}");
            if listed.scan_truncated {
                message.push_str(" (the search stopped before every candidate had been inspected)");
            }
            if !listed.unavailable.is_empty() {
                message.push_str(&format!("; unavailable: {}", listed.unavailable.join(", ")));
            }
            anyhow!(message)
        })
}

pub fn resolve_session(
    backends: &[Box<dyn Backend>],
    query: &str,
) -> std::result::Result<ResolvedSession, ResolveError> {
    let sources = backends
        .iter()
        .map(|backend| BackendIdentitySource(backend.as_ref()))
        .collect::<Vec<_>>();
    let sources = sources
        .iter()
        .map(|source| source as &dyn IdentitySource<Session>)
        .collect::<Vec<_>>();
    let resolved = resolve_with_sources(&sources, query)?;
    Ok(ResolvedSession {
        backend_index: resolved.source_index,
        session: resolved.record,
        diagnostics: SelectionDiagnostics::default(),
    })
}

/// Select one session by ID, exact recorded title, or latest activity in scope.
#[derive(Clone, Debug)]
pub enum Selection<'a> {
    Id(&'a str),
    Occurrence(&'a str),
    Title {
        title: &'a str,
        within: Where<'a>,
        harness: Option<&'a str>,
    },
    Latest {
        within: Where<'a>,
        harness: Option<&'a str>,
        exclude: &'a [String],
    },
}

impl Selection<'_> {
    fn resolve(&self, backends: &[Box<dyn Backend>]) -> Result<ResolvedSession> {
        match self {
            Self::Id(id) => Ok(resolve_session(backends, id)?),
            Self::Occurrence(occurrence) => {
                let mut matches = Vec::new();
                let mut failures = Vec::new();
                for (backend_index, backend) in backends.iter().enumerate() {
                    match backend.locate_occurrence(occurrence) {
                        Ok(Some(session)) => matches.push(ResolvedSession {
                            backend_index,
                            session,
                            diagnostics: SelectionDiagnostics::default(),
                        }),
                        Ok(None) => {}
                        Err(error) => {
                            failures.push((backend.harness().to_owned(), format!("{error:#}")))
                        }
                    }
                }
                match matches.len() {
                    1 => Ok(matches.pop().expect("one occurrence match")),
                    0 if !failures.is_empty() => Err(anyhow!(
                        "occurrence {occurrence} could not be resolved: {}",
                        failures
                            .into_iter()
                            .map(|(backend, error)| format!("{backend}: {error}"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    )),
                    0 => Err(anyhow!("occurrence {occurrence} was not found")),
                    _ => Err(anyhow!("occurrence {occurrence} is ambiguous")),
                }
            }
            Self::Title {
                title,
                within,
                harness,
            } => {
                let scope = within.resolve()?;
                title::resolve(backends, title, *harness, scope.as_ref())
            }
            Self::Latest {
                within,
                harness,
                exclude,
            } => {
                let scope = within.resolve()?;
                latest_with_backends(backends, *harness, scope.as_ref(), exclude)
            }
        }
    }
}

/// Resolve a selection's degraded latest warning without opening its
/// transcript. Non-latest selections have no candidate-scan warning.
pub fn selection_warnings_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<Vec<String>> {
    let resolved = selection.resolve(backends)?;
    Ok(resolved.diagnostics.latest_warnings(&resolved.session))
}

pub fn show(selection: Selection, tail: Option<usize>) -> Result<Transcript> {
    show_with_backends(&backend::backends(), selection, tail.unwrap_or(100))
}

/// One session's usage. The read uses the export-shaped window, so the turn
/// counts are the whole bounded read's rather than a display window's.
pub fn usage(selection: Selection) -> Result<usage::UsageView> {
    usage_with_backends(&backend::backends(), selection)
}

pub fn usage_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<usage::UsageView> {
    Ok(usage::usage(&show_with_backends(
        backends,
        selection,
        EXPORT_TAIL,
    )?))
}

/// Read one session's usage with an optional bounded observation suffix. The
/// bounded form replays the transcript's own source evidence; the full form
/// pins the observation pass to the first streamed read. A backend that does
/// not expose accounting observations refuses only when the caller asks for
/// the opt-in series.
pub fn usage_with_options_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    options: usage::UsageOptions,
) -> Result<usage::UsageView> {
    if let Some(series) = options.series {
        if !(1..=10_000).contains(&series.limit) {
            anyhow::bail!("usage observation rows must be between 1 and 10000");
        }
    }
    let resolved = selection.resolve(backends)?;
    let backend = &backends[resolved.backend_index];
    if options.full {
        let mut turns = usage::TurnTally::default();
        let read = backend.stream_transcript(&resolved.session, None, &mut |turn| {
            turns.add(&turn);
            Ok(())
        })?;
        let read_evidence = read.read_evidence(resolved.session.source.producer.clone());
        let observations = options
            .series
            .map(|series| {
                backend.usage_observations(&resolved.session, Some(&read_evidence), series)
            })
            .transpose()?;
        let session = observations
            .as_ref()
            .map(|observations| observations.session.clone())
            .map_or_else(|| backend.stream_session(&resolved.session, &read), Ok)?;
        let mut view = usage::streamed(&session, turns, &read);
        view.notes
            .extend(resolved.diagnostics.latest_warnings(&resolved.session));
        view.series = observations.map(|observations| observations.series);
        Ok(view)
    } else {
        let mut transcript = declared_transcript(backend.as_ref(), &resolved.session, EXPORT_TAIL)?;
        transcript
            .notes
            .extend(resolved.diagnostics.latest_warnings(&resolved.session));
        let observations = options
            .series
            .map(|series| {
                backend.usage_observations(&transcript.session, transcript.read.as_ref(), series)
            })
            .transpose()?;
        if let Some(observations) = &observations {
            transcript.session = observations.session.clone();
        }
        let mut view = usage::usage(&transcript);
        view.series = observations.map(|observations| observations.series);
        Ok(view)
    }
}

/// One session's usage over its whole recording. Turn counts and the content
/// inventory fold each turn as it streams, and the session's counters are
/// folded from every record up to the length the turn read observed, so both
/// cover the same recording.
pub fn usage_full_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<usage::UsageView> {
    let resolved = selection.resolve(backends)?;
    let backend = &backends[resolved.backend_index];
    let mut turns = usage::TurnTally::default();
    let mut read = backend.stream_transcript(&resolved.session, None, &mut |turn| {
        turns.add(&turn);
        Ok(())
    })?;
    read.notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    let session = backend.stream_session(&resolved.session, &read)?;
    Ok(usage::streamed(&session, turns, &read))
}

/// Every relative one session's whole recording names, read record by record.
pub fn lineage_full_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<lineage::LineageView> {
    let resolved = selection.resolve(backends)?;
    let read = backends[resolved.backend_index].stream_lineage(&resolved.session)?;
    let mut view = lineage::view(&resolved.session, read);
    view.notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    Ok(view)
}

/// One session's recorded relatives. The read never opens a child's turns:
/// ordinary child sessions are read under their own IDs, while Claude
/// subagent evidence is read through the parent-qualified child API.
pub fn lineage(selection: Selection) -> Result<lineage::LineageView> {
    lineage_with_backends(&backend::backends(), selection)
}

pub fn lineage_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<lineage::LineageView> {
    let resolved = selection.resolve(backends)?;
    let read = backends[resolved.backend_index].lineage(&resolved.session)?;
    let mut view = lineage::view(&resolved.session, read);
    view.notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    Ok(view)
}

/// One session's counted facts. The transcript read uses the export-shaped
/// window, and the relatives are counted from the same reference read
/// `lineage` answers with, which never opens a child.
pub fn stats(selection: Selection) -> Result<stats::StatsView> {
    stats_with_backends(&backend::backends(), selection)
}

pub fn stats_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<stats::StatsView> {
    let resolved = selection.resolve(backends)?;
    let backend = backends[resolved.backend_index].as_ref();
    let mut transcript = declared_transcript(backend, &resolved.session, EXPORT_TAIL)?;
    transcript
        .notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    let lineage = backend.lineage(&resolved.session)?;
    Ok(stats::stats(transcript, &lineage))
}

/// One session's counted facts over its whole recording, streamed twice so
/// tool events pair without holding the file, with the relatives every record
/// names.
pub fn stats_full_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<stats::StatsView> {
    let resolved = selection.resolve(backends)?;
    let backend = backends[resolved.backend_index].as_ref();
    let counted = stats::Counted::whole(backend, &resolved.session)?;
    let lineage = backend.stream_lineage(&resolved.session)?;
    let mut view = counted.view(&lineage);
    view.notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    Ok(view)
}

/// What a continuation of one session needs from its recording. The read is
/// export-shaped, so tool pairing sees every call and its result; `tail`
/// bounds the rendered exchange alone.
pub fn brief(selection: Selection, tail: Option<usize>) -> Result<brief::Brief> {
    brief_with_backends(
        &backend::backends(),
        selection,
        tail.unwrap_or(brief::DEFAULT_BRIEF_TAIL),
    )
}

pub fn brief_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    tail: usize,
) -> Result<brief::Brief> {
    let resolved = selection.resolve(backends)?;
    let mut transcript =
        backends[resolved.backend_index].transcript(&resolved.session, EXPORT_TAIL)?;
    transcript
        .notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    let lineage = backends[resolved.backend_index].lineage(&resolved.session);
    Ok(brief::brief(transcript, lineage, tail))
}

pub fn events(selection: Selection, tail: Option<usize>) -> Result<event::EventTranscript> {
    events_with_backends(&backend::backends(), selection, tail.unwrap_or(EXPORT_TAIL))
}

pub fn events_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    tail: usize,
) -> Result<event::EventTranscript> {
    events_with_options_with_backends(backends, selection, tail, event::EventOptions::default())
}

/// Project one session's events with the caller's options: `full_arguments`
/// publishes each tool call's complete recorded arguments rather than the
/// bounded prefix.
pub fn events_with_options_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    tail: usize,
    options: event::EventOptions,
) -> Result<event::EventTranscript> {
    let resolved = selection.resolve(backends)?;
    let mut events =
        backends[resolved.backend_index].events_with_options(&resolved.session, tail, options)?;
    events
        .notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    Ok(events)
}

/// Receives a whole-recording event projection as it streams: the session
/// the recording states, then each kept record in recording order, final.
pub trait EventSink {
    fn session(&mut self, session: &Session) -> Result<()>;
    fn record(&mut self, record: event::EventRecord) -> Result<()>;
}

/// What a whole-recording event projection established beside the records it
/// streamed.
pub struct StreamedEvents {
    /// The projection the streamed records belong to, holding every member
    /// but those records.
    pub events: event::EventTranscript,
    pub read: backend::StreamedTranscript,
}

/// Project one session's whole recording into paired tool events, streaming
/// each record `filter` keeps within the newest `tail` turns to `sink`.
///
/// The recording is streamed twice. The first read observes every tool
/// record, counts the turns so the window is known before anything is
/// written, and notes which calls declare a requested program; the second,
/// replaying the first, pairs each record and hands on the kept ones. Memory
/// follows the calls still awaiting a result, never the file. Nothing reaches
/// `sink` before the second read has opened, so a store that cannot replay a
/// read refuses without writing.
pub fn events_full_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    tail: usize,
    filter: &event::EventFilter<'_>,
    sink: &mut dyn EventSink,
) -> Result<StreamedEvents> {
    events_full_with_options_with_backends(
        backends,
        selection,
        tail,
        filter,
        event::EventOptions::default(),
        sink,
    )
}

/// Stream a whole-recording event projection as `events_full_with_backends`
/// does, honoring the caller's options: `full_arguments` publishes each kept
/// tool call's complete recorded arguments rather than the bounded prefix.
pub fn events_full_with_options_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    tail: usize,
    filter: &event::EventFilter<'_>,
    options: event::EventOptions,
    sink: &mut dyn EventSink,
) -> Result<StreamedEvents> {
    let resolved = selection.resolve(backends)?;
    let backend = backends[resolved.backend_index].as_ref();
    let mut index = event::PairIndex::default();
    let mut content = content::ContentInventory::default();
    let mut title = None;
    let mut turns = 0;
    // The newest ordinal at which each call id was declared with a requested
    // program, so the window can decide which declarations it holds.
    let mut declared = HashMap::<String, usize>::new();
    backend.replayable(&resolved.session)?;
    let mut read = stream_numbered(backend, &resolved.session, None, &mut |turn| {
        turns += 1;
        content.add(&turn);
        keep_title_turn(&mut title, &turn);
        for record in event::turn_records(&turn) {
            if let Some(call_id) = record
                .event
                .call_id
                .as_ref()
                .filter(|_| filter.selects_call(&record))
            {
                declared.insert(call_id.clone(), record.ordinal);
            }
            index.observe(&record);
        }
        Ok(())
    })?;
    read.notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    let returned = turns.min(tail);
    let first = turns - returned;
    let selected = declared
        .into_iter()
        .filter_map(|(call_id, ordinal)| (ordinal >= first).then_some(call_id))
        .collect::<HashSet<_>>();
    let session = whole_session(backend, &resolved.session, &read, &title)?;
    let mut pairing = index.pairing(event::read_was_bounded(&read.source_bounds));
    let mut pairs = event::PairTally::default();
    let mut opened = false;
    stream_numbered(backend, &resolved.session, Some(&read), &mut |turn| {
        if !opened {
            sink.session(&session)?;
            opened = true;
        }
        for record in event::turn_records(&turn) {
            let record = pairing.emit(record)?;
            let kept = record.ordinal >= first
                && filter.keeps(&record, |call_id| selected.contains(call_id));
            pairs.add(&record, kept);
            if kept {
                let mut record = record;
                if options.full_arguments {
                    if let Some(arguments) = record.event.arguments.as_mut() {
                        arguments.widen();
                    }
                }
                sink.record(record)?;
            }
        }
        Ok(())
    })?;
    if !opened {
        sink.session(&session)?;
    }
    pairing.finish()?;
    let transcript = read.transcript(
        session,
        Vec::new(),
        Truncation::window(returned, turns, tail),
    );
    Ok(StreamedEvents {
        events: event::EventTranscript {
            schema: EVENTS_SCHEMA,
            session: transcript.session,
            events: Vec::new(),
            pairs: pairs.finish(),
            read: transcript.read,
            terminal: transcript.terminal,
            text_tail: transcript.text_tail,
            content: content.into_option(),
            truncated: transcript.truncated,
            truncation: transcript.truncation,
            notes: transcript.notes,
        },
        read,
    })
}

/// Keep the recording's first user turn that yields a derived title, the one
/// turn a whole session's title hint needs from the turns streamed past.
fn keep_title_turn(title: &mut Option<model::Turn>, turn: &model::Turn) {
    if title.is_none()
        && turn.role == model::Role::User
        && model::derive_title(&turn.text).is_some()
    {
        *title = Some(turn.clone());
    }
}

/// The session a whole recording states: counters, accounting, model, and
/// activity folded from every record `read` covered, and the title hint its
/// first user turn yields.
fn whole_session(
    backend: &dyn Backend,
    session: &Session,
    read: &backend::StreamedTranscript,
    title: &Option<model::Turn>,
) -> Result<Session> {
    Ok(backend
        .stream_session(session, read)?
        .with_derived_title(title.as_slice()))
}

pub fn show_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    tail: usize,
) -> Result<Transcript> {
    let resolved = selection.resolve(backends)?;
    let mut transcript = declared_transcript(
        backends[resolved.backend_index].as_ref(),
        &resolved.session,
        tail,
    )?;
    transcript
        .notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    Ok(transcript)
}

/// A bounded transcript that names the kinds its harness can record.
pub(crate) fn declared_transcript(
    backend: &dyn Backend,
    session: &Session,
    tail: usize,
) -> Result<Transcript> {
    let mut transcript = backend.transcript(session, tail)?;
    transcript.kinds = Some(backend.kinds());
    Ok(transcript)
}

/// Receives a whole-recording read as it streams: the resolved session first,
/// then each turn in recording order.
pub trait TurnSink {
    fn session(&mut self, session: &Session) -> Result<()>;
    fn turn(&mut self, turn: model::Turn) -> Result<()>;
}

/// Read one session's whole recording, streaming its turns to `sink` with
/// ordinals counted from the recording's first turn.
pub fn show_full_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    sink: &mut dyn TurnSink,
) -> Result<backend::StreamedTranscript> {
    let resolved = selection.resolve(backends)?;
    sink.session(&resolved.session)?;
    let mut read = stream_numbered(
        backends[resolved.backend_index].as_ref(),
        &resolved.session,
        None,
        &mut |turn| sink.turn(turn),
    )?;
    read.notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    Ok(read)
}

/// Stream a whole recording with ordinals counted from its first turn.
pub(crate) fn stream_numbered(
    backend: &dyn Backend,
    session: &Session,
    replay: Option<&backend::StreamedTranscript>,
    turn: &mut dyn FnMut(model::Turn) -> Result<()>,
) -> Result<backend::StreamedTranscript> {
    let mut ordinal = 0;
    let mut read = backend.stream_transcript(session, replay, &mut |mut streamed| {
        streamed.ordinal = ordinal;
        ordinal += 1;
        turn(streamed)
    })?;
    read.kinds = Some(backend.kinds());
    Ok(read)
}

/// How much of a recording an export reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportRead {
    /// The bounded read `show` takes, every turn it reaches.
    #[default]
    Bounded,
    /// The whole recording, streamed twice: once to write the JSON turns and
    /// observe tool events, and once, replaying the first read, to pair those
    /// events and write the Markdown files.
    Whole,
    /// The bounded read of a supplied input, with the source bytes behind it
    /// copied verbatim into an evidence set beside the bundle.
    Evidence,
}

pub fn export(
    selection: Selection,
    bundle: Option<&Path>,
    turns: Option<model::TurnSelection>,
    read: ExportRead,
) -> Result<bundle::Bundle> {
    export_with_backends(&backend::backends(), selection, bundle, turns, read)
}

pub fn export_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    directory: Option<&Path>,
    turns: Option<model::TurnSelection>,
    read: ExportRead,
) -> Result<bundle::Bundle> {
    let directory = directory.unwrap_or_else(|| Path::new("/tmp"));
    let resolved = selection.resolve(backends)?;
    let selection_notes = resolved.diagnostics.latest_warnings(&resolved.session);
    export_session(
        backends[resolved.backend_index].as_ref(),
        &resolved.session,
        directory,
        turns,
        read,
        &selection_notes,
    )
}

fn export_session(
    backend: &dyn Backend,
    session: &Session,
    directory: &Path,
    turns: Option<model::TurnSelection>,
    read: ExportRead,
    selection_notes: &[String],
) -> Result<bundle::Bundle> {
    match read {
        ExportRead::Bounded => {
            let mut transcript = declared_transcript(backend, session, EXPORT_TAIL)?;
            transcript.notes.extend_from_slice(selection_notes);
            bundle::export(&project_export(transcript, turns), directory)
        }
        ExportRead::Whole => export_whole(backend, session, directory, turns, selection_notes),
        ExportRead::Evidence => {
            let mut transcript = declared_transcript(backend, session, EXPORT_TAIL)?;
            transcript.notes.extend_from_slice(selection_notes);
            let transcript = project_export(transcript, turns);
            let mut bundle = bundle::export(&transcript, directory)?;
            match evidence::write(&transcript, &bundle.json.path) {
                Ok(evidence) => bundle.evidence = Some(evidence),
                Err(error) => {
                    let cleanup = bundle.discard();
                    return match cleanup {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(error.context(cleanup.to_string())),
                    };
                }
            }
            Ok(bundle)
        }
    }
}

/// A bundle of the whole recording. Memory follows one record and the tool
/// calls still awaiting a result, never the file. A file appended to between
/// the two reads is replayed to the length the first observed.
fn export_whole(
    backend: &dyn Backend,
    session: &Session,
    directory: &Path,
    turns: Option<model::TurnSelection>,
    selection_notes: &[String],
) -> Result<bundle::Bundle> {
    backend.replayable(session)?;
    let mut first = bundle::JsonPass::open(directory, session)?;
    let mut projection = turns.map(model::Projection::new);
    let read = stream_numbered(backend, session, None, &mut |turn| {
        if let Some(projection) = &mut projection {
            if !projection.admit(turn.kind) {
                return Ok(());
            }
        }
        first.turn(&turn)
    })?;
    let mut facts = read.transcript(session.clone(), Vec::new(), None);
    facts.notes.extend_from_slice(selection_notes);
    facts.projection = projection;
    let mut second = first.close(&facts)?;
    stream_numbered(backend, session, Some(&read), &mut |turn| {
        if turns.is_some_and(|kept| !kept.keeps(turn.kind)) {
            return Ok(());
        }
        second.turn(&turn)
    })?;
    second.finish()
}

/// Which sessions a command acts on in bulk, stated in the same terms a
/// listing is, so the set acted on is exactly the listed one.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionSelection<'a> {
    pub within: Where<'a>,
    pub harness: Option<&'a str>,
    pub limit: Option<usize>,
    pub filters: ListFilters<'a>,
    pub sort: ListSort,
}

/// Which project's sessions a selection could see.
#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SelectedScope {
    Here,
    Project,
    Global,
}

/// The query that produced a bulk export, recorded so the exported set can be
/// audited against the store it came from.
#[derive(Debug, Serialize)]
pub struct SelectionRecord {
    pub scope: SelectedScope,
    /// The path whose project was selected, for the `here` and `project`
    /// scopes. Absent for a global selection, and when the current directory
    /// cannot be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityWindow>,
    pub sort: ListSort,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
}

/// Where one session's three bundle files landed.
#[derive(Debug, Serialize)]
pub struct ExportedFiles {
    pub context: PathBuf,
    pub json: PathBuf,
    pub trace: PathBuf,
    /// The evidence set's manifest, when the export copied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct ExportedSession {
    pub id: String,
    pub harness: String,
    pub last_activity_at: Option<DateTime<Utc>>,
    pub files: ExportedFiles,
}

/// A selected session whose store could not be read into a bundle. The
/// selection stands; only this session is missing from it.
#[derive(Debug, Serialize)]
pub struct FailedExport {
    pub id: String,
    pub harness: String,
    pub error: String,
}

/// The one file a bulk export writes that spans its sessions: what was asked
/// for, what was written, and every diagnostic the listing produced.
#[derive(Debug, Serialize)]
pub struct ExportManifest {
    pub schema: &'static str,
    /// The build of the reader that wrote every bundle it lists.
    pub reader: crate::reader::ReaderIdentity,
    pub selection: SelectionRecord,
    /// Exported sessions, in selection order.
    pub sessions: Vec<ExportedSession>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<crate::content::ArtifactReference>,
    pub failed: Vec<FailedExport>,
    pub unavailable: Vec<String>,
    pub unreadable: Vec<String>,
    pub unsearched: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unplaced: Option<Unplaced>,
}

/// One bundle per selected session, plus the manifest that spans them.
pub struct BulkExport {
    /// Bundles in selection order, positionally matching `manifest.sessions`.
    pub bundles: Vec<bundle::Bundle>,
    pub manifest: ExportManifest,
    pub manifest_file: bundle::BundleFile,
}

impl BulkExport {
    /// Nothing selected could be read. The manifest still records the
    /// selection and every diagnostic, so the run is auditable, but the caller
    /// asked for sessions and holds none.
    pub fn every_session_failed(&self) -> bool {
        self.bundles.is_empty() && !self.manifest.failed.is_empty()
    }
}

pub fn export_selection(
    selection: &SessionSelection<'_>,
    directory: Option<&Path>,
    turns: Option<model::TurnSelection>,
    read: ExportRead,
) -> Result<BulkExport> {
    export_selection_with_backends(&backend::backends(), selection, directory, turns, read)
}

/// Export every session a listing with the same filters would return, in the
/// listing's order, one bundle each, read as `read` says.
///
/// The listing is computed once and its sessions are read through the backend
/// that produced them: an id re-resolved against every store could reach a
/// different session, or none, and the manifest would then describe a set that
/// was never exported.
pub fn export_selection_with_backends(
    backends: &[Box<dyn Backend>],
    selection: &SessionSelection<'_>,
    directory: Option<&Path>,
    turns: Option<model::TurnSelection>,
    read: ExportRead,
) -> Result<BulkExport> {
    let scope = selection.within.resolve()?;
    let limit = selection.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let listed = list_scoped(
        backends,
        selection.harness,
        scope.as_ref(),
        limit,
        &selection.filters,
        selection.sort,
    )?;
    let directory = directory.unwrap_or_else(|| Path::new("/tmp"));
    let unplaced = Unplaced::from_directories(listed.unplaced.clone());

    let mut bundles = Vec::new();
    let mut sessions = Vec::new();
    let mut failed = Vec::new();
    for (session, origin) in listed.sessions.into_iter().zip(listed.origins) {
        match export_session(
            backends[origin].as_ref(),
            &session,
            directory,
            turns,
            read,
            &[],
        ) {
            Ok(bundle) => {
                sessions.push(ExportedSession {
                    id: session.id.clone(),
                    harness: session.harness().to_owned(),
                    last_activity_at: session.last_activity_at,
                    files: ExportedFiles {
                        context: bundle.context.path.clone(),
                        json: bundle.json.path.clone(),
                        trace: bundle.trace.path.clone(),
                        evidence: bundle.evidence.as_ref().map(|file| file.path.clone()),
                    },
                });
                bundles.push(bundle);
            }
            Err(error) => failed.push(FailedExport {
                id: session.id.clone(),
                harness: session.harness().to_owned(),
                error: format!("{error:#}"),
            }),
        }
    }

    let manifest = ExportManifest {
        schema: EXPORT_MANIFEST_SCHEMA,
        reader: crate::reader::identity(),
        selection: selection_record(selection, limit),
        sessions,
        artifacts: listed.artifacts,
        failed,
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
        unplaced,
    };
    let body = serde_json::to_string_pretty(&manifest)
        .context("failed to serialize the export manifest")?;
    let manifest_file = bundle::write_manifest(directory, &body)?;

    Ok(BulkExport {
        bundles,
        manifest,
        manifest_file,
    })
}

/// What a selection of sessions spent, grouped. `selection` restates the
/// query that chose the set, and the listing's own diagnostics are carried
/// verbatim, so a total can be audited against the store it came from.
#[derive(Debug, Serialize)]
pub struct UsageSummary {
    pub schema: &'static str,
    pub selection: SelectionRecord,
    pub groups: Vec<usage::UsageGroup>,
    /// Every selected session, whatever it was grouped under.
    pub totals: usage::UsageTally,
    pub partitions: Vec<usage::AccountingPartition>,
    pub unavailable: Vec<String>,
    pub unreadable: Vec<String>,
    pub unsearched: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unplaced: Option<Unplaced>,
}

pub fn usage_summary(
    selection: &SessionSelection<'_>,
    by: &[usage::GroupBy],
) -> Result<UsageSummary> {
    usage_summary_with_backends(&backend::backends(), selection, by)
}

/// Sum the recorded counters of every session a listing with the same filters
/// would return, grouped by the requested dimensions.
///
/// The listing carries each session's counters and its accounting, so the
/// summary is a projection over it and no transcript is read.
pub fn usage_summary_with_backends(
    backends: &[Box<dyn Backend>],
    selection: &SessionSelection<'_>,
    by: &[usage::GroupBy],
) -> Result<UsageSummary> {
    let scope = selection.within.resolve()?;
    let limit = selection.limit.unwrap_or(DEFAULT_LIST_LIMIT);
    let listed = list_scoped(
        backends,
        selection.harness,
        scope.as_ref(),
        limit,
        &selection.filters,
        selection.sort,
    )?;
    let (aggregate, partitions) = usage::partitioned_aggregate(&listed.sessions, by);
    Ok(UsageSummary {
        schema: USAGE_SUMMARY_SCHEMA,
        selection: selection_record(selection, limit),
        groups: aggregate.groups,
        totals: aggregate.totals,
        partitions,
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
        unplaced: Unplaced::from_directories(listed.unplaced),
    })
}

pub(crate) fn selection_record(selection: &SessionSelection<'_>, limit: usize) -> SelectionRecord {
    let (scope, project) = match selection.within {
        Where::Here => (SelectedScope::Here, std::env::current_dir().ok()),
        Where::Project(path) => (SelectedScope::Project, Some(path.to_path_buf())),
        Where::Global => (SelectedScope::Global, None),
    };
    let filters = &selection.filters;
    SelectionRecord {
        scope,
        project,
        harness: selection.harness.map(str::to_owned),
        model: filters.model.map(str::to_owned),
        directory: filters.directory.map(str::to_owned),
        activity: (filters.since.is_some() || filters.until.is_some()).then_some(ActivityWindow {
            since: filters.since,
            until: filters.until,
        }),
        sort: selection.sort,
        limit,
        search: filters.search.map(str::to_owned),
    }
}
