use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Serialize;

use backend::{Backend, Listing, Query};
use model::{Session, Transcript};
use scope::Scope;

pub use event::{
    Bounded, EventKind, EventRecord, EventTranscript, Incomplete, PairCounts, PairRef, ToolEvent,
    EVENTS_SCHEMA,
};

pub mod backend;
pub mod brief;
pub mod bundle;
pub mod endings;
pub mod event;
pub mod lineage;
pub mod model;
pub mod scope;
pub mod stats;
pub mod stats_summary;
pub mod title;
pub mod usage;

pub const LIST_SCHEMA: &str = "tapes-list/1";
pub const EXPORT_MANIFEST_SCHEMA: &str = "tapes-export-manifest/1";
pub const USAGE_SUMMARY_SCHEMA: &str = "tapes-usage-summary/1";
/// Number of normalized turns a `list --search` query inspects per session.
/// Keeping this fixed makes the listing's cost predictable for callers.
pub const LIST_SEARCH_TAIL: usize = 32;
pub(crate) const DEFAULT_LIST_LIMIT: usize = 20;
const RESOLVE_LIMIT: usize = 1_000;
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Serialize)]
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCandidate {
    pub id: String,
    pub harness: String,
}

#[derive(Debug)]
pub enum ResolveError {
    NotFound {
        query: String,
        /// A backend returned exactly as many sessions as the enumeration cap
        /// allows, so the search may have stopped short of the store. "I
        /// stopped looking" is a different fact from "it is not there".
        truncated: bool,
    },
    Ambiguous {
        query: String,
        candidates: Vec<SessionCandidate>,
    },
    /// Nothing resolved and at least one backend failed while being asked.
    /// Reported instead of `NotFound` because "the store is broken" sends an
    /// operator somewhere entirely different from "no such session".
    BackendFailed {
        query: String,
        failures: Vec<(String, String)>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { query, truncated } => {
                write!(formatter, "session {query} was not found")?;
                if *truncated {
                    write!(
                        formatter,
                        " (the search stopped at {RESOLVE_LIMIT} sessions per harness; pass the full id to look it up directly)"
                    )?;
                }
                Ok(())
            }
            Self::BackendFailed { query, failures } => {
                writeln!(formatter, "session {query} could not be resolved:")?;
                for (harness, error) in failures {
                    writeln!(formatter, "  {harness}: {error}")?;
                }
                Ok(())
            }
            Self::Ambiguous { query, candidates } => {
                writeln!(formatter, "session prefix {query} is ambiguous:")?;
                for candidate in candidates {
                    writeln!(formatter, "  {} ({})", candidate.id, candidate.harness)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

#[derive(Debug)]
pub struct ResolvedSession {
    pub backend_index: usize,
    pub session: Session,
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
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
    })
}

pub(crate) struct Listed {
    pub(crate) sessions: Vec<Session>,
    /// Which backend each session came from, positionally — kept so a
    /// selection can go straight to its transcript without resolving the id
    /// against every store again.
    pub(crate) origins: Vec<usize>,
    pub(crate) unavailable: Vec<String>,
    pub(crate) unreadable: Vec<String>,
    pub(crate) unsearched: Vec<String>,
    pub(crate) scanned: usize,
    pub(crate) scan_truncated: bool,
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
    let query = Query::scoped_with_filters(
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
    let mut found: Vec<(Session, usize)> = Vec::new();
    let mut available_harnesses = HashSet::new();
    let mut unavailable_harnesses = Vec::new();
    let mut unreadable_sessions = Vec::new();
    let mut unsearched_sessions = Vec::new();
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
    let mut seen_search_candidates = HashMap::<String, HashSet<String>>::new();
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
        let candidate_ids = if filters.search.is_some()
            && selected_harness_counts
                .get(backend.harness())
                .is_some_and(|count| *count > 1)
        {
            match backend.list(&query) {
                Ok(listing) => Some(
                    listing
                        .sessions
                        .into_iter()
                        .map(|session| session.id)
                        .collect::<HashSet<_>>(),
                ),
                Err(error) => {
                    unsearched_sessions.push(format!(
                        "{} search could not reconcile duplicate projections: {error:#}",
                        backend.harness()
                    ));
                    None
                }
            }
        } else {
            None
        };
        let listing = if let Some(needle) = filters.search {
            backend.list_with_search(&query, needle, LIST_SEARCH_TAIL)
        } else {
            backend.list(&query)
        };
        match listing {
            Ok(Listing {
                mut sessions,
                unavailable,
                unsearched,
                scanned: inspected,
                scan_truncated: truncated,
            }) => {
                if let Some(candidate_ids) = candidate_ids {
                    let seen = seen_search_candidates
                        .entry(backend.harness().to_owned())
                        .or_default();
                    sessions.retain(|session| !seen.contains(&session.id));
                    seen.extend(candidate_ids);
                }
                available_harnesses.insert(backend.harness().to_owned());
                unreadable_sessions.extend(unavailable);
                unsearched_sessions.extend(unsearched);
                scanned += inspected;
                scan_truncated |= truncated;
                found.extend(sessions.into_iter().map(|session| (session, index)));
            }
            Err(error) => {
                if filters.search.is_some() {
                    unsearched_sessions
                        .push(format!("{} search failed: {error:#}", backend.harness()));
                }
                if let Some(candidate_ids) = candidate_ids {
                    seen_search_candidates
                        .entry(backend.harness().to_owned())
                        .or_default()
                        .extend(candidate_ids);
                }
                unavailable_harnesses.push(backend.harness());
            }
        }
    }
    // Stable OpenCode and opencode2 can expose the same global session id from
    // different projections. The first backend wins, preserving one session
    // row and its transcript origin instead of inventing ambiguity.
    let mut opencode_ids = HashSet::new();
    found.retain(|(session, origin)| {
        backends[*origin].harness() != "opencode" || opencode_ids.insert(session.id.clone())
    });
    found.sort_by(|(left, _), (right, _)| compare_sessions(left, right, sort));

    // A harness may have more than one installed store, such as stable
    // OpenCode and opencode2. Keep the public limit per harness, not per store.
    let mut returned = HashMap::<String, usize>::new();
    let (sessions, origins) = found
        .into_iter()
        .filter_map(|(session, origin)| {
            let count = returned.entry(session.harness.clone()).or_default();
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
        origins,
        unavailable,
        unreadable: unreadable_sessions,
        unsearched: unsearched_sessions,
        scanned,
        scan_truncated,
    })
}

fn compare_sessions(left: &Session, right: &Session, sort: ListSort) -> Ordering {
    let activity = match sort {
        ListSort::Newest => right.last_activity_at.cmp(&left.last_activity_at),
        ListSort::Oldest => left.last_activity_at.cmp(&right.last_activity_at),
    };
    activity
        .then_with(|| left.id.cmp(&right.id))
        .then_with(|| left.harness.cmp(&right.harness))
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
    listed
        .sessions
        .into_iter()
        .zip(listed.origins)
        .filter(|(session, _)| !exclude.contains(&session.id))
        .max_by_key(|(session, _)| session.last_activity_at)
        .map(|(session, backend_index)| ResolvedSession {
            backend_index,
            session,
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
    // Fast path: an exact id never consults a listing. A backend that errors
    // here does not stop another from resolving, but the error is kept: if
    // nothing resolves it surfaces as `BackendFailed`, because "the store is
    // broken" and "no such session" send an operator to different places.
    // No `available()` probe here on purpose. Availability is a listing
    // concern — `list` reports which harnesses it could not reach. On an
    // exact-id path a miss is a miss however it arises, and probing costs a
    // second process spawn for API-backed harnesses.
    let mut located = Vec::new();
    let mut failures = Vec::new();
    for (backend_index, backend) in backends.iter().enumerate() {
        match backend.locate(query) {
            Ok(Some(session)) if session.id == query => located.push(ResolvedSession {
                backend_index,
                session,
            }),
            Ok(_) => {}
            // Kept, not discarded: a hit elsewhere still wins, but if nothing
            // resolves the real failure has to surface rather than hide behind
            // "not found".
            Err(error) => failures.push((backend.harness().to_owned(), format!("{error:#}"))),
        }
    }
    if located.len() > 1
        && located
            .iter()
            .all(|resolved| backends[resolved.backend_index].harness() == "opencode")
    {
        return Ok(located.remove(0));
    }
    match located.len() {
        1 => return Ok(located.pop().expect("one match is present")),
        0 => {}
        _ => {
            let mut candidates = located
                .into_iter()
                .map(|resolved| SessionCandidate {
                    id: resolved.session.id,
                    harness: resolved.session.harness,
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| {
                left.id
                    .cmp(&right.id)
                    .then(left.harness.cmp(&right.harness))
            });
            return Err(ResolveError::Ambiguous {
                query: query.to_owned(),
                candidates,
            });
        }
    }

    // Fallback: a prefix query genuinely needs enumeration, because ambiguity
    // can only be seen across the whole set.
    let mut matches = Vec::new();
    let mut truncated = false;
    for (backend_index, backend) in backends.iter().enumerate() {
        if !backend.available() {
            continue;
        }
        let Ok(listing) = backend.list(&Query::unscoped(RESOLVE_LIMIT)) else {
            continue;
        };
        truncated |= listing.sessions.len() >= RESOLVE_LIMIT;
        matches.extend(
            listing
                .sessions
                .into_iter()
                .filter(|session| session.id.starts_with(query))
                .map(|session| ResolvedSession {
                    backend_index,
                    session,
                }),
        );
    }

    let mut opencode_ids = HashSet::new();
    matches.retain(|resolved| {
        backends[resolved.backend_index].harness() != "opencode"
            || opencode_ids.insert(resolved.session.id.clone())
    });

    let exact = matches
        .iter()
        .enumerate()
        .filter_map(|(index, resolved)| (resolved.session.id == query).then_some(index))
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return Ok(matches.swap_remove(exact[0]));
    }
    if exact.len() > 1 {
        matches.retain(|resolved| resolved.session.id == query);
    }
    match matches.len() {
        0 if !failures.is_empty() => Err(ResolveError::BackendFailed {
            query: query.to_owned(),
            failures,
        }),
        0 => Err(ResolveError::NotFound {
            query: query.to_owned(),
            truncated,
        }),
        1 => Ok(matches.pop().expect("one match is present")),
        _ => {
            let mut candidates = matches
                .into_iter()
                .map(|resolved| SessionCandidate {
                    id: resolved.session.id,
                    harness: resolved.session.harness,
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|left, right| {
                left.id
                    .cmp(&right.id)
                    .then(left.harness.cmp(&right.harness))
            });
            Err(ResolveError::Ambiguous {
                query: query.to_owned(),
                candidates,
            })
        }
    }
}

/// Which session a command was asked for: one named by the caller, or the
/// latest in a scope.
#[derive(Clone, Debug)]
pub enum Selection<'a> {
    Id(&'a str),
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

/// One session's recorded relatives. The read never opens a child's turns:
/// a relationship is a reference, and `show` under the child's own id is how
/// its transcript is read.
pub fn lineage(selection: Selection) -> Result<lineage::LineageView> {
    lineage_with_backends(&backend::backends(), selection)
}

pub fn lineage_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
) -> Result<lineage::LineageView> {
    let resolved = selection.resolve(backends)?;
    let read = backends[resolved.backend_index].lineage(&resolved.session)?;
    Ok(lineage::view(&resolved.session, read))
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
    let backend = &backends[resolved.backend_index];
    let transcript = backend.transcript(&resolved.session, EXPORT_TAIL)?;
    let lineage = backend.lineage(&resolved.session)?;
    Ok(stats::stats(transcript, &lineage))
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
    let transcript = backends[resolved.backend_index].transcript(&resolved.session, EXPORT_TAIL)?;
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
    let resolved = selection.resolve(backends)?;
    backends[resolved.backend_index].events(&resolved.session, tail)
}

pub fn show_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    tail: usize,
) -> Result<Transcript> {
    let resolved = selection.resolve(backends)?;
    backends[resolved.backend_index].transcript(&resolved.session, tail)
}

pub fn export(selection: Selection, bundle: Option<&Path>) -> Result<bundle::Bundle> {
    export_with_backends(&backend::backends(), selection, bundle)
}

pub fn export_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    directory: Option<&Path>,
) -> Result<bundle::Bundle> {
    let transcript = show_with_backends(backends, selection, EXPORT_TAIL)?;
    bundle::export(&transcript, directory.unwrap_or_else(|| Path::new("/tmp")))
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
}

#[derive(Debug, Serialize)]
pub struct ExportedSession {
    pub id: String,
    pub harness: String,
    pub last_activity_at: DateTime<Utc>,
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
    pub selection: SelectionRecord,
    /// Exported sessions, in selection order.
    pub sessions: Vec<ExportedSession>,
    pub failed: Vec<FailedExport>,
    pub unavailable: Vec<String>,
    pub unreadable: Vec<String>,
    pub unsearched: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
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
) -> Result<BulkExport> {
    export_selection_with_backends(&backend::backends(), selection, directory)
}

/// Export every session a listing with the same filters would return, in the
/// listing's order, one bounded bundle each.
///
/// The listing is computed once and its sessions are read through the backend
/// that produced them: an id re-resolved against every store could reach a
/// different session, or none, and the manifest would then describe a set that
/// was never exported.
pub fn export_selection_with_backends(
    backends: &[Box<dyn Backend>],
    selection: &SessionSelection<'_>,
    directory: Option<&Path>,
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

    let mut bundles = Vec::new();
    let mut sessions = Vec::new();
    let mut failed = Vec::new();
    for (session, origin) in listed.sessions.into_iter().zip(listed.origins) {
        match backends[origin]
            .transcript(&session, EXPORT_TAIL)
            .and_then(|transcript| bundle::export(&transcript, directory))
        {
            Ok(bundle) => {
                sessions.push(ExportedSession {
                    id: session.id,
                    harness: session.harness,
                    last_activity_at: session.last_activity_at,
                    files: ExportedFiles {
                        context: bundle.context.path.clone(),
                        json: bundle.json.path.clone(),
                        trace: bundle.trace.path.clone(),
                    },
                });
                bundles.push(bundle);
            }
            Err(error) => failed.push(FailedExport {
                id: session.id,
                harness: session.harness,
                error: format!("{error:#}"),
            }),
        }
    }

    let manifest = ExportManifest {
        schema: EXPORT_MANIFEST_SCHEMA,
        selection: selection_record(selection, limit),
        sessions,
        failed,
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
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
    pub unavailable: Vec<String>,
    pub unreadable: Vec<String>,
    pub unsearched: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
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
    let aggregate = usage::aggregate(&listed.sessions, by);
    Ok(UsageSummary {
        schema: USAGE_SUMMARY_SCHEMA,
        selection: selection_record(selection, limit),
        groups: aggregate.groups,
        totals: aggregate.totals,
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
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
