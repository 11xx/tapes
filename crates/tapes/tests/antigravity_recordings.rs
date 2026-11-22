//! Verify Antigravity identity and recovery via the same workset pickup uses.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use agent_tapes_discovery::{Discovery, Harness, IdentityBasis, NativeStore, NativeStoreKind};
use serde_json::Value;

const ID: &str = "a0000000-0000-4000-8000-000000000001";
const OTHER: &str = "a0000000-0000-4000-8000-000000000002";
const PROFILE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SECOND: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn sqlite(path: &Path, sql: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut child = Command::new("sqlite3")
        .args(["-batch", "-bail", "-init", "/dev/null"])
        .arg(path)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Option<Self> {
        if !Command::new("sqlite3")
            .arg("--version")
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipping Antigravity fixture: sqlite3 is unavailable");
            return None;
        }
        let fixture = Self(tempfile::tempdir().unwrap());
        fs::create_dir(fixture.0.path().join("home")).unwrap();
        fixture.database(PROFILE, ID, ID);
        let profile = fixture.profile(PROFILE);
        let transcript = profile.join(format!("brain/{ID}/.system_generated/logs"));
        fs::create_dir_all(&transcript).unwrap();
        fs::write(
            transcript.join("transcript_full.jsonl"),
            include_bytes!("fixtures/antigravity/transcript_full.jsonl"),
        )
        .unwrap();
        fs::write(
            transcript.join("transcript.jsonl"),
            "mirror-must-not-appear",
        )
        .unwrap();
        fs::write(
            profile.join("antigravity-oauth-token"),
            "credential-sentinel",
        )
        .unwrap();
        sqlite(
            &fixture.0.path().join("userdata/statev2.sqlite"),
            &format!(
                "{}\n{}",
                include_str!("fixtures/t3/statev2.sql"),
                include_str!("fixtures/antigravity/state.sql"),
            ),
        );
        let codex = fixture.0.path().join("codex/sessions");
        fs::create_dir_all(&codex).unwrap();
        let filename = "rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl";
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/codex")
                .join(filename),
            codex.join(filename),
        )
        .unwrap();
        Some(fixture)
    }

    fn profile(&self, hash: &str) -> PathBuf {
        self.0
            .path()
            .join("userdata/providers/antigravity")
            .join(hash)
            .join("antigravity-acp")
    }

    fn database(&self, hash: &str, filename: &str, id: &str) -> PathBuf {
        let path = self
            .profile(hash)
            .join("conversations")
            .join(format!("{filename}.db"));
        sqlite(&path, &format!(
            "CREATE TABLE trajectory_meta (trajectory_id TEXT PRIMARY KEY, cascade_id TEXT, trajectory_type INTEGER, source INTEGER);
             INSERT INTO trajectory_meta VALUES ('b0000000-0000-4000-8000-000000000001','{id}',4,17);"
        ));
        path
    }

    fn discovery(&self) -> Discovery {
        Discovery::new([NativeStore::antigravity(self.0.path())])
    }

    fn run(&self, args: &[&str]) -> Output {
        let root = self.0.path();
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env("HOME", root.join("home"))
            .env("T3CODE_HOME", root)
            .env("CODEX_HOME", root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("PI_CODING_AGENT_SESSION_DIR", root.join("pi"))
            .env_remove("PI_SESSION_FILE")
            .env("XDG_DATA_HOME", root.join("data"))
            .env("TAPES_OFFLINE", "1")
            .args(args)
            .output()
            .unwrap()
    }
}

fn success(output: &Output) -> &str {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::str::from_utf8(&output.stdout).unwrap()
}

/// The native ID works through discovery, transcript reads and pickup.
/// A switched provider's messages and failures stay with its own recording.
#[test]
fn native_identity_recovers_a_pickup_by_id_and_preserves_switch_boundaries() {
    let Some(f) = Fixture::new() else { return };
    let identity = f.discovery().resolve(ID).unwrap();
    assert_eq!(identity.id(), ID);
    assert_eq!(identity.harness(), Harness::Antigravity);
    assert_eq!(identity.store_kind(), NativeStoreKind::AntigravityAcp);
    assert_eq!(identity.identity_basis(), Some(IdentityBasis::Database));
    assert_eq!(f.discovery().resolve("a000").unwrap(), identity);
    let shown = f.run(&["show", ID, "--harness", "antigravity", "--full", "--json"]);
    let text = success(&shown);
    let session: Value = serde_json::from_str(text).unwrap();
    assert_eq!(session["session"]["id"], ID);
    for expected in [
        "Check the sample workspace.",
        "The sample printed correctly.",
        "run_command",
        "Command output:",
        "The sample tool did not finish.",
        "Example provider quota exhausted",
        "T3 provider turn 4 interrupted",
        "RUNNING",
        "malformed-record",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    for excluded in [
        "Other provider failure",
        "credential-sentinel",
        "system-payload-must-not-appear",
        "mirror-must-not-appear",
    ] {
        assert!(!text.contains(excluded), "leaked {excluded}");
    }

    let linked = f.run(&["show", "t3-thread://v1/switch-thread", "--json"]);
    assert_eq!(
        serde_json::from_str::<Value>(success(&linked)).unwrap()["session"]["id"],
        ID
    );
    assert!(String::from_utf8_lossy(&linked.stderr).contains("codex 00000000-"));
    let listed = f.run(&[
        "list",
        "--global",
        "--t3",
        "--harness",
        "antigravity",
        "--json",
    ]);
    let listing = success(&listed);
    assert!(listing.contains(ID) && listing.contains("switch-thread"));

    sqlite(&f.0.path().join("userdata/statev2.sqlite"),
        "UPDATE orchestration_v2_projection_threads SET active_provider_thread_id='pt-example-codex' WHERE thread_id='switch-thread'");
    let switched = f.run(&["show", "t3-thread://v1/switch-thread", "--json"]);
    assert_eq!(
        serde_json::from_str::<Value>(success(&switched)).unwrap()["session"]["id"],
        "00000000-0000-0000-0000-000000000001"
    );
    assert!(String::from_utf8_lossy(&switched.stderr).contains(ID));

    let bundle = f.0.path().join("bundle");
    let workset = f.run(&[
        "workset",
        ID,
        "--harness",
        "antigravity",
        "--bundle",
        bundle.to_str().unwrap(),
    ]);
    success(&workset);
    let manifest = bundle.join("workset.json");
    let frozen = fs::read_to_string(&manifest).unwrap();
    let workset: Value = serde_json::from_str(&frozen).unwrap();
    assert_eq!(workset["sources"][0]["session_id"], ID);
    assert_eq!(workset["sources"][0]["harness"], "antigravity");
    assert!(frozen.contains("Example provider quota exhausted"));
    let refs = f.0.path().join("refs.jsonl");
    fs::write(&refs, "{\"ref\":\"s0#0\",\"lane\":\"s0-0\"}\n").unwrap();
    let located = f.run(&[
        "locate",
        "--workset",
        bundle.to_str().unwrap(),
        "--refs",
        refs.to_str().unwrap(),
        "--text-bytes",
        "1024",
        "--json",
    ]);
    assert!(success(&located).contains("Check the sample workspace."));

    fs::remove_dir_all(f.0.path().join("codex")).unwrap();
    let antigravity_only = f.run(&["list", "--global"]);
    let listing = success(&antigravity_only);
    assert!(listing.contains(ID));
    assert!(listing.contains("Unavailable: claude, codex, opencode, pi"));
    assert!(!listing.contains("No harnesses available."));
}

/// File names and T3 bindings alone cannot prove an identity, and a duplicate
/// profile must not win an exact lookup by traversal order.
#[test]
fn missing_corrupt_foreign_and_duplicate_evidence_does_not_select_a_recording() {
    let Some(f) = Fixture::new() else { return };
    assert!(f
        .discovery()
        .resolve(OTHER)
        .unwrap_err()
        .to_string()
        .contains("not found"));
    let path = f.database(PROFILE, OTHER, ID);
    assert!(f
        .discovery()
        .resolve(OTHER)
        .unwrap_err()
        .to_string()
        .contains("but records session"));
    fs::write(&path, "not a database").unwrap();
    assert!(f.discovery().resolve(OTHER).is_err());
    fs::remove_file(path).unwrap();
    f.database(PROFILE, OTHER, OTHER);
    assert!(f
        .discovery()
        .resolve("a000")
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
    let missing = f.run(&["show", OTHER, "--harness", "antigravity", "--json"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Antigravity transcript unavailable"));

    f.database(SECOND, ID, ID);
    let logs = format!("brain/{ID}/.system_generated/logs");
    let second_logs = f.profile(SECOND).join(&logs);
    fs::create_dir_all(&second_logs).unwrap();
    fs::copy(
        f.profile(PROFILE).join(&logs).join("transcript_full.jsonl"),
        second_logs.join("transcript_full.jsonl"),
    )
    .unwrap();
    let duplicate = f.run(&["show", ID, "--harness", "antigravity", "--json"]);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("multiple provider profiles"));
    assert!(f.discovery().resolve(ID).is_err());
    assert!(f.discovery().resolve("a000").is_err());

    fs::remove_file(f.profile(PROFILE).join(format!("conversations/{ID}.db"))).unwrap();
    fs::remove_file(f.profile(SECOND).join(format!("conversations/{ID}.db"))).unwrap();
    assert!(
        f.discovery().resolve(ID).is_err(),
        "a matching brain path is insufficient"
    );
    let orphan = f.run(&["show", "t3-thread://v1/switch-thread", "--json"]);
    assert!(
        !orphan.status.success(),
        "a T3 link is insufficient without the native database"
    );
}

/// A valid database reached through a redirected conversations directory
/// cannot corroborate the native profile, even when queried by its full ID.
#[test]
fn exact_discovery_rejects_a_symlinked_conversations_directory() {
    let Some(f) = Fixture::new() else { return };
    let conversations = f.profile(PROFILE).join("conversations");
    let external = f.0.path().join("redirected-databases");
    fs::rename(&conversations, &external).unwrap();
    std::os::unix::fs::symlink(&external, &conversations).unwrap();

    for query in [ID, "a000"] {
        let error = f.discovery().resolve(query).unwrap_err();
        assert!(error.to_string().contains("expected a native directory"));
    }
    let exact = f.run(&["show", ID, "--harness", "antigravity", "--json"]);
    assert!(!exact.status.success());
    assert!(String::from_utf8_lossy(&exact.stderr).contains("expected a native directory"));
}
