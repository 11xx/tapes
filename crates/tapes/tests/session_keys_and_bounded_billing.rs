//! A text summary grouped by session writes an id two harnesses' sessions
//! share as `<harness>:<id>`, one row per session.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// One id a Claude session and a Codex session both carry.
const SHARED: &str = "01a10000-0000-7000-8000-0000000000c1";
/// A Claude session whose id no other harness holds.
const ALONE: &str = "30000000-0000-4000-8000-0000000000c2";

struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-cli-session-keys-{}-{nonce}",
            std::process::id()
        ));
        let store = Self(root);
        for id in [SHARED, ALONE] {
            store.write(
                &format!("home/.claude/projects/-fixtures-project/{id}.jsonl"),
                &[
                    json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"),
                        "entrypoint": "sdk-cli", "timestamp": "2026-01-10T10:00:00Z",
                        "cwd": "/fixtures/project",
                        "message": {"role": "user", "content": [{"type": "text", "text": "Lead."}]}}),
                    json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"),
                        "entrypoint": "sdk-cli", "requestId": format!("{id}-request"),
                        "timestamp": "2026-01-10T10:00:30Z", "cwd": "/fixtures/project",
                        "message": {"role": "assistant", "model": "claude-fixture",
                            "content": [{"type": "text", "text": "ok"}],
                            "usage": {"input_tokens": 1, "output_tokens": 100,
                                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}}),
                ],
            );
        }
        let start = "2026-01-10T10:01:00Z";
        let total = json!({"input_tokens": 100, "cached_input_tokens": 0, "output_tokens": 7,
            "reasoning_output_tokens": 0, "total_tokens": 107});
        store.write(
            &format!("codex/sessions/2026/01/10/rollout-2026-01-10T10-01-00-{SHARED}.jsonl"),
            &[
                json!({"timestamp": start, "type": "session_meta",
                    "payload": {"id": SHARED, "timestamp": start, "cwd": "/fixtures/project",
                        "source": "exec", "originator": "codex_exec"}}),
                json!({"timestamp": start, "type": "turn_context",
                    "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
                json!({"timestamp": start, "type": "response_item",
                    "payload": {"type": "message", "role": "user",
                        "content": [{"type": "input_text", "text": "Implement."}]}}),
                json!({"timestamp": "2026-01-10T10:05:00Z", "type": "event_msg",
                    "payload": {"type": "token_count", "info": {"total_token_usage": total,
                        "last_token_usage": total, "model_context_window": 272000}}}),
            ],
        );
        store
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(path, body).unwrap();
    }

    fn usage(&self, arguments: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env_remove("PI_SESSION_FILE")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("PI_CODING_AGENT_SESSION_DIR")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("TAPES_PRICES")
            .env_remove("XDG_DATA_HOME")
            .env("HOME", self.0.join("home"))
            .env("CODEX_HOME", self.0.join("codex"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("PATH", "/usr/bin:/bin")
            .arg("usage")
            .args(arguments)
            .arg("--global")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claim: grouped by session, the text table writes an id a Claude session
/// and a Codex session share as `<harness>:<id>`, one row for each, and an id
/// only one harness holds bare. Rejects the two sessions summed in one row
/// under the bare id, and an ordinary id qualified with its harness.
#[test]
fn a_shared_session_id_is_written_with_its_harness() {
    let store = Store::new();
    let summary = store.usage(&["--by", "session"]);
    let rows = |prefix: String| {
        summary
            .lines()
            .filter(|line| line.starts_with(&prefix))
            .count()
    };
    assert_eq!(rows(format!("claude:{SHARED}\t")), 1, "{summary}");
    assert_eq!(rows(format!("codex:{SHARED}\t")), 1, "{summary}");
    assert_eq!(rows(format!("{SHARED}\t")), 0, "{summary}");
    assert_eq!(rows(format!("{ALONE}\t")), 1, "{summary}");
}
