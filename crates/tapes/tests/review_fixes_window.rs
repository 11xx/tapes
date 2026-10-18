//! `tapes usage --quota-window` answers a selection's consumption inside a
//! quota window, so a single named session refuses it as it refuses
//! `--window`, rather than answering as though it were not asked.

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

/// Claim: `usage <id> --harness codex --quota-window current` refuses,
/// naming the conflict between a single session and a quota window.
/// Rejects answering the session's own `tapes-usage` view with no window,
/// as though `--quota-window` had not been given.
#[test]
fn a_single_session_refuses_a_quota_window() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-quota-single-{}-{nonce}",
        std::process::id()
    ));
    let id = "40000000-0000-7000-8000-0000000000f6";
    let path = root.join(format!(
        "sessions/2026/01/10/rollout-2026-01-10T09-00-00-{id}.jsonl"
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let usage = |input: u64| {
        json!({"input_tokens": input, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
               "output_tokens": 0, "reasoning_output_tokens": 0, "total_tokens": input})
    };
    let lines = [
        json!({"timestamp": "2026-01-10T09:00:00Z", "type": "session_meta", "payload": {"id": id,
               "timestamp": "2026-01-10T09:00:00Z", "cwd": "/fixtures/project", "source": "exec"}}),
        json!({"timestamp": "2026-01-10T09:05:00Z", "type": "event_msg", "payload": {"type": "token_count",
               "info": {"total_token_usage": usage(100), "last_token_usage": usage(100)},
               "rate_limits": {"limit_id": "codex", "plan_type": "plus",
                   "primary": {"used_percent": 10.0, "window_minutes": 300, "resets_at": 1_768_053_600},
                   "secondary": null}}}),
    ];
    let body = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    fs::write(&path, body.join("\n") + "\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .env_remove("TAPES_PRICES")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .args([
            "usage",
            id,
            "--harness",
            "codex",
            "--quota-window",
            "current",
            "--json",
        ])
        .args(["--native-store", "codex", "--native-root"])
        .arg(&root)
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&root);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a single session answered a quota window: {stdout}"
    );
    assert!(
        stderr.contains("--quota-window") && stderr.contains("cannot be used with"),
        "{stderr}"
    );
}
