//! A child's own evidence qualified by the parent that holds its recording.
use std::collections::VecDeque;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::backend::{self, StreamedChild};
use crate::endings::{self, Ending};
use crate::model::{Session, Transcript, TranscriptEvidence, Truncation};
use crate::usage::{self, TurnTally, UsageView};
use crate::Selection;

pub const CHILD_SCHEMA: &str = "tapes-child/3";

#[derive(Debug, Serialize)]
pub struct ChildView {
    pub schema: &'static str,
    pub parent: Session,
    pub reference: String,
    pub transcript: Transcript,
    pub usage: UsageView,
    pub ending: Ending,
}

pub fn read(selection: Selection<'_>, reference: &str, tail: usize) -> Result<ChildView> {
    read_with_backends(&backend::backends(), selection, reference, tail)
}

pub fn read_with_backends(
    backends: &[Box<dyn backend::Backend>],
    selection: Selection<'_>,
    reference: &str,
    tail: usize,
) -> Result<ChildView> {
    let parent = selection.resolve(backends)?;
    let mut transcript =
        backends[parent.backend_index].child_transcript(&parent.session, reference)?;
    let selection_notes = parent.diagnostics.latest_warnings(&parent.session);
    transcript.notes.extend(selection_notes.clone());
    let mut usage = usage::usage(&transcript);
    usage.notes.extend(selection_notes.clone());
    let mut ending = endings::ending(
        transcript.clone(),
        Err(anyhow!(
            "nested child lineage is not inspected by this read"
        )),
        tail,
        false,
    );
    ending.notes.extend(selection_notes);
    let total = transcript.turns.len();
    if total > tail {
        transcript.turns.drain(..total - tail);
        transcript.truncation.window = Truncation::window(transcript.turns.len(), total, tail);
        transcript.truncated = true;
    }
    Ok(ChildView {
        schema: CHILD_SCHEMA,
        parent: parent.session,
        reference: reference.to_owned(),
        transcript,
        usage,
        ending,
    })
}

/// A child's whole recording, streamed. Usage folds every turn; the
/// transcript and the ending keep only the newest `tail` turns, because an
/// ending is established from a recording's newest turns by design.
pub fn read_full_with_backends(
    backends: &[Box<dyn backend::Backend>],
    selection: Selection<'_>,
    reference: &str,
    tail: usize,
) -> Result<ChildView> {
    let parent = selection.resolve(backends)?;
    let mut tally = TurnTally::default();
    let mut window = VecDeque::new();
    let mut total = 0;
    let StreamedChild { session, read } = backends[parent.backend_index].stream_child_transcript(
        &parent.session,
        reference,
        &mut |mut turn| {
            turn.ordinal = total;
            total += 1;
            tally.add(&turn);
            if tail > 0 {
                if window.len() == tail {
                    window.pop_front();
                }
                window.push_back(turn);
            }
            Ok(())
        },
    )?;
    let selection_notes = parent.diagnostics.latest_warnings(&parent.session);
    let mut usage = usage::streamed(&session, tally, &read);
    usage.notes.extend(selection_notes.clone());
    let turns = Vec::from(window);
    let truncation = Truncation {
        window: Truncation::window(turns.len(), total, tail),
        source: read.source_bounds.clone(),
    };
    let evidence = TranscriptEvidence {
        read: Some(read.read_evidence(session.source.producer.clone())),
        terminal: read.terminal.clone(),
        text_tail: None,
    };
    let mut transcript = Transcript::with_evidence(
        session,
        turns,
        truncation,
        evidence,
        read.trailing_record.clone(),
        usage.notes.clone(),
    );
    transcript.notes.extend(selection_notes.clone());
    let mut ending = endings::ending(
        transcript.clone(),
        Err(anyhow!(
            "nested child lineage is not inspected by this read"
        )),
        tail,
        false,
    );
    ending.notes.extend(selection_notes);
    Ok(ChildView {
        schema: CHILD_SCHEMA,
        parent: parent.session,
        reference: reference.to_owned(),
        transcript,
        usage,
        ending,
    })
}
