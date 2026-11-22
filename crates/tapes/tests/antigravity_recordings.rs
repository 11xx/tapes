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

/// A T3 child link resolves the independently recorded child, and a parent
/// workset includes that recording. The task ID alone cannot select either.
#[test]
fn detached_child_requires_exact_native_launch_provenance() {
    let Some(f) = Fixture::new() else { return };
    let child_db = f.database(PROFILE, OTHER, OTHER);
    let profile = f.profile(PROFILE);
    let child = profile.join(format!("brain/{OTHER}/.system_generated"));
    let descriptors = profile.join(format!("brain/{ID}/.system_generated/subagents"));
    fs::create_dir_all(child.join("messages")).unwrap();
    fs::create_dir_all(child.join("logs")).unwrap();
    fs::create_dir_all(&descriptors).unwrap();
    let descriptor = descriptors.join(format!("{OTHER}.json"));
    fs::write(
        descriptor,
        format!(r#"{{"conversationId":"{OTHER}","spawnStepIndex":4}}"#),
    )
    .unwrap();
    let launch = child.join("messages/launch.json");
    let launch_json = format!(
        r#"{{"sender":"{ID}","sourceMetadata":{{"tool":{{"conversationId":"{ID}","toolCall":{{"id":"call_123","name":"invoke_subagent"}}}}}}}}"#
    );
    fs::write(&launch, &launch_json).unwrap();
    fs::write(
        child.join("logs/transcript_full.jsonl"),
        include_bytes!("fixtures/antigravity/transcript_full.jsonl"),
    )
    .unwrap();
    let parent_db = profile.join(format!("conversations/{ID}.db"));
    sqlite(
        &parent_db,
        "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, step_format INTEGER, step_payload BLOB);
         INSERT INTO steps VALUES (4,132,0,X'2A1D221B0A0863616C6C5F313233120F696E766F6B655F7375626167656E74');",
    );
    let state = f.0.path().join("userdata/statev2.sqlite");
    sqlite(
        &state,
        r#"UPDATE orchestration_v2_projection_provider_threads
           SET provider_instance_id='agy' WHERE provider_thread_id='pt-example-acp';
           CREATE TABLE orchestration_v2_projection_runs (
             run_id TEXT PRIMARY KEY, thread_id TEXT, provider_thread_id TEXT, provider_instance_id TEXT
           );
           INSERT INTO orchestration_v2_projection_runs VALUES ('run-launch','switch-thread','pt-example-acp','agy');
           CREATE TABLE orchestration_v2_projection_run_attempts (
             thread_id TEXT, run_id TEXT, root_node_id TEXT, provider_thread_id TEXT, provider_instance_id TEXT
           );
           INSERT INTO orchestration_v2_projection_run_attempts VALUES ('switch-thread','run-launch','node-parent','pt-example-acp','agy');
           INSERT INTO orchestration_v2_projection_threads
             (thread_id, project_id, title, default_provider, runtime_mode, interaction_mode, created_at, updated_at, payload_json)
             VALUES ('projected-child','p','Child','antigravity','m','i','2026-01-01','2026-01-10','{}');
           INSERT INTO orchestration_v2_projection_subagents
             (subagent_id, thread_id, run_id, parent_node_id, provider, child_thread_id, origin, status, updated_at, payload_json, driver, provider_instance_id)
             VALUES ('node-child','switch-thread','run-launch','node-parent','antigravity','projected-child','provider_native','completed','2026-01-10',
               '{"nativeTaskRef":{"driver":"antigravity","nativeId":"call_123","strength":"strong"}}','antigravity','agy');"#,
    );
    let link = ["show", "t3-thread://v1/projected-child", "--json"];
    let shown: Value = serde_json::from_str(success(&f.run(&link))).unwrap();
    assert_eq!(shown["session"]["id"], OTHER);

    let bundle = f.0.path().join("family");
    success(&f.run(&[
        "workset",
        ID,
        "--harness",
        "antigravity",
        "--bundle",
        bundle.to_str().unwrap(),
    ]));
    let manifest: Value =
        serde_json::from_slice(&fs::read(bundle.join("workset.json")).unwrap()).unwrap();
    assert_eq!(manifest["sources"][1]["session_id"], OTHER);
    assert_eq!(manifest["sources"][1]["role"], "child");

    fs::write(&launch, launch_json.replace("call_123", "call_456")).unwrap();
    assert!(!f.run(&link).status.success());
    fs::write(&launch, &launch_json).unwrap();
    sqlite(&parent_db, "UPDATE steps SET step_payload=X'00'");
    assert!(!f.run(&link).status.success());
    sqlite(&parent_db, "UPDATE steps SET step_payload=X'2A1D221B0A0863616C6C5F313233120F696E766F6B655F7375626167656E74'");
    assert!(f.run(&link).status.success());

    let duplicate = f.database(SECOND, OTHER, OTHER);
    assert!(!f.run(&link).status.success());
    fs::remove_file(child_db).unwrap();
    let cross_profile = f.run(&link);
    assert!(!cross_profile.status.success());
    assert!(String::from_utf8_lossy(&cross_profile.stderr).contains("different profile"));
    fs::remove_file(duplicate).unwrap();
    assert!(!f.run(&link).status.success());
    f.database(PROFILE, OTHER, OTHER);

    let hidden_parent = parent_db.with_extension("hidden");
    fs::rename(&parent_db, &hidden_parent).unwrap();
    assert!(!f.run(&link).status.success());
    fs::rename(&hidden_parent, &parent_db).unwrap();
    fs::write(child.join("messages/duplicate.json"), &launch_json).unwrap();
    assert!(!f.run(&link).status.success());
    fs::remove_file(child.join("messages/duplicate.json")).unwrap();

    sqlite(
        &state,
        "INSERT INTO orchestration_v2_projection_subagents
        SELECT 'second-projection',thread_id,run_id,parent_node_id,provider,provider_thread_id,
          child_thread_id,origin,status,started_at,completed_at,updated_at,payload_json,driver,
          provider_instance_id,completion_delivery_state
        FROM orchestration_v2_projection_subagents WHERE subagent_id='node-child'",
    );
    assert!(!f.run(&link).status.success());
    sqlite(
        &state,
        "DELETE FROM orchestration_v2_projection_subagents WHERE subagent_id='second-projection'",
    );

    sqlite(&state, "UPDATE orchestration_v2_projection_provider_threads SET driver='codex' WHERE provider_thread_id='pt-example-acp'");
    assert!(!f.run(&link).status.success());
    sqlite(&state, "UPDATE orchestration_v2_projection_provider_threads SET driver='antigravity' WHERE provider_thread_id='pt-example-acp'");
    sqlite(
        &state,
        "UPDATE orchestration_v2_projection_runs SET provider_instance_id='other'",
    );
    assert!(!f.run(&link).status.success());
    sqlite(
        &state,
        "UPDATE orchestration_v2_projection_runs SET provider_instance_id='agy'",
    );
    assert!(f.run(&link).status.success());
    sqlite(&state, "UPDATE orchestration_v2_projection_threads SET active_provider_thread_id='pt-example-codex' WHERE thread_id='switch-thread'");
    assert!(!f.run(&link).status.success());
}
