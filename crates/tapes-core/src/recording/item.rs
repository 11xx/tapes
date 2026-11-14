use std::io::{self, Write};
use std::ops::Deref;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{ser::SerializeMap, Deserialize, Serialize, Serializer};
use serde_json::Value;

use super::{Kind, Position};
use crate::model::{Model, OperatorOrigin, Role, Turn};

/// A complete tool field sharing the native part's allocation.
#[derive(Clone, Debug)]
pub struct Payload {
    content: Arc<Value>,
    pointer: &'static str,
}
impl Payload {
    fn field(content: &Arc<Value>, pointers: &[&'static str]) -> Option<Self> {
        pointers
            .iter()
            .find(|p| content.pointer(p).is_some_and(|v| !v.is_null()))
            .map(|pointer| Self {
                content: content.clone(),
                pointer,
            })
    }
}
impl Deref for Payload {
    type Target = Value;
    fn deref(&self) -> &Value {
        self.content
            .pointer(self.pointer)
            .expect("retained native field")
    }
}
impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.deref().serialize(serializer)
    }
}

/// Normalized execution state; native status words remain in content.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Running,
    Completed,
    Failed,
    NeverFinished,
    #[serde(other)]
    Unknown,
}
impl Status {
    fn native(word: Option<&str>, failed: bool, kind: Kind) -> Option<Self> {
        if failed {
            return Some(Self::Failed);
        }
        Some(match word {
            Some("completed" | "success") => Self::Completed,
            Some("error" | "failed" | "interrupted" | "cancelled") => Self::Failed,
            Some("running" | "in_progress") => Self::Running,
            Some(_) => Self::Unknown,
            None if kind == Kind::ToolResult => Self::Completed,
            None => return None,
        })
    }
}

/// Complete normalized tool arguments, results and view metadata.
#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    pub name: Option<String>,
    /// Calls report execution state only when their own native record carries
    /// it: OpenCode state or Codex runtime events. Claude/Pi and Codex response
    /// calls omit it; their result with the same call_id carries the outcome.
    /// Running always reflects recorded execution state. An abandoned OpenCode
    /// tool is one never-finished call, without a result item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<Payload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Payload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<Payload>,
    pub arguments: Option<Payload>,
    pub output: Option<Payload>,
}
impl Tool {
    pub(crate) fn native(
        content: &Arc<Value>,
        harness: &str,
        kind: Kind,
    ) -> (Self, Option<String>) {
        let string = |v: &Value, keys: &[&str]| {
            keys.iter()
                .find_map(|key| v[*key].as_str().map(str::to_owned))
        };
        let (name, status, call_id, arguments, output) = match harness {
            "claude" => (
                string(content, &["name"]),
                None,
                string(content, &["id", "tool_use_id"]),
                Payload::field(content, &["/input"]),
                Payload::field(content, &["/content"]),
            ),
            "pi" => (
                string(content, &["name", "toolName"]),
                None,
                string(content, &["id", "toolCallId"]),
                Payload::field(content, &["/arguments"]),
                Payload::field(content, &["/content"]),
            ),
            "opencode" => (
                string(content, &["tool", "name"]),
                string(&content["state"], &["status"]),
                string(content, &["callID", "id"]),
                Payload::field(content, &["/state/input"]),
                Payload::field(
                    content,
                    if content["state"]["status"] == "error" {
                        &["/state/error", "/state/output", "/state/content"]
                    } else {
                        &["/state/content", "/state/output", "/state/error"]
                    },
                ),
            ),
            "codex" if content.get("item").is_some() => (
                string(&content["item"], &["type"]),
                string(&content["item"], &["status"]),
                string(&content["item"], &["id"]),
                (kind == Kind::ToolCall)
                    .then(|| Payload::field(content, &["/item"]))
                    .flatten(),
                (kind == Kind::ToolResult)
                    .then(|| Payload::field(content, &["/item"]))
                    .flatten(),
            ),
            "codex" => (
                string(content, &["name"]).or_else(|| {
                    (content["type"] == "web_search_call").then(|| "web_search_call".to_owned())
                }),
                string(content, &["status"]),
                string(content, &["call_id", "id"]),
                Payload::field(content, &["/arguments", "/input", "/action"]),
                Payload::field(content, &["/output"]),
            ),
            _ => (None, None, None, None, None),
        };
        let arguments = if harness == "codex" && content["type"] == "function_call" {
            arguments.map(
                |arg| match arg.as_str().and_then(|s| serde_json::from_str(s).ok()) {
                    Some(value) => Payload {
                        content: Arc::new(value),
                        pointer: "",
                    },
                    None => arg,
                },
            )
        } else {
            arguments
        };
        let status = if kind == Kind::ToolCall
            && harness != "opencode"
            && !(harness == "codex" && content.get("item").is_some())
        {
            None
        } else {
            Status::native(
                status.as_deref(),
                content["is_error"] == true || content["isError"] == true,
                kind,
            )
        };
        let (title, metadata, caller) = match harness {
            "opencode" => (
                Payload::field(content, &["/state/title"]),
                Payload::field(content, &["/state/metadata"]),
                None,
            ),
            "pi" => (None, Payload::field(content, &["/details"]), None),
            "claude" => (None, None, Payload::field(content, &["/caller"])),
            _ => (None, None, None),
        };
        (
            Self {
                name,
                status,
                title,
                metadata,
                caller,
                arguments,
                output,
            },
            call_id,
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    /// Stable item identity within this recording.
    pub start: Position,
    pub end: Position,
    pub kind: Kind,
    pub role: Role,
    /// Complete normalized message/reasoning text; empty for tool payloads.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    /// Native record/entry ID, shared by its parts; not an item identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<OperatorOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,
    /// Recorded text/reasoning completion state, when the source supplies it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// Claude/Pi: previous native tree entry. OpenCode: the message's parent
    /// message (usually the user prompt). Codex does not record this link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Shared containing-message ID: Claude assistant message.id/user UUID,
    /// Codex message payload.id (not reasoning/tools), Pi entry ID, OpenCode
    /// message ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub representation: Option<String>,
    /// Complete native part, always retained.
    pub content: Arc<Value>,
}
impl Item {
    pub(super) fn from_turn(
        start: Position,
        end: Position,
        kind: Kind,
        turn: Turn,
        content: Arc<Value>,
    ) -> Self {
        Self {
            start,
            end,
            kind,
            role: turn.role,
            text: turn.text,
            ts: turn.ts,
            native_id: turn.native_id,
            request_turn_id: turn.request_turn_id,
            model: turn.model,
            origin: turn.origin,
            call_id: None,
            tool: None,
            status: None,
            parent_id: None,
            message_id: None,
            representation: None,
            content,
        }
    }
    /// Count native content once plus serialized metadata. Normalized text and
    /// tool field projections share that payload budget. The page
    /// envelope and duplicate projections in wire JSON are additional bytes.
    pub fn budget_bytes(&self) -> anyhow::Result<usize> {
        let mut writer = Count(0);
        serde_json::to_writer(&mut writer, &Budget(self))?;
        Ok(writer.0)
    }
}
struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("item size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Budget<'a>(&'a Item);
impl Serialize for Budget<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let i = self.0;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("start", &i.start)?;
        map.serialize_entry("end", &i.end)?;
        map.serialize_entry("kind", &i.kind)?;
        map.serialize_entry("role", &i.role)?;
        for (key, value) in [
            ("native_id", &i.native_id),
            ("request_turn_id", &i.request_turn_id),
            ("call_id", &i.call_id),
            ("parent_id", &i.parent_id),
            ("message_id", &i.message_id),
            ("representation", &i.representation),
        ] {
            if let Some(value) = value {
                map.serialize_entry(key, value)?;
            }
        }
        if let Some(ts) = &i.ts {
            map.serialize_entry("ts", ts)?;
        }
        if let Some(model) = &i.model {
            map.serialize_entry("model", model)?;
        }
        if let Some(origin) = &i.origin {
            map.serialize_entry("origin", origin)?;
        }
        if let Some(status) = &i.status {
            map.serialize_entry("status", status)?;
        }
        if let Some(tool) = &i.tool {
            #[derive(Serialize)]
            struct Header<'a> {
                name: &'a Option<String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                status: &'a Option<Status>,
            }
            map.serialize_entry(
                "tool",
                &Header {
                    name: &tool.name,
                    status: &tool.status,
                },
            )?;
        }
        map.serialize_entry("content", &i.content)?;
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Recorded running state survives normalization; a pending state does not
    // claim execution has started, and a missing state never invents running.
    #[test]
    fn call_status_requires_recorded_execution_state() {
        let content = Arc::new(
            json!({"type":"tool","callID":"c","tool":"bash","state":{"status":"running","input":{}}}),
        );
        let (tool, _) = Tool::native(&content, "opencode", Kind::ToolCall);
        assert_eq!(tool.status, Some(Status::Running));
        assert_ne!(
            Status::native(Some("pending"), false, Kind::ToolCall),
            Some(Status::Running)
        );
        assert_eq!(Status::native(None, false, Kind::ToolCall), None);
    }
}
