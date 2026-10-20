//! A result reports the file edits its call made, as digests of the lines
//! added and removed. The digests are the git reader's, so an edit compares
//! with a commit directly; no line text is kept. A shell call that moved the
//! working tree reports a tree movement, and a shell call that wrote files
//! without a recorded diff is counted as unrecognized.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::edit_observations::{
    EditOp, EditShape, MAX_CALL_EDITS, MAX_DIGESTS_PER_SIDE, MAX_SESSION_DIGESTS, MAX_SESSION_EDITS,
};
use agent_tapes_core::event::{EventFilter, EventOptions, EventRecord, EventTranscript};
use agent_tapes_core::git_reader::{line_digest, GitReader, Selector};
use agent_tapes_core::model::Session;
use agent_tapes_core::{events_full_for_session, EventKind, EventSink, EVENTS_SCHEMA};
use serde_json::json;
use tempfile::TempDir;

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/edit-observations")
        .join(harness)
}

/// One reported edit, with its lines in place of their digests.
#[derive(Debug, PartialEq)]
struct Seen {
    path: String,
    op: EditOp,
    shape: EditShape,
    tool: String,
    added: Vec<u64>,
    removed: Vec<u64>,
}

fn lines(lines: &[&str]) -> Vec<u64> {
    lines.iter().filter_map(|line| line_digest(line)).collect()
}

fn seen(
    path: &str,
    op: EditOp,
    shape: EditShape,
    tool: &str,
    added: &[&str],
    removed: &[&str],
) -> Seen {
    Seen {
        path: path.to_owned(),
        op,
        shape,
        tool: tool.to_owned(),
        added: lines(added),
        removed: lines(removed),
    }
}

#[derive(Debug, Default, PartialEq)]
struct Reported {
    edits: Vec<Seen>,
    tree_movement: Option<(usize, usize, usize, usize)>,
    more_files: usize,
    unrecognized: bool,
}

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
            let edits = record.edits.as_ref()?;
            Some((
                key(record.event.call_id.as_deref()?),
                Reported {
                    edits: edits
                        .observations
                        .iter()
                        .map(|o| Seen {
                            path: o.path.clone(),
                            op: o.op,
                            shape: o.source.shape,
                            tool: o.source.tool.clone(),
                            added: o.added.clone(),
                            removed: o.removed.clone(),
                        })
                        .collect(),
                    tree_movement: edits
                        .tree_movement
                        .map(|m| (m.files, m.more_files, m.added_lines, m.removed_lines)),
                    more_files: edits.more_files,
                    unrecognized: edits.unrecognized,
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

fn only(edits: Vec<Seen>) -> Reported {
    Reported {
        edits,
        ..Reported::default()
    }
}

fn unrecognized() -> Reported {
    Reported {
        unrecognized: true,
        ..Reported::default()
    }
}

fn claude_expected() -> BTreeMap<String, Reported> {
    use EditOp::*;
    use EditShape::*;
    let mut expected = BTreeMap::new();
    let mut add = |key: &str, reported: Reported| {
        expected.insert(key.to_owned(), reported);
    };
    add(
        "edit",
        only(vec![seen(
            "/work/repo/src/lib.rs",
            Edit,
            StructuredPatch,
            "Edit",
            &["fn new_name() {}", "    let added = 1;"],
            &["fn old() {}"],
        )]),
    );
    add(
        "edit-oldnew",
        only(vec![seen(
            "/work/repo/src/util.rs",
            Edit,
            OldNew,
            "Edit",
            &["B"],
            &["b"],
        )]),
    );
    add(
        "multiedit",
        only(vec![seen(
            "/work/repo/src/multi.rs",
            Edit,
            OldNew,
            "MultiEdit",
            &["uno", "dos", "tres"],
            &["one", "two"],
        )]),
    );
    add(
        "write-new",
        only(vec![seen(
            "/work/repo/src/new.rs",
            Write,
            Content,
            "Write",
            &["pub fn fresh() {", "    1", "}"],
            &[],
        )]),
    );
    add(
        "write-over",
        only(vec![seen(
            "/work/repo/README.md",
            Write,
            StructuredPatch,
            "Write",
            &["new body"],
            &["old body"],
        )]),
    );
    add(
        "notebook",
        only(vec![seen(
            "/work/repo/analysis.ipynb",
            Edit,
            Content,
            "NotebookEdit",
            &["x = 1", "print(x)"],
            &[],
        )]),
    );
    add(
        "shell-sed",
        only(vec![seen(
            "/work/repo/src/lib.rs",
            Shell,
            BashEditDiff,
            "Bash",
            &["let newname = 1;"],
            &["let oldname = 1;"],
        )]),
    );
    add(
        "shell-script",
        only(vec![
            seen(
                "/work/repo/gen.rs",
                ShellCreate,
                BashEditDiff,
                "Bash",
                &["fn g() {}"],
                &[],
            ),
            seen(
                "/work/repo/old.rs",
                ShellDelete,
                BashEditDiff,
                "Bash",
                &[],
                &["fn gone() {}"],
            ),
        ]),
    );
    // files, files the recording did not list, added lines, removed lines
    add(
        "tree-checkout",
        Reported {
            tree_movement: Some((2, 3, 2, 2)),
            more_files: 3,
            ..Reported::default()
        },
    );
    add(
        "tree-merge",
        Reported {
            tree_movement: Some((1, 0, 1, 0)),
            ..Reported::default()
        },
    );
    add("shell-unavailable", unrecognized());
    add("shell-unrecorded", unrecognized());
    expected
}

fn pi_expected() -> BTreeMap<String, Reported> {
    use EditOp::*;
    use EditShape::*;
    let mut expected = BTreeMap::new();
    expected.insert(
        "edit".to_owned(),
        only(vec![seen(
            "src/lib.rs",
            Edit,
            UnifiedDiff,
            "edit",
            &["fn new_name() {}", "    let added = 1;"],
            &["fn old() {}"],
        )]),
    );
    expected.insert(
        "edit-nopatch".to_owned(),
        only(vec![seen(
            "src/util.rs",
            Edit,
            OldNew,
            "edit",
            &["B", "zz"],
            &["b", "z"],
        )]),
    );
    expected.insert(
        "write".to_owned(),
        only(vec![seen(
            "src/new.rs",
            Write,
            Content,
            "write",
            &["pub fn fresh() {", "    1", "}"],
            &[],
        )]),
    );
    expected.insert("shell-sed".to_owned(), unrecognized());
    expected
}

fn codex_expected() -> BTreeMap<String, Reported> {
    use EditOp::*;
    use EditShape::*;
    let mut expected = BTreeMap::new();
    expected.insert(
        "patch".to_owned(),
        only(vec![seen(
            "src/lib.rs",
            Edit,
            ApplyPatch,
            "apply_patch",
            &["fn new_name() {}", "    let added = 1;"],
            &["fn old() {}"],
        )]),
    );
    // The reader leaves out a runtime record that follows the call it mirrors,
    // so the patch text is the record.
    expected.insert(
        "patch-and-runtime".to_owned(),
        only(vec![seen(
            "src/twice.rs",
            Edit,
            ApplyPatch,
            "apply_patch",
            &["new"],
            &["old"],
        )]),
    );
    // A runtime record that comes first wins over the patch text of its call.
    expected.insert(
        "runtime-first".to_owned(),
        only(vec![seen(
            "src/first.rs",
            Edit,
            UnifiedDiff,
            "FileChange",
            &["after"],
            &["before"],
        )]),
    );
    expected.insert(
        "runtime".to_owned(),
        only(vec![
            seen(
                "src/gone.rs",
                Delete,
                Content,
                "FileChange",
                &[],
                &["fn gone() {}"],
            ),
            seen(
                "src/lib.rs",
                Edit,
                UnifiedDiff,
                "FileChange",
                &["fn new_name() {}", "    let added = 1;"],
                &["fn old() {}"],
            ),
            seen(
                "src/new.rs",
                Write,
                Content,
                "FileChange",
                &["pub fn fresh() {", "    1", "}"],
                &[],
            ),
        ]),
    );
    expected.insert("shell-sed".to_owned(), unrecognized());
    expected
}

fn assert_scenarios(
    backend: &dyn Backend,
    id: &str,
    expected: BTreeMap<String, Reported>,
    counts: (usize, usize, usize, usize),
) {
    let (bounded, streamed, tail) = both(backend, id);
    assert_eq!(reported(&bounded.events), expected, "bounded read");
    assert_eq!(reported(&streamed), expected, "whole-recording stream");
    let (kept, tree_movements, tree_movement_files, unrecognized) = counts;
    let c = bounded.edits;
    assert_eq!(
        (
            c.kept,
            c.tree_movements,
            c.tree_movement_files,
            c.unrecognized
        ),
        (kept, tree_movements, tree_movement_files, unrecognized)
    );
    assert_eq!((c.omitted, c.digests_omitted, c.truncated_calls), (0, 0, 0));
    assert_eq!(tail.edits, bounded.edits);
}

#[test]
fn claude_reports_the_edits_its_tools_and_shell_calls_made() {
    assert_scenarios(
        &ClaudeBackend::new(fixtures("claude")),
        "c6000000-0000-4000-8000-000000000001",
        claude_expected(),
        (9, 2, 3, 2),
    );
}

#[test]
fn pi_reports_the_edits_its_edit_and_write_tools_made() {
    assert_scenarios(
        &PiBackend::new(fixtures("pi")),
        "c6000000-0000-4000-8000-000000000001",
        pi_expected(),
        (3, 0, 0, 1),
    );
}

#[test]
fn codex_reports_the_edits_its_patches_and_runtime_records_made() {
    assert_scenarios(
        &CodexBackend::new(fixtures("codex")),
        "c6000000-0000-7000-8000-000000000001",
        codex_expected(),
        (6, 0, 0, 1),
    );
}

#[test]
fn failed_edits_and_reads_yield_nothing() {
    for (backend, id, quiet) in [
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))) as Box<dyn Backend>,
            "c6000000-0000-4000-8000-000000000001",
            vec!["edit-failed", "shell-read"],
        ),
        (
            Box::new(PiBackend::new(fixtures("pi"))),
            "c6000000-0000-4000-8000-000000000001",
            vec!["edit-failed", "shell-read"],
        ),
        (
            Box::new(CodexBackend::new(fixtures("codex"))),
            "c6000000-0000-7000-8000-000000000001",
            vec!["patch-failed", "runtime-failed", "shell-read"],
        ),
    ] {
        let session = backend.locate(id).unwrap().unwrap();
        let events = backend.events(&session, usize::MAX).unwrap();
        let found = reported(&events.events);
        for name in quiet {
            assert!(
                !found.contains_key(name),
                "{} reported edits for {name}: {:?}",
                session.harness(),
                found[name]
            );
        }
    }
}

#[test]
fn a_tree_movement_is_not_an_authored_edit() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = backend
        .locate("c6000000-0000-4000-8000-000000000001")
        .unwrap()
        .unwrap();
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    // Neither the checkout's nor the merge's files are among the edits.
    for movement_path in ["/work/repo/a.rs", "/work/repo/b.rs", "/work/repo/c.rs"] {
        assert!(
            all.observations
                .iter()
                .all(|edit| edit.path != movement_path),
            "{movement_path} was reported as an edit"
        );
    }
    assert_eq!(all.counts.tree_movements, 2);
    assert_eq!(all.counts.tree_movement_files, 3);
}

#[test]
fn the_events_json_carries_edits_on_the_result_with_no_line_text() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = backend
        .locate("c6000000-0000-4000-8000-000000000001")
        .unwrap()
        .unwrap();
    let events = backend.events(&session, usize::MAX).unwrap();
    let value = serde_json::to_value(&events).unwrap();
    assert_eq!(value["schema"], EVENTS_SCHEMA);
    assert_eq!(EVENTS_SCHEMA, "tapes-events/16");
    let records = value["events"].as_array().unwrap();
    let find = |id: &str, kind: &str| {
        records
            .iter()
            .find(|r| r["call_id"] == id && r["kind"] == kind)
            .unwrap()
    };
    assert!(find("toolu_edit", "tool-call").get("edits").is_none());
    let result = find("toolu_edit", "tool-result");
    assert_eq!(
        result["edits"],
        json!({
            "observations": [{
                "at": "2026-01-10T10:00:11Z",
                "path": "/work/repo/src/lib.rs",
                "op": "edit",
                "added": lines(&["fn new_name() {}", "    let added = 1;"]),
                "removed": lines(&["fn old() {}"]),
                "source": {"tool": "Edit", "shape": "structured-patch"},
                "call": {"ordinal": result["pair"]["ordinal"], "call_id": "toolu_edit"},
            }]
        })
    );
    let text = serde_json::to_string(&value["events"]).unwrap();
    let digested = serde_json::to_string(&result["edits"]).unwrap();
    assert!(
        !digested.contains("new_name") && !digested.contains("added = 1"),
        "{digested}"
    );
    assert!(
        text.contains("new_name"),
        "the tool call itself still shows its input"
    );
    assert_eq!(
        find("toolu_tree-checkout", "tool-result")["edits"],
        json!({
            "observations": [],
            "tree_movement": {"files": 2, "more_files": 3, "added_lines": 2, "removed_lines": 2},
            "more_files": 3,
        })
    );
    assert_eq!(value["edits"]["kept"], 9);
    assert_eq!(value["edits"]["tree_movements"], 2);
    assert_eq!(value["edits"]["unrecognized"], 2);
}

#[test]
fn the_library_reports_a_session_s_edits_in_recording_order() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = backend
        .locate("c6000000-0000-4000-8000-000000000001")
        .unwrap()
        .unwrap();
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    let events = backend.events(&session, usize::MAX).unwrap();
    let from_events = events
        .events
        .iter()
        .filter_map(|record| record.edits.as_ref())
        .flat_map(|edits| edits.observations.clone())
        .collect::<Vec<_>>();
    assert_eq!(all.observations, from_events);
    assert_eq!(all.observations.len(), 9);
    assert_eq!(all.counts, events.edits);
}

// ------------------------------------------------------------------ digests

/// A synthetic repository whose git sees no user or system configuration.
struct Repo {
    root: TempDir,
    clock: Cell<i64>,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            root: TempDir::with_prefix("tapes-edit-observations-").unwrap(),
            clock: Cell::new(1_700_000_000),
        };
        fs::write(repo.config(), "").unwrap();
        fs::create_dir(repo.dir()).unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("gitconfig")
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.dir())
            .args(["-c", "user.name=Fixture Author"])
            .args(["-c", "user.email=author@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .env("GIT_CONFIG_GLOBAL", self.config())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn commit(&self, name: &str, contents: &str) -> String {
        fs::write(self.dir().join(name), contents).unwrap();
        self.git(&["add", "-A"]);
        let tick = self.clock.get() + 60;
        self.clock.set(tick);
        self.git(&[
            "commit",
            "-q",
            "-m",
            "change",
            "--date",
            &format!("{tick} +0000"),
        ]);
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }
}

#[test]
fn an_edits_digests_equal_the_matching_commits_from_the_git_reader() {
    // Rejects: digests that differ from the reader's for the same lines
    // (trailing whitespace kept, blank and punctuation-only lines counted, a
    // different hash), which would make an edit and its commit incomparable.
    let repo = Repo::new();
    let before = "fn keep() {}\nfn old() {}   \n\n}\nlet tail = 1;\n";
    let after = "fn keep() {}\nfn new_name() {}\n    let added = 2;  \n\n}\n}\nlet tail = 1;\n";
    repo.commit("src.rs", before);
    let sha = repo.commit("src.rs", after);
    let commit = GitReader::at(&repo.dir())
        .unwrap()
        .unwrap()
        .with_env("GIT_CONFIG_GLOBAL", repo.config())
        .with_env("GIT_CONFIG_NOSYSTEM", "1")
        .commit(&sha)
        .unwrap();
    let path = commit.paths.iter().find(|p| p.path == "src.rs").unwrap();
    assert!(!path.added.is_empty() && !path.removed.is_empty());

    // The same change as each harness records it, from git's own diff.
    let diff = repo.git(&["diff", "-U1", "HEAD~1", "HEAD", "--", "src.rs"]);
    let hunks_start = diff.find("@@").unwrap();
    let hunks = &diff[hunks_start..];
    let hunk_lines = hunks
        .lines()
        .filter(|line| !line.starts_with("@@"))
        .map(str::to_owned)
        .collect::<Vec<_>>();

    // Pi: the unified patch with its file headers.
    let pi_root = TempDir::new().unwrap();
    let id = "c7000000-0000-4000-8000-000000000001";
    let pi_lines = [
        json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-01-11T10:00:00.000Z", "cwd": "/work"}),
        json!({"type": "message", "id": "m1", "parentId": null, "timestamp": "2026-01-11T10:00:01.000Z",
               "message": {"role": "user", "timestamp": 1, "content": [{"type": "text", "text": "Edit."}]}}),
        json!({"type": "message", "id": "m2", "parentId": "m1", "timestamp": "2026-01-11T10:00:02.000Z",
               "message": {"role": "assistant", "model": "gpt-fixture", "content": [{"type": "toolCall", "id": "call_1", "name": "edit",
                           "arguments": {"path": "src.rs", "edits": [{"oldText": "x", "newText": "y"}]}}]}}),
        json!({"type": "message", "id": "m3", "parentId": "m2", "timestamp": "2026-01-11T10:00:03.000Z",
               "message": {"role": "toolResult", "toolCallId": "call_1", "toolName": "edit", "isError": false, "timestamp": 2,
                           "content": [{"type": "text", "text": "ok"}],
                           "details": {"patch": format!("--- src.rs\n+++ src.rs\n{hunks}"), "firstChangedLine": 2}}}),
    ];
    let pi_file = pi_root
        .path()
        .join(format!("2026-01-11T10-00-00-000Z_{id}.jsonl"));
    fs::write(
        &pi_file,
        pi_lines
            .iter()
            .map(|l| l.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let backend = PiBackend::new(pi_root.path().to_path_buf());
    let session = backend.locate(id).unwrap().unwrap();
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    assert_eq!(all.observations.len(), 1);
    assert_eq!(all.observations[0].added, path.added);
    assert_eq!(all.observations[0].removed, path.removed);

    // Claude: the same hunk as numbered lines.
    let claude_root = TempDir::new().unwrap();
    let id = "c7000000-0000-4000-8000-000000000002";
    let claude_lines = [
        json!({"parentUuid": null, "isSidechain": false, "type": "user", "uuid": "u0", "timestamp": "2026-01-11T10:00:00.000Z",
               "sessionId": id, "cwd": "/work", "message": {"role": "user", "content": "Edit."}}),
        json!({"parentUuid": "u0", "isSidechain": false, "type": "assistant", "uuid": "a1", "timestamp": "2026-01-11T10:00:01.000Z",
               "sessionId": id, "cwd": "/work", "message": {"id": "m1", "role": "assistant", "model": "claude-fixture",
               "content": [{"type": "tool_use", "id": "toolu_1", "name": "Edit", "input": {"file_path": "/work/src.rs", "old_string": "x", "new_string": "y"}}]}}),
        json!({"parentUuid": "a1", "isSidechain": false, "type": "user", "uuid": "r1", "timestamp": "2026-01-11T10:00:02.000Z",
               "sessionId": id, "cwd": "/work", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok", "is_error": false}]},
               "toolUseResult": {"filePath": "/work/src.rs", "oldString": "x", "newString": "y",
                                 "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": hunk_lines}]}}),
    ];
    let claude_file = claude_root
        .path()
        .join("project")
        .join(format!("{id}.jsonl"));
    fs::create_dir_all(claude_file.parent().unwrap()).unwrap();
    fs::write(
        &claude_file,
        claude_lines
            .iter()
            .map(|l| l.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let backend = ClaudeBackend::new(claude_root.path().to_path_buf());
    let session = backend.locate(id).unwrap().unwrap();
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    assert_eq!(all.observations.len(), 1);
    assert_eq!(all.observations[0].added, path.added);
    assert_eq!(all.observations[0].removed, path.removed);

    // Codex: the runtime's unified diff hunks.
    let codex_root = TempDir::new().unwrap();
    let id = "c7000000-0000-7000-8000-000000000003";
    let codex_lines = [
        json!({"timestamp": "2026-01-11T10:00:00Z", "type": "session_meta", "payload": {"id": id, "session_id": id, "timestamp": "2026-01-11T10:00:00Z", "cwd": "/work", "model_provider": "openai"}}),
        json!({"timestamp": "2026-01-11T10:00:01Z", "type": "turn_context", "payload": {"cwd": "/work", "model": "gpt-fixture", "effort": "high"}}),
        json!({"timestamp": "2026-01-11T10:00:02Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Edit."}]}}),
        json!({"timestamp": "2026-01-11T10:00:03Z", "type": "event_msg", "payload": {"type": "item_completed", "thread_id": id, "turn_id": "t1",
               "item": {"type": "FileChange", "id": "call_1", "status": "completed", "stdout": "ok",
                        "changes": {"src.rs": {"type": "update", "unified_diff": hunks, "move_path": null}}}}}),
    ];
    let codex_file = codex_root
        .path()
        .join(format!("rollout-2026-01-11T10-00-00-{id}.jsonl"));
    fs::write(
        &codex_file,
        codex_lines
            .iter()
            .map(|l| l.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let backend = CodexBackend::new(codex_root.path().to_path_buf());
    let session = backend.locate(id).unwrap().unwrap();
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    assert_eq!(all.observations.len(), 1);
    assert_eq!(all.observations[0].added, path.added);
    assert_eq!(all.observations[0].removed, path.removed);

    // And the commit selector the reader exposes finds the same commit.
    let set = GitReader::at(&repo.dir())
        .unwrap()
        .unwrap()
        .with_env("GIT_CONFIG_GLOBAL", repo.config())
        .with_env("GIT_CONFIG_NOSYSTEM", "1")
        .commits(&Selector::parse(&sha))
        .unwrap();
    assert_eq!(set.commits[0].sha, sha);
}

// --------------------------------------------------------------------- caps

fn claude_file(root: &Path, id: &str, results: Vec<serde_json::Value>) {
    let mut lines = vec![json!({
        "parentUuid": null, "isSidechain": false, "type": "user", "uuid": "u0",
        "timestamp": "2026-01-11T10:00:00.000Z", "sessionId": id, "cwd": "/work",
        "message": {"role": "user", "content": "Write."}
    })];
    let mut parent = "u0".to_owned();
    for (index, (input, result)) in results
        .into_iter()
        .map(|r| (r["input"].clone(), r))
        .enumerate()
    {
        let uuid_a = format!("a{index}");
        let uuid_r = format!("r{index}");
        let (name, tur) = (
            result["name"].as_str().unwrap().to_owned(),
            result["tur"].clone(),
        );
        lines.push(json!({
            "parentUuid": parent, "isSidechain": false, "type": "assistant", "uuid": uuid_a,
            "timestamp": "2026-01-11T10:00:01.000Z", "sessionId": id, "cwd": "/work",
            "message": {"id": format!("m{index}"), "role": "assistant", "model": "claude-fixture",
                        "content": [{"type": "tool_use", "id": format!("toolu_{index}"), "name": name, "input": input}]}
        }));
        lines.push(json!({
            "parentUuid": uuid_a, "isSidechain": false, "type": "user", "uuid": uuid_r,
            "timestamp": "2026-01-11T10:00:02.000Z", "sessionId": id, "cwd": "/work",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("toolu_{index}"), "content": "ok", "is_error": false}]},
            "toolUseResult": tur
        }));
        parent = uuid_r;
    }
    let path = root.join("project").join(format!("{id}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        lines
            .iter()
            .map(|l| l.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
}

fn write_call(index: usize, lines_each: usize) -> serde_json::Value {
    let content = (0..lines_each)
        .map(|line| format!("line {index} {line}\n"))
        .collect::<String>();
    json!({
        "name": "Write",
        "input": {"file_path": format!("/work/f{index}.rs"), "content": content},
        "tur": {"type": "create", "filePath": format!("/work/f{index}.rs"), "content": content, "structuredPatch": []},
    })
}

#[test]
fn an_edit_keeps_at_most_the_per_edit_digests_and_says_so() {
    let root = TempDir::new().unwrap();
    let id = "c8000000-0000-4000-8000-000000000001";
    claude_file(
        root.path(),
        id,
        vec![write_call(0, MAX_DIGESTS_PER_SIDE + 50)],
    );
    let backend = ClaudeBackend::new(root.path().to_path_buf());
    let session = backend.locate(id).unwrap().unwrap();
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    assert_eq!(all.observations[0].added.len(), MAX_DIGESTS_PER_SIDE);
    assert!(all.observations[0].truncated);
}

#[test]
fn a_result_keeps_at_most_the_per_call_edits_and_says_so() {
    let root = TempDir::new().unwrap();
    let id = "c8000000-0000-4000-8000-000000000002";
    let files = (0..MAX_CALL_EDITS + 8)
        .map(|index| {
            json!({"filePath": format!("/work/f{index}.rs"), "hunks": [{"lines": [format!("+line {index}")]}]})
        })
        .collect::<Vec<_>>();
    claude_file(
        root.path(),
        id,
        vec![json!({
            "name": "Bash",
            "input": {"command": "sed -i s/a/b/ f.rs"},
            "tur": {"stdout": "", "stderr": "", "bashEditDiff": {"files": files, "moreFiles": 0}},
        })],
    );
    let backend = ClaudeBackend::new(root.path().to_path_buf());
    let session = backend.locate(id).unwrap().unwrap();
    let events = backend.events(&session, usize::MAX).unwrap();
    let edits = events
        .events
        .iter()
        .find_map(|record| record.edits.as_ref())
        .unwrap();
    assert_eq!(edits.observations.len(), MAX_CALL_EDITS);
    assert!(edits.truncated);
    assert_eq!(events.edits.truncated_calls, 1);
}

#[test]
fn a_session_keeps_its_first_edits_and_counts_the_rest() {
    let root = TempDir::new().unwrap();
    let id = "c8000000-0000-4000-8000-000000000003";
    claude_file(
        root.path(),
        id,
        (0..MAX_SESSION_EDITS + 3)
            .map(|index| write_call(index, 2))
            .collect(),
    );
    let backend = ClaudeBackend::new(root.path().to_path_buf());
    let session = backend.locate(id).unwrap().unwrap();
    let (bounded, streamed, tail) = both(&backend, id);
    let kept = |events: &[EventRecord]| {
        events
            .iter()
            .filter_map(|r| r.edits.as_ref())
            .map(|e| e.observations.len())
            .sum::<usize>()
    };
    assert_eq!(kept(&bounded.events), MAX_SESSION_EDITS);
    assert_eq!(kept(&streamed), MAX_SESSION_EDITS);
    assert_eq!(bounded.edits.kept, MAX_SESSION_EDITS);
    assert_eq!(bounded.edits.omitted, 3);
    assert_eq!(tail.edits, bounded.edits);
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    assert_eq!(all.counts.omitted, 3);
    let value = serde_json::to_value(&bounded).unwrap();
    assert_eq!(value["edits"]["omitted"], 3);
}

#[test]
fn a_session_keeps_digests_up_to_its_budget_and_says_which_edits_lost_them() {
    let root = TempDir::new().unwrap();
    let id = "c8000000-0000-4000-8000-000000000004";
    let writes = MAX_SESSION_DIGESTS / MAX_DIGESTS_PER_SIDE + 3;
    claude_file(
        root.path(),
        id,
        (0..writes)
            .map(|index| write_call(index, MAX_DIGESTS_PER_SIDE))
            .collect(),
    );
    let backend = ClaudeBackend::new(root.path().to_path_buf());
    let session = backend.locate(id).unwrap().unwrap();
    let all = agent_tapes_core::session_edits(&backend, &session).unwrap();
    assert_eq!(all.observations.len(), writes);
    assert_eq!(all.counts.digests_omitted, 3);
    let bare = all
        .observations
        .iter()
        .filter(|e| e.added.is_empty())
        .count();
    assert_eq!(bare, 3);
    assert!(all
        .observations
        .iter()
        .filter(|e| e.added.is_empty())
        .all(|e| e.truncated));
}
