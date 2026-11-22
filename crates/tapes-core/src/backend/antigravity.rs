use std::path::Path;

use agent_tapes_discovery::{NativeSession, NativeStore};
use anyhow::{Context, Result};
use serde_json::Value;

use super::{
    attach_record_refs, read_recording, replay_pin, stream_jsonl, timestamp,
    transcript_from_recording, ActivityRange, Backend, FoldedSession, Listing, Query,
    StreamCoordinates, StreamOptions, StreamedTranscript, UnmappedTally,
};
use crate::content::{text_part, tool_coverage, tool_part};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::model::{
    EntryMetadata, KindDeclaration, Model, Role, Session, SourceDescriptor, Transcript, Turn,
    TurnKind, TurnSelection,
};

const HARNESS: &str = "antigravity";

pub(super) struct AntigravityBackend {
    store: NativeStore,
    read_bytes: u64,
}

impl AntigravityBackend {
    pub(super) fn new(store: NativeStore, read_bytes: u64) -> Self {
        Self { store, read_bytes }
    }

    fn session(&self, identity: &NativeSession) -> Result<Session> {
        let database = identity
            .locator()
            .context("Antigravity identity has no database locator")?;
        let acp = database
            .parent()
            .and_then(Path::parent)
            .context("Antigravity database has no profile")?;
        let path = acp
            .join("brain")
            .join(identity.id())
            .join(".system_generated/logs/transcript_full.jsonl");
        check_recording_path(&path)?;
        let recording = read_recording(&path, self.read_bytes)?;
        let opening = recording.opening();
        anyhow::ensure!(
            opening.iter().chain(&recording.tail.values).any(|record| {
                record["step_index"].is_u64()
                    && record["source"].is_string()
                    && record["type"].is_string()
                    && record["status"].is_string()
            }),
            "Antigravity transcript has no readable native steps at {}",
            path.display()
        );
        let latest_model = recording.tail.values.iter().rev().find_map(record_model);
        let mut activity = ActivityRange::default();
        for record in recording.head.iter().chain(&recording.tail.values) {
            activity.observe_timestamp(timestamp(&record["created_at"]));
        }
        let mut source = SourceDescriptor::installed(HARNESS, path.display().to_string());
        source.representation = "antigravity-acp-transcript".to_owned();
        if let Some(location) = &mut source.location {
            location.native_path = Some(path);
            location.container = Some(database.display().to_string());
        }
        let mut session = Session {
            id: identity.id().to_owned(),
            source,
            metadata: None,
            model: latest_model,
            model_observation: None,
            model_selections: Vec::new(),
            launch: None,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory: None,
            started_at: None,
            last_activity_at: None,
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            start_uncertain: false,
            occurrence: None,
            usage_detail: None,
            list_price_estimate: None,
            t3: None,
        };
        activity.apply(&mut session);
        session.start_uncertain = recording.tail.truncated
            && !opening
                .iter()
                .any(|record| timestamp(&record["created_at"]).is_some());
        Ok(
            session.with_derived_title_from(opening.iter().filter_map(|record| {
                (record["type"] == "USER_INPUT" && record["source"] == "USER_EXPLICIT")
                    .then(|| read_turns(record).into_iter().next())
                    .flatten()
            })),
        )
    }

    fn list_candidates(&self, query: &Query) -> Listing {
        let page = self
            .store
            .candidates_with_prefix(query.ceiling, query.id_prefix.as_deref());
        let mut listing = Listing {
            scanned: page.scanned,
            scan_truncated: !page.complete,
            unavailable_ids: page.unreadable_ids,
            unavailable: page
                .failures
                .into_iter()
                .map(|error| error.to_string())
                .collect(),
            ..Listing::default()
        };
        for native in page.records {
            if !query.admits_id(native.id()) {
                continue;
            }
            match self.session(&native) {
                Ok(session) if query.scope.is_none() && query.matches(&session) => {
                    listing.sessions.push(session);
                }
                Ok(_) => {}
                Err(error) => {
                    listing.unavailable_ids.push(native.id().to_owned());
                    listing
                        .unavailable
                        .push(format!("Antigravity session {}: {error:#}", native.id()));
                }
            }
        }
        listing
            .sessions
            .sort_by(|a, b| crate::compare_sessions(a, b, query.sort));
        listing
    }

    fn t3_notes(&self, id: &str) -> Vec<String> {
        let Some(root) = self.store.root() else {
            return Vec::new();
        };
        crate::t3::T3Store::at(root)
            .antigravity_turn_notes(id)
            .unwrap_or_else(|error| {
                vec![format!(
                    "T3 Antigravity turn evidence unavailable: {error:#}"
                )]
            })
    }
}

impl Backend for AntigravityBackend {
    fn harness(&self) -> &'static str {
        HARNESS
    }

    fn available(&self) -> bool {
        self.store.available()
    }

    fn terminal_exact_failure(&self) -> bool {
        true
    }

    fn kinds(&self) -> KindDeclaration {
        KindDeclaration {
            recordable: TurnSelection::only([
                TurnKind::Operator,
                TurnKind::Assistant,
                TurnKind::Reasoning,
                TurnKind::Tool,
                TurnKind::Notice,
            ]),
            user_default: None,
        }
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let mut listing = self.list_candidates(query);
        listing.sessions.truncate(query.limit);
        Ok(listing)
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        let mut listing =
            super::filter_listing_search(self, self.list_candidates(query), needle, tail);
        listing.sessions.truncate(query.limit);
        Ok(listing)
    }

    fn locate(&self, id: &str) -> Result<Option<Session>> {
        self.store
            .locate_exact(id)?
            .as_ref()
            .map(|native| self.session(native))
            .transpose()
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let path = recording_path(session)?;
        let recording = read_recording(path, self.read_bytes)?;
        let domain = format!("file:{}", path.display());
        let mut turns = Vec::new();
        let mut unmapped = UnmappedTally::default();
        for (record, span) in recording.tail.values.iter().zip(&recording.tail.spans) {
            let mut parsed = read_and_tally(record, &mut unmapped);
            attach_record_refs(
                &mut parsed,
                &domain,
                Some(&recording.tail.source_revision),
                Some(*span),
            );
            turns.extend(parsed);
        }
        let mut transcript = transcript_from_recording(
            session.clone(),
            turns,
            tail,
            &recording,
            None,
            None,
            self.t3_notes(&session.id),
        );
        transcript.kinds = Some(self.kinds());
        if let Some(read) = &mut transcript.read {
            read.unmapped = Some(unmapped.finish());
        }
        Ok(transcript)
    }

    fn supports_full_search(&self) -> bool {
        true
    }

    fn stream_lineage(&self, session: &Session) -> Result<crate::lineage::Lineage> {
        self.lineage(session)
    }

    fn stream_transcript_with(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        options: StreamOptions,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let path = recording_path(session)?;
        let domain = format!("file:{}", path.display());
        let mut whole = session.clone();
        whole.model = None;
        let mut activity = ActivityRange::default();
        let mut unmapped = UnmappedTally::default();
        let mut stamps = super::TurnStamps::new(options);
        let mut emit = super::timed(&mut stamps, turn);
        let read = stream_jsonl(path, replay_pin(replay)?, |record, span, revision| {
            activity.observe_timestamp(timestamp(&record["created_at"]));
            if let Some(model) = record_model(record) {
                whole.model = Some(model);
            }
            let mut turns = read_and_tally(record, &mut unmapped);
            attach_record_refs(&mut turns, &domain, Some(revision), Some(span));
            let produced = !turns.is_empty();
            for turn in turns {
                emit(turn)?;
            }
            Ok(produced)
        })?;
        drop(emit);
        activity.apply(&mut whole);
        let (turn_times, requests, timeline_withheld) = stamps.finish(Ok(None));
        let notes = replay.map_or_else(|| self.t3_notes(&session.id), |read| read.notes.clone());
        Ok(StreamedTranscript {
            coordinates: StreamCoordinates::FileBytes,
            source_length: read.source_length,
            source_bounds: Vec::new(),
            source_revision: Some(read.revision),
            source_prefix_sha256: Some(read.prefix_sha256),
            skipped: read.skipped,
            gaps: read.gaps,
            trailing_record: None,
            terminal: None,
            notes,
            unmapped: Some(unmapped.finish()),
            unrecognized_usage: 0,
            kinds: Some(self.kinds()),
            folded: Some(Box::new(FoldedSession {
                asked: session.clone(),
                whole,
            })),
            turn_times,
            requests,
            timeline_withheld,
            locations: None,
        })
    }

    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        read.folded_for(session)
            .context("Antigravity stream belongs to a different session")
    }
}

fn recording_path(session: &Session) -> Result<&Path> {
    let path = super::session_file_path(session)
        .context("Antigravity session has no transcript locator")?;
    check_recording_path(path)?;
    Ok(path)
}

fn check_recording_path(path: &Path) -> Result<()> {
    // Check the brain path through its profile before opening any content.
    for directory in path.ancestors().skip(1).take(5) {
        let metadata = std::fs::symlink_metadata(directory)
            .with_context(|| format!("Antigravity transcript unavailable at {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_dir(),
            "Antigravity transcript directory is not a native directory: {}",
            directory.display()
        );
    }
    let info = std::fs::symlink_metadata(path)
        .with_context(|| format!("Antigravity transcript unavailable at {}", path.display()))?;
    anyhow::ensure!(
        info.is_file(),
        "Antigravity transcript is not a regular native file: {}",
        path.display()
    );
    Ok(())
}

fn record_model(record: &Value) -> Option<Model> {
    (record["type"] == "PLANNER_RESPONSE")
        .then(|| record["model"].as_str().filter(|id| !id.is_empty()))
        .flatten()
        .map(|id| Model {
            id: id.to_owned(),
            variant: None,
        })
}

fn read_and_tally(record: &Value, tally: &mut UnmappedTally) -> Vec<Turn> {
    let parsed = read_turns(record);
    if parsed.is_empty() {
        let kind = match record["type"].as_str() {
            Some("SYSTEM_MESSAGE" | "CHECKPOINT") => {
                tally.add(record["type"].as_str().unwrap_or_default().to_owned(), true);
                return parsed;
            }
            Some("PLANNER_RESPONSE") => "PLANNER_RESPONSE",
            Some("USER_INPUT") => "USER_INPUT",
            Some("GENERIC") => "GENERIC",
            _ => "(unknown Antigravity step)",
        };
        tally.add(kind.to_owned(), false);
    }
    parsed
}

fn read_turns(record: &Value) -> Vec<Turn> {
    let mut turns = Vec::new();
    match (record["type"].as_str(), record["source"].as_str()) {
        (Some("USER_INPUT"), Some("USER_EXPLICIT")) => {
            turns.extend(text_turn(record, Role::User, TurnKind::Operator, "content"));
        }
        (Some("PLANNER_RESPONSE"), Some("MODEL")) => {
            turns.extend(text_turn(
                record,
                Role::Assistant,
                TurnKind::Assistant,
                "content",
            ));
            turns.extend(text_turn(
                record,
                Role::Reasoning,
                TurnKind::Reasoning,
                "thinking",
            ));
            if let Some(calls) = record["tool_calls"].as_array() {
                for (index, call) in calls.iter().enumerate() {
                    if let Some(name) = call["name"].as_str().filter(|name| !name.is_empty()) {
                        let mut turn = turn(record, Role::Tool, TurnKind::Tool, call.to_string());
                        turn.parts.push(tool_part(
                            call,
                            &format!("tool_calls[{index}]"),
                            "tool-call",
                        ));
                        turn.coverage = Some(tool_coverage());
                        turn.tool = Some(tool_event(
                            EventKind::ToolCall,
                            "PLANNER_RESPONSE",
                            Some(name.to_owned()),
                            None,
                            Bounded::retaining_value(&call["args"]),
                            None,
                        ));
                        turns.push(turn);
                    }
                }
            }
        }
        (Some("GENERIC"), Some("MODEL")) => {
            if let Some(text) = record["content"].as_str().filter(|text| !text.is_empty()) {
                let mut result = turn(record, Role::Tool, TurnKind::Tool, text.to_owned());
                result
                    .parts
                    .push(tool_part(&record["content"], "content", "tool-result"));
                result.coverage = Some(tool_coverage());
                // A generic step's payload carries no call ID or tool name.
                result.tool = Some(tool_event(
                    EventKind::ToolResult,
                    "GENERIC",
                    None,
                    record["status"].as_str().map(str::to_owned),
                    None,
                    Some(Bounded::from_text(text)),
                ));
                turns.push(result);
            }
        }
        _ => {}
    }
    if record["status"] == "ERROR" && record["source"] == "MODEL" {
        turns.push(
            text_turn(record, Role::System, TurnKind::Notice, "error").unwrap_or_else(|| {
                turn(
                    record,
                    Role::System,
                    TurnKind::Notice,
                    "Antigravity recorded an error".to_owned(),
                )
            }),
        );
    }
    turns
}

fn text_turn(record: &Value, role: Role, kind: TurnKind, field: &str) -> Option<Turn> {
    let text = record[field].as_str().filter(|text| !text.is_empty())?;
    let mut turn = turn(record, role, kind, text.to_owned());
    turn.parts.push(text_part(
        text,
        field,
        record["type"].as_str().unwrap_or_default(),
    ));
    Some(turn)
}

fn turn(record: &Value, role: Role, kind: TurnKind, text: String) -> Turn {
    Turn {
        origin: None,
        text_truncated: None,
        text_chars: None,
        context_event: None,
        model: record_model(record),
        role,
        kind,
        text,
        ts: timestamp(&record["created_at"]),
        ordinal: 0,
        native_id: record["step_index"].as_u64().map(|index| index.to_string()),
        request_turn_id: None,
        metadata: record["status"].as_str().map(|status| EntryMetadata {
            engine: None,
            status: Some(status.to_owned()),
            label: None,
            source_fields: [("status".to_owned(), "/status".to_owned())].into(),
        }),
        record_ref: None,
        channel: None,
        recipient: None,
        parts: Vec::new(),
        coverage: None,
        tool: None,
    }
}

fn tool_event(
    kind: EventKind,
    subtype: &str,
    name: Option<String>,
    status: Option<String>,
    arguments: Option<Bounded>,
    output: Option<Bounded>,
) -> ToolEvent {
    ToolEvent {
        exit: None,
        kind,
        subtype: subtype.to_owned(),
        name,
        call_id: None,
        status,
        arguments,
        output,
        read: None,
        returned_read: None,
        commit_scan: None,
        edit_scan: None,
        completed_ts: None,
        invocations: Vec::new(),
        artifact_references: Vec::new(),
        artifact_consumptions: Vec::new(),
        self_contained: false,
    }
}
