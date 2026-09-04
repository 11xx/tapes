use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::model::{Session, SourceBound, Transcript, Truncation};

pub const EVENTS_SCHEMA: &str = "tapes-events/1";
const PREVIEW_CHARS: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    ToolCall,
    ToolResult,
}

/// A payload field represented by its character count and a short prefix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Bounded {
    pub chars: usize,
    pub preview: String,
}

impl Bounded {
    pub fn from_value(value: &Value) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        let text = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        Some(Self::from_text(&text))
    }

    pub fn from_text(text: &str) -> Self {
        Self {
            chars: text.chars().count(),
            preview: text.chars().take(PREVIEW_CHARS).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolEvent {
    pub kind: EventKind,
    pub subtype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Bounded>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Bounded>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct PairRef {
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Incomplete {
    NoResultInRead,
    CallBeforeReadBound,
    CallNotRecorded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EventRecord {
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub event: ToolEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair: Option<PairRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<Incomplete>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PairCounts {
    pub complete: usize,
    pub incomplete: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventTranscript {
    pub schema: &'static str,
    pub session: Session,
    pub events: Vec<EventRecord>,
    pub pairs: PairCounts,
    pub truncated: bool,
    #[serde(skip_serializing_if = "truncation_is_empty")]
    pub truncation: Truncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl EventTranscript {
    pub fn retain(&mut self, names: &[String], call_ids: &[String]) {
        self.events.retain(|record| {
            (names.is_empty()
                || record
                    .event
                    .name
                    .as_ref()
                    .is_some_and(|name| names.contains(name)))
                && (call_ids.is_empty()
                    || record
                        .event
                        .call_id
                        .as_ref()
                        .is_some_and(|call_id| call_ids.contains(call_id)))
        });
        self.pairs = pair_counts(&self.events);
    }
}

fn truncation_is_empty(truncation: &Truncation) -> bool {
    truncation.is_empty()
}

/// Pair every event in the bounded read before applying the requested turn
/// window. Pair references therefore survive when their counterpart is
/// outside the returned window.
pub fn project(transcript: Transcript, tail: usize) -> EventTranscript {
    let total_turns = transcript.turns.len();
    let read_was_bounded = transcript.truncation.source.iter().any(|bound| {
        matches!(
            bound,
            SourceBound::FileTail { .. } | SourceBound::RecordPage { .. }
        )
    });
    let mut records = transcript
        .turns
        .iter()
        .filter_map(|turn| {
            turn.tool.clone().map(|event| EventRecord {
                ordinal: turn.ordinal,
                native_id: turn.native_id.clone(),
                ts: turn.ts,
                event,
                pair: None,
                duration_ms: None,
                incomplete: None,
            })
        })
        .flat_map(|call| {
            let result = ((call.event.kind == EventKind::ToolCall)
                && call.event.subtype == "tool"
                && matches!(call.event.status.as_deref(), Some("completed" | "error")))
            .then(|| {
                let mut result = call.clone();
                result.ts = result.event.completed_ts;
                result.event.kind = EventKind::ToolResult;
                result.event.arguments = None;
                result
            });
            std::iter::once(call).chain(result)
        })
        .collect::<Vec<_>>();

    pair(&mut records, read_was_bounded);

    let returned_turns = total_turns.min(tail);
    let first_ordinal = total_turns.saturating_sub(returned_turns);
    records.retain(|record| record.ordinal >= first_ordinal);

    let truncation = Truncation {
        window: transcript
            .truncation
            .window
            .or_else(|| Truncation::window(returned_turns, total_turns, tail)),
        source: transcript.truncation.source,
    };
    let pairs = pair_counts(&records);
    EventTranscript {
        schema: EVENTS_SCHEMA,
        session: transcript.session,
        events: records,
        pairs,
        truncated: !truncation.is_empty(),
        truncation,
        notes: transcript.notes,
    }
}

fn pair(records: &mut [EventRecord], read_was_bounded: bool) {
    let mut calls = HashMap::<String, Vec<usize>>::new();
    for index in 0..records.len() {
        let Some(call_id) = records[index].event.call_id.clone() else {
            records[index].incomplete = Some(match records[index].event.kind {
                EventKind::ToolCall => Incomplete::NoResultInRead,
                EventKind::ToolResult if read_was_bounded => Incomplete::CallBeforeReadBound,
                EventKind::ToolResult => Incomplete::CallNotRecorded,
            });
            continue;
        };
        match records[index].event.kind {
            EventKind::ToolCall => calls.entry(call_id).or_default().push(index),
            EventKind::ToolResult => {
                let Some(call_index) = calls.get_mut(&call_id).and_then(Vec::pop) else {
                    records[index].incomplete = Some(if read_was_bounded {
                        Incomplete::CallBeforeReadBound
                    } else {
                        Incomplete::CallNotRecorded
                    });
                    continue;
                };
                records[call_index].pair = Some(reference(&records[index]));
                records[index].pair = Some(reference(&records[call_index]));
                records[call_index].duration_ms = match (records[call_index].ts, records[index].ts)
                {
                    (Some(call_ts), Some(result_ts)) if result_ts >= call_ts => {
                        Some(result_ts.signed_duration_since(call_ts).num_milliseconds())
                    }
                    _ => None,
                };
            }
        }
    }
    for pending in calls.into_values().flatten() {
        records[pending].incomplete = Some(Incomplete::NoResultInRead);
    }
}

fn reference(record: &EventRecord) -> PairRef {
    PairRef {
        ordinal: record.ordinal,
        native_id: record.native_id.clone(),
    }
}

fn pair_counts(records: &[EventRecord]) -> PairCounts {
    let incomplete = records
        .iter()
        .filter(|record| record.incomplete.is_some())
        .count();
    let mut complete = HashSet::<(PairRef, PairRef, Option<String>)>::new();
    for record in records.iter().filter(|record| record.pair.is_some()) {
        let own = reference(record);
        let counterpart = record.pair.clone().expect("paired record has a reference");
        let endpoints = match record.event.kind {
            EventKind::ToolCall => (own, counterpart),
            EventKind::ToolResult => (counterpart, own),
        };
        complete.insert((endpoints.0, endpoints.1, record.event.call_id.clone()));
    }
    PairCounts {
        complete: complete.len(),
        incomplete,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::model::{Role, Turn, TurnKind};

    fn session() -> Session {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Session {
            id: "fixture-session".to_owned(),
            harness: "fixture".to_owned(),
            model: None,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory: None,
            started_at: ts,
            last_activity_at: ts,
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            store: None,
            start_uncertain: false,
        }
    }

    fn event(kind: EventKind, call_id: &str) -> ToolEvent {
        ToolEvent {
            kind,
            subtype: "fixture".to_owned(),
            name: Some("read".to_owned()),
            call_id: Some(call_id.to_owned()),
            status: None,
            arguments: None,
            output: None,
            completed_ts: None,
        }
    }

    fn turn(ordinal: usize, seconds: i64, event: ToolEvent) -> Turn {
        Turn {
            role: Role::Tool,
            kind: TurnKind::Tool,
            text: "fixture envelope".to_owned(),
            ts: Some(Utc.timestamp_opt(seconds, 0).unwrap()),
            ordinal,
            native_id: Some(format!("native-{ordinal}")),
            tool: Some(event),
        }
    }

    fn transcript(turns: Vec<Turn>, source: Vec<SourceBound>) -> Transcript {
        Transcript::new(
            session(),
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
    fn bounded_counts_characters_and_keeps_a_unicode_safe_prefix() {
        let text = format!("{}end", "é".repeat(PREVIEW_CHARS));
        let bounded = Bounded::from_text(&text);

        assert_eq!(bounded.chars, PREVIEW_CHARS + 3);
        assert_eq!(bounded.preview.chars().count(), PREVIEW_CHARS);
        assert_eq!(bounded.preview, "é".repeat(PREVIEW_CHARS));
    }

    #[test]
    fn pairing_uses_the_most_recent_unpaired_call_and_never_makes_negative_duration() {
        let records = project(
            transcript(
                vec![
                    turn(0, 10, event(EventKind::ToolCall, "repeated")),
                    turn(1, 20, event(EventKind::ToolCall, "repeated")),
                    turn(2, 23, event(EventKind::ToolResult, "repeated")),
                    turn(3, 30, event(EventKind::ToolCall, "backwards")),
                    turn(4, 29, event(EventKind::ToolResult, "backwards")),
                ],
                Vec::new(),
            ),
            usize::MAX,
        );

        assert_eq!(
            records.events[0].incomplete,
            Some(Incomplete::NoResultInRead)
        );
        assert_eq!(records.events[1].pair.as_ref().unwrap().ordinal, 2);
        assert_eq!(records.events[1].duration_ms, Some(3_000));
        assert_eq!(records.events[3].pair.as_ref().unwrap().ordinal, 4);
        assert_eq!(records.events[3].duration_ms, None);
        assert_eq!(records.pairs.complete, 2);
        assert_eq!(records.pairs.incomplete, 1);
    }

    #[test]
    fn an_unmatched_result_names_whether_the_read_reached_the_start() {
        for (source, expected) in [
            (Vec::new(), Incomplete::CallNotRecorded),
            (
                vec![SourceBound::FileTail { bytes: 4_194_304 }],
                Incomplete::CallBeforeReadBound,
            ),
            (
                vec![SourceBound::RecordPage {
                    records: 1_000,
                    of: "messages".to_owned(),
                }],
                Incomplete::CallBeforeReadBound,
            ),
        ] {
            let projected = project(
                transcript(
                    vec![turn(0, 10, event(EventKind::ToolResult, "missing"))],
                    source,
                ),
                usize::MAX,
            );
            assert_eq!(projected.events[0].incomplete, Some(expected));
        }
    }

    #[test]
    fn a_combined_opencode_part_projects_both_halves_on_one_ordinal() {
        let completed = ToolEvent {
            kind: EventKind::ToolCall,
            subtype: "tool".to_owned(),
            name: Some("read".to_owned()),
            call_id: Some("part-1".to_owned()),
            status: Some("completed".to_owned()),
            arguments: Some(Bounded::from_text("{}")),
            output: Some(Bounded::from_text("done")),
            completed_ts: None,
        };
        let projected = project(
            transcript(vec![turn(0, 10, completed)], Vec::new()),
            usize::MAX,
        );

        assert_eq!(projected.events.len(), 2);
        assert_eq!(projected.events[0].event.kind, EventKind::ToolCall);
        assert_eq!(projected.events[1].event.kind, EventKind::ToolResult);
        assert_eq!(projected.events[0].ordinal, projected.events[1].ordinal);
        assert_eq!(projected.pairs.complete, 1);
    }

    #[test]
    fn a_window_keeps_pair_references_to_counterparts_outside_it() {
        let projected = project(
            transcript(
                vec![
                    turn(0, 10, event(EventKind::ToolCall, "outside")),
                    turn(1, 11, event(EventKind::ToolResult, "outside")),
                ],
                Vec::new(),
            ),
            1,
        );

        assert_eq!(projected.events.len(), 1);
        assert_eq!(projected.events[0].event.kind, EventKind::ToolResult);
        assert_eq!(projected.events[0].pair.as_ref().unwrap().ordinal, 0);
        assert!(projected.events[0].incomplete.is_none());
        assert_eq!(projected.pairs.complete, 1);
    }
}
