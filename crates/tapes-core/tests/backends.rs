use std::cell::Cell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use tapes_core::backend::claude::ClaudeBackend;
use tapes_core::backend::codex::CodexBackend;
use tapes_core::backend::opencode::OpenCodeBackend;
use tapes_core::backend::pi::PiBackend;
use tapes_core::backend::{Backend, Listing, Query};
use tapes_core::model::{Role, Session, SourceBound, Transcript, Truncation, Turn};
use tapes_core::{
    latest_with_backends, list_with_backends, list_with_backends_filtered,
    list_with_backends_filtered_and_search, resolve_session, scope::Scope, show_with_backends,
    ResolveError, Selection, LIST_SEARCH_TAIL,
};

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(harness)
}

fn file_fixture_backends() -> Vec<(Box<dyn Backend>, &'static str)> {
    vec![
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))),
            "session-claude",
        ),
        (
            Box::new(CodexBackend::new(fixtures("codex"))),
            "00000000-0000-0000-0000-000000000001",
        ),
        (Box::new(PiBackend::new(fixtures("pi"))), "session-pi"),
    ]
}

fn fixture_backends() -> Vec<(Box<dyn Backend>, &'static str)> {
    let mut backends = file_fixture_backends();
    backends.insert(
        2,
        (
            Box::new(OpenCodeBackend::new(opencode_fixture_program())),
            "ses_000000fixtureSharedSession",
        ),
    );
    backends
}

fn located(backend: &dyn Backend, id: &str) -> Session {
    backend
        .locate(id)
        .unwrap()
        .unwrap_or_else(|| panic!("fixture session {id} is missing"))
}

fn opencode_fixture_program() -> PathBuf {
    fixtures("opencode").join("opencode2")
}

static OPENCODE_ALIAS_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct OpenCodeAlias {
    path: PathBuf,
    calls: Option<PathBuf>,
    cleanup_dir: Option<PathBuf>,
}

impl OpenCodeAlias {
    fn new(tag: &str) -> Self {
        let serial = OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("opencode2-{tag}-{}-{serial}", std::process::id()));
        Self::link(path, opencode_fixture_program(), None)
    }

    fn malformed_database() -> Self {
        let serial = OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "tapes-opencode-malformed-row-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        Self::link(
            directory.join("opencode"),
            fixtures("opencode").join("opencode-malformed-row"),
            Some(directory),
        )
    }

    fn database() -> Self {
        let serial = OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "tapes-opencode-database-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        Self::link(
            directory.join("opencode"),
            opencode_fixture_program(),
            Some(directory),
        )
    }

    fn link(path: PathBuf, target: PathBuf, cleanup_dir: Option<PathBuf>) -> Self {
        let _ = fs::remove_file(&path);
        std::os::unix::fs::symlink(target, &path).unwrap();
        Self {
            path,
            calls: None,
            cleanup_dir,
        }
    }

    fn oversized() -> Self {
        let mut alias = Self::new("oversized");
        let calls = PathBuf::from(format!("{}.calls", alias.path.display()));
        let _ = fs::remove_file(&calls);
        alias.calls = Some(calls);
        alias
    }

    fn counting() -> Self {
        let mut alias = Self::new("counting");
        let calls = PathBuf::from(format!("{}.calls", alias.path.display()));
        let _ = fs::remove_file(&calls);
        alias.calls = Some(calls);
        alias
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn calls(&self) -> &Path {
        self.calls
            .as_deref()
            .expect("only a counting OpenCode alias has a call log")
    }
}

impl Drop for OpenCodeAlias {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        if let Some(calls) = &self.calls {
            let _ = fs::remove_file(calls);
        }
        if let Some(directory) = &self.cleanup_dir {
            let _ = fs::remove_dir(directory);
        }
    }
}

fn opencode_alias_program(tag: &str) -> OpenCodeAlias {
    OpenCodeAlias::new(tag)
}

fn opencode_titleless_fixture_program() -> OpenCodeAlias {
    opencode_alias_program("titleless")
}

fn opencode_counting_fixture_program() -> OpenCodeAlias {
    OpenCodeAlias::counting()
}

#[test]
fn opencode_fixture_executables_are_stable() {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/backends.rs"));
    assert!(
        !source.contains(concat!("fs", "::set_permissions")),
        "OpenCode backend tests must not chmod executable files at runtime"
    );
    assert!(
        !source.contains(concat!("#!", "/bin/sh")),
        "OpenCode backend tests must not write shell executable bytes at runtime"
    );

    for name in ["opencode2", "opencode-malformed-row"] {
        let program = fixtures("opencode").join(name);
        let metadata = fs::metadata(&program).unwrap();
        assert!(metadata.is_file(), "checked-in OpenCode fixture is missing");
        assert_ne!(metadata.permissions().mode() & 0o111, 0);
        assert!(!fs::symlink_metadata(&program)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

#[test]
fn every_backend_satisfies_shared_normalization_assertions() {
    for (backend, id) in fixture_backends() {
        assert!(
            backend.available(),
            "{} fixture is unavailable",
            backend.harness()
        );

        let sessions = backend.list(&Query::unscoped(10)).unwrap().sessions;
        let listed = sessions.iter().find(|session| session.id == id).unwrap();
        assert_eq!(listed.harness, backend.harness());
        assert!(listed.started_at <= listed.last_activity_at);
        if listed.title.is_none() {
            assert_eq!(
                listed.derived_title.as_deref(),
                Some("Inspect the fixture.")
            );
        } else {
            assert!(listed.derived_title.is_none());
        }

        let transcript = backend.transcript(listed, 10).unwrap();
        assert_eq!(transcript.session, *listed);
        assert!(transcript.turns.len() >= 2);
        assert_eq!(transcript.turns.first().unwrap().role, Role::User);
        assert_eq!(transcript.turns.last().unwrap().role, Role::Assistant);
        assert!(transcript.turns.iter().all(|turn| !turn.text.is_empty()));

        let tailed = backend.transcript(listed, 1).unwrap();
        assert_eq!(tailed.turns.len(), 1);
        assert!(tailed.truncated);
    }
}

#[test]
fn every_backend_searches_normalized_fixture_turns() {
    for (backend, id) in fixture_backends() {
        let session = located(backend.as_ref(), id);

        assert!(
            backend
                .search(&session, "FIXTURE", LIST_SEARCH_TAIL)
                .unwrap(),
            "{} fixture was not searchable",
            backend.harness()
        );
        assert!(!backend
            .search(&session, "not in this fixture", LIST_SEARCH_TAIL)
            .unwrap());
    }
}

#[test]
fn malformed_lines_do_not_hide_searchable_turns() {
    let cases: Vec<(Box<dyn Backend>, &str)> = vec![
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))),
            "malformed",
        ),
        (
            Box::new(CodexBackend::new(fixtures("codex"))),
            "10000000-0000-0000-0000-000000000002",
        ),
        (Box::new(PiBackend::new(fixtures("pi"))), "malformed"),
    ];

    for (backend, id) in cases {
        let session = located(backend.as_ref(), id);
        assert!(backend.search(&session, "VALID", LIST_SEARCH_TAIL).unwrap());
    }
}

#[test]
fn list_search_uses_each_backend_search_path_before_the_limit() {
    let backends = fixture_backends()
        .into_iter()
        .map(|(backend, _)| backend)
        .collect::<Vec<_>>();
    let result = list_with_backends_filtered_and_search(
        &backends,
        None,
        None,
        1,
        None,
        None,
        Some("fixture"),
    )
    .unwrap();

    assert_eq!(result.sessions.len(), 4);
    assert!(result.unsearched.is_empty());
}

#[test]
fn show_reuses_resolved_opencode_session() {
    let program = opencode_counting_fixture_program();
    let id = "ses_000000fixtureSharedSession";
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(program.path()))];

    let transcript = show_with_backends(&backends, Selection::Id(id), 10).unwrap();
    assert_eq!(transcript.session.id, id);

    let routes = fs::read_to_string(program.calls())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(
        routes,
        vec![
            format!("/api/session/{id}"),
            format!("/api/session/{id}/message?limit=10&order=desc")
        ]
    );
}

#[test]
fn duplicate_opencode_projections_are_not_ambiguous() {
    let first = opencode_counting_fixture_program();
    let second = opencode_counting_fixture_program();
    let id = "ses_000000fixtureSharedSession";
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(OpenCodeBackend::new(first.path())),
        Box::new(OpenCodeBackend::new(second.path())),
    ];

    let resolved = resolve_session(&backends, id).unwrap();
    assert_eq!(resolved.backend_index, 0);

    let listing = list_with_backends(&backends, Some("opencode"), None, 10).unwrap();
    let listed = listing
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    assert!(listed.iter().any(|session| session == id));
    let mut distinct = listed.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), listed.len(), "{listed:?}");
}

#[test]
fn titleless_opencode_metadata_stays_titleless_through_message_reads() {
    let program = opencode_titleless_fixture_program();
    let alias_path = program.path().to_owned();
    let backend = OpenCodeBackend::new(program.path());
    let listed = backend
        .list(&Query::unscoped(10))
        .unwrap()
        .sessions
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(listed.id, "ses_titleless_fixture");
    assert!(listed.title.is_none());
    assert!(listed.derived_title.is_none());

    let transcript = backend.transcript(&listed, 10).unwrap();
    assert!(transcript.session.title.is_none());
    assert!(transcript.session.derived_title.is_none());
    assert_eq!(transcript.turns[0].text, "Inspect the title-less fixture.");

    drop(program);
    assert!(
        !alias_path.exists(),
        "titleless OpenCode alias was not removed"
    );
}

#[test]
fn opencode_metadata_filters_apply_before_the_listing_limit() {
    let backends: Vec<Box<dyn Backend>> =
        vec![Box::new(OpenCodeBackend::new(opencode_fixture_program()))];

    let result = list_with_backends_filtered(
        &backends,
        Some("opencode"),
        None,
        1,
        Some("FIXTURE-API"),
        Some("API-PROJECT"),
    )
    .unwrap();

    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0].id, "ses_api_only_fixture");
    assert_eq!(result.scanned, 2);
}

#[test]
fn opencode_database_search_prefilters_candidates_before_bounded_reads() {
    let program = OpenCodeAlias::database();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(program.path()))];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("opencode"),
        None,
        10,
        None,
        None,
        Some("FIXTURE"),
    )
    .unwrap();

    assert_eq!(
        result
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "ses_database_only_fixture",
            "ses_000000fixtureSharedSession"
        ]
    );
    assert!(result.unsearched.is_empty());
}

#[test]
fn opencode_database_search_prefilter_failure_is_visible_before_fallback() {
    let program = OpenCodeAlias::database();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(program.path()))];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("opencode"),
        None,
        10,
        None,
        None,
        Some("PREFILTERERROR"),
    )
    .unwrap();

    assert!(result.sessions.is_empty());
    assert_eq!(result.unsearched.len(), 1);
    assert!(result.unsearched[0].contains("opencode search prefilter failed"));
    assert!(result.unsearched[0].contains("searched without prefilter"));
}

#[test]
fn opencode_v2_only_search_keeps_all_genuine_fixture_matches() {
    let api = OpenCodeAlias::counting();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(api.path()))];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("opencode"),
        None,
        10,
        None,
        None,
        Some("fixture"),
    )
    .unwrap();
    let ids = result
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<HashSet<_>>();

    assert_eq!(
        ids,
        HashSet::from(["ses_000000fixtureSharedSession", "ses_api_only_fixture"])
    );
    assert!(result.unsearched.is_empty());
}

#[test]
fn opencode_search_uses_the_first_projection_before_deduplicating() {
    let stable = OpenCodeAlias::database();
    let api = OpenCodeAlias::new("counting");
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(OpenCodeBackend::new(stable.path())),
        Box::new(OpenCodeBackend::new(api.path())),
    ];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("opencode"),
        None,
        10,
        None,
        None,
        Some("complete"),
    )
    .unwrap();
    let ids = result
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<HashSet<_>>();

    assert_eq!(
        ids,
        HashSet::from(["ses_database_only_fixture", "ses_api_only_fixture"])
    );
    assert!(!ids.contains("ses_000000fixtureSharedSession"));
    assert!(result.unsearched.is_empty());
}

#[test]
fn a_failed_second_opencode_projection_is_not_a_silent_non_match() {
    let stable = OpenCodeAlias::database();
    let broken_api = OpenCodeAlias::new("broken-list");
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(OpenCodeBackend::new(stable.path())),
        Box::new(OpenCodeBackend::new(broken_api.path())),
    ];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("opencode"),
        None,
        10,
        None,
        None,
        Some("fixture"),
    )
    .unwrap();

    assert!(result
        .unsearched
        .iter()
        .any(|diagnostic| diagnostic.contains("opencode v2 search could not list candidates")));
    assert!(result
        .unsearched
        .iter()
        .any(|diagnostic| diagnostic.contains("search could not reconcile duplicate projections")));
}

#[test]
fn opencode_aliases_remove_their_owned_paths_on_drop() {
    let program = OpenCodeAlias::counting();
    let alias_path = program.path().to_owned();
    let calls_path = program.calls().to_owned();
    fs::write(&calls_path, "/api/session?order=desc&limit=10\n").unwrap();
    assert!(alias_path.exists());
    assert!(calls_path.exists());

    drop(program);
    assert!(!alias_path.exists());
    assert!(!calls_path.exists());
}

#[test]
fn codex_skips_instruction_wrappers_when_deriving_title() {
    let backend = CodexBackend::new(fixtures("codex"));
    let session = located(&backend, "10000000-0000-0000-0000-000000000003");

    assert!(session.title.is_none());
    assert_eq!(
        session.derived_title.as_deref(),
        Some("Implement the readable title.")
    );

    let transcript = backend.transcript(&session, 10).unwrap();
    assert_eq!(transcript.session, session);
}

#[test]
fn every_file_backend_resolves_full_ids_and_unambiguous_prefixes() {
    for (backend, id) in file_fixture_backends() {
        let sessions = backend.list(&Query::unscoped(10)).unwrap().sessions;
        let prefix = (1..id.len())
            .map(|length| &id[..length])
            .find(|prefix| {
                sessions
                    .iter()
                    .filter(|session| session.id.starts_with(prefix))
                    .count()
                    == 1
            })
            .expect("fixture has an unambiguous proper prefix");
        let backends = vec![backend];

        for query in [id, prefix] {
            let transcript = show_with_backends(&backends, Selection::Id(query), 10).unwrap();
            assert_eq!(transcript.session.id, id);
        }
    }
}

#[test]
fn every_file_backend_preserves_reasoning_and_tool_chronology() {
    for (backend, id) in fixture_backends() {
        let session = located(backend.as_ref(), id);
        let transcript = backend.transcript(&session, 10).unwrap();
        let roles = transcript
            .turns
            .iter()
            .map(|turn| turn.role.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            roles,
            vec![
                Role::User,
                Role::Reasoning,
                Role::Tool,
                Role::Tool,
                Role::Assistant
            ],
            "{} chronology differs",
            backend.harness()
        );
        assert_eq!(transcript.turns[1].text, "Consider the fixture.");
        assert!(transcript.turns[2].text.contains("fixture_tool"));
        assert!(transcript.turns[3].text.contains("Tool complete."));
    }
}

#[test]
fn verified_file_backends_report_their_final_non_turn_record() {
    let cases: Vec<(Box<dyn Backend>, &str, &str)> = vec![
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))),
            "session-claude",
            "atis-latch",
        ),
        (
            Box::new(CodexBackend::new(fixtures("codex"))),
            "00000000-0000-0000-0000-000000000001",
            "event_msg",
        ),
        (
            Box::new(PiBackend::new(fixtures("pi"))),
            "session-pi",
            "thinking_level_change",
        ),
    ];

    for (backend, id, expected_kind) in cases {
        let session = located(backend.as_ref(), id);
        let transcript = backend.transcript(&session, 10).unwrap();
        let trailing = transcript
            .trailing_record
            .as_ref()
            .unwrap_or_else(|| panic!("{} did not report its fixture suffix", backend.harness()));

        assert_eq!(trailing.kind, expected_kind);
        if backend.harness() == "claude" {
            assert!(trailing.timestamp.is_none());
        } else {
            assert!(trailing.timestamp.is_some());
        }
    }
}

#[test]
fn claude_lists_only_parent_sessions_and_reports_subagent_transcripts() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let sessions = backend.list(&Query::unscoped(10)).unwrap().sessions;

    assert_eq!(
        sessions
            .iter()
            .filter(|session| session.id == "session-claude")
            .count(),
        1
    );
    assert_eq!(sessions.len(), 2);

    let session = located(&backend, "session-claude");
    let transcript = backend.transcript(&session, 10).unwrap();
    assert_eq!(
        transcript.notes,
        vec!["1 subagent transcript belongs to this session.".to_owned()]
    );
}

#[test]
fn malformed_lines_leave_parseable_turns_and_a_note() {
    let cases: Vec<(Box<dyn Backend>, &str)> = vec![
        (
            Box::new(ClaudeBackend::new(fixtures("claude"))),
            "malformed",
        ),
        (
            Box::new(CodexBackend::new(fixtures("codex"))),
            "10000000-0000-0000-0000-000000000002",
        ),
        (Box::new(PiBackend::new(fixtures("pi"))), "malformed"),
    ];

    for (backend, id) in cases {
        let session = located(backend.as_ref(), id);
        let transcript = backend.transcript(&session, 10).unwrap();
        assert_eq!(transcript.turns.len(), 2);
        assert!(
            transcript.trailing_record.is_none(),
            "{} reported a trailing record after its final turn",
            backend.harness()
        );
        assert_eq!(
            transcript.notes,
            vec!["Skipped 1 unparseable line.".to_owned()]
        );
    }
}

#[test]
fn malformed_opencode_database_rows_leave_other_sessions_and_a_diagnostic() {
    let program = OpenCodeAlias::malformed_database();
    let backend = OpenCodeBackend::new(program.path());
    let listing = backend.list(&Query::unscoped(10)).unwrap();

    assert_eq!(listing.scanned, 3);
    assert_eq!(
        listing
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "ses_database_only_fixture",
            "ses_000000fixtureSharedSession"
        ]
    );
    assert_eq!(listing.unavailable.len(), 1);
    assert!(listing.unavailable[0].contains("ses_truncated_fixture"));
    assert!(listing.unavailable[0].contains("EOF while parsing a string"));
}

#[test]
fn malformed_opencode_database_rows_do_not_hide_searchable_sessions() {
    let program = OpenCodeAlias::malformed_database();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(program.path()))];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("opencode"),
        None,
        10,
        None,
        None,
        Some("FIXTURE"),
    )
    .unwrap();

    assert_eq!(result.sessions.len(), 2);
    assert_eq!(result.unreadable.len(), 1);
    assert!(result.unreadable[0].contains("ses_truncated_fixture"));
    assert!(result.unsearched.is_empty());
}

#[test]
fn pi_reports_entries_outside_the_active_leaf_path() {
    let backend = PiBackend::new(fixtures("pi"));
    let session = located(&backend, "session-pi");
    let transcript = backend.transcript(&session, 10).unwrap();

    assert_eq!(
        transcript.notes,
        vec!["1 entry belongs to an abandoned branch.".to_owned()]
    );
    assert!(transcript
        .turns
        .iter()
        .all(|turn| turn.text != "Abandoned branch."));
}

#[test]
fn codex_reads_model_and_effort_from_turn_context() {
    let backend = CodexBackend::new(fixtures("codex"));
    let session = located(&backend, "00000000-0000-0000-0000-000000000001");
    let transcript = backend.transcript(&session, 10).unwrap();
    let model = transcript.session.model.unwrap();

    assert_eq!(model.id, "gpt-fixture");
    assert_eq!(model.variant.as_deref(), Some("high"));
}

#[test]
fn opencode_is_unavailable_when_its_binary_is_absent() {
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(CodexBackend::new(fixtures("codex"))),
        Box::new(OpenCodeBackend::new("/definitely/missing/opencode2")),
    ];

    assert!(!backends[1].available());
    let result = list_with_backends(&backends, None, None, 10).unwrap();
    assert!(!result.sessions.is_empty());
    assert_eq!(result.unavailable, vec!["opencode"]);
}

/// A store holding one session per directory, newest last, in codex's format.
/// Built rather than checked in because scoping is about directories that
/// exist on disk: a recorded path that is gone cannot be attributed to any
/// project.
fn scoped_store(name: &str, directories: &[&Path]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tapes-scoped-{name}"));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/08/09");
    fs::create_dir_all(&day).unwrap();
    for (index, directory) in directories.iter().enumerate() {
        let id = format!("00000000-0000-0000-0000-00000000000{index}");
        let path = day.join(format!("rollout-2026-08-09T00-00-0{index}-{id}.jsonl"));
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{directory}"}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#,
                    "\n"
                ),
                index = index,
                id = id,
                directory = directory.display()
            ),
        )
        .unwrap();
        // Candidates are walked in modification order, so the store's own
        // recency has to be real rather than incidental.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_800_000_000 + index as u64),
            )
            .unwrap();
    }
    root
}

fn filtered_store(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tapes-filtered-{name}"));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/08/09");
    fs::create_dir_all(&day).unwrap();
    let sessions = [
        ("wanted-a", "wanted-model", "/fixtures/wanted-project"),
        ("wanted-b", "wanted-model", "/fixtures/wanted-project"),
        ("other", "other-model", "/fixtures/other-project"),
    ];
    for (index, (id, model, directory)) in sessions.iter().enumerate() {
        let path = day.join(format!("rollout-2026-08-09T00-00-0{index}-{id}.jsonl"));
        let mut file = BufWriter::new(File::create(&path).unwrap());
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{directory}"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"turn_context","payload":{{"cwd":"{directory}","model":"{model}","effort":"low"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"hello"}}]}}}}"#
        )
        .unwrap();
        drop(file);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_800_000_000 + index as u64),
            )
            .unwrap();
    }
    root
}

#[test]
fn metadata_filters_are_case_insensitive_and_keep_absent_values_absent() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(fixtures("codex")))];

    let all = list_with_backends(&backends, Some("codex"), None, 10).unwrap();
    assert!(all
        .sessions
        .iter()
        .any(|session| session.id == "20000000-0000-0000-0000-000000000004"));
    assert!(all.sessions.iter().any(|session| {
        session.id == "20000000-0000-0000-0000-000000000004" && session.model.is_none()
    }));

    let models = list_with_backends_filtered(
        &backends,
        Some("codex"),
        None,
        10,
        Some("GPT-FIXTURE"),
        None,
    )
    .unwrap();
    assert_eq!(models.sessions.len(), 3);
    assert!(models
        .sessions
        .iter()
        .all(|session| session.model.is_some()));
    assert!(!models
        .sessions
        .iter()
        .any(|session| session.id == "30000000-0000-0000-0000-000000000005"));

    let high_variant = list_with_backends_filtered(
        &backends,
        Some("codex"),
        None,
        10,
        Some("GPT-FIXTURE (HIGH)"),
        None,
    )
    .unwrap();
    assert_eq!(
        high_variant
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "10000000-0000-0000-0000-000000000003",
            "00000000-0000-0000-0000-000000000001"
        ]
    );

    let directory = list_with_backends_filtered(
        &backends,
        Some("codex"),
        None,
        10,
        None,
        Some("OTHER-PROJECT"),
    )
    .unwrap();
    assert_eq!(directory.sessions.len(), 1);
    assert_eq!(
        directory.sessions[0].id,
        "30000000-0000-0000-0000-000000000005"
    );

    let composed = list_with_backends_filtered(
        &backends,
        Some("codex"),
        None,
        10,
        Some("OTHER-FIXTURE"),
        Some("other-project"),
    )
    .unwrap();
    assert_eq!(composed.sessions.len(), 1);
    assert_eq!(
        composed.sessions[0].id,
        "30000000-0000-0000-0000-000000000005"
    );

    let missing_model =
        list_with_backends_filtered(&backends, Some("codex"), None, 10, Some("anything"), None)
            .unwrap();
    assert!(!missing_model
        .sessions
        .iter()
        .any(|session| session.id == "20000000-0000-0000-0000-000000000004"));
}

#[test]
fn metadata_filters_fill_the_limit_after_rejecting_candidates() {
    let root = filtered_store("before-limit");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];

    let result =
        list_with_backends_filtered(&backends, Some("codex"), None, 2, Some("WANTED"), None)
            .unwrap();

    assert_eq!(
        result
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["wanted-b", "wanted-a"]
    );
    assert_eq!(result.scanned, 3);
    assert!(!result.scan_truncated);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_scoped_listing_finds_the_project_past_the_limit() {
    let mine = std::env::temp_dir().join("tapes-scoped-mine/nested");
    let theirs = std::env::temp_dir().join("tapes-scoped-theirs");
    fs::create_dir_all(&mine).unwrap();
    fs::create_dir_all(&theirs).unwrap();
    // The project's session is the older of the two, so a limit applied
    // before the scope would return the other one and report nothing.
    let root = scoped_store("listing", &[&mine, &theirs]);
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let scope = Scope::at(mine.parent().unwrap()).unwrap();

    let result = list_with_backends(&backends, None, Some(&scope), 1).unwrap();
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(
        result.sessions[0].directory.as_deref(),
        Some(mine.as_path())
    );
    assert_eq!(result.scanned, 2);
    assert!(!result.scan_truncated);

    let elsewhere = Scope::at(&theirs).unwrap();
    let result = list_with_backends(&backends, None, Some(&elsewhere), 10).unwrap();
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(
        result.sessions[0].directory.as_deref(),
        Some(theirs.as_path())
    );

    fs::remove_dir_all(root).unwrap();
}

/// The probe decides whether a candidate is skipped without parsing it, so a
/// disagreement between the probe's rule and the parse's rule is a lost
/// session rather than a slow one. Codex is where the two could diverge: past
/// the bounded read the tail carries only `turn_context`, whose `cwd` a
/// resumed session can have moved away from the `session_meta` the probe sees.
#[test]
fn the_probe_and_the_parse_place_a_session_in_the_same_directory() {
    let opening = std::env::temp_dir().join("tapes-scoped-opening");
    let later = std::env::temp_dir().join("tapes-scoped-later");
    fs::create_dir_all(&opening).unwrap();
    fs::create_dir_all(&later).unwrap();
    let root = std::env::temp_dir().join("tapes-store-moved");
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/08/09");
    fs::create_dir_all(&day).unwrap();

    let id = "00000000-0000-0000-0000-0000000000aa";
    let mut file = BufWriter::new(
        File::create(day.join(format!("rollout-2026-08-09T00-00-00-{id}.jsonl"))).unwrap(),
    );
    writeln!(
        file,
        r#"{{"timestamp":"2026-08-09T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"{}"}}}}"#,
        opening.display()
    )
    .unwrap();
    let filler = "y".repeat(4096);
    for index in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-09T00:00:01Z","type":"turn_context","payload":{{"cwd":"{}","model":"m-{index}"}}}}"#,
            later.display()
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-09T00:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let backend = CodexBackend::new(&root);
    let session = located(&backend, id);
    let parsed = backend.transcript(&session, 1).unwrap().session.directory;
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let scope = Scope::at(parsed.as_deref().expect("a directory was parsed")).unwrap();
    let result = list_with_backends(&backends, None, Some(&scope), 10).unwrap();
    assert_eq!(
        result.sessions.len(),
        1,
        "a scoped listing lost the session the parse places in that scope"
    );

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(opening).unwrap();
    fs::remove_dir_all(later).unwrap();
}

/// The cheap directory probe reads a fixed window of a file's opening. A
/// session whose first line is larger than that window is not placed by the
/// probe, and must still be placed by the full read — a probe that could hide
/// a session would be a wrong answer rather than a fast one.
#[test]
fn a_session_the_probe_cannot_place_is_still_found() {
    let project = std::env::temp_dir().join("tapes-scoped-unprobeable");
    fs::create_dir_all(&project).unwrap();
    let root = std::env::temp_dir().join("tapes-store-unprobeable");
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/08/09");
    fs::create_dir_all(&day).unwrap();

    let id = "00000000-0000-0000-0000-0000000000ff";
    let mut file = BufWriter::new(
        File::create(day.join(format!("rollout-2026-08-09T00-00-00-{id}.jsonl"))).unwrap(),
    );
    // One opening line wider than the probe window, so the `cwd` on it is
    // outside anything a bounded head read can see.
    writeln!(
        file,
        r#"{{"timestamp":"2026-08-09T00:00:00Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{}"}}]}}}}"#,
        "x".repeat(128 * 1024)
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"2026-08-09T00:00:01Z","type":"turn_context","payload":{{"cwd":"{}"}}}}"#,
        project.display()
    )
    .unwrap();
    drop(file);

    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let scope = Scope::at(&project).unwrap();
    let result = list_with_backends(&backends, None, Some(&scope), 10).unwrap();
    assert_eq!(result.sessions.len(), 1, "the probe hid a session in scope");

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(project).unwrap();
}

/// pi writes the working directory once, on its header line, and the bounded
/// read keeps only a transcript's tail. Past that window the header is gone
/// and nothing later repeats it, so a session this size would otherwise
/// normalize with no directory at all — invisible to every scoped listing.
#[test]
fn a_transcript_past_the_read_window_still_reports_its_directory() {
    let root = std::env::temp_dir().join("tapes-pi-oversized");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("2026-01-01T10-00-00-000Z_session-huge.jsonl");

    let mut file = BufWriter::new(File::create(&path).unwrap());
    writeln!(
        file,
        r#"{{"type":"session","version":3,"id":"session-huge","timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project"}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"message","id":"user-first","parentId":null,"timestamp":"2026-01-01T10:00:01Z","message":{{"role":"user","content":[{{"type":"text","text":"Open the oversized fixture."}}]}}}}"#
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for index in 0..1200 {
        writeln!(
            file,
            r#"{{"type":"message","id":"user-{index}","parentId":null,"timestamp":"2026-01-01T12:00:01Z","message":{{"role":"user","content":[{{"type":"text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);
    assert!(fs::metadata(&path).unwrap().len() > 4 * 1024 * 1024);

    let backend = PiBackend::new(&root);
    let sessions = backend.list(&Query::unscoped(10)).unwrap().sessions;
    let session = sessions
        .iter()
        .find(|session| session.id == "session-huge")
        .expect("an oversized session is still listed");
    assert_eq!(
        session.directory.as_deref(),
        Some(Path::new("/fixtures/project"))
    );
    assert_eq!(
        session.started_at,
        "2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        "the header's timestamp is the start, not the first retained tail entry"
    );
    assert!(
        session.derived_title.is_none(),
        "past the bound pi cannot prove which root the active path descends from"
    );
    assert_eq!(
        backend
            .transcript(session, 1)
            .unwrap()
            .session
            .directory
            .as_deref(),
        Some(Path::new("/fixtures/project"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_latest_in_scope_needs_no_id_and_honours_exclusions() {
    let project = std::env::temp_dir().join("tapes-scoped-latest-project");
    fs::create_dir_all(&project).unwrap();
    let root = scoped_store("latest", &[&project, &project]);
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let scope = Scope::at(&project).unwrap();

    let newest = latest_with_backends(&backends, None, Some(&scope), &[]).unwrap();
    assert_eq!(newest.session.id, "00000000-0000-0000-0000-000000000001");

    // What an agent asking from inside its own session passes.
    let previous = latest_with_backends(
        &backends,
        None,
        Some(&scope),
        std::slice::from_ref(&newest.session.id),
    )
    .unwrap();
    assert_eq!(previous.session.id, "00000000-0000-0000-0000-000000000000");

    let exhausted = latest_with_backends(
        &backends,
        None,
        Some(&scope),
        &[newest.session.id, previous.session.id],
    );
    assert!(exhausted.is_err());

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(project).unwrap();
}

struct SearchFixture {
    session: Session,
    requested_tail: Rc<Cell<usize>>,
    fail: bool,
    /// The read stops at a source bound before the searched tail is covered.
    bounded: bool,
}

impl Backend for SearchFixture {
    fn harness(&self) -> &'static str {
        "fixture"
    }

    fn available(&self) -> bool {
        true
    }

    fn list(&self, query: &Query) -> anyhow::Result<Listing> {
        assert_eq!(
            query.limit,
            usize::MAX,
            "content search must inspect candidates before applying --limit"
        );
        Ok(Listing::from_sessions(vec![self.session.clone()]))
    }

    fn locate(&self, id: &str) -> anyhow::Result<Option<Session>> {
        Ok((self.session.id == id).then(|| self.session.clone()))
    }

    fn transcript(&self, session: &Session, tail: usize) -> anyhow::Result<Transcript> {
        self.requested_tail.set(tail);
        if self.fail {
            anyhow::bail!("bounded search read failed");
        }
        Ok(Transcript {
            session: session.clone(),
            turns: vec![Turn {
                role: Role::User,
                text: "needle".into(),
                ts: None,
                ordinal: 0,
                native_id: None,
            }],
            truncated: self.bounded,
            truncation: Truncation {
                window: None,
                source: if self.bounded {
                    vec![SourceBound::FileTail {
                        bytes: 4 * 1024 * 1024,
                    }]
                } else {
                    Vec::new()
                },
            },
            trailing_record: None,
            notes: Vec::new(),
        })
    }
}

#[test]
fn list_search_uses_a_fixed_tail_before_applying_the_limit() {
    let requested_tail = Rc::new(Cell::new(0));
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(SearchFixture {
        session: resolver_session("search-session"),
        requested_tail: Rc::clone(&requested_tail),
        fail: false,
        bounded: false,
    })];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("fixture"),
        None,
        1,
        None,
        None,
        Some("NEEDLE"),
    )
    .unwrap();

    assert_eq!(result.sessions.len(), 1);
    assert_eq!(requested_tail.get(), LIST_SEARCH_TAIL);
    assert!(result.unsearched.is_empty());
}

#[test]
fn file_search_does_not_match_before_the_fixed_recent_turn_window() {
    let root = std::env::temp_dir().join(format!("tapes-search-window-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/08/09");
    fs::create_dir_all(&day).unwrap();
    let id = "00000000-0000-0000-0000-0000000000cc";
    let path = day.join(format!("rollout-2026-08-09T00-00-00-{id}.jsonl"));
    let mut file = BufWriter::new(File::create(&path).unwrap());
    writeln!(
        file,
        r#"{{"timestamp":"2026-08-09T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    for index in 0..33 {
        let text = match index {
            0 => "old needle",
            32 => "recent needle",
            _ => "filler",
        };
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-09T00:00:{index:02}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let backend: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let old = list_with_backends_filtered_and_search(
        &backend,
        Some("codex"),
        None,
        10,
        None,
        None,
        Some("old needle"),
    )
    .unwrap();
    assert!(old.sessions.is_empty());

    let recent = list_with_backends_filtered_and_search(
        &backend,
        Some("codex"),
        None,
        10,
        None,
        None,
        Some("RECENT NEEDLE"),
    )
    .unwrap();
    assert_eq!(recent.sessions.len(), 1);
    assert_eq!(recent.sessions[0].id, id);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_failed_bounded_search_is_reported_as_unsearched() {
    let requested_tail = Rc::new(Cell::new(0));
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(SearchFixture {
        session: resolver_session("unsearched-session"),
        requested_tail,
        fail: true,
        bounded: false,
    })];

    let result = list_with_backends_filtered_and_search(
        &backends,
        Some("fixture"),
        None,
        1,
        None,
        None,
        Some("needle"),
    )
    .unwrap();

    assert!(result.sessions.is_empty());
    assert_eq!(
        result.unsearched,
        vec!["fixture session unsearched-session: bounded search read failed"]
    );
}

#[derive(Clone, Default)]
struct ResolverFixture {
    sessions: Vec<Session>,
    /// Panic if `list` is called: proves an exact id never enumerates.
    forbid_list: bool,
    /// Fail `locate`: proves one broken backend cannot block another.
    locate_errors: bool,
}

impl ResolverFixture {
    fn with(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            ..Self::default()
        }
    }
}

impl Backend for ResolverFixture {
    fn harness(&self) -> &'static str {
        "fixture"
    }

    fn available(&self) -> bool {
        true
    }

    fn list(&self, query: &Query) -> anyhow::Result<Listing> {
        assert!(
            !self.forbid_list,
            "an exact id must not enumerate the store"
        );
        Ok(Listing::from_sessions(
            self.sessions.iter().take(query.limit).cloned().collect(),
        ))
    }

    fn locate(&self, id: &str) -> anyhow::Result<Option<Session>> {
        if self.locate_errors {
            anyhow::bail!("fixture locate is broken");
        }
        Ok(self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .cloned())
    }

    fn transcript(&self, _session: &Session, _tail: usize) -> anyhow::Result<Transcript> {
        unreachable!("resolver tests do not read transcripts")
    }
}

fn resolver_session(id: &str) -> Session {
    let timestamp = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    Session {
        id: id.into(),
        harness: "fixture".into(),
        model: None,
        title: None,
        derived_title: None,
        derived_title_truncated: None,
        directory: None,
        started_at: timestamp,
        last_activity_at: timestamp,
        live: None,
        cost: None,
        tokens: None,
        store: None,
        start_uncertain: false,
    }
}

#[test]
fn resolver_accepts_only_unambiguous_prefixes() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ResolverFixture::with(vec![
        resolver_session("ses_alpha"),
        resolver_session("ses_alpine"),
        resolver_session("ses_beta"),
    ]))];

    let resolved = resolve_session(&backends, "ses_b").unwrap();
    assert_eq!(resolved.session.id, "ses_beta");

    let error = resolve_session(&backends, "ses_al").unwrap_err();
    let ResolveError::Ambiguous { candidates, .. } = error else {
        panic!("expected ambiguous prefix");
    };
    assert_eq!(
        candidates
            .into_iter()
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>(),
        vec!["ses_alpha", "ses_alpine"]
    );
}

#[test]
fn transcript_reads_are_capped_at_four_megabytes() {
    let root = std::env::temp_dir().join(format!("tapes-bounds-{}", std::process::id()));
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let path = project.join("bounded.jsonl");
    let mut file = BufWriter::new(File::create(&path).unwrap());
    file.write_all(b"{\"padding\":\"").unwrap();
    file.write_all(&vec![b'x'; 4 * 1024 * 1024]).unwrap();
    file.write_all(b"\"}\n").unwrap();
    writeln!(
        file,
        r#"{{"type":"user","sessionId":"bounded","timestamp":"2026-01-01T12:00:00Z","cwd":"/fixtures/project","message":{{"role":"user","content":"Tail input."}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"type":"assistant","sessionId":"bounded","timestamp":"2026-01-01T12:00:01Z","cwd":"/fixtures/project","message":{{"role":"assistant","model":"claude-fixture","content":[{{"type":"text","text":"Tail output."}}]}}}}"#
    )
    .unwrap();
    file.flush().unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "bounded");
    let transcript = backend.transcript(&session, 10).unwrap();

    assert!(transcript.truncated);
    assert_eq!(transcript.turns.len(), 2);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn exact_id_resolves_without_enumerating_the_store() {
    // `list` panics, so resolution can only succeed through `locate`.
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ResolverFixture {
        sessions: vec![resolver_session("ses_alpha")],
        forbid_list: true,
        locate_errors: false,
    })];

    let resolved = resolve_session(&backends, "ses_alpha").unwrap();
    assert_eq!(resolved.session.id, "ses_alpha");
}

#[test]
fn exact_id_resolves_beyond_the_enumeration_limit() {
    // More sessions than resolution would ever list, with the target last.
    let mut sessions = (0..1_200)
        .map(|index| resolver_session(&format!("ses_filler{index:05}")))
        .collect::<Vec<_>>();
    sessions.push(resolver_session("ses_buried"));
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ResolverFixture {
        sessions,
        forbid_list: true,
        locate_errors: false,
    })];

    let resolved = resolve_session(&backends, "ses_buried").unwrap();
    assert_eq!(resolved.session.id, "ses_buried");
}

#[test]
fn a_broken_locate_does_not_prevent_another_backend_resolving() {
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(ResolverFixture {
            sessions: vec![resolver_session("ses_other")],
            forbid_list: false,
            locate_errors: true,
        }),
        Box::new(ResolverFixture::with(vec![resolver_session("ses_alpha")])),
    ];

    let resolved = resolve_session(&backends, "ses_alpha").unwrap();
    assert_eq!(resolved.session.id, "ses_alpha");
}

#[test]
fn an_unknown_id_is_still_not_found() {
    let backends: Vec<Box<dyn Backend>> =
        vec![Box::new(ResolverFixture::with(vec![resolver_session(
            "ses_alpha",
        )]))];

    let error = resolve_session(&backends, "ses_missing").unwrap_err();
    let ResolveError::NotFound { truncated, .. } = error else {
        panic!("expected not found");
    };
    assert!(!truncated, "a short store must not claim a capped search");
}

#[test]
fn two_backends_holding_one_exact_id_are_ambiguous() {
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(ResolverFixture::with(vec![resolver_session("shared-id")])),
        Box::new(ResolverFixture::with(vec![resolver_session("shared-id")])),
    ];

    let error = resolve_session(&backends, "shared-id").unwrap_err();
    let ResolveError::Ambiguous { candidates, .. } = error else {
        panic!("expected ambiguity across backends");
    };
    assert_eq!(candidates.len(), 2);
}

#[test]
fn a_capped_search_says_it_stopped_short() {
    // Enough sessions that enumeration hits the cap, and a query that matches
    // none of them, so the miss must admit the search was bounded.
    let sessions = (0..1_200)
        .map(|index| resolver_session(&format!("ses_filler{index:05}")))
        .collect::<Vec<_>>();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ResolverFixture::with(sessions))];

    let error = resolve_session(&backends, "nothing-matches-this").unwrap_err();
    let ResolveError::NotFound { truncated, .. } = error else {
        panic!("expected not found");
    };
    assert!(
        truncated,
        "a capped search must report that it stopped short"
    );
    assert!(error.to_string().contains("stopped at"));
}

#[test]
fn a_broken_opencode_session_call_surfaces_the_failure() {
    // A program that answers the listing but fails the single-session GET.
    // locate must propagate that, not disguise it as a missing session:
    // "the API is broken" and "no such session" send an operator to
    // different places.
    let program = broken_opencode_program("resolve");

    let backend = OpenCodeBackend::new(program.path());
    let error = backend
        .locate("ses_anything")
        .expect_err("a failing session call must be an error, not a miss");
    assert!(
        !error.to_string().contains("is unavailable"),
        "the real failure must survive, got: {error}"
    );
}

/// A program that answers the listing but fails or corrupts the single-session
/// GET, so `locate` errors while `list` succeeds empty — the exact arrangement
/// under which resolution used to return a bare NotFound.
fn broken_opencode_program(tag: &str) -> OpenCodeAlias {
    opencode_alias_program(&format!("broken-{tag}"))
}

#[test]
fn a_failing_opencode_call_surfaces_through_resolution_not_as_not_found() {
    let program = broken_opencode_program("resolve");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(program.path()))];

    let error = resolve_session(&backends, "ses_anything").unwrap_err();
    let ResolveError::BackendFailed { failures, .. } = error else {
        panic!("a broken backend must not read as a missing session");
    };
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "opencode");
}

#[test]
fn malformed_opencode_session_data_is_an_error_not_a_miss() {
    let program = broken_opencode_program("malformed");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(program.path()))];

    let error = resolve_session(&backends, "ses_anything").unwrap_err();
    assert!(
        matches!(error, ResolveError::BackendFailed { .. }),
        "a malformed session object is a failure, got: {error}"
    );
}

#[test]
fn an_absent_opencode_session_is_still_a_plain_miss() {
    // `data: null` is a well-formed answer meaning "no such session".
    let program = broken_opencode_program("absent");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(program.path()))];

    let error = resolve_session(&backends, "ses_anything").unwrap_err();
    assert!(
        matches!(error, ResolveError::NotFound { .. }),
        "an absent session must stay NotFound, got: {error}"
    );
}

#[test]
fn a_hit_elsewhere_still_wins_over_a_broken_backend() {
    let program = broken_opencode_program("hit-wins");
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(OpenCodeBackend::new(program.path())),
        Box::new(ResolverFixture::with(vec![resolver_session("ses_alpha")])),
    ];

    let resolved = resolve_session(&backends, "ses_alpha").unwrap();
    assert_eq!(resolved.session.id, "ses_alpha");
}

/// A file past the bounded tail loses its own opening, where every harness
/// writes the session header. The head probe keeps the header's facts — the
/// recorded start, the id, the directory, the first user turn — while the
/// tail still decides truncation and supplies the turns.
#[test]
fn oversized_codex_and_claude_files_keep_their_header_facts() {
    let root = std::env::temp_dir().join(format!("tapes-oversized-heads-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let codex_day = root.join("codex/2026/01/01");
    let claude_project = root.join("claude/-fixtures-project");
    fs::create_dir_all(&codex_day).unwrap();
    fs::create_dir_all(&claude_project).unwrap();
    let filler = "x".repeat(4096);

    let codex_id = "00000000-0000-0000-0000-00000000aaaa";
    let codex_path = codex_day.join(format!("rollout-2026-01-01T10-00-00-{codex_id}.jsonl"));
    let mut file = BufWriter::new(File::create(&codex_path).unwrap());
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{codex_id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Open the oversized rollout."}}]}}}}"#
    )
    .unwrap();
    for _ in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T12:00:00Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let claude_id = "0000aaaa-0000-0000-0000-000000000000";
    let claude_path = claude_project.join(format!("{claude_id}.jsonl"));
    let mut file = BufWriter::new(File::create(&claude_path).unwrap());
    writeln!(
        file,
        r#"{{"type":"user","sessionId":"{claude_id}","uuid":"u0","parentUuid":null,"timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","message":{{"role":"user","content":"Open the oversized transcript."}}}}"#
    )
    .unwrap();
    for index in 0..1200 {
        writeln!(
            file,
            r#"{{"type":"assistant","sessionId":"{claude_id}","uuid":"a{index}","parentUuid":"u0","timestamp":"2026-01-01T12:00:00Z","cwd":"/fixtures/project","message":{{"role":"assistant","model":"claude-fixture","content":[{{"type":"text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let cases: Vec<(Box<dyn Backend>, &str, &str, &str)> = vec![
        (
            Box::new(CodexBackend::new(root.join("codex"))),
            codex_id,
            "Open the oversized rollout.",
            "rollout",
        ),
        (
            Box::new(ClaudeBackend::new(root.join("claude"))),
            claude_id,
            "Open the oversized transcript.",
            "transcript",
        ),
    ];
    let start = "2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let end = "2026-01-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
    for (backend, id, first_turn, what) in cases {
        let session = located(backend.as_ref(), id);
        assert_eq!(session.id, id, "{what}: the header names the session");
        assert_eq!(
            session.started_at, start,
            "{what}: the header's timestamp is the start"
        );
        assert_eq!(
            session.last_activity_at, end,
            "{what}: the tail's newest timestamp is the end"
        );
        assert_eq!(
            session.directory.as_deref(),
            Some(Path::new("/fixtures/project")),
            "{what}: the header's directory survives"
        );
        assert_eq!(
            session.derived_title.as_deref(),
            Some(first_turn),
            "{what}: the opening's first user turn is the session's first"
        );

        let transcript = backend.transcript(&session, 5).unwrap();
        assert!(transcript.truncated, "{what}: the tail is still a window");
        assert_eq!(transcript.turns.len(), 5, "{what}: turns stay bounded");
        assert_eq!(transcript.session.started_at, start);
        let window = transcript
            .truncation
            .window
            .as_ref()
            .unwrap_or_else(|| panic!("{what}: a 5-turn window over more turns is a window"));
        assert_eq!(window.returned, 5, "{what}");
        assert_eq!(window.bound, 5, "{what}");
        assert!(window.omitted > 0, "{what}");
        assert_eq!(
            transcript.truncation.source,
            vec![SourceBound::FileTail {
                bytes: 4 * 1024 * 1024
            }],
            "{what}: the file bound is its own cause"
        );

        let wide = backend.transcript(&session, usize::MAX).unwrap();
        assert!(wide.truncation.window.is_none(), "{what}: nothing windowed");
        assert_eq!(
            wide.truncation.source.len(),
            1,
            "{what}: the file bound remains"
        );
        assert!(
            wide.truncated,
            "{what}: the derived flag follows the source bound"
        );

        let listed = backend.list(&Query::unscoped(10)).unwrap().sessions;
        assert_eq!(listed.len(), 1, "{what}: listing finds the session");
        assert_eq!(listed[0].started_at, start, "{what}: listing agrees");
    }

    fs::remove_dir_all(root).unwrap();
}

/// A whole fixture read through a smaller window is windowed and nothing else,
/// and the same read through a wide enough window is not truncated at all.
#[test]
fn a_window_is_the_only_cause_when_the_file_fits_the_reader() {
    for (backend, id) in fixture_backends() {
        let session = located(backend.as_ref(), id);
        let whole = backend.transcript(&session, usize::MAX).unwrap();
        assert!(!whole.truncated, "{}: whole fixture", backend.harness());
        assert!(whole.truncation.is_empty(), "{}", backend.harness());

        let windowed = backend.transcript(&session, 1).unwrap();
        assert!(windowed.truncated, "{}", backend.harness());
        let window = windowed.truncation.window.as_ref().unwrap();
        assert_eq!(window.returned, 1, "{}", backend.harness());
        assert_eq!(
            window.omitted,
            whole.turns.len() - 1,
            "{}",
            backend.harness()
        );
        assert_eq!(window.bound, 1, "{}", backend.harness());
        assert!(
            windowed.truncation.source.is_empty(),
            "{}",
            backend.harness()
        );
    }
}

/// Every backend numbers its turns densely over the normalized sequence, the
/// same way on every read, and names the store it read from. A native id is
/// present where the harness records one and absent, not empty, where it
/// does not.
#[test]
fn every_backend_gives_turns_stable_ordinals_and_names_its_store() {
    for (backend, id) in fixture_backends() {
        let harness = backend.harness();
        let session = located(backend.as_ref(), id);
        assert!(session.store.is_some(), "{harness}: store coordinate");

        let first = backend.transcript(&session, usize::MAX).unwrap();
        let second = backend.transcript(&session, usize::MAX).unwrap();
        assert_eq!(first.turns, second.turns, "{harness}: stable across reads");
        for (index, turn) in first.turns.iter().enumerate() {
            assert_eq!(turn.ordinal, index, "{harness}: dense ordinals");
        }

        let last = first.turns.len() - 1;
        let windowed = backend.transcript(&session, 1).unwrap();
        assert_eq!(
            windowed.turns[0].ordinal, last,
            "{harness}: window keeps ordinals"
        );
        assert_eq!(
            windowed.truncation.window.as_ref().unwrap().ordinals,
            Some(tapes_core::model::OrdinalRange { first: last, last }),
            "{harness}: the window names its ordinals"
        );

        let native_ids = first
            .turns
            .iter()
            .map(|turn| turn.native_id.as_deref())
            .collect::<Vec<_>>();
        if harness == "codex" {
            // Codex ids some response items (reasoning) and not others, so a
            // turn is named where its record was and left absent where not.
            assert!(
                native_ids.contains(&Some("reasoning-1")),
                "{harness}: {native_ids:?}"
            );
            assert!(native_ids.contains(&None), "{harness}: {native_ids:?}");
        } else {
            assert!(
                native_ids
                    .iter()
                    .all(|id| id.is_some_and(|id| !id.is_empty())),
                "{harness}: {native_ids:?}"
            );
        }
    }
}

/// A session whose whole message projection is larger than the transport
/// bound is still read: newest first, a page at a time, stopping once the
/// requested tail is in hand. The unpaged projection is never requested.
#[test]
fn an_oversized_opencode_session_is_read_in_pages() {
    let program = OpenCodeAlias::oversized();
    let backend = OpenCodeBackend::new(program.path());
    let session = located(&backend, "ses_oversized_fixture");

    let tail = backend.transcript(&session, 1).unwrap();
    assert_eq!(tail.turns.len(), 1);
    assert_eq!(tail.turns[0].text, "Page 1 message 0");
    assert!(tail.truncated);
    assert!(
        tail.truncation.source.is_empty(),
        "stopping with the window full is not a source bound: a wider request fetches more"
    );
    let window = tail.truncation.window.as_ref().unwrap();
    assert_eq!((window.returned, window.omitted), (1, 7));
    assert!(!window.omitted_exact, "only the fetched page is counted");

    let whole = backend.transcript(&session, usize::MAX).unwrap();
    assert_eq!(whole.turns.len(), 100, "two full pages and one short page");
    assert!(whole.truncation.source.is_empty());
    assert!(!whole.truncated);
    assert_eq!(whole.turns[0].text, "Ask page 2 message 49");
    assert_eq!(whole.turns[99].text, "Page 1 message 0");

    assert!(backend.search(&session, "PAGE 1 MESSAGE 3", 32).unwrap());
    assert!(!backend.search(&session, "page 2 message 49", 32).unwrap());

    let calls = fs::read_to_string(program.calls()).unwrap();
    let message_requests = calls
        .lines()
        .filter(|line| line.contains("/message"))
        .collect::<Vec<_>>();
    assert!(!message_requests.is_empty(), "{calls}");
    assert!(
        message_requests.iter().all(|line| line.contains("limit=")),
        "an unpaged message request was made: {calls}"
    );
    assert!(
        message_requests
            .iter()
            .filter(|line| line.contains("cursor="))
            .all(|line| !line.contains("order=")),
        "a cursor page carried an order: {calls}"
    );
}

/// One message the transport cannot carry does not make the session
/// unreadable: the read hands over every newer message, names the bound it
/// stopped at, and says why.
#[test]
fn a_giant_message_stops_the_paged_read_before_it_with_a_note() {
    let program = OpenCodeAlias::oversized();
    let backend = OpenCodeBackend::new(program.path());
    let session = located(&backend, "ses_giant_message_fixture");

    let whole = backend.transcript(&session, usize::MAX).unwrap();
    assert_eq!(whole.turns.len(), 100, "both readable pages");
    assert!(whole.truncated);
    assert_eq!(
        whole.truncation.source,
        vec![SourceBound::RecordPage {
            records: 100,
            of: "messages".to_owned()
        }]
    );
    assert_eq!(whole.notes.len(), 1, "{:?}", whole.notes);
    assert!(
        whole.notes[0].contains("older than the 100 fetched")
            && whole.notes[0].contains("8 MiB transport bound"),
        "{:?}",
        whole.notes
    );

    let tail = backend.transcript(&session, 1).unwrap();
    assert_eq!(tail.turns.len(), 1);
    assert!(tail.notes.is_empty(), "the giant message was never reached");

    let calls = fs::read_to_string(program.calls()).unwrap();
    let attempts = calls
        .lines()
        .filter(|line| line.contains("cursor=3&") || line.ends_with("cursor=3"))
        .count();
    assert!(
        attempts >= 2,
        "the oversized page is retried smaller before the read gives up on it: {calls}"
    );
}

/// Codex writes cumulative totals after every response. The newest event in
/// the read window is the session's accounting; a counter the newest event
/// did not write is absent even when an older event carried it, a counter it
/// wrote as zero is zero, and an event without usage is passed over.
#[test]
fn codex_reads_cumulative_token_totals_from_the_latest_event_with_usage() {
    let backend = CodexBackend::new(fixtures("codex"));
    let session = located(&backend, "00000000-0000-0000-0000-000000000001");
    assert_eq!(
        session.tokens,
        Some(tapes_core::model::Tokens {
            input: Some(1200),
            output: Some(300),
            reasoning: None,
            cache_read: Some(1000),
            cache_write: Some(0),
        })
    );
    assert!(session.cost.is_none(), "Codex records no cost");

    let transcript = backend.transcript(&session, 10).unwrap();
    assert_eq!(transcript.session.tokens, session.tokens);
    assert_eq!(transcript.turns.len(), 5, "token events are not turns");

    let without = located(&backend, "20000000-0000-0000-0000-000000000004");
    assert!(without.tokens.is_none(), "no token event, no counters");
}

/// A read that reached fewer turns than the searched tail because of a source
/// bound cannot say the needle is absent; the session is unsearched, not a
/// non-match. A hit inside the reached turns is still a match.
#[test]
fn a_source_bounded_read_short_of_the_tail_is_unsearched_not_a_non_match() {
    let requested_tail = Rc::new(Cell::new(0));
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(SearchFixture {
        session: resolver_session("bounded-session"),
        requested_tail,
        fail: false,
        bounded: true,
    })];

    let miss = list_with_backends_filtered_and_search(
        &backends,
        Some("fixture"),
        None,
        1,
        None,
        None,
        Some("absent"),
    )
    .unwrap();
    assert!(miss.sessions.is_empty());
    assert_eq!(miss.unsearched.len(), 1, "{:?}", miss.unsearched);
    assert!(
        miss.unsearched[0].contains("reached 1 of the last 32 turns"),
        "{:?}",
        miss.unsearched
    );

    let hit = list_with_backends_filtered_and_search(
        &backends,
        Some("fixture"),
        None,
        1,
        None,
        None,
        Some("needle"),
    )
    .unwrap();
    assert_eq!(hit.sessions.len(), 1);
    assert!(hit.unsearched.is_empty());
}

/// The message ceiling is exact: a store with more messages than the ceiling
/// yields exactly that many, whatever page size the read had grown back to.
#[test]
fn the_paged_read_stops_exactly_at_the_message_ceiling() {
    let program = OpenCodeAlias::oversized();
    let backend = OpenCodeBackend::new(program.path());
    let session = located(&backend, "ses_endless_fixture");

    let whole = backend.transcript(&session, usize::MAX).unwrap();
    assert_eq!(whole.turns.len(), 1000);
    assert_eq!(
        whole.truncation.source,
        vec![SourceBound::RecordPage {
            records: 1000,
            of: "messages".to_owned()
        }]
    );
    assert!(whole.truncation.window.is_none());
}

/// A first line longer than the head probe grows the probe rather than
/// emptying the opening, so the header behind it still supplies the start.
#[test]
fn a_first_line_longer_than_the_head_probe_still_yields_the_header() {
    let root = std::env::temp_dir().join(format!("tapes-long-first-line-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/01/01");
    fs::create_dir_all(&day).unwrap();
    let id = "00000000-0000-0000-0000-00000000dddd";
    let path = day.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut file = BufWriter::new(File::create(&path).unwrap());
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project","note":"{}"}}}}"#,
        "h".repeat(70 * 1024)
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for _ in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T12:00:00Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let backend = CodexBackend::new(&root);
    let session = located(&backend, id);
    assert_eq!(
        session.started_at,
        "2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap()
    );
    assert_eq!(
        session.directory.as_deref(),
        Some(Path::new("/fixtures/project"))
    );
    fs::remove_dir_all(root).unwrap();
}

/// A first line longer than the head probe's ceiling leaves the opening
/// empty; the start is then the earliest record reached and the session says
/// so, rather than presenting that record as the recorded start.
#[test]
fn a_first_line_past_the_probe_ceiling_marks_the_start_uncertain() {
    let root = std::env::temp_dir().join(format!("tapes-huge-first-line-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/01/01");
    fs::create_dir_all(&day).unwrap();
    let id = "00000000-0000-0000-0000-00000000eeee";
    let path = day.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut file = BufWriter::new(File::create(&path).unwrap());
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project","note":"{}"}}}}"#,
        "h".repeat(1100 * 1024)
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for _ in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T12:00:00Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let backend = CodexBackend::new(&root);
    let session = located(&backend, id);
    assert!(session.start_uncertain);
    assert_eq!(
        session.started_at,
        "2026-01-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap(),
        "the earliest record reached"
    );
    let value = serde_json::to_value(&session).unwrap();
    assert_eq!(value["start_uncertain"], true);

    let certain = located(&backend, id);
    let mut certain_value = serde_json::to_value(&certain).unwrap();
    certain_value["start_uncertain"] = serde_json::Value::Bool(false);
    let decoded: Session = serde_json::from_value(certain_value).unwrap();
    assert!(!decoded.start_uncertain);
    assert!(
        serde_json::to_value(&decoded)
            .unwrap()
            .get("start_uncertain")
            .is_none(),
        "omitted when false"
    );
    fs::remove_dir_all(root).unwrap();
}

/// The fast JSONL search path applies the same rule as the bounded read: a
/// file past the reader's bound whose retained tail holds fewer turns than
/// the search covers is unsearched on a miss, and still a match on a hit.
#[test]
fn file_search_short_of_the_tail_behind_the_file_bound_is_unsearched() {
    let root = std::env::temp_dir().join(format!("tapes-search-file-bound-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/01/01");
    fs::create_dir_all(&day).unwrap();
    let id = "00000000-0000-0000-0000-00000000ffff";
    let path = day.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut file = BufWriter::new(File::create(&path).unwrap());
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:01Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"the older needle"}}]}}}}"#
    )
    .unwrap();
    let huge = "x".repeat(3 * 1024 * 1024);
    for index in 0..2 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T10:00:0{}Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"retained {index} {huge}"}}]}}}}"#,
            index + 2
        )
        .unwrap();
    }
    drop(file);

    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let miss = list_with_backends_filtered_and_search(
        &backends,
        Some("codex"),
        None,
        10,
        None,
        None,
        Some("older needle"),
    )
    .unwrap();
    assert!(miss.sessions.is_empty());
    assert_eq!(miss.unsearched.len(), 1, "{:?}", miss.unsearched);
    assert!(
        miss.unsearched[0].contains(id) && miss.unsearched[0].contains("of the last 32 turns"),
        "{:?}",
        miss.unsearched
    );

    let hit = list_with_backends_filtered_and_search(
        &backends,
        Some("codex"),
        None,
        10,
        None,
        None,
        Some("retained 1"),
    )
    .unwrap();
    assert_eq!(hit.sessions.len(), 1);
    assert!(hit.unsearched.is_empty());

    // A candidate the metadata filters exclude was never asked about, so it
    // is neither returned nor reported as unsearched.
    let filtered_out = list_with_backends_filtered_and_search(
        &backends,
        Some("codex"),
        None,
        10,
        Some("some-other-model"),
        None,
        Some("older needle"),
    )
    .unwrap();
    assert!(filtered_out.sessions.is_empty());
    assert!(
        filtered_out.unsearched.is_empty(),
        "{:?}",
        filtered_out.unsearched
    );
    fs::remove_dir_all(root).unwrap();
}
