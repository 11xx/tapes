use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tapes-audited-evidence-{name}-{}",
            std::process::id()
        ));
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

fn run(root: &Path, args: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
        .args(args)
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", root.join("codex"))
        .env_remove("XDG_DATA_HOME")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap()
}

fn json_command(root: &Path, args: &[String]) -> Value {
    let output = run(root, args);
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write_jsonl(path: &Path, values: impl IntoIterator<Item = Value>) {
    let mut body = String::new();
    for value in values {
        body.push_str(&value.to_string());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn assert_content_reference(
    reference: &Value,
    parent_part: u64,
    content_part: u64,
    pointer: Option<&str>,
) {
    assert_eq!(reference["part_index"], parent_part);
    assert_eq!(reference["content_part_index"], content_part);
    match pointer {
        Some(pointer) => assert_eq!(reference["pointer"], pointer),
        None => assert!(reference.get("pointer").is_none()),
    }
}

#[test]
fn claude_coordinates_survive_show_export_and_page() {
    let root = TemporaryDirectory::new("claude");
    let project = root.path().join("home/.claude/projects/fixture");
    fs::create_dir_all(&project).unwrap();
    let id = "synthetic-claude-coordinate";
    let path = project.join(format!("{id}.jsonl"));
    write_jsonl(
        &path,
        [json!({
            "type": "assistant",
            "sessionId": id,
            "uuid": "shared-native-record",
            "timestamp": "2026-01-01T10:00:00Z",
            "message": {
                "role": "assistant",
                "model": "synthetic-model",
                "content": [
                    {"type": "text", "text": "first native block"},
                    {"type": "text", "text": "second native block"}
                ]
            }
        })],
    );

    let show = json_command(root.path(), &["show".into(), id.into(), "--json".into()]);
    assert_eq!(show["schema"], "tapes-session/9");
    let turns = show["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    for (index, turn) in turns.iter().enumerate() {
        assert_eq!(turn["native_id"], "shared-native-record");
        assert_eq!(turn["record_ref"]["part_index"], index as u64);
        assert!(turn["record_ref"].get("content_part_index").is_none());
        assert_content_reference(&turn["parts"][0]["record_ref"], index as u64, 0, None);
    }
    assert_ne!(turns[0]["record_ref"], turns[1]["record_ref"]);
    assert_ne!(
        turns[0]["parts"][0]["record_ref"],
        turns[1]["parts"][0]["record_ref"]
    );

    let bundle_root = root.path().join("bundle");
    fs::create_dir_all(&bundle_root).unwrap();
    let bundle_root_string = bundle_root.to_str().unwrap().to_owned();
    let exported = run(
        root.path(),
        &[
            "export".into(),
            id.into(),
            "--bundle".into(),
            bundle_root_string,
        ],
    );
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let bundle_json_path = fs::read_dir(&bundle_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let bundle: Value = serde_json::from_slice(&fs::read(bundle_json_path).unwrap()).unwrap();
    assert_eq!(bundle["schema"], "tapes-session/9");
    assert_content_reference(&bundle["turns"][1]["parts"][0]["record_ref"], 1, 0, None);

    let page = json_command(
        root.path(),
        &[
            "page".into(),
            id.into(),
            "--bytes".into(),
            "1024".into(),
            "--json".into(),
        ],
    );
    assert_eq!(page["schema"], "tapes-page/4");
    assert_eq!(page["read"]["projection"], page["schema"]);
    assert_eq!(page["read"]["projection_options"], json!(["transcript"]));
    assert_content_reference(&page["turns"][0]["parts"][0]["record_ref"], 0, 0, None);
}

#[test]
fn supplied_graph_coordinates_keep_source_pointers_and_part_levels() {
    let root = TemporaryDirectory::new("supplied-graph");
    let input = root.path().join("conversation.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "id": "synthetic-supplied-coordinate",
            "current_node": "b",
            "mapping": {
                "root": {"parent": null},
                "a": {
                    "parent": "root",
                    "message": {
                        "id": "native-a",
                        "author": {"role": "user"},
                        "content": {"parts": [
                            {"type": "text", "text": "a first"},
                            {"type": "text", "text": "a second"}
                        ]}
                    }
                },
                "b": {
                    "parent": "a",
                    "message": {
                        "id": "native-b",
                        "author": {"role": "assistant"},
                        "content": {"parts": [
                            {"type": "text", "text": "b only"}
                        ]}
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let input = input.to_str().unwrap().to_owned();
    let shown = json_command(
        root.path(),
        &[
            "show".into(),
            "synthetic-supplied-coordinate".into(),
            "--input".into(),
            input,
            "--input-format".into(),
            "chatgpt-exporter".into(),
            "--json".into(),
        ],
    );
    assert_eq!(shown["schema"], "tapes-session/9");
    assert_eq!(shown["turns"].as_array().unwrap().len(), 2);
    let a = shown["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|turn| turn["native_id"] == "native-a")
        .unwrap();
    assert_content_reference(
        &a["parts"][0]["record_ref"],
        a["record_ref"]["part_index"].as_u64().unwrap(),
        0,
        Some("/mapping/a/message"),
    );
    assert_content_reference(
        &a["parts"][1]["record_ref"],
        a["record_ref"]["part_index"].as_u64().unwrap(),
        1,
        Some("/mapping/a/message"),
    );
    assert_eq!(
        a["parts"][0]["record_ref"]["pointer"],
        a["record_ref"]["pointer"]
    );

    let graph_a = shown["graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == "a")
        .unwrap();
    assert_eq!(
        graph_a["message"]["record_ref"]["pointer"],
        "/mapping/a/message"
    );
    assert_eq!(
        graph_a["message"]["parts"][0]["record_ref"]["part_index"],
        graph_a["message"]["record_ref"]["part_index"]
    );
    assert_eq!(
        graph_a["message"]["parts"][1]["record_ref"]["content_part_index"],
        1
    );
}

#[test]
fn codex_history_events_and_metadata_report_their_actual_reads() {
    let root = TemporaryDirectory::new("codex-history");
    let sessions = root.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "synthetic-codex-history";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut values = vec![json!({
        "type": "session_meta",
        "timestamp": "2026-01-01T10:00:00Z",
        "payload": {"id": id, "source": "cli", "cwd": "/synthetic"}
    })];
    values.push(json!({
        "type": "response_item",
        "timestamp": "2026-01-01T10:00:01Z",
        "payload": {"type": "message", "role": "user", "content": [
            {"type": "input_text", "text": "older operator"}
        ]}
    }));
    values.push(json!({
        "type": "response_item",
        "timestamp": "2026-01-01T10:00:02Z",
        "payload": {"type": "function_call", "name": "synthetic_tool", "call_id": "call-1", "arguments": "{}"}
    }));
    values.push(json!({
        "type": "response_item",
        "timestamp": "2026-01-01T10:00:03Z",
        "payload": {"type": "function_call_output", "call_id": "call-1", "output": "ok"}
    }));
    for index in 0..5 {
        values.push(json!({"type": "padding", "index": index, "value": "x".repeat(300)}));
    }
    values.push(json!({
        "type": "event_msg",
        "timestamp": "2026-01-01T10:00:09Z",
        "payload": {"type": "user_message", "message": "older operator"}
    }));
    values.push(json!({
        "type": "event_msg",
        "timestamp": "2026-01-01T10:00:10Z",
        "payload": {"type": "token_count", "info": {
            "total_token_usage": {"input_tokens": 7, "output_tokens": 3},
            "model_context_window": 4096
        }, "rate_limits": null}
    }));
    write_jsonl(&path, values);

    let page = |bytes: &str, cursor: Option<&str>| {
        let mut args = vec!["page".into(), id.into(), "--bytes".into(), bytes.into()];
        if let Some(cursor) = cursor {
            args.push("--cursor".into());
            args.push(cursor.to_owned());
        }
        args.push("--json".into());
        json_command(root.path(), &args)
    };
    let first_small = page("1024", None);
    let small_cursor = first_small["next_cursor"].as_str().unwrap().to_owned();
    let mut cursor = Some(small_cursor.clone());
    let older_small = loop {
        let candidate = page("1024", cursor.as_deref());
        if candidate["turns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|turn| turn["text"] == "older operator")
        {
            break candidate;
        }
        cursor = candidate["next_cursor"].as_str().map(str::to_owned);
        assert!(cursor.is_some(), "history ended before the older operator");
    };
    assert_eq!(older_small["schema"], "tapes-page/4");
    assert_eq!(older_small["read"]["projection"], older_small["schema"]);
    assert!(older_small["read"]["projection_options"]
        .as_array()
        .unwrap()
        .iter()
        .any(|option| option == "opening-and-newer-provenance"));
    assert!(older_small["read"]["ranges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|range| range["kind"] == "head"));
    assert!(older_small["read"]["ranges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|range| range["kind"] == "context"));
    assert!(!older_small["read"]["context_records"]
        .as_array()
        .unwrap()
        .is_empty());
    let page_records = older_small["read"]["records"].as_array().unwrap();
    assert!(older_small["read"]["context_records"]
        .as_array()
        .unwrap()
        .iter()
        .all(|context| !page_records.iter().any(|record| record == context)));
    let older_turn = older_small["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|turn| turn["text"] == "older operator")
        .unwrap();
    assert_eq!(older_turn["kind"], "operator");
    assert!(older_small["turns"]
        .as_array()
        .unwrap()
        .iter()
        .all(|turn| turn["text"] != id));

    let first_large = page("2048", None);
    let large_cursor = first_large["next_cursor"].as_str().unwrap().to_owned();
    let mut cursor = Some(large_cursor);
    let older_large = loop {
        let candidate = page("2048", cursor.as_deref());
        if candidate["turns"]
            .as_array()
            .unwrap()
            .iter()
            .any(|turn| turn["text"] == "older operator")
        {
            break candidate;
        }
        cursor = candidate["next_cursor"].as_str().map(str::to_owned);
        assert!(cursor.is_some(), "history ended before the older operator");
    };
    let older_large_turn = older_large["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|turn| turn["text"] == "older operator")
        .unwrap();
    assert_eq!(older_turn["record_ref"], older_large_turn["record_ref"]);

    let metadata = json_command(
        root.path(),
        &[
            "metadata".into(),
            id.into(),
            "--bytes".into(),
            "1024".into(),
            "--pages".into(),
            "1".into(),
            "--json".into(),
        ],
    );
    assert_eq!(metadata["schema"], "tapes-metadata-history/4");
    assert_eq!(metadata["reads"][0]["projection"], "tapes-page/4");
    assert_eq!(
        metadata["reads"][0]["projection_options"],
        json!(["models-only"])
    );
    assert_eq!(metadata["context_bytes"], 0);
    assert!(metadata["reads"][0].get("context_records").is_none());

    let events = json_command(root.path(), &["events".into(), id.into(), "--json".into()]);
    assert_eq!(events["schema"], "tapes-events/6");
    let event_records = events["events"].as_array().unwrap();
    assert_eq!(event_records.len(), 2);
    for event in event_records {
        assert_eq!(event["record_ref"]["part_index"], 0);
        assert_eq!(event["parts"][0]["record_ref"]["part_index"], 0);
        assert_eq!(event["parts"][0]["record_ref"]["content_part_index"], 0);
    }
    assert_eq!(
        event_records[0]["pair"]["record_ref"],
        event_records[1]["record_ref"]
    );

    let shown = json_command(root.path(), &["show".into(), id.into(), "--json".into()]);
    assert_eq!(shown["schema"], "tapes-session/9");
    assert_eq!(shown["session"]["accounting"]["coverage"], "session");
    assert_eq!(shown["session"]["tokens"]["input"], 7);

    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let changed = run(
        root.path(),
        &[
            "page".into(),
            id.into(),
            "--bytes".into(),
            "1024".into(),
            "--cursor".into(),
            small_cursor,
            "--json".into(),
        ],
    );
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("source changed"));
}

#[test]
fn codex_header_coverage_subtracts_the_reached_prefix_from_tail_gaps() {
    let root = TemporaryDirectory::new("codex-head-coverage");
    let sessions = root.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "synthetic-codex-head-coverage";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut values = vec![json!({
        "type": "session_meta",
        "timestamp": "2026-01-01T10:00:00Z",
        "payload": {"id": id, "source": "cli", "cwd": "/synthetic"}
    })];
    for index in 0..1100 {
        values.push(json!({"type": "padding", "index": index, "value": "x".repeat(4096)}));
    }
    write_jsonl(&path, values);
    let shown = json_command(root.path(), &["show".into(), id.into(), "--json".into()]);
    let head = shown["read"]["ranges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|range| range["kind"] == "head")
        .unwrap();
    assert!(head["span"]["end"].as_u64().unwrap() > 0);
    assert!(shown["read"]["context_records"]
        .as_array()
        .unwrap()
        .iter()
        .any(|span| span["start"] == 0));
    for gap in shown["read"]["gaps"].as_array().unwrap() {
        let gap_start = gap["span"]["start"].as_u64().unwrap();
        let gap_end = gap["span"]["end"].as_u64().unwrap();
        let head_start = head["span"]["start"].as_u64().unwrap();
        let head_end = head["span"]["end"].as_u64().unwrap();
        assert!(
            gap_end <= head_start || gap_start >= head_end,
            "gap overlaps reached header: {gap}"
        );
    }
}

#[test]
fn overlapping_head_and_tail_reads_keep_malformed_record_evidence_in_show_and_export() {
    let root = TemporaryDirectory::new("overlap-malformed");
    let sessions = root.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "synthetic-codex-overlap-malformed";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let header = json!({
        "type": "session_meta",
        "timestamp": "2026-01-01T10:00:00Z",
        "payload": {"id": id, "source": "exec"}
    })
    .to_string()
        + "\n";
    let malformed = "not-json\n";
    let target_size = 4 * 1024 * 1024 + 100;
    let padding_prefix = "{\"padding\":\"";
    let padding_suffix = "\"}\n";
    let padding_size = target_size - header.len() - malformed.len();
    assert!(padding_size > padding_prefix.len() + padding_suffix.len());
    let padding = format!(
        "{padding_prefix}{}{padding_suffix}",
        "x".repeat(padding_size - padding_prefix.len() - padding_suffix.len())
    );
    assert_eq!(header.len() + malformed.len() + padding.len(), target_size);
    fs::write(&path, format!("{header}{malformed}{padding}")).unwrap();

    let shown = json_command(root.path(), &["show".into(), id.into(), "--json".into()]);
    assert!(shown["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note == "Skipped 1 unparseable line."));
    let malformed_gap = shown["read"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gap| gap["reason"] == "malformed-record")
        .unwrap();
    assert_eq!(
        malformed_gap["span"],
        json!({
            "start": header.len(),
            "end": header.len() + malformed.len()
        })
    );
    assert_eq!(shown["read"]["gaps"].as_array().unwrap().len(), 1);
    let head = shown["read"]["ranges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|range| range["kind"] == "head")
        .unwrap();
    let head_end = head["span"]["end"].as_u64().unwrap();
    assert!(shown["read"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|gap| gap["reason"] == "outside-configured-tail-bound")
        .all(|gap| gap["span"]["start"].as_u64().unwrap() >= head_end));

    let bundle_root = root.path().join("bundle");
    fs::create_dir_all(&bundle_root).unwrap();
    let exported = run(
        root.path(),
        &[
            "export".into(),
            id.into(),
            "--bundle".into(),
            bundle_root.to_str().unwrap().to_owned(),
        ],
    );
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let bundle_json_path = fs::read_dir(&bundle_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let bundle: Value = serde_json::from_slice(&fs::read(bundle_json_path).unwrap()).unwrap();
    assert_eq!(bundle["read"]["gaps"].as_array().unwrap().len(), 1);
    assert_eq!(bundle["read"]["gaps"][0]["reason"], "malformed-record");
}

#[test]
fn absent_native_ids_still_have_distinct_record_coordinates() {
    let root = TemporaryDirectory::new("absent-native-ids");
    let sessions = root.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "synthetic-codex-absent-native";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    write_jsonl(
        &path,
        [
            json!({
                "type": "session_meta",
                "payload": {"id": id, "source": "exec", "cwd": "/synthetic"}
            }),
            json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "first without id"}
                ]}
            }),
            json!({
                "type": "response_item",
                "payload": {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "second without id"}
                ]}
            }),
        ],
    );
    let shown = json_command(root.path(), &["show".into(), id.into(), "--json".into()]);
    let turns = shown["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns.iter().all(|turn| turn.get("native_id").is_none()));
    assert_ne!(
        turns[0]["record_ref"]["span"],
        turns[1]["record_ref"]["span"]
    );
    assert_eq!(turns[0]["record_ref"]["part_index"], 0);
    assert_eq!(turns[1]["record_ref"]["part_index"], 0);
    assert_eq!(turns[0]["parts"][0]["record_ref"]["content_part_index"], 0);
    assert_eq!(turns[1]["parts"][0]["record_ref"]["content_part_index"], 0);
}
