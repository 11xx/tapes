//! A read context pays for each recording once: lineage reads over many Codex
//! sessions open each candidate's header once per store walk, and a pi usage
//! read folds its accounting into the transcript pass.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{codex::CodexBackend, Backend};
use agent_tapes_core::counters;
use serde_json::{json, Value};

/// The counters are process-wide, so the tests that read them run one at a
/// time.
static COUNTED: Mutex<()> = Mutex::new(());

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-passes-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write_codex(&self, hour: u32, id: &str, header: Value) {
        let path = self.0.join(format!(
            "2026/01/09/rollout-2026-01-09T{hour:02}-00-00-{id}.jsonl"
        ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut payload = json!({"id": id, "session_id": id,
            "timestamp": format!("2026-01-09T{hour:02}:00:00Z"), "cwd": "/fixtures/project"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(header.as_object().unwrap().clone());
        let lines = [
            json!({"timestamp": format!("2026-01-09T{hour:02}:00:00Z"),
                   "type": "session_meta", "payload": payload}),
            json!({"timestamp": format!("2026-01-09T{hour:02}:00:01Z"), "type": "response_item",
                   "payload": {"type": "message", "role": "user",
                               "content": [{"type": "input_text", "text": "Inspect the fixture."}]}}),
        ];
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const PARENT: &str = "01a00000-0000-7000-8000-000000000001";
const FIRST_CHILD: &str = "01a00000-0000-7000-8000-000000000002";
const SECOND_CHILD: &str = "01a00000-0000-7000-8000-000000000003";
const OTHERS: [&str; 3] = [
    "01a00000-0000-7000-8000-000000000004",
    "01a00000-0000-7000-8000-000000000005",
    "01a00000-0000-7000-8000-000000000006",
];
const LATE_CHILD: &str = "01a00000-0000-7000-8000-000000000007";

fn children(backend: &CodexBackend, id: &str) -> Vec<String> {
    let session = backend.locate(id).unwrap().unwrap();
    let mut found: Vec<String> = backend
        .lineage(&session)
        .unwrap()
        .children
        .into_iter()
        .filter_map(|child| child.session_id)
        .collect();
    found.sort();
    found
}

/// Claim: one backend reading the lineage of every session in a Codex store
/// opens each candidate rollout's header at most once, still finds each
/// parent's children, and finds a child written later once a named lookup
/// has walked the store again. Rejects a lineage read that opens every
/// rollout header in the store for each session it is asked about.
#[test]
fn codex_lineage_reads_each_candidate_header_once_per_walk() {
    let _counted = COUNTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let store = Store::new("codex-lineage");
    store.write_codex(10, PARENT, json!({}));
    for (hour, child, agent) in [
        (11, FIRST_CHILD, "/root/first"),
        (12, SECOND_CHILD, "/root/second"),
    ] {
        store.write_codex(
            hour,
            child,
            json!({"parent_thread_id": PARENT, "agent_path": agent, "agent_nickname": "Worker"}),
        );
    }
    for (hour, other) in (13..).zip(OTHERS) {
        store.write_codex(hour, other, json!({}));
    }
    let rollouts = 3 + OTHERS.len();
    let backend = CodexBackend::new(&store.0);

    let before = counters::lineage_header_reads();
    assert_eq!(
        children(&backend, PARENT),
        vec![FIRST_CHILD.to_owned(), SECOND_CHILD.to_owned()]
    );
    for id in [FIRST_CHILD, SECOND_CHILD].into_iter().chain(OTHERS) {
        assert!(children(&backend, id).is_empty(), "{id}");
    }
    let parent = backend.locate(PARENT).unwrap().unwrap();
    assert_eq!(backend.stream_lineage(&parent).unwrap().children.len(), 2);
    let opened = counters::lineage_header_reads() - before;
    assert!(
        opened <= rollouts,
        "{opened} header reads for {rollouts} rollouts read by {} lineage reads",
        rollouts + 1
    );

    store.write_codex(
        16,
        LATE_CHILD,
        json!({"parent_thread_id": PARENT, "agent_path": "/root/late"}),
    );
    assert!(backend.locate(LATE_CHILD).unwrap().is_some());
    assert_eq!(
        children(&backend, PARENT),
        vec![
            FIRST_CHILD.to_owned(),
            SECOND_CHILD.to_owned(),
            LATE_CHILD.to_owned()
        ]
    );
}
