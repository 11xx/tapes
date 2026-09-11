use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, head_directory, head_jsonl, home_path, jsonl_files, list_files,
    list_files_with_search, matching_session_file, read_bounds, read_recording, session_file,
    timestamp, trailing_record, transcript, Backend, Jsonl, Listing, ParsedFile, Query,
    TokenTotals,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::lineage::{ChildRef, Lineage, ParentRef, SourceRef};
use crate::model::{
    is_known_envelope, without_known_envelopes, AccountingBasis, AccountingCoverage, Model, Role,
    Session, Tokens, TrailingRecord, Transcript, Turn, TurnKind,
};
use crate::usage::{RateLimits, RateWindow, UsageDetail};

#[derive(Clone, Debug)]
pub struct CodexBackend {
    root: Option<PathBuf>,
}

impl CodexBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn parse(&self, path: &Path) -> Result<(Session, Vec<Turn>, Jsonl)> {
        let recording = read_recording(path)?;
        let (started_at, last_activity_at) = recording
            .time_range()
            .ok_or_else(|| anyhow!("{} has no valid timestamps", path.display()))?;
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
        let evidence = user_message_evidence(&read.values, opening);
        let turns = read
            .values
            .iter()
            .flat_map(|value| parse_turns(value, &evidence))
            .collect::<Vec<_>>();
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
            harness: "codex".into(),
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
            store: Some(path.display().to_string()),
            start_uncertain: recording.start_uncertain(),
            usage_detail,
        };
        // The opening is the start of the file, so its first user turn is the
        // session's first user turn even when the tail cannot see it.
        let session = if read.truncated {
            let opening_evidence = user_message_evidence(opening, opening);
            let opening_turns = opening
                .iter()
                .flat_map(|value| parse_turns(value, &opening_evidence))
                .collect::<Vec<_>>();
            session.with_derived_title(&opening_turns)
        } else {
            session.with_derived_title(&turns)
        };

        Ok((session, turns, recording.tail))
    }
}

impl Default for CodexBackend {
    fn default() -> Self {
        let root = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| home_path(&[".codex"]))
            .map(|path| path.join("sessions"));
        Self { root }
    }
}

impl Backend for CodexBackend {
    fn history_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> Result<crate::history::Page> {
        let path = session
            .store
            .as_deref()
            .ok_or_else(|| anyhow!("session has no source file"))?;
        crate::history::read_file(
            session,
            Path::new(path),
            cursor,
            bytes,
            true,
            |values, opening, context| {
                let mut evidence = user_message_evidence(values, opening);
                evidence
                    .text
                    .extend(context.iter().filter_map(codex_user_message));
                let turns = values
                    .iter()
                    .flat_map(|value| parse_turns(value, &evidence))
                    .collect();
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

    fn harness(&self) -> &'static str {
        "codex"
    }

    fn available(&self) -> bool {
        self.root.as_deref().is_some_and(Path::is_dir)
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
        let (turns, read, trailing_record) = read_transcript(&path)?;
        Ok(transcript(
            session.clone(),
            turns,
            tail,
            &read,
            trailing_record,
            Vec::new(),
        ))
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
        let recording = read_recording(&path)?;
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

fn read_transcript(path: &Path) -> Result<(Vec<Turn>, Jsonl, Option<TrailingRecord>)> {
    let recording = read_recording(path)?;
    let read = &recording.tail;
    let evidence = user_message_evidence(&read.values, recording.opening());
    let mut turns = Vec::new();
    let mut last_turn = None;
    for (index, value) in read.values.iter().enumerate() {
        let parsed = parse_turns(value, &evidence);
        if !parsed.is_empty() {
            last_turn = Some(index);
        }
        turns.extend(parsed);
    }
    let trailing_record = trailing_record(read.values.iter(), last_turn, codex_trailing_kind);
    Ok((turns, recording.tail, trailing_record))
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
            .as_f64()
            .map(|used_percent| RateWindow {
                used_percent,
                window_minutes: window["window_minutes"].as_u64(),
                resets_at: window["resets_at"].as_i64().and_then(epoch_seconds),
            })
    };
    let limits = RateLimits {
        primary: window("primary"),
        secondary: window("secondary"),
        plan: limits["plan_type"].as_str().map(str::to_owned),
    };
    (!limits.is_empty()).then_some(limits)
}

fn epoch_seconds(seconds: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(seconds, 0)
}

fn is_token_count(value: &Value) -> bool {
    value["type"] == "event_msg" && value["payload"]["type"] == "token_count"
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

/// What a rollout's own records say about the messages in its user role: the
/// entry point its header names, and the text of every message the operator
/// sent, which the harness records as a `user_message` event of its own.
struct UserMessages<'a> {
    /// Whether the session was started by `codex exec`, whose caller supplies
    /// one prompt and whose records carry no `user_message` event.
    exec: bool,
    text: Vec<&'a str>,
}

fn user_message_evidence<'a>(values: &'a [Value], opening: &'a [Value]) -> UserMessages<'a> {
    UserMessages {
        exec: opening
            .iter()
            .chain(values)
            .find_map(codex_source)
            .is_some_and(|source| source == "exec"),
        text: values.iter().filter_map(codex_user_message).collect(),
    }
}

fn codex_source(value: &Value) -> Option<&str> {
    (value["type"] == "session_meta")
        .then(|| value["payload"]["source"].as_str())
        .flatten()
}

fn codex_user_message(value: &Value) -> Option<&str> {
    (value["type"] == "event_msg" && value["payload"]["type"] == "user_message")
        .then(|| value["payload"]["message"].as_str())
        .flatten()
        .filter(|message| !message.is_empty())
}

/// Codex records the operator's own messages twice: once as the conversation
/// item the model reads, and once as a `user_message` event carrying the text
/// as it was sent. A conversation item the event vouches for is the operator's;
/// one holding only blocks the harness wraps around a message is context it
/// attached. An `exec` session records no such event, and there the wrapper
/// blocks are all that separates the harness's own text from the caller's.
fn codex_user_kind(payload: &Value, text: &str, messages: &UserMessages) -> TurnKind {
    let mut blocks = payload["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "input_text")
        .filter_map(|block| block["text"].as_str())
        .peekable();
    let attached = blocks.peek().is_some() && blocks.all(is_known_envelope);
    if messages.exec {
        return if attached {
            TurnKind::Ambient
        } else {
            TurnKind::Operator
        };
    }
    // The harness wraps a message either in records of its own or in the same
    // block, so the sent text is matched against the message and against the
    // message with the wrapper removed.
    let requested = without_known_envelopes(text);
    let sent = messages
        .text
        .iter()
        .any(|sent| text.starts_with(sent) || requested.trim_start().starts_with(sent));
    if sent {
        TurnKind::Operator
    } else if attached {
        TurnKind::Ambient
    } else {
        TurnKind::Unknown
    }
}

fn parse_turns(value: &Value, messages: &UserMessages) -> Vec<Turn> {
    if value["type"] != "response_item" {
        return Vec::new();
    }
    let payload = &value["payload"];
    let ts = timestamp(&value["timestamp"]);
    let native_id = payload["id"].as_str().map(str::to_owned);
    let (role, kind, text, tool) = match payload["type"].as_str() {
        Some("message") => {
            let role = match payload["role"].as_str() {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => return Vec::new(),
            };
            let text = payload["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| {
                    matches!(block["type"].as_str(), Some("input_text" | "output_text"))
                })
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let kind = role
                .kind()
                .unwrap_or_else(|| codex_user_kind(payload, &text, messages));
            (role, kind, text, None)
        }
        Some("reasoning") => (
            Role::Reasoning,
            TurnKind::Reasoning,
            reasoning_text(payload),
            None,
        ),
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
        ),
        _ => return Vec::new(),
    };
    (!text.is_empty())
        .then_some(Turn {
            role,
            kind,
            text,
            ts,
            ordinal: 0,
            native_id,
            tool,
        })
        .into_iter()
        .collect()
}

fn codex_tool_event(payload: &Value, subtype: &str) -> ToolEvent {
    let call = matches!(subtype, "function_call" | "custom_tool_call");
    let arguments = match subtype {
        "function_call" => Bounded::from_value(&payload["arguments"]),
        "custom_tool_call" => Bounded::from_value(&payload["input"]),
        _ => None,
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
