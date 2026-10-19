//! A window read applies every selection filter to the sessions it reaches
//! through their children, a pool admits only the sessions whose recorded
//! limits include its own, and a whole read keeps a request timeline only
//! when asked, within a bound.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::backend::{StreamOptions, StreamedTranscript};
use agent_tapes_core::model::Session;
use agent_tapes_core::span::{MAX_TIMELINE_REQUESTS, TIMELINE_REQUESTS_WITHHELD};
use agent_tapes_core::usage::{self, GroupBy};
use agent_tapes_core::window::WindowBounds;
use agent_tapes_core::{usage_window_with_backends, ListFilters, SessionSelection, WindowAsk};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-review2-window-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn at(stamp: &str) -> DateTime<Utc> {
    stamp.parse().unwrap()
}

fn window_read(
    backend: Box<dyn Backend>,
    harness: &'static str,
    filters: ListFilters<'_>,
    from: &str,
    to: &str,
) -> Value {
    let selection = SessionSelection {
        harness: Some(harness),
        limit: Some(50),
        filters,
        ..Default::default()
    };
    let bounds = WindowBounds {
        from: Some(at(from)),
        to: Some(at(to)),
    };
    serde_json::to_value(
        usage_window_with_backends(
            &[backend],
            &selection,
            &[GroupBy::Session],
            WindowAsk::Bounds(bounds),
            None,
        )
        .unwrap(),
    )
    .unwrap()
}

fn shares(read: &Value) -> Vec<String> {
    let mut ids = read["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|share| share["id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

// --- A quiet Claude parent reached through its subagent --------------------

const PARENT: &str = "30000000-0000-4000-8000-0000000000f7";
const AGENT: &str = "a00000000000000f7";

fn claude_record(agent: Option<&str>, entry: &str, stamp: &str, text: &str) -> Value {
    let mut record = json!({
        "sessionId": PARENT, "cwd": "/fixtures/project", "entrypoint": "cli", "version": "2.1.0",
        "type": "assistant", "uuid": entry, "parentUuid": null, "requestId": format!("req_{entry}"),
        "timestamp": stamp,
        "message": {"id": format!("msg_{entry}"), "role": "assistant", "model": "claude-fixture",
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                      "output_tokens": 50},
            "stop_reason": "end_turn", "content": [{"type": "text", "text": text}]}
    });
    if let Some(agent) = agent {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(agent);
    }
    record
}

/// A parent whose own last record is at 09:59, before a 10:00 window, and a
/// subagent recording, written now, that spends at 10:05.
fn quiet_parent() -> Store {
    let store = Store::new("quiet-parent");
    store.write(
        &format!("project/{PARENT}.jsonl"),
        &[claude_record(
            None,
            "parent-1",
            "2026-01-12T09:59:00Z",
            "Trace the lighthouse fixture.",
        )],
    );
    store.write(
        &format!("project/{PARENT}/subagents/agent-{AGENT}.jsonl"),
        &[claude_record(
            Some(AGENT),
            "child-1",
            "2026-01-12T10:05:00Z",
            "Done.",
        )],
    );
    store
}

fn quiet_parent_read(store: &Store, filters: ListFilters<'_>) -> Vec<String> {
    shares(&window_read(
        Box::new(ClaudeBackend::new(&store.0)),
        "claude",
        filters,
        "2026-01-12T10:00:00Z",
        "2026-01-12T11:00:00Z",
    ))
}

/// Claim: a window read reaches a quiet parent's subagent only when the
/// subagent's recording was written since the caller's `--since`, as it
/// keeps a listed session only when its activity is since it, and since the
/// window opened. Rejects reaching parents through children by the window's
/// start alone, which answers a future `--since` with past consumption.
#[test]
fn a_parent_reached_through_children_honours_since() {
    let store = quiet_parent();
    let child = format!("{PARENT}::{AGENT}");
    assert_eq!(
        quiet_parent_read(&store, ListFilters::default()),
        vec![child.clone()]
    );
    let earlier = ListFilters {
        since: Some(at("2026-01-01T00:00:00Z")),
        ..ListFilters::default()
    };
    assert_eq!(quiet_parent_read(&store, earlier), vec![child]);
    let future = ListFilters {
        since: Some(at("2999-01-01T00:00:00Z")),
        ..ListFilters::default()
    };
    assert_eq!(quiet_parent_read(&store, future), Vec::<String>::new());
}

/// Claim: under `--search`, a quiet parent whose own turns match is reached
/// through its subagent, and one whose turns do not match is not, as a
/// listed session is kept or dropped by its own turns. Rejects skipping the
/// parents reached through children whenever a search is given.
#[test]
fn a_parent_reached_through_children_honours_search() {
    let store = quiet_parent();
    let matching = ListFilters {
        search: Some("LIGHTHOUSE"),
        ..ListFilters::default()
    };
    assert_eq!(
        quiet_parent_read(&store, matching),
        vec![format!("{PARENT}::{AGENT}")]
    );
    let missing = ListFilters {
        search: Some("harbour"),
        ..ListFilters::default()
    };
    assert_eq!(quiet_parent_read(&store, missing), Vec::<String>::new());
}

// --- Pool membership by recorded limit ------------------------------------

fn codex_usage(input: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
           "output_tokens": 0, "reasoning_output_tokens": 0, "total_tokens": input})
}

enum Event {
    /// The primary used percent the provider reported under a limit.
    Observed(&'static str, &'static str, f64),
    /// One request's uncached input.
    Request(&'static str, u64),
}

fn rollout(store: &Store, id: &str, events: &[Event]) {
    let mut lines = vec![
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "session_meta", "payload": {"id": id,
               "timestamp": "2026-01-10T09:00:00Z", "cwd": "/fixtures/project", "source": "exec",
               "creator_account_id": "account-a"}}),
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
    ];
    let mut total = 0;
    for event in events {
        match event {
            Event::Observed(stamp, limit, used) => lines.push(json!({
                "timestamp": stamp, "type": "event_msg",
                "payload": {"type": "token_count", "info": null, "rate_limits": {
                    "limit_id": limit, "plan_type": "plus",
                    "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_768_053_600},
                    "secondary": null}}
            })),
            Event::Request(stamp, input) => {
                total += input;
                lines.push(json!({
                    "timestamp": stamp, "type": "event_msg",
                    "payload": {"type": "token_count", "info": {"total_token_usage": codex_usage(total),
                        "last_token_usage": codex_usage(*input), "model_context_window": 272000},
                        "rate_limits": null}
                }));
            }
        }
    }
    store.write(
        &format!("2026/01/10/rollout-2026-01-10T09-00-00-{id}.jsonl"),
        &lines,
    );
}

/// Claim: a session whose recording observed only another quota limit, just
/// before the window opened, spends from no pool of this limit, so A's 3%
/// rise is A's alone while B spends 100 inside the window. Rejects deriving
/// a session's limits from the observations inside the window, which leaves
/// B none and admits it, giving B 0.75 and A 2.25.
#[test]
fn a_limit_observed_before_the_window_keeps_a_session_out_of_another_pool() {
    use Event::{Observed, Request};
    let store = Store::new("limits");
    let a = "40000000-0000-7000-8000-0000000000f7";
    let b = "40000000-0000-7000-8000-0000000000f8";
    rollout(
        &store,
        a,
        &[
            Observed("2026-01-10T10:00:00Z", "codex", 10.0),
            Request("2026-01-10T10:05:00Z", 300),
            Observed("2026-01-10T10:10:00Z", "codex", 13.0),
        ],
    );
    rollout(
        &store,
        b,
        &[
            Observed("2026-01-10T09:55:00Z", "premium", 40.0),
            Request("2026-01-10T10:06:00Z", 100),
        ],
    );
    let read = window_read(
        Box::new(CodexBackend::new(&store.0)),
        "codex",
        ListFilters::default(),
        "2026-01-10T10:00:00Z",
        "2026-01-10T12:00:00Z",
    );
    let estimate = |id: &str| {
        read["window"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|share| share["id"] == id)
            .unwrap_or_else(|| panic!("{id}'s share: {}", read["window"]))["allowance"][0]
            ["estimate"]
            .as_f64()
    };
    let a_share = estimate(a).unwrap();
    assert!((a_share - 3.0).abs() < 1e-9, "A's share: {a_share}");
    assert_eq!(estimate(b), None, "{}", read["window"]);
}

// --- The request timeline --------------------------------------------------

const CODEX_ID: &str = "50000000-0000-7000-8000-0000000000f7";

/// A Codex rollout of `requests` requests, each a token count moving the
/// cumulative total by one input token.
fn many_requests(store: &Store, requests: usize) {
    let mut lines = vec![
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "session_meta", "payload": {
               "id": CODEX_ID, "timestamp": "2026-01-10T09:00:00Z", "cwd": "/fixtures/project",
               "source": "exec"}}),
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
    ];
    for index in 1..=requests as u64 {
        lines.push(
            json!({"timestamp": "2026-01-10T10:00:00Z", "type": "event_msg",
            "payload": {"type": "token_count", "info": {"total_token_usage": codex_usage(index),
                "last_token_usage": codex_usage(1)}}}),
        );
    }
    store.write(
        &format!("2026/01/10/rollout-2026-01-10T09-00-00-{CODEX_ID}.jsonl"),
        &lines,
    );
}

fn codex_read(store: &Store) -> (CodexBackend, Session) {
    let backend = CodexBackend::new(&store.0);
    let session = backend.locate(CODEX_ID).unwrap().unwrap();
    (backend, session)
}

/// Claim: a whole read that was not asked for its timeline keeps none, so
/// its memory follows the record being read rather than the recording.
/// Rejects a read that keeps every counted request whether or not anyone
/// asked.
#[test]
fn a_read_not_asked_for_its_timeline_keeps_none() {
    let store = Store::new("unasked");
    many_requests(&store, 3);
    let (backend, session) = codex_read(&store);
    let read: StreamedTranscript = backend
        .stream_transcript(&session, None, &mut |_| Ok(()))
        .unwrap();
    assert!(usage::timeline(&read).is_none());
}

/// Claim: a read asked for its timeline keeps every request up to the bound,
/// and past it withholds the timeline whole with its fixed reason, while the
/// session's figures still count every request. Rejects a timeline kept
/// without bound, or one cut short in silence.
#[test]
fn a_timeline_past_its_bound_is_withheld_whole() {
    let asked = StreamOptions { timeline: true };
    let store = Store::new("within");
    many_requests(&store, 3);
    let (backend, session) = codex_read(&store);
    let read = backend
        .stream_transcript_with(&session, None, asked, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(usage::timeline(&read).unwrap().requests.len(), 3);
    assert_eq!(usage::timeline_withheld(&read), None);
    assert!(usage::stretches(&read, 60_000).is_some());

    let store = Store::new("beyond");
    many_requests(&store, MAX_TIMELINE_REQUESTS + 1);
    let (backend, session) = codex_read(&store);
    let read = backend
        .stream_transcript_with(&session, None, asked, &mut |_| Ok(()))
        .unwrap();
    assert!(usage::timeline(&read).is_none());
    assert_eq!(
        usage::timeline_withheld(&read),
        Some(TIMELINE_REQUESTS_WITHHELD)
    );
    assert!(usage::stretches(&read, 60_000).is_none());
    let whole = backend.stream_session(&session, &read).unwrap();
    assert_eq!(
        whole.tokens.and_then(|tokens| tokens.input),
        Some(MAX_TIMELINE_REQUESTS as u64 + 1)
    );
}
