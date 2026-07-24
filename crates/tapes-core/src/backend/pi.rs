use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    home_path, jsonl_files, read_jsonl, session_file, time_range, timestamp, transcript, Backend,
    Jsonl,
};
use crate::model::{Model, Role, Session, Transcript, Turn};

#[derive(Clone, Debug)]
pub struct PiBackend {
    root: Option<PathBuf>,
}

impl PiBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    fn parse(&self, path: &Path) -> Result<(Session, Vec<Turn>, Jsonl, usize)> {
        let read = read_jsonl(path)?;
        let (started_at, last_activity_at) = time_range(&read.values)
            .ok_or_else(|| anyhow!("{} has no valid timestamps", path.display()))?;
        let header = read.values.iter().find(|value| value["type"] == "session");
        let entries = read
            .values
            .iter()
            .filter(|value| value["type"] != "session")
            .collect::<Vec<_>>();
        let active = active_path(&entries);
        let active_ids = active
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect::<HashSet<_>>();
        let abandoned = entries
            .iter()
            .filter(|entry| {
                entry["id"]
                    .as_str()
                    .is_some_and(|id| !active_ids.contains(id))
            })
            .count();
        let id = header
            .and_then(|value| value["id"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(|stem| stem.rsplit('_').next())
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow!("{} has no session id", path.display()))?;
        let directory = header
            .and_then(|value| value["cwd"].as_str())
            .map(PathBuf::from);
        let variant = active.iter().rev().find_map(|value| {
            (value["type"] == "thinking_level_change")
                .then(|| value["thinkingLevel"].as_str())
                .flatten()
        });
        let model = active.iter().rev().find_map(|value| {
            let message = value.get("message")?;
            (message["role"] == "assistant")
                .then(|| message["model"].as_str())
                .flatten()
                .map(|id| Model {
                    id: id.to_owned(),
                    variant: variant.map(str::to_owned),
                })
        });
        let turns = active
            .iter()
            .filter_map(|value| parse_turn(value))
            .collect();

        Ok((
            Session {
                id,
                harness: "pi".into(),
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
            abandoned,
        ))
    }
}

impl Default for PiBackend {
    fn default() -> Self {
        let root = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("PI_CODING_AGENT_DIR")
                    .map(PathBuf::from)
                    .map(|path| path.join("sessions"))
            })
            .or_else(|| home_path(&[".pi", "agent", "sessions"]));
        Self { root }
    }
}

impl Backend for PiBackend {
    fn harness(&self) -> &'static str {
        "pi"
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
            .filter_map(|path| self.parse(&path).ok().map(|(session, _, _, _)| session))
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.last_activity_at);
        sessions.reverse();
        Ok(sessions)
    }

    fn transcript(&self, id: &str, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("pi store is unavailable"))?;
        let path =
            session_file(root, id).ok_or_else(|| anyhow!("pi session {id} is unavailable"))?;
        let (session, turns, read, abandoned) = self.parse(&path)?;
        let notes = (abandoned > 0)
            .then(|| {
                if abandoned == 1 {
                    "1 entry belongs to an abandoned branch.".to_owned()
                } else {
                    format!("{abandoned} entries belong to abandoned branches.")
                }
            })
            .into_iter()
            .collect();
        Ok(transcript(session, turns, tail, &read, notes))
    }
}

fn active_path<'a>(entries: &[&'a Value]) -> Vec<&'a Value> {
    let by_id = entries
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(|id| (id, *entry)))
        .collect::<HashMap<_, _>>();
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut current = entries.last().copied();
    while let Some(entry) = current {
        let Some(id) = entry["id"].as_str() else {
            break;
        };
        if !seen.insert(id) {
            break;
        }
        path.push(entry);
        current = entry["parentId"]
            .as_str()
            .and_then(|parent| by_id.get(parent).copied());
    }
    path.reverse();
    path
}

fn parse_turn(value: &Value) -> Option<Turn> {
    if value["type"] != "message" {
        return None;
    }
    let message = &value["message"];
    let role = match message["role"].as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "toolResult" => Role::Tool,
        _ => return None,
    };
    let text = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| Turn {
        role,
        text,
        ts: timestamp(&value["timestamp"]),
    })
}
