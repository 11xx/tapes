//! A database record that cannot be read is an unreadable record: a message
//! row whose data does not decode, or a part row whose data does not decode or
//! that names no message. The read keeps every other row's turns and figures,
//! marks a gap, and qualifies the session's counters with `unreadable-record`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{opencode::OpenCodeBackend, Backend};
use agent_tapes_core::model::Turn;
use agent_tapes_core::{
    show_full_with_backends, show_with_backends, usage_full_with_backends, usage_with_backends,
    Selection, TurnSink,
};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const SESSION: &str = "ses_database_only_fixture";
const NATIVE: [&str; 6] = [
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];

/// A copy of the OpenCode fixtures whose database message rows are extended
/// by `rows` and part rows by `parts`, served by a program the copy resolves
/// its responses from.
struct Fixtures(PathBuf);

impl Fixtures {
    fn new(rows: &[Value]) -> Self {
        Self::with_parts(rows, &[])
    }

    fn with_parts(rows: &[Value], parts: &[Value]) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-opencode-unreadable-message-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
        fs::create_dir_all(root.join("bin")).unwrap();
        for entry in fs::read_dir(&source).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                fs::copy(entry.path(), root.join(entry.file_name())).unwrap();
            }
        }
        for (name, rows) in [
            ("database-messages-database-only.json", rows),
            ("database-parts-database-only.json", parts),
        ] {
            let path = root.join(name);
            let mut body = fs::read_to_string(&path).unwrap();
            for row in rows {
                body.push_str(&row.to_string());
                body.push('\n');
            }
            fs::write(&path, body).unwrap();
        }
        std::os::unix::fs::symlink(root.join("opencode2"), root.join("bin/opencode")).unwrap();
        let data = root.join("data/opencode");
        fs::create_dir_all(&data).unwrap();
        fs::File::create(data.join("opencode.db")).unwrap();
        Self(root)
    }

    fn backend(&self) -> Box<dyn Backend> {
        std::env::set_var("XDG_DATA_HOME", self.0.join("data"));
        Box::new(OpenCodeBackend::new(self.0.join("bin/opencode")))
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A message row whose data is not JSON.
fn unreadable_row() -> Value {
    json!({
        "id": "msg_unreadable",
        "time_created": 1784663105000_u64,
        "data": "{not json",
    })
}

fn view(fixtures: &Fixtures, full: bool) -> Value {
    let backends = vec![fixtures.backend()];
    let view = if full {
        usage_full_with_backends(&backends, Selection::Id(SESSION)).unwrap()
    } else {
        usage_with_backends(&backends, Selection::Id(SESSION)).unwrap()
    };
    serde_json::to_value(view).unwrap()
}

fn reasons<'a>(figure: &'a Value, counter: &str) -> Vec<&'a str> {
    figure["measurement_completeness"][counter]
        .as_array()
        .map(|reasons| reasons.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[derive(Default)]
struct Turns(Vec<Turn>);

impl TurnSink for Turns {
    fn session(&mut self, _: &agent_tapes_core::model::Session) -> anyhow::Result<()> {
        Ok(())
    }

    fn turn(&mut self, turn: Turn) -> anyhow::Result<()> {
        self.0.push(turn);
        Ok(())
    }
}

fn texts(turns: &[Turn]) -> Vec<String> {
    turns
        .iter()
        .map(|turn| format!("{:?}", turn.text))
        .collect()
}

#[test]
fn one_unreadable_message_row_leaves_every_other_turn_readable() {
    let healthy = Fixtures::new(&[]);
    let broken = Fixtures::new(&[unreadable_row()]);

    let whole = show_with_backends(&[healthy.backend()], Selection::Id(SESSION), 100).unwrap();
    let kept = show_with_backends(&[broken.backend()], Selection::Id(SESSION), 100).unwrap();
    assert!(!whole.turns.is_empty());
    assert_eq!(texts(&kept.turns), texts(&whole.turns));
    assert!(
        kept.notes
            .iter()
            .any(|note| note.contains("1 unreadable record")),
        "{:?}",
        kept.notes
    );

    let mut expected = Turns::default();
    show_full_with_backends(&[healthy.backend()], Selection::Id(SESSION), &mut expected).unwrap();
    let mut streamed = Turns::default();
    let read = show_full_with_backends(&[broken.backend()], Selection::Id(SESSION), &mut streamed)
        .unwrap();
    assert_eq!(texts(&streamed.0), texts(&expected.0));
    assert_eq!(read.skipped, 1);
    assert_eq!(read.gaps.len(), 1);
    assert_eq!(read.gaps[0].reason, "malformed-record");
}

#[test]
fn an_unreadable_message_row_qualifies_every_counter_of_the_figure() {
    for full in [false, true] {
        let label = format!("opencode full={full}");
        let healthy = view(&Fixtures::new(&[]), full);
        let broken = view(&Fixtures::new(&[unreadable_row()]), full);
        assert_eq!(broken["tokens"], healthy["tokens"], "{label}: {broken}");
        for counter in NATIVE {
            assert!(
                !reasons(&healthy, counter).contains(&"unreadable-record"),
                "{label} × {counter}: {healthy}"
            );
            assert!(
                reasons(&broken, counter).contains(&"unreadable-record"),
                "{label} × {counter}: {broken}"
            );
        }
    }
}

/// A part row of the healthy assistant message whose data is not JSON.
fn unreadable_part() -> Value {
    json!({
        "message_id": "msg_database_assistant",
        "time_created": 1784663109000_u64,
        "data": "{not json",
    })
}

/// A part row that names no message.
fn orphan_part() -> Value {
    json!({
        "time_created": 1784663109000_u64,
        "data": {"type": "text", "text": "Orphan.", "truncated": 0},
    })
}

fn assert_part_gap_is_local(part: Value) {
    let healthy = Fixtures::new(&[]);
    let broken = Fixtures::with_parts(&[], &[part]);

    let whole = show_with_backends(&[healthy.backend()], Selection::Id(SESSION), 100).unwrap();
    let kept = show_with_backends(&[broken.backend()], Selection::Id(SESSION), 100).unwrap();
    assert!(!whole.turns.is_empty());
    assert_eq!(texts(&kept.turns), texts(&whole.turns));
    assert!(
        kept.notes
            .iter()
            .any(|note| note.contains("1 unreadable record")),
        "{:?}",
        kept.notes
    );

    let mut expected = Turns::default();
    show_full_with_backends(&[healthy.backend()], Selection::Id(SESSION), &mut expected).unwrap();
    let mut streamed = Turns::default();
    let read = show_full_with_backends(&[broken.backend()], Selection::Id(SESSION), &mut streamed)
        .unwrap();
    assert_eq!(texts(&streamed.0), texts(&expected.0));
    assert_eq!(read.skipped, 1);
    assert_eq!(read.gaps.len(), 1);
    assert_eq!(read.gaps[0].reason, "malformed-record");

    for full in [false, true] {
        let label = format!("opencode full={full}");
        let healthy = view(&healthy, full);
        let broken = view(&broken, full);
        assert_eq!(broken["tokens"], healthy["tokens"], "{label}: {broken}");
        for counter in NATIVE {
            assert!(
                reasons(&broken, counter).contains(&"unreadable-record"),
                "{label} × {counter}: {broken}"
            );
        }
    }
}

#[test]
fn a_part_row_whose_data_does_not_decode_leaves_every_other_part_readable() {
    assert_part_gap_is_local(unreadable_part());
}

#[test]
fn a_part_row_without_a_message_leaves_every_message_readable() {
    assert_part_gap_is_local(orphan_part());
}
