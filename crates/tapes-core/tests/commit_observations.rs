//! A result reports the commits its call made and the commits it inspected.
//!
//! A commit is *created* when the call ran a commit-creating command and its
//! result names the commit: git's summary line, or for a quiet commit a sha a
//! later command of the same call printed. A failed commit creates nothing. A
//! commit is *inspected* when the call names it as a revision or the result
//! prints it. Only the sha and the subject are kept.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::commit_observations::{
    CommitFound, MAX_CALL_OBSERVATIONS, MAX_SESSION_OBSERVATIONS,
};
use agent_tapes_core::event::{EventFilter, EventOptions, EventRecord, EventTranscript};
use agent_tapes_core::model::Session;
use agent_tapes_core::{events_full_for_session, EventKind, EventSink, EVENTS_SCHEMA};
use serde_json::json;
use tempfile::TempDir;

const FULL: &str = "0123456789abcdef0123456789abcdef01234567";
const SHOWN: &str = "3344aabbccddeeff00112233445566778899aabb";
const FULL_TWO: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/commit-observations")
        .join(harness)
}

/// What one result reported, without its position: created, sha, how found,
/// subject, branch.
type Seen = (bool, String, CommitFound, Option<String>, Option<String>);

#[derive(Debug, Default, PartialEq)]
struct Reported {
    seen: Vec<Seen>,
    truncated: bool,
    created_unnamed: bool,
}

fn seen(
    created: bool,
    sha: &str,
    found: CommitFound,
    subject: Option<&str>,
    branch: Option<&str>,
) -> Seen {
    (
        created,
        sha.to_owned(),
        found,
        subject.map(str::to_owned),
        branch.map(str::to_owned),
    )
}

/// The call key each fixture scenario is named by: the id without the
/// harness's prefix.
fn key(call_id: &str) -> String {
    let id = call_id.split('|').next().unwrap();
    ["toolu_", "call_", "exec-"]
        .iter()
        .find_map(|prefix| id.strip_prefix(prefix))
        .unwrap_or(id)
        .to_owned()
}

fn reported(events: &[EventRecord]) -> BTreeMap<String, Reported> {
    events
        .iter()
        .filter(|record| record.event.kind == EventKind::ToolResult)
        .filter_map(|record| {
            let commits = record.commits.as_ref()?;
            Some((
                key(record.event.call_id.as_deref()?),
                Reported {
                    seen: commits
                        .observations
                        .iter()
                        .map(|o| {
                            (
                                o.created,
                                o.sha.clone(),
                                o.found,
                                o.subject.clone(),
                                o.branch.clone(),
                            )
                        })
                        .collect(),
                    truncated: commits.truncated,
                    created_unnamed: commits.created_unnamed,
                },
            ))
        })
        .collect()
}

#[derive(Default)]
struct Collect(Vec<EventRecord>);

impl EventSink for Collect {
    fn session(&mut self, _: &Session) -> anyhow::Result<()> {
        Ok(())
    }
    fn record(&mut self, record: EventRecord) -> anyhow::Result<()> {
        self.0.push(record);
        Ok(())
    }
}

/// The bounded projection and the whole-recording stream of one session.
fn both(backend: &dyn Backend, id: &str) -> (EventTranscript, Vec<EventRecord>, EventTranscript) {
    let session = backend.locate(id).unwrap().unwrap();
    let bounded = backend.events(&session, usize::MAX).unwrap();
    let mut sink = Collect::default();
    let streamed = events_full_for_session(
        backend,
        &session,
        usize::MAX,
        &EventFilter::default(),
        EventOptions::default(),
        &mut sink,
    )
    .unwrap();
    (bounded, sink.0, streamed.events)
}

/// What every harness reports for the scenarios its fixture shares. Claude
/// also records the commits it made structurally, which is the strongest way
/// to find a sha.
fn expected(claude: bool) -> BTreeMap<String, Reported> {
    use CommitFound::{ChainedOutput, GitOperation, Invocation, SummaryLine};
    let one = |seen: Seen| Reported {
        seen: vec![seen],
        ..Reported::default()
    };
    let summary = if claude { GitOperation } else { SummaryLine };
    let mut expected = BTreeMap::new();
    let mut add = |key: &str, reported: Reported| {
        expected.insert(key.to_owned(), reported);
    };
    add(
        "plain",
        one(seen(
            true,
            "1a2b3c4",
            summary,
            Some("feat: add parser"),
            Some("main"),
        )),
    );
    add(
        "quiet",
        one(seen(
            true,
            "5d6e7f8",
            ChainedOutput,
            Some("fix: handle empty input"),
            None,
        )),
    );
    add(
        "hook",
        one(seen(
            true,
            "9a8b7c6",
            SummaryLine,
            Some("chore: bump"),
            Some("main"),
        )),
    );
    add(
        "amend",
        one(seen(
            true,
            "2b3c4d5",
            summary,
            Some("feat: add parser"),
            Some("main"),
        )),
    );
    add("revparse", one(seen(true, FULL, ChainedOutput, None, None)));
    // A detached head names no branch; a root commit names its branch.
    add(
        "rebase",
        one(seen(
            true,
            "4c5d6e7",
            SummaryLine,
            Some("fix: reorder"),
            None,
        )),
    );
    add(
        "root",
        one(seen(
            true,
            "8a9b0c1",
            SummaryLine,
            Some("init"),
            Some("main"),
        )),
    );
    // The script's exit status is not the commit's: the log ran only because
    // the commit succeeded, whatever failed after it.
    add(
        "late-failure",
        one(seen(
            true,
            "6e7f809",
            ChainedOutput,
            Some("test: pin the reader"),
            None,
        )),
    );
    add(
        "range",
        Reported {
            seen: vec![
                seen(false, "abc1234", Invocation, None, None),
                seen(false, "def5678", Invocation, None, None),
            ],
            ..Reported::default()
        },
    );
    // `git show 3344aab` names the commit and prints it in full: one
    // observation, under the printed name and with the printed subject.
    add(
        "show",
        one(seen(
            false,
            SHOWN,
            Invocation,
            Some("refactor: split the reader"),
            None,
        )),
    );
    // Two quiet commits and a log of the three newest: the first two lines are
    // the commits made, the third was already there and is no observation.
    add(
        "two-quiet",
        Reported {
            seen: vec![
                seen(true, "b2c3d4e", ChainedOutput, Some("feat: second"), None),
                seen(true, "a1b2c3d", ChainedOutput, Some("feat: first"), None),
            ],
            ..Reported::default()
        },
    );
    // Each commit is followed by its own bounded log: the first line of each
    // block is the commit made before it.
    add(
        "interleaved",
        Reported {
            seen: vec![
                seen(true, "c1c1c1c", ChainedOutput, Some("feat: first"), None),
                seen(true, "b2b2b2b", ChainedOutput, Some("feat: second"), None),
            ],
            ..Reported::default()
        },
    );
    // A printer of one commit names the newest of two; the other is unnamed.
    add(
        "two-short",
        Reported {
            seen: vec![seen(true, FULL_TWO, ChainedOutput, None, None)],
            created_unnamed: true,
            ..Reported::default()
        },
    );
    // A printer of unknown length before the commit leaves the later lines unplaced.
    add(
        "unbounded-before",
        Reported {
            created_unnamed: true,
            ..Reported::default()
        },
    );
    add(
        "silent",
        Reported {
            created_unnamed: true,
            ..Reported::default()
        },
    );
    add(
        "many",
        Reported {
            seen: (0..MAX_CALL_OBSERVATIONS)
                .map(|i| {
                    seen(
                        false,
                        &format!("{:07x}", 0x100_0001 + i * 7919),
                        Invocation,
                        None,
                        None,
                    )
                })
                .collect(),
            truncated: true,
            ..Reported::default()
        },
    );
    if claude {
        // The structured record names a commit the call's command does not.
        add(
            "script",
            one(seen(true, "6f7a8b9", GitOperation, None, Some("main"))),
        );
        // It and the text disagree: both are kept, the structured one first.
        add(
            "disagree",
            Reported {
                seen: vec![
                    seen(true, "2222222", GitOperation, None, Some("main")),
                    seen(true, "1111111", SummaryLine, Some("x"), Some("main")),
                ],
                ..Reported::default()
            },
        );
    }
    expected
}

fn assert_scenarios(backend: &dyn Backend, id: &str, claude: bool) {
    let (bounded, streamed, tail) = both(backend, id);
    let expected = expected(claude);
    assert_eq!(reported(&bounded.events), expected, "bounded read");
    assert_eq!(reported(&streamed), expected, "whole-recording stream");
    // Nothing was dropped for the session cap, and the one call that hit its
    // own cap is counted.
    assert_eq!(bounded.commits.omitted, 0);
    assert_eq!(bounded.commits.truncated_calls, 1);
    assert_eq!(tail.commits, bounded.commits);
}

#[test]
fn claude_reports_the_commits_its_bash_calls_made_and_inspected() {
    assert_scenarios(
        &ClaudeBackend::new(fixtures("claude")),
        "c4000000-0000-4000-8000-000000000001",
        true,
    );
}

#[test]
fn codex_reports_the_commits_its_exec_calls_made_and_inspected() {
    assert_scenarios(
        &CodexBackend::new(fixtures("codex")),
        "c4000000-0000-7000-8000-000000000001",
        false,
    );
}

#[test]
fn pi_reports_the_commits_its_bash_calls_made_and_inspected() {
    assert_scenarios(
        &PiBackend::new(fixtures("pi")),
        "c4000000-0000-4000-8000-000000000001",
        false,
    );
}

#[test]
fn a_failed_commit_and_the_shas_a_result_merely_prints_yield_nothing() {
    for (backend, id) in [
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))) as Box<dyn Backend>,
            "c4000000-0000-4000-8000-000000000001",
        ),
        (
            Box::new(CodexBackend::new(fixtures("codex"))),
            "c4000000-0000-7000-8000-000000000001",
        ),
        (
            Box::new(PiBackend::new(fixtures("pi"))),
            "c4000000-0000-4000-8000-000000000001",
        ),
    ] {
        let session = backend.locate(id).unwrap().unwrap();
        let events = backend.events(&session, usize::MAX).unwrap();
        let found = reported(&events.events);
        // `failed` exited non-zero with "nothing to commit"; `hook-failed`
        // stopped at the hook before its chained log ran; `failed-chained` and
        // `failed-exit` printed the commit that was already there; `echoed`
        // printed a log and a line shaped like a summary.
        for nothing in [
            "failed",
            "hook-failed",
            "failed-chained",
            "failed-exit",
            "echoed",
        ] {
            assert!(
                !found.contains_key(nothing),
                "{} reported commits for {nothing}: {:?}",
                session.harness(),
                found[nothing]
            );
        }
    }
}

#[test]
fn the_events_json_carries_observations_on_the_result_only() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = backend
        .locate("c4000000-0000-4000-8000-000000000001")
        .unwrap()
        .unwrap();
    let events = backend.events(&session, usize::MAX).unwrap();
    let value = serde_json::to_value(&events).unwrap();
    assert_eq!(value["schema"], EVENTS_SCHEMA);
    assert_eq!(EVENTS_SCHEMA, "tapes-events/15");
    let records = value["events"].as_array().unwrap();
    let call = records
        .iter()
        .find(|r| r["call_id"] == "toolu_hook" && r["kind"] == "tool-call")
        .unwrap();
    assert!(call.get("commits").is_none());
    let result = records
        .iter()
        .find(|r| r["call_id"] == "toolu_hook" && r["kind"] == "tool-result")
        .unwrap();
    assert_eq!(
        result["commits"],
        json!({
            "observations": [{
                "at": "2026-01-09T10:00:31Z",
                "sha": "9a8b7c6",
                "subject": "chore: bump",
                "branch": "main",
                "created": true,
                "found": "summary-line",
                "call": {"ordinal": result["pair"]["ordinal"], "call_id": "toolu_hook"},
            }]
        })
    );
    let silent = records
        .iter()
        .find(|r| r["call_id"] == "toolu_silent" && r["kind"] == "tool-result")
        .unwrap();
    assert_eq!(
        silent["commits"],
        json!({"observations": [], "created_unnamed": true})
    );
    // One result hit its own cap and none hit the session's: the tail counts
    // the first and omits the second.
    assert_eq!(value["commits"]["truncated_calls"], 1);
    assert!(value["commits"].get("omitted").is_none());
}

/// A Claude recording of `calls` Bash calls, each naming 32 commits.
fn claude_logs(root: &Path, calls: usize) {
    let id = "c5000000-0000-4000-8000-000000000001";
    let mut lines = vec![json!({
        "parentUuid": null, "isSidechain": false, "type": "user", "uuid": "u0",
        "timestamp": "2026-01-09T10:00:00.000Z", "sessionId": id, "cwd": "/work/repo",
        "message": {"role": "user", "content": "Review."}
    })];
    let mut parent = "u0".to_owned();
    for call in 0..calls {
        let command = format!(
            "git show {}",
            (0..MAX_CALL_OBSERVATIONS)
                .map(|line| format!("{:07x}", 0x200_0001 + call * 100 + line))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for (kind, uuid, content) in [
            (
                "assistant",
                format!("a{call}"),
                json!([{"type": "tool_use", "id": format!("toolu_{call}"), "name": "Bash",
                        "input": {"command": command}}]),
            ),
            (
                "user",
                format!("r{call}"),
                json!([{"type": "tool_result", "tool_use_id": format!("toolu_{call}"),
                        "content": "", "is_error": false}]),
            ),
        ] {
            let mut message = json!({"role": kind, "content": content});
            if kind == "assistant" {
                message["id"] = json!(format!("msg_{call}"));
                message["model"] = json!("claude-fixture");
            }
            lines.push(json!({
                "parentUuid": parent, "isSidechain": false, "type": kind, "uuid": uuid,
                "timestamp": format!("2026-01-09T10:{:02}:{:02}.000Z", (call + 1) / 30, (call + 1) % 30 * 2),
                "sessionId": id, "cwd": "/work/repo", "message": message
            }));
            parent = uuid;
        }
    }
    let path = root.join("project").join(format!("{id}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        lines
            .iter()
            .map(|line| line.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
}

#[test]
fn a_session_keeps_its_first_observations_and_counts_the_rest() {
    let root = TempDir::new().unwrap();
    let calls = MAX_SESSION_OBSERVATIONS / MAX_CALL_OBSERVATIONS + 2;
    claude_logs(root.path(), calls);
    let backend = ClaudeBackend::new(root.path().to_path_buf());
    let (bounded, streamed, tail) = both(&backend, "c5000000-0000-4000-8000-000000000001");
    for events in [&bounded.events, &streamed] {
        let kept = events
            .iter()
            .filter_map(|r| r.commits.as_ref())
            .map(|c| c.observations.len())
            .sum::<usize>();
        assert_eq!(kept, MAX_SESSION_OBSERVATIONS);
    }
    assert_eq!(bounded.commits.kept, MAX_SESSION_OBSERVATIONS);
    assert_eq!(bounded.commits.omitted, 2 * MAX_CALL_OBSERVATIONS);
    assert_eq!(bounded.commits.truncated_calls, 0);
    assert_eq!(tail.commits, bounded.commits);
    let value = serde_json::to_value(&bounded).unwrap();
    assert_eq!(value["commits"]["omitted"], 2 * MAX_CALL_OBSERVATIONS);
}
