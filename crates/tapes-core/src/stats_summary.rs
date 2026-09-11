//! Tool statistics over one bounded read per selected backend origin.
use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;

use crate::backend::{self, Backend};
use crate::event;
use crate::stats::{Coverage, PairCoverage, ToolStats};
use crate::usage::{self, UsageSession};
use crate::{list_scoped, selection_record, SelectionRecord, SessionSelection, DEFAULT_LIST_LIMIT};

pub const SCHEMA: &str = "tapes-stats-summary/1";

#[derive(Debug, Serialize)]
pub struct SessionStats {
    pub session: UsageSession,
    pub coverage: Coverage,
    pub tools: ToolStats,
}

#[derive(Debug, Serialize)]
pub struct ReadFailure {
    pub id: String,
    pub harness: String,
    pub store: Option<String>,
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct StatsSummary {
    pub schema: &'static str,
    pub selection: SelectionRecord,
    pub selected: usize,
    pub read: usize,
    pub failed: Vec<ReadFailure>,
    pub sessions: Vec<SessionStats>,
    pub by_harness: BTreeMap<String, ToolStats>,
    pub unavailable: Vec<String>,
    pub unreadable: Vec<String>,
    pub unsearched: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
}

pub fn summary(selection: &SessionSelection<'_>) -> Result<StatsSummary> {
    with_backends(&backend::backends(), selection)
}

pub fn with_backends(
    backends: &[Box<dyn Backend>],
    selection: &SessionSelection<'_>,
) -> Result<StatsSummary> {
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
    let mut report = StatsSummary {
        schema: SCHEMA,
        selection: selection_record(selection, limit),
        selected: listed.sessions.len(),
        read: 0,
        failed: vec![],
        sessions: vec![],
        by_harness: BTreeMap::new(),
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
    };
    for (session, origin) in listed.sessions.into_iter().zip(listed.origins) {
        match backends[origin].transcript(&session, usize::MAX) {
            Ok(transcript) => {
                let usage = usage::usage(&transcript);
                let events = event::project(transcript, usize::MAX);
                let tools = crate::stats::count_tools(&events.events, events.pairs.complete);
                merge(
                    report.by_harness.entry(session.harness).or_default(),
                    &tools,
                );
                report.sessions.push(SessionStats {
                    session: usage.session,
                    coverage: Coverage {
                        turns: usage.turns.coverage,
                        pairs: PairCoverage::CompleteOnly,
                        truncation: events.truncation,
                    },
                    tools,
                });
                report.read += 1;
            }
            Err(error) => report.failed.push(ReadFailure {
                id: session.id,
                harness: session.harness,
                store: session.store,
                error: format!("{error:#}"),
            }),
        }
    }
    Ok(report)
}

fn merge(total: &mut ToolStats, value: &ToolStats) {
    total.calls += value.calls;
    total.results += value.results;
    total.paired += value.paired;
    total.errors += value.errors;
    total.incomplete.no_result_in_read += value.incomplete.no_result_in_read;
    total.incomplete.call_before_read_bound += value.incomplete.call_before_read_bound;
    total.incomplete.call_not_recorded += value.incomplete.call_not_recorded;
    let mut names = std::mem::take(&mut total.by_name)
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect::<BTreeMap<_, _>>();
    for row in &value.by_name {
        if let Some(sum) = names.get_mut(&row.name) {
            sum.calls += row.calls;
            sum.paired += row.paired;
            sum.errors += row.errors;
            if let Some(duration) = row.duration_ms {
                let sum = sum.duration_ms.get_or_insert_with(Default::default);
                sum.total = sum.total.saturating_add(duration.total);
                sum.max = sum.max.max(duration.max);
                sum.count += duration.count;
            }
        } else {
            names.insert(row.name.clone(), row.clone());
        }
    }
    total.by_name = names.into_values().collect();
    total
        .by_name
        .sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));
}
