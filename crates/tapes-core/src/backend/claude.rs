use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, head_directory, home_path, list_files, list_files_with_search,
    matching_session_file, read_bounds, read_jsonl, read_recording, replay_pin,
    skipped_records_note, stream_jsonl, streamed_trailing_record, timestamp, trailing_record,
    transcript, transcript_from_recording, ActivityRange, Backend, Jsonl, Listing, ParsedFile,
    Query, StreamedChild, StreamedTranscript, TokenTotals, UnmappedTally,
};
use crate::content::{
    bounded_shape, text_part, tool_coverage, tool_part, ContentAvailability, ContentCarrier,
    ContentCoverage, ContentPart,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::history::PageProjection;
use crate::lineage::{ChildRef, Lineage, SourceRef};
use crate::model::{
    derive_title, AccountingBasis, AccountingCoverage, Cost, KindDeclaration, Model, Role, Session,
    SourceDescriptor, Tokens, TrailingRecord, Transcript, Turn, TurnKind, TurnSelection,
};
use crate::usage::{Durations, ModelUsage, UsageDetail};

#[derive(Clone, Debug)]
pub struct ClaudeBackend {
    root: Option<PathBuf>,
    /// How much of a recording's end a transcript read takes.
    read_bytes: u64,
}

impl ClaudeBackend {
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

    fn parse(&self, path: &Path) -> Result<(Session, Vec<Turn>, Jsonl)> {
        self.parse_with_parent(path, None)
    }

    fn parse_with_parent(
        &self,
        path: &Path,
        parent: Option<&str>,
    ) -> Result<(Session, Vec<Turn>, Jsonl)> {
        let recording = read_recording(path, self.read_bytes)?;
        if let Some(parent) = parent {
            let mut seen = false;
            for value in recording.opening().iter().chain(&recording.tail.values) {
                if let Some(id) = value.get("sessionId") {
                    seen = true;
                    if id.as_str() != Some(parent) {
                        anyhow::bail!(
                            "child recording contains a different or invalid native parent ID"
                        );
                    }
                }
            }
            if !seen {
                anyhow::bail!("child recording has no native parent identity evidence");
            }
        }
        let (started_at, last_activity_at) = recording
            .time_range()
            .map_or((None, None), |(started, activity)| {
                (Some(started), Some(activity))
            });
        // Claude repeats the session id and working directory on every message
        // line. The opening is consulted first; it also covers a transcript
        // past the bounded read whose remaining tail is all tool output and
        // carries no `cwd`.
        let opening = recording.opening();
        let read = &recording.tail;
        let id = opening
            .iter()
            .chain(&read.values)
            .find_map(|value| value["sessionId"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow!("{} has no session id", path.display()))?;
        let directory = opening
            .iter()
            .chain(&read.values)
            .find_map(claude_cwd)
            .map(PathBuf::from);
        let title = read
            .values
            .iter()
            .rev()
            .find_map(|value| value["aiTitle"].as_str())
            .map(str::to_owned);
        let model = read.values.iter().rev().find_map(claude_model);
        let mut turns = Vec::new();
        for (index, value) in read.values.iter().enumerate() {
            let mut parsed = parse_turns(value);
            super::attach_record_refs(
                &mut parsed,
                &format!("file:{}", path.display()),
                Some(&read.source_revision),
                read.spans.get(index).copied(),
            );
            turns.extend(parsed);
        }
        let requests = claude_request_tokens(&read.values);
        let cost_state = covering_cost_state(newest_cost_state(&read.values), requests.as_ref());
        let (tokens, cost, basis) = recorded_accounting(cost_state, requests);
        // A cost-state record is cumulative for the whole session wherever the
        // read reached it; only a sum over the read's requests is bounded by
        // the read.
        let coverage = if basis == AccountingBasis::RecordedTotal || !read.truncated {
            AccountingCoverage::Session
        } else {
            AccountingCoverage::ReadWindow
        };
        let accounting = accounting_for(tokens.as_ref(), cost.as_ref(), basis, coverage);
        let usage_detail = cost_state
            .map(cost_state_detail)
            .and_then(UsageDetail::into_option);

        let session = Session {
            id,
            source: SourceDescriptor::installed("claude", path.display().to_string()),
            metadata: None,
            model,
            title,
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
            usage_detail,
        };
        // The opening is the start of the file, so its first user turn is the
        // session's first user turn even when the tail cannot see it.
        let session = if read.truncated {
            let opening_turns = opening.iter().flat_map(parse_turns).collect::<Vec<_>>();
            session.with_derived_title(&opening_turns)
        } else {
            session.with_derived_title(&turns)
        };

        Ok((session, turns, recording.tail))
    }
    fn read_history(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
        projection: PageProjection,
    ) -> Result<crate::history::Page> {
        let path = session
            .locator()
            .ok_or_else(|| anyhow!("session has no source file"))?;
        crate::history::read_file(
            session,
            Path::new(path),
            cursor,
            bytes,
            projection,
            |values, spans, revision| {
                let turns = if projection == PageProjection::Transcript {
                    values
                        .iter()
                        .zip(spans)
                        .flat_map(|(value, span)| {
                            let mut turns = parse_turns(value);
                            super::attach_record_refs(
                                &mut turns,
                                &format!("file:{}", path),
                                Some(revision),
                                Some(*span),
                            );
                            turns
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let models = values
                    .iter()
                    .filter_map(|value| {
                        let message = &value["message"];
                        if message["role"] != "assistant" {
                            return None;
                        }
                        Some(crate::history::ModelObservation {
                            model: Model {
                                id: message["model"].as_str()?.to_owned(),
                                variant: None,
                            },
                            timestamp: timestamp(&value["timestamp"]),
                        })
                    })
                    .collect();
                (turns, models)
            },
        )
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        let root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .map(|path| path.join("projects"))
            .or_else(|| home_path(&[".claude", "projects"]));
        Self {
            root,
            read_bytes: super::DEFAULT_READ_BYTES,
        }
    }
}

impl Backend for ClaudeBackend {
    /// Claude writes a sender field, a meta flag, or an envelope of its own on
    /// every user record that is not a person's, so a record with none of
    /// them stays `unknown` rather than taking a default.
    fn kinds(&self) -> KindDeclaration {
        KindDeclaration {
            recordable: TurnSelection::only(TurnKind::ALL),
            user_default: None,
        }
    }

    fn child_transcript(&self, parent: &Session, reference: &str) -> Result<Transcript> {
        let path = child_recording(parent, reference)?;
        let (mut session, turns, read) = self.parse_with_parent(&path, Some(&parent.id))?;
        if session.id != parent.id {
            anyhow::bail!("child recording does not name the selected parent");
        }
        session.id = format!("{}::{reference}", parent.id);
        let last_turn = read
            .values
            .iter()
            .rposition(|value| !parse_turns(value).is_empty());
        let trailing = trailing_record(read.values.iter(), last_turn, claude_trailing_kind);
        Ok(transcript(
            session,
            turns,
            usize::MAX,
            &read,
            trailing,
            Vec::new(),
        ))
    }

    fn history_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> Result<crate::history::Page> {
        self.read_history(session, cursor, bytes, PageProjection::Transcript)
    }

    fn metadata_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> Result<crate::history::Page> {
        self.read_history(session, cursor, bytes, PageProjection::Models)
    }

    fn harness(&self) -> &'static str {
        "claude"
    }

    fn available(&self) -> bool {
        self.root.as_deref().is_some_and(Path::is_dir)
    }

    fn list_titles(&self, query: &Query) -> Result<Listing> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Listing::default());
        };
        let mut listing = Listing::default();
        let mut entries_seen = 0;
        let entry_limit = query.ceiling.saturating_mul(2);
        for project in fs::read_dir(root)? {
            entries_seen += 1;
            if entries_seen > entry_limit {
                listing.scan_truncated = true;
                break;
            }
            let project = match project {
                Ok(entry) => entry,
                Err(error) => {
                    listing
                        .unavailable
                        .push(format!("Claude project entry: {error}"));
                    continue;
                }
            };
            let kind = match project.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    listing
                        .unavailable
                        .push(format!("{}: {error}", project.path().display()));
                    continue;
                }
            };
            if !kind.is_dir() {
                continue;
            }
            let entries = match fs::read_dir(project.path()) {
                Ok(entries) => entries,
                Err(error) => {
                    listing
                        .unavailable
                        .push(format!("{}: {error}", project.path().display()));
                    continue;
                }
            };
            for entry in entries {
                entries_seen += 1;
                if entries_seen > entry_limit || listing.scanned >= query.ceiling {
                    listing.scan_truncated = true;
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        listing
                            .unavailable
                            .push(format!("Claude session entry: {error}"));
                        continue;
                    }
                };
                let path = entry.path();
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(error) => {
                        listing
                            .unavailable
                            .push(format!("{}: {error}", path.display()));
                        continue;
                    }
                };
                if !kind.is_file()
                    || path
                        .extension()
                        .is_none_or(|extension| extension != "jsonl")
                {
                    continue;
                }
                listing.scanned += 1;
                match self.parse(&path) {
                    Ok((session, _, read)) => {
                        if let Some(scope) = query.scope {
                            match session.directory.as_deref() {
                                Some(directory) if !scope.contains(directory) => continue,
                                None => {
                                    listing
                                        .unavailable
                                        .push(format!("{}: unknown project scope", path.display()));
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        if read.skipped > 0 || (read.truncated && session.title.is_none()) {
                            listing.unsearched.push(format!(
                                "{} (claude): incomplete recorded-title evidence",
                                session.id
                            ));
                        }
                        listing.sessions.push(session);
                    }
                    Err(error) => listing
                        .unavailable
                        .push(format!("{}: {error:#}", path.display())),
                }
            }
            if listing.scan_truncated {
                break;
            }
        }
        Ok(listing)
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_files(
            session_files(root),
            query,
            |path| head_directory(path, claude_cwd),
            |path| self.parse(path).ok().map(|(session, _, _)| session),
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
            session_files(root),
            query,
            needle,
            tail,
            self.read_bytes,
            |path| head_directory(path, claude_cwd),
            |path| {
                self.parse(path)
                    .ok()
                    .map(|(session, turns, read)| ParsedFile {
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

    /// Locate by filename: no other session file is parsed, though the store
    /// is still walked to find it.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let Some(root) = self.root.as_deref() else {
            return Ok(None);
        };
        let Some(path) = matching_session_file(session_files(root), id) else {
            return Ok(None);
        };
        Ok(self.parse(&path).ok().map(|(session, _, _)| session))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("claude store is unavailable"))?;
        let path = matching_session_file(session_files(root), &session.id)
            .ok_or_else(|| anyhow!("claude session {} is unavailable", session.id))?;
        let (turns, recording, trailing_record, unmapped) =
            read_transcript(&path, self.read_bytes)?;
        let notes = subagent_notes(&path);
        let mut transcript = transcript_from_recording(
            session.clone(),
            turns,
            tail,
            &recording,
            None,
            trailing_record,
            notes,
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
            .ok_or_else(|| anyhow!("claude store is unavailable"))?;
        let path = matching_session_file(session_files(root), &session.id)
            .ok_or_else(|| anyhow!("claude session {} is unavailable", session.id))?;
        let domain = format!("file:{}", path.display());
        let mut unmapped = UnmappedTally::default();
        let read = stream_jsonl(&path, replay_pin(replay)?, |value, span, revision| {
            let mut parsed = parse_turns(value);
            if parsed.is_empty() {
                count_unmapped(&mut unmapped, value);
            }
            super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
            let produced = !parsed.is_empty();
            for parsed_turn in parsed {
                turn(parsed_turn)?;
            }
            Ok(produced)
        })?;
        Ok(StreamedTranscript {
            terminal: None,
            coordinates: super::StreamCoordinates::FileBytes,
            source_length: read.source_length,
            source_bounds: Vec::new(),
            source_revision: Some(read.revision),
            skipped: read.skipped,
            trailing_record: streamed_trailing_record(read.last.as_ref(), claude_trailing_kind),
            gaps: read.gaps,
            notes: subagent_notes(&path),
            unmapped: Some(unmapped.finish()),
            kinds: None,
        })
    }

    /// A Claude session's children are the subagent transcripts under its own
    /// directory and the `Agent` calls its records hold. The parent names
    /// them; a child's own turns are never read here.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let path = self.recording(session)?;
        let read = read_jsonl(&path, self.read_bytes)?;
        let mut calls = AgentCalls::default();
        for value in &read.values {
            calls.observe(value);
        }
        Ok(Lineage {
            truncation: read_bounds(&read),
            ..self.relatives(&path, calls)
        })
    }

    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        let path = self.recording(session)?;
        let mut calls = AgentCalls::default();
        let read = stream_jsonl(&path, None, |value, _, _| {
            calls.observe(value);
            Ok(false)
        })?;
        let mut lineage = self.relatives(&path, calls);
        lineage.notes.extend(skipped_records_note(read.skipped));
        Ok(lineage)
    }

    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        let path = self.recording(session)?;
        let mut records = ClaudeRecords::default();
        stream_jsonl(&path, Some(read.pin()?), |value, _, _| {
            records.observe(value);
            Ok(false)
        })?;
        let mut whole = session.clone();
        records.apply(&mut whole);
        Ok(whole)
    }

    fn stream_child_transcript(
        &self,
        parent: &Session,
        reference: &str,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedChild> {
        let path = child_recording(parent, reference)?;
        let domain = format!("file:{}", path.display());
        let mut identified = false;
        let mut records = ClaudeRecords::default();
        // The first user turn that yields a title, which is all a derived
        // title needs from the turns streamed past.
        let mut title_turn = None::<Turn>;
        let read = stream_jsonl(&path, None, |value, span, revision| {
            if let Some(id) = value.get("sessionId") {
                if id.as_str() != Some(parent.id.as_str()) {
                    anyhow::bail!(
                        "child recording contains a different or invalid native parent ID"
                    );
                }
                identified = true;
            }
            records.observe(value);
            let mut parsed = parse_turns(value);
            super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
            let produced = !parsed.is_empty();
            for parsed_turn in parsed {
                if title_turn.is_none()
                    && parsed_turn.role == Role::User
                    && derive_title(&parsed_turn.text).is_some()
                {
                    title_turn = Some(parsed_turn.clone());
                }
                turn(parsed_turn)?;
            }
            Ok(produced)
        })?;
        if !identified {
            anyhow::bail!("child recording has no native parent identity evidence");
        }
        let mut session = Session {
            id: format!("{}::{reference}", parent.id),
            source: SourceDescriptor::installed("claude", path.display().to_string()),
            metadata: None,
            model: None,
            title: records.title.take(),
            derived_title: None,
            derived_title_truncated: None,
            directory: records.directory.take().map(PathBuf::from),
            started_at: None,
            last_activity_at: None,
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            start_uncertain: false,
            occurrence: None,
            usage_detail: None,
        };
        records.apply(&mut session);
        Ok(StreamedChild {
            session: session.with_derived_title(title_turn.as_slice()),
            read: StreamedTranscript {
                coordinates: super::StreamCoordinates::FileBytes,
                source_length: read.source_length,
                source_bounds: Vec::new(),
                source_revision: Some(read.revision),
                skipped: read.skipped,
                trailing_record: streamed_trailing_record(read.last.as_ref(), claude_trailing_kind),
                gaps: read.gaps,
                terminal: None,
                notes: Vec::new(),
                unmapped: None,
                kinds: None,
            },
        })
    }
}

impl ClaudeBackend {
    fn recording(&self, session: &Session) -> Result<PathBuf> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("claude store is unavailable"))?;
        matching_session_file(session_files(root), &session.id)
            .ok_or_else(|| anyhow!("claude session {} is unavailable", session.id))
    }

    /// A Claude session's children: the subagent transcripts under its own
    /// directory, joined to the `Agent` calls its records hold.
    fn relatives(&self, path: &Path, calls: AgentCalls) -> Lineage {
        let AgentCalls(mut calls) = calls;
        let mut children = Vec::new();
        let mut notes = Vec::new();
        for file in subagent_files(path) {
            if let Some(note) = file.metadata_note {
                notes.push(note);
            }
            let call = file
                .tool_use_id
                .as_ref()
                .and_then(|call_id| calls.remove(call_id))
                .or_else(|| {
                    let call_id = calls
                        .iter()
                        .find(|(_, call)| call.agent_id.as_deref() == Some(&file.agent_id))
                        .map(|(call_id, _)| call_id.clone())?;
                    calls.remove(&call_id)
                })
                .unwrap_or_default();
            let mut source = vec![
                SourceRef::File {
                    path: file.transcript.display().to_string(),
                },
                SourceRef::File {
                    path: file.meta.display().to_string(),
                },
            ];
            source.extend(call.sources());
            children.push(ChildRef {
                role: file.agent_type.or(call.agent_type).or(call.role),
                model: file.model.or(call.model),
                spawned_at: call.spawned_at,
                completed_at: call.completed_at,
                disposition: call.disposition,
                resolved: true,
                source,
                ..ChildRef::new(file.agent_id, self.harness())
            });
        }
        // A call whose transcript is not in the store stays a child: the
        // records name it, and the missing file is what a reader must see.
        for (call_id, call) in calls {
            let source = call.sources();
            children.push(ChildRef {
                role: call.agent_type.or(call.role),
                model: call.model,
                spawned_at: call.spawned_at,
                completed_at: call.completed_at,
                disposition: call.disposition,
                source,
                ..ChildRef::new(call.agent_id.unwrap_or(call_id), self.harness())
            });
        }

        Lineage {
            children,
            notes,
            ..Lineage::default()
        }
    }
}

/// The subagent recording a parent-qualified reference names.
fn child_recording(parent: &Session, reference: &str) -> Result<PathBuf> {
    if reference.is_empty()
        || !reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        anyhow::bail!("child reference must contain only letters, digits, hyphens or underscores");
    }
    let parent_path = Path::new(
        parent
            .locator()
            .ok_or_else(|| anyhow!("parent source unavailable"))?,
    );
    let directory = parent_path.with_extension("").join("subagents");
    Ok(directory.join(format!("agent-{reference}.jsonl")))
}

/// The session facts a Claude recording's records carry beside its turns,
/// folded one record at a time.
#[derive(Default)]
struct ClaudeRecords {
    cost_state: Option<Value>,
    requests: RequestTokens,
    model: Option<Model>,
    title: Option<String>,
    directory: Option<String>,
    activity: ActivityRange,
}

impl ClaudeRecords {
    fn observe(&mut self, value: &Value) {
        if value["type"] == "cost-state" {
            self.cost_state = Some(value.clone());
        }
        self.requests.add(value);
        if let Some(model) = claude_model(value) {
            self.model = Some(model);
        }
        if let Some(title) = value["aiTitle"].as_str() {
            self.title = Some(title.to_owned());
        }
        if self.directory.is_none() {
            self.directory = claude_cwd(value).map(str::to_owned);
        }
        self.activity.observe(value);
    }

    /// Set the session's counters, accounting, usage detail, model, and
    /// activity range to what every folded record states.
    fn apply(self, session: &mut Session) {
        let requests = self.requests.finish();
        let cost_state = covering_cost_state(self.cost_state.as_ref(), requests.as_ref());
        let (tokens, cost, basis) = recorded_accounting(cost_state, requests);
        session.accounting = accounting_for(
            tokens.as_ref(),
            cost.as_ref(),
            basis,
            AccountingCoverage::Session,
        );
        session.usage_detail = cost_state
            .map(cost_state_detail)
            .and_then(UsageDetail::into_option);
        session.tokens = tokens;
        session.cost = cost;
        session.model = self.model;
        self.activity.apply(session);
    }
}

/// The model an assistant record names.
fn claude_model(value: &Value) -> Option<Model> {
    let message = value.get("message")?;
    (message["role"].as_str()? == "assistant")
        .then(|| message["model"].as_str())
        .flatten()
        .map(|id| Model {
            id: id.to_owned(),
            variant: None,
        })
}

fn read_transcript(
    path: &Path,
    read_bytes: u64,
) -> Result<(
    Vec<Turn>,
    super::Recording,
    Option<TrailingRecord>,
    crate::model::UnmappedRecords,
)> {
    let recording = read_recording(path, read_bytes)?;
    let read = &recording.tail;
    let mut turns = Vec::new();
    let mut last_turn = None;
    let mut unmapped = UnmappedTally::default();
    for (index, value) in read.values.iter().enumerate() {
        let mut parsed = parse_turns(value);
        if parsed.is_empty() {
            count_unmapped(&mut unmapped, value);
        }
        super::attach_record_refs(
            &mut parsed,
            &format!("file:{}", path.display()),
            Some(&read.source_revision),
            read.spans.get(index).copied(),
        );
        if !parsed.is_empty() {
            last_turn = Some(index);
        }
        turns.extend(parsed);
    }
    let trailing_record = trailing_record(read.values.iter(), last_turn, claude_trailing_kind);
    Ok((turns, recording, trailing_record, unmapped.finish()))
}

/// Count a record that produced no turn under its native type: `type`, and
/// for an `attachment` or `system` record the kind it names. The reader
/// declines a record that carries no text a turn would hold — counters, ids,
/// flags, file backups — or that restates text another record carries; any
/// other record's text reaches no view.
fn count_unmapped(tally: &mut UnmappedTally, value: &Value) {
    let kind = value["type"].as_str().unwrap_or("(untyped)");
    let detail = match kind {
        "attachment" => value["attachment"]["type"].as_str(),
        "system" => value["subtype"].as_str(),
        _ => None,
    };
    let native_type = detail.map_or_else(|| kind.to_owned(), |detail| format!("{kind}/{detail}"));
    let declined = matches!(
        kind,
        "cost-state"
            | "ai-title"
            | "custom-title"
            | "agent-name"
            | "last-prompt"
            | "mode"
            | "permission-mode"
            | "atis-latch"
            | "queue-operation"
            | "bridge-session"
            | "file-history-snapshot"
            | "file-history-delta"
    ) || matches!(
        native_type.as_str(),
        "system/turn_duration" | "system/stop_hook_summary"
    );
    tally.add(native_type, declined);
}

/// Subagent transcripts are recordings of their own; a parent read names how
/// many belong to it.
fn subagent_notes(path: &Path) -> Vec<String> {
    match subagent_transcript_count(path) {
        0 => Vec::new(),
        1 => vec!["1 subagent transcript belongs to this session.".to_owned()],
        count => vec![format!(
            "{count} subagent transcripts belong to this session."
        )],
    }
}

/// Claude repeats the working directory on every message line.
fn claude_cwd(value: &Value) -> Option<&str> {
    value["cwd"].as_str()
}

fn newest_cost_state(values: &[Value]) -> Option<&Value> {
    values
        .iter()
        .rev()
        .find(|value| value["type"] == "cost-state")
}

/// The `cost-state` to account from: the newest one, unless it states fewer
/// tokens than the recording's own requests. Some sessions end on a zeroed or
/// partial one, and a total below the requests read cannot be the session's.
fn covering_cost_state<'a>(
    cost_state: Option<&'a Value>,
    requests: Option<&Tokens>,
) -> Option<&'a Value> {
    cost_state.filter(|cost_state| {
        requests.is_none_or(|requests| covers(cost_state_tokens(cost_state), requests))
    })
}

/// Whether `total` counts at least as many input, output, and cache tokens as
/// `part`. Reasoning is left out: it is a share of output, and a `cost-state`
/// often records no thinking count at all.
fn covers(total: Option<Tokens>, part: &Tokens) -> bool {
    let kinds = |tokens: &Tokens| {
        [
            tokens.input,
            tokens.output,
            tokens.cache_read,
            tokens.cache_write,
        ]
        .map(|count| count.unwrap_or(0))
    };
    let total = total.as_ref().map_or([0; 4], kinds);
    total
        .iter()
        .zip(kinds(part))
        .all(|(total, part)| *total >= part)
}

/// A covering `cost-state` record is the session's cumulative total wherever
/// the read reached it. Without one, the counters are a sum over the requests
/// read.
fn recorded_accounting(
    cost_state: Option<&Value>,
    requests: Option<Tokens>,
) -> (Option<Tokens>, Option<Cost>, AccountingBasis) {
    match cost_state {
        Some(cost_state) => (
            cost_state_tokens(cost_state),
            cost_state["totalCostUSD"].as_f64().map(|usd| Cost { usd }),
            AccountingBasis::RecordedTotal,
        ),
        None => (requests, None, AccountingBasis::SummedRequests),
    }
}

/// The durations and the per-model split a `cost-state` records beside its
/// totals. The durations are milliseconds of wall clock; the split names each
/// model the session spent on, ordered by model id so one recording reads the
/// same way twice.
fn cost_state_detail(value: &Value) -> UsageDetail {
    let milliseconds = |name: &str| value[name].as_u64();
    let durations = Durations {
        api: milliseconds("totalAPIDuration"),
        api_without_retries: milliseconds("totalAPIDurationWithoutRetries"),
        tool: milliseconds("totalToolDuration"),
        total: milliseconds("totalDuration"),
    };
    let by_model = value["modelUsage"].as_object().map(|models| {
        let mut usage = models
            .iter()
            .map(|(model, usage)| ModelUsage {
                model: model.clone(),
                tokens: model_usage_tokens(usage),
                cost: usage["costUSD"].as_f64().map(|usd| Cost { usd }),
            })
            .collect::<Vec<_>>();
        usage.sort_by(|left, right| left.model.cmp(&right.model));
        usage
    });
    UsageDetail {
        context_window: None,
        rate_limits: None,
        durations_ms: (!durations.is_empty()).then_some(durations),
        by_model: by_model.filter(|usage| !usage.is_empty()),
    }
}

fn model_usage_tokens(usage: &Value) -> Option<Tokens> {
    let mut totals = TokenTotals::default();
    totals.add(
        usage["inputTokens"].as_u64(),
        usage["outputTokens"].as_u64(),
        usage["thinkingTokens"].as_u64(),
        usage["cacheReadInputTokens"].as_u64(),
        usage["cacheCreationInputTokens"].as_u64(),
    );
    totals.finish()
}

fn cost_state_tokens(value: &Value) -> Option<Tokens> {
    let mut totals = TokenTotals::default();
    if let Some(model_usage) = value["modelUsage"].as_object() {
        for usage in model_usage.values() {
            totals.add(
                usage.get("inputTokens").and_then(Value::as_u64),
                usage.get("outputTokens").and_then(Value::as_u64),
                usage.get("thinkingTokens").and_then(Value::as_u64),
                usage.get("cacheReadInputTokens").and_then(Value::as_u64),
                usage
                    .get("cacheCreationInputTokens")
                    .and_then(Value::as_u64),
            );
        }
    }
    totals.finish()
}

fn claude_request_tokens(values: &[Value]) -> Option<Tokens> {
    let mut requests = RequestTokens::default();
    for value in values {
        requests.add(value);
    }
    requests.finish()
}

/// Per-request usage summed once per request id, since Claude repeats a
/// request's usage on every record the response spans.
#[derive(Default)]
struct RequestTokens {
    seen: HashSet<String>,
    totals: TokenTotals,
}

impl RequestTokens {
    fn finish(self) -> Option<Tokens> {
        self.totals.finish()
    }

    fn add(&mut self, value: &Value) {
        if value["type"] != "assistant" {
            return;
        }
        let Some(message) = value.get("message") else {
            return;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            return;
        };
        if let Some(request_id) = value["requestId"].as_str() {
            if !self.seen.insert(request_id.to_owned()) {
                return;
            }
        }
        self.totals.add(
            usage.get("input_tokens").and_then(Value::as_u64),
            usage.get("output_tokens").and_then(Value::as_u64),
            usage
                .get("output_tokens_details")
                .and_then(|details| details.get("thinking_tokens"))
                .and_then(Value::as_u64),
            usage.get("cache_read_input_tokens").and_then(Value::as_u64),
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64),
        );
    }
}

fn claude_trailing_kind(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "last-prompt" => Some("last-prompt"),
        "ai-title" => Some("ai-title"),
        "mode" => Some("mode"),
        "permission-mode" => Some("permission-mode"),
        "atis-latch" => Some("atis-latch"),
        _ => None,
    }
}

/// Claude names each project directory after the working directory it
/// recorded, which looks like a way to select a scope's sessions without
/// opening a file. It is not one, and this backend deliberately does not do
/// it: the encoding is lossy, and a session recorded through a symlinked
/// spelling of a path lands in a directory whose name no canonical scope
/// root reproduces. Narrowing on the name would hide it, and a filter that
/// can produce false negatives is a wrong answer rather than a fast one.
/// The per-file directory probe is cheap enough to make the trade unnecessary.
fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut files = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|project| {
            project
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| project.path())
        })
        .flat_map(|project| fs::read_dir(project).into_iter().flatten().flatten())
        .filter_map(|entry| {
            let path = entry.path();
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_file())
                .and_then(|_| {
                    path.extension()
                        .is_some_and(|extension| extension == "jsonl")
                        .then_some(path)
                })
        })
        .collect::<Vec<_>>();
    files.sort_by_cached_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    files.reverse();
    files
}

fn subagent_transcript_count(path: &Path) -> usize {
    subagent_files(path).len()
}

fn parse_turns(value: &Value) -> Vec<Turn> {
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    let Some(message_role) = message["role"].as_str() else {
        return Vec::new();
    };
    let role = match message_role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "developer" => Role::Developer,
        _ => return Vec::new(),
    };
    let ts = timestamp(&value["timestamp"]);
    let native_id = value["uuid"].as_str().map(str::to_owned);
    let content = &message["content"];
    let user_kind = claude_user_kind(value, envelope_text(content));
    if let Some(text) = content.as_str() {
        return turn(
            role,
            user_kind,
            text.to_owned(),
            ts,
            native_id,
            None,
            TurnContent {
                parts: vec![text_part(text, "message.content", "text")],
                coverage: ContentCoverage {
                    carrier: ContentCarrier::DirectPart,
                    availability: ContentAvailability::RetainedBody,
                    retained_parts: 1,
                    omitted_parts: 0,
                    omitted_reason: None,
                },
            },
        )
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
                subtype @ ("tool_use" | "tool_result") => (
                    Role::Tool,
                    block.to_string(),
                    Some(claude_tool_event(block, subtype)),
                    vec![tool_part(block, "message.content", subtype)],
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
            turn(
                role,
                user_kind,
                text,
                ts,
                native_id.clone(),
                tool,
                TurnContent { parts, coverage },
            )
        })
        .collect()
}

fn turn(
    role: Role,
    user_kind: TurnKind,
    text: String,
    ts: Option<chrono::DateTime<chrono::Utc>>,
    native_id: Option<String>,
    tool: Option<ToolEvent>,
    content: TurnContent,
) -> Option<Turn> {
    ((!text.is_empty()) || !content.parts.is_empty()).then_some(Turn {
        kind: role.kind().unwrap_or(user_kind),
        role,
        text,
        ts,
        ordinal: 0,
        native_id,
        request_turn_id: None,
        metadata: None,
        record_ref: None,
        channel: None,
        recipient: None,
        parts: content.parts,
        coverage: Some(content.coverage),
        tool,
    })
}

struct TurnContent {
    parts: Vec<ContentPart>,
    coverage: ContentCoverage,
}

/// Claude writes what a user record is beside its content. `origin.kind` and
/// `promptSource` name the sender; `isMeta` marks text the harness attached
/// itself; and a record carrying none of the three holds a local command when
/// its content is one of the harness's own envelopes and nothing else. A
/// record the sender fields do vouch for keeps its sender whatever its text
/// resembles, so a person who types a command envelope is still an operator.
/// The text a user record's envelope can be read from: its string content, or
/// the one text block a single-block array holds.
fn envelope_text(content: &Value) -> Option<&str> {
    if let Some(text) = content.as_str() {
        return Some(text);
    }
    match content.as_array()?.as_slice() {
        [block] if block["type"] == "text" => block["text"].as_str(),
        _ => None,
    }
}

fn claude_user_kind(value: &Value, content: Option<&str>) -> TurnKind {
    let origin = value["origin"]["kind"].as_str();
    let prompt_source = value["promptSource"].as_str();
    let is_meta = value["isMeta"].as_bool() == Some(true);
    match (origin, prompt_source) {
        (Some("human"), _) | (_, Some("typed" | "sdk")) => return TurnKind::Operator,
        (Some("task-notification" | "auto-continuation"), _) | (_, Some("system")) => {
            return TurnKind::Notice
        }
        _ => {}
    }
    if is_meta {
        let carries_caveat = content.is_some_and(|text| text.contains("<local-command-caveat>"));
        return if carries_caveat {
            TurnKind::Control
        } else {
            TurnKind::Ambient
        };
    }
    if origin.is_some() || prompt_source.is_some() {
        return TurnKind::Unknown;
    }
    if value.get("interruptedMessageId").is_some() {
        return TurnKind::Notice;
    }
    if value["isCompactSummary"].as_bool() == Some(true) {
        return TurnKind::Ambient;
    }
    if opens_sidechain(value) {
        return TurnKind::Operator;
    }
    match content {
        Some(text)
            if is_command_envelope(text)
                || is_element(text, "local-command-stdout")
                || is_bash_envelope(text) =>
        {
            TurnKind::Control
        }
        Some(text) if INTERRUPTION_MARKERS.contains(&text.trim()) => TurnKind::Notice,
        _ => TurnKind::Unknown,
    }
}

/// The first record of a subagent's transcript: the brief the caller that
/// launched it wrote. Claude marks the transcript a sidechain, names the
/// agent, and gives the record no parent.
fn opens_sidechain(value: &Value) -> bool {
    value["isSidechain"].as_bool() == Some(true)
        && value["agentId"].is_string()
        && value["parentUuid"].is_null()
}

/// The text Claude writes as a user record when the operator interrupts a
/// response, with or without naming the interrupted message.
const INTERRUPTION_MARKERS: [&str; 2] = [
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
];

/// The envelope Claude records a `!` shell command as: its `<bash-input>`
/// alone, or the `<bash-stdout>` and `<bash-stderr>` of its output.
fn is_bash_envelope(text: &str) -> bool {
    let text = text.trim();
    if is_element(text, "bash-input") {
        return true;
    }
    let Some(rest) = text.strip_prefix("<bash-stdout>") else {
        return false;
    };
    let Some(end) = rest.find("</bash-stdout>") else {
        return false;
    };
    let rest = rest[end + "</bash-stdout>".len()..].trim_start();
    rest.is_empty() || is_element(rest, "bash-stderr")
}

/// The envelope Claude records a slash command as: a `<command-name>` element
/// and the elements that accompany it, separated by whitespace and holding
/// nothing else.
fn is_command_envelope(text: &str) -> bool {
    let mut rest = text.trim();
    if !rest.starts_with("<command-name>") {
        return false;
    }
    while !rest.is_empty() {
        let Some(tag) = COMMAND_ELEMENTS
            .iter()
            .find(|tag| rest.starts_with(&format!("<{tag}>")))
        else {
            return false;
        };
        let Some(end) = rest.find(&format!("</{tag}>")) else {
            return false;
        };
        rest = rest[end + tag.len() + 3..].trim_start();
    }
    true
}

const COMMAND_ELEMENTS: [&str; 4] = [
    "command-name",
    "command-message",
    "command-args",
    "command-contents",
];

/// Whether the text is one element of the named tag and nothing besides.
fn is_element(text: &str, tag: &str) -> bool {
    let text = text.trim();
    text.starts_with(&format!("<{tag}>"))
        && text.ends_with(&format!("</{tag}>"))
        && !text[tag.len() + 2..].contains(&format!("<{tag}>"))
}

fn claude_tool_event(block: &Value, subtype: &str) -> ToolEvent {
    let call = subtype == "tool_use";
    let argument_value = block.get("input").cloned().unwrap_or(Value::Null);
    ToolEvent {
        kind: if call {
            EventKind::ToolCall
        } else {
            EventKind::ToolResult
        },
        subtype: subtype.to_owned(),
        name: call
            .then(|| block["name"].as_str())
            .flatten()
            .map(str::to_owned),
        call_id: if call {
            block["id"].as_str()
        } else {
            block["tool_use_id"].as_str()
        }
        .map(str::to_owned),
        status: (!call && block["is_error"].as_bool() == Some(true)).then(|| "error".to_owned()),
        arguments: call.then(|| Bounded::from_value(&block["input"])).flatten(),
        output: (!call)
            .then(|| Bounded::from_value(&block["content"]))
            .flatten(),
        completed_ts: None,
        invocations: if call {
            crate::event::invocations_from_tool(
                block["name"].as_str(),
                &argument_value,
                "tool.input",
            )
        } else {
            Vec::new()
        },
        artifact_references: if call {
            crate::event::artifact_references(&argument_value)
        } else {
            crate::event::artifact_references(&block["content"])
        },
        artifact_consumptions: Vec::new(),
        self_contained: false,
    }
}

/// The name Claude records a subagent spawn under.
const AGENT_TOOL: &str = "Agent";
/// The status a launch record carries for a subagent still running. Every
/// other status is an ending.
const AGENT_LAUNCHED: &str = "async_launched";

/// What a spawn call in the parent said, and what its result reported. Claude
/// writes the call as an `Agent` tool use and the outcome as a `toolUseResult`
/// object beside the matching tool result, so the tool-use id is the join.
#[derive(Default)]
struct AgentCall {
    role: Option<String>,
    spawned_at: Option<chrono::DateTime<chrono::Utc>>,
    call_record: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
    disposition: Option<String>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    result_record: Option<String>,
}

impl AgentCall {
    fn sources(&self) -> Vec<SourceRef> {
        [&self.call_record, &self.result_record]
            .into_iter()
            .flatten()
            .map(|native_id| SourceRef::Record {
                native_id: native_id.clone(),
            })
            .collect()
    }
}

/// A subagent transcript beside the parent's recording, and the meta record
/// Claude writes next to it.
struct SubagentFile {
    metadata_note: Option<String>,
    agent_id: String,
    transcript: PathBuf,
    meta: PathBuf,
    tool_use_id: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
}

/// The agent calls a Claude recording holds, keyed by tool-use id and folded
/// one record at a time.
#[derive(Default)]
struct AgentCalls(HashMap<String, AgentCall>);

impl AgentCalls {
    fn observe(&mut self, value: &Value) {
        let ts = timestamp(&value["timestamp"]);
        let record = value["uuid"].as_str().map(str::to_owned);
        for block in value["message"]["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("tool_use") if block["name"] == AGENT_TOOL => {
                    let Some(call_id) = block["id"].as_str() else {
                        continue;
                    };
                    let call = self.0.entry(call_id.to_owned()).or_default();
                    call.role = block["input"]["subagent_type"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.role.take());
                    call.spawned_at = ts;
                    call.call_record = record.clone();
                }
                Some("tool_result") => {
                    let Some(call_id) = block["tool_use_id"].as_str() else {
                        continue;
                    };
                    let Some(outcome) = tool_use_result(value) else {
                        continue;
                    };
                    // Claude writes a `toolUseResult` beside every tool's
                    // result, so a result belongs to an agent only when its
                    // call was an `Agent` or the payload names an agent
                    // itself — which is how a spawn behind the read bound is
                    // still recognized.
                    if !self.0.contains_key(call_id) && outcome["agentId"].as_str().is_none() {
                        continue;
                    }
                    let call = self.0.entry(call_id.to_owned()).or_default();
                    let status = outcome["status"].as_str();
                    call.agent_id = outcome["agentId"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.agent_id.take());
                    call.agent_type = outcome["agentType"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.agent_type.take());
                    call.model = outcome["resolvedModel"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.model.take());
                    call.disposition = status.map(str::to_owned).or(call.disposition.take());
                    if status.is_some_and(|status| status != AGENT_LAUNCHED) {
                        call.completed_at = ts;
                    }
                    call.result_record = record.clone();
                }
                _ => {}
            }
        }
    }
}

/// Claude writes the tool result's own payload beside the record, as an
/// object or as the JSON text of one.
fn tool_use_result(value: &Value) -> Option<Value> {
    match value.get("toolUseResult")? {
        Value::Object(object) => Some(Value::Object(object.clone())),
        Value::String(text) => serde_json::from_str(text).ok(),
        _ => None,
    }
}

/// The subagent transcripts recorded under a session's own directory. Each
/// carries a meta record naming the agent, and the file's stem carries the
/// agent id the parent's completion record repeats.
fn subagent_files(path: &Path) -> Vec<SubagentFile> {
    let Some(session) = path.file_stem() else {
        return Vec::new();
    };
    let directory = path.with_file_name(session).join("subagents");
    let mut files = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let agent_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())?
                .strip_prefix("agent-")?
                .to_owned();
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                .then(|| {
                    let meta = path.with_file_name(format!("agent-{agent_id}.meta.json"));
                    let (recorded, metadata_note) = subagent_metadata(&meta);
                    SubagentFile {
                        metadata_note,
                        agent_id,
                        transcript: path,
                        meta,
                        tool_use_id: recorded["toolUseId"].as_str().map(str::to_owned),
                        agent_type: recorded["agentType"].as_str().map(str::to_owned),
                        model: recorded["model"].as_str().map(str::to_owned),
                    }
                })
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
    files
}

fn subagent_metadata(path: &Path) -> (Value, Option<String>) {
    const LIMIT: usize = 64 * 1024;
    let read = || -> Result<Value> {
        let file = fs::File::open(path)?;
        let mut bytes = Vec::new();
        file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
        if bytes.len() > LIMIT {
            anyhow::bail!("metadata exceeds {LIMIT} bytes");
        }
        Ok(serde_json::from_slice(&bytes)?)
    };
    match read() {
        Ok(value) => (value, None),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            (Value::Null, None)
        }
        Err(error) => (
            Value::Null,
            Some(format!(
                "Subagent metadata {} unavailable: {error:#}",
                path.display()
            )),
        ),
    }
}
