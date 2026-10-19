//! A Claude session that continues into a new session id while a background
//! subagent is still running carries that subagent over: the new session's
//! `subagents/agent-<id>.jsonl` is a link to the recording under the earlier
//! session's own `subagents/`, written beside a fresh meta record. The linked
//! recording's records name the earlier session, and the ones written after
//! the continuation name the new one.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::usage::{GroupBy, UsageOptions};
use agent_tapes_core::window::WindowBounds;
use agent_tapes_core::{
    usage_window_with_backends, usage_with_options_with_backends, Selection, SessionSelection,
    WindowAsk,
};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The session that spawned the subagent.
const EARLIER: &str = "3e71c0a4-5b2d-4f19-8c6e-0d9a2b4f7e15";
/// The session that continued it while the subagent ran.
const CONTINUED: &str = "9a4d2f60-1c3b-4e87-b5a2-6f0e8d1c3b74";
/// A session neither recording's location names.
const UNRELATED: &str = "c58e1b27-6d40-4a93-9f1c-2b7e4a0d8c69";
/// The subagent running across the continuation.
const CARRIED: &str = "a7d31c9e0b25f4a68";
/// A subagent the continued session spawned itself.
const OWN: &str = "a2b9e04c71d3f5e86";

struct Store {
    root: TempDir,
}

impl Store {
    fn new() -> Self {
        let store = Self {
            root: TempDir::with_prefix("tapes-claude-continued-session-").unwrap(),
        };
        fs::create_dir_all(store.project()).unwrap();
        store
    }

    fn projects(&self) -> PathBuf {
        self.root.path().join("projects")
    }

    fn project(&self) -> PathBuf {
        self.projects().join("-fixtures-project")
    }

    fn subagents(&self, session: &str) -> PathBuf {
        let directory = self.project().join(session).join("subagents");
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn write(&self, path: PathBuf, records: &[Value]) {
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }

    fn write_meta(&self, session: &str, agent: &str, meta: &Value) {
        fs::write(
            self.subagents(session)
                .join(format!("agent-{agent}.meta.json")),
            meta.to_string(),
        )
        .unwrap();
    }

    fn backend(&self) -> ClaudeBackend {
        ClaudeBackend::new(self.projects())
    }

    /// Both sessions, the subagent spawned under the earlier one, and the
    /// continued session's own subagent. `carried` names the sessions the
    /// subagent's records carry, before and after the continuation; `link`
    /// is whether the continued session reaches the recording through a
    /// link rather than a copy of its own.
    fn continuation(carried: [&str; 2], link: bool) -> Self {
        let store = Self::new();
        store.write(
            store.project().join(format!("{EARLIER}.jsonl")),
            &[
                prompt(EARLIER, "earlier-user", "2026-10-12T10:00:00.000Z"),
                request(
                    EARLIER,
                    None,
                    "earlier-assistant",
                    "req_earlier",
                    "2026-10-12T10:01:00.000Z",
                    300,
                ),
            ],
        );
        let recording = store
            .subagents(EARLIER)
            .join(format!("agent-{CARRIED}.jsonl"));
        let [before, after] = carried;
        store.write(
            recording.clone(),
            &[
                json!({"type": "user", "sessionId": before, "uuid": "carried-brief",
                    "parentUuid": null, "isSidechain": true, "agentId": CARRIED,
                    "timestamp": "2026-10-12T10:02:00.000Z", "cwd": "/fixtures/project",
                    "message": {"role": "user", "content": "Review the change."}}),
                request(
                    before,
                    Some(CARRIED),
                    "carried-assistant",
                    "req_carried",
                    "2026-10-12T10:03:00.000Z",
                    70,
                ),
                json!({"type": "attachment", "sessionId": after, "uuid": "carried-instructions",
                    "parentUuid": "carried-assistant", "isSidechain": true, "agentId": CARRIED,
                    "timestamp": "2026-10-12T10:30:00.000Z", "cwd": "/fixtures/project",
                    "attachment": {"type": "instructions", "files": [], "changed": true,
                        "reason": "session_start"}}),
                json!({"type": "assistant", "sessionId": after, "uuid": "carried-synthetic",
                    "parentUuid": "carried-instructions", "isSidechain": true,
                    "agentId": CARRIED, "timestamp": "2026-10-12T10:30:01.000Z",
                    "cwd": "/fixtures/project",
                    "message": {"role": "assistant", "model": "<synthetic>",
                        "stop_reason": "stop_sequence",
                        "usage": {"input_tokens": 0, "output_tokens": 0,
                            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
                        "content": [{"type": "text", "text": "No response requested."}]}}),
            ],
        );
        store.write_meta(EARLIER, CARRIED, &plugin_meta(false));

        store.write(
            store.project().join(format!("{CONTINUED}.jsonl")),
            &[
                json!({"type": "system", "subtype": "compact_boundary", "sessionId": CONTINUED,
                    "uuid": "continued-boundary", "parentUuid": null,
                    "logicalParentUuid": "earlier-assistant",
                    "timestamp": "2026-10-12T10:29:00.000Z", "cwd": "/fixtures/project"}),
                prompt(CONTINUED, "continued-user", "2026-10-12T10:29:30.000Z"),
                request(
                    CONTINUED,
                    None,
                    "continued-assistant",
                    "req_continued",
                    "2026-10-12T10:31:00.000Z",
                    500,
                ),
            ],
        );
        let carried_path = store
            .subagents(CONTINUED)
            .join(format!("agent-{CARRIED}.jsonl"));
        if link {
            symlink(&recording, &carried_path).unwrap();
        } else {
            fs::copy(&recording, &carried_path).unwrap();
        }
        store.write_meta(CONTINUED, CARRIED, &plugin_meta(true));
        store.write(
            store
                .subagents(CONTINUED)
                .join(format!("agent-{OWN}.jsonl")),
            &[request(
                CONTINUED,
                Some(OWN),
                "own-assistant",
                "req_own",
                "2026-10-12T10:32:00.000Z",
                40,
            )],
        );
        store
    }

    fn lineage(&self, id: &str) -> anyhow::Result<Value> {
        let backend = self.backend();
        let session = backend.locate(id)?.expect("session is stored");
        let bounded = backend.lineage(&session)?;
        let streamed = backend.stream_lineage(&session)?;
        assert_eq!(bounded.children, streamed.children);
        Ok(serde_json::to_value(bounded).unwrap())
    }

    fn reconcile(&self, id: &str) -> Value {
        let view = usage_with_options_with_backends(
            &[Box::new(self.backend()) as Box<dyn Backend>],
            Selection::Id(id),
            UsageOptions {
                full: true,
                series: None,
                reconcile: true,
                event_keys: None,
            },
        )
        .unwrap();
        serde_json::to_value(view.reconciliation.as_ref().unwrap()).unwrap()
    }

    fn window(&self) -> Value {
        let summary = usage_window_with_backends(
            &[Box::new(self.backend()) as Box<dyn Backend>],
            &SessionSelection {
                limit: Some(50),
                ..Default::default()
            },
            &[GroupBy::Parent],
            WindowAsk::Bounds(WindowBounds {
                from: Some("2026-10-12T09:00:00Z".parse().unwrap()),
                to: None,
            }),
            None,
        )
        .unwrap();
        serde_json::to_value(summary).unwrap()
    }
}

fn prompt(session: &str, uuid: &str, at: &str) -> Value {
    json!({"type": "user", "sessionId": session, "uuid": uuid, "timestamp": at,
        "cwd": "/fixtures/project", "entrypoint": "cli",
        "message": {"role": "user", "content": "Carry on with the series."}})
}

fn request(
    session: &str,
    agent: Option<&str>,
    uuid: &str,
    request: &str,
    at: &str,
    output: u64,
) -> Value {
    let mut record = json!({"type": "assistant", "sessionId": session, "uuid": uuid,
        "requestId": request, "timestamp": at, "cwd": "/fixtures/project",
        "message": {"role": "assistant", "model": "claude-fixture", "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": output,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
            "content": [{"type": "text", "text": "Done."}]}});
    if let Some(agent) = agent {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(agent);
    }
    record
}

/// The meta record Claude writes beside a plugin-launched subagent; the one
/// it writes beside a carried-over recording also says the run finished.
fn plugin_meta(finished: bool) -> Value {
    let mut meta = json!({"agentType": "delegated-agents:run", "pluginSteered": true,
        "description": "Delegated run fixture", "toolUseId": "toolu_plugin_0001",
        "spawnDepth": 1, "requestShape": "background", "requestNonInteractive": true});
    if finished {
        meta["agentType"] = json!("general-purpose");
        meta["finished"] = json!(true);
    }
    meta
}

fn child<'a>(lineage: &'a Value, agent: &str) -> &'a Value {
    lineage["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["reference"] == agent)
        .unwrap_or_else(|| panic!("no child {agent}: {lineage:#}"))
}

fn source<'a>(reconciliation: &'a Value, agent: &str) -> Option<&'a Value> {
    reconciliation["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["agent_id"] == agent)
}

/// Claim: the continued session's lineage names the linked subagent as
/// carried over from the session that spawned it, and its own subagent as
/// its own; the spawning session's lineage, whose subagent recording holds
/// the records the continued session wrote through the link, reads too.
/// Rejects refusing either session's whole lineage because a linked child's
/// records name the session it was spawned under, or because the spawning
/// session's child holds records its continuation wrote.
#[test]
fn a_continued_session_names_its_carried_over_subagent() {
    let store = Store::continuation([EARLIER, CONTINUED], true);

    let continued = store.lineage(CONTINUED).unwrap();
    let carried = child(&continued, CARRIED);
    assert_eq!(carried["carried_from"], EARLIER, "{carried:#}");
    assert_eq!(carried["resolved"], true);
    assert_eq!(
        carried["completed_at"], "2026-10-12T10:30:01Z",
        "{carried:#}"
    );
    assert!(child(&continued, OWN).get("carried_from").is_none());

    let earlier = store.lineage(EARLIER).unwrap();
    assert!(
        child(&earlier, CARRIED).get("carried_from").is_none(),
        "{earlier:#}"
    );

    for parent in [CONTINUED, EARLIER] {
        let backend = store.backend();
        let session = backend.locate(parent).unwrap().unwrap();
        let read = backend
            .stream_child_transcript(&session, CARRIED, &mut |_| Ok(()))
            .unwrap_or_else(|error| panic!("child read under {parent}: {error:#}"));
        assert_eq!(read.session.id, format!("{parent}::{CARRIED}"));
    }
}

/// Claim: family reconciliation counts the carried subagent's requests once,
/// in the family of the session that spawned it; the continued session holds
/// no source for it and says why in a note. Rejects counting it under
/// neither family (both refused as foreign) and counting it under both.
#[test]
fn a_carried_over_subagent_is_reconciled_in_the_family_that_spawned_it() {
    let store = Store::continuation([EARLIER, CONTINUED], true);

    let earlier = store.reconcile(EARLIER);
    let spawned = source(&earlier, CARRIED).expect("the spawning family reads the subagent");
    assert!(spawned.get("error").is_none(), "{spawned:#}");
    assert_eq!(spawned["requests"], 1, "{spawned:#}");
    assert_eq!(
        earlier["identified"]["identity_coverage"], "validated-family",
        "{earlier:#}"
    );

    let continued = store.reconcile(CONTINUED);
    assert!(source(&continued, CARRIED).is_none(), "{continued:#}");
    assert!(source(&continued, OWN).is_some());
    assert!(
        continued["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains(&format!(
                "Subagent {CARRIED} is carried over from session {EARLIER}"
            ))),
        "{continued:#}"
    );
}

/// Claim: a window grouped by parent holds the carried subagent's share once,
/// as the spawning session's child, with nothing unreadable. Rejects a share
/// under neither session, both refused as foreign, and a share under both.
#[test]
fn a_window_by_parent_shares_a_carried_over_subagent_once() {
    let store = Store::continuation([EARLIER, CONTINUED], true);
    let window = store.window();
    assert!(
        window
            .get("unreadable")
            .is_none_or(|unreadable| unreadable == &json!([])),
        "{window:#}"
    );
    let shares = window["window"]["sessions"].as_array().unwrap();
    let carried = shares
        .iter()
        .filter(|share| share["id"].as_str().unwrap().ends_with(CARRIED))
        .collect::<Vec<_>>();
    assert_eq!(carried.len(), 1, "{window:#}");
    assert_eq!(carried[0]["id"], format!("{EARLIER}::{CARRIED}"));
    assert_eq!(carried[0]["parent"], EARLIER);
    assert_eq!(carried[0]["requests"], 1);
}

/// Claim: a subagent recording whose identity its location does not explain
/// still refuses the read: a link whose records name a session other than the
/// one it lies under, a copy (not a link) of another session's subagent, and
/// a spawning session's own subagent naming a session that holds no link to
/// it. Rejects admitting any foreign identity once one link is honoured.
#[test]
fn a_subagent_its_location_does_not_explain_still_refuses() {
    for (label, carried, link, refused) in [
        ("foreign-link", [UNRELATED, CONTINUED], true, CONTINUED),
        ("copied", [EARLIER, CONTINUED], false, CONTINUED),
        ("unlinked-continuation", [EARLIER, UNRELATED], true, EARLIER),
    ] {
        let store = Store::continuation(carried, link);
        let error = store
            .lineage(refused)
            .and_then(|lineage| {
                // A child whose ending no meta record asks for is read only
                // by a child read.
                let backend = store.backend();
                let session = backend.locate(refused)?.unwrap();
                backend.stream_child_transcript(&session, CARRIED, &mut |_| Ok(()))?;
                Ok(lineage)
            })
            .expect_err(label);
        assert!(
            format!("{error:#}").contains("different or invalid native parent ID"),
            "{label}: {error:#}"
        );
    }
}
