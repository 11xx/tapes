//! A read context pays for each recording once: lineage reads over many Codex
//! sessions join against one child index per store walk, and a pi usage
//! read folds its accounting into the transcript pass. How often a lineage
//! read opens each rollout is counted in `review2_fixes_reader_passes.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{codex::CodexBackend, pi::PiBackend, Backend};
use agent_tapes_core::counters;
use agent_tapes_core::usage::UsageOptions;
use agent_tapes_core::{usage_with_options_with_backends, Selection};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

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
            "tapes-passes-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
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
/// finds each parent's children, bounded and streamed, and finds a child
/// written later once a named lookup has walked the store again. Rejects a
/// child index that outlives the walk it was built from.
#[test]
fn codex_lineage_follows_the_walk() {
    // Its streamed read moves the pass counter the pi test reads.
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
    let backend = CodexBackend::new(&store.0);

    assert_eq!(
        children(&backend, PARENT),
        vec![FIRST_CHILD.to_owned(), SECOND_CHILD.to_owned()]
    );
    for id in [FIRST_CHILD, SECOND_CHILD].into_iter().chain(OTHERS) {
        assert!(children(&backend, id).is_empty(), "{id}");
    }
    let parent = backend.locate(PARENT).unwrap().unwrap();
    assert_eq!(backend.stream_lineage(&parent).unwrap().children.len(), 2);

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

/// Claim: a whole pi usage read streams its recording twice, the branch pass
/// and the turn pass, and the session it folds from the turn pass is the one
/// a separate accounting pass over the same read produces. Rejects a read
/// that streams the recording a third time for its accounting.
///
/// Two passes are the fewest a bounded read can make. pi's active branch is
/// the ancestry of the recording's last entry, so whether an entry is on it
/// is decided by entries written after it: in `session-pi`, `abandoned-1`
/// and `tool-result-1` are both children of `assistant-1`, and only the last
/// entry's ancestry, through `tool-result-1`, leaves `abandoned-1` off the
/// branch. The read hands over the active branch's turns in recording order,
/// so one pass would have to hold every turn until the last entry, the whole
/// transcript in memory. The first pass keeps each entry's id and parent; the
/// second emits the branch's turns and folds the accounting.
#[test]
fn pi_whole_usage_folds_accounting_into_the_turn_pass() {
    let _counted = COUNTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi");
    for id in ["session-pi", "session-model-split"] {
        let before = counters::recording_passes();
        usage_with_options_with_backends(
            &[Box::new(PiBackend::new(&fixtures)) as Box<dyn Backend>],
            Selection::Id(id),
            UsageOptions {
                full: true,
                series: None,
                reconcile: false,
                event_keys: None,
            },
        )
        .unwrap();
        assert_eq!(
            counters::recording_passes() - before,
            2,
            "pi usage of {id} streams for its branch, then turns and accounting"
        );

        let backend = PiBackend::new(&fixtures);
        let session = backend.locate(id).unwrap().unwrap();
        let read = backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap();
        let folded = backend.stream_session(&session, &read).unwrap();
        let mut asked_elsewhere = session.clone();
        asked_elsewhere.title = Some("asked about another session".to_owned());
        let mut replayed = backend.stream_session(&asked_elsewhere, &read).unwrap();
        replayed.title.clone_from(&session.title);
        assert_eq!(folded, replayed, "{id}");
    }
}
