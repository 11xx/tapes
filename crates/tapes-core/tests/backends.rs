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
use tapes_core::event::{project, EventKind, Incomplete};
use tapes_core::model::{
    Accounting, AccountingBasis, AccountingCoverage, Cost, Role, Session, SourceBound,
    SourceDescriptor, Tokens, Transcript, Truncation, Turn, TurnKind,
};
use tapes_core::usage::{usage, Durations, ModelUsage, RateWindow, TurnCoverage};
use tapes_core::{
    export_selection_with_backends, latest_with_backends, list_with_backends,
    list_with_backends_filtered, list_with_backends_filtered_and_search,
    list_with_backends_options, resolve_session, scope::Scope, show_with_backends, ListFilters,
    ListSort, ResolveError, Selection, SessionSelection, Where, EXPORT_MANIFEST_SCHEMA,
    LIST_SEARCH_TAIL,
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
        source: SourceDescriptor::installed("fixture", "fixture-recording"),
        metadata: None,
        model: None,
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
        r#"{{"type":"cost-state","sessionId":"session-truncated-cost","totalCostUSD":3.5,"modelUsage":{{"claude-fixture":{{"inputTokens":10,"outputTokens":20,"thinkingTokens":30,"cacheReadInputTokens":40,"cacheCreationInputTokens":50}}}},"hasUnknownModelCost":false}}"#
    )
    .unwrap();
    drop(file);
    assert!(fs::metadata(&path).unwrap().len() > 4 * 1024 * 1024);

    let backend = ClaudeBackend::new(&root);
    let session = located(&backend, "session-truncated-cost");
    assert_eq!(
        session.tokens,
        Some(Tokens {
            input: Some(10),
            output: Some(20),
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

/// Codex records the operator's own messages as `user_message` events, and an
/// `exec` session records none because its caller supplies the one prompt.
#[test]
fn codex_types_user_messages_from_its_own_records() {
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
        "an exec session's prompt needs no event"
    );
    assert_eq!(
        kinds("20000000-0000-0000-0000-000000000004"),
        vec![TurnKind::Unknown],
        "a message with neither evidence is not guessed at"
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
    assert!(view.by_model.is_none(), "Codex records no per-model split");

    let without = located(&backend, "20000000-0000-0000-0000-000000000004");
    let without = usage(&backend.transcript(&without, usize::MAX).unwrap());
    assert!(without.context_window.is_none());
    assert!(without.rate_limits.is_none());
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
                tokens: Some(Tokens {
                    input: Some(100),
                    output: Some(200),
                    reasoning: Some(300),
                    cache_read: Some(400),
                    cache_write: Some(500),
                }),
                cost: Some(Cost { usd: 12.0 }),
            },
            ModelUsage {
                model: "claude-second".to_owned(),
                tokens: Some(Tokens {
                    input: Some(1),
                    output: Some(2),
                    reasoning: Some(3),
                    cache_read: Some(4),
                    cache_write: Some(5),
                }),
                cost: Some(Cost { usd: 0.5 }),
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

    for (backend, session) in [
        (&opencode as &dyn Backend, opencode_session),
        (&pi as &dyn Backend, pi_session),
    ] {
        let view = usage(&backend.transcript(&session, usize::MAX).unwrap());
        assert!(view.context_window.is_none(), "{}", session.harness());
        assert!(view.rate_limits.is_none(), "{}", session.harness());
        assert!(view.durations_ms.is_none(), "{}", session.harness());
        assert!(view.by_model.is_none(), "{}", session.harness());

        let value = serde_json::to_value(&view).unwrap();
        for absent in ["context_window", "rate_limits", "durations_ms", "by_model"] {
            assert!(value.get(absent).is_none(), "{absent} in {value}");
        }
        assert_eq!(
            view.turns.total,
            view.turns.user + view.turns.assistant + view.turns.tool + view.turns.reasoning
        );
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

    let export = export_selection_with_backends(&backends, &selection(), Some(&directory)).unwrap();

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

    let export = export_selection_with_backends(&backends, &selection(), Some(&directory)).unwrap();

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

    let export = export_selection_with_backends(&backends, &selection(), Some(&directory)).unwrap();

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
    let summary = tapes_core::stats_summary::with_backends(&backends, &selection()).unwrap();
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
