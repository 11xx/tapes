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
        .env_remove("XDG_DATA_HOME")
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

fn fixture_command(name: &str, args: &[&str]) -> std::process::Output {
    let (codex_home, home) = fixture_store(name);
    let mut command = tapes();
    command.args(args);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    command.output().unwrap()
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

fn malformed_opencode_program(root: &Path) -> OpenCodeAlias {
    let program = root.join("opencode");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/opencode/opencode-malformed-row");
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
    for command in [
        "tapes list",
        "tapes show",
        "tapes events",
        "tapes stats",
        "tapes brief",
        "tapes export",
    ] {
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
    assert!(help.contains("last_activity_at"), "{help}");
    assert!(
        help.contains("newest first by default, or oldest"),
        "{help}"
    );
    assert!(
        help.contains("RFC 3339 timestamps with an offset"),
        "{help}"
    );
    assert!(help.contains("last 32 normalized turns"), "{help}");
    assert!(help.contains("unsearched"), "{help}");
}

#[test]
fn show_help_exits_successfully() {
    assert!(tapes().args(["show", "--help"]).status().unwrap().success());
}

#[test]
fn events_help_explains_pairing_filters_and_the_default_bound() {
    let output = tapes().args(["events", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("Pairing is exact within the bounded read"),
        "{help}"
    );
    assert!(
        help.contains("every turn the bounded reader reaches"),
        "{help}"
    );
    assert!(help.contains("--name <NAME>"), "{help}");
    assert!(help.contains("--call-id <ID>"), "{help}");
    assert!(help.contains("tapes-events/1"), "{help}");
}

#[test]
fn export_help_explains_the_selection_form_and_its_manifest() {
    let output = tapes().args(["export", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("manifest.json"), "{help}");
    assert!(help.contains("`failed`"), "{help}");
    assert!(help.contains("the same order"), "{help}");
    assert!(help.contains("--since <TIMESTAMP>"), "{help}");
    assert!(help.contains("--sort <ORDER>"), "{help}");
    assert!(help.contains("--search <SUBSTRING>"), "{help}");
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
        vec!["events", "some-id", "--latest"],
        vec!["show", "--exclude", "some-id"],
        vec!["show", "--latest", "--here", "--global"],
        vec!["export", "--latest", "--project", "/tmp", "--global"],
        vec!["export", "some-id", "--since", "2026-01-01"],
        vec!["export", "some-id", "--sort", "oldest"],
        vec!["export", "--latest", "--search", "parser"],
        vec!["export"],
        vec!["export", "--bundle", "/tmp"],
        vec!["list", "--here", "--global"],
    ] {
        let misuse = tapes().args(&arguments).output().unwrap();
        assert_eq!(misuse.status.code(), Some(2), "{arguments:?} was accepted");
    }
}

#[test]
fn events_json_answers_call_counts_and_incomplete_calls_without_raw_text() {
    let id = "00000000-0000-0000-0000-000000000001";
    let output = fixture_command("events-json", &["events", id, "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-events/1");
    assert_eq!(value["session"]["id"], id);
    assert!(value.get("truncation").is_none(), "{value}");
    assert_eq!(
        value["pairs"],
        serde_json::json!({"complete": 1, "incomplete": 1})
    );
    let events = value["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|event| event.get("text").is_none()));
    assert_eq!(
        events
            .iter()
            .filter(|event| { event["kind"] == "tool-call" && event["name"] == "fixture_tool" })
            .count(),
        1
    );
    let incomplete = events
        .iter()
        .find(|event| event["incomplete"] == "no-result-in-read")
        .unwrap();
    assert_eq!(incomplete["call_id"], "call-pending");
    assert!(incomplete.get("pair").is_none(), "{incomplete}");
    assert!(incomplete.get("duration_ms").is_none(), "{incomplete}");
    assert_eq!(events[0]["duration_ms"], 1_000);
    assert!(events[1].get("duration_ms").is_none(), "{}", events[1]);
}

#[test]
fn events_filters_after_pairing_and_tail_uses_show_ordinals() {
    let id = "00000000-0000-0000-0000-000000000001";
    let named = fixture_command(
        "events-name",
        &["events", id, "--name", "fixture_tool", "--json"],
    );
    let named: Value = serde_json::from_slice(&named.stdout).unwrap();
    assert_eq!(named["events"].as_array().unwrap().len(), 1);
    assert_eq!(named["events"][0]["kind"], "tool-call");
    assert_eq!(
        named["pairs"],
        serde_json::json!({"complete": 1, "incomplete": 0})
    );

    let wrong_case = fixture_command(
        "events-name-case",
        &["events", id, "--name", "Fixture_Tool", "--json"],
    );
    let wrong_case: Value = serde_json::from_slice(&wrong_case.stdout).unwrap();
    assert!(wrong_case["events"].as_array().unwrap().is_empty());

    let pending = fixture_command(
        "events-call-id",
        &["events", id, "--call-id", "call-pending", "--json"],
    );
    let pending: Value = serde_json::from_slice(&pending.stdout).unwrap();
    assert_eq!(pending["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        pending["pairs"],
        serde_json::json!({"complete": 0, "incomplete": 1})
    );

    let shown = fixture_command("events-show-tail", &["show", id, "--tail", "1", "--json"]);
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let events = fixture_command("events-tail", &["events", id, "--tail", "1", "--json"]);
    let events: Value = serde_json::from_slice(&events.stdout).unwrap();
    assert_eq!(
        events["truncation"]["window"],
        shown["truncation"]["window"]
    );
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    assert_eq!(events["events"][0]["ordinal"], shown["turns"][0]["ordinal"]);

    let human = fixture_command("events-human", &["events", id, "--tail", "1"]);
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(
        human.contains("[tool-call #5 2026-01-01T10:00:06Z]"),
        "{human}"
    );
    assert!(
        human.contains("fixture_pending call-pending completed no-result-in-read"),
        "{human}"
    );
    assert!(human.contains("Showing the last 1 of 6 turns"), "{human}");
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
fn list_filters_activity_before_limit_and_serializes_the_window() {
    let (codex_home, home) = filter_fixture_store("activity");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T10:00:00+00:00",
        "--until",
        "2026-01-01T10:30:00Z",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["sort"], "newest");
    assert_eq!(
        value["activity"],
        serde_json::json!({
            "since": "2026-01-01T10:00:00Z",
            "until": "2026-01-01T10:30:00Z"
        })
    );
    assert_eq!(
        value["sessions"][0]["id"],
        "00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(value["scanned"], 4);
    assert_eq!(value["scan_truncated"], false);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_sort_oldest_orders_recorded_activity_and_keeps_activity_absent() {
    let (codex_home, home) = filter_fixture_store("sort");
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

    let newest = command(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(newest.status.success());
    let newest_value: Value = serde_json::from_slice(&newest.stdout).unwrap();
    let newest_ids = newest_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        newest_ids,
        vec![
            "30000000-0000-0000-0000-000000000005",
            "20000000-0000-0000-0000-000000000004",
            "10000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000001"
        ]
    );
    assert_eq!(newest_value["sort"], "newest");
    assert!(newest_value.get("activity").is_none());

    let oldest = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--sort",
        "oldest",
        "--json",
    ]);
    assert!(oldest.status.success());
    let oldest_value: Value = serde_json::from_slice(&oldest.stdout).unwrap();
    let oldest_ids = oldest_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        oldest_ids,
        vec![
            "00000000-0000-0000-0000-000000000001",
            "10000000-0000-0000-0000-000000000002",
            "20000000-0000-0000-0000-000000000004",
            "30000000-0000-0000-0000-000000000005"
        ]
    );
    assert_eq!(oldest_value["sort"], "oldest");
    assert!(oldest_value.get("activity").is_none());

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_accepts_date_only_activity_bounds_and_omits_absent_bound() {
    let (codex_home, home) = filter_fixture_store("date-only");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["activity"]["since"], "2026-01-01T00:00:00Z");
    assert!(value["activity"].get("until").is_none());
    assert_eq!(
        value["sessions"][0]["id"],
        "30000000-0000-0000-0000-000000000005"
    );

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_rejects_a_non_half_open_activity_window() {
    let cases = [
        ("2026-01-01T10:00:00Z", "2026-01-01T10:00:00Z"),
        ("2026-01-01T11:00:00Z", "2026-01-01T10:00:00Z"),
    ];
    for (since, until) in cases {
        let (codex_home, home) = filter_fixture_store("invalid-window");
        let mut command = tapes();
        command.args([
            "list",
            "--global",
            "--harness",
            "codex",
            "--since",
            since,
            "--until",
            until,
        ]);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("--since must be earlier than --until"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(codex_home).unwrap();
    }
}

#[test]
fn list_searches_recent_fixture_turns_before_the_limit_and_survives_a_bad_line() {
    let (codex_home, home) = filter_fixture_store("search");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--search",
        "VALID",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("last 32 normalized turns"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["sessions"][0]["id"],
        "10000000-0000-0000-0000-000000000002"
    );
    assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(value["scanned"], 4);
    assert_eq!(value["scan_truncated"], false);
    assert!(value["unsearched"].as_array().unwrap().is_empty());

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
fn show_names_a_trailing_record_in_human_and_json_output() {
    let (codex_home, home) = fixture_store("trailing-record");

    let mut human_command = tapes();
    human_command.args([
        "show",
        "00000000-0000-0000-0000-000000000001",
        "--tail",
        "1",
    ]);
    with_fixture_env(&mut human_command, &codex_home, &home, &codex_home);
    let human = human_command.output().unwrap();
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("The newest trailing record is `event_msg` at 2026-01-01T10:00:07Z"),
        "{text}"
    );

    let mut json_command = tapes();
    json_command.args([
        "show",
        "00000000-0000-0000-0000-000000000001",
        "--tail",
        "1",
        "--json",
    ]);
    with_fixture_env(&mut json_command, &codex_home, &home, &codex_home);
    let json_output = json_command.output().unwrap();
    assert!(json_output.status.success());
    let value: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(
        value["trailing_record"],
        serde_json::json!({
            "kind": "event_msg",
            "timestamp": "2026-01-01T10:00:07Z"
        })
    );

    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn show_omits_an_absent_trailing_record_in_human_and_json_output() {
    let (codex_home, home) = fixture_store("no-trailing-record");

    let mut human_command = tapes();
    human_command.args([
        "show",
        "10000000-0000-0000-0000-000000000002",
        "--tail",
        "1",
    ]);
    with_fixture_env(&mut human_command, &codex_home, &home, &codex_home);
    let human = human_command.output().unwrap();
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(!text.contains("trailing record"), "{text}");

    let mut json_command = tapes();
    json_command.args([
        "show",
        "10000000-0000-0000-0000-000000000002",
        "--tail",
        "1",
        "--json",
    ]);
    with_fixture_env(&mut json_command, &codex_home, &home, &codex_home);
    let json_output = json_command.output().unwrap();
    assert!(json_output.status.success());
    let value: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert!(value.get("trailing_record").is_none());

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
            r#"{"timestamp":"2026-01-01T10:00:02.999999999Z","type":"event_msg","payload":{"type":"user_message","message":"Inspect the fixture."}}"#,
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
    assert_eq!(session["derived_title_truncated"], false);
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
        show_text.contains("[user #0 2026-01-01T10:00:02Z]"),
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
    assert_eq!(show_value["session"]["derived_title_truncated"], false);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn list_json_marks_derived_title_truncation_without_changing_the_title_text() {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-derived-title-marker-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
        CODEX_SESSION_ONE,
    )
    .unwrap();
    let long_id = "00000000-0000-0000-0000-0000000000bb";
    let long_request = "word ".repeat(96);
    fs::write(
        sessions.join(format!(
            "rollout-2026-01-01T09-00-00-{long_id}.jsonl"
        )),
        format!(
            concat!(
                r#"{{"timestamp":"2026-01-01T09:00:00Z","type":"session_meta","payload":{{"id":"{long_id}","cwd":"/fixtures/project"}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-01-01T09:00:01Z","type":"turn_context","payload":{{"cwd":"/fixtures/project","model":"gpt-fixture","effort":"low"}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-01-01T09:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{long_request}"}}]}}}}"#,
                "\n"
            ),
            long_id = long_id,
            long_request = long_request
        ),
    )
    .unwrap();

    let mut command = tapes();
    command.args(["list", "--global", "--harness", "codex", "--json"]);
    command
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", &root)
        .env("PATH", "/definitely/missing");
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let complete = session(&value, "00000000-0000-0000-0000-000000000001");
    assert_eq!(complete["derived_title"], "Inspect the fixture.");
    assert_eq!(complete["derived_title_truncated"], false);

    let shortened = session(&value, long_id);
    assert_eq!(shortened["derived_title_truncated"], true);
    assert_eq!(
        shortened["derived_title"].as_str().unwrap().chars().count(),
        96
    );
    assert!(shortened["derived_title"].as_str().unwrap().ends_with('…'));

    fs::remove_dir_all(root).unwrap();
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
    assert!(listed.get("derived_title_truncated").is_none());

    let show = command(&["show", id, "--json"]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show_value: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert!(show_value["session"].get("title").is_none());
    assert!(show_value["session"].get("derived_title").is_none());
    assert!(show_value["session"]
        .get("derived_title_truncated")
        .is_none());

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
fn malformed_opencode_database_row_is_unreadable_without_breaking_listing() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-malformed-row-{}",
        std::process::id()
    )));
    let _stable = malformed_opencode_program(root.path());

    let list = |json: bool| {
        let mut arguments = vec!["list", "--harness", "opencode", "--global"];
        if json {
            arguments.push("--json");
        }
        tapes()
            .args(arguments)
            .env("HOME", root.path().join("home"))
            .env("PATH", root.path())
            .output()
            .unwrap()
    };

    let json = list(true);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
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
            "ses_database_only_fixture"
        ]
    );
    let unreadable = value["unreadable"].as_array().unwrap();
    assert_eq!(value["scanned"], 3);
    assert_eq!(unreadable.len(), 1);
    assert!(unreadable[0]
        .as_str()
        .unwrap()
        .contains("ses_truncated_fixture"));
    assert!(unreadable[0]
        .as_str()
        .unwrap()
        .contains("EOF while parsing a string"));
    // A corrupt row says nothing about the harness, which was read fine.
    assert!(
        value["unavailable"].as_array().unwrap().is_empty(),
        "{value}"
    );

    let human = list(false);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human_text = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_text.contains("ses_database_only_fixture"),
        "{human_text}"
    );
    assert!(
        human_text.contains("ses_000000fixtureSharedSession"),
        "{human_text}"
    );
    assert!(
        human_text.contains("Unreadable: opencode session ses_truncated_fixture:"),
        "{human_text}"
    );
    assert!(
        human_text.contains("EOF while parsing a string"),
        "{human_text}"
    );
    assert!(
        !human_text.contains("No harnesses available."),
        "{human_text}"
    );

    let show = tapes()
        .args(["show", "ses_truncated_fixture", "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(!show.status.success());
    let error = String::from_utf8_lossy(&show.stderr);
    assert!(error.contains("ses_truncated_fixture"), "{error}");
    assert!(error.contains("EOF while parsing a string"), "{error}");
}

#[test]
fn database_prefilter_failure_is_visible_while_the_safe_fallback_runs() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-prefilter-failure-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let mut command = tapes();
    command.args([
        "list",
        "--harness",
        "opencode",
        "--global",
        "--search",
        "PREFILTERERROR",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &root.path().join("codex"),
        &root.path().join("home"),
        root.path(),
    );

    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["sessions"].as_array().unwrap().is_empty());
    assert_eq!(value["unsearched"].as_array().unwrap().len(), 1);
    assert!(value["unsearched"][0]
        .as_str()
        .unwrap()
        .contains("opencode search prefilter failed"));
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

    let output = tapes()
        .args(["events", id, "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 2);
    assert_eq!(events["events"][0]["call_id"], "call_database_fixture");
    assert_eq!(events["events"][0]["duration_ms"], 999);
    assert_eq!(events["events"][1]["kind"], "tool-result");
    assert_eq!(events["events"][1]["ts"], "2026-07-21T19:45:08.999Z");
}

/// The bounded reader keeps a file's tail, so an oversized session would
/// otherwise report the first retained record as its start. `show --json`
/// reports the header's recorded start and still marks the window truncated.
#[test]
fn show_reports_the_recorded_start_of_an_oversized_session() {
    let root =
        std::env::temp_dir().join(format!("tapes-cli-oversized-start-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-00000000bbbb";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    use std::io::Write;
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for _ in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T12:00:00Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);
    assert!(fs::metadata(&path).unwrap().len() > 4 * 1024 * 1024);

    let mut command = tapes();
    command.args(["show", id, "--tail", "1", "--json"]);
    command
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", &root)
        .env("PATH", "/definitely/missing");
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["session"]["started_at"], "2026-01-01T10:00:00Z");
    assert_eq!(value["session"]["last_activity_at"], "2026-01-01T12:00:00Z");
    assert_eq!(value["session"]["directory"], "/fixtures/project");
    assert_eq!(value["truncated"], true);
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["truncation"]["source"],
        serde_json::json!([{ "kind": "file-tail", "bytes": 4_194_304 }])
    );
    assert_eq!(value["truncation"]["window"]["returned"], 1);
    assert_eq!(value["truncation"]["window"]["bound"], 1);

    fs::remove_dir_all(root).unwrap();
}

/// The recorded case: a 147-turn session shown through the default window
/// returns 100 turns and says 47 earlier ones were windowed out, with no
/// source bound; a wide enough window clears it; export reads every turn.
#[test]
fn show_distinguishes_the_turn_window_from_source_truncation() {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-window-provenance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-00000000cccc";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    use std::io::Write;
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    for index in 0..147 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T10:00:{:02}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"turn {index}"}}]}}}}"#,
            index % 60
        )
        .unwrap();
    }
    drop(file);

    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        command
            .env("HOME", root.join("home"))
            .env("CODEX_HOME", &root)
            .env("PATH", "/definitely/missing");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 100);
    assert_eq!(value["truncated"], true);
    assert_eq!(
        value["truncation"],
        serde_json::json!({
            "window": { "returned": 100, "omitted": 47, "omitted_from": "head", "bound": 100, "ordinals": { "first": 47, "last": 146 } }
        })
    );

    let human = String::from_utf8(run(&["show", id])).unwrap();
    assert!(
        human.contains(
            "Showing the last 100 of 147 turns; 47 earlier turns fall outside the 100-turn window"
        ),
        "{human}"
    );
    assert!(human.contains("--tail 147"), "{human}");

    let value: Value =
        serde_json::from_slice(&run(&["show", id, "--tail", "200", "--json"])).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 147);
    assert_eq!(value["truncated"], false);
    assert!(value.get("truncation").is_none(), "{value}");

    let bundle = root.join("bundle");
    let listing =
        String::from_utf8(run(&["export", id, "--bundle", bundle.to_str().unwrap()])).unwrap();
    let json_path = listing
        .lines()
        .find(|line| line.contains(".json"))
        .and_then(|line| line.split('\t').next())
        .unwrap();
    let exported: Value = serde_json::from_str(&fs::read_to_string(json_path).unwrap()).unwrap();
    assert_eq!(exported["turns"].as_array().unwrap().len(), 147);
    assert_eq!(exported["truncated"], false);
    assert!(exported.get("truncation").is_none(), "{exported}");

    fs::remove_dir_all(root).unwrap();
}

/// The database projection cuts long part text; the transcript names that
/// bound and the count of parts it cut rather than a bare flag.
#[test]
fn opencode_database_reads_name_cut_turn_text_as_a_source_bound() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-cut-text-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");

    let output = tapes()
        .args(["show", "ses_database_only_fixture", "--json"])
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
    assert_eq!(value["truncated"], true);
    assert_eq!(
        value["truncation"],
        serde_json::json!({
            "source": [{ "kind": "turn-text", "turns": 1, "chars": 4000 }]
        })
    );

    let human = tapes()
        .args(["show", "ses_database_only_fixture"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("1 turn carries text cut at 4000 characters"),
        "{text}"
    );
    assert!(!text.contains("Use --tail"), "{text}");
}

/// A consumer re-finds a turn from the session id and the ordinal alone, so
/// ordinals count the whole normalized sequence even under a window, the
/// window names the ordinals it holds, and the session names where it was
/// read from.
#[test]
fn show_gives_every_turn_a_coordinate_a_consumer_can_write_down() {
    let (codex_home, home) = fixture_store("source-reference");
    let id = "00000000-0000-0000-0000-000000000001";
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let whole: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    let turns = whole["turns"].as_array().unwrap();
    for (index, turn) in turns.iter().enumerate() {
        assert_eq!(turn["ordinal"], index, "{turn}");
    }
    assert!(
        whole["session"]["store"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{id}.jsonl")),
        "{}",
        whole["session"]
    );
    assert!(whole.get("truncation").is_none());

    let last = turns.len() - 1;
    let windowed: Value =
        serde_json::from_slice(&run(&["show", id, "--tail", "1", "--json"])).unwrap();
    assert_eq!(windowed["turns"][0]["ordinal"], last);
    assert_eq!(windowed["turns"][0]["text"], turns[last]["text"]);
    assert_eq!(
        windowed["truncation"]["window"]["ordinals"],
        serde_json::json!({ "first": last, "last": last })
    );

    let again: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    assert_eq!(
        again["turns"], whole["turns"],
        "ordinals are stable across reads"
    );

    let human = String::from_utf8(run(&["show", id])).unwrap();
    assert!(human.contains("[user #0 "), "{human}");
    assert!(human.contains(&format!(" #{last} ")), "{human}");

    let listed: Value =
        serde_json::from_slice(&run(&["list", "--global", "--harness", "codex", "--json"]))
            .unwrap();
    assert_eq!(session(&listed, id)["store"], whole["session"]["store"]);

    fs::remove_dir_all(codex_home).unwrap();
}

/// `show --tail 1` on a session whose whole message projection exceeds the
/// transport bound returns the one turn, names the page bound, and never asks
/// for the unpaged projection.
#[test]
fn show_tail_on_an_oversized_opencode_session_stays_bounded() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-oversized-{}",
        std::process::id()
    )));
    let program = opencode_program(root.path(), "opencode2");
    let id = "ses_oversized_fixture";
    let run = |args: &[&str]| {
        let output = tapes()
            .args(args)
            .env("HOME", root.path().join("home"))
            .env("PATH", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value =
        serde_json::from_slice(&run(&["show", id, "--tail", "1", "--json"])).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(value["truncated"], true);
    assert!(value["truncation"].get("source").is_none(), "{value}");
    assert_eq!(
        value["truncation"]["window"],
        serde_json::json!({
            "returned": 1, "omitted": 7, "omitted_from": "head", "bound": 1,
            "ordinals": { "first": 7, "last": 7 }, "omitted_exact": false
        })
    );

    let human = String::from_utf8(run(&["show", id, "--tail", "1"])).unwrap();
    assert!(
        human.contains("Showing the last 1 of at least 8 turns; the read stopped once the 1-turn window was full"),
        "{human}"
    );
    assert!(!human.contains("were fetched from the store"), "{human}");

    let calls = fs::read_to_string(format!("{}.calls", program.path().display())).unwrap();
    assert!(
        calls
            .lines()
            .filter(|line| line.contains("/message"))
            .all(|line| line.contains("limit=")),
        "{calls}"
    );
}

/// A window wide enough to reach a message the transport cannot carry still
/// renders every turn before it, and the human output names where and why the
/// read stopped.
#[test]
fn show_renders_what_precedes_a_message_the_transport_cannot_carry() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-oversized-giant-{}",
        std::process::id()
    )));
    let _program = opencode_program(root.path(), "opencode2");
    let output = tapes()
        .args(["show", "ses_giant_message_fixture", "--tail", "120"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("[assistant #99 "), "{text}");
    assert!(
        text.contains("Only the newest 100 messages were fetched from the store"),
        "{text}"
    );
    assert!(
        text.contains("is larger than the 8 MiB transport bound; the read stopped before it"),
        "{text}"
    );
}

/// The normalized totals reach every surface unchanged: list, show, and the
/// export bundle carry the same counters, with absent ones omitted.
#[test]
fn codex_token_totals_agree_across_list_show_and_export() {
    let (codex_home, home) = fixture_store("codex-tokens");
    let id = "00000000-0000-0000-0000-000000000001";
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let expected = serde_json::json!({
        "input": 1200,
        "output": 300,
        "cache_read": 1000,
        "cache_write": 0
    });
    let expected_accounting = serde_json::json!({
        "basis": "recorded-total",
        "coverage": "session"
    });

    let shown: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    assert_eq!(shown["session"]["tokens"], expected);
    assert_eq!(shown["session"]["accounting"], expected_accounting);
    assert!(shown["session"].get("cost").is_none());

    let listed: Value =
        serde_json::from_slice(&run(&["list", "--global", "--harness", "codex", "--json"]))
            .unwrap();
    assert_eq!(session(&listed, id)["tokens"], expected);
    assert_eq!(session(&listed, id)["accounting"], expected_accounting);

    let bundle = codex_home.join("bundle");
    let listing =
        String::from_utf8(run(&["export", id, "--bundle", bundle.to_str().unwrap()])).unwrap();
    let json_path = listing
        .lines()
        .find(|line| line.contains(".json"))
        .and_then(|line| line.split('\t').next())
        .unwrap();
    let exported: Value = serde_json::from_str(&fs::read_to_string(json_path).unwrap()).unwrap();
    assert_eq!(exported["session"]["tokens"], expected);
    assert_eq!(exported["session"]["accounting"], expected_accounting);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_sort_oldest_keeps_the_oldest_sessions_under_the_limit() {
    let (codex_home, home) = filter_fixture_store("sort-oldest-limit");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--sort",
        "oldest",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["sessions"][0]["id"],
        "00000000-0000-0000-0000-000000000001"
    );

    fs::remove_dir_all(codex_home).unwrap();
}

const CLAUDE_SESSION: &str = include_str!("../fixtures/claude/project/session-claude.jsonl");
const PI_SESSION: &str = include_str!("../fixtures/pi/2026-01-01T10-00-00-000Z_session-pi.jsonl");
const CLAUDE_SUBAGENT: &str =
    include_str!("../fixtures/claude/project/session-claude/subagents/agent-fixture.jsonl");
const CLAUDE_SUBAGENT_META: &str =
    include_str!("../fixtures/claude/project/session-claude/subagents/agent-fixture.meta.json");

/// Every turn says what it is, and a user turn the harness recorded its own
/// command in says so in the heading a reader judges an ending by.
#[test]
fn show_types_every_turn_and_names_a_control_turn_in_human_output() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-turn-kinds-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    let pi_sessions = home.join(".pi/agent/sessions");
    fs::create_dir_all(&pi_sessions).unwrap();
    fs::write(
        pi_sessions.join("2026-01-01T10-00-00-000Z_session-pi.jsonl"),
        PI_SESSION,
    )
    .unwrap();
    let codex_home = root.path().join("codex");
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    fs::write(
        codex_sessions
            .join("rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
        CODEX_SESSION_ONE,
    )
    .unwrap();

    let _opencode = opencode_program(root.path(), "opencode2");

    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(&mut command, &codex_home, &home, root.path());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    for id in [
        "session-claude",
        "session-pi",
        "00000000-0000-0000-0000-000000000001",
        "ses_000000fixtureSharedSession",
    ] {
        let value: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
        let turns = value["turns"].as_array().unwrap();
        assert!(!turns.is_empty(), "{id} has no turns");
        for turn in turns {
            let kind = turn["kind"].as_str().unwrap_or_else(|| panic!("{turn}"));
            assert!(
                [
                    "operator",
                    "assistant",
                    "reasoning",
                    "tool",
                    "control",
                    "ambient",
                    "notice",
                    "unknown"
                ]
                .contains(&kind),
                "{id} carries an unnamed kind: {turn}"
            );
            let role = turn["role"].as_str().unwrap();
            if role != "user" {
                assert_eq!(kind, role, "{id}: {turn}");
            }
        }
    }

    let human = String::from_utf8(run(&["show", "session-claude"])).unwrap();
    assert!(human.contains("[user/control #10 "), "{human}");
    assert!(human.contains("[user/notice #12 "), "{human}");
    assert!(human.contains("[user #5 "), "{human}");
}

#[test]
fn lineage_help_names_the_schema_and_the_rule_it_follows() {
    let output = tapes().args(["lineage", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("tapes-lineage/1"), "{help}");
    assert!(
        help.contains("A relationship exists only where a record states it"),
        "{help}"
    );
    assert!(help.contains("referred to, never"), "{help}");
}

/// The command reports the relationships a store recorded, keeps a reference
/// the store cannot resolve, and never reads a child's transcript.
#[test]
fn lineage_reports_a_subagent_reference_and_an_unresolved_parent() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-lineage-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    let subagents = project.join("session-claude/subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    fs::write(subagents.join("agent-fixture.jsonl"), CLAUDE_SUBAGENT).unwrap();
    fs::write(
        subagents.join("agent-fixture.meta.json"),
        CLAUDE_SUBAGENT_META,
    )
    .unwrap();
    let pi_sessions = home.join(".pi/agent/sessions");
    fs::create_dir_all(&pi_sessions).unwrap();
    fs::write(
        pi_sessions.join("2026-01-01T10-00-00-000Z_session-pi.jsonl"),
        PI_SESSION,
    )
    .unwrap();
    let codex_home = root.path().join("codex");
    fs::create_dir_all(codex_home.join("sessions")).unwrap();

    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value =
        serde_json::from_slice(&run(&["lineage", "session-claude", "--json"])).unwrap();
    assert_eq!(value["schema"], "tapes-lineage/1");
    assert_eq!(value["session"]["id"], "session-claude");
    let children = value["lineage"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "{value}");
    assert_eq!(children[0]["reference"], "fixture");
    assert_eq!(children[0]["role"], "Explore");
    assert_eq!(children[0]["disposition"], "completed");
    assert_eq!(children[0]["resolved"], true);
    assert!(value["lineage"].get("parent").is_none(), "{value}");
    // The parent refers to the child; nothing the child recorded is in here.
    assert!(
        !String::from_utf8(run(&["lineage", "session-claude", "--json"]))
            .unwrap()
            .contains("Subagent fixture."),
        "{value}"
    );

    let human = String::from_utf8(run(&["lineage", "session-claude"])).unwrap();
    assert!(human.contains("child: Explore fixture"), "{human}");
    assert!(human.contains("status completed"), "{human}");

    let value: Value = serde_json::from_slice(&run(&["lineage", "session-pi", "--json"])).unwrap();
    assert_eq!(value["lineage"]["parent"]["native_id"], "session-pi-parent");
    assert_eq!(value["lineage"]["parent"]["resolved"], false);
    assert_eq!(value["lineage"]["children"].as_array().unwrap().len(), 0);

    let human = String::from_utf8(run(&["lineage", "session-pi"])).unwrap();
    assert!(human.contains("parent: session-pi-parent"), "{human}");
    assert!(human.contains("unresolved"), "{human}");
}

#[test]
fn usage_help_names_the_schema_and_what_the_figures_mean() {
    let output = tapes().args(["usage", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("tapes-usage/1"), "{help}");
    assert!(
        help.contains("basis and coverage decide whether figures may be summed"),
        "{help}"
    );
    assert!(help.contains("quota is a separate fact"), "{help}");
    assert!(help.contains("tapes-usage-summary/1"), "{help}");
    assert!(help.contains("--by"), "{help}");
}

/// Neither form was asked for, so the error names both rather than guessing
/// which one was meant.
#[test]
fn usage_without_a_session_or_a_selection_names_both_forms() {
    let output = tapes().arg("usage").output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("--latest"), "{error}");
    assert!(error.contains("--global"), "{error}");
}

/// The usage view counts the same normalized turns `show` renders, by role.
#[test]
fn usage_json_reports_recorded_facts_and_turn_counts_show_agrees_with() {
    let id = "00000000-0000-0000-0000-000000000001";
    let output = fixture_command("usage-json", &["usage", id, "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-usage/1");
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["session"]["harness"], "codex");
    assert_eq!(
        value["tokens"],
        serde_json::json!({
            "input": 1200,
            "output": 300,
            "cache_read": 1000,
            "cache_write": 0
        })
    );
    assert_eq!(
        value["accounting"],
        serde_json::json!({ "basis": "recorded-total", "coverage": "session" })
    );
    assert_eq!(value["context_window"], 272_000);
    assert_eq!(value["rate_limits"]["primary"]["used_percent"], 12.0);
    assert_eq!(value["rate_limits"]["secondary"]["window_minutes"], 10_080);
    assert_eq!(
        value["rate_limits"]["primary"]["resets_at"],
        "2026-01-01T14:00:00Z"
    );
    assert_eq!(value["rate_limits"]["plan"], "fixture");
    for absent in ["cost", "durations_ms", "by_model", "truncation"] {
        assert!(value.get(absent).is_none(), "{absent} in {value}");
    }

    let shown = fixture_command("usage-show", &["show", id, "--json"]);
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let turns = shown["turns"].as_array().unwrap();
    let counted = |role: &str| turns.iter().filter(|turn| turn["role"] == role).count();
    assert_eq!(value["turns"]["user"], counted("user"));
    assert_eq!(value["turns"]["assistant"], counted("assistant"));
    assert_eq!(value["turns"]["tool"], counted("tool"));
    assert_eq!(value["turns"]["reasoning"], counted("reasoning"));
    assert_eq!(value["turns"]["total"], turns.len());
    assert_eq!(value["turns"]["coverage"], "session");
}

/// Human output states each recorded fact once and invents no line for a
/// fact the harness did not record.
#[test]
fn usage_human_output_names_the_recorded_facts_only() {
    let id = "00000000-0000-0000-0000-000000000001";
    let output = fixture_command("usage-human", &["usage", id]);
    let rendered = String::from_utf8(output.stdout).unwrap();

    assert!(
        rendered.starts_with(
            "session: codex 00000000-0000-0000-0000-000000000001 gpt-fixture (high)\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("tokens: input 1200, output 300, cache read 1000, cache write 0\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("accounting: a recorded total, covering the whole session\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "turns: 6 total, 1 user, 1 assistant, 3 tool, 1 reasoning, covering the whole session\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("context window: 272000 tokens\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "rate limit primary: 12% used of a 300-minute window, resets 2026-01-01T14:00:00Z\n"
        ),
        "{rendered}"
    );
    assert!(rendered.contains("plan: fixture\n"), "{rendered}");
    for absent in ["cost:", "durations:", "model ", "Note:"] {
        assert!(!rendered.contains(absent), "{absent} in {rendered}");
    }
}

/// The summary sums the same counters the listing reports per session, and
/// `counted` says how many sessions were behind each sum.
#[test]
fn usage_summary_sums_the_listings_own_counters_and_counts_the_sessions_behind_them() {
    let (codex_home, home) = filter_fixture_store("usage-summary");
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

    let listed = command(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let counted_session = session(&listed, "00000000-0000-0000-0000-000000000001");

    let output = command(&[
        "usage",
        "--global",
        "--harness",
        "codex",
        "--by",
        "harness,model",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-usage-summary/1");
    assert_eq!(value["selection"]["scope"], "global");
    assert_eq!(value["selection"]["harness"], "codex");
    assert_eq!(value["scanned"], 4);
    assert_eq!(value["scan_truncated"], false);

    let groups = value["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    // A session that recorded no model is keyed by that absence.
    assert_eq!(groups[0]["key"], serde_json::json!({ "harness": "codex" }));
    assert_eq!(groups[0]["sessions"], 1);
    assert!(groups[0].get("tokens").is_none(), "{value}");

    let gpt = &groups[1];
    assert_eq!(
        gpt["key"],
        serde_json::json!({ "harness": "codex", "model": "gpt-fixture" })
    );
    assert_eq!(gpt["sessions"], 2);
    assert_eq!(gpt["tokens"], counted_session["tokens"]);
    assert_eq!(
        gpt["counted"],
        serde_json::json!({
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache_read": 1,
            "cache_write": 1,
            "cost": 0
        })
    );
    assert_eq!(
        gpt["coverage"],
        serde_json::json!({
            "recorded_total": 1,
            "summed_session": 0,
            "summed_read_window": 0,
            "no_accounting": 1
        })
    );
    assert!(gpt.get("cost").is_none(), "{value}");

    assert_eq!(value["totals"]["sessions"], 4);
    assert_eq!(value["totals"]["tokens"], counted_session["tokens"]);
    assert_eq!(value["totals"]["counted"]["input"], 1);
    assert_eq!(value["totals"]["coverage"]["no_accounting"], 3);

    fs::remove_dir_all(codex_home).unwrap();
}

/// The activity window narrows the summed set the same way it narrows a
/// listing, and sessions whose harness recorded no counters are reported as
/// such rather than as zeroes.
#[test]
fn usage_summary_narrows_by_activity_and_reports_sessions_with_no_accounting() {
    let (codex_home, home) = filter_fixture_store("usage-window");
    let mut command = tapes();
    command.args([
        "usage",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T12:00:00Z",
        "--by",
        "model",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(
        value["selection"]["activity"],
        serde_json::json!({ "since": "2026-01-01T12:00:00Z" })
    );
    let groups = value["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["key"], serde_json::json!({}));
    assert_eq!(
        groups[1]["key"],
        serde_json::json!({ "model": "other-fixture" })
    );
    for group in groups {
        assert!(group.get("tokens").is_none(), "{group}");
        assert!(group.get("cost").is_none(), "{group}");
        assert_eq!(group["coverage"]["no_accounting"], 1);
    }
    assert_eq!(value["totals"]["sessions"], 2);
    assert_eq!(value["totals"]["coverage"]["no_accounting"], 2);
    assert!(value["totals"].get("tokens").is_none(), "{value}");

    fs::remove_dir_all(codex_home).unwrap();
}

/// The table prints one row per group and a closing total, and a sum drawn
/// from fewer sessions than the row holds says so.
#[test]
fn usage_summary_human_output_says_how_many_sessions_are_behind_each_sum() {
    let (codex_home, home) = filter_fixture_store("usage-table");
    let mut command = tapes();
    command.args(["usage", "--global", "--harness", "codex"]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    let rendered = String::from_utf8(output.stdout).unwrap();
    let lines = rendered.lines().collect::<Vec<_>>();

    assert_eq!(
        lines[0],
        "HARNESS\tMODEL\tSESSIONS\tINPUT\tOUTPUT\tREASONING\tCACHE READ\tCACHE WRITE\tCOST\t\
         COVERAGE (RECORDED/SUMMED/WINDOW/NONE)"
    );
    assert_eq!(
        lines[2],
        "codex\tgpt-fixture\t2\t1200 (1 of 2)\t300 (1 of 2)\t\t1000 (1 of 2)\t0 (1 of 2)\t\t1/0/0/1"
    );
    assert_eq!(
        lines[4],
        "TOTAL\t\t4\t1200 (1 of 4)\t300 (1 of 4)\t\t1000 (1 of 4)\t0 (1 of 4)\t\t1/0/0/3"
    );

    fs::remove_dir_all(codex_home).unwrap();
}

/// The selection is exactly what `list` returns for the same flags, in the
/// same order, and each session keeps its own bounded bundle.
#[test]
fn export_over_an_activity_window_writes_one_bundle_per_session_and_a_manifest() {
    let (codex_home, home) = filter_fixture_store("export-window");
    let bundle = codex_home.join("bundle");
    let mut command = tapes();
    command.args([
        "export",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T11:00:00Z",
        "--until",
        "2026-01-01T14:00:00Z",
        "--sort",
        "oldest",
        "--bundle",
    ]);
    command.arg(&bundle);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema"], "tapes-export-manifest/1");
    assert_eq!(
        manifest["selection"],
        serde_json::json!({
            "scope": "global",
            "harness": "codex",
            "activity": {
                "since": "2026-01-01T11:00:00Z",
                "until": "2026-01-01T14:00:00Z"
            },
            "sort": "oldest",
            "limit": 20
        })
    );
    let exported = manifest["sessions"].as_array().unwrap();
    assert_eq!(
        exported
            .iter()
            .map(|session| session["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "10000000-0000-0000-0000-000000000002",
            "20000000-0000-0000-0000-000000000004"
        ]
    );
    assert!(manifest["failed"].as_array().unwrap().is_empty());
    assert_eq!(manifest["scan_truncated"], false);

    // Two sessions exported in one run keep separate bundles: the stem carries
    // the session id, so sharing an export second cannot collide.
    let mut stdout_paths = Vec::new();
    for session in exported {
        for kind in ["context", "json", "trace"] {
            let path = PathBuf::from(session["files"][kind].as_str().unwrap());
            assert!(path.is_file(), "{} is missing", path.display());
            stdout_paths.push(path);
        }
    }
    assert_eq!(fs::read_dir(&bundle).unwrap().count(), 7);

    let stdout = String::from_utf8(output.stdout).unwrap();
    let printed = stdout
        .lines()
        .map(|line| PathBuf::from(line.split('\t').next().unwrap()))
        .collect::<Vec<_>>();
    stdout_paths.push(bundle.join("manifest.json"));
    assert_eq!(printed, stdout_paths);

    fs::remove_dir_all(codex_home).unwrap();
}

/// A limit bounds the selection the way it bounds a listing, and a selection
/// that holds nothing is a fact rather than a failure.
#[test]
fn export_honours_the_limit_and_writes_a_manifest_for_an_empty_selection() {
    let (codex_home, home) = filter_fixture_store("export-limit");
    let run = |arguments: &[&str], bundle: &Path| {
        let mut command = tapes();
        command.args(arguments).arg("--bundle").arg(bundle);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        command.output().unwrap()
    };

    let limited_bundle = codex_home.join("limited");
    let limited = run(
        &["export", "--global", "--harness", "codex", "--limit", "1"],
        &limited_bundle,
    );
    assert!(
        limited.status.success(),
        "{}",
        String::from_utf8_lossy(&limited.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(limited_bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["selection"]["limit"], 1);
    assert_eq!(
        manifest["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["30000000-0000-0000-0000-000000000005"]
    );
    assert_eq!(fs::read_dir(&limited_bundle).unwrap().count(), 4);

    let empty_bundle = codex_home.join("empty");
    let empty = run(
        &[
            "export",
            "--global",
            "--harness",
            "codex",
            "--since",
            "2030-01-01",
        ],
        &empty_bundle,
    );
    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(empty_bundle.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["sessions"].as_array().unwrap().is_empty());
    assert!(manifest["failed"].as_array().unwrap().is_empty());
    assert_eq!(fs::read_dir(&empty_bundle).unwrap().count(), 1);
    let stdout = String::from_utf8(empty.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout}");

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn endings_help_names_the_schema_and_what_it_does_not_do() {
    let output = tapes().args(["endings", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(help.contains("tapes-endings/1"), "{help}");
    assert!(help.contains("never on their text"), "{help}");
    assert!(help.contains("labels no session complete"), "{help}");
    assert!(help.contains("--tail <N>"), "{help}");
    assert!(help.contains("[default: 12]"), "{help}");
    assert!(help.contains("--text"), "{help}");
}

/// The activity window chooses the set before a transcript is opened, so the
/// report holds exactly the sessions `list` would return for the same flags.
#[test]
fn endings_applies_the_activity_window_before_reading_any_transcript() {
    let (codex_home, home) = filter_fixture_store("endings-window");
    let mut command = tapes();
    command.args([
        "endings",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T12:00:00Z",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-endings/1");
    assert_eq!(
        value["selection"]["activity"],
        serde_json::json!({ "since": "2026-01-01T12:00:00Z" })
    );
    let endings = value["endings"].as_array().unwrap();
    assert_eq!(
        endings
            .iter()
            .map(|ending| ending["session"]["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "30000000-0000-0000-0000-000000000005",
            "20000000-0000-0000-0000-000000000004"
        ]
    );
    for ending in endings {
        assert_eq!(ending["source"]["schema"], "tapes-endings/1");
        assert_eq!(ending["source"]["harness"], "codex");
        assert_eq!(ending["facts"], serde_json::json!(["assistant-close"]));
        // The structural report carries no transcript text of its own.
        assert!(ending.get("tail").is_none(), "{ending}");
    }
    assert!(value["unread"].as_array().unwrap().is_empty());
    assert_eq!(value["scan_truncated"], false);

    fs::remove_dir_all(codex_home).unwrap();
}

/// The text tail is a glimpse of the exchange: an entry longer than the bound
/// is cut and says so, and the human render prints it beneath its session.
#[test]
fn endings_text_tail_is_bounded_and_marks_what_it_cut() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-endings-text-{}", std::process::id())),
    );
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let codex_home = root.path().join("codex");
    let sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "40000000-0000-0000-0000-000000000006";
    let long = "word ".repeat(200);
    let long = long.trim_end();
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        format!(
            r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}
{{"timestamp":"2026-01-01T10:00:01Z","type":"turn_context","payload":{{"cwd":"/fixtures/project","model":"gpt-fixture"}}}}
{{"timestamp":"2026-01-01T10:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Inspect the fixture."}}]}}}}
{{"timestamp":"2026-01-01T10:00:02.500Z","type":"event_msg","payload":{{"type":"user_message","message":"Inspect the fixture."}}}}
{{"timestamp":"2026-01-01T10:00:03Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{long}"}}]}}}}
"#
        ),
    )
    .unwrap();

    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value =
        serde_json::from_slice(&run(&["endings", "--global", "--text", "--json"])).unwrap();
    let tail = value["endings"][0]["tail"].as_array().unwrap();
    assert_eq!(tail.len(), 2, "{tail:#?}");
    assert_eq!(tail[0]["kind"], "operator");
    assert_eq!(tail[0]["text"], "Inspect the fixture.");
    assert_eq!(tail[0]["truncated"], false);
    assert_eq!(tail[1]["kind"], "assistant");
    assert_eq!(tail[1]["text"].as_str().unwrap().chars().count(), 400);
    assert_eq!(tail[1]["truncated"], true);

    let human = String::from_utf8(run(&["endings", "--global", "--text"])).unwrap();
    let lines = human.lines().collect::<Vec<_>>();
    assert!(
        lines[0].starts_with(&format!(
            "{id} codex 2026-01-01T10:00:03Z assistant assistant-close"
        )),
        "{human}"
    );
    assert_eq!(lines[1], "  [user #0 2026-01-01T10:00:02Z]");
    assert_eq!(lines[2], "    Inspect the fixture.");
    assert_eq!(
        lines[3],
        "  [assistant #1 2026-01-01T10:00:03Z cut at 400 characters]"
    );

    let without_text = String::from_utf8(run(&["endings", "--global"])).unwrap();
    assert!(
        !without_text
            .lines()
            .any(|line| line.starts_with("  [") || line.starts_with("    ")),
        "{without_text}"
    );
}

const CODEX_SESSION_TITLE: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T12-00-00-10000000-0000-0000-0000-000000000003.jsonl"
);

/// A recording whose every counted figure is chosen: a call repeated under
/// one id, a result whose call is outside the pair, a tool the harness
/// recorded an error on, two complete pairs with known durations, recorded
/// token counters, and one child.
const STATS_PARENT: &str = concat!(
    r#"{"timestamp":"2026-02-02T09:00:00Z","type":"session_meta","payload":{"id":"44444444-0000-0000-0000-000000000001","cwd":"/fixtures/project"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:01Z","type":"turn_context","payload":{"cwd":"/fixtures/project","model":"gpt-fixture","effort":"high"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Measure the fixture."}]}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:02.500Z","type":"event_msg","payload":{"type":"user_message","message":"Measure the fixture."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:03Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Consider the fixture."}]}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:04Z","type":"response_item","payload":{"type":"function_call","name":"read","arguments":"{}","call_id":"call-a"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:05Z","type":"response_item","payload":{"type":"function_call","name":"read","arguments":"{}","call_id":"call-a"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:07Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-a","output":"Read complete."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:09Z","type":"response_item","payload":{"type":"custom_tool_call","name":"probe","input":"probe fixture","call_id":"call-b","status":"error"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:11Z","type":"response_item","payload":{"type":"custom_tool_call","name":"probe","input":"probe fixture","call_id":"call-c","status":"completed"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:12Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-c","output":"Probe complete."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:13Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","arguments":"{\"task_name\":\"explore\",\"model\":\"gpt-fixture\"}","call_id":"call-d"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:14Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-d","output":"{\"task_name\":\"/root/explore\",\"agents\":[{\"agent_name\":\"/root/explore\",\"agent_status\":\"completed\"}]}"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:15Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-z","output":"Orphan result."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:16Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixture measured."}]}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:16.500Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2500,"cached_input_tokens":1000,"cache_write_input_tokens":500,"output_tokens":400,"total_tokens":4400},"model_context_window":272000},"rate_limits":null}}"#,
    "\n",
);

const STATS_CHILD: &str = concat!(
    r#"{"timestamp":"2026-02-02T09:00:13Z","type":"session_meta","payload":{"id":"44444444-0000-0000-0000-000000000002","cwd":"/fixtures/project","parent_thread_id":"44444444-0000-0000-0000-000000000001","agent_path":"/root/explore","agent_nickname":"explorer"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:14Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Explored."}]}}"#,
    "\n",
);

fn stats_store(name: &str) -> TemporaryDirectory {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-stats-{name}-{}", std::process::id())),
    );
    let sessions = root.path().join("codex/sessions/2026/02/02");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(root.path().join("home")).unwrap();
    fs::write(
        sessions.join("rollout-2026-02-02T09-00-00-44444444-0000-0000-0000-000000000001.jsonl"),
        STATS_PARENT,
    )
    .unwrap();
    fs::write(
        sessions.join("rollout-2026-02-02T09-00-13-44444444-0000-0000-0000-000000000002.jsonl"),
        STATS_CHILD,
    )
    .unwrap();
    root
}

fn stats_command(root: &Path, arguments: &[&str]) -> std::process::Output {
    let mut command = tapes();
    command.args(arguments);
    with_fixture_env(
        &mut command,
        &root.join("codex"),
        &root.join("home"),
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn stats_help_names_the_schema_and_what_the_figures_cover() {
    let output = tapes().args(["stats", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("tapes-stats/1"), "{help}");
    assert!(help.contains("complete pairs only"), "{help}");
    assert!(
        help.contains("share of recorded token counts rather than of cost"),
        "{help}"
    );
    assert!(help.contains("Nothing is judged"), "{help}");
}

/// Every figure in the answer is a figure the recording chose, so the whole
/// object is asserted rather than a sample of it.
#[test]
fn stats_json_counts_a_chosen_recording_exactly() {
    let root = stats_store("exact");
    let id = "44444444-0000-0000-0000-000000000001";
    let output = stats_command(root.path(), &["stats", id, "--json"]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(
        value,
        serde_json::json!({
            "schema": "tapes-stats/1",
            "session": {
                "id": id,
                "harness": "codex",
                "model": { "id": "gpt-fixture", "variant": "high" },
                "started_at": "2026-02-02T09:00:00Z",
                "last_activity_at": "2026-02-02T09:00:16.500Z",
                "directory": "/fixtures/project",
                "store": root
                    .path()
                    .join("codex/sessions/2026/02/02/rollout-2026-02-02T09-00-00-44444444-0000-0000-0000-000000000001.jsonl")
                    .display()
                    .to_string()
            },
            "coverage": { "turns": "session", "pairs": "complete-only" },
            "turns": {
                "operator": 1,
                "assistant": 1,
                "tool": 9,
                "reasoning": 1,
                "control": 0,
                "ambient": 0,
                "notice": 0,
                "unknown": 0,
                "total": 12
            },
            "tools": {
                "calls": 5,
                "results": 4,
                "paired": 3,
                "incomplete": {
                    "no-result-in-read": 2,
                    "call-before-read-bound": 0,
                    "call-not-recorded": 1
                },
                "by_name": [
                    {
                        "name": "probe",
                        "calls": 2,
                        "paired": 1,
                        "errors": 1,
                        "duration_ms": { "total": 1_000, "max": 1_000, "count": 1 }
                    },
                    {
                        "name": "read",
                        "calls": 2,
                        "paired": 1,
                        "errors": 0,
                        "duration_ms": { "total": 2_000, "max": 2_000, "count": 1 }
                    },
                    {
                        "name": "spawn_agent",
                        "calls": 1,
                        "paired": 1,
                        "errors": 0,
                        "duration_ms": { "total": 1_000, "max": 1_000, "count": 1 }
                    }
                ],
                "errors": 1
            },
            "durations_ms": {
                "recorded_span": 14_000,
                "in_tool": 4_000,
                "between_turns_max": 2_000,
                "count_with_timestamps": 12
            },
            "usage": {
                "tokens": {
                    "input": 2500,
                    "output": 400,
                    "cache_read": 1000,
                    "cache_write": 500
                },
                "accounting": { "basis": "recorded-total", "coverage": "session" },
                "cache_read_ratio": 0.25,
                "cache_write_ratio": 0.125
            },
            "lineage": {
                "children": 1,
                "resolved": 1,
                "by_disposition": { "completed": 1 }
            },
            "warnings": ["incomplete-pairs"]
        }),
        "{value:#}"
    );
}

/// The counts are the `events` projection's own, so the two commands answer
/// the same question about the same read.
#[test]
fn stats_tool_counts_are_the_events_projections_counts() {
    let root = stats_store("events");
    let id = "44444444-0000-0000-0000-000000000001";
    let stats: Value =
        serde_json::from_slice(&stats_command(root.path(), &["stats", id, "--json"]).stdout)
            .unwrap();
    let events: Value =
        serde_json::from_slice(&stats_command(root.path(), &["events", id, "--json"]).stdout)
            .unwrap();

    let records = events["events"].as_array().unwrap();
    let counted = |kind: &str| {
        records
            .iter()
            .filter(|record| record["kind"] == kind)
            .count()
    };
    assert_eq!(stats["tools"]["calls"], counted("tool-call"));
    assert_eq!(stats["tools"]["results"], counted("tool-result"));
    assert_eq!(stats["tools"]["paired"], events["pairs"]["complete"]);
    let incomplete = |reason: &str| {
        records
            .iter()
            .filter(|record| record["incomplete"] == reason)
            .count()
    };
    for reason in [
        "no-result-in-read",
        "call-before-read-bound",
        "call-not-recorded",
    ] {
        assert_eq!(
            stats["tools"]["incomplete"][reason],
            incomplete(reason),
            "{reason}"
        );
    }
    // The tool distribution comes from the typed events rather than from the
    // harness envelope each tool turn keeps as its text.
    let named = |name: &str| {
        records
            .iter()
            .filter(|record| record["kind"] == "tool-call" && record["name"] == name)
            .count()
    };
    for tool in stats["tools"]["by_name"].as_array().unwrap() {
        assert_eq!(
            tool["calls"],
            named(tool["name"].as_str().unwrap()),
            "{tool}"
        );
    }
}

/// A read that stopped at a source bound counted only what it reached, and
/// says so in the coverage and in a warning.
#[test]
fn stats_of_an_oversized_recording_reports_the_read_window() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-stats-oversized-{}", std::process::id())),
    );
    let sessions = root.path().join("codex/sessions/2026/02/02");
    fs::create_dir_all(&sessions).unwrap();
    let id = "44444444-0000-0000-0000-0000000000ff";
    let mut file = std::io::BufWriter::new(
        fs::File::create(sessions.join(format!("rollout-2026-02-02T09-00-00-{id}.jsonl"))).unwrap(),
    );
    use std::io::Write;
    writeln!(
        file,
        r#"{{"timestamp":"2026-02-02T09:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for _ in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-02-02T10:00:00Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let value: Value =
        serde_json::from_slice(&stats_command(root.path(), &["stats", id, "--json"]).stdout)
            .unwrap();
    assert_eq!(value["coverage"]["turns"], "read-window");
    assert_eq!(
        value["coverage"]["truncation"]["source"],
        serde_json::json!([{ "kind": "file-tail", "bytes": 4_194_304 }])
    );
    assert_eq!(value["warnings"], serde_json::json!(["read-window"]));

    let human = String::from_utf8(stats_command(root.path(), &["stats", id]).stdout).unwrap();
    assert!(
        human.contains("coverage: turns cover the bounded read window"),
        "{human}"
    );
    assert!(human.contains("warnings: read-window"), "{human}");
    assert!(
        human.contains("Only the final 4 MiB of the recording was read"),
        "{human}"
    );
}

/// Human output states each group once and invents no line for a group the
/// recording holds nothing for.
#[test]
fn stats_human_output_prints_no_absent_group() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-stats-human-{}", std::process::id())),
    );
    let sessions = root.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T12-00-00-10000000-0000-0000-0000-000000000003.jsonl"),
        CODEX_SESSION_TITLE,
    )
    .unwrap();

    let rendered = String::from_utf8(
        stats_command(
            root.path(),
            &["stats", "10000000-0000-0000-0000-000000000003"],
        )
        .stdout,
    )
    .unwrap();

    assert!(
        rendered.starts_with(
            "session: codex 10000000-0000-0000-0000-000000000003 gpt-fixture (high)\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "coverage: turns cover the whole session, durations cover complete pairs only\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("turns: 3 total, 1 operator, 1 assistant, 1 ambient\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("tools: 0 calls, 0 results, 0 paired, 0 errors\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("durations: recorded span 2000ms, longest gap between turns 1000ms, 3 turns with timestamps\n"),
        "{rendered}"
    );
    for absent in [
        "incomplete:",
        "NAME\tCALLS",
        "tokens:",
        "cost:",
        "accounting:",
        "cache of input",
        "children:",
        "warnings:",
        "Note:",
    ] {
        assert!(!rendered.contains(absent), "{absent} in {rendered}");
    }
}

/// The stats read is the export-shaped one, so its turn counts are the whole
/// bounded read's and agree with what `usage` counts by role.
#[test]
fn stats_and_usage_count_the_same_bounded_read() {
    let root = stats_store("usage");
    let id = "44444444-0000-0000-0000-000000000001";
    let stats: Value =
        serde_json::from_slice(&stats_command(root.path(), &["stats", id, "--json"]).stdout)
            .unwrap();
    let usage: Value =
        serde_json::from_slice(&stats_command(root.path(), &["usage", id, "--json"]).stdout)
            .unwrap();

    assert_eq!(stats["turns"]["total"], usage["turns"]["total"]);
    assert_eq!(stats["turns"]["tool"], usage["turns"]["tool"]);
    assert_eq!(stats["turns"]["assistant"], usage["turns"]["assistant"]);
    assert_eq!(stats["coverage"]["turns"], usage["turns"]["coverage"]);
    assert_eq!(stats["usage"]["tokens"], usage["tokens"]);
    assert_eq!(stats["usage"]["accounting"], usage["accounting"]);
}

#[test]
fn brief_help_names_the_schema_and_the_half_it_reads() {
    let output = tapes().args(["brief", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(help.contains("tapes-brief/1"), "{help}");
    assert!(help.contains("reads the recording alone"), "{help}");
    assert!(help.contains("--tail <N>"), "{help}");
    assert!(help.contains("[default: 12]"), "{help}");
}

/// The human brief is one screen in a fixed reading order: which session this
/// is, where it worked, where it stopped, what it left open, what it last
/// said.
#[test]
fn brief_renders_the_continuation_in_reading_order() {
    let (codex_home, home) = fixture_store("brief-order");
    let id = "00000000-0000-0000-0000-000000000001";
    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let human = String::from_utf8(run(&["brief", id])).unwrap();
    let lines = human.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[0],
        format!("session: codex {id} ~Inspect the fixture."),
        "{human}"
    );
    // The recorded directory is not on this machine, which is stated rather
    // than left blank, and nothing about a commit is claimed for it.
    assert_eq!(lines[1], "working set: /fixtures/project (gone)", "{human}");
    assert_eq!(lines[2], "ending: tool call-without-result", "{human}");
    assert_eq!(
        lines[3], "in flight: call fixture_pending call-pending #5 2026-01-01T10:00:06Z",
        "{human}"
    );
    assert_eq!(lines[4], "tail:", "{human}");
    assert_eq!(lines[5], "[user #0 2026-01-01T10:00:02Z]", "{human}");
    assert_eq!(lines[6], "Inspect the fixture.", "{human}");
    assert_eq!(lines[7], "[assistant #4 2026-01-01T10:00:06Z]", "{human}");
    assert_eq!(lines[8], "Fixture inspected.", "{human}");
    assert_eq!(lines.len(), 9, "{human}");

    // The window bounds the rendered exchange alone.
    let narrow = String::from_utf8(run(&["brief", id, "--tail", "1"])).unwrap();
    assert!(!narrow.contains("[user #0"), "{narrow}");
    assert!(narrow.contains("[assistant #4"), "{narrow}");

    let value: Value = serde_json::from_slice(&run(&["brief", id, "--json"])).unwrap();
    assert_eq!(value["schema"], "tapes-brief/1");
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["working_set"]["directory_exists"], false);
    assert!(value["working_set"].get("git").is_none(), "{value}");
    assert_eq!(
        value["ending"]["facts"],
        serde_json::json!(["call-without-result"])
    );
    assert_eq!(
        value["in_flight"]["calls_without_result"][0]["call_id"],
        "call-pending"
    );
    assert_eq!(value["usage"]["tokens"]["input"], 1200);
    assert_eq!(value["tail"].as_array().unwrap().len(), 2);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn selection_stats_agree_with_individual_reads() {
    let (path, home) = fixture_store("stats-summary");
    let root = TemporaryDirectory { path };
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, root.path(), &home, root.path());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let summary = run(&["stats", "--global", "--harness", "codex", "--json"]);
    assert_eq!(summary["schema"], "tapes-stats-summary/1");
    assert_eq!(summary["selected"], 2);
    assert_eq!(summary["read"], 2);
    let mut totals = [0_u64; 4];
    for row in summary["sessions"].as_array().unwrap() {
        let one = run(&["stats", row["session"]["id"].as_str().unwrap(), "--json"]);
        assert_eq!(row["tools"], one["tools"]);
        assert_eq!(row["coverage"], one["coverage"]);
        for (index, key) in ["calls", "results", "paired", "errors"].iter().enumerate() {
            totals[index] += one["tools"][key].as_u64().unwrap();
        }
    }
    for (index, key) in ["calls", "results", "paired", "errors"].iter().enumerate() {
        assert_eq!(summary["by_harness"]["codex"][key], totals[index]);
    }
}

#[test]
fn selection_stats_cli_reports_partial_and_total_read_failure() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-stats-failure-{}", std::process::id())),
    );
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/opencode/opencode2");
    let program = root.path().join("opencode2");
    let calls = root.path().join("calls");
    for (pattern, reads, success) in [
        ("/api/session/ses_api_only_fixture/message*", 1, true),
        ("/api/session/*/message*", 0, false),
    ] {
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$4\" >> '{}'\ncase \"$4\" in {pattern}) exit 7;; esac\nexec '{}' \"$@\"\n", calls.display(),fixture.display());
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = tapes();
        command.args(["stats", "--global", "--harness", "opencode", "--json"]);
        with_fixture_env(
            &mut command,
            &root.path().join("codex"),
            &root.path().join("home"),
            root.path(),
        );
        let output = command.output().unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["selected"], 2);
        assert_eq!(report["read"], reads);
        assert_eq!(report["failed"].as_array().unwrap().len(), 2 - reads);
        assert_eq!(report["sessions"].as_array().unwrap().len(), reads);
    }
    let calls = fs::read_to_string(calls).unwrap();
    assert!(
        !calls.contains("parent"),
        "summary must not request child lineage: {calls}"
    );
}

#[test]
fn recorded_title_selects_every_single_session_view_and_refuses_hidden_ambiguity() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-title-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    fs::create_dir_all(&project).unwrap();
    let body = format!(
        "{}\n{{\"type\":\"ai-title\",\"aiTitle\":\"Exact Title\"}}\n",
        CLAUDE_SESSION
    );
    fs::write(project.join("session-claude.jsonl"), &body).unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
        command.output().unwrap()
    };
    for view in ["show", "brief", "usage", "stats", "lineage", "events"] {
        let output = run(&[
            view,
            "--title",
            "Exact Title",
            "--global",
            "--harness",
            "claude",
            "--json",
        ]);
        assert!(
            output.status.success(),
            "{view}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["session"]["id"], "session-claude", "{view}");
    }
    let output = run(&[
        "export",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
        "--bundle",
        root.path().join("bundles").to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let miss = run(&[
        "show",
        "--title",
        "exact title",
        "--global",
        "--harness",
        "claude",
    ]);
    assert!(!miss.status.success());
    assert!(String::from_utf8_lossy(&miss.stderr).contains("was not found"));
    for i in 0..25 {
        let content = body
            .replace("session-claude", &format!("session-{i}"))
            .replace("Exact Title", &format!("filler-{i}"));
        fs::write(project.join(format!("session-{i}.jsonl")), content).unwrap();
    }
    fs::write(
        project.join("duplicate.jsonl"),
        body.replace("session-claude", "duplicate"),
    )
    .unwrap();
    let ambiguous = run(&[
        "show",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
    ]);
    let diagnostic = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(!ambiguous.status.success());
    assert!(
        diagnostic.contains("ambiguous")
            && diagnostic.contains("duplicate")
            && diagnostic.contains("session-claude"),
        "{diagnostic}"
    );
    fs::remove_file(project.join("duplicate.jsonl")).unwrap();
    fs::write(project.join("unreadable.jsonl"), "{malformed\n").unwrap();
    let unreadable = run(&[
        "show",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
    ]);
    assert!(!unreadable.status.success());
    assert!(String::from_utf8_lossy(&unreadable.stderr).contains("incomplete lookup"));
    fs::remove_file(project.join("unreadable.jsonl")).unwrap();

    let hidden = format!(
        "{}{}\n",
        body.replace("session-claude", "hidden"),
        format!(
            "{{\"type\":\"padding\",\"text\":\"{}\"}}\n",
            "x".repeat(4096)
        )
        .repeat(1100)
    );
    fs::write(project.join("hidden.jsonl"), hidden).unwrap();
    let incomplete = run(&[
        "show",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
    ]);
    let diagnostic = String::from_utf8_lossy(&incomplete.stderr);
    assert!(!incomplete.status.success());
    assert!(
        diagnostic.contains("incomplete lookup") && diagnostic.contains("hidden"),
        "{diagnostic}"
    );
}

#[test]
fn show_preserves_exec_operator_evidence_outside_the_source_tail() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-exec-header-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let id = "40000000-0000-0000-0000-000000000001";
    let header = serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T10:00:00Z",
        "payload":{"id":id,"source":"exec","cwd":"/fixture"}});
    let padding = serde_json::json!({"type":"padding","text":"x".repeat(4096)});
    let prompt = serde_json::json!({"type":"response_item","timestamp":"2026-01-01T10:01:00Z",
        "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"finish the bounded task"}]}});
    let body = format!(
        "{header}\n{}{prompt}\n",
        format!("{padding}\n").repeat(1100)
    );
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    let mut command = tapes();
    command.args(["show", id, "--json"]);
    with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["turns"].as_array().unwrap().len(), 1);
    assert_eq!(view["turns"][0]["kind"], "operator");
    assert_eq!(view["truncation"]["source"][0]["kind"], "file-tail");
}

#[test]
fn lineage_bounds_subagent_metadata_and_reports_the_gap() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-meta-bound-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    let children = project.join("session-claude/subagents");
    fs::create_dir_all(&children).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    fs::write(children.join("agent-fixture.jsonl"), CLAUDE_SUBAGENT).unwrap();
    fs::write(
        children.join("agent-fixture.meta.json"),
        serde_json::json!({"model":"x".repeat(100000)}).to_string(),
    )
    .unwrap();
    let mut command = tapes();
    command.args(["lineage", "session-claude", "--json"]);
    with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
    let output = command.output().unwrap();
    assert!(output.status.success());
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    let child = view["lineage"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["reference"] == "fixture")
        .unwrap();
    assert!(child["model"] == "claude-fixture-sonnet");
    assert!(view["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str().unwrap().contains("exceeds 65536 bytes")));
}
