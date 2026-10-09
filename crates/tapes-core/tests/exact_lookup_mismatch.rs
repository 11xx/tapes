use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::backend::{backends_from_stores, MIN_READ_BYTES};
use agent_tapes_core::{resolve_session, NativeStore, ResolveError};

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tapes-exact-mismatch-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, relative: &str, body: &str) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn assert_mismatch(result: Result<impl std::fmt::Debug, ResolveError>, queried: &str, found: &str) {
    match result {
        Err(ResolveError::BackendFailed { failures, .. }) => {
            let text = failures.join("; ");
            assert!(text.contains(queried), "{text}");
            assert!(text.contains(found), "{text}");
        }
        other => panic!("expected a backend failure naming both ids, got {other:?}"),
    }
}

const CODEX_A: &str = "00000000-0000-4000-8000-00000000000a";
const CODEX_B: &str = "00000000-0000-4000-8000-00000000000b";

fn codex_rollout(id: &str, body_id: &str) -> (String, String) {
    (
        format!("2026/01/01/rollout-2026-01-01T10-00-00-000Z-{id}.jsonl"),
        format!(
            "{{\"timestamp\":\"2026-01-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{body_id}\",\"timestamp\":\"2026-01-01T10:00:00Z\",\"cwd\":\"/work\"}}}}\n\
             {{\"timestamp\":\"2026-01-01T10:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"hello\"}}}}\n"
        ),
    )
}

#[test]
fn codex_recording_under_another_session_name_fails_the_lookup() {
    let temp = Temp::new("codex");
    let (name, body) = codex_rollout(CODEX_A, CODEX_B);
    write(&temp.0, &name, &body);
    let backends = backends_from_stores([NativeStore::codex(&temp.0)], MIN_READ_BYTES).unwrap();
    assert_mismatch(resolve_session(&backends, CODEX_A), CODEX_A, CODEX_B);

    let (name, body) = codex_rollout(CODEX_A, CODEX_A);
    write(&temp.0, &name.replace("2026/01/01", "2026/01/02"), &body);
    let backends = backends_from_stores([NativeStore::codex(&temp.0)], MIN_READ_BYTES).unwrap();
    let resolved = resolve_session(&backends, CODEX_A).unwrap();
    assert_eq!(resolved.session.id, CODEX_A);
}

#[test]
fn pi_recording_under_another_session_name_fails_the_lookup() {
    let temp = Temp::new("pi");
    write(
        &temp.0,
        "2026-01-01T10-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-b\",\"timestamp\":\"2026-01-01T10:00:00Z\",\"cwd\":\"/work\"}\n",
    );
    let backends = backends_from_stores([NativeStore::pi(&temp.0)], MIN_READ_BYTES).unwrap();
    assert_mismatch(
        resolve_session(&backends, "session-a"),
        "session-a",
        "session-b",
    );

    write(
        &temp.0,
        "2026-01-02T10-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-a\",\"timestamp\":\"2026-01-02T10:00:00Z\",\"cwd\":\"/work\"}\n",
    );
    let backends = backends_from_stores([NativeStore::pi(&temp.0)], MIN_READ_BYTES).unwrap();
    assert_eq!(
        resolve_session(&backends, "session-a").unwrap().session.id,
        "session-a"
    );
}
