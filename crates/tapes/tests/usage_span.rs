//! `tapes usage <session> --from/--to` and `--requests` answer one span of
//! one session with the whole-session shape and a `span` object, the halves
//! of a split add up to the `--full` answer, and span flags refuse what they
//! cannot answer.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const SESSION: &str = "30000000-0000-4000-8000-000000000001";

struct Store(PathBuf);

impl Store {
    /// A Claude store root holding the resumed-session fixture.
    fn claude() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("tapes-cli-span-{}-{nonce}", std::process::id()));
        let project = root.join("projects/-fixtures-resumed-project");
        fs::create_dir_all(&project).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tapes-core/tests/fixtures/claude-resumed/project")
            .join(format!("{SESSION}.jsonl"));
        fs::copy(fixture, project.join(format!("{SESSION}.jsonl"))).unwrap();
        Self(root)
    }

    fn usage(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env_remove("PI_SESSION_FILE")
            .arg("usage")
            .args(arguments)
            .args(["--native-store", "claude", "--native-root"])
            .arg(&self.0)
            .output()
            .unwrap()
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self.usage(&[arguments, &["--json"]].concat());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claim: a time span answers the whole-session shape under the usage
/// schema, with a `span` naming its bounds and boundary requests, and the
/// span before an instant plus the span from it add up to the `--full`
/// answer. Rejects a span flag that answers the whole session, or halves
/// that do not add up to it.
#[test]
fn a_time_split_adds_up_to_the_full_answer() {
    let store = Store::claude();
    let whole = store.json(&[SESSION, "--full"]);
    let before = store.json(&[SESSION, "--to", "2026-01-05T13:00:00Z"]);
    let after = store.json(&[SESSION, "--from", "2026-01-05T13:00:00Z"]);
    assert_eq!(before["schema"], "tapes-usage/12");
    assert_eq!(before["span"]["bounds"]["kind"], "time");
    assert_eq!(before["span"]["bounds"]["to"], "2026-01-05T13:00:00Z");
    assert_eq!(before["span"]["requests"], 4);
    assert_eq!(before["span"]["session_requests"], 5);
    assert_eq!(before["span"]["last"]["id"], "req_fixture_4");
    assert_eq!(after["span"]["first"]["ordinal"], 5);
    assert_eq!(after["span"]["first"]["timestamp"], "2026-01-05T14:00:01Z");
    assert!(whole.get("span").is_none());
    for counter in ["input", "output", "reasoning", "cache_read", "cache_write"] {
        assert_eq!(
            before["tokens"][counter].as_u64().unwrap()
                + after["tokens"][counter].as_u64().unwrap(),
            whole["tokens"][counter].as_u64().unwrap(),
            "{counter}"
        );
    }
    for category in ["uncached_input", "cache_read", "output"] {
        assert_eq!(
            before["disjoint"][category].as_u64().unwrap()
                + after["disjoint"][category].as_u64().unwrap(),
            whole["disjoint"][category].as_u64().unwrap(),
            "{category}"
        );
    }
}

/// Claim: a request-ordinal span selects counted requests 1-based and
/// inclusive, either end open, and the text answer names the span and its
/// boundary requests. Rejects an off-by-one range.
#[test]
fn a_request_range_selects_counted_requests_inclusively() {
    let store = Store::claude();
    let middle = store.json(&[SESSION, "--requests", "3..4"]);
    assert_eq!(middle["span"]["bounds"]["kind"], "requests");
    assert_eq!(middle["span"]["requests"], 2);
    assert_eq!(middle["span"]["first"]["id"], "req_fixture_3");
    assert_eq!(middle["span"]["last"]["id"], "req_fixture_4");
    let open = store.json(&[SESSION, "--requests", "5.."]);
    assert_eq!(open["span"]["requests"], 1);
    let output = store.usage(&[SESSION, "--requests", "..2"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("span: requests ..2: 2 of 5 requests"),
        "{text}"
    );
    assert!(
        text.contains("span last: request 2 at 2026-01-05T10:00:04+00:00 (req_fixture_2)"),
        "{text}"
    );
    assert!(text.contains(", within the span"), "{text}");
}

/// Claim: span flags select within one named session: they refuse a
/// selection, refuse each other, and refuse an ordinal of zero or a range
/// that ends before it starts. Rejects a span flag silently applied to a
/// summary or a malformed range read as the whole session.
#[test]
fn span_flags_refuse_what_they_cannot_answer() {
    let store = Store::claude();
    let refused = |arguments: &[&str], expected: &str| {
        let output = store.usage(arguments);
        assert!(!output.status.success(), "{arguments:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{arguments:?}: {stderr}");
    };
    refused(
        &["--global", "--from", "2026-01-05T13:00:00Z"],
        "answer a span of one session",
    );
    refused(
        &[
            SESSION,
            "--from",
            "2026-01-05T13:00:00Z",
            "--requests",
            "1..2",
        ],
        "cannot be used with",
    );
    refused(&[SESSION, "--requests", "0..2"], "ordinals count from 1");
    refused(&[SESSION, "--requests", "4..2"], "ends before it starts");
    refused(
        &[SESSION, "--from", "2026-01-05"],
        "not an RFC 3339 timestamp",
    );
}
