//! Two forks that declare the same origin hold copies of that origin's
//! history, whether or not the origin itself is selected. A summary relates
//! them and withholds their exact totals instead of adding the copied prefix
//! twice, while an unrelated session beside them stays summed in the bounds.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::{usage_summary_with_backends, SessionSelection};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-shared-origin-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    /// Writes a recording last modified at `hour`, so recency follows the
    /// recorded hour and not the order of writes.
    fn write(&self, relative: &str, lines: &[Value], hour: u32) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(&path, body).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_800_000_000 + u64::from(hour) * 3600))
            .unwrap();
        path
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn totals(backends: &[Box<dyn Backend>], harness: &str, limit: usize) -> Value {
    let selection = SessionSelection {
        harness: Some(harness),
        limit: Some(limit),
        ..Default::default()
    };
    let summary = usage_summary_with_backends(backends, &selection, &[]).unwrap();
    serde_json::to_value(summary).unwrap()["totals"].clone()
}

fn evidence(totals: &Value) -> Vec<Value> {
    totals["overlap"]["related"][0]["evidence"]
        .as_array()
        .unwrap_or_else(|| panic!("no related group: {totals}"))
        .clone()
}

const ORIGIN: &str = "00000000-0000-0000-0000-0000000000a1";
const FIRST: &str = "00000000-0000-0000-0000-0000000000b2";
const SECOND: &str = "00000000-0000-0000-0000-0000000000c3";
const UNRELATED: &str = "00000000-0000-0000-0000-0000000000d4";

fn codex_rollout(root: &Root, id: &str, hour: u32, forked_from: Option<&str>, input: u64) {
    let mut meta = json!({"id": id, "cwd": "/fixtures/project", "model_provider": "openai"});
    if let Some(origin) = forked_from {
        meta["forked_from_id"] = json!(origin);
    }
    let usage = json!({"input_tokens": input, "cached_input_tokens": 0, "output_tokens": 10,
                       "reasoning_output_tokens": 0, "total_tokens": input + 10});
    root.write(
        &format!("2026/01/01/rollout-2026-01-01T{hour:02}-00-00-{id}.jsonl"),
        &[
            json!({"timestamp": format!("2026-01-01T{hour:02}:00:00Z"),
                   "type": "session_meta", "payload": meta}),
            json!({"timestamp": format!("2026-01-01T{hour:02}:00:01Z"), "type": "turn_context",
                   "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
            json!({"timestamp": format!("2026-01-01T{hour:02}:00:02Z"), "type": "event_msg",
                   "payload": {"type": "token_count", "info": {
                       "total_token_usage": usage, "last_token_usage": usage,
                       "model_context_window": 272000}, "rate_limits": null}}),
        ],
        hour,
    );
}

fn codex_store() -> Root {
    let root = Root::new("codex");
    codex_rollout(&root, ORIGIN, 10, None, 100);
    codex_rollout(&root, FIRST, 11, Some(ORIGIN), 130);
    codex_rollout(&root, SECOND, 12, Some(ORIGIN), 140);
    codex_rollout(&root, UNRELATED, 13, None, 7);
    root
}

#[test]
fn codex_forks_of_an_unselected_origin_are_withheld_with_bounds() {
    let root = codex_store();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root.0))];
    // The limit keeps the newest three: both forks and the unrelated session.
    let totals = totals(&backends, "codex", 3);
    assert_eq!(totals["sessions"], 3, "{totals}");
    assert_eq!(totals["identity_unestablished"], true, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert_eq!(
        totals["identity_bounds"]["input"],
        json!({"at_least": 147, "at_most": 277}),
        "{totals}"
    );
    assert_eq!(evidence(&totals), vec![json!("shared-declared-origin")]);
    assert_eq!(
        totals["overlap"]["related"][0]["union"], "withheld",
        "{totals}"
    );
}

#[test]
fn codex_forks_with_their_origin_selected_are_one_withheld_group() {
    let root = codex_store();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root.0))];
    let totals = totals(&backends, "codex", 20);
    assert_eq!(totals["sessions"], 4, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert_eq!(
        totals["identity_bounds"]["input"],
        json!({"at_least": 147, "at_most": 377}),
        "{totals}"
    );
    assert_eq!(
        totals["overlap"]["related"][0]["sessions"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "{totals}"
    );
    assert!(evidence(&totals).contains(&json!("declared-fork")));
}

fn pi_request(id: &str, parent: Option<&str>, minute: u32, input: u64) -> Value {
    json!({"type": "message", "id": id, "parentId": parent,
           "timestamp": format!("2026-01-15T10:{minute:02}:01Z"),
           "message": {"role": "assistant", "provider": "fixture-provider", "model": "fixture-model",
                       "responseId": format!("response-{id}"), "stopReason": "stop",
                       "usage": {"input": input, "output": 1, "reasoning": 0, "cacheRead": 0,
                                 "cacheWrite": 0, "totalTokens": input + 1,
                                 "cost": {"total": 0.125}},
                       "content": [{"type": "text", "text": "A response."}]}})
}

fn pi_header(id: &str, hour: u32, parent: Option<&Path>) -> Value {
    let mut line = json!({"type": "session", "version": 3, "id": id,
                          "timestamp": format!("2026-01-15T{hour:02}:00:00Z"),
                          "cwd": "/fixtures/project"});
    if let Some(parent) = parent {
        line["parentSession"] = json!(parent.display().to_string());
    }
    line
}

fn pi_store() -> Root {
    let root = Root::new("pi");
    let directory = "--fixtures-project--";
    let shared = pi_request("shared-request", None, 0, 10);
    let origin = root.write(
        &format!("{directory}/2026-01-15T10-00-00-000Z_{ORIGIN}.jsonl"),
        &[
            pi_header(ORIGIN, 10, None),
            shared.clone(),
            pi_request("origin-request", Some("shared-request"), 1, 30),
        ],
        10,
    );
    for (id, hour, input) in [(FIRST, 11, 40), (SECOND, 12, 50)] {
        root.write(
            &format!("{directory}/2026-01-15T{hour:02}-00-00-000Z_{id}.jsonl"),
            &[
                pi_header(id, hour, Some(&origin)),
                shared.clone(),
                pi_request(&format!("own-{id}"), Some("shared-request"), 2, input),
            ],
            hour,
        );
    }
    root.write(
        &format!("{directory}/2026-01-15T13-00-00-000Z_{UNRELATED}.jsonl"),
        &[
            pi_header(UNRELATED, 13, None),
            pi_request("unrelated-request", None, 3, 7),
        ],
        13,
    );
    root
}

#[test]
fn pi_forks_of_an_unselected_origin_are_withheld_with_bounds() {
    let root = pi_store();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(&root.0))];
    let totals = totals(&backends, "pi", 3);
    assert_eq!(totals["sessions"], 3, "{totals}");
    assert_eq!(totals["identity_unestablished"], true, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert!(totals.get("cost").is_none(), "{totals}");
    assert!(totals["disjoint"].get("total").is_none(), "{totals}");
    // Each fork holds the shared request (10) and its own (40, 50); the
    // unrelated session adds 7.
    assert_eq!(
        totals["identity_bounds"]["input"],
        json!({"at_least": 67, "at_most": 117}),
        "{totals}"
    );
    assert_eq!(evidence(&totals), vec![json!("shared-declared-origin")]);
    // Neither fork's keys name the other as its parent.
    assert_eq!(
        totals["overlap"]["related"][0]["withheld"],
        json!(["unestablished-lineage"]),
        "{totals}"
    );
}

#[test]
fn pi_forks_with_their_origin_selected_are_one_withheld_group() {
    let root = pi_store();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(&root.0))];
    let totals = totals(&backends, "pi", 20);
    assert_eq!(totals["sessions"], 4, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert_eq!(
        totals["identity_bounds"]["input"],
        json!({"at_least": 67, "at_most": 157}),
        "{totals}"
    );
    assert!(evidence(&totals).contains(&json!("declared-fork")));
}

/// A copy of the OpenCode fixture whose session table declares `rows`.
fn opencode_totals(label: &str, rows: &[Value]) -> Value {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
    let root = Root::new(label);
    for entry in fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), root.0.join(entry.file_name())).unwrap();
        }
    }
    let body = rows
        .iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
    fs::write(root.0.join("database-lineage-database-only.json"), body).unwrap();
    fs::create_dir_all(root.0.join("bin")).unwrap();
    std::os::unix::fs::symlink(root.0.join("opencode2"), root.0.join("bin/opencode")).unwrap();
    let data = root.0.join("data/opencode");
    fs::create_dir_all(&data).unwrap();
    fs::File::create(data.join("opencode.db")).unwrap();
    std::env::set_var("XDG_DATA_HOME", root.0.join("data"));
    let backends: Vec<Box<dyn Backend>> =
        vec![Box::new(OpenCodeBackend::new(root.0.join("bin/opencode")))];
    totals(&backends, "opencode", 20)
}

const OPENCODE_SESSIONS: [&str; 2] = [
    "ses_database_only_fixture",
    "ses_000000fixtureSharedSession",
];

/// Both OpenCode cases run in one test: the fixture's binary reads its
/// database from `XDG_DATA_HOME`, which is process-wide.
#[test]
fn opencode_forks_of_one_source_relate_and_children_of_one_parent_do_not() {
    let forks = OPENCODE_SESSIONS
        .map(|id| json!({"id": id, "parent_id": null, "fork_session_id": "ses_unselected_source"}));
    let totals = opencode_totals("opencode-forks", &forks);
    assert_eq!(totals["sessions"], 2, "{totals}");
    assert_eq!(totals["identity_unestablished"], true, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert_eq!(evidence(&totals), vec![json!("shared-declared-origin")]);

    // A child starts its own history, so children of one parent share none.
    let children = OPENCODE_SESSIONS
        .map(|id| json!({"id": id, "parent_id": "ses_unselected_parent", "fork_session_id": null}));
    let totals = opencode_totals("opencode-children", &children);
    assert_eq!(totals["sessions"], 2, "{totals}");
    assert!(totals.get("identity_unestablished").is_none(), "{totals}");
    assert!(totals.get("tokens").is_some(), "{totals}");
    assert!(totals["overlap"].get("related").is_none(), "{totals}");
}
