use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::content::{self, ArtifactReference, ContentCoverage, ContentInventory, ContentPart};
use crate::model::{
    BoundedText, ByteSpan, ReadEvidence, RecordRef, Session, SourceBound, TerminalObservation,
    TextTailEvidence, Transcript, Truncation, Turn,
};

pub const EVENTS_SCHEMA: &str = "tapes-events/8";
const PREVIEW_CHARS: usize = 200;
pub const MAX_INVOCATION_TEXT_CHARS: usize = 64 * 1024;
pub const MAX_INVOCATIONS: usize = 32;
pub const MAX_INVOCATION_ARGUMENTS: usize = 32;
pub const MAX_INVOCATION_STRING_CHARS: usize = 2 * 1024;
pub const MAX_INVOCATION_DEPTH: usize = 32;
const MAX_ARTIFACT_REFERENCES: usize = 64;

/// What a caller asks of an event projection beyond its bounded default. The
/// options change what a record carries, never the object's members or its
/// schema version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventOptions {
    /// Publish each tool call's complete recorded argument text in place of
    /// the 200-character prefix. `chars` already states the whole length, so
    /// the two agree and no member is added.
    pub full_arguments: bool,
}

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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    ToolCall,
    ToolResult,
}

/// A payload field represented by its character count and a bounded prefix.
/// A tool call's arguments retain their complete text, which is published
/// instead of the prefix when a caller asks for complete arguments; every
/// other payload retains only what it publishes.
#[derive(Clone, Debug, Eq, Serialize)]
pub struct Bounded {
    pub chars: usize,
    pub preview: String,
    /// The complete recorded text, set only where the reader retained it, and
    /// never a serialized member of its own: `widen` publishes it as the
    /// prefix so the object's shape is the same either way.
    #[serde(skip)]
    full: Option<String>,
}

impl PartialEq for Bounded {
    /// Retention is a capability rather than a published fact, so two values
    /// stating the same count and prefix are the same value.
    fn eq(&self, other: &Self) -> bool {
        self.chars == other.chars && self.preview == other.preview
    }
}

impl Bounded {
    pub fn from_value(value: &Value) -> Option<Self> {
        Self::from_value_with(value, false)
    }

    /// As `from_value`, retaining the complete text so a caller that asks for
    /// complete arguments can be handed more than the prefix.
    pub fn retaining_value(value: &Value) -> Option<Self> {
        Self::from_value_with(value, true)
    }

    fn from_value_with(value: &Value, retain: bool) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        let text = value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string());
        Some(Self::from_text_with(&text, retain))
    }

    pub fn from_text(text: &str) -> Self {
        Self::from_text_with(text, false)
    }

    fn from_text_with(text: &str, retain: bool) -> Self {
        Self {
            chars: text.chars().count(),
            preview: text.chars().take(PREVIEW_CHARS).collect(),
            full: retain.then(|| text.to_owned()),
        }
    }

    /// Publish the complete retained text in place of the prefix. A value
    /// that retained nothing keeps the prefix it has.
    pub(crate) fn widen(&mut self) {
        if let Some(full) = self.full.take() {
            self.preview = full;
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
    pub read: Option<ReadLocator>,
    #[serde(skip)]
    pub(crate) returned_read: Option<ReturnedRead>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invocations: Vec<InvocationEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_references: Vec<ArtifactReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_consumptions: Vec<ArtifactConsumption>,
    /// A single native record carries both the invocation and its result.
    /// This is internal pairing state, not another wire field.
    #[serde(skip)]
    pub(crate) self_contained: bool,
}

/// A file read declared by the recorded tool call. Missing range means the
/// tool did not establish one; `whole` is emitted only by a whole-file form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReadLocator {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<ReadLines>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whole: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub succeeded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReadLines {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReturnedRead {
    pub succeeded: Option<bool>,
    pub sha256: Option<String>,
}

pub(crate) fn returned_text(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| {
        let parts = value.as_array()?;
        (parts.len() == 1 && parts[0]["type"] == "text")
            .then(|| parts[0]["text"].as_str())
            .flatten()
    })
}

pub(crate) fn returned_read(value: &Value, succeeded: Option<bool>) -> ReturnedRead {
    ReturnedRead {
        succeeded,
        sha256: (succeeded != Some(false))
            .then(|| returned_text(value))
            .flatten()
            .map(|text| format!("sha256:{:x}", Sha256::digest(text.as_bytes()))),
    }
}

pub(crate) fn direct_read(name: Option<&str>, input: &Value) -> Option<ReadLocator> {
    if !matches!(name, Some("Read" | "read")) {
        return None;
    }
    let path = input["file_path"]
        .as_str()
        .or_else(|| input["path"].as_str())?;
    let offset = input["offset"].as_u64();
    let limit = input["limit"].as_u64();
    let lines = offset.zip(limit).and_then(|(start, count)| {
        (start > 0 && count > 0).then(|| ReadLines {
            start,
            end: start.saturating_add(count - 1),
        })
    });
    Some(ReadLocator {
        path: path.to_owned(),
        lines,
        whole: (input["whole"].as_bool() == Some(true)).then_some(true),
        succeeded: None,
        sha256: None,
    })
}

pub(crate) fn shell_read(command: &str) -> Option<ReadLocator> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let (path, lines, whole) = match words.as_slice() {
        ["cat", path] => (*path, None, Some(true)),
        ["sed", "-n", range, path] => {
            let range = range.trim_matches(|c| c == '\'' || c == '"');
            let lines = range
                .strip_suffix('p')
                .and_then(|range| range.split_once(','))
                .and_then(|(start, end)| {
                    Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?))
                })
                .and_then(|(start, end)| {
                    (start > 0 && end >= start).then_some(ReadLines { start, end })
                });
            (*path, lines, None)
        }
        _ => return None,
    };
    if path.is_empty() || path.contains(['|', ';', '&', '$', '*', '?', '`', '<', '>']) {
        return None;
    }
    Some(ReadLocator {
        path: path.trim_matches(|c| c == '\'' || c == '"').to_owned(),
        lines,
        whole,
        succeeded: None,
        sha256: None,
    })
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
    pub event_id: Option<String>,
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
    /// Keep the records `filter` admits and count the pairs among them.
    pub fn retain(&mut self, filter: &EventFilter<'_>) {
        let selected = self
            .events
            .iter()
            .filter(|record| filter.selects_call(record))
            .filter_map(|record| record.event.call_id.clone())
            .collect::<HashSet<_>>();
        self.events
            .retain(|record| filter.keeps(record, |call_id| selected.contains(call_id)));
        self.pairs = pair_counts(&self.events);
    }
}

/// Which paired records a caller asked for. Each list is matched exactly and
/// an empty one admits every record. A record declaring a requested nested
/// program selects its call id, so the records sharing that id are kept with
/// it; filtering never changes how a record was paired.
#[derive(Clone, Copy, Debug, Default)]
pub struct EventFilter<'a> {
    pub names: &'a [String],
    pub call_ids: &'a [String],
    pub programs: &'a [String],
}

impl EventFilter<'_> {
    /// Whether `record` declares a requested program, selecting its call id.
    pub fn selects_call(&self, record: &EventRecord) -> bool {
        !self.programs.is_empty() && program_matches(record, self.programs)
    }

    /// Whether `record` is kept, where `selected` answers whether a call id
    /// was selected by a record [`selects_call`](Self::selects_call) admits
    /// among the records the caller returns.
    pub fn keeps(&self, record: &EventRecord, selected: impl Fn(&str) -> bool) -> bool {
        let names = self.names.is_empty()
            || record
                .event
                .name
                .as_ref()
                .is_some_and(|name| self.names.contains(name));
        let call_ids = self.call_ids.is_empty()
            || record
                .event
                .call_id
                .as_ref()
                .is_some_and(|call_id| self.call_ids.contains(call_id));
        let programs = self.programs.is_empty()
            || program_matches(record, self.programs)
            || record.event.call_id.as_deref().is_some_and(selected);
        names && call_ids && programs
    }
}

fn program_matches(record: &EventRecord, programs: &[String]) -> bool {
    record.event.invocations.iter().any(|invocation| {
        invocation
            .program
            .as_ref()
            .is_some_and(|program| programs.iter().any(|wanted| wanted == program))
    })
}

/// A `tapes-events` object written one record at a time, for a read that
/// streams a recording: [`open`](Self::open) before the first record,
/// [`record`](Self::record) for each, and [`close`](Self::close) with the
/// object carrying every fact but its records. The bytes are the ones
/// serializing that object with the records in place would produce.
pub struct StreamedEventsJson {
    records: usize,
}

impl StreamedEventsJson {
    pub fn open(out: &mut impl std::io::Write, session: &Session) -> std::io::Result<Self> {
        out.write_all(b"{\"schema\":")?;
        serde_json::to_writer(&mut *out, EVENTS_SCHEMA)?;
        out.write_all(b",\"session\":")?;
        serde_json::to_writer(&mut *out, session)?;
        out.write_all(b",\"events\":[")?;
        Ok(Self { records: 0 })
    }

    pub fn record(
        &mut self,
        out: &mut impl std::io::Write,
        record: &EventRecord,
    ) -> std::io::Result<()> {
        if self.records > 0 {
            out.write_all(b",")?;
        }
        serde_json::to_writer(&mut *out, record)?;
        self.records += 1;
        Ok(())
    }

    pub fn close(
        self,
        out: &mut impl std::io::Write,
        events: &EventTranscript,
    ) -> std::io::Result<()> {
        let tail = serde_json::to_vec(&EventsTail::of(events))?;
        // The tail is an object that always opens on `pairs`, so dropping both
        // its braces continues the events object.
        out.write_all(b"],")?;
        out.write_all(&tail[1..tail.len() - 1])?;
        out.write_all(b"}")
    }
}

/// The members of an [`EventTranscript`] that follow its records, serialized
/// under the same names and omission rules.
#[derive(Serialize)]
struct EventsTail<'a> {
    pairs: &'a PairCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    read: Option<&'a ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<&'a TerminalObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_tail: Option<&'a TextTailEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a ContentInventory>,
    truncated: bool,
    #[serde(skip_serializing_if = "truncation_is_empty")]
    truncation: &'a Truncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: &'a Vec<String>,
}

impl<'a> EventsTail<'a> {
    fn of(events: &'a EventTranscript) -> Self {
        Self {
            pairs: &events.pairs,
            read: events.read.as_ref(),
            terminal: events.terminal.as_ref(),
            text_tail: events.text_tail.as_ref(),
            content: events.content.as_ref(),
            truncated: events.truncated,
            truncation: &events.truncation,
            notes: &events.notes,
        }
    }
}

fn truncation_is_empty(truncation: &Truncation) -> bool {
    truncation.is_empty()
}

/// Pair every event in the bounded read before applying the requested turn
/// window. Pair references therefore survive when their counterpart is
/// outside the returned window.
pub fn project(transcript: Transcript, tail: usize) -> EventTranscript {
    project_with(transcript, tail, EventOptions::default())
}

/// Pair and project as `project` does, honoring a caller's options.
pub fn project_with(transcript: Transcript, tail: usize, options: EventOptions) -> EventTranscript {
    let total_turns = transcript.turns.len();
    let unpaired = transcript
        .turns
        .iter()
        .flat_map(turn_records)
        .collect::<Vec<_>>();
    let mut index = PairIndex::default();
    for record in &unpaired {
        index.observe(record);
    }
    let mut pairing = index.pairing(read_was_bounded(&transcript.truncation.source));
    let mut records = unpaired
        .into_iter()
        .map(|record| pairing.emit(record))
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("records replayed from one vector match their observation");

    let returned_turns = total_turns.min(tail);
    let first_ordinal = total_turns.saturating_sub(returned_turns);
    records.retain(|record| record.ordinal >= first_ordinal);
    if options.full_arguments {
        for record in &mut records {
            if let Some(arguments) = record.event.arguments.as_mut() {
                arguments.widen();
            }
        }
    }

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

/// Whether the read stopped short of the start of its source, so a result
/// without its call may answer a call recorded beyond the bound.
pub fn read_was_bounded(source: &[SourceBound]) -> bool {
    source.iter().any(|bound| {
        matches!(
            bound,
            SourceBound::FileTail { .. }
                | SourceBound::RecordPage { .. }
                | SourceBound::InputCoverage { .. }
        )
    })
}

/// The unpaired tool-event records one turn contributes, in order: its tool
/// event, then the result a call part carries when it already records its
/// own completion.
pub fn turn_records(turn: &Turn) -> impl Iterator<Item = EventRecord> {
    let call = turn.tool.clone().map(|event| {
        let event_id = event_identity(turn, &event.kind);
        EventRecord {
            ordinal: turn.ordinal,
            native_id: turn
                .native_id
                .clone()
                .or_else(|| event_id.as_ref().map(|id| format!("record-{id}"))),
            event_id,
            record_ref: turn.record_ref.clone(),
            parts: turn.parts.clone(),
            coverage: turn.coverage.clone(),
            ts: turn.ts,
            event,
            pair: None,
            duration_ms: None,
            incomplete: None,
        }
    });
    let result = call
        .as_ref()
        .filter(|call| {
            call.event.kind == EventKind::ToolCall
                && (call.event.self_contained
                    || matches!(
                        call.event.status.as_deref(),
                        Some("completed" | "error" | "failed")
                    ))
                && (call.event.self_contained
                    || call.event.subtype == "tool"
                    || (call.event.arguments.is_some() && call.event.output.is_some()))
        })
        .map(|call| {
            let mut result = call.clone();
            result.ts = result.event.completed_ts;
            result.event.kind = EventKind::ToolResult;
            result.event_id = event_identity(turn, &EventKind::ToolResult);
            if turn.native_id.is_none() {
                result.native_id = result.event_id.as_ref().map(|id| format!("record-{id}"));
            }
            result.event.arguments = None;
            result.event.read = None;
            result.event.invocations.clear();
            result.event.artifact_references.clear();
            result.event.artifact_consumptions.clear();
            result
        });
    call.into_iter().chain(result)
}

fn event_identity(turn: &Turn, kind: &EventKind) -> Option<String> {
    let reference = turn.record_ref.as_ref()?;
    let coordinate = format!(
        "{}\0{:?}\0{:?}\0{:?}\0{}\0{:?}",
        reference.domain,
        reference.span,
        reference.native_id,
        reference.pointer,
        reference.part_index,
        kind
    );
    Some(format!(
        "sha256:{:x}",
        Sha256::digest(coordinate.as_bytes())
    ))
}

/// The first of two pairing passes over one sequence of unpaired records.
///
/// A result pairs with the most recent open call carrying its call id. A call
/// learns everything its result contributes (the pair reference, the
/// duration, artifact consumption, and the witnessed structured runtime
/// invocations) only once that result is seen, so this pass observes every
/// record and keeps just those facts for each answered call, keyed by the
/// call's position among the calls. [`PairIndex::pairing`] then replays the
/// same records and emits each one final, in order. Memory follows the
/// calls, never the records.
#[derive(Default)]
pub struct PairIndex {
    open: HashMap<String, Vec<ObservedCall>>,
    results: HashMap<usize, ResultFacts>,
    calls: usize,
    replay: Replay,
}

/// The second pairing pass: each record handed to [`Pairing::emit`] leaves
/// final. The records must be the ones [`PairIndex::observe`] saw, in the
/// same order; a replay that differs is an error rather than a wrong pair.
pub struct Pairing {
    open: HashMap<String, Vec<EmittedCall>>,
    results: HashMap<usize, ResultFacts>,
    calls: usize,
    read_was_bounded: bool,
    observed: Replay,
    replay: Replay,
    counts: PairCounts,
}

struct ObservedCall {
    sequence: usize,
    declares_artifacts: bool,
}

struct EmittedCall {
    reference: PairRef,
    result: Option<PairRef>,
}

/// What an answered call takes from its result.
struct ResultFacts {
    reference: PairRef,
    ts: Option<DateTime<Utc>>,
    artifact_references: Vec<ArtifactReference>,
    returned_read: Option<ReturnedRead>,
}

/// The identity of a record sequence, so a replay can be checked against the
/// observation without keeping the records.
#[derive(Default)]
struct Replay {
    records: usize,
    digest: DefaultHasher,
}

impl Replay {
    fn add(&mut self, record: &EventRecord) {
        self.records += 1;
        reference(record).hash(&mut self.digest);
        record.event.kind.hash(&mut self.digest);
        record.event.call_id.hash(&mut self.digest);
    }

    fn matches(&self, other: &Self) -> bool {
        self.records == other.records && self.digest.finish() == other.digest.finish()
    }
}

const REPLAY_DIVERGED: &str =
    "tool events differed between the two pairing passes; the recording changed while it was read";

impl PairIndex {
    pub fn observe(&mut self, record: &EventRecord) {
        self.replay.add(record);
        let Some(call_id) = &record.event.call_id else {
            return;
        };
        match record.event.kind {
            EventKind::ToolCall => {
                self.open
                    .entry(call_id.clone())
                    .or_default()
                    .push(ObservedCall {
                        sequence: self.calls,
                        declares_artifacts: !record.event.artifact_references.is_empty(),
                    });
                self.calls += 1;
            }
            EventKind::ToolResult => {
                let Some(call) = pop_open(&mut self.open, call_id) else {
                    return;
                };
                self.results.insert(
                    call.sequence,
                    ResultFacts {
                        reference: reference(record),
                        ts: record.ts,
                        artifact_references: if call.declares_artifacts {
                            record.event.artifact_references.clone()
                        } else {
                            Vec::new()
                        },
                        returned_read: record.event.returned_read.clone(),
                    },
                );
            }
        }
    }

    /// Start the emitting pass. `read_was_bounded` names why a result
    /// without its call is incomplete; see [`read_was_bounded`].
    pub fn pairing(self, read_was_bounded: bool) -> Pairing {
        Pairing {
            open: HashMap::new(),
            results: self.results,
            calls: 0,
            read_was_bounded,
            observed: self.replay,
            replay: Replay::default(),
            counts: PairCounts::default(),
        }
    }
}

impl Pairing {
    pub fn emit(&mut self, mut record: EventRecord) -> anyhow::Result<EventRecord> {
        self.replay.add(&record);
        if self.replay.records > self.observed.records {
            anyhow::bail!(REPLAY_DIVERGED);
        }
        let orphan = if self.read_was_bounded {
            Incomplete::CallBeforeReadBound
        } else {
            Incomplete::CallNotRecorded
        };
        let Some(call_id) = record.event.call_id.clone() else {
            record.incomplete = Some(match record.event.kind {
                EventKind::ToolCall => Incomplete::NoResultInRead,
                EventKind::ToolResult => orphan,
            });
            self.counts.incomplete += 1;
            return Ok(record);
        };
        match record.event.kind {
            EventKind::ToolCall => {
                let result = self.results.remove(&self.calls);
                self.calls += 1;
                let expected = result.as_ref().map(|facts| facts.reference.clone());
                match result {
                    Some(facts) => complete_call(&mut record, facts),
                    None => {
                        record.incomplete = Some(Incomplete::NoResultInRead);
                        record.event.artifact_consumptions =
                            artifact_consumptions(&record.event.artifact_references, &[], None);
                        self.counts.incomplete += 1;
                    }
                }
                self.open.entry(call_id).or_default().push(EmittedCall {
                    reference: reference(&record),
                    result: expected,
                });
            }
            EventKind::ToolResult => match pop_open(&mut self.open, &call_id) {
                Some(call) => {
                    if call.result.as_ref() != Some(&reference(&record)) {
                        anyhow::bail!(REPLAY_DIVERGED);
                    }
                    record.pair = Some(call.reference);
                    self.counts.complete += 1;
                }
                None => {
                    record.incomplete = Some(orphan);
                    self.counts.incomplete += 1;
                }
            },
        }
        Ok(record)
    }

    /// Close the emitting pass. The counts are those of the emitted records:
    /// every pair once, and every incomplete record.
    pub fn finish(self) -> anyhow::Result<PairCounts> {
        if !self.replay.matches(&self.observed) {
            anyhow::bail!(REPLAY_DIVERGED);
        }
        Ok(self.counts)
    }
}

fn pop_open<T>(open: &mut HashMap<String, Vec<T>>, call_id: &str) -> Option<T> {
    let calls = open.get_mut(call_id)?;
    let call = calls.pop();
    if calls.is_empty() {
        open.remove(call_id);
    }
    call
}

fn complete_call(call: &mut EventRecord, result: ResultFacts) {
    if let (Some(read), Some(returned)) = (&mut call.event.read, &result.returned_read) {
        read.succeeded = returned.succeeded;
        read.sha256 = returned.sha256.clone();
    }
    call.event.artifact_consumptions = artifact_consumptions(
        &call.event.artifact_references,
        &result.artifact_references,
        result.reference.record_ref.clone(),
    );
    let separate_native_record = matches!(
        (call.record_ref.as_ref(), result.reference.record_ref.as_ref()),
        (Some(call), Some(result)) if call != result
    );
    if separate_native_record {
        for invocation in &mut call.event.invocations {
            if invocation.coverage == InvocationCoverage::StructuredRuntime {
                invocation.witnessed_result = result.reference.record_ref.clone();
            }
        }
    }
    call.duration_ms = match (call.ts, result.ts) {
        (Some(call_ts), Some(result_ts)) if result_ts >= call_ts => {
            Some(result_ts.signed_duration_since(call_ts).num_milliseconds())
        }
        _ => None,
    };
    call.pair = Some(result.reference);
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
    let shortened_name = values
        .iter()
        .take(2)
        .enumerate()
        .find_map(|(index, value)| {
            value
                .truncated
                .then_some(if index == 0 { "program" } else { "subcommand" })
        });
    if let Some(name) = shortened_name {
        let origin = if coverage == InvocationCoverage::StructuredRuntime {
            InvocationOrigin::StructuredRuntime
        } else {
            InvocationOrigin::StaticDeclaration
        };
        return unsupported_invocation_from(
            origin,
            source_field,
            intent,
            &format!("{name} token was shortened; exact invocation identity is unsupported"),
            span,
        );
    }
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
    let mut tally = PairTally::default();
    for record in records {
        tally.add(record, true);
    }
    tally.finish()
}

/// Pair counts over the records a caller keeps, taken as paired records pass
/// in the order [`Pairing::emit`] leaves them, so a call always passes before
/// its result. A complete pair counts once when either of its halves is kept,
/// and an incomplete record counts when it is kept. Memory follows the kept
/// calls whose result has not passed yet.
#[derive(Default)]
pub struct PairTally {
    counts: PairCounts,
    awaiting: HashSet<(PairRef, PairRef, Option<String>)>,
}

impl PairTally {
    /// Count one record, which the caller either `kept` or passed over.
    pub fn add(&mut self, record: &EventRecord, kept: bool) {
        if kept && record.incomplete.is_some() {
            self.counts.incomplete += 1;
        }
        let Some(counterpart) = record.pair.clone() else {
            return;
        };
        let own = reference(record);
        let call_id = record.event.call_id.clone();
        match record.event.kind {
            EventKind::ToolCall => {
                if kept && self.awaiting.insert((own, counterpart, call_id)) {
                    self.counts.complete += 1;
                }
            }
            EventKind::ToolResult => {
                if !self.awaiting.remove(&(counterpart, own, call_id)) && kept {
                    self.counts.complete += 1;
                }
            }
        }
    }

    pub fn finish(self) -> PairCounts {
        self.counts
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
            model_observation: None,
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
            read: None,
            returned_read: None,
            completed_ts: None,
            invocations: Vec::new(),
            artifact_references: Vec::new(),
            artifact_consumptions: Vec::new(),
            self_contained: false,
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
            read: None,
            returned_read: None,
            completed_ts: None,
            invocations: Vec::new(),
            artifact_references: Vec::new(),
            artifact_consumptions: Vec::new(),
            self_contained: false,
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

    /// An independent statement of pairing over one indexed vector: each call
    /// is mutated in place when its result arrives and unanswered calls are
    /// closed after the last record. The two-pass core must reproduce it.
    fn whole_vector_pairing(records: &mut [EventRecord], read_was_bounded: bool) {
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
                    if let (Some(read), Some(returned)) = (
                        &mut records[call_index].event.read,
                        &records[index].event.returned_read,
                    ) {
                        read.succeeded = returned.succeeded;
                        read.sha256 = returned.sha256.clone();
                    }
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
                    records[call_index].duration_ms =
                        match (records[call_index].ts, records[index].ts) {
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

    type Replayer<'a> = dyn FnMut(&mut dyn FnMut(Turn)) + 'a;

    /// Pair the way a streaming reader does: `replay` hands every turn to its
    /// sink one at a time, once for each pass.
    fn two_passes(
        replay: &mut Replayer<'_>,
        read_was_bounded: bool,
    ) -> (Vec<EventRecord>, PairCounts) {
        let mut index = PairIndex::default();
        replay(&mut |turn| turn_records(&turn).for_each(|record| index.observe(&record)));
        let mut pairing = index.pairing(read_was_bounded);
        let mut emitted = Vec::new();
        replay(&mut |turn| {
            for record in turn_records(&turn) {
                emitted.push(pairing.emit(record).expect("an identical replay pairs"));
            }
        });
        (
            emitted,
            pairing.finish().expect("an identical replay closes"),
        )
    }

    /// Assert that both passes over `replay` reproduce the whole-vector
    /// pairing of `turns`, and return that pairing.
    fn assert_two_passes_match(
        label: &str,
        turns: &[Turn],
        replay: &mut Replayer<'_>,
        read_was_bounded: bool,
    ) -> Vec<EventRecord> {
        let mut expected = turns.iter().flat_map(turn_records).collect::<Vec<_>>();
        whole_vector_pairing(&mut expected, read_was_bounded);
        let (emitted, counts) = two_passes(replay, read_was_bounded);
        assert_eq!(emitted, expected, "{label}");
        assert_eq!(counts, pair_counts(&expected), "{label}");
        expected
    }

    fn fixture_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
    }

    #[test]
    fn two_pass_pairing_reproduces_the_whole_vector_over_every_fixture_recording() {
        use crate::backend::{
            claude::ClaudeBackend, codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend,
            Backend, Query,
        };
        use crate::input::{InputBackend, InputFormat, InputOptions};

        let root = fixture_root();
        let mut totals = PairCounts::default();
        let mut sessions = 0;
        let streamed: Vec<Box<dyn Backend>> = vec![
            Box::new(ClaudeBackend::new(root.join("claude"))),
            Box::new(CodexBackend::new(root.join("codex"))),
            Box::new(PiBackend::new(root.join("pi"))),
        ];
        for backend in &streamed {
            for session in backend.list(&Query::unscoped(usize::MAX)).unwrap().sessions {
                let label = format!("{} {}", backend.harness(), session.id);
                let mut replay = |sink: &mut dyn FnMut(Turn)| {
                    let mut ordinal = 0;
                    backend
                        .stream_transcript(&session, None, &mut |mut turn| {
                            turn.ordinal = ordinal;
                            ordinal += 1;
                            sink(turn);
                            Ok(())
                        })
                        .unwrap();
                };
                let mut turns = Vec::new();
                replay(&mut |turn| turns.push(turn));
                let paired = assert_two_passes_match(&label, &turns, &mut replay, false);
                let counts = pair_counts(&paired);
                totals.complete += counts.complete;
                totals.incomplete += counts.incomplete;
                sessions += 1;
            }
        }
        let read: Vec<Box<dyn Backend>> = vec![
            Box::new(OpenCodeBackend::new(root.join("opencode/opencode2"))),
            Box::new(
                InputBackend::new(InputOptions::new(
                    vec![root.join("input")],
                    InputFormat::Auto,
                ))
                .unwrap(),
            ),
        ];
        for backend in &read {
            for session in backend.list(&Query::unscoped(usize::MAX)).unwrap().sessions {
                let label = format!("{} {}", backend.harness(), session.id);
                let transcript = backend.transcript(&session, usize::MAX).unwrap();
                let bounded = read_was_bounded(&transcript.truncation.source);
                let turns = transcript.turns.clone();
                let mut replay =
                    |sink: &mut dyn FnMut(Turn)| turns.iter().cloned().for_each(&mut *sink);
                let paired = assert_two_passes_match(&label, &turns, &mut replay, bounded);
                assert_eq!(project(transcript, usize::MAX).events, paired, "{label}");
                let counts = pair_counts(&paired);
                totals.complete += counts.complete;
                totals.incomplete += counts.incomplete;
                sessions += 1;
            }
        }
        assert!(sessions >= 8, "fixture sessions reached: {sessions}");
        assert!(totals.complete > 0 && totals.incomplete > 0, "{totals:?}");
    }

    /// Counting kept records as they pass, with the passed-over ones offered
    /// too, answers what counting the kept subset afterwards does, for every
    /// way of keeping either half of a pair.
    #[test]
    fn a_pair_tally_over_passing_records_counts_what_the_kept_subset_holds() {
        let records = project(
            transcript(
                vec![
                    turn(0, 10, event(EventKind::ToolCall, "call-1")),
                    turn(1, 11, event(EventKind::ToolCall, "call-2")),
                    turn(2, 12, event(EventKind::ToolResult, "call-1")),
                    turn(3, 13, event(EventKind::ToolResult, "call-orphan")),
                    turn(4, 14, event(EventKind::ToolResult, "call-2")),
                ],
                Vec::new(),
            ),
            usize::MAX,
        )
        .events;
        for mask in 0..1_u32 << records.len() {
            let kept = |index: usize| mask & (1 << index) != 0;
            let mut tally = PairTally::default();
            for (index, record) in records.iter().enumerate() {
                tally.add(record, kept(index));
            }
            let subset = records
                .iter()
                .enumerate()
                .filter(|(index, _)| kept(*index))
                .map(|(_, record)| record.clone())
                .collect::<Vec<_>>();
            let mut expected = HashSet::new();
            for record in subset.iter().filter(|record| record.pair.is_some()) {
                let own = reference(record);
                let counterpart = record.pair.clone().unwrap();
                expected.insert(match record.event.kind {
                    EventKind::ToolCall => (own, counterpart),
                    EventKind::ToolResult => (counterpart, own),
                });
            }
            assert_eq!(
                tally.finish(),
                PairCounts {
                    complete: expected.len(),
                    incomplete: subset.iter().filter(|r| r.incomplete.is_some()).count(),
                },
                "mask {mask:05b}"
            );
        }
    }

    /// The streamed object is byte for byte the serialized one, whichever
    /// optional members are present.
    #[test]
    fn streamed_events_json_writes_the_serialized_object() {
        let mut events = project(
            transcript(
                vec![
                    turn(0, 10, event(EventKind::ToolCall, "call-1")),
                    turn(1, 12, event(EventKind::ToolResult, "call-1")),
                ],
                vec![SourceBound::FileTail { bytes: 1024 }],
            ),
            1,
        );
        let write = |events: &EventTranscript| {
            let mut out = Vec::new();
            let mut writer = StreamedEventsJson::open(&mut out, &events.session).unwrap();
            for record in &events.events {
                writer.record(&mut out, record).unwrap();
            }
            let mut closing = events.clone();
            closing.events.clear();
            writer.close(&mut out, &closing).unwrap();
            out
        };
        assert!(events.truncated && !events.events.is_empty());
        assert_eq!(write(&events), serde_json::to_vec(&events).unwrap());

        events.events.clear();
        events.truncation = Truncation::default();
        events.truncated = false;
        events.notes = vec!["a note".to_owned()];
        assert_eq!(write(&events), serde_json::to_vec(&events).unwrap());
    }

    fn record_at(offset: u64) -> RecordRef {
        RecordRef {
            domain: "file:fixture.jsonl".to_owned(),
            revision: None,
            span: Some(ByteSpan {
                start: offset,
                end: offset + 1,
            }),
            native_id: None,
            pointer: None,
            part_index: 0,
            content_part_index: None,
        }
    }

    fn located(mut turn: Turn, offset: u64) -> Turn {
        turn.record_ref = Some(record_at(offset));
        turn
    }

    fn declaring(mut event: ToolEvent, artifacts: Value) -> ToolEvent {
        event.artifact_references = artifact_references(&artifacts);
        event
    }

    fn without_call_id(mut event: ToolEvent) -> ToolEvent {
        event.call_id = None;
        event
    }

    fn running(mut event: ToolEvent) -> ToolEvent {
        event.invocations = structured_runtime_invocations(
            &serde_json::json!({ "command": ["git", "status"] }),
            "payload.item.command",
        );
        event
    }

    #[test]
    fn two_pass_pairing_covers_every_pairing_rule_in_both_read_bounds() {
        let combined = ToolEvent {
            subtype: "tool".to_owned(),
            status: Some("completed".to_owned()),
            completed_ts: Some(Utc.timestamp_opt(50, 0).unwrap()),
            ..event(EventKind::ToolCall, "part-1")
        };
        let turns = vec![
            located(
                turn(
                    0,
                    10,
                    declaring(
                        event(EventKind::ToolCall, "repeated"),
                        serde_json::json!([{ "path": "a.txt" }, { "path": "b.txt" }]),
                    ),
                ),
                0,
            ),
            located(turn(1, 20, event(EventKind::ToolCall, "repeated")), 1),
            located(turn(2, 23, event(EventKind::ToolResult, "repeated")), 2),
            located(
                turn(
                    3,
                    25,
                    declaring(
                        event(EventKind::ToolResult, "repeated"),
                        serde_json::json!({ "path": "a.txt" }),
                    ),
                ),
                3,
            ),
            located(turn(4, 26, event(EventKind::ToolResult, "orphan")), 4),
            located(
                turn(
                    5,
                    27,
                    declaring(
                        event(EventKind::ToolCall, "unanswered"),
                        serde_json::json!({ "path": "c.txt" }),
                    ),
                ),
                5,
            ),
            located(
                turn(6, 28, without_call_id(event(EventKind::ToolCall, "-"))),
                6,
            ),
            located(
                turn(7, 29, without_call_id(event(EventKind::ToolResult, "-"))),
                7,
            ),
            located(
                turn(8, 40, running(event(EventKind::ToolCall, "runtime"))),
                8,
            ),
            located(turn(9, 39, event(EventKind::ToolResult, "runtime")), 9),
            located(turn(10, 45, combined), 10),
            located(
                turn(11, 60, running(event(EventKind::ToolCall, "inline"))),
                11,
            ),
            located(turn(12, 61, event(EventKind::ToolResult, "inline")), 11),
        ];

        for (bounded, orphan) in [
            (false, Incomplete::CallNotRecorded),
            (true, Incomplete::CallBeforeReadBound),
        ] {
            let label = format!("bounded={bounded}");
            let mut replay =
                |sink: &mut dyn FnMut(Turn)| turns.iter().cloned().for_each(&mut *sink);
            let records = assert_two_passes_match(&label, &turns, &mut replay, bounded);

            assert_eq!(records.len(), 14, "{label}");
            // Duplicate ids pair with the most recent open call first.
            assert_eq!(records[1].pair.as_ref().unwrap().ordinal, 2, "{label}");
            assert_eq!(records[0].pair.as_ref().unwrap().ordinal, 3, "{label}");
            assert_eq!(records[0].duration_ms, Some(15_000), "{label}");
            assert_eq!(
                records[0]
                    .event
                    .artifact_consumptions
                    .iter()
                    .map(|consumption| (consumption.status, consumption.consumer.clone()))
                    .collect::<Vec<_>>(),
                vec![
                    (
                        ConsumptionStatus::MatchingConsumptionObserved,
                        Some(record_at(3))
                    ),
                    (ConsumptionStatus::NoMatchingConsumptionObservedInRead, None),
                ],
                "{label}"
            );
            assert_eq!(records[4].incomplete, Some(orphan.clone()), "{label}");
            assert_eq!(
                records[5].incomplete,
                Some(Incomplete::NoResultInRead),
                "{label}"
            );
            assert_eq!(
                records[5].event.artifact_consumptions[0].status,
                ConsumptionStatus::NoMatchingConsumptionObservedInRead,
                "{label}"
            );
            assert_eq!(
                records[6].incomplete,
                Some(Incomplete::NoResultInRead),
                "{label}"
            );
            assert_eq!(records[7].incomplete, Some(orphan), "{label}");
            // A result in its own native record witnesses the runtime call;
            // a result recorded before its call yields no duration.
            assert_eq!(
                records[8].event.invocations[0].witnessed_result,
                Some(record_at(9)),
                "{label}"
            );
            assert_eq!(records[8].duration_ms, None, "{label}");
            // A completed combined part pairs with the result it carries.
            assert_eq!(records[11].event.kind, EventKind::ToolResult, "{label}");
            assert_eq!(records[10].pair.as_ref().unwrap().ordinal, 10, "{label}");
            assert_eq!(records[10].duration_ms, Some(5_000), "{label}");
            // A result sharing its call's native record witnesses nothing.
            assert_eq!(records[12].pair.as_ref().unwrap().ordinal, 12, "{label}");
            assert_eq!(
                records[12].event.invocations[0].witnessed_result, None,
                "{label}"
            );
            assert_eq!(
                pair_counts(&records),
                PairCounts {
                    complete: 5,
                    incomplete: 4
                },
                "{label}"
            );
        }
    }

    #[test]
    fn a_replay_that_differs_from_its_observation_is_refused() {
        let observe = |turns: &[Turn]| {
            let mut index = PairIndex::default();
            turns
                .iter()
                .flat_map(turn_records)
                .for_each(|record| index.observe(&record));
            index.pairing(false)
        };
        let answered = [
            turn(0, 10, event(EventKind::ToolCall, "call")),
            turn(1, 11, event(EventKind::ToolResult, "call")),
        ];

        let mut longer = observe(&answered);
        for record in answered.iter().flat_map(turn_records) {
            longer.emit(record).unwrap();
        }
        let appended = turn(2, 12, event(EventKind::ToolCall, "later"));
        assert!(longer
            .emit(turn_records(&appended).next().unwrap())
            .is_err());

        let mut moved = observe(&answered);
        moved
            .emit(turn_records(&answered[0]).next().unwrap())
            .unwrap();
        let elsewhere = turn(2, 11, event(EventKind::ToolResult, "call"));
        assert!(moved
            .emit(turn_records(&elsewhere).next().unwrap())
            .is_err());

        let mut shorter = observe(&answered);
        shorter
            .emit(turn_records(&answered[0]).next().unwrap())
            .unwrap();
        assert!(shorter.finish().is_err());

        let mut renamed = observe(&[turn(0, 10, event(EventKind::ToolCall, "first"))]);
        let other = turn(0, 10, event(EventKind::ToolCall, "second"));
        renamed.emit(turn_records(&other).next().unwrap()).unwrap();
        assert!(renamed.finish().is_err());
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
    fn invocation_parser_does_not_publish_shortened_program_names() {
        let long_program = "p".repeat(MAX_INVOCATION_STRING_CHARS + 1);
        let direct = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"argv":[long_program.clone(), "sub"]}),
            "payload.argv",
        );
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].coverage, InvocationCoverage::Unsupported);
        assert!(direct[0].program.is_none());
        assert!(direct[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("program token"));

        let long_subcommand = "s".repeat(MAX_INVOCATION_STRING_CHARS + 1);
        let direct_subcommand = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"argv":["echo", long_subcommand.clone()]}),
            "payload.argv",
        );
        assert_eq!(
            direct_subcommand[0].coverage,
            InvocationCoverage::Unsupported
        );
        assert!(direct_subcommand[0].program.is_none());
        assert!(direct_subcommand[0]
            .unsupported_reason
            .as_ref()
            .unwrap()
            .text
            .contains("subcommand token"));

        let runtime = structured_runtime_invocations(
            &serde_json::json!({"command":[long_program, "sub"]}),
            "payload.item.command",
        );
        assert_eq!(runtime.len(), 1);
        assert_eq!(runtime[0].origin, InvocationOrigin::StructuredRuntime);
        assert_eq!(runtime[0].coverage, InvocationCoverage::Unsupported);
        assert!(runtime[0].program.is_none());

        let shell = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"cmd": format!("echo {long_subcommand}")}),
            "payload.arguments",
        );
        assert_eq!(shell.len(), 1);
        assert_eq!(shell[0].coverage, InvocationCoverage::Unsupported);
        assert!(shell[0].program.is_none());

        let javascript = invocations_from_tool(
            Some("orchestrator"),
            &serde_json::Value::String(format!(
                "tools.exec_command({{cmd:'echo {long_subcommand}'}});"
            )),
            "payload.arguments",
        );
        assert_eq!(javascript.len(), 1);
        assert_eq!(javascript[0].coverage, InvocationCoverage::Unsupported);
        assert!(javascript[0].program.is_none());

        let long_argument = "a".repeat(MAX_INVOCATION_STRING_CHARS + 1);
        let bounded_argument = invocations_from_tool(
            Some("exec"),
            &serde_json::json!({"argv":["echo", "next", long_argument]}),
            "payload.argv",
        );
        assert_eq!(bounded_argument.len(), 1);
        assert_eq!(
            bounded_argument[0].coverage,
            InvocationCoverage::StaticLiteral
        );
        assert_eq!(bounded_argument[0].program.as_deref(), Some("echo"));
        assert_eq!(bounded_argument[0].subcommand.as_deref(), Some("next"));
        assert_eq!(bounded_argument[0].arguments.len(), 1);
        assert_eq!(
            bounded_argument[0].arguments[0].chars,
            MAX_INVOCATION_STRING_CHARS + 1
        );
        assert!(bounded_argument[0].arguments[0].truncated);
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
