//! A window read keeps a parent reached through its children only by
//! recorded activity since --since; allowance rates keep every lane either
//! estimator gives a percent; a forecast sizes a plan by lanes measured
//! under the same capacity; and request-term pricing takes billing from the
//! whole recording, qualifies untimed usage a row may not cover, notices
//! billing that changes only an upper bound, and leaves wholly unpriced
//! usage without an estimate.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::allowance::PlannedLanes;
use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend, Query};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_forecast_with_backends, allowance_rates_with_backends, usage_full_with_backends,
    usage_spans_with_backends, usage_window_with_backends, usage_with_backends, ListFilters,
    Selection, SessionSelection, WindowAsk,
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
            "tapes-review3-pricing-{label}-{}-{nonce}",
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

// --- A quiet parent reached through its subagent, under --since -----------

const PARENT: &str = "30000000-0000-4000-8000-0000000000f9";
const AGENT: &str = "a00000000000000f9";

fn subagent_record(agent: Option<&str>, entry: &str, stamp: &str) -> Value {
    let mut record = json!({
        "sessionId": PARENT, "cwd": "/fixtures/project", "entrypoint": "cli", "version": "2.1.0",
        "type": "assistant", "uuid": entry, "parentUuid": null, "requestId": format!("req_{entry}"),
        "timestamp": stamp,
        "message": {"id": format!("msg_{entry}"), "role": "assistant", "model": "claude-fixture",
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                      "output_tokens": 50},
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "Done."}]}
    });
    if let Some(agent) = agent {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(agent);
    }
    record
}

fn quiet_parent_shares(since: &str) -> Vec<String> {
    let store = Store::new("since");
    store.write(
        &format!("project/{PARENT}.jsonl"),
        &[subagent_record(None, "parent-1", "2026-01-12T09:59:00Z")],
    );
    // Written now: the file's modification time is long after its records.
    store.write(
        &format!("project/{PARENT}/subagents/agent-{AGENT}.jsonl"),
        &[subagent_record(
            Some(AGENT),
            "child-1",
            "2026-01-12T10:05:00Z",
        )],
    );
    let selection = SessionSelection {
        harness: Some("claude"),
        limit: Some(50),
        filters: ListFilters {
            since: Some(at(since)),
            ..ListFilters::default()
        },
        ..Default::default()
    };
    let read = serde_json::to_value(
        usage_window_with_backends(
            &[Box::new(ClaudeBackend::new(&store.0)) as Box<dyn Backend>],
            &selection,
            &[GroupBy::Session],
            WindowAsk::Bounds(WindowBounds {
                from: Some(at("2026-01-12T10:00:00Z")),
                to: Some(at("2026-01-12T11:00:00Z")),
            }),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    read["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|share| share["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Claim: under --since 10:30, a parent whose records end at 09:59 and whose
/// subagent's records end at 10:05 contributes nothing to a 10:00..11:00
/// window, though the subagent's file was written later; under --since
/// 10:00 the subagent's 10:05 share stands. Rejects applying --since to the
/// subagent file's modification time alone, which returns the 10:05
/// consumption although neither recording has activity since 10:30.
#[test]
fn a_parent_reached_through_children_needs_recorded_activity_since() {
    assert_eq!(
        quiet_parent_shares("2026-01-12T10:30:00Z"),
        Vec::<String>::new()
    );
    assert_eq!(
        quiet_parent_shares("2026-01-12T10:00:00Z"),
        vec![format!("{PARENT}::{AGENT}")]
    );
}
