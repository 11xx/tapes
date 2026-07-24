use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    home_path, jsonl_files, read_jsonl, session_file, time_range, timestamp, transcript, Backend,
    Jsonl,
};
use crate::model::{Model, Role, Session, Transcript, Turn};

#[derive(Clone, Debug)]
pub struct ClaudeBackend {
    root: Option<PathBuf>,
}

impl ClaudeBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn parse(&self, path: &Path) -> Result<(Session, Vec<Turn>, Jsonl)> {
        let read = read_jsonl(path)?;
        let (started_at, last_activity_at) = time_range(&read.values)
            .ok_or_else(|| anyhow!("{} has no valid timestamps", path.display()))?;
        let id = read
            .values
            .iter()
            .find_map(|value| value["sessionId"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow!("{} has no session id", path.display()))?;
        let directory = read
            .values
            .iter()
            .find_map(|value| value["cwd"].as_str())
            .map(PathBuf::from);
        let title = read
            .values
            .iter()
            .rev()
            .find_map(|value| value["aiTitle"].as_str())
            .map(str::to_owned);
        let model = read.values.iter().rev().find_map(|value| {
            let message = value.get("message")?;
            (message["role"].as_str()? == "assistant")
                .then(|| message["model"].as_str())
                .flatten()
                .map(|id| Model {
                    id: id.to_owned(),
                    variant: None,
                })
        });
        let turns = read.values.iter().filter_map(parse_turn).collect();

        Ok((
            Session {
                id,
                harness: "claude".into(),
                model,
                title,
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

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self {
            root: home_path(&[".claude", "projects"]),
        }
    }
}

impl Backend for ClaudeBackend {
    fn harness(&self) -> &'static str {
        "claude"
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
            .ok_or_else(|| anyhow!("claude store is unavailable"))?;
        let path =
            session_file(root, id).ok_or_else(|| anyhow!("claude session {id} is unavailable"))?;
        let (session, turns, read) = self.parse(&path)?;
        Ok(transcript(session, turns, tail, &read, Vec::new()))
    }
}

fn parse_turn(value: &Value) -> Option<Turn> {
    let message = value.get("message")?;
    let role = match message["role"].as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let text = content_text(&message["content"]);
    (!text.is_empty()).then(|| Turn {
        role,
        text,
        ts: timestamp(&value["timestamp"]),
    })
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
