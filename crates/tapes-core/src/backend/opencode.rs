use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use chrono::{TimeZone, Utc};
use serde_json::Value;

use super::Backend;
use crate::model::{Cost, Model, Role, Session, Tokens, Transcript, Turn};

const MAX_API_BYTES: u64 = 8 * 1024 * 1024;
const MAX_API_SESSIONS: usize = 1_000;
/// Every OpenCode session id carries this prefix.
const SESSION_ID_PREFIX: &str = "ses_";

#[derive(Clone, Debug)]
pub struct OpenCodeBackend {
    program: OsString,
}

impl OpenCodeBackend {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    fn request(&self, path: &str) -> Result<Value> {
        let mut child = Command::new(&self.program)
            .args(["api", "--standalone", "get", path])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to run {}", self.program.to_string_lossy()))?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .expect("piped stdout is present")
            .take(MAX_API_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("failed to read opencode API response")?;
        if bytes.len() as u64 > MAX_API_BYTES {
            child
                .kill()
                .context("failed to stop oversized opencode API response")?;
            child
                .wait()
                .context("failed to reap opencode API process")?;
            return Err(anyhow!(
                "opencode API response exceeded {MAX_API_BYTES} bytes"
            ));
        }
        let status = child
            .wait()
            .context("failed to wait for opencode API process")?;
        if !status.success() {
            return Err(anyhow!("opencode API exited with {status}"));
        }
        serde_json::from_slice(&bytes).context("opencode API returned invalid JSON")
    }

    fn sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let limit = limit.min(MAX_API_SESSIONS);
        let response = self.request(&format!("/api/session?order=desc&limit={limit}"))?;
        parse_sessions(&response)
    }
}

impl Default for OpenCodeBackend {
    fn default() -> Self {
        Self::new("opencode2")
    }
}

impl Backend for OpenCodeBackend {
    fn harness(&self) -> &'static str {
        "opencode"
    }

    fn available(&self) -> bool {
        self.sessions(1).is_ok()
    }

    fn list(&self, limit: usize) -> Result<Vec<Session>> {
        self.sessions(limit)
    }

    /// One session GET rather than a listing page, so an exact id costs a
    /// single request regardless of how many sessions the store holds.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        // OpenCode ids are self-identifying. Rejecting a foreign shape here
        // avoids spawning the API for every claude, codex, or pi lookup —
        // each spawn costs about a second.
        if !id.starts_with(SESSION_ID_PREFIX) {
            return Ok(None);
        }
        let Ok(response) = self.request(&format!("/api/session/{id}")) else {
            return Ok(None);
        };
        let data = &response["data"];
        if !data.is_object() {
            return Ok(None);
        }
        Ok(parse_session(data).ok())
    }

    fn transcript(&self, id: &str, tail: usize) -> Result<Transcript> {
        let session = self
            .locate(id)?
            .ok_or_else(|| anyhow!("opencode session {id} is unavailable"))?;
        let response = self.request(&format!("/api/session/{id}/message"))?;
        parse_transcript(session, &response, tail)
    }
}

fn parse_sessions(response: &Value) -> Result<Vec<Session>> {
    response["data"]
        .as_array()
        .ok_or_else(|| anyhow!("opencode session response has no data array"))?
        .iter()
        .map(parse_session)
        .collect()
}

fn parse_session(value: &Value) -> Result<Session> {
    let id = required_string(value, "id")?;
    let started_at = epoch_millis(&value["time"]["created"])
        .ok_or_else(|| anyhow!("opencode session {id} has no creation time"))?;
    let last_activity_at = epoch_millis(&value["time"]["updated"])
        .ok_or_else(|| anyhow!("opencode session {id} has no update time"))?;
    let model = value["model"]["id"].as_str().map(|id| Model {
        id: id.to_owned(),
        variant: value["model"]["variant"].as_str().map(str::to_owned),
    });
    let tokens = value["tokens"].as_object().map(|tokens| Tokens {
        input: tokens["input"].as_u64(),
        output: tokens["output"].as_u64(),
        reasoning: tokens["reasoning"].as_u64(),
        cache_read: tokens["cache"]["read"].as_u64(),
        cache_write: tokens["cache"]["write"].as_u64(),
    });

    Ok(Session {
        id,
        harness: "opencode".into(),
        model,
        title: value["title"].as_str().map(str::to_owned),
        directory: value["location"]["directory"].as_str().map(PathBuf::from),
        started_at,
        last_activity_at,
        cost: value["cost"].as_f64().map(|usd| Cost { usd }),
        tokens,
    })
}

fn parse_transcript(session: Session, response: &Value, tail: usize) -> Result<Transcript> {
    let messages = response["data"]
        .as_array()
        .ok_or_else(|| anyhow!("opencode message response has no data array"))?;
    let mut turns = messages
        .iter()
        .rev()
        .flat_map(parse_message)
        .collect::<Vec<_>>();
    let tail_truncated = turns.len() > tail;
    if tail_truncated {
        turns.drain(..turns.len() - tail);
    }
    let page_truncated = response["cursor"]["next"].as_str().is_some();
    let notes = page_truncated
        .then(|| "Older OpenCode messages are outside the API page.".to_owned())
        .into_iter()
        .collect();

    Ok(Transcript {
        session,
        turns,
        truncated: page_truncated || tail_truncated,
        notes,
    })
}

fn parse_message(message: &Value) -> Vec<Turn> {
    let Some(message_role) = message["type"].as_str() else {
        return Vec::new();
    };
    let role = match message_role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return Vec::new(),
    };
    let message_ts = epoch_millis(&message["time"]["created"]);
    if role == Role::User {
        return message["text"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| Turn {
                role,
                text: text.to_owned(),
                ts: message_ts,
            })
            .into_iter()
            .collect();
    }

    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| {
            let ts = epoch_millis(&part["time"]["created"]).or(message_ts);
            let (role, text) = match part["type"].as_str()? {
                "text" => (role.clone(), part["text"].as_str()?.to_owned()),
                "reasoning" => (Role::Reasoning, part["text"].as_str()?.to_owned()),
                "tool" => (Role::Tool, part.to_string()),
                _ => return None,
            };
            (!text.is_empty()).then_some(Turn { role, text, ts })
        })
        .collect()
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value[field]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("opencode record has no {field}"))
}

fn epoch_millis(value: &Value) -> Option<chrono::DateTime<Utc>> {
    value
        .as_i64()
        .and_then(|millis| Utc.timestamp_millis_opt(millis).single())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_api_response_is_rejected() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/opencode/malformed.json"
        ))
        .unwrap();

        assert!(parse_sessions(&fixture).is_err());
    }

    #[test]
    fn program_can_be_an_explicit_path() {
        let backend = OpenCodeBackend::new("/missing/opencode2");

        assert_eq!(backend.program, OsString::from("/missing/opencode2"));
        assert!(!backend.available());
    }
}
