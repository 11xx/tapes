use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    accounting_for, bounded_launch, default_native_store, enrich_native_session, head_directory,
    list_discovered_files, list_discovered_files_with_search, merge_launch, read_bounds,
    read_recording, replay_pin, skipped_records_note, stream_jsonl, streamed_trailing_record,
    timestamp, trailing_record, transcript, transcript_from_recording, ActivityRange, Backend,
    Jsonl, Listing, ParsedFile, Query, StreamedChild, StreamedTranscript, TokenTotals,
    UnmappedTally,
};
use crate::content::{
    bounded_shape, bounded_text, text_part, tool_coverage, tool_part, ArtifactReference,
    ContentAvailability, ContentCarrier, ContentCoverage, ContentPart,
};
use crate::event::{Bounded, EventKind, ReadLines, ToolEvent};
use crate::history::PageProjection;
use crate::lineage::{ChildRef, Lineage, SourceRef};
use crate::model::{
    derive_title, Accounting, AccountingBasis, AccountingCoverage, CacheWrites, Cost, CostCoverage,
    KindDeclaration, Launch, Model, Role, Session, SourceDescriptor, Tokens, TrailingRecord,
    Transcript, Turn, TurnKind, TurnSelection,
};
use crate::reconcile::{FamilyLedger, Reconciliation, SourceRole};
use crate::usage::{Durations, ModelUsage, RecordedUsage, RequestDimensions, UsageDetail};
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
            for turn in &mut parsed {
                crate::artifact::attach_tokens(turn, native_id, &self.source(path));
            }
            turns.extend(parsed);
        }
        let requests = claude_request_tokens(&read.values);
        let observed_cost_state = newest_cost_state(&read.values);
        let cost_state = covering_cost_state(observed_cost_state, requests.tokens.as_ref());
        let (tokens, cost, basis) = recorded_accounting(cost_state, requests.tokens);
        // A cost-state record is cumulative for the whole session wherever the
        // read reached it; only a sum over the read's requests is bounded by
        // the read.
        let coverage = if basis == AccountingBasis::RecordedTotal || !read.truncated {
            AccountingCoverage::Session
        } else {
            AccountingCoverage::ReadWindow
        };
        let accounting = accounting_for(tokens.as_ref(), cost.as_ref(), basis, coverage);
        let usage_detail = claude_usage_detail(
            observed_cost_state,
            requests.dimensions,
            requests.failures,
            basis,
            tokens.as_ref(),
        );

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
            "claude",
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
                            for turn in &mut turns {
                                crate::artifact::attach_tokens(turn, &session.id, &session.source);
                            }
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
            |native, path| head_directory(path, native, claude_cwd),
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
            |native, path| head_directory(path, native, claude_cwd),
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
        let (turns, recording, trailing_record, unmapped) =
            read_transcript(path, self.read_bytes, session)?;
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

    fn supports_full_search(&self) -> bool {
        true
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
            super::validate_native_identity(value, "claude", &session.id)?;
            launch = merge_launch(launch, claude_entrypoint(value));
            Ok(false)
        })?;
        let domain = format!("file:{}", path.display());
        let mut unmapped = UnmappedTally::default();
        let read = stream_jsonl(path, Some(opening.pin()), |value, span, revision| {
            super::validate_native_identity(value, "claude", &session.id)?;
            let mut parsed = parse_turns(value);
            if parsed.is_empty() {
                count_unmapped(&mut unmapped, value);
            }
            super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
            for parsed_turn in &mut parsed {
                crate::artifact::attach_tokens(parsed_turn, &session.id, &session.source);
            }
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
    /// them; a plugin-steered child's bounded ending can establish completion.
    /// Enumeration and child-ending I/O failures leave lineage unreadable.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let path = self.recording(session)?;
        let recording = read_recording(&path, self.read_bytes)?;
        super::validate_recording_identity(&recording, "claude", &session.id)?;
        let read = &recording.tail;
        let mut calls = AgentCalls::default();
        for value in &read.values {
            calls.observe(value);
        }
        Ok(Lineage {
            truncation: read_bounds(read),
            ..self.relatives(&path, &session.id, calls)?
        })
    }

    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        let path = self.recording(session)?;
        let mut calls = AgentCalls::default();
        let read = stream_jsonl(&path, None, |value, _, _| {
            super::validate_native_identity(value, "claude", &session.id)?;
            calls.observe(value);
            Ok(false)
        })?;
        let mut lineage = self.relatives(&path, &session.id, calls)?;
        lineage.notes.extend(skipped_records_note(read.skipped));
        Ok(lineage)
    }

    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        let path = self.recording(session)?;
        let mut records = ClaudeRecords::default();
        let mut last_end = 0;
        stream_jsonl(&path, Some(read.pin()?), |value, span, _| {
            super::validate_native_identity(value, "claude", &session.id)?;
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

    fn usage_reconciliation(&self, session: &Session) -> Result<Reconciliation> {
        let path = self.recording(session)?;
        let mut ledger = FamilyLedger::default();
        let parent = ledger.source(SourceRole::Parent, None, format!("file:{}", path.display()));
        reconcile_source(&mut ledger, parent, &path, &session.id, true)?;
        match subagent_files(&path) {
            Ok(files) => {
                for file in files {
                    let child = ledger.source(
                        SourceRole::Child,
                        Some(file.agent_id),
                        format!("file:{}", file.transcript.display()),
                    );
                    if let Err(error) =
                        reconcile_source(&mut ledger, child, &file.transcript, &session.id, false)
                    {
                        ledger.failed(child, format!("{error:#}"));
                    }
                }
            }
            Err(error) => ledger.unenumerated(format!("{error:#}")),
        }
        Ok(ledger.finish())
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
            for parsed_turn in &mut parsed {
                crate::artifact::attach_tokens(parsed_turn, &parent.id, &self.source(&path));
            }
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
    fn relatives(&self, path: &Path, parent_id: &str, calls: AgentCalls) -> Result<Lineage> {
        let AgentCalls(mut calls) = calls;
        let mut children = Vec::new();
        let mut notes = Vec::new();
        for file in subagent_files(path)? {
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
            let ending = if file.plugin_steered && call.completed_at.is_none() {
                child_ending(&file.transcript, parent_id, self.read_bytes)?
            } else {
                None
            };
            if let Some(native_id) = ending.as_ref().and_then(|ending| ending.record.clone()) {
                source.push(SourceRef::Record { native_id });
            }
            children.push(ChildRef {
                role: file.agent_type.or(call.agent_type).or(call.role),
                model: file.model.or(call.model),
                spawned_at: call.spawned_at,
                completed_at: call.completed_at.or(ending.map(|ending| ending.at)),
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

        Ok(Lineage {
            children,
            notes,
            ..Lineage::default()
        })
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
        let observed_cost_state = self.cost_state.as_ref();
        let cost_state = covering_cost_state(observed_cost_state, requests.tokens.as_ref());
        let (tokens, cost, basis) = recorded_accounting(cost_state, requests.tokens);
        session.accounting = accounting_for(
            tokens.as_ref(),
            cost.as_ref(),
            basis,
            AccountingCoverage::Session,
        );
        session.usage_detail = claude_usage_detail(
            observed_cost_state,
            requests.dimensions,
            requests.failures,
            basis,
            tokens.as_ref(),
        );
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
    session: &Session,
) -> Result<(
    Vec<Turn>,
    super::Recording,
    Option<TrailingRecord>,
    crate::model::UnmappedRecords,
)> {
    let recording = read_recording(path, read_bytes)?;
    super::validate_recording_identity(&recording, "claude", &session.id)?;
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
        for turn in &mut parsed {
            crate::artifact::attach_tokens(turn, &session.id, &session.source);
        }
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
    let declined = claude_trailing_kind(value).is_some();
    tally.add(native_type, declined);
}

/// Subagent transcripts are recordings of their own; a parent read names how
/// many belong to it.
fn subagent_notes(path: &Path) -> Vec<String> {
    match subagent_transcript_count(path) {
        Ok(0) => Vec::new(),
        Ok(1) => vec!["1 subagent transcript belongs to this session.".to_owned()],
        Ok(count) => vec![format!(
            "{count} subagent transcripts belong to this session."
        )],
        Err(error) => vec![format!("Subagent transcript count unavailable: {error:#}")],
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

/// The newest `cost-state` whose counters cover the recording's request sum.
/// A total below that sum cannot cover the requests read; its recorded
/// observation remains separate from the selected request accounting.
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
            cost_state["totalCostUSD"]
                .as_f64()
                .and_then(|usd| Cost::from_written(usd, cost_state_tokens(cost_state).as_ref())),
            AccountingBasis::RecordedTotal,
        ),
        None => (requests, None, AccountingBasis::SummedRequests),
    }
}

/// Selected accounting detail, with a set-aside cumulative state kept
/// separately. Request dimensions describe the requests read; their TTL split
/// describes the session counters only under request-sum accounting.
fn claude_usage_detail(
    cost_state: Option<&Value>,
    requests: Option<RequestDimensions>,
    failures: usize,
    basis: AccountingBasis,
    selected_tokens: Option<&Tokens>,
) -> Option<UsageDetail> {
    let mut detail = if basis == AccountingBasis::RecordedTotal {
        cost_state.map(cost_state_detail).unwrap_or_default()
    } else {
        UsageDetail::default()
    };
    if basis == AccountingBasis::SummedRequests {
        if let Some(value) = cost_state {
            let recorded = recorded_usage(value);
            detail.accounting_notes.push(format!(
                "Newest recorded cost-state total {} was set aside because its counters are below or omit the request sum {}; accounting uses summed-requests. Its dollar facts remain separate in recorded_total.",
                serde_json::to_string(&recorded.tokens).unwrap_or_default(),
                serde_json::to_string(&selected_tokens).unwrap_or_default(),
            ));
            detail.recorded_total = Some(recorded);
        }
        detail.cache_write_by_ttl = requests.as_ref().map(|requests| requests.cache_write);
    }
    detail.requests = requests;
    detail.request_failures = failures;
    detail.into_option()
}

fn recorded_usage(value: &Value) -> RecordedUsage {
    let tokens = cost_state_tokens(value);
    let detail = cost_state_detail(value);
    RecordedUsage {
        accounting: Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        },
        cost: value["totalCostUSD"]
            .as_f64()
            .and_then(|usd| Cost::from_written(usd, tokens.as_ref())),
        written_cost_usd: value["totalCostUSD"].as_f64(),
        tokens,
        cost_coverage: detail.cost_coverage,
        by_model: detail.by_model,
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
                cost: usage["costUSD"]
                    .as_f64()
                    .and_then(|usd| Cost::from_written(usd, model_usage_tokens(usage).as_ref())),
                cost_coverage: CostCoverage::written(
                    usage["costUSD"].as_f64(),
                    model_usage_tokens(usage).as_ref(),
                ),
                request_count: None,
            })
            .collect::<Vec<_>>();
        usage.sort_by(|left, right| left.model.cmp(&right.model));
        usage
    });
    let mut cost_coverage = CostCoverage::written(
        value["totalCostUSD"].as_f64(),
        cost_state_tokens(value).as_ref(),
    );
    if let Some(models) = &by_model {
        for coverage in models
            .iter()
            .filter_map(|model| model.cost_coverage.as_ref())
        {
            let total = cost_coverage.get_or_insert_with(CostCoverage::default);
            total.zero_written_observations += coverage.zero_written_observations;
        }
    }
    UsageDetail {
        context_window: None,
        rate_limits: None,
        durations_ms: (!durations.is_empty()).then_some(durations),
        by_model: by_model.filter(|usage| !usage.is_empty()),
        cost_coverage,
        ..UsageDetail::default()
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

fn claude_request_tokens(values: &[Value]) -> RequestUsage {
    let mut requests = RequestTokens::default();
    for value in values {
        requests.add(value);
    }
    requests.finish()
}

/// Per-request usage summed once per request id, since Claude repeats a
/// request's usage on every record the response spans. A response streamed
/// into several records can carry a partial count before its final one, so a
/// request counts the witness that dominates the others; of two witnesses
/// neither of which dominates, the first stands. The same requests'
/// price dimensions retain compatible observations independently of that
/// token selection. Absence erases no observed value; a TTL split applies
/// only to its recorded cache-write count, and incompatible dimensions are
/// qualified separately. `answeredWithoutRequest` contributes no request.
#[derive(Default)]
struct RequestTokens {
    requests: BTreeMap<String, RequestWitness>,
    unkeyed: TokenTotals,
    dimensions: RequestDimensions,
    failures: usize,
}

struct RequestWitness {
    tokens: Tokens,
    dimensions: RequestDimensions,
    ttl_witnesses: BTreeMap<u64, Option<CacheWrites>>,
    tool_envelope: [Option<u64>; 2],
}

const MAX_TTL_WITNESSES: usize = 16;
const TOOL_DIMENSIONS: [&str; 2] = ["web_search_requests", "web_fetch_requests"];

impl RequestWitness {
    fn new(tokens: Tokens, dimensions: RequestDimensions) -> Self {
        let mut witness = Self {
            tokens,
            dimensions,
            ttl_witnesses: BTreeMap::new(),
            tool_envelope: [None; 2],
        };
        witness.remember_ttl(witness.dimensions.cache_write);
        witness.tool_envelope = witness.tools();
        witness
    }

    fn tools(&self) -> [Option<u64>; 2] {
        [
            self.dimensions.web_search_requests,
            self.dimensions.web_fetch_requests,
        ]
    }

    fn remember_ttl(&mut self, split: CacheWrites) {
        if split.ephemeral_5m.is_none() || split.ephemeral_1h.is_none() {
            return;
        }
        let count = split.total();
        if let Some(held) = self.ttl_witnesses.get_mut(&count) {
            if *held != Some(split) {
                *held = None;
            }
        } else if self.ttl_witnesses.len() < MAX_TTL_WITNESSES {
            self.ttl_witnesses.insert(count, Some(split));
        } else {
            self.dimensions.omitted_values += 1;
        }
    }

    fn retain_tools(&mut self, incoming: &RequestDimensions) {
        let held = self.tools();
        let fresh = [incoming.web_search_requests, incoming.web_fetch_requests];
        let values = [fresh[0].or(held[0]), fresh[1].or(held[1])];
        let conflicting = TOOL_DIMENSIONS
            .iter()
            .any(|name| self.dimensions.conflicting.contains_key(name));
        let dominates = |left: [Option<u64>; 2], right: [Option<u64>; 2]| {
            left.into_iter()
                .zip(right)
                .all(|(left, right)| match (left, right) {
                    (_, None) => true,
                    (Some(left), Some(right)) => left >= right,
                    _ => false,
                })
        };
        if dominates(values, self.tool_envelope)
            && (!conflicting || dominates(fresh, self.tool_envelope))
        {
            self.dimensions.web_search_requests = values[0];
            self.dimensions.web_fetch_requests = values[1];
            for name in TOOL_DIMENSIONS {
                self.dimensions.conflicting.remove(name);
            }
        } else if !dominates(held, values) {
            for (index, name) in TOOL_DIMENSIONS.iter().enumerate() {
                if held[index] != values[index] {
                    self.dimensions.conflicting.insert(name, 1);
                }
            }
        }
        for (bound, value) in self.tool_envelope.iter_mut().zip(values) {
            if let Some(value) = value {
                *bound = Some(bound.unwrap_or(0).max(value));
            }
        }
    }

    fn observe(&mut self, tokens: Tokens, dimensions: RequestDimensions) {
        self.remember_ttl(dimensions.cache_write);
        self.retain_tools(&dimensions);
        self.dimensions.requests = self.dimensions.requests.max(dimensions.requests);
        self.dimensions.omitted_values += dimensions.omitted_values;
        for (name, held, incoming) in [
            (
                "service_tier",
                &mut self.dimensions.service_tier,
                dimensions.service_tier,
            ),
            ("speed", &mut self.dimensions.speed, dimensions.speed),
            (
                "inference_geo",
                &mut self.dimensions.inference_geo,
                dimensions.inference_geo,
            ),
        ] {
            if self.dimensions.conflicting.contains_key(name) {
                continue;
            }
            if !incoming.is_empty() {
                if held.is_empty() {
                    *held = incoming;
                } else if *held != incoming {
                    held.clear();
                    self.dimensions.conflicting.insert(name, 1);
                }
            }
        }
        if tokens.dominates(&self.tokens) {
            self.tokens = tokens;
        }
    }

    fn finish(mut self) -> (Tokens, RequestDimensions) {
        if self.dimensions.requests == 0 {
            return (self.tokens, self.dimensions);
        }
        for (name, missing) in [
            ("service_tier", self.dimensions.service_tier.is_empty()),
            ("speed", self.dimensions.speed.is_empty()),
            ("inference_geo", self.dimensions.inference_geo.is_empty()),
            (
                "web_search_requests",
                self.dimensions.web_search_requests.is_none(),
            ),
            (
                "web_fetch_requests",
                self.dimensions.web_fetch_requests.is_none(),
            ),
        ] {
            self.dimensions.missing.remove(name);
            if missing && !self.dimensions.conflicting.contains_key(name) {
                self.dimensions.missing.insert(name, 1);
            }
        }
        for (name, value) in [
            (
                "web_search_requests",
                &mut self.dimensions.web_search_requests,
            ),
            (
                "web_fetch_requests",
                &mut self.dimensions.web_fetch_requests,
            ),
        ] {
            if self.dimensions.conflicting.contains_key(name) {
                *value = None;
            }
        }
        self.dimensions.missing.remove("cache_write_ttl");
        self.dimensions.cache_write = match self.tokens.cache_write {
            Some(count) => match self.ttl_witnesses.get(&count) {
                Some(Some(split)) => *split,
                held => {
                    if held.is_some() {
                        self.dimensions.conflicting.insert("cache_write_ttl", 1);
                    } else if count > 0 {
                        self.dimensions.missing.insert("cache_write_ttl", 1);
                    }
                    CacheWrites {
                        ttl_unrecorded: Some(count),
                        ..CacheWrites::default()
                    }
                }
            },
            None => CacheWrites::default(),
        };
        (self.tokens, self.dimensions)
    }
}

/// What the requests a read reached sum to, and what they recorded beside it.
struct RequestUsage {
    tokens: Option<Tokens>,
    dimensions: Option<RequestDimensions>,
    failures: usize,
}

impl RequestTokens {
    fn finish(self) -> RequestUsage {
        let mut totals = self.unkeyed;
        let mut dimensions = self.dimensions;
        for witness in self.requests.into_values() {
            let (tokens, request_dimensions) = witness.finish();
            dimensions.add(request_dimensions);
            totals.add(
                tokens.input,
                tokens.output,
                tokens.reasoning,
                tokens.cache_read,
                tokens.cache_write,
            );
        }
        RequestUsage {
            tokens: totals.finish(),
            failures: self.failures,
            dimensions: (dimensions.requests > 0).then_some(dimensions),
        }
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
        if message["model"] == "<synthetic>" {
            self.failures += usize::from(value["isApiErrorMessage"] == true);
            return;
        }
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            return;
        };
        let tokens = request_usage_tokens(usage);
        let mut dimensions = RequestDimensions::default();
        if value["answeredWithoutRequest"] != true {
            request_dimensions(&mut dimensions, usage);
        }
        match value["requestId"].as_str() {
            Some(request_id) => match self.requests.get_mut(request_id) {
                Some(held) => {
                    held.observe(tokens, dimensions);
                    return;
                }
                None => {
                    self.requests.insert(
                        request_id.to_owned(),
                        RequestWitness::new(tokens, dimensions),
                    );
                    return;
                }
            },
            None => self.unkeyed.add(
                tokens.input,
                tokens.output,
                tokens.reasoning,
                tokens.cache_read,
                tokens.cache_write,
            ),
        }
        self.dimensions.add(dimensions);
    }
}

/// One request record's native counters; thinking is a share of output.
fn request_usage_tokens(usage: &serde_json::Map<String, Value>) -> Tokens {
    let counter = |name: &str| usage.get(name).and_then(Value::as_u64);
    Tokens {
        input: counter("input_tokens"),
        output: counter("output_tokens"),
        reasoning: usage
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(Value::as_u64),
        cache_read: counter("cache_read_input_tokens"),
        cache_write: counter("cache_creation_input_tokens"),
    }
}

/// The usage an assistant record carries for the request it answers.
fn assistant_usage(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    if value["type"] != "assistant" {
        return None;
    }
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    message.get("usage").and_then(Value::as_object)
}

/// Read one family recording whole into the reconciliation ledger: its
/// request records, its hook answers, the `<synthetic>` records Claude writes
/// in place of a failed response, and, for the parent, every cost-state it
/// recorded. A child must positively witness matching native parent identity;
/// validation pins its bytes before any counters enter the ledger.
fn reconcile_source(
    ledger: &mut FamilyLedger,
    source: usize,
    path: &Path,
    session_id: &str,
    parent: bool,
) -> Result<()> {
    let mut identity_seen = false;
    let validated = stream_jsonl(path, None, |value, _, _| {
        identity_seen |= super::validate_native_identity(value, "claude", session_id)?;
        Ok(false)
    })?;
    if !parent && !identity_seen {
        anyhow::bail!("child recording has no native parent identity evidence");
    }
    let mut position = 0;
    let read = stream_jsonl(path, Some(validated.pin()), |value, _, _| {
        super::validate_native_identity(value, "claude", session_id)?;
        position += 1;
        if parent && value["type"] == "cost-state" {
            let by_model = value["modelUsage"]
                .as_object()
                .map(|models| {
                    models
                        .iter()
                        .filter_map(|(model, usage)| {
                            model_usage_tokens(usage).map(|tokens| (model.clone(), tokens))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ledger.recorded(
                position,
                recorded_counters(&by_model),
                by_model,
                recorded_usage(value),
                timestamp(&value["timestamp"]),
            );
        } else if value["type"] == "assistant" && value["message"]["role"] == "assistant" {
            if value["answeredWithoutRequest"] == true {
                ledger.unrequested(source);
            } else if value["isApiErrorMessage"] == true
                || value["message"]["model"] == "<synthetic>"
            {
                ledger.failed_request(source);
            } else if let Some(usage) = assistant_usage(value) {
                ledger.request(
                    source,
                    position,
                    value["requestId"].as_str(),
                    value["message"]["model"].as_str(),
                    request_usage_tokens(usage),
                    timestamp(&value["timestamp"]),
                );
            }
        }
        Ok(false)
    })?;
    let sha256 = read
        .prefix_sha256
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    ledger.read(
        source,
        read.revision,
        read.source_length,
        sha256,
        read.skipped,
    );
    Ok(())
}

/// A cost-state's counters summed over its models. A counter one model
/// does not record has no total: a sum over the others would understate it.
fn recorded_counters(by_model: &BTreeMap<String, Tokens>) -> Tokens {
    let sum = |counter: fn(&Tokens) -> Option<u64>| {
        (!by_model.is_empty())
            .then(|| {
                by_model.values().map(counter).try_fold(0u64, |sum, count| {
                    count.map(|count| sum.saturating_add(count))
                })
            })
            .flatten()
    };
    Tokens {
        input: sum(|tokens| tokens.input),
        output: sum(|tokens| tokens.output),
        reasoning: sum(|tokens| tokens.reasoning),
        cache_read: sum(|tokens| tokens.cache_read),
        cache_write: sum(|tokens| tokens.cache_write),
    }
}

/// Tally one request's price-relevant dimensions. Its cache writes count
/// toward a TTL only when `cache_creation` splits exactly the writes
/// `cache_creation_input_tokens` counts; otherwise they are writes without a
/// recorded TTL. A dimension recorded as null is not recorded.
fn request_dimensions(dimensions: &mut RequestDimensions, usage: &serde_json::Map<String, Value>) {
    dimensions.requests += 1;
    let counter = |value: Option<&Value>, name: &str| value.and_then(|value| value[name].as_u64());
    if let Some(written) = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
    {
        let split = usage.get("cache_creation");
        let request = match (
            counter(split, "ephemeral_5m_input_tokens"),
            counter(split, "ephemeral_1h_input_tokens"),
        ) {
            (Some(short), Some(long)) if short.checked_add(long) == Some(written) => CacheWrites {
                ephemeral_5m: Some(short),
                ephemeral_1h: Some(long),
                ttl_unrecorded: None,
            },
            _ => {
                if written > 0 {
                    *dimensions.missing.entry("cache_write_ttl").or_default() += 1;
                }
                CacheWrites {
                    ttl_unrecorded: Some(written),
                    ..CacheWrites::default()
                }
            }
        };
        dimensions.cache_write.add(&request);
    }
    for name in ["service_tier", "speed", "inference_geo"] {
        dimensions.tally(name, usage.get(name).and_then(Value::as_str));
    }
    let server = usage.get("server_tool_use");
    for name in ["web_search_requests", "web_fetch_requests"] {
        dimensions.count(name, counter(server, name));
    }
}

fn claude_trailing_kind(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "cost-state" => Some("cost-state"),
        "last-prompt" => Some("last-prompt"),
        "ai-title" => Some("ai-title"),
        "custom-title" => Some("custom-title"),
        "agent-name" => Some("agent-name"),
        "mode" => Some("mode"),
        "permission-mode" => Some("permission-mode"),
        "atis-latch" => Some("atis-latch"),
        "queue-operation" => Some("queue-operation"),
        "bridge-session" => Some("bridge-session"),
        "file-history-snapshot" => Some("file-history-snapshot"),
        "file-history-delta" => Some("file-history-delta"),
        "system" => match value["subtype"].as_str()? {
            "turn_duration" => Some("system/turn_duration"),
            "stop_hook_summary" => Some("system/stop_hook_summary"),
            _ => None,
        },
        _ => None,
    }
}

/// Count transcript filenames only when the entire child directory is readable.
fn subagent_transcript_count(path: &Path) -> Result<usize> {
    Ok(subagent_files(path)?.len())
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
    if matches!(kind, "instructions" | "environment" | "prompt_snapshot") {
        return attached_context(value, kind);
    }
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
                omitted_reason: (availability == ContentAvailability::ReadBound).then(|| {
                    "attachment body exceeds a recorded or retained field bound".to_owned()
                }),
            },
        },
    )
}

fn attached_context(value: &Value, kind: &str) -> Option<Turn> {
    let attachment = &value["attachment"];
    let native_kind = format!("attachment/{kind}");
    let mut parts = Vec::new();
    let mut omitted_parts = 0;
    let mut cut = false;
    let mut unsupported = false;
    match kind {
        "instructions" => {
            if let Some(files) = attachment["files"].as_array() {
                omitted_parts = files
                    .len()
                    .saturating_sub(crate::content::MAX_ARTIFACT_REFERENCES);
                cut |= omitted_parts > 0;
                for (index, file) in files
                    .iter()
                    .take(crate::content::MAX_ARTIFACT_REFERENCES)
                    .enumerate()
                {
                    let Some(path) = file["path"].as_str() else {
                        omitted_parts += 1;
                        unsupported = true;
                        continue;
                    };
                    let path = bounded_text(path, crate::content::MAX_DESCRIPTOR_CHARS);
                    cut |= path.truncated;
                    let body = file["content"]
                        .as_str()
                        .map(|body| bounded_text(body, crate::content::MAX_ARTIFACT_BODY_CHARS));
                    let availability = match &body {
                        Some(body) if body.truncated => ContentAvailability::ReadBound,
                        Some(_) => ContentAvailability::RetainedBody,
                        None => ContentAvailability::ReferenceOnly,
                    };
                    cut |= body.as_ref().is_some_and(|body| body.truncated);
                    let text = body.as_ref().map(|body| body.text.clone());
                    let source_field = format!("attachment.files[{index}]");
                    parts.push(ContentPart::FileReference {
                        reference: ArtifactReference {
                            kind: native_kind.clone(),
                            path: Some(path.text),
                            descriptor_truncated: path.truncated,
                            body,
                            body_availability: Some(availability),
                            ..ArtifactReference::default()
                        },
                        source_field: source_field.clone(),
                        native_kind: native_kind.clone(),
                        record_ref: None,
                    });
                    if let Some(text) = text {
                        parts.push(text_part(
                            text,
                            &format!("{source_field}.content"),
                            &native_kind,
                        ));
                    }
                    unsupported |= file.get("content").is_some_and(|body| !body.is_string());
                    unsupported |= file.as_object().is_some_and(|file| {
                        file.keys()
                            .any(|key| !matches!(key.as_str(), "path" | "content" | "type"))
                    });
                }
            } else {
                unsupported = true;
            }
        }
        "environment" => {
            for field in ["snapshot", "changes"] {
                let Some(environment) = attachment.get(field) else {
                    continue;
                };
                let projected = environment_projection(
                    environment,
                    field,
                    &mut unsupported,
                    &mut omitted_parts,
                );
                if let Some(projected) = projected {
                    let body = bounded_json_body(&projected);
                    cut |= body.truncated;
                    parts.push(text_part(
                        body.text,
                        &format!("attachment.{field}"),
                        &native_kind,
                    ));
                }
            }
            unsupported |= parts.is_empty();
            cut |= omitted_parts > 0;
        }
        "prompt_snapshot" => {
            let mut descriptor = serde_json::Map::new();
            if let Some(prompt) = attachment["systemPrompt"].as_array() {
                if prompt.iter().all(Value::is_string) {
                    let chars: usize = prompt
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|text| text.chars().count())
                        .sum();
                    descriptor.insert("system_prompt_chars".to_owned(), Value::from(chars));
                } else {
                    unsupported = true;
                }
            } else if attachment.get("systemPrompt").is_some() {
                unsupported = true;
            }
            if let Some(tools) = attachment["tools"].as_array() {
                descriptor.insert("tool_count".to_owned(), Value::from(tools.len()));
            } else if attachment.get("tools").is_some() {
                unsupported = true;
            }
            if let Some(system_turns) = attachment["systemTurns"].as_bool() {
                descriptor.insert("system_turns".to_owned(), Value::from(system_turns));
            } else if attachment.get("systemTurns").is_some() {
                unsupported = true;
            }
            parts.push(ContentPart::StructuredArtifact {
                descriptor: bounded_text(
                    &Value::Object(descriptor).to_string(),
                    crate::content::MAX_DESCRIPTOR_CHARS,
                ),
                source_field: "attachment".to_owned(),
                native_kind: native_kind.clone(),
                reference: Some(ArtifactReference {
                    kind: native_kind,
                    body_availability: Some(ContentAvailability::ReferenceOnly),
                    ..ArtifactReference::default()
                }),
                record_ref: None,
            });
        }
        _ => return None,
    }
    if parts.is_empty() {
        parts.push(ContentPart::Unknown {
            native_kind: format!("attachment/{kind}"),
            descriptor: bounded_text(
                "missing or unsupported attachment fields",
                crate::content::MAX_DESCRIPTOR_CHARS,
            ),
            source_field: "attachment".to_owned(),
            record_ref: None,
        });
    }
    let availability = if cut {
        ContentAvailability::ReadBound
    } else if unsupported {
        ContentAvailability::UnsupportedRepresentation
    } else if kind == "prompt_snapshot" || !parts.iter().any(|part| part.text().is_some()) {
        ContentAvailability::ReferenceOnly
    } else {
        ContentAvailability::RetainedBody
    };
    let retained_parts = parts.len();
    turn(
        Role::System,
        TurnKind::Ambient,
        crate::content::project_text(&parts),
        timestamp(&value["timestamp"]),
        value["uuid"].as_str().map(str::to_owned),
        None,
        TurnContent {
            parts,
            coverage: ContentCoverage {
                carrier: if kind == "environment" {
                    ContentCarrier::DirectPart
                } else {
                    ContentCarrier::AssociatedArtifact
                },
                availability,
                retained_parts,
                omitted_parts,
                omitted_reason: if cut {
                    Some(
                        "attachment content exceeds a retained field or collection bound"
                            .to_owned(),
                    )
                } else if unsupported {
                    Some("attachment field is missing or has an unsupported shape".to_owned())
                } else {
                    None
                },
            },
        },
    )
}

fn environment_projection(
    value: &Value,
    field: &str,
    unsupported: &mut bool,
    omitted: &mut usize,
) -> Option<Value> {
    const SNAPSHOT_FIELDS: &[&str] = &[
        "additionalWorkingDirectories",
        "isGitRepo",
        "isWorktree",
        "osVersion",
        "platform",
        "scratchpadDirectory",
        "shell",
        "workingDirectory",
    ];
    const CHANGE_FIELDS: &[&str] = &["field", "from", "added", "removed"];
    let project = |value: &Value, fields: &[&str], unsupported: &mut bool| {
        let object = value.as_object()?;
        let mut result = serde_json::Map::new();
        for (key, value) in object {
            if !fields.contains(&key.as_str()) {
                *unsupported = true;
                continue;
            }
            let valid = match key.as_str() {
                "isGitRepo" | "isWorktree" => value.is_boolean(),
                "additionalWorkingDirectories" | "added" | "removed" => value
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string)),
                _ => value.is_string(),
            };
            if valid {
                result.insert(key.clone(), value.clone());
            } else {
                *unsupported = true;
            }
        }
        Some(Value::Object(result))
    };
    if field == "snapshot" {
        let result = project(value, SNAPSHOT_FIELDS, unsupported);
        *unsupported |= result.is_none();
        result
    } else if let Some(changes) = value.as_array() {
        *omitted += changes
            .len()
            .saturating_sub(crate::content::MAX_CONTENT_PARTS);
        let mut result = Vec::new();
        for change in changes.iter().take(crate::content::MAX_CONTENT_PARTS) {
            if !change["field"]
                .as_str()
                .is_some_and(|field| SNAPSHOT_FIELDS.contains(&field))
            {
                *unsupported = true;
                continue;
            }
            if let Some(change) = project(change, CHANGE_FIELDS, unsupported) {
                result.push(change);
            } else {
                *unsupported = true;
            }
        }
        Some(Value::Array(result))
    } else {
        *unsupported = true;
        None
    }
}

fn bounded_json_body(value: &Value) -> crate::model::BoundedText {
    // Serialization stops at the byte ceiling; the retained prefix ends on a UTF-8 boundary.
    struct Writer {
        bytes: Vec<u8>,
        cut: bool,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let remaining = crate::content::MAX_ARTIFACT_BODY_CHARS * 4 - self.bytes.len();
            self.bytes
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
            if bytes.len() > remaining {
                self.cut = true;
                return Err(std::io::Error::other("attachment field bound"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        cut: false,
    };
    let _ = serde_json::to_writer(&mut writer, value);
    while std::str::from_utf8(&writer.bytes).is_err() {
        writer.bytes.pop();
    }
    let mut body = bounded_text(
        std::str::from_utf8(&writer.bytes).unwrap(),
        crate::content::MAX_ARTIFACT_BODY_CHARS,
    );
    body.truncated |= writer.cut;
    body
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
    session: &'a str,
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
        session,
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

pub(crate) fn persisted_mapping_matches(value: &Value, session: &str) -> bool {
    value["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "tool_result")
        .filter_map(persisted_tool_output)
        .all(|output| output.session == session)
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
    /// A plugin launched this subagent and nothing answers it afterwards, so
    /// the parent records no `Agent` call for it and its own recording is the
    /// only statement of where it ended.
    plugin_steered: bool,
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

/// Where a subagent's own recording ends its turn.
struct ChildEnding {
    at: chrono::DateTime<chrono::Utc>,
    /// The identifier Claude wrote on the ending record.
    record: Option<String>,
}

/// The ending a subagent's recording states: its final conversation record is
/// an assistant record that stopped on `end_turn`. Housekeeping records
/// Claude appends after the conversation do not reopen it. A recording whose
/// final line cannot be parsed, whose final record lies outside the read bound,
/// or whose ending carries no timestamp states no ending. The opening and tail
/// must carry matching native parent identity evidence. Identity and I/O failures
/// are errors.
fn child_ending(path: &Path, parent_id: &str, read_bytes: u64) -> Result<Option<ChildEnding>> {
    let recording = read_recording(path, read_bytes)
        .with_context(|| format!("Subagent ending {} unavailable", path.display()))?;
    let mut identified = false;
    for value in recording.opening().iter().chain(&recording.tail.values) {
        if let Some(id) = value.get("sessionId") {
            if id.as_str() != Some(parent_id) {
                anyhow::bail!("child recording contains a different or invalid native parent ID");
            }
            identified = true;
        }
    }
    if !identified {
        anyhow::bail!("child recording has no native parent identity evidence");
    }
    let read = &recording.tail;
    let Some((index, last)) = read
        .values
        .iter()
        .enumerate()
        .rev()
        .find(|(_, value)| claude_trailing_kind(value).is_none())
    else {
        return Ok(None);
    };
    if read
        .gaps
        .iter()
        .any(|gap| gap.span.start >= read.spans[index].end)
    {
        return Ok(None);
    }
    if last["type"] != "assistant" || last["message"]["stop_reason"] != "end_turn" {
        return Ok(None);
    }
    Ok(timestamp(&last["timestamp"]).map(|at| ChildEnding {
        at,
        record: last["uuid"].as_str().map(str::to_owned),
    }))
}

/// The subagent transcripts recorded under a session's own directory. Each
/// carries a meta record naming the agent, and the file's stem carries the
/// agent id the parent's completion record repeats.
fn subagent_files(path: &Path) -> Result<Vec<SubagentFile>> {
    let session = path
        .file_stem()
        .ok_or_else(|| anyhow!("Claude recording has no file stem"))?;
    let directory = path.with_file_name(session).join("subagents");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Subagent enumeration {} unavailable", directory.display())
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry
            .with_context(|| format!("Subagent enumeration {} unavailable", directory.display()))?;
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            continue;
        }
        let Some(agent_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.strip_prefix("agent-"))
        else {
            continue;
        };
        let agent_id = agent_id.to_owned();
        let meta = path.with_file_name(format!("agent-{agent_id}.meta.json"));
        let (recorded, metadata_note) = subagent_metadata(&meta);
        files.push(SubagentFile {
            metadata_note,
            agent_id,
            transcript: path,
            meta,
            tool_use_id: recorded["toolUseId"].as_str().map(str::to_owned),
            agent_type: recorded["agentType"].as_str().map(str::to_owned),
            model: recorded["model"].as_str().map(str::to_owned),
            plugin_steered: recorded["pluginSteered"] == true
                && recorded["requestNonInteractive"] != false,
        });
    }
    files.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
    Ok(files)
}

fn subagent_metadata(path: &Path) -> (Value, Option<String>) {
    const LIMIT: usize = 64 * 1024;
    let read = || -> Result<Value> {
        let file = super::open_regular_file(path)?;
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
    fn attached_context_bounds_bodies_collections_and_unverified_fields() {
        let record = |attachment: Value| json!({"type":"attachment","attachment":attachment});
        let files: Vec<Value> = (0..65).map(|_| json!({"path":"AGENTS.md","content":"界".repeat(crate::content::MAX_ARTIFACT_BODY_CHARS + 1)})).collect();
        let instructions =
            claude_attachment(&record(json!({"type":"instructions","files":files}))).unwrap();
        assert_eq!(instructions.parts.len(), 128);
        let coverage = instructions.coverage.unwrap();
        assert_eq!(coverage.omitted_parts, 1);
        assert_eq!(coverage.availability, ContentAvailability::ReadBound);
        assert!(coverage.omitted_reason.is_some());
        for part in &instructions.parts {
            if let ContentPart::FileReference { reference, .. } = part {
                let body = reference.body.as_ref().unwrap();
                assert!(body.truncated);
                assert_eq!(body.chars, crate::content::MAX_ARTIFACT_BODY_CHARS + 1);
                assert_eq!(
                    reference.body_availability,
                    Some(ContentAvailability::ReadBound)
                );
            }
        }
        let environment = claude_attachment(&record(json!({"type":"environment","snapshot":{
            "workingDirectory":"界".repeat(crate::content::MAX_ARTIFACT_BODY_CHARS + 1),
            "unverified":"excluded"
        }})))
        .unwrap();
        assert!(environment.text.chars().count() <= crate::content::MAX_ARTIFACT_BODY_CHARS);
        assert!(!environment.text.contains("excluded"));
        assert_eq!(
            environment.coverage.as_ref().unwrap().availability,
            ContentAvailability::ReadBound
        );
        assert!(environment.coverage.unwrap().omitted_reason.is_some());
        let malformed = claude_attachment(&record(
            json!({"type":"instructions","files":[{"path":17,"content":"excluded"}]}),
        ))
        .unwrap();
        let coverage = malformed.coverage.unwrap();
        assert_eq!(
            coverage.availability,
            ContentAvailability::UnsupportedRepresentation
        );
        assert_eq!(coverage.omitted_parts, 1);
        assert!(malformed.text.is_empty());
        let changes: Vec<Value> = (0..129)
            .map(|_| json!({"field":"workingDirectory","from":"/fixtures/previous"}))
            .collect();
        let environment =
            claude_attachment(&record(json!({"type":"environment","changes":changes}))).unwrap();
        assert_eq!(environment.coverage.unwrap().omitted_parts, 1);
        let unverified = claude_attachment(&record(json!({"type":"environment","changes":[{"field":"unverified","from":"excluded"}],"snapshot":{"shell":{"nested":"excluded"}}}))).unwrap();
        assert!(!unverified.text.contains("excluded"));
        assert_eq!(
            unverified.coverage.unwrap().availability,
            ContentAvailability::UnsupportedRepresentation
        );
        assert!(
            claude_attachment(&record(json!({"type":"unverified","content":"excluded"}))).is_none()
        );
    }

    #[test]
    fn prompt_descriptor_preserves_false_and_omits_missing_counts() {
        let snapshot = claude_attachment(&json!({"type":"attachment","attachment":{
            "type":"prompt_snapshot","systemTurns":false,"cliPrefix":"excluded"
        }}))
        .unwrap();
        assert!(snapshot.text.is_empty());
        let ContentPart::StructuredArtifact {
            descriptor,
            reference,
            ..
        } = &snapshot.parts[0]
        else {
            panic!("missing descriptor")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&descriptor.text).unwrap(),
            json!({"system_turns":false})
        );
        assert!(reference.as_ref().unwrap().body.is_none());
        for kind in ["instructions", "environment", "prompt_snapshot"] {
            let turn = claude_attachment(&json!({"type":"attachment","attachment":{"type":kind}}))
                .unwrap();
            assert_eq!(turn.kind, crate::model::TurnKind::Ambient);
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
