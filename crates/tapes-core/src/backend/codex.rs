use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, head_directory, head_jsonl, home_path, jsonl_files, list_files,
    list_files_with_search, matching_session_file, read_bounds, read_recording, session_file,
    stream_jsonl, streamed_trailing_record, terminal_from_values, timestamp, trailing_record,
    transcript_from_recording, Backend, Jsonl, Listing, ParsedFile, Query, StreamedTranscript,
    TokenTotals,
};
use crate::content::{parts_from_array, project_text, tool_coverage, tool_part};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::history::{PageProjection, ReadContext};
use crate::lineage::{ChildRef, Lineage, ParentRef, SourceRef};
use crate::model::{
    is_known_envelope, is_known_notice, AccountingBasis, AccountingCoverage, Model, Role, Session,
    SourceDescriptor, TerminalObservation, Tokens, TrailingRecord, Transcript, Turn, TurnKind,
};
use crate::usage::{Credits, RateLimits, RateWindow, UsageDetail};

#[derive(Clone, Debug)]
pub struct CodexBackend {
    root: Option<PathBuf>,
    /// How much of a recording's end a transcript read takes.
    read_bytes: u64,
}

type CodexTranscriptRead = (
    Vec<Turn>,
    super::Recording,
    Option<TrailingRecord>,
    Option<TerminalObservation>,
);

impl CodexBackend {
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
        let recording = read_recording(path, self.read_bytes)?;
        let (started_at, last_activity_at) = recording
            .time_range()
            .map_or((None, None), |(started, activity)| {
                (Some(started), Some(activity))
            });
        // `session_meta` is the file's first line, so the opening is where the
        // id and the recorded working directory live whatever the file's size.
        let opening = recording.opening();
        let read = &recording.tail;
        let id = opening
            .iter()
            .find(|value| value["type"] == "session_meta")
            .and_then(|value| value["payload"]["id"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.get(stem.len().saturating_sub(36)..))
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow!("{} has no session id", path.display()))?;
        let directory = opening
            .iter()
            .find_map(codex_cwd)
            .map(PathBuf::from)
            .or_else(|| {
                read.values
                    .iter()
                    .rev()
                    .find_map(codex_cwd)
                    .map(PathBuf::from)
            });
        let model = read
            .values
            .iter()
            .rev()
            .find(|value| value["type"] == "turn_context")
            .and_then(|value| {
                let payload = &value["payload"];
                payload["model"].as_str().map(|id| Model {
                    id: id.to_owned(),
                    variant: payload["effort"].as_str().map(str::to_owned),
                })
            });
        let turns = read.values.iter().flat_map(parse_turns).collect::<Vec<_>>();
        let tokens = read.values.iter().rev().find_map(codex_tokens);
        let accounting = accounting_for(
            tokens.as_ref(),
            None,
            AccountingBasis::RecordedTotal,
            AccountingCoverage::Session,
        );
        let usage_detail = UsageDetail {
            context_window: read.values.iter().rev().find_map(codex_context_window),
            rate_limits: read.values.iter().rev().find_map(codex_rate_limits),
            ..UsageDetail::default()
        }
        .into_option();

        let session = Session {
            id,
            source: SourceDescriptor::installed("codex", path.display().to_string()),
            metadata: None,
            model,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory,
            started_at,
            last_activity_at,
            live: None,
            cost: None,
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
            if projection == PageProjection::Transcript {
                ReadContext::OperatorProvenance
            } else {
                ReadContext::None
            },
            |values, spans, _opening, _context, revision| {
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
                    .filter(|value| value["type"] == "turn_context")
                    .filter_map(|value| {
                        let payload = &value["payload"];
                        Some(crate::history::ModelObservation {
                            model: Model {
                                id: payload["model"].as_str()?.to_owned(),
                                variant: payload["effort"].as_str().map(str::to_owned),
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

impl Default for CodexBackend {
    fn default() -> Self {
        let root = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home_path(&[".codex"]))
            .map(|path| path.join("sessions"));
        Self {
            root,
            read_bytes: super::DEFAULT_READ_BYTES,
        }
    }
}

impl Backend for CodexBackend {
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
        "codex"
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
            |path| head_directory(path, codex_cwd),
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
            jsonl_files(root),
            query,
            needle,
            tail,
            self.read_bytes,
            |path| head_directory(path, codex_cwd),
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
        let Some(path) = session_file(root, id) else {
            return Ok(None);
        };
        Ok(self.parse(&path).ok().map(|(session, _, _)| session))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("codex store is unavailable"))?;
        let path = session_file(root, &session.id)
            .ok_or_else(|| anyhow!("codex session {} is unavailable", session.id))?;
        let (turns, recording, trailing_record, terminal) =
            read_transcript(&path, self.read_bytes)?;
        Ok(transcript_from_recording(
            session.clone(),
            turns,
            tail,
            &recording,
            terminal,
            trailing_record,
            Vec::new(),
        ))
    }

    fn stream_transcript(
        &self,
        session: &Session,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("codex store is unavailable"))?;
        let path = session_file(root, &session.id)
            .ok_or_else(|| anyhow!("codex session {} is unavailable", session.id))?;
        let domain = format!("file:{}", path.display());
        let mut terminal = None;
        let read = stream_jsonl(&path, |value, span, revision| {
            if let Some(observed) = codex_terminal(value) {
                terminal = Some(observed);
            }
            let mut parsed = parse_turns(value);
            super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
            let produced = !parsed.is_empty();
            for parsed_turn in parsed {
                turn(parsed_turn)?;
            }
            Ok(produced)
        })?;
        Ok(StreamedTranscript {
            source_length: read.source_length,
            skipped: read.skipped,
            trailing_record: streamed_trailing_record(read.last.as_ref(), codex_trailing_kind),
            terminal,
            gaps: read.gaps,
            notes: Vec::new(),
        })
    }

    /// A Codex child is an ordinary rollout whose header names its parent, so
    /// lineage joins two independent records: the spawn and wait calls in the
    /// parent, keyed by the agent path they name, and the headers of the
    /// store's own recordings. Only headers are read; no child's turns are.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("codex store is unavailable"))?;
        let files = jsonl_files(root);
        let path = matching_session_file(files.clone(), &session.id)
            .ok_or_else(|| anyhow!("codex session {} is unavailable", session.id))?;
        let recording = read_recording(&path, self.read_bytes)?;
        let header = recording
            .opening()
            .iter()
            .find(|value| value["type"] == "session_meta")
            .map(|value| value["payload"].clone())
            .unwrap_or(Value::Null);

        let parent = header["parent_thread_id"]
            .as_str()
            .map(|native_id| ParentRef {
                resolved: matching_session_file(files.clone(), native_id).is_some(),
                native_id: native_id.to_owned(),
                source: "session_meta.parent_thread_id".to_owned(),
            });

        let mut agents = spawned_agents(&recording.tail.values);
        let (probed, probe_bound) = child_headers(&files, &path, &session.id);
        for child in probed {
            let entry = agents.entry(child.agent_path.clone()).or_default();
            entry.session_id = Some(child.thread_id);
            entry.nickname = child.nickname.or_else(|| entry.nickname.take());
            entry.source.push(SourceRef::File {
                path: child.path.display().to_string(),
            });
        }

        let children = agents
            .into_iter()
            .map(|(reference, agent)| ChildRef {
                role: agent.nickname.or(agent.task_name),
                model: agent.model,
                group: agent_group(&reference),
                spawned_at: agent.spawned_at,
                completed_at: agent.completed_at,
                disposition: agent.disposition,
                resolved: agent.session_id.is_some(),
                session_id: agent.session_id,
                source: agent.source,
                ..ChildRef::new(reference, self.harness())
            })
            .collect();

        let mut notes = Vec::new();
        if let Some(inspected) = probe_bound {
            notes.push(format!(
                "The lineage read inspected the newest {inspected} recordings in the store; \
                 an older child recording was not looked for."
            ));
        }
        let mut lineage = Lineage {
            parent,
            children,
            forked_from: header["forked_from_id"].as_str().map(str::to_owned),
            truncation: read_bounds(&recording.tail),
            notes,
        };
        // The agents are gathered by path rather than in record order.
        lineage.sort_children();
        Ok(lineage)
    }
}

fn read_transcript(path: &Path, read_bytes: u64) -> Result<CodexTranscriptRead> {
    let recording = read_recording(path, read_bytes)?;
    let read = &recording.tail;
    let mut turns = Vec::new();
    let mut last_turn = None;
    for (index, value) in read.values.iter().enumerate() {
        let mut parsed = parse_turns(value);
        super::attach_record_refs(
            &mut parsed,
            &format!("file:{}", path.display()),
            Some(&recording.tail.source_revision),
            read.spans.get(index).copied(),
        );
        if !parsed.is_empty() {
            last_turn = Some(index);
        }
        turns.extend(parsed);
    }
    let trailing_record = trailing_record(read.values.iter(), last_turn, codex_trailing_kind);
    let terminal = terminal_from_values(&read.values, codex_terminal);
    Ok((turns, recording, trailing_record, terminal))
}

/// Codex records the working directory in its `session_meta` header and
/// repeats it on every `turn_context`, so either line answers the question.
fn codex_cwd(value: &Value) -> Option<&str> {
    match value["type"].as_str() {
        Some("session_meta") | Some("turn_context") => value["payload"]["cwd"].as_str(),
        _ => None,
    }
}

/// The cumulative session totals a `token_count` event carries. Codex writes
/// one after every model response with `info.total_token_usage` as the running
/// total and `info.last_token_usage` as that response alone; the total is what
/// the normalized counters mean, so the newest event in the read window is the
/// session's accounting so far. An event without usage carries no answer and
/// is passed over for an older one. Counters Codex did not write stay absent;
/// a counter it wrote as zero is zero.
fn codex_tokens(value: &Value) -> Option<Tokens> {
    if !is_token_count(value) {
        return None;
    }
    let usage = value["payload"]["info"]["total_token_usage"].as_object()?;
    let counter = |name: &str| usage.get(name).and_then(Value::as_u64);
    let mut totals = TokenTotals::default();
    totals.add(
        counter("input_tokens"),
        counter("output_tokens"),
        counter("reasoning_output_tokens"),
        counter("cached_input_tokens"),
        counter("cache_write_input_tokens"),
    );
    totals.finish()
}

/// The context window the newest `token_count` event naming one reports.
/// Events carrying no `info` say nothing about it and are passed over.
fn codex_context_window(value: &Value) -> Option<u64> {
    is_token_count(value)
        .then(|| value["payload"]["info"]["model_context_window"].as_u64())
        .flatten()
}

/// The provider quota the newest `token_count` event carrying one observed.
/// It rides alongside `info` and describes the account rather than this
/// session, so it is reported as its own fact and never mixed into the
/// session's counters.
fn codex_rate_limits(value: &Value) -> Option<RateLimits> {
    if !is_token_count(value) {
        return None;
    }
    let limits = &value["payload"]["rate_limits"];
    let window = |name: &str| {
        let window = &limits[name];
        window["used_percent"]
            .is_number()
            .then(|| RateWindow {
                used_percent: window["used_percent"].clone(),
                window_minutes: window["window_minutes"].as_u64(),
                resets_at: window["resets_at"].as_i64().and_then(epoch_seconds),
            })
            .or_else(|| {
                window["used_percent"].as_str().map(|_| RateWindow {
                    used_percent: window["used_percent"].clone(),
                    window_minutes: window["window_minutes"].as_u64(),
                    resets_at: window["resets_at"].as_i64().and_then(epoch_seconds),
                })
            })
    };
    let credits = limits["credits"].as_object().map(|credits| Credits {
        balance: credits.get("balance").cloned(),
        has_credits: credits.get("has_credits").and_then(Value::as_bool),
        unlimited: credits.get("unlimited").and_then(Value::as_bool),
    });
    let mut limits = RateLimits {
        primary: window("primary"),
        secondary: window("secondary"),
        plan: limits["plan_type"].as_str().map(str::to_owned),
        credits: credits.filter(|credits| !credits.is_empty()),
        spend_control_reached: limits["spend_control_reached"].as_bool(),
        rate_limit_reached: limits["rate_limit_reached"].as_bool(),
        rate_limit_reached_type: limits["rate_limit_reached_type"]
            .as_str()
            .map(str::to_owned),
        observed_at: None,
    };
    if !limits.is_empty() {
        limits.observed_at = timestamp(&value["timestamp"]);
    }
    (!limits.is_empty()).then_some(limits)
}

fn epoch_seconds(seconds: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(seconds, 0)
}

fn is_token_count(value: &Value) -> bool {
    value["type"] == "event_msg" && value["payload"]["type"] == "token_count"
}

fn codex_terminal(value: &Value) -> Option<TerminalObservation> {
    if value["type"] != "event_msg" {
        return None;
    }
    let payload = &value["payload"];
    let payload_type = payload["type"].as_str()?;
    let explicitly_terminal = matches!(
        payload_type,
        "task_complete" | "turn_aborted" | "turn_complete" | "error"
    ) || payload["terminal"].as_bool() == Some(true)
        || payload.get("outcome").is_some()
        || payload.get("code").is_some();
    if !explicitly_terminal {
        return None;
    }
    // Codex puts the actionable failure on `error` for usage-limit task
    // completions. That nested record is the native error authority when it
    // supplies either field; the outer fields remain fallbacks for terminal
    // records written in the flatter form.
    let nested_error = payload.get("error");
    let code = nested_error
        .and_then(|error| error.get("codex_error_info"))
        .filter(|value| !value.is_null())
        .or_else(|| payload.get("code").filter(|value| !value.is_null()))
        .cloned();
    let message = nested_error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .map(bounded_terminal_text);
    Some(TerminalObservation {
        record_type: value["type"].as_str()?.to_owned(),
        payload_type: Some(payload_type.to_owned()),
        timestamp: timestamp(&value["timestamp"]),
        native_id: payload["id"]
            .as_str()
            .or_else(|| payload["event_id"].as_str())
            .map(str::to_owned),
        turn_id: payload["turn_id"]
            .as_str()
            .or_else(|| payload["context"]["turn_id"].as_str())
            .map(str::to_owned),
        outcome: payload["outcome"]
            .as_str()
            .or_else(|| payload["status"].as_str())
            .map(str::to_owned),
        code,
        message,
        duration_ms: payload["duration_ms"].as_i64(),
    })
}

fn bounded_terminal_text(text: &str) -> crate::model::BoundedText {
    const MAX_TERMINAL_MESSAGE_CHARS: usize = 2_048;
    let chars = text.chars().count();
    crate::model::BoundedText {
        text: text.chars().take(MAX_TERMINAL_MESSAGE_CHARS).collect(),
        chars,
        truncated: chars > MAX_TERMINAL_MESSAGE_CHARS,
    }
}

fn codex_trailing_kind(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "session_meta" => Some("session_meta"),
        "turn_context" => Some("turn_context"),
        "event_msg" => Some("event_msg"),
        "world_state" => Some("world_state"),
        _ => None,
    }
}

/// Codex writes no field naming who a user message came from; the elements it
/// wraps its own text in are what separate that text from somebody's request.
/// A message whose every block is context the harness attached is ambient,
/// one that adds only a message the harness raised on its own is a notice, and
/// any other text was addressed to the agent. Each message answers from its
/// own record, so every read of it gives the same kind.
fn codex_user_kind(payload: &Value) -> TurnKind {
    let blocks = payload["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "input_text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        TurnKind::Operator
    } else if blocks.iter().all(|block| is_known_envelope(block)) {
        TurnKind::Ambient
    } else if blocks
        .iter()
        .all(|block| is_known_envelope(block) || is_known_notice(block))
    {
        TurnKind::Notice
    } else {
        TurnKind::Operator
    }
}

fn parse_turns(value: &Value) -> Vec<Turn> {
    if value["type"] == "event_msg"
        && matches!(
            value["payload"]["type"].as_str(),
            Some("item_started" | "item_completed")
        )
    {
        let payload = &value["payload"];
        let item = &payload["item"];
        if !is_codex_runtime_tool(item) {
            return Vec::new();
        }
        let completed = payload["type"] == "item_completed";
        let event = codex_runtime_tool_event(item, completed);
        return vec![Turn {
            role: Role::Tool,
            kind: TurnKind::Tool,
            text: item.to_string(),
            ts: timestamp(&value["timestamp"]),
            ordinal: 0,
            native_id: item["id"]
                .as_str()
                .or_else(|| payload["id"].as_str())
                .map(str::to_owned),
            request_turn_id: payload["turn_id"].as_str().map(str::to_owned),
            metadata: None,
            record_ref: None,
            channel: None,
            recipient: None,
            parts: vec![tool_part(item, "payload.item", "codex-item")],
            coverage: Some(tool_coverage()),
            tool: Some(event),
        }];
    }
    if value["type"] != "response_item" {
        return Vec::new();
    }
    let payload = &value["payload"];
    let ts = timestamp(&value["timestamp"]);
    let native_id = payload["id"].as_str().map(str::to_owned);
    let (role, kind, text, tool, parts, coverage) = match payload["type"].as_str() {
        Some("message") => {
            let role = match payload["role"].as_str() {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                Some("developer") => Role::Developer,
                _ => return Vec::new(),
            };
            let (parts, coverage) = parts_from_array(&payload["content"], "payload.content");
            let text = project_text(&parts);
            let kind = role.kind().unwrap_or_else(|| codex_user_kind(payload));
            (role, kind, text, None, parts, coverage)
        }
        Some("reasoning") => {
            let text = reasoning_text(payload);
            let parts = if text == "[encrypted reasoning]" {
                vec![crate::content::ContentPart::Unknown {
                    native_kind: "encrypted".to_owned(),
                    descriptor: crate::content::bounded_shape(payload),
                    source_field: "payload.encrypted_content".to_owned(),
                    record_ref: None,
                }]
            } else {
                vec![crate::content::text_part(
                    text.as_str(),
                    "payload.summary",
                    "reasoning",
                )]
            };
            let coverage = if text == "[encrypted reasoning]" {
                crate::content::ContentCoverage {
                    carrier: crate::content::ContentCarrier::DirectPart,
                    availability: crate::content::ContentAvailability::Unknown,
                    retained_parts: 1,
                    omitted_parts: 0,
                    omitted_reason: Some("encrypted-reasoning-placeholder".to_owned()),
                }
            } else {
                crate::content::ContentCoverage {
                    carrier: crate::content::ContentCarrier::DirectPart,
                    availability: crate::content::ContentAvailability::RetainedBody,
                    retained_parts: 1,
                    omitted_parts: 0,
                    omitted_reason: None,
                }
            };
            (
                Role::Reasoning,
                TurnKind::Reasoning,
                text,
                None,
                parts,
                coverage,
            )
        }
        Some(
            subtype @ ("function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"),
        ) => (
            Role::Tool,
            TurnKind::Tool,
            payload.to_string(),
            Some(codex_tool_event(payload, subtype)),
            vec![tool_part(payload, "payload", subtype)],
            tool_coverage(),
        ),
        _ => return Vec::new(),
    };
    ((!text.is_empty()) || !parts.is_empty())
        .then_some(Turn {
            role,
            kind,
            text,
            ts,
            ordinal: 0,
            native_id,
            request_turn_id: payload["turn_id"]
                .as_str()
                .or_else(|| payload["context"]["turn_id"].as_str())
                .map(str::to_owned),
            metadata: None,
            record_ref: None,
            channel: payload["channel"].as_str().map(str::to_owned),
            recipient: payload["recipient"].as_str().map(str::to_owned),
            parts,
            coverage: Some(coverage),
            tool,
        })
        .into_iter()
        .collect()
}

fn is_codex_runtime_tool(item: &Value) -> bool {
    // These are the verified native item variants that describe a tool
    // operation. AgentMessage, Reasoning, UserMessage, and ContextCompaction
    // are lifecycle mirrors of ordinary conversation state.
    matches!(
        item["type"].as_str(),
        Some("CommandExecution" | "FileChange")
    )
}

fn codex_tool_event(payload: &Value, subtype: &str) -> ToolEvent {
    let call = matches!(subtype, "function_call" | "custom_tool_call");
    let arguments = match subtype {
        "function_call" => Bounded::from_value(&payload["arguments"]),
        "custom_tool_call" => Bounded::from_value(&payload["input"]),
        _ => None,
    };
    let argument_value = match subtype {
        "function_call" => payload_json(&payload["arguments"]),
        "custom_tool_call" => payload_json(&payload["input"]),
        _ => Value::Null,
    };
    ToolEvent {
        kind: if call {
            EventKind::ToolCall
        } else {
            EventKind::ToolResult
        },
        subtype: subtype.to_owned(),
        name: call
            .then(|| payload["name"].as_str())
            .flatten()
            .map(str::to_owned),
        call_id: payload["call_id"].as_str().map(str::to_owned),
        status: (subtype == "custom_tool_call")
            .then(|| payload["status"].as_str())
            .flatten()
            .map(str::to_owned),
        arguments,
        output: (!call)
            .then(|| Bounded::from_value(&payload["output"]))
            .flatten(),
        completed_ts: None,
        invocations: if call {
            crate::event::invocations_from_tool(
                payload["name"].as_str(),
                match subtype {
                    "function_call" => &payload["arguments"],
                    "custom_tool_call" => &payload["input"],
                    _ => &argument_value,
                },
                "payload.arguments",
            )
        } else {
            Vec::new()
        },
        artifact_references: if call {
            crate::event::artifact_references(&argument_value)
        } else {
            crate::event::artifact_references(&payload_json(&payload["output"]))
        },
        artifact_consumptions: Vec::new(),
    }
}

fn codex_runtime_tool_event(item: &Value, completed: bool) -> ToolEvent {
    let command = item.get("command").filter(|value| !value.is_null());
    ToolEvent {
        kind: if completed {
            EventKind::ToolResult
        } else {
            EventKind::ToolCall
        },
        subtype: item["type"].as_str().unwrap_or("codex-item").to_owned(),
        name: item["type"].as_str().map(str::to_owned),
        call_id: item["id"].as_str().map(str::to_owned),
        status: item["status"].as_str().map(str::to_owned),
        arguments: (!completed)
            .then(|| command.and_then(Bounded::from_value))
            .flatten(),
        output: completed
            .then(|| {
                ["stdout", "stderr", "formatted_output", "aggregated_output"]
                    .into_iter()
                    .find_map(|key| Bounded::from_value(&item[key]))
            })
            .flatten(),
        completed_ts: completed
            .then(|| timestamp(&item["completed_at"]))
            .flatten(),
        invocations: crate::event::structured_runtime_invocations(item, "payload.item.command"),
        artifact_references: if completed {
            crate::event::artifact_references(item)
        } else {
            Vec::new()
        },
        artifact_consumptions: Vec::new(),
    }
}

fn reasoning_text(payload: &Value) -> String {
    let text = ["summary", "content"]
        .into_iter()
        .flat_map(|field| payload[field].as_array().into_iter().flatten())
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        text
    } else if payload["encrypted_content"].as_str().is_some() {
        "[encrypted reasoning]".to_owned()
    } else {
        String::new()
    }
}

/// Recordings whose headers the lineage read will open while looking for
/// children. Well above any store seen in practice, so it bounds a
/// pathological one without hiding a real child.
const MAX_LINEAGE_PROBES: usize = 5_000;
/// The function call Codex records when a session spawns another agent.
const SPAWN_AGENT: &str = "spawn_agent";
/// The status an agent report carries once that agent has finished.
const AGENT_COMPLETED: &str = "completed";

/// One agent a parent drove, as its own records name it. The key is the agent
/// path, which is what a child's header and the parent's outputs share.
#[derive(Default)]
struct SpawnedAgent {
    task_name: Option<String>,
    nickname: Option<String>,
    model: Option<String>,
    spawned_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    disposition: Option<String>,
    session_id: Option<String>,
    source: Vec<SourceRef>,
}

/// A child recording found by its header.
struct ChildHeader {
    thread_id: String,
    agent_path: String,
    nickname: Option<String>,
    path: PathBuf,
}

/// The agents a rollout's own records say it drove. A `spawn_agent` call
/// carries the request and its output names the path the agent runs under;
/// every call that reports on agents answers with their statuses under those
/// same paths, so an agent whose spawn is behind the read bound is still
/// named by the report that mentions it.
fn spawned_agents(values: &[Value]) -> HashMap<String, SpawnedAgent> {
    let mut agents = HashMap::<String, SpawnedAgent>::new();
    let mut spawns = HashMap::<String, SpawnedAgent>::new();
    for value in values {
        if value["type"] != "response_item" {
            continue;
        }
        let payload = &value["payload"];
        let ts = timestamp(&value["timestamp"]);
        let call_id = payload["call_id"].as_str().unwrap_or_default().to_owned();
        match payload["type"].as_str() {
            Some("function_call") if payload["name"] == SPAWN_AGENT => {
                let arguments = payload_json(&payload["arguments"]);
                spawns.insert(
                    call_id.clone(),
                    SpawnedAgent {
                        task_name: arguments["task_name"].as_str().map(str::to_owned),
                        model: arguments["model"].as_str().map(str::to_owned),
                        spawned_at: ts,
                        source: vec![SourceRef::Record { native_id: call_id }],
                        ..SpawnedAgent::default()
                    },
                );
            }
            Some("function_call_output") => {
                let output = payload_json(&payload["output"]);
                if let Some(spawn) = spawns.remove(&call_id) {
                    let Some(path) = output["task_name"].as_str() else {
                        continue;
                    };
                    let agent = agents.entry(path.to_owned()).or_default();
                    agent.task_name = spawn.task_name;
                    agent.model = spawn.model;
                    agent.spawned_at = spawn.spawned_at;
                    agent.source.extend(spawn.source);
                    agent.source.push(SourceRef::Record {
                        native_id: call_id.clone(),
                    });
                }
                for reported in output["agents"].as_array().into_iter().flatten() {
                    let Some(name) = reported["agent_name"].as_str() else {
                        continue;
                    };
                    let Some(status) = reported["agent_status"].as_str() else {
                        continue;
                    };
                    let agent = agents.entry(name.to_owned()).or_default();
                    agent.disposition = Some(status.to_owned());
                    if status == AGENT_COMPLETED && agent.completed_at.is_none() {
                        agent.completed_at = ts;
                    }
                    agent.source.push(SourceRef::Record {
                        native_id: call_id.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    agents
}

/// A Codex payload field that carries JSON as text, or as the value itself.
fn payload_json(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// The store's recordings whose headers name this session as their parent,
/// and the probe bound when the store held more than the read inspected.
fn child_headers(files: &[PathBuf], own: &Path, id: &str) -> (Vec<ChildHeader>, Option<usize>) {
    let inspected = files.len().min(MAX_LINEAGE_PROBES);
    let bound = (files.len() > inspected).then_some(inspected);
    let children = files
        .iter()
        .take(inspected)
        .filter(|path| path.as_path() != own)
        .filter_map(|path| {
            let payload = head_jsonl(path)
                .into_iter()
                .find(|value| value["type"] == "session_meta")
                .map(|value| value["payload"].clone())?;
            (payload["parent_thread_id"].as_str() == Some(id)).then_some(())?;
            Some(ChildHeader {
                thread_id: payload["id"].as_str()?.to_owned(),
                agent_path: payload["agent_path"].as_str()?.to_owned(),
                nickname: payload["agent_nickname"].as_str().map(str::to_owned),
                path: path.clone(),
            })
        })
        .collect();
    (children, bound)
}

/// The namespace an agent path sits in, where the harness organizes children
/// under one.
fn agent_group(path: &str) -> Option<String> {
    let (group, name) = path.rsplit_once('/')?;
    (!name.is_empty()).then(|| {
        if group.is_empty() {
            "/".to_owned()
        } else {
            group.to_owned()
        }
    })
}
