use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_discovery::{Discovery, DiscoveryError, IdentityBasis, NativeStore, ResolveError};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tapes-discovery-consumer-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
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

#[test]
fn explicit_native_file_roots_resolve_canonical_headers() {
    let temp = Temp::new();
    let claude_root = temp.path().join("claude");
    let codex_root = temp.path().join("codex");
    let pi_root = temp.path().join("pi");
    let claude_file = write(
        &claude_root,
        "project/filename.jsonl",
        "{\"sessionId\":\"claude-native\"}\n",
    );
    let codex_file = write(
        &codex_root,
        "2026/09/24/rollout-2026-09-24T12-00-00-000Z-00000000-0000-0000-0000-000000000001.jsonl",
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-native\"}}\n",
    );
    let pi_file = write(
        &pi_root,
        "2026-09-24T12-00-00-000Z_session-native.jsonl",
        "{\"type\":\"session\",\"id\":\"pi-native\"}\n",
    );
    let discovery = Discovery::new([
        NativeStore::claude(&claude_root),
        NativeStore::codex(&codex_root),
        NativeStore::pi(&pi_root),
    ]);

    for (id, harness, path) in [
        ("claude-native", "claude", claude_file),
        ("codex-native", "codex", codex_file),
        ("pi-native", "pi", pi_file),
    ] {
        let session = discovery.resolve(id).unwrap();
        assert_eq!(session.id(), id);
        assert_eq!(session.harness().as_str(), harness);
        assert_eq!(session.locator(), Some(path.as_path()));
        assert_eq!(session.identity_basis(), Some(IdentityBasis::Header));
    }
}

#[test]
fn missing_native_root_is_absence_and_wrong_root_type_is_failure() {
    let temp = Temp::new();
    let missing = NativeStore::claude(temp.path().join("missing")).candidates(100);
    assert!(missing.complete);
    assert!(missing.records.is_empty());

    let wrong_root = write(temp.path(), "not-a-directory", "x");
    let page = NativeStore::claude(wrong_root).candidates(100);
    assert!(!page.complete);
    assert!(matches!(
        page.failures.first(),
        Some(DiscoveryError::InvalidMetadata { .. })
    ));
}

#[test]
fn malformed_header_is_not_reported_as_not_found_or_filename_fallback() {
    let temp = Temp::new();
    let root = temp.path().join("claude");
    write(&root, "project/name.jsonl", "{\"sessionId\":null}\n");
    let error = NativeStore::claude(root).locate_exact("name").unwrap_err();
    assert!(matches!(error, DiscoveryError::InvalidMetadata { .. }));

    let result = Discovery::new([NativeStore::claude(temp.path().join("claude"))]).resolve("name");
    assert!(matches!(
        result,
        Err(ResolveError::BackendFailed { .. }) | Err(ResolveError::Incomplete { .. })
    ));
}

#[test]
fn filename_fallback_uses_the_native_filename_convention() {
    let temp = Temp::new();
    let root = temp.path().join("pi");
    let path = write(
        &root,
        "2026-09-24T12-00-00-000Z_session-native.jsonl",
        "not json\n",
    );
    let session = NativeStore::pi(root)
        .locate_exact("session-native")
        .unwrap()
        .unwrap();
    assert_eq!(session.identity_basis(), Some(IdentityBasis::Filename));
    assert_eq!(session.locator(), Some(path.as_path()));
}

#[cfg(unix)]
#[test]
fn a_directory_symlink_cycle_is_detected_without_losing_the_store() {
    use std::os::unix::fs::symlink;
    let temp = Temp::new();
    let root = temp.path().join("pi");
    let path = write(
        &root,
        "2026-09-24T12-00-00-000Z_session-native.jsonl",
        "{\"type\":\"session\",\"id\":\"session-native\"}\n",
    );
    symlink(&root, root.join("back")).unwrap();
    let page = NativeStore::pi(&root).candidates(10);
    assert!(page.complete);
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].locator(), Some(path.as_path()));
}

#[test]
fn native_roots_keep_empty_and_relative_environment_overrides() {
    let _guard = ENV_LOCK.lock().unwrap();
    let temp = Temp::new();
    let old_home = std::env::var_os("HOME");
    let old_claude = std::env::var_os("CLAUDE_CONFIG_DIR");
    let old_codex = std::env::var_os("CODEX_HOME");
    let old_pi_dir = std::env::var_os("PI_CODING_AGENT_DIR");
    let old_pi_sessions = std::env::var_os("PI_CODING_AGENT_SESSION_DIR");
    let old_pi_file = std::env::var_os("PI_SESSION_FILE");

    std::env::set_var("HOME", temp.path().join("home"));
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CODEX_HOME");
    std::env::remove_var("PI_CODING_AGENT_DIR");
    std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
    std::env::remove_var("PI_SESSION_FILE");
    let defaults = Discovery::from_env();
    assert_eq!(
        defaults.stores()[0].root(),
        Some(temp.path().join("home/.claude/projects").as_path())
    );
    assert_eq!(
        defaults.stores()[1].root(),
        Some(temp.path().join("home/.codex/sessions").as_path())
    );
    assert_eq!(
        defaults.stores()[2].root(),
        Some(temp.path().join("home/.pi/agent/sessions").as_path())
    );

    std::env::set_var("CLAUDE_CONFIG_DIR", "relative-config");
    std::env::set_var("CODEX_HOME", "");
    std::env::set_var("PI_CODING_AGENT_SESSION_DIR", "relative-pi");
    std::env::set_var("PI_SESSION_FILE", "");
    let overrides = Discovery::from_env();
    assert_eq!(
        overrides.stores()[0].root(),
        Some(Path::new("relative-config/projects"))
    );
    assert_eq!(overrides.stores()[1].root(), Some(Path::new("sessions")));
    assert_eq!(overrides.stores()[2].root(), Some(Path::new("relative-pi")));

    restore_env("HOME", old_home);
    restore_env("CLAUDE_CONFIG_DIR", old_claude);
    restore_env("CODEX_HOME", old_codex);
    restore_env("PI_CODING_AGENT_DIR", old_pi_dir);
    restore_env("PI_CODING_AGENT_SESSION_DIR", old_pi_sessions);
    restore_env("PI_SESSION_FILE", old_pi_file);
}

#[test]
fn live_pi_session_file_adds_authoritative_identity_to_configured_directory() {
    let _guard = ENV_LOCK.lock().unwrap();
    let temp = Temp::new();
    let old_home = std::env::var_os("HOME");
    let old_claude = std::env::var_os("CLAUDE_CONFIG_DIR");
    let old_codex = std::env::var_os("CODEX_HOME");
    let old_xdg = std::env::var_os("XDG_DATA_HOME");
    let old_pi_file = std::env::var_os("PI_SESSION_FILE");
    let old_pi_dir = std::env::var_os("PI_CODING_AGENT_DIR");
    let old_pi_sessions = std::env::var_os("PI_CODING_AGENT_SESSION_DIR");

    let home = temp.path().join("home");
    let selected_root = temp.path().join("explicit-session");
    let configured_root = temp.path().join("configured-sessions");
    let selected = write(
        &selected_root,
        "runtime-session.jsonl",
        "{\"type\":\"session\",\"id\":\"session-live\"}\n",
    );
    write(
        &selected_root,
        "2026-09-24T12-01-00-000Z_session-sibling.jsonl",
        "{\"type\":\"session\",\"id\":\"session-sibling\"}\n",
    );
    let configured_live = write(
        &configured_root,
        "2026-09-24T12-01-00-000Z_session-live.jsonl",
        "{\"type\":\"session\",\"id\":\"session-live\"}\n",
    );
    let configured = write(
        &configured_root,
        "2026-09-24T12-02-00-000Z_session-configured.jsonl",
        "{\"type\":\"session\",\"id\":\"session-configured\"}\n",
    );

    std::env::set_var("HOME", &home);
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CODEX_HOME");
    std::env::set_var("XDG_DATA_HOME", temp.path().join("xdg"));
    std::env::remove_var("PI_CODING_AGENT_DIR");
    std::env::set_var("PI_CODING_AGENT_SESSION_DIR", &configured_root);
    std::env::set_var("PI_SESSION_FILE", &selected);

    let discovery = Discovery::from_env();
    let session = discovery.resolve("session-live").unwrap();
    assert_eq!(session.id(), "session-live");
    assert_eq!(session.harness().as_str(), "pi");
    assert_eq!(session.locator(), Some(selected.as_path()));
    let page = discovery
        .stores()
        .iter()
        .find(|store| store.harness().as_str() == "pi")
        .unwrap()
        .candidates(10);
    assert_eq!(page.records.len(), 2);
    assert!(page.complete);
    let live_record = page
        .records
        .iter()
        .find(|record| record.id() == "session-live")
        .unwrap();
    let configured_record = page
        .records
        .iter()
        .find(|record| record.id() == "session-configured")
        .unwrap();
    assert_eq!(live_record.locator(), Some(selected.as_path()));
    assert_ne!(live_record.locator(), Some(configured_live.as_path()));
    assert_eq!(configured_record.locator(), Some(configured.as_path()));
    assert!(matches!(
        discovery.resolve("session-sibling"),
        Err(ResolveError::NotFound { .. })
    ));
    assert_eq!(
        discovery.resolve("session-configured").unwrap().locator(),
        Some(configured.as_path())
    );

    let malformed = write(
        temp.path(),
        "malformed.jsonl",
        "{\"type\":\"session\",\"id\":\"one\"}\n{\"type\":\"session\",\"id\":\"two\"}\n",
    );
    std::env::set_var("PI_SESSION_FILE", &malformed);
    assert_eq!(
        Discovery::from_env()
            .resolve("session-configured")
            .unwrap()
            .locator(),
        Some(configured.as_path())
    );

    std::env::set_var("PI_SESSION_FILE", temp.path().join("missing.jsonl"));
    assert_eq!(
        Discovery::from_env()
            .resolve("session-configured")
            .unwrap()
            .locator(),
        Some(configured.as_path())
    );

    std::env::remove_var("PI_SESSION_FILE");
    let fallback = Discovery::from_env().resolve("session-configured").unwrap();
    assert_eq!(fallback.locator(), Some(configured.as_path()));

    restore_env("HOME", old_home);
    restore_env("CLAUDE_CONFIG_DIR", old_claude);
    restore_env("CODEX_HOME", old_codex);
    restore_env("XDG_DATA_HOME", old_xdg);
    restore_env("PI_SESSION_FILE", old_pi_file);
    restore_env("PI_CODING_AGENT_DIR", old_pi_dir);
    restore_env("PI_CODING_AGENT_SESSION_DIR", old_pi_sessions);
}

fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}

const CODEX_ID_A: &str = "00000000-0000-4000-8000-00000000000a";
const CODEX_ID_B: &str = "00000000-0000-4000-8000-00000000000b";

fn codex_record(id: &str) -> String {
    format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n")
}

fn codex_name(id: &str) -> String {
    format!("2026/09/24/rollout-2026-09-24T12-00-00-000Z-{id}.jsonl")
}

fn assert_mismatch_names_both(error: &str, path: &Path, queried: &str, found: &str) {
    assert!(error.contains(queried), "{error}");
    assert!(error.contains(found), "{error}");
    assert!(error.contains(&path.display().to_string()), "{error}");
}

#[test]
fn codex_file_named_for_one_id_but_recording_another_is_a_failure() {
    let temp = Temp::new();
    let root = temp.path().join("codex");
    let path = write(&root, &codex_name(CODEX_ID_A), &codex_record(CODEX_ID_B));

    let error = NativeStore::codex(&root)
        .locate_exact(CODEX_ID_A)
        .unwrap_err();
    assert!(matches!(error, DiscoveryError::IdentityMismatch { .. }));
    assert_mismatch_names_both(&error.to_string(), &path, CODEX_ID_A, CODEX_ID_B);

    let result = Discovery::new([NativeStore::codex(&root)]).resolve(CODEX_ID_A);
    match result {
        Err(ResolveError::BackendFailed { failures, .. }) => {
            assert_mismatch_names_both(&failures.join("; "), &path, CODEX_ID_A, CODEX_ID_B);
        }
        other => panic!("expected a backend failure, got {other:?}"),
    }
}

#[test]
fn pi_file_named_for_one_id_but_recording_another_is_a_failure() {
    let temp = Temp::new();
    let root = temp.path().join("pi");
    let path = write(
        &root,
        "2026-09-24T12-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-b\"}\n",
    );

    let error = NativeStore::pi(&root)
        .locate_exact("session-a")
        .unwrap_err();
    assert_mismatch_names_both(&error.to_string(), &path, "session-a", "session-b");
    assert!(matches!(
        Discovery::new([NativeStore::pi(&root)]).resolve("session-a"),
        Err(ResolveError::BackendFailed { .. })
    ));
}

#[test]
fn claude_file_named_for_one_id_but_recording_another_is_a_failure() {
    let temp = Temp::new();
    let root = temp.path().join("claude");
    let path = write(
        &root,
        "project/session-a.jsonl",
        "{\"sessionId\":\"session-b\"}\n",
    );
    let error = NativeStore::claude(&root)
        .locate_exact("session-a")
        .unwrap_err();
    assert_mismatch_names_both(&error.to_string(), &path, "session-a", "session-b");
}

#[test]
fn a_correct_file_for_the_queried_id_wins_over_a_mismatched_one() {
    let temp = Temp::new();
    let root = temp.path().join("codex");
    write(&root, &codex_name(CODEX_ID_A), &codex_record(CODEX_ID_B));
    let good = write(
        &root,
        &format!("2026/09/25/rollout-2026-09-25T12-00-00-000Z-{CODEX_ID_A}.jsonl"),
        &codex_record(CODEX_ID_A),
    );
    for file_first in [false, true] {
        if file_first {
            let older = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            fs::File::options()
                .write(true)
                .open(&good)
                .unwrap()
                .set_modified(older)
                .unwrap();
        }
        let session = NativeStore::codex(&root)
            .locate_exact(CODEX_ID_A)
            .unwrap()
            .unwrap();
        assert_eq!(session.id(), CODEX_ID_A);
        assert_eq!(session.locator(), Some(good.as_path()));
        assert_eq!(
            Discovery::new([NativeStore::codex(&root)])
                .resolve(CODEX_ID_A)
                .unwrap()
                .id(),
            CODEX_ID_A
        );
    }
}

#[test]
fn the_first_mismatched_file_in_sort_order_is_reported() {
    let temp = Temp::new();
    let root = temp.path().join("codex");
    let first = write(&root, &codex_name(CODEX_ID_A), &codex_record(CODEX_ID_B));
    write(
        &root,
        &format!("2026/09/23/rollout-2026-09-23T12-00-00-000Z-{CODEX_ID_A}.jsonl"),
        &codex_record("00000000-0000-4000-8000-00000000000c"),
    );
    let older = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(root.join("2026/09/23").join(format!(
            "rollout-2026-09-23T12-00-00-000Z-{CODEX_ID_A}.jsonl"
        )))
        .unwrap()
        .set_modified(older)
        .unwrap();
    let error = NativeStore::codex(&root)
        .locate_exact(CODEX_ID_A)
        .unwrap_err();
    assert_mismatch_names_both(&error.to_string(), &first, CODEX_ID_A, CODEX_ID_B);
}

#[test]
fn a_named_mismatch_cannot_resolve_as_a_unique_prefix() {
    let temp = Temp::new();
    for (harness, name, body) in [
        (
            "pi",
            "2026-09-24T12-00-00-000Z_session-a.jsonl",
            "{\"type\":\"session\",\"id\":\"session-a-other\"}\n",
        ),
        (
            "claude",
            "project/session-a.jsonl",
            "{\"sessionId\":\"session-a-other\"}\n",
        ),
    ] {
        let root = temp.path().join(harness);
        let path = write(&root, name, body);
        let store = if harness == "pi" {
            NativeStore::pi(&root)
        } else {
            NativeStore::claude(&root)
        };
        match Discovery::new([store]).resolve("session-a") {
            Err(ResolveError::BackendFailed { failures, .. }) => {
                assert_mismatch_names_both(
                    &failures.join("; "),
                    &path,
                    "session-a",
                    "session-a-other",
                );
            }
            other => panic!("expected a named mismatch failure, got {other:?}"),
        }
        let (name, body) = if harness == "pi" {
            (
                "2026-09-25T12-00-00-000Z_other.jsonl",
                "{\"type\":\"session\",\"id\":\"session-a\"}\n",
            )
        } else {
            ("project/other.jsonl", "{\"sessionId\":\"session-a\"}\n")
        };
        write(&root, name, body);
        let store = if harness == "pi" {
            NativeStore::pi(&root)
        } else {
            NativeStore::claude(&root)
        };
        assert_eq!(
            Discovery::new([store]).resolve("session-a").unwrap().id(),
            "session-a"
        );
    }
}

#[test]
fn a_later_correct_file_wins_after_mismatch_and_invalid_metadata() {
    let temp = Temp::new();
    let root = temp.path().join("pi");
    let now = SystemTime::now();
    for (offset, body) in [
        (0, "{\"type\":\"session\",\"id\":\"other\"}\n"),
        (1, "{\"type\":\"session\",\"id\":42}\n"),
        (2, "{\"type\":\"session\",\"id\":\"id\"}\n"),
    ] {
        let path = write(
            &root,
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
    assert_eq!(
        NativeStore::pi(&root)
            .locate_exact("id")
            .unwrap()
            .unwrap()
            .id(),
        "id"
    );
    assert_eq!(
        Discovery::new([NativeStore::pi(&root)])
            .resolve("id")
            .unwrap()
            .id(),
        "id"
    );
}

#[test]
fn an_explicit_pi_file_retains_named_mismatches_and_directory_precedence() {
    let temp = Temp::new();
    let root = temp.path().join("pi");
    let path = write(
        &root,
        "2026-09-24T12-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-a-other\"}\n",
    );
    let error = NativeStore::pi_file(&path)
        .locate_exact("session-a")
        .unwrap_err();
    assert!(matches!(error, DiscoveryError::IdentityMismatch { .. }));
    assert_mismatch_names_both(&error.to_string(), &path, "session-a", "session-a-other");
    assert!(matches!(
        Discovery::new([NativeStore::pi_file(&path)]).resolve("session-a"),
        Err(ResolveError::BackendFailed { .. })
    ));
    let correct = write(
        &root,
        "2026-09-25T12-00-00-000Z_session-a.jsonl",
        "{\"type\":\"session\",\"id\":\"session-a\"}\n",
    );
    let store = NativeStore::pi_with_file(&root, &path);
    let session = store.locate_exact("session-a").unwrap().unwrap();
    assert_eq!(session.locator(), Some(correct.as_path()));
    assert_eq!(
        Discovery::new([store]).resolve("session-a").unwrap().id(),
        "session-a"
    );
    assert!(NativeStore::pi_file(&path)
        .locate_exact("unrelated")
        .unwrap()
        .is_none());
    assert_eq!(
        NativeStore::pi_file(&path)
            .locate_exact("session-a-other")
            .unwrap()
            .unwrap()
            .id(),
        "session-a-other"
    );
}
