//! Whole-recording search over metadata-selected sessions.
use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{backend::Backend, model, scope::Scope, ListFilters, ListSort, SessionList};

#[derive(Debug, Serialize, Deserialize)]
pub struct Match {
    pub ordinal: usize,
    pub kind: model::TurnKind,
    pub ts: Option<chrono::DateTime<chrono::Utc>>,
    pub record_ref: Option<model::RecordRef>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSearch {
    pub session: String,
    pub harness: String,
    pub matches: Vec<Match>,
    pub matches_truncated: bool,
    /// All source records were read and normalized without gaps or bounds.
    pub whole_recording_read: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FullSearch {
    pub sessions_read: usize,
    pub selection_truncated: bool,
    pub match_limit: usize,
    /// Coverage includes nonmatching sessions as well as matching sessions.
    pub coverage: Vec<SessionSearch>,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub limit: usize,
    pub sort: ListSort,
    pub match_limit: usize,
}

pub fn list(
    backends: &[Box<dyn Backend>],
    harness: Option<&str>,
    scope: Option<&Scope>,
    filters: &ListFilters<'_>,
    options: Options,
) -> Result<SessionList> {
    let Options {
        limit,
        sort,
        match_limit,
    } = options;
    let metadata = ListFilters {
        search: None,
        ..*filters
    };
    let mut selected = crate::list_scoped(
        backends,
        harness,
        scope,
        limit.saturating_add(1),
        &metadata,
        sort,
    )?;
    let selection_truncated = selected.scan_truncated || selected.sessions.len() > limit;
    let mut result = crate::list_with_backends_options(&[], None, None, 0, &metadata, sort)?;
    result.unavailable = selected.unavailable;
    result.unreadable = selected.unreadable;
    result.scanned = selected.scanned;
    result.scan_truncated = selected.scan_truncated;
    result.unplaced = crate::Unplaced::from_directories(selected.unplaced);
    result.artifacts = selected.artifacts;
    let needle = filters.search.unwrap_or_default().to_lowercase();
    let mut report = FullSearch {
        sessions_read: 0,
        selection_truncated,
        match_limit,
        coverage: Vec::new(),
    };
    let mut unsupported = std::collections::HashSet::new();
    for (session, origin) in selected
        .sessions
        .drain(..)
        .zip(selected.origins)
        .take(limit)
    {
        let mut coverage = SessionSearch {
            session: session.id.clone(),
            harness: session.harness().to_owned(),
            matches: Vec::new(),
            matches_truncated: false,
            whole_recording_read: false,
        };
        let mut ordinal = 0;
        let mut matched = false;
        if !backends[origin].supports_full_search() {
            if unsupported.insert(origin) {
                result.unsearched.push(format!(
                    "{} store {} is not searchable in full",
                    backends[origin].harness(),
                    backends[origin]
                        .store()
                        .unwrap_or_else(|| backends[origin].harness().to_owned())
                ));
            }
            report.coverage.push(coverage);
            continue;
        }
        report.sessions_read += 1;
        let read = backends[origin].stream_transcript(&session, None, &mut |turn| {
            if turn.text.to_lowercase().contains(&needle) {
                matched = true;
                if coverage.matches.len() < match_limit {
                    coverage.matches.push(Match {
                        ordinal,
                        kind: turn.kind,
                        ts: turn.ts,
                        record_ref: turn.record_ref,
                    });
                } else {
                    coverage.matches_truncated = true;
                }
            }
            ordinal += 1;
            Ok(())
        });
        match read {
            Ok(read) => {
                coverage.whole_recording_read = read.skipped == 0
                    && read.gaps.is_empty()
                    && read.source_bounds.is_empty()
                    && read.trailing_record.is_none()
            }
            Err(error) => {
                result
                    .unsearched
                    .push(format!("{} {}: {error:#}", session.harness(), session.id))
            }
        }
        if matched {
            result.sessions.push(session);
        }
        report.coverage.push(coverage);
    }
    result.full_search = Some(report);
    Ok(result)
}
