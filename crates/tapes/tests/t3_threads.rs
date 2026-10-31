//! T3 Code thread references and listings, against a store built with the real
//! `sqlite3` from a synthetic fixture and native recordings in a temporary home.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const ENV: &str = "11111111-2222-4333-8444-555555555555";
const CODEX_ID: &str = "00000000-0000-0000-0000-000000000001";
const HANDOFF_ACTIVE: &str = "10000000-0000-0000-0000-000000000002";
const DELEGATED: &str = "thread:delegated-task:command%3Amcp%3Aaaaa%3Adelegate-task%3Abbbb";
const DELEGATED_WEB: &str =
    "thread%3Adelegated-task%3Acommand%253Amcp%253Aaaaa%253Adelegate-task%253Abbbb";
const SUBAGENT: &str =
    "thread:provider:claudeAgent:native-thread:provider-thread%3Apending%253Arun%3Aagent-fixture";

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    /// `None` when `sqlite3` is absent; the caller skips.
    fn new() -> Option<Self> {
        let present = Command::new("sqlite3")
            .arg("-version")
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !present {
            eprintln!("skipping T3 test: sqlite3 is absent from PATH");
            return None;
        }
        let fixture = Self::without_store();
        fixture.create_store();
        Some(fixture)
    }

    fn without_store() -> Self {
        let root = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let codex = root.path().join("codex/sessions/2026/01/01");
        fs::create_dir_all(&codex).unwrap();
        for name in [
            "rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl",
            "rollout-2026-01-01T11-00-00-10000000-0000-0000-0000-000000000002.jsonl",
        ] {
            fs::copy(fixtures.join("codex").join(name), codex.join(name)).unwrap();
        }
        let pi = root.path().join("pi-sessions");
        fs::create_dir_all(&pi).unwrap();
        let pi_name = "2026-01-01T10-00-00-000Z_session-pi.jsonl";
        fs::copy(fixtures.join("pi").join(pi_name), pi.join(pi_name)).unwrap();
        let claude = root.path().join("claude/projects/project");
        fs::create_dir_all(&claude).unwrap();
        fs::copy(
            fixtures.join("claude/project/session-claude.jsonl"),
            claude.join("session-claude.jsonl"),
        )
        .unwrap();
        fs::create_dir_all(root.path().join("home")).unwrap();
        Self { root }
    }

    fn database(&self) -> PathBuf {
        self.root.path().join("t3/userdata/statev2.sqlite")
    }

    fn create_store(&self) {
        let state = self.root.path().join("t3/userdata");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("environment-id"), format!("{ENV}\n")).unwrap();
        let schema = include_str!("fixtures/t3/statev2.sql").replace(
            "@PI_SESSIONS@",
            self.root.path().join("pi-sessions").to_str().unwrap(),
        );
        let mut sqlite3 = Command::new("sqlite3")
            .arg(self.database())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        sqlite3
            .stdin
            .take()
            .unwrap()
            .write_all(schema.as_bytes())
            .unwrap();
        assert!(sqlite3.wait().unwrap().success());
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tapes"));
        command
            .env("TAPES_OFFLINE", "1")
            .env("T3CODE_HOME", self.root.path().join("t3"))
            .env("HOME", self.root.path().join("home"))
            .env("CODEX_HOME", self.root.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.root.path().join("claude"))
            .env(
                "PI_CODING_AGENT_SESSION_DIR",
                self.root.path().join("pi-sessions"),
            )
            .env("XDG_DATA_HOME", self.root.path().join("xdg"))
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("PI_SESSION_FILE");
        command
    }

    /// `show --json` through a reference: the session id it reached and stderr.
    fn shown(&self, reference: &str) -> (String, String) {
        let output = self.run(&["show", reference, "--tail", "1", "--json"]);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{reference}: {stderr}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (value["session"]["id"].as_str().unwrap().to_owned(), stderr)
    }

    fn refused(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(!output.status.success(), "{args:?} succeeded");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn listed(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

macro_rules! fixture {
    () => {
        match Fixture::new() {
            Some(fixture) => fixture,
            None => return,
        }
    };
}

fn ids(list: &Value) -> Vec<String> {
    let mut ids: Vec<String> = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

/// Every form of a reference reaches the native recording, and says what it
/// became; a form that were not rewritten would fail as an unknown session.
#[test]
fn every_reference_form_reads_the_native_session() {
    let fixture = fixture!();
    for reference in [
        format!("http://192.168.0.2:3773/{ENV}/user-thread"),
        format!("t3-thread://v2/{ENV}/user-thread"),
        "t3-thread://v1/user-thread".to_owned(),
        "user-thread".to_owned(),
    ] {
        let (id, stderr) = fixture.shown(&reference);
        assert_eq!(id, CODEX_ID, "{reference}");
        assert!(
            stderr.contains(&format!(
                "Note: T3 thread user-thread \"Codex thread title\" is codex session {CODEX_ID}."
            )),
            "{stderr}"
        );
    }
}

/// A delegated id holds a literal `%3A`. Skipping the web decode, or decoding
/// the app form, misses the thread.
#[test]
fn a_delegated_thread_resolves_through_both_encodings_to_its_pi_session() {
    let fixture = fixture!();
    for reference in [
        format!("http://h/{ENV}/{DELEGATED_WEB}"),
        format!("t3-thread://v2/{ENV}/{DELEGATED}"),
    ] {
        let (id, _) = fixture.shown(&reference);
        assert_eq!(id, "session-pi", "{reference}");
    }
}

/// A link from another environment names a thread on another machine, even if
/// a thread of that id exists here.
#[test]
fn a_link_from_another_environment_is_refused_naming_both() {
    let fixture = fixture!();
    let other = "99999999-0000-4000-8000-000000000000";
    let stderr = fixture.refused(&["show", &format!("http://h/{other}/user-thread")]);
    assert!(stderr.contains(other) && stderr.contains(ENV), "{stderr}");
}

/// A link is always a reference, while a bare unknown id stays an ordinary
/// argument for the native lookup.
#[test]
fn an_unknown_thread_fails_as_a_link_and_passes_through_bare() {
    let fixture = fixture!();
    let stderr = fixture.refused(&["show", "t3-thread://v1/nope"]);
    assert!(
        stderr.contains("T3 thread nope is not in the T3 store"),
        "{stderr}"
    );
    assert!(stderr.contains("statev2.sqlite"), "{stderr}");
    let stderr = fixture.refused(&["show", "nope"]);
    assert!(!stderr.contains("T3"), "{stderr}");
}

/// A subagent has no session of its own: the error gives the command that
/// reads it, through the parent's Claude session.
#[test]
fn a_claude_subagent_thread_names_the_child_command() {
    let fixture = fixture!();
    let stderr = fixture.refused(&["show", SUBAGENT]);
    assert!(
        stderr.contains("tapes child session-claude --reference agent-fixture"),
        "{stderr}"
    );
}

/// A driver without a backend, and a run not yet started, are facts about the
/// thread and not an unknown session.
#[test]
fn threads_without_a_readable_session_say_why() {
    let fixture = fixture!();
    let stderr = fixture.refused(&["show", "t3-thread://v1/cursor-thread"]);
    assert!(stderr.contains("runs on cursor"), "{stderr}");
    let stderr = fixture.refused(&["show", "pending-thread"]);
    assert!(stderr.contains("no native session yet"), "{stderr}");
}

/// The active provider thread wins over a higher run ordinal, and the other
/// session is named rather than dropped.
#[test]
fn two_provider_threads_choose_the_active_one_and_name_the_other() {
    let fixture = fixture!();
    let (id, stderr) = fixture.shown("handoff-thread");
    assert_eq!(id, HANDOFF_ACTIVE);
    assert!(
        stderr.contains("It also ran on codex 00000000-0000-0000-0000-00000000000b."),
        "{stderr}"
    );
}

/// `--harness` naming another harness must refuse rather than look the native
/// id up in the wrong store.
#[test]
fn a_harness_naming_another_store_is_refused() {
    let fixture = fixture!();
    let stderr = fixture.refused(&["show", "user-thread", "--harness", "claude"]);
    assert!(stderr.contains("--harness names claude"), "{stderr}");
}

/// `--exclude` takes a link and passes over its native session, without
/// narrowing the latest selection to that session's harness.
#[test]
fn exclude_takes_a_link_without_selecting_a_harness() {
    let fixture = fixture!();
    let latest = |extra: &[&str]| {
        let mut args = vec!["show", "--latest", "--global", "--tail", "1", "--json"];
        args.extend_from_slice(extra);
        let value = fixture.listed(&args);
        value["session"]["id"].as_str().unwrap().to_owned()
    };
    let first = latest(&["--harness", "codex"]);
    assert_eq!(first, HANDOFF_ACTIVE);
    let link = format!("http://h/{ENV}/handoff-thread");
    let next = latest(&["--harness", "codex", "--exclude", &link]);
    assert_eq!(next, CODEX_ID);
    let unrestricted = latest(&["--exclude", &link]);
    assert_ne!(unrestricted, HANDOFF_ACTIVE);
}

/// `--t3` lists only mapped sessions with their objects, and its link
/// round-trips into a session argument. The deleted thread's session and the
/// cursor thread are not in the native stores, so they do not appear.
#[test]
fn list_t3_lists_only_mapped_sessions_with_links_that_resolve() {
    let fixture = fixture!();
    let list = fixture.listed(&["list", "--t3", "--global", "--json"]);
    assert_eq!(
        ids(&list),
        [CODEX_ID, HANDOFF_ACTIVE, "session-claude", "session-pi"]
    );
    let pi = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == "session-pi")
        .unwrap();
    assert_eq!(pi["t3"]["thread_id"], DELEGATED);
    assert_eq!(pi["t3"]["environment_id"], ENV);
    assert_eq!(pi["t3"]["title"], "Delegated");
    let link = pi["t3"]["link"].as_str().unwrap();
    assert_eq!(link, format!("t3-thread://v2/{ENV}/{DELEGATED}"));
    assert_eq!(fixture.shown(link).0, "session-pi");
}

/// The restriction applies before the limit. Applied after it, the newest
/// unmapped session would spend the limit and the mapped one would vanish.
#[test]
fn the_t3_restriction_applies_before_the_limit() {
    let fixture = fixture!();
    let unmapped = "20000000-0000-0000-0000-000000000009";
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/codex/rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl",
        ),
    )
    .unwrap()
    .replace(CODEX_ID, unmapped);
    let name = format!("rollout-2026-01-01T23-00-00-{unmapped}.jsonl");
    fs::write(
        fixture
            .root
            .path()
            .join("codex/sessions/2026/01/01")
            .join(name),
        source,
    )
    .unwrap();
    let everything = fixture.listed(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(ids(&everything).contains(&unmapped.to_owned()));

    let list = fixture.listed(&[
        "list",
        "--t3",
        "--global",
        "--harness",
        "codex",
        "--limit",
        "1",
        "--json",
    ]);
    let got = ids(&list);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_ne!(got[0], unmapped);
}

/// `--t3` composes with content search on both search paths.
#[test]
fn list_t3_composes_with_search() {
    let fixture = fixture!();
    for extra in [&[][..], &["--full"][..]] {
        let mut args = vec![
            "list",
            "--t3",
            "--global",
            "--search",
            "Inspect the fixture",
            "--json",
        ];
        args.extend_from_slice(extra);
        let list = fixture.listed(&args);
        assert_eq!(
            ids(&list),
            [CODEX_ID, "session-claude", "session-pi"],
            "{extra:?}"
        );
        assert!(list["sessions"][0]["t3"]["title"].is_string(), "{extra:?}");
    }
}

/// A plain listing annotates T3 sessions and no others, and the text TITLE
/// prefers a recorded title, then the T3 title.
#[test]
fn a_plain_listing_annotates_only_t3_sessions() {
    let fixture = fixture!();
    let unmapped_name = "rollout-2026-01-01T12-00-00-10000000-0000-0000-0000-000000000003.jsonl";
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(unmapped_name),
        fixture
            .root
            .path()
            .join("codex/sessions/2026/01/01")
            .join(unmapped_name),
    )
    .unwrap();
    let list = fixture.listed(&["list", "--global", "--json"]);
    let sessions = list["sessions"].as_array().unwrap();
    for session in sessions {
        let mapped = session["id"] != "10000000-0000-0000-0000-000000000003";
        assert_eq!(session.get("t3").is_some(), mapped, "{session}");
    }

    let text = fixture.run(&["list", "--global", "--harness", "codex"]);
    let text = String::from_utf8_lossy(&text.stdout).into_owned();
    let row = text
        .lines()
        .find(|line| line.starts_with(CODEX_ID))
        .unwrap();
    assert_eq!(row.split('\t').nth(5), Some("Codex thread title"), "{row}");
}

/// Absence of a T3 store is a fact for a listing and an error for whoever
/// named a thread.
#[test]
fn without_a_store_a_listing_is_unchanged_and_t3_requests_fail() {
    let fixture = Fixture::without_store();
    let list = fixture.listed(&["list", "--global", "--json"]);
    assert!(!list["unavailable"].to_string().contains("T3"), "{list}");
    assert!(list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session.get("t3").is_none()));
    assert_eq!(fixture.shown(CODEX_ID).0, CODEX_ID);

    for stderr in [
        fixture.refused(&["list", "--t3", "--global"]),
        fixture.refused(&["show", &format!("http://h/{ENV}/user-thread")]),
    ] {
        assert!(
            stderr.contains("no T3 store") && stderr.contains("T3CODE_HOME"),
            "{stderr}"
        );
    }
}

/// A store that exists and cannot be read must not break an ordinary read or
/// listing, but says so; a request defined by the store fails.
#[test]
fn an_unreadable_store_warns_and_leaves_ordinary_reads_intact() {
    let fixture = fixture!();
    fs::write(fixture.database(), b"this is not a database, not at all").unwrap();
    let (id, stderr) = fixture.shown(CODEX_ID);
    assert_eq!(id, CODEX_ID);
    assert_eq!(stderr.matches("Warning: T3").count(), 1, "{stderr}");

    let list = fixture.listed(&["list", "--global", "--json"]);
    assert!(!ids(&list).is_empty());
    let unavailable = list["unavailable"].to_string();
    assert!(unavailable.contains("T3 store"), "{unavailable}");

    fixture.refused(&["list", "--t3", "--global"]);
    fixture.refused(&["show", "t3-thread://v1/user-thread"]);
}

/// A supplied source holds nothing of the T3 store: a link there is refused,
/// and a bare id is not looked up.
#[test]
fn a_native_store_refuses_a_link_and_t3_and_never_looks_up_a_bare_id() {
    let fixture = fixture!();
    let codex = fixture.root.path().join("codex");
    let codex = codex.to_str().unwrap();
    let selected = ["--native-store", "codex", "--native-root", codex];
    let mut link = vec!["show", "t3-thread://v1/user-thread"];
    link.extend_from_slice(&selected);
    assert!(fixture.refused(&link).contains("installed T3 store"));
    let mut t3 = vec!["list", "--t3"];
    t3.extend_from_slice(&selected);
    assert!(fixture.refused(&t3).contains("--t3"));
    let mut bare = vec!["show", "user-thread"];
    bare.extend_from_slice(&selected);
    assert!(!fixture.refused(&bare).contains("Note: T3"));
    let mut listing = vec!["list", "--json"];
    listing.extend_from_slice(&selected);
    let list = fixture.listed(&listing);
    assert!(list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session.get("t3").is_none()));
}

/// A replica holds its own T3 store: `--t3` and a reference reach it unchanged,
/// and no local store is consulted, so a machine without one can still ask.
#[test]
fn remote_forwards_t3_and_references_unchanged() {
    let fixture = Fixture::without_store();
    let log = fixture.root.path().join("ssh.log");
    let ssh = fixture.root.path().join("fake-ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let link = format!("t3-thread://v2/{ENV}/{DELEGATED}");
    for args in [
        vec!["list", "--t3", "--remote", "replica"],
        vec!["show", link.as_str(), "--remote", "replica"],
    ] {
        let stderr = fixture
            .command()
            .args(&args)
            .env("TAPES_SSH", &ssh)
            .output()
            .unwrap()
            .stderr;
        assert!(
            !String::from_utf8_lossy(&stderr).contains("no T3 store"),
            "{args:?}"
        );
    }
    let sent = fs::read_to_string(&log).unwrap();
    assert!(sent.contains("--t3"), "{sent}");
    assert!(sent.contains(DELEGATED), "{sent}");
}
