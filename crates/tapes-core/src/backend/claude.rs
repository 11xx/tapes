use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    accounting_for, bounded_launch, default_native_store, enrich_native_session, head_directory,
    list_discovered_files, list_discovered_files_with_search, merge_launch, read_bounds,
    read_jsonl, read_recording, replay_pin, skipped_records_note, stream_jsonl,
    streamed_trailing_record, timestamp, trailing_record, transcript, transcript_from_recording,
    ActivityRange, Backend, Jsonl, Listing, ParsedFile, Query, StreamedChild, StreamedTranscript,
    TokenTotals, UnmappedTally,
};
use crate::content::{
    bounded_shape, bounded_text, text_part, tool_coverage, tool_part, ArtifactReference,
    ContentAvailability, ContentCarrier, ContentCoverage, ContentPart,
};
use crate::event::{Bounded, EventKind, ReadLines, ToolEvent};
use crate::history::PageProjection;
use crate::lineage::{ChildRef, Lineage, SourceRef};
use crate::model::{
    derive_title, AccountingBasis, AccountingCoverage, Cost, KindDeclaration, Launch, Model, Role,
    Session, SourceDescriptor, Tokens, TrailingRecord, Transcript, Turn, TurnKind, TurnSelection,
};
use crate::usage::{Durations, ModelUsage, UsageDetail};
use agent_tapes_discovery::{Harness as NativeHarness, IdentityBasis, NativeSession, NativeStore};

#[derive(Clone, Debug)]
pub struct ClaudeBackend {
    native_root: Option<PathBuf>,
    store: Option<NativeStore>,
    /// How much of a recording's end a transcript read takes.
    read_bytes: u64,
}

impl ClaudeBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            store: Some(NativeStore::claude(root)),
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
        let mut source = SourceDescriptor::installed("claude", path.display().to_string());
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

    fn parse(&self, native: &NativeSession, path: &Path) -> Result<(Session, Vec<Turn>, Jsonl)> {
        self.parse_with_identity(
            path,
            native.id(),
            native.identity_basis() == Some(IdentityBasis::Header),
            None,
        )
    }

    fn parse_with_parent(
        &self,
        path: &Path,
        native_id: &str,
        parent: Option<&str>,
    ) -> Result<(Session, Vec<Turn>, Jsonl)> {
        self.parse_with_identity(path, native_id, true, parent)
    }

    fn parse_with_identity(
        &self,
        path: &Path,
        native_id: &str,
        require_identity_record: bool,
        parent: Option<&str>,
    ) -> Result<(Session, Vec<Turn>, Jsonl)> {
        let recording = read_recording(path, self.read_bytes)?;
        let opening = recording.opening();
        let read = &recording.tail;
        let mut identity_seen = false;
        for value in opening.iter().chain(&read.values) {
            if let Some(id) = value.get("sessionId") {
                identity_seen = true;
                if id.as_str() != Some(native_id) {
                    if parent.is_some() {
                        anyhow::bail!(
                            "child recording contains a different or invalid native parent ID"
                        );
                    }
                    anyhow::bail!(
                        "Claude record contains a different or invalid native session ID"
                    );
                }
            }
        }
        if require_identity_record && !identity_seen {
            if parent.is_some() {
                anyhow::bail!("child recording has no native parent identity evidence");
            }
            anyhow::bail!("Claude recording has no native session identity evidence");
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
        let launch = bounded_launch(
            claude_launch(opening.iter().chain(&read.values)),
            read.truncated,
        );
        let (model_observation, model_selections) = claude_selections(read);
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
            id: native_id.to_owned(),
            source: self.source(path),
            metadata: None,
            model,
            model_observation,
            model_selections,
            launch,
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
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("session has no source file"))?;
        crate::history::read_file(
            session,
            path,
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
                                &format!("file:{}", path.display()),
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
                        Some(crate::history::ModelObservation {
                            model: claude_model(value)?,
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
        Self::from_store(default_native_store(NativeHarness::Claude))
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
        let (mut session, turns, read) =
            self.parse_with_parent(&path, &parent.id, Some(&parent.id))?;
        session.id = format!("{}::{reference}", parent.id);
        if let Some(location) = session.source.location.as_mut() {
            location.native_path = Some(path);
        }
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
        self.store.as_ref().is_some_and(NativeStore::available)
    }

    fn list_titles(&self, query: &Query) -> Result<Listing> {
        let Some(store) = self.store.as_ref() else {
            return Ok(Listing::default());
        };
        let page = store.candidates(query.ceiling);
        let mut listing = Listing {
            scanned: page.scanned,
            scan_truncated: !page.complete,
            unavailable: page.failures.iter().map(ToString::to_string).collect(),
            unavailable_ids: page.unreadable_ids.clone(),
            ..Listing::default()
        };
        for native in page.records {
            let Some(path) = native.locator() else {
                listing.unavailable.push(format!(
                    "Claude session {}: native file locator is missing",
                    native.id()
                ));
                listing.unavailable_ids.push(native.id().to_owned());
                continue;
            };
            let (session, _, read) = match self.parse(&native, path) {
                Ok(parsed) => parsed,
                Err(error) => {
                    listing
                        .unavailable
                        .push(format!("Claude session {}: {error:#}", native.id()));
                    listing.unavailable_ids.push(native.id().to_owned());
                    continue;
                }
            };
            let session = match enrich_native_session(&native, session) {
                Ok(session) => session,
                Err(error) => {
                    listing
                        .unavailable
                        .push(format!("Claude session {}: {error:#}", native.id()));
                    listing.unavailable_ids.push(native.id().to_owned());
                    continue;
                }
            };
            if let Some(scope) = query.scope {
                match session.directory.as_deref() {
                    Some(directory) if !scope.contains(directory) => continue,
                    None => {
                        listing.unavailable.push(format!(
                            "Claude session {}: unknown project scope",
                            native.id()
                        ));
                        listing.unavailable_ids.push(native.id().to_owned());
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
        Ok(listing)
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let Some(store) = self.store.as_ref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_discovered_files(
            store,
            query,
            |path| head_directory(path, claude_cwd),
            |native, path| self.parse(native, path).map(|(session, _, _)| session),
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
            |path| head_directory(path, claude_cwd),
            |native, path| {
                self.parse(native, path)
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

    /// Resolve native identity before reading normalized transcript metadata.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        let Some(native) = store.locate_exact(id)? else {
            return Ok(None);
        };
        let path = native
            .locator()
            .ok_or_else(|| anyhow!("Claude native session {} has no file locator", native.id()))?;
        let (session, _, _) = self.parse(&native, path)?;
        Ok(Some(enrich_native_session(&native, session)?))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("claude session {} has no file locator", session.id))?;
        let (turns, recording, trailing_record, unmapped) = read_transcript(path, self.read_bytes)?;
        let notes = subagent_notes(path);
        let mut session = session.clone();
        session.model = recording.tail.values.iter().rev().find_map(claude_model);
        let opening = recording.opening();
        session.launch = bounded_launch(
            claude_launch(opening.iter().chain(&recording.tail.values)),
            recording.tail.truncated,
        );
        (session.model_observation, session.model_selections) = claude_selections(&recording.tail);
        let mut transcript = transcript_from_recording(
            session,
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
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("claude session {} has no file locator", session.id))?;
        let mut launch = None;
        let opening = stream_jsonl(path, replay_pin(replay)?, |value, _, _| {
            launch = merge_launch(launch, claude_entrypoint(value));
            Ok(false)
        })?;
        let domain = format!("file:{}", path.display());
        let mut unmapped = UnmappedTally::default();
        let read = stream_jsonl(path, Some(opening.pin()), |value, span, revision| {
            let mut parsed = parse_turns(value);
            if parsed.is_empty() {
                count_unmapped(&mut unmapped, value);
            }
            super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
            let produced = !parsed.is_empty();
            for mut parsed_turn in parsed {
                parsed_turn.establish_origin(launch);
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
            source_prefix_sha256: Some(read.prefix_sha256),
            skipped: read.skipped,
            trailing_record: streamed_trailing_record(read.last.as_ref(), claude_trailing_kind),
            gaps: read.gaps,
            notes: subagent_notes(path),
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
        let mut last_end = 0;
        stream_jsonl(&path, Some(read.pin()?), |value, span, _| {
            if span.start != last_end {
                records.selections.interrupt();
            }
            last_end = span.end;
            records.observe(value);
            Ok(false)
        })?;
        if !read.gaps.is_empty() {
            records.selections.interrupt();
        }
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
        let mut launch = None;
        let opening = stream_jsonl(&path, None, |value, _, _| {
            launch = merge_launch(launch, claude_entrypoint(value));
            Ok(false)
        })?;
        let domain = format!("file:{}", path.display());
        let mut identified = false;
        let mut records = ClaudeRecords::default();
        // The first user turn that yields a title, which is all a derived
        // title needs from the turns streamed past.
        let mut title_turn = None::<Turn>;
        let mut last_end = 0;
        let read = stream_jsonl(&path, Some(opening.pin()), |value, span, revision| {
            if let Some(id) = value.get("sessionId") {
                if id.as_str() != Some(parent.id.as_str()) {
                    anyhow::bail!(
                        "child recording contains a different or invalid native parent ID"
                    );
                }
                identified = true;
            }
            if span.start != last_end {
                records.selections.interrupt();
            }
            last_end = span.end;
            records.observe(value);
            let mut parsed = parse_turns(value);
            super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
            let produced = !parsed.is_empty();
            for mut parsed_turn in parsed {
                parsed_turn.establish_origin(launch);
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
        let mut source = self.source(&path);
        if let Some(location) = source.location.as_mut() {
            location.native_path = Some(path.clone());
        }
        let mut session = Session {
            id: format!("{}::{reference}", parent.id),
            source,
            metadata: None,
            model: None,
            model_observation: None,
            model_selections: Vec::new(),
            launch: None,
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
        if !read.gaps.is_empty() {
            records.selections.interrupt();
        }
        records.apply(&mut session);
        Ok(StreamedChild {
            session: session.with_derived_title(title_turn.as_slice()),
            read: StreamedTranscript {
                coordinates: super::StreamCoordinates::FileBytes,
                source_length: read.source_length,
                source_bounds: Vec::new(),
                source_revision: Some(read.revision),
                source_prefix_sha256: Some(read.prefix_sha256),
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
        super::session_file_path(session)
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("claude session {} has no file locator", session.id))
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
    let parent_path =
        super::session_file_path(parent).ok_or_else(|| anyhow!("parent source unavailable"))?;
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
    launch: Option<Launch>,
    selections: crate::model_observation::Selections,
    title: Option<String>,
    directory: Option<String>,
    activity: ActivityRange,
}

impl ClaudeRecords {
    fn observe(&mut self, value: &Value) {
        self.launch = merge_launch(self.launch, claude_entrypoint(value));
        if value["type"] == "cost-state" {
            self.cost_state = Some(value.clone());
        }
        self.requests.add(value);
        if let Some(model) = claude_model(value) {
            self.selections.observe(
                model.clone(),
                timestamp(&value["timestamp"]),
                value["uuid"].as_str(),
            );
            self.model = Some(model);
        } else if value["message"]["role"] == "assistant"
            && value["message"]["model"] != "<synthetic>"
        {
            self.selections.interrupt();
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
        session.launch = self.launch;
        (session.model_observation, session.model_selections) = self.selections.finish(true);
        self.activity.apply(session);
    }
}

fn claude_entrypoint(value: &Value) -> Option<Launch> {
    if !matches!(
        value["type"].as_str(),
        Some("user" | "assistant" | "attachment" | "system")
    ) {
        return None;
    }
    match value["entrypoint"].as_str()? {
        "cli" | "claude-desktop" => Some(Launch::Interactive),
        "sdk-cli" => Some(Launch::Headless),
        _ => None,
    }
}

fn claude_launch<'a>(values: impl IntoIterator<Item = &'a Value>) -> Option<Launch> {
    values.into_iter().fold(None, |current, value| {
        merge_launch(current, claude_entrypoint(value))
    })
}

/// The model an assistant record names, with the effort the turn ran at: the
/// turn's own `perTurnEffort`, else the session-wide `effort`. Claude writes
/// assistant records of its own for API errors under the model `<synthetic>`;
/// they name no model a turn ran under.
fn claude_model(value: &Value) -> Option<Model> {
    let message = value.get("message")?;
    if message["role"].as_str()? != "assistant" {
        return None;
    }
    let id = message["model"].as_str()?;
    if id == "<synthetic>" {
        return None;
    }
    Some(Model {
        id: id.to_owned(),
        variant: value["perTurnEffort"]
            .as_str()
            .or_else(|| value["effort"].as_str())
            .filter(|effort| !effort.is_empty())
            .map(str::to_owned),
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
                variant: None,
                tokens: model_usage_tokens(usage),
                cost: usage["costUSD"].as_f64().map(|usd| Cost { usd }),
                request_count: None,
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
        attribution: None,
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
fn subagent_transcript_count(path: &Path) -> usize {
    subagent_files(path).len()
}

pub(crate) fn parse_turns(value: &Value) -> Vec<Turn> {
    let model = claude_model(value);
    let mut turns = parse_turns_raw(value);
    for turn in &mut turns {
        turn.model = model.clone();
        if turn.kind == TurnKind::Operator {
            turn.origin = if value["isMeta"].as_bool() == Some(true) {
                Some(crate::model::OperatorOrigin::Harness)
            } else if value["promptSource"] == "typed" {
                Some(crate::model::OperatorOrigin::Person)
            } else {
                None
            };
        }
    }
    turns
}

fn claude_selections(
    read: &Jsonl,
) -> (
    Option<crate::model::ModelObservationStatus>,
    Vec<crate::model::ModelSelectionSpan>,
) {
    let mut selections = crate::model_observation::Selections::default();
    for (index, value) in read.values.iter().enumerate() {
        if index > 0 && read.spans[index - 1].end != read.spans[index].start {
            selections.interrupt();
        }
        if let Some(model) = claude_model(value) {
            selections.observe(
                model,
                timestamp(&value["timestamp"]),
                value["uuid"].as_str(),
            );
        } else if value["message"]["role"] == "assistant"
            && value["message"]["model"] != "<synthetic>"
        {
            selections.interrupt();
        }
    }
    if read.skipped > 0 {
        selections.interrupt();
    }
    selections.finish(!read.truncated)
}

fn parse_turns_raw(value: &Value) -> Vec<Turn> {
    if value["type"] == "system" && value["subtype"] == "compact_boundary" {
        let metadata = &value["compactMetadata"];
        let messages = &metadata["preservedMessages"];
        let segment = &metadata["preservedSegment"];
        let string = |value: &Value| value.as_str().map(str::to_owned);
        return vec![Turn {
            origin: None,
            text_truncated: None,
            text_chars: None,
            context_event: Some(crate::model::ContextEvent::Compaction(
                crate::model::Compaction {
                    trigger: string(&metadata["trigger"]),
                    pre_tokens: metadata["preTokens"].as_u64(),
                    post_tokens: metadata["postTokens"].as_u64(),
                    duration_ms: metadata["durationMs"].as_u64(),
                    cumulative_dropped_tokens: metadata["cumulativeDroppedTokens"].as_u64(),
                    preserved_messages_anchor: string(&messages["anchorUuid"]),
                    preserved_segment_anchor: string(&segment["anchorUuid"]),
                    preserved_segment_head: string(&segment["headUuid"]),
                    preserved_segment_tail: string(&segment["tailUuid"]),
                    omitted_preserved_uuids: messages["uuids"].as_array().map(Vec::len),
                    omitted_all_preserved_uuids: messages["allUuids"].as_array().map(Vec::len),
                },
            )),
            model: None,
            role: Role::System,
            kind: TurnKind::Notice,
            text:
                "Model context compacted; retained source history is independent of this boundary."
                    .to_owned(),
            ts: timestamp(&value["timestamp"]),
            ordinal: 0,
            native_id: value["uuid"].as_str().map(str::to_owned),
            request_turn_id: None,
            metadata: None,
            record_ref: None,
            channel: None,
            recipient: None,
            parts: Vec::new(),
            coverage: None,
            tool: None,
        }];
    }
    if value["type"] == "attachment" {
        return claude_attachment(value).into_iter().collect();
    }
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
                subtype @ ("tool_use" | "tool_result") => {
                    let persisted = (subtype == "tool_result")
                        .then(|| persisted_tool_output(block))
                        .flatten();
                    let (text, parts, coverage) = if let Some(persisted) = persisted {
                        (
                            persisted.preview.to_owned(),
                            vec![
                                text_part(persisted.preview, "message.content.content", subtype),
                                ContentPart::FileReference {
                                    reference: persisted.reference,
                                    source_field: "message.content.content".to_owned(),
                                    native_kind: "persisted-output".to_owned(),
                                    record_ref: None,
                                },
                            ],
                            ContentCoverage {
                                carrier: ContentCarrier::RecordedReference,
                                availability: ContentAvailability::ReferenceOnly,
                                retained_parts: 2,
                                omitted_parts: 0,
                                omitted_reason: None,
                            },
                        )
                    } else {
                        (
                            block.to_string(),
                            vec![tool_part(block, "message.content", subtype)],
                            tool_coverage(),
                        )
                    };
                    (
                        Role::Tool,
                        text,
                        Some(claude_tool_event(
                            block,
                            subtype,
                            value.get("toolUseResult").filter(|_| {
                                message["content"]
                                    .as_array()
                                    .is_some_and(|parts| parts.len() == 1)
                            }),
                        )),
                        parts,
                        coverage,
                    )
                }
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
        origin: None,
        text_truncated: None,
        text_chars: None,
        context_event: None,
        model: None,
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

fn claude_attachment(value: &Value) -> Option<Turn> {
    let attachment = &value["attachment"];
    let kind = attachment["type"].as_str()?;
    if !matches!(kind, "file" | "compact_file_reference") {
        return None;
    }
    let file = &attachment["content"]["file"];
    let path = attachment["displayPath"]
        .as_str()
        .or_else(|| {
            (kind == "file")
                .then(|| file["filePath"].as_str())
                .flatten()
        })
        .or_else(|| attachment["filename"].as_str());
    let path = path.map(|path| bounded_text(path, crate::content::MAX_DESCRIPTOR_CHARS));
    let mut reference = ArtifactReference {
        kind: format!("attachment/{kind}"),
        descriptor_truncated: path.as_ref().is_some_and(|path| path.truncated),
        path: path.map(|path| path.text),
        body_availability: Some(ContentAvailability::ReferenceOnly),
        ..ArtifactReference::default()
    };
    if kind == "file" && attachment["content"]["type"] == "text" {
        if let Some((body, lines, total)) = attached_file_body(file) {
            let body = bounded_text(body, crate::content::MAX_ARTIFACT_BODY_CHARS);
            reference.body_availability = Some(
                if body.truncated || file["truncatedByTokenCap"].as_bool() == Some(true) {
                    ContentAvailability::ReadBound
                } else {
                    ContentAvailability::RetainedBody
                },
            );
            reference.body = Some(body);
            reference.body_lines = lines;
            reference.total_lines = total;
        }
    }
    let availability = reference.body_availability?;
    let text = reference
        .body
        .as_ref()
        .map_or_else(String::new, |body| body.text.clone());
    let body_present = reference.body.is_some();
    let part = ContentPart::FileReference {
        reference,
        source_field: "attachment".to_owned(),
        native_kind: format!("attachment/{kind}"),
        record_ref: None,
    };
    let mut parts = vec![part];
    if body_present {
        parts.push(text_part(&text, "attachment.content.file.content", "text"));
    }
    let retained_parts = parts.len();
    turn(
        Role::System,
        TurnKind::Ambient,
        text,
        timestamp(&value["timestamp"]),
        value["uuid"].as_str().map(str::to_owned),
        None,
        TurnContent {
            parts,
            coverage: ContentCoverage {
                carrier: ContentCarrier::AssociatedArtifact,
                availability,
                retained_parts,
                omitted_parts: 0,
                omitted_reason: None,
            },
        },
    )
}

fn attached_file_body(file: &Value) -> Option<(&str, Option<ReadLines>, Option<u64>)> {
    let body = file["content"].as_str()?;
    file["filePath"].as_str()?;
    let start = file["startLine"].as_u64()?;
    let count = file["numLines"].as_u64()?;
    let total = match file.get("totalLines") {
        Some(value) => Some(value.as_u64()?),
        None => None,
    };
    let end = start.checked_sub(1)?.checked_add(count)?;
    if total.is_some_and(|total| end > total) {
        return None;
    }
    Some((body, (count > 0).then_some(ReadLines { start, end }), total))
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

fn claude_read_content(
    output: &Value,
    structured: Option<&Value>,
) -> Option<(String, Option<ReadLines>)> {
    if let Some(result) = structured {
        if result["type"] != "text" {
            return None;
        }
        let file = &result["file"];
        if file["truncatedByTokenCap"].as_bool() == Some(true) {
            return None;
        }
        let content = file["content"].as_str()?;
        let start = file["startLine"].as_u64()?;
        let count = file["numLines"].as_u64()?;
        let total = file["totalLines"].as_u64()?;
        let end = start.checked_sub(1)?.checked_add(count)?;
        if end > total {
            return None;
        }
        let lines = (count > 0).then_some(ReadLines { start, end });
        return Some((content.to_owned(), lines));
    }
    let text = crate::event::returned_text(output)?;
    if text.is_empty() {
        return None;
    }
    let mut content = String::new();
    let mut previous: Option<u64> = None;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        let (prefix, body) = line.split_once('\t')?;
        if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let number = prefix.parse::<u64>().ok()?;
        if prefix != number.to_string()
            || number == 0
            || previous.is_some_and(|previous| previous.checked_add(1) != Some(number))
        {
            return None;
        }
        start.get_or_insert(number);
        previous = Some(number);
        content.push_str(body);
    }
    Some((
        content,
        Some(ReadLines {
            start: start?,
            end: previous?,
        }),
    ))
}

struct PersistedOutput<'a> {
    preview: &'a str,
    reference: ArtifactReference,
}

fn persisted_tool_output(block: &Value) -> Option<PersistedOutput<'_>> {
    let text = block["content"].as_str()?.trim_end_matches(['\r', '\n']);
    let inner = text
        .strip_prefix("<persisted-output>\n")?
        .strip_suffix("\n</persisted-output>")?;
    if inner.contains("<persisted-output>") || inner.contains("</persisted-output>") {
        return None;
    }
    let (header, preview) = inner.split_once("\n\n")?;
    let (size, path) = header
        .strip_prefix("Output too large (")?
        .split_once("). Full output saved to: ")?;
    if !persisted_size(size) {
        return None;
    }
    let (preview_header, preview) = preview.split_once('\n')?;
    let preview_size = preview_header
        .strip_prefix("Preview (first ")?
        .strip_suffix("):")?;
    if !persisted_size(preview_size) {
        return None;
    }
    let path = Path::new(path);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".txt") || name.len() <= 4 || name.chars().any(char::is_control) {
        return None;
    }
    let results = path.parent()?;
    if results.file_name()?.to_str()? != "tool-results" {
        return None;
    }
    let session = results.parent()?.file_name()?.to_str()?;
    if session.len() != 36
        || !session.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    let path = bounded_text(
        &format!("tool-results/{name}"),
        crate::content::MAX_DESCRIPTOR_CHARS,
    );
    let call_id = block["tool_use_id"]
        .as_str()
        .map(|id| bounded_text(id, crate::content::MAX_DESCRIPTOR_CHARS));
    Some(PersistedOutput {
        preview,
        reference: ArtifactReference {
            kind: "claude-persisted-tool-output".to_owned(),
            descriptor_truncated: path.truncated || call_id.as_ref().is_some_and(|id| id.truncated),
            path: Some(path.text),
            recorded_size: Some(size.to_owned()),
            tool_call_id: call_id.map(|id| id.text),
            body_availability: Some(ContentAvailability::ReferenceOnly),
            ..ArtifactReference::default()
        },
    })
}

fn persisted_size(size: &str) -> bool {
    size.len() <= 32
        && size.ends_with('B')
        && !size.chars().any(char::is_whitespace)
        && size.starts_with(|character: char| character.is_ascii_digit())
        && size.parse::<crate::byte_size::ByteSize>().is_ok()
}

fn claude_tool_event(block: &Value, subtype: &str, structured: Option<&Value>) -> ToolEvent {
    let call = subtype == "tool_use";
    let argument_value = block.get("input").cloned().unwrap_or(Value::Null);
    let persisted = (!call).then(|| persisted_tool_output(block)).flatten();
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
        arguments: call
            .then(|| Bounded::retaining_value(&block["input"]))
            .flatten(),
        output: (!call)
            .then(|| {
                persisted.as_ref().map_or_else(
                    || Bounded::from_value(&block["content"]),
                    |output| Some(Bounded::from_text(output.preview)),
                )
            })
            .flatten(),
        read: call
            .then(|| crate::event::direct_read(block["name"].as_str(), &block["input"]))
            .flatten(),
        returned_read: (!call).then(|| {
            let mut returned = crate::event::returned_read(
                &block["content"],
                block["is_error"].as_bool().map(|error| !error),
            );
            if returned.succeeded != Some(false) {
                if let Some((content, lines)) = claude_read_content(&block["content"], structured) {
                    returned.content_sha256 =
                        Some(format!("sha256:{:x}", Sha256::digest(content.as_bytes())));
                    returned.content_lines = lines;
                }
            }
            returned
        }),
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
        } else if let Some(persisted) = persisted {
            vec![persisted.reference]
        } else {
            crate::event::artifact_references(&block["content"])
        },
        artifact_consumptions: Vec::new(),
        exit: None,
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

#[cfg(test)]
mod read_content_tests {
    use super::{claude_attachment, claude_read_content, claude_tool_event, persisted_tool_output};
    use crate::content::{ContentAvailability, ContentPart};
    use crate::event::ReadLines;
    use serde_json::json;
    use serde_json::Value;

    #[test]
    fn numbered_content_preserves_bytes_and_requires_every_line() {
        for (output, expected) in [
            ("10\tfirst\n11\tsecond\n", Some("first\nsecond\n")),
            ("1\tfirst\r\n2\tlast", Some("first\r\nlast")),
            ("1\t\n2\t\tindented", Some("\n\tindented")),
            ("1\tfirst\ntruncated", None),
            ("1\tfirst\n3\tthird", None),
            ("1\tfirst\n2→second", None),
            ("plain text", None),
            ("", None),
            ("0\tfirst", None),
            ("01\tfirst", None),
        ] {
            assert_eq!(
                claude_read_content(&json!(output), None)
                    .as_ref()
                    .map(|(content, _)| content.as_str()),
                expected
            );
        }
    }

    #[test]
    fn failed_reads_have_no_content_digest_and_ranges_follow_returned_lines() {
        let block = json!({"content":"10\tfirst\n11\tsecond", "is_error":false});
        let returned = claude_tool_event(&block, "tool_result", None)
            .returned_read
            .unwrap();
        assert!(returned.content_sha256.is_some());
        let lines = returned.content_lines.unwrap();
        assert_eq!((lines.start, lines.end), (10, 11));
        let failed = json!({"content":"1\ttext", "is_error":true});
        let returned = claude_tool_event(&failed, "tool_result", None)
            .returned_read
            .unwrap();
        assert!(returned.content_sha256.is_none());
        assert!(returned.content_lines.is_none());
    }

    #[test]
    fn structured_text_is_authoritative_and_other_shapes_are_unknown() {
        let result = json!({"type":"text", "file": {
            "content":"original\n", "startLine":10,"numLines":1,"totalLines":20
        }});
        assert_eq!(
            claude_read_content(&json!("decorated"), Some(&result))
                .as_ref()
                .map(|(content, _)| content.as_str()),
            Some("original\n")
        );
        for result in [
            json!({"type":"image","file":{"content":"text"}}),
            json!({"type":"text","file":{"content":"text"}}),
            json!({"type":"text","file":{"content":"text","startLine":2,"numLines":2,"totalLines":2}}),
            json!({"type":"text","file":{"content":"text","startLine":1,"numLines":1,"totalLines":1,"truncatedByTokenCap":true}}),
        ] {
            assert_eq!(claude_read_content(&json!("1\ttext"), Some(&result)), None);
        }
    }

    #[test]
    fn attached_body_validates_ranges_and_keeps_path_fallbacks() {
        let file = json!({"content":"body", "filePath":"nested.rs", "startLine":2,"numLines":2});
        let record = json!({"type":"attachment", "attachment": {
            "type":"file", "displayPath":"shown.rs", "filename":"named.rs",
            "content":{"type":"text", "file":file}
        }});
        let artifact = |value: &Value| match claude_attachment(value).unwrap().parts.remove(0) {
            ContentPart::FileReference { reference, .. } => reference,
            _ => unreachable!(),
        };
        let reference = artifact(&record);
        assert_eq!(reference.path.as_deref(), Some("shown.rs"));
        assert_eq!(reference.body_lines, Some(ReadLines { start: 2, end: 3 }));
        assert_eq!(reference.total_lines, None);
        let mut fallback = record.clone();
        fallback["attachment"]
            .as_object_mut()
            .unwrap()
            .remove("displayPath");
        assert_eq!(artifact(&fallback).path.as_deref(), Some("nested.rs"));
        fallback["attachment"]["content"]["file"]
            .as_object_mut()
            .unwrap()
            .remove("filePath");
        assert_eq!(artifact(&fallback).path.as_deref(), Some("named.rs"));
        assert!(artifact(&fallback).body.is_none());
        fallback["attachment"]
            .as_object_mut()
            .unwrap()
            .remove("filename");
        assert!(artifact(&fallback).path.is_none());
        for file in [
            json!({"content":"body","filePath":"nested.rs","startLine":0,"numLines":1}),
            json!({"content":"body","filePath":"nested.rs","startLine":2,"numLines":2,"totalLines":2}),
            json!({"content":"body","filePath":"nested.rs","startLine":2,"numLines":2,"totalLines":"4"}),
            json!({"content":"body","filePath":"nested.rs","startLine":18446744073709551615u64,"numLines":2}),
            json!({"content":17,"filePath":"nested.rs","startLine":1,"numLines":1}),
            json!({"content":"body","filePath":"nested.rs","startLine":1}),
        ] {
            let mut malformed = record.clone();
            malformed["attachment"]["content"]["file"] = file;
            let reference = artifact(&malformed);
            assert_eq!(reference.path.as_deref(), Some("shown.rs"));
            assert!(reference.body.is_none());
            assert_eq!(
                reference.body_availability,
                Some(ContentAvailability::ReferenceOnly)
            );
        }
        let mut unsupported = record.clone();
        unsupported["attachment"]["content"]["type"] = json!("image");
        assert!(artifact(&unsupported).body.is_none());
        let mut oversized = record;
        oversized["attachment"]["content"]["file"]["content"] =
            json!("界".repeat(crate::content::MAX_ARTIFACT_BODY_CHARS + 1));
        let reference = artifact(&oversized);
        let body = reference.body.unwrap();
        assert_eq!(
            body.text.chars().count(),
            crate::content::MAX_ARTIFACT_BODY_CHARS
        );
        assert!(body.truncated);
        assert_eq!(
            reference.body_availability,
            Some(ContentAvailability::ReadBound)
        );
    }

    #[test]
    fn persisted_envelopes_require_complete_qualified_carriers() {
        let envelope = "<persisted-output>\nOutput too large (30.1KB). Full output saved to: /recordings/20000000-0000-4000-8000-000000000002/tool-results/result001.txt\n\nPreview (first 2KB):\npreview \n\n</persisted-output>";
        let block = json!({"content":envelope,"tool_use_id":"tool-synthetic"});
        let output = persisted_tool_output(&block).unwrap();
        assert_eq!(output.preview, "preview \n");
        assert_eq!(
            output.reference.tool_call_id.as_deref(),
            Some("tool-synthetic")
        );
        for malformed in [
            envelope.replace("30.1KB", "unknown"),
            envelope.replace("Preview (first 2KB):", "Preview:"),
            envelope.replace("/tool-results/", "/tool-results/../tool-results/"),
            envelope.replace("20000000-0000-4000-8000-000000000002", "not-a-session"),
            envelope.replace("/recordings/", "recordings/"),
            envelope.replace("result001.txt", "result001.bin"),
            envelope.replace("</persisted-output>", ""),
            format!("prefix {envelope}"),
            format!("{envelope} suffix"),
        ] {
            assert!(persisted_tool_output(&json!({"content":malformed})).is_none());
        }
        assert!(
            persisted_tool_output(&json!({"content":[{"type":"text","text":envelope}]})).is_none()
        );
    }
}
