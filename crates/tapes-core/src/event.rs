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
            SourceBound::FileTail { .. }
                | SourceBound::RecordPage { .. }
                | SourceBound::InputCoverage { .. }
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
                for invocation in &mut records[call_index].event.invocations {
                    invocation.witnessed_result = records[index].record_ref.clone();
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
            let values = argv
                .iter()
                .filter_map(Value::as_str)
                .map(|value| bounded_text(value, MAX_INVOCATION_STRING_CHARS))
                .collect::<Vec<_>>();
            if !values.is_empty() && values.len() <= MAX_INVOCATION_ARGUMENTS {
                return vec![literal_invocation(
                    &values,
                    source_field,
                    intent,
                    InvocationCoverage::StaticLiteral,
                    None,
                    false,
                )];
            }
            if values.len() > MAX_INVOCATION_ARGUMENTS {
                return vec![unsupported_invocation(
                    source_field,
                    intent,
                    "argv argument count exceeded the supported bound",
                    None,
                )];
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

/// Project a command argv that a runtime record supplied as structured data.
/// The runtime's argv is stronger evidence than a preview but still describes
/// only the recorded outer execution; nested shell text is not reinterpreted.
pub fn structured_runtime_invocations(item: &Value, source_field: &str) -> Vec<InvocationEvidence> {
    let Some(argv) = item.get("parsed_cmd").and_then(Value::as_array) else {
        return Vec::new();
    };
    let values = argv
        .iter()
        .filter_map(Value::as_str)
        .take(MAX_INVOCATION_ARGUMENTS)
        .map(|value| bounded_text(value, MAX_INVOCATION_STRING_CHARS))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Vec::new();
    }
    let intent = item
        .get("intent")
        .or_else(|| item.get("description"))
        .and_then(Value::as_str)
        .map(|value| bounded_text(value, MAX_INVOCATION_STRING_CHARS));
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
    InvocationEvidence {
        origin: InvocationOrigin::StaticDeclaration,
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
    let mut escaped = false;
    let mut conditional = false;
    let mut unsupported = None;
    let mut token_start = 0usize;
    let mut index = 0usize;
    let flush_token = |current: &mut Vec<BoundedText>, token: &mut String| {
        if !token.is_empty() {
            current.push(bounded_text(token, MAX_INVOCATION_STRING_CHARS));
            token.clear();
        }
    };
    let chars = text.char_indices().collect::<Vec<_>>();
    while index < chars.len() {
        let (offset, character) = chars[index];
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                token.push(character);
            } else if character == '\\' && active_quote == '"' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            } else {
                token.push(character);
            }
            index += 1;
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                if token.is_empty() {
                    token_start = offset;
                }
            }
            character if character.is_ascii_whitespace() => {
                flush_token(&mut current, &mut token);
            }
            ';' | '\n' => {
                flush_token(&mut current, &mut token);
                if !current.is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
            }
            '&' | '|' => {
                flush_token(&mut current, &mut token);
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
            }
            '#' if token.is_empty() && current.is_empty() => {
                while index < chars.len() && chars[index].1 != '\n' {
                    index += 1;
                }
                continue;
            }
            '$' | '`' | '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?' => {
                unsupported = Some(format!(
                    "shell syntax at character {} requires evaluation",
                    token_start.max(offset)
                ));
                break;
            }
            _ => {
                if token.is_empty() {
                    token_start = offset;
                }
                token.push(character);
            }
        }
        index += 1;
    }
    if quote.is_some() {
        unsupported = Some("unterminated shell quote".to_owned());
    }
    flush_token(&mut current, &mut token);
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
    let Ok(tokens) = javascript_tokens(text) else {
        return vec![unsupported_invocation(
            source_field,
            intent,
            "JavaScript text was not lexically complete",
            None,
        )];
    };
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
        let mut command = None;
        let mut declaration_intent = intent.clone();
        let mut cursor = object_start + 1;
        while cursor < object_end {
            let Some(key) = tokens.get(cursor) else { break };
            let key_name = match &key.kind {
                JsTokenKind::Identifier(value) | JsTokenKind::String(value) => value,
                _ => {
                    cursor += 1;
                    continue;
                }
            };
            if !matches!(
                tokens.get(cursor + 1).map(|token| &token.kind),
                Some(JsTokenKind::Punctuation(':'))
            ) {
                cursor += 1;
                continue;
            }
            let Some(value) = tokens.get(cursor + 2) else {
                break;
            };
            match (&value.kind, key_name.as_str()) {
                (JsTokenKind::String(value), "cmd" | "command") => {
                    command = Some(value.clone());
                }
                (JsTokenKind::String(value), "why") => {
                    declaration_intent = Some(bounded_text(value, MAX_INVOCATION_STRING_CHARS));
                }
                (_, "cmd" | "command") => {
                    results.push(unsupported_invocation(
                        source_field,
                        declaration_intent.clone(),
                        "JavaScript command property is not a literal string",
                        span,
                    ));
                }
                _ => {}
            }
            cursor += 3;
        }
        if let Some(command) = command {
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
    results.truncate(MAX_INVOCATIONS);
    results
}

fn javascript_tokens(text: &str) -> Result<Vec<JsToken>, ()> {
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
                return Err(());
            }
            continue;
        }
        if matches!(character, '\'' | '"' | '`') {
            let quote = character;
            let mut value = String::new();
            let mut escaped = false;
            let mut cursor = index + 1;
            let mut closed = false;
            while cursor < chars.len() {
                let (_, character) = chars[cursor];
                if quote == '`'
                    && character == '$'
                    && chars.get(cursor + 1).is_some_and(|(_, next)| *next == '{')
                {
                    return Err(());
                }
                if escaped {
                    if character == '{' && quote == '`' {
                        return Err(());
                    }
                    escaped = false;
                    value.push(character);
                } else if character == '\\' {
                    escaped = true;
                } else if character == quote {
                    closed = true;
                    cursor += 1;
                    break;
                } else {
                    value.push(character);
                }
                cursor += 1;
            }
            if !closed {
                return Err(());
            }
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
    Ok(tokens)
}

fn matching_punctuation(
    tokens: &[JsToken],
    start: usize,
    opening: char,
    closing: char,
) -> Option<usize> {
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match &token.kind {
            JsTokenKind::Punctuation(character) if *character == opening => depth += 1,
            JsTokenKind::Punctuation(character) if *character == closing => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        if depth > MAX_INVOCATION_DEPTH {
            return None;
        }
    }
    None
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
            metadata: None,
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
