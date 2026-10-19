//! The OpenCode API fixture answers as the real endpoint does: messages in
//! the order a request asks for, and `tokens` and `cost` on each completed
//! assistant message. A whole read, which pages oldest first, sees the
//! session in recording order and counts each message's usage.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use agent_tapes_core::backend::{opencode::OpenCodeBackend, Backend};
use agent_tapes_core::model::Tokens;
use serde_json::Value;

const SESSION: &str = "ses_000000fixtureSharedSession";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode")
}

/// The fixture program, answering through its API with no database present.
fn backend() -> OpenCodeBackend {
    static XDG: OnceLock<PathBuf> = OnceLock::new();
    let root = XDG.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("tapes-opencode-shape-{}", std::process::id()));
        let opencode = root.join("opencode");
        fs::create_dir_all(&opencode).unwrap();
        File::create(opencode.join("opencode.db")).unwrap();
        root
    });
    std::env::set_var("XDG_DATA_HOME", root);
    OpenCodeBackend::new(fixtures().join("opencode2"))
}

/// Claim: a whole read hands over the session's turns in the order a bounded
/// read gives them, the operator's prompt first. Rejects a whole read fed
/// newest-first pages, which streams the session backwards.
#[test]
fn a_whole_read_streams_the_session_in_recording_order() {
    let backend = backend();
    let session = backend.locate(SESSION).unwrap().unwrap();
    let bounded = backend
        .transcript(&session, usize::MAX)
        .unwrap()
        .turns
        .into_iter()
        .map(|turn| (turn.kind, turn.text))
        .collect::<Vec<_>>();
    let mut whole = Vec::new();
    backend
        .stream_transcript(&session, None, &mut |turn| {
            whole.push((turn.kind, turn.text));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        whole.first().map(|(_, text)| text.as_str()),
        Some("Inspect the fixture.")
    );
    assert_eq!(whole, bounded);
}

/// Claim: a whole span read counts the usage each completed assistant
/// message records, 1,200 and 300 input, 80 and 40 output, 30 reasoning,
/// 4,000 and 5,000 cache reads and 600 cache writes, and nothing for the
/// pending one. Rejects a fixture whose messages carry no counters, under
/// which the whole read counts nothing.
#[test]
fn a_whole_read_counts_each_messages_usage() {
    let backend = backend();
    let session = backend.locate(SESSION).unwrap().unwrap();
    let read = backend
        .stream_transcript(&session, None, &mut |_| Ok(()))
        .unwrap();
    let spans = backend.stream_spans(&session, &read, &[]).unwrap();
    assert_eq!(
        spans.whole.tokens,
        Some(Tokens {
            input: Some(1500),
            output: Some(120),
            reasoning: Some(30),
            cache_read: Some(9000),
            cache_write: Some(600),
        })
    );
}

/// Claim: the fixture's newest-first copy, which answers `order=desc`, holds
/// exactly the messages of the oldest-first file in reverse. Rejects the two
/// answers drifting apart.
#[test]
fn the_newest_first_answer_is_the_same_messages_reversed() {
    let read = |name: &str| -> Value {
        serde_json::from_str(&fs::read_to_string(fixtures().join(name)).unwrap()).unwrap()
    };
    let oldest_first = read("messages.json");
    let newest_first = read("messages-newest-first.json");
    let mut reversed = oldest_first["data"].as_array().unwrap().clone();
    reversed.reverse();
    assert_eq!(newest_first["data"], Value::Array(reversed));
    let created = oldest_first["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["time"]["created"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(created.windows(2).all(|pair| pair[0] <= pair[1]));
}
