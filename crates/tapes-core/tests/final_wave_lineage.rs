//! Lineage facts read where a recording states them: a Claude background
//! subagent's end from the parent's task notification or the child's own meta
//! record, and a session's root as the harness and id together, so roots that
//! share an id in different harnesses stay apart.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::lineage::{ChildRef, Lineage, SourceRef};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::WindowBounds;
use agent_tapes_core::{
    usage_summary_with_backends, usage_window_with_backends, usage_with_backends, Selection,
    SessionSelection, WindowAsk,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-final-wave-lineage-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, records: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }

    fn claude_projects(&self) -> PathBuf {
        self.0.join("claude/projects")
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.claude_projects())),
            Box::new(CodexBackend::new(self.0.join("codex"))),
        ]
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn instant(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
}

// --- Claude background subagents -------------------------------------------

const PARENT: &str = "6b0e2d14-0000-4000-8000-0000000000b1";
/// Ended, and said so by two task notifications on attachment records.
const NOTIFIED: &str = "a0cbe2de6b06527b1";
/// Ended, and said so by a task notification on a user record.
const KILLED: &str = "a1e372fed4092a1d9";
/// Ended, with its spawn and notification outside the parent's recording and
/// only its meta record saying it finished.
const FINISHED: &str = "a50d88701944893df";
/// Still running: launched, no notification, and a meta record that does not
/// say it finished.
const RUNNING: &str = "acd0f1e27e510deb4";

fn call_id(agent_id: &str) -> String {
    format!("toolu_01{agent_id}")
}

fn parent_record(kind: &str, uuid: &str, at: &str, message: Value) -> Value {
    json!({"type": kind, "sessionId": PARENT, "uuid": uuid, "timestamp": at,
        "cwd": "/fixtures/project", "isSidechain": false, "message": message})
}

/// An `Agent` call run in the background, and the result Claude answers it
/// with at once.
fn background_launch(agent_id: &str, at: &str) -> [Value; 2] {
    let call = call_id(agent_id);
    [
        parent_record(
            "assistant",
            &format!("{agent_id}-call"),
            at,
            json!({"role": "assistant", "model": "claude-fixture", "stop_reason": "tool_use",
                "content": [{"type": "tool_use", "id": call, "name": "Agent", "input": {
                    "description": "Scout the fixture", "subagent_type": "general-purpose",
                    "model": "haiku", "prompt": "Scout.", "run_in_background": true}}]}),
        ),
        {
            let mut result = parent_record(
                "user",
                &format!("{agent_id}-launched"),
                at,
                json!({"role": "user", "content": [{"tool_use_id": call, "type": "tool_result",
                    "content": [{"type": "text", "text": "Async agent launched successfully."}]}]}),
            );
            result["toolUseResult"] = json!({"isAsync": true, "status": "async_launched",
                "agentId": agent_id, "description": "Scout the fixture",
                "resolvedModel": "claude-haiku-fixture", "prompt": "Scout."});
            result
        },
    ]
}

/// The text Claude writes when a background task stops.
fn notification_text(task_id: &str, call: &str, status: &str) -> String {
    format!(
        "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>{call}</tool-use-id>\n\
         <output-file>/tmp/claude-fixture/tasks/{task_id}.output</output-file>\n\
         <status>{status}</status>\n<summary>Agent \"Scout the fixture\" finished</summary>\n\
         <note>A task-notification fires each time this agent stops with no live background \
         children of its own.</note>\n<result>Scouted.</result>\n\
         <usage><subagent_tokens>1200</subagent_tokens><tool_uses>3</tool_uses>\
         <duration_ms>4000</duration_ms></usage>\n</task-notification>"
    )
}

/// A notification as Claude queues it: the queue operation, which names no
/// record, then the attachment that carries it into the conversation.
fn attached_notification(
    task_id: &str,
    call: &str,
    status: &str,
    uuid: &str,
    at: &str,
) -> [Value; 2] {
    let text = notification_text(task_id, call, status);
    [
        json!({"type": "queue-operation", "operation": "enqueue", "timestamp": at,
            "sessionId": PARENT, "content": text}),
        json!({"parentUuid": "fixture-previous", "isSidechain": false,
            "attachment": {"type": "queued_command", "prompt": text,
                "commandMode": "task-notification", "timestamp": at},
            "type": "attachment", "uuid": uuid, "timestamp": at, "cwd": "/fixtures/project",
            "sessionId": PARENT}),
    ]
}

/// A notification Claude records as a user turn of its own.
fn user_notification(task_id: &str, call: &str, status: &str, uuid: &str, at: &str) -> Value {
    let mut record = parent_record(
        "user",
        uuid,
        at,
        json!({"role": "user", "content": notification_text(task_id, call, status)}),
    );
    record["origin"] = json!({"kind": "task-notification", "producer": "session-task"});
    record
}

fn child_meta(agent_id: &str, finished: bool) -> Value {
    let mut meta = json!({"agentType": "general-purpose", "description": "Scout the fixture",
        "toolUseId": call_id(agent_id), "spawnDepth": 1, "requestShape": "background",
        "requestNonInteractive": true, "model": "haiku"});
    if finished {
        meta["finished"] = json!(true);
    }
    meta
}

/// A child recording: its brief, then an assistant record at `last` that
/// stopped on `stop_reason`.
fn child_recording(agent_id: &str, last: &str, stop_reason: Value) -> Vec<Value> {
    let record = |kind: &str, uuid: String, at: &str, message: Value| {
        json!({"type": kind, "sessionId": PARENT, "agentId": agent_id, "uuid": uuid,
            "timestamp": at, "cwd": "/fixtures/project", "isSidechain": true,
            "parentUuid": null, "message": message})
    };
    vec![
        record(
            "user",
            format!("{agent_id}-brief"),
            "2026-10-12T06:49:00.000Z",
            json!({"role": "user", "content": "Scout."}),
        ),
        record(
            "assistant",
            format!("{agent_id}-ending"),
            last,
            json!({"role": "assistant", "model": "claude-haiku-fixture",
                "stop_reason": stop_reason, "content": [{"type": "text", "text": "Scouted."}]}),
        ),
    ]
}

fn background_store() -> Store {
    let store = Store::new("background");
    let mut parent = vec![parent_record(
        "user",
        "parent-prompt",
        "2026-10-12T06:48:00.000Z",
        json!({"role": "user", "content": "Scout the fixture in the background."}),
    )];
    for agent_id in [NOTIFIED, KILLED, RUNNING] {
        parent.extend(background_launch(agent_id, "2026-10-12T06:48:59.130Z"));
    }
    // A background shell command notifies the same way; its call is no
    // `Agent` call, so it names no child.
    parent.push(user_notification(
        "bi9udd1az",
        "toolu_01shellcommand",
        "completed",
        "shell-notified",
        "2026-10-12T06:50:00.000Z",
    ));
    parent.extend(attached_notification(
        NOTIFIED,
        &call_id(NOTIFIED),
        "completed",
        "notified-first",
        "2026-10-12T06:52:53.033Z",
    ));
    parent.push(user_notification(
        KILLED,
        &call_id(KILLED),
        "killed",
        "killed-notified",
        "2026-10-12T06:53:09.292Z",
    ));
    // The same child, resumed and stopped again, notifies again.
    parent.extend(attached_notification(
        NOTIFIED,
        &call_id(NOTIFIED),
        "completed",
        "notified-again",
        "2026-10-12T07:01:00.500Z",
    ));
    let project = "claude/projects/-fixtures-project";
    store.write(&format!("{project}/{PARENT}.jsonl"), &parent);
    let children = [
        (NOTIFIED, false, "2026-10-12T07:01:00.000Z"),
        (KILLED, false, "2026-10-12T06:53:09.000Z"),
        (FINISHED, true, "2026-10-12T06:55:30.250Z"),
        (RUNNING, false, "2026-10-12T06:54:00.000Z"),
    ];
    for (agent_id, finished, last) in children {
        let stop = if agent_id == RUNNING {
            json!("tool_use")
        } else {
            json!("end_turn")
        };
        store.write(
            &format!("{project}/{PARENT}/subagents/agent-{agent_id}.jsonl"),
            &child_recording(agent_id, last, stop),
        );
        let meta = store.0.join(format!(
            "{project}/{PARENT}/subagents/agent-{agent_id}.meta.json"
        ));
        fs::write(meta, child_meta(agent_id, finished).to_string()).unwrap();
    }
    store
}

fn lineage(store: &Store) -> Lineage {
    let backend = ClaudeBackend::new(store.claude_projects());
    let session = backend.locate(PARENT).unwrap().expect("parent is stored");
    let bounded = backend.lineage(&session).unwrap();
    let streamed = backend.stream_lineage(&session).unwrap();
    assert_eq!(
        bounded.children, streamed.children,
        "a streamed read states what a bounded one does"
    );
    bounded
}

fn child<'a>(lineage: &'a Lineage, agent_id: &str) -> &'a ChildRef {
    lineage
        .children
        .iter()
        .find(|child| child.reference == agent_id)
        .unwrap_or_else(|| panic!("no child {agent_id}: {:#?}", lineage.children))
}

fn cites(child: &ChildRef, native_id: &str) -> bool {
    child
        .source
        .iter()
        .any(|source| matches!(source, SourceRef::Record { native_id: id } if id == native_id))
}

/// Claim: a native Claude background subagent whose `Agent` call answered
/// `async_launched` reads as completed at the parent's latest task
/// notification naming its call, on an attachment or a user record, with the
/// notification's status as its disposition and the notification record
/// cited; a notification for a call that is no `Agent` call names no child.
/// Rejects a finished background child read as unfinished because its
/// launch result is the only answer lineage reads, an earlier notification
/// standing for a later stop, and a shell task notification read as a child.
#[test]
fn a_notified_background_child_reads_as_completed() {
    let store = background_store();
    let lineage = lineage(&store);
    assert_eq!(lineage.children.len(), 4, "{:#?}", lineage.children);

    let notified = child(&lineage, NOTIFIED);
    assert_eq!(
        notified.completed_at,
        Some(instant("2026-10-12T07:01:00.500Z")),
        "{notified:#?}"
    );
    assert_eq!(notified.disposition.as_deref(), Some("completed"));
    assert_eq!(
        notified.spawned_at,
        Some(instant("2026-10-12T06:48:59.130Z"))
    );
    assert!(cites(notified, "notified-again"), "{notified:#?}");

    let killed = child(&lineage, KILLED);
    assert_eq!(
        killed.completed_at,
        Some(instant("2026-10-12T06:53:09.292Z")),
        "{killed:#?}"
    );
    assert_eq!(killed.disposition.as_deref(), Some("killed"));
    assert!(cites(killed, "killed-notified"), "{killed:#?}");
}

/// Claim: a native background subagent whose parent's recording holds no
/// notification for it, its spawn included, reads as completed when its meta
/// record says it finished, at its own recording's final conversation record,
/// which is cited; one launched, never notified and not finished in its meta
/// stays without a completion. Rejects a finished child read as unfinished
/// because only plugin-launched children consult the meta record, and a
/// running one stamped completed for having launched.
#[test]
fn a_finished_meta_completes_a_child_the_parent_never_notified() {
    let store = background_store();
    let lineage = lineage(&store);

    let finished = child(&lineage, FINISHED);
    assert_eq!(
        finished.completed_at,
        Some(instant("2026-10-12T06:55:30.250Z")),
        "{finished:#?}"
    );
    assert!(
        cites(finished, &format!("{FINISHED}-ending")),
        "{finished:#?}"
    );

    let running = child(&lineage, RUNNING);
    assert_eq!(running.completed_at, None, "{running:#?}");
    assert_eq!(running.disposition.as_deref(), Some("async_launched"));
}

// --- Roots qualified by harness --------------------------------------------

/// A Claude session id another harness's marker also names.
const SHARED: &str = "30000000-0000-4000-8000-0000000000f1";
/// A Codex session launched by a pi session whose id is `SHARED`.
const LAUNCHED: &str = "01a10000-0000-7000-8000-0000000000f2";
/// A Claude session whose marker names a pi session of its own id.
const NAMESAKE: &str = "30000000-0000-4000-8000-0000000000f3";

fn claude_session(store: &Store, id: &str, minute: u32, prompt: &str) {
    let stamp = |second: u32| format!("2026-01-10T10:{minute:02}:{second:02}Z");
    store.write(
        &format!("claude/projects/-fixtures-project/{id}.jsonl"),
        &[
            json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"),
                "entrypoint": "sdk-cli", "timestamp": stamp(1), "cwd": "/fixtures/project",
                "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}}),
            json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"),
                "entrypoint": "sdk-cli", "requestId": format!("{id}-request"),
                "timestamp": stamp(30), "cwd": "/fixtures/project",
                "message": {"role": "assistant", "model": "claude-fixture",
                    "content": [{"type": "text", "text": "ok"}],
                    "usage": {"input_tokens": 1, "output_tokens": 5,
                        "cache_read_input_tokens": 10, "cache_creation_input_tokens": 2}}}),
        ],
    );
}

fn codex_session(store: &Store, id: &str, minute: u32, prompt: &str) {
    let stamp = format!("2026-01-10T10:{minute:02}:00Z");
    let total = json!({"input_tokens": 100, "cached_input_tokens": 0, "output_tokens": 7,
        "reasoning_output_tokens": 0, "total_tokens": 107});
    store.write(
        &format!("codex/2026/01/10/rollout-2026-01-10T10-{minute:02}-00-{id}.jsonl"),
        &[
            json!({"timestamp": stamp, "type": "session_meta",
                "payload": {"id": id, "session_id": id, "timestamp": stamp,
                    "cwd": "/fixtures/project", "source": "exec", "originator": "codex_exec"}}),
            json!({"timestamp": stamp, "type": "turn_context",
                "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
            json!({"timestamp": stamp, "type": "response_item",
                "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": prompt}]}}),
            json!({"timestamp": stamp, "type": "event_msg",
                "payload": {"type": "token_count", "info": {"total_token_usage": total,
                    "last_token_usage": total, "model_context_window": 272000}}}),
        ],
    );
}

fn marker(harness: &str, id: &str) -> String {
    format!("tapes-parent: {harness}:{id}\nInspect the fixture.")
}

fn selection() -> SessionSelection<'static> {
    SessionSelection {
        limit: Some(50),
        ..Default::default()
    }
}

fn window() -> WindowAsk {
    WindowAsk::Bounds(WindowBounds {
        from: Some("2026-01-10T09:00:00Z".parse().unwrap()),
        to: None,
    })
}

/// Each parent group's root, as its harness and id, and its session count.
fn groups(summary: &Value) -> BTreeMap<(String, String), u64> {
    summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            let key = &group["key"];
            (
                (
                    key["parent_harness"].as_str().unwrap_or("-").to_owned(),
                    key["parent"].as_str().unwrap().to_owned(),
                ),
                group["sessions"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn share<'a>(summary: &'a Value, id: &str) -> &'a Value {
    summary["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"] == id)
        .unwrap_or_else(|| panic!("no share for {id}: {summary:#}"))
}

fn root(group: (&str, &str), sessions: u64) -> ((String, String), u64) {
    ((group.0.to_owned(), group.1.to_owned()), sessions)
}

/// Claim: grouping by parent keys each group by its root's harness and id,
/// so a session launched by a pi session that the store does not hold stays
/// apart from a selected Claude session of the same id, in a summary and in
/// a window read, and a window share names its root's harness beside its id.
/// Rejects two roots that share an id in different harnesses summed as one
/// group, and a share whose root reads as the Claude session it is not.
#[test]
fn roots_sharing_an_id_across_harnesses_stay_apart() {
    let store = Store::new("roots");
    claude_session(&store, SHARED, 0, "Lead the fixture work.");
    codex_session(&store, LAUNCHED, 1, &marker("pi", SHARED));
    let backends = store.backends();

    let summary = serde_json::to_value(
        usage_summary_with_backends(&backends, &selection(), &[GroupBy::Parent]).unwrap(),
    )
    .unwrap();
    let expected = BTreeMap::from([root(("claude", SHARED), 1), root(("pi", SHARED), 1)]);
    assert_eq!(groups(&summary), expected, "{summary:#}");

    let windowed = serde_json::to_value(
        usage_window_with_backends(&backends, &selection(), &[GroupBy::Parent], window(), None)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(groups(&windowed), expected, "{windowed:#}");
    let launched = share(&windowed, LAUNCHED);
    assert_eq!(launched["root"], SHARED, "{launched:#}");
    assert_eq!(launched["root_harness"], "pi", "{launched:#}");
    assert_eq!(launched["root_resolved"], false, "{launched:#}");
    let lead = share(&windowed, SHARED);
    assert_eq!(lead["root"], SHARED, "{lead:#}");
    assert_eq!(lead["root_harness"], "claude", "{lead:#}");
    assert_eq!(lead["root_resolved"], true, "{lead:#}");
}

/// Claim: a marker naming another harness's session of the session's own id
/// names that session as its launch parent, an unresolved root in that
/// harness, and leaves no note that the marker names the session itself.
/// Rejects a marker read as self-naming by its id alone.
#[test]
fn a_marker_naming_another_harness_by_the_same_id_names_a_parent() {
    let store = Store::new("namesake");
    claude_session(&store, NAMESAKE, 0, &marker("pi", NAMESAKE));
    let backends = store.backends();

    let windowed = serde_json::to_value(
        usage_window_with_backends(&backends, &selection(), &[GroupBy::Parent], window(), None)
            .unwrap(),
    )
    .unwrap();
    let namesake = share(&windowed, NAMESAKE);
    assert_eq!(namesake["parent"], NAMESAKE, "{namesake:#}");
    assert_eq!(namesake["parent_evidence"], "declared-launch-parent");
    assert_eq!(namesake["root_harness"], "pi", "{namesake:#}");
    assert_eq!(namesake["root_resolved"], false, "{namesake:#}");
    assert_eq!(
        groups(&windowed),
        BTreeMap::from([root(("pi", NAMESAKE), 1)]),
        "{windowed:#}"
    );

    let usage =
        serde_json::to_value(usage_with_backends(&backends, Selection::Id(NAMESAKE)).unwrap())
            .unwrap();
    let notes = usage["notes"].as_array().cloned().unwrap_or_default();
    assert!(
        !notes
            .iter()
            .any(|note| note.as_str().unwrap().contains("names this session itself")),
        "{notes:?}"
    );
}
