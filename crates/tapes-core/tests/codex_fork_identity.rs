//! A forked Codex rollout copies its origin's history, `session_meta`
//! included. The first `session_meta` is the rollout's identity; the copy
//! claims nothing, on either the bounded read or the whole streamed one.

use std::fs;
use std::path::Path;

use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::{Backend, Query, MIN_READ_BYTES};
use agent_tapes_core::model::{Launch, OperatorOrigin, Session};
use serde_json::{json, Value};

const OWN: &str = "71000000-0000-7000-8000-000000000001";
const ORIGIN: &str = "71000000-0000-7000-8000-000000000002";
const STRANGER: &str = "71000000-0000-7000-8000-000000000003";

fn own_meta() -> Value {
    json!({"timestamp": "2026-03-04T05:06:07Z", "type": "session_meta", "payload": {
        "id": OWN, "session_id": OWN, "timestamp": "2026-03-04T05:06:07Z",
        "forked_from_id": ORIGIN, "parent_thread_id": ORIGIN,
        "source": {"subagent": {"thread_spawn": {"parent_thread_id": ORIGIN, "depth": 1}}},
        "thread_source": "subagent", "agent_path": "/root/worker",
        "cwd": "/fixtures/fork", "originator": "fixture"}})
}

fn meta_named(id: &str) -> Value {
    json!({"timestamp": "2026-03-04T05:06:07Z", "type": "session_meta", "payload": {
        "id": id, "session_id": id, "timestamp": "2026-03-04T01:00:00Z", "source": "vscode",
        "cwd": "/fixtures/origin", "originator": "fixture",
        "git": {"branch": "origin-branch"}}})
}

fn exchange(text: &str) -> [Value; 2] {
    let at = "2026-03-04T05:06:08Z";
    [
        json!({"timestamp": at, "type": "response_item", "payload": {"type": "message",
            "role": "user", "content": [{"type": "input_text", "text": text}]}}),
        json!({"timestamp": at, "type": "response_item", "payload": {"type": "message",
            "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}}),
    ]
}

/// A store holding one rollout under the harness's dated directory and name.
fn store(records: &[Value]) -> (tempfile::TempDir, CodexBackend) {
    let root = tempfile::tempdir().unwrap();
    let day = root.path().join("2026/03/04");
    fs::create_dir_all(&day).unwrap();
    let body: String = records.iter().map(|record| format!("{record}\n")).collect();
    fs::write(
        day.join(format!("rollout-2026-03-04T05-06-07-{OWN}.jsonl")),
        body,
    )
    .unwrap();
    let backend = CodexBackend::new(root.path());
    (root, backend)
}

fn forked_records() -> Vec<Value> {
    let mut records = vec![own_meta(), meta_named(ORIGIN)];
    records.extend(exchange("forked work"));
    records
}

fn located(backend: &CodexBackend) -> Session {
    backend.locate(OWN).unwrap().expect("the fork resolves")
}

fn assert_own_header(session: &Session) {
    assert_eq!(session.id, OWN);
    assert_eq!(session.launch, Some(Launch::Headless));
    assert_eq!(
        session.directory.as_deref(),
        Some(Path::new("/fixtures/fork"))
    );
}

/// Claim: the bounded read reports the rollout's own id, launch and directory.
/// Rejects the origin's record refusing the read or overwriting them.
#[test]
fn the_bounded_read_keeps_the_forks_own_header() {
    let (_root, backend) = store(&forked_records());
    let session = located(&backend);
    assert_own_header(&session);
    let transcript = backend.transcript(&session, 10).unwrap();
    assert_own_header(&transcript.session);
    assert!(!transcript.turns.is_empty());
}

/// Claim: a rollout larger than the read bound, read as an opening and a tail,
/// keeps the same header. Rejects judging the two windows apart.
#[test]
fn a_truncated_read_keeps_the_forks_own_header() {
    let mut records = forked_records();
    let filler = "x".repeat(4096);
    records
        .extend((0..(2 * MIN_READ_BYTES as usize / filler.len())).flat_map(|_| exchange(&filler)));
    let (_root, backend) = store(&records);
    let session = located(&backend);
    assert_own_header(&session);
    assert_own_header(&backend.transcript(&session, 10).unwrap().session);
}

/// Claim: the streamed whole read keeps the rollout's own launch on its turns
/// and its own place. Rejects the last `session_meta` winning, which reads a
/// headless subagent's prompt as an operator's and adds the origin's directory.
#[test]
fn the_streamed_read_keeps_the_forks_own_launch_and_place() {
    let (_root, backend) = store(&forked_records());
    let session = located(&backend);
    let mut origins = Vec::new();
    let read = backend
        .stream_transcript(&session, None, &mut |turn| {
            origins.extend(turn.origin);
            Ok(())
        })
        .unwrap();
    assert_eq!(origins, [OperatorOrigin::Program]);
    let transcript = read.transcript(session, Vec::new(), None);
    assert_own_header(&transcript.session);
    let places = serde_json::to_string(&transcript.locations).unwrap();
    assert!(places.contains("/fixtures/fork"), "{places}");
    assert!(!places.contains("/fixtures/origin"), "{places}");
    assert!(!places.contains("origin-branch"), "{places}");
}

/// Claim: a fact the rollout's own header omits stays absent. Rejects the
/// origin's directory filling the gap.
#[test]
fn the_origins_directory_never_fills_a_gap_in_the_forks_header() {
    let mut own = own_meta();
    own["payload"].as_object_mut().unwrap().remove("cwd");
    let (_root, backend) = store(&[own, meta_named(ORIGIN)]);
    assert_eq!(located(&backend).directory, None);
    let listed = backend.list(&Query::unscoped(10)).unwrap().sessions;
    assert_eq!(listed[0].directory, None);
}

/// Claim: both lineage reads still name the rollout's own parent and origin.
/// Rejects the copied record, which names neither, erasing them.
#[test]
fn lineage_reports_the_forks_own_parent() {
    let (_root, backend) = store(&forked_records());
    let session = located(&backend);
    for lineage in [
        backend.lineage(&session).unwrap(),
        backend.stream_lineage(&session).unwrap(),
    ] {
        assert_eq!(lineage.parent.unwrap().native_id, ORIGIN);
        assert_eq!(lineage.forked_from.as_deref(), Some(ORIGIN));
    }
}

/// Claim: a `session_meta` that is no origin of the rollout fails the bounded
/// and streamed reads. Rejects skipping every later `session_meta`.
#[test]
fn a_session_meta_that_is_no_origin_is_still_refused() {
    let (root, backend) = store(&forked_records());
    let session = located(&backend);
    let rollout = root.path().join(format!(
        "2026/03/04/rollout-2026-03-04T05-06-07-{OWN}.jsonl"
    ));
    let mut body = fs::read_to_string(&rollout).unwrap();
    body.push_str(&format!("{}\n", meta_named(STRANGER)));
    fs::write(&rollout, body).unwrap();
    assert!(backend.transcript(&session, 10).is_err());
    let refused = backend.stream_transcript(&session, None, &mut |_| Ok(()));
    assert!(refused.is_err());
}
