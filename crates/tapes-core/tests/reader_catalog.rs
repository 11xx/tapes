//! Reader work stays proportional to what a command answers: listing builds
//! no turns beyond those a derived title needs, and usage reads a recording
//! no more often than its figures require.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend, Query,
};
use agent_tapes_core::counters;
use serde_json::{json, Value};

/// The counters are process-wide, so the tests that read them run one at a
/// time.
static COUNTED: Mutex<()> = Mutex::new(());

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-reader-{label}-{}-{nonce}",
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

const REPLIES: usize = 40;

fn claude_session(id: &str, title: Option<&str>) -> Vec<Value> {
    let mut lines = vec![json!({
        "type": "user", "sessionId": id, "uuid": format!("{id}-user"),
        "timestamp": "2026-01-09T10:00:00Z", "cwd": "/fixtures/project",
        "message": {"role": "user", "content": "Inspect the listing fixture."}
    })];
    if let Some(title) = title {
        lines.push(json!({"type": "ai-title", "sessionId": id, "aiTitle": title}));
    }
    for index in 0..REPLIES {
        lines.push(json!({
            "type": "assistant", "sessionId": id, "uuid": format!("{id}-reply-{index}"),
            "requestId": format!("{id}-request-{index}"),
            "timestamp": format!("2026-01-09T10:00:{:02}Z", index + 1), "cwd": "/fixtures/project",
            "message": {"role": "assistant", "model": "claude-fixture",
                        "content": [{"type": "text", "text": "Reply."}],
                        "usage": {"input_tokens": 1, "output_tokens": 2}}
        }));
    }
    lines
}

fn codex_session(id: &str) -> Vec<Value> {
    let mut lines = vec![
        json!({"timestamp": "2026-01-09T10:00:00Z", "type": "session_meta",
               "payload": {"id": id, "timestamp": "2026-01-09T10:00:00Z", "cwd": "/fixtures/project"}}),
        json!({"timestamp": "2026-01-09T10:00:01Z", "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "Inspect the listing fixture."}]}}),
    ];
    for index in 0..REPLIES {
        lines.push(json!({"timestamp": "2026-01-09T10:00:02Z", "type": "response_item",
                          "payload": {"type": "message", "role": "assistant",
                                      "content": [{"type": "output_text", "text": format!("Reply {index}.")}]}}));
    }
    lines
}

fn pi_session(id: &str) -> Vec<Value> {
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-01-09T10:00:00Z", "cwd": "/fixtures/project"}),
        json!({"type": "message", "id": "user-1", "parentId": null, "timestamp": "2026-01-09T10:00:01Z",
               "message": {"role": "user", "content": [{"type": "text", "text": "Inspect the listing fixture."}]}}),
    ];
    let mut parent = "user-1".to_owned();
    for index in 0..REPLIES {
        let id = format!("reply-{index}");
        lines.push(json!({"type": "message", "id": id, "parentId": parent, "timestamp": "2026-01-09T10:00:02Z",
                          "message": {"role": "assistant", "content": [{"type": "text", "text": "Reply."}]}}));
        parent = id;
    }
    lines
}

/// Claim: listing a store builds only the turns a derived title needs, the
/// first user message of each untitled session and none for a titled one,
/// while a transcript read of the same session still builds every turn and
/// the listing's derived titles are unchanged. Rejects a listing that builds
/// each recording's turns and discards them.
#[test]
fn listing_builds_only_the_turns_a_derived_title_needs() {
    let _counted = COUNTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let store = Store::new("listing");
    store.write(
        "claude/projects/-fixtures-project/30000000-0000-4000-8000-0000000000b1.jsonl",
        &claude_session(
            "30000000-0000-4000-8000-0000000000b1",
            Some("Recorded title"),
        ),
    );
    store.write(
        "claude/projects/-fixtures-project/30000000-0000-4000-8000-0000000000b2.jsonl",
        &claude_session("30000000-0000-4000-8000-0000000000b2", None),
    );
    let codex_id = "40000000-0000-7000-8000-0000000000b3";
    store.write(
        &format!("codex/2026/01/09/rollout-2026-01-09T10-00-00-{codex_id}.jsonl"),
        &codex_session(codex_id),
    );
    let pi_id = "50000000-0000-7000-8000-0000000000b4";
    store.write(
        &format!("pi/--fixtures-project--/2026-01-09T10-00-00-000Z_{pi_id}.jsonl"),
        &pi_session(pi_id),
    );
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(ClaudeBackend::new(store.0.join("claude/projects"))),
        Box::new(CodexBackend::new(store.0.join("codex"))),
        Box::new(PiBackend::new(store.0.join("pi"))),
    ];

    let before = counters::turns_built();
    let mut titles = Vec::new();
    for backend in &backends {
        let listing = backend.list(&Query::unscoped(20)).unwrap();
        for session in listing.sessions {
            titles.push((session.id.clone(), session.title.or(session.derived_title)));
        }
    }
    let listed = counters::turns_built() - before;
    titles.sort();
    assert_eq!(titles.len(), 4);
    for (id, title) in &titles {
        let expected = if id.ends_with("b1") {
            "Recorded title"
        } else {
            "Inspect the listing fixture."
        };
        assert_eq!(title.as_deref(), Some(expected), "{id}");
    }
    assert_eq!(
        listed, 3,
        "one user turn for each of the three untitled sessions"
    );

    let before = counters::turns_built();
    let backend = &backends[0];
    let session = backend
        .locate("30000000-0000-4000-8000-0000000000b2")
        .unwrap()
        .unwrap();
    assert_eq!(
        counters::turns_built() - before,
        1,
        "a lookup builds only its title's turn"
    );
    let transcript = backend.transcript(&session, usize::MAX).unwrap();
    assert_eq!(transcript.turns.len(), 1 + REPLIES);
}
