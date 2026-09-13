use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::content::{self, ArtifactReference, ContentCoverage, ContentInventory, ContentPart};
use crate::model::{
    BoundedText, ByteSpan, ReadEvidence, RecordRef, Session, SourceBound, TerminalObservation,
    TextTailEvidence, Transcript, Truncation,
};

pub const EVENTS_SCHEMA: &str = "tapes-events/4";
const PREVIEW_CHARS: usize = 200;
pub const MAX_INVOCATION_TEXT_CHARS: usize = 64 * 1024;
pub const MAX_INVOCATIONS: usize = 32;
pub const MAX_INVOCATION_ARGUMENTS: usize = 32;
pub const MAX_INVOCATION_STRING_CHARS: usize = 2 * 1024;
pub const MAX_INVOCATION_DEPTH: usize = 32;
const MAX_ARTIFACT_REFERENCES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvocationOrigin {
    StructuredRuntime,
    StaticDeclaration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InvocationCoverage {
    StructuredRuntime,
    StaticLiteral,
    ConditionalDeclaration,
    Unsupported,
}

/// A command-shaped fact nested inside one recorded outer tool event. It is a
/// declaration unless a separate native result observes the same qualified
/// operation; it never inherits the wrapper's duration or success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InvocationEvidence {
    pub origin: InvocationOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subcommand: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<BoundedText>,
    pub source_field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<ByteSpan>,
    pub coverage: InvocationCoverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<BoundedText>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conditional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    /// A separate native result for this operation; an outer wrapper result
    /// does not establish an individual nested execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witnessed_result: Option<RecordRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsumptionStatus {
    MatchingConsumptionObserved,
    NoMatchingConsumptionObservedInRead,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArtifactConsumption {
    pub reference: ArtifactReference,
    pub status: ConsumptionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumer: Option<RecordRef>,
}

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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invocations: Vec<InvocationEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_references: Vec<ArtifactReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_consumptions: Vec<ArtifactConsumption>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct PairRef {
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
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
    pub record_ref: Option<RecordRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<ContentCoverage>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_tail: Option<TextTailEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "truncation_is_empty")]
    pub truncation: Truncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl EventTranscript {
    pub fn retain(&mut self, names: &[String], call_ids: &[String]) {
        self.retain_with_program(names, call_ids, &[]);
    }

    pub fn retain_with_program(
        &mut self,
        names: &[String],
        call_ids: &[String],
        programs: &[String],
    ) {
        let selected_call_ids = self
            .events
            .iter()
            .filter(|record| program_matches(record, programs))
            .filter_map(|record| record.event.call_id.clone())
            .collect::<HashSet<_>>();
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
                && (programs.is_empty()
                    || program_matches(record, programs)
                    || record
                        .event
                        .call_id
                        .as_ref()
                        .is_some_and(|call_id| selected_call_ids.contains(call_id)))
        });
        self.pairs = pair_counts(&self.events);
    }
}

fn program_matches(record: &EventRecord, programs: &[String]) -> bool {
    programs.is_empty()
        || record.event.invocations.iter().any(|invocation| {
            invocation
                .program
                .as_ref()
                .is_some_and(|program| programs.iter().any(|wanted| wanted == program))
        })
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
                record_ref: turn.record_ref.clone(),
                parts: turn.parts.clone(),
                coverage: turn.coverage.clone(),
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
                result.event.invocations.clear();
                result.event.artifact_references.clear();
                result.event.artifact_consumptions.clear();
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
        read: transcript.read,
        terminal: transcript.terminal,
        text_tail: transcript.text_tail,
        content: content::inventory(&transcript.turns),
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
                records[call_index].event.artifact_consumptions = artifact_consumptions(
                    &records[call_index].event.artifact_references,
                    &records[index].event.artifact_references,
                    records[index].record_ref.clone(),
                );
                let separate_native_record = matches!(
                    (
                        records[call_index].record_ref.as_ref(),
                        records[index].record_ref.as_ref()
                    ),
                    (Some(call), Some(result)) if call != result
                );
                for invocation in &mut records[call_index].event.invocations {
                    if separate_native_record
                        && invocation.coverage == InvocationCoverage::StructuredRuntime
                    {
                        invocation.witnessed_result = records[index].record_ref.clone();
                    }
                }
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
        records[pending].event.artifact_consumptions = records[pending]
            .event
            .artifact_references
            .iter()
            .cloned()
            .map(|reference| ArtifactConsumption {
                reference,
                status: ConsumptionStatus::NoMatchingConsumptionObservedInRead,
                consumer: None,
            })
            .collect();
    }
}

fn artifact_key(
    reference: &ArtifactReference,
) -> (&str, Option<&str>, Option<&str>, Option<&str>, Option<u64>) {
    (
        reference.kind.as_str(),
        reference.uri.as_deref(),
        reference.path.as_deref(),
        reference.digest.as_deref(),
        reference.bytes,
    )
}

fn artifact_consumptions(
    declared: &[ArtifactReference],
    observed: &[ArtifactReference],
    consumer: Option<RecordRef>,
) -> Vec<ArtifactConsumption> {
    declared
        .iter()
        .cloned()
        .map(|reference| {
            let matched = observed
                .iter()
                .any(|candidate| artifact_key(candidate) == artifact_key(&reference));
            ArtifactConsumption {
                reference,
                status: if matched {
                    ConsumptionStatus::MatchingConsumptionObserved
                } else {
                    ConsumptionStatus::NoMatchingConsumptionObservedInRead
                },
                consumer: matched.then(|| consumer.clone()).flatten(),
            }
        })
        .collect()
}

/// Extract only explicit structured artifact descriptors from a native value.
/// Strings are never scanned for path-shaped substrings.
pub fn artifact_references(value: &Value) -> Vec<ArtifactReference> {
    let mut references = Vec::new();
    collect_artifact_references(value, 0, &mut references);
    references
}

/// Inspect a native tool argument carrier without evaluating it. The result
/// contains only literal command declarations; variables, substitutions and
/// control-flow-sensitive forms remain qualified as unsupported.
pub fn invocations_from_tool(
    name: Option<&str>,
    arguments: &Value,
    source_field: &str,
) -> Vec<InvocationEvidence> {
    let argument_value = if let Some(text) = arguments.as_str() {
        serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.to_owned()))
    } else {
        arguments.clone()
    };
    let intent = argument_value
        .get("why")
        .and_then(Value::as_str)
        .map(|text| bounded_text(text, MAX_INVOCATION_STRING_CHARS));
    if let Some(object) = argument_value.as_object() {
        if let Some(argv) = object.get("argv").and_then(Value::as_array) {
            match bounded_argv(argv) {
                Ok(values) if !values.is_empty() => {
                    return vec![literal_invocation(
                        &values,
                        source_field,
                        intent,
                        InvocationCoverage::StaticLiteral,
                        None,
                        false,
                    )];
                }
                Ok(_) => {}
                Err(reason) => {
                    return vec![unsupported_invocation(source_field, intent, reason, None)];
                }
            }
        }
        if let Some(command) = object.get("cmd").and_then(Value::as_str) {
            return declarations_from_text(command, source_field, intent);
        }
        if let Some(command) = object.get("command").and_then(Value::as_str) {
            return declarations_from_text(command, source_field, intent);
        }
    }
    if let Some(command) = argument_value.as_str() {
        if matches!(name, Some("exec" | "exec_command" | "shell" | "bash"))
            || command.contains("tools.exec_command")
        {
            return declarations_from_text(command, source_field, intent);
        }
    }
    if intent.is_some() {
        return vec![unsupported_invocation(
            source_field,
            intent,
            "tool arguments did not carry a supported literal command",
            None,
        )];
    }
    Vec::new()
}

/// Project the native `command` argv that a runtime record supplied as
/// structured data. The runtime's argv is stronger evidence than a preview but
/// still describes only the recorded outer execution; nested shell text is not
/// reinterpreted.
pub fn structured_runtime_invocations(item: &Value, source_field: &str) -> Vec<InvocationEvidence> {
    let intent = item
        .get("intent")
        .or_else(|| item.get("description"))
        .and_then(Value::as_str)
        .map(|value| bounded_text(value, MAX_INVOCATION_STRING_CHARS));
    let Some(command) = item.get("command").filter(|value| !value.is_null()) else {
        return Vec::new();
    };
    let Some(argv) = command.as_array() else {
        return vec![unsupported_invocation_from(
            InvocationOrigin::StructuredRuntime,
            source_field,
            intent,
            "runtime command was not an argv array",
            None,
        )];
    };
    let values = match bounded_argv(argv) {
        Ok(values) => values,
        Err(reason) => {
            return vec![unsupported_invocation_from(
                InvocationOrigin::StructuredRuntime,
                source_field,
                intent,
                reason,
                None,
            )];
        }
    };
    if values.is_empty() {
        return Vec::new();
    }
    let mut invocation = literal_invocation(
        &values,
        source_field,
        intent,
        InvocationCoverage::StructuredRuntime,
        None,
        false,
    );
    invocation.origin = InvocationOrigin::StructuredRuntime;
    vec![invocation]
}

fn bounded_argv(argv: &[Value]) -> Result<Vec<BoundedText>, &'static str> {
    if argv.len() > MAX_INVOCATION_ARGUMENTS {
        return Err("argv argument count exceeded the supported bound; no arguments were retained");
    }
    if argv.iter().any(|value| !value.is_string()) {
        return Err("argv contained a non-string argument; no arguments were retained");
    }
    Ok(argv
        .iter()
        .map(|value| {
            bounded_text(
                value
                    .as_str()
                    .expect("argv string validation precedes projection"),
                MAX_INVOCATION_STRING_CHARS,
            )
        })
        .collect())
}

fn declarations_from_text(
    text: &str,
    source_field: &str,
    intent: Option<BoundedText>,
) -> Vec<InvocationEvidence> {
    if text.chars().count() > MAX_INVOCATION_TEXT_CHARS {
        return vec![unsupported_invocation(
            source_field,
            intent,
            "command text exceeded the 64 KiB inspection bound",
            None,
        )];
    }
    if text.contains("tools.exec_command") {
        return javascript_declarations(text, source_field, intent);
    }
    let mut declarations = shell_declarations(text, source_field, intent);
    let span = Some(ByteSpan {
        start: 0,
        end: text.len() as u64,
    });
    for declaration in &mut declarations {
        declaration.span = span;
    }
    declarations
}

fn literal_invocation(
    values: &[BoundedText],
    source_field: &str,
    intent: Option<BoundedText>,
    coverage: InvocationCoverage,
    span: Option<ByteSpan>,
    conditional: bool,
) -> InvocationEvidence {
    InvocationEvidence {
        origin: InvocationOrigin::StaticDeclaration,
        program: values.first().map(|value| value.text.clone()),
        subcommand: values.get(1).map(|value| value.text.clone()),
        arguments: values.iter().skip(2).cloned().collect(),
        intent,
        source_field: source_field.to_owned(),
        span,
        coverage,
        unsupported_reason: None,
        conditional,
        record_ref: None,
        witnessed_result: None,
    }
}

fn unsupported_invocation(
    source_field: &str,
    intent: Option<BoundedText>,
    reason: &str,
    span: Option<ByteSpan>,
) -> InvocationEvidence {
    unsupported_invocation_from(
        InvocationOrigin::StaticDeclaration,
        source_field,
        intent,
        reason,
        span,
    )
}

fn unsupported_invocation_from(
    origin: InvocationOrigin,
    source_field: &str,
    intent: Option<BoundedText>,
    reason: &str,
    span: Option<ByteSpan>,
) -> InvocationEvidence {
    InvocationEvidence {
        origin,
        program: None,
        subcommand: None,
        arguments: Vec::new(),
        intent,
        source_field: source_field.to_owned(),
        span,
        coverage: InvocationCoverage::Unsupported,
        unsupported_reason: Some(bounded_text(reason, MAX_INVOCATION_STRING_CHARS)),
        conditional: false,
        record_ref: None,
        witnessed_result: None,
    }
}

fn shell_declarations(
    text: &str,
    source_field: &str,
    intent: Option<BoundedText>,
) -> Vec<InvocationEvidence> {
    let mut segments = Vec::<Vec<BoundedText>>::new();
    let mut current = Vec::<BoundedText>::new();
    let mut token = String::new();
    let mut quote = None;
    let mut word_started = false;
    let mut conditional = false;
    let mut unsupported = None;
    let mut token_start = 0usize;
    let mut index = 0usize;
    let chars = text.char_indices().collect::<Vec<_>>();
    while index < chars.len() {
        let (offset, character) = chars[index];
        if let Some(active_quote) = quote {
            match active_quote {
                '\'' => {
                    if character == active_quote {
                        quote = None;
                    } else {
                        token.push(character);
                    }
                    index += 1;
                }
                '"' => {
                    if character == active_quote {
                        quote = None;
                        index += 1;
                    } else if matches!(character, '$' | '`') {
                        unsupported = Some(format!(
                            "shell expansion at character {offset} requires evaluation"
                        ));
                        break;
                    } else if character == '\\' {
                        let Some(next) = chars.get(index + 1).map(|(_, next)| *next) else {
                            unsupported = Some("shell escape was not complete".to_owned());
                            break;
                        };
                        match next {
                            '\\' | '"' | '$' | '`' => {
                                token.push(next);
                                index += 2;
                            }
                            '\n' => index += 2,
                            '\r' => {
                                index += 2;
                                if chars.get(index).is_some_and(|(_, next)| *next == '\n') {
                                    index += 1;
                                }
                            }
                            _ => {
                                unsupported = Some(format!(
                                    "shell escape at character {offset} requires evaluation"
                                ));
                                break;
                            }
                        }
                    } else {
                        token.push(character);
                        index += 1;
                    }
                }
                _ => unreachable!("shell quote is either single or double"),
            }
            word_started = true;
            continue;
        }
        match character {
            '\'' | '"' => {
                if !word_started {
                    token_start = offset;
                }
                quote = Some(character);
                word_started = true;
                index += 1;
            }
            '\\' => {
                let Some(next) = chars.get(index + 1).map(|(_, next)| *next) else {
                    unsupported = Some("shell escape was not complete".to_owned());
                    break;
                };
                if next == '\n' {
                    index += 2;
                    continue;
                }
                if next == '\r' {
                    index += 2;
                    if chars.get(index).is_some_and(|(_, next)| *next == '\n') {
                        index += 1;
                    }
                    continue;
                }
                if !word_started {
                    token_start = offset;
                }
                word_started = true;
                token.push(next);
                index += 2;
            }
            ';' | '\n' => {
                flush_shell_word(&mut current, &mut token, &mut word_started);
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
                index += 1;
            }
            character if character.is_ascii_whitespace() => {
                flush_shell_word(&mut current, &mut token, &mut word_started);
                index += 1;
            }
            '&' | '|' => {
                flush_shell_word(&mut current, &mut token, &mut word_started);
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
                conditional = true;
                if chars
                    .get(index + 1)
                    .is_some_and(|(_, next)| *next == character)
                {
                    index += 1;
                }
                index += 1;
            }
            '#' if !word_started => {
                while index < chars.len() && chars[index].1 != '\n' {
                    index += 1;
                }
            }
            '$' | '`' | '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?' | '[' | ']' => {
                unsupported = Some(format!(
                    "shell syntax at character {} requires evaluation",
                    token_start.max(offset)
                ));
                break;
            }
            _ => {
                if !word_started {
                    token_start = offset;
                }
                token.push(character);
                word_started = true;
                index += 1;
            }
        }
    }
    if quote.is_some() && unsupported.is_none() {
        unsupported = Some("unterminated shell quote".to_owned());
    }
    flush_shell_word(&mut current, &mut token, &mut word_started);
    if !current.is_empty() {
        segments.push(current);
    }
    if let Some(reason) = unsupported {
        return vec![unsupported_invocation(source_field, intent, &reason, None)];
    }
    if segments.len() > MAX_INVOCATIONS {
        return vec![unsupported_invocation(
            source_field,
            intent,
            "command candidate count exceeded the supported bound",
            None,
        )];
    }
    if let Some(count) = segments
        .iter()
        .map(Vec::len)
        .find(|count| *count > MAX_INVOCATION_ARGUMENTS)
    {
        return vec![unsupported_invocation(
            source_field,
            intent,
            &format!(
                "shell argument count {count} exceeded the supported bound; no arguments were retained"
            ),
            None,
        )];
    }
    if let Some(reason) = segments
        .iter()
        .find_map(|values| shell_segment_unsupported_reason(values))
    {
        return vec![unsupported_invocation(source_field, intent, reason, None)];
    }
    segments
        .into_iter()
        .filter(|values| !values.is_empty())
        .map(|values| {
            literal_invocation(
                &values,
                source_field,
                intent.clone(),
                if conditional {
                    InvocationCoverage::ConditionalDeclaration
                } else {
                    InvocationCoverage::StaticLiteral
                },
                None,
                conditional,
            )
        })
        .collect()
}

fn flush_shell_word(current: &mut Vec<BoundedText>, token: &mut String, word_started: &mut bool) {
    if *word_started {
        current.push(bounded_text(token, MAX_INVOCATION_STRING_CHARS));
        token.clear();
        *word_started = false;
    }
}

fn shell_segment_unsupported_reason(values: &[BoundedText]) -> Option<&'static str> {
    let first = values.first()?.text.as_str();
    if is_shell_assignment(first) {
        return Some("leading shell assignments are unsupported");
    }
    matches!(
        first,
        "case"
            | "do"
            | "done"
            | "elif"
            | "else"
            | "esac"
            | "fi"
            | "for"
            | "function"
            | "if"
            | "in"
            | "select"
            | "then"
            | "until"
            | "while"
    )
    .then_some("shell control or reserved syntax is unsupported")
}

fn is_shell_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[derive(Clone, Debug)]
enum JsTokenKind {
    Identifier(String),
    String(String),
    Punctuation(char),
}

#[derive(Clone, Debug)]
struct JsToken {
    kind: JsTokenKind,
    start: usize,
    end: usize,
}

fn javascript_declarations(
    text: &str,
    source_field: &str,
    intent: Option<BoundedText>,
) -> Vec<InvocationEvidence> {
    let tokens = match javascript_tokens(text) {
        Ok(tokens) => tokens,
        Err(reason) => {
            return vec![unsupported_invocation(source_field, intent, reason, None)];
        }
    };
    if let Some(reason) = javascript_control_context(&tokens) {
        return vec![unsupported_invocation(source_field, intent, reason, None)];
    }
    let conditional = tokens.iter().any(|token| {
        matches!(
            &token.kind,
            JsTokenKind::Identifier(identifier)
                if matches!(identifier.as_str(), "if" | "for" | "while" | "switch" | "function")
        )
    });
    let mut results = Vec::new();
    let mut index = 0;
    while index + 3 < tokens.len() {
        let is_call = matches!(&tokens[index].kind, JsTokenKind::Identifier(value) if value == "tools")
            && matches!(&tokens[index + 1].kind, JsTokenKind::Punctuation('.'))
            && matches!(&tokens[index + 2].kind, JsTokenKind::Identifier(value) if value == "exec_command")
            && matches!(&tokens[index + 3].kind, JsTokenKind::Punctuation('('));
        if !is_call {
            index += 1;
            continue;
        }
        let Some(close) = matching_punctuation(&tokens, index + 3, '(', ')') else {
            return vec![unsupported_invocation(
                source_field,
                intent,
                "JavaScript call has no complete argument list",
                None,
            )];
        };
        let span = Some(ByteSpan {
            start: tokens[index].start as u64,
            end: tokens[close].end as u64,
        });
        let object_start = index + 4;
        if !matches!(
            tokens.get(object_start).map(|token| &token.kind),
            Some(JsTokenKind::Punctuation('{'))
        ) {
            results.push(unsupported_invocation(
                source_field,
                intent.clone(),
                "JavaScript exec_command arguments are not a literal object",
                span,
            ));
            index = close + 1;
            continue;
        }
        let Some(object_end) = matching_punctuation(&tokens, object_start, '{', '}') else {
            return vec![unsupported_invocation(
                source_field,
                intent,
                "JavaScript object literal has no complete closing brace",
                span,
            )];
        };
        if object_end + 1 != close {
            results.push(unsupported_invocation(
                source_field,
                intent.clone(),
                "JavaScript exec_command takes one literal object argument",
                span,
            ));
            index = close + 1;
            continue;
        }
        let Some(fields) = javascript_object_fields(&tokens, object_start, object_end) else {
            results.push(unsupported_invocation(
                source_field,
                intent.clone(),
                "JavaScript object properties were not complete",
                span,
            ));
            index = close + 1;
            continue;
        };
        let mut command = None;
        let mut command_error = None;
        let mut declaration_intent = intent.clone();
        for (field_start, field_end) in fields {
            let field = &tokens[field_start..field_end];
            if field.is_empty() {
                continue;
            }
            let Some(colon) = top_level_colon(field) else {
                command_error.get_or_insert("JavaScript object property form is unsupported");
                continue;
            };
            let Some(key) = (colon == 1).then(|| javascript_key(&field[0])).flatten() else {
                command_error.get_or_insert("JavaScript computed object property is unsupported");
                continue;
            };
            let value = &field[colon + 1..];
            if is_command_property(key) {
                let Some(literal) = (value.len() == 1)
                    .then(|| match &value[0].kind {
                        JsTokenKind::String(value) => Some(value.clone()),
                        _ => None,
                    })
                    .flatten()
                else {
                    command_error = Some("JavaScript command property is not a literal string");
                    continue;
                };
                match command.as_deref() {
                    None => command = Some(literal),
                    Some(existing) if existing == literal => {}
                    Some(_) => {
                        command_error =
                            Some("JavaScript object has conflicting literal command properties");
                    }
                }
            } else if key == "why" && value.len() == 1 {
                if let JsTokenKind::String(value) = &value[0].kind {
                    declaration_intent = Some(bounded_text(value, MAX_INVOCATION_STRING_CHARS));
                }
            }
        }
        if let Some(reason) = command_error {
            results.push(unsupported_invocation(
                source_field,
                declaration_intent,
                reason,
                span,
            ));
        } else if let Some(command) = command {
            let mut declarations =
                shell_declarations(&command, source_field, declaration_intent.clone());
            if declarations.is_empty() {
                declarations.push(unsupported_invocation(
                    source_field,
                    declaration_intent.clone(),
                    "JavaScript command contained no literal invocation",
                    span,
                ));
            }
            for declaration in &mut declarations {
                declaration.span = span;
                declaration.conditional |= conditional;
                if conditional {
                    declaration.coverage = InvocationCoverage::ConditionalDeclaration;
                }
            }
            results.extend(declarations);
        } else {
            results.push(unsupported_invocation(
                source_field,
                declaration_intent,
                "JavaScript exec_command object has no literal command",
                span,
            ));
        }
        index = close + 1;
    }
    if results.len() > MAX_INVOCATIONS {
        return vec![unsupported_invocation(
            source_field,
            intent,
            "JavaScript invocation count exceeded the supported bound",
            None,
        )];
    }
    results
}

fn javascript_control_context(tokens: &[JsToken]) -> Option<&'static str> {
    for pair in tokens.windows(2) {
        if matches!(
            (&pair[0].kind, &pair[1].kind),
            (JsTokenKind::Punctuation('='), JsTokenKind::Punctuation('>'))
        ) {
            return Some("JavaScript arrow function bodies are unsupported");
        }
        if matches!(
            (&pair[0].kind, &pair[1].kind),
            (JsTokenKind::Punctuation('&'), JsTokenKind::Punctuation('&'))
                | (JsTokenKind::Punctuation('|'), JsTokenKind::Punctuation('|'))
        ) {
            return Some("JavaScript short-circuit expressions are unsupported");
        }
    }
    tokens
        .iter()
        .any(|token| matches!(&token.kind, JsTokenKind::Punctuation('?')))
        .then_some("JavaScript conditional expressions are unsupported")
}

fn javascript_key(token: &JsToken) -> Option<&str> {
    match &token.kind {
        JsTokenKind::Identifier(value) | JsTokenKind::String(value) => Some(value),
        JsTokenKind::Punctuation(_) => None,
    }
}

fn is_command_property(key: &str) -> bool {
    matches!(key, "cmd" | "command")
}

fn javascript_object_fields(
    tokens: &[JsToken],
    object_start: usize,
    object_end: usize,
) -> Option<Vec<(usize, usize)>> {
    let mut fields = Vec::new();
    let mut field_start = object_start + 1;
    let mut depth: usize = 0;
    for (index, token) in tokens
        .iter()
        .enumerate()
        .take(object_end)
        .skip(object_start + 1)
    {
        match &token.kind {
            JsTokenKind::Punctuation('(')
            | JsTokenKind::Punctuation('[')
            | JsTokenKind::Punctuation('{') => depth += 1,
            JsTokenKind::Punctuation(')')
            | JsTokenKind::Punctuation(']')
            | JsTokenKind::Punctuation('}') => depth = depth.checked_sub(1)?,
            JsTokenKind::Punctuation(',') if depth == 0 => {
                fields.push((field_start, index));
                field_start = index + 1;
            }
            _ => {}
        }
    }
    (depth == 0).then_some(()).map(|_| {
        fields.push((field_start, object_end));
        fields
    })
}

fn top_level_colon(field: &[JsToken]) -> Option<usize> {
    let mut depth: usize = 0;
    for (index, token) in field.iter().enumerate() {
        match &token.kind {
            JsTokenKind::Punctuation('(')
            | JsTokenKind::Punctuation('[')
            | JsTokenKind::Punctuation('{') => depth += 1,
            JsTokenKind::Punctuation(')')
            | JsTokenKind::Punctuation(']')
            | JsTokenKind::Punctuation('}') => depth = depth.saturating_sub(1),
            JsTokenKind::Punctuation(':') if depth == 0 => return Some(index),
            _ => {}
        }
    }
    None
}

fn javascript_tokens(text: &str) -> Result<Vec<JsToken>, &'static str> {
    let chars = text.char_indices().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let (start, character) = chars[index];
        if character.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if character == '/' && chars.get(index + 1).is_some_and(|(_, next)| *next == '/') {
            index += 2;
            while index < chars.len() && chars[index].1 != '\n' {
                index += 1;
            }
            continue;
        }
        if character == '/' && chars.get(index + 1).is_some_and(|(_, next)| *next == '*') {
            index += 2;
            let mut closed = false;
            while index + 1 < chars.len() {
                if chars[index].1 == '*' && chars[index + 1].1 == '/' {
                    index += 2;
                    closed = true;
                    break;
                }
                index += 1;
            }
            if !closed {
                return Err("JavaScript block comment was not closed");
            }
            continue;
        }
        if character == '/' {
            return Err("JavaScript slash syntax is unsupported");
        }
        if matches!(character, '\'' | '"' | '`') {
            let quote = character;
            let (value, cursor) = javascript_string(&chars, index, quote)?;
            tokens.push(JsToken {
                kind: JsTokenKind::String(value),
                start,
                end: chars[cursor.saturating_sub(1)].0 + quote.len_utf8(),
            });
            index = cursor;
            continue;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '$') {
            let mut cursor = index + 1;
            while cursor < chars.len()
                && (chars[cursor].1.is_ascii_alphanumeric() || matches!(chars[cursor].1, '_' | '$'))
            {
                cursor += 1;
            }
            let end = cursor
                .checked_sub(1)
                .map_or(start + character.len_utf8(), |last| {
                    chars[last].0 + chars[last].1.len_utf8()
                });
            tokens.push(JsToken {
                kind: JsTokenKind::Identifier(text[start..end].to_owned()),
                start,
                end,
            });
            index = cursor;
            continue;
        }
        tokens.push(JsToken {
            kind: JsTokenKind::Punctuation(character),
            start,
            end: start + character.len_utf8(),
        });
        index += 1;
    }
    javascript_punctuation_balance(&tokens)?;
    if tokens
        .last()
        .is_some_and(javascript_expression_is_incomplete)
    {
        return Err("JavaScript expression was incomplete");
    }
    Ok(tokens)
}

fn javascript_string(
    chars: &[(usize, char)],
    start: usize,
    quote: char,
) -> Result<(String, usize), &'static str> {
    let mut value = String::new();
    let mut cursor = start + 1;
    while cursor < chars.len() {
        let character = chars[cursor].1;
        if character == quote {
            return Ok((value, cursor + 1));
        }
        if quote != '`' && matches!(character, '\n' | '\r') {
            return Err("JavaScript string literal contains an unescaped line break");
        }
        if quote == '`'
            && character == '$'
            && chars.get(cursor + 1).is_some_and(|(_, next)| *next == '{')
        {
            return Err("JavaScript template interpolation is unsupported");
        }
        if character != '\\' {
            value.push(character);
            cursor += 1;
            continue;
        }

        let Some(escaped) = chars.get(cursor + 1).map(|(_, character)| *character) else {
            return Err("JavaScript string literal was not closed");
        };
        match escaped {
            '\\' | '/' | '\'' | '"' | '`' => {
                value.push(escaped);
                cursor += 2;
            }
            'b' => {
                value.push('\u{0008}');
                cursor += 2;
            }
            'f' => {
                value.push('\u{000c}');
                cursor += 2;
            }
            'n' => {
                value.push('\n');
                cursor += 2;
            }
            'r' => {
                value.push('\r');
                cursor += 2;
            }
            't' => {
                value.push('\t');
                cursor += 2;
            }
            'v' => {
                value.push('\u{000b}');
                cursor += 2;
            }
            '0' => {
                if chars
                    .get(cursor + 2)
                    .is_some_and(|(_, character)| character.is_ascii_digit())
                {
                    return Err("JavaScript legacy octal escapes are unsupported");
                }
                value.push('\0');
                cursor += 2;
            }
            'x' => {
                let Some((character, next)) = javascript_hex_escape(chars, cursor + 2, 2) else {
                    return Err("JavaScript hexadecimal escape was invalid");
                };
                value.push(character);
                cursor = next;
            }
            'u' => {
                if chars
                    .get(cursor + 2)
                    .is_some_and(|(_, character)| *character == '{')
                {
                    return Err("JavaScript Unicode code-point escapes are unsupported");
                }
                let Some((character, next)) = javascript_hex_escape(chars, cursor + 2, 4) else {
                    return Err("JavaScript Unicode escape was invalid");
                };
                value.push(character);
                cursor = next;
            }
            '\n' => cursor += 2,
            '\r' => {
                cursor += 2;
                if chars
                    .get(cursor)
                    .is_some_and(|(_, character)| *character == '\n')
                {
                    cursor += 1;
                }
            }
            _ => return Err("JavaScript string escape is unsupported"),
        }
    }
    Err("JavaScript string literal was not closed")
}

fn javascript_hex_escape(
    chars: &[(usize, char)],
    start: usize,
    digits: usize,
) -> Option<(char, usize)> {
    let mut value = 0u32;
    for offset in 0..digits {
        value = value.checked_mul(16)?;
        value += chars.get(start + offset)?.1.to_digit(16)?;
    }
    Some((char::from_u32(value)?, start + digits))
}

fn javascript_punctuation_balance(tokens: &[JsToken]) -> Result<(), &'static str> {
    let mut stack = Vec::new();
    for token in tokens {
        let JsTokenKind::Punctuation(character) = &token.kind else {
            continue;
        };
        if matches!(*character, '(' | '[' | '{') {
            stack.push(*character);
            if stack.len() > MAX_INVOCATION_DEPTH {
                return Err("JavaScript nesting exceeded the supported bound");
            }
            continue;
        }
        if !matches!(*character, ')' | ']' | '}') {
            continue;
        }
        let Some(opening) = stack.pop() else {
            return Err("JavaScript punctuation was unbalanced");
        };
        if matching_delimiter(opening) != Some(*character) {
            return Err("JavaScript punctuation was unbalanced");
        }
    }
    stack
        .is_empty()
        .then_some(())
        .ok_or("JavaScript punctuation was unbalanced")
}

fn javascript_expression_is_incomplete(token: &JsToken) -> bool {
    match &token.kind {
        JsTokenKind::Punctuation(character) => {
            matches!(
                character,
                '=' | '+'
                    | '-'
                    | '*'
                    | '/'
                    | '%'
                    | '&'
                    | '|'
                    | '!'
                    | '?'
                    | ':'
                    | '.'
                    | ','
                    | '<'
                    | '>'
                    | '^'
                    | '~'
            )
        }
        JsTokenKind::Identifier(identifier) => matches!(
            identifier.as_str(),
            "await"
                | "case"
                | "class"
                | "const"
                | "default"
                | "delete"
                | "else"
                | "extends"
                | "export"
                | "for"
                | "function"
                | "if"
                | "import"
                | "instanceof"
                | "in"
                | "let"
                | "new"
                | "of"
                | "return"
                | "throw"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "yield"
        ),
        JsTokenKind::String(_) => false,
    }
}

fn matching_punctuation(
    tokens: &[JsToken],
    start: usize,
    opening: char,
    closing: char,
) -> Option<usize> {
    if !matches!(
        tokens.get(start).map(|token| &token.kind),
        Some(JsTokenKind::Punctuation(character)) if *character == opening
    ) {
        return None;
    }
    let mut stack = vec![opening];
    for (index, token) in tokens.iter().enumerate().skip(start + 1) {
        let JsTokenKind::Punctuation(character) = &token.kind else {
            continue;
        };
        if matches!(*character, '(' | '[' | '{') {
            stack.push(*character);
            if stack.len() > MAX_INVOCATION_DEPTH {
                return None;
            }
        } else if matches!(*character, ')' | ']' | '}') {
            if matching_delimiter(stack.pop()?) != Some(*character) {
                return None;
            }
            if stack.is_empty() {
                return (*character == closing).then_some(index);
            }
        }
    }
    None
}

fn matching_delimiter(opening: char) -> Option<char> {
    match opening {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

fn bounded_text(text: &str, max: usize) -> BoundedText {
    let chars = text.chars().count();
    BoundedText {
        text: text.chars().take(max).collect(),
        chars,
        truncated: chars > max,
    }
}

fn collect_artifact_references(
    value: &Value,
    depth: usize,
    references: &mut Vec<ArtifactReference>,
) {
    if depth >= content::MAX_STRUCTURED_DEPTH || references.len() >= MAX_ARTIFACT_REFERENCES {
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                collect_artifact_references(value, depth + 1, references);
            }
        }
        Value::Object(values) => {
            if let Some(reference) = content::artifact_reference_object(values, "recorded-artifact")
            {
                references.push(reference);
            }
            for value in values.values() {
                collect_artifact_references(value, depth + 1, references);
            }
        }
        _ => {}
    }
}

fn reference(record: &EventRecord) -> PairRef {
    PairRef {
        ordinal: record.ordinal,
        native_id: record.native_id.clone(),
        record_ref: record.record_ref.clone(),
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
    use crate::model::{Role, SourceDescriptor, Turn, TurnKind};

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
            invocations: Vec::new(),
            artifact_references: Vec::new(),
            artifact_consumptions: Vec::new(),
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
            request_turn_id: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
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
            invocations: Vec::new(),
            artifact_references: Vec::new(),
            artifact_consumptions: Vec::new(),
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

    #[test]
    fn invocation_parser_keeps_literal_shell_and_argv_declarations_separate() {
        let shell = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({
                "cmd": "echo 'git status'; cargo test",
                "why": "check the build"
            }),
            "payload.arguments",
        );
        assert_eq!(
            shell
                .iter()
                .map(|invocation| invocation.program.as_deref())
                .collect::<Vec<_>>(),
            [Some("echo"), Some("cargo")]
        );
        assert_eq!(shell[0].subcommand.as_deref(), Some("git status"));
        assert_eq!(shell[0].intent.as_ref().unwrap().text, "check the build");

        let argv = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"argv":["git","status","--short"]}),
            "payload.argv",
        );
        assert_eq!(argv[0].coverage, InvocationCoverage::StaticLiteral);
        assert_eq!(argv[0].program.as_deref(), Some("git"));
        assert_eq!(argv[0].subcommand.as_deref(), Some("status"));
    }

    #[test]
    fn invocation_parser_marks_dynamic_and_conditional_javascript_without_evaluation() {
        let conditional = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String(
                "if (ready) { await tools.exec_command({cmd: `cargo test`}); }".to_owned(),
            ),
            "payload.arguments",
        );
        assert_eq!(conditional[0].program.as_deref(), Some("cargo"));
        assert!(conditional[0].conditional);
        assert_eq!(
            conditional[0].coverage,
            InvocationCoverage::ConditionalDeclaration
        );

        let dynamic = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String(
                "const command = `cargo test`; tools.exec_command({cmd: command});".to_owned(),
            ),
            "payload.arguments",
        );
        assert_eq!(dynamic[0].coverage, InvocationCoverage::Unsupported);
        assert!(dynamic[0].unsupported_reason.is_some());

        let comment = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String("// tools.exec_command({cmd: 'cargo test'})".to_owned()),
            "payload.arguments",
        );
        assert!(comment.is_empty());
    }

    #[test]
    fn shell_parser_preserves_boundaries_literals_comments_and_empty_words() {
        let separated = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": "echo one\necho two"}),
            "payload.arguments",
        );
        assert_eq!(separated.len(), 2);
        assert_eq!(separated[0].program.as_deref(), Some("echo"));
        assert_eq!(separated[0].subcommand.as_deref(), Some("one"));
        assert_eq!(separated[1].subcommand.as_deref(), Some("two"));

        let expanded = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": "echo \"$VALUE\""}),
            "payload.arguments",
        );
        assert_eq!(expanded[0].coverage, InvocationCoverage::Unsupported);
        assert!(expanded[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("expansion"));

        let escaped = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": r"echo a\ b"}),
            "payload.arguments",
        );
        assert_eq!(escaped.len(), 1);
        assert_eq!(escaped[0].subcommand.as_deref(), Some("a b"));

        let quoted_dollar = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": "echo '$VALUE'"}),
            "payload.arguments",
        );
        assert_eq!(quoted_dollar[0].subcommand.as_deref(), Some("$VALUE"));

        let comment = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": "echo a # trailing comment"}),
            "payload.arguments",
        );
        assert_eq!(comment.len(), 1);
        assert_eq!(comment[0].subcommand.as_deref(), Some("a"));
        assert!(comment[0].arguments.is_empty());

        let embedded_hash = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": "echo a#literal"}),
            "payload.arguments",
        );
        assert_eq!(embedded_hash[0].subcommand.as_deref(), Some("a#literal"));

        let empty_argument = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": "printf '%s' ''"}),
            "payload.arguments",
        );
        assert_eq!(empty_argument.len(), 1);
        assert_eq!(empty_argument[0].program.as_deref(), Some("printf"));
        assert_eq!(empty_argument[0].subcommand.as_deref(), Some("%s"));
        assert_eq!(empty_argument[0].arguments.len(), 1);
        assert_eq!(empty_argument[0].arguments[0].text, "");
        assert_eq!(empty_argument[0].arguments[0].chars, 0);

        let regex = invocations_from_tool(
            Some("exec"),
            &serde_json::Value::String(
                r#"const pattern = /tools.exec_command({cmd:"echo fake"})/;"#.to_owned(),
            ),
            "payload.arguments",
        );
        assert_eq!(regex.len(), 1);
        assert_eq!(regex[0].coverage, InvocationCoverage::Unsupported);
        assert!(regex[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("slash"));

        let too_many = (0..MAX_INVOCATION_ARGUMENTS)
            .map(|index| format!("arg-{index}"))
            .collect::<Vec<_>>();
        let too_many = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": format!("echo {}", too_many.join(" "))}),
            "payload.arguments",
        );
        assert_eq!(too_many.len(), 1);
        assert_eq!(too_many[0].coverage, InvocationCoverage::Unsupported);
        assert!(too_many[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("shell argument count"));
    }

    #[test]
    fn invocation_context_parser_rejects_assignments_control_and_deferred_calls() {
        let continuation = invocations_from_tool(
            Some("exec"),
            &serde_json::Value::String("echo \\\n next".to_owned()),
            "input",
        );
        assert_eq!(continuation.len(), 1);
        assert_eq!(continuation[0].program.as_deref(), Some("echo"));
        assert_eq!(continuation[0].subcommand.as_deref(), Some("next"));
        assert!(continuation[0].arguments.is_empty());

        for (script, reason) in [
            (
                "FLAG=1 echo next",
                "leading shell assignments are unsupported",
            ),
            (
                "if false; then echo next; fi",
                "shell control or reserved syntax is unsupported",
            ),
            (
                "const f = () => tools.exec_command({cmd:'echo next'});",
                "JavaScript arrow function bodies are unsupported",
            ),
            (
                "false && tools.exec_command({cmd:'echo next'});",
                "JavaScript short-circuit expressions are unsupported",
            ),
        ] {
            let declarations = invocations_from_tool(
                Some("exec"),
                &serde_json::Value::String(script.to_owned()),
                "input",
            );
            assert_eq!(declarations.len(), 1, "{script}");
            assert_eq!(declarations[0].coverage, InvocationCoverage::Unsupported);
            assert_eq!(
                declarations[0].unsupported_reason.as_ref().unwrap().text,
                reason,
                "{script}"
            );
        }
    }

    #[test]
    fn invocation_parser_accepts_only_complete_top_level_literal_javascript_properties() {
        let concatenated = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String(
                "await tools.exec_command({cmd: \"echo \" + variable});".to_owned(),
            ),
            "payload.arguments",
        );
        assert_eq!(concatenated[0].coverage, InvocationCoverage::Unsupported);
        assert!(concatenated[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("not a literal string"));

        let nested = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String(
                "await tools.exec_command({cmd: \"echo actual\", metadata: {cmd: \"touch imaginary\"}});"
                    .to_owned(),
            ),
            "payload.arguments",
        );
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].program.as_deref(), Some("echo"));
        assert_eq!(nested[0].subcommand.as_deref(), Some("actual"));

        for script in [
            "await tools.exec_command({cmd: \"echo actual\", ...metadata});",
            "await tools.exec_command({cmd: \"echo actual\", [key]: \"touch imaginary\"});",
        ] {
            let dynamic_property = invocations_from_tool(
                Some("orchestrator"),
                &serde_json::Value::String(script.to_owned()),
                "payload.arguments",
            );
            assert_eq!(dynamic_property.len(), 1);
            assert_eq!(
                dynamic_property[0].coverage,
                InvocationCoverage::Unsupported
            );
            assert!(dynamic_property[0]
                .unsupported_reason
                .as_ref()
                .unwrap()
                .text
                .contains("property"));
        }

        let escaped = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String(
                "await tools.exec_command({cmd: \"echo \\u0041\"});".to_owned(),
            ),
            "payload.arguments",
        );
        assert_eq!(escaped.len(), 1);
        assert_eq!(escaped[0].subcommand.as_deref(), Some("A"));

        let unsupported_escape = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String("await tools.exec_command({cmd: \"echo \\q\"});".to_owned()),
            "payload.arguments",
        );
        assert_eq!(
            unsupported_escape[0].coverage,
            InvocationCoverage::Unsupported
        );
        assert!(unsupported_escape[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("escape"));

        let incomplete = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String(
                "await tools.exec_command({cmd: \"echo complete\"}); const unfinished =".to_owned(),
            ),
            "payload.arguments",
        );
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].coverage, InvocationCoverage::Unsupported);
        assert!(incomplete[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("incomplete"));
    }

    #[test]
    fn invocation_parser_reports_malformed_and_truncated_argv_without_filtering_it() {
        let non_string = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"argv":["echo", 7, "not silently dropped"]}),
            "payload.arguments",
        );
        assert_eq!(non_string.len(), 1);
        assert_eq!(non_string[0].coverage, InvocationCoverage::Unsupported);
        assert!(non_string[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("non-string"));

        let too_many = (0..=MAX_INVOCATION_ARGUMENTS)
            .map(|index| Value::String(format!("arg-{index}")))
            .collect::<Vec<_>>();
        let too_many = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"argv": too_many}),
            "payload.arguments",
        );
        assert_eq!(too_many.len(), 1);
        assert_eq!(too_many[0].coverage, InvocationCoverage::Unsupported);
        assert!(too_many[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("exceeded"));

        let runtime = structured_runtime_invocations(
            &serde_json::json!({
                "command": ["echo", 7, "not silently dropped"],
                "intent": "run the recorded command"
            }),
            "payload.item.command",
        );
        assert_eq!(runtime.len(), 1);
        assert_eq!(runtime[0].origin, InvocationOrigin::StructuredRuntime);
        assert_eq!(runtime[0].coverage, InvocationCoverage::Unsupported);
        assert!(runtime[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("non-string"));

        let preferred = structured_runtime_invocations(
            &serde_json::json!({
                "command": ["echo", "actual"],
                "parsed_cmd": [{"kind": "structured-token"}]
            }),
            "payload.item.command",
        );
        assert_eq!(preferred.len(), 1);
        assert_eq!(preferred[0].program.as_deref(), Some("echo"));
        assert_eq!(preferred[0].subcommand.as_deref(), Some("actual"));

        let malformed = structured_runtime_invocations(
            &serde_json::json!({"command": "echo actual"}),
            "payload.item.command",
        );
        assert_eq!(malformed.len(), 1);
        assert_eq!(malformed[0].coverage, InvocationCoverage::Unsupported);
        assert!(malformed[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("argv array"));

        let runtime_too_many = (0..=MAX_INVOCATION_ARGUMENTS)
            .map(|index| Value::String(format!("arg-{index}")))
            .collect::<Vec<_>>();
        let runtime_too_many = structured_runtime_invocations(
            &serde_json::json!({"command": runtime_too_many}),
            "payload.item.command",
        );
        assert_eq!(runtime_too_many.len(), 1);
        assert_eq!(
            runtime_too_many[0].coverage,
            InvocationCoverage::Unsupported
        );
        assert!(runtime_too_many[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("exceeded"));
    }

    #[test]
    fn wrapper_results_do_not_witness_nested_static_declarations() {
        let mut call = event(EventKind::ToolCall, "wrapper");
        call.subtype = "tool".to_owned();
        call.status = Some("completed".to_owned());
        call.invocations = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd":"echo nested"}),
            "payload.arguments",
        );
        let projected = project(transcript(vec![turn(0, 10, call)], Vec::new()), usize::MAX);

        assert_eq!(projected.events.len(), 2);
        assert!(projected.events[0].event.invocations[0]
            .witnessed_result
            .is_none());
        assert!(projected.events[1].event.invocations.is_empty());
        assert!(projected.events[0].duration_ms.is_none());
        assert!(projected.events[1].duration_ms.is_none());
    }

    #[test]
    fn invocation_parser_bounds_scripts_and_does_not_scan_arbitrary_text_for_artifacts() {
        let oversized = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd":"x".repeat(MAX_INVOCATION_TEXT_CHARS + 1)}),
            "payload.arguments",
        );
        assert_eq!(oversized[0].coverage, InvocationCoverage::Unsupported);
        assert!(oversized[0].unsupported_reason.is_some());

        let references = artifact_references(&serde_json::json!(
            "a path /private/file and https://example.invalid/file"
        ));
        assert!(references.is_empty());
    }
}
