//! One session's recorded statistics: what the bounded read holds, counted.
//!
//! Every figure here is a count of records the harness wrote, and every total
//! says what it covers. Nothing is labelled useful or wasteful, no reason is
//! attributed to a latency or an ending, and no counter is derived from
//! another: a fact the read did not reach is absent rather than zero.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::content::{self, ContentInventory};
use crate::event::{self, EventKind, EventRecord, Incomplete, PairRef};
use crate::lineage::Lineage;
use crate::model::{
    Accounting, Cost, ReadEvidence, TerminalObservation, TextTailEvidence, Tokens, Transcript,
    Truncation, TurnKind,
};
use crate::usage::{self, TurnCoverage, UsageSession};

pub const STATS_SCHEMA: &str = "tapes-stats/5";

/// One session's counted facts, in the order a reader takes them: what the
/// figures cover, the turns, the tool calls behind them, the recorded clock,
/// the recorded counters, the recorded relatives, and what the read could
/// not settle.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StatsView {
    pub schema: &'static str,
    pub session: UsageSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_tail: Option<TextTailEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    pub coverage: Coverage,
    pub turns: TurnKindCounts,
    pub tools: ToolStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durations_ms: Option<TimeStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage: Option<LineageStats>,
    pub warnings: Vec<Warning>,
}

/// What the figures are figures about. `turns` is `read-window` when a source
/// bound withheld whole turns, `pairs` says that every duration comes from a
/// call and result the read holds both halves of, and `truncation` is the
/// read's own record of what it did not reach.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Coverage {
    pub turns: TurnCoverage,
    pub pairs: PairCoverage,
    #[serde(skip_serializing_if = "Truncation::is_empty")]
    pub truncation: Truncation,
}

/// Durations are measured over complete pairs only: a call whose result the
/// read never reached contributes no time at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PairCoverage {
    #[default]
    CompleteOnly,
}

/// Normalized turns the read reached, by the kind the harness recorded them
/// as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TurnKindCounts {
    pub operator: usize,
    pub assistant: usize,
    pub tool: usize,
    pub reasoning: usize,
    pub control: usize,
    pub ambient: usize,
    pub notice: usize,
    pub unknown: usize,
    pub total: usize,
}

/// The tool records the read reached. `calls` and `results` count typed event
/// records, `paired` counts the distinct call-and-result pairs among them,
/// and `errors` counts calls whose recorded outcome is an error, once per
/// call however many halves the harness wrote it on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolStats {
    pub calls: usize,
    pub results: usize,
    pub paired: usize,
    pub incomplete: IncompleteCounts,
    pub by_name: Vec<ToolNameStats>,
    pub errors: usize,
}

/// Unpaired events by the boundary that left them unpaired, in the terms the
/// event projection names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IncompleteCounts {
    #[serde(rename = "no-result-in-read")]
    pub no_result_in_read: usize,
    #[serde(rename = "call-before-read-bound")]
    pub call_before_read_bound: usize,
    #[serde(rename = "call-not-recorded")]
    pub call_not_recorded: usize,
}

impl IncompleteCounts {
    pub fn total(&self) -> usize {
        self.no_result_in_read + self.call_before_read_bound + self.call_not_recorded
    }
}

/// One tool's calls, ordered by calls descending and then by name. A record
/// whose tool name neither it nor its counterpart carries is counted in the
/// totals and has no row here, because the read holds no name to put on one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolNameStats {
    pub name: String,
    pub calls: usize,
    pub paired: usize,
    pub errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<DurationStats>,
}

/// Time spent in one tool's calls. `count` is how many complete pairs carried
/// both timestamps, which is what the total and the maximum are over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DurationStats {
    pub total: i64,
    pub max: i64,
    pub count: usize,
}

/// The clock the recording carries. A turn without a timestamp is in no
/// figure here but its own count, so `count_with_timestamps` is what the rest
/// covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TimeStats {
    /// From the first timestamped turn to the last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_span: Option<i64>,
    /// Summed durations of the complete pairs that carried both timestamps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_tool: Option<i64>,
    /// The longest interval between two consecutive timestamped turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub between_turns_max: Option<i64>,
    pub count_with_timestamps: usize,
}

/// The session's own counters, repeated, with the share of
/// `input + cache_read + cache_write` each cache counter accounts for. A
/// ratio is a ratio of recorded token counts and never a share of cost, and
/// it is present only when every counter in its denominator is. Whether a
/// harness's `input` already includes what it read from the cache is that
/// harness's own convention, so a ratio compares recordings of one harness
/// rather than of two.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_ratio: Option<f64>,
}

/// The children this session's store records, counted without reading one.
/// `by_disposition` keys the outcomes the harness wrote; a child whose
/// outcome it did not write is in `children` and in no disposition.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LineageStats {
    pub children: usize,
    pub resolved: usize,
    pub by_disposition: BTreeMap<String, usize>,
}

/// A limit of the read the figures came from, so a total is not read as more
/// than it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Warning {
    /// A source bound withheld whole turns; the counts are the read's.
    ReadWindow,
    /// A turn window dropped turns the read had produced.
    TailWindow,
    /// A user-envelope turn carries no evidence of what it is.
    KindUnknown,
    /// A call or result the read holds has no counterpart in it.
    IncompletePairs,
    /// A turn the read reached carries no timestamp, so the clock covers
    /// fewer turns than the counts do.
    NoTimestamps,
}

/// Count one read session and the relatives its store records. The counters
/// come from the transcript's turns and from the same typed event projection
/// `events` returns, so nothing is parsed twice and nothing is read from a
/// turn's harness envelope.
pub fn stats(transcript: Transcript, lineage: &Lineage) -> StatsView {
    let read = transcript.read.clone();
    let terminal = transcript.terminal.clone();
    let text_tail = transcript.text_tail.clone();
    let content = content::inventory(&transcript.turns);
    let usage = usage::usage(&transcript);
    let turns = turn_kinds(&transcript);
    let clock = clock(&transcript);
    let events = event::project(transcript, usize::MAX);
    let tools = tool_stats(&events.events, events.pairs.complete);
    let coverage = Coverage {
        turns: usage.turns.coverage,
        pairs: PairCoverage::CompleteOnly,
        truncation: events.truncation,
    };
    let durations_ms = clock.finish(tools.in_tool);
    let warnings = warnings(&coverage, &turns, &tools.stats, durations_ms.as_ref());
    StatsView {
        schema: STATS_SCHEMA,
        session: usage.session,
        read,
        terminal,
        text_tail,
        content,
        coverage,
        turns,
        tools: tools.stats,
        durations_ms,
        usage: usage_stats(usage.tokens, usage.cost, usage.accounting),
        lineage: lineage_stats(lineage),
        warnings,
    }
}

fn turn_kinds(transcript: &Transcript) -> TurnKindCounts {
    let mut counts = TurnKindCounts::default();
    for turn in &transcript.turns {
        let slot = match turn.kind {
            TurnKind::Operator => &mut counts.operator,
            TurnKind::Assistant => &mut counts.assistant,
            TurnKind::Tool => &mut counts.tool,
            TurnKind::Reasoning => &mut counts.reasoning,
            TurnKind::Control => &mut counts.control,
            TurnKind::Ambient => &mut counts.ambient,
            TurnKind::Notice => &mut counts.notice,
            TurnKind::Unknown => &mut counts.unknown,
        };
        *slot += 1;
        counts.total += 1;
    }
    counts
}

/// The recorded clock, accumulated over the turns that carry a timestamp.
#[derive(Default)]
struct Clock {
    first: Option<DateTime<Utc>>,
    last: Option<DateTime<Utc>>,
    previous: Option<DateTime<Utc>>,
    between_turns_max: Option<i64>,
    counted: usize,
}

impl Clock {
    fn add(&mut self, ts: DateTime<Utc>) {
        self.counted += 1;
        self.first.get_or_insert(ts);
        self.last = Some(ts);
        if let Some(previous) = self.previous {
            let gap = ts.signed_duration_since(previous).num_milliseconds();
            if gap >= 0 {
                self.between_turns_max = Some(self.between_turns_max.unwrap_or(gap).max(gap));
            }
        }
        self.previous = Some(ts);
    }

    /// The clock, or nothing when no turn the read reached carried a stamp.
    fn finish(self, in_tool: Option<i64>) -> Option<TimeStats> {
        (self.counted > 0).then(|| TimeStats {
            // One stamp is a moment rather than a span, so a session with a
            // single timestamped turn records no span at all.
            recorded_span: (self.counted > 1)
                .then(|| self.first.zip(self.last))
                .flatten()
                .map(|(first, last)| last.signed_duration_since(first).num_milliseconds()),
            in_tool,
            between_turns_max: self.between_turns_max,
            count_with_timestamps: self.counted,
        })
    }
}

fn clock(transcript: &Transcript) -> Clock {
    let mut clock = Clock::default();
    for ts in transcript.turns.iter().filter_map(|turn| turn.ts) {
        clock.add(ts);
    }
    clock
}

/// What a paired counterpart contributes to a record that carries neither the
/// tool's name nor the outcome: a harness may write either on one half alone.
struct CallFacts {
    name: Option<String>,
    status: Option<String>,
}

struct ToolTally {
    stats: ToolStats,
    in_tool: Option<i64>,
}

const ERROR_STATUS: &str = "error";

/// Count the typed events of one bounded read. Pairing, and therefore every
/// duration, is the event projection's own; this adds nothing to it.
pub(crate) fn count_tools(records: &[EventRecord], paired: usize) -> ToolStats {
    tool_stats(records, paired).stats
}

fn tool_stats(records: &[EventRecord], paired: usize) -> ToolTally {
    let calls = index_calls(records);
    let mut stats = ToolStats {
        paired,
        ..ToolStats::default()
    };
    let mut by_name: HashMap<String, ToolNameStats> = HashMap::new();
    let mut in_tool: Option<i64> = None;
    for record in records {
        let counterpart = record
            .pair
            .as_ref()
            .and_then(|pair| calls.get(pair))
            .filter(|_| record.event.kind == EventKind::ToolResult);
        let name = record
            .event
            .name
            .clone()
            .or_else(|| counterpart.and_then(|call| call.name.clone()));
        let own_error = record.event.status.as_deref() == Some(ERROR_STATUS);
        // A harness records the outcome on the call, on the result, or on
        // both. Counting a paired call's error at its result leaves one count
        // per call whichever half carries it.
        let counted_error = match record.event.kind {
            EventKind::ToolCall => own_error && record.pair.is_none(),
            EventKind::ToolResult => {
                own_error
                    || counterpart.is_some_and(|call| call.status.as_deref() == Some(ERROR_STATUS))
            }
        };
        match record.event.kind {
            EventKind::ToolCall => stats.calls += 1,
            EventKind::ToolResult => stats.results += 1,
        }
        if counted_error {
            stats.errors += 1;
        }
        if let Some(incomplete) = &record.incomplete {
            let slot = match incomplete {
                Incomplete::NoResultInRead => &mut stats.incomplete.no_result_in_read,
                Incomplete::CallBeforeReadBound => &mut stats.incomplete.call_before_read_bound,
                Incomplete::CallNotRecorded => &mut stats.incomplete.call_not_recorded,
            };
            *slot += 1;
        }
        if let Some(duration) = record.duration_ms {
            in_tool = Some(in_tool.unwrap_or_default().saturating_add(duration));
        }
        let Some(name) = name else {
            continue;
        };
        let entry = by_name
            .entry(name.clone())
            .or_insert_with(|| ToolNameStats {
                name,
                calls: 0,
                paired: 0,
                errors: 0,
                duration_ms: None,
            });
        if record.event.kind == EventKind::ToolCall {
            entry.calls += 1;
            if record.pair.is_some() {
                entry.paired += 1;
            }
        }
        if counted_error {
            entry.errors += 1;
        }
        if let Some(duration) = record.duration_ms {
            let durations = entry.duration_ms.get_or_insert_with(DurationStats::default);
            durations.total = durations.total.saturating_add(duration);
            durations.max = durations.max.max(duration);
            durations.count += 1;
        }
    }
    stats.by_name = by_name.into_values().collect();
    stats.by_name.sort_by(|left, right| {
        right
            .calls
            .cmp(&left.calls)
            .then_with(|| left.name.cmp(&right.name))
    });
    ToolTally { stats, in_tool }
}

/// The paired calls, by the reference their results name them with, so a
/// result can be read under the tool name and outcome its call carries.
fn index_calls(records: &[EventRecord]) -> HashMap<PairRef, CallFacts> {
    let mut calls = HashMap::new();
    for record in records
        .iter()
        .filter(|record| record.event.kind == EventKind::ToolCall && record.pair.is_some())
    {
        calls
            .entry(PairRef {
                ordinal: record.ordinal,
                native_id: record.native_id.clone(),
                record_ref: record.record_ref.clone(),
            })
            .or_insert_with(|| CallFacts {
                name: record.event.name.clone(),
                status: record.event.status.clone(),
            });
    }
    calls
}

fn usage_stats(
    tokens: Option<Tokens>,
    cost: Option<Cost>,
    accounting: Option<Accounting>,
) -> Option<UsageStats> {
    if tokens.is_none() && cost.is_none() && accounting.is_none() {
        return None;
    }
    // The share of the summed input counters each cache counter accounts for.
    // It needs every counter in its denominator, so a harness that recorded
    // only some of them yields no ratio.
    let context = tokens.as_ref().and_then(|tokens| {
        let read = tokens.cache_read?;
        let write = tokens.cache_write?;
        let counted = tokens.input? + read + write;
        (counted > 0).then_some((read, write, counted))
    });
    Some(UsageStats {
        cache_read_ratio: context.map(|(read, _, counted)| read as f64 / counted as f64),
        cache_write_ratio: context.map(|(_, write, counted)| write as f64 / counted as f64),
        tokens,
        cost,
        accounting,
    })
}

fn lineage_stats(lineage: &Lineage) -> Option<LineageStats> {
    if lineage.children.is_empty() {
        return None;
    }
    let mut stats = LineageStats {
        children: lineage.children.len(),
        ..LineageStats::default()
    };
    for child in &lineage.children {
        if child.resolved {
            stats.resolved += 1;
        }
        if let Some(disposition) = &child.disposition {
            *stats.by_disposition.entry(disposition.clone()).or_default() += 1;
        }
    }
    Some(stats)
}

fn warnings(
    coverage: &Coverage,
    turns: &TurnKindCounts,
    tools: &ToolStats,
    durations: Option<&TimeStats>,
) -> Vec<Warning> {
    let stamped = durations.map_or(0, |durations| durations.count_with_timestamps);
    [
        (
            coverage.turns == TurnCoverage::ReadWindow,
            Warning::ReadWindow,
        ),
        (coverage.truncation.window.is_some(), Warning::TailWindow),
        (turns.unknown > 0, Warning::KindUnknown),
        (tools.incomplete.total() > 0, Warning::IncompletePairs),
        (stamped < turns.total, Warning::NoTimestamps),
    ]
    .into_iter()
    .filter_map(|(present, warning)| present.then_some(warning))
    .collect()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::{json, Value};

    use super::*;
    use crate::event::{Bounded, ToolEvent};
    use crate::lineage::ChildRef;
    use crate::model::{
        AccountingBasis, AccountingCoverage, Role, Session, SourceBound, SourceDescriptor, Turn,
    };

    fn session() -> Session {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Session {
            id: "fixture-session".to_owned(),
            source: SourceDescriptor::installed("fixture", "fixture-recording"),
            metadata: None,
            model: None,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory: None,
            started_at: Some(ts),
            last_activity_at: Some(ts),
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            start_uncertain: false,
            occurrence: None,
            usage_detail: None,
        }
    }

    fn turn(ordinal: usize, kind: TurnKind, seconds: Option<i64>) -> Turn {
        Turn {
            role: match kind {
                TurnKind::Assistant => Role::Assistant,
                TurnKind::Reasoning => Role::Reasoning,
                TurnKind::Tool => Role::Tool,
                _ => Role::User,
            },
            kind,
            text: "fixture envelope".to_owned(),
            ts: seconds.map(|seconds| Utc.timestamp_opt(seconds, 0).unwrap()),
            ordinal,
            native_id: Some(format!("native-{ordinal}")),
            request_turn_id: None,
            metadata: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
            tool: None,
        }
    }

    fn tool_turn(ordinal: usize, seconds: i64, event: ToolEvent) -> Turn {
        Turn {
            tool: Some(event),
            ..turn(ordinal, TurnKind::Tool, Some(seconds))
        }
    }

    fn call(name: Option<&str>, call_id: &str, status: Option<&str>) -> ToolEvent {
        ToolEvent {
            kind: EventKind::ToolCall,
            subtype: "function_call".to_owned(),
            name: name.map(str::to_owned),
            call_id: Some(call_id.to_owned()),
            status: status.map(str::to_owned),
            arguments: Some(Bounded::from_text("{}")),
            output: None,
            completed_ts: None,
            invocations: Vec::new(),
            artifact_references: Vec::new(),
            artifact_consumptions: Vec::new(),
        }
    }

    fn result(call_id: &str, status: Option<&str>) -> ToolEvent {
        ToolEvent {
            kind: EventKind::ToolResult,
            subtype: "function_call_output".to_owned(),
            name: None,
            call_id: Some(call_id.to_owned()),
            status: status.map(str::to_owned),
            arguments: None,
            output: Some(Bounded::from_text("done")),
            completed_ts: None,
            invocations: Vec::new(),
            artifact_references: Vec::new(),
            artifact_consumptions: Vec::new(),
        }
    }

    fn transcript(session: Session, turns: Vec<Turn>, source: Vec<SourceBound>) -> Transcript {
        Transcript::new(
            session,
            turns,
            Truncation {
                window: None,
                source,
            },
            None,
            Vec::new(),
        )
    }

    #[test]
    fn a_session_with_no_tools_counters_or_children_omits_every_optional_group() {
        let view = stats(
            transcript(
                session(),
                vec![
                    turn(0, TurnKind::Operator, Some(10)),
                    turn(1, TurnKind::Assistant, Some(30)),
                ],
                Vec::new(),
            ),
            &Lineage::default(),
        );
        let value = serde_json::to_value(&view).unwrap();

        assert_eq!(value["schema"], STATS_SCHEMA);
        assert_eq!(
            value["coverage"],
            json!({ "turns": "session", "pairs": "complete-only" })
        );
        assert_eq!(
            value["turns"],
            json!({
                "operator": 1,
                "assistant": 1,
                "tool": 0,
                "reasoning": 0,
                "control": 0,
                "ambient": 0,
                "notice": 0,
                "unknown": 0,
                "total": 2
            })
        );
        assert_eq!(
            value["tools"],
            json!({
                "calls": 0,
                "results": 0,
                "paired": 0,
                "incomplete": {
                    "no-result-in-read": 0,
                    "call-before-read-bound": 0,
                    "call-not-recorded": 0
                },
                "by_name": [],
                "errors": 0
            })
        );
        assert_eq!(
            value["durations_ms"],
            json!({ "recorded_span": 20_000, "between_turns_max": 20_000, "count_with_timestamps": 2 })
        );
        assert_eq!(value["warnings"], json!([]) as Value);
        for absent in ["usage", "lineage"] {
            assert!(value.get(absent).is_none(), "{absent} in {value}");
        }
    }

    /// The tool name and the outcome may each be recorded on one half of a
    /// pair alone, and a failure recorded on both halves is one failure.
    #[test]
    fn an_error_counts_once_per_call_under_the_name_either_half_carries() {
        let combined = ToolEvent {
            subtype: "tool".to_owned(),
            status: Some("error".to_owned()),
            completed_ts: Some(Utc.timestamp_opt(43, 0).unwrap()),
            ..call(Some("edit"), "part-1", None)
        };
        let view = stats(
            transcript(
                session(),
                vec![
                    tool_turn(0, 10, call(Some("read"), "call-1", None)),
                    tool_turn(1, 12, result("call-1", Some("error"))),
                    tool_turn(2, 40, combined),
                ],
                Vec::new(),
            ),
            &Lineage::default(),
        );

        assert_eq!(view.tools.errors, 2);
        assert_eq!(view.tools.calls, 2);
        assert_eq!(view.tools.results, 2);
        assert_eq!(view.tools.paired, 2);
        assert_eq!(
            serde_json::to_value(&view.tools.by_name).unwrap(),
            json!([
                {
                    "name": "edit",
                    "calls": 1,
                    "paired": 1,
                    "errors": 1,
                    "duration_ms": { "total": 3_000, "max": 3_000, "count": 1 }
                },
                {
                    "name": "read",
                    "calls": 1,
                    "paired": 1,
                    "errors": 1,
                    "duration_ms": { "total": 2_000, "max": 2_000, "count": 1 }
                }
            ])
        );
        assert_eq!(view.durations_ms.unwrap().in_tool, Some(5_000));
    }

    /// An unpaired call contributes no duration, and the boundary that left
    /// it unpaired is the bucket it lands in.
    #[test]
    fn an_incomplete_call_is_bucketed_and_times_nothing() {
        let view = stats(
            transcript(
                session(),
                vec![
                    tool_turn(0, 10, call(Some("read"), "call-1", None)),
                    tool_turn(1, 20, result("call-orphan", None)),
                ],
                vec![SourceBound::FileTail { bytes: 4_194_304 }],
            ),
            &Lineage::default(),
        );
        let value = serde_json::to_value(&view).unwrap();

        assert_eq!(
            value["tools"]["incomplete"],
            json!({
                "no-result-in-read": 1,
                "call-before-read-bound": 1,
                "call-not-recorded": 0
            })
        );
        assert_eq!(value["tools"]["paired"], 0);
        assert!(
            value["tools"]["by_name"][0].get("duration_ms").is_none(),
            "{value}"
        );
        assert!(value["durations_ms"].get("in_tool").is_none(), "{value}");
        assert_eq!(value["coverage"]["turns"], "read-window");
        assert_eq!(
            value["coverage"]["truncation"]["source"],
            json!([{ "kind": "file-tail", "bytes": 4_194_304 }])
        );
        assert_eq!(
            value["warnings"],
            json!(["read-window", "incomplete-pairs"])
        );
    }

    /// A ratio is a ratio of recorded counts, and it needs every counter in
    /// its denominator before it can be one.
    #[test]
    fn cache_ratios_need_every_counter_they_divide_by() {
        let mut spender = session();
        spender.tokens = Some(Tokens {
            input: Some(600),
            output: Some(50),
            reasoning: None,
            cache_read: Some(300),
            cache_write: Some(100),
        });
        spender.accounting = Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        });
        let with_cache = stats(
            transcript(
                spender.clone(),
                vec![turn(0, TurnKind::Operator, None)],
                Vec::new(),
            ),
            &Lineage::default(),
        );
        let value = serde_json::to_value(&with_cache).unwrap();
        assert_eq!(value["usage"]["cache_read_ratio"], 0.3);
        assert_eq!(value["usage"]["cache_write_ratio"], 0.1);
        assert_eq!(
            value["usage"]["accounting"],
            json!({ "basis": "recorded-total", "coverage": "session" })
        );
        assert!(value.get("durations_ms").is_none(), "{value}");
        assert_eq!(value["warnings"], json!(["no-timestamps"]));

        spender.tokens = Some(Tokens {
            input: Some(600),
            output: Some(50),
            reasoning: None,
            cache_read: Some(300),
            cache_write: None,
        });
        let partial = serde_json::to_value(stats(
            transcript(spender, vec![turn(0, TurnKind::Operator, None)], Vec::new()),
            &Lineage::default(),
        ))
        .unwrap();
        assert_eq!(partial["usage"]["tokens"]["cache_read"], 300);
        for absent in ["cache_read_ratio", "cache_write_ratio"] {
            assert!(
                partial["usage"].get(absent).is_none(),
                "{absent} in {partial}"
            );
        }
    }

    /// Children are counted from the references the store recorded; an
    /// outcome it did not record keys no disposition.
    #[test]
    fn children_are_counted_by_reference_and_by_recorded_outcome() {
        let lineage = Lineage {
            children: vec![
                ChildRef {
                    resolved: true,
                    disposition: Some("completed".to_owned()),
                    ..ChildRef::new("first".to_owned(), "fixture")
                },
                ChildRef {
                    disposition: Some("completed".to_owned()),
                    ..ChildRef::new("second".to_owned(), "fixture")
                },
                ChildRef::new("third".to_owned(), "fixture"),
            ],
            ..Lineage::default()
        };
        let value = serde_json::to_value(stats(
            transcript(
                session(),
                vec![turn(0, TurnKind::Unknown, Some(10))],
                Vec::new(),
            ),
            &lineage,
        ))
        .unwrap();

        assert_eq!(
            value["lineage"],
            json!({
                "children": 3,
                "resolved": 1,
                "by_disposition": { "completed": 2 }
            })
        );
        assert_eq!(value["warnings"], json!(["kind-unknown"]));
    }
}
