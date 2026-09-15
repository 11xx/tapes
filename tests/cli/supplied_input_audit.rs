use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};

fn tapes() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
}

fn run(arguments: &[String]) -> Output {
    tapes().args(arguments).output().unwrap()
}

fn successful_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn conversation(id: &str, text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!([{
        "id": id,
        "title": id,
        "current_node": "message",
        "mapping": {
            "message": {
                "parent": null,
                "message": {
                    "id": format!("{id}-message"),
                    "author": {"role": "assistant"},
                    "content": {"parts": [text]}
                }
            }
        }
    }]))
    .unwrap()
}

#[test]
fn another_input_gap_does_not_become_a_local_byte_range() {
    let root = TempRoot::new("qualified-collection-gaps");
    let valid = root.path().join("valid.json");
    let invalid = root.path().join("invalid.json");
    fs::write(&valid, conversation("known", "complete member")).unwrap();
    fs::write(&invalid, format!("{}!", " ".repeat(1_000))).unwrap();
    let mut listing = input_args("list", &valid);
    listing.extend(["--input".to_owned(), invalid.display().to_string()]);
    let listed = successful_json(run(&listing));
    let occurrence = listed["sessions"][0]["occurrence"].as_str().unwrap();
    let mut show = input_args("show", &valid);
    show.extend([
        "--input".to_owned(),
        invalid.display().to_string(),
        "--occurrence".to_owned(),
        occurrence.to_owned(),
    ]);
    let shown = successful_json(run(&show));
    assert_eq!(shown["truncated"], true);
    assert!(shown["read"]["gaps"].as_array().is_none_or(Vec::is_empty));
}

fn report(backing: Option<&str>, identity: &str, body: &str) -> Vec<u8> {
    let mut value = json!({
        "widget_session_id": identity,
        "widget_state": {
            "status": "completed",
            "report_message": {
                "author": {"role": "assistant"},
                "content": {"parts": [body]}
            }
        }
    });
    if let Some(backing) = backing {
        value["backing_conversation_id"] = Value::String(backing.to_owned());
    }
    serde_json::to_vec(&value).unwrap()
}

fn manifest(
    conversation_files: &[&str],
    library_files: &[&str],
    logical_library_members: &[&str],
    sizes: &BTreeMap<&str, usize>,
) -> Vec<u8> {
    let mut logical_files = serde_json::Map::new();
    logical_files.insert(
        "conversations.json".to_owned(),
        json!({
            "files": conversation_files,
            "shard_count": conversation_files.len(),
            "sharded": conversation_files.len() > 1
        }),
    );
    if !library_files.is_empty() {
        logical_files.insert(
            "library_files.json".to_owned(),
            json!({"files": library_files, "sharded": false}),
        );
    }
    for member in logical_library_members {
        logical_files.insert(
            (*member).to_owned(),
            json!({"files": [member], "sharded": false}),
        );
    }
    let export_files = sizes
        .iter()
        .map(|(path, size)| json!({"path": path, "size_bytes": size}))
        .collect::<Vec<_>>();
    serde_json::to_vec(&json!({
        "version": 1,
        "logical_files": logical_files,
        "export_files": export_files
    }))
    .unwrap()
}

fn archive(path: &Path, members: &[(&str, &[u8])]) {
    let file = fs::File::create(path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    for (name, body) in members {
        writer.start_file(*name, options).unwrap();
        writer.write_all(body).unwrap();
    }
    writer.finish().unwrap();
}

struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tapes-supplied-audit-{name}-{}",
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

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn input_args(command: &str, input: &Path) -> Vec<String> {
    vec![
        command.to_owned(),
        "--input".to_owned(),
        input.display().to_string(),
        "--json".to_owned(),
    ]
}

#[test]
fn native_manifest_selects_conversation_and_associated_library_members() {
    let root = TempRoot::new("manifest");
    let conversation_bytes = conversation("included", "selected body");
    let unrelated = conversation("excluded", "unrelated body");
    let library =
        br#"[{"file_id":"report.dat","origination_message_id":"included-message"}]"#.to_vec();
    let report = report(None, "report-id", "associated body");
    fs::write(
        root.path().join("conversations-000.json"),
        &conversation_bytes,
    )
    .unwrap();
    fs::write(root.path().join("unrelated.json"), &unrelated).unwrap();
    fs::write(root.path().join("library_files.json"), &library).unwrap();
    fs::write(root.path().join("report.dat"), &report).unwrap();
    fs::write(
        root.path().join("account.json"),
        conversation("account-json", "must not be parsed"),
    )
    .unwrap();
    fs::create_dir_all(root.path().join("sites")).unwrap();
    fs::write(
        root.path().join("sites/export_manifest.json"),
        conversation("nested-manifest-json", "must not be parsed"),
    )
    .unwrap();

    let mut sizes = BTreeMap::new();
    sizes.insert("conversations-000.json", conversation_bytes.len());
    sizes.insert("library_files.json", library.len());
    sizes.insert("report.dat", report.len());
    fs::write(
        root.path().join("export_manifest.json"),
        manifest(
            &["conversations-000.json"],
            &["library_files.json"],
            &["report.dat"],
            &sizes,
        ),
    )
    .unwrap();

    let directory = successful_json(run(&input_args("list", root.path())));
    let sessions = directory["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "included");
    assert!(directory["unsearched"].as_array().unwrap().is_empty());

    let mut show = input_args("show", root.path());
    show.splice(1..1, ["included".to_owned()]);
    let shown = successful_json(run(&show));
    assert_eq!(shown["artifacts"][0]["backing"], "included");
    assert_eq!(shown["artifacts"][0]["body"]["text"], "associated body");

    let zip_path = root.path().join("native-export.zip");
    let manifest_bytes = manifest(
        &["conversations-000.json"],
        &["library_files.json"],
        &["report.dat"],
        &sizes,
    );
    archive(
        &zip_path,
        &[
            ("export_manifest.json", &manifest_bytes),
            ("conversations-000.json", &conversation_bytes),
            ("unrelated.json", &unrelated),
            ("library_files.json", &library),
            ("report.dat", &report),
            ("user_settings.json", &unrelated),
            ("sites/export_manifest.json", &unrelated),
        ],
    );
    let zipped = successful_json(run(&input_args("list", &zip_path)));
    let sessions = zipped["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "included");
    assert!(zipped["unsearched"].as_array().unwrap().is_empty());
}

#[test]
fn manifest_gaps_are_explicit_and_only_an_exact_occurrence_can_be_read() {
    let root = TempRoot::new("manifest-gap");
    let body = conversation("known", "known body");
    fs::write(root.path().join("conversations-000.json"), &body).unwrap();
    let mut sizes = BTreeMap::new();
    sizes.insert("conversations-000.json", body.len());
    sizes.insert("conversations-001.json", 99);
    fs::write(
        root.path().join("export_manifest.json"),
        manifest(
            &["conversations-000.json", "conversations-001.json"],
            &[],
            &[],
            &sizes,
        ),
    )
    .unwrap();

    let listed = successful_json(run(&input_args("list", root.path())));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "known");
    assert_eq!(listed["scan_truncated"], true);
    assert!(listed["unsearched"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry.as_str().unwrap().contains("conversations-001.json")));
    let occurrence = listed["sessions"][0]["occurrence"].as_str().unwrap();

    let mut by_id = input_args("show", root.path());
    by_id.splice(1..1, ["known".to_owned()]);
    let rejected = run(&by_id);
    assert!(!rejected.status.success());
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(error.contains("incomplete"), "{error}");
    assert!(error.contains("--occurrence"), "{error}");

    let mut exact = input_args("show", root.path());
    exact.splice(1..1, ["--occurrence".to_owned(), occurrence.to_owned()]);
    let shown = successful_json(run(&exact));
    assert_eq!(shown["session"]["id"], "known");
    assert_eq!(shown["truncated"], true);
    assert!(shown["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str().unwrap().contains("conversations-001.json")));

    let mismatch_root = TempRoot::new("manifest-mismatch");
    fs::write(mismatch_root.path().join("conversations-000.json"), &body).unwrap();
    let mut mismatch_sizes = BTreeMap::new();
    mismatch_sizes.insert("conversations-000.json", body.len() + 1);
    fs::write(
        mismatch_root.path().join("export_manifest.json"),
        manifest(&["conversations-000.json"], &[], &[], &mismatch_sizes),
    )
    .unwrap();
    let mismatch = run(&input_args("list", mismatch_root.path()));
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("size mismatch"));
}

#[test]
fn incomplete_framing_is_reachable_by_occurrence_and_invalid_cursors_fail() {
    let root = TempRoot::new("framing");
    let input = root.path().join("partial.json");
    fs::write(
        &input,
        br#"[{"id":"partial","current_node":"message","mapping":{"message":{"parent":null,"message":{"id":"message","author":{"role":"assistant"},"content":{"parts":["body"]}}}}}, !"#,
    )
    .unwrap();
    let listed = successful_json(run(&input_args("list", &input)));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["scan_truncated"], true);
    let occurrence = listed["sessions"][0]["occurrence"].as_str().unwrap();

    let mut by_id = input_args("show", &input);
    by_id.splice(1..1, ["partial".to_owned()]);
    assert!(!run(&by_id).status.success());

    let mut by_title = input_args("show", &input);
    by_title.splice(1..1, ["--title".to_owned(), "partial".to_owned()]);
    let by_title = run(&by_title);
    assert!(!by_title.status.success());
    assert!(String::from_utf8_lossy(&by_title.stderr).contains("incomplete"));

    let mut exact = input_args("show", &input);
    exact.splice(1..1, ["--occurrence".to_owned(), occurrence.to_owned()]);
    let shown = successful_json(run(&exact));
    assert_eq!(shown["truncated"], true);
    assert!(!shown["read"]["gaps"].as_array().unwrap().is_empty());
    assert!(shown["notes"].as_array().unwrap().iter().any(|note| {
        note.as_str()
            .unwrap()
            .contains("structural synchronization")
    }));

    let mut invalid = input_args("list", &input);
    invalid.extend([
        "--after-occurrence".to_owned(),
        "input:v2:00:00:00:not-a-number".to_owned(),
    ]);
    let invalid = run(&invalid);
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("occurrence"));
}

#[test]
fn escaped_keys_decode_without_resynchronizing_malformed_roots() {
    let root = TempRoot::new("escaped");
    let valid = root.path().join("escaped.json");
    fs::write(
        &valid,
        br#"{"\u0063onversations":[{"context_uuid":"escaped","entries":[{"entry_uuid":"entry","query":"question","answer":"answer"}]}],"a\"b":"quoted-key","slash\\key":"backslash-key"}"#,
    )
    .unwrap();
    let listed = successful_json(run(&input_args("list", &valid)));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "escaped");

    let trailing = root.path().join("trailing.json");
    let mut trailing_bytes = fs::read(&valid).unwrap();
    trailing_bytes.extend_from_slice(b", !");
    fs::write(&trailing, trailing_bytes).unwrap();
    let listed = successful_json(run(&input_args("list", &trailing)));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["scan_truncated"], true);
    assert!(listed["unsearched"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| { entry.as_str().unwrap().contains("unexpected trailing byte") }));

    let malformed = root.path().join("malformed-key.json");
    fs::write(&malformed, br#"{"bad\uZZZZ":[]}"#).unwrap();
    let malformed = run(&input_args("list", &malformed));
    assert!(!malformed.status.success());
    assert!(String::from_utf8_lossy(&malformed.stderr).contains("invalid JSON object key"));

    let ambiguous = root.path().join("ambiguous.json");
    fs::write(
        &ambiguous,
        br#"{"context_uuid":"ambiguous","entries":[],"mapping":{}}"#,
    )
    .unwrap();
    let ambiguous = run(&input_args("list", &ambiguous));
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("both"));
}

#[test]
fn perplexity_entry_metadata_stays_on_each_entry() {
    let root = TempRoot::new("per-entry");
    let input = root.path().join("per-entry.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "conversations": [{
                "context_uuid": "per-entry",
                "context_title": "Synthetic",
                "entries": [
                    {"entry_uuid":"e1","query":"q1","answer":"a1","engine_mode":"first-mode","query_status":"FIRST","label":"first-label"},
                    {"entry_uuid":"e2","query":"q2","answer":"a2","engine_mode":"second-mode","query_status":"SECOND","label":"second-label"},
                    {"entry_uuid":"e3","query":"q3","answer":"a3","engine_mode":"","query_status":null,"label":null}
                ]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut show = input_args("show", &input);
    show.splice(1..1, ["per-entry".to_owned()]);
    let shown = successful_json(run(&show));
    assert!(shown["session"]["metadata"].get("engine").is_none());
    assert!(shown["session"]["metadata"].get("status").is_none());
    assert!(shown["session"]["metadata"].get("label").is_none());
    let turns = shown["turns"].as_array().unwrap();
    assert_eq!(turns[0]["metadata"]["engine"], "first-mode");
    assert_eq!(turns[0]["metadata"]["status"], "FIRST");
    assert_eq!(turns[0]["metadata"]["label"], "first-label");
    assert_eq!(
        turns[0]["metadata"]["source_fields"]["engine"],
        "/entries/0/engine_mode"
    );
    assert_eq!(turns[1]["metadata"]["engine"], "first-mode");
    assert_eq!(turns[2]["metadata"]["engine"], "second-mode");
    assert_eq!(turns[2]["metadata"]["status"], "SECOND");
    assert_eq!(turns[2]["metadata"]["label"], "second-label");
    assert_eq!(turns[4]["metadata"]["engine"], "");
    assert!(turns[4]["metadata"].get("status").is_none());
    assert_eq!(turns[0]["ts"], Value::Null);
    assert!(turns[1].get("ts").is_none());
}

#[test]
fn homogeneous_content_parts_use_the_shared_bound_and_empty_coverage() {
    let root = TempRoot::new("parts");
    let input = root.path().join("parts.json");
    let parts = (0..129)
        .map(|index| format!("part-{index}"))
        .collect::<Vec<_>>();
    let body = json!([{
        "id": "parts",
        "current_node": "message",
        "mapping": {
            "message": {
                "parent": null,
                "message": {
                    "id": "message",
                    "author": {"role": "assistant"},
                    "content": {"parts": parts}
                }
            }
        }
    }]);
    fs::write(&input, serde_json::to_vec(&body).unwrap()).unwrap();
    let mut show = input_args("show", &input);
    show.splice(1..1, ["parts".to_owned()]);
    let shown = successful_json(run(&show));
    assert_eq!(shown["turns"][0]["parts"].as_array().unwrap().len(), 128);
    assert_eq!(shown["turns"][0]["coverage"]["retained_parts"], 128);
    assert_eq!(shown["turns"][0]["coverage"]["omitted_parts"], 1);
    assert_eq!(
        shown["turns"][0]["coverage"]["omitted_reason"],
        "part-count-bound"
    );
    assert!(!shown["turns"][0]["text"]
        .as_str()
        .unwrap()
        .contains("part-128"));

    let empty = root.path().join("empty.json");
    let empty_body = json!([{
        "id": "empty",
        "messages": [{"role":"assistant","content":{"parts":[]}}]
    }]);
    fs::write(&empty, serde_json::to_vec(&empty_body).unwrap()).unwrap();
    let mut empty_show = input_args("show", &empty);
    empty_show.splice(1..1, ["empty".to_owned()]);
    let shown = successful_json(run(&empty_show));
    assert!(shown["turns"].as_array().unwrap().is_empty());
}

#[test]
fn source_and_decoded_budgets_cover_archive_metadata_and_reader_io() {
    let root = TempRoot::new("budgets");
    let zip_path = root.path().join("metadata-heavy.zip");
    let selected = conversation("selected", "small");
    let mut members = vec![("selected.json", selected.as_slice())];
    let unrelated = b"ignored";
    let names = (0..32)
        .map(|index| format!("unrelated-member-{index:02}.txt"))
        .collect::<Vec<_>>();
    for name in &names {
        members.push((name.as_str(), unrelated.as_slice()));
    }
    archive(&zip_path, &members);
    let mut args = input_args("list", &zip_path);
    args.extend(["--scan-bytes".to_owned(), "256".to_owned()]);
    let output = run(&args);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("scan budget"));

    let jsonl = root.path().join("decoded-bound.jsonl");
    let first = serde_json::to_vec(&json!({
        "id": "first",
        "messages": [{"role":"assistant","content":"first"}]
    }))
    .unwrap();
    let mut bytes = first;
    bytes.extend(std::iter::repeat_n(b' ', 1024));
    fs::write(&jsonl, bytes).unwrap();
    let mut args = input_args("list", &jsonl);
    args.extend(["--decoded-bytes".to_owned(), "256".to_owned()]);
    let listed = successful_json(run(&args));
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["scan_truncated"], true);
    assert!(listed["unsearched"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry.as_str().unwrap().contains("decoded")));
}

fn dated_conversation(id: &str, update_time: u64) -> Value {
    json!({
        "id": id,
        "title": id,
        "update_time": update_time,
        "current_node": "message",
        "mapping": {
            "message": {
                "parent": null,
                "message": {
                    "id": format!("{id}-message"),
                    "author": {"role": "assistant"},
                    "content": {"parts": [id]}
                }
            }
        }
    })
}

fn listed_ids(listing: &Value) -> Vec<&str> {
    listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect()
}

fn with(mut arguments: Vec<String>, extra: &[&str]) -> Vec<String> {
    arguments.extend(extra.iter().map(|argument| (*argument).to_owned()));
    arguments
}

#[test]
fn supplied_listing_orders_before_its_limit_and_continues_in_that_order() {
    let root = TempRoot::new("ordering");
    let input = root.path().join("ordered.json");
    let records = json!([
        dated_conversation("older", 1_000),
        dated_conversation("newer", 3_000),
        dated_conversation("middle", 2_000),
    ]);
    fs::write(&input, serde_json::to_vec(&records).unwrap()).unwrap();

    let all = successful_json(run(&input_args("list", &input)));
    assert_eq!(listed_ids(&all), ["newer", "middle", "older"]);

    let first = successful_json(run(&with(input_args("list", &input), &["--limit", "1"])));
    assert_eq!(listed_ids(&first), ["newer"]);
    let cursor = first["sessions"][0]["occurrence"].as_str().unwrap();
    let next = successful_json(run(&with(
        input_args("list", &input),
        &["--limit", "1", "--after-occurrence", cursor],
    )));
    assert_eq!(listed_ids(&next), ["middle"]);

    let oldest = successful_json(run(&with(
        input_args("list", &input),
        &["--sort", "oldest", "--limit", "1"],
    )));
    assert_eq!(listed_ids(&oldest), ["older"]);
    let cursor = oldest["sessions"][0]["occurrence"].as_str().unwrap();
    let rest = successful_json(run(&with(
        input_args("list", &input),
        &["--sort", "oldest", "--after-occurrence", cursor],
    )));
    assert_eq!(listed_ids(&rest), ["middle", "newer"]);
}

#[test]
fn a_conversation_without_a_native_id_is_a_gap_rather_than_an_invented_identity() {
    let root = TempRoot::new("missing-id");
    let input = root.path().join("missing-id.json");
    let mut anonymous = dated_conversation("anonymous", 1_000);
    anonymous.as_object_mut().unwrap().remove("id");
    let records = json!([anonymous, dated_conversation("named", 2_000)]);
    fs::write(&input, serde_json::to_vec(&records).unwrap()).unwrap();
    let listed = successful_json(run(&input_args("list", &input)));
    assert_eq!(listed_ids(&listed), ["named"]);
    assert!(listed["unsearched"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry
            .as_str()
            .unwrap()
            .contains("no native conversation id")));

    let exporter = root.path().join("exporter.json");
    let body = json!({"messages": [{"role": "assistant", "content": "body"}]});
    fs::write(&exporter, serde_json::to_vec(&body).unwrap()).unwrap();
    let rejected = run(&input_args("list", &exporter));
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("no native conversation id"));
}

#[test]
fn a_record_without_a_native_id_does_not_block_lookup_by_id() {
    let root = TempRoot::new("identity-coverage");
    let input = root.path().join("missing-id.json");
    let mut anonymous = dated_conversation("anonymous", 1_000);
    anonymous.as_object_mut().unwrap().remove("id");
    let records = json!([anonymous, dated_conversation("named", 2_000)]);
    fs::write(&input, serde_json::to_vec(&records).unwrap()).unwrap();
    let show = |input: &Path, selector: &[&str], extra: &[&str]| {
        let mut arguments = input_args("show", input);
        arguments.splice(1..1, selector.iter().map(|argument| (*argument).to_owned()));
        run(&with(arguments, extra))
    };

    let shown = successful_json(show(&input, &["named"], &[]));
    assert_eq!(shown["session"]["id"], "named");

    let absent = show(&input, &["absent"], &[]);
    assert!(!absent.status.success());
    let stderr = String::from_utf8_lossy(&absent.stderr);
    assert!(stderr.contains("session absent was not found"), "{stderr}");

    // The ID-less record may still carry the title being asked for.
    let titled = show(&input, &["--title", "named"], &[]);
    assert!(!titled.status.success());
    let stderr = String::from_utf8_lossy(&titled.stderr);
    assert!(stderr.contains("incomplete lookup"), "{stderr}");

    // A skipped record was never read, so its ID may be the one asked for.
    let skipped = root.path().join("skipped.json");
    let mut oversized = dated_conversation("oversized", 1_000);
    oversized["mapping"]["message"]["message"]["content"]["parts"] = json!(["x".repeat(4_096)]);
    let records = json!([oversized, dated_conversation("named", 2_000)]);
    fs::write(&skipped, serde_json::to_vec(&records).unwrap()).unwrap();
    let refused = show(&skipped, &["named"], &["--record-bytes", "1024"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("cannot prove that session named is unique"),
        "{stderr}"
    );
}

#[test]
fn a_named_input_that_does_not_exist_is_an_error() {
    let root = TempRoot::new("missing-path");
    let missing = root.path().join("absent.json");
    let mut show = input_args("show", &missing);
    show.splice(1..1, ["absent".to_owned()]);
    for arguments in [input_args("list", &missing), show] {
        let output = run(&arguments);
        assert!(!output.status.success(), "{arguments:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("does not exist"), "{error}");
    }
}

#[test]
fn an_unreached_id_under_incomplete_discovery_is_not_reported_absent() {
    let root = TempRoot::new("resident-unreached");
    let input = root.path().join("many.json");
    let records = (0..50_u64)
        .map(|index| {
            let mut record = dated_conversation(&format!("c{index:03}"), 1_000 + index);
            record["mapping"]["message"]["message"]["content"]["parts"] = json!(["x".repeat(200)]);
            record
        })
        .collect::<Vec<_>>();
    fs::write(&input, serde_json::to_vec(&records).unwrap()).unwrap();

    let listed = successful_json(run(&with(
        input_args("list", &input),
        &["--resident-bytes", "4096", "--limit", "100"],
    )));
    let ids = listed_ids(&listed);
    assert!(!ids.is_empty());
    assert!(!ids.contains(&"c049"));

    let mut show = with(input_args("show", &input), &["--resident-bytes", "4096"]);
    show.splice(1..1, ["c049".to_owned()]);
    let output = run(&show);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("incomplete"), "{error}");
    assert!(!error.contains("was not found"), "{error}");
}

#[test]
fn empty_and_null_perplexity_fields_neither_request_nor_close() {
    let root = TempRoot::new("perplexity-null");
    let input = root.path().join("unanswered.json");
    let body = json!({"conversations": [{
        "context_uuid": "unanswered",
        "entries": [
            {"entry_uuid": "e1", "query": "asked", "answer": null},
            {"entry_uuid": "e2", "query": "", "answer": ""}
        ]
    }]});
    fs::write(&input, serde_json::to_vec(&body).unwrap()).unwrap();

    let mut show = input_args("show", &input);
    show.splice(1..1, ["unanswered".to_owned()]);
    let shown = successful_json(run(&show));
    let turns = shown["turns"].as_array().unwrap();
    let kinds = turns
        .iter()
        .map(|turn| turn["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["operator", "unknown", "unknown", "unknown"]);
    assert_eq!(turns[1]["parts"][0]["native_kind"], "null");
    assert_eq!(turns[2]["parts"][0]["kind"], "text");

    let endings = successful_json(run(&input_args("endings", &input)));
    assert_eq!(
        endings["endings"][0]["facts"],
        json!(["operator-turn-after-assistant"])
    );
}

#[test]
fn records_from_a_member_that_fails_verification_are_withheld() {
    let root = TempRoot::new("checksum");
    let zip_path = root.path().join("corrupt.zip");
    let mut writer = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    writer.start_file("kept.json", stored).unwrap();
    writer
        .write_all(&conversation("kept", "intact member"))
        .unwrap();
    let damaged = serde_json::to_vec(&json!([
        {"id": "first-damaged", "messages": [{"role": "assistant", "content": "one"}]},
        {"id": "second-damaged", "messages": [{"role": "assistant", "content": "two"}]}
    ]))
    .unwrap();
    writer.start_file("damaged.json", stored).unwrap();
    writer.write_all(&damaged).unwrap();
    writer.finish().unwrap();

    // Alter one stored byte without breaking the JSON, so only the checksum
    // can tell the parsed records are not the archived ones.
    let mut bytes = fs::read(&zip_path).unwrap();
    let at = bytes
        .windows(b"second-damaged".len())
        .position(|window| window == b"second-damaged")
        .unwrap();
    bytes[at] = b'S';
    fs::write(&zip_path, bytes).unwrap();

    let listed = successful_json(run(&input_args("list", &zip_path)));
    assert_eq!(listed_ids(&listed), ["kept"]);
    assert_eq!(listed["scan_truncated"], true);
    assert!(listed["unsearched"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry.as_str().unwrap().contains("failed verification")));
}
