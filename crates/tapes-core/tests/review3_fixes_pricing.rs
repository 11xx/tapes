//! A window read keeps a parent reached through its children only by
//! recorded activity since --since; allowance rates keep every lane either
//! estimator gives a percent; a forecast sizes a plan by lanes measured
//! under the same capacity; and request-term pricing takes billing from the
//! whole recording, qualifies untimed usage a row may not cover, notices
//! billing that changes only an upper bound, and leaves wholly unpriced
//! usage without an estimate.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::allowance::PlannedLanes;
use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend, Query};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_forecast_with_backends, allowance_rates_with_backends, usage_full_with_backends,
    usage_spans_with_backends, usage_window_with_backends, usage_with_backends, ListFilters,
    Selection, SessionSelection, WindowAsk,
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
            "tapes-review3-pricing-{label}-{}-{nonce}",
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

fn close(left: &Value, right: f64) -> bool {
    left.as_f64()
        .is_some_and(|left| (left - right).abs() < 1e-9)
}

fn reasons(estimate: &Value, category: &str) -> Vec<String> {
    estimate["measurement_completeness"][category]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .map(|reason| reason.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

// --- A quiet parent reached through its subagent, under --since -----------

const PARENT: &str = "30000000-0000-4000-8000-0000000000f9";
const AGENT: &str = "a00000000000000f9";

fn subagent_record(agent: Option<&str>, entry: &str, stamp: &str) -> Value {
    let mut record = json!({
        "sessionId": PARENT, "cwd": "/fixtures/project", "entrypoint": "cli", "version": "2.1.0",
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

fn quiet_parent_shares(since: &str) -> Vec<String> {
    let store = Store::new("since");
    store.write(
        &format!("project/{PARENT}.jsonl"),
        &[subagent_record(None, "parent-1", "2026-01-12T09:59:00Z")],
    );
    // Written now: the file's modification time is long after its records.
    store.write(
        &format!("project/{PARENT}/subagents/agent-{AGENT}.jsonl"),
        &[subagent_record(
            Some(AGENT),
            "child-1",
            "2026-01-12T10:05:00Z",
        )],
    );
    let selection = SessionSelection {
        harness: Some("claude"),
        limit: Some(50),
        filters: ListFilters {
            since: Some(at(since)),
            ..ListFilters::default()
        },
        ..Default::default()
    };
    let read = serde_json::to_value(
        usage_window_with_backends(
            &[Box::new(ClaudeBackend::new(&store.0)) as Box<dyn Backend>],
            &selection,
            &[GroupBy::Session],
            WindowAsk::Bounds(WindowBounds {
                from: Some(at("2026-01-12T10:00:00Z")),
                to: Some(at("2026-01-12T11:00:00Z")),
            }),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    read["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|share| share["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Claim: under --since 10:30, a parent whose records end at 09:59 and whose
/// subagent's records end at 10:05 contributes nothing to a 10:00..11:00
/// window, though the subagent's file was written later; under --since
/// 10:00 the subagent's 10:05 share stands. Rejects applying --since to the
/// subagent file's modification time alone, which returns the 10:05
/// consumption although neither recording has activity since 10:30.
#[test]
fn a_parent_reached_through_children_needs_recorded_activity_since() {
    assert_eq!(
        quiet_parent_shares("2026-01-12T10:30:00Z"),
        Vec::<String>::new()
    );
    assert_eq!(
        quiet_parent_shares("2026-01-12T10:00:00Z"),
        vec![format!("{PARENT}::{AGENT}")]
    );
}

// --- Codex allowance pools ------------------------------------------------

fn codex_usage(input: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
           "output_tokens": 0, "reasoning_output_tokens": 0, "total_tokens": input})
}

enum Event {
    /// The primary used percent the provider reported under a plan.
    Observed(&'static str, &'static str, f64),
    /// One request's uncached input.
    Request(&'static str, u64),
}

fn rollout(store: &Store, id: &str, day: &str, events: &[Event]) {
    let start = format!("{day}T09:00:00Z");
    let mut lines = vec![
        json!({"timestamp": start, "type": "session_meta", "payload": {"id": id,
               "timestamp": start, "cwd": "/fixtures/project", "source": "exec",
               "creator_account_id": "account-a"}}),
        json!({"timestamp": start, "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
    ];
    let mut total = 0;
    for event in events {
        match event {
            Event::Observed(stamp, plan, used) => lines.push(json!({
                "timestamp": stamp, "type": "event_msg",
                "payload": {"type": "token_count", "info": null, "rate_limits": {
                    "limit_id": "codex", "plan_type": plan,
                    "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_768_053_600},
                    "secondary": null}}
            })),
            Event::Request(stamp, input) => {
                total += input;
                lines.push(json!({
                    "timestamp": stamp, "type": "event_msg",
                    "payload": {"type": "token_count", "info": {"total_token_usage": codex_usage(total),
                        "last_token_usage": codex_usage(*input), "model_context_window": 272000},
                        "rate_limits": null}
                }));
            }
        }
    }
    let folder = day.replace('-', "/");
    store.write(
        &format!("{folder}/rollout-{day}T09-00-00-{id}.jsonl"),
        &lines,
    );
}

fn codex_selection() -> SessionSelection<'static> {
    SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        ..Default::default()
    }
}

/// Claim: in one period observed at 10.25, 10.25 and 13.25, B spending 100
/// tokens in the first interval and A 300 in the second, B is a lane whose
/// period-smoothed percent is 0.75 beside A's 2.25, so the smoothed lanes
/// add up to the 3% rise as the interval-apportioned ones do. Rejects
/// keeping only lanes with an interval-apportioned percent, which drops B
/// and leaves the smoothed lanes 0.75 short of the rise.
#[test]
fn a_lane_with_only_a_smoothed_percent_stays_a_lane() {
    use Event::{Observed, Request};
    let store = Store::new("smoothed");
    let a = "40000000-0000-7000-8000-0000000000f9";
    let b = "40000000-0000-7000-8000-0000000000fa";
    rollout(
        &store,
        b,
        "2026-01-10",
        &[
            Observed("2026-01-10T10:00:00Z", "plus", 10.25),
            Request("2026-01-10T10:05:00Z", 100),
            Observed("2026-01-10T10:10:00Z", "plus", 10.25),
        ],
    );
    rollout(
        &store,
        a,
        "2026-01-10",
        &[
            Observed("2026-01-10T10:10:00Z", "plus", 10.25),
            Request("2026-01-10T10:15:00Z", 300),
            Observed("2026-01-10T10:20:00Z", "plus", 13.25),
        ],
    );
    let rates = serde_json::to_value(
        allowance_rates_with_backends(
            &[Box::new(CodexBackend::new(&store.0)) as Box<dyn Backend>],
            &codex_selection(),
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
    let group = |id: &str| {
        rates["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["key"]["session"] == id)
            .unwrap_or_else(|| panic!("{id}'s lane: {rates:#}"))
            .clone()
    };
    let (a, b) = (group(a), group(b));
    let smoothed = |group: &Value| group["smoothed_percent_per_lane"]["estimate"]["median"].clone();
    let interval = |group: &Value| group["percent_per_lane"]["estimate"]["median"].clone();
    assert!(close(&smoothed(&b), 0.75), "{b:#}");
    assert!(close(&smoothed(&a), 2.25), "{a:#}");
    assert!(close(&interval(&b), 0.0), "{b:#}");
    assert!(close(&interval(&a), 3.0), "{a:#}");
}

/// Claim: a forecast of one lane under plan X sizes it by X's measured lanes
/// alone, 1%, when a pool of another plan with windows of the same length
/// measured an identical lane at 10%. Rejects matching window length alone,
/// which merges the two plans' rates into 5.5%.
#[test]
fn a_forecast_sizes_lanes_by_the_newest_pools_plan() {
    use Event::{Observed, Request};
    let store = Store::new("plans");
    rollout(
        &store,
        "40000000-0000-7000-8000-0000000000fb",
        "2026-01-10",
        &[
            Observed("2026-01-10T10:00:00Z", "pro", 20.0),
            Request("2026-01-10T10:05:00Z", 1_000),
            Observed("2026-01-10T10:10:00Z", "pro", 30.0),
        ],
    );
    rollout(
        &store,
        "40000000-0000-7000-8000-0000000000fc",
        "2026-01-11",
        &[
            Observed("2026-01-11T10:00:00Z", "plus", 20.0),
            Request("2026-01-11T10:05:00Z", 1_000),
            Observed("2026-01-11T10:10:00Z", "plus", 21.0),
        ],
    );
    let forecast = serde_json::to_value(
        allowance_forecast_with_backends(
            &[Box::new(CodexBackend::new(&store.0)) as Box<dyn Backend>],
            &codex_selection(),
            WindowBounds {
                from: Some(at("2026-01-10T00:00:00Z")),
                to: Some(at("2026-01-12T00:00:00Z")),
            },
            QuotaKind::Primary,
            &[PlannedLanes {
                model: "gpt-fixture".to_owned(),
                variant: Some("high".to_owned()),
                lanes: 1,
            }],
            None,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        forecast["window_state"]["pool"]["plan"], "plus",
        "{forecast:#}"
    );
    let sized = &forecast["plan"][0];
    assert_eq!(sized["measured_lanes"], 1, "{sized:#}");
    assert!(
        close(&sized["percent_per_lane"]["estimate"], 1.0),
        "{sized:#}"
    );
}

// --- Request-term pricing of Claude recordings ----------------------------

const CLAUDE_ID: &str = "30000000-0000-4000-8000-0000000000fa";

fn claude_path() -> String {
    format!("-fixtures-project/{CLAUDE_ID}.jsonl")
}

fn user(entry: &str, stamp: &str, text: &str) -> Value {
    json!({"type": "user", "sessionId": CLAUDE_ID, "uuid": entry, "parentUuid": null,
           "timestamp": stamp, "cwd": "/fixtures/project",
           "message": {"role": "user", "content": text}})
}

fn assistant(request: &str, stamp: &str, usage: Value) -> Value {
    json!({"type": "assistant", "sessionId": CLAUDE_ID, "uuid": format!("assistant-{request}"),
           "parentUuid": "user-1", "timestamp": stamp, "cwd": "/fixtures/project",
           "requestId": request,
           "message": {"id": format!("msg-{request}"), "role": "assistant", "model": "claude-haiku-5-5",
                       "content": [{"type": "text", "text": "Done."}], "usage": usage}})
}

fn api_request(stamp: &str, betas: &[&str]) -> Value {
    json!({"type": "api-request", "sessionId": CLAUDE_ID, "timestamp": stamp,
           "params": {"model": "claude-haiku-5-5", "betas": betas}})
}

/// Every pricing path of one session: the bounded and the whole usage
/// view, and the session as a listing prices it.
fn estimates(store: &Store, table: &PriceTable) -> [Value; 3] {
    let backend = || Box::new(ClaudeBackend::new(&store.0)) as Box<dyn Backend>;
    let mut view = usage_with_backends(&[backend()], Selection::Id(CLAUDE_ID)).unwrap();
    pricing::price_view(&mut view, table);
    let mut whole = usage_full_with_backends(&[backend()], Selection::Id(CLAUDE_ID)).unwrap();
    pricing::price_view(&mut whole, table);
    let mut listed = backend()
        .list(&Query::unscoped(5))
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.id == CLAUDE_ID)
        .unwrap();
    listed.price(table);
    [
        serde_json::to_value(view).unwrap()["list_price_estimate"].clone(),
        serde_json::to_value(whole).unwrap()["list_price_estimate"].clone(),
        serde_json::to_value(&listed.list_price_estimate).unwrap(),
    ]
}

const HAIKU_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "claude-haiku-5-5"
effective = 2026-10-18
source = "fixture"
per_million = { uncached_input = 0.10, cache_read = 0.01, cache_write_5m = 0.125, cache_write_1h = 0.20, output = 0.50 }

[models.long_context]
above_prompt_tokens = 100000
prompt_tokens = ["uncached_input", "cache_read", "cache_write"]
multipliers = { uncached_input = 5.0, cache_read = 5.0, cache_write_5m = 5.0, cache_write_1h = 5.0, output = 5.0 }
"#;

fn haiku_usage(input: u64, read: u64, write_5m: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cache_read_input_tokens": read,
           "cache_creation_input_tokens": write_5m, "output_tokens": output,
           "cache_creation": {"ephemeral_5m_input_tokens": write_5m, "ephemeral_1h_input_tokens": 0}})
}

/// Claim: a Claude recording past the bounded read, whose only OAuth
/// evidence comes before the bounded read's tail, prices its 110k-prompt
/// Haiku request at base rates as a subscription's under --from as under
/// --full, both naming the subscription billing. Rejects a span that takes
/// its billing from the bounded lookup, which establishes none and prices
/// the request between base and surcharged rates, billing-unestablished.
#[test]
fn a_span_takes_billing_from_the_whole_recording() {
    let store = Store::new("span-billing");
    let filler = "x".repeat(64 * 1024);
    let mut lines = vec![
        user("user-1", "2026-10-19T10:00:00Z", "Survey the fixture."),
        api_request(
            "2026-10-19T10:00:01Z",
            &["claude-code-20250219", "oauth-2025-04-20"],
        ),
    ];
    // Six MiB of turns between the evidence and the request, past the
    // four-MiB bounded read and any opening it keeps.
    for index in 0..96 {
        lines.push(user(
            &format!("filler-{index}"),
            "2026-10-19T10:10:00Z",
            &filler,
        ));
    }
    lines.push(assistant(
        "req-large",
        "2026-10-19T10:30:00Z",
        haiku_usage(20_000, 80_000, 10_000, 2_000),
    ));
    store.write(&claude_path(), &lines);
    let table = PriceTable::parse(HAIKU_TABLE).unwrap();
    // 20k x 0.10 + 80k x 0.01 + 10k x 0.125 + 2k x 0.50, at base rates.
    let base = (2_000.0 + 800.0 + 1_250.0 + 1_000.0) / 1e6;
    let backend = || Box::new(ClaudeBackend::new(&store.0)) as Box<dyn Backend>;
    let mut whole = usage_full_with_backends(&[backend()], Selection::Id(CLAUDE_ID)).unwrap();
    pricing::price_view(&mut whole, &table);
    let whole = serde_json::to_value(whole).unwrap()["list_price_estimate"].clone();
    let mut spans = usage_spans_with_backends(
        &[backend()],
        Selection::Id(CLAUDE_ID),
        &[SpanBounds::Time {
            from: Some(at("2026-10-19T10:20:00Z")),
            to: None,
        }],
    )
    .unwrap();
    pricing::price_spans(&mut spans, &table);
    let span = serde_json::to_value(&spans.spans[0]).unwrap()["list_price_estimate"].clone();
    for estimate in [whole, span] {
        assert!(close(&estimate["amount"], base), "{estimate:#}");
        assert_eq!(
            estimate["billing"][0]["terms"], "subscription",
            "{estimate:#}"
        );
        assert!(
            estimate.get("measurement_completeness").is_none(),
            "{estimate:#}"
        );
    }
}
