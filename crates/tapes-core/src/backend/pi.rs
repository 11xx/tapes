use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, head_directory, home_path, jsonl_files, list_files, list_files_with_search,
    read_bounds, read_recording, session_file, timestamp, trailing_record,
    transcript_from_recording, Backend, Jsonl, Listing, ParsedFile, Query, TokenTotals,
};
use crate::content::{
    bounded_shape, text_part, tool_coverage, tool_part, ContentAvailability, ContentCarrier,
    ContentCoverage, ContentPart,
};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::lineage::{Lineage, ParentRef};
use crate::model::{
    AccountingBasis, AccountingCoverage, Cost, Model, Role, Session, SourceDescriptor, Tokens,
    TrailingRecord, Transcript, Turn, TurnKind,
};

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
        let recording = read_recording(path)?;
        let (started_at, last_activity_at) = recording
            .time_range()
            .map_or((None, None), |(started, activity)| {
                (Some(started), Some(activity))
            });
        // pi writes the session header once, as the first line, and nothing
        // past it repeats the id or the working directory. The opening is the
        // only place either can be read on a file past the bounded tail.
        let opening = recording.opening();
        let read = &recording.tail;
        let header = opening.iter().find(|value| value["type"] == "session");
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
        let directory = opening.iter().find_map(pi_cwd).map(PathBuf::from);
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
            .flat_map(|value| parse_turns(value))
            .collect::<Vec<_>>();
        let (tokens, cost) = pi_usage(&active);
        let coverage = if read.truncated {
            AccountingCoverage::ReadWindow
        } else {
            AccountingCoverage::Session
        };
        let accounting = accounting_for(
            tokens.as_ref(),
            cost.as_ref(),
            AccountingBasis::SummedRequests,
            coverage,
        );

        let session = Session {
            id,
            source: SourceDescriptor::installed("pi", path.display().to_string()),
            model,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory,
            started_at,
            last_activity_at,
            live: None,
            cost,
            tokens,
            accounting,
            start_uncertain: recording.start_uncertain(),
            occurrence: None,
            usage_detail: None,
        };
        // pi rewinds by appending a new branch, so the first user message in
        // the file may sit on a root the active path never reaches. The
        // active path is known only when the whole file is in the tail; past
        // the bound the hint stays absent rather than naming an abandoned
        // prompt as the session's first.
        let session = if read.truncated {
            session
        } else {
            session.with_derived_title(&turns)
        };

        Ok((session, turns, recording.tail, abandoned))
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
            |path| head_directory(path, pi_cwd),
            |path| self.parse(path).ok().map(|(session, _, _, _)| session),
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
            |path| head_directory(path, pi_cwd),
            |path| {
                self.parse(path)
                    .ok()
                    .map(|(session, turns, read, _)| ParsedFile {
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

    /// Locate by filename alone: no other session file is opened, so an exact
    /// id costs one directory walk and one parse regardless of store size.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let Some(root) = self.root.as_deref() else {
            return Ok(None);
        };
        let Some(path) = session_file(root, id) else {
            return Ok(None);
        };
        Ok(self.parse(&path).ok().map(|(session, _, _, _)| session))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("pi store is unavailable"))?;
        let path = session_file(root, &session.id)
            .ok_or_else(|| anyhow!("pi session {} is unavailable", session.id))?;
        let (turns, recording, abandoned, trailing_record) = read_transcript(&path)?;
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
        Ok(transcript_from_recording(
            session.clone(),
            turns,
            tail,
            &recording,
            None,
            trailing_record,
            notes,
        ))
    }

    /// pi records a relationship on the session that has one: its header
    /// names the session it came from. A recording names no children of its
    /// own, so a parent's list of them stays empty.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let root = self
            .root
            .as_deref()
            .ok_or_else(|| anyhow!("pi store is unavailable"))?;
        let path = session_file(root, &session.id)
            .ok_or_else(|| anyhow!("pi session {} is unavailable", session.id))?;
        let recording = read_recording(&path)?;
        let parent = recording
            .opening()
            .iter()
            .find(|value| value["type"] == "session")
            .and_then(|value| value["parentSession"].as_str())
            .map(|native_id| ParentRef {
                resolved: session_file(root, native_id).is_some(),
                native_id: native_id.to_owned(),
                source: "session.parentSession".to_owned(),
            });
        Ok(Lineage {
            parent,
            truncation: read_bounds(&recording.tail),
            ..Lineage::default()
        })
    }
}

fn read_transcript(
    path: &Path,
) -> Result<(Vec<Turn>, super::Recording, usize, Option<TrailingRecord>)> {
    let recording = read_recording(path)?;
    let read = &recording.tail;
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
    let mut turns = Vec::new();
    let mut last_turn = None;
    for (index, value) in active.iter().enumerate() {
        let mut parsed = parse_turns(value);
        let span = read
            .values
            .iter()
            .position(|candidate| std::ptr::eq(candidate, *value))
            .and_then(|source_index| read.spans.get(source_index).copied());
        super::attach_record_refs(
            &mut parsed,
            &format!("file:{}", path.display()),
            Some(&read.source_revision),
            span,
        );
        if !parsed.is_empty() {
            last_turn = Some(index);
        }
        turns.extend(parsed);
    }
    let trailing_record = trailing_record(active.iter().copied(), last_turn, pi_trailing_kind);

    Ok((turns, recording, abandoned, trailing_record))
}

/// pi records the working directory once, on the `session` header line.
fn pi_cwd(value: &Value) -> Option<&str> {
    (value["type"] == "session")
        .then(|| value["cwd"].as_str())
        .flatten()
}

fn pi_usage(entries: &[&Value]) -> (Option<Tokens>, Option<Cost>) {
    let mut totals = TokenTotals::default();
    let mut usage_count = 0;
    let mut cost_total = Some(0.0);

    for entry in entries {
        if entry["type"] != "message" {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(usage) = message.get("usage").and_then(Value::as_object) else {
            continue;
        };
        usage_count += 1;
        totals.add(
            usage.get("input").and_then(Value::as_u64),
            usage.get("output").and_then(Value::as_u64),
            usage.get("reasoning").and_then(Value::as_u64),
            usage.get("cacheRead").and_then(Value::as_u64),
            usage.get("cacheWrite").and_then(Value::as_u64),
        );
        let Some(cost) = usage.get("cost").and_then(Value::as_object) else {
            cost_total = None;
            continue;
        };
        let Some(total) = cost.get("total").and_then(Value::as_f64) else {
            cost_total = None;
            continue;
        };
        if let Some(sum) = cost_total.as_mut() {
            *sum += total;
        }
    }

    let cost = (usage_count > 0)
        .then_some(cost_total)
        .flatten()
        .map(|usd| Cost { usd });
    (totals.finish(), cost)
}

fn pi_trailing_kind(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "model_change" => Some("model_change"),
        "thinking_level_change" => Some("thinking_level_change"),
        _ => None,
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

/// pi records its harness commands outside the conversation, so every message
/// in its user role is one the operator sent.
fn user_kind(role: &Role) -> TurnKind {
    role.kind().unwrap_or(TurnKind::Operator)
}

fn parse_turns(value: &Value) -> Vec<Turn> {
    if value["type"] != "message" {
        return Vec::new();
    }
    let message = &value["message"];
    let Some(message_role) = message["role"].as_str() else {
        return Vec::new();
    };
    let ts = timestamp(&value["timestamp"]);
    let native_id = value["id"].as_str().map(str::to_owned);
    let role = match message_role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "developer" => Role::Developer,
        "toolResult" => {
            return vec![Turn {
                role: Role::Tool,
                kind: TurnKind::Tool,
                text: message.to_string(),
                ts,
                ordinal: 0,
                native_id,
                request_turn_id: None,
                record_ref: None,
                channel: None,
                recipient: None,
                parts: vec![tool_part(message, "message", "toolResult")],
                coverage: Some(tool_coverage()),
                tool: Some(pi_result_event(message)),
            }];
        }
        _ => return Vec::new(),
    };
    let content = &message["content"];
    if let Some(text) = content.as_str() {
        return (!text.is_empty())
            .then(|| Turn {
                kind: user_kind(&role),
                role,
                text: text.to_owned(),
                ts,
                ordinal: 0,
                native_id,
                request_turn_id: None,
                record_ref: None,
                channel: None,
                recipient: None,
                parts: vec![text_part(text, "message.content", "text")],
                coverage: Some(ContentCoverage {
                    carrier: ContentCarrier::DirectPart,
                    availability: ContentAvailability::RetainedBody,
                    retained_parts: 1,
                    omitted_parts: 0,
                    omitted_reason: None,
                }),
                tool: None,
            })
            .into_iter()
            .collect();
    }

    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let native_kind = block["type"].as_str()?;
            let (role, text, tool, parts, coverage) = match native_kind {
                "text" => (
                    role.clone(),
                    block["text"].as_str()?.to_owned(),
                    None,
                    vec![text_part(
                        block["text"].as_str()?,
                        "message.content",
                        native_kind,
                    )],
                    ContentCoverage {
                        carrier: ContentCarrier::DirectPart,
                        availability: ContentAvailability::RetainedBody,
                        retained_parts: 1,
                        omitted_parts: 0,
                        omitted_reason: None,
                    },
                ),
                "thinking" => (
                    Role::Reasoning,
                    block["thinking"].as_str()?.to_owned(),
                    None,
                    vec![text_part(
                        block["thinking"].as_str()?,
                        "message.content",
                        native_kind,
                    )],
                    ContentCoverage {
                        carrier: ContentCarrier::DirectPart,
                        availability: ContentAvailability::RetainedBody,
                        retained_parts: 1,
                        omitted_parts: 0,
                        omitted_reason: None,
                    },
                ),
                "toolCall" => (
                    Role::Tool,
                    block.to_string(),
                    Some(pi_call_event(block)),
                    vec![tool_part(block, "message.content", native_kind)],
                    tool_coverage(),
                ),
                _ => (
                    role.clone(),
                    String::new(),
                    None,
                    vec![ContentPart::Unknown {
                        native_kind: native_kind.to_owned(),
                        descriptor: bounded_shape(block),
                        source_field: "message.content".to_owned(),
                        record_ref: None,
                    }],
                    ContentCoverage {
                        carrier: ContentCarrier::DirectPart,
                        availability: ContentAvailability::Unknown,
                        retained_parts: 1,
                        omitted_parts: 0,
                        omitted_reason: None,
                    },
                ),
            };
            ((!text.is_empty()) || !parts.is_empty()).then_some(Turn {
                kind: user_kind(&role),
                role,
                text,
                ts,
                ordinal: 0,
                native_id: native_id.clone(),
                request_turn_id: None,
                record_ref: None,
                channel: None,
                recipient: None,
                parts,
                coverage: Some(coverage),
                tool,
            })
        })
        .collect()
}

fn pi_call_event(block: &Value) -> ToolEvent {
    ToolEvent {
        kind: EventKind::ToolCall,
        subtype: "toolCall".to_owned(),
        name: block["name"].as_str().map(str::to_owned),
        call_id: block["id"].as_str().map(str::to_owned),
        status: None,
        arguments: Bounded::from_value(&block["arguments"]),
        output: None,
        completed_ts: None,
    }
}

fn pi_result_event(message: &Value) -> ToolEvent {
    ToolEvent {
        kind: EventKind::ToolResult,
        subtype: "toolResult".to_owned(),
        name: message["toolName"].as_str().map(str::to_owned),
        call_id: message["toolCallId"].as_str().map(str::to_owned),
        status: (message["isError"].as_bool() == Some(true)).then(|| "error".to_owned()),
        arguments: None,
        output: Bounded::from_value(&message["content"]),
        completed_ts: None,
    }
}
