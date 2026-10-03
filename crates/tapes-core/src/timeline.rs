//! Payload-free turn coordinates, emitted as the recording is read.
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::backend::Backend;
use crate::model::{Session, Turn, TurnKind};
use crate::{Selection, SessionSelection};

pub const SCHEMA: &str = "tapes-timeline/1";

#[derive(Debug, Serialize)]
pub struct Record {
    pub ordinal: usize,
    pub kind: TurnKind,
    pub ts: Option<DateTime<Utc>>,
}

impl From<Turn> for Record {
    fn from(turn: Turn) -> Self {
        Self {
            ordinal: turn.ordinal,
            kind: turn.kind,
            ts: turn.ts,
        }
    }
}

pub trait Sink {
    fn session(&mut self, session: &Session) -> Result<()>;
    fn record(&mut self, record: Record) -> Result<()>;
    fn end(&mut self, truncated: bool, notes: &[String]) -> Result<()>;
}

fn read(
    backend: &dyn Backend,
    session: &Session,
    full: bool,
    tail: usize,
    sink: &mut dyn Sink,
) -> Result<()> {
    if full {
        sink.session(session)?;
        let read =
            crate::stream_numbered(backend, session, None, &mut |turn| sink.record(turn.into()))?;
        sink.end(false, &read.notes)
    } else {
        let transcript = crate::declared_transcript(backend, session, tail)?;
        sink.session(&transcript.session)?;
        for turn in transcript.turns {
            sink.record(turn.into())?;
        }
        sink.end(transcript.truncated, &transcript.notes)
    }
}

pub fn single(
    backends: &[Box<dyn Backend>],
    selection: Selection<'_>,
    full: bool,
    sink: &mut dyn Sink,
) -> Result<()> {
    let resolved = selection.resolve(backends)?;
    read(
        backends[resolved.backend_index].as_ref(),
        &resolved.session,
        full,
        crate::EXPORT_TAIL,
        sink,
    )
}

pub fn selection(
    backends: &[Box<dyn Backend>],
    selection: &SessionSelection<'_>,
    full: bool,
    sink: &mut dyn Sink,
) -> Result<()> {
    let scope = selection.within.resolve()?;
    let listed = crate::list_scoped(
        backends,
        selection.harness,
        scope.as_ref(),
        selection.limit.unwrap_or(crate::DEFAULT_LIST_LIMIT),
        &selection.filters,
        selection.sort,
    )?;
    for (session, origin) in listed.sessions.into_iter().zip(listed.origins) {
        read(backends[origin].as_ref(), &session, full, usize::MAX, sink)?;
    }
    Ok(())
}
