//! Span, price and Codex attribution figures stay what they claim to be in
//! the cases an independent review named: a qualification reaches every
//! figure it bears on, an amount priced under uncertain attribution is a
//! known subtotal rather than a guess, a batched Codex observation is not
//! mistaken for a request outside the total, and a price table that is not
//! a regular file is refused without waiting on it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{codex::CodexBackend, pi::PiBackend, Backend};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::UsageView;
use agent_tapes_core::{usage_full_with_backends, usage_spans_with_backends, Selection};
use serde_json::{json, Value};

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

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

fn reasons(completeness: &Value, counter: &str) -> Vec<String> {
    completeness[counter]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .map(|reason| reason.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Claim: a pi span the read could not cover, because a malformed record
/// lies among its requests, qualifies its retained-branch consumption
/// `unreadable-record` as it qualifies its active-branch counters. Rejects
/// a consumption figure that reads as complete beside a span that counts
/// the record as unplaced.
#[test]
fn a_pi_span_qualifies_its_consumption_with_an_unreadable_record() {
    let answer = usage_spans_with_backends(
        &[Box::new(PiBackend::new(fixtures("pi")))],
        Selection::Id("malformed"),
        &[SpanBounds::Requests {
            first: None,
            last: None,
        }],
    )
    .unwrap();
    let span = serde_json::to_value(&answer.spans[0]).unwrap();
    assert!(span["span"]["unplaced"].as_u64().unwrap() > 0, "{span}");
    assert!(
        reasons(&span["measurement_completeness"], "output")
            .contains(&"unreadable-record".to_owned()),
        "{span}"
    );
    assert!(
        reasons(&span["consumption"]["measurement_completeness"], "output")
            .contains(&"unreadable-record".to_owned()),
        "the consumption beside an unplaced record: {}",
        span["consumption"]
    );
}

fn table(rows: &str) -> PriceTable {
    PriceTable::parse(&format!(
        "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n{rows}"
    ))
    .unwrap()
}

fn amount(estimate: &Value) -> f64 {
    estimate["amount"]
        .as_f64()
        .unwrap_or_else(|| panic!("an amount: {estimate}"))
}

/// Claim: pi consumption over two branches served by different models is
/// priced share by share, each model's events by its own row: model A's 100
/// output tokens at 10 per million and the abandoned branch's model B's 100
/// at 1 per million come to 0.0011. Rejects pricing every retained branch
/// under the active branch's model, which gives 0.002.
#[test]
fn pi_consumption_on_another_branch_is_priced_by_its_own_model() {
    let store = Store::new("pi-branches");
    let id = "60000000-0000-4000-8000-0000000000c1";
    let assistant = |entry: &str, stamp: &str, model: &str| {
        json!({"type": "message", "id": entry, "parentId": "user-1", "timestamp": stamp,
            "message": {"role": "assistant", "timestamp": 0, "provider": "openai-codex", "model": model,
                "usage": {"input": 0, "output": 100, "reasoning": 0, "cacheRead": 0, "cacheWrite": 0,
                          "totalTokens": 100},
                "content": [{"type": "text", "text": "Done."}]}})
    };
    store.write(
        &format!("--fixtures-project--/2026-01-03T10-00-00-000Z_{id}.jsonl"),
        &[
            json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-01-03T10:00:00Z",
                   "cwd": "/fixtures/project"}),
            json!({"type": "message", "id": "user-1", "parentId": null, "timestamp": "2026-01-03T10:00:01Z",
                   "message": {"role": "user", "timestamp": 0, "content": [{"type": "text", "text": "Go."}]}}),
            assistant("abandoned-1", "2026-01-03T10:00:02Z", "model-b"),
            assistant("assistant-1", "2026-01-03T10:00:03Z", "model-a"),
        ],
    );
    let mut view =
        usage_full_with_backends(&[Box::new(PiBackend::new(&store.0))], Selection::Id(id)).unwrap();
    let prices = table(
        "[[models]]\nid = \"model-a\"\neffective = 2026-01-01\nsource = \"fixture\"\n\
         per_million = { uncached_input = 1.0, cache_read = 0.1, output = 10.0 }\n\
         [[models]]\nid = \"model-b\"\neffective = 2026-01-01\nsource = \"fixture\"\n\
         per_million = { uncached_input = 1.0, cache_read = 0.1, output = 1.0 }\n",
    );
    pricing::price_view(&mut view, &prices);
    let consumption = serde_json::to_value(view.consumption.as_ref().unwrap()).unwrap();
    assert_eq!(consumption["events"]["other_branches"], 1);
    let estimate = &consumption["list_price_estimate"];
    assert!(
        (amount(estimate) - 0.0011).abs() < 1e-12,
        "consumption priced share by share: {estimate}"
    );
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

fn estimate_of(view: &UsageView) -> Value {
    serde_json::to_value(view).unwrap()["list_price_estimate"].clone()
}

/// Claim: a Codex span holding a request made before any model context
/// and a request under model A prices only A's share, and names the rest
/// `incomplete-attribution`, so its amount is a known subtotal: 100 output
/// tokens at 10 per million. Rejects pricing all 200 tokens as A.
#[test]
fn a_codex_span_with_an_unattributed_request_prices_only_what_it_attributes() {
    let store = Store::new("codex-unattributed");
    let id = "70000000-0000-7000-8000-0000000000d2";
    store.write(
        &format!("2026/01/11/rollout-2026-01-11T11-00-00-{id}.jsonl"),
        &[
            codex_meta(id),
            codex_record(id, "2026-01-11T11:00:01Z", "resp_unknown_1", 0, 100),
            codex_count("2026-01-11T11:00:01.100Z", (0, 100), (0, 100)),
            codex_context("2026-01-11T11:00:02Z", "model-a"),
            codex_record(id, "2026-01-11T11:00:03Z", "resp_model_a_1", 0, 100),
            codex_count("2026-01-11T11:00:03.100Z", (0, 200), (0, 100)),
        ],
    );
    let mut spans = usage_spans_with_backends(
        &[Box::new(CodexBackend::new(&store.0))],
        Selection::Id(id),
        &[SpanBounds::Requests {
            first: None,
            last: None,
        }],
    )
    .unwrap();
    let prices = table(
        "[[models]]\nid = \"model-a\"\neffective = 2026-01-01\nsource = \"fixture\"\n\
         per_million = { uncached_input = 1.0, cache_read = 0.1, output = 10.0 }\n",
    );
    pricing::price_spans(&mut spans, &prices);
    let span = &spans.spans[0];
    assert_eq!(span.tokens.as_ref().unwrap().output, Some(200));
    let estimate = estimate_of(span);
    assert!(
        (amount(&estimate) - 0.001).abs() < 1e-12,
        "only model A's 100 output tokens are priced: {estimate}"
    );
    assert!(
        reasons(&estimate["measurement_completeness"], "output")
            .contains(&"incomplete-attribution".to_owned()),
        "{estimate}"
    );
}

/// Claim: a price table that names a FIFO is refused as not a regular file
/// without waiting for a writer. Rejects opening it first, which blocks
/// until something writes.
#[test]
fn a_price_table_that_is_a_fifo_is_refused_without_waiting() {
    let store = Store::new("prices-fifo");
    let path = store.0.join("prices.toml");
    let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (sender, receiver) = mpsc::channel();
    let named = path.clone();
    std::thread::spawn(move || {
        let _ = sender.send(PriceTable::read(&named).map(|_| ()));
    });
    let read = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("reading the price table blocked on a FIFO with no writer");
    let error = format!("{:#}", read.expect_err("a FIFO is no price table"));
    assert!(error.contains("not a"), "{error}");
}
