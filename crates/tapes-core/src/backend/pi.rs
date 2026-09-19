use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, head_directory, home_path, jsonl_files, list_files, list_files_with_search,
    read_bounds, read_recording, replay_pin, session_file, skipped_records_note, stream_jsonl,
    timestamp, trailing_record, transcript_from_recording, ActivityRange, Backend, Jsonl, Listing,
    ParsedFile, Query, StreamedTranscript, TokenTotals, UnmappedTally,
};
use crate::content::{
    bounded_shape, text_part, tool_coverage, tool_part, ContentAvailability, ContentCarrier,
    ContentCoverage, ContentPart,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::lineage::{Lineage, ParentRef};
use crate::model::{
    AccountingBasis, AccountingCoverage, Cost, KindDeclaration, Model, Role, Session,
    SourceDescriptor, Tokens, TrailingRecord, Transcript, Turn, TurnKind, TurnSelection,
    UserDefault,
};

#[derive(Clone, Debug)]
pub struct PiBackend {
    root: Option<PathBuf>,
    /// How much of a recording's end a transcript read takes.
    read_bytes: u64,
}

impl PiBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            read_bytes: super::DEFAULT_READ_BYTES,
        }
    }

    /// Read at most `read_bytes` from the end of each recording.
    pub fn with_read_bytes(self, read_bytes: u64) -> Self {
        Self { read_bytes, ..self }
    }

    fn parse(&self, path: &Path) -> Result<(Session, Vec<Turn>, Jsonl, usize)> {
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
        let header = opening.iter().find(|value| value["type"] == "session");
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
        let id = header
            .and_then(|value| value["id"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.rsplit('_').next())
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow!("{} has no session id", path.display()))?;
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
        let turns = active
            .iter()
            .flat_map(|value| parse_turns(value))
            .collect::<Vec<_>>();
        let (tokens, cost) = pi_usage(&active);
        let coverage = if read.truncated {
            AccountingCoverage::ReadWindow
        } else {
            AccountingCoverage::Session
        };
        let accounting = accounting_for(
            tokens.as_ref(),
            cost.as_ref(),
            AccountingBasis::SummedRequests,
            coverage,
        );

        let session = Session {
            id,
            source: SourceDescriptor::installed("pi", path.display().to_string()),
            metadata: None,
            model,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory,
            started_at,
            last_activity_at,
            live: None,
            cost,
            tokens,
            accounting,
            start_uncertain: recording.start_uncertain(),
            occurrence: None,
            usage_detail: None,
        };
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
        let root = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("PI_CODING_AGENT_DIR")
                    .map(PathBuf::from)
                    .map(|path| path.join("sessions"))
            })
            .or_else(|| home_path(&[".pi", "agent", "sessions"]));
        Self {
            root,
            read_bytes: super::DEFAULT_READ_BYTES,
        }
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
        self.root.as_deref().is_some_and(Path::is_dir)
    }

    fn list_titles(&self, _query: &Query) -> Result<Listing> {
        // This backend supplies display hints, not recorded titles.
        Ok(Listing::default())
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_files(
            jsonl_files(root),
            query,
            |path| head_directory(path, pi_cwd),
            |path| self.parse(path).ok().map(|(session, _, _, _)| session),
        );
        listing
            .sessions
            .sort_by_key(|session| session.last_activity_at);
        listing.sessions.reverse();
        Ok(listing)
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_files_with_search(
            jsonl_files(root),
            query,
            needle,
            tail,
            self.read_bytes,
            |path| head_directory(path, pi_cwd),
            |path| {
                self.parse(path)
                    .ok()
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

    /// Locate by filename alone: no other session file is opened, so an exact
    /// id costs one directory walk and one parse regardless of store size.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let Some(root) = self.root.as_deref() else {
            return Ok(None);
        };
        let Some(path) = session_file(root, id) else {
            return Ok(None);
        };
        Ok(self.parse(&path).ok().map(|(session, _, _, _)| session))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("pi store is unavailable"))?;
        let path = session_file(root, &session.id)
            .ok_or_else(|| anyhow!("pi session {} is unavailable", session.id))?;
        let (turns, recording, abandoned, trailing_record, unmapped) =
            read_transcript(&path, self.read_bytes)?;
        let mut transcript = transcript_from_recording(
            session.clone(),
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

    fn stream_transcript(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("pi store is unavailable"))?;
        let path = session_file(root, &session.id)
            .ok_or_else(|| anyhow!("pi session {} is unavailable", session.id))?;
        // The active branch is the last entry's ancestry. The first pass keeps
        // only each entry's position, id, and parent; the second emits the
        // active entries' turns in recording order.
        let mut positions = HashMap::<String, (usize, Option<String>)>::new();
        let mut last = None;
        let mut entries = 0;
        let first = stream_jsonl(&path, replay_pin(replay)?, |value, _, _| {
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
        let read = stream_jsonl(&path, Some(first.pin()), |value, span, revision| {
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
            let mut parsed = parse_turns(value);
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
            for parsed_turn in parsed {
                turn(parsed_turn)?;
            }
            Ok(produced)
        })?;
        Ok(StreamedTranscript {
            coordinates: super::StreamCoordinates::FileBytes,
            source_length: read.source_length,
            source_bounds: Vec::new(),
            source_revision: Some(read.revision),
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
        let (root, path) = self.recording(session)?;
        let recording = read_recording(&path, self.read_bytes)?;
        let parent = recording
            .opening()
            .iter()
            .find(|value| value["type"] == "session")
            .and_then(|value| value["parentSession"].as_str())
            .map(|native_id| parent_ref(root, native_id));
        Ok(Lineage {
            parent,
            truncation: read_bounds(&recording.tail),
            ..Lineage::default()
        })
    }

    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        let (root, path) = self.recording(session)?;
        let mut header = None;
        let read = stream_jsonl(&path, None, |value, _, _| {
            if header.is_none() && value["type"] == "session" {
                header = Some(value["parentSession"].as_str().map(str::to_owned));
            }
            Ok(false)
        })?;
        Ok(Lineage {
            parent: header
                .flatten()
                .map(|native_id| parent_ref(root, &native_id)),
            notes: skipped_records_note(read.skipped).into_iter().collect(),
            ..Lineage::default()
        })
    }

    /// pi sums per-request usage along the active branch, which is the last
    /// entry's ancestry. One pass keeps each entry's place in the tree and the
    /// facts it carries; the branch is walked once the last entry is known.
    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        let (_, path) = self.recording(session)?;
        let mut entries = Vec::<PiEntry>::new();
        let mut positions = HashMap::<String, usize>::new();
        let mut activity = ActivityRange::default();
        stream_jsonl(&path, Some(read.pin()?), |value, _, _| {
            activity.observe(value);
            if value["type"] == "session" {
                return Ok(false);
            }
            let id = value["id"].as_str().map(str::to_owned);
            if let Some(id) = &id {
                positions.insert(id.clone(), entries.len());
            }
            entries.push(PiEntry {
                id,
                parent: value["parentId"].as_str().map(str::to_owned),
                request: pi_request(value),
                model: pi_model(value).map(str::to_owned),
                thinking: pi_thinking(value).map(str::to_owned),
            });
            Ok(false)
        })?;
        let mut active = Vec::new();
        let mut seen = HashSet::new();
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
            current = entry
                .parent
                .as_deref()
                .and_then(|parent| positions.get(parent).copied());
        }
        active.reverse();

        let mut usage = PiUsage::new();
        let mut model = None;
        let mut variant = None;
        for entry in active.iter().map(|position| &entries[*position]) {
            if let Some(request) = &entry.request {
                usage.add(request);
            }
            if entry.model.is_some() {
                model.clone_from(&entry.model);
            }
            if entry.thinking.is_some() {
                variant.clone_from(&entry.thinking);
            }
        }
        let (tokens, cost) = usage.finish();
        let mut whole = session.clone();
        whole.accounting = accounting_for(
            tokens.as_ref(),
            cost.as_ref(),
            AccountingBasis::SummedRequests,
            AccountingCoverage::Session,
        );
        whole.tokens = tokens;
        whole.cost = cost;
        whole.model = model.map(|id| Model { id, variant });
        activity.apply(&mut whole);
        Ok(whole)
    }
}

impl PiBackend {
    fn recording<'a>(&'a self, session: &Session) -> Result<(&'a Path, PathBuf)> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("pi store is unavailable"))?;
        let path = session_file(root, &session.id)
            .ok_or_else(|| anyhow!("pi session {} is unavailable", session.id))?;
        Ok((root, path))
    }
}

/// pi records a relationship on the session that has one: its header names
/// the session it came from.
fn parent_ref(root: &Path, native_id: &str) -> ParentRef {
    ParentRef {
        resolved: session_file(root, native_id).is_some(),
        native_id: native_id.to_owned(),
        source: "session.parentSession".to_owned(),
    }
}

/// One entry's place in pi's branch tree and the session facts it carries.
struct PiEntry {
    id: Option<String>,
    parent: Option<String>,
    request: Option<PiRequest>,
    model: Option<String>,
    thinking: Option<String>,
}

type PiTranscriptRead = (
    Vec<Turn>,
    super::Recording,
    usize,
    Option<TrailingRecord>,
    crate::model::UnmappedRecords,
);

fn read_transcript(path: &Path, read_bytes: u64) -> Result<PiTranscriptRead> {
    let recording = read_recording(path, read_bytes)?;
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
    for (index, value) in active.iter().enumerate() {
        let mut parsed = parse_turns(value);
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
    let declined = matches!(kind, "session" | "model_change" | "thinking_level_change");
    tally.add(kind.to_owned(), declined);
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

fn pi_usage(entries: &[&Value]) -> (Option<Tokens>, Option<Cost>) {
    let mut usage = PiUsage::new();
    for request in entries.iter().filter_map(|entry| pi_request(entry)) {
        usage.add(&request);
    }
    usage.finish()
}

/// The usage one assistant message recorded for its request.
struct PiRequest {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    cost: Option<f64>,
}

fn pi_request(entry: &Value) -> Option<PiRequest> {
    if entry["type"] != "message" {
        return None;
    }
    let message = entry.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let usage = message.get("usage").and_then(Value::as_object)?;
    let counter = |name: &str| usage.get(name).and_then(Value::as_u64);
    Some(PiRequest {
        input: counter("input"),
        output: counter("output"),
        reasoning: counter("reasoning"),
        cache_read: counter("cacheRead"),
        cache_write: counter("cacheWrite"),
        cost: usage
            .get("cost")
            .and_then(Value::as_object)
            .and_then(|cost| cost.get("total"))
            .and_then(Value::as_f64),
    })
}

/// Requests summed in order. A request without a recorded cost leaves the
/// sum's cost unknown rather than understated.
struct PiUsage {
    totals: TokenTotals,
    requests: usize,
    cost: Option<f64>,
}

impl PiUsage {
    fn new() -> Self {
        Self {
            totals: TokenTotals::default(),
            requests: 0,
            cost: Some(0.0),
        }
    }

    fn add(&mut self, request: &PiRequest) {
        self.requests += 1;
        self.totals.add(
            request.input,
            request.output,
            request.reasoning,
            request.cache_read,
            request.cache_write,
        );
        match (self.cost.as_mut(), request.cost) {
            (Some(sum), Some(cost)) => *sum += cost,
            _ => self.cost = None,
        }
    }

    fn finish(self) -> (Option<Tokens>, Option<Cost>) {
        let cost = (self.requests > 0)
            .then_some(self.cost)
            .flatten()
            .map(|usd| Cost { usd });
        (self.totals.finish(), cost)
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
    let by_id = entries
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(|id| (id, *entry)))
        .collect::<HashMap<_, _>>();
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = entries.last().copied();
    while let Some(entry) = current {
        let Some(id) = entry["id"].as_str() else {
            break;
        };
        if !seen.insert(id) {
            break;
        }
        path.push(entry);
        current = entry["parentId"]
            .as_str()
            .and_then(|parent| by_id.get(parent).copied());
    }
    path.reverse();
    path
}

/// pi records its harness commands outside the conversation, so every message
/// in its user role is one the operator sent.
fn user_kind(role: &Role) -> TurnKind {
    role.kind().unwrap_or(TurnKind::Operator)
}

fn parse_turns(value: &Value) -> Vec<Turn> {
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
        arguments: Bounded::from_value(&block["arguments"]),
        output: None,
        completed_ts: None,
        invocations: crate::event::invocations_from_tool(
            block["name"].as_str(),
            &block["arguments"],
            "toolCall.arguments",
        ),
        artifact_references: crate::event::artifact_references(&block["arguments"]),
        artifact_consumptions: Vec::new(),
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
        completed_ts: None,
        invocations: Vec::new(),
        artifact_references: crate::event::artifact_references(&message["content"]),
        artifact_consumptions: Vec::new(),
        self_contained: false,
    }
}
