//! A caller can persist page positions across actual CLI process restarts.
//! This rejects process-local cursors and append-invalidated snapshots.
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::process::Command;

#[test]
fn read_positions_cross_processes_and_follow_growth() {
    for harness in ["claude", "codex", "pi"] {
        let root = tempfile::tempdir().unwrap();
        let id = "00000000-0000-0000-0000-000000000001";
        let filename = match harness {
            "claude" => format!("{id}.jsonl"),
            "codex" => format!("rollout-2026-01-01T10-00-00-{id}.jsonl"),
            _ => format!("2026-01-01T10-00-00-000Z_{id}.jsonl"),
        };
        let file = root.path().join(filename);
        let message = |n| {
            let parts = if harness == "codex" {
                json!([{"type":"output_text","text":format!("message-{n}-a")},{"type":"output_text","text":format!("message-{n}-b")}])
            } else {
                json!([{"type":"text","text":format!("message-{n}-a")},{"type":"thinking","thinking":"private"},{"type":"text","text":format!("message-{n}-b")}])
            };
            match harness {
                "claude"=>json!({"type":"assistant","sessionId":id,"uuid":format!("m{n}"),"message":{"role":"assistant","content":parts}}),
                "codex"=>json!({"type":"response_item","payload":{"type":"message","id":format!("m{n}"),"role":"assistant","content":parts}}),
                _=>json!({"type":"message","id":format!("m{n}"),"message":{"role":"assistant","content":parts}}),
            }.to_string()
        };
        let header = match harness {
            "codex" => format!("{}\n", json!({"type":"session_meta","payload":{"id":id}})),
            "pi" => format!("{}\n", json!({"type":"session","id":id,"version":3})),
            _ => String::new(),
        };
        fs::write(
            &file,
            header
                + &(0..8)
                    .map(|n| format!("{}\n", message(n)))
                    .collect::<String>(),
        )
        .unwrap();
        let read = |extra: &[&str]| {
            let out = Command::new(env!("CARGO_BIN_EXE_tapes"))
                .args(["read", id, "--native-store", harness, "--native-root"])
                .arg(root.path())
                .arg("--native-file")
                .arg(&file)
                .args(["--items", "3", "--kind", "assistant", "--json"])
                .args(extra)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice::<Value>(&out.stdout).unwrap()
        };
        let tail = read(&[]);
        assert_eq!(tail["schema"], "tapes-recording-page/1");
        let before = read(&["--before", tail["start"].as_str().unwrap()]);
        assert_eq!(before["items"][0]["text"], "message-5-a");
        writeln!(
            OpenOptions::new().append(true).open(&file).unwrap(),
            "{}",
            message(8)
        )
        .unwrap();
        let again = read(&["--after", tail["start"].as_str().unwrap()]);
        assert_eq!(again["items"], tail["items"]);
        let appended = read(&["--after", tail["end"].as_str().unwrap()]);
        assert_eq!(appended["items"][0]["text"], "message-8-a");
        let older_forward = read(&["--after", before["start"].as_str().unwrap()]);
        assert_eq!(older_forward["items"], before["items"]);
    }
}

// Invalid reader arguments fail before discovery, and JSON refusals
// carry machine-readable cases and oversized-item gap coordinates.
#[test]
fn read_validates_before_discovery_and_serializes_typed_refusals() {
    let root = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_tapes");
    for (flags, reason) in [
        (vec!["--kind", "typo"], "kind"),
        (vec!["--before", "bad-position"], "position"),
        (vec!["--after", "bad-position"], "position"),
        (vec!["--items", "0"], "positive"),
        (vec!["--read-bytes", "0"], "scan"),
    ] {
        let out = Command::new(binary)
            .args([
                "read",
                "absent",
                "--native-store",
                "claude",
                "--native-root",
            ])
            .arg(root.path())
            .args(["--json"])
            .args(flags)
            .output()
            .unwrap();
        assert!(!out.status.success());
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(error["schema"], "tapes-recording-error/1");
        if reason == "position" {
            assert_eq!(
                error["refusal"]["kind"], "invalid-position",
                "discovery obscured position: {error}"
            );
        } else {
            assert!(
                error["refusal"]["reason"]
                    .as_str()
                    .unwrap()
                    .contains(reason),
                "discovery obscured {reason}: {error}"
            );
        }
        assert!(!error.to_string().contains("could not be completed"));
    }
    let id = "00000000-0000-0000-0000-000000000001";
    let file = root.path().join(format!("{id}.jsonl"));
    fs::write(&file,format!("{}\n",json!({"sessionId":id,"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"x".repeat(10_000)}]}}))).unwrap();
    let args = vec![
        "read",
        id,
        "--native-store",
        "claude",
        "--native-root",
        root.path().to_str().unwrap(),
        "--native-file",
        file.to_str().unwrap(),
        "--json",
    ];
    let out = Command::new(binary)
        .args(&args)
        .args(["--bytes", "100"])
        .output()
        .unwrap();
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["refusal"]["kind"], "oversized");
    assert!(error["refusal"]["needed"].as_u64().unwrap() > 10_000);
    assert!(error["refusal"]["start"].is_string() && error["refusal"]["end"].is_string());
    let skip = Command::new(binary)
        .args(&args)
        .args(["--after", error["refusal"]["end"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(skip.status.success());
    assert!(
        serde_json::from_slice::<Value>(&skip.stdout).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Conflicting directions have the same JSON error surface as other inputs.
    let both = Command::new(binary)
        .args(["read", "absent", "--before", "x", "--after", "y", "--json"])
        .output()
        .unwrap();
    assert!(!both.status.success());
    let conflict: Value = serde_json::from_slice(&both.stdout).unwrap();
    assert_eq!(conflict["refusal"]["kind"], "invalid-input");
    assert!(conflict["refusal"]["reason"]
        .as_str()
        .unwrap()
        .contains("conflicts"));
}

// An unreadable selected store is unavailable, distinct from a malformed input.
#[cfg(unix)]
#[test]
fn read_json_names_unavailable_stores() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let id = "00000000-0000-0000-0000-000000000001";
    let file = root.path().join(format!("{id}.jsonl"));
    fs::write(&file,format!("{}\n",json!({"type":"assistant","sessionId":id,"message":{"role":"assistant","content":[{"type":"text","text":"fixture"}]}}))).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0)).unwrap();
    if fs::File::open(&file).is_ok() {
        eprintln!("skipped: current user bypasses mode 000");
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .args(["read", id, "--native-store", "claude", "--native-root"])
        .arg(root.path())
        .arg("--native-file")
        .arg(&file)
        .arg("--json")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["refusal"]["kind"], "unavailable");
}
