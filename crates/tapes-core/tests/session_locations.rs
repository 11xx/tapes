//! A transcript reports where its session worked over time: each distinct
//! directory, branch and commit the recording shows, in recording order, as
//! changes only. A field the harness did not record is absent. A bounded read
//! says it covers only its window, and the stream is capped.

use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::model::{
    LocationCoverage, LocationObservation, SessionLocations, Transcript, MAX_LOCATION_OBSERVATIONS,
};
use agent_tapes_core::{child, Selection};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;

mod support;

const CLAUDE: &str = "c1000000-0000-4000-8000-000000000001";
const CODEX: &str = "c2000000-0000-7000-8000-000000000001";
const CODEX_DETACHED: &str = "c2000000-0000-7000-8000-000000000002";
const PI: &str = "c3000000-0000-4000-8000-000000000001";
const REPO: &str = "/work/repo";
const WORKTREE: &str = "/work/repo-wt";

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/session-locations")
        .join(harness)
}

fn at(hour: u32, minute: u32, second: u32) -> Option<DateTime<Utc>> {
    Some(
        Utc.with_ymd_and_hms(2026, 1, 8, hour, minute, second)
            .unwrap(),
    )
}

fn place(at: Option<DateTime<Utc>>, cwd: &str, branch: Option<&str>) -> LocationObservation {
    LocationObservation {
        at,
        cwd: cwd.to_owned(),
        branch: branch.map(str::to_owned),
        head_commit: None,
        repository_url: None,
    }
}

/// The locations of the bounded read and of the whole-recording read of one
/// session, which must agree wherever the bounded read reaches the whole.
fn both(backend: &dyn Backend, id: &str) -> (SessionLocations, SessionLocations) {
    let session = backend.locate(id).unwrap().unwrap();
    let bounded = backend
        .transcript(&session, usize::MAX)
        .unwrap()
        .locations
        .expect("the bounded read reports locations");
    let read = backend
        .stream_transcript(&session, None, &mut |_| Ok(()))
        .unwrap();
    let whole = read
        .transcript(session, Vec::new(), None)
        .locations
        .expect("the whole read reports locations");
    (bounded, whole)
}

#[test]
fn claude_reports_each_change_of_branch_and_directory_once() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let (bounded, whole) = both(&backend, CLAUDE);
    let expected = vec![
        place(at(10, 0, 0), REPO, Some("main")),
        place(at(10, 1, 0), REPO, Some("feature-x")),
        // `HEAD` is what Claude writes when no branch is checked out; it is no
        // branch name, and the move off `feature-x` is still a change.
        place(at(10, 2, 0), REPO, None),
        place(at(10, 3, 0), WORKTREE, Some("feature-x")),
        place(at(10, 4, 0), REPO, Some("main")),
    ];
    for locations in [&bounded, &whole] {
        assert_eq!(locations.coverage, LocationCoverage::Whole);
        assert_eq!(locations.omitted, 0);
        assert_eq!(locations.observations, expected);
    }
}

#[test]
fn claude_subagent_reports_the_places_its_own_recording_shows() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let parent = backend.locate(CLAUDE).unwrap().unwrap();
    let expected = vec![
        place(at(10, 0, 10), "/work/delegates/lane", Some("delegate-lane")),
        place(at(10, 5, 0), "/work/delegates/lane", None),
    ];
    let bounded = backend
        .child_transcript(&parent, "a0123456789abcdef")
        .unwrap()
        .locations
        .unwrap();
    assert_eq!(bounded.coverage, LocationCoverage::Whole);
    assert_eq!(bounded.observations, expected);
    let child = backend
        .stream_child_transcript(&parent, "a0123456789abcdef", &mut |_| Ok(()))
        .unwrap();
    let whole = child
        .read
        .transcript(child.session, Vec::new(), None)
        .locations
        .unwrap();
    assert_eq!(whole.observations, expected);
}

#[test]
fn the_child_view_carries_its_own_places_in_both_reads() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(fixtures("claude")))];
    let expected = vec![
        place(at(10, 0, 10), "/work/delegates/lane", Some("delegate-lane")),
        place(at(10, 5, 0), "/work/delegates/lane", None),
    ];
    let bounded =
        child::read_with_backends(&backends, Selection::Id(CLAUDE), "a0123456789abcdef", 20)
            .unwrap();
    let full =
        child::read_full_with_backends(&backends, Selection::Id(CLAUDE), "a0123456789abcdef", 20)
            .unwrap();
    for view in [bounded, full] {
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["schema"], "tapes-child/11");
        let locations = view.transcript.locations.unwrap();
        assert_eq!(locations.coverage, LocationCoverage::Whole);
        assert_eq!(locations.observations, expected);
        assert_eq!(
            json["transcript"]["locations"]["observations"][0]["branch"],
            "delegate-lane"
        );
    }
}

#[test]
fn a_bounded_claude_read_covers_only_its_window() {
    let path = fixtures("claude")
        .join("project")
        .join(format!("{CLAUDE}.jsonl"));
    let size = fs::metadata(&path).unwrap().len();
    let backend = ClaudeBackend::new(fixtures("claude")).with_read_bytes(size / 3);
    let session = backend.locate(CLAUDE).unwrap().unwrap();
    let locations = backend
        .transcript(&session, usize::MAX)
        .unwrap()
        .locations
        .unwrap();
    assert_eq!(locations.coverage, LocationCoverage::FromWindowStart);
    // The window holds the last two records only: its first place is where
    // the window opens, stamped by the record that opens it, and the places
    // before it are not claimed.
    assert_eq!(
        locations.observations,
        vec![
            place(at(10, 3, 1), WORKTREE, Some("feature-x")),
            place(at(10, 4, 0), REPO, Some("main")),
        ]
    );
}

#[test]
fn codex_reports_the_session_meta_repository_and_each_turn_directory_change() {
    let backend = CodexBackend::new(fixtures("codex"));
    let (bounded, whole) = both(&backend, CODEX);
    let expected = vec![
        LocationObservation {
            at: at(10, 0, 0),
            cwd: REPO.to_owned(),
            branch: Some("main".to_owned()),
            head_commit: Some("1".repeat(40)),
            // The URL carried credentials; only where the repository is stays.
            repository_url: Some("https://example.test/org/repo.git".to_owned()),
        },
        // A turn records only its directory: no branch or commit is claimed
        // for it, and an unchanged directory is no new place.
        place(at(10, 1, 0), WORKTREE, None),
        place(at(10, 3, 0), REPO, None),
    ];
    for locations in [&bounded, &whole] {
        assert_eq!(locations.coverage, LocationCoverage::Whole);
        assert_eq!(locations.observations, expected);
    }
}

#[test]
fn codex_detached_head_has_a_commit_and_no_branch() {
    let backend = CodexBackend::new(fixtures("codex"));
    let (bounded, whole) = both(&backend, CODEX_DETACHED);
    let expected = vec![LocationObservation {
        at: at(11, 0, 0),
        cwd: REPO.to_owned(),
        branch: None,
        head_commit: Some("2".repeat(40)),
        repository_url: Some("git@example.test:org/repo.git".to_owned()),
    }];
    assert_eq!(bounded.observations, expected);
    assert_eq!(whole.observations, expected);
}

#[test]
fn pi_reports_the_header_directory_and_nothing_it_does_not_record() {
    let backend = PiBackend::new(fixtures("pi"));
    let (bounded, whole) = both(&backend, PI);
    let expected = vec![place(at(10, 0, 0), REPO, None)];
    assert_eq!(bounded.observations, expected);
    assert_eq!(whole.observations, expected);
}

#[test]
fn opencode_reports_its_session_directory_as_one_observation() {
    std::env::set_var("XDG_DATA_HOME", support::empty_opencode_data_home());
    let program = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/opencode2");
    let backend = OpenCodeBackend::new(program);
    let (bounded, whole) = both(&backend, "ses_api_only_fixture");
    for locations in [bounded, whole] {
        assert_eq!(locations.coverage, LocationCoverage::Whole);
        assert_eq!(locations.observations.len(), 1);
        let observation = &locations.observations[0];
        assert_eq!(observation.cwd, "/fixtures/api-project");
        assert_eq!(observation.branch, None);
        assert_eq!(observation.head_commit, None);
        assert_eq!(observation.repository_url, None);
    }
}

#[test]
fn a_long_stream_keeps_the_newest_places_and_counts_what_it_dropped() {
    let root = tempfile::TempDir::with_prefix("tapes-locations-cap-").unwrap();
    let project = root.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let id = "c1000000-0000-4000-8000-0000000000ff";
    let total = MAX_LOCATION_OBSERVATIONS + 76;
    let mut body = String::new();
    for index in 0..total {
        body.push_str(
            &json!({
                "type": "user",
                "uuid": format!("r{index}"),
                "sessionId": id,
                "timestamp": format!("2026-01-08T10:{:02}:{:02}Z", index / 60 % 60, index % 60),
                "cwd": REPO,
                "gitBranch": format!("branch-{index}"),
                "message": {"role": "user", "content": "Prompt."},
            })
            .to_string(),
        );
        body.push('\n');
    }
    fs::write(project.join(format!("{id}.jsonl")), body).unwrap();
    let backend = ClaudeBackend::new(root.path());
    let (bounded, whole) = both(&backend, id);
    for locations in [bounded, whole] {
        assert_eq!(locations.observations.len(), MAX_LOCATION_OBSERVATIONS);
        assert_eq!(locations.omitted, 76);
        assert_eq!(
            locations.observations[0].branch.as_deref(),
            Some("branch-76")
        );
        assert_eq!(
            locations.observations.last().unwrap().branch.as_deref(),
            Some(format!("branch-{}", total - 1).as_str())
        );
    }
}

#[test]
fn the_session_json_carries_locations_and_omits_absent_fields() {
    let backend = CodexBackend::new(fixtures("codex"));
    let session = backend.locate(CODEX).unwrap().unwrap();
    let transcript: Transcript = backend.transcript(&session, usize::MAX).unwrap();
    let value = serde_json::to_value(&transcript).unwrap();
    assert_eq!(value["schema"], "tapes-session/23");
    let locations = &value["locations"];
    assert_eq!(locations["coverage"], "whole");
    assert!(locations.get("omitted").is_none());
    let observations = locations["observations"].as_array().unwrap();
    assert_eq!(observations.len(), 3);
    assert_eq!(observations[0]["branch"], "main");
    assert_eq!(observations[0]["at"], "2026-01-08T10:00:00Z");
    let turn_place = observations[1].as_object().unwrap();
    assert_eq!(turn_place["cwd"], WORKTREE);
    for absent in ["branch", "head_commit", "repository_url"] {
        assert!(!turn_place.contains_key(absent), "{absent} was defaulted");
    }
    let round_trip: Transcript = serde_json::from_value(value).unwrap();
    assert_eq!(round_trip.locations, transcript.locations);
}

#[test]
fn a_window_that_holds_no_place_says_it_is_a_window() {
    let backend = PiBackend::new(fixtures("pi"));
    let path = fs::read_dir(fixtures("pi"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let backend = backend.with_read_bytes(fs::metadata(path).unwrap().len() / 4);
    let session = backend.locate(PI).unwrap().unwrap();
    let locations = backend
        .transcript(&session, usize::MAX)
        .unwrap()
        .locations
        .unwrap();
    assert_eq!(locations.coverage, LocationCoverage::FromWindowStart);
    assert!(locations.observations.is_empty());
}
