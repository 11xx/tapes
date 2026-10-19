//! A backend declares the accounting bases its reader can produce, and no
//! figure a reader attaches to a session carries another. A consumer relies
//! on the declaration to decide which basis totals an unmeasured run may
//! belong to.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend, Backend,
    Query,
};
use agent_tapes_core::model::{AccountingBasis, Session};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-bases-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
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

/// Every basis in the accounting of a session and the figures it carries.
fn session_bases(session: &Session) -> Vec<AccountingBasis> {
    let mut bases: Vec<_> = session.accounting.iter().map(|a| a.basis).collect();
    if let Some(detail) = &session.usage_detail {
        bases.extend(detail.recorded_total.iter().map(|r| r.accounting.basis));
        bases.extend(detail.consumption.iter().map(|c| c.accounting.basis));
    }
    bases
}

fn view_bases(view: &Value) -> Vec<String> {
    [
        &view["accounting"]["basis"],
        &view["recorded_total"]["accounting"]["basis"],
        &view["consumption"]["accounting"]["basis"],
    ]
    .into_iter()
    .filter_map(|basis| basis.as_str().map(str::to_owned))
    .collect()
}

fn label(basis: AccountingBasis) -> String {
    serde_json::to_value(basis)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

/// Read every session of `backend` the ways a consumer can, and return the
/// bases produced, failing on one the backend did not declare.
fn produced(backend: Box<dyn Backend>) -> BTreeSet<String> {
    let declared: Vec<String> = backend
        .accounting_bases()
        .iter()
        .map(|basis| label(*basis))
        .collect();
    let harness = backend.harness();
    let sessions = backend.list(&Query::unscoped(50)).unwrap().sessions;
    assert!(!sessions.is_empty(), "{harness} fixture lists no session");
    let mut seen = BTreeSet::new();
    let mut check = |origin: &str, bases: Vec<String>| {
        for basis in bases {
            assert!(
                declared.contains(&basis),
                "{harness} produced undeclared basis {basis} ({origin}); declared {declared:?}"
            );
            seen.insert(basis);
        }
    };
    let backends = vec![backend];
    for session in &sessions {
        check(
            "listing",
            session_bases(session).into_iter().map(label).collect(),
        );
        let transcript = backends[0].transcript(session, 20).unwrap();
        check(
            "transcript",
            session_bases(&transcript.session)
                .into_iter()
                .map(label)
                .collect(),
        );
        for full in [false, true] {
            let selection = Selection::Id(&session.id);
            let view = if full {
                usage_full_with_backends(&backends, selection)
            } else {
                usage_with_backends(&backends, selection)
            };
            let view = serde_json::to_value(view.unwrap()).unwrap();
            check(if full { "full usage" } else { "usage" }, view_bases(&view));
        }
    }
    seen
}

#[test]
fn the_declarations_name_each_harness_bases() {
    let declared = |backend: &dyn Backend| backend.accounting_bases().to_vec();
    let root = Path::new("/nonexistent");
    assert_eq!(
        declared(&ClaudeBackend::new(root)),
        [
            AccountingBasis::SummedRequests,
            AccountingBasis::RecordedTotal
        ]
    );
    assert_eq!(
        declared(&CodexBackend::new(root)),
        [AccountingBasis::RecordedTotal]
    );
    assert_eq!(
        declared(&OpenCodeBackend::new(root)),
        [AccountingBasis::RecordedTotal]
    );
    assert_eq!(
        declared(&PiBackend::new(root)),
        [AccountingBasis::SummedRequests]
    );
}

#[test]
fn a_claude_reader_produces_only_its_declared_bases() {
    let store = Store::new("claude");
    let request = |id: &str| {
        json!({"type": "assistant", "sessionId": id, "requestId": format!("{id}-request"),
               "uuid": format!("{id}-assistant"),
               "message": {"role": "assistant", "model": "fixture-model", "content": [],
                           "usage": {"input_tokens": 10, "output_tokens": 4}}})
    };
    store.write("project/summed.jsonl", &[request("summed")]);
    store.write(
        "project/recorded.jsonl",
        &[
            request("recorded"),
            json!({"type": "cost-state", "sessionId": "recorded", "totalCostUSD": 0.5,
                   "modelUsage": {"model-a": {"inputTokens": 20, "outputTokens": 8, "costUSD": 0.5}}}),
        ],
    );
    // A cost-state below the request sum is set aside beside the summed
    // requests.
    store.write(
        "project/set-aside.jsonl",
        &[
            request("set-aside"),
            json!({"type": "cost-state", "sessionId": "set-aside", "totalCostUSD": 0.1,
                   "modelUsage": {"model-a": {"inputTokens": 1, "outputTokens": 1, "costUSD": 0.1}}}),
        ],
    );
    let seen = produced(Box::new(ClaudeBackend::new(&store.0)));
    assert_eq!(
        seen,
        BTreeSet::from(["summed-requests".to_owned(), "recorded-total".to_owned()])
    );
}

#[test]
fn a_codex_reader_produces_only_its_declared_bases() {
    let id = "70000000-0000-0000-0000-0000000000b1";
    let store = Store::new("codex");
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &[
            json!({"type": "session_meta", "payload": {"id": id, "timestamp": "2026-01-01T10:00:00Z"}}),
            json!({"type": "turn_context", "payload": {"model": "fixture-model"}}),
            json!({"type": "token_usage_record", "payload": {"response_id": "one",
                   "usage": {"input_tokens": 10, "output_tokens": 4}}}),
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                   "total_token_usage": {"input_tokens": 10, "output_tokens": 4}}}}),
        ],
    );
    let seen = produced(Box::new(CodexBackend::new(&store.0)));
    assert_eq!(seen, BTreeSet::from(["recorded-total".to_owned()]));
}

#[test]
fn a_pi_reader_produces_only_its_declared_bases() {
    let store = Store::new("pi");
    store.write(
        "2026-01-01T10-00-00-000Z_session.jsonl",
        &[
            json!({"type": "session", "version": 3, "id": "session", "timestamp": "2026-01-01T10:00:00Z"}),
            json!({"type": "message", "id": "root", "parentId": null,
                   "message": {"role": "assistant", "model": "fixture-model", "content": [],
                               "usage": {"input": 10, "output": 4, "cost": {"total": 0.1}}}}),
        ],
    );
    let seen = produced(Box::new(PiBackend::new(&store.0)));
    assert_eq!(seen, BTreeSet::from(["summed-requests".to_owned()]));
}

#[test]
fn an_opencode_reader_produces_only_its_declared_bases() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
    let root = std::env::temp_dir().join(format!("tapes-bases-opencode-{}", std::process::id()));
    let data = root.join("data/opencode");
    fs::create_dir_all(&data).unwrap();
    fs::File::create(data.join("opencode.db")).unwrap();
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    // The program answers from the database when invoked as `opencode`, and
    // from the API under the fixture's own name.
    fs::create_dir_all(root.join("bin")).unwrap();
    std::os::unix::fs::symlink(fixtures.join("opencode2"), root.join("bin/opencode")).unwrap();
    for program in [root.join("bin/opencode"), fixtures.join("opencode2")] {
        let seen = produced(Box::new(OpenCodeBackend::new(program)));
        assert_eq!(seen, BTreeSet::from(["recorded-total".to_owned()]));
    }
    let _ = fs::remove_dir_all(&root);
}
