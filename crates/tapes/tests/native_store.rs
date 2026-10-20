use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Stores {
    root: PathBuf,
    harness: &'static str,
    id: &'static str,
    recording: PathBuf,
}

impl Stores {
    fn new(harness: &'static str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "tapes-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let (id, relative, fixture) = match harness {
            "claude" => (
                "session-claude",
                "projects/project/session-claude.jsonl",
                include_str!("../../tapes-core/tests/fixtures/claude/project/session-claude.jsonl"),
            ),
            "codex" => (
                "00000000-0000-0000-0000-000000000001",
                "sessions/2026/01/01/rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl",
                include_str!("../../tapes-core/tests/fixtures/codex/rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
            ),
            "pi" => (
                "session-pi",
                "sessions/project/2026-01-01T10-00-00-000Z_session-pi.jsonl",
                include_str!("../../tapes-core/tests/fixtures/pi/2026-01-01T10-00-00-000Z_session-pi.jsonl"),
            ),
            _ => unreachable!(),
        };
        for generation in ["selected", "other", "ambient"] {
            let path = root.join(generation).join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let contents = fixture
                .replace("Inspect the fixture.", generation)
                .replace("Fixture session", generation);
            fs::write(path, contents).unwrap();
        }
        if harness == "claude" {
            let child = root.join("selected/projects/project/session-claude/subagents");
            fs::create_dir_all(&child).unwrap();
            fs::write(child.join("agent-fixture.jsonl"), include_str!("../../tapes-core/tests/fixtures/claude/project/session-claude/subagents/agent-fixture.jsonl")).unwrap();
            fs::write(child.join("agent-fixture.meta.json"), include_str!("../../tapes-core/tests/fixtures/claude/project/session-claude/subagents/agent-fixture.meta.json")).unwrap();
        }
        Self {
            recording: root.join("selected").join(relative),
            root,
            harness,
            id,
        }
    }

    fn command(&self, args: &[&str], generation: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tapes"));
        command.env("TAPES_OFFLINE", "1");
        command
            .args(args)
            .args(["--native-store", self.harness, "--native-root"])
            .arg(self.root.join(generation))
            .env("CLAUDE_CONFIG_DIR", self.root.join("ambient"))
            .env("CODEX_HOME", self.root.join("ambient"))
            .env("PI_CODING_AGENT_DIR", self.root.join("ambient"))
            .env_remove("PI_SESSION_FILE")
            .env("CODEX_THREAD_ID", self.id)
            .env_remove("CLAUDE_SESSION_ID")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("OPENCODE_SESSION")
            .env_remove("PI_SESSION_ID")
            .env("PATH", "/usr/bin:/bin")
            .current_dir(env!("CARGO_MANIFEST_DIR"));
        command
    }

    fn json(&self, args: &[&str], generation: &str) -> Value {
        let output = self.command(args, generation).output().unwrap();
        json_output(output)
    }

    fn source(&self, source: &Value) {
        assert_eq!(source["kind"], "explicit-native-store");
        assert_eq!(source["recorded_harness"], self.harness);
        assert_eq!(
            source["location"]["container"],
            self.root.join("selected").to_str().unwrap()
        );
        assert_eq!(
            source["location"]["locator"],
            self.recording.to_str().unwrap()
        );
    }
}

impl Drop for Stores {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn json_output(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn native_flags_isolate_each_harness_and_materialized_generation() {
    for harness in ["claude", "codex", "pi"] {
        let stores = Stores::new(harness);
        let list = stores.json(&["list", "--limit", "1", "--json"], "selected");
        assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
        stores.source(&list["sessions"][0]["source"]);
        assert!(list["sessions"][0].get("live").is_none());
        for generation in ["selected", "other"] {
            let show = stores.json(&["show", stores.id, "--json"], generation);
            assert!(show["turns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|turn| turn["text"] == generation));
            assert!(show["session"].get("live").is_none());
        }
        let prefix = &stores.id[..stores.id.len() - 1];
        let show = stores.json(
            &["show", prefix, "--harness", harness, "--json"],
            "selected",
        );
        stores.source(&show["session"]["source"]);
        let latest = stores.json(&["show", "--latest", "--json"], "selected");
        assert_eq!(latest["session"]["id"], stores.id);
        let full = stores.json(
            &["show", stores.id, "--full", "--tail", "1", "--json"],
            "selected",
        );
        stores.source(&full["session"]["source"]);
        let capture = stores.json(&["capture", stores.id, "--json"], "selected");
        assert_eq!(capture["state"], "caller-selected-native-store");
        let absent = stores.json(&["list", "--json"], "missing");
        assert!(absent["sessions"].as_array().unwrap().is_empty());
        let missing = stores
            .command(&["show", stores.id, "--json"], "missing")
            .output()
            .unwrap();
        assert!(!missing.status.success());
    }
}

#[test]
fn native_store_reaches_history_views_aggregates_and_export() {
    let stores = Stores::new("codex");
    for args in [
        vec!["page", stores.id, "--bytes", "64KiB", "--json"],
        vec![
            "history-search",
            stores.id,
            "--search",
            "selected",
            "--bytes",
            "64KiB",
            "--pages",
            "1",
            "--json",
        ],
        vec![
            "metadata", stores.id, "--bytes", "64KiB", "--pages", "1", "--json",
        ],
        vec!["brief", stores.id, "--json"],
        vec!["usage", stores.id, "--full", "--json"],
        vec!["stats", stores.id, "--full", "--json"],
        vec!["lineage", stores.id, "--full", "--json"],
        vec!["events", stores.id, "--full", "--json"],
    ] {
        let value = stores.json(&args, "selected");
        stores.source(&value["session"]["source"]);
    }
    for command in ["stats", "endings"] {
        let value = stores.json(&[command, "--json"], "selected");
        assert!(serde_json::to_string(&value)
            .unwrap()
            .contains("explicit-native-store"));
        assert!(!serde_json::to_string(&value)
            .unwrap()
            .contains("installed-recording"));
    }
    let usage = stores.json(&["usage", "--json"], "selected");
    assert_eq!(usage["totals"]["sessions"], 1);
    let bundle = stores.root.join("bundle");
    let output = stores
        .command(&["export", stores.id, "--full"], "selected")
        .arg("--bundle")
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json_file = fs::read_dir(&bundle)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let exported: Value = serde_json::from_slice(&fs::read(json_file).unwrap()).unwrap();
    stores.source(&exported["session"]["source"]);
    let bulk = stores.root.join("bulk");
    let output = stores
        .command(&["export", "--limit", "1"], "selected")
        .arg("--bundle")
        .arg(&bulk)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(bulk.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema"], "tapes-export-manifest/7");
    assert_eq!(manifest["sessions"].as_array().unwrap().len(), 1);
}

#[test]
fn native_store_title_scope_self_and_child_are_source_local() {
    let stores = Stores::new("claude");
    let title = stores.json(&["show", "--title", "selected", "--json"], "selected");
    stores.source(&title["session"]["source"]);
    let scoped = stores
        .command(&["show", "--latest", "--here", "--json"], "selected")
        .output()
        .unwrap();
    assert!(!scoped.status.success());
    for full in [false, true] {
        let mut args = vec!["child", stores.id, "--reference", "fixture", "--json"];
        if full {
            args.push("--full");
        }
        let child = stores.json(&args, "selected");
        assert!(serde_json::to_string(&child)
            .unwrap()
            .contains("explicit-native-store"));
    }
    let stores = Stores::new("codex");
    let own = stores.json(&["show", "self", "--json"], "selected");
    stores.source(&own["session"]["source"]);
    let missing = stores
        .command(&["show", "self", "--json"], "missing")
        .output()
        .unwrap();
    assert!(!missing.status.success());
}

#[test]
fn native_store_conflicts_refuse_before_reading_or_remote_transport() {
    let stores = Stores::new("codex");
    for args in [
        vec!["list", "--input", "unused.json"],
        vec!["list", "--remote", "example.invalid"],
        vec!["show", stores.id, "--remote", "example.invalid"],
        vec!["export", stores.id, "--remote", "example.invalid"],
        vec!["list", "--harness", "claude"],
        vec!["show", stores.id, "--harness", "pi"],
        vec!["capture", stores.id, "--harness", "claude"],
    ] {
        let output = stores.command(&args, "selected").output().unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("conflict")
                || String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for args in [
        vec!["list", "--native-store", "codex"],
        vec!["list", "--native-root", "unused"],
        vec![
            "list",
            "--native-store",
            "opencode",
            "--native-root",
            "unused",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env("TAPES_OFFLINE", "1")
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        if args.contains(&"opencode") {
            assert!(String::from_utf8_lossy(&output.stderr).contains("API or database"));
        }
    }
}

#[test]
fn repeated_codex_ids_select_the_same_occurrence_and_allow_an_explicit_locator() {
    use sha2::Digest;
    use std::time::{Duration, UNIX_EPOCH};
    let stores = Stores::new("codex");
    let newer = stores.recording.with_file_name(format!(
        "rollout-2026-01-02T10-00-00-{}_00000000-0000-4000-8000-000000000002.jsonl",
        stores.id
    ));
    let old_bytes = fs::read(&stores.recording).unwrap();
    let new_bytes = String::from_utf8(old_bytes.clone())
        .unwrap()
        .replace("2026-01-01", "2026-01-02")
        .replace("selected", "newer occurrence");
    fs::write(&newer, &new_bytes).unwrap();
    for (path, seconds) in [(&stores.recording, 1), (&newer, 2)] {
        fs::File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
    }
    let listing = stores.json(&["list", "--json"], "selected");
    assert_eq!(listing["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        listing["sessions"][0]["source"]["location"]["locator"],
        newer.to_str().unwrap()
    );
    for args in [
        vec!["show", stores.id, "--json"],
        vec!["show", stores.id, "--full", "--json"],
    ] {
        let view = stores.json(&args, "selected");
        assert!(
            serde_json::to_string(&view)
                .unwrap()
                .contains(newer.to_str().unwrap()),
            "{view}"
        );
    }
    for (label, locator, expected) in [
        ("default", None, &newer),
        ("older", Some(&stores.recording), &stores.recording),
        ("newer", Some(&newer), &newer),
    ] {
        let mut show = stores.command(&["show", stores.id, "--full", "--json"], "selected");
        let mut export = stores.command(&["export", stores.id, "--full"], "selected");
        if let Some(locator) = locator {
            show.arg("--native-file").arg(locator);
            export.arg("--native-file").arg(locator);
        }
        let shown = json_output(show.output().unwrap());
        assert_eq!(
            shown["session"]["source"]["location"]["locator"],
            expected.to_str().unwrap()
        );
        let mut page = stores.command(
            &["page", stores.id, "--pin", "--bytes", "64KiB", "--json"],
            "selected",
        );
        if let Some(locator) = locator {
            page.arg("--native-file").arg(locator);
        }
        let pinned = json_output(page.output().unwrap());
        let expected_bytes = fs::read(expected).unwrap();
        let digest = format!("{:x}", sha2::Sha256::digest(&expected_bytes));
        assert_eq!(
            pinned["read"]["source_prefix_digest"]["value"], digest,
            "{pinned}"
        );
        let bundle = stores.root.join(label);
        let output = export.arg("--bundle").arg(&bundle).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json = fs::read_dir(bundle)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .unwrap();
        let exported: Value = serde_json::from_slice(&fs::read(json).unwrap()).unwrap();
        assert_eq!(exported["session"]["source"], shown["session"]["source"]);
        assert_eq!(exported["turns"], shown["turns"]);
    }
    let wrong = stores
        .command(&["show", "missing-id", "--json"], "selected")
        .arg("--native-file")
        .arg(&newer)
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    fs::remove_file(&newer).unwrap();
    let missing = stores
        .command(&["show", stores.id, "--json"], "selected")
        .arg("--native-file")
        .arg(&newer)
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert_eq!(fs::read(&stores.recording).unwrap(), old_bytes);
}

#[test]
fn explicit_native_files_work_for_each_file_harness() {
    for harness in ["claude", "codex", "pi"] {
        let stores = Stores::new(harness);
        let shown = json_output(
            stores
                .command(&["show", stores.id, "--json"], "selected")
                .arg("--native-file")
                .arg(&stores.recording)
                .output()
                .unwrap(),
        );
        stores.source(&shown["session"]["source"]);
    }
}
