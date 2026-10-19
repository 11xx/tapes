//! An OpenCode session names its launcher through the `tapes-parent` marker
//! opening its first prompt, through the API and the database alike, and a
//! Codex rollout whose first prompt lies past a truncated read's opening is
//! still titled by it.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{codex::CodexBackend, opencode::OpenCodeBackend, Backend, Query};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::{usage_summary_with_backends, SessionSelection};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode")
}

/// The fixture program under a name that selects its response set, with no
/// installed database, so the API or the fixture's own rows answer.
struct Alias(PathBuf, Option<PathBuf>);

impl Alias {
    fn new(name: &str, in_directory: bool) -> Self {
        std::env::set_var("XDG_DATA_HOME", support::empty_opencode_data_home());
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let (path, directory) = if in_directory {
            let directory = std::env::temp_dir().join(format!(
                "tapes-series-opencode-{}-{}-{nonce}",
                std::process::id(),
                TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&directory).unwrap();
            (directory.join(name), Some(directory))
        } else {
            (
                std::env::temp_dir().join(format!(
                    "{name}-{}-{}-{nonce}",
                    std::process::id(),
                    TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
                )),
                None,
            )
        };
        std::os::unix::fs::symlink(fixtures().join("opencode2"), &path).unwrap();
        Self(path, directory)
    }
}

impl Drop for Alias {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        if let Some(directory) = &self.1 {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

/// The parent each session of a `--by parent` summary groups under.
fn parent_groups(backend: Box<dyn Backend>) -> Vec<(String, u64)> {
    let summary = usage_summary_with_backends(
        &[backend],
        &SessionSelection {
            limit: Some(50),
            ..Default::default()
        },
        &[GroupBy::Parent],
    )
    .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    let mut groups = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["key"]["parent"].as_str().unwrap().to_owned(),
                group["sessions"].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    groups.sort();
    groups
}

/// Claim: an OpenCode session read through the API whose first message opens
/// with a `tapes-parent` marker groups under the session it names in a
/// `--by parent` summary. Rejects an OpenCode listing that never reads the
/// first prompt, which leaves the launched session its own group.
#[test]
fn an_opencode_api_session_groups_under_its_marker_parent() {
    let alias = Alias::new("opencode-launched", false);
    let groups = parent_groups(Box::new(OpenCodeBackend::new(&alias.0)));
    assert!(
        groups
            .iter()
            .any(|(parent, _)| parent == "30000000-0000-4000-8000-0000000000c1"),
        "{groups:?}"
    );
}

/// Claim: an OpenCode session read from the database whose first user
/// message's first text part opens with a marker groups under the session it
/// names. Rejects a database listing that never asks for the first prompt.
#[test]
fn an_opencode_database_session_groups_under_its_marker_parent() {
    let alias = Alias::new("opencode", true);
    let groups = parent_groups(Box::new(OpenCodeBackend::new(&alias.0)));
    assert!(
        groups
            .iter()
            .any(|(parent, _)| parent == "019f0000-0000-7000-8000-0000000000c2"),
        "{groups:?}"
    );
}

const CODEX_ID: &str = "01a40000-0000-7000-8000-000000000001";

/// Claim: a Codex rollout past the read window whose AGENTS.md instructions
/// push the first prompt past the 64 KiB opening lists with the prompt as
/// its derived title, read by the first-prompt probe within its 1 MiB bound.
/// Rejects a listing that titles from the opening alone and leaves the
/// session untitled.
#[test]
fn a_large_codex_rollout_is_titled_by_its_first_prompt() {
    let temporary = tempfile::TempDir::with_prefix("tapes-series-codex-").unwrap();
    let root = temporary.path();
    let path = root.join(format!(
        "2026/01/10/rollout-2026-01-10T10-00-00-{CODEX_ID}.jsonl"
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let message = |text: String| {
        json!({"timestamp": "2026-01-10T10:00:01Z", "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": text}]}})
    };
    let mut lines = vec![
        json!({"timestamp": "2026-01-10T10:00:00Z", "type": "session_meta",
               "payload": {"id": CODEX_ID, "session_id": CODEX_ID, "timestamp": "2026-01-10T10:00:00Z",
                           "cwd": "/fixtures/project", "source": "exec", "originator": "codex_exec"}}),
        message(format!(
            "# AGENTS.md instructions for /fixtures/project\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>",
            "r".repeat(100 * 1024)
        )),
        message("Survey the large fixture.".to_owned()),
    ];
    for index in 0..400 {
        lines.push(json!({"timestamp": "2026-01-10T10:01:00Z", "type": "response_item",
            "payload": {"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": format!("Reply {index} {}", "x".repeat(200))}]}}));
    }
    let body: Vec<String> = lines.iter().map(Value::to_string).collect();
    fs::write(&path, body.join("\n") + "\n").unwrap();
    let backend = CodexBackend::new(root).with_read_bytes(64 * 1024);
    let listing = backend.list(&Query::unscoped(5)).unwrap();
    let session = listing
        .sessions
        .iter()
        .find(|session| session.id == CODEX_ID)
        .unwrap();
    assert_eq!(
        session.derived_title.as_deref(),
        Some("Survey the large fixture.")
    );
}
