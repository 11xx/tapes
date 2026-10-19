//! Allowance rates and forecasts measure every completed session of their
//! lookback, and an explicit `--limit` that keeps fewer says how many it
//! left out.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const LOOKBACK: &str = "2026-01-10T09:00:00Z..2026-01-10T16:00:00Z";
const LANES: usize = 25;

struct Store(PathBuf);

impl Store {
    /// Twenty-five completed Codex sessions of one account, each spending
    /// inside the interval of one session's two observations, 10% at 10:00
    /// and 20% at 12:00.
    fn lanes() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-series-lanes-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("home")).unwrap();
        let store = Self(root);
        for lane in 0..LANES {
            store.rollout(lane);
        }
        store
    }

    fn rollout(&self, lane: usize) {
        let id = format!("40000000-0000-7000-8000-{lane:012}");
        let usage = json!({"input_tokens": 100, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
                           "output_tokens": 0, "reasoning_output_tokens": 0, "total_tokens": 100});
        let observed = |stamp: &str, used: f64| {
            json!({"timestamp": stamp, "type": "event_msg",
                   "payload": {"type": "token_count", "info": null, "rate_limits": {
                       "limit_id": "codex", "plan_type": "plus",
                       "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_768_060_800},
                       "secondary": null}}})
        };
        let mut lines = vec![
            json!({"timestamp": "2026-01-10T09:00:00Z", "type": "session_meta", "payload": {"id": id,
                   "timestamp": "2026-01-10T09:00:00Z", "cwd": "/fixtures/project", "source": "exec",
                   "creator_account_id": "account-a"}}),
            json!({"timestamp": "2026-01-10T09:00:00Z", "type": "turn_context",
                   "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
        ];
        if lane == 0 {
            lines.push(observed("2026-01-10T10:00:00Z", 10.0));
            lines.push(observed("2026-01-10T12:00:00Z", 20.0));
        }
        lines.push(
            json!({"timestamp": format!("2026-01-10T10:{:02}:00Z", 10 + lane), "type": "event_msg",
                          "payload": {"type": "token_count", "info": {"total_token_usage": usage,
                              "last_token_usage": usage, "model_context_window": 272000},
                              "rate_limits": null}}),
        );
        let path = self.0.join(format!(
            "sessions/2026/01/10/rollout-2026-01-10T09-00-00-{id}.jsonl"
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .arg("usage")
            .args(arguments)
            .args(["--harness", "codex", "--window", LOOKBACK, "--json"])
            .args(["--native-store", "codex", "--native-root"])
            .arg(&self.0)
            .output()
            .unwrap();
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

/// Claim: without `--limit`, allowance rates measure all 25 completed
/// sessions of the lookback and say they read up to the lane bound; an
/// explicit `--limit 5` keeps the newest five and says it left out the other
/// twenty; a forecast says the same of the rates it sizes from. Rejects
/// rates that take the listing's default of 20 sessions as their lanes, or
/// a limit that leaves sessions out without saying so.
#[test]
fn rates_measure_every_lane_and_name_a_limit() {
    let store = Store::lanes();
    let rates = store.json(&["--allowance-rates", "--by", "model"]);
    assert_eq!(rates["groups"][0]["lanes"], LANES, "{rates:#}");
    assert_eq!(rates["lanes_read"]["explicit_limit"], false);
    assert_eq!(rates["lanes_read"]["limit"], 10_000);
    assert!(rates["lanes_read"].get("left_out_by_limit").is_none());

    let limited = store.json(&["--allowance-rates", "--by", "model", "--limit", "5"]);
    assert_eq!(limited["groups"][0]["lanes"], 5, "{limited:#}");
    assert_eq!(limited["lanes_read"]["explicit_limit"], true);
    assert_eq!(limited["lanes_read"]["limit"], 5);
    assert_eq!(limited["lanes_read"]["left_out_by_limit"], LANES - 5);

    let forecast = store.json(&["--forecast", "gpt-fixture#high=1"]);
    assert_eq!(forecast["plan"][0]["measured_lanes"], LANES, "{forecast:#}");
    assert_eq!(forecast["lanes_read"]["explicit_limit"], false);
}
