//! Window reads and allowance stay what they claim to be in the cases an
//! independent review named: a subagent spending inside a window counts
//! though its parent went quiet before it.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::WindowBounds;
use agent_tapes_core::{usage_window_with_backends, SessionSelection, WindowAsk};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

/// A store laid out as its harness lays it out, removed when dropped.
struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-review-window-{label}-{}-{nonce}",
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

fn window_read(backend: Box<dyn Backend>, harness: &'static str, from: &str, to: &str) -> Value {
    let selection = SessionSelection {
        harness: Some(harness),
        limit: Some(50),
        ..Default::default()
    };
    let bounds = WindowBounds {
        from: Some(at(from)),
        to: Some(at(to)),
    };
    serde_json::to_value(
        usage_window_with_backends(
            &[backend],
            &selection,
            &[GroupBy::Session],
            WindowAsk::Bounds(bounds),
            None,
        )
        .unwrap(),
    )
    .unwrap()
}

fn share<'a>(read: &'a Value, id: &str) -> Option<&'a Value> {
    read["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"] == id)
}

fn claude_record(id: &str, agent: Option<&str>, entry: &str, stamp: &str, output: u64) -> Value {
    let mut record = json!({
        "sessionId": id, "cwd": "/fixtures/project", "entrypoint": "cli", "version": "2.1.0",
        "type": "assistant", "uuid": entry, "parentUuid": null, "requestId": format!("req_{entry}"),
        "timestamp": stamp,
        "message": {"id": format!("msg_{entry}"), "role": "assistant", "model": "claude-fixture",
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                      "output_tokens": output},
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "Done."}]}
    });
    if let Some(agent) = agent {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(agent);
    }
    record
}

/// Claim: a subagent that spends inside a window contributes its share
/// though its parent's own last record precedes the window. Rejects a window
/// read that drops the parent by its own activity before it looks for the
/// parent's subagent recordings.
#[test]
fn a_subagent_spending_in_a_window_counts_after_its_parent_went_quiet() {
    let store = Store::new("quiet-parent");
    let id = "30000000-0000-4000-8000-0000000000f1";
    let agent = "a00000000000000f1";
    store.write(
        &format!("project/{id}.jsonl"),
        &[claude_record(
            id,
            None,
            "parent-1",
            "2026-01-12T09:59:00Z",
            50,
        )],
    );
    store.write(
        &format!("project/{id}/subagents/agent-{agent}.jsonl"),
        &[claude_record(
            id,
            Some(agent),
            "child-1",
            "2026-01-12T10:05:00Z",
            70,
        )],
    );
    let read = window_read(
        Box::new(ClaudeBackend::new(&store.0)),
        "claude",
        "2026-01-12T10:00:00Z",
        "2026-01-12T11:00:00Z",
    );
    let child = share(&read, &format!("{id}::{agent}"))
        .unwrap_or_else(|| panic!("the subagent's share: {}", read["window"]));
    assert_eq!(child["requests"], 1);
    assert_eq!(child["disjoint"]["output"], 70);
}
