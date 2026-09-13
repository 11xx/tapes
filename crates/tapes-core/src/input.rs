//! Bounded readers for caller-supplied conversation exports.
//!
//! Explicit inputs are read through one backend so every view shares the
//! normalized source, occurrence, content, and coverage contracts. The reader
//! never extracts an archive, creates an index, opens referenced artifacts, or
//! falls back to installed stores.

use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
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
    ByteSpan, ConversationEdge, ConversationGraph, ConversationNode, EmptyTextTailReason, Model,
    ReadEvidence, ReadRange, ReadRangeKind, RecordRef, Role, ScopeAuthority, Session,
    SessionMetadata, SourceDescriptor, SourceLocation, SourceScope, TerminalObservation,
    TextTailEvidence, Transcript, TranscriptEvidence, Truncation, Turn, TurnKind,
};

pub const DEFAULT_SCAN_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_DECODED_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_RECORD_BYTES: u64 = 8 * 1024 * 1024;
pub const DEFAULT_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_SCAN_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_DECODED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_MEMBERS: usize = 10_000;
pub const MAX_DEPTH: usize = 128;
pub const MAX_ARTIFACT_BODY_CHARS: usize = 64 * 1024;

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
        ] {
            if value == 0 || value > maximum {
                bail!("input {name} must be between 1 and {maximum}");
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
    fn harness(&self) -> &'static str {
        "input"
    }

    fn available(&self) -> bool {
        self.options.paths.iter().all(|path| path.exists())
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
        let after = self
            .options
            .after_occurrence
            .as_deref()
            .map(parse_occurrence);
        let after = after
            .as_ref()
            .map(|after| after.as_ref().map_err(|error| anyhow!("{error}")))
            .transpose()?;
        if let Some((observation, _)) = after.as_ref() {
            if observation != &dataset.observation {
                bail!("--after-occurrence belongs to a different supplied input observation");
            }
        }
        for occurrence in &dataset.occurrences {
            if let Some((_, ordinal)) = after.as_ref() {
                if occurrence.ordinal <= *ordinal {
                    continue;
                }
            }
            let session = &occurrence.session;
            let placed = query.scope.is_none_or(|scope| {
                session
                    .directory
                    .as_deref()
                    .is_some_and(|directory| scope.contains(directory))
            });
            if placed && query.matches(session) {
                listing.sessions.push(session.clone());
                if listing.sessions.len() >= query.limit {
                    break;
                }
            }
        }
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
        let transcript = input_transcript(occurrence, tail)?;
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
}

struct InputDataset {
    occurrences: Vec<InputOccurrence>,
    artifacts: Vec<ArtifactReference>,
    observation: String,
    diagnostics: Vec<String>,
    scanned: usize,
    scan_truncated: bool,
}

#[derive(Clone)]
struct AssociatedReport {
    member: String,
    backing: Option<String>,
    reference: ArtifactReference,
    part: ContentPart,
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
    let truncation = Truncation {
        window: Truncation::window(turns.len(), total, tail),
        source: Vec::new(),
    };
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
        occurrence.notes.clone(),
    );
    transcript.artifacts = occurrence.artifacts.clone();
    transcript.graph = occurrence.graph.clone();
    Ok(transcript)
}

fn load_dataset(options: &InputOptions) -> Result<InputDataset> {
    let mut budget = Budget::new(options);
    let mut occurrences = Vec::new();
    let mut diagnostics = Vec::new();
    let mut associated_reports = Vec::new();
    let mut observation_parts = Vec::new();
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
            let mut files = Vec::new();
            collect_json_files(path, &mut files)?;
            files.sort();
            for file in files {
                if budget.members >= MAX_MEMBERS {
                    scan_truncated = true;
                    diagnostics.push("input member limit reached".to_owned());
                    break;
                }
                budget.members += 1;
                observation_parts.push(format!(
                    "file:{}:{}",
                    file.display(),
                    metadata_revision(&fs::metadata(&file)?)
                ));
                scan_file(
                    &file,
                    None,
                    options,
                    &mut budget,
                    &mut occurrences,
                    &mut diagnostics,
                    &mut scanned,
                    &mut scan_truncated,
                )?;
            }
        } else if is_zip_path(path)? {
            scan_zip(
                path,
                options,
                &mut budget,
                &mut occurrences,
                &mut diagnostics,
                &mut associated_reports,
                &mut observation_parts,
                &mut scanned,
                &mut scan_truncated,
            )?;
        } else {
            budget.members += 1;
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
                &mut budget,
                &mut occurrences,
                &mut diagnostics,
                &mut scanned,
                &mut scan_truncated,
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
    attach_associated_reports(&mut occurrences, &associated_reports, &mut diagnostics);
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
    })
}

struct Budget {
    scan_used: u64,
    decoded_used: u64,
    members: usize,
    scan_limit: u64,
    decoded_limit: u64,
}

impl Budget {
    fn new(options: &InputOptions) -> Self {
        Self {
            scan_used: 0,
            decoded_used: 0,
            members: 0,
            scan_limit: options.scan_bytes,
            decoded_limit: options.decoded_bytes,
        }
    }

    fn exhausted(&self) -> bool {
        self.scan_used >= self.scan_limit || self.decoded_used >= self.decoded_limit
    }
}

struct Scanner<'a, R> {
    reader: R,
    budget: &'a mut Budget,
    buffer: [u8; 64 * 1024],
    position: usize,
    length: usize,
    offset: u64,
    pending: Option<u8>,
}

impl<'a, R: Read> Scanner<'a, R> {
    fn new(reader: R, budget: &'a mut Budget) -> Self {
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
        if self.budget.scan_used >= self.budget.scan_limit {
            return Err(io::Error::other("input scan budget exhausted"));
        }
        if let Some(byte) = self.pending.take() {
            self.offset += 1;
            self.budget.scan_used += 1;
            return Ok(Some(byte));
        }
        if self.position == self.length {
            let read = self.reader.read(&mut self.buffer)?;
            if read == 0 {
                return Ok(None);
            }
            self.position = 0;
            self.length = read;
        }
        let byte = self.buffer[self.position];
        self.position += 1;
        self.offset += 1;
        self.budget.scan_used += 1;
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

    fn value(&mut self, first: u8, start: u64, max: u64) -> io::Result<Frame> {
        let mut depth = match first {
            b'{' | b'[' => 1usize,
            _ => 0,
        };
        let mut in_string = false;
        let mut escaped = false;
        let mut bytes = Vec::new();
        bytes.push(first);
        let mut oversized = false;
        if depth == 0 {
            return Ok(Frame::Invalid);
        }
        while depth > 0 {
            let Some(byte) = self.byte()? else {
                return Ok(Frame::Incomplete);
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
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let end = self.offset;
        if oversized {
            Ok(Frame::Oversized {
                span: ByteSpan { start, end },
            })
        } else {
            Ok(Frame::Record {
                bytes,
                span: ByteSpan { start, end },
            })
        }
    }

    fn scalar(&mut self, first: u8, start: u64, max: u64) -> io::Result<Frame> {
        let mut bytes = vec![first];
        let mut in_string = first == b'"';
        let mut escaped = false;
        let mut oversized = false;
        loop {
            let Some(byte) = self.byte()? else {
                let end = self.offset;
                return if oversized {
                    Ok(Frame::Oversized {
                        span: ByteSpan { start, end },
                    })
                } else {
                    Ok(Frame::Record {
                        bytes,
                        span: ByteSpan { start, end },
                    })
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
                self.budget.scan_used = self.budget.scan_used.saturating_sub(1);
                self.pending = Some(byte);
                let end = self.offset;
                return if oversized {
                    Ok(Frame::Oversized {
                        span: ByteSpan { start, end },
                    })
                } else {
                    Ok(Frame::Record {
                        bytes,
                        span: ByteSpan { start, end },
                    })
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
        let mut value = Vec::new();
        let mut escaped = false;
        loop {
            let Some(byte) = self.byte()? else {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated JSON object key",
                ));
            };
            if escaped {
                escaped = false;
                if value.len() < max {
                    value.push(byte);
                }
                continue;
            }
            match byte {
                b'\\' => escaped = true,
                b'"' => {
                    return String::from_utf8(value).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "JSON object key is not UTF-8")
                    })
                }
                _ if value.len() < max => value.push(byte),
                _ => {}
            }
            if value.len() >= max {
                return Err(io::Error::other(
                    "JSON object key exceeds the structural bound",
                ));
            }
        }
    }
}

enum Frame {
    Record { bytes: Vec<u8>, span: ByteSpan },
    Oversized { span: ByteSpan },
    Invalid,
    Incomplete,
}

#[allow(clippy::too_many_arguments)]
fn scan_reader<R: Read>(
    reader: R,
    locator: &str,
    member: Option<&str>,
    source_length: u64,
    revision: String,
    options: &InputOptions,
    budget: &mut Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    scanned: &mut usize,
) -> Result<()> {
    let mut scanner = Scanner::new(reader, budget);
    let first_occurrence = occurrences.len();
    let mut gaps = Vec::new();
    let Some((first, start)) = scanner.non_whitespace()? else {
        return Ok(());
    };
    match first {
        b'[' => loop {
            let Some((byte, item_start)) = scanner.non_whitespace()? else {
                diagnostics.push(format!("{locator}: incomplete top-level array"));
                break;
            };
            if byte == b']' {
                break;
            }
            let frame = scanner.value(byte, item_start, options.record_bytes)?;
            match frame {
                Frame::Record { bytes, span } => {
                    *scanned += 1;
                    scanner.budget.decoded_used = scanner
                        .budget
                        .decoded_used
                        .saturating_add(bytes.len() as u64);
                    if scanner.budget.decoded_used > scanner.budget.decoded_limit {
                        diagnostics.push(format!("{locator}: decoded-byte budget exhausted"));
                        break;
                    }
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
                    )?;
                }
                Frame::Oversized { span } => {
                    *scanned += 1;
                    gaps.push(crate::model::ReadGap {
                        span,
                        reason: "record-bytes-bound".to_owned(),
                    });
                    diagnostics.push(format!(
                        "{locator}: skipped oversized record at {}..{}",
                        span.start, span.end
                    ));
                }
                Frame::Invalid | Frame::Incomplete => {
                    diagnostics.push(format!(
                        "{locator}: top-level array lost structural synchronization"
                    ));
                    break;
                }
            }
            let Some((separator, _)) = scanner.non_whitespace()? else {
                diagnostics.push(format!("{locator}: incomplete top-level array"));
                break;
            };
            if separator == b']' {
                break;
            }
            if separator != b',' {
                diagnostics.push(format!(
                    "{locator}: top-level array has an unexpected separator"
                ));
                break;
            }
        },
        b'{' => {
            scan_root_object(
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
                &mut gaps,
            )?;
            while let Some((next, next_start)) = scanner.non_whitespace()? {
                if next != b'{' {
                    diagnostics.push(format!(
                        "{locator}: unexpected trailing byte {next:?} at {next_start}"
                    ));
                    break;
                }
                scan_root_object(
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
                    &mut gaps,
                )?;
            }
        }
        _ => diagnostics.push(format!("{locator}: root must be a JSON array or object")),
    }
    for occurrence in occurrences.iter_mut().skip(first_occurrence) {
        occurrence.evidence.gaps.extend(gaps.iter().cloned());
    }
    Ok(())
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
) -> Result<()> {
    let mut fields = serde_json::Map::new();
    let mut has_conversations = false;
    let mut field_bytes = 0u64;
    loop {
        let Some((first, key_start)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level object"));
            return Ok(());
        };
        if first == b'}' {
            break;
        }
        if first != b'"' {
            diagnostics.push(format!("{locator}: top-level object expected a quoted key"));
            return Ok(());
        }
        let key = scanner.string(key_start, 64 * 1024)?;
        let Some((colon, _)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: object key has no value"));
            return Ok(());
        };
        if colon != b':' {
            diagnostics.push(format!("{locator}: top-level object expected a colon"));
            return Ok(());
        }
        let Some((value_first, value_start)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: object key has no value"));
            return Ok(());
        };
        if key == "conversations" && value_first == b'[' {
            has_conversations = true;
            scan_array_items(
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
            )?;
        } else {
            let frame = if matches!(value_first, b'{' | b'[') {
                scanner.value(value_first, value_start, options.record_bytes)?
            } else {
                scanner.scalar(value_first, value_start, options.record_bytes)?
            };
            match frame {
                Frame::Record { bytes, .. } => {
                    field_bytes = field_bytes.saturating_add(bytes.len() as u64);
                    if field_bytes <= options.record_bytes {
                        scanner.budget.decoded_used = scanner
                            .budget
                            .decoded_used
                            .saturating_add(bytes.len() as u64);
                        if scanner.budget.decoded_used <= scanner.budget.decoded_limit {
                            if let Ok(value) = serde_json::from_slice(&bytes) {
                                fields.insert(key, value);
                            } else {
                                diagnostics.push(format!("{locator}: malformed object member"));
                            }
                        } else {
                            diagnostics.push(format!("{locator}: decoded-byte budget exhausted"));
                            break;
                        }
                    } else {
                        diagnostics.push(format!(
                            "{locator}: top-level object fields exceed --record-bytes"
                        ));
                    }
                }
                Frame::Oversized { span } => {
                    gaps.push(crate::model::ReadGap {
                        span,
                        reason: "record-bytes-bound".to_owned(),
                    });
                    diagnostics.push(format!(
                        "{locator}: skipped oversized object member at {}..{}",
                        span.start, span.end
                    ));
                }
                Frame::Invalid | Frame::Incomplete => {
                    diagnostics.push(format!("{locator}: incomplete top-level object member"));
                    return Ok(());
                }
            }
        }
        let Some((separator, _)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level object"));
            return Ok(());
        };
        if separator == b'}' {
            break;
        }
        if separator != b',' {
            diagnostics.push(format!(
                "{locator}: top-level object has an unexpected separator"
            ));
            return Ok(());
        }
    }
    if has_conversations {
        return Ok(());
    }
    let bytes = serde_json::to_vec(&Value::Object(fields))?;
    *scanned += 1;
    if bytes.len() as u64 > options.record_bytes {
        diagnostics.push(format!(
            "{locator}: top-level object exceeds --record-bytes at {}..{}",
            start, scanner.offset
        ));
        return Ok(());
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
    )
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
) -> Result<()> {
    loop {
        let Some((first, item_start)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level array"));
            return Ok(());
        };
        if first == b']' {
            return Ok(());
        }
        let frame = scanner.value(first, item_start, options.record_bytes)?;
        match frame {
            Frame::Record { bytes, span } => {
                *scanned += 1;
                scanner.budget.decoded_used = scanner
                    .budget
                    .decoded_used
                    .saturating_add(bytes.len() as u64);
                if scanner.budget.decoded_used > scanner.budget.decoded_limit {
                    diagnostics.push(format!("{locator}: decoded-byte budget exhausted"));
                    return Ok(());
                }
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
                )?;
            }
            Frame::Oversized { span } => {
                *scanned += 1;
                gaps.push(crate::model::ReadGap {
                    span,
                    reason: "record-bytes-bound".to_owned(),
                });
                diagnostics.push(format!(
                    "{locator}: skipped oversized record at {}..{}",
                    span.start, span.end
                ));
            }
            Frame::Invalid | Frame::Incomplete => {
                diagnostics.push(format!(
                    "{locator}: top-level array lost structural synchronization"
                ));
                return Ok(());
            }
        }
        let Some((separator, _)) = scanner.non_whitespace()? else {
            diagnostics.push(format!("{locator}: incomplete top-level array"));
            return Ok(());
        };
        if separator == b']' {
            return Ok(());
        }
        if separator != b',' {
            diagnostics.push(format!(
                "{locator}: top-level array has an unexpected separator"
            ));
            return Ok(());
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
) -> Result<()> {
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(format!(
                "{locator}: malformed record at {}..{}: {error}",
                span.start, span.end
            ));
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
            )?;
        }
        return Ok(());
    }
    let format = match options.format {
        InputFormat::Auto => detect_format(&value),
        declared => Some(declared),
    };
    let Some(format) = format else {
        diagnostics.push(format!(
            "{locator}: record shape is not a supported conversation export"
        ));
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
    )) = normalize_value(&value, format, ordinal)?
    else {
        return Ok(());
    };
    let (source_origin, representation, producer) = match format {
        InputFormat::Openai => ("openai", "openai-conversation", Some("OpenAI export")),
        InputFormat::ChatgptExporter => (
            "chatgpt-exporter",
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
            pointer: turn
                .record_ref
                .as_ref()
                .and_then(|reference| reference.pointer.clone()),
        };
        turn.record_ref = Some(reference.clone());
        for (part_index, part) in turn.parts.iter_mut().enumerate() {
            part.set_record_ref_part(reference.clone(), part_index);
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
                pointer: message
                    .record_ref
                    .as_ref()
                    .and_then(|reference| reference.pointer.clone()),
            };
            message.record_ref = Some(reference.clone());
            for (part_index, part) in message.parts.iter_mut().enumerate() {
                part.set_record_ref_part(reference.clone(), part_index);
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
        gaps: Vec::new(),
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
    });
    Ok(())
}

type NormalizedConversation = Option<(
    String,
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

fn normalize_value(
    value: &Value,
    format: InputFormat,
    ordinal: usize,
) -> Result<NormalizedConversation> {
    match format {
        InputFormat::Openai => normalize_openai(value, ordinal),
        InputFormat::ChatgptExporter => normalize_chatgpt_exporter(value, ordinal),
        InputFormat::Perplexity => normalize_perplexity(value, ordinal),
        InputFormat::Auto => unreachable!(),
    }
}

fn normalize_openai(value: &Value, ordinal: usize) -> Result<NormalizedConversation> {
    normalize_mapping(value, ordinal)
}

fn normalize_mapping(value: &Value, ordinal: usize) -> Result<NormalizedConversation> {
    if !value.is_object() {
        return Ok(None);
    }
    let id = value["conversation_id"]
        .as_str()
        .or_else(|| value["id"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("openai-occurrence-{ordinal}"));
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
        record_ref: Some(RecordRef {
            domain: "input-pending".to_owned(),
            revision: None,
            span: None,
            native_id,
            part_index: index,
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

fn normalize_chatgpt_exporter(value: &Value, ordinal: usize) -> Result<NormalizedConversation> {
    if value["mapping"].is_object() {
        return normalize_mapping(value, ordinal);
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
        .map(str::to_owned)
        .unwrap_or_else(|| format!("chatgpt-exporter-occurrence-{ordinal}"));
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
            record_ref: Some(RecordRef {
                domain: "input-pending".to_owned(),
                revision: None,
                span: None,
                native_id,
                part_index: index,
                pointer: Some(format!("/messages/{index}")),
            }),
            channel: message["channel"].as_str().map(str::to_owned),
            recipient: message["recipient"].as_str().map(str::to_owned),
            parts,
            coverage: Some(coverage),
            tool: None,
        });
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

fn normalize_perplexity(value: &Value, _ordinal: usize) -> Result<NormalizedConversation> {
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
        engine: string_field(object.get("engine_mode")).or_else(|| {
            entries
                .iter()
                .find_map(|entry| entry.get("engine_mode").and_then(Value::as_str))
                .map(str::to_owned)
        }),
        status: string_field(object.get("query_status")).or_else(|| {
            entries
                .iter()
                .find_map(|entry| entry.get("query_status").and_then(Value::as_str))
                .map(str::to_owned)
        }),
        label: string_field(object.get("label")).or_else(|| {
            entries
                .iter()
                .find_map(|entry| entry.get("label").and_then(Value::as_str))
                .map(str::to_owned)
        }),
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
            ));
        }
    }
    Ok(Some((
        id.to_owned(),
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

fn perplexity_turn(
    value: &Value,
    role: Role,
    kind: TurnKind,
    source_field: &str,
    native_id: Option<String>,
    ts: Option<DateTime<Utc>>,
    pointer: String,
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
    let text = content::project_text(&parts);
    Turn {
        role: role.clone(),
        kind,
        text,
        ts,
        ordinal: 0,
        native_id: native_id.clone(),
        request_turn_id: None,
        record_ref: Some(RecordRef {
            domain: "input-pending".to_owned(),
            revision: None,
            span: None,
            native_id,
            pointer: Some(pointer),
            part_index: 0,
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
    let parts_value = content_value
        .get("parts")
        .or_else(|| message.get("parts"))
        .unwrap_or(content_value);
    if let Some(parts) = parts_value.as_array() {
        if parts.iter().all(Value::is_string) {
            let parts = parts
                .iter()
                .filter_map(Value::as_str)
                .map(|text| content::text_part(text, "message.content.parts", "text"))
                .collect::<Vec<_>>();
            let count = parts.len();
            return (
                parts,
                ContentCoverage {
                    carrier: ContentCarrier::DirectPart,
                    availability: ContentAvailability::RetainedBody,
                    retained_parts: count,
                    omitted_parts: 0,
                    omitted_reason: None,
                },
            );
        }
        return content::parts_from_array(parts_value, "message.content.parts");
    }
    (
        vec![ContentPart::Unknown {
            native_kind: message["content_type"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned(),
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

fn detect_format(value: &Value) -> Option<InputFormat> {
    if value["conversations"].is_array()
        || (value["context_uuid"].is_string() && value["entries"].is_array())
    {
        Some(InputFormat::Perplexity)
    } else if value["mapping"].is_object() && value["conversation_id"].is_string() {
        Some(InputFormat::Openai)
    } else if value["mapping"].is_object()
        && (value["id"].is_string() || value["current_node"].is_string())
    {
        Some(InputFormat::ChatgptExporter)
    } else if value["mapping"].is_object() {
        Some(InputFormat::Openai)
    } else if value["messages"].is_array()
        || value["entries"].is_array()
        || value["prompt"].is_string()
        || value.as_array().is_some()
    {
        Some(InputFormat::ChatgptExporter)
    } else {
        None
    }
}

fn collect_json_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        fs::read_dir(root).with_context(|| format!("read input directory {}", root.display()))?
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
            collect_json_files(&path, files)?;
        } else if kind.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json" || extension == "jsonl")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn is_zip_path(path: &Path) -> Result<bool> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        return Ok(true);
    }
    let mut file = File::open(path).with_context(|| format!("open input {}", path.display()))?;
    let mut magic = [0; 4];
    let read = file.read(&mut magic)?;
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
    budget: &mut Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("open input {}", path.display()))?;
    let metadata = file.metadata()?;
    let revision = metadata_revision(&metadata);
    let locator = path.display().to_string();
    let result = scan_reader(
        file,
        &locator,
        member,
        metadata.len(),
        revision,
        options,
        budget,
        occurrences,
        diagnostics,
        scanned,
    );
    match result {
        Err(error) if budget.exhausted() => {
            *scan_truncated = true;
            diagnostics.push(format!("{locator}: input scan budget exhausted: {error}"));
            Ok(())
        }
        result => result,
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_zip(
    path: &Path,
    options: &InputOptions,
    budget: &mut Budget,
    occurrences: &mut Vec<InputOccurrence>,
    diagnostics: &mut Vec<String>,
    associated_reports: &mut Vec<AssociatedReport>,
    observation_parts: &mut Vec<String>,
    scanned: &mut usize,
    scan_truncated: &mut bool,
) -> Result<()> {
    let file =
        File::open(path).with_context(|| format!("open input archive {}", path.display()))?;
    let metadata = file.metadata()?;
    let mut archive = ZipArchive::new(file).context("read supplied ZIP central directory")?;
    if archive.len() > MAX_MEMBERS {
        bail!(
            "supplied ZIP has {} members, above the {MAX_MEMBERS} limit",
            archive.len()
        );
    }
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        budget.members += 1;
        if budget.members > MAX_MEMBERS {
            *scan_truncated = true;
            diagnostics.push("input member limit reached".to_owned());
            break;
        }
        let mut member_file = archive.by_index(index)?;
        let name = member_file.name().to_owned();
        validate_member_name(&name)?;
        if !names.insert(name.clone()) {
            bail!("supplied ZIP has duplicate normalized member {name}");
        }
        observation_parts.push(format!(
            "zip:{}:{}:{}:{}:{}",
            path.display(),
            metadata_revision(&metadata),
            name,
            member_file.size(),
            member_file.crc32()
        ));
        if member_file.is_dir() {
            continue;
        }
        if member_file
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("supplied ZIP contains symlink member {name}");
        }
        let extension = Path::new(&name)
            .extension()
            .and_then(|extension| extension.to_str());
        let is_json =
            extension.is_some_and(|extension| extension == "json" || extension == "jsonl");
        let is_report = extension.is_some_and(|extension| extension == "dat");
        if !is_json && !is_report {
            continue;
        }
        let member_size = member_file.size();
        if member_size > options.decoded_bytes {
            diagnostics.push(format!("{name}: member exceeds decoded-byte budget"));
            continue;
        }
        if is_report {
            *scanned += 1;
            let report = read_associated_report(
                &mut member_file,
                &name,
                member_size,
                options,
                budget,
                diagnostics,
            );
            match report {
                Ok(Some(report)) => associated_reports.push(report),
                Ok(None) => {}
                Err(error) if budget.exhausted() => {
                    *scan_truncated = true;
                    diagnostics.push(format!("{name}: input scan budget exhausted: {error}"));
                    break;
                }
                Err(error) => diagnostics.push(format!("{name}: unreadable member: {error:#}")),
            }
            if budget.exhausted() {
                *scan_truncated = true;
                break;
            }
            continue;
        }
        let revision = format!(
            "{}:member:{}:{}:{}",
            metadata_revision(&metadata),
            name,
            member_size,
            member_file.crc32()
        );
        let locator = path.display().to_string();
        let result = scan_reader(
            &mut member_file,
            &locator,
            Some(&name),
            member_size,
            revision,
            options,
            budget,
            occurrences,
            diagnostics,
            scanned,
        );
        match result {
            Ok(()) => {}
            Err(error) if budget.exhausted() => {
                *scan_truncated = true;
                diagnostics.push(format!("{name}: input scan budget exhausted: {error}"));
                break;
            }
            Err(error) => {
                diagnostics.push(format!("{name}: unreadable member: {error:#}"));
                continue;
            }
        }
        if budget.exhausted() {
            *scan_truncated = true;
            break;
        }
    }
    Ok(())
}

fn validate_member_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("supplied ZIP member escapes its container: {name}");
    }
    Ok(())
}

fn read_associated_report<R: Read>(
    reader: &mut R,
    member: &str,
    member_size: u64,
    options: &InputOptions,
    budget: &mut Budget,
    diagnostics: &mut Vec<String>,
) -> Result<Option<AssociatedReport>> {
    if member_size > options.record_bytes {
        diagnostics.push(format!(
            "{member}: skipped associated report above --record-bytes"
        ));
        return Ok(None);
    }
    let allowed_scan = options.scan_bytes.saturating_sub(budget.scan_used);
    let allowed_decoded = options.decoded_bytes.saturating_sub(budget.decoded_used);
    let allowed = member_size.min(allowed_scan).min(allowed_decoded);
    if allowed < member_size {
        diagnostics.push(format!(
            "{member}: associated report stopped at the input byte budget"
        ));
        budget.scan_used = budget.scan_used.saturating_add(allowed);
        budget.decoded_used = budget.decoded_used.saturating_add(allowed);
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(member_size as usize);
    let mut buffer = [0; 64 * 1024];
    while bytes.len() < member_size as usize {
        let remaining = member_size as usize - bytes.len();
        let read_size = remaining.min(buffer.len());
        let read = reader.read(&mut buffer[..read_size])?;
        if read == 0 {
            diagnostics.push(format!(
                "{member}: associated report ended before its header size"
            ));
            return Ok(None);
        }
        budget.scan_used = budget.scan_used.saturating_add(read as u64);
        budget.decoded_used = budget.decoded_used.saturating_add(read as u64);
        bytes.extend_from_slice(&buffer[..read]);
    }
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(None),
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
        .map(str::to_owned);
    let identity = object
        .get("widget_session_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let author = report_message
        .and_then(|message| message["author"]["role"].as_str())
        .map(str::to_owned);
    let completion = widget_state
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| report_message.and_then(|message| message["status"].as_str()))
        .map(str::to_owned);
    let (body, citations) = report_message
        .map(|message| {
            let (parts, _) = message_parts(message);
            let body = parts.iter().any(|part| part.text().is_some()).then(|| {
                content::bounded_text(&content::project_text(&parts), MAX_ARTIFACT_BODY_CHARS)
            });
            (body, collect_citations(message))
        })
        .unwrap_or((None, Vec::new()));
    let mut reference = content::artifact_reference(&value, "openai-library-report")
        .unwrap_or_else(|| crate::content::ArtifactReference {
            kind: "openai-library-report".to_owned(),
            identity: None,
            origin: None,
            backing: None,
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
        });
    reference.identity = identity;
    reference.origin = Some("openai-widget-state".to_owned());
    reference.backing = backing.clone();
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
    Ok(Some(AssociatedReport {
        member: member.to_owned(),
        backing,
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

fn collect_citations(value: &Value) -> Vec<content::ArtifactCitation> {
    let mut citations = Vec::new();
    collect_citations_inner(value, 0, &mut citations);
    citations
}

fn collect_citations_inner(
    value: &Value,
    depth: usize,
    citations: &mut Vec<content::ArtifactCitation>,
) {
    if depth >= content::MAX_STRUCTURED_DEPTH || citations.len() >= content::MAX_ARTIFACT_REFERENCES
    {
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, value) in object {
        if key == "content_references" {
            collect_citation_values(value, depth + 1, citations);
        } else {
            collect_citations_inner(value, depth + 1, citations);
        }
    }
}

fn collect_citation_values(
    value: &Value,
    depth: usize,
    citations: &mut Vec<content::ArtifactCitation>,
) {
    if depth >= content::MAX_STRUCTURED_DEPTH {
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                collect_citation_values(value, depth + 1, citations);
                if citations.len() >= content::MAX_ARTIFACT_REFERENCES {
                    break;
                }
            }
        }
        Value::Object(object) => {
            if let Some(citation) = citation_from_value(object) {
                citations.push(citation);
            } else {
                for value in object.values() {
                    collect_citation_values(value, depth + 1, citations);
                    if citations.len() >= content::MAX_ARTIFACT_REFERENCES {
                        break;
                    }
                }
            }
        }
        _ => {}
    }
}

fn citation_from_value(
    value: &serde_json::Map<String, Value>,
) -> Option<content::ArtifactCitation> {
    let kind = value.get("type").and_then(Value::as_str).map(str::to_owned);
    let uri = ["uri", "url", "href"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str).map(str::to_owned));
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let start = ["start_idx", "start_index", "start"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
        .and_then(|value| usize::try_from(value).ok());
    let end = ["end_idx", "end_index", "end"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64))
        .and_then(|value| usize::try_from(value).ok());
    (kind.is_some() || uri.is_some() || title.is_some() || start.is_some() || end.is_some())
        .then_some(content::ArtifactCitation {
            kind,
            uri,
            title,
            start,
            end,
        })
}

fn attach_associated_reports(
    occurrences: &mut [InputOccurrence],
    reports: &[AssociatedReport],
    diagnostics: &mut Vec<String>,
) {
    for report in reports {
        let Some(backing) = report.backing.as_deref() else {
            diagnostics.push(format!(
                "associated report {} has no backing conversation and remains unjoined",
                report.member
            ));
            continue;
        };
        let matching = occurrences
            .iter()
            .filter(|occurrence| occurrence.session.id == backing)
            .count();
        if matching == 0 {
            diagnostics.push(format!(
                "associated report {} names an unreached backing conversation",
                report.member
            ));
            continue;
        }
        if matching > 1 {
            diagnostics.push(format!(
                "associated report {} names an ambiguous backing conversation {backing} and remains unjoined",
                report.member
            ));
            continue;
        }
        let occurrence = occurrences
            .iter_mut()
            .find(|occurrence| occurrence.session.id == backing)
            .expect("matching associated report occurrence");
        let mut artifact = report.reference.clone();
        let artifact_reference = RecordRef {
            domain: format!("{}:associated", occurrence.session.source.origin),
            revision: occurrence.evidence.source_revision.clone(),
            span: None,
            native_id: None,
            pointer: Some(format!("/associated/{}", report.member)),
            part_index: occurrence.artifacts.len(),
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
        part.set_record_ref(artifact_reference);
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
