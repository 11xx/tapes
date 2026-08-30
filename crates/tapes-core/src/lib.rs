use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::Serialize;

use backend::{Backend, Listing, Query};
use model::{Session, Transcript};
use scope::Scope;

pub mod backend;
pub mod bundle;
pub mod model;
pub mod scope;

pub const LIST_SCHEMA: &str = "tapes-list/1";
const DEFAULT_LIST_LIMIT: usize = 20;
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

#[derive(Debug, Serialize)]
pub struct SessionList {
    pub schema: &'static str,
    pub sessions: Vec<Session>,
    pub unavailable: Vec<String>,
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
    let scope = within.resolve()?;
    list_with_backends_filtered(
        &backend::backends(),
        harness,
        scope.as_ref(),
        limit.unwrap_or(DEFAULT_LIST_LIMIT),
        model,
        directory,
    )
}

pub fn list_with_backends(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
) -> Result<SessionList> {
    list_with_backends_filtered(backends, harness, scope, limit, None, None)
}

pub fn list_with_backends_filtered(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
    model: Option<&str>,
    directory: Option<&str>,
) -> Result<SessionList> {
    let listed = list_scoped(backends, harness, scope, limit, model, directory)?;
    Ok(SessionList {
        schema: LIST_SCHEMA,
        sessions: listed.sessions,
        unavailable: listed.unavailable,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
    })
}

struct Listed {
    sessions: Vec<Session>,
    /// Which backend each session came from, positionally — kept so a
    /// selection can go straight to its transcript without resolving the id
    /// against every store again.
    origins: Vec<usize>,
    unavailable: Vec<String>,
    scanned: usize,
    scan_truncated: bool,
}

fn list_scoped(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    limit: usize,
    model: Option<&str>,
    directory: Option<&str>,
) -> Result<Listed> {
    if let Some(harness) = harness {
        if !backends.iter().any(|backend| backend.harness() == harness) {
            return Err(anyhow!("unknown harness: {harness}"));
        }
    }

    let query = Query::scoped_with_filters(
        scope,
        limit,
        if scope.is_some() {
            SCAN_CEILING
        } else {
            usize::MAX
        },
        model,
        directory,
    );
    let mut found: Vec<(Session, usize)> = Vec::new();
    let mut available_harnesses = HashSet::new();
    let mut unavailable_harnesses = Vec::new();
    let mut scanned = 0;
    let mut scan_truncated = false;
    for (index, backend) in backends
        .iter()
        .enumerate()
        .filter(|(_, backend)| harness.is_none_or(|name| backend.harness() == name))
    {
        if !backend.available() {
            unavailable_harnesses.push(backend.harness());
            continue;
        }
        match backend.list(&query) {
            Ok(Listing {
                sessions,
                scanned: inspected,
                scan_truncated: truncated,
            }) => {
                available_harnesses.insert(backend.harness().to_owned());
                scanned += inspected;
                scan_truncated |= truncated;
                found.extend(sessions.into_iter().map(|session| (session, index)));
            }
            Err(_) => unavailable_harnesses.push(backend.harness()),
        }
    }
    // Stable OpenCode and opencode2 can expose the same global session id from
    // different projections. The first backend wins, preserving one session
    // row and its transcript origin instead of inventing ambiguity.
    let mut opencode_ids = HashSet::new();
    found.retain(|(session, origin)| {
        backends[*origin].harness() != "opencode" || opencode_ids.insert(session.id.clone())
    });
    found.sort_by_key(|(session, _)| session.last_activity_at);
    found.reverse();

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
        scanned,
        scan_truncated,
    })
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
    let listed = list_scoped(
        backends,
        harness,
        scope,
        exclude.len() + LATEST_WINDOW,
        None,
        None,
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
