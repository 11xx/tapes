//! A request sum keeps every request its recording states, and a figure's
//! qualifications and recorded price dimensions reach the model shares and
//! the estimates derived from it.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::UsageView;
use agent_tapes_core::{usage_spans_with_backends, usage_with_backends, Selection};
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
            "tapes-review2-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    /// Write `lines` as JSONL; a string line is written as it is, so a
    /// fixture can hold a line that does not decode.
    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines
            .iter()
            .map(|line| match line {
                Value::String(raw) => raw.clone(),
                other => other.to_string(),
            })
            .collect();
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

/// Two models priced so a share's estimate shows which model's row priced
/// it: every category costs 1 per million, output 10 under `model-a` and 1
/// under `model-b`.
fn table() -> PriceTable {
    let row = |id: &str, output: f64| {
        format!(
            "[[models]]\nid = \"{id}\"\neffective = 2026-01-01\nsource = \"fixture\"\n\
             per_million = {{ uncached_input = 1.0, cache_read = 1.0, cache_write_5m = 1.0, \
             cache_write_1h = 1.0, output = {output} }}\n"
        )
    };
    PriceTable::parse(&format!(
        "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n{}{}",
        row("model-a", 10.0),
        row("model-b", 1.0)
    ))
    .unwrap()
}

fn reasons(value: &Value) -> Vec<String> {
    let mut found = value["measurement_completeness"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(_, reasons)| reasons.as_array().cloned().unwrap_or_default())
        .filter_map(|reason| reason.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    found.sort();
    found.dedup();
    found
}

// --- Codex ---------------------------------------------------------------

const CODEX_ID: &str = "50000000-0000-7000-8000-0000000000a1";

fn codex_meta(stamp: &str) -> Value {
    json!({"timestamp": stamp, "type": "session_meta",
           "payload": {"id": CODEX_ID, "session_id": CODEX_ID, "timestamp": stamp,
                       "cwd": "/fixtures/project", "originator": "codex_exec", "source": "exec"}})
}

fn codex_context(stamp: &str, model: &str) -> Value {
    json!({"timestamp": stamp, "type": "turn_context",
           "payload": {"turn_id": "turn-a", "cwd": "/fixtures/project", "model": model, "effort": "high"}})
}

fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
           "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output})
}

fn usage_record(stamp: &str, response: &str, input: u64, output: u64) -> Value {
    json!({"timestamp": stamp, "type": "token_usage_record",
           "payload": {"thread_id": CODEX_ID, "turn_id": "turn-a", "session_id": CODEX_ID,
                       "root_turn_id": "turn-a", "response_id": response, "usage": usage(input, output)}})
}

fn token_count(stamp: &str, total: (u64, u64), last: (u64, u64)) -> Value {
    json!({"timestamp": stamp, "type": "event_msg",
           "payload": {"type": "token_count",
                       "info": {"total_token_usage": usage(total.0, total.1),
                                "last_token_usage": usage(last.0, last.1),
                                "model_context_window": 272000}}})
}

fn codex_store(label: &str, lines: &[Value]) -> Store {
    let store = Store::new(label);
    store.write(
        &format!("2026/01/09/rollout-2026-01-09T10-00-00-{CODEX_ID}.jsonl"),
        lines,
    );
    store
}

fn codex(store: &Store) -> Vec<Box<dyn Backend>> {
    vec![Box::new(CodexBackend::new(&store.0))]
}

/// Claim: a usage record a `compacted` record names as its compaction is a
/// request the cumulative total never moved for, whatever the next total's
/// move, so a batch of 100, a 200-token compaction and 300 whose total moves
/// by 400 sums to 600, and a span holding only the compaction counts its
/// 200. Rejects a settlement that, finding the move matches neither zero nor
/// the newest request, treats the compaction as inside the total.
#[test]
fn a_named_compaction_stays_outside_the_total_in_a_batched_settlement() {
    let store = codex_store(
        "compaction",
        &[
            codex_meta("2026-01-09T10:00:00Z"),
            codex_context("2026-01-09T10:00:00.100Z", "model-a"),
            json!({"timestamp": "2026-01-09T10:00:00.200Z", "type": "response_item",
                   "payload": {"type": "message", "role": "user",
                               "content": [{"type": "input_text", "text": "Work until it compacts."}]}}),
            usage_record("2026-01-09T10:00:01Z", "resp_1", 100, 0),
            usage_record("2026-01-09T10:00:02Z", "resp_compaction", 200, 0),
            json!({"timestamp": "2026-01-09T10:00:02.010Z", "type": "compacted",
                   "payload": {"message": "", "replacement_history": [], "window_number": 1,
                               "compaction_response_id": "resp_compaction"}}),
            usage_record("2026-01-09T10:00:03Z", "resp_2", 300, 0),
            token_count("2026-01-09T10:00:03.100Z", (400, 0), (300, 0)),
        ],
    );
    let answer = usage_spans_with_backends(
        &codex(&store),
        Selection::Id(CODEX_ID),
        &[SpanBounds::Time {
            from: Some(at("2026-01-09T10:00:01.500Z")),
            to: Some(at("2026-01-09T10:00:02.500Z")),
        }],
    )
    .unwrap();
    let input = |view: &UsageView| view.tokens.as_ref().and_then(|tokens| tokens.input);
    assert_eq!(input(&answer.whole), Some(600), "the thread's requests");
    assert_eq!(input(&answer.spans[0]), Some(200), "the compaction alone");
    assert_eq!(input(&answer.remainder), Some(400));
}

/// Claim: a malformed line between a `model-a` request and a `model-b`
/// request qualifies the span's model shares and its mixed-model estimate
/// with unreadable-record, as it qualifies the span. Rejects shares and an
/// estimate that claim completeness the span does not have.
#[test]
fn an_unreadable_record_qualifies_a_spans_model_shares_and_estimate() {
    let store = codex_store(
        "span-shares",
        &[
            codex_meta("2026-01-09T10:00:00Z"),
            codex_context("2026-01-09T10:00:00.100Z", "model-a"),
            usage_record("2026-01-09T10:00:01Z", "resp_1", 0, 100),
            token_count("2026-01-09T10:00:01.100Z", (0, 100), (0, 100)),
            Value::String("{\"timestamp\": \"2026-01-09T10:00:01.500Z\", \"type\":".to_owned()),
            codex_context("2026-01-09T10:00:02Z", "model-b"),
            usage_record("2026-01-09T10:00:03Z", "resp_2", 0, 100),
            token_count("2026-01-09T10:00:03.100Z", (0, 200), (0, 100)),
        ],
    );
    let mut answer = usage_spans_with_backends(
        &codex(&store),
        Selection::Id(CODEX_ID),
        &[SpanBounds::Time {
            from: None,
            to: None,
        }],
    )
    .unwrap();
    pricing::price_spans(&mut answer, &table());
    let span = serde_json::to_value(&answer.spans[0]).unwrap();
    assert!(
        reasons(&span).contains(&"unreadable-record".to_owned()),
        "{span:#}"
    );
    let shares = span["by_model"].as_array().unwrap();
    assert_eq!(shares.len(), 2, "{span:#}");
    for share in shares {
        assert!(
            reasons(share).contains(&"unreadable-record".to_owned()),
            "{share:#}"
        );
    }
    let estimate = &span["list_price_estimate"];
    assert!((estimate["amount"].as_f64().unwrap() - 0.0011).abs() < 1e-12);
    assert!(
        reasons(estimate).contains(&"unreadable-record".to_owned()),
        "{estimate:#}"
    );
}

// --- Claude --------------------------------------------------------------

const CLAUDE_ID: &str = "30000000-0000-4000-8000-0000000000a2";

fn claude_user() -> Value {
    json!({"type": "user", "sessionId": CLAUDE_ID, "uuid": "u-1", "entrypoint": "sdk-cli",
           "timestamp": "2026-01-09T10:00:00Z", "cwd": "/fixtures/project",
           "message": {"role": "user", "content": [{"type": "text", "text": "Inspect the fixture."}]}})
}

fn claude_reply(index: u32, request: Option<&str>, model: &str, usage: Value) -> Value {
    let mut record = json!({"type": "assistant", "sessionId": CLAUDE_ID, "uuid": format!("a-{index}"),
        "entrypoint": "sdk-cli", "timestamp": format!("2026-01-09T10:00:{:02}Z", index + 1),
        "cwd": "/fixtures/project",
        "message": {"role": "assistant", "model": model,
                    "content": [{"type": "text", "text": "ok"}], "usage": usage}});
    if let Some(request) = request {
        record["requestId"] = json!(request);
    }
    record
}

/// One request's complete usage: 100 output tokens and 1,000 cache writes,
/// all of them recorded as 1-hour writes.
fn complete_usage() -> Value {
    json!({"input_tokens": 0, "output_tokens": 100, "cache_read_input_tokens": 0,
           "cache_creation_input_tokens": 1000,
           "cache_creation": {"ephemeral_1h_input_tokens": 1000, "ephemeral_5m_input_tokens": 0}})
}

fn claude_store(label: &str, lines: &[Value]) -> Store {
    let store = Store::new(label);
    store.write(
        &format!("projects/-fixtures-project/{CLAUDE_ID}.jsonl"),
        lines,
    );
    store
}

fn claude_usage(store: &Store) -> UsageView {
    let backends: Vec<Box<dyn Backend>> =
        vec![Box::new(ClaudeBackend::new(store.0.join("projects")))];
    usage_with_backends(&backends, Selection::Id(CLAUDE_ID)).unwrap()
}

/// Claim: an assistant record with no request id whose usage writes no
/// counter is a request of unknown consumption, counted with its counters
/// missing, while one that writes every counter as zero stands in for a
/// request without being one. Rejects reading a missing counter as a written
/// zero, which erases the request and says the recording holds no
/// consumption of its own.
#[test]
fn a_request_without_counters_is_unknown_not_a_placeholder() {
    let store = claude_store(
        "placeholder",
        &[
            claude_user(),
            claude_reply(0, None, "model-a", json!({})),
            claude_reply(
                1,
                None,
                "model-a",
                json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0,
                       "cache_read_input_tokens": 0}),
            ),
        ],
    );
    let view = serde_json::to_value(claude_usage(&store)).unwrap();
    let notes = view["notes"].as_array().cloned().unwrap_or_default();
    assert!(
        !notes.iter().any(|note| note
            .as_str()
            .unwrap()
            .contains("holds no consumption of its own")),
        "{notes:?}"
    );
    assert!(
        reasons(&view).contains(&"missing-counter".to_owned()),
        "{view:#}"
    );
    assert_eq!(
        view["measurement_coverage"]["output"],
        json!({"measured": 0, "missing": 1, "unpriced": 0}),
        "the unknown request is the one counted event: {view:#}"
    );
}

/// Claim: a malformed line beside two fully measured model shares qualifies
/// each share and their combined estimate with unreadable-record, as it
/// qualifies the figure, and each share prices the 1-hour cache writes its
/// requests recorded, so the estimate is one amount, never a TTL range.
/// Rejects shares that drop the figure's qualification or the TTL split
/// their requests recorded.
#[test]
fn claude_model_shares_keep_the_figures_qualifications_and_ttl_split() {
    let store = claude_store(
        "shares",
        &[
            claude_user(),
            claude_reply(0, Some("req-1"), "model-a", complete_usage()),
            Value::String("{\"type\": \"assistant\", \"sessionId\":".to_owned()),
            claude_reply(1, Some("req-2"), "model-b", complete_usage()),
        ],
    );
    let mut view = claude_usage(&store);
    pricing::price_view(&mut view, &table());
    let view = serde_json::to_value(view).unwrap();
    assert!(
        reasons(&view).contains(&"unreadable-record".to_owned()),
        "{view:#}"
    );
    let shares = view["by_model"].as_array().unwrap();
    assert_eq!(shares.len(), 2, "{view:#}");
    for share in shares {
        assert!(
            reasons(share).contains(&"unreadable-record".to_owned()),
            "{share:#}"
        );
        let estimate = &share["list_price_estimate"];
        assert!(
            !reasons(estimate).contains(&"ttl-unrecorded".to_owned()),
            "{estimate:#}"
        );
    }
    let estimate = &view["list_price_estimate"];
    assert!(
        reasons(estimate).contains(&"unreadable-record".to_owned()),
        "{estimate:#}"
    );
    assert!(
        !reasons(estimate).contains(&"ttl-unrecorded".to_owned()),
        "{estimate:#}"
    );
    // 100 output at 10 and 1 per million, and 2,000 1-hour writes at 1.
    let amount = estimate["amount"].as_f64().unwrap();
    assert!(
        (amount - (1000.0 + 100.0 + 2000.0) / 1e6).abs() < 1e-12,
        "{amount}"
    );
}
