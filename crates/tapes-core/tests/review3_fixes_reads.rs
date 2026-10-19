//! Reads stay inside the bounds and relationships they claim in the cases an
//! independent review named: a recording's final record is whole without a
//! trailing line feed, and a discovery page charges the lineage metadata it
//! retains.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::{usage_summary_with_backends, NativeStore, SessionSelection};
use serde_json::{json, Value};

/// A temporary store root, removed when dropped.
struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-review3-reads-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write_text(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn write(&self, relative: &str, lines: &[Value]) -> PathBuf {
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        self.write_text(relative, &(body.join("\n") + "\n"))
    }

    fn claude(&self, id: &str, minute: u32, prompt: &str) {
        self.write(
            &format!("claude/projects/-fixtures-project/{id}.jsonl"),
            &claude_session(id, minute, prompt),
        );
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A headless Claude session whose first user message is `prompt`, then one
/// request at `minute`.
fn claude_session(id: &str, minute: u32, prompt: &str) -> Vec<Value> {
    let stamp = |second: u32| format!("2026-01-10T10:{minute:02}:{second:02}Z");
    vec![
        json!({"type": "queue-operation", "operation": "enqueue", "sessionId": id,
               "timestamp": stamp(0)}),
        json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"), "entrypoint": "sdk-cli",
               "timestamp": stamp(1), "cwd": "/fixtures/project",
               "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}}),
        json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"), "entrypoint": "sdk-cli",
               "requestId": format!("{id}-request"), "timestamp": stamp(30), "cwd": "/fixtures/project",
               "message": {"role": "assistant", "model": "claude-fixture",
                           "content": [{"type": "text", "text": "ok"}],
                           "usage": {"input_tokens": 1, "output_tokens": 5, "cache_read_input_tokens": 10,
                                     "cache_creation_input_tokens": 2}}}),
    ]
}

/// Where a Codex rollout of `id` starting at `stamp` lives under its root.
fn codex_path(id: &str, stamp: &str) -> String {
    let date = &stamp[..10];
    let time = stamp[11..19].replace(':', "-");
    format!(
        "{}/rollout-{date}T{time}-{id}.jsonl",
        date.replace('-', "/")
    )
}

fn marker(harness: &str, id: &str) -> String {
    format!("tapes-parent: {harness}:{id}\nInspect the fixture.")
}

fn parent_groups(summary: &Value) -> Vec<(String, u64)> {
    summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["key"]["parent"].as_str().unwrap().to_owned(),
                group["sessions"].as_u64().unwrap(),
            )
        })
        .collect()
}

const LAUNCHER: &str = "30000000-0000-4000-8000-0000000000b1";
const UNTERMINATED: &str = "30000000-0000-4000-8000-0000000000b2";

/// Claim: a recording whose final record is its marked first prompt,
/// complete but without a trailing line feed, names its launcher, as the
/// recording's opening reads that record. Rejects a first-prompt read that
/// drops the final record, which groups the child under itself.
#[test]
fn a_final_first_prompt_without_a_line_feed_names_its_launcher() {
    let store = Store::new("unterminated");
    store.claude(LAUNCHER, 1, "Lead the fixture work.");
    let records = claude_session(UNTERMINATED, 2, &marker("claude", LAUNCHER));
    let body: Vec<String> = records[..2].iter().map(Value::to_string).collect();
    store.write_text(
        &format!("claude/projects/-fixtures-project/{UNTERMINATED}.jsonl"),
        &body.join("\n"),
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(
        store.0.join("claude/projects"),
    ))];
    let selection = SessionSelection {
        limit: Some(50),
        ..Default::default()
    };
    let summary = serde_json::to_value(
        usage_summary_with_backends(&backends, &selection, &[GroupBy::Parent]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parent_groups(&summary),
        vec![(LAUNCHER.to_owned(), 2)],
        "{summary:#}"
    );
}

const LINEAGE_PARENT: &str = "01a10000-0000-7000-8000-0000000000d1";
const LINEAGE_HEADERS: usize = 64;
const NICKNAME_BYTES: usize = 512 * 1024;

/// Claim: a discovery page charges the Codex lineage metadata it retains
/// against its 16 MiB bound, so 64 child headers each naming a 512 KiB
/// nickname end the page incomplete at that bound with at most 16 MiB of
/// metadata held. Rejects a page that keeps over 32 MiB of metadata and
/// reports itself complete.
#[test]
fn a_discovery_page_charges_the_lineage_metadata_it_retains() {
    let store = Store::new("lineage-budget");
    let nickname = "n".repeat(NICKNAME_BYTES);
    for index in 0..LINEAGE_HEADERS {
        let id = format!("01a10000-0000-7000-8000-{index:012x}");
        let stamp = format!("2026-01-10T10:{:02}:{:02}Z", index / 60, index % 60);
        let header = json!({"timestamp": stamp, "type": "session_meta",
            "payload": {"id": id, "timestamp": stamp, "cwd": "/fixtures/project",
                        "parent_thread_id": LINEAGE_PARENT, "agent_path": "/root/worker",
                        "agent_nickname": nickname}});
        store.write(&codex_path(&id, &stamp), &[header]);
    }
    let page = NativeStore::codex(&store.0).candidates(LINEAGE_HEADERS * 2);
    let retained: usize = page
        .records
        .iter()
        .filter_map(|record| record.metadata())
        .map(|metadata| metadata.to_string().len())
        .sum();
    assert!(
        retained <= 16 * 1024 * 1024,
        "{} records retain {retained} bytes of metadata",
        page.records.len()
    );
    assert!(!page.complete, "a page cut at its bound is incomplete");
    assert!(
        page.failures
            .iter()
            .any(|failure| failure.to_string().contains("16 MiB retained data")),
        "{:?}",
        page.failures
    );
}
