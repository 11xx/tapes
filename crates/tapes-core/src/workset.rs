//! A frozen working set: one session and the relatives its own lineage resolves
//! (one level: not a parent's parent nor a child's children),
//! each read in full and cut into lanes. A lane is a contiguous range of turn
//! ordinals in one source's normalized read, sized in the bytes of the lane
//! projection. Sources are streamed one at a time; no source's turns are held
//! beyond the turn being measured.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backend::{Backend, StreamedChild, StreamedTranscript};
use crate::lane_file::{self, LaneSource, MarginPair};
use crate::lineage::Lineage;
use crate::model::{
    ByteSpan, ContextEvent, DigestAlgorithm, LiveState, Session, SourceDescriptor,
    SourcePrefixDigest, Turn, TurnKind, UnmappedRecords,
};
use crate::projection;
use crate::Selection;

pub const WORKSET_SCHEMA: &str = "tapes-workset/1";
pub const DEFAULT_LANE_BYTES: u64 = 64 * 1024;
pub const MIN_LANE_BYTES: u64 = 8 * 1024;
pub const MAX_LANE_BYTES: u64 = 4 * 1024 * 1024;
/// The bundle directory's lane-file subdirectory.
pub const LANES_DIR: &str = "lanes";
/// The bundle directory's manifest.
pub const MANIFEST_FILE: &str = "workset.json";
/// Operator turns, each with its first reply, a lane's margin holds.
pub const MARGIN_PAIRS: usize = 2;

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
    /// The digest of the source's normalized content, which pins a source
    /// that has no file prefix to pin and checks one that has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<ContentDigest>,
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
    /// The lane file, relative to the bundle directory, when one was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
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

/// SHA-256 over [`projection::project_whole`] of each of the first `turns`
/// turns, in ordinal order: the source's normalized content, whatever store
/// it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDigest {
    pub algorithm: DigestAlgorithm,
    pub value: String,
    pub turns: usize,
}

/// Folds turns, in ordinal order, into a [`ContentDigest`].
pub struct ContentDigester {
    key: String,
    hasher: Sha256,
    turns: usize,
}

impl ContentDigester {
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_owned(),
            hasher: Sha256::new(),
            turns: 0,
        }
    }

    pub fn turn(&mut self, turn: &Turn) {
        self.hasher
            .update(projection::project_whole(&self.key, turn).as_bytes());
        self.turns += 1;
    }

    pub fn finish(self) -> ContentDigest {
        ContentDigest {
            algorithm: DigestAlgorithm::Sha256,
            value: self
                .hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            turns: self.turns,
        }
    }
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

/// Cuts a stream of turns into lanes, a source record at a time: a record
/// whose turns would push the open lane past the bound starts a new one, so
/// a lane is never cut inside a record and file-byte ranges tile. A record,
/// or a turn, larger than the bound gets a lane of its own. With lane files,
/// each lane is written when it closes: only the open lane's body, the
/// pending record's turns, and a ring of the latest operator turns with
/// their first replies are held.
pub struct LaneCutter {
    key: String,
    bound: u64,
    lanes: Vec<Lane>,
    open: Option<OpenLane>,
    files: Option<LaneFiles>,
    /// The turns of the record being read, placed once the record ends so
    /// a lane is never cut inside it. Each holds a capped projection.
    record: Vec<Pending>,
}

struct Pending {
    ordinal: usize,
    kind: TurnKind,
    span: Option<ByteSpan>,
    text: String,
    omitted_bytes: u64,
}

struct LaneFiles {
    directory: PathBuf,
    source: LaneSource,
    body: String,
    margin: Vec<MarginPair>,
    ring: VecDeque<MarginPair>,
    written: Vec<PathBuf>,
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
            files: None,
            record: Vec::new(),
        }
    }

    /// A cutter that writes each lane to `<directory>/<lane-id>.md`.
    pub fn with_files(source: LaneSource, bound: u64, directory: &Path) -> Self {
        Self {
            files: Some(LaneFiles {
                directory: directory.to_owned(),
                source: source.clone(),
                body: String::new(),
                margin: Vec::new(),
                ring: VecDeque::with_capacity(MARGIN_PAIRS + 1),
                written: Vec::new(),
            }),
            ..Self::new(&source.key, bound)
        }
    }

    pub fn push(&mut self, turn: &Turn) -> Result<()> {
        let projection = projection::project(&self.key, turn);
        let span = turn.record_ref.as_ref().and_then(|record| record.span);
        if span.is_none() || span != self.record.first().and_then(|pending| pending.span) {
            self.flush()?;
        }
        self.record.push(Pending {
            ordinal: turn.ordinal,
            kind: turn.kind,
            span,
            text: projection.text,
            omitted_bytes: projection.omitted_bytes,
        });
        Ok(())
    }

    /// Place the pending record's turns: in the open lane when they fit
    /// beside it, otherwise at the start of a new one.
    fn flush(&mut self) -> Result<()> {
        let record = std::mem::take(&mut self.record);
        let bytes: u64 = record.iter().map(|pending| pending.text.len() as u64).sum();
        if self
            .open
            .as_ref()
            .is_some_and(|open| open.lane.projected_bytes + bytes > self.bound)
        {
            self.close()?;
        }
        for pending in record {
            self.place(pending);
        }
        Ok(())
    }

    fn place(&mut self, turn: Pending) {
        let bytes = turn.text.len() as u64;
        if let Some(files) = &mut self.files {
            if self.open.is_none() {
                files.margin = files.ring.iter().cloned().collect();
            }
            files.body.push_str(&turn.text);
            match turn.kind {
                TurnKind::Operator => {
                    files.ring.push_back(MarginPair {
                        operator: turn.text.clone(),
                        reply: None,
                    });
                    if files.ring.len() > MARGIN_PAIRS {
                        files.ring.pop_front();
                    }
                }
                TurnKind::Assistant => {
                    for pair in files.ring.iter_mut().filter(|pair| pair.reply.is_none()) {
                        pair.reply = Some(turn.text.clone());
                    }
                }
                _ => {}
            }
        }
        let open = self.open.get_or_insert_with(|| OpenLane {
            lane: Lane {
                id: format!("{}-{}", self.key, self.lanes.len()),
                first_turn: turn.ordinal,
                last_turn: turn.ordinal,
                byte_range: turn.span,
                projected_bytes: 0,
                file: None,
                turns_by_kind: BTreeMap::new(),
                omitted_bytes: BTreeMap::new(),
            },
            spanned: true,
        });
        open.lane.last_turn = turn.ordinal;
        open.lane.projected_bytes += bytes;
        *open.lane.turns_by_kind.entry(turn.kind).or_default() += 1;
        if turn.omitted_bytes > 0 {
            *open.lane.omitted_bytes.entry(turn.kind).or_default() += turn.omitted_bytes;
        }
        match (turn.span, &mut open.lane.byte_range) {
            (Some(span), Some(range)) => {
                range.start = range.start.min(span.start);
                range.end = range.end.max(span.end);
            }
            (Some(span), None) => open.lane.byte_range = Some(span),
            (None, _) => open.spanned = false,
        }
    }

    pub fn finish(mut self) -> Result<Vec<Lane>> {
        self.flush()?;
        self.close()?;
        Ok(self.lanes)
    }

    /// Remove every lane file this cutter wrote, for a source whose read
    /// failed after some of its lanes closed.
    pub fn discard(self) {
        for path in self.files.into_iter().flat_map(|files| files.written) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn close(&mut self) -> Result<()> {
        if let Some(open) = self.open.take() {
            let mut lane = open.lane;
            if !open.spanned {
                lane.byte_range = None;
            }
            if let Some(files) = &mut self.files {
                let name = format!("{}.md", lane.id);
                let path = files.directory.join(&name);
                let text = lane_file::render(
                    &files.source,
                    &lane.id,
                    lane.first_turn,
                    lane.last_turn,
                    &files.margin,
                    &files.body,
                );
                std::fs::write(&path, text)
                    .map_err(|error| anyhow!("write {}: {error}", path.display()))?;
                files.written.push(path);
                files.body.clear();
                lane.file = Some(format!("{LANES_DIR}/{name}"));
            }
            self.lanes.push(lane);
        }
        Ok(())
    }
}

/// Folds one source's turns into lanes and coverage as they stream.
struct SourceFold {
    cutter: LaneCutter,
    content: ContentDigester,
    coverage: Coverage,
    spanned: bool,
    records: usize,
    last_span: Option<ByteSpan>,
    covered: u64,
    covered_end: u64,
}

impl SourceFold {
    fn new(key: &str, bound: u64, files: Option<(&Path, LaneSource)>) -> Self {
        Self {
            content: ContentDigester::new(key),
            cutter: match files {
                Some((directory, source)) => LaneCutter::with_files(source, bound, directory),
                None => LaneCutter::new(key, bound),
            },
            coverage: Coverage::default(),
            spanned: true,
            records: 0,
            last_span: None,
            covered: 0,
            covered_end: 0,
        }
    }

    fn turn(&mut self, turn: &Turn) -> Result<()> {
        self.cutter.push(turn)?;
        self.content.turn(turn);
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
        Ok(())
    }

    fn finish(mut self, key: String, role: SourceRole, read: Read) -> Result<WorksetSource> {
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
        let lanes = self.cutter.finish()?;
        self.coverage.omitted_bytes = lanes
            .iter()
            .flat_map(|lane| lane.omitted_bytes.values())
            .sum();
        let evidence = read
            .streamed
            .read_evidence(read.session.source.producer.clone());
        Ok(WorksetSource {
            key,
            harness: read.harness,
            session_id: read.session.id.clone(),
            role,
            reference: read.reference,
            source: Some(read.session.source.clone()),
            source_length: Some(evidence.source_length),
            source_revision: evidence.source_revision,
            source_prefix_digest: evidence.source_prefix_digest,
            content_digest: Some(self.content.finish()),
            live: None,
            lanes: Some(lanes),
            coverage: Some(self.coverage),
            diagnostic: None,
        })
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
/// With a bundle directory, each lane is also written to
/// `<bundle>/lanes/<lane-id>.md`; a bundle that already holds a manifest or a
/// lanes directory is refused. The root must be readable; a relative that is
/// not is reported with its diagnostic and has no lane files.
pub fn build_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    lane_bytes: u64,
    bundle: Option<&Path>,
) -> Result<Workset> {
    check_lane_bytes(lane_bytes)?;
    let lanes = bundle.map(prepare_bundle).transpose()?;
    let built = build(backends, selection, lane_bytes, lanes.as_deref());
    if let (Err(_), Some(lanes)) = (&built, &lanes) {
        let _ = std::fs::remove_dir_all(lanes);
    }
    built
}

/// Write the manifest into the bundle as a new file, refusing to replace one.
pub fn write_manifest(bundle: &Path, workset: &Workset) -> Result<PathBuf> {
    use std::io::Write as _;
    let path = bundle.join(MANIFEST_FILE);
    let mut body = serde_json::to_vec_pretty(workset)?;
    body.push(b'\n');
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut file| file.write_all(&body))
        .map_err(|error| anyhow!("write {}: {error}", path.display()))?;
    Ok(path)
}

/// Create the bundle's lanes directory, refusing a bundle a workset was
/// already written into.
fn prepare_bundle(bundle: &Path) -> Result<PathBuf> {
    let manifest = bundle.join(MANIFEST_FILE);
    let lanes = bundle.join(LANES_DIR);
    if manifest.exists() || lanes.exists() {
        bail!(
            "{} already holds a workset; choose an empty bundle directory",
            bundle.display()
        );
    }
    std::fs::create_dir_all(bundle)
        .map_err(|error| anyhow!("create {}: {error}", bundle.display()))?;
    std::fs::create_dir(&lanes).map_err(|error| anyhow!("create {}: {error}", lanes.display()))?;
    Ok(lanes)
}

fn build(
    backends: &[Box<dyn Backend>],
    selection: Selection,
    lane_bytes: u64,
    lanes: Option<&Path>,
) -> Result<Workset> {
    let observed_at = Utc::now();
    let resolved = selection.resolve(backends)?;
    let backend = backends[resolved.backend_index].as_ref();
    let harness = backend.harness().to_owned();
    let root_key = key(0);
    let files = lanes.map(|directory| {
        (
            directory,
            LaneSource {
                key: root_key.clone(),
                harness: harness.clone(),
                session: resolved.session.id.clone(),
            },
        )
    });
    let mut fold = SourceFold::new(&root_key, lane_bytes, files);
    let mut streamed = crate::stream_numbered(backend, &resolved.session, None, &mut |turn| {
        fold.turn(&turn)
    })?;
    streamed
        .notes
        .extend(resolved.diagnostics.latest_warnings(&resolved.session));
    let session = resolved.session;

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
    )?;

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
            backends, backend, &session, relative, key, lane_bytes, lanes,
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
    lanes: Option<&Path>,
) -> WorksetSource {
    let (harness, id) = relative.identity();
    let child = matches!(relative.target, Target::ChildOfRoot(_));
    let label = if child {
        format!("{} child {id}", root.id)
    } else {
        id.to_owned()
    };
    let files = lanes.map(|directory| {
        (
            directory,
            LaneSource {
                key: key.clone(),
                harness: harness.to_owned(),
                session: label,
            },
        )
    });
    let mut fold = SourceFold::new(&key, lane_bytes, files);
    let read = match &relative.target {
        Target::Session(id) => Selection::IdInHarness {
            id,
            harness: &relative.harness,
        }
        .resolve(backends)
        .and_then(|resolved| {
            let backend = backends[resolved.backend_index].as_ref();
            let streamed = crate::stream_numbered(backend, &resolved.session, None, &mut |turn| {
                fold.turn(&turn)
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
                    fold.turn(&turn)
                })
                .map(|StreamedChild { session, read }| Read {
                    harness: root_backend.harness().to_owned(),
                    session,
                    reference: Some(reference.clone()),
                    streamed: read,
                })
        }
    };
    let error = match read {
        Ok(read) => match fold.finish(key.clone(), relative.role, read) {
            Ok(source) => return source,
            Err(error) => error,
        },
        Err(error) => {
            fold.cutter.discard();
            error
        }
    };
    let (harness, id) = relative.identity();
    WorksetSource {
        key,
        harness: harness.to_owned(),
        session_id: match &relative.target {
            Target::Session(id) => id.clone(),
            Target::ChildOfRoot(_) => root.id.clone(),
        },
        role: relative.role,
        reference: child.then(|| id.to_owned()),
        source: None,
        source_length: None,
        source_revision: None,
        source_prefix_digest: None,
        content_digest: None,
        live: None,
        lanes: None,
        coverage: None,
        diagnostic: Some(format!("{error:#}")),
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
            None,
        );
        assert_eq!(source.session_id, "absent-parent");
        assert!(source.diagnostic.is_some(), "{source:?}");
        assert!(source.lanes.is_none() && source.coverage.is_none());
    }
}
