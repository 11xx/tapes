use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

use super::{
    accounting_for, filter_listing_search, filter_listing_search_parallel, search_turns, Backend,
    Listing, Query, StreamCoordinates, StreamedTranscript, TokenTotals,
};
use crate::content::{
    bounded_shape, text_part, tool_coverage, tool_part, ContentAvailability, ContentCarrier,
    ContentCoverage, ContentPart,
};
use crate::event::{self, Bounded, EventKind, EventTranscript, ToolEvent};
use crate::lineage::{ChildRef, Lineage, ParentRef, SourceRef};
use crate::model::{
    human_bytes, AccountingBasis, AccountingCoverage, Cost, Model, Role, Session, SourceBound,
    SourceDescriptor, SourceLocation, Tokens, Transcript, Truncation, Turn, TurnKind, TurnWindow,
};

const MAX_COMMAND_BYTES: u64 = 8 * 1024 * 1024;
const MAX_API_SESSIONS: usize = 1_000;
const MAX_DB_MESSAGES: usize = 1_000;
/// Messages the API transcript read will page through before it stops and
/// reports the bound; the same ceiling the database read applies.
const MAX_API_MESSAGES: usize = 1_000;
/// Messages per API page. Small enough that a page of a heavy session fits
/// the transport bound, large enough that a default `show` needs few pages.
const MESSAGE_PAGE: usize = 50;
/// Fewer messages than this per page would spend a process spawn per turn on
/// a session whose newest messages carry no text.
const MIN_MESSAGE_PAGE: usize = 8;
const MAX_DB_PARTS: usize = 5_000;
const MAX_DB_TEXT_CHARS: usize = 4_000;
const MAX_DB_TOOL_CHARS: usize = 2_000;
/// Parts per page of a database part read. A projected part holds at most
/// 6,000 characters, so this many fit the transport bound even when every
/// character is written as a six-byte JSON escape.
const MAX_DB_PART_PAGE: usize = 128;
/// Keep each SQL prefilter query small without limiting the union of matches.
const MAX_DB_SEARCH_IDS: usize = 256;
/// A v2 message this large may still make the API confirmation ambiguous if its
/// output is truncated by a client, so keep its session as a candidate.
const MAX_V2_PREFILTER_MESSAGE_BYTES: usize = 64 * 1024;
const API_SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const API_SERVER_RETRY_DELAY: Duration = Duration::from_millis(10);
const API_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// Every OpenCode session id carries this prefix.
const SESSION_ID_PREFIX: &str = "ses_";
/// Child rows a lineage read returns before it reports that it stopped.
const MAX_LINEAGE_CHILDREN: usize = 1_000;

/// A response the transport refused to carry whole. Typed so a paged read can
/// tell it from every other failure and ask for a smaller page.
#[derive(Debug)]
struct ResponseTooLarge {
    source: String,
}

impl std::fmt::Display for ResponseTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} response exceeded {MAX_COMMAND_BYTES} bytes",
            self.source
        )
    }
}

impl std::error::Error for ResponseTooLarge {}

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

#[derive(Clone, Copy, Debug)]
struct OpenCodeApiClient {
    port: u16,
}

struct OpenCodeApiServer {
    client: OpenCodeApiClient,
    child: std::process::Child,
}

impl Drop for OpenCodeApiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl OpenCodeApiServer {
    fn start(program: &OsStr) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("failed to reserve a local port for the opencode API")?;
        let port = listener
            .local_addr()
            .context("failed to inspect the reserved opencode API port")?
            .port();
        drop(listener);

        let port_text = port.to_string();
        let mut child = Command::new(program)
            .args(["serve", "--hostname", "127.0.0.1", "--port", &port_text])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start the opencode API server with {}",
                    program.to_string_lossy()
                )
            })?;
        let client = OpenCodeApiClient { port };
        let deadline = Instant::now() + API_SERVER_START_TIMEOUT;
        loop {
            if let Some(status) = child
                .try_wait()
                .context("failed to check the opencode API server")?
            {
                return Err(anyhow!("opencode API server exited with {status}"));
            }
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Ok(Self { client, child });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("opencode API server did not become ready"));
            }
            thread::sleep(API_SERVER_RETRY_DELAY);
        }
    }
}

impl OpenCodeApiClient {
    fn request(&self, path: &str) -> Result<Value> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))
            .context("failed to connect to the opencode API server")?;
        stream
            .set_read_timeout(Some(API_RESPONSE_TIMEOUT))
            .context("failed to bound the opencode API read")?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
            self.port
        );
        stream
            .write_all(request.as_bytes())
            .context("failed to request the opencode API")?;

        let mut bytes = Vec::new();
        stream
            .take(MAX_COMMAND_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("failed to read the opencode API response")?;
        if bytes.len() as u64 > MAX_COMMAND_BYTES {
            return Err(ResponseTooLarge {
                source: "opencode API".to_owned(),
            }
            .into());
        }
        let separator = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or_else(|| anyhow!("opencode API response has no header terminator"))?;
        let headers = &bytes[..separator];
        let body = &bytes[separator + 4..];
        let status = http_status(headers)?;
        if !(200..300).contains(&status) {
            return Err(anyhow!("opencode API server returned HTTP {status}"));
        }
        let body = if is_chunked(headers)? {
            decode_chunked_body(body)?
        } else {
            body.to_vec()
        };
        serde_json::from_slice(&body).context("opencode API server returned invalid JSON")
    }

    fn search(&self, session: &Session, needle: &str, tail: usize) -> Result<bool> {
        let pages = paged_messages(&|path| self.request(path), &session.id, tail)?;
        let transcript = paged_transcript(session.clone(), &pages, tail);
        search_turns(&transcript, needle, tail)
    }

    fn sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let response = self.request(&format!("/api/session?order=desc&limit={limit}"))?;
        parse_sessions(&response)
    }
}

fn http_status(headers: &[u8]) -> Result<u16> {
    let line_end = headers
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(headers.len());
    let line = std::str::from_utf8(&headers[..line_end])
        .context("opencode API response has invalid headers")?
        .trim_end_matches('\r');
    line.split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("opencode API response has no status"))?
        .parse()
        .context("opencode API response has an invalid status")
}

fn is_chunked(headers: &[u8]) -> Result<bool> {
    let headers =
        std::str::from_utf8(headers).context("opencode API response has invalid headers")?;
    Ok(headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
        })
    }))
}

fn decode_chunked_body(body: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut offset = 0;
    loop {
        let line_end = body[offset..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .map(|end| offset + end)
            .ok_or_else(|| anyhow!("opencode API response has an incomplete chunk size"))?;
        let size = std::str::from_utf8(&body[offset..line_end])
            .context("opencode API response has an invalid chunk size")?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        let size = usize::from_str_radix(size, 16)
            .context("opencode API response has an invalid chunk size")?;
        offset = line_end + 2;
        if size == 0 {
            return Ok(decoded);
        }
        let chunk_end = offset
            .checked_add(size)
            .and_then(|end| end.checked_add(2))
            .ok_or_else(|| anyhow!("opencode API chunk size overflowed"))?;
        if chunk_end > body.len() || &body[offset + size..chunk_end] != b"\r\n" {
            return Err(anyhow!("opencode API response has an incomplete chunk"));
        }
        if decoded.len().saturating_add(size) > MAX_COMMAND_BYTES as usize {
            return Err(anyhow!(
                "opencode API response exceeded {MAX_COMMAND_BYTES} bytes"
            ));
        }
        decoded.extend_from_slice(&body[offset..offset + size]);
        offset = chunk_end;
    }
}

const MAX_API_SEARCH_WORKERS: usize = 8;

fn filter_listing_search_api(
    mut listing: Listing,
    client: &OpenCodeApiClient,
    needle: &str,
    tail: usize,
) -> Listing {
    if listing.sessions.is_empty() {
        return listing;
    }
    let worker_count = thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_API_SEARCH_WORKERS)
        .min(listing.sessions.len());
    let chunk_size = listing.sessions.len().div_ceil(worker_count);
    let sessions = std::mem::take(&mut listing.sessions);
    let searched = thread::scope(|scope| {
        let handles = sessions
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(|| {
                    chunk
                        .iter()
                        .map(|session| (session, client.search(session, needle, tail)))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("content search worker panicked"))
            .collect::<Vec<_>>()
    });
    for (session, result) in searched {
        match result {
            Ok(true) => listing.sessions.push(session.clone()),
            Ok(false) => {}
            Err(error) => listing
                .unsearched
                .push(format!("{} session {}: {error:#}", "opencode", session.id)),
        }
    }
    listing
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
        self.command_bytes_with(&self.program, args, source)
    }

    fn command_bytes_with(&self, program: &OsStr, args: &[&str], source: &str) -> Result<Vec<u8>> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to run {}", program.to_string_lossy()))?;
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
            return Err(ResponseTooLarge {
                source: source.to_owned(),
            }
            .into());
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
        if self.is_default_program("opencode") && program_available(OsStr::new("sqlite3")) {
            if let Some(database) = database_path("opencode.db").filter(|path| path.is_file()) {
                let database = database
                    .to_str()
                    .ok_or_else(|| anyhow!("opencode database path is not UTF-8"))?;
                // List mode prints the one `json_object` column byte for
                // byte, and `json_object` escapes every control character,
                // so each row is one line. Tab mode is not verbatim: sqlite3
                // 3.53.4 quotes a value that contains `"`.
                let bytes = self.command_bytes_with(
                    OsStr::new("sqlite3"),
                    &["-readonly", "-batch", "-list", "-header", database, query],
                    "opencode database",
                )?;
                let text = String::from_utf8(bytes)
                    .context("opencode database returned non-UTF-8 output")?;
                return parse_database_rows_lossy(&text);
            }
        }
        let bytes = self.command_bytes(&["db", "--format", "tsv", query], "opencode database")?;
        let text =
            String::from_utf8(bytes).context("opencode database returned non-UTF-8 output")?;
        parse_database_rows_lossy(&text)
    }

    fn is_default_program(&self, name: &str) -> bool {
        Path::new(&self.program).components().count() == 1
            && Path::new(&self.program).file_name() == Some(OsStr::new(name))
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

    fn v2_database_search_ids(
        &self,
        needle: &str,
        candidates: &[Session],
    ) -> Result<Option<HashSet<String>>> {
        if !self.is_default_program("opencode2") {
            return Ok(None);
        }
        if !program_available(OsStr::new("sqlite3")) {
            return Ok(None);
        }
        let Some(database) = database_path("opencode-next.db") else {
            return Ok(None);
        };
        if !database.is_file() {
            return Ok(None);
        }
        let candidate_ids = candidates
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let Some(needle) = search_needle_literal(needle) else {
            return Ok(None);
        };

        let mut matching = HashSet::new();
        for ids in candidate_ids.chunks(MAX_DB_SEARCH_IDS) {
            let id_list = sql_id_list(ids);
            let coverage = self.sqlite_json(
                &database,
                &format!("SELECT count(*) AS count FROM session WHERE id IN ({id_list})"),
            )?;
            let count = coverage
                .as_array()
                .and_then(|rows| rows.first())
                .and_then(|row| row["count"].as_u64())
                .ok_or_else(|| anyhow!("opencode v2 database returned no session count"))?;
            if count != ids.len() as u64 {
                return Ok(None);
            }

            let rows = self.sqlite_json(&database, &v2_database_search_query(&needle, ids))?;
            let rows = rows
                .as_array()
                .ok_or_else(|| anyhow!("opencode v2 database returned a non-array result"))?;
            matching.extend(
                rows.iter()
                    .map(|row| required_string(row, "id"))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        Ok(Some(matching))
    }

    fn sqlite_json(&self, database: &Path, query: &str) -> Result<Value> {
        let database = database
            .to_str()
            .ok_or_else(|| anyhow!("opencode v2 database path is not UTF-8"))?;
        let args = ["-readonly", "-batch", "-json", database, query];
        let bytes =
            self.command_bytes_with(OsStr::new("sqlite3"), &args, "opencode v2 database")?;
        parse_sqlite_json(&bytes)
    }
}

fn parse_sqlite_json(bytes: &[u8]) -> Result<Value> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Array(Vec::new()));
    }
    serde_json::from_slice(bytes).context("opencode v2 database returned invalid JSON output")
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
                Ok(mut session) => {
                    session.source.location = Some(SourceLocation {
                        locator: self.store_coordinate(&session.id),
                        member: None,
                    });
                    listing.sessions.push(session);
                }
                Err(error) => {
                    if let Some(id) = row["id"].as_str() {
                        listing.unavailable_ids.push(id.to_owned());
                    }
                    listing
                        .unavailable
                        .push(session_diagnostic(row["id"].as_str(), error.to_string()));
                }
            }
        }
        for row in rows.invalid {
            if let Some(id) = &row.id {
                listing.unavailable_ids.push(id.clone());
            }
            listing.unavailable.push(session_diagnostic(
                row.id.as_deref(),
                format!("opencode database returned an invalid row: {}", row.error),
            ));
        }
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
        let mut session = rows.first().map(parse_database_session).transpose()?;
        if let Some(session) = session.as_mut() {
            session.source.location = Some(SourceLocation {
                locator: self.store_coordinate(&session.id),
                member: None,
            });
        }
        Ok(session)
    }

    /// The relationships the session table records. `parent_id` is the
    /// column a child carries; a row naming this session in it is a child.
    /// A store that does not record a fork answers the shorter query.
    fn database_lineage(&self, session: &Session) -> Result<Lineage> {
        let id = sql_literal(&session.id);
        let own = self.first_supported(&[
            format!(
                "SELECT json_object('parent_id', parent_id, 'fork_session_id', fork_session_id) \
                 AS row FROM session WHERE id = {id} LIMIT 1"
            ),
            format!(
                "SELECT json_object('parent_id', parent_id) AS row FROM session \
                 WHERE id = {id} LIMIT 1"
            ),
        ])?;
        let own = own.first().cloned().unwrap_or(Value::Null);
        let parent = own["parent_id"]
            .as_str()
            .map(|native_id| -> Result<ParentRef> {
                Ok(ParentRef {
                    resolved: self.database_locate(native_id)?.is_some(),
                    native_id: native_id.to_owned(),
                    source: "session.parent_id".to_owned(),
                })
            })
            .transpose()?;

        let rows = self.first_supported(&[
            format!(
                "SELECT json_object('id', id, 'agent', agent, 'model', model) AS row \
                 FROM session WHERE parent_id = {id} ORDER BY time_created LIMIT \
                 {MAX_LINEAGE_CHILDREN}"
            ),
            format!(
                "SELECT json_object('id', id, 'model', model) AS row FROM session \
                 WHERE parent_id = {id} ORDER BY time_created LIMIT {MAX_LINEAGE_CHILDREN}"
            ),
        ])?;
        let mut notes = Vec::new();
        if rows.len() >= MAX_LINEAGE_CHILDREN {
            notes.push(format!(
                "The lineage read returned the first {MAX_LINEAGE_CHILDREN} child sessions; \
                 the store may hold more."
            ));
        }
        let children = rows
            .iter()
            .filter_map(|row| {
                let id = row["id"].as_str()?.to_owned();
                Some(ChildRef {
                    session_id: Some(id.clone()),
                    role: row["agent"].as_str().map(str::to_owned),
                    model: database_model(&row["model"]).map(|model| model.identity()),
                    resolved: true,
                    source: vec![SourceRef::Session { id: id.clone() }],
                    ..ChildRef::new(id, self.harness())
                })
            })
            .collect();

        Ok(Lineage {
            parent,
            children,
            forked_from: own["fork_session_id"].as_str().map(str::to_owned),
            notes,
            ..Lineage::default()
        })
    }

    /// The first of these queries the store answers. A column a store does
    /// not have is a fact about that store, not a failure of the read.
    fn first_supported(&self, queries: &[String]) -> Result<Vec<Value>> {
        let mut last = None;
        for query in queries {
            match self.database(query) {
                Ok(rows) => return Ok(rows),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("opencode database was not asked anything")))
    }

    /// The relationships the API records: a session names its parent, and the
    /// listing page names the sessions that carry this one as theirs.
    fn api_lineage(&self, session: &Session) -> Result<Lineage> {
        let response = self.request(&format!("/api/session/{}", session.id))?;
        let parent = response["data"]["parentID"]
            .as_str()
            .map(|native_id| -> Result<ParentRef> {
                Ok(ParentRef {
                    resolved: self.locate(native_id)?.is_some(),
                    native_id: native_id.to_owned(),
                    source: "session.parentID".to_owned(),
                })
            })
            .transpose()?;

        let page = self.request(&format!("/api/session?order=desc&limit={MAX_API_SESSIONS}"))?;
        let rows = page["data"]
            .as_array()
            .ok_or_else(|| anyhow!("opencode session response has no data array"))?;
        let children = rows
            .iter()
            .filter(|row| row["parentID"].as_str() == Some(session.id.as_str()))
            .filter_map(|row| {
                let id = row["id"].as_str()?.to_owned();
                Some(ChildRef {
                    session_id: Some(id.clone()),
                    role: row["agent"].as_str().map(str::to_owned),
                    model: row["model"]["id"].as_str().map(str::to_owned),
                    resolved: true,
                    source: vec![SourceRef::Session { id: id.clone() }],
                    ..ChildRef::new(id, self.harness())
                })
            })
            .collect();
        let mut notes = Vec::new();
        if rows.len() >= MAX_API_SESSIONS {
            notes.push(format!(
                "The lineage read looked for children in the newest {MAX_API_SESSIONS} sessions \
                 the API returned; an older child was not looked for."
            ));
        }
        Ok(Lineage {
            parent,
            children,
            notes,
            ..Lineage::default()
        })
    }

    /// The coordinate a consumer writes down beside the session id: the
    /// database file for the stable store, the program and endpoint for the
    /// API. Opaque by contract; only its stability matters.
    fn store_coordinate(&self, id: &str) -> String {
        if self.uses_database() {
            database_path("opencode.db").map_or_else(
                || format!("{} db", self.program.to_string_lossy()),
                |path| path.display().to_string(),
            )
        } else {
            format!("{}:/api/session/{id}", self.program.to_string_lossy())
        }
    }

    fn database_transcript(&self, session: Session, tail: usize) -> Result<Transcript> {
        let id = sql_literal(&session.id);
        let mut source = Vec::new();
        let mut message_rows = self.database(&database_message_query(&id))?;
        if message_rows.len() > MAX_DB_MESSAGES {
            message_rows.truncate(MAX_DB_MESSAGES);
            source.push(SourceBound::RecordPage {
                records: MAX_DB_MESSAGES,
                of: "messages".to_owned(),
            });
        }

        let mut part_rows = self.database_message_parts(
            &id,
            &message_rows,
            RowOrder::NewestFirst,
            MAX_DB_PARTS + 1,
        )?;
        if part_rows.len() > MAX_DB_PARTS {
            part_rows.truncate(MAX_DB_PARTS);
            source.push(SourceBound::RecordPage {
                records: MAX_DB_PARTS,
                of: "parts".to_owned(),
            });
        }
        part_rows.reverse();
        let parts = group_parts(part_rows)?;

        let mut cuts = TextCuts::default();
        cuts.count(&message_rows, &parts);
        source.extend(cuts.bounds());

        let messages = database_messages(&message_rows, parts)?;
        Ok(parse_transcript(
            session,
            &messages,
            tail,
            Vec::new(),
            |returned, total| Truncation {
                window: Truncation::window(returned, total, tail),
                source,
            },
        ))
    }

    /// Every message of a database session, oldest first, a page at a time.
    /// The read stops at the newest message present when it opened, so a
    /// message written while it streams belongs to the next read. Part text
    /// keeps the projection's cuts, reported as source bounds.
    fn database_stream(
        &self,
        session: &Session,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let id = sql_literal(&session.id);
        let newest = self.database(&format!(
            "SELECT json_object('time_created', time_created, 'id', id) AS row FROM message \
             WHERE session_id = {id} ORDER BY time_created DESC, id DESC LIMIT 1"
        ))?;
        let mut cuts = TextCuts::default();
        let mut read = 0;
        if let Some(newest) = newest.first() {
            let newest = RowKey::from_row(newest)?;
            let mut after = None;
            loop {
                let rows = self.database(&format!(
                    "SELECT {DATABASE_MESSAGE_ROW} FROM message WHERE session_id = {id} \
                     AND {}{} ORDER BY time_created, id LIMIT {MESSAGE_PAGE}",
                    newest.at_or_before(),
                    after
                        .as_ref()
                        .map_or_else(String::new, |key: &RowKey| format!(" AND {}", key.after())),
                ))?;
                let Some(last) = rows.last() else {
                    break;
                };
                after = Some(RowKey::from_row(last)?);
                let parts = group_parts(self.database_message_parts(
                    &id,
                    &rows,
                    RowOrder::OldestFirst,
                    usize::MAX,
                )?)?;
                cuts.count(&rows, &parts);
                read += rows.len() as u64;
                let whole_page = rows.len() == MESSAGE_PAGE;
                for message in database_messages(&rows, parts)? {
                    for parsed in parse_message(&message) {
                        turn(parsed)?;
                    }
                }
                if !whole_page {
                    break;
                }
            }
        }
        Ok(StreamedTranscript {
            coordinates: StreamCoordinates::OpenCodeMessages,
            source_length: read,
            source_bounds: cuts.bounds(),
            source_revision: None,
            skipped: 0,
            gaps: Vec::new(),
            trailing_record: None,
            terminal: None,
            notes: Vec::new(),
        })
    }

    /// The first `most` parts of these messages in `order`, fetched in pages
    /// the transport always carries.
    fn database_message_parts(
        &self,
        session: &str,
        messages: &[Value],
        order: RowOrder,
        most: usize,
    ) -> Result<Vec<Value>> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        let ids = messages
            .iter()
            .map(|row| required_string(row, "id"))
            .collect::<Result<Vec<_>>>()?;
        let ids = sql_id_list(&ids);
        let mut rows: Vec<Value> = Vec::new();
        while rows.len() < most {
            let limit = MAX_DB_PART_PAGE.min(most - rows.len());
            let past = match rows.last() {
                Some(last) => format!(" AND {}", order.past(&RowKey::from_row(last)?)),
                None => String::new(),
            };
            let page = self.database(&format!(
                "SELECT {} FROM part WHERE session_id = {session} AND message_id IN ({ids}){past} \
                 ORDER BY {} LIMIT {limit}",
                database_part_row(),
                order.sql(),
            ))?;
            let whole_page = page.len() == limit;
            rows.extend(page);
            if !whole_page {
                break;
            }
        }
        Ok(rows)
    }

    /// Every message the API projects, oldest first, a page at a time. The
    /// endpoint pages until it runs out, so a message written while the read
    /// streams may be part of it. A message too large for the transport
    /// cannot be stepped over, so the read refuses there.
    fn api_stream(
        &self,
        session: &Session,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let request = |path: &str| self.request(path);
        let mut pager = MessagePager::new(&request, &session.id, "asc", MESSAGE_PAGE);
        let mut read = 0;
        while !pager.exhausted {
            let page = match pager.next_page(MESSAGE_PAGE) {
                Ok(page) => page,
                Err(error) if error.downcast_ref::<ResponseTooLarge>().is_some() => {
                    return Err(anyhow!(
                        "message {} of opencode session {} is larger than the {} transport \
                         bound, so the session cannot be read whole; show without --full reads \
                         the messages newer than it",
                        read + 1,
                        session.id,
                        human_bytes(MAX_COMMAND_BYTES)
                    ));
                }
                Err(error) => return Err(error),
            };
            read += page.len() as u64;
            for message in &page {
                for parsed in parse_message(message) {
                    turn(parsed)?;
                }
            }
        }
        Ok(StreamedTranscript {
            coordinates: StreamCoordinates::OpenCodeMessages,
            source_length: read,
            source_bounds: Vec::new(),
            source_revision: None,
            skipped: 0,
            gaps: Vec::new(),
            trailing_record: None,
            terminal: None,
            notes: Vec::new(),
        })
    }

    /// The session this store records under `id`, where it holds one.
    fn locate_in_store(&self, id: &str) -> Result<Option<Session>> {
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
        let mut session = parse_session(data)?;
        session.source.location = Some(SourceLocation {
            locator: self.store_coordinate(&session.id),
            member: None,
        });
        Ok(Some(session))
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

    fn filter_sessions(&self, query: &Query, sessions: Vec<Session>) -> Listing {
        let scanned = sessions.len();
        let sessions = sessions
            .into_iter()
            .map(|mut session| {
                session.source.location = Some(SourceLocation {
                    locator: self.store_coordinate(&session.id),
                    member: None,
                });
                session
            })
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
        Listing {
            sessions,
            scanned,
            scan_truncated: scanned >= MAX_API_SESSIONS,
            ..Listing::default()
        }
    }

    fn api_listing(&self, query: &Query, client: &OpenCodeApiClient) -> Result<Listing> {
        let candidate_limit = if query.scope.is_some() || query.has_filters() {
            MAX_API_SESSIONS
        } else {
            query.limit
        };
        Ok(self.filter_sessions(
            query,
            client.sessions(candidate_limit.min(MAX_API_SESSIONS))?,
        ))
    }

    fn filter_v2_search(
        &self,
        mut listing: Listing,
        needle: &str,
        tail: usize,
        client: Option<&OpenCodeApiClient>,
    ) -> Listing {
        let prefilter_error = match self.v2_database_search_ids(needle, &listing.sessions) {
            Ok(Some(ids)) => {
                listing.sessions.retain(|session| ids.contains(&session.id));
                None
            }
            Ok(None) => None,
            Err(error) => Some(error),
        };
        let mut searched = match client {
            Some(client) => filter_listing_search_api(listing, client, needle, tail),
            None => filter_listing_search_parallel(self, listing, needle, tail),
        };
        if let Some(error) = prefilter_error {
            searched.unsearched.push(format!(
                "opencode v2 search prefilter failed: {error:#}; searched without prefilter"
            ));
        }
        searched
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
        let page = self.session_listing(candidate_limit)?;
        let unavailable = page.unavailable;
        let unavailable_ids = page.unavailable_ids;
        let mut filtered = self.filter_sessions(query, page.sessions);
        filtered.scanned = page.scanned;
        filtered.scan_truncated = page.scanned >= MAX_API_SESSIONS;
        filtered.unavailable = unavailable;
        // The rows this store could not read keep their precedence: a filtered
        // page still says which ids it holds and could not normalize.
        filtered.unavailable_ids = unavailable_ids;
        Ok(filtered)
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        if !self.uses_database() {
            let api_fallback = match OpenCodeApiServer::start(&self.program) {
                Ok(server) => match self.api_listing(query, &server.client) {
                    Ok(listing) => {
                        return Ok(self.filter_v2_search(
                            listing,
                            needle,
                            tail,
                            Some(&server.client),
                        ));
                    }
                    Err(error) => Some(format!(
                        "opencode v2 search could not use the local API server while listing candidates: \
                         {error:#}; searched through the CLI instead"
                    )),
                },
                Err(error) => Some(format!(
                    "opencode v2 search could not use the local API server at startup: \
                     {error:#}; searched through the CLI instead"
                )),
            };
            let listing = match self.list(query) {
                Ok(listing) => listing,
                Err(error) => {
                    let mut unsearched = vec![format!(
                        "opencode v2 search could not list candidates: {error:#}"
                    )];
                    if let Some(api_fallback) = api_fallback {
                        unsearched.push(api_fallback);
                    }
                    return Ok(Listing {
                        unsearched,
                        ..Listing::default()
                    });
                }
            };
            let mut searched = self.filter_v2_search(listing, needle, tail, None);
            if let Some(api_fallback) = api_fallback {
                searched.unsearched.push(api_fallback);
            }
            return Ok(searched);
        }
        let listing = self.list(query)?;
        let ids = match self.database_search_ids(needle, &listing.sessions) {
            Ok(Some(ids)) => ids,
            Ok(None) => {
                return Ok(filter_listing_search(self, listing, needle, tail));
            }
            Err(error) => {
                let mut fallback = filter_listing_search(self, listing, needle, tail);
                fallback.unsearched.push(format!(
                    "opencode search prefilter failed: {error:#}; searched without prefilter"
                ));
                return Ok(fallback);
            }
        };
        let mut candidates = listing;
        candidates
            .sessions
            .retain(|session| ids.contains(&session.id));
        Ok(filter_listing_search(self, candidates, needle, tail))
    }

    /// One session GET rather than a listing page, so an exact id costs a
    /// single request regardless of how many sessions the store holds. A
    /// failure names the store it came from, because two OpenCode stores
    /// answer many of the same ids and resolution reports which one failed.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        // OpenCode ids are self-identifying. Rejecting a foreign shape here
        // avoids spawning OpenCode for every claude, codex, or pi lookup — each
        // spawn costs about a second.
        if !id.starts_with(SESSION_ID_PREFIX) {
            return Ok(None);
        }
        self.locate_in_store(id)
            .with_context(|| format!("opencode store {}", self.store_coordinate(id)))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        if self.uses_database() {
            return self.database_transcript(session.clone(), tail);
        }
        let pages = paged_messages(&|path| self.request(path), &session.id, tail)?;
        Ok(paged_transcript(session.clone(), &pages, tail))
    }

    fn stream_transcript(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        // OpenCode updates a message's rows and parts in place, so a second
        // read of the same messages can hand over different turns.
        if replay.is_some() {
            return Err(anyhow!(
                "opencode sessions cannot be read whole twice: their messages are updated in \
                 place, so a second read may not repeat the first"
            ));
        }
        if self.uses_database() {
            return self.database_stream(session, turn);
        }
        self.api_stream(session, turn)
    }

    fn lineage(&self, session: &Session) -> Result<Lineage> {
        if self.uses_database() {
            return self.database_lineage(session);
        }
        self.api_lineage(session)
    }

    /// OpenCode keeps a session's counters on its own row as a recorded total
    /// for the whole session, so the resolved session already states them.
    fn stream_session(&self, session: &Session, _read: &StreamedTranscript) -> Result<Session> {
        Ok(session.clone())
    }

    /// OpenCode records relationships on session rows rather than in the
    /// messages, so its lineage read reaches no transcript bound.
    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        self.lineage(session)
    }

    fn events(&self, session: &Session, tail: usize) -> Result<EventTranscript> {
        if self.uses_database() {
            return Ok(event::project(
                self.database_transcript(session.clone(), usize::MAX)?,
                tail,
            ));
        }
        let pages = paged_messages(&|path| self.request(path), &session.id, tail)?;
        Ok(event::project(
            paged_event_source(session.clone(), &pages, tail),
            tail,
        ))
    }
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn search_needle_literal(needle: &str) -> Option<String> {
    needle
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric())
        .then(|| sql_literal(&needle.to_lowercase()))
}

fn sql_id_list(ids: &[String]) -> String {
    ids.iter()
        .map(|id| sql_literal(id))
        .collect::<Vec<_>>()
        .join(", ")
}

fn database_path(name: &str) -> Option<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(data_home.join("opencode").join(name))
}

fn database_search_queries(needle: &str, candidate_ids: &[String]) -> Option<Vec<String>> {
    let needle = search_needle_literal(needle)?;
    Some(
        candidate_ids
            .chunks(MAX_DB_SEARCH_IDS)
            .map(|ids| database_search_query(&needle, ids))
            .collect(),
    )
}

fn database_search_query(needle: &str, candidate_ids: &[String]) -> String {
    let candidate_ids = sql_id_list(candidate_ids);
    format!(
        "SELECT json_object('id', session_id) AS row FROM ( \
             SELECT DISTINCT session_id FROM message \
             WHERE session_id IN ({candidate_ids}) AND json_valid(data) = 0 \
             UNION \
             SELECT DISTINCT session_id FROM part \
             WHERE session_id IN ({candidate_ids}) AND ({}))",
        json_search_predicate(needle)
    )
}

fn v2_database_search_query(needle: &str, candidate_ids: &[String]) -> String {
    let candidate_ids = sql_id_list(candidate_ids);
    format!(
        "SELECT id FROM ( \
             SELECT DISTINCT session_id AS id FROM session_message \
             WHERE session_id IN ({candidate_ids}) AND ({} OR length(CAST(data AS BLOB)) > {MAX_V2_PREFILTER_MESSAGE_BYTES}) \
             UNION \
             SELECT session_id AS id FROM session_message \
             WHERE session_id IN ({candidate_ids}) \
             GROUP BY session_id \
             HAVING sum(length(CAST(data AS BLOB))) > {MAX_COMMAND_BYTES})",
        json_search_predicate(needle)
    )
}

fn json_search_predicate(needle: &str) -> String {
    // `json_tree` sees escaped strings after JSON decoding. SQLite's `lower`
    // is ASCII-only; the replacements cover the non-ASCII code points whose
    // lowercase forms can expose the ASCII letters handled here.
    format!(
        "json_valid(data) = 0 \
         OR instr(lower(data), {needle}) > 0 \
         OR EXISTS ( \
             SELECT 1 FROM json_tree(CASE WHEN json_valid(data) = 1 THEN data ELSE '{{}}' END) \
             WHERE instr(lower(CAST(key AS TEXT)), {needle}) > 0 \
                OR instr(lower(replace(replace(CAST(value AS TEXT), char(8490), 'k'), char(304), 'i')), {needle}) > 0)"
    )
}

/// A message row as a transcript read consumes it: the `role` and `time` of
/// its data and never the raw data, which can outgrow a query response.
const DATABASE_MESSAGE_ROW: &str = "json_object( \
     'id', id, 'time_created', time_created, 'data', \
     json_object('role', json_extract(data, '$.role'), \
                 'time', json_extract(data, '$.time'))) AS row";

fn database_message_query(id: &str) -> String {
    format!(
        "SELECT {DATABASE_MESSAGE_ROW} FROM message WHERE session_id = {id} \
         ORDER BY time_created DESC, id DESC LIMIT {}",
        MAX_DB_MESSAGES + 1
    )
}

/// A part row cut to what a transcript read keeps: text and reasoning at
/// `MAX_DB_TEXT_CHARS`, each tool payload field at `MAX_DB_TOOL_CHARS`, with
/// `truncated` saying whether anything was cut.
fn database_part_row() -> String {
    format!(
        "json_object( \
             'id', id, 'message_id', message_id, 'time_created', time_created, 'data', \
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
             ELSE json_object('type', json_extract(data, '$.type')) END) AS row"
    )
}

/// The direction a database read pages rows in: `time_created, id`, the
/// order a transcript reads, or its reverse for a read that keeps the newest.
#[derive(Clone, Copy)]
enum RowOrder {
    OldestFirst,
    NewestFirst,
}

impl RowOrder {
    fn sql(self) -> &'static str {
        match self {
            Self::OldestFirst => "time_created, id",
            Self::NewestFirst => "time_created DESC, id DESC",
        }
    }

    /// The rows a page in this order reaches once it has passed `key`.
    fn past(self, key: &RowKey) -> String {
        match self {
            Self::OldestFirst => key.after(),
            Self::NewestFirst => key.before(),
        }
    }
}

/// A row's position in `time_created, id` order.
struct RowKey {
    time_created: i64,
    id: String,
}

impl RowKey {
    fn from_row(row: &Value) -> Result<Self> {
        Ok(Self {
            time_created: row["time_created"]
                .as_i64()
                .ok_or_else(|| anyhow!("opencode record has no time_created"))?,
            id: required_string(row, "id")?,
        })
    }

    fn after(&self) -> String {
        let (time, id) = (self.time_created, sql_literal(&self.id));
        format!("(time_created > {time} OR (time_created = {time} AND id > {id}))")
    }

    fn before(&self) -> String {
        let (time, id) = (self.time_created, sql_literal(&self.id));
        format!("(time_created < {time} OR (time_created = {time} AND id < {id}))")
    }

    fn at_or_before(&self) -> String {
        let (time, id) = (self.time_created, sql_literal(&self.id));
        format!("(time_created < {time} OR (time_created = {time} AND id <= {id}))")
    }
}

/// Part rows grouped by message id, each message's parts in row order.
fn group_parts(rows: Vec<Value>) -> Result<HashMap<String, Vec<Value>>> {
    let mut parts = HashMap::<String, Vec<Value>>::new();
    for row in rows {
        let message_id = required_string(&row, "message_id")?;
        parts
            .entry(message_id)
            .or_default()
            .push(database_data(&row)?);
    }
    Ok(parts)
}

/// The messages these rows name, in row order, each carrying its parts.
fn database_messages(
    message_rows: &[Value],
    mut parts: HashMap<String, Vec<Value>>,
) -> Result<Vec<Value>> {
    message_rows
        .iter()
        .filter_map(|row| {
            database_message(row, parts.remove(row["id"].as_str()?).unwrap_or_default())
        })
        .collect()
}

/// Normalized turns whose text the part projection cut. Text and tool parts
/// are cut at different lengths, so each bound is reported with the count of
/// turns it touched: a user message's text parts join into one turn, an
/// assistant message yields one turn per part.
#[derive(Default)]
struct TextCuts {
    text: usize,
    tool: usize,
}

impl TextCuts {
    fn count(&mut self, message_rows: &[Value], parts: &HashMap<String, Vec<Value>>) {
        for row in message_rows {
            let Ok(data) = database_data(row) else {
                continue;
            };
            let message_parts = row["id"]
                .as_str()
                .and_then(|id| parts.get(id))
                .map_or(&[][..], Vec::as_slice);
            let (text, tool) = cut_turn_counts(data["role"].as_str(), message_parts);
            self.text += text;
            self.tool += tool;
        }
    }

    fn bounds(&self) -> Vec<SourceBound> {
        [
            (self.text, MAX_DB_TEXT_CHARS),
            (self.tool, MAX_DB_TOOL_CHARS),
        ]
        .into_iter()
        .filter(|(turns, _)| *turns > 0)
        .map(|(turns, chars)| SourceBound::TurnText { turns, chars })
        .collect()
    }
}

fn database_data(row: &Value) -> Result<Value> {
    if row["data"].is_object() {
        return Ok(row["data"].clone());
    }
    let data = required_string(row, "data")?;
    serde_json::from_str(&data).context("opencode database record contains invalid JSON")
}

/// How many normalized turns the projection's text cut touched in one message,
/// as (text or reasoning turns, tool turns). A user message joins its text
/// parts into one turn, so any cut part there is one cut turn; an assistant
/// message yields one turn per part.
fn cut_turn_counts(role: Option<&str>, parts: &[Value]) -> (usize, usize) {
    let cut = |part: &Value| {
        part["truncated"].as_bool().unwrap_or(false) || part["truncated"].as_i64() == Some(1)
    };
    match role {
        Some("user") => {
            let any = parts.iter().any(|part| part["type"] == "text" && cut(part));
            (usize::from(any), 0)
        }
        Some("assistant") => {
            parts
                .iter()
                .filter(|part| cut(part))
                .fold((0, 0), |(text, tool), part| {
                    if part["type"] == "tool" {
                        (text, tool + 1)
                    } else {
                        (text + 1, tool)
                    }
                })
        }
        _ => (0, 0),
    }
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
            if let Some(end) = time.get("end").cloned() {
                time.entry("completed").or_insert(end);
            }
        }
    }
    Some(Ok(if role == "user" {
        json!({
            "type": "user",
            "id": row["id"],
            "time": { "created": message_time },
            "text": user_text
        })
    } else {
        json!({
            "type": "assistant",
            "id": row["id"],
            "time": { "created": message_time },
            "content": parts
        })
    }))
}

fn parse_database_session(value: &Value) -> Result<Session> {
    let id = required_string(value, "id")?;
    let started_at = epoch_millis(&value["time_created"]);
    let last_activity_at = epoch_millis(&value["time_updated"]);
    let model = database_model(&value["model"]);
    let tokens = database_tokens(value);
    let cost = value["cost"].as_f64().map(|usd| Cost { usd });
    let accounting = accounting_for(
        tokens.as_ref(),
        cost.as_ref(),
        AccountingBasis::RecordedTotal,
        AccountingCoverage::Session,
    );

    Ok(Session {
        id,
        source: SourceDescriptor::installed("opencode", "opencode-database"),
        metadata: None,
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
        cost,
        tokens,
        accounting,
        start_uncertain: false,
        occurrence: None,
        usage_detail: None,
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
    let mut totals = TokenTotals::default();
    totals.add(
        value["tokens_input"].as_u64(),
        value["tokens_output"].as_u64(),
        value["tokens_reasoning"].as_u64(),
        value["tokens_cache_read"].as_u64(),
        value["tokens_cache_write"].as_u64(),
    );
    totals.finish()
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
    let started_at = epoch_millis(&value["time"]["created"]);
    let last_activity_at = epoch_millis(&value["time"]["updated"]);
    let model = value["model"]["id"].as_str().map(|id| Model {
        id: id.to_owned(),
        variant: value["model"]["variant"].as_str().map(str::to_owned),
    });
    let tokens = api_tokens(value);
    let cost = value["cost"].as_f64().map(|usd| Cost { usd });
    let accounting = accounting_for(
        tokens.as_ref(),
        cost.as_ref(),
        AccountingBasis::RecordedTotal,
        AccountingCoverage::Session,
    );

    Ok(Session {
        id,
        source: SourceDescriptor::installed("opencode", "opencode-api"),
        metadata: None,
        model,
        title: value["title"].as_str().map(str::to_owned),
        derived_title: None,
        derived_title_truncated: None,
        directory: value["location"]["directory"].as_str().map(PathBuf::from),
        started_at,
        last_activity_at,
        live: None,
        cost,
        tokens,
        accounting,
        start_uncertain: false,
        occurrence: None,
        usage_detail: None,
    })
}

fn api_tokens(value: &Value) -> Option<Tokens> {
    let tokens = value["tokens"].as_object()?;
    let cache = tokens.get("cache").and_then(Value::as_object);
    let mut totals = TokenTotals::default();
    totals.add(
        tokens.get("input").and_then(Value::as_u64),
        tokens.get("output").and_then(Value::as_u64),
        tokens.get("reasoning").and_then(Value::as_u64),
        cache
            .and_then(|cache| cache.get("read"))
            .and_then(Value::as_u64),
        cache
            .and_then(|cache| cache.get("write"))
            .and_then(Value::as_u64),
    );
    totals.finish()
}

/// What a paged read handed over: newest-first messages, why it stopped, and
/// anything a reader should know that has no field.
struct MessagePages {
    messages: Vec<Value>,
    stop: PageStop,
    notes: Vec<String>,
}

/// Why a paged read stopped. Only a ceiling is a source bound: a read that
/// stopped because the requested window was full is a window, and a wider
/// request fetches what it left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageStop {
    /// A page shorter than asked for: the store has nothing older.
    Exhausted,
    /// Enough turns for the requested window were in hand.
    WindowFull,
    /// The message ceiling, or a message the transport cannot carry.
    Ceiling,
}

/// One session's API message pages in one order. The endpoint answers every
/// page with a `cursor.next`, including the page after its last message, so a
/// page shorter than asked for is the only sign the store is exhausted; a
/// follow-up page carries the cursor and no `order`, which the endpoint
/// refuses to combine, and the cursor keeps the order it was issued in. A
/// page the transport cannot carry is retried one message at a time, and the
/// page size doubles back up while pages fit; every failed attempt costs a
/// full transport bound of transfer, so one retry at the floor beats a ladder
/// of them. A single message too large to carry fails with
/// [`ResponseTooLarge`], and the caller decides what that means.
struct MessagePager<'a> {
    request: &'a dyn Fn(&str) -> Result<Value>,
    id: &'a str,
    order: &'static str,
    page_size: usize,
    limit: usize,
    cursor: Option<String>,
    exhausted: bool,
}

impl<'a> MessagePager<'a> {
    fn new(
        request: &'a dyn Fn(&str) -> Result<Value>,
        id: &'a str,
        order: &'static str,
        page_size: usize,
    ) -> Self {
        Self {
            request,
            id,
            order,
            page_size,
            limit: page_size,
            cursor: None,
            exhausted: false,
        }
    }

    /// The next page, of at most `most` messages.
    fn next_page(&mut self, most: usize) -> Result<Vec<Value>> {
        let id = self.id;
        loop {
            let limit = self.limit.min(most);
            let path = match &self.cursor {
                None => format!(
                    "/api/session/{id}/message?limit={limit}&order={}",
                    self.order
                ),
                Some(cursor) => format!("/api/session/{id}/message?limit={limit}&cursor={cursor}"),
            };
            let mut page = match (self.request)(&path) {
                Ok(page) => page,
                Err(error) if limit > 1 && error.downcast_ref::<ResponseTooLarge>().is_some() => {
                    self.limit = 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let Some(Value::Array(data)) = page.get_mut("data").map(Value::take) else {
                return Err(match page["message"].as_str() {
                    Some(message) => anyhow!("opencode message request failed: {message}"),
                    None => anyhow!("opencode message response has no data array"),
                });
            };
            self.cursor = page["cursor"]["next"].as_str().map(str::to_owned);
            self.exhausted = data.len() < limit || self.cursor.is_none();
            self.limit = (self.limit * 2).min(self.page_size);
            return Ok(data);
        }
    }
}

/// Newest-first message pages, fetched until the requested number of turns is
/// in hand, the store runs out, or the message ceiling is reached. A single
/// message too large to carry is the store's own limit: the read stops before
/// it and says so, handing over the newer messages it has, unless that
/// message is the newest one and nothing is readable at all.
fn paged_messages(
    request: &dyn Fn(&str) -> Result<Value>,
    id: &str,
    tail: usize,
) -> Result<MessagePages> {
    let mut pager = MessagePager::new(
        request,
        id,
        "desc",
        tail.clamp(MIN_MESSAGE_PAGE, MESSAGE_PAGE),
    );
    let mut messages: Vec<Value> = Vec::new();
    let mut notes = Vec::new();
    let mut turns = 0;
    let stop = loop {
        if turns >= tail {
            break PageStop::WindowFull;
        }
        let remaining = MAX_API_MESSAGES - messages.len();
        if remaining == 0 {
            break PageStop::Ceiling;
        }
        let page = match pager.next_page(remaining) {
            Ok(page) => page,
            Err(error)
                if !messages.is_empty() && error.downcast_ref::<ResponseTooLarge>().is_some() =>
            {
                notes.push(format!(
                    "The message older than the {} fetched is larger than the {} transport \
                     bound; the read stopped before it.",
                    messages.len(),
                    human_bytes(MAX_COMMAND_BYTES)
                ));
                break PageStop::Ceiling;
            }
            Err(error) => return Err(error),
        };
        turns += page
            .iter()
            .map(|message| parse_message(message).len())
            .sum::<usize>();
        messages.extend(page);
        if pager.exhausted {
            break PageStop::Exhausted;
        }
    };
    Ok(MessagePages {
        messages,
        stop,
        notes,
    })
}

/// The truncation a paged read produced. A read that stopped with the window
/// full has an inexact window: the omitted count covers only what was fetched,
/// and a wider request fetches older messages. A read that hit a ceiling
/// reports the source bound.
fn paged_truncation(
    pages: &MessagePages,
    returned: usize,
    total: usize,
    tail: usize,
) -> Truncation {
    let mut window = Truncation::window(returned, total, tail);
    let mut source = Vec::new();
    match pages.stop {
        PageStop::Exhausted => {}
        PageStop::WindowFull => {
            let mut inexact = window.unwrap_or_else(|| TurnWindow::whole(returned, tail));
            inexact.omitted_exact = false;
            window = Some(inexact);
        }
        PageStop::Ceiling => source.push(SourceBound::RecordPage {
            records: pages.messages.len(),
            of: "messages".to_owned(),
        }),
    }
    Truncation { window, source }
}

/// A transcript from a paged API read, whose truncation depends on why the
/// paging stopped.
fn paged_transcript(session: Session, pages: &MessagePages, tail: usize) -> Transcript {
    parse_transcript(
        session,
        &pages.messages,
        tail,
        pages.notes.clone(),
        |returned, total| paged_truncation(pages, returned, total, tail),
    )
}

/// Keep every turn from the pages fetched for an event read. The event layer
/// pairs this sequence before applying the same window metadata as `show`.
fn paged_event_source(session: Session, pages: &MessagePages, tail: usize) -> Transcript {
    let turns = normalized_turns(&pages.messages);
    let returned = turns.len().min(tail);
    let truncation = paged_truncation(pages, returned, turns.len(), tail);
    Transcript::new(session, turns, truncation, None, pages.notes.clone())
}

/// Normalize newest-first messages into a chronological transcript. The
/// caller supplies the truncation from the returned and total turn counts,
/// since only it knows how the messages were read; `notes` is what the read
/// learned that has no field. The session endpoint is the normalized source
/// of metadata, so a message read does not invent a title that a title-less
/// listing could not provide.
fn parse_transcript(
    session: Session,
    messages: &[Value],
    tail: usize,
    notes: Vec<String>,
    truncation: impl FnOnce(usize, usize) -> Truncation,
) -> Transcript {
    let mut turns = normalized_turns(messages);
    let total = turns.len();
    if total > tail {
        turns.drain(..total - tail);
    }
    let truncation = truncation(turns.len(), total);

    Transcript::new(session, turns, truncation, None, notes)
}

fn normalized_turns(messages: &[Value]) -> Vec<Turn> {
    let mut turns = messages
        .iter()
        .rev()
        .flat_map(parse_message)
        .collect::<Vec<_>>();
    for (ordinal, turn) in turns.iter_mut().enumerate() {
        turn.ordinal = ordinal;
    }
    turns
}

/// OpenCode gives the harness's own messages parts of their own inside an
/// assistant message, so every message of type `user` is one the operator sent.
fn user_kind(role: &Role) -> TurnKind {
    role.kind().unwrap_or(TurnKind::Operator)
}

fn parse_message(message: &Value) -> Vec<Turn> {
    let Some(message_role) = message["type"].as_str() else {
        return Vec::new();
    };
    let role = match message_role {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "developer" => Role::Developer,
        _ => return Vec::new(),
    };
    let message_ts = epoch_millis(&message["time"]["created"]);
    let message_id = message["id"].as_str().map(str::to_owned);
    if role == Role::User {
        return message["text"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| Turn {
                kind: user_kind(&role),
                role,
                text: text.to_owned(),
                ts: message_ts,
                ordinal: 0,
                native_id: message_id,
                request_turn_id: None,
                metadata: None,
                record_ref: None,
                channel: None,
                recipient: None,
                parts: vec![text_part(text, "message.text", "text")],
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

    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| {
            let ts = epoch_millis(&part["time"]["created"]).or(message_ts);
            let native_kind = part["type"].as_str()?;
            let (role, text, tool, parts, coverage) = match native_kind {
                "text" => (
                    role.clone(),
                    part["text"].as_str()?.to_owned(),
                    None,
                    vec![text_part(
                        part["text"].as_str()?,
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
                "reasoning" => (
                    Role::Reasoning,
                    part["text"].as_str()?.to_owned(),
                    None,
                    vec![text_part(
                        part["text"].as_str()?,
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
                "tool" => (
                    Role::Tool,
                    part.to_string(),
                    Some(opencode_tool_event(part)),
                    vec![tool_part(part, "message.content", native_kind)],
                    tool_coverage(),
                ),
                _ => (
                    role.clone(),
                    String::new(),
                    None,
                    vec![ContentPart::Unknown {
                        native_kind: native_kind.to_owned(),
                        descriptor: bounded_shape(part),
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
            // A part names itself where the store keeps part ids; the
            // message id stands in where the projection carries none.
            let native_id = part["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| message_id.clone());
            ((!text.is_empty()) || !parts.is_empty()).then_some(Turn {
                kind: user_kind(&role),
                role,
                text,
                ts,
                ordinal: 0,
                native_id,
                request_turn_id: None,
                metadata: None,
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

fn opencode_tool_event(part: &Value) -> ToolEvent {
    let state = &part["state"];
    let status = state["status"].as_str().map(str::to_owned);
    let output = if status.as_deref() == Some("error") {
        ["error", "output", "content"]
            .into_iter()
            .find_map(|field| Bounded::from_value(&state[field]))
    } else {
        ["content", "output", "error"]
            .into_iter()
            .find_map(|field| Bounded::from_value(&state[field]))
    };
    ToolEvent {
        kind: EventKind::ToolCall,
        subtype: "tool".to_owned(),
        name: part["name"]
            .as_str()
            .or_else(|| part["tool"].as_str())
            .map(str::to_owned),
        call_id: part["callID"]
            .as_str()
            .or_else(|| part["id"].as_str())
            .map(str::to_owned),
        status,
        arguments: Bounded::from_value(&state["input"]),
        output,
        completed_ts: epoch_millis(&part["time"]["completed"]),
        invocations: crate::event::invocations_from_tool(
            part["name"].as_str().or_else(|| part["tool"].as_str()),
            &state["input"],
            "part.state.input",
        ),
        artifact_references: crate::event::artifact_references(&state["input"]),
        artifact_consumptions: Vec::new(),
    }
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
    fn v2_prefilter_searches_materialized_messages_without_limiting_matches() {
        let query = v2_database_search_query("'quota'", &["ses_fixture".to_owned()]);

        assert!(query.contains("FROM session_message"));
        assert!(query.contains("json_tree(CASE"));
        assert!(query.contains("json_valid(data) = 0"));
        assert!(query.contains("length(CAST(data AS BLOB)) > 65536"));
        assert!(!query.contains("FROM event"));
        assert!(!query.contains("LIMIT"));
    }

    #[test]
    fn cut_parts_are_counted_as_the_turns_they_become() {
        let cut_text = json!({"type": "text", "truncated": 1});
        let whole_text = json!({"type": "text", "truncated": 0});
        let cut_tool = json!({"type": "tool", "truncated": true});
        let cut_reasoning = json!({"type": "reasoning", "truncated": 1});

        // Two cut text parts of one user message join into one turn.
        assert_eq!(
            cut_turn_counts(Some("user"), &[cut_text.clone(), cut_text.clone()]),
            (1, 0)
        );
        assert_eq!(
            cut_turn_counts(Some("user"), std::slice::from_ref(&whole_text)),
            (0, 0)
        );
        // Each assistant part is its own turn, split by the bound that cut it.
        assert_eq!(
            cut_turn_counts(
                Some("assistant"),
                &[cut_text, whole_text, cut_tool, cut_reasoning]
            ),
            (2, 1)
        );
        assert_eq!(
            cut_turn_counts(Some("system"), &[json!({"type": "text", "truncated": 1})]),
            (0, 0)
        );
    }

    #[test]
    fn empty_sqlite_json_output_is_an_empty_result_set() {
        assert_eq!(parse_sqlite_json(b"").unwrap(), json!([]));
        assert_eq!(parse_sqlite_json(b"\n").unwrap(), json!([]));
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
    fn chunked_api_responses_are_decoded_before_json_parsing() {
        assert_eq!(
            decode_chunked_body(b"7\r\n{\"data\"\r\n5\r\n:[1]}\r\n0\r\n\r\n").unwrap(),
            br#"{"data":[1]}"#
        );
    }

    #[test]
    fn api_status_parser_accepts_success_and_rejects_malformed_headers() {
        assert_eq!(http_status(b"HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert_eq!(http_status(b"HTTP/1.1 204 No Content\r\n").unwrap(), 204);
        assert!(http_status(b"not http\r\n").is_err());
    }

    #[test]
    fn program_can_be_an_explicit_path() {
        let backend = OpenCodeBackend::new("/missing/opencode2");

        assert_eq!(backend.program, OsString::from("/missing/opencode2"));
        assert!(!backend.available());
    }
}
