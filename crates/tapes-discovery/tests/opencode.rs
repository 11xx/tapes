use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_discovery::{
    Discovery, NativeStore, NativeStoreKind, OpenCodeFlavor, OpenCodeStore, ResolveError,
};

struct Temp(PathBuf);

static XDG_LOCK: Mutex<()> = Mutex::new(());

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tapes-discovery-opencode-it-{}-{nonce}",
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

fn fake_program(temp: &Temp, name: &str, body: &str) -> PathBuf {
    let path = temp.path().join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn set_xdg(temp: &Temp) -> Option<std::ffi::OsString> {
    let previous = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_DATA_HOME", temp.path());
    fs::create_dir_all(temp.path().join("opencode")).unwrap();
    fs::write(
        temp.path().join("opencode/opencode.db"),
        b"fixture database",
    )
    .unwrap();
    previous
}

fn restore_xdg(previous: Option<std::ffi::OsString>) {
    if let Some(previous) = previous {
        std::env::set_var("XDG_DATA_HOME", previous);
    } else {
        std::env::remove_var("XDG_DATA_HOME");
    }
}

#[test]
fn v2_resolves_from_session_metadata_without_transcript_routes() {
    let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = Temp::new();
    let previous = set_xdg(&temp);
    let log = temp.path().join("requests.log");
    let program = fake_program(
        &temp,
        "opencode2-fixture",
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; printf '{{\"data\":{{\"id\":\"ses_fixture\",\"title\":\"metadata only\"}}}}\\n'",
            log.display()
        ),
    );
    let discovery = Discovery::new([NativeStore::opencode_v2(&program)]);
    let session = discovery.resolve("ses_fixture").unwrap();
    assert_eq!(session.id(), "ses_fixture");
    assert_eq!(session.harness().as_str(), "opencode");
    assert_eq!(
        session.store_kind(),
        NativeStoreKind::OpenCode(OpenCodeFlavor::V2)
    );
    assert_eq!(session.metadata().unwrap()["title"], "metadata only");
    assert!(session.store_coordinate().contains("opencode2-fixture"));
    assert_eq!(OpenCodeStore::v2(&program).flavor(), OpenCodeFlavor::V2);

    let requests = fs::read_to_string(log).unwrap();
    assert!(requests.contains("/api/session/ses_fixture"));
    assert!(!requests.contains("message"));
    assert!(!requests.contains("part"));
    assert!(!requests.contains("export"));
    assert!(!requests.contains("serve"));
    restore_xdg(previous);
}

#[test]
fn malformed_identity_and_null_have_distinct_resolver_outcomes() {
    let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = Temp::new();
    let previous = set_xdg(&temp);
    let null_program = fake_program(
        &temp,
        "opencode2-null",
        "case \"$4\" in /api/session\\?*) printf '{\"data\":[]}\\n' ;; *) printf '{\"data\":null}\\n' ;; esac",
    );
    let absent = Discovery::new([NativeStore::opencode_v2(null_program)]);
    assert!(matches!(
        absent.resolve("ses_absent"),
        Err(ResolveError::NotFound { .. })
    ));

    let malformed_program = fake_program(
        &temp,
        "opencode2-malformed",
        "case \"$4\" in /api/session\\?*) printf '{\"data\":[{\"id\":42}]}\\n' ;; *) printf '{\"data\":{\"id\":42}}\\n' ;; esac",
    );
    let malformed = Discovery::new([NativeStore::opencode_v2(malformed_program)]);
    assert!(matches!(
        malformed.resolve("ses_bad"),
        Err(ResolveError::BackendFailed { .. })
    ));
    restore_xdg(previous);
}

#[test]
fn stable_program_constructor_keeps_database_store_flavor() {
    let temp = Temp::new();
    let program = temp.path().join("opencode");
    assert_eq!(OpenCodeStore::new(program).flavor(), OpenCodeFlavor::Stable);
}

#[test]
fn v2_prefix_uses_one_exact_read_and_one_bounded_listing() {
    let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = Temp::new();
    let previous = set_xdg(&temp);
    let log = temp.path().join("requests.log");
    let json = r#"{"data":[{"id":"ses_unique-prefix-one"},{"id":"ses_elsewhere"}]}"#;
    let program = fake_program(
        &temp,
        "opencode2-prefix",
        &format!(
            "printf '%s\\n' \"$*\" >> '{}'; case \"$4\" in /api/session\\?*) printf '%s\\n' '{}' ;; *) printf '{{\"data\":null}}\\n' ;; esac",
            log.display(),
            json
        ),
    );
    let discovery = Discovery::new([NativeStore::opencode_v2(program)]);
    let session = discovery.resolve("ses_unique-prefix").unwrap();
    assert_eq!(session.id(), "ses_unique-prefix-one");
    let requests = fs::read_to_string(log).unwrap();
    assert_eq!(requests.lines().count(), 2);
    assert!(requests.contains("/api/session/ses_unique-prefix"));
    assert!(requests.contains("/api/session?order=desc&limit=1000"));
    assert!(!requests.contains("message"));
    assert!(!requests.contains("part"));
    restore_xdg(previous);
}

#[test]
fn one_prefix_match_in_a_capped_opencode_listing_is_incomplete() {
    let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = Temp::new();
    let previous = set_xdg(&temp);
    let mut rows = Vec::with_capacity(1_000);
    rows.push(r#"{"id":"ses_prefix-one"}"#.to_owned());
    rows.extend((0..999).map(|index| format!("{{\"id\":\"ses_elsewhere-{index}\"}}")));
    let json = format!("{{\"data\":[{}]}}", rows.join(","));
    let program = fake_program(
        &temp,
        "opencode2-capped",
        &format!(
            "case \"$4\" in /api/session\\?*) printf '%s\\n' '{}' ;; *) printf '{{\"data\":null}}\\n' ;; esac",
            json
        ),
    );
    let discovery = Discovery::new([NativeStore::opencode_v2(program)]);
    assert!(matches!(
        discovery.resolve("ses_prefix"),
        Err(ResolveError::Incomplete { .. })
    ));
    restore_xdg(previous);
}
