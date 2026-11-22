//! T3 Code threads. T3 Code is a host that drives other harnesses: each of its
//! threads runs on a provider whose own store holds the recording. So a thread
//! is a name for a native session, and this module turns one into the other,
//! reading T3's database through the `sqlite3` program, read-only and bounded.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::backend::opencode::{
    bounded_stderr, finished_stderr, program_available, retry_transient, Attempt,
};
use crate::model::{Session, T3Thread};

/// Threads the index covers, newest first.
const INDEX_THREADS: usize = 10_000;
/// Provider threads one resolution considers.
const MAX_PROVIDER_THREADS: usize = 64;
/// Output of one resolution query.
const MAX_RESOLVE_BYTES: u64 = 8 * 1024 * 1024;
/// Output of the index query: its thread bound at a few kilobytes a row.
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;
/// The environment-id file is one line holding a UUID.
const MAX_ENVIRONMENT_BYTES: u64 = 4 * 1024;

const THREADS: &str = "orchestration_v2_projection_threads";
const PROVIDER_THREADS: &str = "orchestration_v2_projection_provider_threads";
const SUBAGENTS: &str = "orchestration_v2_projection_subagents";

/// SQL for one member of a row's `payload_json`, NULL when the payload is not
/// JSON, so one malformed row cannot fail a whole read.
fn payload_member(path: &str) -> String {
    format!("CASE WHEN json_valid(payload_json) THEN json_extract(payload_json, '{path}') END")
}

/// A T3 Code store: its base directory holds `userdata`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct T3Store {
    base: PathBuf,
}

/// A thread address: the environment it names, if any, and the thread id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct T3Link {
    pub environment_id: Option<String>,
    pub thread_id: String,
}

/// The native session a thread runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeThread {
    pub thread_id: String,
    pub title: String,
    pub harness: &'static str,
    pub native_id: String,
    /// Other native sessions of the thread, each as `<harness> <id>`.
    pub others: Vec<String>,
    pub deleted: bool,
}

/// What a store says about a thread id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The store holds no such thread.
    Absent,
    Native(NativeThread),
    /// The thread exists and has no session tapes can read; the text says why
    /// and names the thread.
    Refused(String),
}

/// Native sessions mapped to the newest T3 thread that ran them.
#[derive(Clone, Debug, Default)]
pub struct T3Index {
    threads: HashMap<(String, String), T3Thread>,
    total_threads: usize,
    thread_bound: usize,
    database: PathBuf,
}

impl T3Index {
    /// The native session ids the index maps, across harnesses.
    pub fn native_ids(&self) -> HashSet<String> {
        self.threads.keys().map(|(_, id)| id.clone()).collect()
    }

    /// Set each session's thread from the index.
    pub fn annotate(&self, sessions: &mut [Session]) {
        for session in sessions {
            session.t3 = self
                .threads
                .get(&(session.harness().to_owned(), session.id.clone()))
                .cloned();
        }
    }

    /// What the index left out, when its thread bound was reached.
    pub fn truncation(&self) -> Option<String> {
        (self.total_threads > self.thread_bound).then(|| {
            format!(
                "T3 store {}: only the newest {} of {} threads were read; older threads are not annotated",
                self.database.display(),
                self.thread_bound,
                self.total_threads
            )
        })
    }
}

impl T3Store {
    /// The store this process's environment names; `None` without a home
    /// directory to find it in.
    pub fn from_env() -> Option<Self> {
        agent_tapes_discovery::t3_home().map(Self::at)
    }

    pub fn at(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    fn state(&self) -> PathBuf {
        self.base.join("userdata")
    }

    pub fn database(&self) -> PathBuf {
        self.state().join("statev2.sqlite")
    }

    pub fn exists(&self) -> bool {
        self.database().is_file()
    }

    /// The environment this store serves; `None` when its file is absent or
    /// holds nothing.
    pub fn environment_id(&self) -> Option<String> {
        let mut text = String::new();
        File::open(self.state().join("environment-id"))
            .ok()?
            .take(MAX_ENVIRONMENT_BYTES)
            .read_to_string(&mut text)
            .ok()?;
        text.lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
    }

    /// Resolve a thread id, exactly. `Err` means the store could not be read.
    pub fn resolve(&self, thread_id: &str) -> Result<Resolution> {
        let (thread, providers, subagent) = self.thread_rows(thread_id)?;
        if thread.is_none() && subagent.is_none() {
            return Ok(Resolution::Absent);
        }
        if let Some(chosen) = choose(&providers, thread.as_ref()) {
            let others = others_of(&providers, chosen);
            let Some(harness) = harness_of(&chosen.driver) else {
                let also = if others.is_empty() {
                    String::new()
                } else {
                    format!("; it also ran on {}", others.join(", "))
                };
                return Ok(Resolution::Refused(format!(
                    "T3 thread {thread_id} runs on {}, which tapes cannot read{also}",
                    chosen.driver
                )));
            };
            let thread = thread.unwrap_or_default();
            return Ok(Resolution::Native(NativeThread {
                thread_id: thread_id.to_owned(),
                title: thread.title,
                harness,
                native_id: native_session_id(harness, &chosen.native_id),
                others,
                deleted: thread.deleted,
            }));
        }
        if let Some(subagent) = subagent.filter(|row| row.driver.as_deref() == Some("claudeAgent"))
        {
            if let Some(agent) = subagent.agent {
                return self.refuse_subagent(thread_id, &subagent.parent, &agent);
            }
        }
        Ok(Resolution::Refused(format!(
            "T3 thread {thread_id} has no native session yet: its run has not started"
        )))
    }

    fn refuse_subagent(&self, thread_id: &str, parent: &str, agent: &str) -> Result<Resolution> {
        let (thread, providers, _) = self.thread_rows(parent)?;
        let session = choose(&providers, thread.as_ref())
            .filter(|chosen| harness_of(&chosen.driver) == Some("claude"));
        Ok(Resolution::Refused(match session {
            Some(chosen) => format!(
                "T3 thread {thread_id} is a Claude subagent; read it with: tapes child {} --reference {agent}",
                chosen.native_id
            ),
            None => format!(
                "T3 thread {thread_id} is Claude subagent {agent} of T3 thread {parent}, which has no Claude session to read it through"
            ),
        }))
    }

    fn thread_rows(
        &self,
        thread_id: &str,
    ) -> Result<(Option<ThreadRow>, Vec<ProviderRow>, Option<SubagentRow>)> {
        let id = sql_literal(thread_id)?;
        let driver = payload_member("$.nativeThreadRef.driver");
        let native = payload_member("$.nativeThreadRef.nativeId");
        let agent = payload_member("$.nativeTaskRef.nativeId");
        let sql = format!(
            "SELECT \
             (SELECT json_object('title', title, 'active', active_provider_thread_id, 'deleted', deleted_at IS NOT NULL) \
              FROM {THREADS} WHERE thread_id = {id}) AS thread, \
             (SELECT json_group_array(json_object('id', provider_thread_id, 'ordinal', last_run_ordinal, 'driver', driver, 'native', native)) \
              FROM (SELECT provider_thread_id, last_run_ordinal, {driver} AS driver, {native} AS native \
                    FROM {PROVIDER_THREADS} WHERE thread_id = {id} \
                    ORDER BY last_run_ordinal DESC, provider_thread_id LIMIT {MAX_PROVIDER_THREADS})) AS providers, \
             (SELECT json_object('parent', thread_id, 'driver', driver, 'agent', {agent}) \
              FROM {SUBAGENTS} WHERE child_thread_id = {id} LIMIT 1) AS subagent"
        );
        let rows = self.query(&sql, MAX_RESOLVE_BYTES)?;
        let row = rows
            .first()
            .ok_or_else(|| anyhow!("the T3 store returned no row"))?;
        let thread = embedded(row, "thread")?.map(|value| ThreadRow {
            title: text(&value, "title").unwrap_or_default(),
            active: text(&value, "active"),
            deleted: value["deleted"].as_i64().is_some_and(|flag| flag != 0),
        });
        let providers = embedded(row, "providers")?
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(ProviderRow::from_value)
            .collect();
        let subagent = embedded(row, "subagent")?.and_then(|value| {
            Some(SubagentRow {
                parent: text(&value, "parent")?,
                driver: text(&value, "driver"),
                agent: text(&value, "agent"),
            })
        });
        Ok((thread, providers, subagent))
    }

    /// Native sessions of the newest threads.
    pub fn index(&self) -> Result<T3Index> {
        self.index_bounded(INDEX_THREADS)
    }

    fn index_bounded(&self, bound: usize) -> Result<T3Index> {
        let driver = payload_member("$.nativeThreadRef.driver");
        let native = payload_member("$.nativeThreadRef.nativeId");
        let sql = format!(
            "SELECT t.thread_id AS thread_id, t.title AS title, \
             (SELECT count(*) FROM {THREADS}) AS total, \
             {driver} AS driver, {native} AS native \
             FROM (SELECT thread_id, title, updated_at FROM {THREADS} \
                   ORDER BY updated_at DESC, thread_id LIMIT {bound}) AS t \
             LEFT JOIN {PROVIDER_THREADS} AS p ON p.thread_id = t.thread_id \
             ORDER BY t.updated_at DESC, t.thread_id"
        );
        let rows = self.query(&sql, MAX_INDEX_BYTES)?;
        let environment = self.environment_id();
        let mut index = T3Index {
            thread_bound: bound,
            database: self.database(),
            ..T3Index::default()
        };
        for row in &rows {
            index.total_threads = row["total"].as_u64().unwrap_or(0) as usize;
            let (Some(thread_id), Some(driver), Some(native)) = (
                text(row, "thread_id"),
                text(row, "driver"),
                text(row, "native"),
            ) else {
                continue;
            };
            let Some(harness) = harness_of(&driver) else {
                continue;
            };
            let key = (harness.to_owned(), native_session_id(harness, &native));
            index.threads.entry(key).or_insert_with(|| {
                T3Thread::new(
                    thread_id,
                    environment.clone(),
                    text(row, "title").unwrap_or_default(),
                )
            });
        }
        Ok(index)
    }

    /// Read failed or interrupted provider turns tied to one exact native ID.
    /// T3's errors are separate evidence from the ACP transcript.
    pub(crate) fn antigravity_turn_notes(&self, id: &str) -> Result<Vec<String>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let id = sql_literal(id)?;
        let native = payload_member("$.nativeThreadRef.nativeId");
        let driver = payload_member("$.nativeThreadRef.driver");
        let sql = format!(
            "WITH matched AS MATERIALIZED (
                SELECT thread_id, provider_thread_id FROM {PROVIDER_THREADS}
                WHERE {driver} = 'antigravity' AND {native} = {id} LIMIT 2
             )
             SELECT (SELECT count(*) FROM matched) AS bindings,
                    t.status, t.ordinal, coalesce(t.completed_at, t.started_at) AS at,
                    CASE WHEN json_valid(i.payload_json)
                      THEN substr(json_extract(i.payload_json, '$.failure.message'), 1, 2048) END AS message
             FROM matched AS p
             LEFT JOIN orchestration_v2_projection_provider_turns AS t
               ON t.provider_thread_id = p.provider_thread_id AND t.thread_id = p.thread_id
               AND t.status IN ('failed', 'interrupted', 'cancelled', 'canceled', 'aborted')
             LEFT JOIN orchestration_v2_projection_turn_items AS i
               ON i.provider_turn_id = t.provider_turn_id AND i.provider_thread_id = p.provider_thread_id
               AND i.thread_id = p.thread_id AND i.type = 'error'
             ORDER BY t.ordinal, i.ordinal LIMIT 129"
        );
        let rows = self.query(&sql, MAX_RESOLVE_BYTES)?;
        if rows.first().and_then(|row| row["bindings"].as_u64()) == Some(2) {
            anyhow::bail!("multiple T3 provider threads claim the same Antigravity session");
        }
        let mut notes = rows
            .iter()
            .take(128)
            .filter_map(|row| {
                let status = row["status"].as_str()?;
                let ordinal = row["ordinal"].as_u64()?;
                let at = row["at"].as_str().unwrap_or_default();
                let mut note = format!("T3 provider turn {ordinal} {status}");
                if !at.is_empty() {
                    note.push_str(&format!(" at {at}"));
                }
                if let Some(message) = row["message"]
                    .as_str()
                    .filter(|message| !message.is_empty())
                {
                    note.push_str(&format!(": {message}"));
                }
                Some(note)
            })
            .collect::<Vec<_>>();
        if rows.len() > 128 {
            notes.push("T3 failed or interrupted turn evidence exceeds 128 rows".to_owned());
        }
        Ok(notes)
    }

    fn query(&self, sql: &str, bound: u64) -> Result<Vec<Value>> {
        let database = self.database();
        if !program_available(OsStr::new("sqlite3")) {
            return Err(sqlite3_missing(&database));
        }
        let source = format!("T3 store {}", database.display());
        let bytes = retry_transient(&source, || sqlite_attempt(&database, sql, bound, &source))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Vec::new());
        }
        serde_json::from_slice(&bytes).with_context(|| format!("{source} returned invalid JSON"))
    }
}

fn sqlite3_missing(database: &Path) -> anyhow::Error {
    anyhow!(
        "reading the T3 store {} takes the sqlite3 program, which is not available",
        database.display()
    )
}

fn sqlite_attempt(database: &Path, sql: &str, bound: u64, source: &str) -> Result<Attempt> {
    let mut child = match Command::new("sqlite3")
        .args(["-readonly", "-batch", "-init", "/dev/null", "-json"])
        .arg(database)
        .arg(sql)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(sqlite3_missing(database)),
        Err(error) => return Err(error).context("failed to run sqlite3"),
    };
    let stderr = bounded_stderr(child.stderr.take().expect("piped stderr is present"));
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .expect("piped stdout is present")
        .take(bound + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {source}"))?;
    if bytes.len() as u64 > bound {
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!("{source} response exceeded {bound} bytes"));
    }
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for sqlite3 reading {source}"))?;
    if status.success() {
        return Ok(Attempt::Answered(bytes));
    }
    let stderr = finished_stderr(stderr);
    if let Some(cause) = agent_tapes_discovery::transient::busy_output(&stderr) {
        return Ok(Attempt::Busy(cause));
    }
    let said = String::from_utf8_lossy(&stderr);
    Err(anyhow!(
        "{source} could not be read: sqlite3 exited with {status}: {}",
        said.lines().next().unwrap_or_default()
    ))
}

#[derive(Default)]
struct ThreadRow {
    title: String,
    active: Option<String>,
    deleted: bool,
}

struct ProviderRow {
    id: String,
    driver: String,
    native_id: String,
}

impl ProviderRow {
    /// A provider thread with a native recording; `None` before its run
    /// starts or when its payload holds no usable reference.
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            id: text(value, "id")?,
            driver: text(value, "driver")?,
            native_id: text(value, "native")?,
        })
    }
}

struct SubagentRow {
    parent: String,
    driver: Option<String>,
    agent: Option<String>,
}

/// The active provider thread, else the one with the highest run ordinal,
/// among those that hold a native recording.
fn choose<'a>(providers: &'a [ProviderRow], thread: Option<&ThreadRow>) -> Option<&'a ProviderRow> {
    let active = thread.and_then(|thread| thread.active.as_deref());
    providers
        .iter()
        .find(|row| Some(row.id.as_str()) == active)
        .or_else(|| providers.first())
}

fn others_of(providers: &[ProviderRow], chosen: &ProviderRow) -> Vec<String> {
    providers
        .iter()
        .filter(|row| row.id != chosen.id)
        .map(|row| match harness_of(&row.driver) {
            Some(harness) => format!("{harness} {}", native_session_id(harness, &row.native_id)),
            None => format!("{} {}", row.driver, row.native_id),
        })
        .collect()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}

/// A column holding JSON text, parsed. A NULL column is `None`.
fn embedded(row: &Value, column: &str) -> Result<Option<Value>> {
    row[column]
        .as_str()
        .map(|cell| {
            serde_json::from_str(cell)
                .with_context(|| format!("the T3 store returned an invalid {column} column"))
        })
        .transpose()
}

/// The tapes harness that reads a T3 provider driver's recordings.
fn harness_of(driver: &str) -> Option<&'static str> {
    match driver {
        "antigravity" => Some("antigravity"),
        "codex" => Some("codex"),
        "claudeAgent" => Some("claude"),
        "pi" => Some("pi"),
        "opencode" => Some("opencode"),
        _ => None,
    }
}

/// The session id a driver's native reference names. Pi's reference is its
/// recording file, whose name carries the id.
fn native_session_id(harness: &str, native_id: &str) -> String {
    if harness == "pi" {
        if let Some(id) = agent_tapes_discovery::pi_recording_id(Path::new(native_id)) {
            return id.to_owned();
        }
    }
    native_id.to_owned()
}

/// Read an argument as a T3 link. `Ok(None)` means it is not written as a
/// link; `Err` means it is a link that cannot name a thread.
///
/// A web link percent-encodes each path segment, so segments decode exactly
/// once. An app link is not encoded, so its thread id is the rest of the
/// string verbatim, including any literal `%3A`.
pub fn parse_link(argument: &str) -> Result<Option<T3Link>> {
    let trimmed = argument.trim();
    let inner = trimmed
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .map_or(trimmed, str::trim);
    if let Some(rest) = strip_scheme(inner, "t3-thread://") {
        return parse_app_link(inner, rest).map(Some);
    }
    let web = strip_scheme(inner, "http://").or_else(|| strip_scheme(inner, "https://"));
    match web {
        Some(rest) => parse_web_link(inner, rest).map(Some),
        None => Ok(None),
    }
}

fn strip_scheme<'a>(text: &'a str, scheme: &str) -> Option<&'a str> {
    let head = text.get(..scheme.len())?;
    head.eq_ignore_ascii_case(scheme)
        .then(|| &text[scheme.len()..])
}

fn parse_app_link(link: &str, rest: &str) -> Result<T3Link> {
    let refused = || {
        anyhow!("{link} is not a T3 thread link: expected t3-thread://v2/<environment>/<thread> or t3-thread://v1/<thread>")
    };
    let (environment_id, thread_id) = if let Some(rest) = rest.strip_prefix("v2/") {
        let (environment, thread) = rest.split_once('/').ok_or_else(refused)?;
        (Some(environment), thread)
    } else if let Some(thread) = rest.strip_prefix("v1/") {
        (None, thread)
    } else {
        return Err(refused());
    };
    if thread_id.is_empty() || environment_id.is_some_and(str::is_empty) {
        return Err(refused());
    }
    Ok(T3Link {
        environment_id: environment_id.map(str::to_owned),
        thread_id: thread_id.to_owned(),
    })
}

fn parse_web_link(link: &str, rest: &str) -> Result<T3Link> {
    let refused =
        || anyhow!("{link} is not a T3 thread link: expected <origin>/<environment>/<thread>");
    let address = rest.split(['?', '#']).next().unwrap_or_default();
    let (authority, path) = address.split_once('/').ok_or_else(refused)?;
    if authority.is_empty() {
        return Err(refused());
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut segments = path.split('/');
    let (Some(environment), Some(thread), None) =
        (segments.next(), segments.next(), segments.next())
    else {
        return Err(refused());
    };
    if environment.is_empty() || thread.is_empty() {
        return Err(refused());
    }
    let decoded = |segment| {
        percent_decode(segment).ok_or_else(|| anyhow!("{link} has invalid percent-encoding"))
    };
    Ok(T3Link {
        environment_id: Some(decoded(environment)?),
        thread_id: decoded(thread)?,
    })
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let high = (*bytes.get(at + 1)? as char).to_digit(16)?;
            let low = (*bytes.get(at + 2)? as char).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// A SQL string literal: quotes doubled, control characters refused.
pub fn sql_literal(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(anyhow!("a T3 thread id cannot hold a control character"));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: &str = "11111111-2222-4333-8444-555555555555";

    fn link(environment: Option<&str>, thread: &str) -> T3Link {
        T3Link {
            environment_id: environment.map(str::to_owned),
            thread_id: thread.to_owned(),
        }
    }

    /// A web link is accepted from any origin, and what follows the path does
    /// not leak into the thread id.
    #[test]
    fn web_links_read_any_origin_and_ignore_what_follows_the_path() {
        for address in [
            format!("http://192.168.0.2:3773/{ENV}/t1"),
            format!("http://t3.example/{ENV}/t1"),
            format!("https://t3.example/{ENV}/t1"),
            format!("HTTPS://t3.example/{ENV}/t1"),
            format!("http://h:1/{ENV}/t1/"),
            format!("http://h:1/{ENV}/t1?x=/y"),
            format!("http://h:1/{ENV}/t1#node/2"),
            format!("<http://h:1/{ENV}/t1>"),
            format!(" < http://h:1/{ENV}/t1 > "),
        ] {
            assert_eq!(
                parse_link(&address).unwrap(),
                Some(link(Some(ENV), "t1")),
                "{address}"
            );
        }
    }

    /// A web path that is not exactly environment and thread names no thread;
    /// reading a longer one by its first segments would open the wrong thread.
    #[test]
    fn web_links_refuse_any_other_path_shape_and_bad_encoding() {
        for address in [
            "http://h/a/b/c",
            "http://h/a",
            "http://h/",
            "http://h",
            "http:///a/b",
            "http://h//b",
            "http://h/a/%ZZ",
            "http://h/a/b%2",
            "http://h/a/%FF",
        ] {
            assert!(parse_link(address).is_err(), "{address}");
        }
    }

    /// A delegated id holds a literal `%3A`: its web form encodes that as
    /// `%253A`, which one decode restores. Decoding twice would lose it.
    #[test]
    fn a_web_segment_decodes_exactly_once() {
        assert_eq!(
            parse_link("http://h/e/thread%3Ad%3Acommand%253Amcp").unwrap(),
            Some(link(Some("e"), "thread:d:command%3Amcp"))
        );
    }

    /// An app link is not encoded: decoding one would turn a delegated id's
    /// literal `%3A` into a colon and miss the thread.
    #[test]
    fn an_app_link_keeps_its_thread_id_verbatim() {
        assert_eq!(
            parse_link("t3-thread://v2/e/thread:d:command%3Amcp").unwrap(),
            Some(link(Some("e"), "thread:d:command%3Amcp"))
        );
        assert_eq!(
            parse_link("<t3-thread://v1/thread:a/b?c#d>").unwrap(),
            Some(link(None, "thread:a/b?c#d"))
        );
        for address in [
            "t3-thread://v3/e/t",
            "t3-thread://v2/e",
            "t3-thread://v2//t",
            "t3-thread://v2/e/",
            "t3-thread://v1/",
        ] {
            assert!(parse_link(address).is_err(), "{address}");
        }
    }

    /// An ordinary session id or prefix is not a link and is left for the
    /// bare-id lookup, so it can never fail as a malformed link.
    #[test]
    fn anything_else_is_not_a_link() {
        for argument in ["self", "0a1b2c3d", "thread:a%3Ab", "<id>", "http"] {
            assert_eq!(parse_link(argument).unwrap(), None, "{argument}");
        }
    }

    /// A quote cannot end the literal, and a control character cannot reach
    /// the program's argument.
    #[test]
    fn a_sql_literal_doubles_quotes_and_refuses_control_characters() {
        assert_eq!(sql_literal("a'b").unwrap(), "'a''b'");
        assert!(sql_literal("a\nb").is_err());
    }

    /// Past its bound the index says what it left out instead of implying the
    /// older threads have no sessions, and the newest thread wins a session.
    #[test]
    fn the_index_keeps_the_newest_thread_and_reports_its_bound() {
        if !program_available(OsStr::new("sqlite3")) {
            eprintln!("skipping T3 index test: sqlite3 is absent from PATH");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("userdata")).unwrap();
        let store = T3Store::at(root.path());
        let sql = format!(
            "CREATE TABLE {THREADS} (thread_id TEXT PRIMARY KEY, title TEXT, updated_at TEXT);
             CREATE TABLE {PROVIDER_THREADS} (thread_id TEXT, payload_json TEXT);
             INSERT INTO {THREADS} VALUES ('old', 'Old', '2026-01-01'), ('new', 'New', '2026-01-03'), ('mid', 'Mid', '2026-01-02');
             INSERT INTO {PROVIDER_THREADS} VALUES
               ('old', '{{\"nativeThreadRef\":{{\"driver\":\"codex\",\"nativeId\":\"s1\"}}}}'),
               ('new', '{{\"nativeThreadRef\":{{\"driver\":\"codex\",\"nativeId\":\"s1\"}}}}'),
               ('mid', 'not json'),
               ('mid', '{{\"nativeThreadRef\":{{\"driver\":\"codex\",\"nativeId\":\"s2\"}}}}');"
        );
        let created = Command::new("sqlite3")
            .arg(store.database())
            .arg(sql)
            .status()
            .unwrap();
        assert!(created.success());

        let whole = store.index_bounded(3).unwrap();
        assert_eq!(whole.truncation(), None);
        assert_eq!(
            whole.threads[&("codex".to_owned(), "s1".to_owned())].thread_id,
            "new"
        );
        assert_eq!(whole.native_ids().len(), 2);

        let bounded = store.index_bounded(2).unwrap();
        let note = bounded.truncation().unwrap();
        assert!(note.contains("newest 2 of 3"), "{note}");
    }
}
