//! What a selection of sessions ends on, one bounded record each.
//!
//! Choosing which of many dead sessions deserve reading is a survey, and a
//! survey that reads transcripts to make it costs more than the reading it
//! saves. This projection spends one bounded read per session on structure
//! alone: which kinds of turn the recording ends on, whether a tool call in
//! that read ever reached a result, what the read could not establish, and
//! the coordinate to write down for whichever endings turn out to matter.
//!
//! Every fact rests on the normalized kinds and typed tool events of the turns
//! that were read, never on their text. The report classifies nothing and
//! infers no reason for an ending; naming what an ending means is the reader's
//! work, and the facts here are what it has to work from.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::backend::Backend;
use crate::content::{self, ContentCoverage, ContentInventory, ContentPart};
use crate::event::{self, EventKind, EventRecord};
use crate::lineage::{Lineage, ParentRef};
use crate::model::{
    LiveState, Model, ReadEvidence, RecordRef, Role, SessionMetadata, SourceBound,
    SourceDescriptor, TerminalObservation, TextTailEvidence, TrailingRecord, Transcript,
    Truncation, Turn, TurnKind,
};
use crate::{list_scoped, selection_record, SelectionRecord, SessionSelection, DEFAULT_LIST_LIMIT};

pub const ENDINGS_SCHEMA: &str = "tapes-endings/3";
/// Newest turns read per session when the caller names no window. Wide enough
/// to hold a tool call and the exchange around it, narrow enough that a
/// selection of hundreds stays a survey.
pub const DEFAULT_ENDINGS_TAIL: usize = 12;
/// Characters kept per text-tail entry. The tail is an identifying glimpse of
/// an ending, not a substitute for reading the session.
const TAIL_TEXT_CHARS: usize = 400;

/// One structural fact about an ending, established from the kinds and typed
/// tool events of the turns that were read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fact {
    /// The newest operator turn is later than the newest assistant turn, or
    /// the read holds an operator turn and no assistant turn at all: a
    /// request the recording never answers.
    OperatorTurnAfterAssistant,
    /// A tool call in the read reached no result, and nothing in the read
    /// results after it.
    CallWithoutResult,
    /// The newest turn is a paired tool result, so results landed and no
    /// assistant turn narrates them.
    ResultsWithoutNarration,
    /// The newest turn that is neither control nor notice is an assistant
    /// turn.
    AssistantClose,
    /// The last turn is a harness command or control message. What the
    /// recording ends on is decided by the turns before it.
    ControlTurnLast,
    /// The last turn is a message the harness injected. What the recording
    /// ends on is decided by the turns before it.
    NoticeTurnLast,
}

impl Fact {
    /// The name a human render uses for the fact.
    pub fn label(self) -> &'static str {
        match self {
            Self::OperatorTurnAfterAssistant => "operator-turn-after-assistant",
            Self::CallWithoutResult => "call-without-result",
            Self::ResultsWithoutNarration => "results-without-narration",
            Self::AssistantClose => "assistant-close",
            Self::ControlTurnLast => "control-turn-last",
            Self::NoticeTurnLast => "notice-turn-last",
        }
    }
}

/// What the read left unestablished, which qualifies every fact beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Incomplete {
    /// A source bound withheld turns, so the recording continues beyond this read.
    ReadWindow,
    /// The turn window omitted turns the read had produced. A wider `--tail`
    /// recovers them.
    TailWindow,
    /// A user turn in the read carries no evidence of what it holds, so what
    /// the recording ends on may be a request or may be the harness's own.
    KindUnknown,
    /// A turn the ordering depended on carries no timestamp, so the order
    /// rests on the normalized sequence alone.
    NoTimestamps,
}

impl Incomplete {
    /// The name a human render uses for the limit.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadWindow => "read-window",
            Self::TailWindow => "tail-window",
            Self::KindUnknown => "kind-unknown",
            Self::NoTimestamps => "no-timestamps",
        }
    }
}

/// How much of a session the read behind an ending covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Coverage {
    /// Every turn of the recording.
    Session,
    /// A source bound the reader itself reached.
    ReadWindow,
    /// The requested turn window, which a wider one widens.
    Window,
}

/// The session identity an ending is about, without its transcript.
#[derive(Clone, Debug, Serialize)]
pub struct EndingSession {
    pub id: String,
    pub source: SourceDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveState>,
}

/// The reference a caller writes down elsewhere for an ending: where it was
/// read, which turn it ends on, and how much of the session the read covered.
/// It carries no transcript text.
#[derive(Clone, Debug, Serialize)]
pub struct EndingSource {
    pub source: SourceDescriptor,
    pub session: String,
    /// The last read turn's timestamp, or the session's newest recorded
    /// activity when that turn carries none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    /// The last read turn's ordinal, the same coordinate `show` prints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    pub schema: &'static str,
    pub coverage: Coverage,
}

/// A turn named by what it is and where it sits.
#[derive(Clone, Debug, Serialize)]
pub struct TurnRef {
    pub role: Role,
    pub kind: TurnKind,
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
}

/// Where a turn sits, for a turn whose role and kind the field name already
/// states.
#[derive(Clone, Debug, Serialize)]
pub struct TurnMark {
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
}

/// The relatives a session's store records, counted. Children are referred to
/// and never read: their own endings are read under their own ids.
#[derive(Clone, Debug, Serialize)]
pub struct LineageSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentRef>,
    pub children: usize,
    /// Children whose own recording is not in the store.
    pub children_unresolved: usize,
    /// How many children carry each recorded outcome, in the harness's own
    /// vocabulary. A child whose outcome the harness did not record is
    /// counted under none of them.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub children_by_disposition: BTreeMap<String, usize>,
}

impl LineageSummary {
    fn is_empty(&self) -> bool {
        self.parent.is_none() && self.children == 0
    }
}

/// One bounded glimpse of an operator or assistant turn.
#[derive(Clone, Debug, Serialize)]
pub struct TailEntry {
    pub ordinal: usize,
    pub kind: TurnKind,
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<ContentCoverage>,
    pub text: String,
    /// Whether the entry's text was cut at the entry bound.
    pub truncated: bool,
}

/// What one session ends on, as its bounded read establishes it.
#[derive(Clone, Debug, Serialize)]
pub struct Ending {
    pub session: EndingSession,
    pub source: EndingSource,
    /// The newest turn the read reached. Absent when the read reached none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<TurnRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_operator: Option<TurnMark>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant: Option<TurnMark>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_tail_evidence: Option<TextTailEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_record: Option<TrailingRecord>,
    pub facts: Vec<Fact>,
    pub incomplete: Vec<Incomplete>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lineage: Option<LineageSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tail: Option<Vec<TailEntry>>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Truncation::is_empty")]
    pub truncation: Truncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A selected session whose bounded read failed. The report stands; only this
/// session's ending is missing from it.
#[derive(Clone, Debug, Serialize)]
pub struct UnreadSession {
    pub id: String,
    pub harness: String,
    pub error: String,
}

/// What a selection of sessions ends on. `selection` restates the query that
/// chose the set and the listing's own diagnostics are carried verbatim, so
/// the report can be audited against the store it came from.
#[derive(Debug, Serialize)]
pub struct EndingsReport {
    pub schema: &'static str,
    pub selection: SelectionRecord,
    /// One record per selected session, in selection order.
    pub endings: Vec<Ending>,
    pub unread: Vec<UnreadSession>,
    pub unavailable: Vec<String>,
    pub unreadable: Vec<String>,
    pub unsearched: Vec<String>,
    pub scanned: usize,
    pub scan_truncated: bool,
}

/// What every session a listing with the same filters would return ends on.
pub fn endings(selection: &SessionSelection<'_>, tail: usize, text: bool) -> Result<EndingsReport> {
    endings_with_backends(&crate::backend::backends(), selection, tail, text)
}

/// The scope and listing filters choose the set before any transcript is
/// opened, so a report never pays for a session the caller excluded. Each
/// selected session is then read through the backend that listed it: an id
/// re-resolved against every store could reach a different session, and the
/// report would describe endings that were never read.
///
/// A session costs one bounded transcript read of `tail` turns and one
/// lineage read, and a session whose read fails is recorded in `unread`
/// rather than stopping the run.
pub fn endings_with_backends(
    backends: &[Box<dyn Backend>],
    selection: &SessionSelection<'_>,
    tail: usize,
    text: bool,
) -> Result<EndingsReport> {
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

    let mut endings = Vec::new();
    let mut unread = Vec::new();
    for (session, origin) in listed.sessions.into_iter().zip(listed.origins) {
        match backends[origin].transcript(&session, tail) {
            Ok(transcript) => {
                let lineage = backends[origin].lineage(&session);
                endings.push(ending(transcript, lineage, tail, text));
            }
            Err(error) => unread.push(UnreadSession {
                id: session.id.clone(),
                harness: session.harness().to_owned(),
                error: format!("{error:#}"),
            }),
        }
    }

    Ok(EndingsReport {
        schema: ENDINGS_SCHEMA,
        selection: selection_record(selection, limit),
        endings,
        unread,
        unavailable: listed.unavailable,
        unreadable: listed.unreadable,
        unsearched: listed.unsearched,
        scanned: listed.scanned,
        scan_truncated: listed.scan_truncated,
    })
}

/// Project one bounded read into the ending it establishes.
pub fn ending(transcript: Transcript, lineage: Result<Lineage>, tail: usize, text: bool) -> Ending {
    let events = event::project(transcript.clone(), usize::MAX).events;
    let turns = &transcript.turns;
    let mut by_ordinal = false;
    let facts = facts(turns, &events, &mut by_ordinal);

    let withheld_turns = withheld_turns(&transcript.truncation);
    let windowed = transcript
        .truncation
        .window
        .as_ref()
        .is_some_and(|window| window.omitted > 0 || !window.omitted_exact);
    let mut incomplete = Vec::new();
    if withheld_turns {
        incomplete.push(Incomplete::ReadWindow);
    }
    if windowed {
        incomplete.push(Incomplete::TailWindow);
    }
    if turns.iter().any(|turn| turn.kind == TurnKind::Unknown) {
        incomplete.push(Incomplete::KindUnknown);
    }
    if by_ordinal {
        incomplete.push(Incomplete::NoTimestamps);
    }

    let last_turn = turns.last();
    let session = &transcript.session;
    let read = transcript.read.clone();
    let terminal = transcript.terminal.clone();
    let text_tail_evidence = transcript.text_tail.clone();
    let content = content::inventory(&transcript.turns);
    let mut notes = transcript.notes.clone();
    let lineage = match lineage {
        Ok(lineage) => Some(summarize_lineage(lineage)).filter(|summary| !summary.is_empty()),
        Err(error) => {
            notes.push(format!("Recorded relatives could not be read: {error:#}"));
            None
        }
    };

    Ending {
        session: EndingSession {
            id: session.id.clone(),
            source: session.source.clone(),
            metadata: session.metadata.clone(),
            model: session.model.clone(),
            title: session.title.clone(),
            derived_title: session.derived_title.clone(),
            directory: session.directory.clone(),
            last_activity_at: session.last_activity_at,
            live: session.live.clone(),
        },
        source: EndingSource {
            source: session.source.clone(),
            session: session.id.clone(),
            ts: last_turn
                .and_then(|turn| turn.ts)
                .or(session.last_activity_at),
            turn: last_turn.map(|turn| turn.ordinal),
            native_id: last_turn.and_then(|turn| turn.native_id.clone()),
            record_ref: last_turn.and_then(|turn| turn.record_ref.clone()),
            schema: ENDINGS_SCHEMA,
            coverage: match (withheld_turns, windowed) {
                (true, _) => Coverage::ReadWindow,
                (false, true) => Coverage::Window,
                (false, false) => Coverage::Session,
            },
        },
        last_turn: last_turn.map(|turn| TurnRef {
            role: turn.role.clone(),
            kind: turn.kind,
            ordinal: turn.ordinal,
            ts: turn.ts,
            record_ref: turn.record_ref.clone(),
        }),
        last_operator: mark(newest(turns, TurnKind::Operator)),
        last_assistant: mark(newest(turns, TurnKind::Assistant)),
        read,
        terminal,
        text_tail_evidence,
        content,
        trailing_record: transcript.trailing_record.clone(),
        facts,
        incomplete,
        lineage,
        tail: text.then(|| text_tail(turns, tail)),
        truncated: transcript.truncated,
        truncation: transcript.truncation.clone(),
        notes,
    }
}

/// The facts the read establishes, in a fixed order so one store reports the
/// same ending the same way twice.
fn facts(turns: &[Turn], events: &[EventRecord], by_ordinal: &mut bool) -> Vec<Fact> {
    let mut facts = Vec::new();
    let last_operator = newest(turns, TurnKind::Operator);
    let last_assistant = newest(turns, TurnKind::Assistant);
    if let Some(operator) = last_operator {
        if last_assistant.is_none_or(|assistant| follows(operator, assistant, by_ordinal)) {
            facts.push(Fact::OperatorTurnAfterAssistant);
        }
    }
    match turns.last().map(|turn| turn.kind) {
        Some(TurnKind::Control) => facts.push(Fact::ControlTurnLast),
        Some(TurnKind::Notice) => facts.push(Fact::NoticeTurnLast),
        _ => {}
    }
    if call_without_result(events) {
        facts.push(Fact::CallWithoutResult);
    }
    // The newest turn is the result, so nothing narrates it: an assistant turn
    // after it would be the newest turn instead.
    if let Some(last) = turns.last().filter(|turn| turn.kind == TurnKind::Tool) {
        if events.iter().any(|record| {
            record.ordinal == last.ordinal
                && record.event.kind == EventKind::ToolResult
                && record.pair.is_some()
        }) {
            facts.push(Fact::ResultsWithoutNarration);
        }
    }
    let closing = turns
        .iter()
        .rev()
        .find(|turn| !matches!(turn.kind, TurnKind::Control | TurnKind::Notice));
    if closing.is_some_and(|turn| turn.kind == TurnKind::Assistant) {
        facts.push(Fact::AssistantClose);
    }
    facts
}

/// A call the read never saw a result for, with no result after it. A result
/// later in the read means the recording went on working, whatever an older
/// call is missing.
fn call_without_result(events: &[EventRecord]) -> bool {
    let newest_result = events
        .iter()
        .filter(|record| record.event.kind == EventKind::ToolResult)
        .map(|record| record.ordinal)
        .max();
    events.iter().any(|record| {
        record.event.kind == EventKind::ToolCall
            && record.incomplete == Some(event::Incomplete::NoResultInRead)
            && newest_result.is_none_or(|result| record.ordinal > result)
    })
}

fn newest(turns: &[Turn], kind: TurnKind) -> Option<&Turn> {
    turns.iter().rev().find(|turn| turn.kind == kind)
}

fn mark(turn: Option<&Turn>) -> Option<TurnMark> {
    turn.map(|turn| TurnMark {
        ordinal: turn.ordinal,
        ts: turn.ts,
        record_ref: turn.record_ref.clone(),
    })
}

/// Whether `later` comes after `earlier` in the recording. Timestamps settle
/// it where both turns carry one; otherwise the normalized sequence does, and
/// the caller reports that the ordering rests on it.
fn follows(later: &Turn, earlier: &Turn, by_ordinal: &mut bool) -> bool {
    match (later.ts, earlier.ts) {
        (Some(later_ts), Some(earlier_ts)) => later_ts > earlier_ts,
        _ => {
            *by_ordinal = true;
            later.ordinal > earlier.ordinal
        }
    }
}

/// Only a bound that withheld whole turns leaves the ending itself partial.
/// Text cut inside a turn the read did reach leaves its kind intact.
fn withheld_turns(truncation: &Truncation) -> bool {
    truncation.source.iter().any(|bound| {
        matches!(
            bound,
            SourceBound::FileTail { .. } | SourceBound::RecordPage { .. }
        )
    })
}

fn summarize_lineage(lineage: Lineage) -> LineageSummary {
    let mut children_by_disposition = BTreeMap::new();
    for disposition in lineage
        .children
        .iter()
        .filter_map(|child| child.disposition.clone())
    {
        *children_by_disposition.entry(disposition).or_default() += 1;
    }
    LineageSummary {
        parent: lineage.parent,
        children: lineage.children.len(),
        children_unresolved: lineage
            .children
            .iter()
            .filter(|child| !child.resolved)
            .count(),
        children_by_disposition,
    }
}

/// The newest operator and assistant turns of the read, each cut to the entry
/// bound. The harness's own commands, notices, and attached context stay out
/// of it: what a reader is judging is the exchange.
fn text_tail(turns: &[Turn], tail: usize) -> Vec<TailEntry> {
    let entries = turns
        .iter()
        .filter(|turn| matches!(turn.kind, TurnKind::Operator | TurnKind::Assistant))
        .map(|turn| {
            let mut characters = turn.text.chars();
            let text = characters
                .by_ref()
                .take(TAIL_TEXT_CHARS)
                .collect::<String>();
            TailEntry {
                ordinal: turn.ordinal,
                kind: turn.kind,
                role: turn.role.clone(),
                ts: turn.ts,
                record_ref: turn.record_ref.clone(),
                parts: turn.parts.clone(),
                coverage: turn.coverage.clone(),
                truncated: characters.next().is_some(),
                text,
            }
        })
        .collect::<Vec<_>>();
    let first = entries.len().saturating_sub(tail);
    entries[first..].to_vec()
}
