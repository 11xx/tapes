//! A workset's lanes partition every source's turns exactly, whichever
//! harness recorded the source, and are cut at the projected-byte bound.

use std::path::{Path, PathBuf};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::opencode::OpenCodeBackend;
use agent_tapes_core::backend::pi::PiBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::lane_file::{LaneSource, EMPTY_MARGIN, LANE_MARKER, MARGIN_MARKER};
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
    build_with_backends(&backends, Selection::Id(id), MIN_LANE_BYTES, None).unwrap()
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
            turns.iter().for_each(|turn| cutter.push(turn).unwrap());
            let lanes = cutter.finish().unwrap();
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
    turns.iter().for_each(|turn| cutter.push(turn).unwrap());
    let lanes = cutter.finish().unwrap();
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
    assert!(build_with_backends(
        &backends,
        Selection::Id("absent-session"),
        MIN_LANE_BYTES,
        None
    )
    .is_err());
}

#[test]
fn a_lane_bound_outside_the_accepted_range_is_refused() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures("pi")))];
    for bound in [MIN_LANE_BYTES - 1, 4 * 1024 * 1024 + 1] {
        assert!(build_with_backends(&backends, Selection::Id("session-pi"), bound, None).is_err());
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

fn turn_of(kind: &str, text: &str) -> Turn {
    serde_json::from_value(serde_json::json!({
        "role": if kind == "operator" { "user" } else { "assistant" },
        "kind": kind,
        "text": text,
        "ordinal": 7,
    }))
    .unwrap()
}

#[test]
fn an_oversized_tool_result_renders_within_its_cap_and_a_marker() {
    let rendered = project_turn("s0", &turn_of("tool", &"x".repeat(1_000_000)));
    assert!(rendered.len() < 2 * 1024 + 200, "{} bytes", rendered.len());
    assert!(rendered.contains("[… 997952 bytes omitted; cite s0#7 to read the whole turn]"));
}

#[test]
fn an_operator_turn_is_cut_at_sixteen_kibibytes_on_a_character_boundary() {
    let rendered = project_turn("s0", &turn_of("operator", &"y".repeat(20 * 1024)));
    let body = rendered.split("[…").next().unwrap();
    assert_eq!(body.matches('y').count(), 16 * 1024);
    assert!(rendered.contains("[… 4096 bytes omitted;"), "{rendered}");

    // A two-byte character straddles the 16 KiB cut; it is kept out whole.
    let text = format!("{}é{}", "z".repeat(16 * 1024 - 1), "z".repeat(100));
    let rendered = project_turn("s0", &turn_of("assistant", &text));
    assert!(!rendered.contains('é'));
    let body = rendered.split("[…").next().unwrap();
    assert_eq!(body.matches('z').count(), 16 * 1024 - 1);
    assert!(
        rendered.contains(&format!("[… {} bytes omitted;", 2 + 100)),
        "{rendered}"
    );
}

#[test]
fn omitted_bytes_add_up_from_turns_to_lanes_to_coverage() {
    let turns = [
        turn_of("tool", &"t".repeat(5000)),
        turn_of("operator", &"o".repeat(17 * 1024)),
        turn_of("assistant", "short"),
    ];
    let mut cutter = LaneCutter::new("s0", 1);
    for (ordinal, turn) in turns.iter().enumerate() {
        let mut turn = turn.clone();
        turn.ordinal = ordinal;
        cutter.push(&turn).unwrap();
    }
    let lanes = cutter.finish().unwrap();
    let by_lane: Vec<u64> = lanes
        .iter()
        .map(|l| l.omitted_bytes.values().sum())
        .collect();
    assert_eq!(by_lane, [5000 - 2048, 1024, 0]);
    assert!(lanes[2].omitted_bytes.is_empty());

    let workset = workset(
        Box::new(ClaudeBackend::new(fixtures("claude"))),
        "session-claude",
    );
    for source in workset.sources.iter().filter(|s| s.lanes.is_some()) {
        let lanes: u64 = source
            .lanes
            .as_ref()
            .unwrap()
            .iter()
            .flat_map(|l| l.omitted_bytes.values())
            .sum();
        assert_eq!(source.coverage.as_ref().unwrap().omitted_bytes, lanes);
    }
}

fn scratch(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("tapes-workset-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn bundled(backend: Box<dyn Backend>, id: &str, bundle: &Path) -> Workset {
    build_with_backends(&[backend], Selection::Id(id), MIN_LANE_BYTES, Some(bundle)).unwrap()
}

/// The lane file's margin and lane sections, split at their marker lines.
fn sections(text: &str) -> (&str, &str) {
    let (_, rest) = text
        .split_once(&format!("{MARGIN_MARKER}\n"))
        .expect("margin marker");
    rest.split_once(&format!("{LANE_MARKER}\n"))
        .expect("lane marker")
}

/// Ordinals of the projected turns a section holds, by their `## <key>#N` headers.
fn ordinals(section: &str, key: &str) -> Vec<usize> {
    section
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("## {key}#")))
        .map(|rest| rest.split(' ').next().unwrap().parse().unwrap())
        .collect()
}

#[test]
fn lane_sections_reproduce_the_projection_of_every_turn_byte_for_byte() {
    for (index, (backend, id)) in cases().into_iter().enumerate() {
        let bundle = scratch(&format!("bytes-{index}"));
        let workset = bundled(backend, id, &bundle);
        for source in &workset.sources {
            let Some(lanes) = &source.lanes else { continue };
            let mut concatenated = String::new();
            for lane in lanes {
                let file = lane.file.as_deref().expect("a lane file");
                let text = std::fs::read_to_string(bundle.join(file)).unwrap();
                let (_, body) = sections(&text);
                assert_eq!(body.len() as u64, lane.projected_bytes, "{id} {}", lane.id);
                concatenated.push_str(body);
            }
            let backend = cases().into_iter().nth(index).unwrap().0;
            let turns = match &source.reference {
                Some(reference) => child_turns(backend.as_ref(), &workset.root, reference),
                None => turns(backend, &source.session_id),
            };
            let expected: String = turns
                .iter()
                .map(|turn| project_turn(&source.key, turn))
                .collect();
            assert_eq!(concatenated, expected, "{id} source {}", source.key);
        }
        std::fs::remove_dir_all(bundle).unwrap();
    }
}

fn assert_margins(directory: &Path, lanes: &[Lane], key: &str, context: &str) -> usize {
    let mut operator_seen = false;
    let mut filled = 0;
    for lane in lanes {
        let name = lane.file.as_deref().unwrap().trim_start_matches("lanes/");
        let text = std::fs::read_to_string(directory.join(name)).unwrap();
        let (margin, body) = sections(&text);
        let margin_ordinals = ordinals(margin, key);
        assert!(
            margin_ordinals
                .iter()
                .all(|ordinal| *ordinal < lane.first_turn),
            "{context} {}: {margin_ordinals:?}",
            lane.id
        );
        if operator_seen {
            assert!(!margin_ordinals.is_empty(), "{context} {}", lane.id);
            filled += 1;
        } else {
            assert_eq!(
                margin,
                format!("{EMPTY_MARGIN}\n\n"),
                "{context} {}",
                lane.id
            );
        }
        operator_seen |= body
            .lines()
            .any(|line| line.starts_with(&format!("## {key}#")) && line.contains(" operator"));
    }
    filled
}

#[test]
fn margins_hold_only_earlier_operator_context() {
    let mut filled = 0;
    for (index, (backend, id)) in cases().into_iter().enumerate() {
        let bundle = scratch(&format!("margin-{index}"));
        let workset = bundled(backend, id, &bundle);
        for source in &workset.sources {
            if let Some(lanes) = &source.lanes {
                assert_margins(&bundle.join("lanes"), lanes, &source.key, id);
            }
        }
        std::fs::remove_dir_all(&bundle).unwrap();

        // Cut far below the workset bound, so later lanes carry margins.
        let backend = cases().into_iter().nth(index).unwrap().0;
        std::fs::create_dir_all(&bundle).unwrap();
        let source = LaneSource {
            key: "s0".to_owned(),
            harness: backend.harness().to_owned(),
            session: id.to_owned(),
        };
        let mut cutter = LaneCutter::with_files(source, 300, &bundle);
        for turn in turns(backend, id) {
            cutter.push(&turn).unwrap();
        }
        let lanes = cutter.finish().unwrap();
        filled += assert_margins(&bundle, &lanes, "s0", id);
        std::fs::remove_dir_all(bundle).unwrap();
    }
    assert!(filled > 0, "no fixture lane carried a margin");
}

#[test]
fn a_margin_keeps_the_last_two_operator_turns_with_their_first_replies() {
    let turns = [
        ("operator", "first ask"),
        ("assistant", "first reply"),
        ("operator", "second ask"),
        ("tool", "a call"),
        ("assistant", "second reply"),
        ("assistant", "later reply"),
        ("operator", "third ask"),
        ("assistant", "third reply"),
        ("operator", "fourth ask"),
    ];
    let directory = scratch("ring");
    std::fs::create_dir_all(&directory).unwrap();
    let source = LaneSource {
        key: "s0".to_owned(),
        harness: "fixture".to_owned(),
        session: "fixture-session".to_owned(),
    };
    let mut cutter = LaneCutter::with_files(source, 1, &directory);
    for (ordinal, (kind, text)) in turns.iter().enumerate() {
        let mut turn = turn_of(kind, text);
        turn.ordinal = ordinal;
        cutter.push(&turn).unwrap();
    }
    let lanes = cutter.finish().unwrap();
    let last = std::fs::read_to_string(directory.join("s0-8.md")).unwrap();
    let (margin, _) = sections(&last);
    assert_eq!(ordinals(margin, "s0"), [2, 4, 6, 7], "{margin}");
    assert!(last.contains("turns: s0#8..s0#8"), "{last}");
    assert_eq!(lanes[8].file.as_deref(), Some("lanes/s0-8.md"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn an_existing_bundle_is_refused_and_left_alone() {
    let bundle = scratch("refuse");
    let backend = || Box::new(PiBackend::new(fixtures("pi"))) as Box<dyn Backend>;
    bundled(backend(), "session-pi", &bundle);
    let lane = std::fs::read(bundle.join("lanes/s0-0.md")).unwrap();
    let error = build_with_backends(
        &[backend()],
        Selection::Id("session-pi"),
        MIN_LANE_BYTES,
        Some(&bundle),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("already holds a workset"),
        "{error:#}"
    );
    assert_eq!(std::fs::read(bundle.join("lanes/s0-0.md")).unwrap(), lane);

    let manifest_only = scratch("refuse-manifest");
    std::fs::create_dir_all(&manifest_only).unwrap();
    std::fs::write(manifest_only.join("workset.json"), "{}").unwrap();
    assert!(build_with_backends(
        &[backend()],
        Selection::Id("session-pi"),
        MIN_LANE_BYTES,
        Some(&manifest_only),
    )
    .is_err());
    assert!(!manifest_only.join("lanes").exists());
    std::fs::remove_dir_all(bundle).unwrap();
    std::fs::remove_dir_all(manifest_only).unwrap();
}

#[test]
fn an_unreadable_root_leaves_no_lane_files() {
    let bundle = scratch("absent-root");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures("pi")))];
    assert!(build_with_backends(
        &backends,
        Selection::Id("absent-session"),
        MIN_LANE_BYTES,
        Some(&bundle)
    )
    .is_err());
    assert!(!bundle.join("lanes").exists());
    let _ = std::fs::remove_dir_all(bundle);
}

fn child_turns(backend: &dyn Backend, root: &str, reference: &str) -> Vec<Turn> {
    let root = backend.locate(root).unwrap().unwrap();
    let mut turns = Vec::new();
    backend
        .stream_child_transcript(&root, reference, &mut |mut turn| {
            turn.ordinal = turns.len();
            turns.push(turn);
            Ok(())
        })
        .unwrap();
    turns
}
