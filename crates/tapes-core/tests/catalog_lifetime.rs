//! A file-backed backend shares one walk of its store across a read context.
//! A caller-named lookup never reports a recording absent from a walk taken
//! before the call, while hits and joins that expect misses still answer
//! from the shared walk.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend, Query,
};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::SessionSelection;
use agent_tapes_core::{lineage_with_backends, usage_summary_with_backends, Selection};
use serde_json::{json, Value};

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-catalog-lifetime-{label}-{}-{nonce}",
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

fn claude_session(id: &str) -> Vec<Value> {
    vec![
        json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"),
               "timestamp": "2026-01-09T10:00:00Z", "cwd": "/fixtures/project",
               "message": {"role": "user", "content": "Catalog fixture."}}),
        json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"),
               "requestId": format!("{id}-request"), "timestamp": "2026-01-09T10:00:01Z",
               "cwd": "/fixtures/project",
               "message": {"role": "assistant", "model": "claude-fixture",
                           "content": [{"type": "text", "text": "Reply."}],
                           "usage": {"input_tokens": 1, "output_tokens": 2}}}),
    ]
}

fn claude_path(id: &str) -> String {
    format!("-fixtures-project/{id}.jsonl")
}

fn codex_session(id: &str, header: Value) -> Vec<Value> {
    let mut meta = json!({"id": id, "timestamp": "2026-01-09T10:00:00Z",
                          "cwd": "/fixtures/project", "source": "exec"});
    for (key, value) in header.as_object().unwrap() {
        meta[key] = value.clone();
    }
    let total = json!({"input_tokens": 100, "cached_input_tokens": 0,
                       "output_tokens": 5, "reasoning_output_tokens": 0, "total_tokens": 105});
    vec![
        json!({"timestamp": "2026-01-09T10:00:00Z", "type": "session_meta", "payload": meta}),
        json!({"timestamp": "2026-01-09T10:00:00Z", "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
        json!({"timestamp": "2026-01-09T10:00:01Z", "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "Catalog fixture."}]}}),
        json!({"timestamp": "2026-01-09T10:00:02Z", "type": "event_msg",
               "payload": {"type": "token_count",
                           "info": {"total_token_usage": total, "last_token_usage": total}}}),
    ]
}

fn codex_path(id: &str) -> String {
    format!("2026/01/09/rollout-2026-01-09T10-00-00-{id}.jsonl")
}

fn pi_session(id: &str, parent: Option<&str>) -> Vec<Value> {
    let mut header = json!({"type": "session", "version": 3, "id": id,
                            "timestamp": "2026-01-09T10:00:00.000Z", "cwd": "/fixtures/project"});
    if let Some(parent) = parent {
        header["parentSession"] = json!(parent);
    }
    vec![
        header,
        json!({"type": "message", "id": "user-1", "parentId": null,
               "timestamp": "2026-01-09T10:00:01.000Z",
               "message": {"role": "user", "content": [{"type": "text", "text": "Catalog fixture."}]}}),
    ]
}

fn pi_path(id: &str) -> String {
    format!("--fixtures-project--/2026-01-09T10-00-00-000Z_{id}.jsonl")
}

const CLAUDE_ID: &str = "30000000-0000-4000-8000-0000000000e1";
const CODEX_ID: &str = "40000000-0000-7000-8000-0000000000e2";
const PI_ID: &str = "50000000-0000-7000-8000-0000000000e3";

/// Claim: a backend held across a lookup that missed finds a recording
/// written afterwards under the harness's own filename convention, and the
/// walk that found it is the one the backend's next listing answers from.
/// Rejects a held backend that keeps reporting the id absent because its
/// first walk did not see the file.
#[test]
fn a_held_backend_finds_a_recording_written_after_a_miss() {
    let store = Store::new("late");
    let claude = ClaudeBackend::new(store.0.join("claude"));
    let codex = CodexBackend::new(store.0.join("codex"));
    let pi = PiBackend::new(store.0.join("pi"));
    store.write(
        &format!(
            "claude/{}",
            claude_path("30000000-0000-4000-8000-0000000000e0")
        ),
        &claude_session("30000000-0000-4000-8000-0000000000e0"),
    );
    store.write(
        &format!(
            "codex/{}",
            codex_path("40000000-0000-7000-8000-0000000000e0")
        ),
        &codex_session("40000000-0000-7000-8000-0000000000e0", json!({})),
    );
    store.write(
        &format!("pi/{}", pi_path("50000000-0000-7000-8000-0000000000e0")),
        &pi_session("50000000-0000-7000-8000-0000000000e0", None),
    );

    let held: [(&dyn Backend, &str); 3] = [(&claude, CLAUDE_ID), (&codex, CODEX_ID), (&pi, PI_ID)];
    for (backend, id) in held {
        assert!(
            backend.locate(id).unwrap().is_none(),
            "{id} is not written yet"
        );
    }
    store.write(
        &format!("claude/{}", claude_path(CLAUDE_ID)),
        &claude_session(CLAUDE_ID),
    );
    store.write(
        &format!("codex/{}", codex_path(CODEX_ID)),
        &codex_session(CODEX_ID, json!({})),
    );
    store.write(&format!("pi/{}", pi_path(PI_ID)), &pi_session(PI_ID, None));

    for (backend, id) in held {
        let found = backend.locate(id).unwrap();
        assert_eq!(
            found.map(|session| session.id),
            Some(id.to_owned()),
            "{} lookup reported a written recording absent",
            backend.harness()
        );
        let listed = backend.list(&Query::unscoped(20)).unwrap();
        assert!(
            listed.sessions.iter().any(|session| session.id == id),
            "{} listing missed the walk that found {id}",
            backend.harness()
        );
    }
    assert_eq!(
        [
            claude.catalog_walks(),
            codex.catalog_walks(),
            pi.catalog_walks()
        ],
        [2, 2, 2],
        "one walk for the miss, one for the lookup that found it, none for the listing"
    );
}

/// Claim: many exact lookups that hit through one backend make one walk of
/// its store. Rejects a lookup that walks again whenever it is asked.
#[test]
fn exact_hits_share_one_walk() {
    let store = Store::new("hits");
    let ids = [
        "30000000-0000-4000-8000-0000000000f1",
        "30000000-0000-4000-8000-0000000000f2",
        "30000000-0000-4000-8000-0000000000f3",
    ];
    for id in ids {
        store.write(&claude_path(id), &claude_session(id));
    }
    let claude = ClaudeBackend::new(&store.0);
    for _ in 0..5 {
        for id in ids {
            assert_eq!(claude.locate(id).unwrap().unwrap().id, id);
        }
    }
    assert_eq!(claude.catalog_walks(), 1);
}

/// Claim: grouping a listing by parent, over sessions whose named parents
/// the store does not hold, answers every missing parent from the listing's
/// walk. Rejects a parent join that walks the store again per missing
/// parent.
#[test]
fn a_parent_join_walks_once_whatever_the_missing_parents() {
    let store = Store::new("parents");
    for index in 0..6 {
        let id = format!("40000000-0000-7000-8000-00000000010{index}");
        let parent = format!("40000000-0000-7000-8000-00000000020{index}");
        store.write(
            &codex_path(&id),
            &codex_session(&id, json!({"parent_thread_id": parent})),
        );
    }
    let codex = CodexBackend::new(&store.0);
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(codex.clone())];
    let selection = SessionSelection {
        limit: Some(50),
        ..Default::default()
    };
    let summary = usage_summary_with_backends(&backends, &selection, &[GroupBy::Parent]).unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    assert_eq!(summary["groups"].as_array().unwrap().len(), 6, "{summary}");
    assert_eq!(codex.catalog_walks(), 1);
}

/// Claim: reading the lineage of a session whose parent recording is
/// missing resolves the session and the parent from one walk, for Pi and
/// for Codex. Rejects a lineage parent lookup that walks again on its miss.
#[test]
fn a_lineage_with_a_missing_parent_walks_once() {
    let store = Store::new("lineage");
    let missing_pi = "50000000-0000-7000-8000-0000000000ff";
    let pi_parent = store.0.join("pi").join(pi_path(missing_pi));
    store.write(
        &format!("pi/{}", pi_path(PI_ID)),
        &pi_session(PI_ID, Some(pi_parent.to_str().unwrap())),
    );
    store.write(
        &format!("codex/{}", codex_path(CODEX_ID)),
        &codex_session(
            CODEX_ID,
            json!({"parent_thread_id": "40000000-0000-7000-8000-0000000000ff"}),
        ),
    );
    let pi = PiBackend::new(store.0.join("pi"));
    let codex = CodexBackend::new(store.0.join("codex"));

    let pi_backends: Vec<Box<dyn Backend>> = vec![Box::new(pi.clone())];
    let view = lineage_with_backends(&pi_backends, Selection::Id(PI_ID)).unwrap();
    let view = serde_json::to_value(view).unwrap();
    assert_eq!(
        view["lineage"]["parent"]["resolved"],
        json!(false),
        "{view}"
    );
    assert_eq!(pi.catalog_walks(), 1);

    let codex_backends: Vec<Box<dyn Backend>> = vec![Box::new(codex.clone())];
    let view = lineage_with_backends(&codex_backends, Selection::Id(CODEX_ID)).unwrap();
    let view = serde_json::to_value(view).unwrap();
    assert_eq!(
        view["lineage"]["parent"]["resolved"],
        json!(false),
        "{view}"
    );
    assert_eq!(codex.catalog_walks(), 1);
}
