use std::io::{self, Write};
use std::ops::Deref;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{ser::SerializeMap, Serialize, Serializer};
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

/// Complete normalized tool arguments and results.
#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    pub name: Option<String>,
    pub status: Option<String>,
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
                (content["is_error"] == true).then(|| "error".to_owned()),
                string(content, &["id", "tool_use_id"]),
                Payload::field(content, &["/input"]),
                Payload::field(content, &["/content"]),
            ),
            "pi" => (
                string(content, &["name", "toolName"]),
                (content["isError"] == true).then(|| "error".to_owned()),
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
                Payload::field(content, &["/arguments", "/input"]),
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
        (
            Self {
                name,
                status,
                arguments,
                output,
            },
            call_id,
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub start: Position,
    pub end: Position,
    pub kind: Kind,
    pub role: Role,
    /// Complete normalized message/reasoning text; empty for tool payloads.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
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
    /// Previous entry in the recorded conversation tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Native identity of the message containing this part.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub representation: Option<String>,
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
            parent_id: None,
            message_id: None,
            representation: None,
            content,
        }
    }
    /// Count native content once plus serialized metadata. Normalized text and
    /// tool argument/output projections share that payload budget. The page
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
        if let Some(tool) = &i.tool {
            #[derive(Serialize)]
            struct Header<'a> {
                name: &'a Option<String>,
                status: &'a Option<String>,
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
