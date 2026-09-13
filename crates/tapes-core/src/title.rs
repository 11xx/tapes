//! Exact recorded-title resolution with bounded, explicit evidence.
use std::collections::HashSet;

use anyhow::{anyhow, Result};

use crate::backend::{Backend, Query};
use crate::scope::Scope;
use crate::ResolvedSession;

const TITLE_SCAN_LIMIT: usize = 5_000;

pub fn resolve(
    backends: &[Box<dyn Backend>],
    title: &str,
    harness: Option<&str>,
    scope: Option<&Scope>,
) -> Result<ResolvedSession> {
    if harness.is_some_and(|name| !backends.iter().any(|b| b.harness() == name)) {
        return Err(anyhow!("unknown harness: {}", harness.unwrap()));
    }
    let query = Query::scoped_with_filters(
        scope,
        TITLE_SCAN_LIMIT,
        TITLE_SCAN_LIMIT,
        None,
        None,
        None,
        None,
    );
    let mut matches = Vec::new();
    let mut incomplete = Vec::new();
    let mut opencode_ids = HashSet::new();
    for (origin, backend) in backends.iter().enumerate() {
        if harness.is_some_and(|name| name != backend.harness()) || !backend.available() {
            continue;
        }
        let listing = match backend.list_titles(&query) {
            Ok(listing) => listing,
            Err(error) => {
                incomplete.push(format!("{}: {error:#}", backend.harness()));
                continue;
            }
        };
        if listing.scan_truncated || listing.sessions.len() >= TITLE_SCAN_LIMIT {
            incomplete.push(format!(
                "{}: title scan stopped at {TITLE_SCAN_LIMIT} candidates",
                backend.harness()
            ));
        }
        incomplete.extend(listing.unavailable);
        incomplete.extend(listing.unsearched);
        for session in listing.sessions {
            if backend.harness() == "opencode" && !opencode_ids.insert(session.id.clone()) {
                continue;
            }
            if session.title.as_deref() == Some(title) {
                matches.push(ResolvedSession {
                    backend_index: origin,
                    session,
                });
            }
        }
    }
    let candidates = matches
        .iter()
        .map(|found| {
            let coordinate = found
                .session
                .occurrence
                .as_deref()
                .map_or_else(String::new, |occurrence| {
                    format!("; occurrence {occurrence}")
                });
            format!(
                "  {} ({}) [{}{}]",
                found.session.id,
                found.session.harness(),
                found.session.locator().unwrap_or("origin unavailable"),
                coordinate
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    if matches.len() > 1 {
        return Err(anyhow!(
            "recorded title {title:?} is ambiguous:\n{candidates}"
        ));
    }
    if !incomplete.is_empty() {
        return Err(anyhow!("recorded title {title:?}: incomplete lookup; choose an explicit ID or narrower scope\nobserved candidates:\n{candidates}\n{}", incomplete.join("\n")));
    }
    matches
        .pop()
        .ok_or_else(|| anyhow!("recorded title {title:?} was not found in the selected scope"))
}
