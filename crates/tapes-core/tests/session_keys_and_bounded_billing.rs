//! A `--by session` group names its session by harness and id together, so
//! sessions of different harnesses that share an id stay apart; and a bounded
//! read that leaves part of its recording unread establishes no billing, so a
//! bounded `usage` view, a listing or a summary is never more certain of a
//! recording's billing than its whole read.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_rates_with_backends, list_with_backends, usage_full_with_backends,
    usage_summary_priced, usage_summary_with_backends, usage_window_with_backends,
    usage_with_backends, Selection, SessionSelection, WindowAsk,
};
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
            "tapes-session-keys-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
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

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.0.join("claude/projects"))),
            Box::new(CodexBackend::new(self.0.join("codex"))),
        ]
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

fn selection() -> SessionSelection<'static> {
    SessionSelection {
        limit: Some(50),
        ..Default::default()
    }
}

/// 64 KiB lines enough to carry a recording past the 4 MiB bounded read.
fn filler_lines(count: usize, line: impl Fn(usize, &str) -> Value) -> Vec<Value> {
    let filler = "x".repeat(64 * 1024);
    (0..count).map(|index| line(index, &filler)).collect()
}

// --- Session groups keyed by harness and id ---------------------------------

/// One id a Claude session and a Codex session both carry.
const SHARED: &str = "01a10000-0000-7000-8000-0000000000d1";

fn claude_session(store: &Store, id: &str) {
    store.write(
        &format!("claude/projects/-fixtures-project/{id}.jsonl"),
        &[
            json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"),
                "entrypoint": "sdk-cli", "timestamp": "2026-01-10T10:00:01Z",
                "cwd": "/fixtures/project",
                "message": {"role": "user", "content": [{"type": "text", "text": "Lead."}]}}),
            json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"),
                "entrypoint": "sdk-cli", "requestId": format!("{id}-request"),
                "timestamp": "2026-01-10T10:00:30Z", "cwd": "/fixtures/project",
                "message": {"role": "assistant", "model": "claude-fixture",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input_tokens": 1, "output_tokens": 5,
                        "cache_read_input_tokens": 10, "cache_creation_input_tokens": 2}}}),
        ],
    );
}

fn codex_usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "output_tokens": output,
           "reasoning_output_tokens": 0, "total_tokens": input + output})
}

fn codex_quota(stamp: &str, used: f64) -> Value {
    json!({"timestamp": stamp, "type": "event_msg",
           "payload": {"type": "token_count", "info": null, "rate_limits": {
               "limit_id": "codex", "plan_type": "plus",
               "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_768_053_600},
               "secondary": null}}})
}

fn codex_session(store: &Store, id: &str) {
    let start = "2026-01-10T10:01:00Z";
    store.write(
        &format!("codex/2026/01/10/rollout-2026-01-10T10-01-00-{id}.jsonl"),
        &[
            json!({"timestamp": start, "type": "session_meta",
                "payload": {"id": id, "timestamp": start, "cwd": "/fixtures/project",
                    "source": "exec", "originator": "codex_exec"}}),
            json!({"timestamp": start, "type": "turn_context",
                "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
            json!({"timestamp": start, "type": "response_item",
                "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "Implement."}]}}),
            codex_quota("2026-01-10T10:02:00Z", 10.0),
            json!({"timestamp": "2026-01-10T10:05:00Z", "type": "event_msg",
                "payload": {"type": "token_count", "info": {
                    "total_token_usage": codex_usage(100, 7),
                    "last_token_usage": codex_usage(100, 7), "model_context_window": 272000},
                    "rate_limits": null}}),
            codex_quota("2026-01-10T10:10:00Z", 15.0),
        ],
    );
}

/// Each session group's key, as its harness and id, and its session count
/// and input tokens.
fn session_groups(summary: &Value) -> BTreeMap<(String, String), (u64, u64)> {
    summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            let key = &group["key"];
            (
                (
                    key["session_harness"].as_str().unwrap_or("-").to_owned(),
                    key["session"].as_str().unwrap().to_owned(),
                ),
                (
                    group["sessions"].as_u64().unwrap(),
                    group["tokens"]["input"].as_u64().unwrap_or(0),
                ),
            )
        })
        .collect()
}

/// Claim: grouping by session keys each group by the session's harness and
/// id, as `session` and `session_harness`, in a summary and in a window read,
/// so a Claude session and a Codex session sharing an id are two groups of
/// one session each, under tapes-usage-summary/16. Rejects a group keyed by
/// the bare id that sums the two sessions' tokens as one.
#[test]
fn sessions_sharing_an_id_across_harnesses_are_separate_groups() {
    let store = Store::new("summary");
    claude_session(&store, SHARED);
    codex_session(&store, SHARED);
    let backends = store.backends();
    let expected = BTreeMap::from([
        (("claude".to_owned(), SHARED.to_owned()), (1, 1)),
        (("codex".to_owned(), SHARED.to_owned()), (1, 100)),
    ]);

    let summary = serde_json::to_value(
        usage_summary_with_backends(&backends, &selection(), &[GroupBy::Session]).unwrap(),
    )
    .unwrap();
    assert_eq!(session_groups(&summary), expected, "{summary:#}");
    assert_eq!(summary["schema"], "tapes-usage-summary/16", "{summary:#}");

    let windowed = serde_json::to_value(
        usage_window_with_backends(
            &backends,
            &selection(),
            &[GroupBy::Session],
            WindowAsk::Bounds(WindowBounds {
                from: Some(at("2026-01-10T09:00:00Z")),
                to: None,
            }),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(session_groups(&windowed), expected, "{windowed:#}");
}

/// Claim: an allowance-rate group by session names its session by harness
/// and id, under tapes-allowance-rates/6, so the lane a Codex session ran is
/// not read as the Claude session of the same id. Rejects a rate group whose
/// key is the bare id.
#[test]
fn allowance_rate_groups_name_a_session_by_harness_and_id() {
    let store = Store::new("rates");
    claude_session(&store, SHARED);
    codex_session(&store, SHARED);
    let rates = serde_json::to_value(
        allowance_rates_with_backends(
            &store.backends(),
            &selection(),
            &[GroupBy::Session],
            WindowBounds {
                from: Some(at("2026-01-10T09:30:00Z")),
                to: Some(at("2026-01-10T11:00:00Z")),
            },
            QuotaKind::Primary,
        )
        .unwrap(),
    )
    .unwrap();
    let keys = rates["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["key"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [json!({"session": SHARED, "session_harness": "codex"})],
        "{rates:#}"
    );
    assert_eq!(rates["schema"], "tapes-allowance-rates/6", "{rates:#}");
}

// --- Billing of a bounded read ------------------------------------------------

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

const PI_ID: &str = "019f0000-0000-7000-8000-0000000000d2";
const PI_SMALL_ID: &str = "019f0000-0000-7000-8000-0000000000d3";
const CLAUDE_ID: &str = "30000000-0000-4000-8000-0000000000d4";
const CODEX_ID: &str = "01a10000-0000-7000-8000-0000000000d5";

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

fn pi_chatgpt_reply(id: &str, parent: &str) -> Value {
    json!({"type": "message", "id": id, "parentId": parent,
        "timestamp": "2026-10-19T10:30:00.000Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": "Done."}],
                    "api": "openai-codex-responses", "provider": "openai-codex",
                    "model": "gpt-6-fixture", "usage": pi_usage(100_000, 200_000, 1_000),
                    "stopReason": "stop", "timestamp": 1_791_000_000_000_u64,
                    "responseId": format!("resp_{id}")}})
}

fn pi_opening(id: &str) -> Vec<Value> {
    vec![
        json!({"type": "session", "version": 3, "id": id,
               "timestamp": "2026-10-19T10:00:00.000Z", "cwd": "/fixtures/project"}),
        json!({"type": "model_change", "id": "model-1", "parentId": null,
               "timestamp": "2026-10-19T10:00:00.000Z", "provider": "openai-codex",
               "modelId": "gpt-6-fixture"}),
        pi_user("user-1", "model-1", "2026-10-19T10:00:01.000Z", "Survey."),
    ]
}

/// A pi recording past the bounded read: a compaction that carries tokens
/// and names no API, six MiB of turns, and then one 300K-prompt request
/// through the ChatGPT sign-in's API, the only token-carrying event the
/// bounded read's tail holds.
fn pi_compacted_session(store: &Store) {
    let mut lines = pi_opening(PI_ID);
    lines.push(
        json!({"type": "compaction", "id": "compaction-1", "parentId": "user-1",
               "timestamp": "2026-10-19T10:01:00.000Z", "summary": "Summary.",
               "firstKeptEntryId": "user-1", "tokensBefore": 300_000,
               "usage": pi_usage(5_000, 0, 500), "fromHook": false}),
    );
    let fillers = filler_lines(96, |index, filler| {
        let parent = match index {
            0 => "compaction-1".to_owned(),
            index => format!("filler-{}", index - 1),
        };
        pi_user(
            &format!("filler-{index}"),
            &parent,
            "2026-10-19T10:10:00.000Z",
            filler,
        )
    });
    lines.extend(fillers);
    lines.push(pi_chatgpt_reply("assistant-1", "filler-95"));
    store.write(
        &format!("--fixtures-project--/2026-10-19T10-00-00-000Z_{PI_ID}.jsonl"),
        &lines,
    );
}

/// The list-price estimate's billing terms and evidence, and whether it
/// priced the figure as one amount.
fn billing(estimate: &Value) -> (String, String, bool) {
    let billing = &estimate["billing"][0];
    (
        billing["terms"].as_str().unwrap_or("-").to_owned(),
        billing["evidence"].as_str().unwrap_or("-").to_owned(),
        estimate.get("amount").is_some(),
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

/// Asserts a bounded read's estimate names its billing unestablished, with
/// evidence naming what its read window shows, and prices no more certainly
/// than the whole read, which names the billing unestablished too.
fn assert_no_more_certain(read: &str, bounded: &Value, whole: &Value, shown: &str) {
    let (terms, evidence, priced) = billing(bounded);
    assert_eq!(terms, "unestablished", "{read}: {bounded:#}");
    assert!(
        evidence.contains("read window") && evidence.contains(shown),
        "{read}: {bounded:#}"
    );
    let (whole_terms, _, whole_priced) = billing(whole);
    assert_eq!(whole_terms, "unestablished", "--full: {whole:#}");
    assert!(
        !priced || whole_priced,
        "{read}: {bounded:#}\n--full: {whole:#}"
    );
}

/// Claim: the bounded `usage` view, a listing and a summary of a pi
/// recording past the bounded read, whose compaction before the tail carries
/// tokens through no API and whose tail holds only a ChatGPT request, name
/// the billing unestablished as `--full` does, saying the read window shows
/// a subscription, and price the 300K-prompt request between base and
/// surcharged rates. Rejects the bounded read naming the subscription its
/// tail suggests and pricing the request at base rates as one amount.
#[test]
fn a_bounded_pi_read_is_no_more_certain_of_billing_than_its_whole_recording() {
    let store = Store::new("pi-bounded-billing");
    pi_compacted_session(&store);
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(PiBackend::new(&store.0)) };
    let whole = whole_estimate(backend(), PI_ID, &table);

    let bounded = bounded_estimate(backend(), PI_ID, &table);
    assert_no_more_certain("usage", &bounded, &whole, "subscription");

    let mut listed = list_with_backends(&[backend()], None, None, 50).unwrap();
    let session = &mut listed.sessions[0];
    session.price(&table);
    let listed = serde_json::to_value(&session.list_price_estimate).unwrap();
    assert_no_more_certain("list", &listed, &whole, "subscription");

    let summary = usage_summary_priced(&[backend()], &selection(), &[], Some(&table)).unwrap();
    let summary = serde_json::to_value(summary).unwrap()["totals"]["list_price_estimate"].clone();
    assert_no_more_certain("summary", &summary, &whole, "subscription");
}

/// Claim: a pi recording the bounded read holds whole, every token-carrying
/// event a ChatGPT request, still bills as a subscription in the bounded
/// view. Rejects a bounded read declining a billing its records establish
/// when nothing went unread.
#[test]
fn a_bounded_read_of_a_whole_recording_keeps_its_billing() {
    let store = Store::new("pi-whole-billing");
    let mut lines = pi_opening(PI_SMALL_ID);
    lines.push(pi_chatgpt_reply("assistant-1", "user-1"));
    store.write(
        &format!("--fixtures-project--/2026-10-19T10-00-00-000Z_{PI_SMALL_ID}.jsonl"),
        &lines,
    );
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let bounded = bounded_estimate(Box::new(PiBackend::new(&store.0)), PI_SMALL_ID, &table);
    let (terms, _, priced) = billing(&bounded);
    assert_eq!(terms, "subscription", "{bounded:#}");
    assert!(priced, "{bounded:#}");
}

fn claude_user(entry: &str, stamp: &str, text: &str) -> Value {
    json!({"type": "user", "sessionId": CLAUDE_ID, "uuid": entry, "parentUuid": null,
           "timestamp": stamp, "cwd": "/fixtures/project",
           "message": {"role": "user", "content": text}})
}

fn claude_api_request(stamp: &str, betas: &[&str]) -> Value {
    json!({"type": "api-request", "sessionId": CLAUDE_ID, "timestamp": stamp,
           "params": {"model": "claude-haiku-5-5", "betas": betas}})
}

/// Claim: the bounded `usage` view of a Claude recording past the bounded
/// read, which sent an API-key request before the tail and an OAuth request
/// in it, names the billing unestablished as `--full` does, saying the read
/// window shows a subscription, and prices its 110K-prompt request between
/// base and surcharged rates. Rejects the subscription the tail's OAuth
/// request alone suggests.
#[test]
fn a_bounded_claude_read_is_no_more_certain_of_billing_than_its_whole_recording() {
    let store = Store::new("claude-bounded-billing");
    let mut lines = vec![
        claude_user("user-1", "2026-10-19T10:00:00Z", "Survey the fixture."),
        claude_api_request("2026-10-19T10:00:01Z", &["claude-code-20250219"]),
    ];
    lines.extend(filler_lines(96, |index, filler| {
        claude_user(&format!("filler-{index}"), "2026-10-19T10:10:00Z", filler)
    }));
    lines.push(claude_api_request(
        "2026-10-19T10:29:59Z",
        &["claude-code-20250219", "oauth-2025-04-20"],
    ));
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
    assert_no_more_certain("usage", &bounded, &whole, "subscription");
}

/// Claim: the bounded `usage` view of a Codex rollout past the bounded read,
/// whose opening names a model provider other than `openai` and whose quota
/// window before the tail reports a used percent, names the billing
/// unestablished as `--full` does, saying the read window shows per-token
/// billing. Rejects the per-token billing the opening's provider alone
/// suggests once the quota window falls outside the read.
#[test]
fn a_bounded_codex_read_is_no_more_certain_of_billing_than_its_whole_recording() {
    let store = Store::new("codex-bounded-billing");
    let start = "2026-10-19T10:00:00Z";
    let mut lines = vec![
        json!({"timestamp": start, "type": "session_meta",
            "payload": {"id": CODEX_ID, "timestamp": start, "cwd": "/fixtures/project",
                "source": "exec", "originator": "codex_exec",
                "model_provider": "fixture-provider"}}),
        json!({"timestamp": start, "type": "turn_context",
            "payload": {"cwd": "/fixtures/project", "model": "gpt-6-fixture"}}),
        codex_quota("2026-10-19T10:01:00Z", 10.0),
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
    lines.push(
        json!({"timestamp": "2026-10-19T10:30:00Z", "type": "event_msg",
        "payload": {"type": "token_count", "info": {
            "total_token_usage": codex_usage(300_000, 1_000),
            "last_token_usage": codex_usage(300_000, 1_000), "model_context_window": 400000},
            "rate_limits": null}}),
    );
    store.write(
        &format!("codex/2026/10/19/rollout-2026-10-19T10-00-00-{CODEX_ID}.jsonl"),
        &lines,
    );
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(CodexBackend::new(store.0.join("codex"))) };
    let whole = whole_estimate(backend(), CODEX_ID, &table);
    let bounded = bounded_estimate(backend(), CODEX_ID, &table);
    assert_no_more_certain("usage", &bounded, &whole, "per-token");
}
