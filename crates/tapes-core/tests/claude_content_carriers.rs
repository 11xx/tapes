use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/content-carriers")
}

#[test]
fn attachment_fixture_records_each_carrier_and_field_state() {
    let text = std::fs::read_to_string(
        fixture_root().join("project/20000000-0000-4000-8000-000000000001.jsonl"),
    )
    .unwrap();
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for kind in [
        "file",
        "compact_file_reference",
        "instructions",
        "environment",
        "prompt_snapshot",
    ] {
        for state in ["present", "missing", "malformed"] {
            let id = format!("{kind}-{state}");
            let record = records.iter().find(|record| record["uuid"] == id).unwrap();
            assert_eq!(record["attachment"]["type"], kind);
            assert!(record.get("message").is_none());
        }
    }
    let file = &records[0]["attachment"]["content"];
    assert_eq!(file["type"], "text");
    assert!(file["file"]["content"].is_string());
    assert_eq!(file["file"]["startLine"], 3);
    assert_eq!(file["file"]["numLines"], 1);
}
