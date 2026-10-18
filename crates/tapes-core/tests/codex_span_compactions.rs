//! Codex records some requests that its cumulative `total_token_usage` never
//! moves for: a remote compaction, whose token count leaves the total where
//! it was, and a request no token count follows before the next one's. They
//! are consumption all the same, so a span places each by its own timestamp
//! and its own usage, the spans' whole is the thread's total, and a window
//! and its allowance count them where they were spent.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{codex::CodexBackend, Backend};
use agent_tapes_core::model::{AccountingBasis, Tokens};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::{GroupBy, SpanUsage, UsageView};
use agent_tapes_core::window::WindowBounds;
use agent_tapes_core::{
    usage_spans_with_backends, usage_window_with_backends, Selection, SessionSelection, WindowAsk,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

/// A session with a remote compaction at 10:00:03 and a request at 10:00:04
/// that no token count followed; its thread total is input 6600, output 490,
/// reasoning 40 and cache read 4200, of which the cumulative total recorded
/// 3400, 150, 35 and 1600.
const ID: &str = "50000000-0000-7000-8000-000000000009";

fn store() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-span-resets/sessions")
}

fn backend() -> Box<dyn Backend> {
    Box::new(CodexBackend::new(store()))
}

fn at(stamp: &str) -> DateTime<Utc> {
    stamp.parse().unwrap()
}

fn spans(bounds: &[SpanBounds]) -> SpanUsage {
    usage_spans_with_backends(&[backend()], Selection::Id(ID), bounds).unwrap()
}

fn counters(view: &UsageView) -> [u64; 5] {
    let tokens = view.tokens.clone().unwrap_or(Tokens {
        input: None,
        output: None,
        reasoning: None,
        cache_read: None,
        cache_write: None,
    });
    [
        tokens.input,
        tokens.output,
        tokens.reasoning,
        tokens.cache_read,
        tokens.cache_write,
    ]
    .map(|counter| counter.unwrap_or(0))
}

/// Claim: a span holding a compaction's timestamp counts the compaction's
/// own usage, and the request no token count followed is placed by its own
/// timestamp, so spans and the remainder add up, at every request boundary,
/// to a whole equal to the thread's total, in a basis that sums requests.
/// Rejects spans that read only the moves of the cumulative total, which
/// leave both requests out of every span and the whole.
#[test]
fn a_span_counts_the_compaction_its_timestamp_falls_in() {
    let split = at("2026-01-09T10:00:03.500Z");
    let halves = spans(&[
        SpanBounds::Time {
            from: None,
            to: Some(split),
        },
        SpanBounds::Time {
            from: Some(split),
            to: None,
        },
    ]);
    assert_eq!(
        counters(&halves.spans[0]),
        [1000 + 1500 + 2400, 50 + 70 + 300, 10 + 20, 900 + 2000, 0],
        "the span before the split holds two requests and the compaction"
    );
    assert_eq!(
        counters(&halves.spans[1]),
        [800 + 900, 40 + 30, 5 + 5, 600 + 700, 0],
        "the span after the split holds the unfollowed request and the last"
    );
    assert_eq!(counters(&halves.whole), [6600, 490, 40, 4200, 0]);
    assert_eq!(
        halves.whole.accounting.as_ref().unwrap().basis,
        AccountingBasis::SummedRequests
    );
    let session_requests = halves.spans[0].span.as_ref().unwrap().session_requests;
    assert_eq!(session_requests, 5);
    for boundary in 0..=session_requests {
        let split = spans(&[
            SpanBounds::Requests {
                first: None,
                last: Some(boundary),
            },
            SpanBounds::Requests {
                first: Some(boundary + 1),
                last: None,
            },
        ]);
        let parts = [&split.spans[0], &split.spans[1], &split.remainder];
        let summed = parts.iter().fold([0; 5], |mut sum, part| {
            for (sum, counter) in sum.iter_mut().zip(counters(part)) {
                *sum += counter;
            }
            sum
        });
        assert_eq!(
            summed,
            counters(&split.whole),
            "split after request {boundary}: spans and remainder against whole"
        );
    }
}

fn window(from: &str, to: &str, root: PathBuf) -> Value {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(root))];
    let selection = SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        ..Default::default()
    };
    let bounds = WindowBounds {
        from: Some(at(from)),
        to: Some(at(to)),
    };
    serde_json::to_value(
        usage_window_with_backends(
            &backends,
            &selection,
            &[GroupBy::Session],
            WindowAsk::Bounds(bounds),
            None,
        )
        .unwrap(),
    )
    .unwrap()
}

/// Claim: a window holding only the compaction and the unfollowed request
/// gives the session a share of exactly their usage. Rejects a window that
/// reads the cumulative total, in which neither moved it and the session
/// placed nothing.
#[test]
fn a_window_counts_the_compaction_inside_it() {
    let read = window(
        "2026-01-09T10:00:02.500Z",
        "2026-01-09T10:00:04.500Z",
        store(),
    );
    let shares = read["window"]["sessions"].as_array().unwrap();
    let share = shares
        .iter()
        .find(|share| share["id"] == ID)
        .unwrap_or_else(|| panic!("the session has a share: {}", read["window"]));
    assert_eq!(share["requests"], 2);
    let disjoint = &share["disjoint"];
    assert_eq!(
        [
            &disjoint["uncached_input"],
            &disjoint["cache_read"],
            &disjoint["output"]
        ],
        [&json!(400 + 200), &json!(2000 + 600), &json!(300 + 40)],
        "{share}"
    );
}

/// A store laid out as Codex lays out its sessions, removed when dropped.
struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-span-compactions-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, id: &str, lines: &[Value]) {
        let path = self
            .0
            .join(format!("2026/01/10/rollout-2026-01-10T10-00-00-{id}.jsonl"));
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

fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
           "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output})
}

fn opening(id: &str, stamp: &str) -> Vec<Value> {
    vec![
        json!({"timestamp": stamp, "type": "session_meta", "payload": {"id": id, "session_id": id,
               "timestamp": stamp, "cwd": "/fixtures/project", "source": "exec",
               "creator_account_id": "fixture-account"}}),
        json!({"timestamp": stamp, "type": "turn_context", "payload": {"turn_id": "turn-a",
               "cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
    ]
}

fn observed(stamp: &str, used: f64) -> Value {
    json!({"timestamp": stamp, "type": "event_msg", "payload": {"type": "token_count", "info": null,
           "rate_limits": {"limit_id": "codex", "plan_type": "plus",
               "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_768_053_600},
               "secondary": null}}})
}

fn record(id: &str, stamp: &str, response: &str, input: u64) -> Value {
    json!({"timestamp": stamp, "type": "token_usage_record", "payload": {"thread_id": id,
           "turn_id": "turn-a", "session_id": id, "root_turn_id": "turn-a",
           "response_id": response, "usage": usage(input, 0), "turn_token_usage": usage(input, 0)}})
}

fn count(stamp: &str, total: u64, last: u64) -> Value {
    json!({"timestamp": stamp, "type": "event_msg", "payload": {"type": "token_count",
           "info": {"total_token_usage": usage(total, 0), "last_token_usage": usage(last, 0),
                    "model_context_window": 272000}, "rate_limits": null}})
}

/// Claim: the allowance a pool's rise apportions over the sessions that
/// spent it counts a compaction's tokens in the interval holding its
/// timestamp. Session A spends 100 tokens and then compacts for 300, B
/// spends 400, and the pool rises 4 percent, so A's share is 2. Rejects an
/// apportioning that reads only the cumulative total, which gives A 0.8.
#[test]
fn the_allowance_apportions_the_compaction() {
    let store = Store::new();
    let a = "50000000-0000-7000-8000-0000000000a1";
    let b = "50000000-0000-7000-8000-0000000000b2";
    let mut lines = opening(a, "2026-01-10T10:00:00Z");
    lines.extend([
        observed("2026-01-10T10:00:00Z", 10.0),
        record(a, "2026-01-10T10:05:00Z", "resp_a_1", 100),
        count("2026-01-10T10:05:00.100Z", 100, 100),
        record(a, "2026-01-10T10:06:00Z", "resp_a_compaction", 300),
        json!({"timestamp": "2026-01-10T10:06:00.010Z", "type": "compacted", "payload": {
               "message": "", "replacement_history": [], "compaction_response_id": "resp_a_compaction"}}),
        count("2026-01-10T10:06:00.030Z", 100, 0),
        observed("2026-01-10T10:10:00Z", 14.0),
    ]);
    store.write(a, &lines);
    let mut lines = opening(b, "2026-01-10T10:01:00Z");
    lines.extend([
        record(b, "2026-01-10T10:07:00Z", "resp_b_1", 400),
        count("2026-01-10T10:07:00.100Z", 400, 400),
    ]);
    store.write(b, &lines);
    let read = window(
        "2026-01-10T09:00:00Z",
        "2026-01-10T12:00:00Z",
        store.0.clone(),
    );
    let estimate = |id: &str| {
        read["window"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|share| share["id"] == id)
            .unwrap()["allowance"][0]["estimate"]
            .as_f64()
            .unwrap()
    };
    assert!(
        (estimate(a) - 2.0).abs() < 1e-9,
        "A spent 400 of 800 tokens of a 4 percent rise: {}",
        read["window"]["sessions"]
    );
    assert!((estimate(b) - 2.0).abs() < 1e-9);
}
