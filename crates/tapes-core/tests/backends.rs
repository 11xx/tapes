use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{TimeZone, Utc};
use tapes_core::backend::claude::ClaudeBackend;
use tapes_core::backend::codex::CodexBackend;
use tapes_core::backend::opencode::OpenCodeBackend;
use tapes_core::backend::pi::PiBackend;
use tapes_core::backend::{Backend, Listing, Query};
use tapes_core::model::{Role, Session, Transcript};
use tapes_core::{
    latest_with_backends, list_with_backends, resolve_session, scope::Scope, show_with_backends,
    ResolveError, Selection,
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
}

impl OpenCodeAlias {
    fn new(tag: &str) -> Self {
        let serial = OPENCODE_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("opencode2-{tag}-{}-{serial}", std::process::id()));
        let _ = fs::remove_file(&path);
        std::os::unix::fs::symlink(opencode_fixture_program(), &path).unwrap();
        Self { path, calls: None }
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

    let program = opencode_fixture_program();
    let metadata = fs::metadata(&program).unwrap();
    assert!(metadata.is_file(), "checked-in OpenCode fixture is missing");
    assert_ne!(metadata.permissions().mode() & 0o111, 0);
    assert!(!fs::symlink_metadata(&program)
        .unwrap()
        .file_type()
        .is_symlink());
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
            format!("/api/session/{id}/message")
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
    assert_eq!(listing.sessions.len(), 1);
    assert_eq!(listing.sessions[0].id, id);
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
        assert_eq!(
            transcript.notes,
            vec!["Skipped 1 unparseable line.".to_owned()]
        );
    }
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
    let filler = "x".repeat(4096);
    for index in 0..1200 {
        writeln!(
            file,
            r#"{{"type":"message","id":"user-{index}","parentId":null,"timestamp":"2026-01-01T10:00:01Z","message":{{"role":"user","content":[{{"type":"text","text":"{filler}"}}]}}}}"#
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
    assert!(
        session.derived_title.is_none(),
        "a bounded tail cannot prove which user turn was first"
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
        directory: None,
        started_at: timestamp,
        last_activity_at: timestamp,
        live: None,
        cost: None,
        tokens: None,
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
