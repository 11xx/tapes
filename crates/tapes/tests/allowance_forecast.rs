//! Allowance rates give each lane's percent smoothed over its window
//! periods beside the interval attribution, and `usage --forecast` sizes a
//! planned lane mix from those rates against the window's newest
//! observation.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const A: &str = "40000000-0000-7000-8000-0000000000a1";
const B: &str = "40000000-0000-7000-8000-0000000000a2";

/// The first period's reset, 2026-01-10T16:00:00Z, and the second's,
/// 2030-01-01T00:00:00Z, far enough ahead that it has not passed.
const FIRST_RESET: i64 = 1_768_060_800;
const SECOND_RESET: i64 = 1_893_456_000;

const LOOKBACK: &str = "2026-01-10T09:00:00Z..2026-01-10T16:00:00Z";

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-forecast-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("home")).unwrap();
        Self(root)
    }

    fn run(&self, store: &str, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env_remove("TAPES_PRICES")
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .arg("usage")
            .args(arguments)
            .args(["--native-store", store, "--native-root"])
            .arg(&self.0)
            .output()
            .unwrap()
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self.run("codex", &[arguments, &["--json"]].concat());
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn usage(input: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
           "output_tokens": 0, "reasoning_output_tokens": 0, "total_tokens": input})
}

enum Event {
    /// The primary used percent, under the period that resets at the given
    /// instant.
    Observed(&'static str, f64, i64),
    /// One request's uncached input.
    Request(&'static str, u64),
}

fn rollout(store: &Store, id: &str, events: &[Event]) {
    let mut lines = vec![
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "session_meta", "payload": {"id": id,
               "timestamp": "2026-01-10T09:00:00Z", "cwd": "/fixtures/project", "source": "exec",
               "creator_account_id": "account-a"}}),
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
    ];
    let mut total = 0;
    for event in events {
        lines.push(match event {
            Event::Observed(stamp, used, resets) => json!({
                "timestamp": stamp, "type": "event_msg",
                "payload": {"type": "token_count", "info": null, "rate_limits": {
                    "limit_id": "codex", "plan_type": "plus",
                    "primary": {"used_percent": used, "window_minutes": 300, "resets_at": resets},
                    "secondary": null}}
            }),
            Event::Request(stamp, input) => {
                total += input;
                json!({"timestamp": stamp, "type": "event_msg",
                       "payload": {"type": "token_count", "info": {
                           "total_token_usage": usage(total), "last_token_usage": usage(*input),
                           "model_context_window": 272000}, "rate_limits": null}})
            }
        });
    }
    let path = store.0.join(format!(
        "sessions/2026/01/10/rollout-2026-01-10T09-00-00-{id}.jsonl"
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body: Vec<String> = lines.iter().map(Value::to_string).collect();
    fs::write(path, body.join("\n") + "\n").unwrap();
}

/// Two lanes over two window periods. The first period rises 10 to 13
/// while A spends 300 tokens and B 100; the second rises 2 to 4 while A
/// spends 100, then 4 to 6 while B spends 300.
fn two_periods() -> Store {
    use Event::{Observed, Request};
    let store = Store::new("periods");
    rollout(
        &store,
        A,
        &[
            Observed("2026-01-10T10:00:00Z", 10.0, FIRST_RESET),
            Request("2026-01-10T10:30:00Z", 300),
            Observed("2026-01-10T11:00:00Z", 13.0, FIRST_RESET),
            Observed("2026-01-10T12:00:00Z", 2.0, SECOND_RESET),
            Request("2026-01-10T12:30:00Z", 100),
            Observed("2026-01-10T13:00:00Z", 4.0, SECOND_RESET),
            Observed("2026-01-10T14:00:00Z", 6.0, SECOND_RESET),
        ],
    );
    rollout(
        &store,
        B,
        &[
            Request("2026-01-10T10:40:00Z", 100),
            Request("2026-01-10T13:30:00Z", 300),
        ],
    );
    store
}

fn close(actual: &Value, expected: f64, what: &str) {
    let actual = actual
        .as_f64()
        .unwrap_or_else(|| panic!("{what}: {actual}"));
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: {actual} != {expected}"
    );
}

fn lane(rates: &Value, id: &str) -> Value {
    rates["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["key"]["session"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no lane {id}: {rates:#}"))
}

/// Claim: each lane's smoothed percent is its share of each period's tokens
/// times the period's rise, with the period's endpoint rounding as bounds:
/// A is 300/400 x 3 + 100/400 x 4 = 3.25 (between 300/400 x 2 + 100/400 x 3
/// = 2.25 and 300/400 x 4 + 100/400 x 5 = 4.25), B is 3.75 (2.75 to 4.75),
/// beside the interval attribution of 4.25 and 2.75, each figure naming its
/// estimator. Rejects rates that report only the interval attribution.
#[test]
fn the_smoothed_lane_estimate_spreads_each_periods_rise() {
    let store = two_periods();
    let rates = store.json(&[
        "--allowance-rates",
        "--harness",
        "codex",
        "--window",
        LOOKBACK,
        "--by",
        "session",
    ]);
    assert_eq!(rates["schema"], "tapes-allowance-rates/6");
    for (id, smoothed, low, high, interval) in
        [(A, 3.25, 2.25, 4.25, 4.25), (B, 3.75, 2.75, 4.75, 2.75)]
    {
        let lane = lane(&rates, id);
        let figure = &lane["smoothed_percent_per_lane"];
        assert_eq!(figure["estimator"], "period-smoothed");
        close(&figure["estimate"]["median"], smoothed, id);
        close(&figure["at_least"]["median"], low, id);
        close(&figure["at_most"]["median"], high, id);
        assert_eq!(
            lane["percent_per_lane"]["estimator"],
            "interval-apportioned"
        );
        close(
            &lane["percent_per_lane"]["estimate"]["median"],
            interval,
            id,
        );
        close(&lane["tokens_per_lane"]["median"], 400.0, id);
    }
}

/// Claim: a forecast of two lanes sizes each as the group's median lane
/// tokens over its tokens per percent, 400 / (800 / 7) = 3.5, with the
/// ratio's bounds, so the mix is 7 of the 300-minute window; the newest
/// observation leaves 94 (93.5 to 94.5) before the 2030 reset; and two lanes
/// at once, each spending 3.5 over the median 145 minutes, spend it in
/// 94 / (7 / 145) minutes, before that reset. Rejects a forecast that
/// assumes the window's length or reset, or that does not size the mix.
#[test]
fn a_forecast_sizes_the_mix_and_the_time_left() {
    let store = two_periods();
    let rates = store.json(&[
        "--allowance-rates",
        "--harness",
        "codex",
        "--window",
        LOOKBACK,
        "--by",
        "model,variant",
    ]);
    let ratio = &rates["groups"][0]["tokens_per_percent"]["total"];
    let forecast = store.json(&[
        "--forecast",
        "gpt-fixture#high=2",
        "--concurrency",
        "2",
        "--harness",
        "codex",
        "--window",
        LOOKBACK,
    ]);
    assert_eq!(forecast["schema"], "tapes-allowance-forecast/2");
    let state = &forecast["window_state"];
    assert_eq!(state["window_minutes"], 300);
    assert_eq!(state["resets_at"], "2030-01-01T00:00:00Z");
    close(&state["used_percent"], 6.0, "used");
    close(&state["remaining_percent"]["estimate"], 94.0, "left");
    close(&state["remaining_percent"]["at_least"], 93.5, "left");
    close(&state["remaining_percent"]["at_most"], 94.5, "left");
    let lanes = &forecast["plan"][0];
    assert_eq!(lanes["measured_lanes"], 2);
    close(&lanes["median_lane_tokens"], 400.0, "tokens");
    close(
        &lanes["tokens_per_percent"]["estimate"],
        800.0 / 7.0,
        "ratio",
    );
    close(&lanes["percent_per_lane"]["estimate"], 3.5, "lane");
    close(
        &lanes["percent_per_lane"]["at_least"],
        400.0 / ratio["at_most"].as_f64().unwrap(),
        "lane low",
    );
    close(
        &lanes["percent_per_lane"]["at_most"],
        400.0 / ratio["at_least"].as_f64().unwrap(),
        "lane high",
    );
    close(&forecast["percent"]["estimate"], 7.0, "mix");
    close(&lanes["median_duration_minutes"], 145.0, "duration");
    close(
        &forecast["percent_per_minute"]["estimate"],
        7.0 / 145.0,
        "pace",
    );
    close(
        &forecast["minutes_to_exhaustion"]["estimate"],
        94.0 / (7.0 / 145.0),
        "exhaustion",
    );
    assert_eq!(forecast["fits"], "yes");
    assert_eq!(forecast["exhausted_before_reset"], true);
}

/// Claim: a model and variant with no measured lanes refuses, naming it,
/// and a Claude forecast refuses because Claude records no used percent.
/// Rejects sizing an unmeasured model from another, or a Claude answer.
#[test]
fn an_unmeasured_model_and_a_claude_forecast_refuse() {
    let store = two_periods();
    let output = store.run(
        "codex",
        &[
            "--forecast",
            "gpt-fixture#high=1,gpt-other#low=1",
            "--harness",
            "codex",
            "--window",
            LOOKBACK,
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("gpt-other#low has no measured lanes"),
        "{stderr}"
    );
    assert!(stderr.contains("300-minute codex window"), "{stderr}");

    fs::create_dir_all(store.0.join("projects")).unwrap();
    let output = store.run(
        "claude",
        &["--forecast", "claude-fixture=1", "--harness", "claude"],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("claude records no used percent"),
        "{stderr}"
    );
}
