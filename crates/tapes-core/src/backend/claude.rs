use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, head_directory, home_path, list_files, list_files_with_search,
    matching_session_file, read_bounds, read_jsonl, read_recording, timestamp, trailing_record,
    transcript, Backend, Jsonl, Listing, ParsedFile, Query, TokenTotals,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::lineage::{ChildRef, Lineage, SourceRef};
use crate::model::{
    AccountingBasis, AccountingCoverage, Cost, Model, Role, Session, Tokens, TrailingRecord,
    Transcript, Turn, TurnKind,
};
use crate::usage::{Durations, ModelUsage, UsageDetail};

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
        let (tokens, cost, basis) = claude_accounting(&read.values);
        // A cost-state record is cumulative for the whole session wherever the
        // read reached it; only a sum over the read's requests is bounded by
        // the read.
        let coverage = if basis == AccountingBasis::RecordedTotal || !read.truncated {
            AccountingCoverage::Session
        } else {
            AccountingCoverage::ReadWindow
        };
        let accounting = accounting_for(tokens.as_ref(), cost.as_ref(), basis, coverage);
        let usage_detail = newest_cost_state(&read.values)
            .map(cost_state_detail)
            .and_then(UsageDetail::into_option);

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
            cost,
            tokens,
            accounting,
            store: Some(path.display().to_string()),
            start_uncertain: recording.start_uncertain(),
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
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self {
            root: home_path(&[".claude", "projects"]),
        }
    }
}

impl Backend for ClaudeBackend {
    fn child_transcript(&self, parent: &Session, reference: &str) -> Result<Transcript> {
        if reference.is_empty()
            || !reference
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            anyhow::bail!(
                "child reference must contain only letters, digits, hyphens or underscores"
            );
        }
        let parent_path = Path::new(
            parent
                .store
                .as_deref()
                .ok_or_else(|| anyhow!("parent source unavailable"))?,
        );
        let directory = parent_path.with_extension("").join("subagents");
        let path = directory.join(format!("agent-{reference}.jsonl"));
        let (mut session, turns, read) = self.parse(&path)?;
        if session.id != parent.id {
            anyhow::bail!("child recording does not name the selected parent");
        }
        session.id = format!("{}::{reference}", parent.id);
        let last_turn = read
            .values
            .iter()
            .rposition(|value| !parse_turns(value).is_empty());
        let trailing = trailing_record(read.values.iter(), last_turn, claude_trailing_kind);
        Ok(transcript(
            session,
            turns,
            usize::MAX,
            &read,
            trailing,
            Vec::new(),
        ))
    }

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

    /// A Claude session's children are the subagent transcripts under its own
    /// directory and the `Agent` calls its records hold. The parent names
    /// them; a child's own turns are never read here.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("claude store is unavailable"))?;
        let path = matching_session_file(session_files(root), &session.id)
            .ok_or_else(|| anyhow!("claude session {} is unavailable", session.id))?;
        let read = read_jsonl(&path)?;
        let mut calls = agent_calls(&read.values);

        let mut children = Vec::new();
        for file in subagent_files(&path) {
            let call = file
                .tool_use_id
                .as_ref()
                .and_then(|call_id| calls.remove(call_id))
                .or_else(|| {
                    let call_id = calls
                        .iter()
                        .find(|(_, call)| call.agent_id.as_deref() == Some(&file.agent_id))
                        .map(|(call_id, _)| call_id.clone())?;
                    calls.remove(&call_id)
                })
                .unwrap_or_default();
            let mut source = vec![
                SourceRef::File {
                    path: file.transcript.display().to_string(),
                },
                SourceRef::File {
                    path: file.meta.display().to_string(),
                },
            ];
            source.extend(call.sources());
            children.push(ChildRef {
                role: file.agent_type.or(call.agent_type).or(call.role),
                model: file.model.or(call.model),
                spawned_at: call.spawned_at,
                completed_at: call.completed_at,
                disposition: call.disposition,
                resolved: true,
                source,
                ..ChildRef::new(file.agent_id, self.harness())
            });
        }
        // A call whose transcript is not in the store stays a child: the
        // records name it, and the missing file is what a reader must see.
        for (call_id, call) in calls {
            let source = call.sources();
            children.push(ChildRef {
                role: call.agent_type.or(call.role),
                model: call.model,
                spawned_at: call.spawned_at,
                completed_at: call.completed_at,
                disposition: call.disposition,
                source,
                ..ChildRef::new(call.agent_id.unwrap_or(call_id), self.harness())
            });
        }

        Ok(Lineage {
            children,
            truncation: read_bounds(&read),
            ..Lineage::default()
        })
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

fn newest_cost_state(values: &[Value]) -> Option<&Value> {
    values
        .iter()
        .rev()
        .find(|value| value["type"] == "cost-state")
}

fn claude_accounting(values: &[Value]) -> (Option<Tokens>, Option<Cost>, AccountingBasis) {
    if let Some(cost_state) = newest_cost_state(values) {
        let tokens = cost_state_tokens(cost_state);
        let cost = cost_state["totalCostUSD"].as_f64().map(|usd| Cost { usd });
        return (tokens, cost, AccountingBasis::RecordedTotal);
    }

    (
        claude_request_tokens(values),
        None,
        AccountingBasis::SummedRequests,
    )
}

/// The durations and the per-model split a `cost-state` records beside its
/// totals. The durations are milliseconds of wall clock; the split names each
/// model the session spent on, ordered by model id so one recording reads the
/// same way twice.
fn cost_state_detail(value: &Value) -> UsageDetail {
    let milliseconds = |name: &str| value[name].as_u64();
    let durations = Durations {
        api: milliseconds("totalAPIDuration"),
        api_without_retries: milliseconds("totalAPIDurationWithoutRetries"),
        tool: milliseconds("totalToolDuration"),
        total: milliseconds("totalDuration"),
    };
    let by_model = value["modelUsage"].as_object().map(|models| {
        let mut usage = models
            .iter()
            .map(|(model, usage)| ModelUsage {
                model: model.clone(),
                tokens: model_usage_tokens(usage),
                cost: usage["costUSD"].as_f64().map(|usd| Cost { usd }),
            })
            .collect::<Vec<_>>();
        usage.sort_by(|left, right| left.model.cmp(&right.model));
        usage
    });
    UsageDetail {
        context_window: None,
        rate_limits: None,
        durations_ms: (!durations.is_empty()).then_some(durations),
        by_model: by_model.filter(|usage| !usage.is_empty()),
    }
}

fn model_usage_tokens(usage: &Value) -> Option<Tokens> {
    let mut totals = TokenTotals::default();
    totals.add(
        usage["inputTokens"].as_u64(),
        usage["outputTokens"].as_u64(),
        usage["thinkingTokens"].as_u64(),
        usage["cacheReadInputTokens"].as_u64(),
        usage["cacheCreationInputTokens"].as_u64(),
    );
    totals.finish()
}

fn cost_state_tokens(value: &Value) -> Option<Tokens> {
    let mut totals = TokenTotals::default();
    if let Some(model_usage) = value["modelUsage"].as_object() {
        for usage in model_usage.values() {
            totals.add(
                usage.get("inputTokens").and_then(Value::as_u64),
                usage.get("outputTokens").and_then(Value::as_u64),
                usage.get("thinkingTokens").and_then(Value::as_u64),
                usage.get("cacheReadInputTokens").and_then(Value::as_u64),
                usage
                    .get("cacheCreationInputTokens")
                    .and_then(Value::as_u64),
            );
        }
    }
    totals.finish()
}

fn claude_request_tokens(values: &[Value]) -> Option<Tokens> {
    let mut request_ids = HashSet::new();
    let mut totals = TokenTotals::default();
    for value in values {
        if value["type"] != "assistant" {
            continue;
        }
        let Some(message) = value.get("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            continue;
        };
        if let Some(request_id) = value["requestId"].as_str() {
            if !request_ids.insert(request_id) {
                continue;
            }
        }
        totals.add(
            usage.get("input_tokens").and_then(Value::as_u64),
            usage.get("output_tokens").and_then(Value::as_u64),
            usage
                .get("output_tokens_details")
                .and_then(|details| details.get("thinking_tokens"))
                .and_then(Value::as_u64),
            usage.get("cache_read_input_tokens").and_then(Value::as_u64),
            usage
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64),
        );
    }
    totals.finish()
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
    subagent_files(path).len()
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
    let user_kind = claude_user_kind(value, content.as_str());
    if let Some(text) = content.as_str() {
        return turn(role, user_kind, text.to_owned(), ts, native_id, None)
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
            turn(role, user_kind, text, ts, native_id.clone(), tool)
        })
        .collect()
}

fn turn(
    role: Role,
    user_kind: TurnKind,
    text: String,
    ts: Option<chrono::DateTime<chrono::Utc>>,
    native_id: Option<String>,
    tool: Option<ToolEvent>,
) -> Option<Turn> {
    (!text.is_empty()).then_some(Turn {
        kind: role.kind().unwrap_or(user_kind),
        role,
        text,
        ts,
        ordinal: 0,
        native_id,
        tool,
    })
}

/// Claude writes what a user record is beside its content. `origin.kind` and
/// `promptSource` name the sender; `isMeta` marks text the harness attached
/// itself; and a record carrying none of the three holds a local command when
/// its content is one of the harness's own envelopes and nothing else. A
/// record the sender fields do vouch for keeps its sender whatever its text
/// resembles, so a person who types a command envelope is still an operator.
fn claude_user_kind(value: &Value, content: Option<&str>) -> TurnKind {
    let origin = value["origin"]["kind"].as_str();
    let prompt_source = value["promptSource"].as_str();
    let is_meta = value["isMeta"].as_bool() == Some(true);
    match (origin, prompt_source) {
        (Some("human"), _) | (_, Some("typed")) => return TurnKind::Operator,
        (Some("task-notification" | "auto-continuation"), _) | (_, Some("system")) => {
            return TurnKind::Notice
        }
        _ => {}
    }
    if is_meta {
        let carries_caveat = content.is_some_and(|text| text.contains("<local-command-caveat>"));
        return if carries_caveat {
            TurnKind::Control
        } else {
            TurnKind::Ambient
        };
    }
    if origin.is_some() || prompt_source.is_some() {
        return TurnKind::Unknown;
    }
    match content {
        Some(text) if is_command_envelope(text) || is_element(text, "local-command-stdout") => {
            TurnKind::Control
        }
        _ => TurnKind::Unknown,
    }
}

/// The envelope Claude records a slash command as: a `<command-name>` element
/// and the elements that accompany it, separated by whitespace and holding
/// nothing else.
fn is_command_envelope(text: &str) -> bool {
    let mut rest = text.trim();
    if !rest.starts_with("<command-name>") {
        return false;
    }
    while !rest.is_empty() {
        let Some(tag) = COMMAND_ELEMENTS
            .iter()
            .find(|tag| rest.starts_with(&format!("<{tag}>")))
        else {
            return false;
        };
        let Some(end) = rest.find(&format!("</{tag}>")) else {
            return false;
        };
        rest = rest[end + tag.len() + 3..].trim_start();
    }
    true
}

const COMMAND_ELEMENTS: [&str; 4] = [
    "command-name",
    "command-message",
    "command-args",
    "command-contents",
];

/// Whether the text is one element of the named tag and nothing besides.
fn is_element(text: &str, tag: &str) -> bool {
    let text = text.trim();
    text.starts_with(&format!("<{tag}>"))
        && text.ends_with(&format!("</{tag}>"))
        && !text[tag.len() + 2..].contains(&format!("<{tag}>"))
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

/// The name Claude records a subagent spawn under.
const AGENT_TOOL: &str = "Agent";
/// The status a launch record carries for a subagent still running. Every
/// other status is an ending.
const AGENT_LAUNCHED: &str = "async_launched";

/// What a spawn call in the parent said, and what its result reported. Claude
/// writes the call as an `Agent` tool use and the outcome as a `toolUseResult`
/// object beside the matching tool result, so the tool-use id is the join.
#[derive(Default)]
struct AgentCall {
    role: Option<String>,
    spawned_at: Option<chrono::DateTime<chrono::Utc>>,
    call_record: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
    disposition: Option<String>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    result_record: Option<String>,
}

impl AgentCall {
    fn sources(&self) -> Vec<SourceRef> {
        [&self.call_record, &self.result_record]
            .into_iter()
            .flatten()
            .map(|native_id| SourceRef::Record {
                native_id: native_id.clone(),
            })
            .collect()
    }
}

/// A subagent transcript beside the parent's recording, and the meta record
/// Claude writes next to it.
struct SubagentFile {
    agent_id: String,
    transcript: PathBuf,
    meta: PathBuf,
    tool_use_id: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
}

fn agent_calls(values: &[Value]) -> HashMap<String, AgentCall> {
    let mut calls = HashMap::<String, AgentCall>::new();
    for value in values {
        let ts = timestamp(&value["timestamp"]);
        let record = value["uuid"].as_str().map(str::to_owned);
        for block in value["message"]["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("tool_use") if block["name"] == AGENT_TOOL => {
                    let Some(call_id) = block["id"].as_str() else {
                        continue;
                    };
                    let call = calls.entry(call_id.to_owned()).or_default();
                    call.role = block["input"]["subagent_type"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.role.take());
                    call.spawned_at = ts;
                    call.call_record = record.clone();
                }
                Some("tool_result") => {
                    let Some(call_id) = block["tool_use_id"].as_str() else {
                        continue;
                    };
                    let Some(outcome) = tool_use_result(value) else {
                        continue;
                    };
                    // Claude writes a `toolUseResult` beside every tool's
                    // result, so a result belongs to an agent only when its
                    // call was an `Agent` or the payload names an agent
                    // itself — which is how a spawn behind the read bound is
                    // still recognized.
                    if !calls.contains_key(call_id) && outcome["agentId"].as_str().is_none() {
                        continue;
                    }
                    let call = calls.entry(call_id.to_owned()).or_default();
                    let status = outcome["status"].as_str();
                    call.agent_id = outcome["agentId"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.agent_id.take());
                    call.agent_type = outcome["agentType"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.agent_type.take());
                    call.model = outcome["resolvedModel"]
                        .as_str()
                        .map(str::to_owned)
                        .or(call.model.take());
                    call.disposition = status.map(str::to_owned).or(call.disposition.take());
                    if status.is_some_and(|status| status != AGENT_LAUNCHED) {
                        call.completed_at = ts;
                    }
                    call.result_record = record.clone();
                }
                _ => {}
            }
        }
    }
    calls
}

/// Claude writes the tool result's own payload beside the record, as an
/// object or as the JSON text of one.
fn tool_use_result(value: &Value) -> Option<Value> {
    match value.get("toolUseResult")? {
        Value::Object(object) => Some(Value::Object(object.clone())),
        Value::String(text) => serde_json::from_str(text).ok(),
        _ => None,
    }
}

/// The subagent transcripts recorded under a session's own directory. Each
/// carries a meta record naming the agent, and the file's stem carries the
/// agent id the parent's completion record repeats.
fn subagent_files(path: &Path) -> Vec<SubagentFile> {
    let Some(session) = path.file_stem() else {
        return Vec::new();
    };
    let directory = path.with_file_name(session).join("subagents");
    let mut files = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let agent_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())?
                .strip_prefix("agent-")?
                .to_owned();
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
                .then(|| {
                    let meta = path.with_file_name(format!("agent-{agent_id}.meta.json"));
                    let recorded = fs::read_to_string(&meta)
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .unwrap_or(Value::Null);
                    SubagentFile {
                        agent_id,
                        transcript: path,
                        meta,
                        tool_use_id: recorded["toolUseId"].as_str().map(str::to_owned),
                        agent_type: recorded["agentType"].as_str().map(str::to_owned),
                        model: recorded["model"].as_str().map(str::to_owned),
                    }
                })
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
    files
}
