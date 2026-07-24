use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    home_path, jsonl_files, read_jsonl, session_file, time_range, timestamp, transcript, Backend,
    Jsonl,
};
use crate::model::{Model, Role, Session, Transcript, Turn};

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
        let directory = metadata
            .and_then(|value| value["cwd"].as_str())
            .or_else(|| {
                read.values
                    .iter()
                    .rev()
                    .find(|value| value["type"] == "turn_context")
                    .and_then(|value| value["payload"]["cwd"].as_str())
            })
            .map(PathBuf::from);
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
        let turns = read.values.iter().filter_map(parse_turn).collect();

        Ok((
            Session {
                id,
                harness: "codex".into(),
                model,
                title: None,
                directory,
                started_at,
                last_activity_at,
                cost: None,
                tokens: None,
            },
            turns,
            read,
        ))
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

    fn list(&self, limit: usize) -> Result<Vec<Session>> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Vec::new());
        };
        let mut sessions = jsonl_files(root)
            .into_iter()
            .take(limit)
            .filter_map(|path| self.parse(&path).ok().map(|(session, _, _)| session))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.last_activity_at);
        sessions.reverse();
        Ok(sessions)
    }

    fn transcript(&self, id: &str, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("codex store is unavailable"))?;
        let path =
            session_file(root, id).ok_or_else(|| anyhow!("codex session {id} is unavailable"))?;
        let (session, turns, read) = self.parse(&path)?;
        Ok(transcript(session, turns, tail, &read, Vec::new()))
    }
}

fn parse_turn(value: &Value) -> Option<Turn> {
    if value["type"] != "response_item" || value["payload"]["type"] != "message" {
        return None;
    }
    let payload = &value["payload"];
    let role = match payload["role"].as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let text = payload["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| matches!(block["type"].as_str(), Some("input_text" | "output_text")))
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| Turn {
        role,
        text,
        ts: timestamp(&value["timestamp"]),
    })
}
