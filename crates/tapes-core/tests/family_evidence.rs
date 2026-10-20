//! An evidence file links the members of a family that recordings leave
//! silent. The reader keeps what is valid, counts what is not, and refuses a
//! file that is not evidence at all.

use std::fs;
use std::path::Path;

use agent_tapes_core::attribution_evidence::{
    read_file, read_paths, EvidenceError, Malformed, Tier, TruncationReason, MAX_LINES,
    MAX_LINE_BYTES,
};
use serde_json::{json, Value};

const PRODUCER: &str = "fixture-producer/1.0.0";
const LEAD: &str = "30000000-0000-4000-8000-0000000000f1";
const CODEX_CHILD: &str = "01a10000-0000-7000-8000-0000000000f2";

fn header(producer: &str) -> Value {
    json!({"schema": "tapes-attribution-evidence/1", "producer": producer,
           "written_at": "2026-01-10T12:00:00Z"})
}

fn lineage(parent: (&str, &str), child: (&str, &str), tier: &str) -> Value {
    json!({"kind": "lineage",
           "parent": {"harness": parent.0, "id": parent.1},
           "child": {"harness": child.0, "id": child.1},
           "tier": tier})
}

fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn write_values(path: &Path, lines: &[Value]) {
    let lines: Vec<String> = lines.iter().map(Value::to_string).collect();
    write_lines(path, &lines);
}

/// Claim: a malformed line, a record whose child is its parent, a record of
/// an unknown kind and an identical record are each reported by line number
/// and a fixed reason or a count, never by the line's text, while the file's
/// valid records are kept. Rejects skipping a bad line silently, aborting the
/// read at the first one, and echoing a line's text.
#[test]
fn a_file_reports_what_it_could_not_use_and_keeps_the_rest() {
    let root = tempfile::TempDir::with_prefix("tapes-evidence-reader-").unwrap();
    let path = root.path().join("lineage.jsonl");
    let good = lineage(("claude", LEAD), ("codex", CODEX_CHILD), "bound");
    let mut missing_tier = good.clone();
    missing_tier.as_object_mut().unwrap().remove("tier");
    let mut bad_instant = good.clone();
    bad_instant["child"]["id"] = json!("another-child");
    bad_instant["at"] = json!("yesterday");
    let lines = vec![
        header(PRODUCER).to_string(),
        good.to_string(),
        "not json SECRET-TEXT-NOT-TO-ECHO".to_owned(),
        lineage(("claude", LEAD), ("claude", LEAD), "bound").to_string(),
        json!({"kind": "mystery", "note": "SECRET-TEXT-NOT-TO-ECHO"}).to_string(),
        String::new(),
        missing_tier.to_string(),
        good.to_string(),
        lineage(("claude", LEAD), ("pi", "x"), "sure").to_string(),
        bad_instant.to_string(),
        json!({"kind": "binding", "session": {"harness": "claude", "id": LEAD},
               "work": {"commits": ["abc"]}, "tier": "bound"})
        .to_string(),
        json!({"kind": "binding", "session": {"harness": "claude", "id": LEAD},
               "work": {"label": "change"}, "tier": "declared",
               "span": {"from": "2026-01-10T11:00:00Z", "to": "2026-01-10T10:00:00Z"}})
        .to_string(),
        json!({"kind": "binding", "session": {"harness": "claude", "id": LEAD},
               "work": {"commits": ["0123456789abcdef0123456789abcdef01234567"]},
               "tier": "bound", "role": "implementer"})
        .to_string(),
    ];
    write_lines(&path, &lines);

    let file = read_file(&path).unwrap();
    let report = &file.report;
    assert_eq!(report.producer, PRODUCER);
    assert_eq!(report.path, path.display().to_string());
    let malformed: Vec<(usize, Malformed)> = report
        .malformed
        .iter()
        .map(|line| (line.line, line.reason))
        .collect();
    assert_eq!(
        malformed,
        vec![
            (3, Malformed::NotAnObject),
            (4, Malformed::ChildIsParent),
            (7, Malformed::MissingField),
            (9, Malformed::UnknownTier),
            (10, Malformed::InvalidInstant),
            (11, Malformed::InvalidCommits),
            (12, Malformed::SpanReversed),
        ]
    );
    assert_eq!(report.unknown_kinds.get("mystery"), Some(&1));
    assert_eq!((report.lineage.read, report.lineage.used), (2, 1));
    assert_eq!((report.binding.read, report.binding.used), (1, 1));
    assert_eq!(report.duplicates, 1);
    assert_eq!(report.truncated, None);
    assert_eq!(file.lineage.len(), 1);
    assert_eq!(file.lineage[0].tier, Tier::Bound);
    assert_eq!(file.bindings.len(), 1);
    assert_eq!(file.bindings[0].role.as_deref(), Some("implementer"));
    let shown = serde_json::to_string(report).unwrap();
    assert!(!shown.contains("SECRET-TEXT-NOT-TO-ECHO"), "{shown}");
}

/// Claim: a path that does not exist and a file whose first line is not this
/// schema's header each refuse, and the message names the path. Rejects
/// reading a file that is not evidence as an empty one, and a refusal that
/// does not say which path it concerns.
#[test]
fn a_missing_path_and_a_bad_header_refuse_naming_the_path() {
    let root = tempfile::TempDir::with_prefix("tapes-evidence-refusal-").unwrap();
    let missing = root.path().join("absent.jsonl");
    let error = read_paths(std::slice::from_ref(&missing)).unwrap_err();
    assert!(matches!(error, EvidenceError::Missing { .. }), "{error}");
    assert!(error.to_string().contains(&missing.display().to_string()));

    let wrong_schema = root.path().join("wrong.jsonl");
    write_values(
        &wrong_schema,
        &[
            json!({"schema": "tapes-attribution-evidence/2", "producer": PRODUCER}),
            lineage(("claude", LEAD), ("codex", CODEX_CHILD), "bound"),
        ],
    );
    let not_a_header = root.path().join("record-first.jsonl");
    write_values(
        &not_a_header,
        &[lineage(("claude", LEAD), ("codex", CODEX_CHILD), "bound")],
    );
    let no_producer = root.path().join("anonymous.jsonl");
    write_values(
        &no_producer,
        &[json!({"schema": "tapes-attribution-evidence/1", "producer": ""})],
    );
    let empty = root.path().join("empty.jsonl");
    fs::write(&empty, "").unwrap();
    for path in [&wrong_schema, &not_a_header, &no_producer, &empty] {
        let error = read_paths(std::slice::from_ref(path)).unwrap_err();
        assert!(
            matches!(error, EvidenceError::NotEvidence { .. }),
            "{path:?}: {error}"
        );
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
    }
}

/// Claim: a directory reads every `*.jsonl` file directly inside it in name
/// order and nothing below it, and a file in it that is not evidence refuses
/// the whole read, naming that file. Rejects a recursive read, an order that
/// follows the directory listing, and skipping a bad file silently.
#[test]
fn a_directory_reads_its_jsonl_files_in_name_order_without_recursion() {
    let root = tempfile::TempDir::with_prefix("tapes-evidence-directory-").unwrap();
    let directory = root.path().join("evidence");
    for (name, producer) in [("b.jsonl", "second/1"), ("a.jsonl", "first/1")] {
        write_values(&directory.join(name), &[header(producer)]);
    }
    fs::write(directory.join("notes.txt"), "not evidence").unwrap();
    write_values(&directory.join("nested/c.jsonl"), &[header("nested/1")]);

    let files = read_paths(std::slice::from_ref(&directory)).unwrap();
    let producers: Vec<&str> = files
        .iter()
        .map(|file| file.report.producer.as_str())
        .collect();
    assert_eq!(producers, vec!["first/1", "second/1"]);
    assert_eq!(
        files[0].report.path,
        directory.join("a.jsonl").display().to_string()
    );

    fs::write(directory.join("stray.jsonl"), "{}\n").unwrap();
    let error = read_paths(std::slice::from_ref(&directory)).unwrap_err();
    assert!(error.to_string().contains("stray.jsonl"), "{error}");
}

/// Claim: a line past the line bound and a file past the line count stop the
/// read of that file and are reported as a truncation at the first line not
/// read, keeping the records before it. Rejects a read that grows with the
/// file and one that stops without saying so.
#[test]
fn a_read_stops_at_its_bounds_and_says_where() {
    let root = tempfile::TempDir::with_prefix("tapes-evidence-bounds-").unwrap();
    let good = lineage(("claude", LEAD), ("codex", CODEX_CHILD), "bound").to_string();

    let long = root.path().join("long.jsonl");
    let wide = "x".repeat(MAX_LINE_BYTES);
    write_lines(
        &long,
        &[
            header(PRODUCER).to_string(),
            good.clone(),
            format!("{{\"kind\":\"{wide}\"}}"),
            good.clone(),
        ],
    );
    let report = read_file(&long).unwrap().report;
    let truncated = report.truncated.expect("a line past the bound truncates");
    assert_eq!(
        (truncated.line, truncated.reason),
        (3, TruncationReason::LineTooLong)
    );
    assert_eq!(report.lineage.read, 1);

    let many = root.path().join("many.jsonl");
    let mut lines = vec![header(PRODUCER).to_string()];
    lines.extend((0..MAX_LINES + 5).map(|number| {
        lineage(
            ("claude", LEAD),
            ("codex", &format!("child-{number}")),
            "bound",
        )
        .to_string()
    }));
    write_lines(&many, &lines);
    let report = read_file(&many).unwrap().report;
    let truncated = report.truncated.expect("a file past the bound truncates");
    assert_eq!(
        (truncated.line, truncated.reason),
        (MAX_LINES + 2, TruncationReason::TooManyLines)
    );
    assert_eq!(report.lineage.used, MAX_LINES);
}
