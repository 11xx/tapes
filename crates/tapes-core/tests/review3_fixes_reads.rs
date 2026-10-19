//! Reads stay inside the bounds they claim in the cases an independent
//! review named: a discovery page charges the lineage metadata it retains.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::NativeStore;
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
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
