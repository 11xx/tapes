//! A summary adds sessions only where no recording relates them. These
//! tests drive `usage_summary_with_backends` over synthetic stores: Pi
//! sessions, whose parent and fork both carry event keys, and an OpenCode
//! database whose session table declares a fork.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::{usage_summary_with_backends, SessionSelection};
use serde_json::{json, Value};

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn summary(backends: &[Box<dyn Backend>], harness: &str) -> Value {
    let selection = SessionSelection {
        harness: Some(harness),
        limit: Some(20),
        ..Default::default()
    };
    serde_json::to_value(usage_summary_with_backends(backends, &selection, &[]).unwrap()).unwrap()
}

/// A temporary directory holding copies of fixture files.
struct Copy(PathBuf);

impl Copy {
    fn of(label: &str, source: &Path) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-summary-overlap-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                fs::copy(entry.path(), root.join(entry.file_name())).unwrap();
            }
        }
        Self(root)
    }
}

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A Pi fork records the history it copied from its parent, so the two share
/// events; with event keys on both sides their union is exact.
#[test]
fn a_pi_parent_and_fork_with_event_keys_add_by_their_exact_union() {
    let copy = Copy::of("pi-union", &fixtures("pi-consumption-keys"));
    // A fork's header names the recording it was copied from.
    let parent = "2026-01-15T10-00-00-000Z_keys-parent.jsonl";
    let fork = copy.0.join("2026-01-15T11-00-00-000Z_keys-fork.jsonl");
    let header = fs::read_to_string(&fork).unwrap().replace(
        &format!("/fixtures/{parent}"),
        &copy.0.join(parent).display().to_string(),
    );
    fs::write(fork, header).unwrap();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(&copy.0))];
    let value = summary(&backends, "pi");
    for figure in [&value["totals"], &value["groups"][0]] {
        let related = &figure["overlap"]["related"];
        assert_eq!(related[0]["union"], "exact", "{figure}");
        assert_eq!(related[0]["evidence"], json!(["declared-fork"]), "{figure}");
        assert!(figure.get("identity_unestablished").is_none(), "{figure}");
        // Three parent events and the fork's own request, with the two shared
        // events counted once.
        assert_eq!(figure["cost"]["usd"], 1.25, "{figure}");
        assert_eq!(figure["disjoint"]["total"], 110, "{figure}");
        assert_eq!(figure["disjoint"]["sessions"], 2, "{figure}");
        assert!(figure.get("tokens").is_none(), "{figure}");
        assert_eq!(figure["session_figures"].as_array().unwrap().len(), 2);
    }
}

/// Pi sessions that name no parent and share no ids are summed, and the
/// figure says its disjointness is inferred.
#[test]
fn unrelated_pi_sessions_are_summed_and_labelled_inferred() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures("pi")))];
    let value = summary(&backends, "pi");
    let totals = &value["totals"];
    assert!(totals["sessions"].as_u64().unwrap() > 1, "{totals}");
    assert!(totals.get("identity_unestablished").is_none(), "{totals}");
    assert!(totals.get("session_figures").is_none(), "{totals}");
    assert!(totals.get("tokens").is_some(), "{totals}");
    assert_eq!(
        totals["overlap"],
        json!({"disjointness": "inferred-from-absent-copy-evidence"}),
        "{totals}"
    );
}

/// The session table names a fork's source, which the listing does not
/// carry; the summary asks for it, so the pair is withheld.
#[test]
fn an_opencode_fork_pair_withholds_its_exact_totals() {
    let copy = Copy::of("opencode", &fixtures("opencode"));
    let rows = [
        json!({"id": "ses_database_only_fixture",
               "parent_id": null, "fork_session_id": "ses_000000fixtureSharedSession"}),
        json!({"id": "ses_000000fixtureSharedSession",
               "parent_id": null, "fork_session_id": null}),
    ];
    let body = rows
        .iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
    fs::write(copy.0.join("database-lineage-database-only.json"), body).unwrap();
    fs::create_dir_all(copy.0.join("bin")).unwrap();
    std::os::unix::fs::symlink(copy.0.join("opencode2"), copy.0.join("bin/opencode")).unwrap();
    let data = copy.0.join("data/opencode");
    fs::create_dir_all(&data).unwrap();
    fs::File::create(data.join("opencode.db")).unwrap();
    std::env::set_var("XDG_DATA_HOME", copy.0.join("data"));
    let backends: Vec<Box<dyn Backend>> =
        vec![Box::new(OpenCodeBackend::new(copy.0.join("bin/opencode")))];
    let value = summary(&backends, "opencode");
    let totals = &value["totals"];
    assert_eq!(totals["sessions"], 2, "{totals}");
    assert_eq!(totals["identity_unestablished"], true, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert_eq!(
        totals["overlap"]["related"][0]["evidence"],
        json!(["declared-fork"]),
        "{totals}"
    );
}

/// A temporary store root, removed on drop.
struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-summary-overlap-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(path, body).unwrap();
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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
    );
}

#[test]
fn a_codex_fork_and_its_origin_are_withheld_with_bounds() {
    let root = Root::new("codex-fork");
    let origin = "00000000-0000-0000-0000-0000000000a1";
    codex_rollout(&root, origin, 10, None, 100);
    codex_rollout(
        &root,
        "00000000-0000-0000-0000-0000000000b2",
        11,
        Some(origin),
        130,
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root.0))];
    let totals = summary(&backends, "codex")["totals"].clone();
    assert_eq!(totals["identity_unestablished"], true, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert_eq!(
        totals["identity_bounds"]["input"],
        json!({"at_least": 130, "at_most": 230}),
        "{totals}"
    );
    assert_eq!(
        totals["overlap"]["related"][0]["evidence"],
        json!(["declared-fork"])
    );
}

#[test]
fn unrelated_codex_sessions_are_summed_and_labelled_inferred() {
    let root = Root::new("codex-unrelated");
    codex_rollout(&root, "00000000-0000-0000-0000-0000000000a1", 10, None, 100);
    codex_rollout(&root, "00000000-0000-0000-0000-0000000000b2", 11, None, 130);
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root.0))];
    let totals = summary(&backends, "codex")["totals"].clone();
    assert_eq!(totals["tokens"]["input"], 230, "{totals}");
    assert_eq!(
        totals["overlap"],
        json!({"disjointness": "inferred-from-absent-copy-evidence"}),
        "{totals}"
    );
}

/// A Claude fork rewrites each record's session id and keeps the request ids
/// and message uuids of the history it copied.
fn claude_session(root: &Root, id: &str, requests: &[&str], own: &str) {
    let mut lines = vec![json!({
        "type": "user", "sessionId": id, "uuid": format!("{id}-user"),
        "timestamp": "2026-01-01T10:00:00Z",
        "message": {"role": "user", "content": "fixture"}
    })];
    for request in requests.iter().chain(&[own]) {
        lines.push(json!({
            "type": "assistant", "sessionId": id, "requestId": request, "uuid": format!("msg-{request}"),
            "message": {"role": "assistant", "model": "fixture-model", "content": [],
                        "usage": {"input_tokens": 5, "output_tokens": 7}}
        }));
    }
    root.write(&format!("fixture-project/{id}.jsonl"), &lines);
}

#[test]
fn a_claude_fork_that_kept_its_origins_request_ids_is_withheld() {
    let root = Root::new("claude-fork");
    claude_session(&root, "origin", &[], "req-a");
    claude_session(&root, "fork", &["req-a"], "req-b");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(&root.0))];
    let totals = summary(&backends, "claude")["totals"].clone();
    assert_eq!(totals["identity_unestablished"], true, "{totals}");
    assert!(totals.get("tokens").is_none(), "{totals}");
    assert_eq!(
        totals["identity_bounds"]["output"],
        json!({"at_least": 14, "at_most": 21}),
        "{totals}"
    );
    let evidence = &totals["overlap"]["related"][0]["evidence"];
    assert_eq!(
        evidence,
        &json!(["shared-request-id", "shared-message-uuid"]),
        "{totals}"
    );
}

#[test]
fn unrelated_claude_sessions_are_summed_and_labelled_inferred() {
    let root = Root::new("claude-unrelated");
    claude_session(&root, "first", &[], "req-a");
    claude_session(&root, "second", &[], "req-b");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(&root.0))];
    let totals = summary(&backends, "claude")["totals"].clone();
    assert_eq!(totals["tokens"]["output"], 14, "{totals}");
    assert_eq!(
        totals["overlap"],
        json!({"disjointness": "inferred-from-absent-copy-evidence"}),
        "{totals}"
    );
}
