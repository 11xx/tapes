use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, bounded_launch, default_native_store, enrich_native_session, head_directory,
    list_discovered_files, list_discovered_files_with_search, merge_launch, read_bounds,
    read_recording, replay_pin, skipped_records_note, stream_jsonl, timestamp, trailing_record,
    transcript_from_recording, ActivityRange, Backend, Jsonl, Listing, ParsedFile, Query,
    StreamedTranscript, TokenTotals, UnmappedTally,
};
use crate::content::{
    bounded_shape, text_part, tool_coverage, tool_part, ContentAvailability, ContentCarrier,
    ContentCoverage, ContentPart,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::lineage::{Lineage, ParentRef};
use crate::measurement::{Measured, MeasurementCompleteness, MeasurementReason};
use crate::model::{
    Accounting, AccountingBasis, AccountingCoverage, AccountingSelection, Cost, CostCoverage,
    KindDeclaration, Launch, Model, ReadGap, Role, Session, SourceDescriptor, Tokens,
    TrailingRecord, Transcript, Turn, TurnKind, TurnSelection, UserDefault,
};
use crate::usage::{Consumption, ConsumptionEvents, ConsumptionIdentity, ModelUsage, UsageDetail};
use agent_tapes_discovery::{Harness as NativeHarness, IdentityBasis, NativeSession, NativeStore};

mod consumption_keys;

#[derive(Clone, Debug)]
pub struct PiBackend {
    native_root: Option<PathBuf>,
    store: Option<NativeStore>,
    /// How much of a recording's end a transcript read takes.
    read_bytes: u64,
}

impl PiBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            store: Some(NativeStore::pi(root)),
            read_bytes: super::DEFAULT_READ_BYTES,
            native_root: None,
        }
    }

    pub(crate) fn from_store(store: Option<NativeStore>) -> Self {
        Self {
            store,
            read_bytes: super::DEFAULT_READ_BYTES,
            native_root: None,
        }
    }

    pub(crate) fn with_native_root(mut self, root: PathBuf) -> Self {
        self.native_root = Some(root);
        self
    }

    fn source(&self, path: &Path) -> SourceDescriptor {
        let mut source = SourceDescriptor::installed("pi", path.display().to_string());
        if let Some(root) = &self.native_root {
            source.kind = crate::model::SourceKind::ExplicitNativeStore;
            if let Some(location) = &mut source.location {
                location.container = Some(root.display().to_string());
            }
        }
        source
    }

    /// Read at most `read_bytes` from the end of each recording.
    pub fn with_read_bytes(self, read_bytes: u64) -> Self {
        Self { read_bytes, ..self }
    }

    fn parse(
        &self,
        native: &NativeSession,
        path: &Path,
    ) -> Result<(Session, Vec<Turn>, Jsonl, usize)> {
        let recording = read_recording(path, self.read_bytes)?;
        let (started_at, last_activity_at) = recording
            .time_range()
            .map_or((None, None), |(started, activity)| {
                (Some(started), Some(activity))
            });
        // pi writes the session header once, as the first line, and nothing
        // past it repeats the id or the working directory. The opening is the
        // only place either can be read on a file past the bounded tail.
        let opening = recording.opening();
        let read = &recording.tail;
        if opening.is_empty() && read.values.is_empty() && read.skipped > 0 {
            anyhow::bail!("recording-unreadable");
        }
        let mut identity_seen = false;
        for header in opening.iter().filter(|value| value["type"] == "session") {
            identity_seen = true;
            let id = header["id"]
                .as_str()
                .ok_or_else(|| anyhow!("{} has an invalid session identity", path.display()))?;
            if id != native.id() {
                anyhow::bail!(
                    "Pi session header identity {id} disagrees with selected native identity {}",
                    native.id()
                );
            }
        }
        if native.identity_basis() == Some(IdentityBasis::Header) && !identity_seen {
            anyhow::bail!(
                "{} no longer contains the native session identity selected during discovery",
                path.display()
            );
        }
        let entries = read
            .values
            .iter()
            .filter(|value| value["type"] != "session")
            .collect::<Vec<_>>();
        let (active, rooted) = active_branch(&entries);
        let active_ids = active
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect::<HashSet<_>>();
        let abandoned = entries
            .iter()
            .filter(|entry| {
                entry["id"]
                    .as_str()
                    .is_some_and(|id| !active_ids.contains(id))
            })
            .count();
        let directory = opening.iter().find_map(pi_cwd).map(PathBuf::from);
        let variant = active.iter().rev().find_map(|value| pi_thinking(value));
        let model = active
            .iter()
            .rev()
            .find_map(|value| pi_model(value))
            .map(|id| Model {
                id: id.to_owned(),
                variant: variant.map(str::to_owned),
            });
        let mut thinking = None;
        let turns = active
            .iter()
            .flat_map(|value| pi_turns(value, &mut thinking))
            .collect::<Vec<_>>();
        let (model_observation, model_selections) = pi_selections(&active, read);
        let mut session = Session {
            id: native.id().to_owned(),
            source: self.source(path),
            metadata: None,
            model,
            model_observation,
            model_selections,
            launch: recording_launch(&recording, native.id()),
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory,
            started_at,
            last_activity_at,
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            start_uncertain: recording.start_uncertain(),
            occurrence: None,
            usage_detail: None,
        };
        apply_recording_usage(&mut session, &recording, &active, rooted);
        // pi rewinds by appending a new branch, so the first user message in
        // the file may sit on a root the active path never reaches. The
        // active path is known only when the whole file is in the tail; past
        // the bound the hint stays absent rather than naming an abandoned
        // prompt as the session's first.
        let session = if read.truncated {
            session
        } else {
            session.with_derived_title(&turns)
        };

        Ok((session, turns, recording.tail, abandoned))
    }
}

impl Default for PiBackend {
    fn default() -> Self {
        Self::from_store(default_native_store(NativeHarness::Pi))
    }
}

impl Backend for PiBackend {
    fn kinds(&self) -> KindDeclaration {
        KindDeclaration {
            recordable: TurnSelection::only([
                TurnKind::Operator,
                TurnKind::Assistant,
                TurnKind::Reasoning,
                TurnKind::Tool,
            ]),
            user_default: Some(UserDefault {
                kind: TurnKind::Operator,
                basis: "pi records its model changes, compactions, and extension messages as entries of their own types, so its user role holds what the operator sent".to_owned(),
            }),
        }
    }

    fn harness(&self) -> &'static str {
        "pi"
    }

    fn available(&self) -> bool {
        self.store.as_ref().is_some_and(NativeStore::available)
    }

    fn list_titles(&self, _query: &Query) -> Result<Listing> {
        // This backend supplies display hints, not recorded titles.
        Ok(Listing::default())
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let Some(store) = self.store.as_ref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_discovered_files(
            store,
            query,
            |native, path| head_directory(path, native, pi_cwd),
            |native, path| self.parse(native, path).map(|(session, _, _, _)| session),
        );
        listing
            .sessions
            .sort_by_key(|session| session.last_activity_at);
        listing.sessions.reverse();
        Ok(listing)
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        let Some(store) = self.store.as_ref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_discovered_files_with_search(
            store,
            query,
            needle,
            tail,
            self.read_bytes,
            |native, path| head_directory(path, native, pi_cwd),
            |native, path| {
                self.parse(native, path)
                    .map(|(session, turns, read, _)| ParsedFile {
                        session,
                        turns,
                        truncated: read.truncated,
                    })
            },
        );
        listing
            .sessions
            .sort_by_key(|session| session.last_activity_at);
        listing.sessions.reverse();
        Ok(listing)
    }

    /// Resolve native identity before normalizing the bounded transcript.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let Some(native) = store.locate_exact(id)? else {
            return Ok(None);
        };
        let path = native
            .locator()
            .ok_or_else(|| anyhow!("pi native session {} has no file locator", native.id()))?;
        let (session, _, _, _) = self.parse(&native, path)?;
        Ok(Some(enrich_native_session(&native, session)?))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("pi session {} has no file locator", session.id))?;
        let (mut turns, recording, abandoned, trailing_record, unmapped) =
            read_transcript(path, self.read_bytes, &session.id)?;
        let entries = recording
            .tail
            .values
            .iter()
            .filter(|v| v["type"] != "session")
            .collect::<Vec<_>>();
        let (active, rooted) = active_branch(&entries);
        let mut session = session.clone();
        apply_recording_usage(&mut session, &recording, &active, rooted);
        session.launch = recording_launch(&recording, &session.id);
        (session.model_observation, session.model_selections) =
            pi_selections(&active, &recording.tail);
        session.model = active
            .iter()
            .rev()
            .find_map(|v| pi_model(v))
            .map(|id| Model {
                id: id.to_owned(),
                variant: active
                    .iter()
                    .rev()
                    .find_map(|v| pi_thinking(v))
                    .map(str::to_owned),
            });
        for turn in &mut turns {
            pi_origin(turn, session.launch);
        }
        let mut transcript = transcript_from_recording(
            session,
            turns,
            tail,
            &recording,
            None,
            trailing_record,
            abandoned_notes(abandoned),
        );
        if let Some(read) = &mut transcript.read {
            read.unmapped = Some(unmapped);
        }
        Ok(transcript)
    }

    fn supports_full_search(&self) -> bool {
        true
    }

    fn consumption_event_keys(
        &self,
        session: &Session,
        read: Option<&crate::model::ReadEvidence>,
        options: crate::usage::ConsumptionKeyOptions,
    ) -> Result<crate::usage::ConsumptionEventKeys> {
        consumption_keys::read_keys(self, session, read, options)
    }

    fn stream_transcript(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("pi session {} has no file locator", session.id))?;
        // The active branch is the last entry's ancestry. The first pass keeps
        // only each entry's position, id, and parent; the second emits the
        // active entries' turns in recording order.
        let mut positions = HashMap::<String, (usize, Option<String>)>::new();
        let mut launch = None;
        let mut last = None;
        let mut entries = 0;
        let first = stream_jsonl(path, replay_pin(replay)?, |value, _, _| {
            super::validate_native_identity(value, "pi", &session.id)?;
            launch = merge_launch(launch, pi_launch_entry(value, &session.id));
            if value["type"] == "session" {
                return Ok(false);
            }
            let id = value["id"].as_str().map(str::to_owned);
            let parent = value["parentId"].as_str().map(str::to_owned);
            if let Some(id) = &id {
                positions.insert(id.clone(), (entries, parent.clone()));
            }
            last = Some((entries, id, parent));
            entries += 1;
            Ok(false)
        })?;
        let mut active = HashSet::new();
        let mut active_ids = HashSet::new();
        let mut current = last;
        while let Some((position, Some(id), parent)) = current.take() {
            if !active_ids.insert(id) {
                break;
            }
            active.insert(position);
            current = parent.and_then(|parent| {
                let (position, grandparent) = positions.get(&parent)?;
                Some((*position, Some(parent), grandparent.clone()))
            });
        }
        drop(positions);

        let domain = format!("file:{}", path.display());
        let mut position = 0;
        let mut abandoned = 0;
        let mut trailing_record = None;
        let mut unmapped = UnmappedTally::default();
        let mut thinking = None;
        let read = stream_jsonl(path, Some(first.pin()), |value, span, revision| {
            super::validate_native_identity(value, "pi", &session.id)?;
            if value["type"] == "session" {
                count_unmapped(&mut unmapped, value);
                return Ok(false);
            }
            let entry = position;
            position += 1;
            if value["id"]
                .as_str()
                .is_some_and(|id| !active_ids.contains(id))
            {
                abandoned += 1;
            }
            if !active.contains(&entry) {
                return Ok(false);
            }
            let mut parsed = pi_turns(value, &mut thinking);
            if parsed.is_empty() {
                count_unmapped(&mut unmapped, value);
            }
            super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
            let produced = !parsed.is_empty();
            trailing_record = (!produced)
                .then(|| pi_trailing_kind(value))
                .flatten()
                .map(|kind| TrailingRecord {
                    kind: kind.to_owned(),
                    timestamp: timestamp(&value["timestamp"]),
                });
            for mut parsed_turn in parsed {
                pi_origin(&mut parsed_turn, launch);
                turn(parsed_turn)?;
            }
            Ok(produced)
        })?;
        Ok(StreamedTranscript {
            coordinates: super::StreamCoordinates::FileBytes,
            source_length: read.source_length,
            source_bounds: Vec::new(),
            source_revision: Some(read.revision),
            source_prefix_sha256: Some(read.prefix_sha256),
            skipped: read.skipped,
            gaps: read.gaps,
            trailing_record,
            terminal: None,
            notes: abandoned_notes(abandoned),
            unmapped: Some(unmapped.finish()),
            kinds: None,
        })
    }

    /// pi records a relationship on the session that has one: its header
    /// names the session it came from. A recording names no children of its
    /// own, so a parent's list of them stays empty.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let path = self.recording(session)?;
        let recording = read_recording(&path, self.read_bytes)?;
        super::validate_recording_identity(&recording, "pi", &session.id)?;
        let parent = recording
            .opening()
            .iter()
            .find(|value| value["type"] == "session")
            .and_then(|value| value["parentSession"].as_str())
            .map(|reference| self.parent_ref(reference))
            .transpose()?
            .flatten();
        let uncertain = recording
            .opening()
            .iter()
            .any(|value| pi_parent_session(value) == Some(None));
        Ok(Lineage {
            parent,
            notes: uncertain
                .then(|| "Fork parent identity is unestablished.".to_owned())
                .into_iter()
                .collect(),
            truncation: read_bounds(&recording.tail),
            ..Lineage::default()
        })
    }

    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        let path = self.recording(session)?;
        let mut header = None;
        let read = stream_jsonl(&path, None, |value, _, _| {
            super::validate_native_identity(value, "pi", &session.id)?;
            if header.is_none() && value["type"] == "session" {
                header = Some(value["parentSession"].as_str().map(str::to_owned));
            }
            Ok(false)
        })?;
        let uncertain = header
            .as_ref()
            .and_then(|value| value.as_ref())
            .is_some_and(|reference| pi_parent_id(reference).is_none());
        let parent = header
            .flatten()
            .map(|reference| self.parent_ref(&reference))
            .transpose()?
            .flatten();
        Ok(Lineage {
            parent,
            notes: skipped_records_note(read.skipped)
                .into_iter()
                .chain(uncertain.then(|| "Fork parent identity is unestablished.".to_owned()))
                .collect(),
            ..Lineage::default()
        })
    }

    /// pi sums per-request usage along the active branch, which is the last
    /// entry's ancestry, and consumption over every entry. One pass keeps each
    /// entry's place in the tree and the facts it carries; the branch is
    /// walked once the last entry is known.
    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        let path = self.recording(session)?;
        let mut entries = Vec::<PiEntry>::new();
        let mut positions = HashMap::<String, usize>::new();
        let mut activity = ActivityRange::default();
        let mut launch = None;
        let mut forked_from = None;
        let streamed = stream_jsonl(&path, Some(read.pin()?), |value, span, _| {
            super::validate_native_identity(value, "pi", &session.id)?;
            activity.observe(value);
            launch = merge_launch(launch, pi_launch_entry(value, &session.id));
            if value["type"] == "session" {
                if forked_from.is_none() {
                    forked_from = pi_parent_session(value);
                }
                return Ok(false);
            }
            let id = value["id"].as_str().map(str::to_owned);
            if let Some(id) = &id {
                positions.insert(id.clone(), entries.len());
            }
            entries.push(PiEntry {
                id,
                parent: value["parentId"].as_str().map(str::to_owned),
                consumed: pi_consumed(value),
                model: pi_model(value).map(str::to_owned),
                thinking: pi_thinking(value).map(str::to_owned),
                timestamp: timestamp(&value["timestamp"]),
                end: span.end,
            });
            Ok(false)
        })?;
        let mut active = Vec::new();
        let mut seen = HashSet::new();
        let mut rooted = false;
        let mut current = entries.len().checked_sub(1);
        while let Some(position) = current {
            let entry = &entries[position];
            let Some(id) = entry.id.as_deref() else {
                break;
            };
            if !seen.insert(id) {
                break;
            }
            active.push(position);
            rooted = entry.parent.is_none();
            current = entry
                .parent
                .as_deref()
                .and_then(|parent| positions.get(parent).copied());
        }
        active.reverse();
        let coverage = branch_coverage(
            rooted,
            entries.last().map(|entry| entry.end),
            &streamed.gaps,
        );
        let mut consumption = PiConsumption::default();
        for entry in &entries {
            if let Some(consumed) = &entry.consumed {
                let id = entry.id.as_deref();
                consumption.observe(id, consumed, id.is_some_and(|id| seen.contains(id)));
            }
        }
        let consumption =
            consumption.finish(AccountingCoverage::Session, streamed.skipped, forked_from);

        let mut usage = PiUsage::default();
        let mut model = None;
        let mut variant = None;
        let mut selections = crate::model_observation::Selections::default();
        for entry in active.iter().map(|position| &entries[*position]) {
            if let Some(consumed) = &entry.consumed {
                usage.add(entry.id.as_deref(), consumed, variant.as_deref());
            }
            if let Some(id) = &entry.model {
                selections.observe(
                    Model {
                        id: id.clone(),
                        variant: variant.clone(),
                    },
                    entry.timestamp,
                    entry.id.as_deref(),
                );
                model.clone_from(&entry.model);
            }
            if entry.thinking.is_some() {
                variant.clone_from(&entry.thinking);
            }
        }
        let usage = usage.finish();
        let tokens = usage.tokens;
        let cost = usage.cost;
        let mut whole = session.clone();
        whole.accounting = branch_accounting(tokens.as_ref(), cost.as_ref(), coverage);
        whole.tokens = tokens;
        whole.cost = cost;
        whole.usage_detail = UsageDetail {
            by_model: usage.by_model,
            cost_coverage: usage.cost_coverage,
            measurement_coverage: usage.measurement_coverage,
            measurement_incomplete: usage.measurement_incomplete,
            measurement_completeness: usage.measurement_completeness,
            consumption,
            ..UsageDetail::default()
        }
        .into_option();
        whole.model = model.map(|id| Model { id, variant });
        if !read.gaps.is_empty() {
            selections.interrupt();
        }
        (whole.model_observation, whole.model_selections) = selections.finish(true);
        whole.launch = launch;
        activity.apply(&mut whole);
        Ok(whole)
    }
}

impl PiBackend {
    fn recording(&self, session: &Session) -> Result<PathBuf> {
        super::session_file_path(session)
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("pi session {} has no file locator", session.id))
    }

    /// pi records a relationship on the session that has one: its header
    /// names the session it came from.
    fn parent_ref(&self, reference: &str) -> Result<Option<ParentRef>> {
        let Some(native_id) = pi_parent_id(reference) else {
            return Ok(None);
        };
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("pi store is unavailable"))?;
        let native = store.locate_exact(&native_id)?;
        let resolved = native.is_some_and(|native| {
            if reference == native_id {
                return true;
            }
            native
                .locator()
                .and_then(|path| path.canonicalize().ok())
                .zip(Path::new(reference).canonicalize().ok())
                .is_some_and(|(located, referenced)| located == referenced)
        });
        Ok(Some(ParentRef {
            resolved,
            native_id,
            source: "session.parentSession".to_owned(),
        }))
    }
}

/// One entry's place in pi's branch tree and the session facts it carries.
struct PiEntry {
    id: Option<String>,
    parent: Option<String>,
    consumed: Option<PiConsumed>,
    model: Option<String>,
    thinking: Option<String>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
    /// Where the entry's record ends in the file.
    end: u64,
}

type PiTranscriptRead = (
    Vec<Turn>,
    super::Recording,
    usize,
    Option<TrailingRecord>,
    crate::model::UnmappedRecords,
);

fn read_transcript(path: &Path, read_bytes: u64, expected: &str) -> Result<PiTranscriptRead> {
    let recording = read_recording(path, read_bytes)?;
    super::validate_recording_identity(&recording, "pi", expected)?;
    let read = &recording.tail;
    let mut unmapped = UnmappedTally::default();
    for header in read
        .values
        .iter()
        .filter(|value| value["type"] == "session")
    {
        count_unmapped(&mut unmapped, header);
    }
    let entries = read
        .values
        .iter()
        .filter(|value| value["type"] != "session")
        .collect::<Vec<_>>();
    let active = active_path(&entries);
    let active_ids = active
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect::<HashSet<_>>();
    let abandoned = entries
        .iter()
        .filter(|entry| {
            entry["id"]
                .as_str()
                .is_some_and(|id| !active_ids.contains(id))
        })
        .count();
    let mut turns = Vec::new();
    let mut last_turn = None;
    let mut thinking = None;
    for (index, value) in active.iter().enumerate() {
        let mut parsed = pi_turns(value, &mut thinking);
        if parsed.is_empty() {
            count_unmapped(&mut unmapped, value);
        }
        let span = read
            .values
            .iter()
            .position(|candidate| std::ptr::eq(candidate, *value))
            .and_then(|source_index| read.spans.get(source_index).copied());
        super::attach_record_refs(
            &mut parsed,
            &format!("file:{}", path.display()),
            Some(&read.source_revision),
            span,
        );
        if !parsed.is_empty() {
            last_turn = Some(index);
        }
        turns.extend(parsed);
    }
    let trailing_record = trailing_record(active.iter().copied(), last_turn, pi_trailing_kind);

    Ok((
        turns,
        recording,
        abandoned,
        trailing_record,
        unmapped.finish(),
    ))
}

/// Count an active-branch entry, or the session header, that produced no
/// turn under its `type`. The reader declines the header and the model and
/// thinking-level changes it reads into the session; an entry of any other
/// type carries content no view holds.
fn count_unmapped(tally: &mut UnmappedTally, value: &Value) {
    let kind = value["type"].as_str().unwrap_or("(untyped)");
    let declined = matches!(kind, "session" | "model_change" | "thinking_level_change")
        || is_launch_entry(value);
    tally.add(kind.to_owned(), declined);
}

/// The `customType` of the entry tapes' pi extension appends each time a pi
/// process opens a session. Its `data.mode` is pi's run mode and its
/// `data.session` the session it was written for.
pub const LAUNCH_ENTRY_TYPE: &str = "tapes.launch";

fn is_launch_entry(value: &Value) -> bool {
    value["type"] == "custom" && value["customType"] == LAUNCH_ENTRY_TYPE
}

/// The terminal UI is a person at the keyboard; print and JSON modes are a
/// program. RPC serves editor front ends and programs alike, so it is no
/// evidence either way. An entry written for another session is a fork's
/// inheritance from its parent and says nothing about this one.
fn pi_launch_entry(value: &Value, session_id: &str) -> Option<Launch> {
    if !is_launch_entry(value) || value["data"]["session"].as_str()? != session_id {
        return None;
    }
    match value["data"]["mode"].as_str()? {
        "tui" => Some(Launch::Interactive),
        "print" | "json" => Some(Launch::Headless),
        _ => None,
    }
}

/// Launch evidence from both bounded windows of a recording.
fn recording_launch(recording: &super::Recording, session_id: &str) -> Option<Launch> {
    let launch = recording
        .opening()
        .iter()
        .chain(&recording.tail.values)
        .fold(None, |current, value| {
            merge_launch(current, pi_launch_entry(value, session_id))
        });
    bounded_launch(launch, recording.tail.truncated)
}

fn abandoned_notes(abandoned: usize) -> Vec<String> {
    match abandoned {
        0 => Vec::new(),
        1 => vec!["1 entry belongs to an abandoned branch.".to_owned()],
        count => vec![format!("{count} entries belong to abandoned branches.")],
    }
}

/// pi records the working directory once, on the `session` header line.
fn pi_cwd(value: &Value) -> Option<&str> {
    (value["type"] == "session")
        .then(|| value["cwd"].as_str())
        .flatten()
}

/// The session a fork's header names as the one it was copied from.
fn pi_parent_session(value: &Value) -> Option<Option<String>> {
    (value["type"] == "session")
        .then(|| value["parentSession"].as_str())
        .flatten()
        .map(pi_parent_id)
}

/// Paths and timestamp-prefixed basenames contribute their filename-derived
/// identity. Other bare references are ids, including `parent.jsonl`. Pi ids begin
/// and end with an ASCII alphanumeric and allow dots, hyphens, and underscores
/// inside; a normalized parent id holds at most 128 bytes.
fn pi_parent_id(reference: &str) -> Option<String> {
    let id = if reference.contains('/') || reference.contains('\\') {
        agent_tapes_discovery::pi_recording_id(Path::new(reference))?
    } else {
        agent_tapes_discovery::pi_recording_id(Path::new(reference)).unwrap_or(reference)
    };
    (id.len() <= 128
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')))
    .then(|| id.to_owned())
}

/// The accounting of requests summed along the active branch.
fn branch_accounting(
    tokens: Option<&Tokens>,
    cost: Option<&Cost>,
    coverage: AccountingCoverage,
) -> Option<Accounting> {
    accounting_for(tokens, cost, AccountingBasis::SummedRequests, coverage).map(|accounting| {
        Accounting {
            selection: Some(AccountingSelection::ActiveBranch),
            ..accounting
        }
    })
}

/// The active branch is whole when its walk from the leaf reached an entry
/// naming no parent and no record after the leaf went undecoded, since an
/// undecoded record there could have been the leaf.
fn branch_coverage(rooted: bool, leaf_end: Option<u64>, gaps: &[ReadGap]) -> AccountingCoverage {
    let whole = rooted && leaf_end.is_some_and(|end| gaps.iter().all(|gap| gap.span.start < end));
    if whole {
        AccountingCoverage::Session
    } else {
        AccountingCoverage::ReadWindow
    }
}

fn apply_recording_usage(
    session: &mut Session,
    recording: &super::Recording,
    active: &[&Value],
    rooted: bool,
) {
    let read = &recording.tail;
    let active_ids = active
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect::<HashSet<_>>();
    let leaf_end = read
        .values
        .iter()
        .zip(&read.spans)
        .rfind(|(value, _)| value["type"] != "session")
        .map(|(_, span)| span.end);
    let mut consumption = PiConsumption::default();
    for entry in &read.values {
        if let Some(consumed) = pi_consumed(entry) {
            let id = entry["id"].as_str();
            consumption.observe(id, &consumed, id.is_some_and(|id| active_ids.contains(id)));
        }
    }
    let consumption = consumption.finish(
        if read.truncated {
            AccountingCoverage::ReadWindow
        } else {
            AccountingCoverage::Session
        },
        read.skipped,
        recording.opening().iter().find_map(pi_parent_session),
    );
    let usage = pi_usage(active);
    session.accounting = branch_accounting(
        usage.tokens.as_ref(),
        usage.cost.as_ref(),
        branch_coverage(rooted, leaf_end, &read.gaps),
    );
    session.tokens = usage.tokens;
    session.cost = usage.cost;
    session.usage_detail = UsageDetail {
        by_model: usage.by_model,
        cost_coverage: usage.cost_coverage,
        measurement_coverage: usage.measurement_coverage,
        measurement_incomplete: usage.measurement_incomplete,
        measurement_completeness: usage.measurement_completeness,
        consumption,
        ..UsageDetail::default()
    }
    .into_option();
}

fn pi_usage(entries: &[&Value]) -> PiUsageResult {
    let mut usage = PiUsage::default();
    let mut variant = None;
    for entry in entries {
        if let Some(level) = pi_thinking(entry) {
            variant = Some(level);
        }
        if let Some(consumed) = pi_consumed(entry) {
            usage.add(entry["id"].as_str(), &consumed, variant);
        }
    }
    usage.finish()
}

/// The usage recorded on one consumption entry.
#[derive(Clone, Copy, Default, serde::Serialize)]
struct PiRequest {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    /// Native total retained for per-event pricing classification.
    total_tokens: Option<u64>,
    cost: Option<f64>,
}

impl PiRequest {
    fn is_unpriced(&self) -> bool {
        self.cost == Some(0.0)
            && [
                self.input,
                self.output,
                self.reasoning,
                self.cache_read,
                self.cache_write,
                self.total_tokens,
            ]
            .into_iter()
            .flatten()
            .any(|count| count > 0)
    }
}

/// The counters of one usage object, in the shape pi writes on every entry
/// that records consumption.
fn pi_usage_counters(usage: &Value) -> Option<PiRequest> {
    let usage = usage.as_object()?;
    let counter = |name: &str| usage.get(name).and_then(Value::as_u64);
    Some(PiRequest {
        input: counter("input"),
        output: counter("output"),
        reasoning: counter("reasoning"),
        cache_read: counter("cacheRead"),
        cache_write: counter("cacheWrite"),
        total_tokens: counter("totalTokens"),
        cost: usage
            .get("cost")
            .and_then(Value::as_object)
            .and_then(|cost| cost.get("total"))
            .and_then(Value::as_f64),
    })
}

/// Known counters accumulated from consumed entries, with missing or unpriced
/// costs recorded independently of the known estimate subtotal.
#[derive(Default)]
struct PiTotals {
    tokens: TokenTotals,
    cost: f64,
    cost_missing: bool,
    zero_written: usize,
}

impl PiTotals {
    fn add(&mut self, request: &PiRequest) {
        self.tokens.add(
            request.input,
            request.output,
            request.reasoning,
            request.cache_read,
            request.cache_write,
        );
        match request.cost {
            Some(0.0) if request.is_unpriced() => {
                self.zero_written += 1;
                self.cost_missing = true;
            }
            Some(cost) => self.cost += cost,
            None => self.cost_missing = true,
        }
    }

    fn cost_coverage(&self) -> Option<CostCoverage> {
        self.cost_missing.then_some(CostCoverage {
            complete: false,
            zero_written_requests: self.zero_written,
            zero_written_observations: self.zero_written,
        })
    }
}

/// Active consumption counted once, attributed only to a recorded model and
/// the reasoning level the latest `thinking_level_change` set. Missing model
/// attribution leaves the model split absent.
#[derive(Default)]
struct PiUsage {
    totals: PiConsumption,
    unattributed: usize,
    by_model: BTreeMap<PiModelKey, PiConsumption>,
}

#[derive(Default)]
struct PiUsageResult {
    tokens: Option<Tokens>,
    cost: Option<Cost>,
    by_model: Option<Vec<ModelUsage>>,
    cost_coverage: Option<CostCoverage>,
    measurement_coverage: BTreeMap<String, crate::usage::MeasurementCoverage>,
    measurement_incomplete: Vec<String>,
    measurement_completeness: MeasurementCompleteness,
}

/// The model and reasoning level one request is attributed to, ordered by
/// model id and then level so one recording reads the same way twice.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct PiModelKey {
    model: String,
    variant: Option<String>,
}

impl PiUsage {
    fn add(&mut self, id: Option<&str>, consumed: &PiConsumed, variant: Option<&str>) {
        if !self.totals.observe(id, consumed, true) {
            return;
        }
        match consumed.model.as_deref() {
            Some(model) => {
                self.by_model
                    .entry(PiModelKey {
                        model: model.to_owned(),
                        variant: variant.map(str::to_owned),
                    })
                    .or_default()
                    .observe(id, consumed, true);
            }
            None => self.unattributed += 1,
        }
    }

    fn finish(self) -> PiUsageResult {
        let Some(totals) = self.totals.finish(AccountingCoverage::Session, 0, None) else {
            return PiUsageResult::default();
        };
        let by_model = (self.unattributed == 0)
            .then(|| {
                self.by_model
                    .into_iter()
                    .map(|(key, totals)| {
                        let requests = totals.assistant_requests;
                        let totals = totals.finish(AccountingCoverage::Session, 0, None).unwrap();
                        ModelUsage {
                            measurement_completeness: totals.measurement_completeness,
                            model: key.model,
                            variant: key.variant,
                            tokens: totals.tokens,
                            cost: totals.cost,
                            cost_coverage: totals.cost_coverage,
                            request_count: Some(requests),
                            measurement_coverage: totals.measurement_coverage,
                            measurement_incomplete: totals.incomplete,
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|models| !models.is_empty());
        PiUsageResult {
            tokens: totals.tokens,
            cost: totals.cost,
            by_model,
            cost_coverage: totals.cost_coverage,
            measurement_coverage: totals.measurement_coverage,
            measurement_incomplete: totals.incomplete,
            measurement_completeness: totals.measurement_completeness,
        }
    }
}

/// One consumption event an entry records: an assistant message's request,
/// or the usage pi writes on a tool result, a compaction, a branch summary,
/// or an entry of type `usage`.
#[derive(serde::Serialize)]
struct PiConsumed {
    usage: PiRequest,
    assistant_request: bool,
    model: Option<String>,
    /// The provider and the response identity it returned, which an
    /// assistant message records when the provider supplied one.
    response: Option<(String, String)>,
    /// The request ended on an error or an abort, so its usage stopped with it.
    stopped: bool,
}

fn pi_consumed(entry: &Value) -> Option<PiConsumed> {
    let message = &entry["message"];
    let assistant = entry["type"] == "message" && message["role"] == "assistant";
    let usage = match entry["type"].as_str()? {
        "message"
            if assistant || (message["role"] == "toolResult" && message.get("usage").is_some()) =>
        {
            &message["usage"]
        }
        "compaction" | "branch_summary" | "usage" => &entry["usage"],
        _ => return None,
    };
    let response = assistant
        .then(|| {
            Some((
                message["provider"]
                    .as_str()
                    .filter(|id| !id.is_empty())?
                    .to_owned(),
                message["responseId"]
                    .as_str()
                    .filter(|id| !id.is_empty())?
                    .to_owned(),
            ))
        })
        .flatten();
    Some(PiConsumed {
        usage: pi_usage_counters(usage).unwrap_or_default(),
        assistant_request: assistant,
        model: if assistant {
            message["model"].as_str()
        } else if entry["type"] == "usage" {
            entry["model"].as_str()
        } else {
            None
        }
        .map(str::to_owned),
        response,
        stopped: assistant && matches!(message["stopReason"].as_str(), Some("error" | "aborted")),
    })
}

/// Consumption events summed in recording order, each once. An entry whose
/// id or provider response was already counted is a repeat of that event.
#[derive(Default)]
struct PiConsumption {
    totals: PiTotals,
    events: ConsumptionEvents,
    identity: ConsumptionIdentity,
    stopped: usize,
    zero_written_requests: usize,
    assistant_requests: usize,
    measurement_coverage: BTreeMap<String, crate::usage::MeasurementCoverage>,
    entries: HashSet<String>,
    responses: HashSet<(String, String)>,
}

impl PiConsumption {
    fn observe(&mut self, id: Option<&str>, consumed: &PiConsumed, on_branch: bool) -> bool {
        let id = id.filter(|id| !id.is_empty());
        let repeat = id.is_some_and(|id| !self.entries.insert(id.to_owned()))
            | consumed
                .response
                .as_ref()
                .is_some_and(|response| !self.responses.insert(response.clone()));
        if repeat {
            self.events.repeats += 1;
            return false;
        }
        let usage = &consumed.usage;
        for (name, value) in [
            ("input", usage.input),
            ("output", usage.output),
            ("reasoning", usage.reasoning),
            ("cache_read", usage.cache_read),
            ("cache_write", usage.cache_write),
        ] {
            let coverage = self
                .measurement_coverage
                .entry(name.to_owned())
                .or_default();
            if value.is_some() {
                coverage.measured += 1;
            } else {
                coverage.missing += 1;
            }
        }
        let unpriced = usage.is_unpriced();
        let coverage = self
            .measurement_coverage
            .entry("cost".to_owned())
            .or_default();
        if unpriced {
            coverage.unpriced += 1;
            self.zero_written_requests += usize::from(consumed.assistant_request);
        } else if usage.cost.is_none() {
            coverage.missing += 1;
        } else {
            coverage.measured += 1;
        }
        self.totals.add(usage);
        self.events.counted += 1;
        self.assistant_requests += usize::from(consumed.assistant_request);
        if on_branch {
            self.events.active_branch += 1;
        } else {
            self.events.other_branches += 1;
        }
        match (&consumed.response, id) {
            (Some(_), _) => self.identity.provider_response += 1,
            (None, Some(_)) => self.identity.recording_entry += 1,
            (None, None) => self.identity.unidentified += 1,
        }
        if consumed.stopped {
            self.stopped += 1;
        }
        true
    }

    /// The figure, absent when the read reached no event. `coverage` is how
    /// much of the recording the read reached and `unreadable` the records in
    /// it that could not be decoded.
    fn finish(
        self,
        coverage: AccountingCoverage,
        unreadable: usize,
        forked_from: Option<Option<String>>,
    ) -> Option<Consumption> {
        if self.events.counted == 0 {
            return None;
        }
        let mut incomplete = Vec::new();
        match unreadable {
            0 => {}
            1 => incomplete.push("1 record could not be read".to_owned()),
            count => incomplete.push(format!("{count} records could not be read")),
        }
        match self.stopped {
            0 => {}
            1 => incomplete.push(
                "1 request stopped on an error or an abort; its recorded usage is partial"
                    .to_owned(),
            ),
            count => incomplete.push(format!(
                "{count} requests stopped on an error or an abort; their recorded usage is partial"
            )),
        }
        for (counter, coverage) in &self.measurement_coverage {
            if coverage.missing > 0 || coverage.unpriced > 0 {
                incomplete.push(format!(
                    "{counter} is a known subtotal: {}/{} events measured, {} missing, {} unpriced",
                    coverage.measured, self.events.counted, coverage.missing, coverage.unpriced
                ));
            }
        }
        let cost = (self.measurement_coverage["cost"].measured > 0).then_some(Cost {
            usd: self.totals.cost,
        });
        let cost_coverage = self.totals.cost_coverage().map(|coverage| CostCoverage {
            zero_written_requests: self.zero_written_requests,
            ..coverage
        });
        let tokens = self.totals.tokens.finish();
        let mut measurement_completeness = MeasurementCompleteness::assess(Measured {
            tokens: tokens.as_ref(),
            cost: cost.as_ref(),
            cost_coverage: cost_coverage.as_ref(),
            coverage: Some(&self.measurement_coverage),
            scope: Some(coverage),
        });
        if unreadable > 0 {
            measurement_completeness.qualify_all(MeasurementReason::UnreadableRecord);
        }
        if self.stopped > 0 {
            measurement_completeness.qualify_all(MeasurementReason::StoppedAttempt);
        }
        Some(Consumption {
            accounting: Accounting {
                basis: AccountingBasis::SummedRequests,
                coverage,
                selection: Some(AccountingSelection::RetainedBranches),
            },
            tokens,
            cost,
            cost_coverage,
            events: self.events,
            identity: ConsumptionIdentity {
                fork_parent_unestablished: forked_from == Some(None),
                forked_from: forked_from.flatten(),
                provider_account_namespace: (self.identity.provider_response > 0)
                    .then(|| "unestablished".to_owned()),
                ..self.identity
            },
            measurement_coverage: self.measurement_coverage,
            measurement_completeness,
            incomplete,
        })
    }
}

/// The model an assistant message names.
fn pi_model(value: &Value) -> Option<&str> {
    let message = value.get("message")?;
    (message["role"] == "assistant")
        .then(|| message["model"].as_str())
        .flatten()
}

/// The reasoning level a `thinking_level_change` entry sets.
fn pi_thinking(value: &Value) -> Option<&str> {
    (value["type"] == "thinking_level_change")
        .then(|| value["thinkingLevel"].as_str())
        .flatten()
}

fn pi_trailing_kind(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "model_change" => Some("model_change"),
        "thinking_level_change" => Some("thinking_level_change"),
        _ => None,
    }
}

fn active_path<'a>(entries: &[&'a Value]) -> Vec<&'a Value> {
    active_branch(entries).0
}

/// The last entry's ancestry, root first, and whether the walk reached an
/// entry naming no parent rather than stopping at one the entries lack.
fn active_branch<'a>(entries: &[&'a Value]) -> (Vec<&'a Value>, bool) {
    let by_id = entries
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(|id| (id, *entry)))
        .collect::<HashMap<_, _>>();
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut rooted = false;
    let mut current = entries.last().copied();
    while let Some(entry) = current {
        let Some(id) = entry["id"].as_str() else {
            break;
        };
        if !seen.insert(id) {
            break;
        }
        path.push(entry);
        let parent = entry["parentId"].as_str();
        rooted = parent.is_none();
        current = parent.and_then(|parent| by_id.get(parent).copied());
    }
    path.reverse();
    (path, rooted)
}

/// pi records harness commands outside the conversation. User-role messages
/// and extension custom messages carry input addressed to the agent.
fn user_kind(role: &Role) -> TurnKind {
    role.kind().unwrap_or(TurnKind::Operator)
}

/// Interactive user messages are TUI input; custom messages carry extension input.
fn pi_origin(turn: &mut Turn, launch: Option<Launch>) {
    if turn.kind == TurnKind::Operator
        && turn.origin.is_none()
        && launch == Some(Launch::Interactive)
    {
        turn.origin = Some(crate::model::OperatorOrigin::Person);
    }
    turn.establish_origin(launch);
}

fn pi_turns(value: &Value, thinking: &mut Option<String>) -> Vec<Turn> {
    if let Some(level) = pi_thinking(value) {
        *thinking = Some(level.to_owned());
    }
    let model = pi_model(value).map(|id| Model {
        id: id.to_owned(),
        variant: thinking.clone(),
    });
    let mut turns = parse_turns(value);
    for turn in &mut turns {
        turn.model = model.clone();
    }
    turns
}

fn pi_selections(
    active: &[&Value],
    read: &Jsonl,
) -> (
    Option<crate::model::ModelObservationStatus>,
    Vec<crate::model::ModelSelectionSpan>,
) {
    let mut selections = crate::model_observation::Selections::default();
    let mut thinking = None;
    for value in active {
        if let Some(level) = pi_thinking(value) {
            thinking = Some(level.to_owned());
        }
        if let Some(id) = pi_model(value) {
            selections.observe(
                Model {
                    id: id.to_owned(),
                    variant: thinking.clone(),
                },
                timestamp(&value["timestamp"]),
                value["id"].as_str(),
            );
        }
    }
    if read.skipped > 0 {
        selections.interrupt();
    }
    selections.finish(!read.truncated)
}

fn parse_turns(value: &Value) -> Vec<Turn> {
    if value["type"] == "custom_message" {
        let mut normalized = value.clone();
        normalized["type"] = Value::String("message".to_owned());
        normalized["message"] = serde_json::json!({"role": "user", "content": value["content"]});
        let mut turns = parse_turns(&normalized);
        for turn in &mut turns {
            turn.origin = Some(crate::model::OperatorOrigin::Harness);
            for part in &mut turn.parts {
                let source_field = match part {
                    ContentPart::Text { source_field, .. }
                    | ContentPart::Transcription { source_field, .. }
                    | ContentPart::MediaReference { source_field, .. }
                    | ContentPart::FileReference { source_field, .. }
                    | ContentPart::StructuredArtifact { source_field, .. }
                    | ContentPart::ToolPayload { source_field, .. }
                    | ContentPart::Unknown { source_field, .. } => source_field,
                };
                *source_field = source_field.replacen("message.content", "content", 1);
            }
        }
        return turns;
    }
    if value["type"] != "message" {
        return Vec::new();
    }
    let message = &value["message"];
    let Some(message_role) = message["role"].as_str() else {
        return Vec::new();
    };
    let ts = timestamp(&value["timestamp"]);
    let native_id = value["id"].as_str().map(str::to_owned);
    let role = match message_role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "developer" => Role::Developer,
        "toolResult" => {
            return vec![Turn {
                origin: None,
                text_truncated: None,
                text_chars: None,
                context_event: None,
                model: None,
                role: Role::Tool,
                kind: TurnKind::Tool,
                text: message.to_string(),
                ts,
                ordinal: 0,
                native_id,
                request_turn_id: None,
                metadata: None,
                record_ref: None,
                channel: None,
                recipient: None,
                parts: vec![tool_part(message, "message", "toolResult")],
                coverage: Some(tool_coverage()),
                tool: Some(pi_result_event(message)),
            }];
        }
        _ => return Vec::new(),
    };
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        return (!text.is_empty())
            .then(|| Turn {
                origin: None,
                text_truncated: None,
                text_chars: None,
                context_event: None,
                model: None,
                kind: user_kind(&role),
                role,
                text: text.to_owned(),
                ts,
                ordinal: 0,
                native_id,
                request_turn_id: None,
                metadata: None,
                record_ref: None,
                channel: None,
                recipient: None,
                parts: vec![text_part(text, "message.content", "text")],
                coverage: Some(ContentCoverage {
                    carrier: ContentCarrier::DirectPart,
                    availability: ContentAvailability::RetainedBody,
                    retained_parts: 1,
                    omitted_parts: 0,
                    omitted_reason: None,
                }),
                tool: None,
            })
            .into_iter()
            .collect();
    }

    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let native_kind = block["type"].as_str()?;
            let (role, text, tool, parts, coverage) = match native_kind {
                "text" => (
                    role.clone(),
                    block["text"].as_str()?.to_owned(),
                    None,
                    vec![text_part(
                        block["text"].as_str()?,
                        "message.content",
                        native_kind,
                    )],
                    ContentCoverage {
                        carrier: ContentCarrier::DirectPart,
                        availability: ContentAvailability::RetainedBody,
                        retained_parts: 1,
                        omitted_parts: 0,
                        omitted_reason: None,
                    },
                ),
                "thinking" => (
                    Role::Reasoning,
                    block["thinking"].as_str()?.to_owned(),
                    None,
                    vec![text_part(
                        block["thinking"].as_str()?,
                        "message.content",
                        native_kind,
                    )],
                    ContentCoverage {
                        carrier: ContentCarrier::DirectPart,
                        availability: ContentAvailability::RetainedBody,
                        retained_parts: 1,
                        omitted_parts: 0,
                        omitted_reason: None,
                    },
                ),
                "toolCall" => (
                    Role::Tool,
                    block.to_string(),
                    Some(pi_call_event(block)),
                    vec![tool_part(block, "message.content", native_kind)],
                    tool_coverage(),
                ),
                _ => (
                    role.clone(),
                    String::new(),
                    None,
                    vec![ContentPart::Unknown {
                        native_kind: native_kind.to_owned(),
                        descriptor: bounded_shape(block),
                        source_field: "message.content".to_owned(),
                        record_ref: None,
                    }],
                    ContentCoverage {
                        carrier: ContentCarrier::DirectPart,
                        availability: ContentAvailability::Unknown,
                        retained_parts: 1,
                        omitted_parts: 0,
                        omitted_reason: None,
                    },
                ),
            };
            ((!text.is_empty()) || !parts.is_empty()).then_some(Turn {
                origin: None,
                text_truncated: None,
                text_chars: None,
                context_event: None,
                model: None,
                kind: user_kind(&role),
                role,
                text,
                ts,
                ordinal: 0,
                native_id: native_id.clone(),
                request_turn_id: None,
                metadata: None,
                record_ref: None,
                channel: None,
                recipient: None,
                parts,
                coverage: Some(coverage),
                tool,
            })
        })
        .collect()
}

fn pi_call_event(block: &Value) -> ToolEvent {
    ToolEvent {
        kind: EventKind::ToolCall,
        subtype: "toolCall".to_owned(),
        name: block["name"].as_str().map(str::to_owned),
        call_id: block["id"].as_str().map(str::to_owned),
        status: None,
        arguments: Bounded::retaining_value(&block["arguments"]),
        output: None,
        read: crate::event::direct_read(block["name"].as_str(), &block["arguments"]),
        returned_read: None,
        completed_ts: None,
        invocations: crate::event::invocations_from_tool(
            block["name"].as_str(),
            &block["arguments"],
            "toolCall.arguments",
        ),
        artifact_references: crate::event::artifact_references(&block["arguments"]),
        artifact_consumptions: Vec::new(),
        exit: None,
        self_contained: false,
    }
}

fn pi_result_event(message: &Value) -> ToolEvent {
    ToolEvent {
        kind: EventKind::ToolResult,
        subtype: "toolResult".to_owned(),
        name: message["toolName"].as_str().map(str::to_owned),
        call_id: message["toolCallId"].as_str().map(str::to_owned),
        status: (message["isError"].as_bool() == Some(true)).then(|| "error".to_owned()),
        arguments: None,
        output: Bounded::from_value(&message["content"]),
        read: None,
        returned_read: Some(crate::event::returned_read(
            &message["content"],
            message["isError"].as_bool().map(|error| !error),
        )),
        completed_ts: None,
        invocations: Vec::new(),
        artifact_references: crate::event::artifact_references(&message["content"]),
        artifact_consumptions: Vec::new(),
        exit: None,
        self_contained: false,
    }
}
