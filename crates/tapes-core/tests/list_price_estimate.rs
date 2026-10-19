//! A list-price estimate prices a figure's disjoint categories by the row of
//! the model that served them, from a table the caller supplies. What the
//! table or the figure cannot price leaves the estimate a known subtotal, or
//! bounds, with a reason; nothing priced leaves it absent.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::pricing::{self, PriceLookup, PriceTable};
use agent_tapes_core::usage::UsageView;
use agent_tapes_core::{
    usage_full_with_backends, usage_summary_priced, Selection, SessionSelection,
};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn table() -> PriceTable {
    PriceTable::read(&fixtures("prices/prices.toml")).unwrap()
}

/// Tests that read or set process environment take this lock.
fn environment() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn opencode_program() -> PathBuf {
    std::env::set_var("XDG_DATA_HOME", support::empty_opencode_data_home());
    fixtures("opencode").join("opencode2")
}

fn priced(backend: Box<dyn Backend>, id: &str) -> Value {
    let mut view: UsageView = usage_full_with_backends(&[backend], Selection::Id(id)).unwrap();
    pricing::price_view(&mut view, &table());
    serde_json::to_value(view).unwrap()
}

fn close(left: &Value, right: f64) -> bool {
    left.as_f64()
        .is_some_and(|left| (left - right).abs() < 1e-12)
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

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-price-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claim: a Claude, a Codex, a Pi and an OpenCode fixture session are each
/// priced by their model's row from their disjoint categories, the amount
/// matching hand arithmetic, labelled as a list-price estimate that names
/// the row's id and effective date. Rejects an estimate priced from native
/// counters (Codex input includes its cache reads; OpenCode output leaves
/// reasoning out), by the wrong row, or unlabelled.
#[test]
fn fixture_sessions_are_priced_by_their_models_rows() {
    let _environment = environment();
    // claude-fixture: 28 uncached x 3 + 10400 read x 0.3 + 2900 1h writes x 6
    // + 750 output x 15, per million.
    let claude = priced(
        Box::new(ClaudeBackend::new(fixtures("claude-resumed"))),
        "30000000-0000-4000-8000-000000000001",
    );
    let estimate = &claude["list_price_estimate"];
    assert_eq!(estimate["kind"], "list-price-estimate");
    assert_eq!(estimate["currency"], "USD");
    assert_eq!(estimate["schema"], "tapes-price-table/1");
    assert_eq!(
        estimate["models"],
        json!([{"id": "claude-fixture", "effective": "2026-01-01", "source": "fixture"}])
    );
    assert!(close(
        &estimate["amount"],
        (84.0 + 3120.0 + 17400.0 + 11250.0) / 1e6
    ));
    assert!(estimate.get("measurement_completeness").is_none());
    assert!(
        claude["cost"].is_null(),
        "the estimate never stands in for a written cost"
    );

    // gpt-fixture: Codex input 1200 includes 1000 cache reads, so 200
    // uncached x 2 + 1000 read x 0.2 + 300 output x 10.
    let codex = priced(
        Box::new(CodexBackend::new(fixtures("codex"))),
        "00000000-0000-0000-0000-000000000001",
    );
    assert!(close(
        &codex["list_price_estimate"]["amount"],
        (400.0 + 200.0 + 3000.0) / 1e6
    ));
    assert_eq!(
        codex["list_price_estimate"]["models"][0]["effective"],
        "2026-01-02"
    );

    // gpt-fixture on pi: 40 uncached x 2 + 12 read x 0.2 + 60 output x 10; the
    // 14 cache writes recorded no TTL and the row has no 1-hour price.
    let pi = priced(Box::new(PiBackend::new(fixtures("pi"))), "session-pi");
    let estimate = &pi["list_price_estimate"];
    assert!(close(&estimate["amount"], (80.0 + 2.4 + 600.0) / 1e6));
    assert!(reasons(estimate, "cache_write").contains(&"missing-price".to_owned()));

    // kimi-k3: 200 uncached x 1 + 1100 read x 0.1 + (125 output + 15
    // reasoning) x 5 + 350 TTL-unrecorded writes x 1.25 at either TTL.
    let opencode = priced(
        Box::new(OpenCodeBackend::new(opencode_program())),
        "ses_000000fixtureSpanSession",
    );
    let estimate = &opencode["list_price_estimate"];
    assert!(close(
        &estimate["amount"],
        (200.0 + 110.0 + 700.0 + 437.5) / 1e6
    ));
    assert_eq!(reasons(estimate, "cache_write"), ["ttl-unrecorded"]);
    assert!(
        close(&opencode["cost"]["usd"], 0.08),
        "the harness cost stays beside it"
    );
}

/// Claim: a model the table lacks leaves the estimate a known subtotal of
/// the models it lists, every category qualified `unlisted-model`, and the
/// unlisted model's own share has no estimate. Rejects pricing the unlisted
/// model by another row, or reporting the listed share as the whole.
#[test]
fn a_model_the_table_lacks_leaves_a_known_subtotal() {
    let pi = priced(
        Box::new(PiBackend::new(fixtures("pi"))),
        "session-model-split",
    );
    let estimate = &pi["list_price_estimate"];
    for category in ["uncached_input", "cache_read", "cache_write", "output"] {
        assert!(
            reasons(estimate, category).contains(&"unlisted-model".to_owned()),
            "{category}: {estimate}"
        );
    }
    assert_eq!(estimate["models"].as_array().unwrap().len(), 1);
    assert_eq!(estimate["models"][0]["id"], "pi-model-a");
    let models = pi["by_model"].as_array().unwrap();
    assert!(models[0]["list_price_estimate"].is_object());
    assert!(models[1]["list_price_estimate"].is_null());
    assert!(models[2]["list_price_estimate"].is_null());
}

/// Claim: a cache write with no recorded TTL is bounded between the 5-minute
/// and 1-hour prices, `at_least` and `at_most` with no `amount`, and its
/// category says `ttl-unrecorded`. Rejects an estimate that guesses a TTL.
#[test]
fn a_cache_write_without_a_ttl_is_bounded() {
    // pi-model-a share: 10 x 1 + 4 x 0.1 + 20 x 4, plus 5 writes at 1 or 2.
    let pi = priced(
        Box::new(PiBackend::new(fixtures("pi"))),
        "session-model-split",
    );
    let estimate = &pi["list_price_estimate"];
    assert!(estimate.get("amount").is_none());
    assert!(close(
        &estimate["at_least"],
        (10.0 + 0.4 + 80.0 + 5.0) / 1e6
    ));
    assert!(close(
        &estimate["at_most"],
        (10.0 + 0.4 + 80.0 + 10.0) / 1e6
    ));
    assert!(reasons(estimate, "cache_write").contains(&"ttl-unrecorded".to_owned()));
}

/// Claim: a counter the figure measured incompletely keeps the estimate
/// qualified with the same reason, a known subtotal; and consumption on
/// another branch, whose every event names its model, is priced by those
/// models rather than left unattributed. Rejects a complete estimate over
/// incompletely measured counters.
#[test]
fn a_qualified_counter_keeps_the_estimate_qualified() {
    let claude = priced(
        Box::new(ClaudeBackend::new(fixtures("claude"))),
        "session-claude",
    );
    let figure = &claude["disjoint"]["measurement_completeness"];
    let estimate = &claude["list_price_estimate"];
    for category in ["uncached_input", "cache_read", "output"] {
        assert!(figure[category]
            .as_array()
            .unwrap()
            .contains(&json!("missing-counter")));
        assert!(reasons(estimate, category).contains(&"missing-counter".to_owned()));
    }
    assert!(
        estimate["at_least"].is_number(),
        "a known subtotal keeps its amount"
    );
    let pi = priced(Box::new(PiBackend::new(fixtures("pi"))), "session-pi");
    assert!(pi["consumption"]["events"]["other_branches"].as_u64() > Some(0));
    let consumption = &pi["consumption"]["list_price_estimate"];
    assert_eq!(
        consumption["models"][0]["id"], "gpt-fixture",
        "{consumption}"
    );
    assert!(!reasons(consumption, "output").contains(&"incomplete-attribution".to_owned()));
}

/// Claim: usage a harness attributed to no model leaves the estimate a known
/// subtotal of the attributed shares, every category qualified
/// `incomplete-attribution`; a Codex share's uncached input, a difference of
/// counters that attribution leaves incomplete, is not priced. Rejects
/// pricing the unattributed remainder by a guessed model, or a complete
/// estimate beside it.
#[test]
fn unattributed_usage_leaves_a_known_subtotal() {
    let scratch = Scratch::new("unattributed");
    let id = "50000000-0000-7000-8000-0000000000a1";
    let usage = |input: u64, cached: u64, output: u64| {
        json!({"input_tokens": input, "cached_input_tokens": cached, "cache_write_input_tokens": 0,
               "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output})
    };
    let count = |stamp: &str, total: Value, last: Value| {
        json!({"timestamp": stamp, "type": "event_msg", "payload": {"type": "token_count",
               "info": {"total_token_usage": total, "last_token_usage": last, "model_context_window": 272000}}})
    };
    let context = |stamp: &str, model: &str| json!({"timestamp": stamp, "type": "turn_context", "payload": {"cwd": "/fixtures/project", "model": model, "effort": "high"}});
    let lines = [
        json!({"timestamp": "2026-01-08T10:00:00Z", "type": "session_meta", "payload": {"id": id, "timestamp": "2026-01-08T10:00:00Z", "cwd": "/fixtures/project", "source": "exec"}}),
        count(
            "2026-01-08T10:00:01Z",
            usage(1000, 0, 100),
            usage(1000, 0, 100),
        ),
        context("2026-01-08T10:00:02Z", "gpt-fixture"),
        count(
            "2026-01-08T10:00:03Z",
            usage(3000, 1000, 300),
            usage(2000, 1000, 200),
        ),
        context("2026-01-08T10:00:04Z", "gpt-fixture-alias"),
        count(
            "2026-01-08T10:00:05Z",
            usage(7000, 4000, 700),
            usage(4000, 3000, 400),
        ),
    ];
    scratch.write(
        &format!("rollout-2026-01-08T10-00-00-{id}.jsonl"),
        &(lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"),
    );
    let codex = priced(Box::new(CodexBackend::new(&scratch.0)), id);
    let estimate = &codex["list_price_estimate"];
    // Attributed shares: (1000 read x 0.2 + 200 output x 10) and
    // (3000 read x 0.2 + 400 output x 10); the first request is no model's.
    assert!(
        close(&estimate["amount"], (2200.0 + 4600.0) / 1e6),
        "{estimate}"
    );
    assert_eq!(estimate["models"][0]["id"], "gpt-fixture");
    for category in ["uncached_input", "cache_read", "cache_write", "output"] {
        assert!(
            reasons(estimate, category).contains(&"incomplete-attribution".to_owned()),
            "{category}: {estimate}"
        );
    }
}

/// Claim: a summary adds its sessions' estimates where their disjoint
/// categories add, and withholds them, saying why, where accounting differs
/// or an overlap group's totals are withheld. Rejects a summary estimate
/// added across incomparable figures or across sessions that share requests.
#[test]
fn a_summary_adds_estimates_only_where_disjoint_categories_add() {
    let _environment = environment();
    let summary = |backends: Vec<Box<dyn Backend>>| {
        let selection = SessionSelection {
            limit: Some(20),
            ..Default::default()
        };
        serde_json::to_value(
            usage_summary_priced(&backends, &selection, &[], Some(&table())).unwrap(),
        )
        .unwrap()
    };
    let pi = summary(vec![Box::new(PiBackend::new(fixtures("pi")))]);
    let totals = &pi["totals"]["list_price_estimate"];
    // session-pi 682.4, session-model-split 95.4 to 100.4, and
    // session-turn-durations 500 micro-dollars; the malformed session has none.
    assert!(
        close(&totals["at_least"], (682.4 + 95.4 + 500.0) / 1e6),
        "{totals}"
    );
    assert!(close(&totals["at_most"], (682.4 + 100.4 + 500.0) / 1e6));
    assert!(reasons(totals, "cache_write").contains(&"unlisted-model".to_owned()));

    let mixed = summary(vec![
        Box::new(ClaudeBackend::new(fixtures("claude"))),
        Box::new(CodexBackend::new(fixtures("codex"))),
    ]);
    assert!(mixed["totals"]["list_price_estimate"].is_null());
    assert_eq!(
        mixed["totals"]["list_price_withheld"],
        json!(["mixed-accounting"])
    );

    let copies = summary(vec![Box::new(ClaudeBackend::new(fixtures(
        "claude-resumed",
    )))]);
    assert!(copies["totals"]["list_price_estimate"].is_null());
    assert_eq!(
        copies["totals"]["list_price_withheld"],
        json!(["identity-unestablished"])
    );
}

/// Claim: a price table refuses unknown keys, negative or non-finite prices,
/// a non-date effective, and an id or alias named twice, naming the entry.
/// Rejects a table that silently drops or reorders a bad row.
#[test]
fn an_invalid_table_refuses_naming_the_entry() {
    let row = |extra: &str| {
        format!(
            "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n[[models]]\nid = \"a\"\neffective = 2026-01-01\nsource = \"s\"\nper_million = {{ output = 1.0 }}\n{extra}"
        )
    };
    let refused = |text: String, expected: &str| {
        let error = format!("{:#}", PriceTable::parse(&text).unwrap_err());
        assert!(error.contains(expected), "{expected}: {error}");
    };
    refused(row("colour = \"red\"\n"), "unknown field `colour`");
    refused(
        row("[[models]]\nid = \"b\"\naliases = [\"a\"]\neffective = 2026-01-01\nsource = \"s\"\nper_million = {}\n"),
        "models[1] (id `b`) names `a`, which models[0] already names",
    );
    refused(
        row("").replace("output = 1.0", "output = -1.0"),
        "models[0] (id `a`): the output price must be a finite number of at least zero",
    );
    refused(
        row("").replace("output = 1.0", "output = nan"),
        "the output price must be a finite",
    );
    refused(
        row("").replace("output = 1.0", "cache_write_15m = 1.0"),
        "unknown field `cache_write_15m`",
    );
    refused(
        row("").replace("2026-01-01", "2026-01-01T10:00:00Z"),
        "models[0] (id `a`): effective must be a date",
    );
    refused(
        row("").replace("tapes-price-table/1", "tapes-price-table/3"),
        "schema",
    );
    assert!(PriceTable::parse(&row("")).is_ok());
}

/// Claim: the lookup takes the named table, then `TAPES_PRICES`, then the
/// default location; a named or environment table that is missing or
/// invalid refuses, an invalid default table is reported without refusing,
/// and no table is absence. Rejects falling through from a broken explicit
/// table to another one.
#[test]
fn the_lookup_refuses_an_explicit_table_and_reports_a_default_one() {
    let _environment = environment();
    let scratch = Scratch::new("lookup");
    let good = scratch.write(
        "good.toml",
        &fs::read_to_string(fixtures("prices/prices.toml")).unwrap(),
    );
    let bad = scratch.write("bad.toml", "schema = \"tapes-price-table/1\"\n");
    std::env::set_var("XDG_CONFIG_HOME", scratch.0.join("config"));
    std::env::remove_var(pricing::PRICES_ENV);
    assert!(matches!(
        pricing::lookup(None).unwrap(),
        PriceLookup::Absent
    ));
    assert!(matches!(
        pricing::lookup(Some(&good)).unwrap(),
        PriceLookup::Loaded(loaded) if loaded.origin == pricing::PriceOrigin::Named
    ));
    assert!(pricing::lookup(Some(&bad)).is_err());
    assert!(pricing::lookup(Some(&scratch.0.join("missing.toml"))).is_err());
    std::env::set_var(pricing::PRICES_ENV, &bad);
    assert!(pricing::lookup(None).is_err());
    std::env::set_var(pricing::PRICES_ENV, &good);
    assert!(matches!(
        pricing::lookup(None).unwrap(),
        PriceLookup::Loaded(loaded) if loaded.origin == pricing::PriceOrigin::Environment
    ));
    std::env::remove_var(pricing::PRICES_ENV);
    scratch.write("config/tapes/prices.toml", "schema = 1\n");
    assert!(matches!(
        pricing::lookup(None).unwrap(),
        PriceLookup::DefaultInvalid { .. }
    ));
    std::env::remove_var("XDG_CONFIG_HOME");
}

/// Claim: span figures are priced like whole ones, so the runs of a resumed
/// session add up to the whole session's estimate. Rejects a span answer
/// without estimates, or spans priced by a different rule than the whole.
#[test]
fn span_estimates_add_up_to_the_whole() {
    let at = |stamp: &str| stamp.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    let mut answer = agent_tapes_core::usage_spans_with_backends(
        &[Box::new(ClaudeBackend::new(fixtures("claude-resumed")))],
        Selection::Id("30000000-0000-4000-8000-000000000001"),
        &[
            agent_tapes_core::span::SpanBounds::Time {
                from: None,
                to: Some(at("2026-01-05T13:00:00Z")),
            },
            agent_tapes_core::span::SpanBounds::Time {
                from: Some(at("2026-01-05T13:00:00Z")),
                to: None,
            },
        ],
    )
    .unwrap();
    pricing::price_spans(&mut answer, &table());
    let amount = |view: &UsageView| view.list_price_estimate.as_ref().unwrap().amount.unwrap();
    let parts = amount(&answer.spans[0]) + amount(&answer.spans[1]);
    assert!((parts - amount(&answer.whole)).abs() < 1e-12);
    // Run 3 alone: 11 x 3 + 3000 x 0.3 + 1200 x 6 + 300 x 15, per million.
    assert!((amount(&answer.spans[1]) - (33.0 + 900.0 + 7200.0 + 4500.0) / 1e6).abs() < 1e-12);
}
