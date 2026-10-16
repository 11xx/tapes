//! A frozen working set: one session and the relatives its own lineage resolves
//! (one level: not a parent's parent nor a child's children),
//! each read in full and cut into lanes. A lane is a contiguous range of turn
//! ordinals in one source's normalized read, sized in the bytes of the lane
//! projection. Sources are streamed one at a time; no source's turns are held
//! beyond the turn being measured.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::backend::{Backend, StreamedChild, StreamedTranscript};
use crate::lineage::Lineage;
use crate::model::{
    ByteSpan, ContextEvent, LiveState, Session, SourceDescriptor, SourcePrefixDigest, Turn,
    TurnKind, UnmappedRecords,
};
use crate::projection;
use crate::Selection;

pub const WORKSET_SCHEMA: &str = "tapes-workset/1";
pub const DEFAULT_LANE_BYTES: u64 = 64 * 1024;
pub const MIN_LANE_BYTES: u64 = 8 * 1024;
pub const MAX_LANE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct Workset {
    pub schema: &'static str,
    /// The root session's id.
    pub root: String,
    pub tapes_version: String,
    pub observed_at: DateTime<Utc>,
    pub lane_bytes: u64,
    /// The root first, then its relatives in lineage order: parent, the
    /// session it was forked from, then children by spawn order.
    pub sources: Vec<WorksetSource>,
}

/// How a source relates to the root, as lineage names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceRole {
    Root,
    Parent,
    ForkedFrom,
    Child,
}

#[derive(Debug, Serialize)]
pub struct WorksetSource {
    /// Short and unique within the workset; turns are cited `<key>#<ordinal>`.
    pub key: String,
    pub harness: String,
    pub session_id: String,
    pub role: SourceRole,
    /// The name the root's store gives a child read through the root, where
    /// the child is not addressable by a session id of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_prefix_digest: Option<SourcePrefixDigest>,
    /// Present-tense state from the session-status authority, when it
    /// reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lanes: Option<Vec<Lane>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    /// Why the source could not be read. A source carrying one has no lanes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Lane {
    /// `<key>-<n>`, numbered from zero.
    pub id: String,
    pub first_turn: usize,
    pub last_turn: usize,
    /// The file bytes the lane's turns came from, when every one of them
    /// carries a record span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_range: Option<ByteSpan>,
    pub projected_bytes: u64,
    pub turns_by_kind: BTreeMap<TurnKind, usize>,
    /// Text bytes the projection's per-turn cap left out, by turn kind.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub omitted_bytes: BTreeMap<TurnKind, u64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub turns: usize,
    pub turns_by_kind: BTreeMap<TurnKind, usize>,
    /// Source records decoded, counted where every turn names its record
    /// span and the reader counts the records that became no turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records_decoded: Option<usize>,
    /// Records that could not be decoded: malformed, or over the record bound.
    pub records_skipped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmapped: Option<UnmappedRecords>,
    /// Source bytes no turn's record span covers, where every turn carries
    /// a file span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_without_turn: Option<u64>,
    pub notices: Notices,
    /// Text bytes the projection's per-turn cap left out of every lane.
    pub omitted_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unresolved_references: Vec<UnresolvedReference>,
    /// Lineage enumeration that failed, with its diagnostic; the relatives
    /// listed beside it are then incomplete.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub child_enumeration_failures: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Turns that interrupt the exchange rather than carry it.
#[derive(Debug, Default, Serialize)]
pub struct Notices {
    pub compaction_boundaries: usize,
    pub injected: usize,
}

/// A relative the root's records name that its store does not hold.
#[derive(Debug, Serialize)]
pub struct UnresolvedReference {
    pub role: SourceRole,
    pub harness: String,
    pub reference: String,
}

impl SourceRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Parent => "parent",
            Self::ForkedFrom => "forked-from",
            Self::Child => "child",
        }
    }
}

impl WorksetSource {
    pub fn role_label(&self) -> &'static str {
        self.role.label()
    }
}

/// Cuts a stream of turns into lanes: a turn that would push the open lane
/// past the bound starts a new one, and a turn larger than the bound gets a
/// lane of its own.
pub struct LaneCutter {
    key: String,
    bound: u64,
    lanes: Vec<Lane>,
    open: Option<OpenLane>,
}

struct OpenLane {
    lane: Lane,
    spanned: bool,
}

impl LaneCutter {
    pub fn new(key: &str, bound: u64) -> Self {
        Self {
            key: key.to_owned(),
            bound,
            lanes: Vec::new(),
            open: None,
        }
    }

    pub fn push(&mut self, turn: &Turn) {
        let projection = projection::project(&self.key, turn);
        let bytes = projection.text.len() as u64;
        if self
            .open
            .as_ref()
            .is_some_and(|open| open.lane.projected_bytes + bytes > self.bound)
        {
            self.close();
        }
        let span = turn.record_ref.as_ref().and_then(|record| record.span);
        let open = self.open.get_or_insert_with(|| OpenLane {
            lane: Lane {
                id: format!("{}-{}", self.key, self.lanes.len()),
                first_turn: turn.ordinal,
                last_turn: turn.ordinal,
                byte_range: span,
                projected_bytes: 0,
                turns_by_kind: BTreeMap::new(),
                omitted_bytes: BTreeMap::new(),
            },
            spanned: true,
        });
        open.lane.last_turn = turn.ordinal;
        open.lane.projected_bytes += bytes;
        *open.lane.turns_by_kind.entry(turn.kind).or_default() += 1;
        if projection.omitted_bytes > 0 {
            *open.lane.omitted_bytes.entry(turn.kind).or_default() += projection.omitted_bytes;
        }
        match (span, &mut open.lane.byte_range) {
            (Some(span), Some(range)) => {
                range.start = range.start.min(span.start);
                range.end = range.end.max(span.end);
            }
            (Some(span), None) => open.lane.byte_range = Some(span),
            (None, _) => open.spanned = false,
        }
    }

    pub fn finish(mut self) -> Vec<Lane> {
        self.close();
        self.lanes
    }

    fn close(&mut self) {
        if let Some(open) = self.open.take() {
            let mut lane = open.lane;
            if !open.spanned {
                lane.byte_range = None;
            }
            self.lanes.push(lane);
        }
    }
}

/// Folds one source's turns into lanes and coverage as they stream.
struct SourceFold {
    cutter: LaneCutter,
    coverage: Coverage,
    spanned: bool,
    records: usize,
    last_span: Option<ByteSpan>,
    covered: u64,
    covered_end: u64,
}

impl SourceFold {
    fn new(key: &str, bound: u64) -> Self {
        Self {
            cutter: LaneCutter::new(key, bound),
            coverage: Coverage::default(),
            spanned: true,
            records: 0,
            last_span: None,
            covered: 0,
            covered_end: 0,
        }
    }

    fn turn(&mut self, turn: &Turn) {
        self.cutter.push(turn);
        self.coverage.turns += 1;
        *self.coverage.turns_by_kind.entry(turn.kind).or_default() += 1;
        if matches!(turn.context_event, Some(ContextEvent::Compaction(_))) {
            self.coverage.notices.compaction_boundaries += 1;
        } else if turn.kind == TurnKind::Notice {
            self.coverage.notices.injected += 1;
        }
        match turn.record_ref.as_ref().and_then(|record| record.span) {
            Some(span) => {
                if self.last_span != Some(span) {
                    self.records += 1;
                    self.last_span = Some(span);
                }
                if span.start >= self.covered_end {
                    self.covered += span.end - span.start;
                    self.covered_end = span.end;
                } else if span.end > self.covered_end {
                    self.covered += span.end - self.covered_end;
                    self.covered_end = span.end;
                }
            }
            None => self.spanned = false,
        }
    }

    fn finish(mut self, key: String, role: SourceRole, read: Read) -> WorksetSource {
        let file = read.streamed.source_revision.is_some();
        self.coverage.records_skipped = read.streamed.skipped;
        self.coverage.unmapped = read.streamed.unmapped.clone();
        if self.spanned && file {
            self.coverage.bytes_without_turn =
                Some(read.streamed.source_length.saturating_sub(self.covered));
            if let Some(unmapped) = &read.streamed.unmapped {
                let unmapped = unmapped.declined.values().sum::<usize>()
                    + unmapped.unrecognized.values().sum::<usize>();
                self.coverage.records_decoded = Some(self.records + unmapped);
            }
        }
        self.coverage.notes = read.streamed.notes.clone();
        let lanes = self.cutter.finish();
        self.coverage.omitted_bytes = lanes
            .iter()
            .flat_map(|lane| lane.omitted_bytes.values())
            .sum();
        let evidence = read
            .streamed
            .read_evidence(read.session.source.producer.clone());
        WorksetSource {
            key,
            harness: read.harness,
            session_id: read.session.id.clone(),
            role,
            reference: read.reference,
            source: Some(read.session.source.clone()),
            source_length: Some(evidence.source_length),
            source_revision: evidence.source_revision,
            source_prefix_digest: evidence.source_prefix_digest,
            live: None,
            lanes: Some(lanes),
            coverage: Some(self.coverage),
            diagnostic: None,
        }
    }
}

struct Read {
    harness: String,
    session: Session,
    reference: Option<String>,
    streamed: StreamedTranscript,
}

/// A relative the root's lineage names, before it is read.
struct Relative {
    role: SourceRole,
    harness: String,
    target: Target,
}

enum Target {
    Session(String),
    ChildOfRoot(String),
}

impl Relative {
    fn identity(&self) -> (&str, &str) {
        match &self.target {
            Target::Session(id) | Target::ChildOfRoot(id) => (&self.harness, id),
        }
    }
}

/// Refuse a lane bound outside `MIN_LANE_BYTES..=MAX_LANE_BYTES`.
pub fn check_lane_bytes(lane_bytes: u64) -> Result<()> {
    if !(MIN_LANE_BYTES..=MAX_LANE_BYTES).contains(&lane_bytes) {
        bail!(
            "--lane-bytes must be between {} and {} bytes; got {lane_bytes}",
            MIN_LANE_BYTES,
            MAX_LANE_BYTES
        );
    }
    Ok(())
}

/// Read the selected session and every relative its lineage resolves, each
/// in full, and cut each into lanes of at most `lane_bytes` projected bytes.
/// The root must be readable; a relative that is not is reported with its
/// diagnostic.
pub fn build_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    lane_bytes: u64,
) -> Result<Workset> {
    check_lane_bytes(lane_bytes)?;
    let observed_at = Utc::now();
    let resolved = selection.resolve(backends)?;
    let backend = backends[resolved.backend_index].as_ref();
    let root_key = key(0);
    let mut fold = SourceFold::new(&root_key, lane_bytes);
    let mut streamed = crate::stream_numbered(backend, &resolved.session, None, &mut |turn| {
        fold.turn(&turn);
        Ok(())
    })?;
    streamed
        .notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    let session = resolved.session;
    let harness = backend.harness().to_owned();

    let (relatives, unresolved, failures) = match backend.stream_lineage(&session) {
        Ok(lineage) => relatives(lineage, &harness),
        Err(error) => (Vec::new(), Vec::new(), vec![format!("{error:#}")]),
    };
    fold.coverage.unresolved_references = unresolved;
    fold.coverage.child_enumeration_failures = failures;
    let root = fold.finish(
        root_key,
        SourceRole::Root,
        Read {
            harness: harness.clone(),
            session: session.clone(),
            reference: None,
            streamed,
        },
    );

    let mut sources = vec![root];
    let mut seen = vec![(harness.clone(), session.id.clone())];
    for relative in relatives {
        let (relative_harness, id) = relative.identity();
        let identity = (relative_harness.to_owned(), id.to_owned());
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        let key = key(sources.len());
        sources.push(read_relative(
            backends, backend, &session, relative, key, lane_bytes,
        ));
    }

    Ok(Workset {
        schema: WORKSET_SCHEMA,
        root: session.id,
        tapes_version: env!("CARGO_PKG_VERSION").to_owned(),
        observed_at,
        lane_bytes,
        sources,
    })
}

fn key(index: usize) -> String {
    format!("s{index}")
}

type Relatives = (Vec<Relative>, Vec<UnresolvedReference>, Vec<String>);

fn relatives(mut lineage: Lineage, harness: &str) -> Relatives {
    lineage.sort_children();
    let mut relatives = Vec::new();
    let mut unresolved = Vec::new();
    if let Some(parent) = lineage.parent {
        if parent.resolved {
            relatives.push(Relative {
                role: SourceRole::Parent,
                harness: harness.to_owned(),
                target: Target::Session(parent.native_id),
            });
        } else {
            unresolved.push(UnresolvedReference {
                role: SourceRole::Parent,
                harness: harness.to_owned(),
                reference: parent.native_id,
            });
        }
    }
    if let Some(forked_from) = lineage.forked_from {
        relatives.push(Relative {
            role: SourceRole::ForkedFrom,
            harness: harness.to_owned(),
            target: Target::Session(forked_from),
        });
    }
    for child in lineage.children {
        if !child.resolved {
            unresolved.push(UnresolvedReference {
                role: SourceRole::Child,
                harness: child.harness,
                reference: child.reference,
            });
            continue;
        }
        let target = match child.session_id {
            Some(id) => Target::Session(id),
            None => Target::ChildOfRoot(child.reference),
        };
        relatives.push(Relative {
            role: SourceRole::Child,
            harness: child.harness,
            target,
        });
    }
    (relatives, unresolved, Vec::new())
}

fn read_relative(
    backends: &[Box<dyn Backend>],
    root_backend: &dyn Backend,
    root: &Session,
    relative: Relative,
    key: String,
    lane_bytes: u64,
) -> WorksetSource {
    let mut fold = SourceFold::new(&key, lane_bytes);
    let read = match &relative.target {
        Target::Session(id) => Selection::IdInHarness {
            id,
            harness: &relative.harness,
        }
        .resolve(backends)
        .and_then(|resolved| {
            let backend = backends[resolved.backend_index].as_ref();
            let streamed = crate::stream_numbered(backend, &resolved.session, None, &mut |turn| {
                fold.turn(&turn);
                Ok(())
            })?;
            Ok(Read {
                harness: backend.harness().to_owned(),
                session: resolved.session,
                reference: None,
                streamed,
            })
        }),
        Target::ChildOfRoot(reference) => {
            let mut ordinal = 0;
            root_backend
                .stream_child_transcript(root, reference, &mut |mut turn| {
                    turn.ordinal = ordinal;
                    ordinal += 1;
                    fold.turn(&turn);
                    Ok(())
                })
                .map(|StreamedChild { session, read }| Read {
                    harness: root_backend.harness().to_owned(),
                    session,
                    reference: Some(reference.clone()),
                    streamed: read,
                })
        }
    };
    match read {
        Ok(read) => fold.finish(key, relative.role, read),
        Err(error) => {
            let (harness, id) = relative.identity();
            WorksetSource {
                key,
                harness: harness.to_owned(),
                session_id: match &relative.target {
                    Target::Session(id) => id.clone(),
                    Target::ChildOfRoot(_) => root.id.clone(),
                },
                role: relative.role,
                reference: matches!(relative.target, Target::ChildOfRoot(_)).then(|| id.to_owned()),
                source: None,
                source_length: None,
                source_revision: None,
                source_prefix_digest: None,
                live: None,
                lanes: None,
                coverage: None,
                diagnostic: Some(format!("{error:#}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::pi::PiBackend;

    #[test]
    fn a_relative_that_cannot_be_read_is_reported_with_its_diagnostic() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi");
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures))];
        let root = Selection::Id("session-pi")
            .resolve(&backends)
            .unwrap()
            .session;
        let relative = Relative {
            role: SourceRole::Parent,
            harness: "pi".to_owned(),
            target: Target::Session("absent-parent".to_owned()),
        };
        let source = read_relative(
            &backends,
            backends[0].as_ref(),
            &root,
            relative,
            key(1),
            DEFAULT_LANE_BYTES,
        );
        assert_eq!(source.session_id, "absent-parent");
        assert!(source.diagnostic.is_some(), "{source:?}");
        assert!(source.lanes.is_none() && source.coverage.is_none());
    }
}
