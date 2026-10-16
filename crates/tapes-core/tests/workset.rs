//! A workset's lanes partition every source's turns exactly, whichever
//! harness recorded the source, and are cut at the projected-byte bound.

use std::path::{Path, PathBuf};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::opencode::OpenCodeBackend;
use agent_tapes_core::backend::pi::PiBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::model::{Session, Turn};
use agent_tapes_core::projection::project_turn;
use agent_tapes_core::workset::{
    build_with_backends, Lane, LaneCutter, SourceRole, Workset, MIN_LANE_BYTES,
};
use agent_tapes_core::{show_full_with_backends, Selection, TurnSink};

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(harness)
}

fn opencode_backend() -> Box<dyn Backend> {
    let root = std::env::temp_dir().join(format!("tapes-workset-xdg-{}", std::process::id()));
    std::fs::create_dir_all(root.join("opencode")).unwrap();
    std::fs::File::create(root.join("opencode/opencode.db")).unwrap();
    std::env::set_var("XDG_DATA_HOME", &root);
    Box::new(OpenCodeBackend::new(fixtures("opencode").join("opencode2")))
}

/// Every harness's fixture session, and each malformed-line fixture.
fn cases() -> Vec<(Box<dyn Backend>, &'static str)> {
    vec![
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))),
            "session-claude",
        ),
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))),
            "malformed",
        ),
        (
            Box::new(CodexBackend::new(fixtures("codex"))),
            "00000000-0000-0000-0000-000000000001",
        ),
        (Box::new(PiBackend::new(fixtures("pi"))), "session-pi"),
        (Box::new(PiBackend::new(fixtures("pi"))), "malformed"),
        (
            Box::new(PiBackend::new(fixtures("pi-relative-parent"))),
            "child",
        ),
        (opencode_backend(), "ses_000000fixtureSharedSession"),
    ]
}

fn assert_partition(lanes: &[Lane], turns: usize, context: &str) {
    let mut next = 0;
    for lane in lanes {
        assert_eq!(
            lane.first_turn, next,
            "{context}: gap or overlap at {lane:?}"
        );
        assert!(lane.last_turn >= lane.first_turn, "{context}: {lane:?}");
        let counted: usize = lane.turns_by_kind.values().sum();
        assert_eq!(
            counted,
            lane.last_turn - lane.first_turn + 1,
            "{context}: {lane:?}"
        );
        next = lane.last_turn + 1;
    }
    assert_eq!(
        next, turns,
        "{context}: lanes end at turn {next} of {turns}"
    );
}

fn workset(backend: Box<dyn Backend>, id: &str) -> Workset {
    let backends = vec![backend];
    build_with_backends(&backends, Selection::Id(id), MIN_LANE_BYTES).unwrap()
}

#[derive(Default)]
struct Collect(Vec<Turn>);

impl TurnSink for Collect {
    fn session(&mut self, _: &Session) -> anyhow::Result<()> {
        Ok(())
    }
    fn turn(&mut self, turn: Turn) -> anyhow::Result<()> {
        self.0.push(turn);
        Ok(())
    }
}

fn turns(backend: Box<dyn Backend>, id: &str) -> Vec<Turn> {
    let mut sink = Collect::default();
    show_full_with_backends(&[backend], Selection::Id(id), &mut sink).unwrap();
    sink.0
}

#[test]
fn lanes_partition_every_sources_turns_for_every_harness() {
    for (backend, id) in cases() {
        let workset = workset(backend, id);
        assert_eq!(workset.sources[0].role, SourceRole::Root, "{id}");
        for source in &workset.sources {
            let context = format!("{id} source {}", source.key);
            let (Some(lanes), Some(coverage)) = (&source.lanes, &source.coverage) else {
                assert!(source.diagnostic.is_some(), "{context}");
                continue;
            };
            assert!(coverage.turns > 0, "{context}: fixture has no turns");
            assert_partition(lanes, coverage.turns, &context);
            for (index, lane) in lanes.iter().enumerate() {
                assert_eq!(lane.id, format!("{}-{index}", source.key), "{context}");
            }
        }
    }
}

#[test]
fn lanes_partition_turns_cut_far_below_the_workset_bound() {
    for (backend, id) in cases() {
        let turns = turns(backend, id);
        let largest = turns
            .iter()
            .map(|turn| project_turn("s0", turn).len() as u64)
            .max()
            .unwrap();
        for bound in [1, largest / 2, largest, largest * 3] {
            let mut cutter = LaneCutter::new("s0", bound);
            turns.iter().for_each(|turn| cutter.push(turn));
            let lanes = cutter.finish();
            assert_partition(&lanes, turns.len(), &format!("{id} at {bound}"));
            for lane in &lanes {
                assert!(
                    lane.projected_bytes <= bound || lane.first_turn == lane.last_turn,
                    "{id}: {lane:?} exceeds {bound}"
                );
            }
            for pair in lanes.windows(2) {
                let next = &turns[pair[1].first_turn];
                assert!(
                    pair[0].projected_bytes + project_turn("s0", next).len() as u64 > bound,
                    "{id}: lane {} closed before it was full",
                    pair[0].id
                );
            }
        }
    }
}

#[test]
fn a_turn_larger_than_the_bound_gets_a_lane_of_its_own() {
    let turns = turns(
        Box::new(ClaudeBackend::new(fixtures("claude"))),
        "session-claude",
    );
    let mut cutter = LaneCutter::new("s0", 1);
    turns.iter().for_each(|turn| cutter.push(turn));
    let lanes = cutter.finish();
    assert_eq!(lanes.len(), turns.len());
    for (lane, turn) in lanes.iter().zip(&turns) {
        assert_eq!(
            lane.projected_bytes,
            project_turn("s0", turn).len() as u64,
            "{lane:?}"
        );
    }
}

#[test]
fn skipped_records_appear_in_coverage() {
    for backend in [
        Box::new(ClaudeBackend::new(fixtures("claude"))) as Box<dyn Backend>,
        Box::new(PiBackend::new(fixtures("pi"))),
    ] {
        let workset = workset(backend, "malformed");
        let coverage = workset.sources[0].coverage.as_ref().unwrap();
        assert!(coverage.records_skipped > 0, "{coverage:?}");
    }
}

#[test]
fn file_backed_lanes_record_the_byte_range_their_turns_came_from() {
    let workset = workset(
        Box::new(ClaudeBackend::new(fixtures("claude"))),
        "session-claude",
    );
    let root = &workset.sources[0];
    let length = root.source_length.unwrap();
    for lane in root.lanes.as_ref().unwrap() {
        let range = lane.byte_range.expect("every claude turn carries a span");
        assert!(range.start < range.end && range.end <= length, "{lane:?}");
    }
    assert!(root.source_prefix_digest.is_some());
}

#[test]
fn relatives_follow_the_root_and_an_unreadable_root_fails() {
    let workset = workset(
        Box::new(ClaudeBackend::new(fixtures("claude"))),
        "session-claude",
    );
    let child = workset
        .sources
        .iter()
        .find(|source| source.role == SourceRole::Child)
        .expect("the fixture's subagent is a child source");
    assert_eq!(child.reference.as_deref(), Some("fixture"));
    assert!(child.lanes.is_some(), "{child:?}");

    let pi = workset_of(PiBackend::new(pi_store_with_resolvable_parent()), "child");
    assert_eq!(pi.sources.len(), 2, "{pi:?}");
    assert_eq!(pi.sources[1].role, SourceRole::Parent);
    assert_eq!(pi.sources[1].session_id, "native_parent");
    assert!(pi.sources[0]
        .coverage
        .as_ref()
        .unwrap()
        .unresolved_references
        .is_empty());

    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures("pi")))];
    assert!(
        build_with_backends(&backends, Selection::Id("absent-session"), MIN_LANE_BYTES).is_err()
    );
}

#[test]
fn a_lane_bound_outside_the_accepted_range_is_refused() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures("pi")))];
    for bound in [MIN_LANE_BYTES - 1, 4 * 1024 * 1024 + 1] {
        assert!(build_with_backends(&backends, Selection::Id("session-pi"), bound).is_err());
    }
}

/// pi names a parent by its recording's absolute path, so the fixture's
/// child is copied beside its parent and made to name that copy.
fn pi_store_with_resolvable_parent() -> PathBuf {
    let store = std::env::temp_dir().join(format!("tapes-workset-pi-{}", std::process::id()));
    std::fs::create_dir_all(&store).unwrap();
    let parent = store.join("2026-01-13T10-00-00-000Z_native_parent.jsonl");
    std::fs::copy(
        fixtures("pi-relative-parent").join("2026-01-13T10-00-00-000Z_native_parent.jsonl"),
        &parent,
    )
    .unwrap();
    let child = std::fs::read_to_string(
        fixtures("pi-relative-parent").join("2026-01-13T10-00-00-000Z_child.jsonl"),
    )
    .unwrap()
    .replace(
        "\"parentSession\":\"2026-01-13T10-00-00-000Z_native_parent.jsonl\"",
        &format!(
            "\"parentSession\":{}",
            serde_json::to_string(&parent).unwrap()
        ),
    );
    std::fs::write(store.join("2026-01-13T10-00-00-000Z_child.jsonl"), child).unwrap();
    store
}

fn workset_of(backend: impl Backend + 'static, id: &str) -> Workset {
    workset(Box::new(backend), id)
}
