use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tapes_discovery::{Discovery, DiscoveryError, IdentityBasis, NativeStore, ResolveError};

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
    let temp = Temp::new();
    let old_home = std::env::var_os("HOME");
    let old_claude = std::env::var_os("CLAUDE_CONFIG_DIR");
    let old_codex = std::env::var_os("CODEX_HOME");
    let old_pi_dir = std::env::var_os("PI_CODING_AGENT_DIR");
    let old_pi_sessions = std::env::var_os("PI_CODING_AGENT_SESSION_DIR");

    std::env::set_var("HOME", temp.path().join("home"));
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    std::env::remove_var("CODEX_HOME");
    std::env::remove_var("PI_CODING_AGENT_DIR");
    std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
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
}

fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}
