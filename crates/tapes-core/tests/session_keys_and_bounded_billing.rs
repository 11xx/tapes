//! A `--by session` group names its session by harness and id together, so
//! sessions of different harnesses that share an id stay apart.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_rates_with_backends, usage_summary_with_backends, usage_window_with_backends,
    SessionSelection, WindowAsk,
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
            "tapes-session-keys-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(&path, body).unwrap();
        path
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.0.join("claude/projects"))),
            Box::new(CodexBackend::new(self.0.join("codex"))),
        ]
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

fn selection() -> SessionSelection<'static> {
    SessionSelection {
        limit: Some(50),
        ..Default::default()
    }
}

// --- Session groups keyed by harness and id ---------------------------------

/// One id a Claude session and a Codex session both carry.
const SHARED: &str = "01a10000-0000-7000-8000-0000000000d1";

fn claude_session(store: &Store, id: &str) {
    store.write(
        &format!("claude/projects/-fixtures-project/{id}.jsonl"),
        &[
            json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"),
                "entrypoint": "sdk-cli", "timestamp": "2026-01-10T10:00:01Z",
                "cwd": "/fixtures/project",
                "message": {"role": "user", "content": [{"type": "text", "text": "Lead."}]}}),
            json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"),
                "entrypoint": "sdk-cli", "requestId": format!("{id}-request"),
                "timestamp": "2026-01-10T10:00:30Z", "cwd": "/fixtures/project",
                "message": {"role": "assistant", "model": "claude-fixture",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input_tokens": 1, "output_tokens": 5,
                        "cache_read_input_tokens": 10, "cache_creation_input_tokens": 2}}}),
        ],
    );
}

fn codex_usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cached_input_tokens": 0, "output_tokens": output,
           "reasoning_output_tokens": 0, "total_tokens": input + output})
}

fn codex_quota(stamp: &str, used: f64) -> Value {
    json!({"timestamp": stamp, "type": "event_msg",
           "payload": {"type": "token_count", "info": null, "rate_limits": {
               "limit_id": "codex", "plan_type": "plus",
               "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 1_768_053_600},
               "secondary": null}}})
}

fn codex_session(store: &Store, id: &str) {
    let start = "2026-01-10T10:01:00Z";
    store.write(
        &format!("codex/2026/01/10/rollout-2026-01-10T10-01-00-{id}.jsonl"),
        &[
            json!({"timestamp": start, "type": "session_meta",
                "payload": {"id": id, "timestamp": start, "cwd": "/fixtures/project",
                    "source": "exec", "originator": "codex_exec"}}),
            json!({"timestamp": start, "type": "turn_context",
                "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture", "effort": "high"}}),
            json!({"timestamp": start, "type": "response_item",
                "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "Implement."}]}}),
            codex_quota("2026-01-10T10:02:00Z", 10.0),
            json!({"timestamp": "2026-01-10T10:05:00Z", "type": "event_msg",
                "payload": {"type": "token_count", "info": {
                    "total_token_usage": codex_usage(100, 7),
                    "last_token_usage": codex_usage(100, 7), "model_context_window": 272000},
                    "rate_limits": null}}),
            codex_quota("2026-01-10T10:10:00Z", 15.0),
        ],
    );
}

/// Each session group's key, as its harness and id, and its session count
/// and input tokens.
fn session_groups(summary: &Value) -> BTreeMap<(String, String), (u64, u64)> {
    summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            let key = &group["key"];
            (
                (
                    key["session_harness"].as_str().unwrap_or("-").to_owned(),
                    key["session"].as_str().unwrap().to_owned(),
                ),
                (
                    group["sessions"].as_u64().unwrap(),
                    group["tokens"]["input"].as_u64().unwrap_or(0),
                ),
            )
        })
        .collect()
}

/// Claim: grouping by session keys each group by the session's harness and
/// id, as `session` and `session_harness`, in a summary and in a window read,
/// so a Claude session and a Codex session sharing an id are two groups of
/// one session each, under tapes-usage-summary/15. Rejects a group keyed by
/// the bare id that sums the two sessions' tokens as one.
#[test]
fn sessions_sharing_an_id_across_harnesses_are_separate_groups() {
    let store = Store::new("summary");
    claude_session(&store, SHARED);
    codex_session(&store, SHARED);
    let backends = store.backends();
    let expected = BTreeMap::from([
        (("claude".to_owned(), SHARED.to_owned()), (1, 1)),
        (("codex".to_owned(), SHARED.to_owned()), (1, 100)),
    ]);

    let summary = serde_json::to_value(
        usage_summary_with_backends(&backends, &selection(), &[GroupBy::Session]).unwrap(),
    )
    .unwrap();
    assert_eq!(session_groups(&summary), expected, "{summary:#}");
    assert_eq!(summary["schema"], "tapes-usage-summary/15", "{summary:#}");

    let windowed = serde_json::to_value(
        usage_window_with_backends(
            &backends,
            &selection(),
            &[GroupBy::Session],
            WindowAsk::Bounds(WindowBounds {
                from: Some(at("2026-01-10T09:00:00Z")),
                to: None,
            }),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(session_groups(&windowed), expected, "{windowed:#}");
}

/// Claim: an allowance-rate group by session names its session by harness
/// and id, under tapes-allowance-rates/6, so the lane a Codex session ran is
/// not read as the Claude session of the same id. Rejects a rate group whose
/// key is the bare id.
#[test]
fn allowance_rate_groups_name_a_session_by_harness_and_id() {
    let store = Store::new("rates");
    claude_session(&store, SHARED);
    codex_session(&store, SHARED);
    let rates = serde_json::to_value(
        allowance_rates_with_backends(
            &store.backends(),
            &selection(),
            &[GroupBy::Session],
            WindowBounds {
                from: Some(at("2026-01-10T09:30:00Z")),
                to: Some(at("2026-01-10T11:00:00Z")),
            },
            QuotaKind::Primary,
        )
        .unwrap(),
    )
    .unwrap();
    let keys = rates["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["key"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [json!({"session": SHARED, "session_harness": "codex"})],
        "{rates:#}"
    );
    assert_eq!(rates["schema"], "tapes-allowance-rates/6", "{rates:#}");
}
