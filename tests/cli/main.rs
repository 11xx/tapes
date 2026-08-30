use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const CODEX_SESSION_ONE: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"
);
const CODEX_SESSION_TWO: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T11-00-00-10000000-0000-0000-0000-000000000002.jsonl"
);
const CODEX_SESSION_NO_MODEL: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T13-00-00-20000000-0000-0000-0000-000000000004.jsonl"
);
const CODEX_SESSION_OTHER_MODEL: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T14-00-00-30000000-0000-0000-0000-000000000005.jsonl"
);

fn tapes() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
}

fn fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("tapes-cli-live-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
        CODEX_SESSION_ONE,
    )
    .unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T11-00-00-10000000-0000-0000-0000-000000000002.jsonl"),
        CODEX_SESSION_TWO,
    )
    .unwrap();
    (root.clone(), root.join("home"))
}

fn filter_fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("tapes-cli-filter-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let files = [
        (
            "rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl",
            CODEX_SESSION_ONE,
            1_800_000_000,
        ),
        (
            "rollout-2026-01-01T11-00-00-10000000-0000-0000-0000-000000000002.jsonl",
            CODEX_SESSION_TWO,
            1_800_000_001,
        ),
        (
            "rollout-2026-01-01T13-00-00-20000000-0000-0000-0000-000000000004.jsonl",
            CODEX_SESSION_NO_MODEL,
            1_800_000_002,
        ),
        (
            "rollout-2026-01-01T14-00-00-30000000-0000-0000-0000-000000000005.jsonl",
            CODEX_SESSION_OTHER_MODEL,
            1_800_000_003,
        ),
    ];
    for (filename, contents, modified) in files {
        let path = sessions.join(filename);
        fs::write(&path, contents).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(modified))
            .unwrap();
    }
    (root.clone(), root.join("home"))
}

fn fake_status(root: &Path, body: &str, exit: i32) -> (PathBuf, PathBuf) {
    let bin = root.join("bin");
    let snapshot = root.join("snapshot.json");
    let calls = root.join("calls");
    fs::create_dir_all(&bin).unwrap();
    fs::write(&snapshot, body).unwrap();
    let program = bin.join("harness-status");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' call >> '{}'\ncat '{}'\nexit {}\n",
            calls.display(),
            snapshot.display(),
            exit
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    (bin, calls)
}

fn fake_stalled_status(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let bin = root.join("bin");
    let calls = root.join("calls");
    let parent_pid = root.join("parent.pid");
    let child_pid = root.join("child.pid");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join("harness-status");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\nsleep 5 &\nchild=$!\nprintf '%s\\n' \"$child\" > '{}'\nprintf '%s\\n' call >> '{}'\nwait \"$child\"\n",
            parent_pid.display(),
            child_pid.display(),
            calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    (bin, calls, parent_pid, child_pid)
}

fn fake_exited_stalled_status(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let bin = root.join("bin");
    let calls = root.join("calls");
    let parent_pid = root.join("parent.pid");
    let child_pid = root.join("child.pid");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join("harness-status");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\nsleep 5 &\nchild=$!\nprintf '%s\\n' \"$child\" > '{}'\nprintf '%s\\n' call >> '{}'\nexit 0\n",
            parent_pid.display(),
            child_pid.display(),
            calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    (bin, calls, parent_pid, child_pid)
}

fn process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end_of_command = stat.rfind(") ")?;
    stat[end_of_command + 2..].chars().next()
}

fn assert_process_gone(pid_file: &Path) {
    let pid: u32 = fs::read_to_string(pid_file)
        .unwrap_or_else(|error| panic!("read {}: {error}", pid_file.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("parse {}: {error}", pid_file.display()));
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match process_state(pid) {
            None => return,
            Some(state) if Instant::now() >= deadline => {
                panic!(
                    "pid {pid} from {} remains in state {state}",
                    pid_file.display()
                )
            }
            Some(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn with_fixture_env(command: &mut Command, codex_home: &Path, home: &Path, bin: &Path) {
    command
        .env("CODEX_HOME", codex_home)
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
}

fn session<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == id)
        .unwrap()
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(path: PathBuf) -> Self {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct OpenCodeAlias {
    path: PathBuf,
}

impl OpenCodeAlias {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OpenCodeAlias {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn titleless_opencode_program(root: &Path) -> OpenCodeAlias {
    opencode_program(root, "opencode2")
}

fn opencode_program(root: &Path, name: &str) -> OpenCodeAlias {
    let program = root.join(name);
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/opencode/opencode2");
    std::os::unix::fs::symlink(fixture, &program).unwrap();
    OpenCodeAlias { path: program }
}

/// Bare `tapes` is a guide request, not a usage error; every mistyped
/// invocation still fails at clap's exit code 2.
#[test]
fn bare_invocation_guides_while_misuse_still_fails() {
    let guide = tapes().output().unwrap();
    assert!(guide.status.success());
    let text = String::from_utf8_lossy(&guide.stdout);
    for command in ["tapes list", "tapes show", "tapes export"] {
        assert!(text.contains(command), "guide omits `{command}`");
    }

    for arguments in [vec!["bogus"], vec!["show"], vec!["list", "--nope"]] {
        let misuse = tapes().args(&arguments).output().unwrap();
        assert_eq!(misuse.status.code(), Some(2), "{arguments:?} exited wrong");
    }
}

#[test]
fn list_help_exits_successfully() {
    let output = tapes().args(["list", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("full model identity"), "{help}");
    assert!(help.contains("id (variant)"), "{help}");
    assert!(help.contains("directory path"), "{help}");
}

#[test]
fn show_help_exits_successfully() {
    assert!(tapes().args(["show", "--help"]).status().unwrap().success());
}

#[test]
fn export_help_exits_successfully() {
    assert!(tapes()
        .args(["export", "--help"])
        .status()
        .unwrap()
        .success());
}

/// Selecting a session stays explicit: an id or `--latest`, never a silent
/// default, and never two scopes at once.
#[test]
fn selection_and_scope_flags_are_mutually_exclusive() {
    for arguments in [
        vec!["show", "some-id", "--latest"],
        vec!["show", "some-id", "--project", "/tmp"],
        vec!["show", "some-id", "--exclude", "other-id"],
        vec!["show", "some-id", "--harness", "codex"],
        vec!["export", "some-id", "--global"],
        vec!["show", "--exclude", "some-id"],
        vec!["show", "--latest", "--here", "--global"],
        vec!["export", "--latest", "--project", "/tmp", "--global"],
        vec!["list", "--here", "--global"],
    ] {
        let misuse = tapes().args(&arguments).output().unwrap();
        assert_eq!(misuse.status.code(), Some(2), "{arguments:?} was accepted");
    }
}

#[test]
fn list_without_stores_is_empty_and_successful() {
    let temporary_home = std::env::temp_dir().join(format!("tapes-test-{}", std::process::id()));
    let output = tapes()
        .arg("list")
        .env("HOME", temporary_home)
        .env("PATH", "/definitely/missing")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("No harnesses available."));
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("Unavailable: claude, codex, opencode, pi"));
    assert!(output.stderr.is_empty());
}

#[test]
fn list_joins_one_bounded_status_snapshot_and_maps_states() {
    let (codex_home, home) = fixture_store("list");
    let snapshot = r#"{"threads":[
      {"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"},
      {"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"attention"}
    ]}"#;
    let (bin, calls) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        session(&value, "00000000-0000-0000-0000-000000000001")["live"],
        "working"
    );
    assert_eq!(
        session(&value, "10000000-0000-0000-0000-000000000002")["live"],
        "idle"
    );
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn human_list_keeps_id_column_exact() {
    let (codex_home, home) = fixture_store("human-list");
    let snapshot = r#"{"threads":[
      {"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"},
      {"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"idle"}
    ]}"#;
    let (bin, _) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args(["list", "--global"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = text.lines().filter_map(|line| {
        let fields: Vec<_> = line.split('\t').collect();
        (fields.len() == 7).then_some(fields)
    });
    assert_eq!(
        text.lines().next(),
        Some("ID\tLIVE\tHARNESS\tMODEL\tTITLE\tDIRECTORY\tLAST ACTIVITY")
    );
    let first = rows
        .by_ref()
        .find(|row| row[0] != "ID")
        .expect("first session row");
    let second = rows.find(|row| row[0] != "ID").expect("second session row");
    assert!(rows.next().is_none(), "unexpected extra tabular row");
    assert_eq!(first[0], "10000000-0000-0000-0000-000000000002");
    assert_eq!(first[1], "idle");
    assert_eq!(second[0], "00000000-0000-0000-0000-000000000001");
    assert_eq!(second[1], "working");
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn list_filters_metadata_case_insensitively_and_before_limit() {
    let (codex_home, home) = filter_fixture_store("metadata");
    let command = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        command.output().unwrap()
    };

    let model = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--model",
        "GPT-FIXTURE",
        "--json",
    ]);
    assert!(
        model.status.success(),
        "{}",
        String::from_utf8_lossy(&model.stderr)
    );
    let model_value: Value = serde_json::from_slice(&model.stdout).unwrap();
    let model_ids = model_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        model_ids,
        vec![
            "10000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000001"
        ]
    );

    let without_filter = command(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(without_filter.status.success());
    let all: Value = serde_json::from_slice(&without_filter.stdout).unwrap();
    assert_eq!(all["sessions"].as_array().unwrap().len(), 4);
    let no_model = session(&all, "20000000-0000-0000-0000-000000000004");
    assert!(no_model.get("model").is_none());

    let directory = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--directory",
        "OTHER-PROJECT",
        "--json",
    ]);
    assert!(directory.status.success());
    let directory_value: Value = serde_json::from_slice(&directory.stdout).unwrap();
    assert_eq!(directory_value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        directory_value["sessions"][0]["id"],
        "30000000-0000-0000-0000-000000000005"
    );

    let limited = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--model",
        "gpt",
        "--limit",
        "1",
        "--json",
    ]);
    assert!(limited.status.success());
    let limited_value: Value = serde_json::from_slice(&limited.stdout).unwrap();
    assert_eq!(limited_value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        limited_value["sessions"][0]["id"],
        "10000000-0000-0000-0000-000000000002"
    );
    assert_eq!(limited_value["scanned"], 3);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn liveness_tolerates_unknown_states() {
    let (codex_home, home) = fixture_store("unknown-state");
    let snapshot = r#"{"threads":[
      {"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"},
      {"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"waiting-for-future"}
    ]}"#;
    let (bin, _) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        session(&value, "00000000-0000-0000-0000-000000000001")["live"],
        "working"
    );
    assert!(session(&value, "10000000-0000-0000-0000-000000000002")["live"].is_null());
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn liveness_times_out_stalled_authority() {
    let (codex_home, home) = fixture_store("timeout");
    let (bin, calls, parent_pid, child_pid) = fake_stalled_status(&codex_home);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let started = Instant::now();
    let output = command.output().unwrap();
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "stalled authority took {elapsed:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    assert_process_gone(&parent_pid);
    assert_process_gone(&child_pid);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn liveness_reaps_exited_authority_with_inherited_pipe() {
    let (codex_home, home) = fixture_store("exited-timeout");
    let (bin, calls, parent_pid, child_pid) = fake_exited_stalled_status(&codex_home);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let started = Instant::now();
    let output = command.output().unwrap();
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "exited stalled authority took {elapsed:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    assert_process_gone(&parent_pid);
    assert_process_gone(&child_pid);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn oversized_status_is_optional() {
    let (codex_home, home) = fixture_store("oversized");
    let oversized = format!("{{\"threads\":[]}}{}", "x".repeat(64 * 1024));
    let (bin, calls) = fake_status(&codex_home, &oversized, 0);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let started = Instant::now();
    let output = command.output().unwrap();
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "oversized authority took {elapsed:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn show_human_header_marks_matching_live_state() {
    let (codex_home, home) = fixture_store("show");
    let snapshot = r#"{"threads":[{"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"}]}"#;
    let (bin, calls) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args([
        "show",
        "00000000-0000-0000-0000-000000000001",
        "--tail",
        "1",
    ]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("# codex 00000000-0000-0000-0000-000000000001 [working]\n"));
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn show_json_carries_matching_live_state() {
    let (codex_home, home) = fixture_store("show-json");
    let snapshot = r#"{"threads":[{"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"idle"}]}"#;
    let (bin, calls) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args([
        "show",
        "10000000-0000-0000-0000-000000000002",
        "--tail",
        "1",
        "--json",
    ]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["live"], "idle");
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn unusable_status_is_optional_and_export_omits_volatile_state() {
    let (codex_home, home) = fixture_store("optional");
    let (bin, calls) = fake_status(&codex_home, "{not-json", 17);
    let mut list_command = tapes();
    list_command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut list_command, &codex_home, &home, &bin);
    let list_output = list_command.output().unwrap();

    assert!(list_output.status.success());
    let value: Value = serde_json::from_slice(&list_output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);

    let bundle = codex_home.join("bundle");
    let mut export_command = tapes();
    export_command
        .args(["export", "00000000-0000-0000-0000-000000000001", "--bundle"])
        .arg(&bundle);
    with_fixture_env(&mut export_command, &codex_home, &home, &bin);
    let export_output = export_command.output().unwrap();
    assert!(
        export_output.status.success(),
        "{}",
        String::from_utf8_lossy(&export_output.stderr)
    );
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);
    let json_path = fs::read_dir(&bundle)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    assert!(!fs::read_to_string(json_path).unwrap().contains("\"live\""));
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn piped_help_does_not_panic() {
    let mut child = tapes()
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();

    assert!(!String::from_utf8_lossy(&output.stderr).contains("panic"));
}

#[test]
fn exporting_an_unknown_session_writes_no_bundle_and_fails() {
    let directory = std::env::temp_dir().join(format!("tapes-cli-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let output = tapes()
        .args(["export", "definitely-not-a-session"])
        .arg("--bundle")
        .arg(&directory)
        .env("HOME", std::env::temp_dir().join("tapes-cli-empty-home"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "the manifest is the only stdout");
    assert!(!directory.exists(), "a failed export leaves nothing behind");
}

#[test]
fn human_renderers_show_derived_titles_and_whole_second_timestamps() {
    let root = std::env::temp_dir().join(format!("tapes-cli-human-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let id = "00000000-0000-0000-0000-0000000000aa";
    let path = root
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        concat!(
            r#"{"timestamp":"2026-01-01T10:00:00.123456789Z","type":"session_meta","payload":{"id":"00000000-0000-0000-0000-0000000000aa","cwd":"/fixtures/project"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:00:01.123456789Z","type":"turn_context","payload":{"cwd":"/fixtures/project","model":"gpt-fixture","effort":"high"}}"#,
            "\n",
            r##"{"timestamp":"2026-01-01T10:00:02.987654321Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"\n  <recommended_plugins>\n- Fixture helper\n</recommended_plugins>\n\n  # AGENTS.md instructions\n\n<INSTRUCTIONS>\nFollow the repository instructions before acting.\n</INSTRUCTIONS>\n\n<environment_context>\n  <cwd>/fixtures/project</cwd>\n</environment_context>\n\nInspect the fixture."}]}}"##,
            "\n",
            r#"{"timestamp":"2026-01-01T10:00:03.123456789Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixture inspected."}]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:00:06.123456789Z","type":"event","payload":{}}"#,
            "\n"
        ),
    )
    .unwrap();

    let command = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", root.join("home"))
            .env("CODEX_HOME", &root)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };

    let list = command(&["list", "--harness", "codex"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_text = String::from_utf8_lossy(&list.stdout);
    assert!(list_text.contains("~Inspect the fixture."), "{list_text}");
    assert!(
        !list_text.contains("# AGENTS.md instructions"),
        "{list_text}"
    );
    assert!(!list_text.contains("recommended_plugins"), "{list_text}");
    assert!(list_text.contains("2026-01-01T10:00:06Z"), "{list_text}");
    assert!(!list_text.contains(".123456789Z"), "{list_text}");
    assert!(!list_text.contains("+00:00"), "{list_text}");

    let list_json = command(&["list", "--harness", "codex", "--json"]);
    assert!(list_json.status.success());
    let list_value: serde_json::Value = serde_json::from_slice(&list_json.stdout).unwrap();
    let session = &list_value["sessions"][0];
    assert!(session.get("title").is_none());
    assert_eq!(session["derived_title"], "Inspect the fixture.");
    assert_eq!(
        session["last_activity_at"],
        "2026-01-01T10:00:06.123456789Z"
    );

    let show = command(&["show", id]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show_text = String::from_utf8_lossy(&show.stdout);
    assert!(
        show_text.contains("[user 2026-01-01T10:00:02Z]"),
        "{show_text}"
    );
    assert!(!show_text.contains(".987654321Z"), "{show_text}");
    assert!(!show_text.contains("+00:00"), "{show_text}");

    let show_json = command(&["show", id, "--json"]);
    assert!(show_json.status.success());
    let show_value: serde_json::Value = serde_json::from_slice(&show_json.stdout).unwrap();
    assert_eq!(
        show_value["turns"][0]["ts"],
        "2026-01-01T10:00:02.987654321Z"
    );
    assert!(show_value["session"].get("title").is_none());
    assert_eq!(
        show_value["session"]["derived_title"],
        "Inspect the fixture."
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn opencode_titleless_cli_keeps_metadata_absent_without_message_title_derivation() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-titleless-{}",
        std::process::id()
    )));
    let program = titleless_opencode_program(root.path());
    let alias_path = program.path().to_owned();
    let id = "ses_titleless_fixture";

    let command = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", root.path().join("home"))
            .env("PATH", root.path())
            .output()
            .unwrap()
    };

    let list = command(&["list", "--harness", "opencode", "--json"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_value: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let listed = &list_value["sessions"][0];
    assert_eq!(listed["id"], id);
    assert!(listed.get("title").is_none());
    assert!(listed.get("derived_title").is_none());

    let show = command(&["show", id, "--json"]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show_value: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert!(show_value["session"].get("title").is_none());
    assert!(show_value["session"].get("derived_title").is_none());

    let bundle = root.path().join("bundle");
    let bundle_argument = bundle.to_str().unwrap().to_owned();
    let export = command(&["export", id, "--bundle", &bundle_argument]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let context = std::fs::read_dir(&bundle)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "md"))
        .map(|path| std::fs::read_to_string(path).unwrap())
        .unwrap();
    assert!(!context.contains("- title:"), "{context}");

    drop(program);
    assert!(
        !alias_path.exists(),
        "titleless OpenCode alias was not removed"
    );
}

#[test]
fn opencode2_session_resolves_when_stable_cli_is_also_installed() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode2-with-stable-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");
    let id = "ses_000000fixtureSharedSession";

    let output = tapes()
        .args(["show", id, "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["turns"][0]["text"], "Inspect the fixture.");
}

#[test]
fn opencode_listing_is_the_deduplicated_union_of_database_and_api_sessions() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-composition-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");

    let output = tapes()
        .args(["list", "--harness", "opencode", "--global", "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut ids = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            "ses_000000fixtureSharedSession",
            "ses_api_only_fixture",
            "ses_database_only_fixture"
        ]
    );
    assert_eq!(
        value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|session| session["id"] == "ses_000000fixtureSharedSession")
            .count(),
        1
    );
}

#[test]
fn opencode_database_transcripts_preserve_normalized_turn_roles_and_text() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-database-transcript-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");
    let id = "ses_database_only_fixture";

    let output = tapes()
        .args(["show", id, "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["id"], id);
    let turns = value["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 4);
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(turns[0]["text"], "Inspect the database fixture.");
    assert_eq!(turns[1]["role"], "reasoning");
    assert_eq!(turns[1]["text"], "Reason through the database projection.");
    assert_eq!(turns[2]["role"], "tool");
    let tool_text = turns[2]["text"].as_str().unwrap();
    assert!(tool_text.contains("\"tool\":\"read\""), "{tool_text}");
    assert!(tool_text.contains("Database fixture read."), "{tool_text}");
    assert_eq!(turns[3]["role"], "assistant");
    assert_eq!(turns[3]["text"], "Database-backed answer.");
}
