//! `tapes usage --window` and `--quota-window` answer a selection's
//! consumption inside a window, and refuse what they cannot answer.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-cli-window-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("projects/-fixtures-project");
        fs::create_dir_all(&project).unwrap();
        for (id, minutes) in [
            ("30000000-0000-4000-8000-0000000000e1", ["09:50", "10:20"]),
            ("30000000-0000-4000-8000-0000000000e2", ["10:40", "11:10"]),
        ] {
            let mut lines = Vec::new();
            for (index, minute) in minutes.iter().enumerate() {
                lines.push(json!({
                    "type": "assistant", "sessionId": id, "uuid": format!("{id}-{index}"), "entrypoint": "sdk-cli",
                    "requestId": format!("{id}-r{index}"), "timestamp": format!("2026-01-10T{minute}:00Z"),
                    "cwd": "/fixtures/project",
                    "message": {"role": "assistant", "model": "claude-fixture", "content": [{"type": "text", "text": "ok"}],
                                "usage": {"input_tokens": 1, "output_tokens": 100, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
                }).to_string());
            }
            fs::write(project.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
        }
        Self(root)
    }

    fn usage(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env("TAPES_OFFLINE", "1")
            .env_remove("PI_SESSION_FILE")
            .env_remove("TAPES_PRICES")
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .arg("usage")
            .args(arguments)
            .args(["--native-store", "claude", "--native-root"])
            .arg(&self.0)
            .output()
            .unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claim: `--window` answers the summary with a `window` naming its edges
/// and each session's share, counting only requests inside, and the text
/// answer lists the shares before the groups. Rejects a window that sums
/// whole sessions.
#[test]
fn a_window_answers_each_sessions_share() {
    let store = Store::new();
    let output = store.usage(&[
        "--global",
        "--window",
        "2026-01-10T10:00:00Z..2026-01-10T11:00:00Z",
        "--by",
        "session,launch",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["window"]["from"], "2026-01-10T10:00:00Z");
    assert_eq!(summary["window"]["boundary"], "requested");
    assert_eq!(summary["totals"]["tokens"]["output"], 200);
    assert_eq!(summary["groups"][0]["key"]["launch"], "headless");
    let text = String::from_utf8(
        store
            .usage(&[
                "--global",
                "--window",
                "2026-01-10T10:00:00Z..",
                "--by",
                "session",
            ])
            .stdout,
    )
    .unwrap();
    assert!(
        text.starts_with("window: 2026-01-10T10:00:00+00:00 .. now (requested)"),
        "{text}"
    );
    assert!(
        text.contains("30000000-0000-4000-8000-0000000000e1\tclaude\theadless\t-\t1 of 2\t101"),
        "{text}"
    );
}

/// Claim: window flags answer a selection and refuse a single session, an
/// inverted window, a quota window without its harness, and a harness whose
/// recordings hold no quota observation, which names `--window`. Rejects a
/// window silently applied to one session or answered without evidence.
#[test]
fn window_flags_refuse_what_they_cannot_answer() {
    let store = Store::new();
    let refused = |arguments: &[&str], expected: &str| {
        let output = store.usage(arguments);
        assert!(!output.status.success(), "{arguments:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{arguments:?}: {stderr}");
    };
    refused(
        &[
            "30000000-0000-4000-8000-0000000000e1",
            "--window",
            "2026-01-10T10:00:00Z..",
        ],
        "cannot be used with",
    );
    refused(
        &[
            "--global",
            "--window",
            "2026-01-10T11:00:00Z..2026-01-10T10:00:00Z",
        ],
        "ends before it starts",
    );
    refused(&["--global", "--quota-window", "current"], "--harness");
    refused(
        &[
            "--global",
            "--quota-window",
            "current",
            "--harness",
            "claude",
        ],
        "read the window with --window",
    );
}
