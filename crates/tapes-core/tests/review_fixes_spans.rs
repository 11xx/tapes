//! Span, price and Codex attribution figures stay what they claim to be in
//! the cases an independent review named: a qualification reaches every
//! figure it bears on, an amount priced under uncertain attribution is a
//! known subtotal rather than a guess, a batched Codex observation is not
//! mistaken for a request outside the total, and a price table that is not
//! a regular file is refused without waiting on it.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{codex::CodexBackend, Backend};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::{usage_full_with_backends, usage_spans_with_backends, Selection};
use serde_json::{json, Value};

/// A store laid out as its harness lays it out, removed when dropped.
struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-review-fixes-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(&path, body.join("\n") + "\n").unwrap();
        path
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn codex_usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
           "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output})
}

fn codex_meta(id: &str) -> Value {
    json!({"timestamp": "2026-01-11T10:00:00Z", "type": "session_meta", "payload": {"id": id,
           "session_id": id, "timestamp": "2026-01-11T10:00:00Z", "cwd": "/fixtures/project",
           "originator": "codex_exec", "source": "exec"}})
}

fn codex_context(stamp: &str, model: &str) -> Value {
    json!({"timestamp": stamp, "type": "turn_context", "payload": {"turn_id": "turn-a",
           "cwd": "/fixtures/project", "model": model, "effort": "high"}})
}

fn codex_record(id: &str, stamp: &str, response: &str, input: u64, output: u64) -> Value {
    json!({"timestamp": stamp, "type": "token_usage_record", "payload": {"thread_id": id,
           "turn_id": "turn-a", "session_id": id, "root_turn_id": "turn-a", "response_id": response,
           "usage": codex_usage(input, output), "turn_token_usage": codex_usage(input, output)}})
}

fn codex_count(stamp: &str, total: (u64, u64), last: (u64, u64)) -> Value {
    json!({"timestamp": stamp, "type": "event_msg", "payload": {"type": "token_count",
           "info": {"total_token_usage": codex_usage(total.0, total.1),
                    "last_token_usage": codex_usage(last.0, last.1),
                    "model_context_window": 272000}, "rate_limits": null}})
}

/// Claim: two usage records the first token count reports together, total
/// 300 with the newer record's 200 as its last usage, are both inside the
/// recorded total, so attribution names no request outside it and the
/// spans' whole is the recorded 300. Rejects reading the matching last
/// usage as proof that the earlier record was left out, which names 100
/// outside the total and counts it twice in the spans.
#[test]
fn records_a_token_count_reports_together_are_inside_the_total() {
    let store = Store::new("codex-batched");
    let id = "70000000-0000-7000-8000-0000000000d1";
    store.write(
        &format!("2026/01/11/rollout-2026-01-11T10-00-00-{id}.jsonl"),
        &[
            codex_meta(id),
            codex_context("2026-01-11T10:00:00.100Z", "model-a"),
            codex_record(id, "2026-01-11T10:00:01Z", "resp_batched_1", 100, 0),
            codex_record(id, "2026-01-11T10:00:02Z", "resp_batched_2", 200, 0),
            codex_count("2026-01-11T10:00:02.100Z", (300, 0), (200, 0)),
        ],
    );
    let backend = || -> Box<dyn Backend> { Box::new(CodexBackend::new(&store.0)) };
    let full =
        serde_json::to_value(usage_full_with_backends(&[backend()], Selection::Id(id)).unwrap())
            .unwrap();
    assert_eq!(full["tokens"]["input"], 300);
    assert_eq!(
        full["attribution"]["outside_total"], 0,
        "{}",
        full["attribution"]
    );
    assert!(full["attribution"]["outside_total_tokens"].is_null());
    let spans = usage_spans_with_backends(
        &[backend()],
        Selection::Id(id),
        &[SpanBounds::Requests {
            first: None,
            last: None,
        }],
    )
    .unwrap();
    let whole = spans.whole.tokens.as_ref().unwrap();
    assert_eq!(whole.input, Some(300), "the spans' whole");
}
