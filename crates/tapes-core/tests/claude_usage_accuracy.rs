//! A Claude usage figure says what its records support:
//! - a whole read that could not decode a record qualifies every counter;
//! - a request sum keeps each model's share, so a session served by two
//!   models is priced share by share;
//! - a family's recorded total is ordered against its children by its place
//!   in the parent's recording, since a cost-state writes no timestamp;
//! - a mirror row a plugin writes for work done elsewhere carries no
//!   requests of its own.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, Backend};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::usage::{UsageOptions, UsageView};
use agent_tapes_core::{
    child, usage_full_with_backends, usage_with_backends, usage_with_options_with_backends,
    Selection,
};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

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
            "tapes-claude-accuracy-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
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

fn reasons(view: &UsageView, counter: &str) -> Vec<String> {
    serde_json::to_value(&view.measurement_completeness).unwrap()[counter]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .map(|reason| reason.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Claim: a read of a Claude recording that skips an undecodable line,
/// whole or bounded, qualifies every counter `unreadable-record`, as pi and
/// OpenCode do, since the line may have been a request the sum lacks.
/// Rejects counters reported complete beside a skipped record.
#[test]
fn a_whole_read_qualifies_every_counter_with_an_unreadable_record() {
    let backend =
        || -> Box<dyn Backend> { Box::new(ClaudeBackend::new(fixtures("claude-resumed"))) };
    let id = "30000000-0000-4000-8000-000000000002";
    let bounded = usage_with_backends(&[backend()], Selection::Id(id)).unwrap();
    for counter in ["input", "output", "reasoning", "cache_read", "cache_write"] {
        assert!(
            reasons(&bounded, counter).contains(&"unreadable-record".to_owned()),
            "bounded {counter}: {}",
            serde_json::to_value(&bounded.measurement_completeness).unwrap()
        );
    }
    let view = usage_full_with_backends(&[backend()], Selection::Id(id)).unwrap();
    assert!(view
        .read
        .as_ref()
        .is_some_and(|read| read.source_length > 0));
    for counter in ["input", "output", "reasoning", "cache_read", "cache_write"] {
        assert!(
            reasons(&view, counter).contains(&"unreadable-record".to_owned()),
            "{counter}: {}",
            serde_json::to_value(&view.measurement_completeness).unwrap()
        );
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

/// Claim: a Claude request sum served by two models keeps each model's
/// share, by the model its requests name, and its list-price estimate is the
/// shares priced by their own rows: model A's 100 input and 1000 output at 1
/// and 10 per million, and model B's 200 and 2000 at 2 and 4, come to
/// 0.0185. Rejects a sum with no split, which has no estimate at all.
#[test]
fn a_two_model_request_sum_is_priced_share_by_share() {
    let store = Store::new("two-models");
    let id = "30000000-0000-4000-8000-0000000000c1";
    store.write(
        &format!("project/{id}.jsonl"),
        &[
            json!({"sessionId": id, "cwd": "/fixtures/project", "entrypoint": "sdk-cli", "type": "user",
                   "uuid": "user-1", "parentUuid": null, "timestamp": "2026-01-13T10:00:00Z",
                   "message": {"role": "user", "content": "Work."}}),
            record(id, "assistant-1", "2026-01-13T10:00:01Z", Some("req_a_1"), "claude-model-a", usage(100, 1000)),
            record(id, "assistant-2", "2026-01-13T10:00:02Z", Some("req_b_1"), "claude-model-b", usage(200, 2000)),
        ],
    );
    let mut view = usage_full_with_backends(&[store.backend()], Selection::Id(id)).unwrap();
    let shares = view
        .by_model
        .as_ref()
        .expect("a split by model")
        .iter()
        .map(|share| {
            let tokens = share.tokens.as_ref().unwrap();
            (
                share.model.as_str(),
                tokens.input,
                tokens.output,
                share.request_count,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shares,
        [
            ("claude-model-a", Some(100), Some(1000), Some(1)),
            ("claude-model-b", Some(200), Some(2000), Some(1)),
        ]
    );
    let table = PriceTable::parse(
        "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n\
         [[models]]\nid = \"claude-model-a\"\neffective = 2026-01-01\nsource = \"fixture\"\n\
         per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.25, cache_write_1h = 2.0, output = 10.0 }\n\
         [[models]]\nid = \"claude-model-b\"\neffective = 2026-01-01\nsource = \"fixture\"\n\
         per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.5, cache_write_1h = 4.0, output = 4.0 }\n",
    )
    .unwrap();
    pricing::price_view(&mut view, &table);
    let estimate =
        serde_json::to_value(view.list_price_estimate.as_ref().expect("an estimate")).unwrap();
    let amount = estimate["amount"].as_f64().unwrap();
    let expected = (100.0 * 1.0 + 1000.0 * 10.0 + 200.0 * 2.0 + 2000.0 * 4.0) / 1e6;
    assert!((amount - expected).abs() < 1e-12, "{estimate}");
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

fn mirror_row(id: &str, agent: &str, entry: &str, stamp: &str) -> Value {
    json!({
        "parentUuid": null, "isSidechain": true, "agentId": agent, "type": "assistant", "uuid": entry,
        "timestamp": stamp, "userType": "external", "entrypoint": "cli", "cwd": "/fixtures/project",
        "sessionId": id, "version": "2.1.302",
        "message": {"id": format!("msg_hook_{entry}"), "model": "claude-haiku-5-5", "role": "assistant",
            "type": "message", "stop_reason": "tool_use", "stop_sequence": null,
            "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0,
                      "cache_read_input_tokens": 0,
                      "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                      "service_tier": null,
                      "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 0},
                      "inference_geo": null, "speed": null},
            "content": [{"type": "text", "text": "Delegated run progress."}]}
    })
}

/// Claim: a subagent recording made only of mirror rows (no request id,
/// every usage counter zero, under a `delegated-agents:run` `.meta.json`)
/// reads as no requests and no measured tokens, with a note saying it holds
/// no consumption of its own, and the parent's family read counts none of
/// it. Rejects reading each mirror row as a measured request of zero
/// tokens.
#[test]
fn a_mirror_row_recording_holds_no_requests_of_its_own() {
    let store = Store::new("mirror");
    let id = "30000000-0000-4000-8000-0000000000c3";
    let agent = "a00000000000000c3";
    store.write(
        &format!("project/{id}.jsonl"),
        &[record(
            id,
            "parent-1",
            "2026-01-13T10:00:01Z",
            Some("req_parent_c3"),
            "claude-fixture",
            usage(10, 100),
        )],
    );
    store.write(
        &format!("project/{id}/subagents/agent-{agent}.jsonl"),
        &[
            json!({"parentUuid": null, "isSidechain": true, "agentId": agent, "type": "user",
                   "uuid": "mirror-user-1", "timestamp": "2026-01-13T10:00:02Z", "cwd": "/fixtures/project",
                   "sessionId": id, "version": "2.1.302", "entrypoint": "cli",
                   "message": {"role": "user", "content": "Run the delegated work."}}),
            mirror_row(id, agent, "mirror-1", "2026-01-13T10:00:03Z"),
            mirror_row(id, agent, "mirror-2", "2026-01-13T10:00:04Z"),
        ],
    );
    fs::write(
        store
            .0
            .join(format!("project/{id}/subagents/agent-{agent}.meta.json")),
        json!({"agentType": "delegated-agents:run", "pluginSteered": true,
               "description": "delegated fixture run", "toolUseId": "toolu_plugin_fixture",
               "spawnDepth": 1, "requestShape": "background", "requestNonInteractive": true})
        .to_string(),
    )
    .unwrap();

    let view =
        child::read_full_with_backends(&[store.backend()], Selection::Id(id), agent, 20).unwrap();
    let usage = serde_json::to_value(&view.usage).unwrap();
    assert!(
        usage["tokens"].is_null(),
        "no measured zeros: {}",
        usage["tokens"]
    );
    assert!(
        usage["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("no consumption of its own")),
        "{}",
        usage["notes"]
    );
    let family = reconcile(store.backend(), id);
    let child_source = family["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["agent_id"] == agent)
        .unwrap();
    assert_eq!(child_source["requests"], 0, "{child_source}");
    assert_eq!(family["identified"]["requests"], 1);
    assert_eq!(family["identified"]["tokens"]["output"], 100);
}
