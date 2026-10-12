//! A child recording is found through the header it opens with. That header
//! is one JSONL record read from a bounded opening probe, so a record that
//! fills the probe exactly, with the file ending there, is whole.

use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::Backend;
use serde_json::json;

const PARENT: &str = "00000000-0000-0000-0000-000000000001";
const CHILD: &str = "00000000-0000-0000-0000-000000000002";

/// The opening probe's largest window.
const PROBE_BYTES: usize = 1024 * 1024;

struct Store(PathBuf);

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A store holding the parent fixture and a child whose recording is a single
/// `session_meta` record of `bytes` bytes, with a final newline when `newline`.
fn store(label: &str, bytes: usize, newline: bool) -> Store {
    let root =
        std::env::temp_dir().join(format!("tapes-child-header-{label}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(format!("rollout-2026-01-01T10-00-00-{PARENT}.jsonl")),
        root.join(format!("rollout-2026-01-01T10-00-00-{PARENT}.jsonl")),
    )
    .unwrap();
    let header = |padding: usize| {
        json!({"type": "session_meta", "payload": {"id": CHILD, "parent_thread_id": PARENT,
            "agent_path": "/root/worker", "padding": "p".repeat(padding)}})
        .to_string()
    };
    let bare = header(0).len();
    let mut record = header(bytes - usize::from(newline) - bare);
    if newline {
        record.push('\n');
    }
    assert_eq!(record.len(), bytes);
    fs::write(
        root.join(format!("rollout-2026-01-01T11-00-00-{CHILD}.jsonl")),
        record,
    )
    .unwrap();
    Store(root)
}

fn assert_child_found(label: &str, bytes: usize, newline: bool) {
    let store = store(label, bytes, newline);
    let backend = CodexBackend::new(&store.0);
    let parent = backend.locate(PARENT).unwrap().unwrap();
    for (read, lineage) in [
        ("bounded", backend.lineage(&parent)),
        ("streamed", backend.stream_lineage(&parent)),
    ] {
        let lineage = lineage.unwrap();
        assert_eq!(lineage.children.len(), 1, "{read}: {lineage:#?}");
        assert!(lineage.children[0].resolved, "{read}");
        assert_eq!(lineage.children[0].session_id.as_deref(), Some(CHILD));
    }
}

#[test]
fn a_header_one_byte_inside_the_probe_without_a_newline_keeps_its_child() {
    assert_child_found("inside", PROBE_BYTES - 1, false);
}

#[test]
fn a_header_exactly_the_probe_without_a_newline_keeps_its_child() {
    assert_child_found("exact", PROBE_BYTES, false);
}

#[test]
fn a_header_exactly_the_probe_with_a_newline_keeps_its_child() {
    assert_child_found("exact-newline", PROBE_BYTES, true);
}
