//! A bounded read that leaves part of its recording unread and whose read
//! window names no billing says the read window names none, not that the
//! recording does: the records it skipped could name one.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::{
    list_with_backends, usage_full_with_backends, usage_summary_priced, usage_with_backends,
    Selection, SessionSelection,
};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

/// The evidence of a bounded read whose window names no billing.
const WINDOW_NAMES_NONE: &str =
    "the read window names no billing, and records outside it went unread";
/// The evidence of a whole read of a recording that names no billing.
const RECORDING_NAMES_NONE: &str = "the recording names no billing";

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-bounded-wording-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(&path, body).unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 64 KiB lines enough to carry a recording past the 4 MiB bounded read.
fn filler_lines(count: usize, line: impl Fn(usize, &str) -> Value) -> Vec<Value> {
    let filler = "x".repeat(64 * 1024);
    (0..count).map(|index| line(index, &filler)).collect()
}

const GPT_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "gpt-6-fixture"
effective = 2026-01-01
source = "fixture"
per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.5, output = 10.0 }

[models.long_context]
above_prompt_tokens = 272000
prompt_tokens = ["uncached_input", "cache_read", "cache_write"]
multipliers = { uncached_input = 2.0, cache_read = 2.0, cache_write_5m = 2.0, output = 1.5 }
"#;

const HAIKU_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "claude-haiku-5-5"
effective = 2026-01-01
source = "fixture"
per_million = { uncached_input = 0.10, cache_read = 0.01, cache_write_5m = 0.125, cache_write_1h = 0.20, output = 0.50 }

[models.long_context]
above_prompt_tokens = 100000
prompt_tokens = ["uncached_input", "cache_read", "cache_write"]
multipliers = { uncached_input = 5.0, cache_read = 5.0, cache_write_5m = 5.0, cache_write_1h = 5.0, output = 5.0 }
"#;

const PI_ID: &str = "019f0000-0000-7000-8000-0000000000e1";
const CLAUDE_ID: &str = "30000000-0000-4000-8000-0000000000e2";
const CODEX_ID: &str = "01a10000-0000-7000-8000-0000000000e3";

/// The billing an estimate names: its terms and evidence.
fn billing(estimate: &Value) -> (String, String) {
    let billing = &estimate["billing"][0];
    (
        billing["terms"].as_str().unwrap_or("-").to_owned(),
        billing["evidence"].as_str().unwrap_or("-").to_owned(),
    )
}

fn bounded_estimate(backend: Box<dyn Backend>, id: &str, table: &PriceTable) -> Value {
    let mut view = usage_with_backends(&[backend], Selection::Id(id)).unwrap();
    pricing::price_view(&mut view, table);
    serde_json::to_value(view).unwrap()["list_price_estimate"].clone()
}

fn whole_estimate(backend: Box<dyn Backend>, id: &str, table: &PriceTable) -> Value {
    let mut view = usage_full_with_backends(&[backend], Selection::Id(id)).unwrap();
    pricing::price_view(&mut view, table);
    serde_json::to_value(view).unwrap()["list_price_estimate"].clone()
}

/// Asserts a bounded read names its billing unestablished because its window
/// names none, and the whole read because the recording names none.
fn assert_wording(read: &str, bounded: &Value, whole: &Value) {
    assert_eq!(
        billing(bounded),
        ("unestablished".to_owned(), WINDOW_NAMES_NONE.to_owned()),
        "{read}: {bounded:#}"
    );
    assert_eq!(
        billing(whole),
        ("unestablished".to_owned(), RECORDING_NAMES_NONE.to_owned()),
        "--full: {whole:#}"
    );
}

fn pi_usage(input: u64, read: u64, output: u64) -> Value {
    json!({"input": input, "output": output, "cacheRead": read, "cacheWrite": 0, "reasoning": 0,
           "totalTokens": input + read + output,
           "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}})
}

fn pi_user(id: &str, parent: &str, text: &str) -> Value {
    json!({"type": "message", "id": id, "parentId": parent,
           "timestamp": "2026-10-19T10:10:00.000Z",
           "message": {"role": "user", "content": [{"type": "text", "text": text}],
                       "timestamp": 1_791_000_000_000_u64}})
}

/// A pi recording past the bounded read whose token-carrying events, in the
/// read's tail, are a request through an API other than the ChatGPT
/// sign-in's and a compaction, which names no provider, so neither the
/// window nor the whole recording names a billing.
fn pi_session(store: &Store) {
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": PI_ID,
               "timestamp": "2026-10-19T10:00:00.000Z", "cwd": "/fixtures/project"}),
        json!({"type": "model_change", "id": "model-1", "parentId": null,
               "timestamp": "2026-10-19T10:00:00.000Z", "provider": "fixture-provider",
               "modelId": "gpt-6-fixture"}),
        pi_user("user-1", "model-1", "Survey."),
    ];
    lines.extend(filler_lines(96, |index, filler| {
        let parent = match index {
            0 => "user-1".to_owned(),
            index => format!("filler-{}", index - 1),
        };
        pi_user(&format!("filler-{index}"), &parent, filler)
    }));
    lines.push(
        json!({"type": "message", "id": "assistant-1", "parentId": "filler-95",
        "timestamp": "2026-10-19T10:30:00.000Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "Done."}],
                    "api": "openai-completions", "provider": "fixture-provider",
                    "model": "gpt-6-fixture", "usage": pi_usage(100_000, 200_000, 1_000),
                    "stopReason": "stop", "timestamp": 1_791_000_000_000_u64,
                    "responseId": "resp_assistant-1"}}),
    );
    lines.push(
        json!({"type": "compaction", "id": "compaction-1", "parentId": "assistant-1",
        "timestamp": "2026-10-19T10:35:00.000Z", "summary": "Summary.",
        "firstKeptEntryId": "assistant-1", "tokensBefore": 300_000, "details": {},
        "usage": pi_usage(5_000, 0, 500), "fromHook": false}),
    );
    store.write(
        &format!("--fixtures-project--/2026-10-19T10-00-00-000Z_{PI_ID}.jsonl"),
        &lines,
    );
}

/// Claim: the bounded `usage` view, a listing and a summary of a pi
/// recording past the bounded read whose window names no billing say the
/// read window names none and records outside it went unread, while
/// `--full` says the recording names none. Rejects a bounded read claiming
/// the whole recording names no billing, and a whole read claiming a window.
#[test]
fn a_bounded_pi_read_says_its_window_names_no_billing() {
    let store = Store::new("pi");
    pi_session(&store);
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(PiBackend::new(&store.0)) };
    let whole = whole_estimate(backend(), PI_ID, &table);

    let bounded = bounded_estimate(backend(), PI_ID, &table);
    assert_wording("usage", &bounded, &whole);

    let mut listed = list_with_backends(&[backend()], None, None, 50).unwrap();
    let session = &mut listed.sessions[0];
    session.price(&table);
    let listed = serde_json::to_value(&session.list_price_estimate).unwrap();
    assert_wording("list", &listed, &whole);

    let selection = SessionSelection {
        limit: Some(50),
        ..Default::default()
    };
    let summary = usage_summary_priced(&[backend()], &selection, &[], Some(&table)).unwrap();
    let summary = serde_json::to_value(summary).unwrap()["totals"]["list_price_estimate"].clone();
    assert_wording("summary", &summary, &whole);
}

fn claude_user(entry: &str, text: &str) -> Value {
    json!({"type": "user", "sessionId": CLAUDE_ID, "uuid": entry, "parentUuid": null,
           "timestamp": "2026-10-19T10:10:00Z", "cwd": "/fixtures/project",
           "message": {"role": "user", "content": text}})
}

/// Claim: the bounded `usage` view of a Claude recording past the bounded
/// read that records no API request says the read window names no billing,
/// while `--full` says the recording names none. Rejects a bounded read
/// claiming the whole recording names no billing.
#[test]
fn a_bounded_claude_read_says_its_window_names_no_billing() {
    let store = Store::new("claude");
    let mut lines = vec![claude_user("user-1", "Survey the fixture.")];
    lines.extend(filler_lines(96, |index, filler| {
        claude_user(&format!("filler-{index}"), filler)
    }));
    lines.push(
        json!({"type": "assistant", "sessionId": CLAUDE_ID, "uuid": "assistant-large",
        "parentUuid": "user-1", "timestamp": "2026-10-19T10:30:00Z", "cwd": "/fixtures/project",
        "requestId": "req-large",
        "message": {"id": "msg-large", "role": "assistant", "model": "claude-haiku-5-5",
                    "content": [{"type": "text", "text": "Done."}],
                    "usage": {"input_tokens": 20_000, "cache_read_input_tokens": 80_000,
                              "cache_creation_input_tokens": 10_000, "output_tokens": 2_000,
                              "cache_creation": {"ephemeral_5m_input_tokens": 10_000,
                                                 "ephemeral_1h_input_tokens": 0}}}}),
    );
    store.write(&format!("-fixtures-project/{CLAUDE_ID}.jsonl"), &lines);
    let table = PriceTable::parse(HAIKU_TABLE).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(ClaudeBackend::new(&store.0)) };
    let whole = whole_estimate(backend(), CLAUDE_ID, &table);
    let bounded = bounded_estimate(backend(), CLAUDE_ID, &table);
    assert_wording("usage", &bounded, &whole);
}

/// Claim: the bounded `usage` view of a Codex rollout past the bounded read,
/// served by the `openai` provider and reporting no quota window, says the
/// read window names no billing, while `--full` says the recording names
/// none. Rejects a bounded read claiming the whole recording names no
/// billing.
#[test]
fn a_bounded_codex_read_says_its_window_names_no_billing() {
    let store = Store::new("codex");
    let start = "2026-10-19T10:00:00Z";
    let mut lines = vec![
        json!({"timestamp": start, "type": "session_meta",
            "payload": {"id": CODEX_ID, "timestamp": start, "cwd": "/fixtures/project",
                "source": "exec", "originator": "codex_exec", "model_provider": "openai"}}),
        json!({"timestamp": start, "type": "turn_context",
            "payload": {"cwd": "/fixtures/project", "model": "gpt-6-fixture"}}),
    ];
    lines.extend(filler_lines(96, |_, filler| {
        json!({"timestamp": "2026-10-19T10:10:00Z", "type": "response_item",
            "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": filler}]}})
    }));
    lines.push(
        json!({"timestamp": "2026-10-19T10:29:00Z", "type": "turn_context",
        "payload": {"cwd": "/fixtures/project", "model": "gpt-6-fixture"}}),
    );
    let usage = json!({"input_tokens": 300_000, "cached_input_tokens": 0,
        "output_tokens": 1_000, "reasoning_output_tokens": 0, "total_tokens": 301_000});
    lines.push(
        json!({"timestamp": "2026-10-19T10:30:00Z", "type": "event_msg",
        "payload": {"type": "token_count", "info": {
            "total_token_usage": usage, "last_token_usage": usage,
            "model_context_window": 400000},
            "rate_limits": null}}),
    );
    store.write(
        &format!("2026/10/19/rollout-2026-10-19T10-00-00-{CODEX_ID}.jsonl"),
        &lines,
    );
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(CodexBackend::new(&store.0)) };
    let whole = whole_estimate(backend(), CODEX_ID, &table);
    let bounded = bounded_estimate(backend(), CODEX_ID, &table);
    assert_wording("usage", &bounded, &whole);
}
