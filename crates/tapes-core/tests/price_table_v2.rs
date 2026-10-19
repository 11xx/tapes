//! A `tapes-price-table/2` row prices terms by the request: a long-context
//! rate for the whole request above a prompt size, under the billing the
//! term names; time-of-day rates; and a validity window. A figure whose
//! requests its reader kept is priced request by request; one whose
//! requests are unknown is priced between the rates a term could charge,
//! with a reason, and never reads complete.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend, Query,
};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-price-v2-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write_lines(&self, relative: &str, lines: &[Value]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(&path, body.join("\n") + "\n").unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

/// Every pricing path: the bounded and the whole usage view, and the
/// session as a summary or a listing prices it.
fn estimates(backend: &dyn Fn() -> Box<dyn Backend>, id: &str, table: &PriceTable) -> [Value; 3] {
    let mut view = usage_with_backends(&[backend()], Selection::Id(id)).unwrap();
    pricing::price_view(&mut view, table);
    let view = serde_json::to_value(view).unwrap();
    let mut whole = usage_full_with_backends(&[backend()], Selection::Id(id)).unwrap();
    pricing::price_view(&mut whole, table);
    let whole = serde_json::to_value(whole).unwrap();
    let mut listed = backend()
        .list(&Query::unscoped(5))
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.id == id)
        .unwrap();
    listed.price(table);
    [
        view["list_price_estimate"].clone(),
        whole["list_price_estimate"].clone(),
        serde_json::to_value(&listed.list_price_estimate).unwrap(),
    ]
}

// --- Codex above and below a 272K prompt -----------------------------------

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

const CODEX_ID: &str = "019f0000-0000-7000-8000-0000000000e1";

fn codex_usage(input: u64, cached: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
           "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output})
}

/// A Codex rollout of two requests, one whose prompt is 300K (100K
/// uncached, 200K cache read) and one of 50K (10K uncached, 40K read), in
/// the provider's name and with the given `rate_limits`.
fn codex_rollout(scratch: &Scratch, provider: &str, rate_limits: Value) -> PathBuf {
    let large = codex_usage(300_000, 200_000, 1_000);
    let small = codex_usage(50_000, 40_000, 500);
    let total = codex_usage(350_000, 240_000, 1_500);
    let count = |stamp: &str, total: &Value, last: &Value| {
        json!({"timestamp": stamp, "type": "event_msg", "payload": {"type": "token_count",
               "info": {"total_token_usage": total, "last_token_usage": last,
                        "model_context_window": 1_000_000},
               "rate_limits": rate_limits}})
    };
    scratch.write_lines(
        &format!("2026/10/19/rollout-2026-10-19T10-00-00-{CODEX_ID}.jsonl"),
        &[
            json!({"timestamp": "2026-10-19T10:00:00Z", "type": "session_meta", "payload": {
                "id": CODEX_ID, "timestamp": "2026-10-19T10:00:00Z", "cwd": "/fixtures/project",
                "source": "exec", "originator": "codex_exec", "model_provider": provider}}),
            json!({"timestamp": "2026-10-19T10:00:00Z", "type": "turn_context",
                   "payload": {"cwd": "/fixtures/project", "model": "gpt-6-fixture", "effort": "high"}}),
            count("2026-10-19T10:01:00Z", &large, &large),
            count("2026-10-19T10:02:00Z", &total, &small),
        ],
    )
}

fn plan_quota() -> Value {
    json!({"limit_id": "codex", "plan_type": "plus",
           "primary": {"used_percent": 10.0, "window_minutes": 300, "resets_at": 1_791_000_000},
           "secondary": null})
}

// The 300K request: 100K uncached x 2 + 200K read x 0.2 + 1K output x 10 at
// base, and x 2, x 2 and x 1.5 above the threshold. The 50K request: 10K x 2
// + 40K x 0.2 + 500 x 10, below it either way.
const LARGE_BASE: f64 = (200_000.0 + 40_000.0 + 10_000.0) / 1e6;
const LARGE_SURCHARGED: f64 = (400_000.0 + 80_000.0 + 15_000.0) / 1e6;
const SMALL: f64 = (20_000.0 + 8_000.0 + 5_000.0) / 1e6;

fn codex_estimates(provider: &str, rate_limits: Value) -> [Value; 3] {
    let scratch = Scratch::new("codex");
    codex_rollout(&scratch, provider, rate_limits);
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let root = scratch.0.clone();
    estimates(
        &move || Box::new(CodexBackend::new(&root)),
        CODEX_ID,
        &table,
    )
}

/// Claim: a Codex session whose `rate_limits` report a ChatGPT plan's quota
/// is billed by subscription, so its request above 272K pays base rates; the
/// estimate is complete and names the subscription billing and its
/// evidence. Rejects a surcharge charged to subscription usage, or an
/// estimate that does not say which billing it priced.
#[test]
fn a_codex_subscription_session_above_the_threshold_pays_base_rates() {
    for estimate in codex_estimates("openai", plan_quota()) {
        assert!(
            close(&estimate["amount"], LARGE_BASE + SMALL),
            "{estimate:#}"
        );
        assert_eq!(estimate["schema"], "tapes-price-table/2");
        assert_eq!(
            estimate["billing"],
            json!([{"terms": "subscription",
                    "evidence": "codex rate_limits quota window, plan `plus`"}])
        );
        assert!(
            estimate.get("measurement_completeness").is_none(),
            "{estimate:#}"
        );
    }
}

/// Claim: a Codex session whose model provider is not `openai` is billed per
/// token, so its request above 272K pays the long-context rates for the whole
/// request and the other request pays base rates. Rejects pricing the whole
/// figure at one rate, or surcharging only the tokens past the threshold.
#[test]
fn an_api_billed_codex_session_above_the_threshold_pays_the_surcharge() {
    for estimate in codex_estimates("azure", Value::Null) {
        assert!(
            close(&estimate["amount"], LARGE_SURCHARGED + SMALL),
            "{estimate:#}"
        );
        assert_eq!(
            estimate["billing"],
            json!([{"terms": "api", "evidence": "codex model_provider `azure`"}])
        );
        assert!(
            estimate.get("measurement_completeness").is_none(),
            "{estimate:#}"
        );
    }
}

/// Claim: a Codex session whose recording establishes no billing is priced
/// between base and surcharged rates for its request above 272K, as
/// `at_least` and `at_most`, and the categories that request spent say
/// billing-unestablished. Rejects a complete amount at either rate.
#[test]
fn an_unestablished_codex_session_above_the_threshold_gets_bounds() {
    for estimate in codex_estimates("openai", Value::Null) {
        assert!(estimate.get("amount").is_none(), "{estimate:#}");
        assert!(
            close(&estimate["at_least"], LARGE_BASE + SMALL),
            "{estimate:#}"
        );
        assert!(
            close(&estimate["at_most"], LARGE_SURCHARGED + SMALL),
            "{estimate:#}"
        );
        for category in ["uncached_input", "cache_read", "output"] {
            assert_eq!(
                reasons(&estimate, category),
                ["billing-unestablished"],
                "{category}"
            );
        }
        assert_eq!(estimate["billing"][0]["terms"], "unestablished");
    }
}

// --- Claude Haiku over 100k ------------------------------------------------

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

const HAIKU_ID: &str = "30000000-0000-4000-8000-0000000000e2";

fn claude_assistant(request: &str, stamp: &str, usage: Value) -> Value {
    json!({"type": "assistant", "sessionId": HAIKU_ID, "uuid": format!("assistant-{request}"),
           "parentUuid": "user-1", "timestamp": stamp, "cwd": "/fixtures/project",
           "requestId": request,
           "message": {"id": format!("msg-{request}"), "role": "assistant", "model": "claude-haiku-5-5",
                       "content": [{"type": "text", "text": "Done."}], "usage": usage}})
}

fn haiku_usage(input: u64, read: u64, write_5m: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cache_read_input_tokens": read,
           "cache_creation_input_tokens": write_5m, "output_tokens": output,
           "cache_creation": {"ephemeral_5m_input_tokens": write_5m, "ephemeral_1h_input_tokens": 0}})
}

/// A Claude Haiku session of two requests, one whose prompt is 110k (20k
/// uncached, 80k read, 10k five-minute writes) and one of 51k, with an
/// `api-request` record carrying `betas`.
fn haiku_session(scratch: &Scratch, betas: &[&str]) -> PathBuf {
    scratch.write_lines(
        &format!("-fixtures-project/{HAIKU_ID}.jsonl"),
        &[
            json!({"type": "user", "sessionId": HAIKU_ID, "uuid": "user-1", "parentUuid": null,
                   "timestamp": "2026-10-19T10:00:00Z", "cwd": "/fixtures/project",
                   "message": {"role": "user", "content": "Survey the fixture."}}),
            json!({"type": "api-request", "sessionId": HAIKU_ID, "timestamp": "2026-10-19T10:00:01Z",
                   "params": {"model": "claude-haiku-5-5", "betas": betas}}),
            claude_assistant("req-large", "2026-10-19T10:00:10Z", haiku_usage(20_000, 80_000, 10_000, 2_000)),
            claude_assistant("req-small", "2026-10-19T10:00:20Z", haiku_usage(1_000, 50_000, 0, 500)),
        ],
    )
}

// The 110k request: 20k x 0.10 + 80k x 0.01 + 10k x 0.125 + 2k x 0.50 at base,
// five times that above 100k. The 51k request: 1k x 0.10 + 50k x 0.01 + 500 x
// 0.50.
const HAIKU_LARGE: f64 = (2_000.0 + 800.0 + 1_250.0 + 1_000.0) / 1e6;
const HAIKU_SMALL: f64 = (100.0 + 500.0 + 250.0) / 1e6;

fn haiku_estimates(betas: &[&str]) -> [Value; 3] {
    let scratch = Scratch::new("haiku");
    haiku_session(&scratch, betas);
    let table = PriceTable::parse(HAIKU_TABLE).unwrap();
    let root = scratch.0.clone();
    estimates(
        &move || Box::new(ClaudeBackend::new(&root)),
        HAIKU_ID,
        &table,
    )
}

/// Claim: a Claude Haiku session whose requests went out without the OAuth
/// beta is billed per token, so its request over 100k prompt tokens costs
/// five times its base price, whole, and the other its base price; a session
/// whose requests carry the OAuth beta is a subscription's and pays base
/// rates throughout. Rejects pricing either request at one rate, or a
/// surcharge on subscription usage.
#[test]
fn a_claude_haiku_request_over_100k_pays_five_fold_when_api_billed() {
    for estimate in haiku_estimates(&["claude-code-20250219"]) {
        assert!(
            close(&estimate["amount"], 5.0 * HAIKU_LARGE + HAIKU_SMALL),
            "{estimate:#}"
        );
        assert_eq!(estimate["billing"][0]["terms"], "api", "{estimate:#}");
        assert!(
            estimate.get("measurement_completeness").is_none(),
            "{estimate:#}"
        );
    }
    for estimate in haiku_estimates(&["claude-code-20250219", "oauth-2025-04-20"]) {
        assert!(
            close(&estimate["amount"], HAIKU_LARGE + HAIKU_SMALL),
            "{estimate:#}"
        );
        assert_eq!(
            estimate["billing"],
            json!([{"terms": "subscription",
                    "evidence": "claude api-request carries the `oauth-2025-04-20` beta"}])
        );
    }
}

/// Claim: a Claude figure that is a recorded total, with no request it
/// can price one by one, is priced between base and long-context rates
/// under a row with a threshold, as bounds, its categories saying
/// request-terms-unknown. Rejects a complete amount at base rates for usage
/// that may have passed the threshold.
#[test]
fn a_recorded_total_with_a_threshold_term_gets_bounds() {
    let scratch = Scratch::new("total");
    scratch.write_lines(
        &format!("-fixtures-project/{HAIKU_ID}.jsonl"),
        &[
            json!({"type": "user", "sessionId": HAIKU_ID, "uuid": "user-1", "parentUuid": null,
                   "timestamp": "2026-10-19T10:00:00Z", "cwd": "/fixtures/project",
                   "message": {"role": "user", "content": "Survey the fixture."}}),
            json!({"type": "api-request", "sessionId": HAIKU_ID, "timestamp": "2026-10-19T10:00:01Z",
                   "params": {"model": "claude-haiku-5-5", "betas": ["claude-code-20250219"]}}),
            json!({"type": "cost-state", "sessionId": HAIKU_ID, "timestamp": "2026-10-19T10:01:00Z",
                   "totalCostUSD": 0.01, "modelUsage": {"claude-haiku-5-5": {
                       "inputTokens": 20_000, "outputTokens": 2_000, "cacheReadInputTokens": 80_000,
                       "cacheCreationInputTokens": 0, "webSearchRequests": 0, "costUSD": 0.01}}}),
        ],
    );
    let table = PriceTable::parse(HAIKU_TABLE).unwrap();
    let root = scratch.0.clone();
    // 20k x 0.10 + 80k x 0.01 + 2k x 0.50 at base, five times that above.
    let base = (2_000.0 + 800.0 + 1_000.0) / 1e6;
    for estimate in estimates(
        &move || Box::new(ClaudeBackend::new(&root)),
        HAIKU_ID,
        &table,
    ) {
        assert!(estimate.get("amount").is_none(), "{estimate:#}");
        assert!(close(&estimate["at_least"], base), "{estimate:#}");
        assert!(close(&estimate["at_most"], 5.0 * base), "{estimate:#}");
        for category in ["uncached_input", "cache_read", "output"] {
            assert!(
                reasons(&estimate, category).contains(&"request-terms-unknown".to_owned()),
                "{category}: {estimate:#}"
            );
        }
    }
}

// --- A row's validity window -----------------------------------------------

/// Claim: a request made after its model's only row stops being in effect
/// is unpriced, and the categories it spent say outside-validity, while the
/// request inside the window is priced. Rejects pricing a request by a row
/// no longer in effect, or dropping it without a reason.
#[test]
fn a_request_after_the_rows_effective_until_is_unpriced_with_its_reason() {
    let table = PriceTable::parse(
        r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "gpt-6-fixture"
effective = 2026-10-19
source = "fixture"
effective_until = 2026-10-19T10:01:30Z
per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.5, output = 10.0 }
"#,
    )
    .unwrap();
    let scratch = Scratch::new("validity");
    codex_rollout(&scratch, "openai", plan_quota());
    let root = scratch.0.clone();
    for estimate in estimates(
        &move || Box::new(CodexBackend::new(&root)),
        CODEX_ID,
        &table,
    ) {
        // The 10:01 request at base rates; the 10:02 one is past the window.
        assert!(close(&estimate["amount"], LARGE_BASE), "{estimate:#}");
        for category in ["uncached_input", "cache_read", "output"] {
            assert_eq!(
                reasons(&estimate, category),
                ["outside-validity"],
                "{category}"
            );
        }
    }
}

// --- DeepSeek peak and off-peak --------------------------------------------

const DEEPSEEK_ID: &str = "019f0000-0000-7000-8000-0000000000e3";

/// Claim: a pi DeepSeek session with one request at 10:00 UTC and one at
/// 18:00 UTC pays peak rates for the first and the off-peak window's half
/// rates for the second, by each request's own time. Rejects pricing the
/// session at peak rates throughout, or by the session's start time.
#[test]
fn deepseek_requests_pay_peak_and_off_peak_rates_by_their_time() {
    let table = PriceTable::parse(
        r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "deepseek-v4-pro"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 1.32, cache_read = 0.044, output = 3.96 }

[[models.time_of_day]]
from = "16:30"
until = "00:30"
per_million = { uncached_input = 0.66, cache_read = 0.022, output = 1.98 }
"#,
    )
    .unwrap();
    let scratch = Scratch::new("deepseek");
    let message = |id: &str, parent: &str, stamp: &str, body: Value| json!({"type": "message", "id": id, "parentId": parent, "timestamp": stamp, "message": body});
    let assistant = |stamp: &str| {
        json!({"role": "assistant", "timestamp": 1_791_000_000_000_u64, "provider": "deepseek",
               "model": "deepseek-v4-pro", "stopReason": "stop",
               "usage": {"input": 10_000, "output": 2_000, "cacheRead": 90_000, "cacheWrite": 0,
                         "totalTokens": 102_000, "cost": {"total": 0.0251}},
               "content": [{"type": "text", "text": format!("Done at {stamp}.")}]})
    };
    let user = |text: &str| {
        json!({"role": "user", "timestamp": 1_791_000_000_000_u64,
                                    "content": [{"type": "text", "text": text}]})
    };
    scratch.write_lines(
        &format!("--fixtures-project--/2026-10-19T10-00-00-000Z_{DEEPSEEK_ID}.jsonl"),
        &[
            json!({"type": "session", "version": 3, "id": DEEPSEEK_ID,
                   "timestamp": "2026-10-19T10:00:00.000Z", "cwd": "/fixtures/project"}),
            json!({"type": "model_change", "id": "model-1", "parentId": null,
                   "timestamp": "2026-10-19T10:00:00.000Z", "provider": "deepseek",
                   "modelId": "deepseek-v4-pro"}),
            message(
                "user-1",
                "model-1",
                "2026-10-19T10:00:01.000Z",
                user("Survey the fixture."),
            ),
            message(
                "assistant-1",
                "user-1",
                "2026-10-19T10:00:30.000Z",
                assistant("10:00"),
            ),
            message(
                "user-2",
                "assistant-1",
                "2026-10-19T18:00:01.000Z",
                user("Again."),
            ),
            message(
                "assistant-2",
                "user-2",
                "2026-10-19T18:00:30.000Z",
                assistant("18:00"),
            ),
        ],
    );
    let root = scratch.0.clone();
    // 10k uncached x 1.32 + 90k read x 0.044 + 2k output x 3.96 at peak, and
    // half of each off-peak.
    let peak = (13_200.0 + 3_960.0 + 7_920.0) / 1e6;
    for estimate in estimates(
        &move || Box::new(PiBackend::new(&root)),
        DEEPSEEK_ID,
        &table,
    ) {
        assert!(
            close(&estimate["amount"], peak + peak / 2.0),
            "{estimate:#}"
        );
        assert!(
            estimate.get("measurement_completeness").is_none(),
            "{estimate:#}"
        );
        assert!(estimate.get("billing").is_none(), "{estimate:#}");
    }
}

// --- Version 1 tables -------------------------------------------------------

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Every estimate a version 1 table gives the fixture sessions: each
/// session's bounded and whole usage views, with their model shares and
/// consumption, and the session as a summary prices it.
fn v1_estimates() -> Value {
    let table = PriceTable::read(&fixtures("prices/prices.toml")).unwrap();
    let mut estimates = serde_json::Map::new();
    type Open = fn(PathBuf) -> Box<dyn Backend>;
    let stores: [(&str, Open); 7] = [
        ("claude", |root| Box::new(ClaudeBackend::new(root))),
        ("claude-resumed", |root| Box::new(ClaudeBackend::new(root))),
        ("claude-request-dimensions", |root| {
            Box::new(ClaudeBackend::new(root))
        }),
        ("codex", |root| Box::new(CodexBackend::new(root))),
        ("codex-resumed", |root| Box::new(CodexBackend::new(root))),
        ("pi", |root| Box::new(PiBackend::new(root))),
        ("pi-active-consumption", |root| {
            Box::new(PiBackend::new(root))
        }),
    ];
    for (store, backend) in stores {
        let listing = backend(fixtures(store)).list(&Query::unscoped(50)).unwrap();
        for mut session in listing.sessions {
            let estimate_of = |view: Value| {
                json!({
                    "whole": view["list_price_estimate"],
                    "by_model": view["by_model"].as_array().map(|models| models
                        .iter()
                        .map(|model| model["list_price_estimate"].clone())
                        .collect::<Vec<_>>()),
                    "consumption": view["consumption"]["list_price_estimate"],
                })
            };
            let views = [false, true].map(|full| {
                let read = if full {
                    usage_full_with_backends(
                        &[backend(fixtures(store))],
                        Selection::Id(&session.id),
                    )
                } else {
                    usage_with_backends(&[backend(fixtures(store))], Selection::Id(&session.id))
                };
                read.ok().map_or(Value::Null, |mut view| {
                    pricing::price_view(&mut view, &table);
                    estimate_of(serde_json::to_value(view).unwrap())
                })
            });
            session.price(&table);
            estimates.insert(
                format!("{store}/{}", session.id),
                json!({
                    "bounded": views[0],
                    "whole": views[1],
                    "session": serde_json::to_value(&session.list_price_estimate).unwrap(),
                }),
            );
        }
    }
    Value::Object(estimates)
}

/// Claim: a version 1 table prices every fixture session, each model share
/// and each consumption exactly as it did before version 2 existed: the
/// estimates equal the snapshot taken from the release that read version 1
/// alone. Rejects any change to a version 1 estimate's amount, bounds,
/// reasons, rows or shape.
#[test]
fn a_v1_table_prices_identically() {
    let estimates = v1_estimates();
    if let Some(path) = std::env::var_os("TAPES_V1_ESTIMATES_OUT") {
        fs::write(
            path,
            serde_json::to_string_pretty(&estimates).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let snapshot: Value =
        serde_json::from_str(&fs::read_to_string(fixtures("prices/v1-estimates.json")).unwrap())
            .unwrap();
    assert_eq!(estimates, snapshot);
}

fn window_estimate(provider: &str, rate_limits: Value) -> Value {
    let scratch = Scratch::new("window");
    codex_rollout(&scratch, provider, rate_limits);
    let table = PriceTable::parse(GPT_TABLE).unwrap();
    let window = agent_tapes_core::window::WindowBounds {
        from: Some("2026-10-19T10:00:00Z".parse().unwrap()),
        to: Some("2026-10-19T11:00:00Z".parse().unwrap()),
    };
    let summary = agent_tapes_core::usage_window_with_backends(
        &[Box::new(CodexBackend::new(&scratch.0))],
        &agent_tapes_core::SessionSelection {
            within: agent_tapes_core::Where::Global,
            ..Default::default()
        },
        &[],
        agent_tapes_core::WindowAsk::Bounds(window),
        Some(&table),
    )
    .unwrap();
    serde_json::to_value(summary).unwrap()["totals"]["list_price_estimate"].clone()
}

/// Claim: a window share of a Codex session is billed as its session is and
/// priced by its own requests: under a subscription the request above 272K
/// pays base rates and the estimate names the subscription; under billing
/// the recording leaves unestablished only that request is bounded, its
/// categories billing-unestablished alone. Rejects a share that loses its
/// recording's billing, or one priced as if its requests were unknown.
#[test]
fn a_window_share_keeps_its_sessions_billing_and_requests() {
    let estimate = window_estimate("openai", plan_quota());
    assert!(
        close(&estimate["amount"], LARGE_BASE + SMALL),
        "{estimate:#}"
    );
    assert_eq!(
        estimate["billing"][0]["terms"], "subscription",
        "{estimate:#}"
    );

    let estimate = window_estimate("openai", Value::Null);
    assert!(
        close(&estimate["at_least"], LARGE_BASE + SMALL),
        "{estimate:#}"
    );
    assert!(
        close(&estimate["at_most"], LARGE_SURCHARGED + SMALL),
        "{estimate:#}"
    );
    for category in ["uncached_input", "cache_read", "output"] {
        assert_eq!(
            reasons(&estimate, category),
            ["billing-unestablished"],
            "{category}"
        );
    }
}
