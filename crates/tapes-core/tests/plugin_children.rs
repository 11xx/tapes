//! A subagent a plugin launches is named by no `Agent` call in its parent's
//! recording, so the parent states neither its spawn nor its end. A
//! non-interactive one takes no input after it finishes, which makes its own
//! recording's final record the only statement of its completion.

use std::fs;
use std::path::PathBuf;

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::{Backend, MIN_READ_BYTES};
use agent_tapes_core::lineage::{ChildRef, Lineage, SourceRef};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

const PARENT: &str = "5c1f0a2e-0000-4000-8000-000000000001";
const FINISHED: &str = "a5f3c01d7e2b49a60";
const RUNNING: &str = "a9b81e4d03c7f25a1";

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tapes-plugin-children-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn project(&self) -> PathBuf {
        self.0.join("-fixtures-project")
    }

    fn write_parent(&self, records: &[Value]) {
        fs::create_dir_all(self.project()).unwrap();
        fs::write(
            self.project().join(format!("{PARENT}.jsonl")),
            lines(records),
        )
        .unwrap();
    }

    fn write_child(&self, agent_id: &str, meta: &Value, recording: &str) {
        let directory = self.project().join(PARENT).join("subagents");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(format!("agent-{agent_id}.meta.json")),
            meta.to_string(),
        )
        .unwrap();
        fs::write(directory.join(format!("agent-{agent_id}.jsonl")), recording).unwrap();
    }

    fn lineage(&self, backend: &ClaudeBackend) -> Lineage {
        let session = backend.locate(PARENT).unwrap().expect("parent is stored");
        let bounded = backend.lineage(&session).unwrap();
        let streamed = backend.stream_lineage(&session).unwrap();
        assert_eq!(
            bounded.children, streamed.children,
            "a streamed read states what a bounded one does"
        );
        bounded
    }

    fn child(&self, lineage: &Lineage, agent_id: &str) -> ChildRef {
        lineage
            .children
            .iter()
            .find(|child| child.reference == agent_id)
            .unwrap_or_else(|| panic!("no child {agent_id}: {:#?}", lineage.children))
            .clone()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lines(records: &[Value]) -> String {
    records.iter().map(|record| format!("{record}\n")).collect()
}

fn parent_prompt() -> Value {
    json!({"type": "user", "sessionId": PARENT, "uuid": "parent-user-1",
        "timestamp": "2026-10-09T18:30:00.000Z", "cwd": "/fixtures/project",
        "message": {"role": "user", "content": "Delegate the review."}})
}

fn plugin_meta() -> Value {
    json!({"agentType": "delegated-agents:run", "description": "Delegated run fixture",
        "toolUseId": "toolu_plugin_0001", "pluginSteered": true,
        "requestShape": "background", "requestNonInteractive": true})
}

fn brief(agent_id: &str) -> Value {
    json!({"type": "user", "sessionId": PARENT, "uuid": format!("{agent_id}-brief"),
        "parentUuid": null, "timestamp": "2026-10-09T18:31:00.000Z",
        "cwd": "/fixtures/project", "isSidechain": true, "agentId": agent_id,
        "message": {"role": "user", "content": "Review the change."}})
}

fn assistant(agent_id: &str, uuid: &str, at: &str, stop_reason: Value) -> Value {
    json!({"type": "assistant", "sessionId": PARENT, "uuid": uuid,
        "parentUuid": format!("{agent_id}-brief"), "timestamp": at,
        "cwd": "/fixtures/project", "isSidechain": true, "agentId": agent_id,
        "message": {"role": "assistant", "model": "claude-fixture",
            "stop_reason": stop_reason, "content": [{"type": "text", "text": "Reviewed."}]}})
}

fn ended(agent_id: &str) -> Value {
    assistant(
        agent_id,
        &format!("{agent_id}-ending"),
        "2026-10-09T18:39:12.500Z",
        json!("end_turn"),
    )
}

fn instant(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
}

fn has_record(child: &ChildRef, native_id: &str) -> bool {
    child
        .source
        .iter()
        .any(|source| matches!(source, SourceRef::Record { native_id: id } if id == native_id))
}

fn single_child(label: &str, meta: &Value, recording: &str) -> (Store, ChildRef) {
    let store = Store::new(label);
    store.write_parent(&[parent_prompt()]);
    store.write_child(FINISHED, meta, recording);
    let lineage = store.lineage(&ClaudeBackend::new(&store.0));
    let child = store.child(&lineage, FINISHED);
    (store, child)
}

#[test]
fn a_plugin_steered_child_that_ended_its_turn_completed_at_that_record() {
    let (_store, child) = single_child(
        "ended",
        &plugin_meta(),
        &lines(&[brief(FINISHED), ended(FINISHED)]),
    );

    assert!(child.resolved);
    assert_eq!(child.role.as_deref(), Some("delegated-agents:run"));
    assert_eq!(
        child.completed_at,
        Some(instant("2026-10-09T18:39:12.500Z"))
    );
    assert!(
        has_record(&child, &format!("{FINISHED}-ending")),
        "{child:#?}"
    );
    assert!(
        child.source.iter().any(|source| matches!(
            source,
            SourceRef::File { path } if path.ends_with(&format!("subagents/agent-{FINISHED}.jsonl"))
        )),
        "the record is cited beside the recording it is in: {child:#?}"
    );
    // The parent states no spawn, and the child's first record is its own
    // first activity rather than the parent's launch.
    assert!(child.spawned_at.is_none());
    assert!(child.disposition.is_none());
}

#[test]
fn a_running_child_is_not_complete_while_a_finished_one_beside_it_is() {
    let store = Store::new("beside");
    store.write_parent(&[parent_prompt()]);
    store.write_child(
        FINISHED,
        &plugin_meta(),
        &lines(&[brief(FINISHED), ended(FINISHED)]),
    );
    store.write_child(
        RUNNING,
        &plugin_meta(),
        &lines(&[
            brief(RUNNING),
            assistant(
                RUNNING,
                "running-call",
                "2026-10-09T18:40:00.000Z",
                json!("tool_use"),
            ),
        ]),
    );

    let lineage = store.lineage(&ClaudeBackend::new(&store.0));

    assert!(store.child(&lineage, FINISHED).completed_at.is_some());
    let running = store.child(&lineage, RUNNING);
    assert!(running.completed_at.is_none(), "{running:#?}");
    assert!(!has_record(&running, "running-call"), "{running:#?}");
}

#[test]
fn a_final_record_that_stops_for_any_other_reason_leaves_the_child_open() {
    for stop_reason in [json!("tool_use"), json!("max_tokens"), Value::Null] {
        let (_store, child) = single_child(
            "other-stop",
            &plugin_meta(),
            &lines(&[
                brief(FINISHED),
                assistant(
                    FINISHED,
                    "stopped",
                    "2026-10-09T18:39:00.000Z",
                    stop_reason.clone(),
                ),
            ]),
        );
        assert!(child.completed_at.is_none(), "{stop_reason}: {child:#?}");
    }
}

#[test]
fn input_after_the_ending_reopens_the_child() {
    let follow_up = json!({"type": "user", "sessionId": PARENT, "uuid": "follow-up",
        "parentUuid": format!("{FINISHED}-ending"), "timestamp": "2026-10-09T18:41:00.000Z",
        "isSidechain": true, "agentId": FINISHED,
        "message": {"role": "user", "content": "One more thing."}});
    let (_store, child) = single_child(
        "follow-up",
        &plugin_meta(),
        &lines(&[brief(FINISHED), ended(FINISHED), follow_up]),
    );

    assert!(child.completed_at.is_none(), "{child:#?}");
}

#[test]
fn housekeeping_records_after_the_ending_do_not_hide_it() {
    let (_store, child) = single_child(
        "housekeeping",
        &plugin_meta(),
        &lines(&[
            brief(FINISHED),
            ended(FINISHED),
            json!({"type": "ai-title", "sessionId": PARENT, "aiTitle": "Delegated run"}),
            json!({"type": "last-prompt", "sessionId": PARENT, "lastPrompt": "Review the change."}),
        ]),
    );

    assert_eq!(
        child.completed_at,
        Some(instant("2026-10-09T18:39:12.500Z"))
    );
}

#[test]
fn a_final_line_that_cannot_be_read_leaves_the_child_open() {
    let recording =
        lines(&[brief(FINISHED), ended(FINISHED)]) + r#"{"type":"assistant","uuid":"half"#;
    let (_store, child) = single_child("partial", &plugin_meta(), &recording);

    assert!(child.completed_at.is_none(), "{child:#?}");
}

#[test]
fn an_ending_without_a_timestamp_states_no_completion() {
    let mut ending = ended(FINISHED);
    ending.as_object_mut().unwrap().remove("timestamp");
    let (_store, child) = single_child(
        "no-timestamp",
        &plugin_meta(),
        &lines(&[brief(FINISHED), ending]),
    );

    assert!(child.completed_at.is_none(), "{child:#?}");
    assert!(
        !has_record(&child, &format!("{FINISHED}-ending")),
        "{child:#?}"
    );
}

#[test]
fn only_a_child_that_takes_no_further_input_is_read_for_its_ending() {
    let recording = lines(&[brief(FINISHED), ended(FINISHED)]);
    let mut ordinary = plugin_meta();
    ordinary.as_object_mut().unwrap().remove("pluginSteered");
    let mut declined = plugin_meta();
    declined["pluginSteered"] = json!(false);
    let mut interactive = plugin_meta();
    interactive["requestNonInteractive"] = json!(false);
    let mut unlabelled = plugin_meta();
    unlabelled["pluginSteered"] = json!("true");

    for (label, meta) in [
        ("ordinary", ordinary),
        ("declined", declined),
        ("interactive", interactive),
        ("unlabelled", unlabelled),
    ] {
        let (_store, child) = single_child(label, &meta, &recording);
        assert!(
            child.completed_at.is_none(),
            "{label}: its own recording says nothing the parent did not: {child:#?}"
        );
    }
}

#[test]
fn a_child_whose_meta_is_silent_on_interactivity_is_read_for_its_ending() {
    let mut meta = plugin_meta();
    meta.as_object_mut()
        .unwrap()
        .remove("requestNonInteractive");
    let (_store, child) = single_child(
        "silent-interactivity",
        &meta,
        &lines(&[brief(FINISHED), ended(FINISHED)]),
    );

    assert!(child.completed_at.is_some(), "{child:#?}");
}

#[test]
fn the_parents_own_record_of_an_ending_is_preferred_to_the_childs() {
    let store = Store::new("parent-first");
    let meta = json!({"agentType": "delegated-agents:run", "toolUseId": "toolu_call_1",
        "pluginSteered": true, "requestNonInteractive": true});
    store.write_parent(&[
        parent_prompt(),
        json!({"type": "assistant", "sessionId": PARENT, "uuid": "call-record",
            "timestamp": "2026-10-09T18:30:30.000Z",
            "message": {"role": "assistant", "model": "claude-fixture", "content": [
                {"type": "tool_use", "id": "toolu_call_1", "name": "Agent",
                    "input": {"subagent_type": "delegated-agents:run"}}]}}),
        json!({"type": "user", "sessionId": PARENT, "uuid": "result-record",
            "timestamp": "2026-10-09T18:39:30.000Z",
            "toolUseResult": {"status": "completed", "agentId": FINISHED},
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_call_1", "content": "Done."}]}}),
    ]);
    store.write_child(FINISHED, &meta, &lines(&[brief(FINISHED), ended(FINISHED)]));

    let lineage = store.lineage(&ClaudeBackend::new(&store.0));
    let child = store.child(&lineage, FINISHED);

    assert_eq!(
        child.completed_at,
        Some(instant("2026-10-09T18:39:30.000Z"))
    );
    assert!(has_record(&child, "result-record"), "{child:#?}");
    assert!(
        !has_record(&child, &format!("{FINISHED}-ending")),
        "{child:#?}"
    );
}

#[test]
fn a_launch_the_parent_recorded_as_running_is_completed_by_the_childs_ending() {
    let store = Store::new("launched");
    let meta = json!({"agentType": "delegated-agents:run", "toolUseId": "toolu_call_2",
        "pluginSteered": true, "requestNonInteractive": true});
    store.write_parent(&[
        parent_prompt(),
        json!({"type": "assistant", "sessionId": PARENT, "uuid": "call-record",
            "timestamp": "2026-10-09T18:30:30.000Z",
            "message": {"role": "assistant", "model": "claude-fixture", "content": [
                {"type": "tool_use", "id": "toolu_call_2", "name": "Agent", "input": {}}]}}),
        json!({"type": "user", "sessionId": PARENT, "uuid": "launch-record",
            "timestamp": "2026-10-09T18:30:31.000Z",
            "toolUseResult": {"status": "async_launched", "agentId": FINISHED},
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_call_2", "content": "Launched."}]}}),
    ]);
    store.write_child(FINISHED, &meta, &lines(&[brief(FINISHED), ended(FINISHED)]));

    let lineage = store.lineage(&ClaudeBackend::new(&store.0));
    let child = store.child(&lineage, FINISHED);

    assert_eq!(
        child.completed_at,
        Some(instant("2026-10-09T18:39:12.500Z"))
    );
    assert_eq!(child.disposition.as_deref(), Some("async_launched"));
    assert_eq!(child.spawned_at, Some(instant("2026-10-09T18:30:30.000Z")));
}

#[test]
fn the_ending_is_found_in_the_bounded_tail_of_a_large_recording() {
    let filler = json!({"type": "user", "sessionId": PARENT, "uuid": "filler",
        "timestamp": "2026-10-09T18:32:00.000Z", "isSidechain": true, "agentId": FINISHED,
        "message": {"role": "user", "content": "x".repeat(1024)}});
    let mut records = vec![brief(FINISHED)];
    records.extend(std::iter::repeat_n(
        filler,
        3 * MIN_READ_BYTES as usize / 1024,
    ));
    records.push(ended(FINISHED));
    let store = Store::new("large");
    store.write_parent(&[parent_prompt()]);
    store.write_child(FINISHED, &plugin_meta(), &lines(&records));
    let recorded = fs::metadata(
        store
            .project()
            .join(PARENT)
            .join(format!("subagents/agent-{FINISHED}.jsonl")),
    )
    .unwrap()
    .len();
    assert!(
        recorded > 2 * MIN_READ_BYTES,
        "the recording outgrows the bound"
    );

    let lineage = store.lineage(&ClaudeBackend::new(&store.0).with_read_bytes(MIN_READ_BYTES));
    let child = store.child(&lineage, FINISHED);

    assert_eq!(
        child.completed_at,
        Some(instant("2026-10-09T18:39:12.500Z"))
    );
}

#[test]
fn a_final_record_larger_than_the_bound_leaves_the_child_open() {
    let mut ending = ended(FINISHED);
    ending["message"]["content"][0]["text"] = json!("y".repeat(2 * MIN_READ_BYTES as usize));
    let store = Store::new("oversized");
    store.write_parent(&[parent_prompt()]);
    store.write_child(FINISHED, &plugin_meta(), &lines(&[brief(FINISHED), ending]));

    let lineage = store.lineage(&ClaudeBackend::new(&store.0).with_read_bytes(MIN_READ_BYTES));

    assert!(
        store.child(&lineage, FINISHED).completed_at.is_none(),
        "{:#?}",
        lineage.children
    );
}

#[test]
fn a_missing_or_unreadable_child_recording_is_not_an_error() {
    let store = Store::new("missing");
    store.write_parent(&[parent_prompt()]);
    store.write_child(FINISHED, &plugin_meta(), "");
    let directory = store.project().join(PARENT).join("subagents");
    fs::remove_file(directory.join(format!("agent-{FINISHED}.jsonl"))).unwrap();
    fs::create_dir(directory.join(format!("agent-{RUNNING}.jsonl"))).unwrap();
    fs::write(
        directory.join(format!("agent-{RUNNING}.meta.json")),
        plugin_meta().to_string(),
    )
    .unwrap();

    let lineage = store.lineage(&ClaudeBackend::new(&store.0));

    assert_eq!(lineage.children.len(), 1, "{:#?}", lineage.children);
    assert!(lineage.children[0].completed_at.is_none());
}
