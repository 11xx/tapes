//! A session a tool launched names its launcher in a `tapes-parent` marker
//! opening its first prompt. The marker joins parent grouping and window
//! descent as a declared launch parent, never as a copy of the parent.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::WindowBounds;
use agent_tapes_core::{
    usage_summary_with_backends, usage_window_with_backends, usage_with_backends, Selection,
    SessionSelection, WindowAsk,
};
use serde_json::{json, Value};

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-launch-parent-{label}-{}-{nonce}",
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

    fn claude(&self, id: &str, minute: u32, prompt: &str) {
        self.write(
            &format!("claude/projects/-fixtures-project/{id}.jsonl"),
            &claude_session(id, minute, prompt),
        );
    }

    fn codex(&self, id: &str, minute: u32, prompt: &str, padding: usize) {
        self.write(
            &format!("codex/2026/01/10/rollout-2026-01-10T10-{minute:02}-00-{id}.jsonl"),
            &codex_session(id, minute, prompt, padding),
        );
    }

    fn backends(&self, codex_read_bytes: u64) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.0.join("claude/projects"))),
            Box::new(CodexBackend::new(self.0.join("codex")).with_read_bytes(codex_read_bytes)),
        ]
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const PARENT: &str = "30000000-0000-4000-8000-0000000000e1";
const CLAUDE_CHILD: &str = "30000000-0000-4000-8000-0000000000e2";
const UNRELATED: &str = "30000000-0000-4000-8000-0000000000e3";
const MALFORMED: &str = "30000000-0000-4000-8000-0000000000e4";
const CODEX_CHILD: &str = "01a10000-0000-7000-8000-0000000000e5";
const ORPHAN: &str = "01a10000-0000-7000-8000-0000000000e6";
const MISSING: &str = "01a10000-0000-7000-8000-0000000000ff";

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

/// A Codex exec rollout whose first user message is the harness's attached
/// instructions and environment, `padding` bytes of instructions long, and
/// whose second is `prompt`, then one request at `minute`.
fn codex_session(id: &str, minute: u32, prompt: &str, padding: usize) -> Vec<Value> {
    let stamp = format!("2026-01-10T10:{minute:02}:00Z");
    let total = json!({"input_tokens": 100, "cached_input_tokens": 0, "output_tokens": 7,
                       "reasoning_output_tokens": 0, "total_tokens": 107});
    vec![
        json!({"timestamp": stamp, "type": "session_meta",
               "payload": {"id": id, "session_id": id, "timestamp": stamp, "cwd": "/fixtures/project",
                           "source": "exec", "originator": "codex_exec"}}),
        json!({"timestamp": stamp, "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
        json!({"timestamp": stamp, "type": "response_item",
               "payload": {"type": "message", "role": "user", "content": [
                   {"type": "input_text", "text": format!(
                       "# AGENTS.md instructions for /fixtures/project\n\n<INSTRUCTIONS>\n{}\n</INSTRUCTIONS>",
                       "r".repeat(padding))},
                   {"type": "input_text", "text":
                       "<environment_context>\n  <cwd>/fixtures/project</cwd>\n</environment_context>"}]}}),
        json!({"timestamp": stamp, "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": prompt}]}}),
        json!({"timestamp": stamp, "type": "event_msg",
               "payload": {"type": "token_count",
                           "info": {"total_token_usage": total, "last_token_usage": total,
                                    "model_context_window": 272000}}}),
    ]
}

fn marker(harness: &str, id: &str) -> String {
    format!("tapes-parent: {harness}:{id}\nInspect the fixture.")
}

fn selection() -> SessionSelection<'static> {
    SessionSelection {
        limit: Some(50),
        ..Default::default()
    }
}

fn window() -> WindowAsk {
    WindowAsk::Bounds(WindowBounds {
        from: Some("2026-01-10T09:00:00Z".parse().unwrap()),
        to: None,
    })
}

fn groups(summary: &Value) -> BTreeMap<String, u64> {
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

fn share<'a>(summary: &'a Value, id: &str) -> &'a Value {
    summary["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"] == id)
        .unwrap_or_else(|| panic!("no share for {id}: {summary:#}"))
}

/// Claim: a Claude child and a Codex child whose first prompts open with a
/// `tapes-parent` marker naming a Claude session group under that session in
/// a window read by parent, each share naming it with
/// `declared-launch-parent` evidence; the Codex marker is read past the
/// harness's attached instructions even where they outgrow the bounded
/// opening; and a marker naming a session the store does not hold leaves
/// that parent named and unresolved. Rejects a launched run counted as its
/// own group, or a missing launcher dropped or guessed.
#[test]
fn launched_children_group_under_the_marker_parent_in_a_window() {
    let store = Store::new("window");
    store.claude(PARENT, 0, "Lead the fixture work.");
    store.claude(CLAUDE_CHILD, 1, &marker("claude", PARENT));
    store.claude(UNRELATED, 2, "Unrelated fixture work.");
    // Instructions past the opening's 64 KiB, in a recording past the read
    // window, as a real Codex exec rollout's are.
    store.codex(CODEX_CHILD, 3, &marker("claude", PARENT), 100 * 1024);
    store.codex(ORPHAN, 4, &marker("codex", MISSING), 0);
    let summary = usage_window_with_backends(
        &store.backends(64 * 1024),
        &selection(),
        &[GroupBy::Parent],
        window(),
        None,
    )
    .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    assert_eq!(
        groups(&summary),
        BTreeMap::from([
            (PARENT.to_owned(), 3),
            (UNRELATED.to_owned(), 1),
            (MISSING.to_owned(), 1),
        ]),
        "{summary:#}"
    );
    for child in [CLAUDE_CHILD, CODEX_CHILD] {
        let share = share(&summary, child);
        assert_eq!(share["parent"], PARENT, "{child}");
        assert_eq!(
            share["parent_evidence"], "declared-launch-parent",
            "{child}"
        );
        assert_eq!(share["root"], PARENT, "{child}");
        assert_eq!(share["root_resolved"], true, "{child}");
    }
    let orphan = share(&summary, ORPHAN);
    assert_eq!(orphan["parent"], MISSING);
    assert_eq!(orphan["root"], MISSING);
    assert_eq!(orphan["root_resolved"], false);
    assert!(share(&summary, PARENT).get("parent").is_none());
}

/// Claim: a first prompt opening with `tapes-parent:` in any other form names
/// no parent, so its session is its own group, and the session's usage says
/// the marker was malformed; a marker later in the prompt is prose. Rejects
/// a malformed marker read as a parent, guessed at, or dropped silently.
#[test]
fn a_malformed_marker_names_no_parent() {
    let store = Store::new("malformed");
    store.claude(PARENT, 0, "Lead the fixture work.");
    store.claude(
        MALFORMED,
        1,
        &format!("tapes-parent: claude {PARENT}\nInspect the fixture."),
    );
    store.claude(
        UNRELATED,
        2,
        &format!("Inspect the fixture.\ntapes-parent: claude:{PARENT}"),
    );
    let backends = store.backends(4 * 1024 * 1024);
    let summary =
        usage_window_with_backends(&backends, &selection(), &[GroupBy::Parent], window(), None)
            .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    assert_eq!(
        groups(&summary),
        BTreeMap::from([
            (PARENT.to_owned(), 1),
            (MALFORMED.to_owned(), 1),
            (UNRELATED.to_owned(), 1),
        ])
    );
    assert!(share(&summary, MALFORMED).get("parent").is_none());

    let usage =
        serde_json::to_value(usage_with_backends(&backends, Selection::Id(MALFORMED)).unwrap())
            .unwrap();
    let notes = usage["notes"].as_array().cloned().unwrap_or_default();
    assert!(
        notes.iter().any(|note| note
            .as_str()
            .unwrap()
            .contains("malformed tapes-parent marker")),
        "{notes:?}"
    );
    let prose =
        serde_json::to_value(usage_with_backends(&backends, Selection::Id(UNRELATED)).unwrap())
            .unwrap();
    assert!(prose.get("notes").is_none(), "{prose:#}");
}

/// Claim: a launched child and its launcher are added as disjoint sessions in
/// one parent group: the summary relates them as no overlap group, keeps no
/// per-session figures for them, and its totals are their sum. Rejects a
/// declared launch parent read as a fork origin, which would withhold the
/// group's exact totals as if the child copied its parent's history.
#[test]
fn a_launched_child_never_overlaps_its_launcher() {
    let store = Store::new("overlap");
    store.claude(PARENT, 0, "Lead the fixture work.");
    store.claude(CLAUDE_CHILD, 1, &marker("claude", PARENT));
    let summary = usage_summary_with_backends(
        &store.backends(4 * 1024 * 1024),
        &selection(),
        &[GroupBy::Parent],
    )
    .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    assert_eq!(groups(&summary), BTreeMap::from([(PARENT.to_owned(), 2)]));
    let group = &summary["groups"][0];
    for figure in [group, &summary["totals"]] {
        assert!(
            figure["overlap"]["related"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "{figure:#}"
        );
        assert!(figure.get("session_figures").is_none(), "{figure:#}");
        assert!(figure.get("identity_bounds").is_none(), "{figure:#}");
    }
    assert_eq!(group["tokens"]["output"], 10, "{group:#}");
}
