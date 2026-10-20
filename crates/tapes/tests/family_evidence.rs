//! `tapes usage SESSION --family --evidence PATH` adds the sessions an
//! evidence file launches from a member, and refuses what it cannot read.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{json, Value};

const PARENT: &str = "40000000-0000-7000-8000-0000000000a1";
const CHILD: &str = "40000000-0000-7000-8000-0000000000a2";

fn codex_session(root: &Path, id: &str, minute: u32) {
    let path = root.join(format!(
        "sessions/2026/01/10/rollout-2026-01-10T09-{minute:02}-00-{id}.jsonl"
    ));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let at = format!("2026-01-10T09:{minute:02}:00Z");
    let later = format!("2026-01-10T09:{:02}:00Z", minute + 5);
    let usage = json!({"input_tokens": 100, "cached_input_tokens": 0, "output_tokens": 7,
                       "reasoning_output_tokens": 0, "total_tokens": 107});
    let lines = [
        json!({"timestamp": at, "type": "session_meta", "payload": {"id": id, "timestamp": at,
               "cwd": "/fixtures/project", "source": "exec", "originator": "codex_exec"}}),
        json!({"timestamp": at, "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
        json!({"timestamp": at, "type": "response_item", "payload": {"type": "message",
               "role": "user", "content": [{"type": "input_text", "text": "Inspect the fixture."}]}}),
        json!({"timestamp": later, "type": "event_msg", "payload": {"type": "token_count",
               "info": {"total_token_usage": usage, "last_token_usage": usage}}}),
    ];
    let body: Vec<String> = lines.iter().map(ToString::to_string).collect();
    fs::write(path, body.join("\n") + "\n").unwrap();
}

fn tapes(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
        .env("TAPES_OFFLINE", "1")
        .env_remove("TAPES_PRICES")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .arg("usage")
        .args(args)
        .args(["--native-store", "codex", "--native-root"])
        .arg(root)
        .output()
        .unwrap()
}

fn evidence_file(root: &Path) -> std::path::PathBuf {
    let path = root.join("evidence.jsonl");
    let lines = [
        json!({"schema": "tapes-attribution-evidence/1", "producer": "fixture-producer/1.0.0"}),
        json!({"kind": "lineage", "parent": {"harness": "codex", "id": PARENT},
               "child": {"harness": "codex", "id": CHILD}, "tier": "bound",
               "label": "nightly-review", "subject": "auth-rework"}),
    ];
    let body: Vec<String> = lines.iter().map(ToString::to_string).collect();
    fs::write(&path, body.join("\n") + "\n").unwrap();
    path
}

/// Claim: `--evidence` without `--family` refuses, naming `--family`, and
/// reads nothing. Rejects answering a session's own usage as though the
/// file had been applied.
#[test]
fn evidence_without_family_refuses() {
    let temporary = tempfile::TempDir::with_prefix("tapes-cli-evidence-bare-").unwrap();
    let root = temporary.path();
    codex_session(root, PARENT, 0);
    let file = evidence_file(root);
    let output = tapes(
        root,
        &[
            PARENT,
            "--harness",
            "codex",
            "--evidence",
            file.to_str().unwrap(),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("--family"), "{stderr}");
}

/// Claim: a missing evidence path refuses naming the path. Rejects a family
/// printed as though no file had been named.
#[test]
fn a_missing_evidence_path_refuses_naming_it() {
    let temporary = tempfile::TempDir::with_prefix("tapes-cli-evidence-missing-").unwrap();
    let root = temporary.path();
    codex_session(root, PARENT, 0);
    let missing = root.join("absent.jsonl");
    let output = tapes(
        root,
        &[
            PARENT,
            "--harness",
            "codex",
            "--family",
            "--evidence",
            missing.to_str().unwrap(),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains(missing.to_str().unwrap()), "{stderr}");
}

/// Claim: `usage SESSION --family --evidence FILE` adds the markerless child
/// the file launches, as JSON and in the text view, with the file, producer,
/// label and subject that link it. Rejects a command that ignores the flag.
#[test]
fn a_family_with_evidence_names_what_links_each_delegate() {
    let temporary = tempfile::TempDir::with_prefix("tapes-cli-evidence-family-").unwrap();
    let root = temporary.path();
    codex_session(root, PARENT, 0);
    codex_session(root, CHILD, 10);
    let file = evidence_file(root);
    let file = file.to_str().unwrap();

    let output = tapes(
        root,
        &[
            PARENT,
            "--harness",
            "codex",
            "--family",
            "--evidence",
            file,
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let family: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(family["schema"], "tapes-family-usage/3");
    let sessions = family["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{family:#}");
    assert_eq!(sessions[1]["id"], CHILD);
    assert_eq!(sessions[1]["link"], "evidence");
    assert_eq!(sessions[1]["evidence"]["file"], file);
    assert_eq!(
        sessions[1]["evidence"]["producer"],
        "fixture-producer/1.0.0"
    );
    assert_eq!(sessions[1]["evidence"]["label"], "nightly-review");

    let output = tapes(
        root,
        &[PARENT, "--harness", "codex", "--family", "--evidence", file],
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("evidence"), "{text}");
    assert!(
        text.contains("linked by") && text.contains("fixture-producer/1.0.0"),
        "{text}"
    );
    assert!(text.contains("Evidence: 1 files"), "{text}");

    let output = tapes(root, &[PARENT, "--harness", "codex", "--family", "--json"]);
    let family: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        family["sessions"].as_array().unwrap().len(),
        1,
        "{family:#}"
    );
    assert!(family.get("evidence").is_none());
}
