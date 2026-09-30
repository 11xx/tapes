//! What a continuation of one session needs from its recording alone.
//!
//! Resuming a cold session costs its whole history, and starting fresh loses
//! the state that made the work coherent. Most of that state is written down
//! elsewhere by a session that journaled as it worked; the part only the
//! recording holds is where it stopped, the corrections that landed late, and
//! the handles of work still in flight. This projection renders that part,
//! bounded, from one read of the store.
//!
//! It reads no journal, queries no project tool, and judges nothing: the
//! caller joins what the recording says with whatever the project records
//! about the same work. Every fact here is the read's own — the ending as the
//! endings projection establishes it, the calls the read never saw a result
//! for, the children whose outcome the store does not record, and a bounded
//! tail of the exchange.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::bundle::{git_context, GitContext};
use crate::content::{self, ContentInventory};
use crate::endings::{self, Ending, EndingSource, Fact, Incomplete, TailEntry, TurnMark, TurnRef};
use crate::event::{self, Bounded, EventKind};
use crate::lineage::{ChildRef, Lineage};
use crate::model::{
    Accounting, Cost, LiveState, Model, ModelObservationStatus, ReadEvidence, RecordRef,
    SessionMetadata, SourceDescriptor, TerminalObservation, TextTailEvidence, Tokens,
    TrailingRecord, Transcript, Truncation,
};
use crate::usage;

pub const BRIEF_SCHEMA: &str = "tapes-brief/8";
/// Newest operator and assistant turns rendered when the caller names no
/// window. Wide enough to hold the exchange that ended the session, narrow
/// enough that the brief stays one screen.
pub const DEFAULT_BRIEF_TAIL: usize = 12;
/// Characters kept per tail entry. The tail is the ending in the session's
/// own words, not a substitute for reading it.
pub const TAIL_TEXT_CHARS: usize = 600;
/// Handles listed per kind. A continuation reattaches to a few things; a list
/// longer than this is a signal in itself, and the bound says it was cut.
const IN_FLIGHT_LIMIT: usize = 20;

/// The session a brief is about, without its transcript.
#[derive(Clone, Debug, Serialize)]
pub struct BriefSession {
    pub id: String,
    pub source: SourceDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_observation: Option<ModelObservationStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_selections: Vec<crate::model::ModelSelectionSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveState>,
}

/// Where the session worked, as the recording names it and as the machine
/// answers about it now. `directory_exists` is stated either way, because a
/// working directory that is gone is a fact a continuation has to act on
/// rather than a blank. What is uncommitted there is present-tense state the
/// caller reads for itself.
#[derive(Clone, Debug, Serialize)]
pub struct WorkingSet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitContext>,
    /// Whether the recorded directory is a directory on this machine. False
    /// when the recording names none.
    pub directory_exists: bool,
}

/// Where the recording stopped, as the endings projection establishes it.
#[derive(Clone, Debug, Serialize)]
pub struct BriefEnding {
    /// The newest turn the read reached. Absent when the read reached none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn: Option<TurnRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_operator: Option<TurnMark>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant: Option<TurnMark>,
    pub facts: Vec<Fact>,
    pub incomplete: Vec<Incomplete>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_record: Option<TrailingRecord>,
}

/// A tool call the read never saw a result for, named the way a caller has to
/// name it when deciding whether it landed.
#[derive(Clone, Debug, Serialize)]
pub struct OpenCall {
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Bounded>,
}

/// A child the parent's store records no outcome for, or whose own recording
/// is not in the store. Both are handles: what became of the child is a
/// question the caller takes elsewhere.
#[derive(Clone, Debug, Serialize)]
pub struct OpenChild {
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// The work the recording leaves open. Each entry is a handle to reattach to,
/// and what to do with one is the caller's decision.
#[derive(Clone, Debug, Serialize)]
pub struct InFlight {
    /// Calls the read never saw a result for, newest first.
    pub calls_without_result: Vec<OpenCall>,
    /// Children in the order the store records them as spawned.
    pub children: Vec<OpenChild>,
}

/// What one session's quota spend was, for a caller deciding how much of the
/// continuation to spend. Each counter appears only where the harness
/// recorded it, and `accounting` says what the figures cover.
#[derive(Clone, Debug, Serialize)]
pub struct BriefUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
}

impl BriefUsage {
    fn is_empty(&self) -> bool {
        self.tokens.is_none() && self.cost.is_none() && self.accounting.is_none()
    }
}

/// What a continuation of one session needs from its recording.
#[derive(Clone, Debug, Serialize)]
pub struct Brief {
    pub schema: &'static str,
    pub session: BriefSession,
    /// The coordinate to write down for this reading, as the endings
    /// projection emits it, its own schema name included.
    pub source: EndingSource,
    pub working_set: WorkingSet,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_tail: Option<TextTailEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    pub ending: BriefEnding,
    pub in_flight: InFlight,
    /// The newest operator and assistant turns of the read, oldest first.
    pub tail: Vec<TailEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<BriefUsage>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Truncation::is_empty")]
    pub truncation: Truncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Project one read and the relatives its store records into the brief.
///
/// The read is the whole one the reader can reach, so pairing sees every call
/// and its result; `tail` bounds the rendered exchange alone.
pub fn brief(transcript: Transcript, lineage: Result<Lineage>, tail: usize) -> Brief {
    let read = transcript.read.clone();
    let terminal = transcript.terminal.clone();
    let text_tail_evidence = transcript.text_tail.clone();
    let content = content::inventory(&transcript.turns);
    let events = event::project(transcript.clone(), usize::MAX);
    let mut calls_without_result = events
        .events
        .iter()
        .rev()
        .filter(|record| {
            record.event.kind == EventKind::ToolCall
                && record.incomplete == Some(event::Incomplete::NoResultInRead)
        })
        .map(|record| OpenCall {
            ordinal: record.ordinal,
            record_ref: record.record_ref.clone(),
            name: record.event.name.clone(),
            call_id: record.event.call_id.clone(),
            ts: record.ts,
            arguments: record.event.arguments.clone(),
        })
        .collect::<Vec<_>>();
    let calls_cut = bound(&mut calls_without_result);

    let (mut children, lineage) = match lineage {
        Ok(mut lineage) => {
            lineage.sort_children();
            (open_children(&lineage), Ok(lineage))
        }
        Err(error) => (Vec::new(), Err(error)),
    };
    let children_cut = bound(&mut children);

    let usage = usage::usage(&transcript);
    let usage = BriefUsage {
        tokens: usage.tokens,
        cost: usage.cost,
        accounting: usage.accounting,
    };

    let directory = transcript.session.directory.clone();
    let session = BriefSession {
        id: transcript.session.id.clone(),
        source: transcript.session.source.clone(),
        metadata: transcript.session.metadata.clone(),
        model: transcript.session.model.clone(),
        model_observation: transcript.session.model_observation.clone(),
        model_selections: transcript.session.model_selections.clone(),
        title: transcript.session.title.clone(),
        derived_title: transcript.session.derived_title.clone(),
        started_at: transcript.session.started_at,
        last_activity_at: transcript.session.last_activity_at,
        directory: directory.clone(),
        live: transcript.session.live.clone(),
    };
    let working_set = WorkingSet {
        git: git_context(directory.as_deref()),
        directory_exists: directory.as_deref().is_some_and(std::path::Path::is_dir),
        directory,
    };
    let tail = text_tail(&transcript, tail);

    // The endings projection is asked for no text tail of its own: the tail
    // here is rendered under this projection's wider bound.
    let Ending {
        source,
        last_turn,
        last_operator,
        last_assistant,
        trailing_record,
        facts,
        incomplete,
        truncated,
        truncation,
        mut notes,
        ..
    } = endings::ending(transcript, lineage, 0, false);
    if calls_cut {
        notes.push(format!(
            "The {IN_FLIGHT_LIMIT} newest calls without a result are listed; the read holds more."
        ));
    }
    if children_cut {
        notes.push(format!(
            "The first {IN_FLIGHT_LIMIT} children without a recorded outcome are listed; the store records more."
        ));
    }

    Brief {
        schema: BRIEF_SCHEMA,
        session,
        source,
        working_set,
        read,
        terminal,
        text_tail: text_tail_evidence,
        content,
        ending: BriefEnding {
            last_turn,
            last_operator,
            last_assistant,
            facts,
            incomplete,
            trailing_record,
        },
        in_flight: InFlight {
            calls_without_result,
            children,
        },
        tail,
        usage: Some(usage).filter(|usage| !usage.is_empty()),
        truncated,
        truncation,
        notes,
    }
}

/// Cut a list of handles to the bound, saying whether anything was cut.
fn bound<T>(handles: &mut Vec<T>) -> bool {
    let cut = handles.len() > IN_FLIGHT_LIMIT;
    handles.truncate(IN_FLIGHT_LIMIT);
    cut
}

/// The children a continuation still has a question about: one the store
/// records no outcome for — neither a completion stamp nor a disposition —
/// and one whose own recording the store cannot resolve, whatever it records
/// about it. Which of a harness's own outcome words end a child is the
/// harness's vocabulary and not this reader's, so a recorded outcome of any
/// spelling settles the child.
fn open_children(lineage: &Lineage) -> Vec<OpenChild> {
    lineage
        .children
        .iter()
        .filter(|child| {
            !child.resolved || (child.completed_at.is_none() && child.disposition.is_none())
        })
        .map(|child: &ChildRef| OpenChild {
            reference: child.reference.clone(),
            role: child.role.clone(),
            disposition: child.disposition.clone(),
            resolved: child.resolved,
            spawned_at: child.spawned_at,
            completed_at: child.completed_at,
        })
        .collect()
}

/// The newest operator and assistant turns of the read, each cut to the entry
/// bound. The harness's own commands, notices, and attached context stay out
/// of it, as do reasoning and tool turns: what a continuation is picking up
/// is the exchange.
fn text_tail(transcript: &Transcript, tail: usize) -> Vec<TailEntry> {
    let entries = transcript
        .turns
        .iter()
        .filter(|turn| turn.kind.in_exchange())
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
                metadata: turn.metadata.clone(),
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
