use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    head_directory, home_path, list_files, matching_session_file, read_jsonl, time_range,
    timestamp, transcript, Backend, Jsonl, Listing, Query,
};
use crate::model::{Model, Role, Session, Transcript, Turn};
use crate::scope::Scope;

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
            .map(PathBuf::from)
            // A transcript past the bounded read keeps only its tail, which
            // for a session that ended in tool output carries no `cwd`.
            .or_else(|| head_directory(path, |value| value["cwd"].as_str()));
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
        let turns = read.values.iter().flat_map(parse_turns).collect();

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

    fn list(&self, query: &Query) -> Result<Listing> {
        let Some(root) = self.root.as_deref() else {
            return Ok(Listing::default());
        };
        let files = match query.scope {
            Some(scope) => scoped_session_files(root, scope),
            None => session_files(root),
        };
        let mut listing = list_files(
            files,
            query,
            |path| head_directory(path, |value| value["cwd"].as_str()),
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
        let Some(path) = matching_session_file(session_files(root), id) else {
            return Ok(None);
        };
        Ok(self.parse(&path).ok().map(|(session, _, _)| session))
    }

    fn transcript(&self, id: &str, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("claude store is unavailable"))?;
        let path = matching_session_file(session_files(root), id)
            .ok_or_else(|| anyhow!("claude session {id} is unavailable"))?;
        let (session, turns, read) = self.parse(&path)?;
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
        Ok(transcript(session, turns, tail, &read, notes))
    }
}

/// Narrow the store to the project directories a scope could have written,
/// before any file is opened. Claude names each project directory after the
/// cwd it recorded, so a scope's own paths encode forward into the names to
/// look for.
///
/// The encoding is not injective — `/` and a literal `-` both become `-` — so
/// this can only ever over-select, and the parsed `cwd` stays authoritative.
/// A narrowing that finds nothing falls back to the whole store: the naming
/// convention is undocumented and drifts, and a pre-filter that could produce
/// false *negatives* would turn an optimization into a wrong answer.
fn scoped_session_files(root: &Path, scope: &Scope) -> Vec<PathBuf> {
    let prefixes = scope
        .roots()
        .iter()
        .map(|path| project_slug(path))
        .collect::<Vec<_>>();
    let narrowed = project_directories(root)
        .into_iter()
        .filter(|project| {
            project
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    let name = name.to_ascii_lowercase();
                    prefixes.iter().any(|prefix| name.starts_with(prefix))
                })
        })
        .collect::<Vec<_>>();
    if narrowed.is_empty() {
        return session_files(root);
    }
    sorted_transcripts(narrowed)
}

/// A path as claude names the project directory holding its sessions:
/// `/home/user/.config` becomes `-home-user--config`. Every character that is
/// not alphanumeric encodes to `-`, which covers more than the separator alone
/// and keeps the result an over-approximation.
fn project_slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn session_files(root: &Path) -> Vec<PathBuf> {
    sorted_transcripts(project_directories(root))
}

fn project_directories(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
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
        .collect()
}

fn sorted_transcripts(projects: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut files = projects
        .into_iter()
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
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        return turn(role, text.to_owned(), ts).into_iter().collect();
    }

    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let (role, text) = match block["type"].as_str()? {
                "text" => (role.clone(), block["text"].as_str()?.to_owned()),
                "thinking" => (Role::Reasoning, block["thinking"].as_str()?.to_owned()),
                "tool_use" | "tool_result" => (Role::Tool, block.to_string()),
                _ => return None,
            };
            turn(role, text, ts)
        })
        .collect()
}

fn turn(role: Role, text: String, ts: Option<chrono::DateTime<chrono::Utc>>) -> Option<Turn> {
    (!text.is_empty()).then_some(Turn { role, text, ts })
}
