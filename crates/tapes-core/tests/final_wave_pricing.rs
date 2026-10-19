//! A pi consumption on its active branch alone is priced as the session
//! figure is: share by share when its events name several models, and whole
//! by the one model in evidence when some of its usage names none.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{pi::PiBackend, Backend};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
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
            "tapes-final-wave-pricing-{label}-{}-{nonce}",
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

// --- Multi-model pi consumption -------------------------------------------

const PI_ID: &str = "019f0000-0000-7000-8000-0000000000fa";

/// A pi session on one branch: a GLM request at reasoning level `high`, then
/// a `second` model request at `max`, each switched to by `model_change` and
/// `thinking_level_change` entries as pi records them. A `compaction` between
/// them records its own usage and names no model, as pi's compactions do.
fn pi_session(store: &Store, second: &str, compaction: bool) {
    let entry = |kind: &str, id: &str, parent: Option<&str>, stamp: &str, body: Value| {
        let mut line = json!({"type": kind, "id": id, "parentId": parent, "timestamp": stamp});
        line.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        line
    };
    let millis = |stamp: &str| at(stamp).timestamp_millis();
    let user = |stamp: &str, text: &str| {
        json!({"message": {"role": "user", "timestamp": millis(stamp),
                           "content": [{"type": "text", "text": text}]}})
    };
    let assistant = |stamp: &str, model: &str, input: u64, read: u64, output: u64| {
        json!({"message": {"role": "assistant", "timestamp": millis(stamp),
            "provider": "commandcode", "model": model, "api": "openai-completions",
            "stopReason": "stop",
            "usage": {"input": input, "output": output, "cacheRead": read, "cacheWrite": 0,
                      "reasoning": 0, "totalTokens": input + read + output,
                      "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                               "total": 0}},
            "content": [{"type": "text", "text": "Done."}]}})
    };
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": PI_ID,
                   "timestamp": "2026-10-19T08:00:00.000Z", "cwd": "/fixtures/project"}),
        entry(
            "model_change",
            "a0000001",
            None,
            "2026-10-19T08:00:00.100Z",
            json!({"provider": "commandcode", "modelId": "z-ai/glm-5.3-flash"}),
        ),
        entry(
            "thinking_level_change",
            "a0000002",
            Some("a0000001"),
            "2026-10-19T08:00:00.100Z",
            json!({"thinkingLevel": "high"}),
        ),
        entry(
            "message",
            "a0000003",
            Some("a0000002"),
            "2026-10-19T08:00:01.000Z",
            user("2026-10-19T08:00:01.000Z", "Survey the fixture."),
        ),
        entry(
            "message",
            "a0000004",
            Some("a0000003"),
            "2026-10-19T08:00:20.000Z",
            assistant(
                "2026-10-19T08:00:20.000Z",
                "z-ai/glm-5.3-flash",
                10_000,
                90_000,
                2_000,
            ),
        ),
    ];
    if compaction {
        lines.push(entry(
            "compaction",
            "a000000c",
            Some("a0000004"),
            "2026-10-19T08:00:40.000Z",
            json!({"summary": "The fixture was surveyed.", "firstKeptEntryId": "a0000003",
                   "tokensBefore": 102_000, "fromHook": false,
                   "usage": {"input": 5_000, "output": 1_000, "cacheRead": 0, "cacheWrite": 0,
                             "reasoning": 0, "totalTokens": 6_000,
                             "cost": {"input": 0, "output": 0, "cacheRead": 0,
                                      "cacheWrite": 0, "total": 0}}}),
        ));
    }
    let before_switch = if compaction { "a000000c" } else { "a0000004" };
    lines.extend([
        entry(
            "model_change",
            "a0000005",
            Some(before_switch),
            "2026-10-19T08:01:00.000Z",
            json!({"provider": "commandcode", "modelId": second}),
        ),
        entry(
            "thinking_level_change",
            "a0000006",
            Some("a0000005"),
            "2026-10-19T08:01:00.000Z",
            json!({"thinkingLevel": "max"}),
        ),
        entry(
            "message",
            "a0000007",
            Some("a0000006"),
            "2026-10-19T08:01:01.000Z",
            user("2026-10-19T08:01:01.000Z", "Again."),
        ),
        entry(
            "message",
            "a0000008",
            Some("a0000007"),
            "2026-10-19T08:01:20.000Z",
            assistant("2026-10-19T08:01:20.000Z", second, 20_000, 40_000, 4_000),
        ),
    ]);
    store.write(
        &format!("--fixtures-project--/2026-10-19T08-00-00-000Z_{PI_ID}.jsonl"),
        &lines,
    );
}

const MIXED_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "z-ai/glm-5.3-flash"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.0, cache_write_1h = 1.0, output = 4.0 }

[[models]]
id = "deepseek-flash"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 0.5, cache_read = 0.05, cache_write_5m = 0.5, cache_write_1h = 0.5, output = 2.0 }
"#;

// GLM: 10k uncached x 1.0 + 90k read x 0.1 + 2k output x 4.0; DeepSeek:
// 20k uncached x 0.5 + 40k read x 0.05 + 4k output x 2.0.
const MIXED_AMOUNT: f64 = (10_000.0 + 9_000.0 + 8_000.0 + 10_000.0 + 2_000.0 + 8_000.0) / 1e6;

/// Claim: a pi session whose one branch holds a GLM request and a DeepSeek
/// request prices its retained-branch `consumption` share by share, to the
/// same amount as the session figure, in a tail read and a whole read.
/// Rejects pricing a consumption with no other-branch events through the
/// session's model evidence alone, which names two models and no split and
/// so leaves the consumption without an estimate beside a priced figure.
#[test]
fn a_multi_model_consumption_on_one_branch_is_priced_as_its_figure() {
    let store = Store::new("mixed");
    pi_session(&store, "deepseek-flash", false);
    let table = PriceTable::parse(MIXED_TABLE).unwrap();
    let backend = || Box::new(PiBackend::new(&store.0)) as Box<dyn Backend>;
    let tail = usage_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    let whole = usage_full_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    for mut view in [tail, whole] {
        pricing::price_view(&mut view, &table);
        let view = serde_json::to_value(view).unwrap();
        assert_eq!(
            view["consumption"]["events"]["other_branches"], 0,
            "{view:#}"
        );
        let figure = &view["list_price_estimate"];
        let consumption = &view["consumption"]["list_price_estimate"];
        assert!(close(&figure["amount"], MIXED_AMOUNT), "{figure:#}");
        assert!(
            close(&consumption["amount"], MIXED_AMOUNT),
            "{consumption:#}"
        );
        assert!(
            consumption.get("measurement_completeness").is_none(),
            "{consumption:#}"
        );
    }
}

// GLM throughout: the two requests, 27,000 and 40,000 micro-dollars, and
// the compaction's 5k uncached x 1.0 + 1k output x 4.0.
const SOLE_AMOUNT: f64 = (27_000.0 + 40_000.0 + 9_000.0) / 1e6;

/// Claim: a one-model pi session whose compaction records usage naming no
/// model, read with a record it cannot decode, prices its `consumption`
/// whole by that model, as its figure is, so the two agree in amount and in
/// qualifying every category `incomplete-attribution`. Rejects splitting such a consumption by the models its
/// events name, which under the unreadable record's uncertain attribution
/// leaves the compaction out of every share and prices the consumption
/// short of its figure.
#[test]
fn a_consumption_with_usage_no_model_names_is_priced_as_its_figure() {
    let store = Store::new("sole");
    pi_session(&store, "z-ai/glm-5.3-flash", true);
    let path = store.0.join(format!(
        "--fixtures-project--/2026-10-19T08-00-00-000Z_{PI_ID}.jsonl"
    ));
    let mut lines = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.insert(4, r#"{"type":"message","id":"a00000"#.to_owned());
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    let table = PriceTable::parse(MIXED_TABLE).unwrap();
    let backend = || Box::new(PiBackend::new(&store.0)) as Box<dyn Backend>;
    let tail = usage_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    let whole = usage_full_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    for mut view in [tail, whole] {
        pricing::price_view(&mut view, &table);
        let view = serde_json::to_value(view).unwrap();
        let figure = &view["list_price_estimate"];
        let consumption = &view["consumption"]["list_price_estimate"];
        assert!(close(&figure["amount"], SOLE_AMOUNT), "{figure:#}");
        assert!(close(&consumption["amount"], SOLE_AMOUNT), "{view:#}");
        assert_eq!(consumption["models"], figure["models"], "{view:#}");
        for estimate in [figure, consumption] {
            for category in ["uncached_input", "cache_read", "cache_write", "output"] {
                let reasons = &estimate["measurement_completeness"][category];
                assert!(
                    reasons
                        .as_array()
                        .is_some_and(|reasons| reasons.contains(&json!("incomplete-attribution"))),
                    "{category}: {estimate:#}"
                );
            }
        }
    }
}
