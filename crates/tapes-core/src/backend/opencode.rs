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
    accounting_for, bounded_launch, filter_listing_search, filter_listing_search_parallel,
    merge_launch, recorded_total_coverage, search_turns, skipped_records_note,
    with_unreadable_records, Backend, Listing, Query, SearchCoverage, StreamCoordinates,
    StreamedTranscript, TokenTotals,
};
use crate::content::{
    bounded_shape, text_part, tool_coverage, tool_part, ContentAvailability, ContentCarrier,
    ContentCoverage, ContentPart,
};
use crate::event::{self, Bounded, EventKind, EventTranscript, ToolEvent};
use crate::lineage::{ChildRef, Lineage, ParentRef, SourceRef};
use crate::measurement::{known_subtotal_notes, MeasurementReason};
use crate::model::{
    human_bytes, AccountingBasis, AccountingCoverage, ByteSpan, Cost, CostCoverage,
    KindDeclaration, Launch, Model, ReadGap, RecordRef, Role, Session, SourceBound,
    SourceDescriptor, Tokens, Transcript, Truncation, Turn, TurnKind, TurnSelection, TurnWindow,
    UserDefault,
};
use crate::overlap::DeclaredOrigin;
use crate::span::{Placer, SpanBounds, SpanSessions};
use crate::transient::TransientFailure;
use agent_tapes_discovery::{
    DiscoveryError, Harness as NativeHarness, NativeSession, NativeStore, NativeStoreKind,
    OpenCodeFlavor,
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
/// SQL true for a row with a place in `time_created, id` order: an integer
/// creation time and a text id. A row without both cannot be the cursor a page
/// resumes from, so reads page only these rows and count the others apart.
const KEYED: &str = "typeof(time_created) = 'integer' AND typeof(id) = 'text'";
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

/// The leading characters of an OpenCode first prompt read for its launch
/// marker, whose line is shorter.
const FIRST_PROMPT_CHARS: usize = 256;
/// A v2 message this large may still make the API confirmation ambiguous if its
/// output is truncated by a client, so keep its session as a candidate.
const MAX_V2_PREFILTER_MESSAGE_BYTES: usize = 64 * 1024;
const API_SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const API_SERVER_RETRY_DELAY: Duration = Duration::from_millis(10);
const API_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
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
    fn start(store: &agent_tapes_discovery::OpenCodeStore) -> Result<Self> {
        let program = store.program();
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("failed to reserve a local port for the opencode API")?;
        let port = listener
            .local_addr()
            .context("failed to inspect the reserved opencode API port")?
            .port();
        drop(listener);

        let port_text = port.to_string();
        let mut command = Command::new(program);
        store.configure_command(&mut command);
        let mut child = command
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
    /// A response, requested again after a pause while the server answers
    /// that it is momentarily unavailable, up to the transient attempt bound.
    fn request(&self, path: &str) -> Result<Value> {
        retry_transient("opencode API", || self.request_attempt(path))
    }

    fn request_attempt(&self, path: &str) -> Result<Attempt<Value>> {
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
        if agent_tapes_discovery::transient::transient_http_status(status) {
            return Ok(Attempt::Busy(format!("HTTP {status}")));
        }
        if !(200..300).contains(&status) {
            return Err(anyhow!("opencode API server returned HTTP {status}"));
        }
        let body = if is_chunked(headers)? {
            decode_chunked_body(body)?
        } else {
            body.to_vec()
        };
        serde_json::from_slice(&body)
            .context("opencode API server returned invalid JSON")
            .map(Attempt::Answered)
    }

    fn search(&self, session: &Session, needle: &str, tail: usize) -> Result<SearchCoverage> {
        let pages = paged_messages(&|path| self.request(path), &session.id, tail)?;
        let transcript = paged_transcript(session.clone(), &pages, tail);
        Ok(search_turns(&transcript, needle, tail))
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
            Ok(coverage) => {
                if coverage.partial && !coverage.matched {
                    listing.partial_search.push(coverage.diagnostic(session));
                }
                if coverage.matched {
                    listing.sessions.push(session.clone());
                }
            }
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
    store: NativeStore,
}

impl OpenCodeBackend {
    pub fn new(program: impl Into<OsString>) -> Self {
        let program = program.into();
        Self {
            store: NativeStore::opencode(program.clone()),
            program,
        }
    }

    pub(crate) fn from_store(store: NativeStore) -> Self {
        let program = store
            .opencode_store()
            .expect("OpenCode backend requires an OpenCode native store")
            .program()
            .to_os_string();
        Self { program, store }
    }

    fn command_bytes(&self, args: &[&str], source: &str) -> Result<Vec<u8>> {
        self.command_bytes_with(&self.program, args, source)
    }

    /// A command's output, read again after a pause while it fails saying
    /// the store is busy, up to the transient attempt bound.
    fn command_bytes_with(&self, program: &OsStr, args: &[&str], source: &str) -> Result<Vec<u8>> {
        retry_transient(source, || self.command_attempt(program, args, source))
    }

    fn command_attempt(&self, program: &OsStr, args: &[&str], source: &str) -> Result<Attempt> {
        let store = self
            .store
            .opencode_store()
            .expect("OpenCode backend has an OpenCode store");
        let mut command = Command::new(program);
        store.configure_command(&mut command);
        let mut child = command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to run {}", program.to_string_lossy()))?;
        let stderr = bounded_stderr(child.stderr.take().expect("piped stderr is present"));
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
            let stderr = finished_stderr(stderr);
            if let Some(cause) = agent_tapes_discovery::transient::busy_output(&stderr) {
                return Ok(Attempt::Busy(cause));
            }
            return Err(anyhow!("{source} command exited with {status}"));
        }
        Ok(Attempt::Answered(bytes))
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
        parse_database_rows_lossy(&self.database_text(query)?)
    }

    /// A query's output with its header line, from sqlite3 in list mode where
    /// it can reach the database file, or from `opencode db` otherwise.
    fn database_text(&self, query: &str) -> Result<String> {
        if self.is_default_program("opencode") && program_available(OsStr::new("sqlite3")) {
            let database = self
                .store
                .opencode_store()
                .map(|store| store.sqlite_database_file("opencode.db"))
                .transpose()?
                .flatten();
            if let Some(database) = database {
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
                return String::from_utf8(bytes)
                    .context("opencode database returned non-UTF-8 output");
            }
        }
        let bytes = self.command_bytes(&["db", "--format", "tsv", query], "opencode database")?;
        String::from_utf8(bytes).context("opencode database returned non-UTF-8 output")
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
        let database = self
            .store
            .opencode_store()
            .map(|store| store.sqlite_database_file("opencode-next.db"))
            .transpose()?
            .flatten();
        let Some(database) = database else {
            return Ok(None);
        };
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

/// One attempt at a store read: its answer, or what the store said when it
/// was busy.
enum Attempt<T = Vec<u8>> {
    Answered(T),
    Busy(String),
}

/// Bytes of a failed command's stderr kept to classify its failure.
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// Drain a child's stderr on its own thread, keeping its first bytes, so the
/// child never blocks on a full pipe while its stdout is read.
fn bounded_stderr(mut stderr: std::process::ChildStderr) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut stderr)
            .take(MAX_STDERR_BYTES as u64)
            .read_to_end(&mut kept);
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        kept
    })
}

/// How long a failed command's stderr may stay open after it exited, held by
/// a process it started, before its failure is classified without it.
const STDERR_GRACE: Duration = Duration::from_secs(1);

/// The stderr [`bounded_stderr`] kept, once it reaches its end within
/// [`STDERR_GRACE`]; nothing otherwise.
fn finished_stderr(reader: thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    let deadline = Instant::now() + STDERR_GRACE;
    while !reader.is_finished() {
        if Instant::now() >= deadline {
            return Vec::new();
        }
        thread::sleep(Duration::from_millis(5));
    }
    reader.join().unwrap_or_default()
}

/// Run `attempt` until it answers, fails for good, or the store has stayed
/// busy through every bounded attempt, which fails as [`TransientFailure`].
fn retry_transient<T>(source: &str, mut attempt: impl FnMut() -> Result<Attempt<T>>) -> Result<T> {
    let attempts = agent_tapes_discovery::transient::ATTEMPTS;
    let mut made = 0;
    loop {
        made += 1;
        match attempt()? {
            Attempt::Answered(value) => return Ok(value),
            Attempt::Busy(cause) if made >= attempts => {
                return Err(TransientFailure {
                    source: source.to_owned(),
                    cause,
                }
                .into())
            }
            Attempt::Busy(_) => thread::sleep(agent_tapes_discovery::transient::retry_delay(made)),
        }
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
                error: error.to_string(),
            }),
        }
    }
    Ok(rows)
}

fn session_diagnostic(id: Option<&str>, error: String) -> String {
    match id {
        Some(id) => format!("opencode session {id}: {error}"),
        None => format!("opencode session with unknown id: {error}"),
    }
}

impl OpenCodeBackend {
    fn uses_database(&self) -> bool {
        matches!(
            self.store.kind(),
            NativeStoreKind::OpenCode(OpenCodeFlavor::Stable)
        )
    }

    fn parse_native_session(&self, native: &NativeSession) -> Result<Session> {
        let metadata = native
            .metadata()
            .ok_or_else(|| anyhow!("opencode session {} has no metadata row", native.id()))?;
        let metadata_id = required_string(metadata, "id")?;
        if metadata_id != native.id() {
            anyhow::bail!(
                "opencode metadata identity {metadata_id} disagrees with selected native identity {}",
                native.id()
            );
        }
        let session = match native.store_kind() {
            NativeStoreKind::OpenCode(OpenCodeFlavor::Stable) => parse_database_session(metadata)?,
            NativeStoreKind::OpenCode(OpenCodeFlavor::V2) => parse_session(metadata)?,
            _ => anyhow::bail!("opencode discovery returned a non-OpenCode store"),
        };
        super::enrich_native_session(native, session)
    }

    fn session_listing(&self, limit: usize, intentional_result_limit: bool) -> Result<Listing> {
        let page = self.store.candidates(limit.min(MAX_API_SESSIONS));
        let unreadable_ids = page.unreadable_ids.clone();
        if page.failures.len() > unreadable_ids.len() {
            anyhow::bail!(
                "opencode store {} could not be listed: {}",
                self.store.coordinate(),
                page.failures
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        let mut listing = Listing {
            scanned: page.scanned,
            scan_truncated: !page.complete && !intentional_result_limit,
            unavailable_ids: unreadable_ids.clone(),
            ..Listing::default()
        };
        for failure in page.failures {
            if matches!(failure, DiscoveryError::InvalidMetadata { .. })
                && !unreadable_ids.is_empty()
            {
                continue;
            }
            listing.unavailable.push(failure.to_string());
        }
        listing.unavailable.extend(unreadable_ids.iter().map(|id| {
            session_diagnostic(
                Some(id),
                "OpenCode returned malformed row metadata".to_owned(),
            )
        }));
        for native in page.records {
            match self.parse_native_session(&native) {
                Ok(session) => listing.sessions.push(session),
                Err(error) => {
                    listing.unavailable_ids.push(native.id().to_owned());
                    listing.unavailable.push(session_diagnostic(
                        Some(native.id()),
                        format!("metadata normalization failed: {error:#}"),
                    ));
                    listing.scan_truncated = true;
                }
            }
        }
        let unreadable = listing.scanned.saturating_sub(listing.sessions.len());
        if unreadable > 0 {
            listing.unavailable.insert(
                0,
                format!(
                    "opencode store {}: {unreadable} of {} session rows unreadable",
                    self.store.coordinate(),
                    listing.scanned
                ),
            );
        }
        Ok(listing)
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
                    resolved: self.store.locate_exact(native_id)?.is_some(),
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
        part_rows.extend(self.database_unkeyed_parts(&id, &message_rows)?);
        let parts = group_parts(part_rows);

        let mut cuts = TextCuts::default();
        cuts.count(&message_rows, &parts.by_message);
        source.extend(cuts.bounds());

        let unrecognized_usage = database_unread_usage(&message_rows);
        let unplaced = self.database_unkeyed_messages(&id)? + parts.unattributed;
        let (messages, unreadable) = database_messages(&message_rows, parts);
        let unreadable = unreadable.records + unplaced;
        Ok(parse_transcript(
            with_unreadable_records(
                with_unrecognized_usage(session, unrecognized_usage),
                unreadable,
            ),
            &messages,
            tail,
            skipped_records_note(unreadable).into_iter().collect(),
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
        item: &mut dyn FnMut(MessageItem<'_>) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let id = sql_literal(&session.id);
        let newest = self.database(&format!(
            "SELECT json_object('time_created', time_created, 'id', id) AS row FROM message \
             WHERE session_id = {id} AND {KEYED} ORDER BY time_created DESC, id DESC LIMIT 1"
        ))?;
        let mut cuts = TextCuts::default();
        let mut read = 0;
        let mut unrecognized_usage = 0;
        let mut gaps = Vec::new();
        let mut unplaced = 0;
        let mut skipped = 0;
        if let Some(newest) = newest.first() {
            let newest = RowKey::from_row(newest)?;
            let mut after = None;
            loop {
                let rows = self.database(&format!(
                    "SELECT {DATABASE_MESSAGE_ROW} FROM message WHERE session_id = {id} \
                     AND {KEYED} AND {}{} ORDER BY time_created, id LIMIT {MESSAGE_PAGE}",
                    newest.at_or_before(),
                    after
                        .as_ref()
                        .map_or_else(String::new, |key: &RowKey| format!(" AND {}", key.after())),
                ))?;
                let Some(last) = rows.last() else {
                    break;
                };
                after = Some(RowKey::from_row(last)?);
                let mut part_rows =
                    self.database_message_parts(&id, &rows, RowOrder::OldestFirst, usize::MAX)?;
                part_rows.extend(self.database_unkeyed_parts(&id, &rows)?);
                let parts = group_parts(part_rows);
                cuts.count(&rows, &parts.by_message);
                unrecognized_usage += database_unread_usage(&rows);
                unplaced += parts.unattributed;
                let (messages, unreadable) = database_messages(&rows, parts);
                for placed in ordered_items(&rows, &messages, &unreadable) {
                    item(placed)?;
                }
                skipped += unreadable.records;
                gaps.extend(unreadable.positions.into_iter().map(|row| ReadGap {
                    span: ByteSpan {
                        start: read + row as u64,
                        end: read + row as u64 + 1,
                    },
                    reason: "malformed-record".to_owned(),
                }));
                read += rows.len() as u64;
                let whole_page = rows.len() == MESSAGE_PAGE;
                for message in messages {
                    for parsed in message_turns(&message, session) {
                        turn(parsed)?;
                    }
                }
                if !whole_page {
                    break;
                }
            }
        }
        unplaced += self.database_unkeyed_messages(&id)?;
        for _ in 0..unplaced {
            item(MessageItem::Unreadable(None))?;
        }
        gaps.extend((0..unplaced as u64).map(|row| ReadGap {
            span: ByteSpan {
                start: read + row,
                end: read + row + 1,
            },
            reason: "malformed-record".to_owned(),
        }));
        read += unplaced as u64;
        skipped += unplaced;
        Ok(StreamedTranscript {
            coordinates: StreamCoordinates::OpenCodeMessages,
            source_length: read,
            source_bounds: cuts.bounds(),
            source_revision: None,
            source_prefix_sha256: None,
            skipped,
            gaps,
            trailing_record: None,
            terminal: None,
            notes: Vec::new(),
            unmapped: None,
            unrecognized_usage,
            kinds: None,
            folded: None,
            turn_times: Vec::new(),
            timeline_withheld: None,
            requests: None,
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
                "SELECT {} FROM part WHERE session_id = {session} AND message_id IN ({ids}) \
                 AND {KEYED}{past} ORDER BY {} LIMIT {limit}",
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

    /// How many message rows of this session have no place in order.
    fn database_unkeyed_messages(&self, session: &str) -> Result<usize> {
        let rows = self.database(&format!(
            "SELECT json_object('unkeyed', count(*)) AS row FROM message \
             WHERE session_id = {session} AND NOT ({KEYED})"
        ))?;
        Ok(rows
            .first()
            .and_then(|row| row["unkeyed"].as_u64())
            .unwrap_or(0) as usize)
    }

    /// One row for each message of `messages` that has part rows with no
    /// place in order, carrying its message id and how many such parts it
    /// has. Such a part cannot be read, so the row has no data.
    fn database_unkeyed_parts(&self, session: &str, messages: &[Value]) -> Result<Vec<Value>> {
        let ids = messages
            .iter()
            .filter_map(|row| row["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.database(&format!(
            "SELECT json_object('message_id', message_id, 'unkeyed', count(*)) AS row FROM part \
             WHERE session_id = {session} AND message_id IN ({}) AND NOT ({KEYED}) \
             GROUP BY message_id",
            sql_id_list(&ids)
        ))
    }

    /// Every message the API projects, oldest first, a page at a time. The
    /// endpoint pages until it runs out, so a message written while the read
    /// streams may be part of it. A message too large for the transport
    /// cannot be stepped over, so the read refuses there.
    fn api_stream(
        &self,
        session: &Session,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
        item: &mut dyn FnMut(MessageItem<'_>) -> Result<()>,
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
                for parsed in message_turns(message, session) {
                    turn(parsed)?;
                }
                item(MessageItem::Message(message))?;
            }
        }
        Ok(StreamedTranscript {
            coordinates: StreamCoordinates::OpenCodeMessages,
            source_length: read,
            source_bounds: Vec::new(),
            source_revision: None,
            source_prefix_sha256: None,
            skipped: 0,
            gaps: Vec::new(),
            trailing_record: None,
            terminal: None,
            notes: Vec::new(),
            unmapped: None,
            // The API projects counters on assistant messages only, which
            // the span reader counts.
            unrecognized_usage: 0,
            kinds: None,
            folded: None,
            turn_times: Vec::new(),
            timeline_withheld: None,
            requests: None,
        })
    }
}

fn program_available(program: &OsStr) -> bool {
    Path::new(program).is_file()
        || std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
        })
}

impl Default for OpenCodeBackend {
    fn default() -> Self {
        Self::from_store(
            super::default_native_store(NativeHarness::OpenCode)
                .unwrap_or_else(|| NativeStore::opencode_stable("opencode")),
        )
    }
}

impl OpenCodeBackend {
    fn filter_sessions(&self, query: &Query, mut listing: Listing) -> Listing {
        listing.sessions.retain(|session| {
            query.scope.is_none_or(|scope| {
                session
                    .directory
                    .as_deref()
                    .is_some_and(|directory| scope.contains(directory))
            }) && query.matches(session)
        });
        listing.sessions.truncate(query.limit);
        listing
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
        fold_api_refusals(&mut searched);
        searched
    }
}

impl Backend for OpenCodeBackend {
    fn store_label(&self) -> Option<&str> {
        self.store.store_label()
    }

    /// Stable OpenCode and opencode2 project many of the same sessions.
    fn shares_session_ids(&self) -> bool {
        true
    }

    fn store(&self) -> Option<String> {
        Some(self.store.coordinate())
    }

    fn kinds(&self) -> KindDeclaration {
        KindDeclaration {
            recordable: TurnSelection::only([
                TurnKind::Operator,
                TurnKind::Assistant,
                TurnKind::Reasoning,
                TurnKind::Tool,
            ]),
            user_default: Some(UserDefault {
                kind: TurnKind::Operator,
                basis: "OpenCode records a user message as the text parts the operator sent and writes no sender field beside them".to_owned(),
            }),
        }
    }

    fn harness(&self) -> &'static str {
        "opencode"
    }

    fn accounting_bases(&self) -> &'static [AccountingBasis] {
        &[AccountingBasis::RecordedTotal]
    }

    fn available(&self) -> bool {
        self.store.available()
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        // The backend pages globally and carries metadata for every session,
        // so scope and metadata filters are applied to a full page rather
        // than to the caller's limit. Otherwise matching sessions could fall
        // off the end of a page spent on other projects or models.
        if query.limit == 0 {
            return Ok(Listing::default());
        }
        let intentional_result_limit =
            query.scope.is_none() && !query.has_filters() && query.limit < MAX_API_SESSIONS;
        let candidate_limit = if !intentional_result_limit {
            MAX_API_SESSIONS
        } else {
            query.limit
        };
        Ok(self.filter_sessions(
            query,
            self.session_listing(candidate_limit, intentional_result_limit)?,
        ))
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        if !self.uses_database() {
            let store = self
                .store
                .opencode_store()
                .expect("OpenCode backend has an OpenCode store");
            let api_fallback = match OpenCodeApiServer::start(store) {
                Ok(server) => match self.list(query) {
                    Ok(listing) => {
                        let listing_failure = if listing.scan_truncated
                            || !listing.unavailable.is_empty()
                            || !listing.unavailable_ids.is_empty()
                        {
                            Some(format!(
                                "opencode v2 search could not list candidates: {}",
                                listing.unavailable.join("; ")
                            ))
                        } else {
                            None
                        };
                        let mut searched = self.filter_v2_search(
                            listing,
                            needle,
                            tail,
                            Some(&server.client),
                        );
                        if let Some(listing_failure) = listing_failure {
                            searched.unsearched.push(listing_failure);
                        }
                        return Ok(searched);
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
            let listing_failure = if listing.scan_truncated
                || !listing.unavailable.is_empty()
                || !listing.unavailable_ids.is_empty()
            {
                Some(format!(
                    "opencode v2 search could not list candidates: {}",
                    listing.unavailable.join("; ")
                ))
            } else {
                None
            };
            let mut searched = self.filter_v2_search(listing, needle, tail, None);
            if let Some(listing_failure) = listing_failure {
                searched.unsearched.push(listing_failure);
            }
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

    /// Resolve through the configured native store, then normalize its
    /// metadata row without fetching that row a second time.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let Some(native) = self
            .store
            .locate_exact(id)
            .with_context(|| format!("opencode store {}", self.store.coordinate()))?
        else {
            return Ok(None);
        };
        Ok(Some(self.parse_native_session(&native)?))
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        if self.uses_database() {
            return self.database_transcript(session.clone(), tail);
        }
        let pages = paged_messages(&|path| self.request(path), &session.id, tail)?;
        Ok(paged_transcript(session.clone(), &pages, tail))
    }

    fn supports_full_search(&self) -> bool {
        true
    }

    fn stream_transcript_with(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        options: super::StreamOptions,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        if replay.is_some() {
            self.replayable(session)?;
        }
        let mut turn_times = super::TurnStamps::new(options);
        let mut turn = super::timed(&mut turn_times, turn);
        // Its requests are counted as a span read places them.
        let mut placer = Placer::new(&[]).keeping(options.timeline);
        let mut count = |item: MessageItem<'_>| {
            if let MessageItem::Message(message) = item {
                place_message(message, &mut placer);
            }
            Ok(())
        };
        let mut read = if self.uses_database() {
            self.database_stream(session, &mut turn, &mut count)?
        } else {
            self.api_stream(session, &mut turn, &mut count)?
        };
        drop(turn);
        (read.turn_times, read.requests, read.timeline_withheld) =
            turn_times.finish(placer.finish().timeline());
        Ok(read)
    }

    /// The session table names a fork's source in `fork_session_id` and a
    /// child's parent in `parent_id`; the listing's metadata carries neither.
    /// One query per page of ids reads both, and a store without the fork
    /// column answers the shorter query.
    fn declared_origins(&self, sessions: &[&Session]) -> Result<Vec<Vec<DeclaredOrigin>>> {
        if !self.uses_database() {
            return Ok(vec![Vec::new(); sessions.len()]);
        }
        let mut declared: HashMap<String, Vec<DeclaredOrigin>> = HashMap::new();
        for page in sessions.chunks(MAX_DB_SEARCH_IDS) {
            let ids = sql_id_list(
                &page
                    .iter()
                    .map(|session| session.id.clone())
                    .collect::<Vec<_>>(),
            );
            let rows = self.first_supported(&[
                format!(
                    "SELECT json_object('id', id, 'parent_id', parent_id, \
                     'fork_session_id', fork_session_id) AS row FROM session \
                     WHERE id IN ({ids})"
                ),
                format!(
                    "SELECT json_object('id', id, 'parent_id', parent_id) AS row \
                     FROM session WHERE id IN ({ids})"
                ),
            ])?;
            for row in rows {
                let Some(id) = row["id"].as_str() else {
                    continue;
                };
                declared.insert(
                    id.to_owned(),
                    // A fork holds a copy of its source's history; a child
                    // only names the session that started it.
                    [("fork_session_id", true), ("parent_id", false)]
                        .iter()
                        .filter_map(|(column, copied)| {
                            row[*column].as_str().map(|id| DeclaredOrigin {
                                id: id.to_owned(),
                                copied: *copied,
                            })
                        })
                        .collect(),
                );
            }
        }
        Ok(sessions
            .iter()
            .map(|session| declared.remove(&session.id).unwrap_or_default())
            .collect())
    }

    /// OpenCode lists sessions without their messages, so the first prompt
    /// is asked for: the leading characters of the first user message's
    /// first text part, one database query per page of ids, or the API's
    /// first message in ascending order, one bounded request per session
    /// through one API server, the command where no server starts.
    fn first_prompt_markers(
        &self,
        sessions: &[&Session],
    ) -> Vec<Option<Result<crate::launch_parent::LaunchMarker>>> {
        use crate::launch_parent::LaunchMarker;
        let marker = |text: Option<&str>| {
            text.and_then(|text| LaunchMarker::of_first_prompt([text]))
                .unwrap_or(LaunchMarker::Absent)
        };
        if !self.uses_database() {
            if sessions.is_empty() {
                return Vec::new();
            }
            let first_message = |session: &Session| {
                let page = self.request(&format!(
                    "/api/session/{}/message?limit=1&order=asc",
                    session.id
                ));
                Some(page.map(|page| {
                    let first = &page["data"][0];
                    marker(
                        (first["type"] == "user")
                            .then(|| first["text"].as_str())
                            .flatten(),
                    )
                }))
            };
            let workers = thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get)
                .min(MAX_API_SEARCH_WORKERS)
                .min(sessions.len());
            return thread::scope(|scope| {
                sessions
                    .chunks(sessions.len().div_ceil(workers))
                    .map(|chunk| {
                        scope.spawn(|| {
                            chunk
                                .iter()
                                .map(|session| first_message(session))
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .flat_map(|handle| handle.join().expect("first prompt worker panicked"))
                    .collect()
            });
        }
        let mut prompts: HashMap<String, String> = HashMap::new();
        let mut failed: HashMap<String, String> = HashMap::new();
        for page in sessions.chunks(MAX_DB_SEARCH_IDS) {
            let ids = page
                .iter()
                .map(|session| session.id.clone())
                .collect::<Vec<_>>();
            let rows = self.database(&format!(
                "SELECT json_object('session_id', m.session_id, 'first_prompt', \
                 substr(json_extract(p.data, '$.text'), 1, {FIRST_PROMPT_CHARS})) AS row \
                 FROM message m JOIN part p ON p.message_id = m.id \
                 WHERE m.session_id IN ({}) AND m.id = (SELECT first.id FROM message first \
                 WHERE first.session_id = m.session_id \
                 AND json_extract(first.data, '$.role') = 'user' \
                 ORDER BY first.time_created, first.id LIMIT 1) \
                 AND json_extract(p.data, '$.type') = 'text' ORDER BY m.session_id, p.id",
                sql_id_list(&ids)
            ));
            match rows {
                Ok(rows) => {
                    for row in rows {
                        if let (Some(id), Some(text)) =
                            (row["session_id"].as_str(), row["first_prompt"].as_str())
                        {
                            prompts
                                .entry(id.to_owned())
                                .or_insert_with(|| text.to_owned());
                        }
                    }
                }
                Err(error) => failed.extend(ids.into_iter().map(|id| (id, format!("{error:#}")))),
            }
        }
        sessions
            .iter()
            .map(|session| {
                Some(match failed.get(&session.id) {
                    Some(error) => Err(anyhow!("{error}")),
                    None => Ok(marker(prompts.get(&session.id).map(String::as_str))),
                })
            })
            .collect()
    }

    fn lineage(&self, session: &Session) -> Result<Lineage> {
        if self.uses_database() {
            return self.database_lineage(session);
        }
        self.api_lineage(session)
    }

    /// OpenCode keeps a session's counters on its own row as a recorded total
    /// for the whole session, so the resolved session already states them.
    /// OpenCode updates a message's rows and parts in place, so a second read
    /// of the same messages can hand over different turns.
    fn replayable(&self, _session: &Session) -> Result<()> {
        Err(anyhow!(
            "opencode sessions cannot be read whole twice: their messages are updated in \
             place, so a second read may not repeat the first"
        ))
    }

    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        Ok(with_unreadable_records(
            with_unrecognized_usage(session.clone(), read.unrecognized_usage),
            read.skipped,
        ))
    }

    /// An OpenCode request is an assistant message, placed by its creation
    /// time and measured by the counters and cost it records. The session
    /// row's total covers requests no message records, so no span divides
    /// it: every figure, the whole included, sums messages. Messages are
    /// updated in place, so the span read is a read of its own.
    fn stream_spans(
        &self,
        session: &Session,
        read: &StreamedTranscript,
        spans: &[SpanBounds],
    ) -> Result<SpanSessions> {
        let mut placer = Placer::new(spans);
        let mut figures = (0..placer.figures())
            .map(|_| MessageSum::default())
            .collect::<Vec<_>>();
        let mut whole = MessageSum::default();
        let mut observe = |item: MessageItem<'_>| {
            let held = match item {
                MessageItem::Message(message) => {
                    let Some(held) = place_message(message, &mut placer) else {
                        return Ok(());
                    };
                    whole.add(message);
                    for index in &held {
                        figures[*index].add(message);
                    }
                    return Ok(());
                }
                MessageItem::Unrecognized(at) => {
                    whole.unrecognized_usage += 1;
                    placer.record(at, MeasurementReason::UnrecognizedUsage)
                }
                MessageItem::Unreadable(at) => {
                    whole.unreadable = true;
                    match at {
                        Some(at) => placer.record(Some(at), MeasurementReason::UnreadableRecord),
                        None => {
                            placer.unreadable();
                            Vec::new()
                        }
                    }
                }
            };
            for index in held {
                match item {
                    MessageItem::Unrecognized(_) => figures[index].unrecognized_usage += 1,
                    _ => figures[index].unreadable = true,
                }
            }
            Ok(())
        };
        let again = if self.uses_database() {
            self.database_stream(session, &mut |_| Ok(()), &mut observe)?
        } else {
            self.api_stream(session, &mut |_| Ok(()), &mut observe)?
        };
        let whole = whole.finish(session);
        let figures = figures
            .into_iter()
            .map(|figure| {
                let mut figure = figure.finish(session);
                crate::span::keep_identity(&mut figure, &whole);
                figure
            })
            .collect();
        let mut notes = vec![
            "OpenCode span figures sum assistant messages; the session's recorded total is \
             withheld, since it covers requests no message records."
                .to_owned(),
        ];
        if again.source_length != read.source_length {
            notes.push(format!(
                "The span read found {} message rows where the session read found {}; \
                 the session changed between them.",
                again.source_length, read.source_length
            ));
        }
        Ok(SpanSessions::assemble(
            placer.finish(),
            figures,
            whole,
            notes,
        ))
    }

    /// OpenCode records relationships on session rows rather than in the
    /// messages, so its lineage read reaches no transcript bound.
    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        self.lineage(session)
    }

    fn events_with_options(
        &self,
        session: &Session,
        tail: usize,
        options: event::EventOptions,
    ) -> Result<EventTranscript> {
        if self.uses_database() {
            return Ok(event::project_with(
                self.database_transcript(session.clone(), usize::MAX)?,
                tail,
                options,
            ));
        }
        let pages = paged_messages(&|path| self.request(path), &session.id, tail)?;
        Ok(event::project_with(
            paged_event_source(session.clone(), &pages, tail),
            tail,
            options,
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
/// its data, and whether it holds a token counter, never the raw data, which
/// can outgrow a query response. OpenCode's usage shape is a `tokens` object
/// of `input`, `output`, `reasoning`, and `cache.read` and `cache.write`.
const DATABASE_MESSAGE_ROW: &str = "json_object( \
     'id', id, 'time_created', time_created, 'data', \
     CASE WHEN json_valid(data) THEN json_object('role', json_extract(data, '$.role'), \
                 'time', json_extract(data, '$.time'), \
                 'tokens', json_extract(data, '$.tokens'), \
                 'cost', json_extract(data, '$.cost'), \
                 'modelID', json_extract(data, '$.modelID'), \
                 'variant', json_extract(data, '$.variant'), \
                 'usage_counters', \
                 json_type(data, '$.tokens.input') IN ('integer', 'real') \
                 OR json_type(data, '$.tokens.output') IN ('integer', 'real') \
                 OR json_type(data, '$.tokens.reasoning') IN ('integer', 'real') \
                 OR json_type(data, '$.tokens.cache.read') IN ('integer', 'real') \
                 OR json_type(data, '$.tokens.cache.write') IN ('integer', 'real')) END) AS row";

fn database_message_query(id: &str) -> String {
    format!(
        "SELECT {DATABASE_MESSAGE_ROW} FROM message WHERE session_id = {id} AND {KEYED} \
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
             CASE WHEN NOT json_valid(data) THEN NULL ELSE CASE json_extract(data, '$.type') \
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
             ELSE json_object('type', json_extract(data, '$.type')) END END) AS row"
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

/// Part rows grouped by message id.
#[derive(Default)]
struct GroupedParts {
    /// Each message's readable parts, in row order.
    by_message: HashMap<String, Vec<Value>>,
    /// How many unreadable part rows each message has.
    unreadable: HashMap<String, usize>,
    /// Part rows that name no message.
    unattributed: usize,
}

fn group_parts(rows: Vec<Value>) -> GroupedParts {
    let mut parts = GroupedParts::default();
    for row in rows {
        let Ok(message_id) = required_string(&row, "message_id") else {
            parts.unattributed += 1;
            continue;
        };
        match database_data(&row) {
            Ok(data) => parts.by_message.entry(message_id).or_default().push(data),
            Err(_) => {
                let count = row["unkeyed"].as_u64().map_or(1, |count| count as usize);
                *parts.unreadable.entry(message_id).or_default() += count;
            }
        }
    }
    parts
}

/// How many of these message rows carry OpenCode's usage shape on a message
/// whose role the reader has no reading for. The session's counters are the
/// session row's recorded total, so only a message outside the roles the
/// reader projects can hold consumption its figure does not show.
fn database_unread_usage(message_rows: &[Value]) -> usize {
    message_rows
        .iter()
        .filter_map(|row| database_data(row).ok())
        .filter(|data| !matches!(data["role"].as_str(), Some("user" | "assistant")))
        .filter(|data| data["usage_counters"] == true || data["usage_counters"] == 1)
        .count()
}

/// The session with `count` unrecognized usage-bearing records noted.
fn with_unrecognized_usage(mut session: Session, count: usize) -> Session {
    if count > 0 {
        session
            .usage_detail
            .get_or_insert_with(Default::default)
            .unrecognized_usage = count;
    }
    session
}

/// One message row as a span read meets it, in row order: a projected
/// message, a row whose counters no reading counts, or a record that could
/// not be read, each with the row's creation time where it has one.
#[derive(Clone, Copy)]
enum MessageItem<'a> {
    Message(&'a Value),
    Unrecognized(Option<chrono::DateTime<Utc>>),
    Unreadable(Option<chrono::DateTime<Utc>>),
}

/// The rows of one page as a span read meets them, in row order.
fn ordered_items<'a>(
    rows: &[Value],
    messages: &'a [Value],
    unreadable: &UnreadableRecords,
) -> Vec<MessageItem<'a>> {
    let mut messages = messages.iter().peekable();
    let mut items = Vec::new();
    for (position, row) in rows.iter().enumerate() {
        let at = epoch_millis(&row["time_created"]);
        if let Some(message) = messages.next_if(|message| message["id"] == row["id"]) {
            items.push(MessageItem::Message(message));
        } else if database_unread_usage(std::slice::from_ref(row)) > 0 {
            items.push(MessageItem::Unrecognized(at));
        }
        if unreadable.positions.contains(&position) {
            items.push(MessageItem::Unreadable(at));
        }
    }
    items
}

/// Assistant messages summed: each one counted request, its counters and
/// its written cost, a written zero beside usage unpriced.
#[derive(Default)]
struct MessageSum {
    tokens: TokenTotals,
    cost: f64,
    cost_measurement: crate::usage::MeasurementCoverage,
    zero_written: usize,
    unrecognized_usage: usize,
    unreadable: bool,
    model: Option<Model>,
    /// The model ids the summed messages name.
    models: HashSet<String>,
    /// Each summed message, kept for price-table terms priced by the
    /// request.
    uses: crate::pricing::RequestUses,
}

impl MessageSum {
    fn add(&mut self, message: &Value) {
        let tokens = api_tokens(message);
        let model = message["model"]["id"].as_str().map(|id| Model {
            id: id.to_owned(),
            variant: message["model"]["variant"].as_str().map(str::to_owned),
        });
        match (&tokens, crate::model::TokenConvention::of("opencode")) {
            (Some(tokens), Some(convention)) => {
                let disjoint = crate::model::DisjointTokens::from_native(tokens, convention, None);
                self.uses.push(
                    epoch_millis(&message["time"]["created"]),
                    model.as_ref(),
                    &disjoint,
                );
            }
            _ => self.uses.discard(),
        }
        let counter = |read: fn(&Tokens) -> Option<u64>| tokens.as_ref().and_then(read);
        self.tokens.add(
            counter(|tokens| tokens.input),
            counter(|tokens| tokens.output),
            counter(|tokens| tokens.reasoning),
            counter(|tokens| tokens.cache_read),
            counter(|tokens| tokens.cache_write),
        );
        match message["cost"].as_f64() {
            Some(usd) if Cost::from_written(usd, tokens.as_ref()).is_none() => {
                self.cost_measurement.unpriced += 1;
                self.zero_written += 1;
            }
            Some(usd) => {
                self.cost_measurement.measured += 1;
                self.cost += usd;
            }
            None => self.cost_measurement.missing += 1,
        }
        if let Some(model) = model {
            self.models.insert(model.id.clone());
            self.model = Some(model);
        }
    }

    fn finish(self, session: &Session) -> Session {
        let (tokens, mut measurement_coverage) = self.tokens.finish_covered();
        let counted = crate::usage::MeasurementCoverage::counted_any(&measurement_coverage);
        if counted {
            measurement_coverage.insert("cost".to_owned(), self.cost_measurement);
        }
        let cost = (self.cost_measurement.measured > 0).then_some(Cost { usd: self.cost });
        let cost_coverage = (self.cost_measurement.missing > 0
            || self.cost_measurement.unpriced > 0)
            .then_some(CostCoverage {
                complete: false,
                zero_written_requests: self.zero_written,
                zero_written_observations: self.zero_written,
            });
        let mut figure = session.clone();
        figure.accounting = accounting_for(
            tokens.as_ref(),
            cost.as_ref(),
            &measurement_coverage,
            AccountingBasis::SummedRequests,
            AccountingCoverage::Session,
        );
        figure.tokens = tokens;
        figure.cost = cost;
        figure.model = self.model;
        // Messages served by more than one model say so, and name no split.
        figure.model_observation =
            (self.models.len() > 1).then_some(crate::model::ModelObservationStatus {
                mixed: true,
                head_read: true,
                attribution_uncertain: true,
                distinct_observed: Some(self.models.len()),
            });
        let mut detail = crate::usage::UsageDetail {
            cost_coverage,
            measurement_incomplete: known_subtotal_notes(&measurement_coverage),
            measurement_coverage,
            unrecognized_usage: self.unrecognized_usage,
            request_uses: Some(self.uses).filter(|uses| !uses.is_empty()),
            ..crate::usage::UsageDetail::default()
        };
        if self.unreadable {
            detail
                .measurement_completeness
                .qualify_all(MeasurementReason::UnreadableRecord);
        }
        figure.usage_detail = detail.into_option();
        figure
    }
}

/// The unreadable records of some message rows: how many there are, and the
/// positions of the rows that hold them. A message whose data does not decode
/// is one record; each of its parts whose data does not decode is another.
#[derive(Default)]
struct UnreadableRecords {
    records: usize,
    positions: Vec<usize>,
}

/// The messages these rows name, in row order, each carrying its parts, and
/// their unreadable records.
fn database_messages(
    message_rows: &[Value],
    mut parts: GroupedParts,
) -> (Vec<Value>, UnreadableRecords) {
    let mut messages = Vec::new();
    let mut unreadable = UnreadableRecords::default();
    for (position, row) in message_rows.iter().enumerate() {
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        let readable = match database_message(row, parts.by_message.remove(id).unwrap_or_default())
        {
            Some(Ok(message)) => {
                messages.push(message);
                true
            }
            Some(Err(_)) => false,
            None => true,
        };
        let records = usize::from(!readable) + parts.unreadable.get(id).copied().unwrap_or(0);
        if records > 0 {
            unreadable.records += records;
            unreadable.positions.push(position);
        }
    }
    (messages, unreadable)
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
        let mut message = json!({
            "type": "assistant",
            "id": row["id"],
            "time": { "created": message_time },
            "content": parts
        });
        for field in ["tokens", "cost"] {
            if !data[field].is_null() {
                message[field] = data[field].clone();
            }
        }
        if let Some(model) = data["modelID"].as_str() {
            message["model"] = json!({ "id": model, "variant": data["variant"] });
        }
        message
    }))
}

/// Only the session named by the marker owns its process launch evidence.
fn opencode_launch(marker: &Value, id: &str) -> Option<Launch> {
    if marker["session"].as_str() != Some(id) {
        return None;
    }
    marker["modes"]
        .as_array()?
        .iter()
        .fold(None, |current, mode| {
            merge_launch(
                current,
                match mode.as_str() {
                    Some("tui") => Some(Launch::Interactive),
                    Some("run") => Some(Launch::Headless),
                    _ => None,
                },
            )
        })
}

fn parse_database_session(value: &Value) -> Result<Session> {
    let id = required_string(value, "id")?;
    let started_at = epoch_millis(&value["time_created"]);
    let last_activity_at = epoch_millis(&value["time_updated"]);
    let model = database_model(&value["model"]);
    let tokens = database_tokens(value);
    let cost = value["cost"]
        .as_f64()
        .and_then(|usd| Cost::from_written(usd, tokens.as_ref()));
    let cost_coverage = CostCoverage::written(value["cost"].as_f64(), tokens.as_ref());
    let measurement_coverage = recorded_total_coverage(tokens.as_ref());
    let accounting = accounting_for(
        tokens.as_ref(),
        cost.as_ref(),
        &measurement_coverage,
        AccountingBasis::RecordedTotal,
        AccountingCoverage::Session,
    );

    Ok(Session {
        id: id.clone(),
        source: SourceDescriptor::installed("opencode", "opencode-database"),
        metadata: None,
        model,
        model_observation: None,
        model_selections: Vec::new(),
        launch: opencode_launch(&value["tapes_launch"], &id),
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
        usage_detail: crate::usage::UsageDetail {
            cost_coverage,
            measurement_incomplete: known_subtotal_notes(&measurement_coverage),
            measurement_coverage,
            ..crate::usage::UsageDetail::default()
        }
        .into_option(),
        list_price_estimate: None,
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

fn parse_session(value: &Value) -> Result<Session> {
    let id = required_string(value, "id")?;
    let started_at = epoch_millis(&value["time"]["created"]);
    let last_activity_at = epoch_millis(&value["time"]["updated"]);
    let model = value["model"]["id"].as_str().map(|id| Model {
        id: id.to_owned(),
        variant: value["model"]["variant"].as_str().map(str::to_owned),
    });
    let tokens = api_tokens(value);
    let cost = value["cost"]
        .as_f64()
        .and_then(|usd| Cost::from_written(usd, tokens.as_ref()));
    let cost_coverage = CostCoverage::written(value["cost"].as_f64(), tokens.as_ref());
    let measurement_coverage = recorded_total_coverage(tokens.as_ref());
    let accounting = accounting_for(
        tokens.as_ref(),
        cost.as_ref(),
        &measurement_coverage,
        AccountingBasis::RecordedTotal,
        AccountingCoverage::Session,
    );

    Ok(Session {
        id: id.clone(),
        source: SourceDescriptor::installed("opencode", "opencode-api"),
        metadata: None,
        model,
        model_observation: None,
        model_selections: Vec::new(),
        launch: opencode_launch(&value["metadata"]["tapes.launch"], &id),
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
        usage_detail: crate::usage::UsageDetail {
            cost_coverage,
            measurement_incomplete: known_subtotal_notes(&measurement_coverage),
            measurement_coverage,
            ..crate::usage::UsageDetail::default()
        }
        .into_option(),
        list_price_estimate: None,
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
fn paged_event_source(mut session: Session, pages: &MessagePages, tail: usize) -> Transcript {
    let turns = normalized_turns(&session, &pages.messages);
    let returned = turns.len().min(tail);
    let truncation = paged_truncation(pages, returned, turns.len(), tail);
    session.launch = bounded_launch(session.launch, !truncation.is_empty());
    Transcript::new(session, turns, truncation, None, pages.notes.clone())
}

/// Normalize newest-first messages into a chronological transcript. The
/// caller supplies the truncation from the returned and total turn counts,
/// since only it knows how the messages were read; `notes` is what the read
/// learned that has no field. The session endpoint is the normalized source
/// of metadata, so a message read does not invent a title that a title-less
/// listing could not provide.
fn parse_transcript(
    mut session: Session,
    messages: &[Value],
    tail: usize,
    notes: Vec<String>,
    truncation: impl FnOnce(usize, usize) -> Truncation,
) -> Transcript {
    let mut turns = normalized_turns(&session, messages);
    let total = turns.len();
    if total > tail {
        turns.drain(..total - tail);
    }
    let truncation = truncation(turns.len(), total);
    session.launch = bounded_launch(session.launch, !truncation.is_empty());

    Transcript::new(session, turns, truncation, None, notes)
}

fn normalized_turns(session: &Session, messages: &[Value]) -> Vec<Turn> {
    let mut turns = messages
        .iter()
        .rev()
        .flat_map(|message| message_turns(message, session))
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

/// A message's turns, each carrying the message as its source record: the
/// message id names the record under the session's store location, and a
/// turn's position among the message's turns is its `part_index`. The parts
/// of one message are therefore one record however many turns they yield.
fn message_turns(message: &Value, session: &Session) -> Vec<Turn> {
    let mut turns = parse_message(message);
    let domain = session.locator().unwrap_or(&session.id);
    let record = message["id"].as_str().map(str::to_owned);
    for (part_index, turn) in turns.iter_mut().enumerate() {
        let reference = RecordRef {
            domain: domain.to_owned(),
            revision: None,
            span: None,
            native_id: record.clone(),
            pointer: None,
            part_index,
            content_part_index: None,
        };
        super::attach_record_ref(turn, reference);
    }
    turns
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
                origin: None,
                text_truncated: None,
                text_chars: None,
                context_event: None,
                model: None,
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
                origin: None,
                text_truncated: None,
                text_chars: None,
                context_event: None,
                model: None,
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
    let returned = if status.as_deref() == Some("error") {
        [&state["error"], &state["output"], &state["content"]]
    } else {
        [&state["content"], &state["output"], &state["error"]]
    }
    .into_iter()
    .find(|value| !value.is_null())
    .unwrap_or(&Value::Null);
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
        status: status.clone(),
        arguments: Bounded::retaining_value(&state["input"]),
        output,
        read: crate::event::direct_read(
            part["name"].as_str().or_else(|| part["tool"].as_str()),
            &state["input"],
        ),
        returned_read: Some(crate::event::returned_read(
            returned,
            status.as_deref().map(|status| status == "completed"),
        )),
        completed_ts: epoch_millis(&part["time"]["completed"]),
        invocations: crate::event::invocations_from_tool(
            part["name"].as_str().or_else(|| part["tool"].as_str()),
            &state["input"],
            "part.state.input",
        ),
        artifact_references: crate::event::artifact_references(&state["input"]),
        artifact_consumptions: Vec::new(),
        exit: None,
        self_contained: false,
    }
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value[field]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("opencode record has no {field}"))
}

/// An OpenCode request is an assistant message, placed by its creation
/// time; the figures that hold it, or nothing for any other message. The
/// span read and the transcript read count requests through here.
fn place_message(message: &Value, placer: &mut Placer) -> Option<Vec<usize>> {
    (message["type"] == "assistant").then(|| {
        placer.request(
            epoch_millis(&message["time"]["created"]),
            message["id"].as_str(),
        )
    })
}

fn epoch_millis(value: &Value) -> Option<chrono::DateTime<Utc>> {
    value
        .as_i64()
        .and_then(|millis| Utc.timestamp_millis_opt(millis).single())
}

/// An API refusal describes store access rather than individual sessions.
fn fold_api_refusals(listing: &mut Listing) {
    let mut reasons = std::collections::BTreeSet::new();
    listing.unsearched.retain(|diagnostic| {
        if let Some((_, reason)) = diagnostic.split_once(": ") {
            if reason.contains("API server returned HTTP ") {
                reasons.insert(reason.to_owned());
                return false;
            }
        }
        true
    });
    if !reasons.is_empty() {
        listing.unsearched.push(format!(
            "opencode store: {}",
            reasons.into_iter().collect::<Vec<_>>().join("; ")
        ));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn opencode_zero_written_cost_is_unpriced_in_both_projections() {
        let database =
            json!({"id": "ses_fixture", "cost": 0, "tokens_input": 1, "tokens_output": 2});
        let api = json!({"id": "ses_fixture", "cost": 0, "tokens": {"input": 1, "output": 2}});
        for session in [
            parse_database_session(&database).unwrap(),
            parse_session(&api).unwrap(),
        ] {
            assert!(session.cost.is_none());
            let view = crate::usage::usage(&Transcript::new(
                session,
                Vec::new(),
                Truncation::default(),
                None,
                Vec::new(),
            ));
            assert_eq!(
                serde_json::to_value(view).unwrap()["cost_coverage"]["complete"],
                false
            );
        }
    }

    #[test]
    fn an_opencode_total_counts_each_counter_it_did_not_write_as_missing() {
        let database = json!({"id": "ses_fixture", "cost": 0.5, "tokens_input": 1,
            "tokens_output": 2, "tokens_cache_read": 0});
        let api = json!({"id": "ses_fixture", "cost": 0.5,
            "tokens": {"input": 1, "output": 2, "cache": {"read": 0}}});
        for session in [
            parse_database_session(&database).unwrap(),
            parse_session(&api).unwrap(),
        ] {
            let view = serde_json::to_value(crate::usage::usage(&Transcript::new(
                session,
                Vec::new(),
                Truncation::default(),
                None,
                Vec::new(),
            )))
            .unwrap();
            for (counter, measured, missing) in [
                ("input", 1, 0),
                ("output", 1, 0),
                ("cache_read", 1, 0),
                ("reasoning", 0, 1),
                ("cache_write", 0, 1),
            ] {
                assert_eq!(
                    view["measurement_coverage"][counter],
                    json!({"measured": measured, "missing": missing, "unpriced": 0}),
                    "{counter}: {view}"
                );
                assert_eq!(
                    view["measurement_completeness"][counter] == json!(["missing-counter"]),
                    missing > 0,
                    "{counter}: {view}"
                );
            }
            // A counter written as zero is measured; the unwritten cache
            // write leaves the total unestablished.
            assert!(view["disjoint"]["total"].is_null(), "{view}");
        }
    }

    #[test]
    fn launch_metadata_is_session_bound_and_interactive_wins() {
        let rows: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/opencode/database-launch.json"
        ))
        .unwrap();
        for row in rows.as_array().unwrap() {
            let session = parse_database_session(row).unwrap();
            let expected = match row["expected"].as_str() {
                Some("interactive") => Some(Launch::Interactive),
                Some("headless") => Some(Launch::Headless),
                _ => None,
            };
            assert_eq!(session.launch, expected);
            let mut api = row.clone();
            api["metadata"] = json!({"tapes.launch": row["tapes_launch"]});
            assert_eq!(parse_session(&api).unwrap().launch, expected);
            let bounded =
                parse_transcript(session.clone(), &[], 0, Vec::new(), |_, _| Truncation {
                    window: None,
                    source: vec![SourceBound::RecordPage {
                        records: 1,
                        of: "messages".to_owned(),
                    }],
                });
            assert_eq!(
                bounded.session.launch,
                if expected == Some(Launch::Headless) {
                    None
                } else {
                    expected
                }
            );
            assert_eq!(
                parse_transcript(session, &[], 0, Vec::new(), |_, _| Truncation::default())
                    .session
                    .launch,
                expected
            );
        }
    }

    use super::*;

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
        assert!(query.contains(KEYED));
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
