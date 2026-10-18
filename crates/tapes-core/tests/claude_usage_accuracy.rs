//! A Claude usage figure says what its records support:
//! - a family's recorded total is ordered against its children by its place
//!   in the parent's recording, since a cost-state writes no timestamp.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, Backend};
use agent_tapes_core::usage::UsageOptions;
use agent_tapes_core::{usage_with_options_with_backends, Selection};
use serde_json::{json, Value};

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A Claude store laid out as Claude lays it out, removed when dropped.
struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-claude-accuracy-{label}-{}-{nonce}",
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

    fn backend(&self) -> Box<dyn Backend> {
        Box::new(ClaudeBackend::new(&self.0))
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn record(
    id: &str,
    entry: &str,
    stamp: &str,
    request: Option<&str>,
    model: &str,
    usage: Value,
) -> Value {
    let mut record = json!({
        "sessionId": id, "cwd": "/fixtures/project", "entrypoint": "sdk-cli", "version": "2.1.302",
        "type": "assistant", "uuid": entry, "parentUuid": null, "timestamp": stamp,
        "message": {"id": format!("msg_{entry}"), "role": "assistant", "model": model, "usage": usage,
                    "stop_reason": "end_turn", "content": [{"type": "text", "text": "Done."}]}
    });
    if let Some(request) = request {
        record["requestId"] = json!(request);
    }
    record
}

fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens": input, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
           "output_tokens": output, "service_tier": "standard",
           "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 0}})
}

fn counter_rows(reconciliation: &Value) -> Vec<(String, String, Vec<String>)> {
    reconciliation["counters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["counter"].as_str().unwrap().to_owned(),
                row["status"].as_str().unwrap().to_owned(),
                row["reasons"]
                    .as_array()
                    .map(|reasons| {
                        reasons
                            .iter()
                            .map(|reason| reason.as_str().unwrap().to_owned())
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .collect()
}

fn reconcile(backend: Box<dyn Backend>, id: &str) -> Value {
    let view = usage_with_options_with_backends(
        &[backend],
        Selection::Id(id),
        UsageOptions {
            full: true,
            series: None,
            reconcile: true,
            event_keys: None,
        },
    )
    .unwrap();
    serde_json::to_value(view.reconciliation.as_ref().unwrap()).unwrap()
}

/// Claim: a family whose cost-state writes no timestamp, as every real one
/// is written, is ordered against its children's requests by its place
/// after the parent's timestamped records, so its recorded total is
/// compared with its requests and reconciles. Where no timestamped parent
/// record precedes the total, the counters are withheld on the frontier and
/// a note says the total cannot be ordered. Rejects withholding every
/// counter of a real-shaped family for want of a timestamp the recording
/// never writes.
#[test]
fn a_family_orders_its_untimed_total_by_its_place_in_the_parent() {
    let family = reconcile(
        Box::new(ClaudeBackend::new(fixtures("claude-family"))),
        "40000000-0000-4000-8000-000000000001",
    );
    assert!(family["recorded"]
        .get("observed_at")
        .is_none_or(Value::is_null));
    for (counter, status, reasons) in counter_rows(&family) {
        assert_eq!(status, "reconciled", "{counter}: {reasons:?}");
    }

    let store = Store::new("untimed-total");
    let id = "40000000-0000-4000-8000-0000000000c2";
    let agent = "a00000000000000c2";
    let fixture =
        fixtures("claude-family").join("project/40000000-0000-4000-8000-000000000001.jsonl");
    let cost_state = fs::read_to_string(fixture)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|value| value["type"] == "cost-state")
        .map(|mut value| {
            value["sessionId"] = json!(id);
            value
        })
        .unwrap();
    store.write(&format!("project/{id}.jsonl"), &[cost_state]);
    let mut child = record(
        id,
        "child-1",
        "2026-01-13T10:00:05Z",
        Some("req_child_c2"),
        "claude-fixture-small",
        usage(3, 60),
    );
    child["isSidechain"] = json!(true);
    child["agentId"] = json!(agent);
    store.write(
        &format!("project/{id}/subagents/agent-{agent}.jsonl"),
        &[child],
    );
    let untimed = reconcile(store.backend(), id);
    for (counter, status, reasons) in counter_rows(&untimed) {
        assert_eq!(status, "withheld", "{counter}");
        assert!(
            reasons.contains(&"frontier".to_owned()),
            "{counter}: {reasons:?}"
        );
    }
    assert!(
        untimed["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("writes no timestamp")),
        "{}",
        untimed["notes"]
    );
}
