//! An evidence file links the members of a family that recordings leave
//! silent. The reader keeps what is valid, counts what is not, and refuses a
//! file that is not evidence at all; a family adopts the children a lineage
//! record launches from a member, resolved by id, under the recording's
//! precedence, and reports the claims that disagree.

use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::attribution_evidence::{
    read_file, read_paths, EvidenceError, Malformed, Tier, TruncationReason, MAX_LINES,
    MAX_LINE_BYTES,
};
use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::family::{
    family_usage_with_backends, family_usage_with_evidence, FamilyOptions,
};
use agent_tapes_core::{usage_full_with_backends, Selection};
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

// ------------------------------------------------------------------ family

struct Store {
    root: tempfile::TempDir,
}

impl Store {
    fn new(label: &str) -> Self {
        Self {
            root: tempfile::TempDir::with_prefix(format!("tapes-family-evidence-{label}-"))
                .unwrap(),
        }
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        write_values(&self.root.path().join(relative), lines);
    }

    fn claude(&self, id: &str, lines: &[Value]) {
        self.write(
            &format!("claude/projects/-fixtures-project/{id}.jsonl"),
            lines,
        );
    }

    fn claude_subagent(&self, parent: &str, agent: &str, lines: &[Value]) {
        self.write(
            &format!("claude/projects/-fixtures-project/{parent}/subagents/agent-{agent}.jsonl"),
            lines,
        );
        fs::write(
            self.root.path().join(format!(
                "claude/projects/-fixtures-project/{parent}/subagents/agent-{agent}.meta.json"
            )),
            r#"{"agentType":"fixture-reader","description":"Read the fixture","spawnDepth":1}"#,
        )
        .unwrap();
    }

    fn codex(&self, id: &str, hour: u32, minute: u32, lines: &[Value]) {
        self.write(
            &format!("codex/2026/01/10/rollout-2026-01-10T{hour:02}-{minute:02}-00-{id}.jsonl"),
            lines,
        );
    }

    fn pi(&self, id: &str, minute: u32, lines: &[Value]) {
        self.write(
            &format!("pi/--fixtures-project--/2026-01-10T10-{minute:02}-00-000Z_{id}.jsonl"),
            lines,
        );
    }

    /// An evidence file named `name` holding a header for `producer` and
    /// `records`.
    fn evidence(&self, name: &str, producer: &str, records: &[Value]) -> PathBuf {
        let path = self.root.path().join("evidence").join(name);
        let mut lines = vec![header(producer)];
        lines.extend_from_slice(records);
        write_values(&path, &lines);
        path
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.root.path().join("claude/projects"))),
            Box::new(CodexBackend::new(self.root.path().join("codex"))),
            Box::new(PiBackend::new(self.root.path().join("pi"))),
        ]
    }
}

const AGENT: &str = "a0123456789abcdef";
const PI_CHILD: &str = "01a20000-0000-7000-8000-0000000000f3";
const SECOND_CODEX: &str = "01a10000-0000-7000-8000-0000000000f4";
const CLAUDE_CHILD: &str = "30000000-0000-4000-8000-0000000000f6";
const OTHER_PARENT: &str = "30000000-0000-4000-8000-0000000000ff";

fn stamp(hour: u32, minute: u32, second: u32) -> String {
    format!("2026-01-10T{hour:02}:{minute:02}:{second:02}Z")
}

fn claude_user(id: &str, uuid: &str, at: String, prompt: &str) -> Value {
    json!({"type": "user", "sessionId": id, "uuid": uuid, "timestamp": at,
           "cwd": "/fixtures/project", "entrypoint": "cli",
           "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}})
}

fn claude_request(id: &str, uuid: &str, request: &str, at: String, output: u64) -> Value {
    json!({"type": "assistant", "sessionId": id, "uuid": uuid, "requestId": request,
           "timestamp": at, "cwd": "/fixtures/project", "entrypoint": "cli",
           "message": {"role": "assistant", "id": format!("msg_{request}"),
                       "model": "claude-fixture", "stop_reason": "end_turn",
                       "content": [{"type": "text", "text": "ok"}],
                       "usage": {"input_tokens": 1, "output_tokens": output,
                                 "cache_read_input_tokens": 10,
                                 "cache_creation_input_tokens": 2}}})
}

/// A Claude session with a prompt (carrying `marker` first when given) and
/// two requests, the last at `end` minutes past ten.
fn claude_session(id: &str, start: u32, end: u32, marker: Option<(&str, &str)>) -> Vec<Value> {
    let prompt = match marker {
        Some((harness, parent)) => {
            format!("tapes-parent: {harness}:{parent}\nWork on the fixture.")
        }
        None => "Work on the fixture.".to_owned(),
    };
    vec![
        claude_user(id, &format!("{id}-user"), stamp(10, start, 1), &prompt),
        claude_request(
            id,
            &format!("{id}-reply"),
            &format!("req_{id}_a"),
            stamp(10, start, 30),
            5,
        ),
        claude_request(
            id,
            &format!("{id}-end"),
            &format!("req_{id}_b"),
            stamp(10, end, 0),
            6,
        ),
    ]
}

/// A Codex exec rollout starting at `hour:minute`, its prompt carrying
/// `marker` first when given.
fn codex_session(
    id: &str,
    hour: u32,
    minute: u32,
    marker: Option<(&str, &str)>,
    output: u64,
) -> Vec<Value> {
    let at = stamp(hour, minute, 0);
    let prompt = match marker {
        Some((harness, parent)) => {
            format!("tapes-parent: {harness}:{parent}\nInspect the fixture.")
        }
        None => "Inspect the fixture.".to_owned(),
    };
    let total = json!({"input_tokens": 100, "cached_input_tokens": 0, "output_tokens": output,
                       "reasoning_output_tokens": 0, "total_tokens": 100 + output});
    vec![
        json!({"timestamp": at, "type": "session_meta",
               "payload": {"id": id, "session_id": id, "timestamp": at, "cwd": "/fixtures/project",
                           "source": "exec", "originator": "codex_exec"}}),
        json!({"timestamp": at, "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
        json!({"timestamp": at, "type": "response_item",
               "payload": {"type": "message", "role": "user", "content": [
                   {"type": "input_text", "text":
                       "<environment_context>\n  <cwd>/fixtures/project</cwd>\n</environment_context>"}]}}),
        json!({"timestamp": at, "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": prompt}]}}),
        json!({"timestamp": stamp(hour, minute + 5, 0), "type": "event_msg",
               "payload": {"type": "token_count",
                           "info": {"total_token_usage": total, "last_token_usage": total,
                                    "model_context_window": 272000}}}),
    ]
}

fn pi_session(id: &str, minute: u32, marker: Option<(&str, &str)>, output: u64) -> Vec<Value> {
    let prompt = match marker {
        Some((harness, parent)) => {
            format!("tapes-parent: {harness}:{parent}\nInspect the fixture.")
        }
        None => "Inspect the fixture.".to_owned(),
    };
    vec![
        json!({"type": "session", "version": 3, "id": id, "timestamp": stamp(10, minute, 0),
               "cwd": "/fixtures/project"}),
        json!({"type": "message", "id": "p-user", "parentId": null,
               "timestamp": stamp(10, minute, 1),
               "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}}),
        json!({"type": "message", "id": "p-reply", "parentId": "p-user",
               "timestamp": stamp(10, minute, 10),
               "message": {"role": "assistant", "provider": "fixture-provider",
                           "model": "fixture-model", "responseId": "response-1",
                           "stopReason": "stop",
                           "usage": {"input": 20, "output": output, "cacheRead": 3,
                                     "cacheWrite": 4, "totalTokens": 27 + output,
                                     "cost": {"total": 0.5}},
                           "content": [{"type": "text", "text": "ok"}]}}),
    ]
}

/// A Claude lead running from 10:00 to 10:40 and nothing else.
fn store_with_lead(label: &str) -> Store {
    let store = Store::new(label);
    store.claude(LEAD, &claude_session(LEAD, 0, 40, None));
    store
}

fn family_of(store: &Store, root: &str, continuations: bool, files: Option<&[PathBuf]>) -> Value {
    let read = files.map(|files| read_paths(files).unwrap());
    let family = family_usage_with_evidence(
        &store.backends(),
        Selection::Id(root),
        FamilyOptions {
            continuations,
            prices: None,
        },
        read.as_deref(),
    )
    .unwrap();
    serde_json::to_value(family).unwrap()
}

fn family(store: &Store, files: Option<&[PathBuf]>) -> Value {
    family_of(store, LEAD, false, files)
}

fn disjoint_total(figure: &Value) -> u64 {
    let disjoint = &figure["disjoint"];
    let writes = disjoint["cache_write"]
        .as_object()
        .map(|writes| writes.values().filter_map(Value::as_u64).sum::<u64>())
        .unwrap_or(0);
    ["uncached_input", "cache_read", "output"]
        .iter()
        .map(|name| disjoint[name].as_u64().unwrap_or(0))
        .sum::<u64>()
        + writes
}

fn partition_total(family: &Value) -> u64 {
    family["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(disjoint_total)
        .sum()
}

fn full(store: &Store, id: &str) -> u64 {
    let figure = usage_full_with_backends(&store.backends(), Selection::Id(id)).unwrap();
    disjoint_total(&serde_json::to_value(figure).unwrap())
}

fn ids(family: &Value) -> Vec<&str> {
    family["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect()
}

fn member<'a>(family: &'a Value, id: &str) -> &'a Value {
    family["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == id)
        .unwrap_or_else(|| panic!("{id} is not a member: {family:#}"))
}

/// Claim: a Claude lead's markerless Codex delegate, and that delegate's
/// markerless Pi child, join the family through lineage records alone, both
/// with link `evidence`, each naming the file, the producer, and the record's
/// label and subject; the totals equal the sum of the members' own `usage
/// --full` figures. Rejects a family that ignores the file, one that adopts
/// the first generation only, and totals that differ from their members' sum.
#[test]
fn two_generations_join_through_lineage_records_alone() {
    let store = store_with_lead("generations");
    store.codex(
        CODEX_CHILD,
        10,
        10,
        &codex_session(CODEX_CHILD, 10, 10, None, 9),
    );
    store.pi(PI_CHILD, 11, &pi_session(PI_CHILD, 11, None, 11));
    // The grandchild's record comes first: adoption follows membership, not
    // the order of the file.
    let mut grandchild = lineage(("codex", CODEX_CHILD), ("pi", PI_CHILD), "declared");
    grandchild["label"] = json!("review");
    let mut child = lineage(("claude", LEAD), ("codex", CODEX_CHILD), "bound");
    child["label"] = json!("nightly-review");
    child["subject"] = json!("auth-rework");
    let file = store.evidence("lineage.jsonl", PRODUCER, &[grandchild, child]);

    let alone = family(&store, None);
    assert_eq!(ids(&alone), vec![LEAD], "{alone:#}");
    assert!(alone.get("evidence").is_none(), "{alone:#}");

    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert_eq!(family["schema"], "tapes-family-usage/2");
    assert_eq!(
        ids(&family),
        vec![LEAD, CODEX_CHILD, PI_CHILD],
        "{family:#}"
    );
    for (id, parent_harness, parent) in [
        (CODEX_CHILD, "claude", LEAD),
        (PI_CHILD, "codex", CODEX_CHILD),
    ] {
        let row = member(&family, id);
        assert_eq!(row["link"], "evidence");
        assert_eq!(row["role"], "delegate");
        assert_eq!(row["parent"]["id"], parent);
        assert_eq!(row["parent"]["harness"], parent_harness);
        assert_eq!(row["evidence"]["file"], file.display().to_string());
        assert_eq!(row["evidence"]["producer"], PRODUCER);
    }
    let row = member(&family, CODEX_CHILD);
    assert_eq!(row["evidence"]["tier"], "bound");
    assert_eq!(row["evidence"]["label"], "nightly-review");
    assert_eq!(row["evidence"]["subject"], "auth-rework");
    assert_eq!(member(&family, PI_CHILD)["evidence"]["tier"], "declared");

    let summed = full(&store, LEAD) + full(&store, CODEX_CHILD) + full(&store, PI_CHILD);
    assert_eq!(partition_total(&family), summed, "{family:#}");
    assert_eq!(family["totals"]["sessions"], 3);
    let used = &family["evidence"]["files"][0];
    assert_eq!(used["adopted"], 2);
    assert_eq!(family["evidence"]["looked_up"], 2);
    assert_eq!(family["evidence"]["conflicts"], json!([]));
    assert_eq!(family["evidence"]["unresolved"], json!([]));
}

/// Claim: a lineage child whose recording began after every member's interval
/// ended still joins, because it is looked up by id in its harness's store.
/// Rejects resolving the child through the time-windowed listing.
#[test]
fn a_child_that_began_after_every_interval_still_joins() {
    let store = store_with_lead("late");
    let late = "01a10000-0000-7000-8000-0000000000f5";
    store.codex(late, 11, 30, &codex_session(late, 11, 30, None, 3));
    let file = store.evidence(
        "late.jsonl",
        PRODUCER,
        &[lineage(("claude", LEAD), ("codex", late), "bound")],
    );
    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert_eq!(ids(&family), vec![LEAD, late], "{family:#}");
    assert_eq!(member(&family, late)["link"], "evidence");
}

/// A lead with two launch-marked delegates (a Codex one and a Pi one) and a
/// markerless Codex session, `SECOND_CODEX`.
fn store_with_delegates(label: &str) -> Store {
    let store = store_with_lead(label);
    store.codex(
        CODEX_CHILD,
        10,
        10,
        &codex_session(CODEX_CHILD, 10, 10, Some(("claude", LEAD)), 9),
    );
    store.pi(
        PI_CHILD,
        11,
        &pi_session(PI_CHILD, 11, Some(("claude", LEAD)), 11),
    );
    store
}

fn conflict<'a>(family: &'a Value, id: &str) -> &'a Value {
    family["evidence"]["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|conflict| conflict["session"]["id"] == id)
        .unwrap_or_else(|| panic!("no conflict names {id}: {family:#}"))
}

fn claimed_parents(conflict: &Value) -> Vec<(&str, &str)> {
    conflict["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|claim| {
            (
                claim["source"]["kind"].as_str().unwrap(),
                claim["parent"]["id"].as_str().unwrap(),
            )
        })
        .collect()
}

/// Claim: a session whose own marker names member D, and a lineage record
/// naming member LEAD, is counted once under D, and the conflict names both
/// claims and where each came from. Rejects a double count, a session moved
/// under the file's parent, and a silent choice.
#[test]
fn a_recording_outranks_a_file_and_the_disagreement_is_reported() {
    let store = store_with_delegates("outranks");
    store.pi(
        SECOND_CODEX,
        12,
        &pi_session(SECOND_CODEX, 12, Some(("codex", CODEX_CHILD)), 4),
    );
    let file = store.evidence(
        "claims.jsonl",
        PRODUCER,
        &[lineage(("claude", LEAD), ("pi", SECOND_CODEX), "bound")],
    );
    let family = family(&store, Some(std::slice::from_ref(&file)));
    let rows = ids(&family);
    assert_eq!(rows.iter().filter(|id| **id == SECOND_CODEX).count(), 1);
    let row = member(&family, SECOND_CODEX);
    assert_eq!(row["link"], "launch-marker");
    assert_eq!(row["parent"]["id"], CODEX_CHILD);
    assert!(row.get("evidence").is_none());
    let conflict = conflict(&family, SECOND_CODEX);
    assert_eq!(conflict["placed_under"]["id"], CODEX_CHILD);
    assert_eq!(
        claimed_parents(conflict),
        vec![("recording", CODEX_CHILD), ("evidence", LEAD)]
    );
    assert_eq!(conflict["claims"][0]["source"]["link"], "launch-marker");
    assert_eq!(
        conflict["claims"][1]["source"]["file"],
        file.display().to_string()
    );
    assert_eq!(conflict["claims"][1]["source"]["producer"], PRODUCER);
    assert_eq!(family["evidence"]["files"][0]["conflicting"], 1);
    assert_eq!(family["totals"]["sessions"], 4);
}

/// Claim: a session whose marker names a parent that is not a member does not
/// join through a lineage record naming a member, and the conflict is
/// reported with no placement. Rejects letting a file override a recording
/// whose parent lies outside the family.
#[test]
fn a_recorded_parent_outside_the_family_is_not_overridden_by_a_file() {
    let store = store_with_delegates("outside");
    store.codex(
        SECOND_CODEX,
        10,
        20,
        &codex_session(SECOND_CODEX, 10, 20, Some(("claude", OTHER_PARENT)), 4),
    );
    let file = store.evidence(
        "claims.jsonl",
        PRODUCER,
        &[lineage(("claude", LEAD), ("codex", SECOND_CODEX), "bound")],
    );
    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert!(!ids(&family).contains(&SECOND_CODEX), "{family:#}");
    let conflict = conflict(&family, SECOND_CODEX);
    assert!(conflict.get("placed_under").is_none());
    assert_eq!(
        claimed_parents(conflict),
        vec![("recording", OTHER_PARENT), ("evidence", LEAD)]
    );
    assert_eq!(family["evidence"]["files"][0]["conflicting"], 1);
}

/// Claim: a markerless session two lineage records place under different
/// members is counted once, under the parent that became a member first
/// whatever the order of the file, and the conflict names both records.
/// Rejects counting it under both parents and a choice that goes unreported.
#[test]
fn two_records_naming_different_parents_count_the_session_once() {
    let store = store_with_delegates("two-records");
    store.codex(
        SECOND_CODEX,
        10,
        20,
        &codex_session(SECOND_CODEX, 10, 20, None, 4),
    );
    let first = store.evidence(
        "a.jsonl",
        "first-producer/1",
        &[lineage(
            ("codex", CODEX_CHILD),
            ("codex", SECOND_CODEX),
            "declared",
        )],
    );
    let second = store.evidence(
        "b.jsonl",
        "second-producer/1",
        &[lineage(("pi", PI_CHILD), ("codex", SECOND_CODEX), "bound")],
    );
    let family = family(&store, Some(&[first.clone(), second.clone()]));
    let rows = ids(&family);
    assert_eq!(rows.iter().filter(|id| **id == SECOND_CODEX).count(), 1);
    let row = member(&family, SECOND_CODEX);
    assert_eq!(row["link"], "evidence");
    // A listing is oldest first by last activity: the Pi delegate's precedes
    // the Codex one's, so it joined first, though the first file names the
    // Codex one.
    assert_eq!(row["parent"]["id"], PI_CHILD);
    assert_eq!(row["evidence"]["producer"], "second-producer/1");
    let conflict = conflict(&family, SECOND_CODEX);
    assert_eq!(
        claimed_parents(conflict),
        vec![("evidence", CODEX_CHILD), ("evidence", PI_CHILD)]
    );
    assert_eq!(family["totals"]["sessions"], 4);
}

/// Claim: a lineage record naming a session that is already a member under
/// the same parent changes nothing and conflicts with nothing. Rejects
/// reporting agreement as a conflict and counting the session again.
#[test]
fn a_record_that_agrees_with_the_recording_changes_nothing() {
    let store = store_with_delegates("agreement");
    let file = store.evidence(
        "agree.jsonl",
        PRODUCER,
        &[
            lineage(("claude", LEAD), ("codex", CODEX_CHILD), "bound"),
            lineage(("claude", LEAD), ("pi", PI_CHILD), "declared"),
        ],
    );
    let baseline = family(&store, None);
    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert_eq!(ids(&family), ids(&baseline));
    assert_eq!(family["totals"], baseline["totals"]);
    assert_eq!(family["evidence"]["conflicts"], json!([]));
    let used = &family["evidence"]["files"][0];
    assert_eq!(
        (used["agreed"].clone(), used["adopted"].clone()),
        (json!(2), json!(0))
    );
    assert_eq!(member(&family, CODEX_CHILD)["link"], "launch-marker");
}

/// Claim: a Claude subagent recording stays under the session whose
/// directory keeps it, whatever a lineage record says, and the disagreement
/// is reported. Rejects moving a subagent under a delegate.
#[test]
fn a_subagent_directory_outranks_a_file() {
    let store = store_with_delegates("subagent");
    let mut user = claude_user(LEAD, "agent-user", stamp(10, 5, 0), "Read the fixture.");
    let mut reply = claude_request(LEAD, "agent-reply", "req_agent", stamp(10, 5, 30), 7);
    for record in [&mut user, &mut reply] {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(AGENT);
    }
    store.claude_subagent(LEAD, AGENT, &[user, reply]);
    let subagent = format!("{LEAD}::{AGENT}");
    let file = store.evidence(
        "claims.jsonl",
        PRODUCER,
        &[lineage(
            ("codex", CODEX_CHILD),
            ("claude", &subagent),
            "declared",
        )],
    );
    let family = family(&store, Some(std::slice::from_ref(&file)));
    let row = member(&family, &subagent);
    assert_eq!(row["link"], "subagent-recording");
    assert_eq!(row["parent"]["id"], LEAD);
    assert_eq!(
        claimed_parents(conflict(&family, &subagent)),
        vec![("recording", LEAD), ("evidence", CODEX_CHILD)]
    );
}

/// Claim: a markerless Claude delegate joins through a record, as a session
/// of any harness does. Rejects an adoption that resolves Codex and Pi
/// children only.
#[test]
fn a_markerless_claude_delegate_joins_through_a_record() {
    let store = store_with_lead("claude-child");
    store.claude(CLAUDE_CHILD, &claude_session(CLAUDE_CHILD, 6, 8, None));
    let file = store.evidence(
        "claude.jsonl",
        PRODUCER,
        &[lineage(("claude", LEAD), ("claude", CLAUDE_CHILD), "bound")],
    );
    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert_eq!(ids(&family), vec![LEAD, CLAUDE_CHILD], "{family:#}");
    assert_eq!(member(&family, CLAUDE_CHILD)["link"], "evidence");
    assert_eq!(
        partition_total(&family),
        full(&store, LEAD) + full(&store, CLAUDE_CHILD)
    );
}

/// Claim: a lineage child that no installed store holds is listed as
/// unresolved with its parent, file, producer and why, and adds no member and
/// no consumption. Rejects an invented member and a record dropped without a
/// word.
#[test]
fn a_child_no_store_holds_is_unresolved_and_adds_nothing() {
    let store = store_with_lead("unresolved");
    let absent = "01a10000-0000-7000-8000-0000000000aa";
    let file = store.evidence(
        "gone.jsonl",
        PRODUCER,
        &[
            lineage(("claude", LEAD), ("codex", absent), "bound"),
            lineage(("claude", LEAD), ("opencode", "ses_gone"), "declared"),
        ],
    );
    let baseline = family(&store, None);
    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert_eq!(ids(&family), vec![LEAD]);
    assert_eq!(family["totals"], baseline["totals"]);
    let unresolved = family["evidence"]["unresolved"].as_array().unwrap();
    assert_eq!(unresolved.len(), 2, "{family:#}");
    assert_eq!(unresolved[0]["child"]["id"], absent);
    assert_eq!(unresolved[0]["parent"]["id"], LEAD);
    assert_eq!(unresolved[0]["file"], file.display().to_string());
    assert_eq!(unresolved[0]["producer"], PRODUCER);
    assert_eq!(unresolved[0]["reason"], "not-found");
    assert_eq!(unresolved[1]["reason"], "harness-not-installed");
    assert_eq!(family["evidence"]["files"][0]["unresolved"], 2);
}

/// Claim: malformed lines and unknown kinds in a file are reported in the
/// family's evidence with their line numbers while the file's valid records
/// still apply. Rejects a file that applies nothing past its first bad line.
#[test]
fn a_file_with_bad_lines_still_applies_its_valid_records() {
    let store = store_with_lead("malformed");
    store.codex(
        CODEX_CHILD,
        10,
        10,
        &codex_session(CODEX_CHILD, 10, 10, None, 9),
    );
    let path = store.root.path().join("evidence/mixed.jsonl");
    write_lines(
        &path,
        &[
            header(PRODUCER).to_string(),
            "{".to_owned(),
            lineage(("claude", LEAD), ("codex", CODEX_CHILD), "bound").to_string(),
            lineage(("claude", LEAD), ("claude", LEAD), "bound").to_string(),
            json!({"kind": "mystery"}).to_string(),
        ],
    );
    let family = family(&store, Some(std::slice::from_ref(&path)));
    assert_eq!(ids(&family), vec![LEAD, CODEX_CHILD], "{family:#}");
    let used = &family["evidence"]["files"][0];
    assert_eq!(
        used["malformed"],
        json!([{"line": 2, "reason": "not-an-object"},
               {"line": 4, "reason": "child-is-parent"}])
    );
    assert_eq!(used["unknown_kinds"], json!({"mystery": 1}));
    assert_eq!(used["adopted"], 1);
}

/// Claim: a binding record is validated and counted in the family's evidence
/// and consumed by nothing: the family is the one it is without the file.
/// Rejects a binding that adds members or moves a figure.
#[test]
fn a_binding_is_counted_and_not_consumed() {
    let store = store_with_delegates("binding");
    let file = store.evidence(
        "binding.jsonl",
        PRODUCER,
        &[
            json!({"kind": "binding", "session": {"harness": "codex", "id": CODEX_CHILD},
                  "work": {"label": "auth-rework"}, "tier": "bound",
                  "role": "implementer"}),
        ],
    );
    let baseline = family(&store, None);
    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert_eq!(ids(&family), ids(&baseline));
    assert_eq!(family["totals"], baseline["totals"]);
    assert_eq!(family["partitions"], baseline["partitions"]);
    let used = &family["evidence"]["files"][0];
    assert_eq!(used["binding"], json!({"read": 1, "used": 1}));
    assert_eq!(used["lineage"], json!({"read": 0, "used": 0}));
}

/// Claim: naming no evidence leaves the family as the recordings alone state
/// it, with no evidence section and no evidence on any row. Rejects output
/// that changes when nothing is named.
#[test]
fn a_family_without_evidence_carries_none() {
    let store = store_with_delegates("none");
    let plain = serde_json::to_value(
        family_usage_with_backends(
            &store.backends(),
            Selection::Id(LEAD),
            FamilyOptions::default(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(plain, family(&store, None));
    assert!(plain.get("evidence").is_none());
    assert!(plain["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|session| session.get("evidence").is_none()));
}

const EARLIER: &str = "30000000-0000-4000-8000-0000000000e1";
const CONTINUING: &str = "30000000-0000-4000-8000-0000000000e2";

/// A session and the one it continued: the continuing recording opens with a
/// compact boundary naming a record of the earlier one.
fn store_with_continuation(label: &str) -> Store {
    let store = Store::new(label);
    store.claude(
        EARLIER,
        &[
            claude_user(EARLIER, "earlier-user", stamp(10, 0, 1), "Start."),
            claude_request(EARLIER, "earlier-reply", "req_earlier", stamp(10, 0, 30), 4),
        ],
    );
    store.claude(
        CONTINUING,
        &[
            json!({"type": "system", "subtype": "compact_boundary",
                   "logicalParentUuid": "earlier-reply", "uuid": "boundary", "parentUuid": null,
                   "sessionId": CONTINUING, "timestamp": stamp(10, 20, 0),
                   "cwd": "/fixtures/project", "content": "Conversation compacted"}),
            claude_request(
                CONTINUING,
                "continuing-reply",
                "req_continuing",
                stamp(10, 21, 0),
                8,
            ),
        ],
    );
    store.codex(
        CODEX_CHILD,
        10,
        10,
        &codex_session(CODEX_CHILD, 10, 10, None, 9),
    );
    store
}

/// Claim: a markerless child claimed both by a session and by the session it
/// continued is one member with no conflict, because the two are one
/// conversation across its recordings; it joins under whichever became a
/// member first. A pair of parents the family has not linked by continuation
/// still conflicts, so the same pair with continuations off reports it.
/// Rejects a reported conflict between a conversation's recordings and a
/// double count.
#[test]
fn parents_that_are_one_conversation_do_not_conflict() {
    let store = store_with_continuation("conversation");
    // The earlier session's record comes first in the file.
    let file = store.evidence(
        "claims.jsonl",
        PRODUCER,
        &[
            lineage(("claude", EARLIER), ("codex", CODEX_CHILD), "bound"),
            lineage(("claude", CONTINUING), ("codex", CODEX_CHILD), "bound"),
        ],
    );
    let linked = family_of(&store, CONTINUING, true, Some(std::slice::from_ref(&file)));
    assert_eq!(linked["continuations"][0]["status"], "linked", "{linked:#}");
    assert_eq!(
        ids(&linked),
        vec![CONTINUING, EARLIER, CODEX_CHILD],
        "{linked:#}"
    );
    assert_eq!(member(&linked, CODEX_CHILD)["parent"]["id"], CONTINUING);
    assert_eq!(linked["evidence"]["conflicts"], json!([]), "{linked:#}");
    let used = &linked["evidence"]["files"][0];
    assert_eq!(
        (used["adopted"].clone(), used["agreed"].clone()),
        (json!(1), json!(1))
    );

    let unlinked = family_of(&store, CONTINUING, false, Some(std::slice::from_ref(&file)));
    assert_eq!(
        ids(&unlinked),
        vec![CONTINUING, CODEX_CHILD],
        "{unlinked:#}"
    );
    assert_eq!(
        claimed_parents(conflict(&unlinked, CODEX_CHILD)),
        vec![("evidence", EARLIER), ("evidence", CONTINUING)]
    );
}

/// Claim: a disagreement between two claimed parents that are both outside
/// the family is not the family's, so a file that holds sessions the family
/// does not is read without reporting their conflicts. Rejects reporting
/// every disagreement a file holds.
#[test]
fn a_conflict_between_non_members_is_not_reported() {
    let store = store_with_lead("outsiders");
    store.codex(
        CODEX_CHILD,
        10,
        10,
        &codex_session(CODEX_CHILD, 10, 10, None, 9),
    );
    let file = store.evidence(
        "everything.jsonl",
        PRODUCER,
        &[
            lineage(("claude", OTHER_PARENT), ("codex", CODEX_CHILD), "bound"),
            lineage(("claude", EARLIER), ("codex", CODEX_CHILD), "declared"),
            lineage(("claude", OTHER_PARENT), ("codex", SECOND_CODEX), "bound"),
            lineage(("claude", EARLIER), ("codex", SECOND_CODEX), "bound"),
        ],
    );
    let family = family(&store, Some(std::slice::from_ref(&file)));
    assert_eq!(ids(&family), vec![LEAD], "{family:#}");
    assert_eq!(family["evidence"]["conflicts"], json!([]), "{family:#}");
    assert_eq!(family["evidence"]["unresolved"], json!([]), "{family:#}");
    assert_eq!(family["evidence"]["files"][0]["not_applied"], 4);
    assert_eq!(family["evidence"]["looked_up"], 0);
}
