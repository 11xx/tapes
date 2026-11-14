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

