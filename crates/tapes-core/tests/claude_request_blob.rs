//! Claude writes the messages of each API request it sends as
//! `api-request-blob` records, in a run just ahead of the `api-request`
//! record naming them: a `type`, the `sessionId` (and a subagent's
//! `agentId`), a content `hash`, and a `message` holding the request
//! message's `role` and `content`, with no timestamp and no uuid. The content
//! restates the system prompt and conversation records already written,
//! `Agent` calls included, so a blob is no turn of the conversation and no
//! statement of when anything happened.
//!
//! Every test reads the fixture recording as recorded and the same recording
//! with its blob records removed, and expects the same answer from both.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::{Backend, Query, StreamOptions};
use agent_tapes_core::lineage::{ChildRef, Lineage, SourceRef};
use agent_tapes_core::model::{Session, Turn};
use agent_tapes_core::{search, usage, ListFilters, ListSort, Selection};
use chrono::{DateTime, Utc};
use serde_json::Value;

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const SESSION: &str = "21000000-0000-4000-8000-000000000001";
const CHILD: &str = "a21000000000000b1";
/// Text only the fixture's blob records carry: the request's system prompt.
const BLOB_ONLY: &str = "blob-restated system text";
/// Text a conversation record carries.
const CONVERSATION: &str = "Scouted and listed.";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-request-blob")
}

/// A copy of the fixture store, removed when dropped.
struct Store(PathBuf);

impl Store {
    /// The fixture as Claude recorded it.
    fn recorded() -> Self {
        Self::copy("recorded", |_| true)
    }

    /// The fixture without its `api-request-blob` records.
    fn conversation() -> Self {
        Self::copy("conversation", |line| {
            serde_json::from_str::<Value>(line).unwrap()["type"] != "api-request-blob"
        })
    }

    fn copy(label: &str, keep: fn(&str) -> bool) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-claude-request-blob-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        copy_tree(&fixture(), &root, keep);
        Self(root)
    }

    fn backend(&self) -> ClaudeBackend {
        ClaudeBackend::new(&self.0)
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![Box::new(self.backend())]
    }

    fn session(&self) -> Session {
        self.backend().locate(SESSION).unwrap().expect("stored")
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_tree(from: &Path, to: &Path, keep: fn(&str) -> bool) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target, keep);
        } else if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            let body: String = fs::read_to_string(entry.path())
                .unwrap()
                .lines()
                .filter(|line| keep(line))
                .map(|line| format!("{line}\n"))
                .collect();
            fs::write(target, body).unwrap();
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn instant(text: &str) -> DateTime<Utc> {
    text.parse().unwrap()
}

/// What a reader states of a turn, without the byte coordinates that differ
/// between two files.
fn stated(turns: &[Turn]) -> Vec<String> {
    turns
        .iter()
        .map(|turn| {
            format!(
                "{:?} {:?} {:?} {:?} {:?}",
                turn.role, turn.kind, turn.ts, turn.native_id, turn.text
            )
        })
        .collect()
}

fn lineages(store: &Store) -> [Lineage; 2] {
    let backend = store.backend();
    let session = store.session();
    [
        backend.lineage(&session).unwrap(),
        backend.stream_lineage(&session).unwrap(),
    ]
}

fn child(lineage: &Lineage) -> &ChildRef {
    lineage
        .children
        .iter()
        .find(|child| child.reference == CHILD)
        .unwrap_or_else(|| panic!("no child {CHILD}: {:#?}", lineage.children))
}

fn cites(child: &ChildRef, native_id: &str) -> bool {
    child
        .source
        .iter()
        .any(|source| matches!(source, SourceRef::Record { native_id: id } if id == native_id))
}

/// Claim: `lineage` and `lineage --full` keep a background child's spawn time
/// and `Agent` call record when a later request blob repeats the call.
/// Rejects a blob, which carries no timestamp and no uuid, overwriting the
/// call's spawn time and record with nothing.
#[test]
fn lineage_keeps_a_spawn_a_request_blob_repeats() {
    let store = Store::recorded();
    for lineage in lineages(&store) {
        let child = child(&lineage);
        assert_eq!(
            child.spawned_at,
            Some(instant("2026-01-05T10:00:02.000Z")),
            "{child:#?}"
        );
        assert!(cites(child, "agent-call"), "{child:#?}");
        assert_eq!(
            child.completed_at,
            Some(instant("2026-01-05T10:01:00.000Z")),
            "{child:#?}"
        );
        assert!(cites(child, "notified"), "{child:#?}");
    }
    let stated = |store: &Store| {
        lineages(store).map(|lineage| {
            lineage
                .children
                .into_iter()
                .map(|child| {
                    let records = child
                        .source
                        .iter()
                        .filter(|source| matches!(source, SourceRef::Record { .. }))
                        .cloned()
                        .collect::<Vec<_>>();
                    format!(
                        "{} {:?} {:?} {:?} {:?} {:?} {records:?}",
                        child.reference,
                        child.role,
                        child.model,
                        child.spawned_at,
                        child.completed_at,
                        child.disposition
                    )
                })
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(stated(&store), stated(&Store::conversation()));
}

/// Claim: `show`, bounded and `--full`, reads the same turns from a
/// recording with request blobs as from its conversation records alone.
/// Rejects each blob becoming untimestamped turns that restate the system
/// prompt, earlier messages and tool calls.
#[test]
fn show_reads_no_turn_from_a_request_blob() {
    let recorded = Store::recorded();
    let plain = Store::conversation();
    let bounded = |store: &Store| {
        let transcript = store.backend().transcript(&store.session(), 1000).unwrap();
        stated(&transcript.turns)
    };
    let full = |store: &Store| {
        let mut turns = Vec::new();
        store
            .backend()
            .stream_transcript(&store.session(), None, &mut |turn| {
                turns.push(turn);
                Ok(())
            })
            .unwrap();
        stated(&turns)
    };
    assert_eq!(bounded(&recorded), bounded(&plain));
    assert_eq!(full(&recorded), full(&plain));
    assert!(
        bounded(&recorded)
            .iter()
            .all(|turn| !turn.contains(BLOB_ONLY)),
        "{:#?}",
        bounded(&recorded)
    );
}

/// Claim: a history page reads no turn from a request blob. Rejects a page
/// whose turns restate the request's messages.
#[test]
fn a_history_page_reads_no_turn_from_a_request_blob() {
    let page = |store: &Store| {
        let page = store
            .backend()
            .history_page(&store.session(), None, 1 << 20)
            .unwrap();
        stated(&page.turns)
    };
    assert_eq!(page(&Store::recorded()), page(&Store::conversation()));
}

/// Claim: a Claude child's transcript reads no turn from the request blobs
/// in its own recording. Rejects the child's system prompt and brief read
/// again as turns.
#[test]
fn a_child_transcript_reads_no_turn_from_a_request_blob() {
    let child = |store: &Store| {
        let transcript = store
            .backend()
            .child_transcript(&store.session(), CHILD)
            .unwrap();
        stated(&transcript.turns)
    };
    assert_eq!(child(&Store::recorded()), child(&Store::conversation()));
}

/// Claim: `list --search`, bounded and whole-recording, matches no text that
/// only a request blob carries, and still matches the conversation. Rejects
/// a session found by its system prompt.
#[test]
fn search_does_not_match_text_only_a_request_blob_carries() {
    let store = Store::recorded();
    let bounded = |needle: &str| {
        store
            .backend()
            .list_with_search(&Query::unscoped(10), &needle.to_lowercase(), usize::MAX)
            .unwrap()
            .sessions
            .len()
    };
    assert_eq!(bounded(CONVERSATION), 1);
    assert_eq!(bounded(BLOB_ONLY), 0);
    let whole = |needle: &str| {
        let filters = ListFilters {
            search: Some(needle),
            ..Default::default()
        };
        let options = search::Options {
            limit: 10,
            sort: ListSort::Newest,
            match_limit: 10,
        };
        search::list(&store.backends(), None, None, &filters, options)
            .unwrap()
            .sessions
            .len()
    };
    assert_eq!(whole(CONVERSATION), 1);
    assert_eq!(whole(BLOB_ONLY), 0);
}

/// Claim: an exported bundle holds no request blob's text. Rejects a
/// `.context.md` or `.trace.md` that restates the system prompt.
#[test]
fn an_export_bundle_holds_no_request_blob() {
    let store = Store::recorded();
    let transcript = store.backend().transcript(&store.session(), 1000).unwrap();
    let directory = store.0.join("bundle");
    agent_tapes_core::bundle::export(&transcript, &directory).unwrap();
    let mut written = String::new();
    for entry in fs::read_dir(&directory).unwrap() {
        written.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    assert!(written.contains(CONVERSATION));
    assert!(!written.contains(BLOB_ONLY));
}

/// Claim: `stats` counts the same turns and tool calls with request blobs as
/// without. Rejects each tool call a blob repeats counted again, unpaired.
#[test]
fn stats_count_no_request_blob() {
    let counted = |store: &Store| {
        let stats =
            agent_tapes_core::stats_full_with_backends(&store.backends(), Selection::Id(SESSION))
                .unwrap();
        let mut stats = serde_json::to_value(stats).unwrap();
        (stats["turns"].take(), stats["tools"].take())
    };
    let recorded = counted(&Store::recorded());
    assert_eq!(recorded, counted(&Store::conversation()));
    assert!(!recorded.1.is_null(), "the fixture counts tools");
}

/// Claim: the recorded model observation is the same with request blobs as
/// without, bounded and whole. Rejects an assistant blob, which names no
/// model, read as an unobserved model and so as a break in the selection.
#[test]
fn model_observation_ignores_request_blobs() {
    let observed = |store: &Store| {
        let bounded = store
            .backend()
            .transcript(&store.session(), 1000)
            .unwrap()
            .session;
        let full =
            agent_tapes_core::usage_full_with_backends(&store.backends(), Selection::Id(SESSION))
                .unwrap();
        let full = serde_json::to_value(full).unwrap();
        (
            serde_json::to_value(bounded.model_observation).unwrap(),
            serde_json::to_value(bounded.model_selections).unwrap(),
            full["session"]["model_observation"].clone(),
            full["session"]["model_selections"].clone(),
        )
    };
    let recorded = observed(&Store::recorded());
    assert_eq!(recorded, observed(&Store::conversation()));
    assert_eq!(recorded.0["attribution_uncertain"], false, "{recorded:#?}");
}

/// Claim: a read counts a request blob among the records it declined, since
/// its text restates other records, not among those it did not recognize.
/// Rejects a blob counted nowhere, or as text no view holds.
#[test]
fn a_request_blob_is_a_declined_record() {
    let store = Store::recorded();
    let bounded = store.backend().transcript(&store.session(), 1000).unwrap();
    let full = store
        .backend()
        .stream_transcript(&store.session(), None, &mut |_| Ok(()))
        .unwrap();
    for unmapped in [bounded.read.unwrap().unmapped, full.unmapped] {
        let unmapped = unmapped.expect("the read counts unmapped records");
        assert_eq!(
            unmapped.declined.get("api-request-blob"),
            Some(&9),
            "{unmapped:#?}"
        );
        assert!(
            !unmapped.unrecognized.contains_key("api-request-blob"),
            "{unmapped:#?}"
        );
    }
}

/// Claim: usage, the request timeline and working stretches are the same
/// with request blobs as without: a blob carries no usage, no request id and
/// no timestamp. Rejects a blob counted as a request or as a stamp.
#[test]
fn usage_and_timeline_count_no_request_blob() {
    let measured = |store: &Store| {
        let usage =
            agent_tapes_core::usage_full_with_backends(&store.backends(), Selection::Id(SESSION))
                .unwrap();
        let mut usage = serde_json::to_value(usage).unwrap();
        let read = store
            .backend()
            .stream_transcript_with(
                &store.session(),
                None,
                StreamOptions { timeline: true },
                &mut |_| Ok(()),
            )
            .unwrap();
        (
            usage["session"]["tokens"].take(),
            usage["session"]["accounting"].take(),
            usage["session"]["usage_detail"].take(),
            usage::timeline(&read).cloned(),
            usage::stretches(&read, 0),
        )
    };
    let recorded = measured(&Store::recorded());
    assert_eq!(recorded, measured(&Store::conversation()));
    assert_eq!(
        recorded.3.as_ref().map(|timeline| timeline.requests.len()),
        Some(3)
    );
}
