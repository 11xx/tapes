//! What a brief carries into a continuation, one fact per case.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};
use tapes_core::backend::claude::ClaudeBackend;
use tapes_core::backend::codex::CodexBackend;
use tapes_core::backend::Backend;
use tapes_core::brief::{Brief, BRIEF_SCHEMA};
use tapes_core::model::TurnKind;
use tapes_core::Selection;

static STORE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A store of this test's own, removed with the session it holds.
struct Store {
    root: PathBuf,
}

impl Store {
    fn new(tag: &str) -> Self {
        let serial = STORE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("tapes-brief-{tag}-{}-{serial}", std::process::id()));
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

fn codex_header(id: &str, directory: &str) -> Vec<String> {
    vec![
        format!(
            r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{directory}"}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-01-01T10:00:01Z","type":"turn_context","payload":{{"cwd":"{directory}","model":"gpt-fixture"}}}}"#
        ),
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

/// A Codex store holding one rollout made of the given records.
fn codex_store(
    tag: &str,
    id: &str,
    directory: &str,
    records: Vec<String>,
) -> (Store, Vec<Box<dyn Backend>>) {
    let store = Store::new(tag);
    let mut lines = codex_header(id, directory);
    lines.extend(records);
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &lines,
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.root))];
    (store, backends)
}

fn brief(backends: &[Box<dyn Backend>], id: &str, tail: usize) -> Brief {
    tapes_core::brief_with_backends(backends, Selection::Id(id), tail).unwrap()
}

fn strip_record_refs(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("record_ref");
            for value in object.values_mut() {
                strip_record_refs(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_record_refs(value);
            }
        }
        _ => {}
    }
}

fn strip_content_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let had_parts = object.remove("parts").is_some();
            object.remove("content");
            if had_parts {
                object.remove("coverage");
            }
            for value in object.values_mut() {
                strip_content_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_content_fields(value);
            }
        }
        _ => {}
    }
}

/// The whole object, so every member a continuation reads is pinned: the
/// coordinate, the working set, the ending, both kinds of handle, and the
/// bounded tail.
#[test]
fn a_brief_states_where_the_session_stopped_and_what_it_left_open() {
    let id = "00000000-0000-0000-0000-0000000000c1";
    let mut records = codex_operator(2, "Inspect the fixture.");
    records.push(codex_assistant(3, "Spawning a worker."));
    records.extend([
        r#"{"timestamp":"2026-01-01T10:00:04Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","call_id":"call-spawn-1","arguments":"{\"task_name\":\"worker\",\"model\":\"gpt-fixture\"}"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:05Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-spawn-1","output":"{\"task_name\":\"/root/worker\"}"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:06Z","type":"response_item","payload":{"type":"function_call","name":"fixture_tool","arguments":"{}","call_id":"call-1"}}"#.to_owned(),
    ]);
    let (_store, backends) = codex_store("whole", id, "/fixtures/project", records);

    let value = serde_json::to_value(brief(&backends, id, 12)).unwrap();
    let mut comparable = value.clone();
    let read = comparable.as_object_mut().unwrap().remove("read").unwrap();
    let text_tail = comparable
        .as_object_mut()
        .unwrap()
        .remove("text_tail")
        .unwrap();
    strip_record_refs(&mut comparable);
    strip_content_fields(&mut comparable);
    assert_eq!(read["ranges"].as_array().unwrap().len(), 1);
    assert!(read["records"].as_array().unwrap().len() >= 7);
    assert_eq!(text_tail["returned"], 2);
    let source = json!({
        "kind": "installed-recording",
        "origin": "codex",
        "recorded_harness": "codex",
        "representation": "codex-recording",
        "producer": "codex",
        "location": {
            "locator": _store.root.join(format!(
                "2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"
            )).display().to_string(),
        },
    });
    assert_eq!(
        comparable,
        json!({
            "schema": BRIEF_SCHEMA,
            "session": {
                "id": id,
                "source": source.clone(),
                "model": { "id": "gpt-fixture" },
                "derived_title": "Inspect the fixture.",
                "started_at": "2026-01-01T10:00:00Z",
                "last_activity_at": "2026-01-01T10:00:06Z",
                "directory": "/fixtures/project",
            },
            "source": {
                "source": source,
                "session": id,
                "ts": "2026-01-01T10:00:06Z",
                "turn": 4,
                "schema": "tapes-endings/6",
                "coverage": "session",
            },
            "working_set": {
                "directory": "/fixtures/project",
                "directory_exists": false,
            },
            "ending": {
                "last_turn": { "role": "tool", "kind": "tool", "ordinal": 4, "ts": "2026-01-01T10:00:06Z" },
                "last_operator": { "ordinal": 0, "ts": "2026-01-01T10:00:02Z" },
                "last_assistant": { "ordinal": 1, "ts": "2026-01-01T10:00:03Z" },
                "facts": ["call-without-result"],
                "incomplete": [],
            },
            "in_flight": {
                "calls_without_result": [
                    {
                        "ordinal": 4,
                        "name": "fixture_tool",
                        "call_id": "call-1",
                        "ts": "2026-01-01T10:00:06Z",
                        "arguments": { "chars": 2, "preview": "{}" },
                    }
                ],
                "children": [
                    {
                        "reference": "/root/worker",
                        "role": "worker",
                        "resolved": false,
                        "spawned_at": "2026-01-01T10:00:04Z",
                    }
                ],
            },
            "tail": [
                {
                    "ordinal": 0,
                    "kind": "operator",
                    "role": "user",
                    "ts": "2026-01-01T10:00:02Z",
                    "text": "Inspect the fixture.",
                    "truncated": false,
                },
                {
                    "ordinal": 1,
                    "kind": "assistant",
                    "role": "assistant",
                    "ts": "2026-01-01T10:00:03Z",
                    "text": "Spawning a worker.",
                    "truncated": false,
                }
            ],
            "truncated": false,
        })
    );
}

/// A working directory that is gone is a fact a continuation acts on, so it
/// is stated rather than left blank, and nothing about a commit is invented
/// for it.
#[test]
fn a_recorded_directory_that_is_gone_is_stated_as_a_fact() {
    let id = "00000000-0000-0000-0000-0000000000c2";
    let (_store, backends) = codex_store(
        "gone",
        id,
        "/fixtures/removed",
        vec![codex_assistant(2, "Finished.")],
    );

    let value = serde_json::to_value(brief(&backends, id, 12)).unwrap();
    assert_eq!(value["working_set"]["directory"], "/fixtures/removed");
    assert_eq!(value["working_set"]["directory_exists"], false);
    assert!(
        value["working_set"].get("git").is_none(),
        "{}",
        value["working_set"]
    );
}

/// A directory the recording names and the machine still holds is reported as
/// present, whatever a repository under it says.
#[test]
fn a_recorded_directory_that_is_present_is_reported_as_present() {
    let id = "00000000-0000-0000-0000-0000000000c3";
    let store = Store::new("present");
    let directory = store.root.join("project");
    fs::create_dir_all(&directory).unwrap();
    let mut lines = codex_header(id, &directory.display().to_string());
    lines.push(codex_assistant(2, "Finished."));
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &lines,
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.root))];

    let value = serde_json::to_value(brief(&backends, id, 12)).unwrap();
    assert_eq!(value["working_set"]["directory_exists"], true);
}

/// The tail is the exchange and nothing else: the harness's own records and
/// the agent's tool traffic stay out of it, and an entry cut at the bound
/// says it was cut.
#[test]
fn the_tail_holds_the_bounded_exchange_only() {
    let long = "word ".repeat(200);
    let store = Store::new("tail");
    store.write(
        "project/session-brief-claude.jsonl",
        &[
            r#"{"type":"user","sessionId":"session-brief-claude","uuid":"user-1","parentUuid":null,"timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","origin":{"kind":"human"},"promptSource":"typed","message":{"role":"user","content":"Inspect the fixture."}}"#.to_owned(),
            format!(
                r#"{{"type":"assistant","sessionId":"session-brief-claude","uuid":"assistant-1","parentUuid":"user-1","timestamp":"2026-01-01T10:00:01Z","cwd":"/fixtures/project","message":{{"role":"assistant","model":"claude-fixture","content":[{{"type":"text","text":"{}"}}]}}}}"#,
                long.trim_end()
            ),
            r#"{"type":"user","sessionId":"session-brief-claude","uuid":"command-exit","parentUuid":"assistant-1","timestamp":"2026-01-01T10:00:02Z","cwd":"/fixtures/project","message":{"role":"user","content":"<command-name>/exit</command-name>"}}"#.to_owned(),
        ],
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(&store.root))];

    let brief = brief(&backends, "session-brief-claude", 12);
    assert_eq!(brief.schema, BRIEF_SCHEMA);
    let kinds = brief
        .tail
        .iter()
        .map(|entry| entry.kind)
        .collect::<Vec<_>>();
    assert_eq!(kinds, vec![TurnKind::Operator, TurnKind::Assistant]);
    assert_eq!(brief.tail[1].text.chars().count(), 600);
    assert!(brief.tail[1].truncated);
    assert!(!brief.tail[0].truncated);

    // A narrower window keeps the newest turns of the exchange.
    let narrow = brief_tail(&backends, "session-brief-claude", 1);
    assert_eq!(narrow.len(), 1);
    assert_eq!(narrow[0].ordinal, 1);
}

fn brief_tail(
    backends: &[Box<dyn Backend>],
    id: &str,
    tail: usize,
) -> Vec<tapes_core::endings::TailEntry> {
    brief(backends, id, tail).tail
}

/// The window bounds the rendered exchange alone: pairing still sees the
/// whole read, so a call answered outside the window is not reported as open.
#[test]
fn the_window_bounds_the_tail_and_not_the_pairing() {
    let id = "00000000-0000-0000-0000-0000000000c4";
    let mut records = codex_operator(2, "Run the tool.");
    records.extend([
        r#"{"timestamp":"2026-01-01T10:00:03Z","type":"response_item","payload":{"type":"function_call","name":"fixture_tool","arguments":"{}","call_id":"call-1"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:04Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"Tool complete."}}"#.to_owned(),
    ]);
    records.push(codex_assistant(5, "Tool done."));
    let (_store, backends) = codex_store("pairing", id, "/fixtures/project", records);

    let brief = brief(&backends, id, 1);
    assert!(
        brief.in_flight.calls_without_result.is_empty(),
        "{:#?}",
        brief.in_flight.calls_without_result
    );
    let value: Value = serde_json::to_value(&brief).unwrap();
    assert_eq!(value["tail"].as_array().unwrap().len(), 1);
    assert_eq!(value["tail"][0]["kind"], "assistant");
    assert_eq!(value["ending"]["facts"], json!(["assistant-close"]));
}

/// A child is a handle while the store records no outcome for it, and while
/// its own recording is missing whatever outcome the parent recorded. A child
/// the store answers on both counts is not one.
#[test]
fn a_child_is_a_handle_until_the_store_answers_it() {
    let spawn = [
        r#"{"timestamp":"2026-01-01T10:00:02Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","call_id":"call-spawn-1","arguments":"{\"task_name\":\"worker\",\"model\":\"gpt-fixture\"}"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-spawn-1","output":"{\"task_name\":\"/root/worker\"}"}}"#.to_owned(),
    ];
    let wait = [
        r#"{"timestamp":"2026-01-01T10:00:04Z","type":"response_item","payload":{"type":"function_call","name":"wait_agent","call_id":"call-wait-1","arguments":"{}"}}"#.to_owned(),
        r#"{"timestamp":"2026-01-01T10:00:05Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-wait-1","output":"{\"agents\":[{\"agent_name\":\"/root/worker\",\"agent_status\":\"completed\"}]}"}}"#.to_owned(),
    ];

    // The parent records a spawn, the wait that answers it, and the child's
    // own recording is in the store: nothing about it is open.
    let answered = store_with_child("answered-child", "c5", spawn.iter().chain(&wait), true);
    assert!(
        brief(&answered.1, &answered.0, 12)
            .in_flight
            .children
            .is_empty(),
        "{:#?}",
        brief(&answered.1, &answered.0, 12).in_flight.children
    );

    // The parent never waited on it.
    let waiting = store_with_child("waiting-child", "c6", spawn.iter(), true);
    let children = brief(&waiting.1, &waiting.0, 12).in_flight.children;
    assert_eq!(children.len(), 1, "{children:#?}");
    assert_eq!(children[0].reference, "/root/worker");
    assert_eq!(children[0].role.as_deref(), Some("worker"));
    assert!(children[0].resolved);
    assert!(children[0].disposition.is_none());

    // The parent recorded an outcome, but the child's recording is not there.
    let missing = store_with_child("missing-child", "c7", spawn.iter().chain(&wait), false);
    let children = brief(&missing.1, &missing.0, 12).in_flight.children;
    assert_eq!(children.len(), 1, "{children:#?}");
    assert!(!children[0].resolved);
    assert_eq!(children[0].disposition.as_deref(), Some("completed"));
}

/// A Codex parent whose records name one child, whose own rollout is in the
/// store or is not.
fn store_with_child<'a>(
    tag: &str,
    suffix: &str,
    records: impl Iterator<Item = &'a String>,
    child_recorded: bool,
) -> (String, Vec<Box<dyn Backend>>, Store) {
    let parent = format!("00000000-0000-0000-0000-0000000000{suffix}");
    let child = format!("10000000-0000-0000-0000-0000000000{suffix}");
    let store = Store::new(tag);
    let mut lines = codex_header(&parent, "/fixtures/project");
    lines.extend(records.cloned());
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{parent}.jsonl"),
        &lines,
    );
    if child_recorded {
        store.write(
            &format!("2026/01/01/rollout-2026-01-01T10-00-01-{child}.jsonl"),
            &[
                format!(
                    r#"{{"timestamp":"2026-01-01T10:00:01Z","type":"session_meta","payload":{{"id":"{child}","cwd":"/fixtures/project","thread_source":"subagent","parent_thread_id":"{parent}","agent_path":"/root/worker"}}}}"#
                ),
                codex_assistant(6, "Worker finished."),
            ],
        );
    }
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.root))];
    (parent, backends, store)
}

/// The counters are the session's own, carried only where the harness
/// recorded them, with the accounting that says what they cover.
#[test]
fn recorded_counters_are_carried_and_absence_stays_absent() {
    let counted = "00000000-0000-0000-0000-0000000000c6";
    let (_counted_store, backends) = codex_store(
        "counted",
        counted,
        "/fixtures/project",
        vec![
            codex_assistant(2, "Finished."),
            r#"{"timestamp":"2026-01-01T10:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":120,"output_tokens":34}}}}"#.to_owned(),
        ],
    );

    let value = serde_json::to_value(brief(&backends, counted, 12)).unwrap();
    assert_eq!(
        value["usage"],
        json!({
            "tokens": { "input": 120, "output": 34 },
            "accounting": { "basis": "recorded-total", "coverage": "session" },
        })
    );

    let uncounted = "00000000-0000-0000-0000-0000000000c7";
    let (_uncounted_store, backends) = codex_store(
        "uncounted",
        uncounted,
        "/fixtures/project",
        vec![codex_assistant(2, "Finished.")],
    );
    let value = serde_json::to_value(brief(&backends, uncounted, 12)).unwrap();
    assert!(value.get("usage").is_none(), "{value}");
}

/// The handle lists are bounded like every other read here, and a list the
/// bound cut says so rather than reading as the whole of it.
#[test]
fn a_handle_list_the_bound_cut_says_so() {
    let id = "00000000-0000-0000-0000-0000000000c8";
    let records = (0..25)
        .map(|index| {
            format!(
                r#"{{"timestamp":"2026-01-01T10:01:{index:02}Z","type":"response_item","payload":{{"type":"function_call","name":"fixture_tool","arguments":"{{}}","call_id":"call-{index}"}}}}"#
            )
        })
        .collect::<Vec<_>>();
    let (_store, backends) = codex_store("bounded", id, "/fixtures/project", records);

    let brief = brief(&backends, id, 12);
    let calls = &brief.in_flight.calls_without_result;
    assert_eq!(calls.len(), 20);
    // Newest first, so the call most likely still in flight is the first one.
    assert_eq!(calls[0].call_id.as_deref(), Some("call-24"));
    assert_eq!(calls[19].call_id.as_deref(), Some("call-5"));
    assert!(
        brief
            .notes
            .iter()
            .any(|note| note.contains("20 newest calls without a result")),
        "{:#?}",
        brief.notes
    );
}
