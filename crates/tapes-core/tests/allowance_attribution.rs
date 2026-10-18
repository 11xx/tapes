//! Allowance percent is apportioned over the sessions that spent it, by their
//! disjoint tokens in each interval between two observations of a pool, and
//! rates per lane come from the same apportionment.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_rates_with_backends, usage_window_with_backends, ListFilters, SessionSelection,
    WindowAsk,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-allowance-{label}-{}-{nonce}",
            std::process::id()
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

/// What a Codex rollout of the fixture holds, in time order.
enum Event {
    /// The account's primary used percent, and when its window resets.
    Observed(&'static str, f64, i64),
    /// One request: uncached input and output.
    Request(&'static str, u64, u64),
}

fn rollout(id: &str, model: &str, events: &[Event]) -> Vec<Value> {
    let first = match &events[0] {
        Event::Observed(stamp, ..) | Event::Request(stamp, ..) => *stamp,
    };
    let mut lines = vec![
        json!({"timestamp": first, "type": "session_meta", "payload": {"id": id, "timestamp": first,
               "cwd": "/fixtures/project", "source": "exec", "creator_account_id": "fixture-account"}}),
        json!({"timestamp": first, "type": "turn_context", "payload": {"cwd": "/fixtures/project", "model": model, "effort": "high"}}),
    ];
    let (mut input, mut output) = (0u64, 0u64);
    for event in events {
        match event {
            Event::Observed(stamp, used, resets_at) => lines.push(json!({
                "timestamp": stamp, "type": "event_msg",
                "payload": {"type": "token_count", "info": null, "rate_limits": {
                    "limit_id": "codex", "plan_type": "plus",
                    "primary": {"used_percent": used, "window_minutes": 300, "resets_at": resets_at},
                    "secondary": null}}
            })),
            Event::Request(stamp, uncached, out) => {
                input += uncached;
                output += out;
                let usage = |input: u64, output: u64| json!({"input_tokens": input, "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0, "output_tokens": output, "reasoning_output_tokens": 0,
                    "total_tokens": input + output});
                lines.push(json!({
                    "timestamp": stamp, "type": "event_msg",
                    "payload": {"type": "token_count", "info": {"total_token_usage": usage(input, output),
                        "last_token_usage": usage(*uncached, *out), "model_context_window": 272000},
                        "rate_limits": null}
                }));
            }
        }
    }
    lines
}

const RESETS: i64 = 1_768_053_600; // 2026-01-10T14:00:00Z
const NEXT_RESETS: i64 = 1_768_071_600; // 2026-01-10T19:00:00Z

/// Two concurrent sessions of one pool. The pool's percent reads 10, 13, 15
/// and 16 inside one window period, then resets and reads 2 and 4. A spends
/// 300 then 100 tokens across the first two intervals, B 100 then 300; the
/// third interval holds no request; A spends 400 after the reset. D, another
/// model's session, spends 100 in the first interval.
fn pool_store(with_outsider: bool) -> Store {
    use Event::{Observed, Request};
    let store = Store::new("pool");
    let a = "40000000-0000-7000-8000-0000000000a1";
    let b = "40000000-0000-7000-8000-0000000000b2";
    store.write(
        &format!("2026/01/10/rollout-2026-01-10T10-00-00-{a}.jsonl"),
        &rollout(
            a,
            "gpt-fixture",
            &[
                Observed("2026-01-10T10:00:00Z", 10.0, RESETS),
                Request("2026-01-10T10:05:00Z", 270, 30),
                Observed("2026-01-10T10:10:00Z", 13.0, RESETS),
                Request("2026-01-10T10:15:00Z", 90, 10),
                Request("2026-01-10T10:35:00Z", 360, 40),
                Observed("2026-01-10T10:40:00Z", 4.0, NEXT_RESETS),
            ],
        ),
    );
    store.write(
        &format!("2026/01/10/rollout-2026-01-10T10-01-00-{b}.jsonl"),
        &rollout(
            b,
            "gpt-fixture",
            &[
                Request("2026-01-10T10:07:00Z", 30, 70),
                Request("2026-01-10T10:17:00Z", 270, 30),
                Observed("2026-01-10T10:20:00Z", 15.0, RESETS),
                Observed("2026-01-10T10:25:00Z", 16.0, RESETS),
                Observed("2026-01-10T10:30:00Z", 2.0, NEXT_RESETS),
            ],
        ),
    );
    if with_outsider {
        let d = "40000000-0000-7000-8000-0000000000d4";
        store.write(
            &format!("2026/01/10/rollout-2026-01-10T10-02-00-{d}.jsonl"),
            &rollout(d, "other-model", &[Request("2026-01-10T10:08:00Z", 90, 10)]),
        );
    }
    store
}

fn window_read(store: &Store, filters: ListFilters<'static>) -> Value {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.0))];
    let selection = SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        filters,
        ..Default::default()
    };
    let window = WindowBounds {
        from: Some(at("2026-01-10T09:00:00Z")),
        to: Some(at("2026-01-10T12:00:00Z")),
    };
    serde_json::to_value(
        usage_window_with_backends(
            &backends,
            &selection,
            &[GroupBy::Session],
            WindowAsk::Bounds(window),
            None,
        )
        .unwrap(),
    )
    .unwrap()
}

fn share<'a>(read: &'a Value, suffix: &str) -> &'a Value {
    read["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"].as_str().unwrap().ends_with(suffix))
        .unwrap()
}

fn close(value: &Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|value| (value - expected).abs() < 1e-9)
}

fn bounds(value: &Value) -> (f64, f64, f64) {
    (
        value["estimate"].as_f64().unwrap(),
        value["at_least"].as_f64().unwrap(),
        value["at_most"].as_f64().unwrap(),
    )
}

/// Claim: each interval's rise is apportioned over the disjoint tokens each
/// session spent inside it, a reset starts a new chain whose rise is
/// apportioned on its own, and whole-percent endpoints give bounds. Rejects
/// splitting a rise evenly, counting across a reset, or reporting an amount
/// where the observations' rounding matters.
#[test]
fn shares_get_the_rise_of_each_interval_in_proportion_to_their_tokens() {
    let read = window_read(&pool_store(false), ListFilters::default());
    let a = &share(&read, "a1")["allowance"][0];
    let b = &share(&read, "b2")["allowance"][0];
    assert_eq!(a["kind"], "apportioned-allowance");
    assert_eq!(a["window"], "primary");
    assert_eq!(a["window_minutes"], 300);
    // A: 3 x 300/400 + 2 x 100/400 before the reset, 2 x 400/400 after it.
    assert!(close(&a["estimate"], 2.25 + 0.5 + 2.0), "{a}");
    // B: 3 x 100/400 + 2 x 300/400.
    assert!(close(&b["estimate"], 0.75 + 1.5), "{b}");
    // Each whole endpoint is within half a percent: A's weights change by
    // 0.75, 0.5 and 0.25 along the first chain and by 1 twice along the second.
    assert_eq!(bounds(a), (4.75, 4.75 - 0.75 - 1.0, 4.75 + 0.75 + 1.0));
    assert_eq!(bounds(b).1, 2.25 - 0.75);
    // Output weighting: 3 x 30/100 + 2 x 10/40, then 2.
    assert!(
        close(&a["output_weighted"]["estimate"], 0.9 + 0.5 + 2.0),
        "{a}"
    );
    let pool = &read["window"]["allowance"][0];
    assert_eq!(pool["pool"]["account"], "fixture-account");
    // The rises: 3 + 2 + 1 before the reset, 2 after it; the third interval
    // holds no request.
    assert!(close(&pool["observed"]["estimate"], 8.0));
    assert!(close(&pool["without_recorded_use"]["estimate"], 1.0));
    assert!(close(&pool["attributed"]["estimate"], 7.0));
    assert_eq!(pool["coverage"]["observations"], 6);
    assert_eq!(pool["coverage"]["intervals"], 4);
}

/// Claim: a session of the pool the selection leaves out keeps its share as
/// `outside_selection`, so shares, outsiders and rises without recorded use
/// add up to each observed rise; a group and the total carry the same
/// apportionment. Rejects giving the outsider's share to the selection.
#[test]
fn shares_and_unattributed_parts_add_up_to_the_observed_rise() {
    let filters = ListFilters {
        model: Some("gpt-fixture"),
        ..ListFilters::default()
    };
    let read = window_read(&pool_store(true), filters);
    assert_eq!(read["window"]["sessions"].as_array().unwrap().len(), 2);
    let pool = &read["window"]["allowance"][0];
    // D spent 100 of 500 tokens in the first interval: 3 x 100/500.
    assert!(close(&pool["outside_selection"]["estimate"], 0.6), "{pool}");
    let parts = ["attributed", "outside_selection", "without_recorded_use"]
        .iter()
        .map(|part| pool[*part]["estimate"].as_f64().unwrap())
        .sum::<f64>();
    assert!((parts - pool["observed"]["estimate"].as_f64().unwrap()).abs() < 1e-9);
    let shares = ["a1", "b2"]
        .iter()
        .map(|suffix| {
            share(&read, suffix)["allowance"][0]["estimate"]
                .as_f64()
                .unwrap()
        })
        .sum::<f64>();
    assert!(close(&read["totals"]["allowance"][0]["estimate"], shares));
    assert!(close(&pool["attributed"]["estimate"], shares));
    let group = read["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["key"]["session"].as_str().unwrap().ends_with("a1"))
        .unwrap();
    assert!(close(
        &group["allowance"][0]["estimate"],
        share(&read, "a1")["allowance"][0]["estimate"]
            .as_f64()
            .unwrap()
    ));
}

/// Claim: a Claude share carries no allowance and says why, Claude recording
/// no used percent, and its tokens are the window read's as before. Rejects
/// an allowance invented for Claude or a share dropped for lacking one.
#[test]
fn claude_shares_carry_no_allowance_and_say_why() {
    let store = Store::new("claude");
    let id = "30000000-0000-4000-8000-0000000000f1";
    store.write(
        &format!("projects/-fixtures-project/{id}.jsonl"),
        &[json!({
            "type": "assistant", "sessionId": id, "uuid": "a1", "requestId": "r1", "entrypoint": "cli",
            "timestamp": "2026-01-10T10:05:00Z", "cwd": "/fixtures/project",
            "message": {"role": "assistant", "model": "claude-fixture", "content": [{"type": "text", "text": "ok"}],
                        "usage": {"input_tokens": 3, "output_tokens": 7, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
        })],
    );
    let backends: Vec<Box<dyn Backend>> =
        vec![Box::new(ClaudeBackend::new(store.0.join("projects")))];
    let window = WindowBounds {
        from: Some(at("2026-01-10T10:00:00Z")),
        to: None,
    };
    let read = serde_json::to_value(
        usage_window_with_backends(
            &backends,
            &SessionSelection {
                limit: Some(20),
                ..Default::default()
            },
            &[GroupBy::Session],
            WindowAsk::Bounds(window),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let share = &read["window"]["sessions"][0];
    assert!(share.get("allowance").is_none());
    assert!(share["allowance_absent"]
        .as_str()
        .unwrap()
        .starts_with("claude records no used percent"));
    assert_eq!(read["totals"]["tokens"]["output"], 7);
    assert!(read["window"].get("allowance").is_none());
}

/// Claim: rates per lane give the median, the 90th percentile by nearest
/// rank and the maximum of the lanes' allowance, tokens per one percent over
/// the group, the median lane duration, and the dates they were measured
/// over. Rejects an interpolated percentile or an undated rate.
#[test]
fn rates_per_lane_give_quantiles_tokens_per_percent_and_dates() {
    use Event::{Observed, Request};
    let store = Store::new("rates");
    // Five lanes one after another; the percent rises 1, 2, 3, 4 and 10
    // across them. Each lane spends 1000 tokens in two requests minutes apart.
    let observed = [0.0, 1.0, 3.0, 6.0, 10.0, 20.0];
    let durations = [2, 4, 6, 8, 10];
    for lane in 0..5 {
        let id: &'static str =
            Box::leak(format!("40000000-0000-7000-8000-00000000010{lane}").into_boxed_str());
        let hour = 10 + lane;
        let stamp = |minute: u32| -> &'static str {
            Box::leak(format!("2026-01-11T{hour:02}:{minute:02}:00Z").into_boxed_str())
        };
        let mut events = Vec::new();
        if lane == 0 {
            events.push(Observed(stamp(0), observed[0], 1_768_200_000));
        }
        events.push(Request(stamp(1), 450, 50));
        events.push(Request(stamp(1 + durations[lane]), 450, 50));
        events.push(Observed(stamp(30), observed[lane + 1], 1_768_200_000));
        store.write(
            &format!("2026/01/11/rollout-2026-01-11T{hour:02}-00-00-{id}.jsonl"),
            &rollout(id, "gpt-fixture", &events),
        );
    }
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.0))];
    let selection = SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        ..Default::default()
    };
    let lookback = WindowBounds {
        from: Some(at("2026-01-11T00:00:00Z")),
        to: Some(at("2026-01-12T00:00:00Z")),
    };
    let rates = serde_json::to_value(
        allowance_rates_with_backends(
            &backends,
            &selection,
            &[GroupBy::Model],
            lookback,
            QuotaKind::Primary,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(rates["schema"], "tapes-allowance-rates/1");
    let group = &rates["groups"][0];
    assert_eq!(group["key"]["model"], "gpt-fixture");
    assert_eq!(group["lanes"], 5);
    let estimate = &group["percent_per_lane"]["estimate"];
    assert!(close(&estimate["median"], 3.0), "{estimate}");
    assert!(close(&estimate["p90"], 10.0), "{estimate}");
    assert!(close(&estimate["max"], 10.0));
    // 5000 tokens over 20 percent; 4500 of them uncached input, 500 output.
    assert!(close(
        &group["tokens_per_percent"]["total"]["estimate"],
        250.0
    ));
    assert!(close(
        &group["tokens_per_percent"]["uncached_input"]["estimate"],
        225.0
    ));
    assert!(close(
        &group["tokens_per_percent"]["output"]["estimate"],
        25.0
    ));
    assert!(close(&group["median_duration_minutes"], 6.0));
    assert_eq!(group["measured_over"]["from"], "2026-01-11T10:01:00Z");
    assert_eq!(group["measured_over"]["to"], "2026-01-11T14:11:00Z");
}
