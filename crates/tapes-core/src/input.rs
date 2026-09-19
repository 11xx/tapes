//! Bounded readers for caller-supplied conversation exports.
//!
//! Explicit inputs are read through one backend so every view shares the
//! normalized source, occurrence, content, and coverage contracts. The reader
//! never extracts an archive, creates an index, opens referenced artifacts, or
//! falls back to installed stores.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use zip::ZipArchive;

use crate::backend::{Backend, Listing, Query};
use crate::content::{
    self, ArtifactReference, ContentAvailability, ContentCarrier, ContentCoverage, ContentPart,
};
use crate::lineage::Lineage;
use crate::model::{
    ByteSpan, ConversationEdge, ConversationGraph, ConversationNode, EmptyTextTailReason,
    EntryMetadata, KindDeclaration, Model, ReadEvidence, ReadRange, ReadRangeKind, RecordRef, Role,
    ScopeAuthority, Session, SessionMetadata, SourceBound, SourceDescriptor, SourceLocation,
    SourceScope, TerminalObservation, TextTailEvidence, Transcript, TranscriptEvidence, Truncation,
    Turn, TurnKind, TurnSelection, UserDefault,
};

pub const DEFAULT_SCAN_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_DECODED_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_RECORD_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_RESIDENT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_DECODED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_RESIDENT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_MEMBERS: usize = 10_000;
pub const MAX_DEPTH: usize = 128;
pub const MAX_ARTIFACT_BODY_CHARS: usize = 64 * 1024;
const MAX_ZIP_CENTRAL_DIRECTORY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ZIP_EOCD_SEARCH_BYTES: u64 = 65_557;
const MAX_MEMBER_NAME_BYTES: usize = 4 * 1024;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputFormat {
    Auto,
    Openai,
    ChatgptExporter,
    Perplexity,
}

impl InputFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "openai" => Ok(Self::Openai),
            "chatgpt-exporter" => Ok(Self::ChatgptExporter),
            "perplexity" => Ok(Self::Perplexity),
            _ => bail!("input format must be auto, openai, chatgpt-exporter, or perplexity"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Openai => "openai",
            Self::ChatgptExporter => "chatgpt-exporter",
            Self::Perplexity => "perplexity",
        }
    }
}

#[derive(Clone, Debug)]
pub struct InputOptions {
    pub paths: Vec<PathBuf>,
    pub format: InputFormat,
    pub source_scope: Option<String>,
    pub occurrence: Option<String>,
    pub after_occurrence: Option<String>,
    pub scan_bytes: u64,
    pub decoded_bytes: u64,
    pub record_bytes: u64,
    pub output_bytes: u64,
    pub resident_bytes: u64,
}

impl InputOptions {
    pub fn new(paths: Vec<PathBuf>, format: InputFormat) -> Self {
        Self {
            paths,
            format,
            source_scope: None,
            occurrence: None,
            after_occurrence: None,
            scan_bytes: DEFAULT_SCAN_BYTES,
            decoded_bytes: DEFAULT_DECODED_BYTES,
            record_bytes: DEFAULT_RECORD_BYTES,
            output_bytes: DEFAULT_OUTPUT_BYTES,
            resident_bytes: DEFAULT_RESIDENT_BYTES,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.paths.is_empty() {
            bail!("at least one --input path is required");
        }
        for (name, value, maximum) in [
            ("scan bytes", self.scan_bytes, MAX_SCAN_BYTES),
            ("decoded bytes", self.decoded_bytes, MAX_DECODED_BYTES),
            ("record bytes", self.record_bytes, MAX_RECORD_BYTES),
            ("output bytes", self.output_bytes, MAX_OUTPUT_BYTES),
            ("resident bytes", self.resident_bytes, MAX_RESIDENT_BYTES),
        ] {
            if value == 0 || value > maximum {
                bail!(
                    "input {name} must be between 1 and {}",
                    crate::byte_size::ByteSize::new(maximum)
                );
            }
        }
        if self.occurrence.is_some() && self.after_occurrence.is_some() {
            bail!("--occurrence conflicts with --after-occurrence");
        }
        Ok(())
    }
}

pub struct InputBackend {
    options: InputOptions,
    dataset: OnceLock<std::result::Result<InputDataset, String>>,
}

impl InputBackend {
    pub fn new(options: InputOptions) -> Result<Self> {
        options.validate()?;
        Ok(Self {
            options,
            dataset: OnceLock::new(),
        })
    }

    fn dataset(&self) -> Result<&InputDataset> {
        self.dataset
            .get_or_init(|| load_dataset(&self.options).map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow!("supplied input could not be read: {error}"))
    }

    fn matching_occurrences<'a>(
        &'a self,
        dataset: &'a InputDataset,
        id: &str,
    ) -> Vec<&'a InputOccurrence> {
        dataset
            .occurrences
            .iter()
            .filter(|occurrence| {
                occurrence.session.id == id
                    && self.options.occurrence.as_deref().is_none_or(|selected| {
                        occurrence.session.occurrence.as_deref() == Some(selected)
                    })
            })
            .collect()
    }
}

impl Backend for InputBackend {
    fn kinds(&self) -> KindDeclaration {
        KindDeclaration {
            recordable: TurnSelection::only([
                TurnKind::Operator,
                TurnKind::Assistant,
                TurnKind::Reasoning,
                TurnKind::Tool,
                TurnKind::Ambient,
                TurnKind::Unknown,
            ]),
            user_default: Some(UserDefault {
                kind: TurnKind::Operator,
                basis:
                    "an exported conversation's user messages are the ones its account holder sent"
                        .to_owned(),
            }),
        }
    }

    fn harness(&self) -> &'static str {
        "input"
    }

    /// A named input is never an optional store that happens to be absent; a
    /// missing path fails when the collection is read.
    fn available(&self) -> bool {
        true
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let dataset = self.dataset()?;
        let mut listing = Listing {
            scanned: dataset.scanned,
            artifacts: dataset.artifacts.clone(),
            unsearched: dataset.diagnostics.clone(),
            scan_truncated: dataset.scan_truncated,
            ..Listing::default()
        };
        let order = |left: &InputOccurrence, right: &InputOccurrence| {
            crate::compare_sessions(&left.session, &right.session, query.sort)
                .then(left.ordinal.cmp(&right.ordinal))
        };
        let after = match self.options.after_occurrence.as_deref() {
            None => None,
            Some(coordinate) => {
                let (observation, _) = parse_occurrence(coordinate)?;
                if observation != dataset.observation {
                    bail!("--after-occurrence belongs to a different supplied input observation");
                }
                let occurrence = dataset
                    .occurrences
                    .iter()
                    .find(|occurrence| occurrence.session.occurrence.as_deref() == Some(coordinate))
                    .ok_or_else(|| {
                        anyhow!("--after-occurrence is not a reached occurrence in this supplied input observation")
                    })?;
                Some(occurrence)
            }
        };
        let mut rows = dataset
            .occurrences
            .iter()
            .filter(|&occurrence| {
                let session = &occurrence.session;
                after.is_none_or(|after| order(occurrence, after) == Ordering::Greater)
                    && query.scope.is_none_or(|scope| {
                        session
                            .directory
                            .as_deref()
                            .is_some_and(|directory| scope.contains(directory))
                    })
                    && query.matches(session)
            })
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| order(left, right));
        listing.sessions = rows
            .into_iter()
            .take(query.limit)
            .map(|occurrence| occurrence.session.clone())
            .collect();
        Ok(listing)
    }

    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let dataset = self.dataset()?;
        let matches = self.matching_occurrences(dataset, id);
        if matches.len() > 1 {
            let references = matches
                .iter()
                .filter_map(|occurrence| occurrence.session.occurrence.as_deref())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "supplied session {id} occurs {} times; pass --occurrence with one of: {references}",
                matches.len()
            );
        }
        if !dataset.identity_coverage_complete {
            if matches.is_empty() {
                bail!(
                    "supplied input discovery is incomplete; session {id} was not reached, which does not establish that it is absent"
                );
            }
            bail!(
                "supplied input discovery is incomplete; cannot prove that session {id} is unique; pass --occurrence from a complete list row"
            );
        }
        Ok(matches.first().map(|occurrence| occurrence.session.clone()))
    }

    fn locate_occurrence(&self, occurrence: &str) -> Result<Option<Session>> {
        let dataset = self.dataset()?;
        Ok(dataset
            .occurrences
            .iter()
            .find(|candidate| candidate.session.occurrence.as_deref() == Some(occurrence))
            .map(|candidate| candidate.session.clone()))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let dataset = self.dataset()?;
        let occurrence = session
            .occurrence
            .as_deref()
            .ok_or_else(|| anyhow!("supplied session has no occurrence coordinate"))?;
        let occurrence = dataset
            .occurrences
            .iter()
            .find(|candidate| candidate.session.occurrence.as_deref() == Some(occurrence))
            .ok_or_else(|| anyhow!("supplied session occurrence was not reached in this input"))?;
        let mut transcript = input_transcript(occurrence, tail)?;
        if occurrence.collection_gaps > 0 {
            for diagnostic in dataset.diagnostics.iter().take(16) {
                if !transcript.notes.contains(diagnostic) {
                    transcript.notes.push(diagnostic.clone());
                }
            }
            if dataset.diagnostics.len() > 16 {
                transcript.notes.push(format!(
                    "{} additional collection diagnostics are available through list",
                    dataset.diagnostics.len() - 16
                ));
            }
        }
        let output_bytes = serde_json::to_vec(&transcript)?.len() as u64;
        if output_bytes > self.options.output_bytes {
            bail!(
                "serialized supplied transcript is {output_bytes} bytes, above the --output-bytes limit of {}",
                self.options.output_bytes
            );
        }
        Ok(transcript)
    }

    fn lineage(&self, _session: &Session) -> Result<Lineage> {
        Ok(Lineage::default())
    }
}

#[derive(Clone)]
struct InputOccurrence {
    session: Session,
    turns: Vec<Turn>,
    artifacts: Vec<ArtifactReference>,
    graph: Option<ConversationGraph>,
    evidence: ReadEvidence,
    terminal: Option<TerminalObservation>,
    trailing_record: Option<crate::model::TrailingRecord>,
    notes: Vec<String>,
    format: String,
    member: String,
    ordinal: usize,
    collection_gaps: usize,
}

struct InputDataset {
    occurrences: Vec<InputOccurrence>,
    artifacts: Vec<ArtifactReference>,
    observation: String,
    diagnostics: Vec<String>,
    scanned: usize,
    scan_truncated: bool,
    /// Every native ID in the collection was read: nothing was truncated and
    /// no gap can cover a record whose ID went unread. A collection can be
    /// incomplete and still complete for ID resolution.
    identity_coverage_complete: bool,
}

#[derive(Clone)]
struct AssociatedReport {
    member: String,
    backing: Option<String>,
    originating: Option<String>,
    origin_message: Option<String>,
    reference: ArtifactReference,
    part: ContentPart,
}

#[derive(Clone, Debug)]
struct DirectoryMember {
    name: String,
    path: PathBuf,
    size: u64,
    revision: String,
}

#[derive(Clone, Debug)]
struct ZipMemberMetadata {
    name: String,
    index: usize,
    compressed_size: u64,
    size: u64,
    crc32: u32,
    is_dir: bool,
    is_symlink: bool,
}

#[derive(Clone, Debug, Default)]
struct ManifestSelection {
    conversation_members: Vec<String>,
    library_metadata_members: Vec<String>,
    library_content_members: Vec<String>,
    expected_sizes: HashMap<String, u64>,
}

#[derive(Clone, Debug, Default)]
struct LibraryAssociation {
    file_id: Option<String>,
    backing: Option<String>,
    originating: Option<String>,
    origin_message: Option<String>,
}

fn input_transcript(occurrence: &InputOccurrence, tail: usize) -> Result<Transcript> {
    let mut turns = occurrence.turns.clone();
    let total = turns.len();
    for (ordinal, turn) in turns.iter_mut().enumerate() {
        turn.ordinal = ordinal;
    }
    if total > tail {
        turns.drain(..total - tail);
    }
    let text_count = turns
        .iter()
        .filter(|turn| matches!(turn.kind, TurnKind::Operator | TurnKind::Assistant))
        .count();
    let empty_reason = if text_count > 0 {
        None
    } else if tail == 0 {
        Some(EmptyTextTailReason::ZeroRequestedTail)
    } else if occurrence.turns.is_empty() {
        Some(EmptyTextTailReason::EmptyCompleteProjection)
    } else {
        Some(EmptyTextTailReason::NoOperatorAssistantTextInRead)
    };
    let gap_count = occurrence
        .evidence
        .gaps
        .len()
        .max(occurrence.collection_gaps);
    let truncation = Truncation {
        window: Truncation::window(turns.len(), total, tail),
        source: (gap_count > 0)
            .then_some(SourceBound::InputCoverage { gaps: gap_count })
            .into_iter()
            .collect(),
    };
    let mut notes = occurrence.notes.clone();
    if gap_count > 0 && !notes.iter().any(|note| note.contains("coverage gap")) {
        notes.push(format!(
            "the supplied input has {} explicit coverage gap(s); this transcript is a known projection, not a complete collection",
            gap_count
        ));
    }
    let mut transcript = Transcript::with_evidence(
        occurrence.session.clone(),
        turns,
        truncation,
        TranscriptEvidence {
            read: Some(occurrence.evidence.clone()),
            terminal: occurrence.terminal.clone(),
            text_tail: Some(TextTailEvidence {
                requested: tail,
                returned: text_count,
                empty_reason,
            }),
        },
        occurrence.trailing_record.clone(),
        notes,
    );
    transcript.artifacts = occurrence.artifacts.clone();
    transcript.graph = occurrence.graph.clone();
    Ok(transcript)
}

fn load_dataset(options: &InputOptions) -> Result<InputDataset> {
    let budget = Budget::new(options);
    let mut occurrences = Vec::new();
    let mut diagnostics = Vec::new();
    let mut associated_reports = Vec::new();
    let mut observation_parts = Vec::new();
    let mut collection_gaps = Vec::new();
    let mut discovery_incomplete = false;
    let mut scanned = 0;
    let mut scan_truncated = false;
    for path in &options.paths {
        if !path.exists() {
            bail!("input path does not exist: {}", path.display());
        }
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            bail!("supplied input path is a symlink: {}", path.display());
        }
        if path.is_dir() {
            scan_directory(
                path,
                options,
                &budget,
                &mut occurrences,
                &mut diagnostics,
                &mut associated_reports,
                &mut observation_parts,
                &mut collection_gaps,
                &mut scanned,
                &mut scan_truncated,
                &mut discovery_incomplete,
            )?;
        } else if is_zip_path(path, &budget)? {
            scan_zip(
                path,
                options,
                &budget,
                &mut occurrences,
                &mut diagnostics,
                &mut associated_reports,
                &mut observation_parts,
                &mut collection_gaps,
                &mut scanned,
                &mut scan_truncated,
                &mut discovery_incomplete,
            )?;
        } else {
            budget.add_members(1)?;
            let metadata = fs::metadata(path)?;
            observation_parts.push(format!(
                "file:{}:{}",
                path.display(),
                metadata_revision(&metadata)
            ));
            scan_file(
                path,
                None,
                options,
                &budget,
                &mut occurrences,
                &mut diagnostics,
                &mut scanned,
                &mut scan_truncated,
                &mut discovery_incomplete,
                &mut collection_gaps,
            )?;
        }
        if budget.exhausted() {
            scan_truncated = true;
            diagnostics.push("input scan or decoded-byte budget exhausted".to_owned());
            break;
        }
    }
    if occurrences.is_empty() && scanned > 0 && diagnostics.is_empty() {
        diagnostics.push(format!(
            "no {} conversation records were recognized",
            options.format.name()
        ));
    }
    if occurrences.is_empty() && scanned == 0 && diagnostics.is_empty() {
        diagnostics.push(format!(
            "no {} conversation records were recognized",
            options.format.name()
        ));
    }
    if !collection_gaps.is_empty() {
        discovery_incomplete = true;
        scan_truncated |= collection_gaps.iter().any(gap_stops_discovery);
        for occurrence in &mut occurrences {
            occurrence.collection_gaps = collection_gaps.len();
            occurrence.notes.push(format!(
                "the supplied input collection has {} member coverage gap(s)",
                collection_gaps.len()
            ));
        }
    }
    attach_associated_reports(&mut occurrences, &mut associated_reports, &mut diagnostics);
    if occurrences.is_empty() && discovery_incomplete {
        bail!(
            "supplied input discovery is incomplete and produced no complete {} conversation records; {}",
            options.format.name(),
            diagnostics.join("; ")
        );
    }
    if occurrences.is_empty() {
        bail!(
            "supplied input contained no recognized {} conversation records; {}",
            options.format.name(),
            diagnostics.join("; ")
        );
    }
    let identity_coverage_complete =
        !scan_truncated && collection_gaps.iter().all(gap_is_identity_safe);
    let observation = observation_revision(&observation_parts);
    for (ordinal, occurrence) in occurrences.iter_mut().enumerate() {
        occurrence.ordinal = ordinal;
        occurrence.session.occurrence = Some(format!(
            "input:v2:{}:{}:{}:{}",
            encode_component(&observation),
            encode_component(&occurrence.format),
            encode_component(&occurrence.member),
            ordinal
        ));
    }
    Ok(InputDataset {
        artifacts: associated_reports
            .iter()
            .map(|report| report.reference.clone())
            .collect(),
        observation,
        occurrences,
        diagnostics,
        scanned,
        scan_truncated,
        identity_coverage_complete,
    })
}

const MISSING_NATIVE_ID: &str = "missing-native-id";

fn gap_stops_discovery(gap: &crate::model::ReadGap) -> bool {
    !matches!(
        gap.reason.as_str(),
        "record-bytes-bound" | "resident-byte-bound" | "member-size-bound" | MISSING_NATIVE_ID
    )
}

/// A record rejected for lacking a native ID was parsed in full, so it cannot
/// hold the ID a lookup asks for. Any other gap may cover an unread ID.
fn gap_is_identity_safe(gap: &crate::model::ReadGap) -> bool {
    gap.reason == MISSING_NATIVE_ID
}

struct Budget {
    scan_used: Cell<u64>,
    decoded_used: Cell<u64>,
    resident_used: Cell<u64>,
    members: Cell<usize>,
    scan_blocked: Cell<bool>,
    decoded_blocked: Cell<bool>,
    scan_limit: u64,
    decoded_limit: u64,
    resident_limit: u64,
}

impl Budget {
    fn new(options: &InputOptions) -> Self {
        Self {
            scan_used: Cell::new(0),
            decoded_used: Cell::new(0),
            resident_used: Cell::new(0),
            members: Cell::new(0),
            scan_blocked: Cell::new(false),
            decoded_blocked: Cell::new(false),
            scan_limit: options.scan_bytes,
            decoded_limit: options.decoded_bytes,
            resident_limit: options.resident_bytes,
        }
    }

    fn remaining_scan(&self) -> u64 {
        self.scan_limit.saturating_sub(self.scan_used.get())
    }

    fn remaining_decoded(&self) -> u64 {
        self.decoded_limit.saturating_sub(self.decoded_used.get())
    }

    fn charge_scan(&self, amount: u64) -> io::Result<()> {
        let next = self
            .scan_used
            .get()
            .checked_add(amount)
            .ok_or_else(|| io::Error::other("input scan budget overflowed"))?;
        if next > self.scan_limit {
            self.scan_blocked.set(true);
            return Err(io::Error::other("input scan budget exhausted"));
        }
        self.scan_used.set(next);
        Ok(())
    }

    fn charge_decoded(&self, amount: u64) -> io::Result<()> {
        let next = self
            .decoded_used
            .get()
            .checked_add(amount)
            .ok_or_else(|| io::Error::other("input decoded-byte budget overflowed"))?;
        if next > self.decoded_limit {
            self.decoded_blocked.set(true);
            return Err(io::Error::other("input decoded-byte budget exhausted"));
        }
        self.decoded_used.set(next);
        Ok(())
    }

    fn reserve_resident(&self, amount: u64) -> bool {
        let Some(next) = self.resident_used.get().checked_add(amount) else {
            return false;
        };
        if next > self.resident_limit {
            return false;
        }
        self.resident_used.set(next);
        true
    }

    fn add_members(&self, amount: usize) -> Result<()> {
        let next = self
            .members
            .get()
            .checked_add(amount)
            .ok_or_else(|| anyhow!("supplied input member count overflowed"))?;
        if next > MAX_MEMBERS {
            bail!("supplied input has more than {MAX_MEMBERS} members");
        }
        self.members.set(next);
        Ok(())
    }

    fn exhausted(&self) -> bool {
        self.scan_blocked.get() || self.decoded_blocked.get()
    }
}

/// Charges bytes read from a physical source. ZIP decompression reads are
/// charged here as compressed bytes; `Scanner` charges the resulting decoded
/// bytes when its buffer receives them.
struct BudgetedSourceReader<'a, R> {
    reader: R,
    budget: &'a Budget,
}

impl<'a, R> BudgetedSourceReader<'a, R> {
    fn new(reader: R, budget: &'a Budget) -> Self {
        Self { reader, budget }
    }
}

impl<R: Read> Read for BudgetedSourceReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let allowed = buffer
            .len()
            .min(self.budget.remaining_scan().min(usize::MAX as u64) as usize);
        if allowed == 0 {
            self.budget.scan_blocked.set(true);
            return Err(io::Error::other("input scan budget exhausted"));
        }
        let read = self.reader.read(&mut buffer[..allowed])?;
        self.budget.charge_scan(read as u64)?;
        Ok(read)
    }
}

impl<R: Seek> Seek for BudgetedSourceReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.reader.seek(position)
    }
}

struct Scanner<'a, R> {
    reader: R,
    budget: &'a Budget,
    buffer: [u8; 64 * 1024],
    position: usize,
    length: usize,
    offset: u64,
    pending: Option<u8>,
}

impl<'a, R: Read> Scanner<'a, R> {
    fn new(reader: R, budget: &'a Budget) -> Self {
        Self {
            reader,
            budget,
            buffer: [0; 64 * 1024],
            position: 0,
            length: 0,
            offset: 0,
            pending: None,
        }
    }

    fn byte(&mut self) -> io::Result<Option<u8>> {
        if let Some(byte) = self.pending.take() {
            self.offset += 1;
            return Ok(Some(byte));
        }
        if self.position == self.length {
            let remaining = self.budget.remaining_decoded();
            if remaining == 0 {
                self.budget.decoded_blocked.set(true);
                return Err(io::Error::other("input decoded-byte budget exhausted"));
            }
            let read_size = self
                .buffer
                .len()
                .min(remaining.min(usize::MAX as u64) as usize);
            let read = self.reader.read(&mut self.buffer[..read_size])?;
            if read == 0 {
                return Ok(None);
            }
            self.budget.charge_decoded(read as u64)?;
            self.position = 0;
            self.length = read;
        }
        let byte = self.buffer[self.position];
        self.position += 1;
        self.offset += 1;
        Ok(Some(byte))
    }

    fn non_whitespace(&mut self) -> io::Result<Option<(u8, u64)>> {
        loop {
            let start = self.offset;
            let Some(byte) = self.byte()? else {
                return Ok(None);
            };
            if !byte.is_ascii_whitespace() {
                return Ok(Some((byte, start)));
            }
        }
    }

    fn value(
        &mut self,
        first: u8,
        start: u64,
        max: u64,
        reason: &'static str,
    ) -> io::Result<Frame> {
        let mut depth = match first {
            b'{' | b'[' => 1usize,
            _ => 0,
        };
        let mut in_string = false;
        let mut escaped = false;
        let mut bytes = Vec::new();
        let mut oversized = max == 0;
        let mut oversized_reason = reason;
        if !oversized {
            bytes.push(first);
        }
        if depth == 0 {
            return Ok(Frame::Invalid {
                span: ByteSpan {
                    start,
                    end: self.offset,
                },
            });
        }
        while depth > 0 {
            let Some(byte) = self.byte()? else {
                return Ok(Frame::Incomplete {
                    span: ByteSpan {
                        start,
                        end: self.offset,
                    },
                });
            };
            if !oversized {
                if bytes.len() as u64 >= max {
                    oversized = true;
                    bytes.clear();
                } else {
                    bytes.push(byte);
                }
            }
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        oversized = true;
                        oversized_reason = "structural-depth-bound";
                        bytes.clear();
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let span = ByteSpan {
            start,
            end: self.offset,
        };
        if oversized {
            Ok(Frame::Oversized {
                span,
                reason: oversized_reason,
            })
        } else {
            Ok(Frame::Record { bytes, span })
        }
    }

    fn scalar(
        &mut self,
        first: u8,
        start: u64,
        max: u64,
        reason: &'static str,
    ) -> io::Result<Frame> {
        let mut bytes = Vec::new();
        let mut in_string = first == b'"';
        let mut escaped = false;
        let mut oversized = max == 0;
        if !oversized {
            bytes.push(first);
        }
        loop {
            let Some(byte) = self.byte()? else {
                let span = ByteSpan {
                    start,
                    end: self.offset,
                };
                return if oversized {
                    Ok(Frame::Oversized { span, reason })
                } else {
                    Ok(Frame::Record { bytes, span })
                };
            };
            if in_string {
                if !oversized {
                    if bytes.len() as u64 >= max {
                        oversized = true;
                        bytes.clear();
                    } else {
                        bytes.push(byte);
                    }
                }
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            if byte.is_ascii_whitespace() || matches!(byte, b',' | b'}' | b']') {
                self.offset = self.offset.saturating_sub(1);
                self.pending = Some(byte);
                let span = ByteSpan {
                    start,
                    end: self.offset,
                };
                return if oversized {
                    Ok(Frame::Oversized { span, reason })
                } else {
                    Ok(Frame::Record { bytes, span })
                };
            }
            if !oversized {
                if bytes.len() as u64 >= max {
                    oversized = true;
                    bytes.clear();
                } else {
                    bytes.push(byte);
                }
            }
        }
    }

    fn string(&mut self, _start: u64, max: usize) -> io::Result<String> {
        let mut raw = Vec::with_capacity(max.saturating_add(2));
        raw.push(b'"');
        let mut escaped = false;
        loop {
            let Some(byte) = self.byte()? else {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated JSON object key",
                ));
            };
            if raw.len() >= max.saturating_add(2) {
                return Err(io::Error::other(
                    "JSON object key exceeds the structural bound",
                ));
            }
            raw.push(byte);
            if escaped {
                escaped = false;
                continue;
            }
            if byte == b'\\' {
                escaped = true;
                continue;
            }
            if byte == b'"' {
                let value = serde_json::from_slice::<String>(&raw).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid JSON object key: {error}"),
                    )
                })?;
                if value.len() > max {
                    return Err(io::Error::other(
                        "decoded JSON object key exceeds the structural bound",
                    ));
                }
                return Ok(value);
            }
        }
    }
}

enum Frame {
    Record {
        bytes: Vec<u8>,
        span: ByteSpan,
    },
    Oversized {
        span: ByteSpan,
        reason: &'static str,
    },
    Invalid {
        span: ByteSpan,
    },
    Incomplete {
        span: ByteSpan,
    },
}

#[allow(clippy::too_many_arguments)]
fn scan_reader<R: Read>(
    reader: R,
    locator: &str,
    member: Option<&str>,
    source_length: u64,
    revision: String,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    scanned: &mut usize,
    gaps: &mut Vec<crate::model::ReadGap>,
    stopped: &mut bool,
) -> Result<()> {
    let mut scanner = Scanner::new(reader, budget);
    let Some((first, start)) = scanner.non_whitespace()? else {
        return Ok(());
    };
    match first {
        b'[' => loop {
            let Some((byte, item_start)) = scanner.non_whitespace()? else {
                diagnostics.push(format!("{locator}: incomplete top-level array"));
                gaps.push(crate::model::ReadGap {
                    span: ByteSpan {
                        start: scanner.offset,
                        end: source_length,
                    },
                    reason: "incomplete-top-level-array".to_owned(),
                });
                *stopped = true;
                break;
            };
            if byte == b']' {
                break;
            }
            let (record_limit, record_reason) = retention_limit(options, budget);
            let frame = scanner.value(byte, item_start, record_limit, record_reason)?;
            match frame {
                Frame::Record { bytes, span } => {
                    *scanned += 1;
                    if !budget.reserve_resident(bytes.len() as u64) {
                        gaps.push(crate::model::ReadGap {
                            span,
                            reason: "resident-byte-bound".to_owned(),
                        });
                        diagnostics.push(format!(
                            "{locator}: skipped record at {}..{} because the aggregate resident-byte budget is exhausted",
                            span.start, span.end
                        ));
                    } else {
                        parse_record(
                            &bytes,
                            span,
                            locator,
                            member,
                            source_length,
                            revision.clone(),
                            options,
                            occurrences,
                            diagnostics,
                            gaps,
                        )?;
                    }
                }
                Frame::Oversized { span, reason } => {
                    *scanned += 1;
                    gaps.push(crate::model::ReadGap {
                        span,
                        reason: reason.to_owned(),
                    });
                    diagnostics.push(format!(
                        "{locator}: skipped oversized record at {}..{}",
                        span.start, span.end
                    ));
                }
                Frame::Invalid { span } | Frame::Incomplete { span } => {
                    diagnostics.push(format!(
                        "{locator}: top-level array lost structural synchronization"
                    ));
                    gaps.push(crate::model::ReadGap {
                        span,
                        reason: "structural-synchronization".to_owned(),
                    });
                    *stopped = true;
                    break;
                }
            }
            let Some((separator, _)) = scanner.non_whitespace()? else {
                diagnostics.push(format!("{locator}: incomplete top-level array"));
                gaps.push(crate::model::ReadGap {
                    span: ByteSpan {
                        start: scanner.offset,
                        end: source_length,
                    },
                    reason: "incomplete-top-level-array".to_owned(),
                });
                *stopped = true;
                break;
            };
            if separator == b']' {
                break;
            }
            if separator != b',' {
                diagnostics.push(format!(
                    "{locator}: top-level array has an unexpected separator"
                ));
                gaps.push(crate::model::ReadGap {
                    span: ByteSpan {
                        start: scanner.offset.saturating_sub(1),
                        end: scanner.offset,
                    },
                    reason: "unexpected-array-separator".to_owned(),
                });
                *stopped = true;
                break;
            }
        },
        b'{' => {
            let complete = scan_root_object(
                &mut scanner,
                start,
                locator,
                member,
                source_length,
                &revision,
                options,
                occurrences,
                diagnostics,
                scanned,
                gaps,
                stopped,
            )?;
            if complete {
                if scanner.offset >= source_length {
                    return Ok(());
                }
                while let Some((next, next_start)) = scanner.non_whitespace()? {
                    if next != b'{' {
                        diagnostics.push(format!(
                            "{locator}: unexpected trailing byte {next:?} at {next_start}"
                        ));
                        gaps.push(crate::model::ReadGap {
                            span: ByteSpan {
                                start: next_start,
                                end: scanner.offset,
                            },
                            reason: "unexpected-trailing-byte".to_owned(),
                        });
                        *stopped = true;
                        break;
                    }
                    if !scan_root_object(
                        &mut scanner,
                        next_start,
                        locator,
                        member,
                        source_length,
                        &revision,
                        options,
                        occurrences,
                        diagnostics,
                        scanned,
                        gaps,
                        stopped,
                    )? {
                        break;
                    }
                }
            }
        }
        _ => {
            diagnostics.push(format!("{locator}: root must be a JSON array or object"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start,
                    end: scanner.offset,
                },
                reason: "unsupported-root".to_owned(),
            });
            *stopped = true;
        }
    }
    Ok(())
}

fn retention_limit(options: &InputOptions, _budget: &Budget) -> (u64, &'static str) {
    (options.record_bytes, "record-bytes-bound")
}

#[allow(clippy::too_many_arguments)]
fn scan_root_object<R: Read>(
    scanner: &mut Scanner<'_, R>,
    start: u64,
    locator: &str,
    member: Option<&str>,
    source_length: u64,
    revision: &str,
    options: &InputOptions,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    scanned: &mut usize,
    gaps: &mut Vec<crate::model::ReadGap>,
    stopped: &mut bool,
) -> Result<bool> {
    let mut fields = serde_json::Map::new();
    let mut has_conversations = false;
    let mut field_bytes = 0u64;
    let mut field_gap = false;
    loop {
        let Some((first, key_start)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level object"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: scanner.offset,
                    end: source_length,
                },
                reason: "incomplete-top-level-object".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        };
        if first == b'}' {
            break;
        }
        if first != b'"' {
            diagnostics.push(format!("{locator}: top-level object expected a quoted key"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: key_start,
                    end: scanner.offset,
                },
                reason: "invalid-top-level-object-key".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        }
        let key = scanner.string(key_start, 64 * 1024)?;
        let Some((colon, _)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: object key has no value"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: key_start,
                    end: scanner.offset,
                },
                reason: "incomplete-object-member".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        };
        if colon != b':' {
            diagnostics.push(format!("{locator}: top-level object expected a colon"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: key_start,
                    end: scanner.offset,
                },
                reason: "invalid-object-colon".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        }
        let Some((value_first, value_start)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: object key has no value"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: key_start,
                    end: scanner.offset,
                },
                reason: "incomplete-object-member".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        };
        if key == "conversations" && value_first == b'[' {
            has_conversations = true;
            if !scan_array_items(
                scanner,
                value_start,
                locator,
                member,
                source_length,
                revision,
                options,
                occurrences,
                diagnostics,
                scanned,
                gaps,
                stopped,
            )? {
                return Ok(false);
            }
        } else {
            let (record_limit, record_reason) = retention_limit(options, scanner.budget);
            let frame = if matches!(value_first, b'{' | b'[') {
                scanner.value(value_first, value_start, record_limit, record_reason)?
            } else {
                scanner.scalar(value_first, value_start, record_limit, record_reason)?
            };
            match frame {
                Frame::Record { bytes, span } => {
                    field_bytes = field_bytes.saturating_add(bytes.len() as u64);
                    if field_bytes <= options.record_bytes {
                        if scanner.budget.reserve_resident(bytes.len() as u64) {
                            if let Ok(value) = serde_json::from_slice(&bytes) {
                                fields.insert(key, value);
                            } else {
                                diagnostics.push(format!("{locator}: malformed object member"));
                                field_gap = true;
                                gaps.push(crate::model::ReadGap {
                                    span,
                                    reason: "malformed-object-member".to_owned(),
                                });
                            }
                        } else {
                            diagnostics.push(format!(
                                "{locator}: aggregate resident-byte budget exhausted while retaining object member"
                            ));
                            field_gap = true;
                            gaps.push(crate::model::ReadGap {
                                span,
                                reason: "resident-byte-bound".to_owned(),
                            });
                        }
                    } else {
                        diagnostics.push(format!(
                            "{locator}: top-level object fields exceed --record-bytes"
                        ));
                        field_gap = true;
                        gaps.push(crate::model::ReadGap {
                            span,
                            reason: "record-bytes-bound".to_owned(),
                        });
                    }
                }
                Frame::Oversized { span, reason } => {
                    field_gap = true;
                    gaps.push(crate::model::ReadGap {
                        span,
                        reason: reason.to_owned(),
                    });
                    diagnostics.push(format!(
                        "{locator}: skipped oversized object member at {}..{}",
                        span.start, span.end
                    ));
                }
                Frame::Invalid { span } | Frame::Incomplete { span } => {
                    diagnostics.push(format!("{locator}: incomplete top-level object member"));
                    gaps.push(crate::model::ReadGap {
                        span,
                        reason: "structural-synchronization".to_owned(),
                    });
                    *stopped = true;
                    return Ok(false);
                }
            }
        }
        let Some((separator, _)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level object"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: scanner.offset,
                    end: source_length,
                },
                reason: "incomplete-top-level-object".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        };
        if separator == b'}' {
            break;
        }
        if separator != b',' {
            diagnostics.push(format!(
                "{locator}: top-level object has an unexpected separator"
            ));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: scanner.offset.saturating_sub(1),
                    end: scanner.offset,
                },
                reason: "unexpected-object-separator".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        }
    }
    if has_conversations {
        return Ok(true);
    }
    if field_gap {
        *scanned += 1;
        return Ok(true);
    }
    let bytes = serde_json::to_vec(&Value::Object(fields))?;
    *scanned += 1;
    if bytes.len() as u64 > options.record_bytes {
        diagnostics.push(format!(
            "{locator}: top-level object exceeds --record-bytes at {}..{}",
            start, scanner.offset
        ));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan {
                start,
                end: scanner.offset,
            },
            reason: "record-bytes-bound".to_owned(),
        });
        return Ok(true);
    }
    if !scanner.budget.reserve_resident(bytes.len() as u64) {
        diagnostics.push(format!(
            "{locator}: skipped top-level object because the aggregate resident-byte budget is exhausted"
        ));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan {
                start,
                end: scanner.offset,
            },
            reason: "resident-byte-bound".to_owned(),
        });
        return Ok(true);
    }
    parse_record(
        &bytes,
        ByteSpan {
            start,
            end: scanner.offset,
        },
        locator,
        member,
        source_length,
        revision.to_owned(),
        options,
        occurrences,
        diagnostics,
        gaps,
    )?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn scan_array_items<R: Read>(
    scanner: &mut Scanner<'_, R>,
    _start: u64,
    locator: &str,
    member: Option<&str>,
    source_length: u64,
    revision: &str,
    options: &InputOptions,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    scanned: &mut usize,
    gaps: &mut Vec<crate::model::ReadGap>,
    stopped: &mut bool,
) -> Result<bool> {
    loop {
        let Some((first, item_start)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level array"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: scanner.offset,
                    end: source_length,
                },
                reason: "incomplete-top-level-array".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        };
        if first == b']' {
            return Ok(true);
        }
        let (record_limit, record_reason) = retention_limit(options, scanner.budget);
        let frame = scanner.value(first, item_start, record_limit, record_reason)?;
        match frame {
            Frame::Record { bytes, span } => {
                *scanned += 1;
                if !scanner.budget.reserve_resident(bytes.len() as u64) {
                    gaps.push(crate::model::ReadGap {
                        span,
                        reason: "resident-byte-bound".to_owned(),
                    });
                    diagnostics.push(format!(
                        "{locator}: skipped record at {}..{} because the aggregate resident-byte budget is exhausted",
                        span.start, span.end
                    ));
                } else {
                    parse_record(
                        &bytes,
                        span,
                        locator,
                        member,
                        source_length,
                        revision.to_owned(),
                        options,
                        occurrences,
                        diagnostics,
                        gaps,
                    )?;
                }
            }
            Frame::Oversized { span, reason } => {
                *scanned += 1;
                gaps.push(crate::model::ReadGap {
                    span,
                    reason: reason.to_owned(),
                });
                diagnostics.push(format!(
                    "{locator}: skipped oversized record at {}..{}",
                    span.start, span.end
                ));
            }
            Frame::Invalid { span } | Frame::Incomplete { span } => {
                diagnostics.push(format!(
                    "{locator}: top-level array lost structural synchronization"
                ));
                gaps.push(crate::model::ReadGap {
                    span,
                    reason: "structural-synchronization".to_owned(),
                });
                *stopped = true;
                return Ok(false);
            }
        }
        let Some((separator, _)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level array"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: scanner.offset,
                    end: source_length,
                },
                reason: "incomplete-top-level-array".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        };
        if separator == b']' {
            return Ok(true);
        }
        if separator != b',' {
            diagnostics.push(format!(
                "{locator}: top-level array has an unexpected separator"
            ));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: scanner.offset.saturating_sub(1),
                    end: scanner.offset,
                },
                reason: "unexpected-array-separator".to_owned(),
            });
            *stopped = true;
            return Ok(false);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_record(
    bytes: &[u8],
    span: ByteSpan,
    locator: &str,
    member: Option<&str>,
    source_length: u64,
    revision: String,
    options: &InputOptions,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    gaps: &mut Vec<crate::model::ReadGap>,
) -> Result<()> {
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(format!(
                "{locator}: malformed record at {}..{}: {error}",
                span.start, span.end
            ));
            gaps.push(crate::model::ReadGap {
                span,
                reason: "malformed-record".to_owned(),
            });
            return Ok(());
        }
    };
    if let Some(conversations) = value.get("conversations").and_then(Value::as_array) {
        for conversation in conversations {
            let conversation = serde_json::to_vec(conversation)
                .context("serialize bounded conversation envelope member")?;
            parse_record(
                &conversation,
                span,
                locator,
                member,
                source_length,
                revision.clone(),
                options,
                occurrences,
                diagnostics,
                gaps,
            )?;
        }
        return Ok(());
    }
    let format = match options.format {
        InputFormat::Auto => detect_format(&value)?,
        declared => Some(declared),
    };
    let Some(format) = format else {
        diagnostics.push(format!(
            "{locator}: record shape is not a supported conversation export"
        ));
        gaps.push(crate::model::ReadGap {
            span,
            reason: "unsupported-record-shape".to_owned(),
        });
        return Ok(());
    };
    let ordinal = occurrences.len();
    let Some((
        id,
        title,
        started_at,
        last_activity_at,
        directory,
        model,
        metadata,
        turns,
        graph,
        mut notes,
    )) = normalize_value(&value, format)?
    else {
        gaps.push(crate::model::ReadGap {
            span,
            reason: "unsupported-record-shape".to_owned(),
        });
        return Ok(());
    };
    let Some(id) = id else {
        diagnostics.push(format!(
            "{locator}: conversation record at {}..{} has no native conversation id",
            span.start, span.end
        ));
        gaps.push(crate::model::ReadGap {
            span,
            reason: MISSING_NATIVE_ID.to_owned(),
        });
        return Ok(());
    };
    let (source_origin, representation, identified_producer) = match format {
        InputFormat::Openai => ("openai", "openai-conversation", Some("OpenAI export")),
        InputFormat::ChatgptExporter => (
            "openai",
            "chatgpt-exporter-conversation",
            Some("ChatGPT Exporter"),
        ),
        InputFormat::Perplexity => (
            "perplexity",
            "perplexity-conversation-export",
            Some("Perplexity export"),
        ),
        InputFormat::Auto => unreachable!(),
    };
    let (producer, producer_authority) = if options.format == InputFormat::Auto {
        (None, None)
    } else {
        (identified_producer, Some(ScopeAuthority::Declared))
    };
    let scope = options.source_scope.clone().map(|value| SourceScope {
        value,
        authority: ScopeAuthority::Declared,
    });
    let mut source = SourceDescriptor::supplied(
        source_origin,
        representation,
        producer,
        locator.to_owned(),
        scope,
    );
    source.producer_authority = producer_authority;
    source.location = Some(SourceLocation {
        locator: locator.to_owned(),
        member: member.map(str::to_owned),
    });
    let source_domain = format!("{source_origin}:{locator}:{}", member.unwrap_or("root"));
    let mut turns = turns;
    for (turn_index, turn) in turns.iter_mut().enumerate() {
        let reference = RecordRef {
            domain: source_domain.clone(),
            revision: Some(revision.clone()),
            span: Some(span),
            native_id: turn.native_id.clone(),
            part_index: turn_index,
            content_part_index: None,
            pointer: turn
                .record_ref
                .as_ref()
                .and_then(|reference| reference.pointer.clone()),
        };
        turn.record_ref = Some(reference.clone());
        for (content_part_index, part) in turn.parts.iter_mut().enumerate() {
            part.set_record_ref_part(reference.clone(), content_part_index);
        }
    }
    let mut graph = graph;
    if let Some(graph) = graph.as_mut() {
        for (node_index, node) in graph.nodes.iter_mut().enumerate() {
            let Some(message) = node.message.as_mut() else {
                continue;
            };
            let reference = RecordRef {
                domain: source_domain.clone(),
                revision: Some(revision.clone()),
                span: Some(span),
                native_id: message.native_id.clone(),
                part_index: node_index,
                content_part_index: None,
                pointer: message
                    .record_ref
                    .as_ref()
                    .and_then(|reference| reference.pointer.clone()),
            };
            message.record_ref = Some(reference.clone());
            for (content_part_index, part) in message.parts.iter_mut().enumerate() {
                part.set_record_ref_part(reference.clone(), content_part_index);
            }
        }
    }
    let session = Session {
        id,
        occurrence: None,
        source,
        metadata,
        model,
        title,
        derived_title: None,
        derived_title_truncated: None,
        directory,
        started_at,
        last_activity_at,
        live: None,
        cost: None,
        tokens: None,
        accounting: None,
        start_uncertain: false,
        usage_detail: None,
    };
    if turns.is_empty() {
        notes.push("the selected source record has no readable canonical branch".to_owned());
    }
    let evidence = ReadEvidence {
        source_length,
        configured_bound: options.record_bytes,
        coordinate_domain: "supplied-occurrence".to_owned(),
        source_revision: Some(revision.clone()),
        producer: producer.map(str::to_owned),
        projection: crate::model::SESSION_SCHEMA.to_owned(),
        projection_options: vec![format!("format={}", format.name())],
        observed_at: Utc::now(),
        ranges: vec![ReadRange {
            kind: ReadRangeKind::Tail,
            span,
        }],
        records: vec![span],
        context_records: Vec::new(),
        gaps: Vec::new(),
        unmapped: None,
    };
    occurrences.push(InputOccurrence {
        session,
        turns,
        artifacts: Vec::new(),
        graph,
        evidence,
        terminal: None,
        trailing_record: None,
        notes,
        format: format.name().to_owned(),
        member: member.unwrap_or("root").to_owned(),
        ordinal,
        collection_gaps: 0,
    });
    Ok(())
}

type NormalizedConversation = Option<(
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<PathBuf>,
    Option<Model>,
    Option<SessionMetadata>,
    Vec<Turn>,
    Option<ConversationGraph>,
    Vec<String>,
)>;

fn normalize_value(value: &Value, format: InputFormat) -> Result<NormalizedConversation> {
    match format {
        InputFormat::Openai => normalize_openai(value),
        InputFormat::ChatgptExporter => normalize_chatgpt_exporter(value),
        InputFormat::Perplexity => normalize_perplexity(value),
        InputFormat::Auto => unreachable!(),
    }
}

fn normalize_openai(value: &Value) -> Result<NormalizedConversation> {
    normalize_mapping(value)
}

fn normalize_mapping(value: &Value) -> Result<NormalizedConversation> {
    if !value.is_object() {
        return Ok(None);
    }
    let id = value["conversation_id"]
        .as_str()
        .or_else(|| value["id"].as_str())
        .map(str::to_owned);
    let title = value["title"]
        .as_str()
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let started_at = timestamp_value(&value["create_time"]);
    let last_activity_at = timestamp_value(&value["update_time"]);
    let directory = value["metadata"]["cwd"].as_str().map(PathBuf::from);
    let model = value["default_model_slug"].as_str().map(|id| Model {
        id: id.to_owned(),
        variant: None,
    });
    let Some(mapping) = value["mapping"].as_object() else {
        return Ok(None);
    };
    let mut notes = Vec::new();
    let (graph, selected_path) =
        normalize_mapping_graph(mapping, value["current_node"].as_str(), &mut notes);
    let mut turns = Vec::new();
    for (index, node_id) in selected_path.iter().enumerate() {
        let Some(node) = graph.nodes.iter().find(|node| node.id == *node_id) else {
            continue;
        };
        let Some(mut turn) = node.message.clone() else {
            continue;
        };
        turn.ordinal = index;
        turns.push(turn);
    }
    Ok(Some((
        id,
        title,
        started_at,
        last_activity_at,
        directory,
        model,
        None,
        turns,
        Some(graph),
        notes,
    )))
}

const MAX_GRAPH_NODES: usize = 4096;
const MAX_GRAPH_EDGES: usize = 8192;
const MAX_CANONICAL_PATH: usize = 4096;

fn normalize_mapping_graph(
    mapping: &serde_json::Map<String, Value>,
    current: Option<&str>,
    notes: &mut Vec<String>,
) -> (ConversationGraph, Vec<String>) {
    let selected_path = canonical_path(mapping, current, notes);
    let mut ids = mapping.keys().cloned().collect::<Vec<_>>();
    ids.sort();
    let mut retained_ids = selected_path.clone();
    retained_ids.truncate(MAX_GRAPH_NODES);
    for id in &ids {
        if retained_ids.len() >= MAX_GRAPH_NODES {
            break;
        }
        if !retained_ids.contains(id) {
            retained_ids.push(id.clone());
        }
    }

    let nodes = retained_ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let value = &mapping[id];
            let message = value
                .get("message")
                .filter(|message| !message.is_null())
                .and_then(|message| mapping_message(message, id, index, notes));
            ConversationNode {
                id: id.clone(),
                parent: value
                    .get("parent")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                message,
            }
        })
        .collect::<Vec<_>>();

    let mut edge_pairs = Vec::new();
    for id in &ids {
        let value = &mapping[id];
        if let Some(parent) = value.get("parent").and_then(Value::as_str) {
            edge_pairs.push((parent.to_owned(), id.clone()));
        }
        if let Some(children) = value.get("children").and_then(Value::as_array) {
            edge_pairs.extend(
                children
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|child| (id.clone(), child.to_owned())),
            );
        }
    }
    edge_pairs.sort();
    edge_pairs.dedup();
    let omitted_edges = edge_pairs.len().saturating_sub(MAX_GRAPH_EDGES);
    let edges = edge_pairs
        .into_iter()
        .take(MAX_GRAPH_EDGES)
        .map(|(parent, child)| ConversationEdge { parent, child })
        .collect();
    let omitted_nodes = mapping.len().saturating_sub(nodes.len());
    if omitted_nodes > 0 {
        notes.push(format!(
            "conversation mapping retained {}/{} nodes; {omitted_nodes} fell outside the graph bound",
            nodes.len(),
            mapping.len()
        ));
    }
    if omitted_edges > 0 {
        notes.push(format!(
            "conversation mapping retained {} edges; {omitted_edges} fell outside the graph bound",
            MAX_GRAPH_EDGES
        ));
    }
    (
        ConversationGraph {
            current_node: current.map(str::to_owned),
            selected_path: selected_path.clone(),
            nodes,
            edges,
            omitted_nodes,
            omitted_edges,
        },
        selected_path,
    )
}

fn canonical_path(
    mapping: &serde_json::Map<String, Value>,
    current: Option<&str>,
    notes: &mut Vec<String>,
) -> Vec<String> {
    let Some(current) = current else {
        notes.push("canonical branch is unknown because current_node is absent".to_owned());
        return Vec::new();
    };
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(current);
    while let Some(node_id) = current {
        if !seen.insert(node_id) {
            notes.push("conversation mapping cycle stopped canonical traversal".to_owned());
            break;
        }
        if ids.len() >= MAX_CANONICAL_PATH {
            notes.push(format!(
                "canonical traversal stopped at path bound {MAX_CANONICAL_PATH}"
            ));
            break;
        }
        let Some(node) = mapping.get(node_id) else {
            notes.push(format!(
                "canonical traversal stopped at a dangling node {node_id}"
            ));
            break;
        };
        ids.push(node_id.to_owned());
        current = node.get("parent").and_then(Value::as_str);
    }
    ids.reverse();
    ids
}

fn mapping_message(
    message: &Value,
    node_id: &str,
    index: usize,
    notes: &mut Vec<String>,
) -> Option<Turn> {
    let Some(role) = role_from_str(
        message
            .get("author")
            .and_then(|author| author.get("role"))
            .and_then(Value::as_str),
    ) else {
        notes.push(format!("node {node_id} has an unknown author role"));
        return None;
    };
    let role = record_role(message, role);
    let (parts, coverage) = message_parts(message);
    let text = content::project_text(&parts);
    if text.is_empty() && parts.is_empty() {
        return None;
    }
    let native_id = message.get("id").and_then(Value::as_str).map(str::to_owned);
    let kind = imported_kind(&role);
    Some(Turn {
        role,
        kind,
        text,
        ts: message.get("create_time").and_then(timestamp_value),
        ordinal: index,
        native_id: native_id.clone(),
        request_turn_id: message
            .get("metadata")
            .and_then(|metadata| metadata.get("turn_id"))
            .and_then(Value::as_str)
            .or_else(|| message.get("turn_id").and_then(Value::as_str))
            .map(str::to_owned),
        metadata: None,
        record_ref: Some(RecordRef {
            domain: "input-pending".to_owned(),
            revision: None,
            span: None,
            native_id,
            part_index: index,
            content_part_index: None,
            pointer: Some(format!("/mapping/{node_id}/message")),
        }),
        channel: message
            .get("channel")
            .and_then(Value::as_str)
            .map(str::to_owned),
        recipient: message
            .get("recipient")
            .and_then(Value::as_str)
            .map(str::to_owned),
        parts,
        coverage: Some(coverage),
        tool: None,
    })
}

fn imported_kind(role: &Role) -> TurnKind {
    if *role == Role::User {
        TurnKind::Operator
    } else {
        role.kind().unwrap_or(TurnKind::Unknown)
    }
}

fn normalize_chatgpt_exporter(value: &Value) -> Result<NormalizedConversation> {
    if value["mapping"].is_object() {
        return normalize_mapping(value);
    }
    let object = value.as_object();
    let explicit_messages = object
        .and_then(|object| object.get("messages").or_else(|| object.get("entries")))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .or_else(|| value.as_array().map(Vec::as_slice));
    let mut synthetic_messages = Vec::new();
    if explicit_messages.is_none() {
        if let Some(object) = object {
            for (key, role) in [
                ("prompt", "user"),
                ("response", "assistant"),
                ("answer", "assistant"),
            ] {
                if let Some(text) = object.get(key).and_then(Value::as_str) {
                    synthetic_messages.push(serde_json::json!({
                        "role": role,
                        "content": text
                    }));
                }
            }
        }
    }
    let messages = explicit_messages
        .or_else(|| (!synthetic_messages.is_empty()).then_some(synthetic_messages.as_slice()));
    let Some(messages) = messages else {
        return Ok(None);
    };
    if messages.is_empty() && object.is_none() {
        return Ok(None);
    }
    let id = object
        .and_then(|object| {
            ["conversation_id", "id", "uuid", "conversationId"]
                .into_iter()
                .find_map(|key| object.get(key).and_then(Value::as_str))
        })
        .map(str::to_owned);
    let title = object
        .and_then(|object| object.get("title").and_then(Value::as_str))
        .filter(|title| !title.is_empty())
        .map(str::to_owned);
    let started_at = object
        .and_then(|object| {
            object
                .get("created_at")
                .or_else(|| object.get("create_time"))
        })
        .and_then(timestamp_value);
    let last_activity_at = object
        .and_then(|object| {
            object
                .get("updated_at")
                .or_else(|| object.get("update_time"))
        })
        .and_then(timestamp_value);
    let directory = object
        .and_then(|object| {
            object
                .get("metadata")
                .and_then(|metadata| metadata.get("cwd").and_then(Value::as_str))
        })
        .map(PathBuf::from);
    let model = object
        .and_then(|object| object.get("model").and_then(Value::as_str))
        .map(|id| Model {
            id: id.to_owned(),
            variant: None,
        });
    let mut turns = Vec::new();
    let mut notes = Vec::new();
    let mut recognized_message = false;
    for (index, message) in messages.iter().enumerate() {
        let role = role_from_str(
            message["role"]
                .as_str()
                .or_else(|| message["author"]["role"].as_str()),
        );
        let Some(role) = role else {
            notes.push(format!("message {index} has an unknown role"));
            continue;
        };
        recognized_message = true;
        let role = record_role(message, role);
        let (parts, coverage) = message_parts(message);
        let text = content::project_text(&parts);
        if text.is_empty() && parts.is_empty() {
            continue;
        }
        let native_id = ["id", "message_id", "uuid"]
            .into_iter()
            .find_map(|key| message[key].as_str())
            .map(str::to_owned);
        let kind = imported_kind(&role);
        turns.push(Turn {
            role,
            kind,
            text,
            ts: ["timestamp", "created_at", "create_time"]
                .into_iter()
                .find_map(|key| timestamp_value(&message[key])),
            ordinal: index,
            native_id: native_id.clone(),
            request_turn_id: message["turn_id"].as_str().map(str::to_owned),
            metadata: None,
            record_ref: Some(RecordRef {
                domain: "input-pending".to_owned(),
                revision: None,
                span: None,
                native_id,
                part_index: index,
                content_part_index: None,
                pointer: Some(format!("/messages/{index}")),
            }),
            channel: message["channel"].as_str().map(str::to_owned),
            recipient: message["recipient"].as_str().map(str::to_owned),
            parts,
            coverage: Some(coverage),
            tool: None,
        });
    }
    if !recognized_message && !messages.is_empty() {
        return Ok(None);
    }
    Ok(Some((
        id,
        title,
        started_at,
        last_activity_at,
        directory,
        model,
        None,
        turns,
        None,
        notes,
    )))
}

fn normalize_perplexity(value: &Value) -> Result<NormalizedConversation> {
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let Some(id) = object.get("context_uuid").and_then(Value::as_str) else {
        return Ok(None);
    };
    let mut notes = Vec::new();
    let started_at = timestamp_with_note(object.get("created_at"), "created_at", &mut notes);
    let last_activity_at = timestamp_with_note(object.get("updated_at"), "updated_at", &mut notes);
    let Some(entries) = object.get("entries").and_then(Value::as_array) else {
        return Ok(None);
    };
    let metadata = SessionMetadata {
        collection: string_field(object.get("collection_uuid")),
        mode: string_field(object.get("mode")),
        engine: string_field(object.get("engine_mode")),
        status: string_field(object.get("query_status")),
        label: string_field(object.get("label")),
    };
    let metadata = [
        metadata.collection.is_some(),
        metadata.mode.is_some(),
        metadata.engine.is_some(),
        metadata.status.is_some(),
        metadata.label.is_some(),
    ]
    .into_iter()
    .any(|present| present)
    .then_some(metadata);
    let mut turns = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let Some(entry) = entry.as_object() else {
            notes.push(format!("entry {index} has a non-object shape"));
            continue;
        };
        let native_id = entry
            .get("entry_uuid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let entry_metadata = perplexity_entry_metadata(entry, index);
        let created_at =
            timestamp_with_note(entry.get("created_at"), "entry.created_at", &mut notes);
        if let Some(query) = entry.get("query") {
            turns.push(perplexity_turn(
                query,
                Role::User,
                TurnKind::Operator,
                "entry.query",
                native_id.clone(),
                created_at,
                format!("/entries/{index}/query"),
                entry_metadata.clone(),
            ));
        }
        if let Some(answer) = entry.get("answer") {
            turns.push(perplexity_turn(
                answer,
                Role::Assistant,
                TurnKind::Assistant,
                "entry.answer",
                native_id,
                None,
                format!("/entries/{index}/answer"),
                entry_metadata,
            ));
        }
    }
    Ok(Some((
        Some(id.to_owned()),
        object
            .get("context_title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .map(str::to_owned),
        started_at,
        last_activity_at,
        None,
        None,
        metadata,
        turns,
        None,
        notes,
    )))
}

#[allow(clippy::too_many_arguments)]
fn perplexity_turn(
    value: &Value,
    role: Role,
    kind: TurnKind,
    source_field: &str,
    native_id: Option<String>,
    ts: Option<DateTime<Utc>>,
    pointer: String,
    metadata: Option<EntryMetadata>,
) -> Turn {
    let (parts, coverage) = match value {
        Value::String(text) => (
            vec![content::text_part(text, source_field, "text")],
            ContentCoverage {
                carrier: ContentCarrier::DirectPart,
                availability: ContentAvailability::RetainedBody,
                retained_parts: 1,
                omitted_parts: 0,
                omitted_reason: None,
            },
        ),
        Value::Null => (
            vec![ContentPart::Unknown {
                native_kind: "null".to_owned(),
                descriptor: content::bounded_shape(value),
                source_field: source_field.to_owned(),
                record_ref: None,
            }],
            ContentCoverage {
                carrier: ContentCarrier::DirectPart,
                availability: ContentAvailability::Unknown,
                retained_parts: 0,
                omitted_parts: 0,
                omitted_reason: Some("source field was null".to_owned()),
            },
        ),
        _ => (
            vec![ContentPart::Unknown {
                native_kind: "non-string".to_owned(),
                descriptor: content::bounded_shape(value),
                source_field: source_field.to_owned(),
                record_ref: None,
            }],
            ContentCoverage {
                carrier: ContentCarrier::DirectPart,
                availability: ContentAvailability::Unknown,
                retained_parts: 0,
                omitted_parts: 0,
                omitted_reason: Some("source field was not a string".to_owned()),
            },
        ),
    };
    let kind = match value {
        Value::String(text) if !text.is_empty() => kind,
        _ => TurnKind::Unknown,
    };
    let text = content::project_text(&parts);
    Turn {
        role: role.clone(),
        kind,
        text,
        ts,
        ordinal: 0,
        native_id: native_id.clone(),
        request_turn_id: None,
        metadata,
        record_ref: Some(RecordRef {
            domain: "input-pending".to_owned(),
            revision: None,
            span: None,
            native_id,
            pointer: Some(pointer),
            part_index: 0,
            content_part_index: None,
        }),
        channel: None,
        recipient: None,
        parts,
        coverage: Some(coverage),
        tool: None,
    }
}

fn string_field(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

fn perplexity_entry_metadata(
    entry: &serde_json::Map<String, Value>,
    index: usize,
) -> Option<EntryMetadata> {
    let mut metadata = EntryMetadata {
        engine: None,
        status: None,
        label: None,
        source_fields: BTreeMap::new(),
    };
    for (normalized, native) in [
        ("engine", "engine_mode"),
        ("status", "query_status"),
        ("label", "label"),
    ] {
        let Some(value) = entry.get(native).and_then(Value::as_str) else {
            continue;
        };
        metadata
            .source_fields
            .insert(normalized.to_owned(), format!("/entries/{index}/{native}"));
        match normalized {
            "engine" => metadata.engine = Some(value.to_owned()),
            "status" => metadata.status = Some(value.to_owned()),
            "label" => metadata.label = Some(value.to_owned()),
            _ => unreachable!(),
        }
    }
    (metadata.engine.is_some() || metadata.status.is_some() || metadata.label.is_some())
        .then_some(metadata)
}

fn timestamp_with_note(
    value: Option<&Value>,
    field: &str,
    notes: &mut Vec<String>,
) -> Option<DateTime<Utc>> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    let parsed = timestamp_value(value);
    if parsed.is_none() {
        notes.push(format!("{field} was present but not a valid timestamp"));
    }
    parsed
}

fn message_parts(message: &Value) -> (Vec<ContentPart>, ContentCoverage) {
    let content_value = message.get("content").unwrap_or(&Value::Null);
    if let Some(text) = content_value.as_str() {
        return (
            vec![content::text_part(text, "message.content", "text")],
            ContentCoverage {
                carrier: ContentCarrier::DirectPart,
                availability: ContentAvailability::RetainedBody,
                retained_parts: 1,
                omitted_parts: 0,
                omitted_reason: None,
            },
        );
    }
    let content_type = content_type(message);
    if let Some(retained) = reasoning_parts(content_value, content_type) {
        return retained;
    }
    let parts_value = content_value
        .get("parts")
        .or_else(|| message.get("parts"))
        .unwrap_or(content_value);
    if parts_value.is_array() {
        return content::parts_from_array(parts_value, "message.content.parts");
    }
    (
        vec![ContentPart::Unknown {
            native_kind: content_type.unwrap_or("unknown").to_owned(),
            descriptor: content::bounded_shape(message),
            source_field: "message.content".to_owned(),
            record_ref: None,
        }],
        ContentCoverage {
            carrier: ContentCarrier::DirectPart,
            availability: ContentAvailability::Unknown,
            retained_parts: 1,
            omitted_parts: 0,
            omitted_reason: Some("content shape was not recognized".to_owned()),
        },
    )
}

/// OpenAI records reasoning as `thoughts` and `reasoning_recap` messages under
/// the assistant author, so the content type rather than the author names
/// the role.
fn record_role(message: &Value, role: Role) -> Role {
    match content_type(message) {
        Some("thoughts" | "reasoning_recap") => Role::Reasoning,
        _ => role,
    }
}

/// A message's content type, named on its content object or on the message.
fn content_type(message: &Value) -> Option<&str> {
    message["content"]["content_type"]
        .as_str()
        .or_else(|| message["content_type"].as_str())
}

/// Reasoning records hold their text outside `parts`: `thoughts` as a list of
/// summary and content pairs, `reasoning_recap` as one `content` string.
fn reasoning_parts(
    content_value: &Value,
    content_type: Option<&str>,
) -> Option<(Vec<ContentPart>, ContentCoverage)> {
    let native_kind = content_type?;
    let fields: Vec<(String, &str)> = match native_kind {
        "thoughts" => content_value["thoughts"]
            .as_array()?
            .iter()
            .enumerate()
            .flat_map(|(index, thought)| {
                ["summary", "content"].into_iter().filter_map(move |field| {
                    thought[field]
                        .as_str()
                        .filter(|text| !text.is_empty())
                        .map(|text| (format!("message.content.thoughts[{index}].{field}"), text))
                })
            })
            .collect(),
        "reasoning_recap" => vec![(
            "message.content.content".to_owned(),
            content_value["content"].as_str()?,
        )],
        _ => return None,
    };
    if fields.is_empty() {
        return None;
    }
    let parts = fields
        .iter()
        .take(content::MAX_CONTENT_PARTS)
        .map(|(field, text)| content::text_part(*text, field, native_kind))
        .collect::<Vec<_>>();
    let omitted_parts = fields.len() - parts.len();
    let coverage = ContentCoverage {
        carrier: ContentCarrier::DirectPart,
        availability: ContentAvailability::RetainedBody,
        retained_parts: parts.len(),
        omitted_parts,
        omitted_reason: (omitted_parts > 0).then(|| "part-count-bound".to_owned()),
    };
    Some((parts, coverage))
}

fn role_from_str(role: Option<&str>) -> Option<Role> {
    match role? {
        "user" | "human" => Some(Role::User),
        "assistant" | "bot" => Some(Role::Assistant),
        "system" => Some(Role::System),
        "developer" => Some(Role::Developer),
        "tool" | "toolResult" => Some(Role::Tool),
        "reasoning" => Some(Role::Reasoning),
        _ => None,
    }
}

fn timestamp_value(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(number) = value.as_f64() {
        let seconds = number.trunc() as i64;
        let nanos = ((number.fract().abs()) * 1_000_000_000.0).round() as u32;
        return DateTime::from_timestamp(seconds, nanos);
    }
    value
        .as_str()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn detect_format(value: &Value) -> Result<Option<InputFormat>> {
    let perplexity = value["conversations"].is_array()
        || (value["context_uuid"].is_string() && value["entries"].is_array());
    let mapping = value["mapping"].is_object();
    let openai_mapping = mapping && value["conversation_id"].is_string();
    let chatgpt_mapping = mapping
        && !openai_mapping
        && (value["id"].is_string() || value["current_node"].is_string());
    if perplexity && mapping {
        bail!("supplied record matches both Perplexity and mapping conversation shapes");
    }
    if openai_mapping {
        Ok(Some(InputFormat::Openai))
    } else if chatgpt_mapping {
        Ok(Some(InputFormat::ChatgptExporter))
    } else if mapping {
        Ok(Some(InputFormat::Openai))
    } else if perplexity {
        Ok(Some(InputFormat::Perplexity))
    } else if value["messages"].is_array()
        || value["entries"].is_array()
        || value["prompt"].is_string()
        || value["response"].is_string()
        || value["answer"].is_string()
        || value.as_array().is_some()
    {
        Ok(Some(InputFormat::ChatgptExporter))
    } else {
        Ok(None)
    }
}

fn discover_directory(root: &Path) -> Result<Vec<DirectoryMember>> {
    let mut members = Vec::new();
    let mut names = HashSet::new();
    discover_directory_inner(root, root, 0, &mut members, &mut names)?;
    members.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(members)
}

fn discover_directory_inner(
    root: &Path,
    directory: &Path,
    depth: usize,
    members: &mut Vec<DirectoryMember>,
    names: &mut HashSet<String>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("supplied input directory exceeds the structural depth bound of {MAX_DEPTH}");
    }
    for entry in fs::read_dir(directory)
        .with_context(|| format!("read input directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "supplied input directory contains a symlink: {}",
                path.display()
            );
        }
        if kind.is_dir() {
            discover_directory_inner(root, &path, depth + 1, members, names)?;
            continue;
        }
        if !kind.is_file() {
            bail!(
                "supplied input directory contains a non-regular member: {}",
                path.display()
            );
        }
        let relative = path
            .strip_prefix(root)
            .with_context(|| format!("locate directory member {}", path.display()))?;
        let relative = relative.to_str().ok_or_else(|| {
            anyhow!(
                "supplied input directory member is not valid UTF-8: {}",
                path.display()
            )
        })?;
        let name = normalize_member_name(relative)?;
        if !names.insert(name.clone()) {
            bail!("supplied input directory has duplicate normalized member {name}");
        }
        if members.len() >= MAX_MEMBERS {
            bail!(
                "supplied input directory has more than {MAX_MEMBERS} members; discovery stopped before reading bodies"
            );
        }
        let metadata = fs::metadata(&path)?;
        members.push(DirectoryMember {
            name,
            path,
            size: metadata.len(),
            revision: metadata_revision(&metadata),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_directory(
    path: &Path,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    associated_reports: &mut Vec<AssociatedReport>,
    observation_parts: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
) -> Result<()> {
    let members = discover_directory(path)?;
    budget.add_members(members.len())?;
    for member in &members {
        if !budget.reserve_resident(
            member.name.len() as u64 + member.path.to_string_lossy().len() as u64 + 64,
        ) {
            bail!("supplied input directory member index exceeds the resident-byte budget");
        }
        observation_parts.push(format!(
            "file:{}:{}:{}:{}",
            member.name,
            member.path.display(),
            member.size,
            member.revision
        ));
    }
    let manifest = members
        .iter()
        .find(|member| member.name == "export_manifest.json");
    let Some(manifest_member) = manifest else {
        for member in members {
            if is_json_name(&member.name) {
                scan_file(
                    &member.path,
                    None,
                    options,
                    budget,
                    occurrences,
                    diagnostics,
                    scanned,
                    scan_truncated,
                    discovery_incomplete,
                    collection_gaps,
                )?;
            } else if is_report_name(&member.name) {
                scan_directory_report(
                    &member,
                    options,
                    budget,
                    associated_reports,
                    diagnostics,
                    collection_gaps,
                    scan_truncated,
                    discovery_incomplete,
                    None,
                )?;
            }
            if budget.exhausted() {
                *scan_truncated = true;
                diagnostics.push("input scan or decoded-byte budget exhausted".to_owned());
                break;
            }
        }
        return Ok(());
    };

    let manifest_value = read_directory_json(
        manifest_member,
        options,
        budget,
        diagnostics,
        collection_gaps,
    )?
    .ok_or_else(|| anyhow!("native OpenAI export manifest could not be read"))?;
    let selection = parse_manifest(&manifest_value)?;
    scan_manifest_directory(
        path,
        &members,
        &selection,
        options,
        budget,
        occurrences,
        diagnostics,
        associated_reports,
        collection_gaps,
        scanned,
        scan_truncated,
        discovery_incomplete,
    )
}

fn is_json_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == "json" || extension == "jsonl")
}

fn is_report_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == "dat")
}

fn is_zip_path(path: &Path, budget: &Budget) -> Result<bool> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        return Ok(true);
    }
    let mut file = File::open(path).with_context(|| format!("open input {}", path.display()))?;
    let mut magic = [0; 4];
    let read = file.read(&mut magic)?;
    budget.charge_scan(read as u64)?;
    Ok(
        read == 4
            && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06" || magic == *b"PK\x07\x08"),
    )
}

#[allow(clippy::too_many_arguments)]
fn scan_file(
    path: &Path,
    member: Option<&str>,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("open input {}", path.display()))?;
    let metadata = file.metadata()?;
    let revision = metadata_revision(&metadata);
    let locator = path.display().to_string();
    let first_occurrence = occurrences.len();
    let diagnostic_start = diagnostics.len();
    let mut gaps = Vec::new();
    let mut stopped = false;
    let result = scan_reader(
        BudgetedSourceReader::new(file, budget),
        &locator,
        member,
        metadata.len(),
        revision,
        options,
        budget,
        occurrences,
        diagnostics,
        scanned,
        &mut gaps,
        &mut stopped,
    );
    match result {
        Err(error) if budget.exhausted() => {
            *scan_truncated = true;
            diagnostics.push(format!("{locator}: input scan budget exhausted: {error}"));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: metadata.len(),
                },
                reason: "input-budget".to_owned(),
            });
        }
        Err(error) => return Err(error),
        Ok(()) => {}
    }
    if stopped {
        *scan_truncated = true;
    }
    if !gaps.is_empty() || stopped {
        *discovery_incomplete = true;
    }
    for gap in &gaps {
        if !collection_gaps.contains(gap) {
            collection_gaps.push(gap.clone());
        }
    }
    for occurrence in occurrences.iter_mut().skip(first_occurrence) {
        occurrence.evidence.gaps.extend(gaps.iter().cloned());
        occurrence
            .notes
            .extend(diagnostics[diagnostic_start..].iter().cloned());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_directory_report(
    member: &DirectoryMember,
    options: &InputOptions,
    budget: &Budget,
    associated_reports: &mut Vec<AssociatedReport>,
    diagnostics: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
    association: Option<&LibraryAssociation>,
) -> Result<()> {
    let file = File::open(&member.path)
        .with_context(|| format!("open associated report {}", member.path.display()))?;
    let mut reader = BudgetedSourceReader::new(file, budget);
    let report = read_associated_report(
        &mut reader,
        &member.name,
        member.size,
        options,
        budget,
        diagnostics,
        association,
        collection_gaps,
    );
    match report {
        Ok(Some(report)) => associated_reports.push(report),
        Ok(None) => {}
        Err(error) if budget.exhausted() => {
            *scan_truncated = true;
            *discovery_incomplete = true;
            diagnostics.push(format!(
                "{}: input scan or decoded-byte budget exhausted: {error}",
                member.name
            ));
            collection_gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: input budget", member.name),
            });
        }
        Err(error) => {
            *discovery_incomplete = true;
            diagnostics.push(format!("{}: unreadable member: {error:#}", member.name));
            collection_gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: unreadable member", member.name),
            });
        }
    }
    Ok(())
}

fn read_directory_json(
    member: &DirectoryMember,
    options: &InputOptions,
    budget: &Budget,
    diagnostics: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
) -> Result<Option<Value>> {
    let file = File::open(&member.path)
        .with_context(|| format!("open native export manifest {}", member.path.display()))?;
    let mut reader = BudgetedSourceReader::new(file, budget);
    let Some(bytes) = read_member_bytes(
        &mut reader,
        &member.name,
        member.size,
        options,
        budget,
        diagnostics,
        collection_gaps,
        MAX_MANIFEST_BYTES,
        false,
    )?
    else {
        return Ok(None);
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(Some(value)),
        Err(error) => bail!("native OpenAI export manifest is malformed: {error}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_manifest_directory(
    _root: &Path,
    members: &[DirectoryMember],
    selection: &ManifestSelection,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    associated_reports: &mut Vec<AssociatedReport>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
) -> Result<()> {
    let mut associations = HashMap::new();
    for name in &selection.library_metadata_members {
        let Some(member) = members.iter().find(|member| member.name == *name) else {
            manifest_gap(
                name,
                "declared library metadata member is missing",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        };
        if !manifest_member_is_readable(member.size, selection, name, diagnostics, collection_gaps)
        {
            *discovery_incomplete = true;
            *scan_truncated = true;
            continue;
        }
        let Some(value) =
            read_directory_value(member, options, budget, diagnostics, collection_gaps)?
        else {
            *discovery_incomplete = true;
            *scan_truncated = true;
            continue;
        };
        associations.extend(parse_library_associations(
            &value,
            name,
            diagnostics,
            collection_gaps,
        ));
    }
    for name in &selection.conversation_members {
        let Some(member) = members.iter().find(|member| member.name == *name) else {
            manifest_gap(
                name,
                "declared conversation member is missing",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        };
        if !manifest_member_is_readable(member.size, selection, name, diagnostics, collection_gaps)
        {
            *discovery_incomplete = true;
            *scan_truncated = true;
            continue;
        }
        scan_file(
            &member.path,
            Some(&member.name),
            options,
            budget,
            occurrences,
            diagnostics,
            scanned,
            scan_truncated,
            discovery_incomplete,
            collection_gaps,
        )?;
        if budget.exhausted() {
            *scan_truncated = true;
            break;
        }
    }
    for name in &selection.library_content_members {
        let Some(member) = members.iter().find(|member| member.name == *name) else {
            manifest_gap(
                name,
                "declared associated-library member is missing",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        };
        if !manifest_member_is_readable(member.size, selection, name, diagnostics, collection_gaps)
        {
            *discovery_incomplete = true;
            *scan_truncated = true;
            continue;
        }
        let association = library_association_for(&associations, name);
        scan_directory_report(
            member,
            options,
            budget,
            associated_reports,
            diagnostics,
            collection_gaps,
            scan_truncated,
            discovery_incomplete,
            association.as_ref(),
        )?;
        if budget.exhausted() {
            *scan_truncated = true;
            break;
        }
    }
    Ok(())
}

fn read_directory_value(
    member: &DirectoryMember,
    options: &InputOptions,
    budget: &Budget,
    diagnostics: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
) -> Result<Option<Value>> {
    let file = File::open(&member.path)
        .with_context(|| format!("open library metadata {}", member.path.display()))?;
    let mut reader = BudgetedSourceReader::new(file, budget);
    let Some(bytes) = read_member_bytes(
        &mut reader,
        &member.name,
        member.size,
        options,
        budget,
        diagnostics,
        collection_gaps,
        options.record_bytes,
        false,
    )?
    else {
        return Ok(None);
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            diagnostics.push(format!(
                "{}: malformed library metadata: {error}",
                member.name
            ));
            collection_gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: malformed library metadata", member.name),
            });
            Ok(None)
        }
    }
}

fn manifest_member_is_readable(
    actual_size: u64,
    selection: &ManifestSelection,
    name: &str,
    diagnostics: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
) -> bool {
    let Some(expected_size) = selection.expected_sizes.get(name) else {
        manifest_gap_without_flags(
            name,
            "declared member has no export_files size",
            diagnostics,
            collection_gaps,
        );
        return false;
    };
    if *expected_size != actual_size {
        manifest_gap_without_flags(
            name,
            &format!("size mismatch: manifest={expected_size}, actual={actual_size}"),
            diagnostics,
            collection_gaps,
        );
        return false;
    }
    true
}

fn manifest_gap(
    name: &str,
    detail: &str,
    diagnostics: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
) {
    manifest_gap_without_flags(name, detail, diagnostics, collection_gaps);
    *scan_truncated = true;
    *discovery_incomplete = true;
}

fn manifest_gap_without_flags(
    name: &str,
    detail: &str,
    diagnostics: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
) {
    diagnostics.push(format!("manifest member {name}: {detail}"));
    collection_gaps.push(crate::model::ReadGap {
        span: ByteSpan { start: 0, end: 0 },
        reason: format!("manifest member {name}: {detail}"),
    });
}

#[allow(clippy::too_many_arguments)]
fn read_member_bytes<R: Read>(
    reader: &mut R,
    name: &str,
    size: u64,
    options: &InputOptions,
    budget: &Budget,
    diagnostics: &mut Vec<String>,
    gaps: &mut Vec<crate::model::ReadGap>,
    maximum: u64,
    verify_end: bool,
) -> Result<Option<Vec<u8>>> {
    if size > maximum {
        diagnostics.push(format!(
            "{name}: member size {size} is above the bounded read size {maximum}"
        ));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan {
                start: 0,
                end: size,
            },
            reason: "member-size-bound".to_owned(),
        });
        return Ok(None);
    }
    if size > options.record_bytes {
        diagnostics.push(format!("{name}: member exceeds --record-bytes"));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan {
                start: 0,
                end: size,
            },
            reason: "record-bytes-bound".to_owned(),
        });
        return Ok(None);
    }
    if size > budget.remaining_decoded() {
        diagnostics.push(format!(
            "{name}: member exceeds remaining decoded-byte budget"
        ));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan {
                start: 0,
                end: size,
            },
            reason: "decoded-byte-bound".to_owned(),
        });
        return Ok(None);
    }
    if !budget.reserve_resident(size) {
        diagnostics.push(format!(
            "{name}: member exceeds remaining resident-byte budget"
        ));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan {
                start: 0,
                end: size,
            },
            reason: "resident-byte-bound".to_owned(),
        });
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(size as usize);
    let mut buffer = [0; 64 * 1024];
    while bytes.len() < size as usize {
        let remaining = size as usize - bytes.len();
        let read_size = remaining.min(buffer.len());
        let read = reader.read(&mut buffer[..read_size])?;
        if read == 0 {
            diagnostics.push(format!(
                "{name}: member ended before its declared uncompressed size"
            ));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: bytes.len() as u64,
                    end: size,
                },
                reason: "member-ended-early".to_owned(),
            });
            return Ok(None);
        }
        budget.charge_decoded(read as u64)?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    if verify_end {
        let mut end = [0; 1];
        let read = reader.read(&mut end)?;
        if read > 0 {
            budget.charge_decoded(read as u64)?;
        }
    }
    Ok(Some(bytes))
}

fn parse_manifest(value: &Value) -> Result<ManifestSelection> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("native OpenAI export manifest must be an object"))?;
    if object.get("version").and_then(Value::as_u64) != Some(1) {
        bail!("native OpenAI export manifest version is unsupported");
    }
    let logical_files = object
        .get("logical_files")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("native OpenAI export manifest has no logical_files object"))?;
    let conversation_members = logical_members(logical_files, "conversations.json", true)?;
    let library_metadata_members = logical_members(logical_files, "library_files.json", false)?;
    let mut library_content_members = Vec::new();
    for (logical_name, entry) in logical_files {
        if logical_name.ends_with(".dat") {
            library_content_members.extend(logical_entry_members(entry, logical_name)?);
        }
    }
    let mut selected = HashSet::new();
    for name in conversation_members
        .iter()
        .chain(library_metadata_members.iter())
        .chain(library_content_members.iter())
    {
        if !selected.insert(name.clone()) {
            bail!("native OpenAI export manifest selects duplicate member {name}");
        }
    }
    let export_files = object
        .get("export_files")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("native OpenAI export manifest has no export_files array"))?;
    let mut expected_sizes = HashMap::new();
    for entry in export_files {
        let entry = entry.as_object().ok_or_else(|| {
            anyhow!("native OpenAI export manifest has a non-object export_files entry")
        })?;
        let path = entry.get("path").and_then(Value::as_str).ok_or_else(|| {
            anyhow!("native OpenAI export manifest export_files entry has no path")
        })?;
        let path = normalize_member_name(path)?;
        let size = entry
            .get("size_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow!("native OpenAI export manifest size_bytes is not an unsigned integer")
            })?;
        if expected_sizes.insert(path.clone(), size).is_some() {
            bail!("native OpenAI export manifest has duplicate export_files path {path}");
        }
    }
    Ok(ManifestSelection {
        conversation_members,
        library_metadata_members,
        library_content_members,
        expected_sizes,
    })
}

fn logical_members(
    logical_files: &serde_json::Map<String, Value>,
    logical_name: &str,
    required: bool,
) -> Result<Vec<String>> {
    let Some(entry) = logical_files.get(logical_name) else {
        if required {
            bail!("native OpenAI export manifest has no {logical_name} logical file");
        }
        return Ok(Vec::new());
    };
    logical_entry_members(entry, logical_name)
}

fn logical_entry_members(value: &Value, logical_name: &str) -> Result<Vec<String>> {
    let object = value.as_object().ok_or_else(|| {
        anyhow!("native OpenAI manifest logical file {logical_name} is not an object")
    })?;
    let files = object
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!("native OpenAI manifest logical file {logical_name} has no files array")
        })?;
    let mut names = Vec::with_capacity(files.len());
    let mut seen = HashSet::new();
    for file in files {
        let file = file.as_str().ok_or_else(|| {
            anyhow!("native OpenAI manifest logical file {logical_name} has a non-string member")
        })?;
        let file = normalize_member_name(file)?;
        if !seen.insert(file.clone()) {
            bail!("native OpenAI manifest logical file {logical_name} selects duplicate member {file}");
        }
        names.push(file);
    }
    let sharded = object.get("sharded").and_then(Value::as_bool);
    if let Some(sharded) = sharded {
        if sharded != (names.len() > 1) {
            bail!("native OpenAI manifest logical file {logical_name} has inconsistent sharded metadata");
        }
    }
    if let Some(shard_count) = object.get("shard_count").and_then(Value::as_u64) {
        if shard_count != names.len() as u64 {
            bail!(
                "native OpenAI manifest logical file {logical_name} has inconsistent shard_count"
            );
        }
    }
    if names.is_empty() {
        bail!("native OpenAI manifest logical file {logical_name} selects no members");
    }
    Ok(names)
}

fn normalize_member_name(name: &str) -> Result<String> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') || name.starts_with('/') {
        bail!("supplied archive member name is not a relative path: {name:?}");
    }
    let mut components = Vec::new();
    for component in name.split('/') {
        match component {
            "" | "." => {}
            ".." => bail!("supplied archive member escapes its container: {name}"),
            component => components.push(component),
        }
    }
    if components.is_empty() {
        bail!("supplied archive member name is empty after normalization");
    }
    let normalized = components.join("/");
    if normalized.len() > MAX_MEMBER_NAME_BYTES {
        bail!("supplied archive member name exceeds {MAX_MEMBER_NAME_BYTES} bytes");
    }
    Ok(normalized)
}

fn parse_library_associations(
    value: &Value,
    member: &str,
    diagnostics: &mut Vec<String>,
    gaps: &mut Vec<crate::model::ReadGap>,
) -> HashMap<String, LibraryAssociation> {
    let Some(entries) = value.as_array() else {
        diagnostics.push(format!("{member}: library metadata is not an array"));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan { start: 0, end: 0 },
            reason: format!("{member}: library metadata is not an array"),
        });
        return HashMap::new();
    };
    let mut associations = HashMap::new();
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            diagnostics.push(format!(
                "{member}: library metadata contains a non-object entry"
            ));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan { start: 0, end: 0 },
                reason: format!("{member}: non-object library metadata entry"),
            });
            continue;
        };
        let file_id = entry
            .get("file_id")
            .and_then(Value::as_str)
            .or_else(|| entry["id"]["id"].as_str())
            .or_else(|| entry.get("artifact_id").and_then(Value::as_str))
            .map(str::to_owned);
        let Some(file_id) = file_id else {
            continue;
        };
        let string = |field: &str| entry.get(field).and_then(Value::as_str).map(str::to_owned);
        let association = LibraryAssociation {
            file_id: Some(file_id.clone()),
            backing: string("backing_conversation_id"),
            originating: string("origination_thread_id")
                .or_else(|| string("initiating_conversation_id")),
            origin_message: string("origination_message_id"),
        };
        associations.insert(file_id, association);
    }
    associations
}

fn library_association_for(
    associations: &HashMap<String, LibraryAssociation>,
    member: &str,
) -> Option<LibraryAssociation> {
    let basename = member.rsplit('/').next().unwrap_or(member);
    // The official export's `file_id` is the member stem: `file_<hex>` names
    // `file_<hex>.dat`.
    associations
        .get(member)
        .or_else(|| associations.get(basename))
        .or_else(|| {
            basename
                .strip_suffix(".dat")
                .and_then(|stem| associations.get(stem))
        })
        .cloned()
}

#[allow(clippy::too_many_arguments)]
fn scan_zip(
    path: &Path,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    associated_reports: &mut Vec<AssociatedReport>,
    observation_parts: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
) -> Result<()> {
    let metadata = fs::metadata(path)?;
    let members = preflight_zip(path, budget)?;
    budget.add_members(members.len())?;
    for member in &members {
        if !budget.reserve_resident(member.name.len() as u64 + 64) {
            bail!("supplied ZIP member index exceeds the resident-byte budget");
        }
        observation_parts.push(format!(
            "zip:{}:{}:{}:{}:{}:{}",
            path.display(),
            metadata_revision(&metadata),
            member.name,
            member.compressed_size,
            member.size,
            member.crc32
        ));
    }
    let file =
        File::open(path).with_context(|| format!("open input archive {}", path.display()))?;
    let mut archive = ZipArchive::new(BudgetedSourceReader::new(file, budget))
        .context("read supplied ZIP central directory")?;
    if archive.len() != members.len() {
        bail!("supplied ZIP central directory changed during inspection");
    }
    for (expected, actual) in members.iter().zip(archive.file_names()) {
        if normalize_member_name(actual)? != expected.name {
            bail!("supplied ZIP member names changed during inspection");
        }
    }
    let manifest_index = members
        .iter()
        .find(|member| member.name == "export_manifest.json" && !member.is_dir)
        .map(|member| member.index);
    if let Some(index) = manifest_index {
        let manifest_meta = &members[index];
        let value = read_zip_value(
            &mut archive,
            manifest_meta,
            options,
            budget,
            diagnostics,
            collection_gaps,
            MAX_MANIFEST_BYTES,
        )?
        .ok_or_else(|| anyhow!("native OpenAI export manifest could not be read"))?;
        let selection = parse_manifest(&value)?;
        scan_manifest_zip(
            path,
            &members,
            &mut archive,
            &selection,
            options,
            budget,
            occurrences,
            diagnostics,
            associated_reports,
            collection_gaps,
            scanned,
            scan_truncated,
            discovery_incomplete,
        )?;
    } else {
        scan_raw_zip(
            path,
            &members,
            &mut archive,
            options,
            budget,
            occurrences,
            diagnostics,
            associated_reports,
            collection_gaps,
            scanned,
            scan_truncated,
            discovery_incomplete,
        )?;
    }
    Ok(())
}

fn preflight_zip(path: &Path, budget: &Budget) -> Result<Vec<ZipMemberMetadata>> {
    let file =
        File::open(path).with_context(|| format!("open input archive {}", path.display()))?;
    let metadata = file.metadata()?;
    let file_length = metadata.len();
    let mut reader = BudgetedSourceReader::new(file, budget);
    let tail_length = file_length.min(MAX_ZIP_EOCD_SEARCH_BYTES) as usize;
    if tail_length < 22 {
        bail!("supplied ZIP is too small to contain an end-of-central-directory record");
    }
    reader.seek(SeekFrom::Start(file_length - tail_length as u64))?;
    let mut tail = vec![0; tail_length];
    reader.read_exact(&mut tail)?;
    let eocd_offset = (0..=tail_length - 22)
        .rev()
        .find_map(|offset| {
            if &tail[offset..offset + 4] != b"PK\x05\x06" {
                return None;
            }
            let comment_length = le_u16(&tail[offset + 20..offset + 22])? as usize;
            (offset.checked_add(22)?.checked_add(comment_length)? == tail_length).then_some(offset)
        })
        .ok_or_else(|| anyhow!("supplied ZIP has no valid end-of-central-directory record"))?;
    let eocd_absolute = file_length - tail_length as u64 + eocd_offset as u64;
    let disk = le_u16(&tail[eocd_offset + 4..eocd_offset + 6]).unwrap_or_default();
    let central_disk = le_u16(&tail[eocd_offset + 6..eocd_offset + 8]).unwrap_or_default();
    let entries_on_disk = le_u16(&tail[eocd_offset + 8..eocd_offset + 10]).unwrap_or_default();
    let entries_total = le_u16(&tail[eocd_offset + 10..eocd_offset + 12]).unwrap_or_default();
    let central_size_32 = le_u32(&tail[eocd_offset + 12..eocd_offset + 16]).unwrap_or_default();
    let central_offset_32 = le_u32(&tail[eocd_offset + 16..eocd_offset + 20]).unwrap_or_default();
    if disk != 0 || central_disk != 0 || entries_on_disk != entries_total {
        bail!("multi-disk supplied ZIP archives are unsupported");
    }
    let needs_zip64 =
        entries_total == u16::MAX || central_size_32 == u32::MAX || central_offset_32 == u32::MAX;
    let (entry_count, central_size, central_end) = if needs_zip64 {
        if eocd_absolute < 20 {
            bail!("supplied ZIP has ZIP64 markers without a ZIP64 locator");
        }
        reader.seek(SeekFrom::Start(eocd_absolute - 20))?;
        let mut locator = [0; 20];
        reader.read_exact(&mut locator)?;
        if &locator[..4] != b"PK\x06\x07" {
            bail!("supplied ZIP has ZIP64 markers without a ZIP64 locator");
        }
        let zip64_offset = le_u64(&locator[8..16])
            .ok_or_else(|| anyhow!("supplied ZIP has an invalid ZIP64 locator"))?;
        reader.seek(SeekFrom::Start(zip64_offset))?;
        let mut header = [0; 56];
        reader.read_exact(&mut header)?;
        if &header[..4] != b"PK\x06\x06" {
            bail!("supplied ZIP has an invalid ZIP64 end-of-central-directory record");
        }
        let record_size = le_u64(&header[4..12]).unwrap_or_default();
        if record_size < 44 {
            bail!("supplied ZIP has a short ZIP64 end-of-central-directory record");
        }
        let disk = le_u32(&header[16..20]).unwrap_or_default();
        let central_disk = le_u32(&header[20..24]).unwrap_or_default();
        let entries_on_disk = le_u64(&header[24..32]).unwrap_or_default();
        let entries_total = le_u64(&header[32..40]).unwrap_or_default();
        if disk != 0 || central_disk != 0 || entries_on_disk != entries_total {
            bail!("multi-disk supplied ZIP archives are unsupported");
        }
        (
            entries_total,
            le_u64(&header[40..48]).unwrap_or_default(),
            zip64_offset,
        )
    } else {
        (entries_total as u64, central_size_32 as u64, eocd_absolute)
    };
    if entry_count > MAX_MEMBERS as u64 {
        bail!("supplied ZIP has {entry_count} members, above the {MAX_MEMBERS} limit");
    }
    if central_size > MAX_ZIP_CENTRAL_DIRECTORY_BYTES {
        bail!(
            "supplied ZIP central directory is {central_size} bytes, above the {MAX_ZIP_CENTRAL_DIRECTORY_BYTES} limit"
        );
    }
    let central_start = central_end
        .checked_sub(central_size)
        .ok_or_else(|| anyhow!("supplied ZIP central directory precedes the archive"))?;
    if central_end > file_length || central_start > file_length {
        bail!("supplied ZIP central directory lies outside the archive");
    }
    reader.seek(SeekFrom::Start(central_start))?;
    let mut members = Vec::with_capacity(entry_count as usize);
    let mut names = HashSet::new();
    for index in 0..entry_count as usize {
        let mut header = [0; 46];
        reader.read_exact(&mut header)?;
        if &header[..4] != b"PK\x01\x02" {
            bail!("supplied ZIP central directory has an invalid member header");
        }
        let version_made = le_u16(&header[4..6]).unwrap_or_default();
        let compressed_size = le_u32(&header[20..24]).unwrap_or_default();
        let size = le_u32(&header[24..28]).unwrap_or_default();
        let name_length = le_u16(&header[28..30]).unwrap_or_default() as usize;
        let extra_length = le_u16(&header[30..32]).unwrap_or_default() as usize;
        let comment_length = le_u16(&header[32..34]).unwrap_or_default() as usize;
        let crc32 = le_u32(&header[16..20]).unwrap_or_default();
        let external_attributes = le_u32(&header[38..42]).unwrap_or_default();
        if name_length > MAX_MEMBER_NAME_BYTES {
            bail!("supplied ZIP member name exceeds {MAX_MEMBER_NAME_BYTES} bytes");
        }
        if compressed_size == u32::MAX || size == u32::MAX {
            bail!("ZIP64 member size metadata is unsupported by bounded input reading");
        }
        let mut raw_name = vec![0; name_length];
        reader.read_exact(&mut raw_name)?;
        let raw_name =
            String::from_utf8(raw_name).context("supplied ZIP member name is not valid UTF-8")?;
        let name = normalize_member_name(&raw_name)?;
        if !names.insert(name.clone()) {
            bail!("supplied ZIP has duplicate normalized member {name}");
        }
        discard_bytes(&mut reader, extra_length.saturating_add(comment_length))?;
        let mode = if version_made >> 8 == 3 {
            external_attributes >> 16
        } else {
            0
        };
        let is_symlink = mode & 0o170000 == 0o120000;
        if is_symlink {
            bail!("supplied ZIP contains symlink member {name}");
        }
        let is_dir = raw_name.ends_with('/') || mode & 0o170000 == 0o040000;
        let entry_end = reader.stream_position()?;
        if entry_end > central_end {
            bail!("supplied ZIP central directory member exceeds its declared size");
        }
        members.push(ZipMemberMetadata {
            name,
            index,
            compressed_size: compressed_size as u64,
            size: size as u64,
            crc32,
            is_dir,
            is_symlink,
        });
    }
    if reader.stream_position()? != central_end {
        bail!("supplied ZIP central directory size does not match its member headers");
    }
    Ok(members)
}

fn discard_bytes<R: Read>(reader: &mut R, mut bytes: usize) -> io::Result<()> {
    let mut buffer = [0; 64 * 1024];
    while bytes > 0 {
        let read_size = bytes.min(buffer.len());
        let read = reader.read(&mut buffer[..read_size])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ZIP central directory ended before its declared member fields",
            ));
        }
        bytes -= read;
    }
    Ok(())
}

fn le_u16(value: &[u8]) -> Option<u16> {
    (value.len() >= 2).then(|| u16::from_le_bytes([value[0], value[1]]))
}

fn le_u32(value: &[u8]) -> Option<u32> {
    (value.len() >= 4).then(|| u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn le_u64(value: &[u8]) -> Option<u64> {
    (value.len() >= 8).then(|| {
        u64::from_le_bytes([
            value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
        ])
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_manifest_zip<R: Read + Seek>(
    path: &Path,
    members: &[ZipMemberMetadata],
    archive: &mut ZipArchive<BudgetedSourceReader<'_, R>>,
    selection: &ManifestSelection,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    associated_reports: &mut Vec<AssociatedReport>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
) -> Result<()> {
    let mut associations = HashMap::new();
    for name in &selection.library_metadata_members {
        let Some(member) = members.iter().find(|member| member.name == *name) else {
            manifest_gap(
                name,
                "declared library metadata member is missing",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        };
        if member.is_dir || member.is_symlink {
            manifest_gap(
                name,
                "declared library metadata member is not a regular file",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        }
        if !manifest_member_is_readable(member.size, selection, name, diagnostics, collection_gaps)
        {
            *discovery_incomplete = true;
            *scan_truncated = true;
            continue;
        }
        let value = match read_zip_value(
            archive,
            member,
            options,
            budget,
            diagnostics,
            collection_gaps,
            options.record_bytes,
        ) {
            Ok(Some(value)) => value,
            Ok(None) => {
                *discovery_incomplete = true;
                *scan_truncated = true;
                continue;
            }
            Err(error) => {
                manifest_gap(
                    name,
                    &format!("library metadata is unreadable: {error:#}"),
                    diagnostics,
                    collection_gaps,
                    scan_truncated,
                    discovery_incomplete,
                );
                continue;
            }
        };
        associations.extend(parse_library_associations(
            &value,
            name,
            diagnostics,
            collection_gaps,
        ));
    }
    for name in &selection.conversation_members {
        let Some(member) = members.iter().find(|member| member.name == *name) else {
            manifest_gap(
                name,
                "declared conversation member is missing",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        };
        if member.is_dir || member.is_symlink {
            manifest_gap(
                name,
                "declared conversation member is not a regular file",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        }
        if !manifest_member_is_readable(member.size, selection, name, diagnostics, collection_gaps)
        {
            *discovery_incomplete = true;
            *scan_truncated = true;
            continue;
        }
        scan_zip_conversation(
            path,
            member,
            archive,
            options,
            budget,
            occurrences,
            diagnostics,
            scanned,
            scan_truncated,
            discovery_incomplete,
            collection_gaps,
        )?;
        if budget.exhausted() {
            *scan_truncated = true;
            break;
        }
    }
    for name in &selection.library_content_members {
        let Some(member) = members.iter().find(|member| member.name == *name) else {
            manifest_gap(
                name,
                "declared associated-library member is missing",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        };
        if member.is_dir || member.is_symlink {
            manifest_gap(
                name,
                "declared associated-library member is not a regular file",
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
            );
            continue;
        }
        if !manifest_member_is_readable(member.size, selection, name, diagnostics, collection_gaps)
        {
            *discovery_incomplete = true;
            *scan_truncated = true;
            continue;
        }
        let association = library_association_for(&associations, name);
        scan_zip_report(
            member,
            archive,
            options,
            budget,
            associated_reports,
            diagnostics,
            collection_gaps,
            scan_truncated,
            discovery_incomplete,
            association.as_ref(),
        )?;
        if budget.exhausted() {
            *scan_truncated = true;
            break;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_raw_zip<R: Read + Seek>(
    path: &Path,
    members: &[ZipMemberMetadata],
    archive: &mut ZipArchive<BudgetedSourceReader<'_, R>>,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    associated_reports: &mut Vec<AssociatedReport>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
) -> Result<()> {
    for member in members {
        if member.is_dir || member.is_symlink {
            continue;
        }
        if is_json_name(&member.name) {
            scan_zip_conversation(
                path,
                member,
                archive,
                options,
                budget,
                occurrences,
                diagnostics,
                scanned,
                scan_truncated,
                discovery_incomplete,
                collection_gaps,
            )?;
        } else if is_report_name(&member.name) {
            scan_zip_report(
                member,
                archive,
                options,
                budget,
                associated_reports,
                diagnostics,
                collection_gaps,
                scan_truncated,
                discovery_incomplete,
                None,
            )?;
        }
        if budget.exhausted() {
            *scan_truncated = true;
            break;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_zip_conversation<R: Read + Seek>(
    path: &Path,
    member: &ZipMemberMetadata,
    archive: &mut ZipArchive<BudgetedSourceReader<'_, R>>,
    options: &InputOptions,
    budget: &Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
) -> Result<()> {
    let first_occurrence = occurrences.len();
    let diagnostic_start = diagnostics.len();
    let mut gaps = Vec::new();
    let mut stopped = false;
    let revision = format!(
        "zip-member:{}:{}:{}:{}",
        member.name, member.size, member.compressed_size, member.crc32
    );
    let mut member_file = match archive.by_index(member.index) {
        Ok(member_file) => member_file,
        Err(error) => {
            diagnostics.push(format!("{}: unreadable member: {error:#}", member.name));
            let gap = crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: unreadable member", member.name),
            };
            if !collection_gaps.contains(&gap) {
                collection_gaps.push(gap);
            }
            *discovery_incomplete = true;
            *scan_truncated = true;
            return Ok(());
        }
    };
    let result = scan_reader(
        &mut member_file,
        &path.display().to_string(),
        Some(&member.name),
        member.size,
        revision,
        options,
        budget,
        occurrences,
        diagnostics,
        scanned,
        &mut gaps,
        &mut stopped,
    );
    let mut failed_verification = false;
    if let Err(error) = result {
        if budget.exhausted() {
            *scan_truncated = true;
            diagnostics.push(format!(
                "{}: input byte budget exhausted: {error}",
                member.name
            ));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: "input-budget".to_owned(),
            });
        } else {
            failed_verification = true;
            diagnostics.push(format!("{}: unreadable member: {error:#}", member.name));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: unreadable member", member.name),
            });
        }
    }
    if let Err(error) = drain_zip_member(&mut member_file, budget) {
        failed_verification |= !budget.exhausted();
        diagnostics.push(format!("{}: unreadable member tail: {error}", member.name));
        gaps.push(crate::model::ReadGap {
            span: ByteSpan {
                start: 0,
                end: member.size,
            },
            reason: format!("{}: corrupt member", member.name),
        });
    }
    // Decompression and checksum errors mean the bytes already parsed are not
    // the archived bytes, so nothing parsed from them is evidence.
    if failed_verification && occurrences.len() > first_occurrence {
        diagnostics.push(format!(
            "{}: withheld {} record(s) because the member failed verification",
            member.name,
            occurrences.len() - first_occurrence
        ));
        occurrences.truncate(first_occurrence);
        *scan_truncated = true;
    }
    if stopped {
        *scan_truncated = true;
    }
    if !gaps.is_empty() || stopped {
        *discovery_incomplete = true;
    }
    for gap in &gaps {
        if !collection_gaps.contains(gap) {
            collection_gaps.push(gap.clone());
        }
    }
    for occurrence in occurrences.iter_mut().skip(first_occurrence) {
        occurrence.evidence.gaps.extend(gaps.iter().cloned());
        occurrence
            .notes
            .extend(diagnostics[diagnostic_start..].iter().cloned());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn scan_zip_report<R: Read + Seek>(
    member: &ZipMemberMetadata,
    archive: &mut ZipArchive<BudgetedSourceReader<'_, R>>,
    options: &InputOptions,
    budget: &Budget,
    associated_reports: &mut Vec<AssociatedReport>,
    diagnostics: &mut Vec<String>,
    collection_gaps: &mut Vec<crate::model::ReadGap>,
    scan_truncated: &mut bool,
    discovery_incomplete: &mut bool,
    association: Option<&LibraryAssociation>,
) -> Result<()> {
    let mut member_file = match archive.by_index(member.index) {
        Ok(member_file) => member_file,
        Err(error) => {
            *discovery_incomplete = true;
            *scan_truncated = true;
            diagnostics.push(format!("{}: unreadable member: {error:#}", member.name));
            collection_gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: unreadable member", member.name),
            });
            return Ok(());
        }
    };
    let report = read_associated_report(
        &mut member_file,
        &member.name,
        member.size,
        options,
        budget,
        diagnostics,
        association,
        collection_gaps,
    );
    match report {
        Ok(Some(report)) => associated_reports.push(report),
        Ok(None) => {}
        Err(error) if budget.exhausted() => {
            *scan_truncated = true;
            *discovery_incomplete = true;
            diagnostics.push(format!(
                "{}: input byte budget exhausted: {error}",
                member.name
            ));
            collection_gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: input budget", member.name),
            });
        }
        Err(error) => {
            *discovery_incomplete = true;
            diagnostics.push(format!("{}: unreadable member: {error:#}", member.name));
            collection_gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: unreadable member", member.name),
            });
        }
    }
    Ok(())
}

fn read_zip_value<R: Read + Seek>(
    archive: &mut ZipArchive<BudgetedSourceReader<'_, R>>,
    member: &ZipMemberMetadata,
    options: &InputOptions,
    budget: &Budget,
    diagnostics: &mut Vec<String>,
    gaps: &mut Vec<crate::model::ReadGap>,
    maximum: u64,
) -> Result<Option<Value>> {
    let mut member_file = archive
        .by_index(member.index)
        .with_context(|| format!("open ZIP member {}", member.name))?;
    let Some(bytes) = read_member_bytes(
        &mut member_file,
        &member.name,
        member.size,
        options,
        budget,
        diagnostics,
        gaps,
        maximum,
        true,
    )?
    else {
        return Ok(None);
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            diagnostics.push(format!("{}: malformed JSON member: {error}", member.name));
            gaps.push(crate::model::ReadGap {
                span: ByteSpan {
                    start: 0,
                    end: member.size,
                },
                reason: format!("{}: malformed JSON member", member.name),
            });
            Ok(None)
        }
    }
}

fn drain_zip_member<R: Read>(reader: &mut R, budget: &Budget) -> io::Result<()> {
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        budget.charge_decoded(read as u64)?;
    }
}

#[allow(clippy::too_many_arguments)]
fn read_associated_report<R: Read>(
    reader: &mut R,
    member: &str,
    member_size: u64,
    options: &InputOptions,
    budget: &Budget,
    diagnostics: &mut Vec<String>,
    association: Option<&LibraryAssociation>,
    gaps: &mut Vec<crate::model::ReadGap>,
) -> Result<Option<AssociatedReport>> {
    let Some(bytes) = read_member_bytes(
        reader,
        member,
        member_size,
        options,
        budget,
        diagnostics,
        gaps,
        options.record_bytes,
        true,
    )?
    else {
        return Ok(None);
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{') {
                diagnostics.push(format!("{member}: malformed report JSON: {error}"));
                gaps.push(crate::model::ReadGap {
                    span: ByteSpan {
                        start: 0,
                        end: member_size,
                    },
                    reason: format!("{member}: malformed report JSON"),
                });
            }
            return Ok(None);
        }
    };
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let Some(widget_state) = object.get("widget_state").and_then(Value::as_object) else {
        return Ok(None);
    };
    let report_message = widget_state
        .get("report_message")
        .filter(|value| value.is_object());
    let backing = object
        .get("backing_conversation_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| association.and_then(|association| association.backing.clone()));
    let originating = association.and_then(|association| association.originating.clone());
    let origin_message = association.and_then(|association| association.origin_message.clone());
    let identity = object
        .get("widget_session_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| association.and_then(|association| association.file_id.clone()));
    let author = report_message
        .and_then(|message| message["author"]["role"].as_str())
        .map(str::to_owned);
    let completion = widget_state
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| report_message.and_then(|message| message["status"].as_str()))
        .map(str::to_owned);
    let (body, citations, omitted_citations, citation_traversal_incomplete, descriptor_truncated) =
        report_message
            .map(|message| {
                let (parts, _) = message_parts(message);
                let body = parts.iter().any(|part| part.text().is_some()).then(|| {
                    content::bounded_text(&content::project_text(&parts), MAX_ARTIFACT_BODY_CHARS)
                });
                let (
                    citations,
                    omitted_citations,
                    citation_traversal_incomplete,
                    descriptor_truncated,
                ) = collect_citations(message);
                (
                    body,
                    citations,
                    omitted_citations,
                    citation_traversal_incomplete,
                    descriptor_truncated,
                )
            })
            .unwrap_or((None, Vec::new(), 0, false, false));
    let mut reference = content::artifact_reference(&value, "openai-library-report")
        .unwrap_or_else(|| crate::content::ArtifactReference {
            kind: "openai-library-report".to_owned(),
            identity: None,
            origin: None,
            backing: None,
            originating_conversation: None,
            author: None,
            completion: None,
            citation_count: None,
            uri: None,
            path: None,
            digest: None,
            bytes: None,
            timestamp: None,
            source: None,
            action: None,
            body: None,
            body_availability: None,
            citations: Vec::new(),
            omitted_citations: 0,
            citation_traversal_incomplete: false,
            descriptor_truncated: false,
        });
    reference.identity = identity;
    reference.origin = Some("openai-widget-state".to_owned());
    reference.backing = backing.clone();
    reference.originating_conversation = originating.clone();
    reference.author = author;
    reference.completion = completion;
    reference.citation_count = Some(citations.len());
    reference.path = Some(member.to_owned());
    reference.bytes = Some(member_size);
    reference.action = Some("associated-report".to_owned());
    reference.source = Some(RecordRef {
        domain: "supplied-artifact".to_owned(),
        revision: None,
        span: None,
        native_id: reference.identity.clone(),
        pointer: Some(format!("/associated/{member}")),
        part_index: 0,
        content_part_index: None,
    });
    reference.body = body;
    reference.body_availability = Some(if report_message.is_none() {
        ContentAvailability::Unknown
    } else if reference.body.is_some() {
        ContentAvailability::RetainedBody
    } else {
        ContentAvailability::UnsupportedRepresentation
    });
    reference.citations = citations;
    reference.omitted_citations = omitted_citations;
    reference.citation_traversal_incomplete = citation_traversal_incomplete;
    reference.descriptor_truncated = descriptor_truncated;
    Ok(Some(AssociatedReport {
        member: member.to_owned(),
        backing,
        originating,
        origin_message,
        reference: reference.clone(),
        part: ContentPart::StructuredArtifact {
            descriptor: content::bounded_shape(&value),
            source_field: format!("zip-member:{member}"),
            native_kind: "openai-library-report".to_owned(),
            reference: Some(reference),
            record_ref: None,
        },
    }))
}

struct DescriptorBudget {
    used_bytes: usize,
    truncated: bool,
}

impl DescriptorBudget {
    fn new() -> Self {
        Self {
            used_bytes: 0,
            truncated: false,
        }
    }

    fn string(&mut self, value: &Value) -> Option<crate::model::BoundedText> {
        let text = value.as_str()?;
        let remaining = content::MAX_CITATION_DESCRIPTOR_BYTES.saturating_sub(self.used_bytes);
        let bound = remaining.min(content::MAX_CITATION_FIELD_BYTES);
        let bounded = content::bounded_text_bytes(text, bound);
        self.used_bytes = self.used_bytes.saturating_add(bounded.text.len());
        self.truncated |= bounded.truncated;
        Some(bounded)
    }
}

fn collect_citations(value: &Value) -> (Vec<content::ArtifactCitation>, usize, bool, bool) {
    let mut citations = Vec::new();
    let mut omitted = 0;
    let mut incomplete = false;
    let mut descriptors = DescriptorBudget::new();
    collect_citations_inner(
        value,
        0,
        &mut citations,
        &mut omitted,
        &mut incomplete,
        &mut descriptors,
    );
    (citations, omitted, incomplete, descriptors.truncated)
}

fn collect_citations_inner(
    value: &Value,
    depth: usize,
    citations: &mut Vec<content::ArtifactCitation>,
    omitted: &mut usize,
    incomplete: &mut bool,
    descriptors: &mut DescriptorBudget,
) {
    if depth >= content::MAX_STRUCTURED_DEPTH {
        *incomplete = true;
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, value) in object {
        if key == "content_references" {
            collect_citation_values(
                value,
                depth + 1,
                citations,
                omitted,
                incomplete,
                descriptors,
            );
        } else {
            collect_citations_inner(
                value,
                depth + 1,
                citations,
                omitted,
                incomplete,
                descriptors,
            );
        }
    }
}

fn collect_citation_values(
    value: &Value,
    depth: usize,
    citations: &mut Vec<content::ArtifactCitation>,
    omitted: &mut usize,
    incomplete: &mut bool,
    descriptors: &mut DescriptorBudget,
) {
    if depth >= content::MAX_STRUCTURED_DEPTH {
        *incomplete = true;
        return;
    }
    if citations.len() >= content::MAX_ARTIFACT_REFERENCES {
        let (count, complete) = count_citation_candidates(value, depth);
        *omitted = omitted.saturating_add(count);
        *incomplete |= !complete;
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if citations.len() >= content::MAX_ARTIFACT_REFERENCES {
                    let (count, complete) =
                        sum_citation_candidates(values[index..].iter(), depth + 1);
                    *omitted = omitted.saturating_add(count);
                    *incomplete |= !complete;
                    break;
                }
                collect_citation_values(
                    value,
                    depth + 1,
                    citations,
                    omitted,
                    incomplete,
                    descriptors,
                );
            }
        }
        Value::Object(object) => {
            if let Some(mut citation) = citation_from_value(object, descriptors) {
                let mut omitted_sources = 0;
                let mut sources_incomplete = false;
                for key in ["items", "sources", "source"] {
                    if let Some(value) = object.get(key) {
                        collect_citation_sources(
                            value,
                            depth + 1,
                            &mut citation.sources,
                            &mut omitted_sources,
                            &mut sources_incomplete,
                            descriptors,
                        );
                    }
                }
                citation.omitted_sources = omitted_sources;
                citation.source_traversal_incomplete = sources_incomplete;
                citations.push(citation);
            } else {
                for (index, value) in object.values().enumerate() {
                    if citations.len() >= content::MAX_ARTIFACT_REFERENCES {
                        let (count, complete) =
                            sum_citation_candidates(object.values().skip(index), depth + 1);
                        *omitted = omitted.saturating_add(count);
                        *incomplete |= !complete;
                        break;
                    }
                    collect_citation_values(
                        value,
                        depth + 1,
                        citations,
                        omitted,
                        incomplete,
                        descriptors,
                    );
                }
            }
        }
        _ => {}
    }
}

fn collect_citation_sources(
    value: &Value,
    depth: usize,
    sources: &mut Vec<content::ArtifactCitationSource>,
    omitted: &mut usize,
    incomplete: &mut bool,
    descriptors: &mut DescriptorBudget,
) {
    if depth >= content::MAX_STRUCTURED_DEPTH {
        *incomplete = true;
        return;
    }
    if sources.len() >= content::MAX_ARTIFACT_REFERENCES {
        let (count, complete) = count_citation_source_candidates(value, depth);
        *omitted = omitted.saturating_add(count);
        *incomplete |= !complete;
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if sources.len() >= content::MAX_ARTIFACT_REFERENCES {
                    let (count, complete) =
                        sum_citation_source_candidates(values[index..].iter(), depth + 1);
                    *omitted = omitted.saturating_add(count);
                    *incomplete |= !complete;
                    break;
                }
                collect_citation_sources(
                    value,
                    depth + 1,
                    sources,
                    omitted,
                    incomplete,
                    descriptors,
                );
            }
        }
        Value::Object(object) => {
            if let Some(mut source) = citation_source_from_value(object, descriptors) {
                let mut nested_omitted = 0;
                let mut nested_incomplete = false;
                for key in ["items", "sources", "source"] {
                    if let Some(value) = object.get(key) {
                        collect_citation_sources(
                            value,
                            depth + 1,
                            &mut source.sources,
                            &mut nested_omitted,
                            &mut nested_incomplete,
                            descriptors,
                        );
                    }
                }
                source.omitted_sources = nested_omitted;
                source.source_traversal_incomplete = nested_incomplete;
                sources.push(source);
            } else {
                for (index, value) in object.values().enumerate() {
                    if sources.len() >= content::MAX_ARTIFACT_REFERENCES {
                        let (count, complete) =
                            sum_citation_source_candidates(object.values().skip(index), depth + 1);
                        *omitted = omitted.saturating_add(count);
                        *incomplete |= !complete;
                        break;
                    }
                    collect_citation_sources(
                        value,
                        depth + 1,
                        sources,
                        omitted,
                        incomplete,
                        descriptors,
                    );
                }
            }
        }
        _ => {}
    }
}

fn citation_from_value(
    value: &serde_json::Map<String, Value>,
    descriptors: &mut DescriptorBudget,
) -> Option<content::ArtifactCitation> {
    let (kind, uri, title, start, end) = citation_fields(value, descriptors)?;
    Some(content::ArtifactCitation {
        kind,
        uri,
        title,
        start,
        end,
        sources: Vec::new(),
        omitted_sources: 0,
        source_traversal_incomplete: false,
    })
}

fn citation_source_from_value(
    value: &serde_json::Map<String, Value>,
    descriptors: &mut DescriptorBudget,
) -> Option<content::ArtifactCitationSource> {
    let (kind, uri, title, start, end) = citation_fields(value, descriptors)?;
    Some(content::ArtifactCitationSource {
        kind,
        uri,
        title,
        start,
        end,
        sources: Vec::new(),
        omitted_sources: 0,
        source_traversal_incomplete: false,
    })
}

type CitationFields = (
    Option<crate::model::BoundedText>,
    Option<crate::model::BoundedText>,
    Option<crate::model::BoundedText>,
    Option<usize>,
    Option<usize>,
);

fn citation_fields(
    value: &serde_json::Map<String, Value>,
    descriptors: &mut DescriptorBudget,
) -> Option<CitationFields> {
    if !citation_shape_present(value) {
        return None;
    }
    let kind = value
        .get("type")
        .and_then(|value| descriptors.string(value));
    let uri = ["uri", "url", "href"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(|value| descriptors.string(value)));
    let title = value
        .get("title")
        .and_then(|value| descriptors.string(value));
    let start = ["start_idx", "start_index", "start"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
        .and_then(|value| usize::try_from(value).ok());
    let end = ["end_idx", "end_index", "end"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
        .and_then(|value| usize::try_from(value).ok());
    (kind.is_some() || uri.is_some() || title.is_some() || start.is_some() || end.is_some())
        .then_some((kind, uri, title, start, end))
}

fn citation_shape_present(value: &serde_json::Map<String, Value>) -> bool {
    ["type", "uri", "url", "href", "title"]
        .into_iter()
        .any(|key| value.get(key).is_some_and(Value::is_string))
        || [
            "start_idx",
            "start_index",
            "start",
            "end_idx",
            "end_index",
            "end",
        ]
        .into_iter()
        .any(|key| value.get(key).is_some_and(Value::is_u64))
}

fn count_citation_candidates(value: &Value, depth: usize) -> (usize, bool) {
    if depth >= content::MAX_STRUCTURED_DEPTH {
        return (0, false);
    }
    match value {
        Value::Array(values) => sum_citation_candidates(values.iter(), depth + 1),
        Value::Object(object) if citation_shape_present(object) => (1, true),
        Value::Object(object) => sum_citation_candidates(object.values(), depth + 1),
        _ => (0, true),
    }
}

fn sum_citation_candidates<'a>(
    values: impl Iterator<Item = &'a Value>,
    depth: usize,
) -> (usize, bool) {
    values.fold((0, true), |(count, complete), value| {
        let (more, more_complete) = count_citation_candidates(value, depth);
        (count.saturating_add(more), complete && more_complete)
    })
}

fn count_citation_source_candidates(value: &Value, depth: usize) -> (usize, bool) {
    if depth >= content::MAX_STRUCTURED_DEPTH {
        return (0, false);
    }
    match value {
        Value::Array(values) => sum_citation_source_candidates(values.iter(), depth + 1),
        Value::Object(object) if citation_shape_present(object) => {
            let mut count: usize = 1;
            let mut complete = true;
            for key in ["items", "sources", "source"] {
                if let Some(value) = object.get(key) {
                    let (more, more_complete) = count_citation_source_candidates(value, depth + 1);
                    count = count.saturating_add(more);
                    complete &= more_complete;
                }
            }
            (count, complete)
        }
        Value::Object(object) => sum_citation_source_candidates(object.values(), depth + 1),
        _ => (0, true),
    }
}

fn sum_citation_source_candidates<'a>(
    values: impl Iterator<Item = &'a Value>,
    depth: usize,
) -> (usize, bool) {
    values.fold((0, true), |(count, complete), value| {
        let (more, more_complete) = count_citation_source_candidates(value, depth);
        (count.saturating_add(more), complete && more_complete)
    })
}

fn attach_associated_reports(
    occurrences: &mut [InputOccurrence],
    reports: &mut [AssociatedReport],
    diagnostics: &mut Vec<String>,
) {
    for report in reports {
        let backing = report.backing.as_deref();
        let originating = report.originating.as_deref();
        let origin_message = report.origin_message.as_deref();
        let matching_indices = occurrences
            .iter()
            .enumerate()
            .filter_map(|(index, occurrence)| {
                let by_conversation = [backing, originating]
                    .into_iter()
                    .flatten()
                    .any(|conversation| occurrence.session.id == conversation);
                let by_message = origin_message.is_some_and(|message| {
                    occurrence
                        .turns
                        .iter()
                        .any(|turn| turn.native_id.as_deref() == Some(message))
                        || occurrence.graph.as_ref().is_some_and(|graph| {
                            graph.nodes.iter().any(|node| {
                                node.message
                                    .as_ref()
                                    .and_then(|turn| turn.native_id.as_deref())
                                    == Some(message)
                            })
                        })
                });
                (by_conversation || by_message).then_some(index)
            })
            .collect::<Vec<_>>();
        let matching = matching_indices.len();
        if matching == 0 {
            diagnostics.push(format!(
                "associated report {} names an unreached library association",
                report.member
            ));
            continue;
        }
        if matching > 1 {
            let association = backing
                .or(originating)
                .or(origin_message)
                .unwrap_or("unknown");
            diagnostics.push(format!(
                "associated report {} names an ambiguous library association {association} and remains unjoined",
                report.member,
            ));
            continue;
        }
        let occurrence = occurrences
            .get_mut(matching_indices[0])
            .expect("matching associated report occurrence");
        // A report reached only through its originating message records that
        // conversation as its backing; a recorded originating conversation is
        // already the association and is not restated as a backing session.
        if report.reference.backing.is_none() && originating.is_none() {
            report.reference.backing = Some(occurrence.session.id.clone());
            if let ContentPart::StructuredArtifact {
                reference: Some(reference),
                ..
            } = &mut report.part
            {
                reference.backing = report.reference.backing.clone();
            }
        }
        let mut artifact = report.reference.clone();
        if artifact.backing.is_none() && originating.is_none() {
            artifact.backing = Some(occurrence.session.id.clone());
        }
        let artifact_reference = RecordRef {
            domain: format!("{}:associated", occurrence.session.source.origin),
            revision: occurrence.evidence.source_revision.clone(),
            span: None,
            native_id: None,
            pointer: Some(format!("/associated/{}", report.member)),
            part_index: occurrence.artifacts.len(),
            content_part_index: None,
        };
        artifact.source = Some(artifact_reference.clone());
        occurrence.artifacts.push(artifact.clone());
        let Some(turn) = occurrence.turns.last_mut() else {
            diagnostics.push(format!(
                "associated report {} has a backing conversation without a readable turn and remains an unjoined artifact",
                report.member
            ));
            continue;
        };
        let mut part = report.part.clone();
        part.set_record_ref_part(artifact_reference, 0);
        turn.parts.push(part);
        if turn.coverage.is_none() {
            turn.coverage = Some(ContentCoverage {
                carrier: ContentCarrier::AssociatedArtifact,
                availability: if artifact.body.is_some() {
                    ContentAvailability::RetainedBody
                } else {
                    ContentAvailability::ReferenceOnly
                },
                retained_parts: 1,
                omitted_parts: 0,
                omitted_reason: None,
            });
        }
    }
}

fn metadata_revision(metadata: &Metadata) -> String {
    format!(
        "input-stat:{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

fn observation_revision(parts: &[String]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut hasher);
    format!(
        "input-observation-v1:{:016x}:{}",
        hasher.finish(),
        parts.len()
    )
}

fn parse_occurrence(value: &str) -> Result<(String, usize)> {
    let pieces = value.split(':').collect::<Vec<_>>();
    if pieces.len() != 6 || pieces[0] != "input" || pieces[1] != "v2" {
        bail!("invalid occurrence prefix");
    }
    let observation = decode_component(pieces[2])?;
    let ordinal = pieces[5]
        .parse::<usize>()
        .context("occurrence offset is not numeric")?;
    Ok((observation, ordinal))
}

fn encode_component(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_component(value: &str) -> Result<String> {
    if !value.len().is_multiple_of(2) {
        bail!("occurrence source revision is not hex encoded");
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .context("occurrence source revision is not hex encoded")
        })
        .collect::<Result<Vec<_>>>()?;
    String::from_utf8(bytes).context("occurrence source revision is not UTF-8")
}
