//! A span bills as its whole recording even where a bounded read of it
//! would establish a billing, and a recorded-activity --since rejects a
//! parent reached through its children before the parent takes a place
//! under --limit.

use std::fs::{self, File};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, pi::PiBackend, Backend};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_rates_with_backends, usage_full_with_backends, usage_spans_with_backends,
    usage_with_backends, ListFilters, Selection, SessionSelection,
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

// --- --since before --limit for parents reached through children ------------

fn subagent_record(parent: &str, agent: Option<&str>, entry: &str, stamp: &str) -> Value {
    let mut record = json!({
        "sessionId": parent, "cwd": "/fixtures/project", "entrypoint": "cli", "version": "2.1.0",
        "type": "assistant", "uuid": entry, "parentUuid": null, "requestId": format!("req_{entry}"),
        "timestamp": stamp,
        "message": {"id": format!("msg_{entry}"), "role": "assistant", "model": "claude-fixture",
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                      "output_tokens": 50},
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "Done."}]}
    });
    if let Some(agent) = agent {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(agent);
    }
    record
}

/// A quiet parent, its records ending at 09:59, whose one subagent's
/// records end at `child_end`; the parent's recording is stamped `age`
/// before now, so a younger parent is nominated first.
fn quiet_parent(store: &Store, parent: &str, agent: &str, child_end: &str, age: u64) {
    let recording = store.write(
        &format!("project/{parent}.jsonl"),
        &[subagent_record(
            parent,
            None,
            &format!("{parent}-1"),
            "2026-01-12T09:59:00Z",
        )],
    );
    store.write(
        &format!("project/{parent}/subagents/agent-{agent}.jsonl"),
        &[subagent_record(
            parent,
            Some(agent),
            &format!("{agent}-1"),
            child_end,
        )],
    );
    File::options()
        .write(true)
        .open(recording)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(age))
        .unwrap();
}

/// Claim: under --since 10:30 and --limit 1, allowance rates over
/// 10:00..11:00 read the subagent of quiet parent Q, whose records end at
/// 10:45, and leave nothing out by the limit, though quiet parent P, whose
/// subagent's records end at 10:05, is nominated first. Rejects counting P
/// under the limit before --since rejects it, which leaves Q out by the
/// limit and neither family read.
#[test]
fn a_parent_since_rejects_takes_no_place_under_the_limit() {
    let store = Store::new("since-limit");
    let (p, q) = (
        "30000000-0000-4000-8000-0000000000e1",
        "30000000-0000-4000-8000-0000000000e2",
    );
    let (p_agent, q_agent) = ("a000000000000000e1", "a000000000000000e2");
    quiet_parent(&store, p, p_agent, "2026-01-12T10:05:00Z", 60);
    quiet_parent(&store, q, q_agent, "2026-01-12T10:45:00Z", 120);
    let selection = SessionSelection {
        harness: Some("claude"),
        limit: Some(1),
        filters: ListFilters {
            since: Some(at("2026-01-12T10:30:00Z")),
            ..ListFilters::default()
        },
        ..Default::default()
    };
    let rates = serde_json::to_value(
        allowance_rates_with_backends(
            &[Box::new(ClaudeBackend::new(&store.0)) as Box<dyn Backend>],
            &selection,
            &[GroupBy::Session],
            WindowBounds {
                from: Some(at("2026-01-12T10:00:00Z")),
                to: Some(at("2026-01-12T11:00:00Z")),
            },
            QuotaKind::Primary,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        rates["lanes_read"].get("left_out_by_limit").is_none(),
        "{rates:#}"
    );
    let absent = rates["excluded_absent"]
        .as_array()
        .unwrap_or_else(|| panic!("{rates:#}"))
        .iter()
        .map(|lane| lane.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(absent.len(), 1, "{rates:#}");
    assert!(
        absent[0].starts_with(&format!("{q}::{q_agent} (claude)")),
        "{rates:#}"
    );
}
