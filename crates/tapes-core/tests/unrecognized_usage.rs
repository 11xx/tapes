//! A record a reader has no reading for qualifies the figure only when it
//! carries usage-shaped fields for its harness. One without them, such as
//! the attachments Claude writes beside every session, leaves a whole figure
//! complete, and the unmapped tally still reports it.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::usage::{self, GroupBy};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

const NATIVE: [&str; 6] = [
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];
const TOKENS: [&str; 5] = ["input", "output", "reasoning", "cache_read", "cache_write"];

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-unrecognized-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn view(backend: Box<dyn Backend>, id: &str, full: bool) -> Value {
    let backends = vec![backend];
    let view = if full {
        usage_full_with_backends(&backends, Selection::Id(id)).unwrap()
    } else {
        usage_with_backends(&backends, Selection::Id(id)).unwrap()
    };
    serde_json::to_value(view).unwrap()
}

fn reasons<'a>(figure: &'a Value, counter: &str) -> Vec<&'a str> {
    figure["measurement_completeness"][counter]
        .as_array()
        .map(|reasons| reasons.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Every native counter of `figure` carries `unrecognized-usage`.
fn assert_qualified(label: &str, figure: &Value) {
    for counter in NATIVE {
        assert!(
            reasons(figure, counter).contains(&"unrecognized-usage"),
            "{label} × {counter}: {figure}"
        );
    }
}

fn assert_whole(label: &str, figure: &Value) {
    assert!(
        figure
            .get("measurement_completeness")
            .is_none_or(|completeness| {
                TOKENS
                    .iter()
                    .all(|counter| completeness.get(*counter).is_none())
            }),
        "{label}: {figure}"
    );
}

fn claude_lines(id: &str, extra: &[Value]) -> Vec<Value> {
    let mut lines = vec![
        json!({"type": "user", "sessionId": id, "uuid": "user",
               "timestamp": "2026-01-01T10:00:00Z",
               "message": {"role": "user", "content": "fixture"}}),
        json!({"type": "assistant", "sessionId": id, "requestId": "request", "uuid": "reply",
               "message": {"role": "assistant", "model": "fixture-model",
                           "content": [{"type": "text", "text": "reply"}],
                           "usage": {"input_tokens": 10, "output_tokens": 4,
                                     "cache_read_input_tokens": 3,
                                     "cache_creation_input_tokens": 5,
                                     "output_tokens_details": {"thinking_tokens": 2}}}}),
    ];
    lines.extend_from_slice(extra);
    lines
}

fn claude_view(extra: &[Value], full: bool) -> Value {
    let store = Store::new("claude");
    store.write(
        "fixture-project/session.jsonl",
        &claude_lines("session", extra),
    );
    view(Box::new(ClaudeBackend::new(&store.0)), "session", full)
}

/// Claude's usage shape is a `message.usage` object holding a token counter.
#[test]
fn claude_attachments_leave_a_whole_figure_complete() {
    let attachments: Vec<Value> = [
        "credential_org",
        "date",
        "deferred_tools_delta",
        "mcp_instructions_delta",
        "model",
        "session_context",
        "silent_turn_reminder",
        "total_tokens_reminder",
    ]
    .iter()
    .map(|kind| {
        json!({"type": "attachment", "sessionId": "session", "uuid": format!("attachment-{kind}"),
               "attachment": {"type": kind, "content": "reminder"}})
    })
    .collect();
    for full in [false, true] {
        let figure = claude_view(&attachments, full);
        assert_whole(&format!("claude full={full}"), &figure);
        assert!(figure.get("unrecognized_usage").is_none(), "{figure}");
        assert_eq!(figure["disjoint"]["total"], 22, "{figure}");
        let unmapped = &figure["read"]["unmapped"]["unrecognized"];
        if full {
            assert_eq!(unmapped["attachment/credential_org"], 1, "{figure}");
        }
    }
}

#[test]
fn a_claude_record_with_usage_and_no_reading_qualifies_the_figure() {
    let unknown = json!({"type": "mystery", "sessionId": "session", "uuid": "unknown",
                         "message": {"usage": {"input_tokens": 7, "output_tokens": 1}}});
    for full in [false, true] {
        let figure = claude_view(std::slice::from_ref(&unknown), full);
        let label = format!("claude full={full}");
        assert_qualified(&label, &figure);
        assert_eq!(figure["unrecognized_usage"], 1, "{label}: {figure}");
        // The known counters stay as the requests read them.
        assert_eq!(figure["tokens"]["input"], 10, "{label}: {figure}");
        assert!(
            figure["disjoint"].get("total").is_none(),
            "{label}: {figure}"
        );
        for category in ["uncached_input", "output"] {
            assert!(
                reasons(&figure["disjoint"], category).contains(&"unrecognized-usage"),
                "{label} × {category}: {figure}"
            );
        }
    }
}

#[test]
fn a_summary_qualifies_what_a_session_with_unrecognized_usage_adds() {
    let store = Store::new("claude-summary");
    store.write("fixture-project/clean.jsonl", &claude_lines("clean", &[]));
    store.write(
        "fixture-project/unread.jsonl",
        &claude_lines(
            "unread",
            &[
                json!({"type": "mystery", "sessionId": "unread", "uuid": "unknown",
                     "message": {"usage": {"input_tokens": 7}}}),
            ],
        ),
    );
    let backend = ClaudeBackend::new(&store.0);
    let sessions = ["clean", "unread"]
        .map(|id| backend.locate(id).unwrap().unwrap())
        .to_vec();
    let summary = serde_json::to_value(usage::aggregate(&sessions, &[GroupBy::Harness])).unwrap();
    assert_qualified("summary totals", &summary["totals"]);
    // The clean session beside it stays whole.
    let clean =
        serde_json::to_value(usage::aggregate(&sessions[..1], &[GroupBy::Harness])).unwrap();
    assert_whole("clean summary", &clean["totals"]);
}

fn pi_lines(extra: &[Value]) -> Vec<Value> {
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": "session", "timestamp": "2026-01-01T10:00:00Z"}),
        json!({"type": "message", "id": "root", "parentId": null,
               "message": {"role": "assistant", "model": "fixture-model", "content": [],
                           "usage": {"input": 10, "output": 4, "reasoning": 2,
                                     "cacheRead": 3, "cacheWrite": 5, "cost": {"total": 0.1}}}}),
    ];
    lines.extend_from_slice(extra);
    lines
}

fn pi_view(lines: &[Value], full: bool) -> Value {
    let store = Store::new("pi");
    store.write("2026-01-01T10-00-00-000Z_session.jsonl", lines);
    view(Box::new(PiBackend::new(&store.0)), "session", full)
}

/// Pi's usage shape is a `usage` object on an entry or its message holding a
/// token counter or a total cost.
#[test]
fn pi_entries_without_usage_leave_a_whole_figure_complete() {
    let custom = json!({"type": "custom", "id": "note", "parentId": "root",
                        "customType": "note", "data": {"text": "no usage here"}});
    for full in [false, true] {
        let figure = pi_view(&pi_lines(std::slice::from_ref(&custom)), full);
        assert_whole(&format!("pi full={full}"), &figure);
        assert!(figure.get("unrecognized_usage").is_none(), "{figure}");
    }
}

#[test]
fn a_pi_entry_with_usage_and_no_reading_qualifies_the_figure_and_its_consumption() {
    let unknown = json!({"type": "mystery", "id": "unknown", "parentId": "root",
                         "usage": {"input": 7, "cost": {"total": 0.2}}});
    for full in [false, true] {
        let figure = pi_view(&pi_lines(std::slice::from_ref(&unknown)), full);
        let label = format!("pi full={full}");
        assert_qualified(&label, &figure);
        assert_eq!(figure["unrecognized_usage"], 1, "{label}: {figure}");
        assert_qualified(&format!("{label} consumption"), &figure["consumption"]);
        assert!(
            figure["disjoint"].get("total").is_none(),
            "{label}: {figure}"
        );
    }
}

#[test]
fn a_pi_entry_on_an_abandoned_branch_qualifies_only_the_consumption() {
    // The unknown entry's branch is abandoned: a later entry also names the
    // root as its parent, and is the leaf.
    let lines = pi_lines(&[
        json!({"type": "mystery", "id": "unknown", "parentId": "root",
               "usage": {"input": 7}}),
        json!({"type": "message", "id": "leaf", "parentId": "root",
               "message": {"role": "assistant", "model": "fixture-model", "content": [],
                           "usage": {"input": 1, "output": 1, "reasoning": 1,
                                     "cacheRead": 1, "cacheWrite": 1, "cost": {"total": 0.1}}}}),
    ]);
    for full in [false, true] {
        let figure = pi_view(&lines, full);
        let label = format!("pi full={full}");
        assert!(
            figure["consumption"]["measurement_completeness"]["input"]
                .as_array()
                .is_some_and(|reasons| reasons.iter().any(|r| r == "unrecognized-usage")),
            "{label}: {figure}"
        );
        for counter in TOKENS {
            assert!(
                !reasons(&figure, counter).contains(&"unrecognized-usage"),
                "{label} × {counter}: {figure}"
            );
        }
    }
}

fn codex_view(extra: &[Value], full: bool) -> Value {
    let id = "70000000-0000-0000-0000-0000000000a1";
    let store = Store::new("codex");
    let mut lines = vec![
        json!({"type": "session_meta", "payload": {"id": id, "timestamp": "2026-01-01T10:00:00Z"}}),
        json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
            "total_token_usage": {"input_tokens": 30, "output_tokens": 10,
                                  "reasoning_output_tokens": 2,
                                  "cached_input_tokens": 4, "cache_write_input_tokens": 1}}}}),
    ];
    lines.extend_from_slice(extra);
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &lines,
    );
    view(Box::new(CodexBackend::new(&store.0)), id, full)
}

/// Codex's usage shape is a `payload.usage` object holding a token counter,
/// or a `payload.info` holding `total_token_usage` or `last_token_usage`,
/// on a record that is neither a `token_usage_record` nor a `token_count`.
#[test]
fn codex_records_without_usage_leave_a_whole_figure_complete() {
    let lifecycle = json!({"type": "event_msg", "payload": {"type": "task_started"}});
    let unknown = json!({"type": "event_msg", "payload": {"type": "mystery", "note": "text"}});
    for full in [false, true] {
        let figure = codex_view(&[lifecycle.clone(), unknown.clone()], full);
        assert_whole(&format!("codex full={full}"), &figure);
        assert!(figure.get("unrecognized_usage").is_none(), "{figure}");
    }
}

#[test]
fn a_codex_record_with_usage_and_no_reading_qualifies_the_figure() {
    for unknown in [
        json!({"type": "event_msg", "payload": {"type": "mystery", "usage": {"input_tokens": 7}}}),
        json!({"type": "event_msg", "payload": {"type": "mystery",
               "info": {"last_token_usage": {"input_tokens": 7}}}}),
    ] {
        for full in [false, true] {
            let figure = codex_view(std::slice::from_ref(&unknown), full);
            let label = format!("codex full={full}");
            assert_qualified(&label, &figure);
            assert_eq!(figure["unrecognized_usage"], 1, "{label}: {figure}");
        }
    }
}

#[test]
fn a_claude_family_source_with_unrecognized_usage_is_incomplete() {
    let store = Store::new("claude-family");
    let unknown = json!({"type": "mystery", "sessionId": "unread", "uuid": "unknown",
                         "message": {"usage": {"input_tokens": 7}}});
    let attachment = json!({"type": "attachment", "sessionId": "session", "uuid": "reminder",
                            "attachment": {"type": "date", "content": "today"}});
    store.write(
        "fixture-project/session.jsonl",
        &claude_lines("session", std::slice::from_ref(&attachment)),
    );
    store.write(
        "fixture-project/unread.jsonl",
        &claude_lines("unread", std::slice::from_ref(&unknown)),
    );
    let backend = ClaudeBackend::new(&store.0);
    let source = |id: &str| {
        let session = backend.locate(id).unwrap().unwrap();
        let reconciliation = backend.usage_reconciliation(&session).unwrap();
        serde_json::to_value(&reconciliation.sources[0]).unwrap()
    };
    let clean = source("session");
    assert_eq!(clean["complete"], true, "{clean}");
    assert!(clean.get("unrecognized_usage").is_none(), "{clean}");
    let unread = source("unread");
    assert_eq!(unread["complete"], false, "{unread}");
    assert_eq!(unread["unrecognized_usage"], 1, "{unread}");
}
