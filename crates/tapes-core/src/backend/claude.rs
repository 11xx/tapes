use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    head_directory, home_path, list_files, list_files_with_search, matching_session_file,
    read_jsonl, read_recording, timestamp, trailing_record, transcript, Backend, Jsonl, Listing,
    ParsedFile, Query,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::model::{Model, Role, Session, TrailingRecord, Transcript, Turn};

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
        let recording = read_recording(path)?;
        let (started_at, last_activity_at) = recording
            .time_range()
            .ok_or_else(|| anyhow!("{} has no valid timestamps", path.display()))?;
        // Claude repeats the session id and working directory on every message
        // line. The opening is consulted first; it also covers a transcript
        // past the bounded read whose remaining tail is all tool output and
        // carries no `cwd`.
        let opening = recording.opening();
        let read = &recording.tail;
        let id = opening
            .iter()
            .chain(&read.values)
            .find_map(|value| value["sessionId"].as_str())
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
            })
            .ok_or_else(|| anyhow!("{} has no session id", path.display()))?;
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
        let turns = read.values.iter().flat_map(parse_turns).collect::<Vec<_>>();

        let session = Session {
            id,
            harness: "claude".into(),
            model,
            title,
            derived_title: None,
            derived_title_truncated: None,
            directory,
            started_at,
            last_activity_at,
            live: None,
            cost: None,
            tokens: None,
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

    fn list(&self, query: &Query) -> Result<Listing> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_files(
            session_files(root),
            query,
            |path| head_directory(path, claude_cwd),
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
            session_files(root),
            query,
            needle,
            tail,
            |path| head_directory(path, claude_cwd),
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
        let Some(path) = matching_session_file(session_files(root), id) else {
            return Ok(None);
        };
        Ok(self.parse(&path).ok().map(|(session, _, _)| session))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("claude store is unavailable"))?;
        let path = matching_session_file(session_files(root), &session.id)
            .ok_or_else(|| anyhow!("claude session {} is unavailable", session.id))?;
        let (turns, read, trailing_record) = read_transcript(&path)?;
        let subagents = subagent_transcript_count(&path);
        let notes = (subagents > 0)
            .then(|| {
                if subagents == 1 {
                    "1 subagent transcript belongs to this session.".to_owned()
                } else {
                    format!("{subagents} subagent transcripts belong to this session.")
                }
            })
            .into_iter()
            .collect();
        Ok(transcript(
            session.clone(),
            turns,
            tail,
            &read,
            trailing_record,
            notes,
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
    let trailing_record = trailing_record(read.values.iter(), last_turn, claude_trailing_kind);
    Ok((turns, read, trailing_record))
}

/// Claude repeats the working directory on every message line.
fn claude_cwd(value: &Value) -> Option<&str> {
    value["cwd"].as_str()
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
fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut files = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|project| {
            project
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| project.path())
        })
        .flat_map(|project| fs::read_dir(project).into_iter().flatten().flatten())
        .filter_map(|entry| {
            let path = entry.path();
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_file())
                .and_then(|_| {
                    path.extension()
                        .is_some_and(|extension| extension == "jsonl")
                        .then_some(path)
                })
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    files.reverse();
    files
}

fn subagent_transcript_count(path: &Path) -> usize {
    let Some(session) = path.file_stem() else {
        return 0;
    };
    let subagents = path.with_file_name(session).join("subagents");
    fs::read_dir(subagents)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with("agent-"))
                && path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
        })
        .count()
}

fn parse_turns(value: &Value) -> Vec<Turn> {
    let Some(message) = value.get("message") else {
        return Vec::new();
    };
    let Some(message_role) = message["role"].as_str() else {
        return Vec::new();
    };
    let role = match message_role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return Vec::new(),
    };
    let ts = timestamp(&value["timestamp"]);
    let native_id = value["uuid"].as_str().map(str::to_owned);
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        return turn(role, text.to_owned(), ts, native_id, None)
            .into_iter()
            .collect();
    }

    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let (role, text, tool) = match block["type"].as_str()? {
                "text" => (role.clone(), block["text"].as_str()?.to_owned(), None),
                "thinking" => (
                    Role::Reasoning,
                    block["thinking"].as_str()?.to_owned(),
                    None,
                ),
                subtype @ ("tool_use" | "tool_result") => (
                    Role::Tool,
                    block.to_string(),
                    Some(claude_tool_event(block, subtype)),
                ),
                _ => return None,
            };
            turn(role, text, ts, native_id.clone(), tool)
        })
        .collect()
}

fn turn(
    role: Role,
    text: String,
    ts: Option<chrono::DateTime<chrono::Utc>>,
    native_id: Option<String>,
    tool: Option<ToolEvent>,
) -> Option<Turn> {
    (!text.is_empty()).then_some(Turn {
        role,
        text,
        ts,
        ordinal: 0,
        native_id,
        tool,
    })
}

fn claude_tool_event(block: &Value, subtype: &str) -> ToolEvent {
    let call = subtype == "tool_use";
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
        arguments: call.then(|| Bounded::from_value(&block["input"])).flatten(),
        output: (!call)
            .then(|| Bounded::from_value(&block["content"]))
            .flatten(),
        completed_ts: None,
    }
}
