//! A Claude recorded total takes its cache-write TTL split from the
//! recording's requests wherever their split sums exactly to the total's
//! cache writes, for the figure and for each model share; where the requests
//! fall short of the total, its writes stay without a recorded TTL.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, Backend, Query};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

const SESSION: &str = "30000000-0000-4000-8000-0000000000e1";

const TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "claude-opus-5-5"
effective = 2026-10-18
source = "fixture"
per_million = { uncached_input = 5.0, cache_read = 0.5, cache_write_5m = 6.25, cache_write_1h = 10.0, output = 25.0 }

[[models]]
id = "claude-haiku-5-5"
effective = 2026-10-18
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.25, cache_write_1h = 2.0, output = 5.0 }
"#;

struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        static STORES: AtomicUsize = AtomicUsize::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-recorded-total-ttl-{}-{}-{nonce}",
            std::process::id(),
            STORES.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, lines: &[Value]) {
        let path = self.0.join(format!("-fixtures-project/{SESSION}.jsonl"));
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

fn user(stamp: &str) -> Value {
    json!({"type": "user", "sessionId": SESSION, "uuid": "user-1", "parentUuid": null,
           "timestamp": stamp, "cwd": "/fixtures/project",
           "message": {"role": "user", "content": "Survey the fixture."}})
}

/// An Opus request whose usage splits its cache writes by TTL, as Claude
/// Code records every API response.
fn request(id: &str, stamp: &str, counters: [u64; 6]) -> Value {
    let [input, read, write_5m, write_1h, output, thinking] = counters;
    json!({"type": "assistant", "sessionId": SESSION, "uuid": format!("assistant-{id}"),
           "parentUuid": "user-1", "timestamp": stamp, "cwd": "/fixtures/project",
           "requestId": id,
           "message": {"id": format!("msg-{id}"), "role": "assistant", "model": "claude-opus-5-5",
                       "content": [{"type": "text", "text": "Done."}],
                       "usage": {"input_tokens": input, "cache_read_input_tokens": read,
                                 "cache_creation_input_tokens": write_5m + write_1h,
                                 "output_tokens": output,
                                 "output_tokens_details": {"thinking_tokens": thinking},
                                 "cache_creation": {"ephemeral_5m_input_tokens": write_5m,
                                                    "ephemeral_1h_input_tokens": write_1h}}}})
}

fn model_usage(input: u64, read: u64, write: u64, output: u64, thinking: u64) -> Value {
    json!({"inputTokens": input, "outputTokens": output, "thinkingTokens": thinking,
           "cacheReadInputTokens": read, "cacheCreationInputTokens": write,
           "webSearchRequests": 0, "costUSD": 0.5})
}

fn cost_state(models: Value) -> Value {
    json!({"type": "cost-state", "sessionId": SESSION, "timestamp": "2026-10-19T10:05:00Z",
           "totalCostUSD": 1.0, "totalAPIDuration": 1000, "totalDuration": 2000,
           "modelUsage": models, "hasUnknownModelCost": false})
}

/// Two Opus requests that write 400 tokens for five minutes and 2600 for an
/// hour, beside a cost-state that records `models`.
fn recording(models: Value) -> Store {
    let store = Store::new();
    store.write(&[
        user("2026-10-19T10:00:00Z"),
        request(
            "req-1",
            "2026-10-19T10:01:00Z",
            [10, 1000, 0, 2000, 100, 10],
        ),
        request("req-2", "2026-10-19T10:02:00Z", [5, 3000, 400, 600, 50, 5]),
        cost_state(models),
    ]);
    store
}

/// The bounded and the whole usage view, and the session as a listing
/// reads it, each priced under `TABLE`.
fn priced(store: &Store) -> [Value; 3] {
    let table = PriceTable::parse(TABLE).unwrap();
    let backend = || Box::new(ClaudeBackend::new(&store.0)) as Box<dyn Backend>;
    let mut view = usage_with_backends(&[backend()], Selection::Id(SESSION)).unwrap();
    pricing::price_view(&mut view, &table);
    let mut whole = usage_full_with_backends(&[backend()], Selection::Id(SESSION)).unwrap();
    pricing::price_view(&mut whole, &table);
    let mut listed = backend()
        .list(&Query::unscoped(5))
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.id == SESSION)
        .unwrap();
    listed.price(&table);
    let listed = serde_json::to_value(&listed).unwrap();
    [
        serde_json::to_value(view).unwrap(),
        serde_json::to_value(whole).unwrap(),
        json!({"accounting": listed["accounting"],
               "list_price_estimate": listed["list_price_estimate"]}),
    ]
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

/// Claim: a recorded total whose 3000 cache writes the requests split
/// exactly into 400 five-minute and 2600 one-hour writes shows that split in
/// `disjoint` and is priced exactly, $0.034325 (15 x 5 + 4000 x 0.5 + 400 x
/// 6.25 + 2600 x 10 + 150 x 25, per million), on the bounded, whole and
/// listing reads. Rejects a recorded total whose writes are `ttl_unrecorded`
/// and priced as a range between the 5-minute and the 1-hour rate.
#[test]
fn a_recorded_total_the_requests_split_exactly_is_priced_exactly() {
    let store = recording(json!({"claude-opus-5-5": model_usage(15, 4000, 3000, 150, 15)}));
    for figure in priced(&store) {
        assert_eq!(
            figure["accounting"]["basis"], "recorded-total",
            "{figure:#}"
        );
        let estimate = &figure["list_price_estimate"];
        assert!(close(&estimate["amount"], 0.034_325), "{estimate:#}");
        assert!(estimate.get("at_least").is_none(), "{estimate:#}");
        assert!(reasons(estimate, "cache_write").is_empty(), "{estimate:#}");
    }
    let [_, whole, _] = priced(&store);
    let disjoint = &whole["disjoint"];
    assert_eq!(
        disjoint["cache_write"],
        json!({"ephemeral_5m": 400, "ephemeral_1h": 2600}),
        "{disjoint:#}"
    );
    assert!(
        !disjoint["missing"]
            .as_array()
            .is_some_and(|missing| missing.contains(&json!("cache_write_ttl"))),
        "{disjoint:#}"
    );
    assert!(
        close(
            &whole["by_model"][0]["list_price_estimate"]["amount"],
            0.034_325
        ),
        "{whole:#}"
    );
}

/// Claim: a recorded total of 4000 cache writes, of which the requests
/// account for 3000, keeps all 4000 `ttl_unrecorded` and is priced from
/// $0.030825 (every write at the 5-minute rate) to $0.045825 (every write at
/// the 1-hour rate), naming ttl-unrecorded. Rejects a split the requests do
/// not reconcile with: taking their 400/2600 split as the total's, or
/// assigning the 1000 writes they leave out a TTL.
#[test]
fn a_recorded_total_the_requests_fall_short_of_stays_a_range() {
    let store = recording(json!({"claude-opus-5-5": model_usage(15, 4000, 4000, 150, 15)}));
    for figure in priced(&store) {
        assert_eq!(
            figure["accounting"]["basis"], "recorded-total",
            "{figure:#}"
        );
        let estimate = &figure["list_price_estimate"];
        assert!(close(&estimate["at_least"], 0.030_825), "{estimate:#}");
        assert!(close(&estimate["at_most"], 0.045_825), "{estimate:#}");
        assert_eq!(
            reasons(estimate, "cache_write"),
            ["ttl-unrecorded"],
            "{estimate:#}"
        );
    }
    let [_, whole, _] = priced(&store);
    assert_eq!(
        whole["disjoint"]["cache_write"],
        json!({"ttl_unrecorded": 4000}),
        "{whole:#}"
    );
}

/// Claim: in a recorded total over Opus and Haiku, where the Opus requests
/// split the Opus share's 3000 writes exactly and Haiku left no requests,
/// the Opus share is priced exactly at $0.034325 and the Haiku share's 500
/// writes stay a range, $0.000825 to $0.0012; the figure, whose 3500 writes
/// the requests do not reconcile with, is the shares' sum and keeps its
/// writes `ttl_unrecorded`. Rejects an Opus share priced as a range, and a
/// figure that takes the Opus requests' split for writes Haiku made.
#[test]
fn each_model_share_takes_the_split_its_own_requests_reconcile_with() {
    let store = recording(json!({
        "claude-opus-5-5": model_usage(15, 4000, 3000, 150, 15),
        "claude-haiku-5-5": model_usage(100, 0, 500, 20, 0),
    }));
    let [_, whole, _] = priced(&store);
    let share = |model: &str| {
        whole["by_model"]
            .as_array()
            .unwrap()
            .iter()
            .find(|share| share["model"] == model)
            .unwrap()["list_price_estimate"]
            .clone()
    };
    let opus = share("claude-opus-5-5");
    assert!(close(&opus["amount"], 0.034_325), "{opus:#}");
    assert!(reasons(&opus, "cache_write").is_empty(), "{opus:#}");
    let haiku = share("claude-haiku-5-5");
    assert!(close(&haiku["at_least"], 0.000_825), "{haiku:#}");
    assert!(close(&haiku["at_most"], 0.001_2), "{haiku:#}");
    let estimate = &whole["list_price_estimate"];
    assert!(close(&estimate["at_least"], 0.035_15), "{estimate:#}");
    assert!(close(&estimate["at_most"], 0.035_525), "{estimate:#}");
    assert_eq!(
        whole["disjoint"]["cache_write"],
        json!({"ttl_unrecorded": 3500}),
        "{whole:#}"
    );
}
