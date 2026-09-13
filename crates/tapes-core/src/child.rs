//! A child's own evidence qualified by the parent that holds its recording.
use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::backend;
use crate::endings::{self, Ending};
use crate::model::{Session, Transcript, Truncation};
use crate::usage::{self, UsageView};
use crate::Selection;

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
    let backends = backend::backends();
    let parent = selection.resolve(&backends)?;
    let mut transcript =
        backends[parent.backend_index].child_transcript(&parent.session, reference)?;
    let usage = usage::usage(&transcript);
    let ending = endings::ending(
        transcript.clone(),
        Err(anyhow!(
            "nested child lineage is not inspected by this read"
        )),
        tail,
        false,
    );
    let total = transcript.turns.len();
    if total > tail {
        transcript.turns.drain(..total - tail);
        transcript.truncation.window = Truncation::window(transcript.turns.len(), total, tail);
        transcript.truncated = true;
    }
    Ok(ChildView {
        schema: "tapes-child/2",
        parent: parent.session,
        reference: reference.to_owned(),
        transcript,
        usage,
        ending,
    })
}
