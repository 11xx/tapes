//! Check a frozen workset against its sources and resolve the workset
//! references cited from its lanes. Each source is read once, pinned to the
//! bytes the workset froze, and that one pass both checks the pin and serves
//! every reference into the source in ordinal order.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read as _};
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, SourceChanged, StreamedTranscript};
use crate::model::{
    OperatorOrigin, RecordRef, SourceDescriptor, SourcePrefixDigest, Turn, TurnKind,
};
use crate::workset::MANIFEST_FILE;
use crate::{projection, Selection};

pub const LOCATE_SCHEMA: &str = "tapes-locate/1";
/// The most whole-turn text one resolved reference returns.
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
/// The most whole-turn text one locate report returns across references.
pub const MAX_REPORT_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// The largest manifest locate reads.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
/// The most references one refs file may hold.
pub const MAX_REFERENCES: usize = 100_000;
/// A refs line longer than this is malformed.
pub const MAX_REF_LINE_BYTES: usize = 64 * 1024;

/// The parts of a `tapes-workset/1` manifest locate needs.
#[derive(Debug, Deserialize)]
pub struct FrozenWorkset {
    pub schema: String,
    pub root: String,
    pub sources: Vec<FrozenSource>,
}

#[derive(Debug, Deserialize)]
pub struct FrozenSource {
    pub key: String,
    pub harness: String,
    pub session_id: String,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub source: Option<SourceDescriptor>,
    #[serde(default)]
    pub source_length: Option<u64>,
    #[serde(default)]
    pub source_revision: Option<String>,
    #[serde(default)]
    pub source_prefix_digest: Option<SourcePrefixDigest>,
    #[serde(default)]
    pub lanes: Option<Vec<FrozenLane>>,
    #[serde(default)]
    pub coverage: Option<FrozenCoverage>,
    #[serde(default)]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FrozenLane {
    pub id: String,
    pub first_turn: usize,
    pub last_turn: usize,
}

#[derive(Debug, Deserialize)]
pub struct FrozenCoverage {
    pub turns: usize,
}

/// One line of a refs file.
#[derive(Debug, Clone)]
pub enum RefLine {
    Malformed {
        line: usize,
    },
    Cited {
        line: usize,
        reference: String,
        lane: Option<String>,
    },
}

#[derive(Debug, Serialize)]
pub struct Locate {
    pub schema: &'static str,
    pub root: String,
    pub observed_at: DateTime<Utc>,
    /// Whether any source is `changed`.
    pub stale: bool,
    pub sources: Vec<SourceCheck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<Located>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<Outcome, usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceState {
    /// The frozen bytes, and nothing after them.
    Unchanged,
    /// The frozen bytes, with more appended after them.
    Grown,
    /// Shorter, rewritten within the frozen bytes, or replaced.
    Changed,
    Unreadable,
}

impl SourceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Grown => "grown",
            Self::Changed => "changed",
            Self::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SourceCheck {
    pub key: String,
    pub state: SourceState,
    /// The source length observed now, where the check established it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Resolved,
    Unknown,
    OutsideLane,
    SourceChanged,
    Malformed,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Unknown => "unknown",
            Self::OutsideLane => "outside-lane",
            Self::SourceChanged => "source-changed",
            Self::Malformed => "malformed",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Located {
    /// The refs file line, counted from one.
    pub line: usize,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The lane the citing line named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cited_lane: Option<String>,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<TurnKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<OperatorOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    /// The lane that holds the turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    /// The turn's whole projected text, up to the requested bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The whole projected text's length in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_truncated: Option<bool>,
}

impl Located {
    fn new(line: usize, reference: Option<String>, cited_lane: Option<String>) -> Self {
        Self {
            line,
            reference,
            cited_lane,
            outcome: Outcome::Unknown,
            source: None,
            ordinal: None,
            kind: None,
            origin: None,
            ts: None,
            lane: None,
            record_ref: None,
            text: None,
            text_bytes: None,
            text_truncated: None,
        }
    }
}

/// Read a bundle's manifest, refusing one that is missing, oversized, or
/// not a `tapes-workset/1` object.
pub fn read_manifest(bundle: &Path) -> Result<FrozenWorkset> {
    let path = bundle.join(MANIFEST_FILE);
    let file =
        std::fs::File::open(&path).map_err(|error| anyhow!("open {}: {error}", path.display()))?;
    let mut body = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|error| anyhow!("read {}: {error}", path.display()))?;
    if body.len() as u64 > MAX_MANIFEST_BYTES {
        bail!("{} exceeds {MAX_MANIFEST_BYTES} bytes", path.display());
    }
    let manifest: FrozenWorkset = serde_json::from_slice(&body)
        .map_err(|error| anyhow!("parse {}: {error}", path.display()))?;
    if manifest.schema != crate::workset::WORKSET_SCHEMA {
        bail!(
            "{} is {}, not {}",
            path.display(),
            manifest.schema,
            crate::workset::WORKSET_SCHEMA
        );
    }
    Ok(manifest)
}

/// Read a JSON-lines refs file. Each object's `ref` is a workset reference
/// and its optional `lane` a lane id; other fields are ignored. A blank,
/// non-JSON, overlong, or `ref`-less line is malformed and does not stop
/// the read.
pub fn read_refs(path: &Path) -> Result<Vec<RefLine>> {
    let file =
        std::fs::File::open(path).map_err(|error| anyhow!("open {}: {error}", path.display()))?;
    read_refs_from(BufReader::new(file))
        .map(|read| read.lines)
        .map_err(|error| anyhow!("read {}: {error:#}", path.display()))
}

/// The lines a refs read produced, and the most bytes it held for one line.
#[derive(Debug)]
pub struct RefsRead {
    pub lines: Vec<RefLine>,
    pub peak_line_bytes: usize,
}

/// [`read_refs`] over any buffered reader.
pub fn read_refs_from(mut reader: impl BufRead) -> Result<RefsRead> {
    let mut lines = Vec::new();
    let mut buffer = Vec::new();
    let mut peak_line_bytes = 0;
    let mut number = 0;
    loop {
        buffer.clear();
        let read = (&mut reader)
            .take(MAX_REF_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buffer)?;
        if read == 0 {
            break;
        }
        number += 1;
        let mut overlong = false;
        if buffer.len() > MAX_REF_LINE_BYTES && buffer.last() != Some(&b'\n') {
            overlong = true;
            skip_line(&mut reader)?;
        }
        peak_line_bytes = peak_line_bytes.max(buffer.len());
        if lines.len() == MAX_REFERENCES {
            bail!("more than {MAX_REFERENCES} references");
        }
        lines.push(if overlong {
            RefLine::Malformed { line: number }
        } else {
            parse_ref_line(number, &buffer)
        });
    }
    Ok(RefsRead {
        lines,
        peak_line_bytes,
    })
}

/// Discard through the next newline, a buffer-full at a time, holding none
/// of what it skips.
fn skip_line(reader: &mut impl BufRead) -> std::io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(at) => {
                reader.consume(at + 1);
                return Ok(());
            }
            None => {
                let length = available.len();
                reader.consume(length);
            }
        }
    }
}

fn parse_ref_line(line: usize, bytes: &[u8]) -> RefLine {
    #[derive(Deserialize)]
    struct Cited {
        #[serde(rename = "ref")]
        reference: String,
        #[serde(default)]
        lane: Option<String>,
    }
    match serde_json::from_slice::<Cited>(bytes) {
        Ok(cited) => RefLine::Cited {
            line,
            reference: cited.reference,
            lane: cited.lane,
        },
        Err(_) => RefLine::Malformed { line },
    }
}

/// Check every source of a frozen workset and resolve `refs` against it.
pub fn locate_with_backends(
    backends: &[Box<dyn Backend>],
    workset: &FrozenWorkset,
    refs: &[RefLine],
    text_bytes: usize,
) -> Result<Locate> {
    if text_bytes > MAX_TEXT_BYTES {
        bail!("--text-bytes must be at most {MAX_TEXT_BYTES} bytes; got {text_bytes}");
    }
    let observed_at = Utc::now();
    let keys: HashMap<&str, usize> = workset
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| (source.key.as_str(), index))
        .collect();

    let mut located = Vec::with_capacity(refs.len());
    // Per source, the references waiting on its read: ordinal -> located indexes.
    let mut wanted: Vec<BTreeMap<usize, Vec<usize>>> = vec![BTreeMap::new(); workset.sources.len()];
    for line in refs {
        match line {
            RefLine::Malformed { line } => {
                let mut entry = Located::new(*line, None, None);
                entry.outcome = Outcome::Malformed;
                located.push(entry);
            }
            RefLine::Cited {
                line,
                reference,
                lane,
            } => {
                let mut entry = Located::new(*line, Some(reference.clone()), lane.clone());
                let parsed = reference
                    .split_once('#')
                    .and_then(|(key, ordinal)| Some((keys.get(key)?, ordinal.parse().ok()?)));
                if let Some((&index, ordinal)) = parsed {
                    entry.source = Some(workset.sources[index].key.clone());
                    entry.ordinal = Some(ordinal);
                    wanted[index]
                        .entry(ordinal)
                        .or_default()
                        .push(located.len());
                }
                located.push(entry);
            }
        }
    }

    let root = workset.sources.first();
    let mut text_budget = MAX_REPORT_TEXT_BYTES;
    let mut checks = Vec::with_capacity(workset.sources.len());
    for (index, source) in workset.sources.iter().enumerate() {
        let frozen_turns = source.coverage.as_ref().map(|coverage| coverage.turns);
        let mut found: BTreeMap<usize, Hit> = BTreeMap::new();
        let want = &wanted[index];
        let check = check_source(backends, root, source, &mut |turn| {
            if frozen_turns.is_none_or(|turns| turn.ordinal >= turns)
                || !want.contains_key(&turn.ordinal)
            {
                return;
            }
            let text = (text_bytes > 0).then(|| {
                let whole = projection::project_whole(&source.key, &turn);
                let mut end = whole.len().min(text_bytes).min(text_budget);
                while !whole.is_char_boundary(end) {
                    end -= 1;
                }
                text_budget -= end;
                let length = whole.len();
                let mut whole = whole;
                whole.truncate(end);
                (whole, length)
            });
            found.insert(
                turn.ordinal,
                Hit {
                    kind: turn.kind,
                    origin: turn.origin,
                    ts: turn.ts,
                    record_ref: turn.record_ref,
                    text,
                },
            );
        });
        let usable = matches!(check.state, SourceState::Unchanged | SourceState::Grown);
        for (&ordinal, entries) in want {
            for &entry in entries {
                let entry = &mut located[entry];
                entry.outcome = classify(source, frozen_turns, usable, ordinal, entry);
                if entry.outcome != Outcome::Resolved {
                    continue;
                }
                let Some(hit) = found.get(&ordinal) else {
                    entry.outcome = Outcome::Unknown;
                    continue;
                };
                entry.kind = Some(hit.kind);
                entry.origin = hit.origin;
                entry.ts = hit.ts;
                entry.record_ref = hit.record_ref.clone();
                if let Some((text, length)) = &hit.text {
                    entry.text_bytes = Some(*length);
                    entry.text_truncated = Some(text.len() < *length);
                    entry.text = Some(text.clone());
                }
            }
        }
        checks.push(check);
    }

    let mut counts = BTreeMap::new();
    for entry in &located {
        *counts.entry(entry.outcome).or_default() += 1;
    }
    Ok(Locate {
        schema: LOCATE_SCHEMA,
        root: workset.root.clone(),
        observed_at,
        stale: checks
            .iter()
            .any(|check| check.state == SourceState::Changed),
        sources: checks,
        references: located,
        counts,
    })
}

/// What a resolved reference reports about its turn, kept from the read
/// instead of the turn itself.
struct Hit {
    kind: TurnKind,
    origin: Option<OperatorOrigin>,
    ts: Option<DateTime<Utc>>,
    record_ref: Option<RecordRef>,
    text: Option<(String, usize)>,
}

/// The outcome of one reference before its turn is looked up: anything but
/// `Resolved` is final.
fn classify(
    source: &FrozenSource,
    frozen_turns: Option<usize>,
    usable: bool,
    ordinal: usize,
    entry: &mut Located,
) -> Outcome {
    if !usable {
        return Outcome::SourceChanged;
    }
    if frozen_turns.is_none_or(|turns| ordinal >= turns) {
        return Outcome::Unknown;
    }
    let holding = source
        .lanes
        .iter()
        .flatten()
        .find(|lane| (lane.first_turn..=lane.last_turn).contains(&ordinal));
    entry.lane = holding.map(|lane| lane.id.clone());
    match &entry.cited_lane {
        Some(cited) if holding.is_none_or(|lane| &lane.id != cited) => Outcome::OutsideLane,
        _ => Outcome::Resolved,
    }
}

/// Re-open one source as the workset read it, pinned to its frozen bytes
/// where the workset recorded a prefix digest, handing each turn to `turn`.
fn check_source(
    backends: &[Box<dyn Backend>],
    root: Option<&FrozenSource>,
    source: &FrozenSource,
    turn: &mut dyn FnMut(Turn),
) -> SourceCheck {
    let check = |state, source_length, diagnostic| SourceCheck {
        key: source.key.clone(),
        state,
        source_length,
        diagnostic,
    };
    if source.coverage.is_none() {
        let diagnostic = source
            .diagnostic
            .as_deref()
            .unwrap_or("no turns were frozen");
        return check(
            SourceState::Unreadable,
            None,
            Some(format!(
                "not read when the workset was frozen: {diagnostic}"
            )),
        );
    }
    let pin = frozen_pin(source);
    let read = read_source(backends, root, source, pin.as_ref(), turn);
    let read = match read {
        Ok(read) => read,
        Err(error) => {
            let state = if error.chain().any(|cause| cause.is::<SourceChanged>()) {
                SourceState::Changed
            } else {
                SourceState::Unreadable
            };
            return check(state, None, Some(format!("{error:#}")));
        }
    };
    let frozen_length = source.source_length;
    if pin.is_none() {
        let same = Some(read.source_length) == frozen_length
            && read.source_revision == source.source_revision;
        let state = if same {
            SourceState::Unchanged
        } else {
            SourceState::Changed
        };
        return check(state, Some(read.source_length), None);
    }
    let locator = source
        .source
        .as_ref()
        .and_then(|descriptor| descriptor.location.as_ref())
        .map(|location| location.locator.clone());
    match locator.map(std::fs::metadata) {
        Some(Ok(metadata)) => {
            let state = if Some(metadata.len()) == frozen_length {
                SourceState::Unchanged
            } else {
                SourceState::Grown
            };
            check(state, Some(metadata.len()), None)
        }
        Some(Err(error)) => check(SourceState::Unreadable, None, Some(error.to_string())),
        None => check(
            SourceState::Unreadable,
            None,
            Some("the workset recorded no source locator".to_owned()),
        ),
    }
}

fn frozen_pin(source: &FrozenSource) -> Option<StreamedTranscript> {
    let digest = source.source_prefix_digest.as_ref()?;
    let length = source.source_length?;
    if digest.span.start != 0 || digest.span.end != length {
        return None;
    }
    let bytes = decode_sha256(&digest.value)?;
    Some(StreamedTranscript::frozen(
        length,
        source.source_revision.clone()?,
        bytes,
    ))
}

fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(bytes)
}

fn read_source(
    backends: &[Box<dyn Backend>],
    root: Option<&FrozenSource>,
    source: &FrozenSource,
    pin: Option<&StreamedTranscript>,
    turn: &mut dyn FnMut(Turn),
) -> Result<StreamedTranscript> {
    match &source.reference {
        Some(reference) => {
            let root = root.ok_or_else(|| anyhow!("the workset names no root"))?;
            let resolved = Selection::IdInHarness {
                id: &root.session_id,
                harness: &root.harness,
            }
            .resolve(backends)?;
            let mut ordinal = 0;
            let child = backends[resolved.backend_index].replay_child_transcript(
                &resolved.session,
                reference,
                pin,
                &mut |mut streamed| {
                    streamed.ordinal = ordinal;
                    ordinal += 1;
                    turn(streamed);
                    Ok(())
                },
            )?;
            Ok(child.read)
        }
        None => {
            let resolved = Selection::IdInHarness {
                id: &source.session_id,
                harness: &source.harness,
            }
            .resolve(backends)?;
            crate::stream_numbered(
                backends[resolved.backend_index].as_ref(),
                &resolved.session,
                pin,
                &mut |streamed| {
                    turn(streamed);
                    Ok(())
                },
            )
        }
    }
}
