use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    head_directory, home_path, jsonl_files, list_files, read_jsonl, session_file, time_range,
    timestamp, trailing_record, transcript, Backend, Jsonl, Listing, Query,
};
use crate::model::{Model, Role, Session, TrailingRecord, Transcript, Turn};

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
        let read = read_jsonl(path)?;
        let (started_at, last_activity_at) = time_range(&read.values)
            .ok_or_else(|| anyhow!("{} has no valid timestamps", path.display()))?;
        let metadata = read
            .values
            .iter()
            .find(|value| value["type"] == "session_meta")
            .and_then(|value| value.get("payload"));
        let id = metadata
            .and_then(|value| value["id"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.get(stem.len().saturating_sub(36)..))
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow!("{} has no session id", path.display()))?;
        // The file's opening is consulted first, and by the same rule the
        // scoped listing's cheap probe uses. Reading it here rather than only
        // as a fallback is what makes the probe's answer and this one the
        // same answer: a session the probe places outside a scope is one this
        // parse would place there too, so skipping it can never lose it. On a
        // transcript within the read window the opening is `session_meta`,
        // which is what the tail lookup would have found anyway.
        let directory = head_directory(path, codex_cwd)
            .or_else(|| {
                metadata
                    .and_then(|value| value["cwd"].as_str())
                    .map(PathBuf::from)
            })
            .or_else(|| {
                read.values
                    .iter()
                    .rev()
                    .find(|value| value["type"] == "turn_context")
                    .and_then(|value| value["payload"]["cwd"].as_str())
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

        let session = Session {
            id,
            harness: "codex".into(),
            model,
            title: None,
            derived_title: None,
            directory,
            started_at,
            last_activity_at,
            live: None,
            cost: None,
            tokens: None,
        };
        let session = if read.truncated {
            session
        } else {
            session.with_derived_title(&turns)
        };

        Ok((session, turns, read))
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
    let turns = read.values.iter().flat_map(parse_turns).collect();
    let trailing_record = trailing_record(
        read.values.iter(),
        |value| !parse_turns(value).is_empty(),
        codex_trailing_kind,
    );
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
    let (role, text) = match payload["type"].as_str() {
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
            (role, text)
        }
        Some("reasoning") => (Role::Reasoning, reasoning_text(payload)),
        Some(
            "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output",
        ) => (Role::Tool, payload.to_string()),
        _ => return Vec::new(),
    };
    (!text.is_empty())
        .then_some(Turn { role, text, ts })
        .into_iter()
        .collect()
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
