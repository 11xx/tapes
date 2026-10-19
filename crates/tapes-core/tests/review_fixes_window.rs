//! Window reads and allowance stay what they claim to be in the cases an
//! independent review named: a subagent spending inside a window counts
//! though its parent went quiet before it, a pool's rise is apportioned
//! only over the sessions of its own account, rates divide the tokens the
//! allowance covers by the allowance that covers them, apportionment keeps
//! one cell per request it places rather than one per member and interval,
//! and a request whose time is unknown is never moved into the next one.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::allowance::{Apportionment, PoolKey, PoolMember, RateObservation, TimedUse};
use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_rates_with_backends, usage_window_with_backends, SessionSelection, WindowAsk,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

/// A store laid out as its harness lays it out, removed when dropped.
struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-review-window-{label}-{}-{}-{nonce}",
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

fn window_read(backend: Box<dyn Backend>, harness: &'static str, from: &str, to: &str) -> Value {
    let selection = SessionSelection {
        harness: Some(harness),
        limit: Some(50),
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

fn share<'a>(read: &'a Value, id: &str) -> Option<&'a Value> {
    read["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"] == id)
}

fn claude_record(id: &str, agent: Option<&str>, entry: &str, stamp: &str, output: u64) -> Value {
    let mut record = json!({
        "sessionId": id, "cwd": "/fixtures/project", "entrypoint": "cli", "version": "2.1.0",
        "type": "assistant", "uuid": entry, "parentUuid": null, "requestId": format!("req_{entry}"),
        "timestamp": stamp,
        "message": {"id": format!("msg_{entry}"), "role": "assistant", "model": "claude-fixture",
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                      "output_tokens": output},
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "Done."}]}
    });
    if let Some(agent) = agent {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(agent);
    }
    record
}

/// Claim: a subagent that spends inside a window contributes its share
/// though its parent's own last record precedes the window. Rejects a window
/// read that drops the parent by its own activity before it looks for the
/// parent's subagent recordings.
#[test]
fn a_subagent_spending_in_a_window_counts_after_its_parent_went_quiet() {
    let store = Store::new("quiet-parent");
    let id = "30000000-0000-4000-8000-0000000000f1";
    let agent = "a00000000000000f1";
    store.write(
        &format!("project/{id}.jsonl"),
        &[claude_record(
            id,
            None,
            "parent-1",
            "2026-01-12T09:59:00Z",
            50,
        )],
    );
    store.write(
        &format!("project/{id}/subagents/agent-{agent}.jsonl"),
        &[claude_record(
            id,
            Some(agent),
            "child-1",
            "2026-01-12T10:05:00Z",
            70,
        )],
    );
    let read = window_read(
        Box::new(ClaudeBackend::new(&store.0)),
        "claude",
        "2026-01-12T10:00:00Z",
        "2026-01-12T11:00:00Z",
    );
    let child = share(&read, &format!("{id}::{agent}"))
        .unwrap_or_else(|| panic!("the subagent's share: {}", read["window"]));
    assert_eq!(child["requests"], 1);
    assert_eq!(child["disjoint"]["output"], 70);
}

fn codex_usage(input: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
           "output_tokens": 0, "reasoning_output_tokens": 0, "total_tokens": input})
}

/// What a Codex rollout of a fixture holds, in time order.
enum Event {
    /// The primary used percent the provider reported.
    Observed(&'static str, f64),
    /// One request's uncached input, at a time or with none.
    Request(Option<&'static str>, u64),
}

fn rollout(store: &Store, id: &str, account: &str, events: &[Event]) {
    let mut lines = vec![
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "session_meta", "payload": {"id": id,
               "timestamp": "2026-01-10T09:00:00Z", "cwd": "/fixtures/project", "source": "exec",
               "creator_account_id": account}}),
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
    ];
    let mut total = 0;
    for event in events {
        match event {
            Event::Observed(stamp, used) => lines.push(json!({
                "timestamp": stamp, "type": "event_msg",
                "payload": {"type": "token_count", "info": null, "rate_limits": {
                    "limit_id": "codex", "plan_type": "plus",
                    "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_768_053_600},
                    "secondary": null}}
            })),
            Event::Request(stamp, input) => {
                total += input;
                let mut line = json!({
                    "type": "event_msg",
                    "payload": {"type": "token_count", "info": {"total_token_usage": codex_usage(total),
                        "last_token_usage": codex_usage(*input), "model_context_window": 272000},
                        "rate_limits": null}
                });
                if let Some(stamp) = stamp {
                    line["timestamp"] = json!(stamp);
                }
                lines.push(line);
            }
        }
    }
    store.write(
        &format!("2026/01/10/rollout-2026-01-10T09-00-00-{id}.jsonl"),
        &lines,
    );
}

fn allowance_estimate(read: &Value, id: &str) -> Option<f64> {
    share(read, id).and_then(|share| share["allowance"][0]["estimate"].as_f64())
}

/// Claim: a pool's rise is apportioned only over sessions of the account
/// whose pool it is. Account A's percent rises 3 while A spends 300 tokens
/// and a session of account B spends 100, so A's share is all 3 and B has
/// none of A's pool. Rejects apportioning every session of the store over
/// every pool, which gives B 0.75 and A 2.25.
#[test]
fn a_pool_is_apportioned_over_its_own_account() {
    use Event::{Observed, Request};
    let store = Store::new("accounts");
    let a = "40000000-0000-7000-8000-0000000000f1";
    let b = "40000000-0000-7000-8000-0000000000f2";
    rollout(
        &store,
        a,
        "account-a",
        &[
            Observed("2026-01-10T10:00:00Z", 10.0),
            Request(Some("2026-01-10T10:05:00Z"), 300),
            Observed("2026-01-10T10:10:00Z", 13.0),
        ],
    );
    rollout(
        &store,
        b,
        "account-b",
        &[Request(Some("2026-01-10T10:06:00Z"), 100)],
    );
    let read = window_read(
        Box::new(CodexBackend::new(&store.0)),
        "codex",
        "2026-01-10T09:00:00Z",
        "2026-01-10T12:00:00Z",
    );
    let a_share = allowance_estimate(&read, a).unwrap();
    assert!((a_share - 3.0).abs() < 1e-9, "A's share: {a_share}");
    assert_eq!(
        allowance_estimate(&read, b),
        None,
        "B spends from another account's pool: {}",
        share(&read, b).unwrap()
    );
}

/// Claim: rates divide the tokens the allowance covers by the allowance
/// that covers them, and say how many requests fell outside it. A lane
/// spends 400 tokens between two observations 4 percent apart and 100
/// after the last, so its rate is 100 tokens per percent, with one request
/// uncovered. Rejects dividing all 500 tokens by the covered percent, which
/// gives 125, and a rate that drops the uncovered request.
#[test]
fn rates_divide_covered_tokens_by_the_allowance_covering_them() {
    use Event::{Observed, Request};
    let store = Store::new("rates");
    let id = "40000000-0000-7000-8000-0000000000f3";
    rollout(
        &store,
        id,
        "account-a",
        &[
            Observed("2026-01-10T10:00:00Z", 10.0),
            Request(Some("2026-01-10T10:05:00Z"), 400),
            Observed("2026-01-10T10:10:00Z", 14.0),
            Request(Some("2026-01-10T10:15:00Z"), 100),
        ],
    );
    let selection = SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        ..Default::default()
    };
    let rates = serde_json::to_value(
        allowance_rates_with_backends(
            &[Box::new(CodexBackend::new(&store.0))],
            &selection,
            &[GroupBy::Session],
            WindowBounds {
                from: Some(at("2026-01-10T09:00:00Z")),
                to: Some(at("2026-01-10T12:00:00Z")),
            },
            QuotaKind::Primary,
        )
        .unwrap(),
    )
    .unwrap();
    let group = &rates["groups"][0];
    let rate = group["tokens_per_percent"]["total"]["estimate"]
        .as_f64()
        .unwrap();
    assert!((rate - 100.0).abs() < 1e-9, "tokens per percent: {group}");
    assert_eq!(group["uncovered_requests"], 1, "{group}");
}

fn pool() -> PoolKey {
    PoolKey {
        harness: "codex".to_owned(),
        account: Some("account-a".to_owned()),
        limit: Some("codex".to_owned()),
        plan: Some("plus".to_owned()),
        window: QuotaKind::Primary,
        window_minutes: Some(300),
    }
}

/// Claim: apportionment keeps one weight cell per interval a member spent
/// in, so a thousand members of one request each over ten thousand
/// observations keep a thousand cells, and the shares still add up to the
/// observed rise exactly. Rejects a dense member-by-interval matrix, which
/// holds ten million cells here and grows as their product.
#[test]
fn apportionment_keeps_one_cell_per_spent_interval() {
    let start = at("2026-01-10T00:00:00Z");
    let observations = (0..10_000)
        .map(|index| RateObservation {
            at: start + Duration::seconds(index * 10),
            pool: pool(),
            used_percent: (index / 100) as f64,
            resets_at: None,
        })
        .collect::<Vec<_>>();
    let ids = (0..1_000)
        .map(|index| format!("member-{index}"))
        .collect::<Vec<_>>();
    let requests = (0..1_000)
        .map(|index| {
            vec![TimedUse {
                at: start + Duration::seconds(index * 90 + 5),
                uncached_input: 100,
                ..TimedUse::default()
            }]
        })
        .collect::<Vec<_>>();
    let members = ids
        .iter()
        .zip(&requests)
        .map(|(id, requests)| PoolMember {
            id,
            selected: true,
            account: Some("account-a"),
            limits: Vec::new(),
            requests,
            untimed: 0,
        })
        .collect::<Vec<_>>();
    let apportionment = Apportionment::new(pool(), &observations, &members);
    assert_eq!(apportionment.weight_cells(), 1_000);
    let everyone = ids.iter().map(String::as_str).collect::<Vec<_>>();
    let spent = apportionment.allowance(&everyone).percent.estimate;
    let pool = apportionment.pool();
    let parts = pool.attributed.estimate + pool.without_recorded_use.estimate;
    assert!((parts - pool.observed.estimate).abs() < 1e-6);
    assert!((spent - pool.attributed.estimate).abs() < 1e-9);
}

/// Claim: a Codex total recorded without a timestamp is a request no
/// interval can hold. It counts as uncovered and is not added to the next
/// timed request. Session A records an untimed total of 100 and then a
/// timed one of 200, and B spends 100 in the same interval, so a rise of 2
/// gives each 1 and A one uncovered request. Rejects a timed request of 200
/// for A, which gives A 1.33.
#[test]
fn an_untimed_codex_total_is_not_moved_into_the_next_request() {
    use Event::{Observed, Request};
    let store = Store::new("untimed");
    let a = "40000000-0000-7000-8000-0000000000f4";
    let b = "40000000-0000-7000-8000-0000000000f5";
    rollout(
        &store,
        a,
        "account-a",
        &[
            Observed("2026-01-10T10:00:00Z", 10.0),
            Request(None, 100),
            Request(Some("2026-01-10T10:05:00Z"), 100),
            Observed("2026-01-10T10:10:00Z", 12.0),
        ],
    );
    rollout(
        &store,
        b,
        "account-a",
        &[Request(Some("2026-01-10T10:06:00Z"), 100)],
    );
    let read = window_read(
        Box::new(CodexBackend::new(&store.0)),
        "codex",
        "2026-01-10T09:00:00Z",
        "2026-01-10T12:00:00Z",
    );
    let a_share = share(&read, a).unwrap();
    let estimate = a_share["allowance"][0]["estimate"].as_f64().unwrap();
    assert!((estimate - 1.0).abs() < 1e-9, "A's share: {a_share}");
    assert_eq!(
        a_share["allowance"][0]["coverage"]["uncovered_requests"], 1,
        "{a_share}"
    );
}
