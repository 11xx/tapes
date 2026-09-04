use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, head_directory, home_path, jsonl_files, list_files, list_files_with_search,
    read_jsonl, read_recording, session_file, timestamp, trailing_record, transcript, Backend,
    Jsonl, Listing, ParsedFile, Query, TokenTotals,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::model::{
    AccountingBasis, AccountingCoverage, Model, Role, Session, Tokens, TrailingRecord, Transcript,
    Turn,
};

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
        let turns = read.values.iter().flat_map(parse_turns).collect::<Vec<_>>();
        let tokens = read.values.iter().rev().find_map(codex_tokens);
        let accounting = accounting_for(
            tokens.as_ref(),
            None,
            AccountingBasis::RecordedTotal,
            AccountingCoverage::Session,
        );

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
}

fn read_transcript(path: &Path) -> Result<(Vec<Turn>, Jsonl, Option<TrailingRecord>)> {
    let read = read_jsonl(path)?;
    let mut turns = Vec::new();
    let mut last_turn = None;
    for (index, value) in read.values.iter().enumerate() {
        let parsed = parse_turns(value);
        if !parsed.is_empty() {
            last_turn = Some(index);
        }
        turns.extend(parsed);
    }
    let trailing_record = trailing_record(read.values.iter(), last_turn, codex_trailing_kind);
    Ok((turns, read, trailing_record))
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
    if value["type"] != "event_msg" || value["payload"]["type"] != "token_count" {
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

fn codex_trailing_kind(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "session_meta" => Some("session_meta"),
        "turn_context" => Some("turn_context"),
        "event_msg" => Some("event_msg"),
        "world_state" => Some("world_state"),
        _ => None,
    }
}

fn parse_turns(value: &Value) -> Vec<Turn> {
    if value["type"] != "response_item" {
        return Vec::new();
    }
    let payload = &value["payload"];
    let ts = timestamp(&value["timestamp"]);
    let native_id = payload["id"].as_str().map(str::to_owned);
    let (role, text, tool) = match payload["type"].as_str() {
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
            (role, text, None)
        }
        Some("reasoning") => (Role::Reasoning, reasoning_text(payload), None),
        Some(
            subtype @ ("function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"),
        ) => (
            Role::Tool,
            payload.to_string(),
            Some(codex_tool_event(payload, subtype)),
        ),
        _ => return Vec::new(),
    };
    (!text.is_empty())
        .then_some(Turn {
            role,
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
