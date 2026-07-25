use std::path::Path;

use anyhow::{anyhow, Result};
use serde::Serialize;

use backend::Backend;
use model::{Session, Transcript};

pub mod backend;
pub mod bundle;
pub mod model;

pub const LIST_SCHEMA: &str = "tapes-list/1";
const DEFAULT_LIST_LIMIT: usize = 20;
const RESOLVE_LIMIT: usize = 1_000;
/// An export is a rescue: take the whole session the bounded read allows,
/// not the window `show` defaults to.
const EXPORT_TAIL: usize = usize::MAX;

#[derive(Debug, Serialize)]
pub struct SessionList {
    pub schema: &'static str,
    pub sessions: Vec<Session>,
    pub unavailable: Vec<String>,
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

pub fn list(harness: Option<&str>, here: bool, limit: Option<usize>) -> Result<SessionList> {
    let directory = here
        .then(std::env::current_dir)
        .transpose()
        .map_err(anyhow::Error::from)?;
    list_with_backends(
        &backend::backends(),
        harness,
        directory.as_deref(),
        limit.unwrap_or(DEFAULT_LIST_LIMIT),
    )
}

pub fn list_with_backends(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    directory: Option<&Path>,
    limit: usize,
) -> Result<SessionList> {
    if let Some(harness) = harness {
        if !backends.iter().any(|backend| backend.harness() == harness) {
            return Err(anyhow!("unknown harness: {harness}"));
        }
    }

    let mut sessions = Vec::new();
    let mut unavailable = Vec::new();
    for backend in backends
        .iter()
        .filter(|backend| harness.is_none_or(|name| backend.harness() == name))
    {
        if !backend.available() {
            unavailable.push(backend.harness().to_owned());
            continue;
        }
        match backend.list(limit) {
            Ok(listed) => sessions.extend(listed.into_iter().filter(|session| {
                directory.is_none_or(|directory| session.directory.as_deref() == Some(directory))
            })),
            Err(_) => unavailable.push(backend.harness().to_owned()),
        }
    }
    sessions.sort_by_key(|session| session.last_activity_at);
    sessions.reverse();

    Ok(SessionList {
        schema: LIST_SCHEMA,
        sessions,
        unavailable,
    })
}

pub fn resolve_session(
    backends: &[Box<dyn Backend>],
    query: &str,
) -> std::result::Result<ResolvedSession, ResolveError> {
    // Fast path: an exact id never consults a listing. A backend that errors
    // here is treated as a miss, so one broken store cannot stop another from
    // resolving — the same tolerance `list` already applies.
    // No `available()` probe here on purpose. Availability is a listing
    // concern — `list` reports which harnesses it could not reach. On an
    // exact-id path a miss is a miss however it arises, and probing costs a
    // second process spawn for API-backed harnesses.
    let mut located = Vec::new();
    for (backend_index, backend) in backends.iter().enumerate() {
        if let Ok(Some(session)) = backend.locate(query) {
            if session.id == query {
                located.push(ResolvedSession {
                    backend_index,
                    session,
                });
            }
        }
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
        let Ok(sessions) = backend.list(RESOLVE_LIMIT) else {
            continue;
        };
        truncated |= sessions.len() >= RESOLVE_LIMIT;
        matches.extend(
            sessions
                .into_iter()
                .filter(|session| session.id.starts_with(query))
                .map(|session| ResolvedSession {
                    backend_index,
                    session,
                }),
        );
    }

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

pub fn show(session: &str, tail: Option<usize>) -> Result<Transcript> {
    show_with_backends(&backend::backends(), session, tail.unwrap_or(100))
}

pub fn show_with_backends(
    backends: &[Box<dyn Backend>],
    session: &str,
    tail: usize,
) -> Result<Transcript> {
    let resolved = resolve_session(backends, session)?;
    backends[resolved.backend_index].transcript(&resolved.session.id, tail)
}

pub fn export(session: &str, bundle: Option<&Path>) -> Result<bundle::Bundle> {
    export_with_backends(&backend::backends(), session, bundle)
}

pub fn export_with_backends(
    backends: &[Box<dyn Backend>],
    session: &str,
    directory: Option<&Path>,
) -> Result<bundle::Bundle> {
    let transcript = show_with_backends(backends, session, EXPORT_TAIL)?;
    bundle::export(&transcript, directory.unwrap_or_else(|| Path::new("/tmp")))
}
