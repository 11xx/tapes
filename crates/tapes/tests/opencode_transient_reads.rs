//! A command whose OpenCode read failed for a reason that can pass exits 75,
//! saying the failure is transient; one whose read cannot pass exits 1.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const SESSION: &str = "ses_database_only_fixture";

/// A home whose OpenCode database is read through a `sqlite3` that answers
/// every message query with `message_answer`, a shell command, and passes
/// every other query to the fixture's `opencode db`.
fn usage(message_answer: &str) -> Output {
    let root = tempfile::TempDir::with_prefix("tapes-cli-opencode-transient-").unwrap();
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tapes-core/tests/fixtures/opencode/opencode2"),
        bin.join("opencode"),
    )
    .unwrap();
    let sqlite3 = bin.join("sqlite3");
    fs::write(
        &sqlite3,
        format!(
            r#"#!/bin/sh
for argument; do query=$argument; done
case $query in
  *"FROM message"*) {message_answer} ;;
esac
exec "${{0%/*}}/opencode" db --format tsv "$query"
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&sqlite3, fs::Permissions::from_mode(0o700)).unwrap();
    let data = root.path().join("home/.local/share/opencode");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("opencode.db"), b"SQLite format 3\0").unwrap();
    Command::new(env!("CARGO_BIN_EXE_tapes"))
        .args(["usage", SESSION, "--harness", "opencode", "--json"])
        .env_remove("XDG_DATA_HOME")
        .env("HOME", root.path().join("home"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .unwrap()
}

/// Rejects a transient failure that exits like any other, which a caller
/// cannot tell from a read that will never succeed.
#[test]
fn a_read_still_busy_after_every_attempt_exits_75_saying_it_is_transient() {
    let output = usage(
        "echo 'Parse error in 6th command line argument: database is locked (5)' >&2; exit 1",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(75), "{stderr}");
    assert!(stderr.contains("transient"), "{stderr}");
}

/// Rejects a failure that cannot pass reported as transient.
#[test]
fn a_read_failure_that_cannot_pass_exits_1() {
    let output = usage(
        "echo 'Parse error in 6th command line argument: no such table: message' >&2; exit 1",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains("transient"), "{stderr}");
}

/// The fixture answers when its database is not busy.
#[test]
fn a_read_the_database_answers_exits_0() {
    let output = usage(":");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
