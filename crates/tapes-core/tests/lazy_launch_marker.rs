//! A `tapes-parent` launch marker is read only by an answer that uses a
//! session's parent: a summary grouped by parent reads it, and one grouped
//! otherwise asks for no first prompt.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{opencode::OpenCodeBackend, pi::PiBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::{usage_summary_with_backends, SessionSelection};
use serde_json::{json, Value};

/// The request an OpenCode API read makes for a session's first message.
const FIRST_MESSAGE: &str = "/message?limit=1&order=asc";
const OPENCODE_PARENT: &str = "30000000-0000-4000-8000-0000000000c1";

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{nanos}", TEMP_ROOTS.fetch_add(1, Ordering::Relaxed))
}

/// The OpenCode fixture program under a name that answers with a session
/// whose first message carries a marker and logs every request it serves,
/// with no installed database, so the API answers.
struct CountedOpenCode(PathBuf);

impl CountedOpenCode {
    fn new() -> Self {
        static XDG: OnceLock<PathBuf> = OnceLock::new();
        let root = XDG.get_or_init(|| {
            let root = std::env::temp_dir().join(format!("tapes-lazy-xdg-{}", std::process::id()));
            fs::create_dir_all(root.join("opencode")).unwrap();
            File::create(root.join("opencode/opencode.db")).unwrap();
            root
        });
        std::env::set_var("XDG_DATA_HOME", root);
        let path = std::env::temp_dir().join(format!(
            "opencode-launch-counted-{}-{}",
            std::process::id(),
            nonce()
        ));
        std::os::unix::fs::symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/opencode2"),
            &path,
        )
        .unwrap();
        Self(path)
    }

    fn calls(&self) -> PathBuf {
        PathBuf::from(format!("{}.calls", self.0.display()))
    }

    /// The first-message requests the program has served.
    fn first_message_requests(&self) -> usize {
        fs::read_to_string(self.calls())
            .unwrap_or_default()
            .lines()
            .filter(|line| line.ends_with(FIRST_MESSAGE))
            .count()
    }
}

impl Drop for CountedOpenCode {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.calls());
    }
}

fn summary(backend: Box<dyn Backend>, by: &[GroupBy]) -> Value {
    let summary = usage_summary_with_backends(
        &[backend],
        &SessionSelection {
            limit: Some(50),
            ..Default::default()
        },
        by,
    )
    .unwrap();
    serde_json::to_value(summary).unwrap()
}

fn group_keys(summary: &Value, dimension: &str) -> Vec<String> {
    let mut keys = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| group["key"][dimension].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

/// Claim: an OpenCode summary grouped by session or by model asks the API
/// for no session's first message, while the same summary grouped by parent
/// asks for it and groups the launched session under its marker's parent.
/// Rejects a marker read on every summary, a subprocess per listed session
/// whether or not the answer names a parent.
#[test]
fn only_a_summary_by_parent_asks_for_the_first_prompt() {
    let program = CountedOpenCode::new();
    for by in [vec![GroupBy::Session], vec![GroupBy::Model], vec![]] {
        summary(Box::new(OpenCodeBackend::new(&program.0)), &by);
        assert_eq!(program.first_message_requests(), 0, "--by {by:?}");
    }
    let grouped = summary(
        Box::new(OpenCodeBackend::new(&program.0)),
        &[GroupBy::Parent],
    );
    assert!(program.first_message_requests() > 0);
    assert!(
        group_keys(&grouped, "parent").contains(&OPENCODE_PARENT.to_owned()),
        "{grouped:#}"
    );
}

const PI_PARENT: &str = "019f0000-0000-7000-8000-0000000000d1";
const PI_CHILD: &str = "019f0000-0000-7000-8000-0000000000d2";

fn pi_session(root: &Path, id: &str, stamp: &str, prompt: &str) {
    let lines = [
        json!({"type": "session", "version": 3, "id": id, "timestamp": stamp, "cwd": "/fixtures/project"}),
        json!({"type": "model_change", "id": "model-1", "parentId": null, "timestamp": stamp,
               "provider": "openai-codex", "modelId": "gpt-fixture"}),
        json!({"type": "message", "id": "user-1", "parentId": "model-1", "timestamp": stamp,
               "message": {"role": "user", "timestamp": 1_767_261_603_000_u64,
                           "content": [{"type": "text", "text": prompt}]}}),
        json!({"type": "message", "id": "assistant-1", "parentId": "user-1", "timestamp": stamp,
               "message": {"role": "assistant", "timestamp": 1_767_261_604_000_u64,
                           "provider": "openai-codex", "model": "gpt-fixture",
                           "usage": {"input": 10, "output": 20, "cacheRead": 0, "cacheWrite": 0,
                                     "totalTokens": 30, "cost": {"total": 0.01}},
                           "content": [{"type": "text", "text": "Done."}]}}),
    ];
    let file = stamp.replace([':', '.'], "-");
    let path = root.join(format!("--fixtures-project--/{file}_{id}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body: Vec<String> = lines.iter().map(Value::to_string).collect();
    fs::write(path, body.join("\n") + "\n").unwrap();
}

/// Claim: a pi session whose first prompt opens with a marker groups under
/// the session it names in a summary by parent, read from the recording
/// file when the answer asks. Rejects a pi read that never asks the file
/// for its first prompt once the listing stops reading it.
#[test]
fn a_pi_summary_by_parent_groups_under_the_marker() {
    let root =
        std::env::temp_dir().join(format!("tapes-lazy-pi-{}-{}", std::process::id(), nonce()));
    pi_session(
        &root,
        PI_PARENT,
        "2026-01-01T10:00:00.000Z",
        "Lead the fixture work.",
    );
    pi_session(
        &root,
        PI_CHILD,
        "2026-01-01T10:05:00.000Z",
        &format!("tapes-parent: pi:{PI_PARENT}\nInspect the fixture."),
    );
    let grouped = summary(Box::new(PiBackend::new(&root)), &[GroupBy::Parent]);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        group_keys(&grouped, "parent"),
        vec![PI_PARENT.to_owned()],
        "{grouped:#}"
    );
    assert_eq!(grouped["groups"][0]["sessions"], 2, "{grouped:#}");
}
