//! A listing normalizes a recording under the identity its header states. A
//! recording larger than the read bound is read as an opening and a retained
//! tail, and a tail that names another session is rejected the way a direct
//! transcript read rejects it.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::pi::PiBackend;
use agent_tapes_core::backend::{Backend, Listing, Query, MIN_READ_BYTES};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const SESSION: &str = "70000000-0000-0000-0000-0000000000c1";
const OTHER: &str = "70000000-0000-0000-0000-0000000000c2";

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tapes-listing-identity-{label}-{}-{}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, records: &[Value]) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Records that push a recording past the read bound, so that its opening and
/// its tail are separate reads.
fn filler(record: impl Fn(usize) -> Value) -> Vec<Value> {
    let sample = record(0).to_string().len() + 1;
    (0..(3 * MIN_READ_BYTES as usize / sample))
        .map(record)
        .collect()
}

fn codex_recording(store: &Store, tail_identity: &str) {
    let message = |index: usize| {
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": format!("{index} {}", "x".repeat(400))}]}})
    };
    let mut records = vec![
        json!({"type": "session_meta", "payload": {"id": SESSION, "timestamp": "2026-01-01T10:00:00Z", "cwd": "/fixtures/project"}}),
        json!({"type": "turn_context", "payload": {"model": "fixture-model"}}),
    ];
    records.extend(filler(message));
    records.extend([
        json!({"type": "session_meta", "payload": {"id": tail_identity, "timestamp": "2026-01-01T11:00:00Z"}}),
        json!({"timestamp": "2026-01-01T11:00:01Z", "type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "needle from the tail"}]}}),
    ]);
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{SESSION}.jsonl"),
        &records,
    );
}

fn pi_recording(store: &Store, tail_identity: &str) {
    let message = |index: usize| {
        json!({"type": "message", "id": format!("m{index}"), "parentId": null,
            "timestamp": "2026-01-01T10:00:01Z",
            "message": {"role": "assistant", "content": [{"type": "text", "text": format!("{index} {}", "x".repeat(400))}]}})
    };
    let mut records = vec![
        json!({"type": "session", "version": 3, "id": SESSION, "timestamp": "2026-01-01T10:00:00Z", "cwd": "/fixtures/project"}),
    ];
    records.extend(filler(message));
    records.extend([
        json!({"type": "session", "version": 3, "id": tail_identity, "timestamp": "2026-01-01T11:00:00Z"}),
        json!({"type": "message", "id": "tail", "parentId": null, "timestamp": "2026-01-01T11:00:01Z",
            "message": {"role": "user", "content": [{"type": "text", "text": "needle from the tail"}]}}),
    ]);
    store.write(
        &format!("2026-01-01T10-00-00-000Z_{SESSION}.jsonl"),
        &records,
    );
}

fn listed(backend: &dyn Backend) -> Vec<Listing> {
    let query = Query::unscoped(10);
    vec![
        backend.list(&query).unwrap(),
        backend.list_with_search(&query, "needle", 10).unwrap(),
    ]
}

fn assert_listed(listings: &[Listing]) {
    for listing in listings {
        assert_eq!(listing.sessions.len(), 1, "{:?}", listing.unavailable);
        assert!(listing.unavailable.is_empty(), "{:?}", listing.unavailable);
    }
}

fn assert_rejected(listings: &[Listing]) {
    for listing in listings {
        assert!(listing.sessions.is_empty(), "{:?}", listing.sessions);
        assert_eq!(
            listing.unavailable_ids,
            [SESSION],
            "{:?}",
            listing.unavailable
        );
        assert!(
            listing.unavailable[0].contains("disagrees")
                || listing.unavailable[0].contains("different or invalid"),
            "{:?}",
            listing.unavailable
        );
    }
}

#[test]
fn codex_listing_accepts_a_tail_that_names_the_openings_identity() {
    let store = Store::new("codex-match");
    codex_recording(&store, SESSION);
    let backend = CodexBackend::new(&store.0).with_read_bytes(MIN_READ_BYTES);
    assert_listed(&listed(&backend));
}

#[test]
fn codex_listing_rejects_a_tail_that_names_another_identity() {
    let store = Store::new("codex-conflict");
    codex_recording(&store, OTHER);
    let backend = CodexBackend::new(&store.0).with_read_bytes(MIN_READ_BYTES);
    assert_rejected(&listed(&backend));
}

#[test]
fn pi_listing_accepts_a_tail_that_names_the_openings_identity() {
    let store = Store::new("pi-match");
    pi_recording(&store, SESSION);
    let backend = PiBackend::new(&store.0).with_read_bytes(MIN_READ_BYTES);
    assert_listed(&listed(&backend));
}

#[test]
fn pi_listing_rejects_a_tail_that_names_another_identity() {
    let store = Store::new("pi-conflict");
    pi_recording(&store, OTHER);
    let backend = PiBackend::new(&store.0).with_read_bytes(MIN_READ_BYTES);
    assert_rejected(&listed(&backend));
}
