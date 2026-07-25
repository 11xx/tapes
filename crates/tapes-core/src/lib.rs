use std::path::Path;

use anyhow::{anyhow, Result};
use serde::Serialize;

use backend::Backend;
use model::{Session, Transcript};

pub mod backend;
pub mod model;

pub const LIST_SCHEMA: &str = "tapes-list/1";
const DEFAULT_LIST_LIMIT: usize = 20;
const RESOLVE_LIMIT: usize = 1_000;

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
    NotFound(String),
    Ambiguous {
        query: String,
        candidates: Vec<SessionCandidate>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(query) => write!(formatter, "session {query} was not found"),
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
    let mut matches = Vec::new();
    for (backend_index, backend) in backends.iter().enumerate() {
        if !backend.available() {
            continue;
        }
        let Ok(sessions) = backend.list(RESOLVE_LIMIT) else {
            continue;
        };
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
        0 => Err(ResolveError::NotFound(query.to_owned())),
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

pub fn export(_session: &str, _bundle: Option<&Path>) -> &'static str {
    "export is not implemented"
}
