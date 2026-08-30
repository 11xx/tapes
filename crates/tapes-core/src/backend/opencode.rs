use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use super::{filter_listing_search_parallel, Backend, Listing, Query};
use crate::model::{Cost, Model, Role, Session, Tokens, Transcript, Turn};

const MAX_COMMAND_BYTES: u64 = 8 * 1024 * 1024;
const MAX_API_SESSIONS: usize = 1_000;
const MAX_DB_MESSAGES: usize = 1_000;
const MAX_DB_PARTS: usize = 5_000;
const MAX_DB_TEXT_CHARS: usize = 4_000;
const MAX_DB_TOOL_CHARS: usize = 2_000;
/// Keep each SQL prefilter query small without limiting the union of matches.
const MAX_DB_SEARCH_IDS: usize = 256;
/// Every OpenCode session id carries this prefix.
const SESSION_ID_PREFIX: &str = "ses_";

#[derive(Debug, Default)]
struct DatabaseRows {
    values: Vec<Value>,
    invalid: Vec<InvalidDatabaseRow>,
}

#[derive(Debug)]
struct InvalidDatabaseRow {
    id: Option<String>,
    error: String,
}

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

    fn command_bytes(&self, args: &[&str], source: &str) -> Result<Vec<u8>> {
        let mut child = Command::new(&self.program)
            .args(args)
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
            .take(MAX_COMMAND_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read {source} response"))?;
        if bytes.len() as u64 > MAX_COMMAND_BYTES {
            child
                .kill()
                .with_context(|| format!("failed to stop oversized {source} response"))?;
            child
                .wait()
                .with_context(|| format!("failed to reap {source} process"))?;
            return Err(anyhow!(
                "{source} response exceeded {MAX_COMMAND_BYTES} bytes"
            ));
        }
        let status = child
            .wait()
            .with_context(|| format!("failed to wait for {source} process"))?;
        if !status.success() {
            return Err(anyhow!("{source} command exited with {status}"));
        }
        Ok(bytes)
    }

    fn command_json(&self, args: &[&str], source: &str) -> Result<Value> {
        let bytes = self.command_bytes(args, source)?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("{source} command returned invalid JSON"))
    }

    fn request(&self, path: &str) -> Result<Value> {
        self.command_json(&["api", "--standalone", "get", path], "opencode API")
    }

    fn database(&self, query: &str) -> Result<Vec<Value>> {
        strict_database_rows(self.database_rows(query)?)
    }

    fn database_rows(&self, query: &str) -> Result<DatabaseRows> {
        let bytes = self.command_bytes(&["db", "--format", "tsv", query], "opencode database")?;
        let text =
            String::from_utf8(bytes).context("opencode database returned non-UTF-8 output")?;
        parse_database_rows_lossy(&text)
    }

    fn database_search_ids(
        &self,
        needle: &str,
        candidates: &[Session],
    ) -> Result<Option<HashSet<String>>> {
        let candidate_ids = candidates
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let Some(queries) = database_search_queries(needle, &candidate_ids) else {
            return Ok(None);
        };
        let mut matching = HashSet::new();
        for query in queries {
            let rows = self.database(&query)?;
            matching.extend(
                rows.iter()
                    .map(|row| required_string(row, "id"))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        Ok(Some(matching))
    }
}

fn strict_database_rows(rows: DatabaseRows) -> Result<Vec<Value>> {
    if let Some(row) = rows.invalid.into_iter().next() {
        return Err(anyhow!(
            "opencode database returned an invalid row: {}",
            row.error
        ));
    }
    Ok(rows.values)
}

fn parse_database_rows_lossy(text: &str) -> Result<DatabaseRows> {
    if text.trim().is_empty() {
        return Ok(DatabaseRows::default());
    }
    let mut lines = text.lines();
    match lines.next() {
        None => return Ok(DatabaseRows::default()),
        Some("row") => {}
        Some(_) => return Err(anyhow!("opencode database returned unexpected columns")),
    }
    let mut rows = DatabaseRows::default();
    for line in lines.filter(|line| !line.is_empty()) {
        match serde_json::from_str(line) {
            Ok(value) => rows.values.push(value),
            Err(error) => rows.invalid.push(InvalidDatabaseRow {
                id: database_row_id(line).map(str::to_owned),
                error: error.to_string(),
            }),
        }
    }
    Ok(rows)
}

fn database_row_id(line: &str) -> Option<&str> {
    let key_end = line.find("\"id\"")? + "\"id\"".len();
    let remainder = line[key_end..].trim_start().strip_prefix(':')?.trim_start();
    let value = remainder.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

fn session_diagnostic(id: Option<&str>, error: String) -> String {
    match id {
        Some(id) => format!("opencode session {id}: {error}"),
        None => format!("opencode session with unknown id: {error}"),
    }
}

impl OpenCodeBackend {
    fn uses_database(&self) -> bool {
        Path::new(&self.program)
            .file_name()
            .is_some_and(|name| name == "opencode")
    }

    fn sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let limit = limit.min(MAX_API_SESSIONS);
        let response = self.request(&format!("/api/session?order=desc&limit={limit}"))?;
        parse_sessions(&response)
    }

    fn session_listing(&self, limit: usize) -> Result<Listing> {
        let limit = limit.min(MAX_API_SESSIONS);
        if self.uses_database() {
            return self.database_sessions(limit);
        }
        Ok(Listing::from_sessions(self.sessions(limit)?))
    }

    fn database_sessions(&self, limit: usize) -> Result<Listing> {
        let rows = self.database_rows(&format!(
            "SELECT json_object( \
                 'id', id, 'title', title, 'directory', directory, \
                 'time_created', time_created, 'time_updated', time_updated, \
                 'model', model, 'cost', cost, 'tokens_input', tokens_input, \
                 'tokens_output', tokens_output, 'tokens_reasoning', tokens_reasoning, \
                 'tokens_cache_read', tokens_cache_read, \
                 'tokens_cache_write', tokens_cache_write) AS row \
             FROM session ORDER BY time_updated DESC LIMIT {limit}"
        ))?;
        let scanned = rows.values.len() + rows.invalid.len();
        let mut listing = Listing {
            scanned,
            ..Listing::default()
        };
        for row in rows.values {
            match parse_database_session(&row) {
                Ok(session) => listing.sessions.push(session),
                Err(error) => listing
                    .unavailable
                    .push(session_diagnostic(row["id"].as_str(), error.to_string())),
            }
        }
        listing
            .unavailable
            .extend(rows.invalid.into_iter().map(|row| {
                session_diagnostic(
                    row.id.as_deref(),
                    format!("opencode database returned an invalid row: {}", row.error),
                )
            }));
        Ok(listing)
    }

    fn database_locate(&self, id: &str) -> Result<Option<Session>> {
        let rows = self.database(&format!(
            "SELECT json_object( \
                 'id', id, 'title', title, 'directory', directory, \
                 'time_created', time_created, 'time_updated', time_updated, \
                 'model', model, 'cost', cost, 'tokens_input', tokens_input, \
                 'tokens_output', tokens_output, 'tokens_reasoning', tokens_reasoning, \
                 'tokens_cache_read', tokens_cache_read, \
                 'tokens_cache_write', tokens_cache_write) AS row \
             FROM session WHERE id = {} LIMIT 1",
            sql_literal(id)
        ))?;
        rows.first().map(parse_database_session).transpose()
    }

    fn database_transcript(&self, session: Session, tail: usize) -> Result<Transcript> {
        let id = sql_literal(&session.id);
        let mut message_rows = self.database(&database_message_query(&id))?;
        let mut truncated = message_rows.len() > MAX_DB_MESSAGES;
        message_rows.truncate(MAX_DB_MESSAGES);

        let mut part_rows = self.database(&format!(
            "SELECT json_object( \
                 'message_id', message_id, 'time_created', time_created, 'data', \
                 CASE json_extract(data, '$.type') \
             WHEN 'text' THEN json_object( \
                 'type', 'text', \
                 'text', substr(json_extract(data, '$.text'), 1, {MAX_DB_TEXT_CHARS}), \
                 'truncated', length(json_extract(data, '$.text')) > {MAX_DB_TEXT_CHARS}, \
                 'time', json_extract(data, '$.time')) \
             WHEN 'reasoning' THEN json_object( \
                 'type', 'reasoning', \
                 'text', substr(json_extract(data, '$.text'), 1, {MAX_DB_TEXT_CHARS}), \
                 'truncated', length(json_extract(data, '$.text')) > {MAX_DB_TEXT_CHARS}, \
                 'time', json_extract(data, '$.time')) \
             WHEN 'tool' THEN json_object( \
                 'type', 'tool', \
                 'tool', json_extract(data, '$.tool'), \
                 'callID', json_extract(data, '$.callID'), \
                 'state', json_object( \
                     'status', json_extract(data, '$.state.status'), \
                     'input', substr(json_extract(data, '$.state.input'), 1, {MAX_DB_TOOL_CHARS}), \
                     'output', substr(json_extract(data, '$.state.output'), 1, {MAX_DB_TOOL_CHARS}), \
                     'error', substr(json_extract(data, '$.state.error'), 1, {MAX_DB_TOOL_CHARS})), \
                 'truncated', \
                     length(json_extract(data, '$.state.input')) > {MAX_DB_TOOL_CHARS} OR \
                     length(json_extract(data, '$.state.output')) > {MAX_DB_TOOL_CHARS} OR \
                     length(json_extract(data, '$.state.error')) > {MAX_DB_TOOL_CHARS}, \
                 'time', json_extract(data, '$.time')) \
                 ELSE json_object('type', json_extract(data, '$.type')) END) AS row \
             FROM part WHERE session_id = {id} ORDER BY time_created DESC LIMIT {}",
            MAX_DB_PARTS + 1
        ))?;
        truncated |= part_rows.len() > MAX_DB_PARTS;
        part_rows.truncate(MAX_DB_PARTS);

        let mut parts = HashMap::<String, Vec<Value>>::new();
        for row in part_rows {
            let message_id = required_string(&row, "message_id")?;
            let data = database_data(&row)?;
            truncated |= data["truncated"].as_bool().unwrap_or(false)
                || data["truncated"].as_i64() == Some(1);
            parts.entry(message_id).or_default().push(data);
        }
        for values in parts.values_mut() {
            values.reverse();
        }

        let messages = message_rows
            .iter()
            .filter_map(|row| {
                database_message(row, parts.remove(row["id"].as_str()?).unwrap_or_default())
            })
            .collect::<Result<Vec<_>>>()?;
        let response = json!({
            "data": messages,
            "cursor": if truncated { json!({ "next": "database" }) } else { Value::Null }
        });
        parse_transcript(session, &response, tail)
    }
}

fn installed_programs() -> Vec<OsString> {
    ["opencode", "opencode2"]
        .into_iter()
        .filter(|program| program_available(OsStr::new(program)))
        .map(OsString::from)
        .collect()
}

fn program_available(program: &OsStr) -> bool {
    Path::new(program).is_file()
        || std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
        })
}

fn default_program() -> OsString {
    installed_programs()
        .into_iter()
        .next()
        .unwrap_or_else(|| OsString::from("opencode"))
}

impl Default for OpenCodeBackend {
    fn default() -> Self {
        Self::new(default_program())
    }
}

impl OpenCodeBackend {
    pub(crate) fn defaults() -> Vec<Self> {
        let programs = installed_programs();
        if programs.is_empty() {
            vec![Self::default()]
        } else {
            programs.into_iter().map(Self::new).collect()
        }
    }
}

impl Backend for OpenCodeBackend {
    fn harness(&self) -> &'static str {
        "opencode"
    }

    fn available(&self) -> bool {
        // This is only an executable-presence hint. Parsing here would turn a
        // bad row into a false whole-backend absence before `list` can report
        // it as a session-specific diagnostic.
        program_available(&self.program)
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        // The backend pages globally and carries metadata for every session,
        // so scope and metadata filters are applied to a full page rather
        // than to the caller's limit. Otherwise matching sessions could fall
        // off the end of a page spent on other projects or models.
        let candidate_limit = if query.scope.is_some() || query.has_filters() {
            MAX_API_SESSIONS
        } else {
            query.limit
        };
        let mut page = self.session_listing(candidate_limit)?;
        let scanned = page.scanned;
        page.sessions = page
            .sessions
            .into_iter()
            .filter(|session| {
                query.scope.is_none_or(|scope| {
                    session
                        .directory
                        .as_deref()
                        .is_some_and(|directory| scope.contains(directory))
                })
            })
            .filter(|session| query.matches(session))
            .take(query.limit)
            .collect();
        page.scanned = scanned;
        page.scan_truncated = scanned >= MAX_API_SESSIONS;
        Ok(page)
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        let listing = self.list(query)?;
        if !self.uses_database() {
            return Ok(filter_listing_search_parallel(self, listing, needle, tail));
        }
        let ids = match self.database_search_ids(needle, &listing.sessions) {
            Ok(Some(ids)) => ids,
            Ok(None) | Err(_) => {
                return Ok(filter_listing_search_parallel(self, listing, needle, tail));
            }
        };
        let mut candidates = listing;
        candidates
            .sessions
            .retain(|session| ids.contains(&session.id));
        Ok(filter_listing_search_parallel(
            self, candidates, needle, tail,
        ))
    }

    /// One session GET rather than a listing page, so an exact id costs a
    /// single request regardless of how many sessions the store holds.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        // OpenCode ids are self-identifying. Rejecting a foreign shape here
        // avoids spawning OpenCode for every claude, codex, or pi lookup — each
        // spawn costs about a second.
        if !id.starts_with(SESSION_ID_PREFIX) {
            return Ok(None);
        }
        if self.uses_database() {
            return self.database_locate(id);
        }
        // A genuine miss is `Ok(None)`; a broken API or malformed payload is an
        // error and must stay one. Resolution lets another backend win over a
        // failing one, but reports the failure when nothing resolves — so
        // collapsing the two here would hide real breakage from `show` and
        // `export`.
        let response = self.request(&format!("/api/session/{id}"))?;
        let data = &response["data"];
        if !data.is_object() {
            return Ok(None);
        }
        parse_session(data).map(Some)
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        if self.uses_database() {
            return self.database_transcript(session.clone(), tail);
        }
        let response = self.request(&format!("/api/session/{}/message", session.id))?;
        parse_transcript(session.clone(), &response, tail)
    }
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn database_search_queries(needle: &str, candidate_ids: &[String]) -> Option<Vec<String>> {
    if !needle.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    let needle = sql_literal(&needle.to_lowercase());
    Some(
        candidate_ids
            .chunks(MAX_DB_SEARCH_IDS)
            .map(|ids| database_search_query(&needle, ids))
            .collect(),
    )
}

fn database_search_query(needle: &str, candidate_ids: &[String]) -> String {
    let candidate_ids = candidate_ids
        .iter()
        .map(|id| sql_literal(id))
        .collect::<Vec<_>>()
        .join(", ");
    // `json_tree` sees escaped strings after JSON decoding. SQLite's `lower`
    // is ASCII-only; the replacements cover the non-ASCII code points whose
    // lowercase forms can expose the ASCII letters handled here.
    let part_match = format!(
        "json_valid(data) = 0 \
         OR instr(lower(data), {needle}) > 0 \
         OR EXISTS ( \
             SELECT 1 FROM json_tree(CASE WHEN json_valid(data) = 1 THEN data ELSE '{{}}' END) \
             WHERE instr(lower(CAST(key AS TEXT)), {needle}) > 0 \
                OR instr(lower(replace(replace(CAST(value AS TEXT), char(8490), 'k'), char(304), 'i')), {needle}) > 0))"
    );
    format!(
        "SELECT json_object('id', session_id) AS row FROM ( \
             SELECT DISTINCT session_id FROM message \
             WHERE session_id IN ({candidate_ids}) AND json_valid(data) = 0 \
             UNION \
             SELECT DISTINCT session_id FROM part \
             WHERE session_id IN ({candidate_ids}) AND ({part_match}))"
    )
}

fn database_message_query(id: &str) -> String {
    format!(
        "SELECT json_object( \
             'id', id, 'time_created', time_created, 'data', \
             json_object('role', json_extract(data, '$.role'), \
                         'time', json_extract(data, '$.time'))) AS row \
         FROM message WHERE session_id = {id} ORDER BY time_created DESC LIMIT {}",
        MAX_DB_MESSAGES + 1
    )
}

fn database_data(row: &Value) -> Result<Value> {
    if row["data"].is_object() {
        return Ok(row["data"].clone());
    }
    let data = required_string(row, "data")?;
    serde_json::from_str(&data).context("opencode database record contains invalid JSON")
}

fn database_message(row: &Value, parts: Vec<Value>) -> Option<Result<Value>> {
    let data = match database_data(row) {
        Ok(data) => data,
        Err(error) => return Some(Err(error)),
    };
    let role = data["role"].as_str()?;
    if !matches!(role, "user" | "assistant") {
        return None;
    }
    let message_time = data["time"]["created"]
        .as_i64()
        .or_else(|| row["time_created"].as_i64());
    let mut parts = parts;
    let user_text = parts
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for part in &mut parts {
        if let Some(time) = part["time"].as_object_mut() {
            if let Some(start) = time.get("start").cloned() {
                time.entry("created").or_insert(start);
            }
        }
    }
    Some(Ok(if role == "user" {
        json!({
            "type": "user",
            "time": { "created": message_time },
            "text": user_text
        })
    } else {
        json!({
            "type": "assistant",
            "time": { "created": message_time },
            "content": parts
        })
    }))
}

fn parse_database_session(value: &Value) -> Result<Session> {
    let id = required_string(value, "id")?;
    let started_at = epoch_millis(&value["time_created"])
        .ok_or_else(|| anyhow!("opencode session {id} has no creation time"))?;
    let last_activity_at = epoch_millis(&value["time_updated"])
        .ok_or_else(|| anyhow!("opencode session {id} has no update time"))?;
    let model = database_model(&value["model"]);
    let tokens = database_tokens(value);

    Ok(Session {
        id,
        harness: "opencode".into(),
        model,
        title: value["title"]
            .as_str()
            .filter(|title| !title.is_empty())
            .map(str::to_owned),
        derived_title: None,
        derived_title_truncated: None,
        directory: value["directory"].as_str().map(PathBuf::from),
        started_at,
        last_activity_at,
        live: None,
        cost: value["cost"].as_f64().map(|usd| Cost { usd }),
        tokens,
    })
}

fn database_model(value: &Value) -> Option<Model> {
    let parsed = value
        .as_str()
        .and_then(|model| serde_json::from_str(model).ok());
    let value = parsed.as_ref().unwrap_or(value);
    let id = value["id"]
        .as_str()
        .or_else(|| value["modelID"].as_str())?
        .to_owned();
    Some(Model {
        id,
        variant: value["variant"].as_str().map(str::to_owned),
    })
}

fn database_tokens(value: &Value) -> Option<Tokens> {
    let fields = [
        "tokens_input",
        "tokens_output",
        "tokens_reasoning",
        "tokens_cache_read",
        "tokens_cache_write",
    ];
    fields
        .iter()
        .any(|field| !value[*field].is_null())
        .then(|| Tokens {
            input: value["tokens_input"].as_u64(),
            output: value["tokens_output"].as_u64(),
            reasoning: value["tokens_reasoning"].as_u64(),
            cache_read: value["tokens_cache_read"].as_u64(),
            cache_write: value["tokens_cache_write"].as_u64(),
        })
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
        derived_title: None,
        derived_title_truncated: None,
        directory: value["location"]["directory"].as_str().map(PathBuf::from),
        started_at,
        last_activity_at,
        live: None,
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
    let page_truncated = response["cursor"]["next"].as_str().is_some();
    let tail_truncated = turns.len() > tail;
    if tail_truncated {
        turns.drain(..turns.len() - tail);
    }
    let notes = page_truncated
        .then(|| "Some OpenCode content is outside the bounded read.".to_owned())
        .into_iter()
        .collect();

    Ok(Transcript {
        // The session endpoint is the normalized source of metadata. Message
        // reads stay a transcript operation and do not invent a title that a
        // title-less API listing could not provide without extra per-row work.
        session,
        turns,
        truncated: page_truncated || tail_truncated,
        trailing_record: None,
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
    fn empty_database_output_is_a_miss() {
        assert!(parse_database_rows_lossy("").unwrap().values.is_empty());
        assert!(parse_database_rows_lossy("\n").unwrap().values.is_empty());
    }

    #[test]
    fn database_search_prefilter_batches_without_limiting_matches() {
        let candidate_ids = (0..=MAX_DB_SEARCH_IDS)
            .map(|index| format!("ses_{index:03}"))
            .collect::<Vec<_>>();

        let queries = database_search_queries("quota", &candidate_ids).unwrap();

        assert_eq!(queries.len(), 2);
        assert!(queries[0].contains("'ses_000'"));
        assert!(!queries[0].contains("'ses_256'"));
        assert!(queries[1].contains("'ses_256'"));
        assert!(queries.iter().all(|query| query.contains("json_tree(CASE")));
        assert!(queries
            .iter()
            .all(|query| query.contains("json_valid(data) = 0")));
        assert!(queries.iter().all(|query| !query.contains("LIMIT")));
    }

    #[test]
    fn database_message_query_projects_only_bounded_metadata() {
        let query = database_message_query("'ses_fixture'");

        assert!(query.contains("json_extract(data, '$.role')"));
        assert!(query.contains("json_extract(data, '$.time')"));
        assert!(!query.contains("'data', data"));
        assert!(query.contains("LIMIT 1001"));
    }

    #[test]
    fn program_can_be_an_explicit_path() {
        let backend = OpenCodeBackend::new("/missing/opencode2");

        assert_eq!(backend.program, OsString::from("/missing/opencode2"));
        assert!(!backend.available());
    }
}
