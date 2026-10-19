use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_tapes_core::backend::{backends_from_stores, MIN_READ_BYTES};
use agent_tapes_core::{resolve_session, NativeStore, ResolveError};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tapes-exact-mismatch-{label}-{}-{}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
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

#[test]
fn a_named_mismatch_cannot_resolve_as_a_unique_prefix() {
    let temp = Temp::new("prefix");
    write(
        &temp.0,
        "2026-01-01T10-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-a-other\"}\n",
    );
    let backends = backends_from_stores([NativeStore::pi(&temp.0)], MIN_READ_BYTES).unwrap();
    assert_mismatch(
        resolve_session(&backends, "session-a"),
        "session-a",
        "session-a-other",
    );
}

#[test]
fn a_later_correct_file_wins_after_mismatch_and_invalid_metadata() {
    let temp = Temp::new("three-candidates");
    let now = std::time::SystemTime::now();
    for (offset, body) in [
        (0, "{\"type\":\"session\",\"id\":\"other\"}\n"),
        (1, "{\"type\":\"session\",\"id\":42}\n"),
        (2, "{\"type\":\"session\",\"id\":\"id\"}\n"),
    ] {
        let path = write(
            &temp.0,
            &format!("2026-09-0{}T12-00-00-000Z_id.jsonl", offset + 1),
            body,
        );
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(now - std::time::Duration::from_secs(offset * 60))
            .unwrap();
    }
    let backends = backends_from_stores([NativeStore::pi(&temp.0)], MIN_READ_BYTES).unwrap();
    assert_eq!(backends[0].locate("id").unwrap().unwrap().id, "id");
    assert_eq!(resolve_session(&backends, "id").unwrap().session.id, "id");
}

#[test]
fn an_explicit_pi_file_retains_named_mismatches_and_directory_precedence() {
    let temp = Temp::new("explicit-pi");
    let path = write(
        &temp.0,
        "2026-09-24T12-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-a-other\"}\n",
    );
    let backends = backends_from_stores([NativeStore::pi_file(&path)], MIN_READ_BYTES).unwrap();
    assert!(backends[0].locate("session-a").is_err());
    assert_mismatch(
        resolve_session(&backends, "session-a"),
        "session-a",
        "session-a-other",
    );
    write(
        &temp.0,
        "2026-09-25T12-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-a\"}\n",
    );
    let backends =
        backends_from_stores([NativeStore::pi_with_file(&temp.0, &path)], MIN_READ_BYTES).unwrap();
    assert_eq!(
        backends[0].locate("session-a").unwrap().unwrap().id,
        "session-a"
    );
    assert_eq!(
        resolve_session(&backends, "session-a").unwrap().session.id,
        "session-a"
    );
}
