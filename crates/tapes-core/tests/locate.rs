//! `locate` tells a frozen source that grew from one that changed, and
//! resolves each cited workset reference to exactly one outcome.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::lane_file::{LANE_MARKER, MARGIN_MARKER};
use agent_tapes_core::locate::{
    locate_with_backends, read_manifest, read_refs, read_refs_from, Locate, Located, Outcome,
    RefLine, SourceState, MAX_REF_LINE_BYTES, MAX_REPORT_TEXT_BYTES, MAX_TEXT_BYTES,
};
use agent_tapes_core::model::TurnKind;
use agent_tapes_core::workset::{build_with_backends, write_manifest, MIN_LANE_BYTES};
use agent_tapes_core::Selection;

const CODEX_FIXTURE: &str =
    "rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl";
const CODEX_ID: &str = "00000000-0000-0000-0000-000000000001";
const CLAUDE_ID: &str = "50000000-0000-4000-8000-000000000001";

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(harness)
}

fn scratch(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("tapes-locate-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

/// A Codex store holding a copy of the fixture recording under its real name.
fn codex_store(tag: &str) -> (PathBuf, PathBuf) {
    let root = scratch(tag);
    let store = root.join("store");
    fs::create_dir_all(&store).unwrap();
    fs::copy(
        fixtures("codex").join(CODEX_FIXTURE),
        store.join(CODEX_FIXTURE),
    )
    .unwrap();
    (root, store)
}

fn freeze(backend: Box<dyn Backend>, id: &str, bundle: &Path) {
    let workset =
        build_with_backends(&[backend], Selection::Id(id), MIN_LANE_BYTES, Some(bundle)).unwrap();
    write_manifest(bundle, &workset).unwrap();
}

fn locate(backend: Box<dyn Backend>, bundle: &Path, refs: &str, text_bytes: usize) -> Locate {
    let refs_path = bundle.join("refs.jsonl");
    fs::write(&refs_path, refs).unwrap();
    let workset = read_manifest(bundle).unwrap();
    let refs = read_refs(&refs_path).unwrap();
    locate_with_backends(&[backend], &workset, &refs, text_bytes).unwrap()
}

fn codex(store: &Path) -> Box<dyn Backend> {
    Box::new(CodexBackend::new(store.to_owned()))
}

#[test]
fn an_unchanged_workset_checks_unchanged_and_resolves() {
    let (root, store) = codex_store("unchanged");
    let bundle = root.join("bundle");
    freeze(codex(&store), CODEX_ID, &bundle);
    let report = locate(codex(&store), &bundle, "{\"ref\":\"s0#0\"}\n", 0);
    assert_eq!(
        report.sources[0].state,
        SourceState::Unchanged,
        "{report:?}"
    );
    assert!(!report.stale);
    assert_eq!(report.references[0].outcome, Outcome::Resolved);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_appended_source_has_grown_and_a_rewritten_one_has_changed() {
    let (root, store) = codex_store("grown");
    let bundle = root.join("bundle");
    freeze(codex(&store), CODEX_ID, &bundle);
    let recording = store.join(CODEX_FIXTURE);
    let frozen = fs::metadata(&recording).unwrap().len();
    fs::OpenOptions::new()
        .append(true)
        .open(&recording)
        .unwrap()
        .write_all(
            b"{\"timestamp\":\"2026-01-01T10:00:08Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"An appended request.\"}]}}\n",
        )
        .unwrap();
    let report = locate(codex(&store), &bundle, "{\"ref\":\"s0#0\"}\n", 0);
    let check = &report.sources[0];
    assert_eq!(check.state, SourceState::Grown, "{report:?}");
    assert!(check.source_length.unwrap() > frozen);
    assert!(!report.stale);
    assert_eq!(
        report.references[0].outcome,
        Outcome::Resolved,
        "{report:?}"
    );

    // One byte inside the frozen prefix, rewritten in place.
    let mut bytes = fs::read(&recording).unwrap();
    let at = bytes
        .windows(8)
        .position(|window| window == b"fixture.")
        .unwrap();
    bytes[at] = b'F';
    fs::write(&recording, bytes).unwrap();
    let bundle_again = root.join("bundle");
    let report = locate(codex(&store), &bundle_again, "{\"ref\":\"s0#0\"}\n", 0);
    assert_eq!(report.sources[0].state, SourceState::Changed, "{report:?}");
    assert!(report.stale);
    assert_eq!(report.references[0].outcome, Outcome::SourceChanged);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_shortened_source_has_changed() {
    let (root, store) = codex_store("shortened");
    let bundle = root.join("bundle");
    freeze(codex(&store), CODEX_ID, &bundle);
    let recording = store.join(CODEX_FIXTURE);
    let bytes = fs::read(&recording).unwrap();
    fs::write(&recording, &bytes[..bytes.len() / 2]).unwrap();
    let report = locate(codex(&store), &bundle, "", 0);
    assert_eq!(report.sources[0].state, SourceState::Changed, "{report:?}");
    fs::remove_dir_all(root).unwrap();
}

/// A Claude store with one session long enough for several minimum-size
/// lanes, a 20 KiB operator turn, and a 5 KiB reasoning block.
fn claude_store(root: &Path) -> PathBuf {
    let project = root.join("claude/project");
    fs::create_dir_all(&project).unwrap();
    let mut lines = Vec::new();
    let mut parent = serde_json::Value::Null;
    let mut push = |uuid: String, kind: &str, message: serde_json::Value| {
        let mut record = serde_json::json!({
                "type": kind,
                "sessionId": CLAUDE_ID,
                "uuid": uuid,
                "parentUuid": parent,
                "timestamp": "2026-01-01T11:00:00Z",
                "cwd": "/fixtures/project",
                "message": message,
        });
        if kind == "user" {
            record["promptSource"] = "typed".into();
        }
        lines.push(record.to_string());
        parent = serde_json::Value::String(uuid);
    };
    for exchange in 0..12 {
        let ask = if exchange == 3 {
            "q".repeat(20 * 1024)
        } else {
            format!("request {exchange} {}", "r".repeat(1500))
        };
        push(
            format!("user-{exchange}"),
            "user",
            serde_json::json!({"role": "user", "content": ask}),
        );
        let mut content = vec![
            serde_json::json!({"type": "text", "text": format!("reply {exchange} {}", "a".repeat(1500))}),
        ];
        if exchange == 5 {
            content.insert(
                0,
                serde_json::json!({"type": "thinking", "thinking": "t".repeat(5 * 1024), "signature": "fixture"}),
            );
        }
        push(
            format!("assistant-{exchange}"),
            "assistant",
            serde_json::json!({"role": "assistant", "model": "claude-fixture", "content": content}),
        );
    }
    fs::write(
        project.join(format!("{CLAUDE_ID}.jsonl")),
        lines.join("\n") + "\n",
    )
    .unwrap();
    root.join("claude")
}

fn claude(store: &Path) -> Box<dyn Backend> {
    Box::new(ClaudeBackend::new(store.to_owned()))
}

fn outcome(report: &Locate, line: usize) -> &Located {
    report
        .references
        .iter()
        .find(|entry| entry.line == line)
        .unwrap()
}

#[test]
fn each_reference_gets_exactly_one_outcome() {
    let root = scratch("refs");
    let store = claude_store(&root);
    let bundle = root.join("bundle");
    freeze(claude(&store), CLAUDE_ID, &bundle);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.join("workset.json")).unwrap()).unwrap();
    let lanes = manifest["sources"][0]["lanes"].as_array().unwrap();
    assert!(lanes.len() >= 3, "{lanes:?}");
    let turns = manifest["sources"][0]["coverage"]["turns"]
        .as_u64()
        .unwrap();
    let second = lanes[1]["id"].as_str().unwrap();
    let second_first = lanes[1]["first_turn"].as_u64().unwrap();
    let third_first = lanes[2]["first_turn"].as_u64().unwrap();

    // A margin turn, taken from the second lane's own file.
    let lane_file = fs::read_to_string(bundle.join(lanes[1]["file"].as_str().unwrap())).unwrap();
    let margin = lane_file
        .split_once(MARGIN_MARKER)
        .unwrap()
        .1
        .split_once(LANE_MARKER)
        .unwrap()
        .0;
    let margin_ref = margin
        .lines()
        .find_map(|line| line.strip_prefix("## "))
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .to_owned();

    let refs = [
        format!("{{\"ref\":\"s0#{second_first}\",\"lane\":\"{second}\",\"claim\":\"ignored\"}}"),
        "{\"ref\":\"s9#0\"}".to_owned(),
        format!("{{\"ref\":\"s0#{turns}\"}}"),
        format!("{{\"ref\":\"s0#{third_first}\",\"lane\":\"{second}\"}}"),
        format!("{{\"ref\":\"{margin_ref}\",\"lane\":\"{second}\"}}"),
        String::new(),
        "not json".to_owned(),
        "{\"lane\":\"s0-0\"}".to_owned(),
        "{\"ref\":\"s0#0\"}".to_owned(),
    ]
    .join("\n");
    let report = locate(claude(&store), &bundle, &refs, 0);
    assert_eq!(report.sources[0].state, SourceState::Unchanged);

    let resolved = outcome(&report, 1);
    assert_eq!(resolved.outcome, Outcome::Resolved, "{resolved:?}");
    assert_eq!(resolved.lane.as_deref(), Some(second));
    assert!(resolved.record_ref.is_some());
    assert!(resolved.text.is_none());
    let first = outcome(&report, 9);
    assert_eq!(first.outcome, Outcome::Resolved);
    assert_eq!(first.kind, Some(TurnKind::Operator));
    assert_eq!(first.lane.as_deref(), Some("s0-0"));

    assert_eq!(outcome(&report, 2).outcome, Outcome::Unknown);
    assert_eq!(outcome(&report, 3).outcome, Outcome::Unknown);
    assert_eq!(outcome(&report, 4).outcome, Outcome::OutsideLane);
    assert_eq!(
        outcome(&report, 5).outcome,
        Outcome::OutsideLane,
        "{margin_ref}"
    );
    for line in [6, 7, 8] {
        assert_eq!(
            outcome(&report, line).outcome,
            Outcome::Malformed,
            "line {line}"
        );
    }
    assert_eq!(report.counts[&Outcome::Malformed], 3);
    assert_eq!(report.references.len(), 9);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn text_bytes_returns_the_whole_turn_past_the_projection_cap_and_truncates_at_the_bound() {
    let root = scratch("text");
    let store = claude_store(&root);
    let bundle = root.join("bundle");
    freeze(claude(&store), CLAUDE_ID, &bundle);

    // Find the reasoning turn and the long operator turn by resolving all.
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.join("workset.json")).unwrap()).unwrap();
    let turns = manifest["sources"][0]["coverage"]["turns"]
        .as_u64()
        .unwrap();
    let all: String = (0..turns)
        .map(|ordinal| format!("{{\"ref\":\"s0#{ordinal}\"}}\n"))
        .collect();
    let report = locate(claude(&store), &bundle, &all, 1024 * 1024);
    let reasoning = report
        .references
        .iter()
        .find(|entry| entry.kind == Some(TurnKind::Reasoning))
        .unwrap();
    assert_eq!(reasoning.text_truncated, Some(false));
    let text = reasoning.text.as_deref().unwrap();
    assert!(
        text.matches('t').count() >= 5 * 1024,
        "{} bytes",
        text.len()
    );
    assert!(!text.contains("bytes omitted"));
    let long = report
        .references
        .iter()
        .find(|entry| entry.text_bytes.is_some_and(|bytes| bytes > 20 * 1024))
        .unwrap();
    assert_eq!(long.kind, Some(TurnKind::Operator));
    assert_eq!(long.text_truncated, Some(false));

    let report = locate(claude(&store), &bundle, &all, 100);
    for entry in &report.references {
        assert!(entry.text.as_ref().unwrap().len() <= 100);
    }
    let cut = report
        .references
        .iter()
        .find(|entry| entry.kind == Some(TurnKind::Reasoning))
        .unwrap();
    assert_eq!(cut.text_truncated, Some(true));
    assert!(cut.text_bytes.unwrap() > 5 * 1024);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_claude_child_read_through_its_root_checks_unchanged() {
    let root = scratch("child");
    let bundle = root.join("bundle");
    freeze(claude(&fixtures("claude")), "session-claude", &bundle);
    let report = locate(
        claude(&fixtures("claude")),
        &bundle,
        "{\"ref\":\"s1#0\"}\n",
        0,
    );
    assert!(report.sources.len() >= 2, "{report:?}");
    for check in &report.sources {
        assert_eq!(check.state, SourceState::Unchanged, "{report:?}");
    }
    assert_eq!(report.references[0].outcome, Outcome::Resolved);
    fs::remove_dir_all(root).unwrap();
}

/// A refs line tens of MiB long is malformed, and is skipped without ever
/// being held past the line bound.
#[test]
fn an_overlong_refs_line_is_skipped_in_bounded_memory() {
    use std::io::Read as _;
    let long = std::io::repeat(b'x').take(40 * 1024 * 1024);
    let input = std::io::BufReader::new(
        std::io::Cursor::new(b"{\"ref\":\"s0#1\"}\n".to_vec())
            .chain(long)
            .chain(std::io::Cursor::new(b"\n{\"ref\":\"s0#2\"}\n".to_vec())),
    );
    let read = read_refs_from(input).unwrap();
    assert!(
        read.peak_line_bytes <= MAX_REF_LINE_BYTES + 1,
        "held {} bytes for one line",
        read.peak_line_bytes
    );
    assert!(matches!(read.lines[0], RefLine::Cited { line: 1, .. }));
    assert!(matches!(read.lines[1], RefLine::Malformed { line: 2 }));
    assert!(
        matches!(read.lines[2], RefLine::Cited { line: 3, .. }),
        "{:?}",
        read.lines
    );
}

/// Twenty citations of one turn longer than a mebibyte, each asking for a
/// mebibyte of text, return no more than the report's text budget.
#[test]
fn the_text_budget_is_charged_per_citation() {
    let root = scratch("budget");
    let project = root.join("claude/project");
    fs::create_dir_all(&project).unwrap();
    let record = serde_json::json!({
        "type": "user", "sessionId": CLAUDE_ID, "uuid": "user-1", "parentUuid": null,
        "promptSource": "typed", "timestamp": "2026-01-01T11:00:00Z", "cwd": "/fixtures/project",
        "message": {"role": "user", "content": "w".repeat(1536 * 1024)},
    });
    fs::write(
        project.join(format!("{CLAUDE_ID}.jsonl")),
        format!("{record}\n"),
    )
    .unwrap();
    let store = root.join("claude");
    let bundle = root.join("bundle");
    freeze(claude(&store), CLAUDE_ID, &bundle);
    let refs = "{\"ref\":\"s0#0\"}\n".repeat(20);
    let report = locate(claude(&store), &bundle, &refs, MAX_TEXT_BYTES);
    let total: usize = report
        .references
        .iter()
        .map(|entry| entry.text.as_ref().map_or(0, String::len))
        .sum();
    assert!(
        total <= MAX_REPORT_TEXT_BYTES,
        "returned {total} bytes of text"
    );
    assert!(report
        .references
        .iter()
        .all(|entry| entry.text_truncated == Some(true)));
    assert_eq!(report.references.last().unwrap().text.as_deref(), Some(""));
    fs::remove_dir_all(root).unwrap();
}
