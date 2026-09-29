//! What the endings report establishes from a store, one fact per case.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::pi::PiBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::endings::{
    endings_with_backends, Coverage, EndingsReport, Fact, Incomplete, ENDINGS_SCHEMA,
};
use agent_tapes_core::model::{Role, TurnKind};
use agent_tapes_core::SessionSelection;

static STORE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A store of this test's own, removed with the session it holds.
struct Store {
    root: PathBuf,
}

impl Store {
    fn new(tag: &str) -> Self {
        let serial = STORE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "tapes-endings-{tag}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, relative: &str, lines: &[String]) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The header every Codex rollout in these stores opens with.
fn codex_header(id: &str) -> Vec<String> {
    vec![
        format!(
            r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
        ),
        r#"{"timestamp":"2026-01-01T10:00:01Z","type":"turn_context","payload":{"cwd":"/fixtures/project","model":"gpt-fixture"}}"#.to_owned(),
    ]
}

/// A Codex operator message: the conversation item the model reads.
fn codex_operator(second: u32, text: &str) -> Vec<String> {
    vec![format!(
        r#"{{"timestamp":"2026-01-01T10:00:{second:02}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
    )]
}

fn codex_assistant(second: u32, text: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-01-01T10:00:{second:02}Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}}}"#
    )
}

fn codex_call(second: u32, call_id: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-01-01T10:00:{second:02}Z","type":"response_item","payload":{{"type":"function_call","name":"fixture_tool","arguments":"{{}}","call_id":"{call_id}"}}}}"#
    )
}

fn codex_result(second: u32, call_id: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-01-01T10:00:{second:02}Z","type":"response_item","payload":{{"type":"function_call_output","call_id":"{call_id}","output":"Tool complete."}}}}"#
    )
}

/// A Codex store holding one rollout made of the given records.
fn codex_store(tag: &str, id: &str, records: Vec<String>) -> (Store, Vec<Box<dyn Backend>>) {
    let store = Store::new(tag);
    let mut lines = codex_header(id);
    lines.extend(records);
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &lines,
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.root))];
    (store, backends)
}

fn report(backends: &[Box<dyn Backend>], tail: usize, text: bool) -> EndingsReport {
    endings_with_backends(backends, &SessionSelection::default(), tail, text).unwrap()
}

fn facts(report: &EndingsReport) -> Vec<Fact> {
    report.endings[0].facts.clone()
}

#[test]
fn a_closing_assistant_turn_is_named_beside_the_record_the_store_ends_on() {
    let id = "00000000-0000-0000-0000-0000000000e1";
    let mut records = codex_operator(2, "Inspect the fixture.");
    records.push(codex_assistant(3, "Fixture inspected."));
    records.push(
        r#"{"timestamp":"2026-01-01T10:00:04Z","type":"event_msg","payload":{"type":"task_complete"}}"#
            .to_owned(),
    );
    let (_store, backends) = codex_store("assistant-close", id, records);

    let report = report(&backends, 12, false);
    assert_eq!(report.schema, ENDINGS_SCHEMA);
    assert_eq!(report.endings.len(), 1, "{:#?}", report.endings);
    assert_eq!(facts(&report), vec![Fact::AssistantClose]);

    let ending = &report.endings[0];
    assert!(ending.incomplete.is_empty(), "{:#?}", ending.incomplete);
    assert_eq!(ending.session.id, id);
    assert_eq!(ending.source.session, id);
    assert_eq!(
        ending.source.source.recorded_harness.as_deref(),
        Some("codex")
    );
    assert_eq!(ending.source.coverage, Coverage::Session);
    assert_eq!(ending.source.turn, Some(1));
    let last = ending.last_turn.as_ref().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.kind, TurnKind::Assistant);
    assert_eq!(last.ordinal, 1);
    assert_eq!(ending.last_operator.as_ref().unwrap().ordinal, 0);
    assert_eq!(ending.last_assistant.as_ref().unwrap().ordinal, 1);
    // The store ends on a record that is not a turn, and the read says so.
    assert_eq!(ending.trailing_record.as_ref().unwrap().kind, "event_msg");
    // A harness that records no relatives leaves the summary absent.
    assert!(ending.lineage.is_none(), "{:#?}", ending.lineage);
    assert!(ending.tail.is_none(), "{:#?}", ending.tail);

    // Nothing the turns carry as text reaches the report unless it is asked for.
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("Fixture inspected."), "{json}");
}

#[test]
fn a_call_the_read_never_saw_a_result_for_is_named() {
    let id = "00000000-0000-0000-0000-0000000000e2";
    let mut records = codex_operator(2, "Run the tool.");
    records.push(codex_assistant(3, "Running it."));
    records.push(codex_call(4, "call-1"));
    let (_store, backends) = codex_store("call-without-result", id, records);

    let report = report(&backends, 12, false);
    assert_eq!(facts(&report), vec![Fact::CallWithoutResult]);
    assert_eq!(
        report.endings[0].last_turn.as_ref().unwrap().kind,
        TurnKind::Tool
    );
}

/// A result later in the read means the recording kept working, whatever an
/// older call is missing.
#[test]
fn a_call_answered_later_in_the_read_is_not_named() {
    let id = "00000000-0000-0000-0000-0000000000e3";
    let mut records = codex_operator(2, "Run both tools.");
    records.push(codex_call(3, "call-1"));
    records.push(codex_call(4, "call-2"));
    records.push(codex_result(5, "call-2"));
    let (_store, backends) = codex_store("answered-later", id, records);

    let report = report(&backends, 12, false);
    assert!(
        !facts(&report).contains(&Fact::CallWithoutResult),
        "{:#?}",
        facts(&report)
    );
}

#[test]
fn results_that_no_turn_narrates_are_named() {
    let id = "00000000-0000-0000-0000-0000000000e4";
    let mut records = codex_operator(2, "Run the tool.");
    records.push(codex_assistant(3, "Running it."));
    records.push(codex_call(4, "call-1"));
    records.push(codex_result(5, "call-1"));
    let (_store, backends) = codex_store("results-without-narration", id, records);

    let report = report(&backends, 12, false);
    assert_eq!(facts(&report), vec![Fact::ResultsWithoutNarration]);
}

/// A window that dropped turns qualifies every fact drawn from what remains,
/// and the coordinate says the read covered a window rather than a session.
#[test]
fn a_window_that_omitted_turns_says_so() {
    let id = "00000000-0000-0000-0000-0000000000e5";
    let mut records = codex_operator(2, "Inspect the fixture.");
    records.push(codex_assistant(3, "Fixture inspected."));
    let (_store, backends) = codex_store("tail-window", id, records);

    let report = report(&backends, 1, false);
    let ending = &report.endings[0];
    assert_eq!(ending.incomplete, vec![Incomplete::TailWindow]);
    assert_eq!(ending.source.coverage, Coverage::Window);
    assert!(ending.truncated);
    // The window keeps the newest turns, so the ordinal is the session's own.
    assert_eq!(ending.source.turn, Some(1));
    assert_eq!(facts(&report), vec![Fact::AssistantClose]);
}

/// pi records nothing but the operator's own messages in its user role, so a
/// session ending on one ends on a request that was never answered.
#[test]
fn a_request_with_no_answer_after_it_is_named() {
    let store = Store::new("unanswered");
    store.write(
        "project/2026-01-01T10-00-00-000Z_session-endings-pi.jsonl",
        &[
            r#"{"type":"session","version":3,"id":"session-endings-pi","timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project"}"#.to_owned(),
            r#"{"type":"message","id":"user-1","parentId":null,"timestamp":"2026-01-01T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"say ok"}]}}"#.to_owned(),
        ],
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(&store.root))];

    let report = report(&backends, 12, false);
    assert_eq!(facts(&report), vec![Fact::OperatorTurnAfterAssistant]);
    let ending = &report.endings[0];
    assert_eq!(
        ending.session.source.recorded_harness.as_deref(),
        Some("pi")
    );
    assert_eq!(ending.last_turn.as_ref().unwrap().kind, TurnKind::Operator);
    assert!(ending.last_assistant.is_none());
}

/// A session ending on `/exit` ends on the harness's own command. The ending
/// is decided by the turns before it, and the command is not a request.
#[test]
fn a_command_recorded_after_the_last_exchange_is_named_as_one() {
    let store = Store::new("control-last");
    store.write(
        "project/session-endings-claude.jsonl",
        &[
            r#"{"type":"user","sessionId":"session-endings-claude","uuid":"user-1","parentUuid":null,"timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","origin":{"kind":"human"},"promptSource":"typed","message":{"role":"user","content":"Inspect the fixture."}}"#.to_owned(),
            r#"{"type":"assistant","sessionId":"session-endings-claude","uuid":"assistant-1","parentUuid":"user-1","timestamp":"2026-01-01T10:00:01Z","cwd":"/fixtures/project","message":{"role":"assistant","model":"claude-fixture","content":[{"type":"text","text":"Fixture inspected."}]}}"#.to_owned(),
            r#"{"type":"user","sessionId":"session-endings-claude","uuid":"command-exit","parentUuid":"assistant-1","timestamp":"2026-01-01T10:00:02Z","cwd":"/fixtures/project","message":{"role":"user","content":"<command-name>/exit</command-name>"}}"#.to_owned(),
        ],
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(&store.root))];

    let report = report(&backends, 12, false);
    assert_eq!(
        facts(&report),
        vec![Fact::ControlTurnLast, Fact::AssistantClose]
    );
    assert_eq!(
        report.endings[0].last_turn.as_ref().unwrap().kind,
        TurnKind::Control
    );
}

/// The parent refers to its children and never reads them: the summary counts
/// what the parent's own store records.
#[test]
fn recorded_relatives_are_counted_without_reading_a_child() {
    let parent = "00000000-0000-0000-0000-0000000000a1";
    let child = "00000000-0000-0000-0000-0000000000b2";
    let store = Store::new("lineage");
    let mut lines = codex_header(parent);
    lines.extend([
        r#"{"timestamp":"2026-01-01T10:00:02Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","call_id":"call-spawn-1","arguments":"{\"task_name\":\"worker\",\"model\":\"gpt-fixture\"}"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-spawn-1","output":"{\"task_name\":\"/root/worker\"}"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:04Z","type":"response_item","payload":{"type":"function_call","name":"wait_agent","call_id":"call-wait-1","arguments":"{}"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:05Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-wait-1","output":"{\"agents\":[{\"agent_name\":\"/root/worker\",\"agent_status\":\"completed\"}]}"}}"#.to_owned(),
    ]);
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{parent}.jsonl"),
        &lines,
    );
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-01-{child}.jsonl"),
        &[
            format!(
                r#"{{"timestamp":"2026-01-01T10:00:01Z","type":"session_meta","payload":{{"id":"{child}","cwd":"/fixtures/project","thread_source":"subagent","parent_thread_id":"{parent}","agent_path":"/root/worker"}}}}"#
            ),
            codex_assistant(6, "child-only-text"),
        ],
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.root))];

    let report = report(&backends, 12, false);
    let parent_ending = report
        .endings
        .iter()
        .find(|ending| ending.session.id == parent)
        .unwrap_or_else(|| panic!("{:#?}", report.endings));
    let lineage = parent_ending.lineage.as_ref().unwrap();
    assert_eq!(lineage.children, 1);
    assert_eq!(lineage.children_unresolved, 0);
    assert_eq!(lineage.children_by_disposition.get("completed"), Some(&1));
    assert!(lineage.parent.is_none());

    let child_ending = report
        .endings
        .iter()
        .find(|ending| ending.session.id == child)
        .unwrap();
    let child_lineage = child_ending.lineage.as_ref().unwrap();
    assert_eq!(child_lineage.parent.as_ref().unwrap().native_id, parent);
    assert_eq!(child_lineage.children, 0);
}

/// The text tail is the exchange, bounded: the harness's own records stay out
/// of it, and an entry cut at the bound says it was cut.
#[test]
fn the_text_tail_holds_the_bounded_exchange_only() {
    let id = "00000000-0000-0000-0000-0000000000e6";
    let long = "word ".repeat(200);
    let mut records = codex_operator(2, "Inspect the fixture.");
    records.push(codex_call(3, "call-1"));
    records.push(codex_result(4, "call-1"));
    records.push(codex_assistant(5, long.trim_end()));
    let (_store, backends) = codex_store("text-tail", id, records);

    let report = report(&backends, 12, true);
    let tail = report.endings[0].tail.as_ref().unwrap();
    assert_eq!(tail.len(), 2, "{tail:#?}");
    assert_eq!(tail[0].kind, TurnKind::Operator);
    assert_eq!(tail[0].text, "Inspect the fixture.");
    assert!(!tail[0].truncated);
    assert_eq!(tail[1].kind, TurnKind::Assistant);
    assert_eq!(tail[1].text.chars().count(), 400);
    assert!(tail[1].truncated);
}
