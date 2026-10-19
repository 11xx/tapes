//! A span bills as its whole recording even where a bounded read of it
//! would establish a billing.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{pi::PiBackend, Backend};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::{
    usage_full_with_backends, usage_spans_with_backends, usage_with_backends, Selection,
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
            "tapes-review4-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(&path, body).unwrap();
        path
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

// --- Span billing past the bounded read -------------------------------------

const GPT_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "gpt-6-fixture"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.5, output = 10.0 }

[models.long_context]
above_prompt_tokens = 272000
prompt_tokens = ["uncached_input", "cache_read", "cache_write"]
multipliers = { uncached_input = 2.0, cache_read = 2.0, cache_write_5m = 2.0, output = 1.5 }
"#;

const PI_ID: &str = "019f0000-0000-7000-8000-0000000000e4";

fn pi_usage(input: u64, read: u64, output: u64) -> Value {
    json!({"input": input, "output": output, "cacheRead": read, "cacheWrite": 0, "reasoning": 0,
           "totalTokens": input + read + output,
           "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}})
}

fn pi_user(id: &str, parent: &str, stamp: &str, text: &str) -> Value {
    json!({"type": "message", "id": id, "parentId": parent, "timestamp": stamp,
           "message": {"role": "user", "content": [{"type": "text", "text": text}],
                       "timestamp": 1_791_000_000_000_u64}})
}

/// A pi recording past the bounded read: a compaction that carries tokens
/// and names no API, six MiB of turns, and then one 300K-prompt request
/// through the ChatGPT sign-in's API, the only token-carrying event the
/// bounded read's tail holds.
fn pi_compacted_session(store: &Store) {
    let filler = "x".repeat(64 * 1024);
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": PI_ID,
               "timestamp": "2026-10-19T10:00:00.000Z", "cwd": "/fixtures/project"}),
        json!({"type": "model_change", "id": "model-1", "parentId": null,
               "timestamp": "2026-10-19T10:00:00.000Z", "provider": "openai-codex",
               "modelId": "gpt-6-fixture"}),
        pi_user("user-1", "model-1", "2026-10-19T10:00:01.000Z", "Survey."),
        json!({"type": "compaction", "id": "compaction-1", "parentId": "user-1",
               "timestamp": "2026-10-19T10:01:00.000Z", "summary": "Summary.",
               "firstKeptEntryId": "user-1", "tokensBefore": 300_000,
               "usage": pi_usage(5_000, 0, 500), "fromHook": false}),
    ];
    let mut parent = "compaction-1".to_owned();
    for index in 0..96 {
        let id = format!("filler-{index}");
        lines.push(pi_user(&id, &parent, "2026-10-19T10:10:00.000Z", &filler));
        parent = id;
    }
    lines.push(
        json!({"type": "message", "id": "assistant-1", "parentId": parent,
        "timestamp": "2026-10-19T10:30:00.000Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "Done."}],
                    "api": "openai-codex-responses", "provider": "openai-codex",
                    "model": "gpt-6-fixture", "usage": pi_usage(100_000, 200_000, 1_000),
                    "stopReason": "stop", "timestamp": 1_791_000_000_000_u64,
                    "responseId": "resp_assistant-1"}}),
    );
    store.write(
        &format!("--fixtures-project--/2026-10-19T10-00-00-000Z_{PI_ID}.jsonl"),
        &lines,
    );
}

/// Claim: a pi recording past the bounded read, whose compaction before
/// the retained tail carries tokens through no API, establishes no billing
/// under --from covering the whole recording, as under --full: both price
/// its 300K-prompt ChatGPT request between base and surcharged rates and
/// name the billing unestablished. Rejects a span that, where the whole
/// recording establishes no billing, takes the subscription the bounded
/// read's tail alone suggests and prices the request at base rates.
#[test]
fn a_span_is_no_more_certain_of_billing_than_its_whole_recording() {
    let store = Store::new("pi-span-billing");
    pi_compacted_session(&store);
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(PiBackend::new(&store.0)) };
    // The bounded read's tail holds the ChatGPT request alone.
    let mut bounded = usage_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    pricing::price_view(&mut bounded, &table);
    let bounded = serde_json::to_value(bounded).unwrap()["list_price_estimate"].clone();
    assert_eq!(
        bounded["billing"][0]["terms"], "subscription",
        "{bounded:#}"
    );
    let mut whole = usage_full_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    pricing::price_view(&mut whole, &table);
    let whole = serde_json::to_value(whole).unwrap()["list_price_estimate"].clone();
    let mut spans = usage_spans_with_backends(
        &[backend()],
        Selection::Id(PI_ID),
        &[SpanBounds::Time {
            from: Some(at("2026-10-19T10:00:00Z")),
            to: None,
        }],
    )
    .unwrap();
    pricing::price_spans(&mut spans, &table);
    let span = serde_json::to_value(&spans.spans[0]).unwrap()["list_price_estimate"].clone();
    for (read, estimate) in [("--full", whole), ("--from", span)] {
        assert_eq!(
            estimate["billing"][0]["terms"], "unestablished",
            "{read}: {estimate:#}"
        );
        assert!(estimate.get("amount").is_none(), "{read}: {estimate:#}");
    }
}
