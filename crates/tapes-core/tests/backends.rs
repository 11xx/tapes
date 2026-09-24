use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use chrono::{DateTime, TimeZone, Utc};
use tapes_core::backend::claude::ClaudeBackend;
use tapes_core::backend::codex::CodexBackend;
use tapes_core::backend::opencode::OpenCodeBackend;
use tapes_core::backend::pi::PiBackend;
use tapes_core::backend::{Backend, Listing, Query, StreamedTranscript, MIN_READ_BYTES};
use tapes_core::event::{project, EventKind, Incomplete};
use tapes_core::model::{
    Accounting, AccountingBasis, AccountingCoverage, Cost, ReadEvidence, Role, Session,
    SourceBound, SourceDescriptor, Tokens, Transcript, Truncation, Turn, TurnKind,
};
use tapes_core::usage::{
    usage, Durations, ModelUsage, ObservationBasis, ObservationClassification, RateWindow,
    TurnCoverage, UsageObservationOptions, UsageOptions,
};
use tapes_core::{
    export_selection_with_backends, export_with_backends, latest_with_backends, list_with_backends,
    list_with_backends_filtered, list_with_backends_filtered_and_search,
    list_with_backends_options, resolve_session, scope::Scope, show_with_backends,
    stats_full_with_backends, stats_with_backends, usage_full_with_backends,
    usage_with_options_with_backends, ExportRead, ListFilters, ListSort, ResolveError, Selection,
    SessionSelection, Where, EXPORT_MANIFEST_SCHEMA, LIST_SEARCH_TAIL,
};

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(harness)
}

fn configure_opencode_fixture_store() {
    static XDG: OnceLock<PathBuf> = OnceLock::new();
    let root = XDG.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("tapes-opencode-xdg-{}", std::process::id()));
        let opencode = root.join("opencode");
        fs::create_dir_all(&opencode).unwrap();
        File::create(opencode.join("opencode.db")).unwrap();
        std::env::set_var("XDG_DATA_HOME", &root);
        root
    });
    std::env::set_var("XDG_DATA_HOME", root);
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
    configure_opencode_fixture_store();
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
        configure_opencode_fixture_store();
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

    fn serve_failed() -> Self {
        Self::new("serve-failed")
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

    for name in [
        "opencode2",
        "opencode-malformed-row",
        "opencode-unreadable-rows",
        "opencode-quoted-rows",
        "opencode-list-fails",
    ] {
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
        assert_eq!(listed.harness(), backend.harness());
        assert!(listed.started_at <= listed.last_activity_at);
        if listed.title.is_none() {
            assert_eq!(
                listed.derived_title.as_deref(),
                Some("Inspect the fixture.")
            );
        } else {
            assert!(listed.derived_title.is_none());
        }

        let transcript = backend.transcript(listed, usize::MAX).unwrap();
        assert_eq!(transcript.session, *listed);
        assert!(transcript.turns.len() >= 2);
        assert_eq!(transcript.turns.first().unwrap().role, Role::User);
        assert_eq!(transcript.turns.last().unwrap().role, Role::Tool);
        assert!(transcript
            .turns
            .iter()
            .any(|turn| turn.role == Role::Assistant));
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
    assert_eq!(result.unsearched.len(), 1, "{:?}", result.unsearched);
    assert!(result.unsearched[0].contains("opencode v2 search could not use the local API server"));
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
fn opencode_projections_mark_recorded_totals_as_whole_session_accounting() {
    let api = OpenCodeBackend::new(opencode_fixture_program());
    let api_session = located(&api, "ses_api_only_fixture");
    assert_eq!(
        api_session.accounting,
        Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        })
    );

    let database = OpenCodeAlias::database();
    let database_backend = OpenCodeBackend::new(database.path());
    let database_session = located(&database_backend, "ses_database_only_fixture");
    assert_eq!(
        database_session.accounting,
        Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        })
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
    assert_eq!(result.unsearched.len(), 1, "{:?}", result.unsearched);
    assert!(result.unsearched[0].contains("opencode v2 search could not use the local API server"));
}

/// OpenCode cannot promise a second read the first read's turns, so a
/// consumer that joins two passes refuses before reading any message.
#[test]
fn opencode_refuses_a_two_pass_read_before_reading_a_message() {
    let api = OpenCodeAlias::counting();
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(OpenCodeBackend::new(api.path()))];
    let id = "ses_000000fixtureSharedSession";
    let directory =
        std::env::temp_dir().join(format!("tapes-opencode-two-pass-{}", std::process::id()));

    let stats = stats_full_with_backends(&backends, Selection::Id(id)).err();
    let export = export_with_backends(
        &backends,
        Selection::Id(id),
        Some(&directory),
        None,
        ExportRead::Whole,
    )
    .err();
    for error in [stats, export] {
        let error = error.expect("a two-pass OpenCode read refuses").to_string();
        assert!(error.contains("cannot be read whole twice"), "{error}");
    }
    let calls = fs::read_to_string(api.calls.as_ref().unwrap()).unwrap_or_default();
    assert!(
        !calls.lines().any(|path| path.contains("/message")),
        "{calls}"
    );
    assert!(!directory.exists());
}

#[test]
fn opencode_v2_search_reports_a_failed_local_server_fallback() {
    let api = OpenCodeAlias::serve_failed();
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
    assert_eq!(result.unsearched.len(), 1, "{:?}", result.unsearched);
    assert!(result.unsearched[0].contains("opencode v2 search could not use the local API server"));
    assert!(result.unsearched[0].contains("startup"));
    assert!(
        result.unsearched[0].contains("searched through the CLI instead"),
        "{:?}",
        result.unsearched
    );
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
    assert_eq!(result.unsearched.len(), 1, "{:?}", result.unsearched);
    assert!(result.unsearched[0].contains("opencode v2 search could not use the local API server"));
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
fn an_explicit_claude_root_is_authoritative_and_a_missing_root_is_absent() {
    let root =
        std::env::temp_dir().join(format!("tapes-claude-explicit-root-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::copy(
        fixtures("claude").join("project/session-claude.jsonl"),
        project.join("session-claude.jsonl"),
    )
    .unwrap();

    let backend = ClaudeBackend::new(&root);
    assert!(backend.available());
    assert_eq!(
        backend.list(&Query::unscoped(10)).unwrap().sessions.len(),
        1
    );
    assert_eq!(located(&backend, "session-claude").id, "session-claude");

    let missing = ClaudeBackend::new(root.join("missing"));
    assert!(!missing.available());
    assert!(missing
        .list(&Query::unscoped(10))
        .unwrap()
        .sessions
        .is_empty());
    assert!(missing.locate("session-claude").unwrap().is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn every_file_backend_preserves_reasoning_and_tool_chronology() {
    for (backend, id) in fixture_backends() {
        let session = located(backend.as_ref(), id);
        let transcript = backend.transcript(&session, usize::MAX).unwrap();
        let roles = transcript
            .turns
            .iter()
            .map(|turn| turn.role.clone())
            .collect::<Vec<_>>();

        let expected = match backend.harness() {
            "opencode" => vec![
                Role::User,
                Role::Reasoning,
                Role::Tool,
                Role::Assistant,
                Role::Tool,
            ],
            // The claude fixture closes its exchange with the commands and
            // notices its harness records in the user envelope, then spawns a
            // subagent and leaves one call open.
            "claude" => {
                let mut roles = vec![
                    Role::User,
                    Role::Reasoning,
                    Role::Tool,
                    Role::Tool,
                    Role::Assistant,
                ];
                roles.extend((0..8).map(|_| Role::User));
                roles.extend([Role::Tool, Role::Tool, Role::Tool]);
                roles
            }
            _ => vec![
                Role::User,
                Role::Reasoning,
                Role::Tool,
                Role::Tool,
                Role::Assistant,
                Role::Tool,
            ],
        };
        assert_eq!(roles, expected, "{} chronology differs", backend.harness());
        assert_eq!(transcript.turns[1].text, "Consider the fixture.");
        assert!(transcript.turns[2].text.contains("fixture_tool"));
        assert!(transcript
            .turns
            .iter()
            .any(|turn| turn.text.contains("Tool complete.")));
        assert!(transcript
            .turns
            .last()
            .unwrap()
            .text
            .contains("fixture_pending"));
    }
}

#[test]
fn every_backend_exposes_one_complete_pair_and_one_incomplete_call() {
    // The claude fixture also spawns a subagent, whose call and result are a
    // second complete pair.
    let cases = [
        ("claude", "tool-1", "tool-pending", 5, 2),
        ("codex", "call-1", "call-pending", 3, 1),
        (
            "opencode",
            "fixture_tool_call",
            "fixture_tool_pending",
            3,
            1,
        ),
        ("pi", "tool-1", "tool-pending", 3, 1),
    ];
    for ((backend, id), (harness, paired, pending, event_count, complete_pairs)) in
        fixture_backends().into_iter().zip(cases)
    {
        assert_eq!(backend.harness(), harness);
        let session = located(backend.as_ref(), id);
        let transcript = backend.transcript(&session, usize::MAX).unwrap();
        let session_json = serde_json::to_value(&transcript).unwrap();
        assert!(session_json["turns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|turn| turn.get("tool").is_none()));

        let events = backend.events(&session, usize::MAX).unwrap();
        assert_eq!(
            events.events.len(),
            event_count,
            "{harness}: {:#?}",
            events.events
        );
        assert_eq!(events.pairs.complete, complete_pairs, "{harness}");
        assert_eq!(events.pairs.incomplete, 1, "{harness}");
        let call = events
            .events
            .iter()
            .find(|event| {
                event.event.call_id.as_deref() == Some(paired)
                    && event.event.kind == EventKind::ToolCall
            })
            .unwrap();
        assert!(call.pair.is_some(), "{harness}");
        assert!(call.duration_ms.is_some(), "{harness}");
        assert!(call.event.arguments.is_some(), "{harness}");
        let result = events
            .events
            .iter()
            .find(|event| {
                event.event.call_id.as_deref() == Some(paired)
                    && event.event.kind == EventKind::ToolResult
            })
            .unwrap();
        assert!(result.event.output.is_some(), "{harness}");
        let pending = events
            .events
            .iter()
            .find(|event| event.event.call_id.as_deref() == Some(pending))
            .unwrap();
        assert_eq!(pending.incomplete, Some(Incomplete::NoResultInRead));
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

/// The parent names its subagent: the meta record beside the transcript says
/// which agent it was, and the `Agent` call in the parent says when it was
/// spawned and how it ended.
/// pi records the reference on the child, and a reference to a session the
/// store does not hold is kept rather than dropped.
#[test]
fn pi_keeps_an_unresolved_parent_reference() {
    let backend = PiBackend::new(fixtures("pi"));
    let session = located(&backend, "session-pi");
    let lineage = backend.lineage(&session).unwrap();

    let parent = lineage.parent.as_ref().unwrap();
    assert_eq!(parent.native_id, "session-pi-parent");
    assert!(!parent.resolved, "the fixture store holds no such session");
    assert_eq!(parent.source, "session.parentSession");
    assert!(lineage.children.is_empty());
    assert!(lineage.forked_from.is_none());
}

/// Each OpenCode projection records the relationship on the child's own row,
/// so a parent's children are the rows naming it.
#[test]
fn opencode_reads_parent_and_child_rows_from_both_projections() {
    let database = OpenCodeAlias::database();
    let backend = OpenCodeBackend::new(database.path());
    let child = located(&backend, "ses_database_only_fixture");
    let lineage = backend.lineage(&child).unwrap();
    let parent = lineage.parent.as_ref().unwrap();
    assert_eq!(parent.native_id, "ses_000000fixtureSharedSession");
    assert!(parent.resolved);
    assert_eq!(parent.source, "session.parent_id");
    assert_eq!(lineage.forked_from.as_deref(), Some("ses_fork_fixture"));
    assert!(lineage.children.is_empty());

    let parent_session = located(&backend, "ses_000000fixtureSharedSession");
    let lineage = backend.lineage(&parent_session).unwrap();
    assert!(lineage.parent.is_none());
    assert!(lineage.forked_from.is_none());
    assert_eq!(lineage.children.len(), 1, "{:#?}", lineage.children);
    let child = &lineage.children[0];
    assert_eq!(child.reference, "ses_database_only_fixture");
    assert_eq!(
        child.session_id.as_deref(),
        Some("ses_database_only_fixture")
    );
    assert_eq!(child.role.as_deref(), Some("build"));
    assert_eq!(child.model.as_deref(), Some("fixture-db-model (balanced)"));
    assert!(child.resolved);

    let api = OpenCodeBackend::new(opencode_fixture_program());
    let child = located(&api, "ses_api_only_fixture");
    let lineage = api.lineage(&child).unwrap();
    let parent = lineage.parent.as_ref().unwrap();
    assert_eq!(parent.native_id, "ses_000000fixtureSharedSession");
    assert!(parent.resolved);
    assert_eq!(parent.source, "session.parentID");

    let parent_session = located(&api, "ses_000000fixtureSharedSession");
    let lineage = api.lineage(&parent_session).unwrap();
    assert_eq!(lineage.children.len(), 1, "{:#?}", lineage.children);
    let child = &lineage.children[0];
    assert_eq!(child.reference, "ses_api_only_fixture");
    assert_eq!(child.role.as_deref(), Some("build"));
    assert_eq!(child.model.as_deref(), Some("fixture-api-model"));
    assert!(child.resolved);
}

/// A Codex child is an ordinary rollout, joined to its parent by the agent
/// path in the parent's outputs and the parent id in the child's header.
#[test]
fn codex_joins_a_child_rollout_to_the_spawn_that_named_it() {
    let root = std::env::temp_dir().join(format!(
        "tapes-codex-lineage-{}-{}",
        std::process::id(),
        OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let sessions = root.join("2026/01/01");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&sessions).unwrap();
    let parent_id = "00000000-0000-0000-0000-0000000000a1";
    let child_id = "00000000-0000-0000-0000-0000000000b2";
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{parent_id}.jsonl")),
        [
            format!(r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{parent_id}","cwd":"/fixtures/project","forked_from_id":"00000000-0000-0000-0000-0000000000c3"}}}}"#),
            r#"{"timestamp":"2026-01-01T10:00:01Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","call_id":"call-spawn-1","arguments":"{\"task_name\":\"backend_workhorse\",\"model\":\"gpt-fixture\",\"message\":\"Do the work.\"}"}}"#.to_owned(),
            r#"{"timestamp":"2026-01-01T10:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-spawn-1","output":"{\"task_name\":\"/root/backend_workhorse\"}"}}"#.to_owned(),
            r#"{"timestamp":"2026-01-01T10:00:03Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","call_id":"call-spawn-2","arguments":"{\"task_name\":\"absent_worker\"}"}}"#.to_owned(),
            r#"{"timestamp":"2026-01-01T10:00:04Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-spawn-2","output":"{\"task_name\":\"/root/absent_worker\"}"}}"#.to_owned(),
            r#"{"timestamp":"2026-01-01T10:00:05Z","type":"response_item","payload":{"type":"function_call","name":"wait_agent","call_id":"call-wait-1","arguments":"{}"}}"#.to_owned(),
            r#"{"timestamp":"2026-01-01T10:00:06Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-wait-1","output":"{\"agents\":[{\"agent_name\":\"/root/backend_workhorse\",\"agent_status\":\"completed\"},{\"agent_name\":\"/root/absent_worker\",\"agent_status\":\"running\"}]}"}}"#.to_owned(),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-01-{child_id}.jsonl")),
        format!(
            r#"{{"timestamp":"2026-01-01T10:00:01Z","type":"session_meta","payload":{{"id":"{child_id}","cwd":"/fixtures/project","thread_source":"subagent","parent_thread_id":"{parent_id}","agent_nickname":"backend_workhorse","agent_path":"/root/backend_workhorse","multi_agent_version":"v2"}}}}
{{"timestamp":"2026-01-01T10:00:02Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"child-only-text"}}]}}}}
"#
        ),
    )
    .unwrap();

    let backend = CodexBackend::new(&root);
    let parent = located(&backend, parent_id);
    let lineage = backend.lineage(&parent).unwrap();

    assert!(lineage.parent.is_none());
    assert_eq!(
        lineage.forked_from.as_deref(),
        Some("00000000-0000-0000-0000-0000000000c3")
    );
    let children = &lineage.children;
    assert_eq!(children.len(), 2, "{children:#?}");
    let joined = &children[0];
    assert_eq!(joined.reference, "/root/backend_workhorse");
    assert_eq!(joined.session_id.as_deref(), Some(child_id));
    assert!(joined.resolved);
    assert_eq!(joined.role.as_deref(), Some("backend_workhorse"));
    assert_eq!(joined.model.as_deref(), Some("gpt-fixture"));
    assert_eq!(joined.group.as_deref(), Some("/root"));
    assert_eq!(joined.disposition.as_deref(), Some("completed"));
    assert_eq!(
        joined.completed_at,
        Some("2026-01-01T10:00:06Z".parse::<DateTime<Utc>>().unwrap())
    );
    // A task name no recording answers to stays a child, unresolved.
    let dangling = &children[1];
    assert_eq!(dangling.reference, "/root/absent_worker");
    assert!(!dangling.resolved);
    assert!(dangling.session_id.is_none());
    assert_eq!(dangling.disposition.as_deref(), Some("running"));
    assert!(dangling.completed_at.is_none());

    // The parent refers to the child and never absorbs it: nothing the child
    // recorded as a turn reaches the parent's lineage.
    assert!(
        !serde_json::to_string(&lineage)
            .unwrap()
            .contains("child-only-text"),
        "{lineage:#?}"
    );

    // The child names the parent, and listing the store still returns exactly
    // its two recordings.
    let child = located(&backend, child_id);
    let child_lineage = backend.lineage(&child).unwrap();
    let parent_ref = child_lineage.parent.as_ref().unwrap();
    assert_eq!(parent_ref.native_id, parent_id);
    assert!(parent_ref.resolved);
    assert_eq!(parent_ref.source, "session_meta.parent_thread_id");
    assert!(child_lineage.children.is_empty());

    let listed = backend.list(&Query::unscoped(10)).unwrap().sessions;
    assert_eq!(listed.len(), 2, "{listed:#?}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn claude_joins_a_subagent_transcript_to_the_call_that_spawned_it() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = located(&backend, "session-claude");
    let lineage = backend.lineage(&session).unwrap();

    assert!(lineage.parent.is_none());
    assert!(lineage.forked_from.is_none());
    assert_eq!(lineage.children.len(), 1, "{:#?}", lineage.children);
    let child = &lineage.children[0];
    assert_eq!(child.reference, "fixture");
    assert_eq!(child.harness, "claude");
    assert_eq!(child.role.as_deref(), Some("Explore"));
    assert_eq!(child.model.as_deref(), Some("sonnet"));
    assert_eq!(child.disposition.as_deref(), Some("completed"));
    assert!(child.resolved);
    assert_eq!(
        child.spawned_at,
        Some("2026-01-01T10:00:03.900Z".parse::<DateTime<Utc>>().unwrap())
    );
    assert_eq!(
        child.completed_at,
        Some("2026-01-01T10:00:03.950Z".parse::<DateTime<Utc>>().unwrap())
    );
    assert!(
        child.session_id.is_none(),
        "a subagent file is not a session"
    );
    let sources = serde_json::to_value(&child.source).unwrap();
    assert!(
        sources
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("subagents/agent-fixture.meta.json"))),
        "{sources}"
    );
    assert!(
        sources
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["native_id"] == "assistant-agent"),
        "{sources}"
    );
}

/// A spawn whose transcript is not in the store is still a child. Dropping it
/// would hide exactly the case a reader is looking for.
#[test]
fn claude_keeps_a_spawn_whose_transcript_is_absent() {
    let root = std::env::temp_dir().join(format!(
        "tapes-claude-unresolved-agent-{}-{}",
        std::process::id(),
        OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let project = root.join("-fixtures-project");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("session-absent-agent.jsonl"),
        concat!(
            r#"{"type":"user","sessionId":"session-absent-agent","uuid":"user-1","timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","message":{"role":"user","content":"Spawn a subagent."}}"#,
            "
",
            r#"{"type":"assistant","sessionId":"session-absent-agent","uuid":"assistant-1","timestamp":"2026-01-01T10:00:01Z","cwd":"/fixtures/project","message":{"role":"assistant","model":"claude-fixture","content":[{"type":"tool_use","id":"tool-agent-9","name":"Agent","input":{"subagent_type":"Explore","description":"Look around."}}]}}"#,
            "
",
            r#"{"type":"user","sessionId":"session-absent-agent","uuid":"tool-result-1","timestamp":"2026-01-01T10:00:02Z","cwd":"/fixtures/project","toolUseResult":"{\"isAsync\":true,\"status\":\"async_launched\",\"agentId\":\"absent-agent\",\"resolvedModel\":\"claude-fixture-sonnet\"}","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-agent-9","content":"Launched."}]}}"#,
            "
"
        ),
    )
    .unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "session-absent-agent");
    let lineage = backend.lineage(&session).unwrap();

    assert_eq!(lineage.children.len(), 1, "{:#?}", lineage.children);
    let child = &lineage.children[0];
    assert_eq!(child.reference, "absent-agent");
    assert!(!child.resolved);
    assert_eq!(child.disposition.as_deref(), Some("async_launched"));
    assert_eq!(child.role.as_deref(), Some("Explore"));
    assert_eq!(child.model.as_deref(), Some("claude-fixture-sonnet"));
    // A launch is not an ending, so nothing says the child finished.
    assert!(child.completed_at.is_none());
    fs::remove_dir_all(root).unwrap();
}

/// Claude writes a `toolUseResult` beside every tool's result, so what makes
/// one an agent is the `Agent` call it answers or the agent it names itself.
#[test]
fn claude_reads_an_agent_result_without_reading_every_tool_result() {
    let root = std::env::temp_dir().join(format!(
        "tapes-claude-agent-result-{}-{}",
        std::process::id(),
        OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let project = root.join("-fixtures-project");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("session-agent-result.jsonl"),
        concat!(
            r#"{"type":"user","sessionId":"session-agent-result","uuid":"user-1","timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","message":{"role":"user","content":"Read a file."}}"#,
            "\n",
            r#"{"type":"user","sessionId":"session-agent-result","uuid":"tool-result-1","timestamp":"2026-01-01T10:00:01Z","cwd":"/fixtures/project","toolUseResult":{"type":"text","file":{"filePath":"/fixtures/project/notes"}},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-read-1","content":"Notes."}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"session-agent-result","uuid":"tool-result-2","timestamp":"2026-01-01T10:00:02Z","cwd":"/fixtures/project","toolUseResult":{"status":"completed","agentId":"early-agent","agentType":"codex","resolvedModel":"claude-fixture-sonnet","totalDurationMs":900},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-agent-early","content":"Agent complete."}]}}"#,
            "\n"
        ),
    )
    .unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "session-agent-result");
    let lineage = backend.lineage(&session).unwrap();

    assert_eq!(lineage.children.len(), 1, "{:#?}", lineage.children);
    let child = &lineage.children[0];
    assert_eq!(child.reference, "early-agent");
    assert_eq!(child.role.as_deref(), Some("codex"));
    assert_eq!(child.disposition.as_deref(), Some("completed"));
    assert!(child.spawned_at.is_none(), "no call is in this read");
    assert!(!child.resolved);
    fs::remove_dir_all(root).unwrap();
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
fn claude_sums_each_request_once_and_marks_the_read_as_summed_requests() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = located(&backend, "session-claude");

    assert_eq!(
        session.tokens,
        Some(Tokens {
            input: Some(11),
            output: Some(22),
            reasoning: Some(55),
            cache_read: Some(33),
            cache_write: Some(44),
        })
    );
    assert!(
        session.cost.is_none(),
        "Claude request records have no cost"
    );
    assert_eq!(
        session.accounting,
        Some(Accounting {
            basis: AccountingBasis::SummedRequests,
            coverage: AccountingCoverage::Session,
        })
    );

    let transcript = backend.transcript(&session, 10).unwrap();
    assert_eq!(transcript.session.accounting, session.accounting);
}

#[test]
fn claude_cost_state_supplies_recorded_totals_over_request_usage() {
    let root = std::env::temp_dir().join(format!("tapes-claude-cost-state-{}", std::process::id()));
    let project = root.join("project");
    let path = project.join("session-cost-state.jsonl");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&project).unwrap();
    fs::write(
        &path,
        concat!(
            r#"{"type":"user","sessionId":"session-cost-state","uuid":"user-1","timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","message":{"role":"user","content":"Inspect the fixture."}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"session-cost-state","uuid":"assistant-1","requestId":"request-1","timestamp":"2026-01-01T10:00:01Z","cwd":"/fixtures/project","message":{"role":"assistant","model":"claude-fixture","usage":{"input_tokens":1,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4,"output_tokens_details":{"thinking_tokens":5}},"content":[{"type":"text","text":"Fixture inspected."}]}}"#,
            "\n",
            r#"{"type":"cost-state","sessionId":"session-cost-state","totalCostUSD":12.5,"modelUsage":{"claude-fixture":{"inputTokens":100,"outputTokens":200,"thinkingTokens":300,"cacheReadInputTokens":400,"cacheCreationInputTokens":500},"claude-other":{"inputTokens":1,"outputTokens":2,"thinkingTokens":3,"cacheReadInputTokens":4,"cacheCreationInputTokens":5}},"hasUnknownModelCost":false}"#,
            "\n"
        ),
    )
    .unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "session-cost-state");
    assert_eq!(
        session.tokens,
        Some(Tokens {
            input: Some(101),
            output: Some(202),
            reasoning: Some(303),
            cache_read: Some(404),
            cache_write: Some(505),
        })
    );
    assert_eq!(session.cost, Some(Cost { usd: 12.5 }));
    assert_eq!(
        session.accounting,
        Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        })
    );

    let transcript = backend.transcript(&session, 10).unwrap();
    assert_eq!(
        transcript.turns.len(),
        2,
        "cost-state is metadata, not a turn"
    );
    assert!(transcript.trailing_record.is_none());

    fs::remove_dir_all(root).unwrap();
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

    assert_eq!(listing.scanned, 3, "{:?}", listing.unavailable);
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
    assert_eq!(listing.unavailable.len(), 2, "{:?}", listing.unavailable);
    assert!(listing.unavailable[0].ends_with("1 of 3 session rows unreadable"));
    assert!(listing.unavailable[1].contains("ses_truncated_fixture"));
    assert!(listing.unavailable[1].contains("malformed row metadata"));
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
    assert_eq!(result.unreadable.len(), 2, "{:?}", result.unreadable);
    assert!(result.unreadable[1].contains("ses_truncated_fixture"));
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
    assert_eq!(
        session.tokens,
        Some(Tokens {
            input: Some(40),
            output: Some(60),
            reasoning: Some(10),
            cache_read: Some(12),
            cache_write: Some(14),
        })
    );
    assert_eq!(session.cost, Some(Cost { usd: 0.375 }));
    assert_eq!(
        session.accounting,
        Some(Accounting {
            basis: AccountingBasis::SummedRequests,
            coverage: AccountingCoverage::Session,
        })
    );
    assert_eq!(transcript.session.tokens, session.tokens);
}

/// Every pi assistant message names the model and provider that produced it,
/// so the per-model split is read rather than inferred, and the reasoning
/// level in effect is the latest one the recording changed to. A bounded read
/// and a whole-recording read report the same split.
#[test]
fn pi_usage_reports_the_model_and_effort_split() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures("pi")))];
    let id = "session-model-split";
    let expected = Some(vec![
        ModelUsage {
            model: "pi-model-a".to_owned(),
            variant: Some("low".to_owned()),
            tokens: Some(Tokens {
                input: Some(10),
                output: Some(20),
                reasoning: Some(3),
                cache_read: Some(4),
                cache_write: Some(5),
            }),
            cost: Some(Cost { usd: 0.5 }),
            request_count: Some(1),
        },
        ModelUsage {
            model: "pi-model-b".to_owned(),
            variant: Some("high".to_owned()),
            tokens: Some(Tokens {
                input: Some(100),
                output: Some(200),
                reasoning: Some(30),
                cache_read: Some(40),
                cache_write: Some(50),
            }),
            cost: Some(Cost { usd: 0.25 }),
            request_count: Some(1),
        },
        ModelUsage {
            model: "pi-model-b".to_owned(),
            variant: Some("medium".to_owned()),
            tokens: Some(Tokens {
                input: Some(1),
                output: Some(2),
                reasoning: Some(3),
                cache_read: Some(4),
                cache_write: Some(5),
            }),
            cost: Some(Cost { usd: 0.25 }),
            request_count: Some(1),
        },
    ]);

    let session = located(backends[0].as_ref(), id);
    assert_eq!(session.model.as_ref().unwrap().id, "pi-model-b");
    assert_eq!(
        session.model.as_ref().unwrap().variant.as_deref(),
        Some("medium")
    );
    let view = usage(&backends[0].transcript(&session, usize::MAX).unwrap());
    assert_eq!(view.by_model, expected);

    let whole = usage_full_with_backends(&backends, Selection::Id(id)).unwrap();
    assert_eq!(whole.by_model, expected);
    assert_eq!(whole.session.model.as_ref().unwrap().id, "pi-model-b");
    assert_eq!(
        whole.session.model.as_ref().unwrap().variant.as_deref(),
        Some("medium")
    );

    let stats = stats_with_backends(&backends, Selection::Id(id)).unwrap();
    let value = serde_json::to_value(&stats).unwrap();
    assert_eq!(
        value["usage"]["by_model"],
        serde_json::to_value(&expected).unwrap()
    );

    let whole_stats = stats_full_with_backends(&backends, Selection::Id(id)).unwrap();
    let value = serde_json::to_value(&whole_stats).unwrap();
    assert_eq!(
        value["usage"]["by_model"],
        serde_json::to_value(&expected).unwrap()
    );
}

/// A turn's duration is the wall clock from the record before its own source
/// record to that record, so a record that normalizes to several turns
/// contributes one sample. Every assistant record is a model response, whether
/// it carried text, reasoning, or only a tool call, and a tool-result record
/// is not one. A bounded read and a whole-recording read report the same
/// distribution.
#[test]
fn stats_reports_the_assistant_turn_duration_distribution() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(fixtures("pi")))];
    let id = "session-turn-durations";
    // Eleven assistant records: ten carrying text, and one carrying only a
    // thinking block and a tool call. The tool-result record after it is not a
    // sample even though its interval would be the largest.
    let expected = serde_json::json!({
        "count": 11,
        "median": 5_000,
        "p90": 9_000,
        "max": 10_000,
    });

    let stats = stats_with_backends(&backends, Selection::Id(id)).unwrap();
    let value = serde_json::to_value(&stats).unwrap();
    assert_eq!(value["assistant_turns_ms"], expected, "{value}");

    let whole = stats_full_with_backends(&backends, Selection::Id(id)).unwrap();
    let value = serde_json::to_value(&whole).unwrap();
    assert_eq!(value["assistant_turns_ms"], expected, "{value}");

    // A record that normalizes to turns but carries no model response
    // contributes no duration, and every record that does carry one is a
    // sample: the fixture's first assistant record is a thinking block and a
    // tool call, its second is text, and its last is a tool call alone.
    let stats = stats_with_backends(&backends, Selection::Id("session-pi")).unwrap();
    let value = serde_json::to_value(&stats).unwrap();
    assert_eq!(
        value["assistant_turns_ms"],
        serde_json::json!({"count": 3, "median": 1_000, "p90": 1_000, "max": 1_000}),
        "{value}"
    );
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
    let native_page = tapes_discovery::NativeStore::codex(&root).candidates(5_000);
    assert_eq!(
        native_page
            .records
            .iter()
            .map(|session| session.id())
            .collect::<Vec<_>>(),
        vec!["other", "wanted-b", "wanted-a"],
        "{native_page:?}"
    );
    assert!(native_page.complete);
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
fn activity_filters_fill_the_limit_after_rejecting_candidates() {
    let root = filtered_store("activity-window");
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let since = "2026-08-09T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
    let until = "2026-08-09T00:00:02Z".parse::<DateTime<Utc>>().unwrap();

    let result = list_with_backends_options(
        &backends,
        Some("codex"),
        None,
        2,
        &ListFilters {
            since: Some(since),
            until: Some(until),
            ..ListFilters::default()
        },
        ListSort::Newest,
    )
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
fn listing_ties_on_activity_by_session_id() {
    let root = std::env::temp_dir().join(format!("tapes-activity-ties-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/08/09");
    fs::create_dir_all(&day).unwrap();
    for (index, id) in [(0, "tie-z"), (1, "tie-a")] {
        let path = day.join(format!("rollout-2026-08-09T00-00-0{index}-{id}.jsonl"));
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"timestamp":"2026-08-09T00:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-08-09T00:00:00Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#,
                    "\n"
                ),
                id = id
            ),
        )
        .unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_800_000_000 + index),
            )
            .unwrap();
    }

    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let result = list_with_backends_options(
        &backends,
        Some("codex"),
        None,
        10,
        &ListFilters::default(),
        ListSort::Newest,
    )
    .unwrap();

    assert_eq!(
        result
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["tie-a", "tie-z"]
    );
    assert_eq!(
        result.sessions[0].last_activity_at,
        result.sessions[1].last_activity_at
    );

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
        Some("2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap()),
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
    fn kinds(&self) -> tapes_core::model::KindDeclaration {
        tapes_core::model::KindDeclaration {
            recordable: tapes_core::model::TurnSelection::only(TurnKind::ALL),
            user_default: None,
        }
    }

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
                kind: TurnKind::Operator,
                text: "needle".into(),
                ts: None,
                ordinal: 0,
                native_id: None,
                request_turn_id: None,
                metadata: None,
                record_ref: None,
                parts: Vec::new(),
                coverage: None,
                channel: None,
                recipient: None,
                tool: None,
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
            read: None,
            terminal: None,
            text_tail: None,
            artifacts: Vec::new(),
            graph: None,
            trailing_record: None,
            notes: Vec::new(),
            projection: None,
            kinds: None,
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
    fn kinds(&self) -> tapes_core::model::KindDeclaration {
        tapes_core::model::KindDeclaration {
            recordable: tapes_core::model::TurnSelection::only(TurnKind::ALL),
            user_default: None,
        }
    }

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
        source: SourceDescriptor::installed("fixture", "fixture-recording"),
        metadata: None,
        model: None,
        model_observation: None,
        title: None,
        derived_title: None,
        derived_title_truncated: None,
        directory: None,
        started_at: Some(timestamp),
        last_activity_at: Some(timestamp),
        live: None,
        cost: None,
        tokens: None,
        accounting: None,
        start_uncertain: false,
        occurrence: None,
        usage_detail: None,
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
        r#"{{"type":"assistant","sessionId":"bounded","requestId":"bounded-request","timestamp":"2026-01-01T12:00:01Z","cwd":"/fixtures/project","message":{{"role":"assistant","model":"claude-fixture","usage":{{"input_tokens":7,"output_tokens":8,"cache_read_input_tokens":9,"cache_creation_input_tokens":10}},"content":[{{"type":"text","text":"Tail output."}}]}}}}"#
    )
    .unwrap();
    file.flush().unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "bounded");
    let transcript = backend.transcript(&session, 10).unwrap();

    assert!(transcript.truncated);
    assert_eq!(transcript.turns.len(), 2);
    assert_eq!(
        transcript.session.tokens,
        Some(Tokens {
            input: Some(7),
            output: Some(8),
            reasoning: None,
            cache_read: Some(9),
            cache_write: Some(10),
        })
    );
    assert_eq!(
        transcript.session.accounting,
        Some(Accounting {
            basis: AccountingBasis::SummedRequests,
            coverage: AccountingCoverage::ReadWindow,
        })
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unmatched_results_distinguish_missing_calls_from_calls_before_the_file_tail() {
    for (tag, padded, expected) in [
        ("whole", false, Incomplete::CallNotRecorded),
        ("bounded", true, Incomplete::CallBeforeReadBound),
    ] {
        let root = std::env::temp_dir().join(format!(
            "tapes-unmatched-result-{tag}-{}",
            std::process::id()
        ));
        let day = root.join("2026/01/01");
        fs::create_dir_all(&day).unwrap();
        let id = if padded {
            "00000000-0000-0000-0000-00000000b001"
        } else {
            "00000000-0000-0000-0000-00000000a001"
        };
        let path = day.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
        let mut file = BufWriter::new(File::create(&path).unwrap());
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
        )
        .unwrap();
        if padded {
            file.write_all(b"{\"padding\":\"").unwrap();
            file.write_all(&vec![b'x'; 4 * 1024 * 1024]).unwrap();
            file.write_all(b"\"}\n").unwrap();
        }
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T10:00:01Z","type":"response_item","payload":{{"type":"function_call_output","call_id":"missing-call","output":"orphan"}}}}"#
        )
        .unwrap();
        drop(file);

        let backend = CodexBackend::new(&root);
        let session = located(&backend, id);
        let events = project(
            backend.transcript(&session, usize::MAX).unwrap(),
            usize::MAX,
        );
        assert_eq!(events.events.len(), 1);
        assert_eq!(events.events[0].incomplete, Some(expected));
        assert_eq!(events.pairs.incomplete, 1);
        fs::remove_dir_all(root).unwrap();
    }
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
    assert!(error.to_string().contains("coverage is incomplete"));
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
    assert!(failures[0].contains("opencode"));
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
            session.started_at,
            Some(start),
            "{what}: the header's timestamp is the start"
        );
        assert_eq!(
            session.last_activity_at,
            Some(end),
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
        assert_eq!(transcript.session.started_at, Some(start));
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
        assert_eq!(listed[0].started_at, Some(start), "{what}: listing agrees");
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
        assert!(session.locator().is_some(), "{harness}: store coordinate");

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

    let events = backend.events(&session, 1).unwrap();
    assert!(
        events.events.is_empty(),
        "the generated page has no tool parts"
    );
    assert_eq!(events.truncation, tail.truncation);

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
    assert_eq!(
        session.accounting,
        Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        })
    );

    let transcript = backend.transcript(&session, 10).unwrap();
    assert_eq!(transcript.session.tokens, session.tokens);
    assert_eq!(transcript.turns.len(), 6, "token events are not turns");

    let without = located(&backend, "20000000-0000-0000-0000-000000000004");
    assert!(without.tokens.is_none(), "no token event, no counters");
    assert!(without.accounting.is_none());
}

/// Code mode records the operations its `exec` script ran only as runtime
/// items, so each is a tool turn; an item whose id a `response_item` already
/// carried, a web search recorded as both an item and a `web_search_call`,
/// and a plan the next assistant message restates each appear once.
#[test]
fn codex_runtime_items_are_tool_turns_unless_a_record_read_before_names_them() {
    let root =
        std::env::temp_dir().join(format!("tapes-codex-runtime-items-{}", std::process::id()));
    let day = root.join("2026/01/01");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&day).unwrap();
    let id = "00000000-0000-0000-0000-00000000b001";
    let path = day.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let item = |item: serde_json::Value| serde_json::json!({"timestamp": "2026-01-01T10:00:02Z", "type": "event_msg", "payload": {"type": "item_completed", "thread_id": id, "turn_id": "turn-1", "item": item}});
    let response = |payload: serde_json::Value| serde_json::json!({"timestamp": "2026-01-01T10:00:01Z", "type": "response_item", "payload": payload});
    let search = serde_json::json!({"type": "search", "query": "fixture", "queries": ["fixture"]});
    let records = [
        serde_json::json!({"timestamp": "2026-01-01T10:00:00Z", "type": "session_meta", "payload": {"id": id, "source": "cli", "cwd": "/fixtures/project"}}),
        response(
            serde_json::json!({"type": "custom_tool_call", "call_id": "call-exec", "name": "exec", "input": "await tools.view_image({path: 'shot.png'})", "status": "completed"}),
        ),
        item(
            serde_json::json!({"type": "McpToolCall", "id": "exec-mcp", "server": "fixture", "tool": "lookup", "arguments": {"q": "x"}, "status": "completed", "result": {"content": []}}),
        ),
        item(
            serde_json::json!({"type": "ImageView", "id": "exec-image", "path": "file:///fixtures/shot.png"}),
        ),
        item(
            serde_json::json!({"type": "Extension", "kind": "web.search", "id": "exec-extension", "query": "fixture", "results": []}),
        ),
        response(
            serde_json::json!({"type": "custom_tool_call_output", "call_id": "call-patch", "output": "applied"}),
        ),
        item(serde_json::json!({"type": "FileChange", "id": "call-patch", "changes": {}})),
        item(serde_json::json!({"type": "FileChange", "id": "exec-patch", "changes": {}})),
        response(
            serde_json::json!({"type": "function_call", "call_id": "call-spawn", "name": "spawn_agent", "arguments": "{}"}),
        ),
        item(
            serde_json::json!({"type": "SubAgentActivity", "id": "call-spawn", "kind": "interacted", "agent_path": "/root"}),
        ),
        item(
            serde_json::json!({"type": "WebSearch", "id": "ws_first", "query": "fixture", "action": search}),
        ),
        response(
            serde_json::json!({"type": "web_search_call", "status": "completed", "action": search}),
        ),
        item(
            serde_json::json!({"type": "WebSearch", "id": "ws_again", "query": "fixture", "action": search}),
        ),
        response(
            serde_json::json!({"type": "web_search_call", "id": "ws_call", "status": "completed", "action": {"type": "search", "query": "call first"}}),
        ),
        item(
            serde_json::json!({"type": "WebSearch", "id": "ws_call", "query": "call first", "action": {"type": "search", "query": "call first"}}),
        ),
        item(serde_json::json!({"type": "Plan", "id": "turn-1-plan", "text": "# Plan"})),
        response(
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "<proposed_plan>\n# Plan\n</proposed_plan>"}]}),
        ),
    ];
    let body = records
        .iter()
        .map(|record| record.to_string() + "\n")
        .collect::<String>();
    fs::write(&path, body).unwrap();

    let backend = CodexBackend::new(&root);
    let session = located(&backend, id);
    let tools = backend
        .transcript(&session, usize::MAX)
        .unwrap()
        .turns
        .into_iter()
        .filter(|turn| turn.kind == TurnKind::Tool)
        .map(|turn| {
            let record: serde_json::Value = serde_json::from_str(&turn.text).unwrap();
            format!(
                "{}:{}",
                record["type"].as_str().unwrap(),
                record["id"]
                    .as_str()
                    .or_else(|| record["call_id"].as_str())
                    .unwrap_or("-")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tools,
        [
            "custom_tool_call:call-exec",
            "McpToolCall:exec-mcp",
            "ImageView:exec-image",
            "Extension:exec-extension",
            "custom_tool_call_output:call-patch",
            "FileChange:exec-patch",
            "function_call:call-spawn",
            "WebSearch:ws_first",
            "WebSearch:ws_again",
            "web_search_call:ws_call",
        ]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn codex_terminal_prefers_nested_native_error_fields_and_keeps_later_accounting() {
    let root =
        std::env::temp_dir().join(format!("tapes-codex-terminal-error-{}", std::process::id()));
    let day = root.join("2026/01/01");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&day).unwrap();
    let id = "00000000-0000-0000-0000-00000000a001";
    let path = day.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let nested_message = "m".repeat(2_050);
    let records = [
        serde_json::json!({
            "timestamp": "2026-01-01T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": id, "source": "exec", "cwd": "/fixtures/project"}
        }),
        serde_json::json!({
            "timestamp": "2026-01-01T10:00:01Z",
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "outcome": "error",
                "code": "outer-code",
                "message": "outer message",
                "error": {
                    "codex_error_info": "usage_limit_exceeded",
                    "message": nested_message
                }
            }
        }),
        serde_json::json!({
            "timestamp": "2026-01-01T10:00:02Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 19,
                        "output_tokens": 4,
                        "cached_input_tokens": 2,
                        "cache_write_input_tokens": 0
                    }
                },
                "rate_limits": null
            }
        }),
    ];
    fs::write(
        &path,
        records
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();

    let backend = CodexBackend::new(&root);
    let session = located(&backend, id);
    let transcript = backend.transcript(&session, usize::MAX).unwrap();
    let terminal = transcript.terminal.as_ref().unwrap();
    assert_eq!(
        terminal.code,
        Some(serde_json::json!("usage_limit_exceeded"))
    );
    assert_eq!(
        terminal.message.as_ref().unwrap().text.chars().count(),
        2_048
    );
    assert_eq!(terminal.message.as_ref().unwrap().chars, 2_050);
    assert!(terminal.message.as_ref().unwrap().truncated);
    assert_eq!(session.tokens, transcript.session.tokens);
    assert_eq!(
        session.tokens,
        Some(Tokens {
            input: Some(19),
            output: Some(4),
            reasoning: None,
            cache_read: Some(2),
            cache_write: Some(0),
        })
    );
    assert_eq!(
        session.accounting,
        Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        })
    );

    fs::remove_dir_all(root).unwrap();
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
        Some("2026-01-01T10:00:00Z".parse::<DateTime<Utc>>().unwrap())
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
        Some("2026-01-01T12:00:00Z".parse::<DateTime<Utc>>().unwrap()),
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

/// A backend walks its store newest first and stops at the limit, so an
/// oldest-first listing must inspect every candidate before it can know which
/// are the oldest; otherwise the limit would keep the newest and merely print
/// them oldest first.
#[test]
fn sort_oldest_selects_the_oldest_sessions_before_the_limit() {
    let root = std::env::temp_dir().join(format!("tapes-sort-oldest-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let day = root.join("2026/08/09");
    fs::create_dir_all(&day).unwrap();
    for (index, id) in [(0, "old"), (1, "mid"), (2, "new")] {
        let path = day.join(format!("rollout-2026-08-09T00-00-0{index}-{id}.jsonl"));
        fs::write(
            &path,
            format!(
                concat!(
                    r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#,
                    "\n",
                    r#"{{"timestamp":"2026-08-09T00:00:0{index}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hello"}}]}}}}"#,
                    "\n"
                ),
                index = index,
                id = id
            ),
        )
        .unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_secs(1_800_000_000 + index),
            )
            .unwrap();
    }
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&root))];
    let ids = |sort: ListSort| {
        list_with_backends_options(
            &backends,
            Some("codex"),
            None,
            2,
            &ListFilters::default(),
            sort,
        )
        .unwrap()
        .sessions
        .into_iter()
        .map(|session| session.id)
        .collect::<Vec<_>>()
    };

    assert_eq!(ids(ListSort::Oldest), vec!["old", "mid"]);
    assert_eq!(ids(ListSort::Newest), vec!["new", "mid"]);

    fs::remove_dir_all(root).unwrap();
}

/// A `cost-state` record is the session's running total wherever the read
/// reached it, so a recording past the file bound still reports whole-session
/// coverage from it; only a per-request sum is bounded by the read.
#[test]
fn a_truncated_claude_read_keeps_whole_session_coverage_for_a_cost_state_record() {
    let root = std::env::temp_dir().join(format!(
        "tapes-claude-truncated-cost-state-{}",
        std::process::id()
    ));
    let project = root.join("project");
    let path = project.join("session-truncated-cost.jsonl");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&project).unwrap();
    let mut file = BufWriter::new(File::create(&path).unwrap());
    writeln!(
        file,
        r#"{{"type":"user","sessionId":"session-truncated-cost","uuid":"user-1","timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","message":{{"role":"user","content":"Inspect the fixture."}}}}"#
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for index in 0..1100 {
        writeln!(
            file,
            r#"{{"type":"assistant","sessionId":"session-truncated-cost","uuid":"assistant-{index}","requestId":"request-{index}","timestamp":"2026-01-01T10:00:01Z","cwd":"/fixtures/project","message":{{"role":"assistant","model":"claude-fixture","usage":{{"input_tokens":1,"output_tokens":1}},"content":[{{"type":"text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    writeln!(
        file,
        r#"{{"type":"cost-state","sessionId":"session-truncated-cost","totalCostUSD":3.5,"modelUsage":{{"claude-fixture":{{"inputTokens":1100,"outputTokens":2200,"thinkingTokens":30,"cacheReadInputTokens":40,"cacheCreationInputTokens":50}}}},"hasUnknownModelCost":false}}"#
    )
    .unwrap();
    drop(file);
    assert!(fs::metadata(&path).unwrap().len() > 4 * 1024 * 1024);

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "session-truncated-cost");
    assert_eq!(
        session.tokens,
        Some(Tokens {
            input: Some(1100),
            output: Some(2200),
            reasoning: Some(30),
            cache_read: Some(40),
            cache_write: Some(50),
        })
    );
    assert_eq!(session.cost, Some(Cost { usd: 3.5 }));
    assert_eq!(
        session.accounting,
        Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        })
    );
    assert!(backend.transcript(&session, 1).unwrap().truncated);

    fs::remove_dir_all(root).unwrap();
}

/// Every kind rests on a field the harness wrote beside the text, so a record
/// whose text resembles a command is still an operator's when the sender
/// fields vouch for it, and a record with no such field stays unknown.
#[test]
fn claude_types_each_user_record_from_the_fields_it_recorded() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = located(&backend, "session-claude");
    let transcript = backend.transcript(&session, usize::MAX).unwrap();

    let kind = |native_id: &str| {
        transcript
            .turns
            .iter()
            .find(|turn| turn.native_id.as_deref() == Some(native_id))
            .unwrap_or_else(|| panic!("fixture has no record {native_id}"))
            .kind
    };

    assert_eq!(kind("user-1"), TurnKind::Unknown);
    assert_eq!(kind("user-2"), TurnKind::Operator);
    assert_eq!(kind("user-3"), TurnKind::Operator);
    assert_eq!(kind("meta-hook"), TurnKind::Ambient);
    assert_eq!(kind("meta-caveat"), TurnKind::Control);
    assert_eq!(kind("command-low-priority"), TurnKind::Control);
    assert_eq!(kind("command-exit"), TurnKind::Control);
    assert_eq!(kind("command-exit-stdout"), TurnKind::Control);
    assert_eq!(kind("notice-task"), TurnKind::Notice);
    assert_eq!(kind("tool-result-1"), TurnKind::Tool);
    assert_eq!(kind("assistant-1"), TurnKind::Reasoning);
    assert_eq!(kind("assistant-2"), TurnKind::Assistant);
}

/// A backend declares the kinds its harness's records can evidence, and every
/// fixture turn is of a declared kind. A backend that gives user turns a
/// default states why.
#[test]
fn every_fixture_turn_is_of_a_kind_its_backend_declares() {
    for (backend, id) in fixture_backends() {
        let declared = backend.kinds();
        let session = located(backend.as_ref(), id);
        let transcript = show_with_backends(
            std::slice::from_ref(&backend),
            Selection::Id(id),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(transcript.kinds.as_ref(), Some(&declared), "{id}");
        for turn in backend.transcript(&session, usize::MAX).unwrap().turns {
            assert!(
                declared.recordable.keeps(turn.kind),
                "{id}: turn {} is {:?}, which {} does not declare",
                turn.ordinal,
                turn.kind,
                backend.harness()
            );
        }
        if let Some(default) = &declared.user_default {
            assert!(declared.recordable.keeps(default.kind), "{id}");
            assert!(!default.basis.is_empty(), "{id}");
        }
    }
}

/// A read counts every decoded record it represented as no turn, by native
/// type, whether it reads the recording's tail or streams all of it; OpenCode,
/// which reads rows rather than records, does not count them.
#[test]
fn a_read_counts_the_records_it_represented_as_no_turn() {
    let counts = |pairs: &[(&str, usize)]| {
        pairs
            .iter()
            .map(|(kind, count)| ((*kind).to_owned(), *count))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let expected = [
        counts(&[
            ("ai-title", 2),
            ("atis-latch", 1),
            ("last-prompt", 1),
            ("mode", 1),
            ("permission-mode", 1),
        ]),
        counts(&[
            ("event_msg/task_complete", 1),
            ("event_msg/token_count", 3),
            ("session_meta", 1),
            ("turn_context", 1),
        ]),
        counts(&[
            ("model_change", 1),
            ("session", 1),
            ("thinking_level_change", 2),
        ]),
    ];
    for ((backend, id), declined) in file_fixture_backends().into_iter().zip(expected) {
        let session = located(backend.as_ref(), id);
        let bounded = backend
            .transcript(&session, usize::MAX)
            .unwrap()
            .read
            .unwrap()
            .unmapped
            .unwrap_or_else(|| panic!("{id} counts no unmapped records"));
        assert_eq!(bounded.declined, declined, "{id}");
        assert!(bounded.unrecognized.is_empty(), "{id}: {bounded:?}");
        let streamed = backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap()
            .unmapped;
        assert_eq!(streamed.as_ref(), Some(&bounded), "{id}");
    }

    let opencode = OpenCodeBackend::new(opencode_fixture_program());
    let session = located(&opencode, "ses_000000fixtureSharedSession");
    let read = opencode.transcript(&session, usize::MAX).unwrap().read;
    assert!(read.is_none_or(|read| read.unmapped.is_none()));

    let root =
        std::env::temp_dir().join(format!("tapes-claude-unrecognized-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project = root.join("-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    let id = "7f3c2a10-0000-4000-8000-000000000002";
    let records = [
        serde_json::json!({"type": "user", "sessionId": id, "uuid": "user", "timestamp": "2026-01-01T10:00:00Z", "cwd": "/fixtures/project", "promptSource": "typed", "message": {"role": "user", "content": "Inspect the fixture."}}),
        serde_json::json!({"type": "attachment", "sessionId": id, "uuid": "queued", "timestamp": "2026-01-01T10:00:01Z", "attachment": {"type": "queued_command", "prompt": "Also check the tests."}}),
        serde_json::json!({"type": "system", "sessionId": id, "uuid": "away", "timestamp": "2026-01-01T10:00:02Z", "subtype": "away_summary", "content": "Inspection finished."}),
        serde_json::json!({"type": "system", "sessionId": id, "uuid": "duration", "timestamp": "2026-01-01T10:00:03Z", "subtype": "turn_duration", "durationMs": 10}),
    ];
    fs::write(
        project.join(format!("{id}.jsonl")),
        records
            .iter()
            .map(|record| record.to_string() + "\n")
            .collect::<String>(),
    )
    .unwrap();
    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, id);
    let unmapped = backend
        .transcript(&session, usize::MAX)
        .unwrap()
        .read
        .unwrap()
        .unmapped
        .unwrap();
    assert_eq!(unmapped.declined, counts(&[("system/turn_duration", 1)]));
    assert_eq!(
        unmapped.unrecognized,
        counts(&[("attachment/queued_command", 1), ("system/away_summary", 1)])
    );
    fs::remove_dir_all(root).unwrap();
}

/// Each user record types from what Claude wrote beside it: an SDK prompt
/// source, the `!` shell envelope, an interruption, a compaction summary, and
/// the brief that opens a subagent's transcript. A sender value nobody has
/// verified, and text with no evidence at all, stay unknown.
#[test]
fn claude_types_sdk_shell_interruption_compaction_and_subagent_records() {
    let root = std::env::temp_dir().join(format!(
        "tapes-claude-user-populations-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let id = "7f3c2a10-0000-4000-8000-000000000001";
    let project = root.join("-fixtures-project");
    let subagents = project.join(id).join("subagents");
    fs::create_dir_all(&subagents).unwrap();
    let user = |uuid: &str, fields: serde_json::Value, content: serde_json::Value| {
        let mut record = serde_json::json!({"type": "user", "sessionId": id, "uuid": uuid, "parentUuid": "previous", "timestamp": "2026-01-01T10:00:00Z", "cwd": "/fixtures/project", "isSidechain": false, "entrypoint": "cli", "promptId": "prompt", "message": {"role": "user", "content": content}});
        for (key, value) in fields.as_object().unwrap() {
            record[key] = value.clone();
        }
        record.to_string() + "\n"
    };
    let none = serde_json::json!({});
    let recording = [
        user("sdk", serde_json::json!({"promptSource": "sdk", "entrypoint": "sdk-cli"}), "Summarize the fixture.".into()),
        user("bash-input", none.clone(), "<bash-input>ls</bash-input>".into()),
        user("bash-output", none.clone(), "<bash-stdout>fixture.txt</bash-stdout><bash-stderr></bash-stderr>".into()),
        user("interrupted", serde_json::json!({"interruptedMessageId": "msg_fixture"}), "[Request interrupted by user]".into()),
        user("interrupted-tool", none.clone(), serde_json::json!([{"type": "text", "text": "[Request interrupted by user for tool use]"}])),
        user("compacted", serde_json::json!({"isCompactSummary": true, "isVisibleInTranscriptOnly": true}), "This session is being continued from a previous conversation.".into()),
        user("unverified-source", serde_json::json!({"promptSource": "queued"}), "Queued text.".into()),
        user("no-evidence", none.clone(), "Plain text.".into()),
    ]
    .concat();
    fs::write(project.join(format!("{id}.jsonl")), recording).unwrap();
    let child = |uuid: &str, parent: Option<&str>, text: &str| {
        serde_json::json!({"type": "user", "sessionId": id, "uuid": uuid, "parentUuid": parent, "timestamp": "2026-01-01T10:00:01Z", "cwd": "/fixtures/project", "isSidechain": true, "agentId": "a0fixture", "entrypoint": "cli", "promptId": "prompt", "message": {"role": "user", "content": text}}).to_string() + "\n"
    };
    fs::write(
        subagents.join("agent-a0fixture.jsonl"),
        child("brief", None, "Inspect the fixture and report.")
            + &child("later", Some("brief"), "Plain text."),
    )
    .unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, id);
    let transcript = backend.transcript(&session, usize::MAX).unwrap();
    let kind = |turns: &[Turn], native_id: &str| {
        turns
            .iter()
            .find(|turn| turn.native_id.as_deref() == Some(native_id))
            .unwrap_or_else(|| panic!("fixture has no record {native_id}"))
            .kind
    };
    for (native_id, expected) in [
        ("sdk", TurnKind::Operator),
        ("bash-input", TurnKind::Control),
        ("bash-output", TurnKind::Control),
        ("interrupted", TurnKind::Notice),
        ("interrupted-tool", TurnKind::Notice),
        ("compacted", TurnKind::Ambient),
        ("unverified-source", TurnKind::Unknown),
        ("no-evidence", TurnKind::Unknown),
    ] {
        assert_eq!(kind(&transcript.turns, native_id), expected, "{native_id}");
    }

    let subagent = backend.child_transcript(&session, "a0fixture").unwrap();
    assert_eq!(kind(&subagent.turns, "brief"), TurnKind::Operator);
    assert_eq!(kind(&subagent.turns, "later"), TurnKind::Unknown);
    fs::remove_dir_all(root).unwrap();
}

/// The ending a reader judges: the operator's last turn is the one before the
/// commands the harness recorded on its way out, so a session that ends on a
/// control turn is not an unanswered prompt.
#[test]
fn the_last_operator_turn_precedes_the_commands_that_close_a_claude_session() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = located(&backend, "session-claude");
    let transcript = backend.transcript(&session, usize::MAX).unwrap();

    let last_operator = transcript
        .turns
        .iter()
        .rposition(|turn| turn.kind == TurnKind::Operator)
        .unwrap();
    assert!(transcript.turns[last_operator + 1..]
        .iter()
        .filter(|turn| turn.role == Role::User)
        .all(|turn| {
            matches!(
                turn.kind,
                TurnKind::Control | TurnKind::Notice | TurnKind::Ambient
            )
        }));
    assert_eq!(
        transcript.turns[last_operator + 1].kind,
        TurnKind::Ambient,
        "the operator's last turn precedes what the harness recorded after it"
    );
}

/// The bundle's context file is the session's argument, so the harness's own
/// commands, notices, and attached context stay in the trace beside them.
#[test]
fn a_bundle_context_keeps_operator_turns_and_drops_harness_records() {
    let backend = ClaudeBackend::new(fixtures("claude"));
    let session = located(&backend, "session-claude");
    let transcript = backend.transcript(&session, usize::MAX).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "tapes-context-kinds-{}-{}",
        std::process::id(),
        OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let bundle = tapes_core::bundle::export(&transcript, &directory).unwrap();

    let context = fs::read_to_string(&bundle.context.path).unwrap();
    assert!(context.contains("Check the ending."), "{context}");
    assert!(!context.contains("/low-priority"), "{context}");
    assert!(!context.contains("local-command-stdout"), "{context}");
    assert!(
        !context.contains("A background task finished."),
        "{context}"
    );
    assert!(!context.contains("Fixture hook ran"), "{context}");

    let trace = fs::read_to_string(&bundle.trace.path).unwrap();
    assert!(trace.contains("## user/control #10"), "{trace}");
    assert!(trace.contains("## user/notice #12"), "{trace}");
    assert!(trace.contains("## user/ambient #7"), "{trace}");
    fs::remove_dir_all(directory).unwrap();
}

/// Codex writes no sender field, so each user message is typed by the elements
/// the harness wrapped its own text in, whatever entry point started it.
#[test]
fn codex_types_user_messages_by_the_elements_the_harness_wrote() {
    let backend = CodexBackend::new(fixtures("codex"));
    let kinds = |id: &str| {
        let session = located(&backend, id);
        backend
            .transcript(&session, usize::MAX)
            .unwrap()
            .turns
            .into_iter()
            .filter(|turn| turn.role == Role::User)
            .map(|turn| turn.kind)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        kinds("10000000-0000-0000-0000-000000000003"),
        vec![TurnKind::Ambient, TurnKind::Operator]
    );
    assert_eq!(
        kinds("30000000-0000-0000-0000-000000000005"),
        vec![TurnKind::Operator],
        "an exec session's prompt"
    );
    assert_eq!(
        kinds("20000000-0000-0000-0000-000000000004"),
        vec![TurnKind::Operator],
        "a header naming no entry point"
    );
    assert_eq!(
        kinds("50000000-0000-7000-8000-000000000006"),
        vec![
            TurnKind::Ambient,
            TurnKind::Operator,
            TurnKind::Operator,
            TurnKind::Ambient,
            TurnKind::Notice,
            TurnKind::Operator,
            TurnKind::Notice,
            TurnKind::Notice,
        ],
        "an interactive session's instructions, skill, requests, and notices"
    );
}

/// pi and OpenCode keep their harness messages out of the user role, so an
/// unanswered user turn there is a genuine unanswered prompt.
#[test]
fn pi_and_opencode_user_turns_are_the_operator_speaking() {
    let backends: Vec<(Box<dyn Backend>, &str)> = vec![
        (Box::new(PiBackend::new(fixtures("pi"))), "session-pi"),
        (
            Box::new(OpenCodeBackend::new(opencode_fixture_program())),
            "ses_000000fixtureSharedSession",
        ),
    ];
    for (backend, id) in backends {
        let session = located(backend.as_ref(), id);
        let transcript = backend.transcript(&session, usize::MAX).unwrap();
        let user = transcript
            .turns
            .iter()
            .filter(|turn| turn.role == Role::User)
            .collect::<Vec<_>>();
        assert!(!user.is_empty(), "{} has no user turn", backend.harness());
        assert!(
            user.iter().all(|turn| turn.kind == TurnKind::Operator),
            "{} typed a user turn as something else",
            backend.harness()
        );
    }
}

/// Codex records the context window and the account's quota windows on its
/// `token_count` events. The newest event carrying each fact answers for it,
/// so a quota refresh without usage still reports the quota.
#[test]
fn codex_usage_reports_the_context_window_and_the_newest_quota_windows() {
    let backend = CodexBackend::new(fixtures("codex"));
    let session = located(&backend, "00000000-0000-0000-0000-000000000001");
    let view = usage(&backend.transcript(&session, usize::MAX).unwrap());

    assert_eq!(view.context_window, Some(272_000));
    let limits = view.rate_limits.expect("the fixture records rate limits");
    assert_eq!(
        limits.primary,
        Some(RateWindow {
            used_percent: serde_json::json!(12),
            window_minutes: Some(300),
            resets_at: Some("2026-01-01T14:00:00Z".parse().unwrap()),
        })
    );
    assert_eq!(
        limits.secondary,
        Some(RateWindow {
            used_percent: serde_json::json!(92),
            window_minutes: Some(10_080),
            resets_at: Some("2026-01-05T10:00:00Z".parse().unwrap()),
        })
    );
    assert_eq!(limits.plan.as_deref(), Some("fixture"));
    assert_eq!(
        limits
            .credits
            .as_ref()
            .and_then(|credits| credits.balance.as_ref()),
        Some(&serde_json::json!("0"))
    );
    assert_eq!(
        limits
            .credits
            .as_ref()
            .and_then(|credits| credits.has_credits),
        Some(false)
    );
    assert_eq!(limits.spend_control_reached, Some(false));
    assert_eq!(limits.rate_limit_reached, Some(false));
    assert_eq!(limits.rate_limit_reached_type.as_deref(), Some("primary"));
    assert_eq!(
        limits.observed_at,
        Some("2026-01-01T10:00:06.700Z".parse().unwrap())
    );
    assert_eq!(view.tokens, session.tokens);
    assert_eq!(view.turns.total, 6);
    assert_eq!(view.turns.coverage, TurnCoverage::Session);
    assert!(view.durations_ms.is_none(), "Codex records no durations");
    assert_eq!(
        view.by_model,
        Some(vec![ModelUsage {
            model: "gpt-fixture".to_owned(),
            variant: Some("high".to_owned()),
            tokens: Some(Tokens {
                input: Some(1200),
                output: Some(300),
                reasoning: Some(50),
                cache_read: Some(1000),
                cache_write: Some(0),
            }),
            cost: None,
            request_count: Some(2),
        }])
    );
    let attribution = view
        .attribution
        .expect("legacy observations are attributed");
    assert_eq!(attribution.basis, ObservationBasis::TokenEventAdvance);
    assert_eq!(attribution.counted, 2);
    assert_eq!(attribution.attributed, 2);
    assert!(!attribution
        .incomplete
        .iter()
        .any(|reason| reason == "leading-uncounted"));

    let without = located(&backend, "20000000-0000-0000-0000-000000000004");
    let without = usage(&backend.transcript(&without, usize::MAX).unwrap());
    assert!(without.context_window.is_none());
    assert!(without.rate_limits.is_none());
}

#[test]
fn codex_modern_usage_records_choose_the_modern_basis_and_keep_native_rows() {
    let backend: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(fixtures("codex")))];
    let selection = Selection::Id("60000000-0000-0000-0000-000000000007");
    let view = usage_with_options_with_backends(
        &backend,
        selection,
        UsageOptions {
            full: false,
            series: Some(UsageObservationOptions { limit: 16 }),
        },
    )
    .unwrap();

    assert_eq!(view.schema, "tapes-usage/6");
    assert_eq!(
        view.tokens.as_ref().and_then(|tokens| tokens.input),
        Some(60)
    );
    assert!(view.session.model_observation.as_ref().unwrap().mixed);
    let models = view.by_model.as_ref().unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].model, "gpt-alpha");
    assert_eq!(models[0].variant.as_deref(), Some("high"));
    assert_eq!(models[0].request_count, Some(2));
    assert_eq!(models[1].model, "gpt-beta");
    assert_eq!(models[1].request_count, Some(1));

    let attribution = view.attribution.as_ref().unwrap();
    assert_eq!(attribution.basis, ObservationBasis::UsageRecord);
    assert_eq!(attribution.observed, 4);
    assert_eq!(attribution.counted, 3);
    assert_eq!(attribution.attributed, 3);
    assert_eq!(attribution.unattributed, 0);

    let series = view.series.unwrap();
    assert_eq!(series.observed, 6);
    assert_eq!(series.returned, 6);
    assert_eq!(
        series
            .rows
            .iter()
            .map(|row| row.native_ordinal)
            .collect::<Vec<_>>(),
        vec![Some(2), Some(5), Some(8), Some(9), Some(10), Some(11)]
    );
    assert_eq!(
        series.rows[2].classification,
        ObservationClassification::Repeat
    );
    assert!(!series.rows[2].counted);
    assert!(series.rows[4].rate_limits.is_none());
    assert!(series.rows[5].rate_limits.is_some());
}

#[test]
fn codex_legacy_counter_resets_keep_the_newest_total_and_since_reset_coverage() {
    let id = "70000000-0000-0000-0000-000000000008";
    let records = vec![
        codex_meta(id),
        codex_turn_context("gpt-reset"),
        codex_token_count(2, 10, 10),
        codex_token_count(3, 20, 10),
        codex_token_count(4, 5, 5),
    ];
    let root = codex_rollout("legacy-reset", id, &records);

    for full in [false, true] {
        let view = codex_usage_with_options(&root, id, full, Some(8)).unwrap();
        assert_eq!(
            view.tokens.as_ref().and_then(|tokens| tokens.input),
            Some(5),
            "full={full}"
        );
        assert_eq!(
            view.accounting
                .as_ref()
                .map(|accounting| accounting.coverage),
            Some(AccountingCoverage::SinceReset),
            "full={full}"
        );
        let attribution = view.attribution.as_ref().unwrap();
        assert_eq!(
            attribution.coverage,
            AccountingCoverage::Session,
            "full={full}"
        );
        assert_eq!(attribution.resets, 1, "full={full}");
        assert_eq!(attribution.counted, 3, "full={full}");
        assert_eq!(
            view.by_model
                .as_ref()
                .and_then(|models| models.first())
                .and_then(|model| model.tokens.as_ref())
                .and_then(|tokens| tokens.input),
            Some(25),
            "full={full}"
        );
        assert_eq!(
            view.series
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .filter(|row| row.classification == ObservationClassification::ResetAdvance)
                .count(),
            1,
            "full={full}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

/// One synthetic Codex rollout, written under a per-test store root so a
/// usage read can be driven end to end through the ordinary resolver.
fn codex_rollout(tag: &str, id: &str, records: &[serde_json::Value]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tapes-codex-{tag}-{}", std::process::id()));
    let sessions = root.join("sessions/2026/01/01");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&sessions).unwrap();
    let lines = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        sessions.join(format!("rollout-2026-01-01T12-00-00-{id}.jsonl")),
        format!("{lines}\n"),
    )
    .unwrap();
    root
}

fn codex_meta(id: &str) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2026-01-01T12:00:00Z",
        "type": "session_meta",
        "payload": {"id": id, "session_id": id, "cwd": "/fixtures/observed"}
    })
}

fn codex_turn_context(model: &str) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2026-01-01T12:00:01Z",
        "type": "turn_context",
        "payload": {"cwd": "/fixtures/observed", "model": model, "effort": "high"}
    })
}

fn codex_usage_record(ordinal: u64, response_id: &str, input: u64) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2026-01-01T12:00:02Z",
        "type": "token_usage_record",
        "ordinal": ordinal,
        "payload": {
            "response_id": response_id,
            "usage": {"input_tokens": input, "output_tokens": 1}
        }
    })
}

fn codex_token_count(ordinal: u64, total: u64, last: u64) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2026-01-01T12:00:03Z",
        "type": "event_msg",
        "ordinal": ordinal,
        "payload": {
            "type": "token_count",
            "info": {
                "total_token_usage": {"input_tokens": total, "output_tokens": 1},
                "last_token_usage": {"input_tokens": last, "output_tokens": 1}
            },
            "rate_limits": null
        }
    })
}

fn non_utf8_test_roots(tag: &str) -> (PathBuf, PathBuf) {
    let mut name = OsString::from(format!("tapes-{tag}-{}", std::process::id()));
    name.push(OsString::from_vec(vec![b'-', 0xff]));
    let native = std::env::temp_dir().join(name);
    let display_shadow = PathBuf::from(native.to_string_lossy().into_owned());
    let _ = fs::remove_dir_all(&native);
    let _ = fs::remove_dir_all(&display_shadow);
    (native, display_shadow)
}

fn write_jsonl_records(path: &Path, records: &[serde_json::Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines = records
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{lines}\n")).unwrap();
}

#[test]
fn codex_native_path_survives_transcript_history_full_usage_and_parent_reads() {
    let id = "91000000-0000-0000-0000-000000000001";
    let parent_id = "91000000-0000-0000-0000-000000000002";
    let (root, shadow) = non_utf8_test_roots("codex-native-location");
    let session_path = root
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T12-00-00-{id}.jsonl"));
    let parent_path = root
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T11-00-00-{parent_id}.jsonl"));
    let shadow_path = PathBuf::from(session_path.to_string_lossy().into_owned());

    let mut header = codex_meta(id);
    header["payload"]["parent_thread_id"] = parent_id.into();
    write_jsonl_records(
        &session_path,
        &[
            header,
            codex_turn_context("gpt-native-location"),
            codex_usage_record(2, "native-response", 10),
            codex_token_count(3, 11, 11),
            serde_json::json!({
                "timestamp": "2026-01-01T12:00:04Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "selected Codex recording"}]
                }
            }),
        ],
    );
    write_jsonl_records(&parent_path, &[codex_meta(parent_id)]);
    write_jsonl_records(
        &shadow_path,
        &[
            codex_meta(id),
            serde_json::json!({
                "timestamp": "2026-01-01T12:00:04Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "replacement-character shadow"}]
                }
            }),
        ],
    );

    let backend = CodexBackend::new(&root);
    let session = located(&backend, id);
    assert_eq!(
        session.locator(),
        Some(session_path.display().to_string().as_str())
    );
    assert_ne!(session_path, shadow_path);

    let transcript = backend.transcript(&session, 10).unwrap();
    assert!(transcript
        .turns
        .iter()
        .any(|turn| turn.text.contains("selected Codex recording")));

    let page = backend.history_page(&session, None, 1024).unwrap();
    assert!(page
        .turns
        .iter()
        .any(|turn| turn.text.contains("selected Codex recording")));

    let mut streamed_text = String::new();
    let read = backend
        .stream_transcript(&session, None, &mut |turn| {
            streamed_text.push_str(&turn.text);
            Ok(())
        })
        .unwrap();
    assert!(streamed_text.contains("selected Codex recording"));
    let whole = backend.stream_session(&session, &read).unwrap();
    assert_eq!(
        whole.tokens.as_ref().and_then(|tokens| tokens.input),
        Some(11)
    );

    let usage = backend
        .usage_observations(&session, None, UsageObservationOptions { limit: 20 })
        .unwrap();
    assert!(usage.series.returned > 0);

    let lineage = backend.lineage(&session).unwrap();
    assert_eq!(
        lineage
            .parent
            .as_ref()
            .map(|parent| parent.native_id.as_str()),
        Some(parent_id)
    );
    assert!(lineage.parent.unwrap().resolved);

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(shadow).unwrap();
}

#[test]
fn claude_native_parent_path_survives_history_full_and_child_joins() {
    let id = "92000000-0000-4000-8000-000000000001";
    let (root, shadow) = non_utf8_test_roots("claude-native-location");
    let project = PathBuf::from("-fixtures-project");
    let parent_path = root.join(&project).join(format!("{id}.jsonl"));
    let shadow_parent = PathBuf::from(parent_path.to_string_lossy().into_owned());
    let child_path = parent_path
        .with_extension("")
        .join("subagents/agent-agentx.jsonl");
    let shadow_child = PathBuf::from(child_path.to_string_lossy().into_owned());
    let parent_record = serde_json::json!({
        "type": "user",
        "sessionId": id,
        "uuid": "parent-turn",
        "timestamp": "2026-01-01T10:00:00Z",
        "cwd": "/fixtures/project",
        "isSidechain": false,
        "message": {"role": "user", "content": "selected Claude parent"}
    });
    let child_record = serde_json::json!({
        "type": "user",
        "sessionId": id,
        "uuid": "child-turn",
        "timestamp": "2026-01-01T10:00:01Z",
        "cwd": "/fixtures/project",
        "isSidechain": true,
        "agentId": "agentx",
        "message": {"role": "user", "content": "selected Claude child"}
    });
    let shadow_parent_record = serde_json::json!({
        "type": "user",
        "sessionId": id,
        "uuid": "shadow-parent-turn",
        "timestamp": "2026-01-01T10:00:00Z",
        "cwd": "/fixtures/project",
        "isSidechain": false,
        "message": {"role": "user", "content": "replacement-character shadow parent"}
    });
    let shadow_child_record = serde_json::json!({
        "type": "user",
        "sessionId": id,
        "uuid": "shadow-child-turn",
        "timestamp": "2026-01-01T10:00:01Z",
        "cwd": "/fixtures/project",
        "isSidechain": true,
        "agentId": "agentx",
        "message": {"role": "user", "content": "replacement-character shadow child"}
    });
    write_jsonl_records(&parent_path, &[parent_record]);
    write_jsonl_records(&child_path, &[child_record]);
    write_jsonl_records(&shadow_parent, &[shadow_parent_record]);
    write_jsonl_records(&shadow_child, &[shadow_child_record]);

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, id);
    let transcript = backend.transcript(&session, 10).unwrap();
    assert!(transcript
        .turns
        .iter()
        .any(|turn| turn.text.contains("selected Claude parent")));

    let page = backend.history_page(&session, None, 1024).unwrap();
    assert!(page
        .turns
        .iter()
        .any(|turn| turn.text.contains("selected Claude parent")));

    let mut streamed_parent = String::new();
    let read = backend
        .stream_transcript(&session, None, &mut |turn| {
            streamed_parent.push_str(&turn.text);
            Ok(())
        })
        .unwrap();
    assert!(streamed_parent.contains("selected Claude parent"));
    backend.stream_session(&session, &read).unwrap();

    let child = backend.child_transcript(&session, "agentx").unwrap();
    assert!(child
        .turns
        .iter()
        .any(|turn| turn.text.contains("selected Claude child")));

    let mut streamed_child = String::new();
    backend
        .stream_child_transcript(&session, "agentx", &mut |turn| {
            streamed_child.push_str(&turn.text);
            Ok(())
        })
        .unwrap();
    assert!(streamed_child.contains("selected Claude child"));
    assert!(backend
        .lineage(&session)
        .unwrap()
        .children
        .iter()
        .any(|child| child.resolved));

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(shadow).unwrap();
}

#[test]
fn pi_native_path_survives_transcript_full_and_parent_reads() {
    let id = "session-native-location";
    let (root, shadow) = non_utf8_test_roots("pi-native-location");
    let name = format!("2026-01-01T10-00-00-000Z_{id}.jsonl");
    let session_path = root.join("fixture-project").join(&name);
    let shadow_path = PathBuf::from(session_path.to_string_lossy().into_owned());
    let records = |parent: &str, text: &str| {
        vec![
            serde_json::json!({
                "type": "session",
                "id": id,
                "timestamp": "2026-01-01T10:00:00Z",
                "cwd": "/fixtures/project",
                "parentSession": parent
            }),
            serde_json::json!({
                "type": "message",
                "id": "entry-user",
                "parentId": null,
                "timestamp": "2026-01-01T10:00:01Z",
                "message": {"role": "user", "content": text}
            }),
        ]
    };
    write_jsonl_records(
        &session_path,
        &records("selected-parent", "selected Pi recording"),
    );
    write_jsonl_records(
        &shadow_path,
        &records("shadow-parent", "replacement-character shadow"),
    );

    let backend = PiBackend::new(&root);
    let session = located(&backend, id);
    let transcript = backend.transcript(&session, 10).unwrap();
    assert!(transcript
        .turns
        .iter()
        .any(|turn| turn.text.contains("selected Pi recording")));

    let mut streamed_text = String::new();
    let read = backend
        .stream_transcript(&session, None, &mut |turn| {
            streamed_text.push_str(&turn.text);
            Ok(())
        })
        .unwrap();
    assert!(streamed_text.contains("selected Pi recording"));
    backend.stream_session(&session, &read).unwrap();

    let lineage = backend.lineage(&session).unwrap();
    assert_eq!(
        lineage
            .parent
            .as_ref()
            .map(|parent| parent.native_id.as_str()),
        Some("selected-parent")
    );

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(shadow).unwrap();
}

fn codex_usage(root: &Path, id: &str) -> tapes_core::usage::UsageView {
    codex_usage_with_options(root, id, false, None).unwrap()
}

fn codex_usage_with_options(
    root: &Path,
    id: &str,
    full: bool,
    series_limit: Option<usize>,
) -> anyhow::Result<tapes_core::usage::UsageView> {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(root))];
    usage_with_options_with_backends(
        &backends,
        Selection::Id(id),
        UsageOptions {
            full,
            series: series_limit.map(|limit| UsageObservationOptions { limit }),
        },
    )
}

fn codex_rollout_text(tag: &str, id: &str, contents: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("tapes-codex-{tag}-{}", std::process::id()));
    let sessions = root.join("sessions/2026/01/01");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("rollout-2026-01-01T12-00-00-{id}.jsonl")),
        contents,
    )
    .unwrap();
    root
}

fn codex_token_count_with_plan(
    ordinal: u64,
    total: u64,
    last: u64,
    plan: &str,
) -> serde_json::Value {
    let mut value = codex_token_count(ordinal, total, last);
    value["payload"]["rate_limits"] = serde_json::json!({"plan_type": plan});
    value
}

fn codex_padding_record(size: usize) -> serde_json::Value {
    serde_json::json!({
        "timestamp": "2026-01-01T12:00:01Z",
        "type": "padding",
        "padding": "x".repeat(size)
    })
}

#[derive(Clone, Copy)]
enum CodexMutation {
    Replace,
    InPlace,
}

struct MutatingCodexBackend {
    inner: CodexBackend,
    path: PathBuf,
    replacement: String,
    mutation: CodexMutation,
    mutated: Cell<bool>,
}

impl MutatingCodexBackend {
    fn mutate_after_first_stream(&self) {
        if self.mutated.replace(true) {
            return;
        }
        match self.mutation {
            CodexMutation::InPlace => fs::write(&self.path, &self.replacement).unwrap(),
            CodexMutation::Replace => {
                let replacement = self.path.with_extension("replacement");
                let _ = fs::remove_file(&replacement);
                fs::write(&replacement, &self.replacement).unwrap();
                fs::rename(replacement, &self.path).unwrap();
            }
        }
    }
}

impl Backend for MutatingCodexBackend {
    fn harness(&self) -> &'static str {
        self.inner.harness()
    }

    fn available(&self) -> bool {
        self.inner.available()
    }

    fn list(&self, query: &Query) -> anyhow::Result<Listing> {
        self.inner.list(query)
    }

    fn locate(&self, id: &str) -> anyhow::Result<Option<Session>> {
        self.inner.locate(id)
    }

    fn transcript(&self, session: &Session, tail: usize) -> anyhow::Result<Transcript> {
        self.inner.transcript(session, tail)
    }

    fn stream_transcript(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        turn: &mut dyn FnMut(Turn) -> anyhow::Result<()>,
    ) -> anyhow::Result<StreamedTranscript> {
        let read = self.inner.stream_transcript(session, replay, turn)?;
        self.mutate_after_first_stream();
        Ok(read)
    }

    fn kinds(&self) -> tapes_core::model::KindDeclaration {
        self.inner.kinds()
    }

    fn usage_observations(
        &self,
        session: &Session,
        read: Option<&tapes_core::model::ReadEvidence>,
        options: UsageObservationOptions,
    ) -> anyhow::Result<tapes_core::usage::UsageObservationResult> {
        self.inner.usage_observations(session, read, options)
    }
}

#[test]
fn codex_series_gap_omissions_count_unique_source_gaps_for_bounded_and_full_reads() {
    let id = "90000000-0000-0000-0000-000000000005";
    let mut source = serde_json::to_string(&codex_meta(id)).unwrap();
    source.push('\n');
    for index in 0..70 {
        source.push_str(&format!("malformed-{index}\n"));
    }
    let root = codex_rollout_text("gap-budget", id, &source);

    for full in [false, true] {
        let view = codex_usage_with_options(&root, id, full, Some(200)).unwrap();
        let series = view.series.unwrap();
        assert_eq!(series.gaps.len(), 64, "full={full}");
        assert_eq!(series.gaps_omitted, 6, "full={full}");
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn codex_modern_reset_evidence_survives_basis_selection_and_separates_coverage() {
    let id = "90000000-0000-0000-0000-000000000006";
    let records = vec![
        codex_meta(id),
        codex_turn_context("gpt-modern-reset"),
        codex_usage_record(2, "response-1", 10),
        codex_token_count(3, 10, 10),
        codex_usage_record(4, "response-2", 10),
        codex_token_count(5, 20, 10),
        codex_usage_record(6, "response-3", 5),
        codex_token_count(7, 5, 5),
    ];
    let root = codex_rollout("modern-reset", id, &records);

    for full in [false, true] {
        let view = codex_usage_with_options(&root, id, full, Some(16)).unwrap();
        assert_eq!(
            view.tokens.as_ref().and_then(|tokens| tokens.input),
            Some(5),
            "full={full}"
        );
        assert_eq!(
            view.accounting
                .as_ref()
                .map(|accounting| accounting.coverage),
            Some(AccountingCoverage::SinceReset),
            "full={full}"
        );
        let attribution = view.attribution.as_ref().unwrap();
        assert_eq!(
            attribution.basis,
            ObservationBasis::UsageRecord,
            "full={full}"
        );
        assert_eq!(
            attribution.coverage,
            AccountingCoverage::Session,
            "full={full}"
        );
        assert_eq!(attribution.resets, 1, "full={full}");
        assert_eq!(attribution.counted, 3, "full={full}");
        assert_eq!(
            view.by_model
                .as_ref()
                .and_then(|models| models.first())
                .and_then(|model| model.tokens.as_ref())
                .and_then(|tokens| tokens.input),
            Some(25),
            "full={full}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn codex_series_reports_row_byte_budget_omissions_without_dumping_rows() {
    let id = "90000000-0000-0000-0000-000000000007";
    let plan = "x".repeat(16_000);
    let mut records = vec![codex_meta(id), codex_turn_context("gpt-byte-budget")];
    for index in 0..600 {
        records.push(codex_token_count_with_plan(index, index + 1, 1, &plan));
    }
    let root = codex_rollout("series-byte-budget", id, &records);

    let view = codex_usage_with_options(&root, id, true, Some(1_000)).unwrap();
    let series = view.series.unwrap();
    assert_eq!(series.observed, 600);
    assert!(series.omissions.byte_budget > 0);
    assert_eq!(series.omissions.oversized_row, 0);
    assert!(series.returned < series.observed);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn codex_series_reports_an_oversized_row_as_an_omission() {
    let id = "90000000-0000-0000-0000-000000000008";
    let oversized_plan = "x".repeat(8 * 1024 * 1024 + 1_024);
    let records = vec![
        codex_meta(id),
        codex_turn_context("gpt-oversized-row"),
        codex_token_count_with_plan(2, 1, 1, &oversized_plan),
    ];
    let root = codex_rollout("series-oversized-row", id, &records);

    let view = codex_usage_with_options(&root, id, true, Some(8)).unwrap();
    let series = view.series.unwrap();
    assert_eq!(series.observed, 1);
    assert_eq!(series.returned, 0);
    assert_eq!(series.omissions.oversized_row, 1);
    assert_eq!(series.omissions.byte_budget, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn codex_bounded_prefix_reports_leading_uncounted_observation() {
    let id = "90000000-0000-0000-0000-000000000009";
    let records = vec![
        codex_meta(id),
        codex_padding_record(80 * 1024),
        codex_turn_context("gpt-bounded-reset"),
        codex_token_count(2, 10, 10),
        codex_token_count(3, 20, 10),
        codex_token_count(4, 5, 5),
    ];
    let root = codex_rollout("leading-uncounted", id, &records);
    let backend = CodexBackend::new(&root).with_read_bytes(MIN_READ_BYTES);
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(backend)];

    let view = usage_with_options_with_backends(
        &backends,
        Selection::Id(id),
        UsageOptions {
            full: false,
            series: Some(UsageObservationOptions { limit: 8 }),
        },
    )
    .unwrap();
    let attribution = view.attribution.as_ref().unwrap();
    assert_eq!(attribution.leading_uncounted, 1);
    assert_eq!(attribution.coverage, AccountingCoverage::ReadWindow);
    assert_eq!(attribution.resets, 1);
    assert_eq!(attribution.counted, 2);
    assert_eq!(
        view.accounting
            .as_ref()
            .map(|accounting| accounting.coverage),
        Some(AccountingCoverage::SinceReset)
    );
    assert_eq!(
        view.by_model
            .as_ref()
            .and_then(|models| models.first())
            .and_then(|model| model.tokens.as_ref())
            .and_then(|tokens| tokens.input),
        Some(15)
    );
    assert!(attribution
        .incomplete
        .iter()
        .any(|reason| reason == "leading-uncounted"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn codex_bounded_modern_reset_evidence_keeps_read_window_coverage() {
    let id = "90000000-0000-0000-0000-000000000012";
    let records = vec![
        codex_meta(id),
        codex_padding_record(80 * 1024),
        codex_turn_context("gpt-bounded-modern-reset"),
        codex_usage_record(2, "bounded-response-1", 10),
        codex_token_count(3, 10, 10),
        codex_usage_record(4, "bounded-response-2", 10),
        codex_token_count(5, 20, 10),
        codex_usage_record(6, "bounded-response-3", 5),
        codex_token_count(7, 5, 5),
    ];
    let root = codex_rollout("bounded-modern-reset", id, &records);
    let backend = CodexBackend::new(&root).with_read_bytes(MIN_READ_BYTES);
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(backend)];

    let view = usage_with_options_with_backends(
        &backends,
        Selection::Id(id),
        UsageOptions {
            full: false,
            series: Some(UsageObservationOptions { limit: 16 }),
        },
    )
    .unwrap();
    assert_eq!(
        view.tokens.as_ref().and_then(|tokens| tokens.input),
        Some(5)
    );
    assert_eq!(
        view.accounting
            .as_ref()
            .map(|accounting| accounting.coverage),
        Some(AccountingCoverage::SinceReset)
    );
    let attribution = view.attribution.as_ref().unwrap();
    assert_eq!(attribution.basis, ObservationBasis::UsageRecord);
    assert_eq!(attribution.coverage, AccountingCoverage::ReadWindow);
    assert_eq!(attribution.resets, 1);
    assert_eq!(attribution.counted, 3);
    assert_eq!(
        view.by_model
            .as_ref()
            .and_then(|models| models.first())
            .and_then(|model| model.tokens.as_ref())
            .and_then(|tokens| tokens.input),
        Some(25)
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn codex_response_id_dedup_budget_reports_skipped_counting() {
    let id = "90000000-0000-0000-0000-000000000010";
    let mut records = vec![codex_meta(id), codex_turn_context("gpt-response-budget")];
    for index in 0..20_000 {
        records.push(codex_usage_record(index, &format!("response-{index}"), 1));
    }
    let root = codex_rollout("response-id-budget", id, &records);

    let view = codex_usage_with_options(&root, id, true, Some(1)).unwrap();
    let attribution = view.attribution.as_ref().unwrap();
    assert_eq!(attribution.observed, 20_000);
    assert!(attribution.counted < attribution.observed);
    assert!(attribution
        .incomplete
        .iter()
        .any(|reason| reason == "response-id-dedup-incomplete"));
    assert!(attribution
        .bounds
        .iter()
        .any(|reason| reason == "response-id-budget"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn public_full_usage_refuses_replacement_or_in_place_source_rewrite_between_passes() {
    let id = "90000000-0000-0000-0000-000000000011";
    let initial_records = [
        codex_meta(id),
        codex_turn_context("gpt-pinned"),
        codex_usage_record(2, "response-1", 1),
    ];
    let replacement_records = [
        codex_meta(id),
        codex_turn_context("gpt-pinned"),
        codex_usage_record(2, "response-1", 2),
    ];
    let initial = initial_records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let replacement = replacement_records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");

    for mutation in [CodexMutation::Replace, CodexMutation::InPlace] {
        let root = codex_rollout_text("pinned-mutation", id, &format!("{initial}\n"));
        let path = root
            .join("sessions/2026/01/01")
            .join(format!("rollout-2026-01-01T12-00-00-{id}.jsonl"));
        let backend = MutatingCodexBackend {
            inner: CodexBackend::new(&root),
            path,
            replacement: format!("{replacement}\n"),
            mutation,
            mutated: Cell::new(false),
        };
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(backend)];
        let result = usage_with_options_with_backends(
            &backends,
            Selection::Id(id),
            UsageOptions {
                full: true,
                series: Some(UsageObservationOptions { limit: 8 }),
            },
        );
        let error = match result {
            Ok(_) => panic!("a pinned usage replay must not mix a changed source"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("source")
                || error.to_string().contains("changed")
                || error.to_string().contains("replaced"),
            "{error:#}"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn codex_full_usage_rejects_roundtripped_evidence_without_prefix_integrity() {
    let id = "90000000-0000-0000-0000-000000000013";
    let records = vec![
        codex_meta(id),
        codex_turn_context("gpt-roundtrip-pin"),
        codex_usage_record(2, "roundtrip-response", 1),
    ];
    let root = codex_rollout("roundtrip-pin", id, &records);
    let path = root
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T12-00-00-{id}.jsonl"));
    let backend = CodexBackend::new(&root);
    let session = backend.locate(id).unwrap().unwrap();
    let first = backend
        .stream_transcript(&session, None, &mut |_| Ok(()))
        .unwrap();
    let native = first.read_evidence(None);
    let roundtripped: ReadEvidence =
        serde_json::from_slice(&serde_json::to_vec(&native).unwrap()).unwrap();

    let before = fs::metadata(&path).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    let replacement = content.replace("\"input_tokens\":1", "\"input_tokens\":9");
    assert_ne!(content, replacement);
    fs::write(&path, replacement).unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.dev(), after.dev());
    assert_eq!(before.ino(), after.ino());

    let native_result = backend.usage_observations(
        &session,
        Some(&native),
        UsageObservationOptions { limit: 200 },
    );
    let native_error = match native_result {
        Ok(_) => panic!("native evidence must reject the changed prefix"),
        Err(error) => error,
    };
    assert!(native_error.to_string().contains("changed"));

    let roundtripped_result = backend.usage_observations(
        &session,
        Some(&roundtripped),
        UsageObservationOptions { limit: 200 },
    );
    let roundtripped_error = match roundtripped_result {
        Ok(_) => panic!("round-tripped evidence without a prefix hash must fail closed"),
        Err(error) => error,
    };
    assert!(roundtripped_error.to_string().contains("prefix integrity"));

    fs::remove_dir_all(root).unwrap();
}

/// `distinct_observed` answers how many model selections a read actually saw,
/// not how often the selection changed: a recording that alternates between
/// two models observed two, however many times it switched.
#[test]
fn codex_model_observations_count_distinct_selections_not_switches() {
    let id = "90000000-0000-0000-0000-000000000001";
    let mut records = vec![codex_meta(id)];
    for model in ["gpt-alpha", "gpt-beta", "gpt-alpha", "gpt-beta"] {
        records.push(codex_turn_context(model));
    }
    let root = codex_rollout("distinct-selections", id, &records);

    let status = codex_usage(&root, id).session.model_observation.unwrap();
    assert!(status.mixed, "two selections alternated");
    assert_eq!(status.distinct_observed, Some(2));

    fs::remove_dir_all(root).unwrap();
}

/// Past the retained model-key budget no exact distinct count exists, so the
/// count is withheld and the budget is named instead of publishing the
/// truncated one as exact.
#[test]
fn codex_model_observations_withhold_the_count_past_the_key_budget() {
    let id = "90000000-0000-0000-0000-000000000002";
    let mut records = vec![codex_meta(id)];
    for index in 0..40 {
        records.push(codex_turn_context(&format!("gpt-m{index:02}")));
        records.push(codex_usage_record(index, &format!("response-{index}"), 1));
    }
    let root = codex_rollout("key-budget", id, &records);

    let view = codex_usage(&root, id);
    let status = view.session.model_observation.as_ref().unwrap();
    assert!(status.mixed);
    assert_eq!(status.distinct_observed, None);
    assert!(status.attribution_uncertain);
    let attribution = view.attribution.as_ref().unwrap();
    assert_eq!(attribution.attributed, 32);
    assert_eq!(attribution.unattributed, 8);
    assert!(attribution
        .bounds
        .iter()
        .any(|reason| reason == "model-key-budget"));

    fs::remove_dir_all(root).unwrap();
}

/// A modern request record selects the modern basis for the whole read, and
/// the legacy observations that preceded it are discarded rather than summed
/// into it. The read says so: its attribution names the observations the
/// basis cannot account for instead of presenting a fraction of the session
/// as complete session coverage.
#[test]
fn codex_attribution_names_legacy_observations_the_modern_basis_discards() {
    let id = "90000000-0000-0000-0000-000000000003";
    let records = vec![
        codex_meta(id),
        codex_turn_context("gpt-late"),
        codex_token_count(2, 10, 10),
        codex_token_count(3, 25, 15),
        codex_token_count(4, 40, 15),
        codex_usage_record(5, "response-late-1", 7),
    ];
    let root = codex_rollout("late-modern", id, &records);

    let view = codex_usage(&root, id);
    assert_eq!(
        view.tokens.as_ref().and_then(|tokens| tokens.input),
        Some(40)
    );
    let attribution = view.attribution.as_ref().unwrap();
    assert_eq!(attribution.basis, ObservationBasis::UsageRecord);
    assert_eq!(attribution.counted, 1);
    assert!(
        attribution
            .incomplete
            .iter()
            .any(|reason| reason == "legacy-observations-before-modern-basis"),
        "{attribution:?}"
    );
    assert!(
        view.session
            .model_observation
            .as_ref()
            .unwrap()
            .attribution_uncertain
    );

    fs::remove_dir_all(root).unwrap();
}

/// Modern rollouts write a `token_count` beside their request records, so a
/// legacy observation that follows the basis is the ordinary shape and is not
/// reported as evidence the basis missed anything.
#[test]
fn codex_attribution_stays_complete_when_legacy_events_follow_the_modern_basis() {
    let id = "90000000-0000-0000-0000-000000000004";
    let records = vec![
        codex_meta(id),
        codex_turn_context("gpt-modern"),
        codex_usage_record(2, "response-modern-1", 10),
        codex_token_count(3, 10, 10),
        codex_usage_record(4, "response-modern-2", 20),
        codex_token_count(5, 30, 20),
    ];
    let root = codex_rollout("modern-with-events", id, &records);

    let view = codex_usage(&root, id);
    let attribution = view.attribution.as_ref().unwrap();
    assert_eq!(attribution.basis, ObservationBasis::UsageRecord);
    assert_eq!(attribution.counted, 2);
    assert!(attribution.incomplete.is_empty(), "{attribution:?}");
    assert!(
        !view
            .session
            .model_observation
            .as_ref()
            .unwrap()
            .attribution_uncertain
    );

    fs::remove_dir_all(root).unwrap();
}

/// A Claude `cost-state` carries wall-clock durations and the per-model split
/// beside its totals. A recording without one carries neither.
#[test]
fn claude_usage_reports_cost_state_durations_and_the_per_model_split() {
    let root = std::env::temp_dir().join(format!("tapes-claude-usage-{}", std::process::id()));
    let project = root.join("project");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("session-usage.jsonl"),
        concat!(
            r#"{"type":"user","sessionId":"session-usage","uuid":"user-1","timestamp":"2026-01-01T10:00:00Z","cwd":"/fixtures/project","message":{"role":"user","content":"Inspect the fixture."}}"#,
            "\n",
            r#"{"type":"cost-state","sessionId":"session-usage","totalCostUSD":12.5,"totalAPIDuration":5000,"totalAPIDurationWithoutRetries":4000,"totalToolDuration":3000,"totalDuration":9000,"modelUsage":{"claude-second":{"inputTokens":1,"outputTokens":2,"thinkingTokens":3,"cacheReadInputTokens":4,"cacheCreationInputTokens":5,"costUSD":0.5},"claude-first":{"inputTokens":100,"outputTokens":200,"thinkingTokens":300,"cacheReadInputTokens":400,"cacheCreationInputTokens":500,"costUSD":12.0}}}"#,
            "\n"
        ),
    )
    .unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "session-usage");
    let view = usage(&backend.transcript(&session, usize::MAX).unwrap());

    assert_eq!(
        view.durations_ms,
        Some(Durations {
            api: Some(5_000),
            api_without_retries: Some(4_000),
            tool: Some(3_000),
            total: Some(9_000),
        })
    );
    assert_eq!(
        view.by_model,
        Some(vec![
            ModelUsage {
                model: "claude-first".to_owned(),
                variant: None,
                tokens: Some(Tokens {
                    input: Some(100),
                    output: Some(200),
                    reasoning: Some(300),
                    cache_read: Some(400),
                    cache_write: Some(500),
                }),
                cost: Some(Cost { usd: 12.0 }),
                request_count: None,
            },
            ModelUsage {
                model: "claude-second".to_owned(),
                variant: None,
                tokens: Some(Tokens {
                    input: Some(1),
                    output: Some(2),
                    reasoning: Some(3),
                    cache_read: Some(4),
                    cache_write: Some(5),
                }),
                cost: Some(Cost { usd: 0.5 }),
                request_count: None,
            },
        ])
    );
    assert!(view.context_window.is_none(), "Claude records no window");
    assert!(view.rate_limits.is_none(), "Claude records no quota");
    fs::remove_dir_all(root).unwrap();

    let fixture = ClaudeBackend::new(fixtures("claude"));
    let session = located(&fixture, "session-claude");
    let view = usage(&fixture.transcript(&session, usize::MAX).unwrap());
    assert!(view.durations_ms.is_none(), "no cost-state, no durations");
    assert!(view.by_model.is_none(), "no cost-state, no per-model split");
}

/// A harness that records none of the optional usage facts omits each object
/// rather than emitting an empty or null one.
#[test]
fn a_harness_without_usage_detail_omits_every_optional_object() {
    let opencode = OpenCodeBackend::new(opencode_fixture_program());
    let opencode_session = located(&opencode, "ses_000000fixtureSharedSession");
    let pi = PiBackend::new(fixtures("pi"));
    let pi_session = located(&pi, "session-pi");

    let view = usage(&opencode.transcript(&opencode_session, usize::MAX).unwrap());
    assert!(view.context_window.is_none(), "opencode records no window");
    assert!(view.rate_limits.is_none(), "opencode records no quota");
    assert!(view.durations_ms.is_none(), "opencode records no durations");
    assert!(view.by_model.is_none(), "opencode records no model split");

    let value = serde_json::to_value(&view).unwrap();
    for absent in ["context_window", "rate_limits", "durations_ms", "by_model"] {
        assert!(value.get(absent).is_none(), "{absent} in {value}");
    }
    assert_eq!(
        view.turns.total,
        view.turns.user + view.turns.assistant + view.turns.tool + view.turns.reasoning
    );

    // pi records the model split and nothing else in `usage_detail`.
    let view = usage(&pi.transcript(&pi_session, usize::MAX).unwrap());
    let value = serde_json::to_value(&view).unwrap();
    assert_eq!(value["by_model"][0]["model"], "gpt-fixture");
    for absent in ["context_window", "rate_limits", "durations_ms"] {
        assert!(value.get(absent).is_none(), "{absent} in {value}");
    }
}

/// A read that stopped at the file tail counted only the turns it reached.
#[test]
fn a_bounded_read_reports_its_turn_counts_as_a_read_window() {
    let root = std::env::temp_dir().join(format!("tapes-usage-bounds-{}", std::process::id()));
    let project = root.join("project");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&project).unwrap();
    let mut file = BufWriter::new(File::create(project.join("bounded-usage.jsonl")).unwrap());
    file.write_all(b"{\"padding\":\"").unwrap();
    file.write_all(&vec![b'x'; 4 * 1024 * 1024]).unwrap();
    file.write_all(b"\"}\n").unwrap();
    writeln!(
        file,
        r#"{{"type":"user","sessionId":"bounded-usage","timestamp":"2026-01-01T12:00:00Z","cwd":"/fixtures/project","message":{{"role":"user","content":"Tail input."}}}}"#
    )
    .unwrap();
    file.flush().unwrap();

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "bounded-usage");
    let view = usage(&backend.transcript(&session, usize::MAX).unwrap());

    assert_eq!(view.turns.total, 1);
    assert_eq!(view.turns.coverage, TurnCoverage::ReadWindow);
    assert!(view.truncated);
    fs::remove_dir_all(root).unwrap();
}

/// A store that lists two sessions and can no longer read one of them.
struct BulkExportFixture {
    sessions: Vec<Session>,
    unreadable: &'static str,
}

impl Backend for BulkExportFixture {
    fn kinds(&self) -> tapes_core::model::KindDeclaration {
        tapes_core::model::KindDeclaration {
            recordable: tapes_core::model::TurnSelection::only(TurnKind::ALL),
            user_default: None,
        }
    }

    fn harness(&self) -> &'static str {
        "fixture"
    }

    fn available(&self) -> bool {
        true
    }

    fn list(&self, query: &Query) -> anyhow::Result<Listing> {
        Ok(Listing::from_sessions(
            self.sessions.iter().take(query.limit).cloned().collect(),
        ))
    }

    fn locate(&self, _id: &str) -> anyhow::Result<Option<Session>> {
        unreachable!("a selection exports through the backend that listed it")
    }

    fn transcript(&self, session: &Session, _tail: usize) -> anyhow::Result<Transcript> {
        if session.id == self.unreadable {
            anyhow::bail!("the recording is gone");
        }
        Ok(Transcript {
            session: session.clone(),
            turns: vec![Turn {
                role: Role::User,
                kind: TurnKind::Operator,
                text: "rescue me".into(),
                ts: None,
                ordinal: 0,
                native_id: None,
                request_turn_id: None,
                metadata: None,
                record_ref: None,
                parts: Vec::new(),
                coverage: None,
                channel: None,
                recipient: None,
                tool: None,
            }],
            truncated: false,
            truncation: Truncation::default(),
            read: None,
            terminal: None,
            text_tail: None,
            artifacts: Vec::new(),
            graph: None,
            trailing_record: None,
            notes: Vec::new(),
            projection: None,
            kinds: None,
        })
    }
}

fn export_directory(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("tapes-selection-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    directory
}

fn selection() -> SessionSelection<'static> {
    SessionSelection {
        within: Where::Global,
        harness: Some("fixture"),
        limit: None,
        filters: ListFilters::default(),
        sort: ListSort::Newest,
    }
}

/// A store that vanishes between the listing and the read costs its own
/// session and nothing else: the rest of the selection is exported and the
/// manifest names the one that failed.
#[test]
fn a_selection_exports_every_readable_session_and_records_the_rest() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(BulkExportFixture {
        sessions: vec![
            resolver_session("ses_broken"),
            resolver_session("ses_readable"),
        ],
        unreadable: "ses_broken",
    })];
    let directory = export_directory("partial");

    let export = export_selection_with_backends(
        &backends,
        &selection(),
        Some(&directory),
        None,
        tapes_core::ExportRead::Bounded,
    )
    .unwrap();

    assert_eq!(export.manifest.schema, EXPORT_MANIFEST_SCHEMA);
    assert_eq!(
        export
            .manifest
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["ses_readable"]
    );
    assert_eq!(export.bundles.len(), 1);
    assert_eq!(export.manifest.failed.len(), 1);
    assert_eq!(export.manifest.failed[0].id, "ses_broken");
    assert_eq!(export.manifest.failed[0].harness, "fixture");
    assert!(
        export.manifest.failed[0]
            .error
            .contains("the recording is gone"),
        "{}",
        export.manifest.failed[0].error
    );
    assert!(!export.every_session_failed());

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(directory.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["schema"], EXPORT_MANIFEST_SCHEMA);
    assert_eq!(manifest["sessions"][0]["id"], "ses_readable");
    assert_eq!(manifest["failed"][0]["id"], "ses_broken");
    for path in ["context", "json", "trace"] {
        let file = Path::new(manifest["sessions"][0]["files"][path].as_str().unwrap());
        assert!(file.is_file(), "{} is missing", file.display());
    }

    fs::remove_dir_all(directory).unwrap();
}

/// Nothing selected could be read. The manifest still records the selection,
/// and the caller is told it holds no session.
#[test]
fn a_selection_whose_every_session_failed_says_so() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(BulkExportFixture {
        sessions: vec![resolver_session("ses_broken")],
        unreadable: "ses_broken",
    })];
    let directory = export_directory("total-failure");

    let export = export_selection_with_backends(
        &backends,
        &selection(),
        Some(&directory),
        None,
        tapes_core::ExportRead::Bounded,
    )
    .unwrap();

    assert!(export.bundles.is_empty());
    assert!(export.every_session_failed());
    assert_eq!(
        fs::read_dir(&directory).unwrap().count(),
        1,
        "only the manifest is written"
    );

    fs::remove_dir_all(directory).unwrap();
}

/// An empty selection is a fact, not a failure: the manifest records what was
/// asked for and holds no session.
#[test]
fn an_empty_selection_writes_a_manifest_and_no_bundle() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(BulkExportFixture {
        sessions: Vec::new(),
        unreadable: "",
    })];
    let directory = export_directory("empty");

    let export = export_selection_with_backends(
        &backends,
        &selection(),
        Some(&directory),
        None,
        tapes_core::ExportRead::Bounded,
    )
    .unwrap();

    assert!(export.manifest.sessions.is_empty());
    assert!(export.manifest.failed.is_empty());
    assert!(!export.every_session_failed());
    assert_eq!(export.manifest.selection.limit, 20);

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn selection_stats_preserve_read_failures_without_counting_them_as_zero_activity() {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(BulkExportFixture {
        sessions: vec![resolver_session("readable"), resolver_session("gone")],
        unreadable: "gone",
    })];
    let summary = tapes_core::stats_summary::with_backends(
        &backends,
        &selection(),
        tapes_core::stats_summary::SessionRead::Bounded,
    )
    .unwrap();
    assert_eq!(summary.selected, 2);
    assert_eq!(summary.read, 1);
    assert_eq!(summary.failed.len(), 1);
    assert_eq!(summary.failed[0].id, "gone");
    assert_eq!(summary.sessions.len(), 1);
    assert_eq!(summary.sessions[0].session.id, "readable");
}

struct TitleProjection {
    title: &'static str,
    origin: &'static str,
}
impl Backend for TitleProjection {
    fn shares_session_ids(&self) -> bool {
        true
    }

    fn kinds(&self) -> tapes_core::model::KindDeclaration {
        tapes_core::model::KindDeclaration {
            recordable: tapes_core::model::TurnSelection::only(TurnKind::ALL),
            user_default: None,
        }
    }

    fn locate(&self, _: &str) -> anyhow::Result<Option<Session>> {
        panic!("title selection must not re-resolve an ID")
    }
    fn harness(&self) -> &'static str {
        "opencode"
    }
    fn available(&self) -> bool {
        true
    }
    fn list(&self, _: &Query) -> anyhow::Result<Listing> {
        let mut session = resolver_session("shared");
        session.source = SourceDescriptor::installed("opencode", self.origin);
        session.title = Some(self.title.into());
        Ok(Listing::from_sessions(vec![session]))
    }
    fn transcript(&self, session: &Session, _: usize) -> anyhow::Result<Transcript> {
        assert_eq!(session.locator(), Some(self.origin));
        Ok(Transcript::new(
            session.clone(),
            vec![],
            Truncation::default(),
            None,
            vec![],
        ))
    }
}
#[test]
fn title_resolution_retains_the_first_opencode_projection_even_for_a_nonmatch() {
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(TitleProjection {
            title: "first",
            origin: "primary",
        }),
        Box::new(TitleProjection {
            title: "second",
            origin: "secondary",
        }),
    ];
    let selection = |title| Selection::Title {
        title,
        within: Where::Global,
        harness: Some("opencode"),
    };
    let read = show_with_backends(&backends, selection("first"), 10).unwrap();
    assert_eq!(read.session.locator(), Some("primary"));
    assert!(show_with_backends(&backends, selection("second"), 10)
        .unwrap_err()
        .to_string()
        .contains("was not found"));
}
