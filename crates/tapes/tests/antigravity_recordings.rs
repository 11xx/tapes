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

fn encode_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn encode_bytes_field(field: u64, payload: &[u8], out: &mut Vec<u8>) {
    encode_varint((field << 3) | 2, out);
    encode_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// First-party Antigravity usage accounting, workspace scoping (`--here`/`--project`),
/// list-price estimates, span splitting, `--native-store`, `tapes-parent`, and `self`.
#[test]
fn antigravity_usage_accounting_directory_scope_spans_pricing_and_self() {
    let Some(f) = Fixture::new() else { return };
    let workspace = f.0.path().join("sample-project");
    fs::create_dir_all(&workspace).unwrap();
    let workspace_uri = format!("file://{}", workspace.display());
    let mut meta_blob = Vec::new();
    encode_bytes_field(7, workspace_uri.as_bytes(), &mut meta_blob);

    let db_path = f.profile(PROFILE).join(format!("conversations/{ID}.db"));
    sqlite(
        &db_path,
        &format!(
            "CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);
             INSERT INTO trajectory_metadata_blob VALUES ('main', X'{}');",
            hex_upper(&meta_blob)
        ),
    );

    // 1. Existing fixture usage: two PLANNER_RESPONSE steps (160+201=361 in, 0 cr, 24+11=35 out)
    //    plus one malformed line => unreadable-record qualification, no native cost.
    let usage_out = f.run(&["usage", ID, "--harness", "antigravity", "--full", "--json"]);
    let usage: Value = serde_json::from_str(success(&usage_out)).unwrap();
    assert_eq!(usage["accounting"]["basis"], "summed-requests");
    assert_eq!(usage["accounting"]["coverage"], "session");
    assert_eq!(usage["tokens"]["input"], 361);
    assert_eq!(usage["tokens"]["cache_read"], 0);
    assert_eq!(usage["tokens"]["output"], 35);
    assert!(usage["cost"].is_null());
    assert_eq!(usage["disjoint"]["convention"]["input"], "excludes-cache");
    assert_eq!(
        usage["disjoint"]["convention"]["output"],
        "includes-reasoning"
    );
    assert_eq!(usage["disjoint"]["uncached_input"], 361);
    assert_eq!(usage["disjoint"]["cache_read"], 0);
    assert_eq!(usage["disjoint"]["output"], 35);
    assert_eq!(usage["by_model"][0]["model"], "gemini-3-pro");
    assert_eq!(usage["by_model"][0]["request_count"], 2);
    assert_eq!(
        usage["session"]["directory"],
        workspace.display().to_string()
    );
    assert!(usage["measurement_completeness"]["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "unreadable-record"));

    // 2. Scoped `--project` listing includes sessions in the matching workspace
    //    and excludes sessions in other workspaces.
    let listed = f.run(&[
        "list",
        "--project",
        workspace.to_str().unwrap(),
        "--harness",
        "antigravity",
        "--json",
    ]);
    let listed_json: Value = serde_json::from_str(success(&listed)).unwrap();
    assert_eq!(listed_json["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed_json["sessions"][0]["id"], ID);

    let other_workspace = f.0.path().join("workspace-other");
    fs::create_dir_all(&other_workspace).unwrap();
    let excluded = f.run(&[
        "list",
        "--project",
        other_workspace.to_str().unwrap(),
        "--harness",
        "antigravity",
        "--json",
    ]);
    let excluded_json: Value = serde_json::from_str(success(&excluded)).unwrap();
    assert_eq!(excluded_json["sessions"].as_array().unwrap().len(), 0);

    // 3. `--native-store antigravity --native-root <T3_HOME>` reads explicit store cleanly.
    let native_out = f.run(&[
        "list",
        "--native-store",
        "antigravity",
        "--native-root",
        f.0.path().to_str().unwrap(),
        "--json",
    ]);
    let native_json: Value = serde_json::from_str(success(&native_out)).unwrap();
    assert_eq!(native_json["sessions"][0]["id"], ID);
    assert_eq!(
        native_json["sessions"][0]["source"]["kind"],
        "explicit-native-store"
    );

    // 4. `self` resolves ANTIGRAVITY_CONVERSATION_ID.
    let self_out = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .env("HOME", f.0.path().join("home"))
        .env("T3CODE_HOME", f.0.path())
        .env("CODEX_HOME", f.0.path().join("codex"))
        .env("CLAUDE_CONFIG_DIR", f.0.path().join("claude"))
        .env("PI_CODING_AGENT_SESSION_DIR", f.0.path().join("pi"))
        .env_remove("PI_SESSION_FILE")
        .env("XDG_DATA_HOME", f.0.path().join("data"))
        .env("TAPES_OFFLINE", "1")
        .env_remove("CLAUDE_SESSION_ID")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("OPENCODE_SESSION")
        .env_remove("PI_SESSION_ID")
        .env("ANTIGRAVITY_CONVERSATION_ID", ID)
        .args(["show", "self", "--tail", "5", "--json"])
        .output()
        .unwrap();
    let self_json: Value = serde_json::from_str(success(&self_out)).unwrap();
    assert_eq!(self_json["session"]["id"], ID);

    // 5. Rich transcript: model switch, CHECKPOINT compaction (unattributed), duplicate step_index,
    //    stopped attempt, unrecognized usage, list-price estimate, and span split.
    let rich_id = OTHER;
    let rich_db = f.database(PROFILE, rich_id, rich_id);
    sqlite(
        &rich_db,
        &format!(
            "CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);
             INSERT INTO trajectory_metadata_blob VALUES ('main', X'{}');",
            hex_upper(&meta_blob)
        ),
    );
    let rich_logs = f
        .profile(PROFILE)
        .join(format!("brain/{rich_id}/.system_generated/logs"));
    fs::create_dir_all(&rich_logs).unwrap();
    let rich_lines = [
        format!(r#"{{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-01-01T10:00:02Z","content":"tapes-parent: antigravity:{ID}\nStart task."}}"#),
        r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-01-01T10:00:03Z","model":"gemini-4-neon-high","input_tokens":1000,"cache_read_tokens":2000,"output_tokens":300,"thinking":"step 1","content":"answer 1","tool_calls":[{"name":"read_file","args":{"Path":"a"}}]}"#.to_owned(),
        // Identical duplicate step_index must not double count.
        r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-01-01T10:00:03Z","model":"gemini-4-neon-high","input_tokens":1000,"cache_read_tokens":2000,"output_tokens":300,"thinking":"step 1","content":"answer 1","tool_calls":[{"name":"read_file","args":{"Path":"a"}}]}"#.to_owned(),
        // Model switch.
        r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-01-01T10:00:05Z","model":"gemini-4-neon-xhigh","input_tokens":500,"cache_read_tokens":3000,"output_tokens":200,"content":"answer 2"}"#.to_owned(),
        // Compaction CHECKPOINT (no model -> unattributed).
        r#"{"step_index":3,"source":"SYSTEM","type":"CHECKPOINT","status":"DONE","created_at":"2026-01-01T10:00:10Z","input_tokens":100,"cache_read_tokens":5000,"output_tokens":50,"content":"summary"}"#.to_owned(),
    ];
    fs::write(
        rich_logs.join("transcript_full.jsonl"),
        rich_lines.join("\n") + "\n",
    )
    .unwrap();

    let prices_file = f.0.path().join("prices.toml");
    fs::write(
        &prices_file,
        r#"schema = "tapes-price-table/1"
currency = "USD"

[[models]]
id = "gemini-4-neon-high"
effective = 2026-01-01
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.0, cache_write_1h = 1.0, output = 2.0 }

[[models]]
id = "gemini-4-neon-xhigh"
effective = 2026-01-01
source = "fixture"
per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.0, cache_write_1h = 2.0, output = 4.0 }
"#,
    )
    .unwrap();

    let rich_usage: Value = serde_json::from_str(success(&f.run(&[
        "usage",
        rich_id,
        "--harness",
        "antigravity",
        "--full",
        "--prices",
        prices_file.to_str().unwrap(),
        "--json",
    ])))
    .unwrap();
    // 1000 + 500 + 100 = 1600 in; 2000 + 3000 + 5000 = 10000 cr; 300 + 200 + 50 = 550 out.
    assert_eq!(rich_usage["tokens"]["input"], 1600);
    assert_eq!(rich_usage["tokens"]["cache_read"], 10000);
    assert_eq!(rich_usage["tokens"]["output"], 550);
    assert_eq!(rich_usage["by_model"].as_array().unwrap().len(), 2);
    assert_eq!(rich_usage["attribution"]["attributed"], 2);
    assert_eq!(rich_usage["attribution"]["unattributed"], 1);
    assert_eq!(
        rich_usage["attribution"]["unattributed_tokens"]["input"],
        100
    );
    assert_eq!(rich_usage["session"]["model_observation"]["mixed"], true);
    assert_eq!(
        rich_usage["session"]["model_selections"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(rich_usage["cost"].is_null());
    assert!(rich_usage["list_price_estimate"].is_object());

    // Span split selects only step 2 in [10:00:04Z, 10:00:08Z).
    let span_out: Value = serde_json::from_str(success(&f.run(&[
        "usage",
        rich_id,
        "--harness",
        "antigravity",
        "--from",
        "2026-01-01T10:00:04Z",
        "--to",
        "2026-01-01T10:00:08Z",
        "--prices",
        prices_file.to_str().unwrap(),
        "--json",
    ])))
    .unwrap();
    assert_eq!(span_out["tokens"]["input"], 500);
    assert_eq!(span_out["tokens"]["cache_read"], 3000);
    assert_eq!(span_out["tokens"]["output"], 200);
    assert!(span_out["list_price_estimate"].is_object());

    // 6. Launch-parent marker links child `rich_id` into `ID`'s family.
    let family_out: Value = serde_json::from_str(success(&f.run(&[
        "usage",
        ID,
        "--harness",
        "antigravity",
        "--family",
        "--json",
    ])))
    .unwrap();
    let family_sessions = family_out["sessions"].as_array().unwrap();
    assert_eq!(family_sessions.len(), 2, "{family_out:#}");
    assert_eq!(family_sessions[1]["id"], rich_id);
    assert_eq!(family_sessions[1]["link"], "launch-marker");
    assert_eq!(family_out["partitions"][0]["tokens"]["input"], 1961);
    assert_eq!(family_out["partitions"][0]["tokens"]["cache_read"], 10000);
    assert_eq!(family_out["partitions"][0]["tokens"]["output"], 585);

    // 7. `--native-store antigravity --native-root <T3_HOME>` isolates the store,
    //    and `--native-file` refuses cleanly.
    let native_list: Value = serde_json::from_str(success(
        &Command::new(env!("CARGO_BIN_EXE_tapes"))
            .args([
                "list",
                "--native-store",
                "antigravity",
                "--native-root",
                f.0.path().to_str().unwrap(),
                "--json",
            ])
            .env("HOME", f.0.path().join("other-home"))
            .env("T3_HOME", f.0.path().join("other-t3"))
            .output()
            .unwrap(),
    ))
    .unwrap();
    assert_eq!(native_list["sessions"].as_array().unwrap().len(), 2);

    let native_file_refusal = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .args([
            "show",
            ID,
            "--native-store",
            "antigravity",
            "--native-root",
            f.0.path().to_str().unwrap(),
            "--native-file",
            rich_db.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!native_file_refusal.status.success());
    let stderr = String::from_utf8_lossy(&native_file_refusal.stderr);
    assert!(stderr.contains("antigravity"), "{stderr}");
}
