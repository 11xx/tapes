use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use tapes_core::backend::claude::ClaudeBackend;
use tapes_core::backend::codex::CodexBackend;
use tapes_core::backend::opencode::OpenCodeBackend;
use tapes_core::backend::pi::PiBackend;
use tapes_core::backend::Backend;
use tapes_core::model::Role;

fn fixtures(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(harness)
}

fn fixture_backends() -> Vec<(Box<dyn Backend>, &'static str)> {
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

#[test]
fn every_file_backend_satisfies_shared_normalization_assertions() {
    for (backend, id) in fixture_backends() {
        assert!(
            backend.available(),
            "{} fixture is unavailable",
            backend.harness()
        );

        let sessions = backend.list(10).unwrap();
        let listed = sessions.iter().find(|session| session.id == id).unwrap();
        assert_eq!(listed.harness, backend.harness());
        assert!(listed.started_at <= listed.last_activity_at);

        let transcript = backend.transcript(id, 10).unwrap();
        assert_eq!(transcript.session, *listed);
        assert!(transcript.turns.len() >= 2);
        assert_eq!(transcript.turns.first().unwrap().role, Role::User);
        assert_eq!(transcript.turns.last().unwrap().role, Role::Assistant);
        assert!(transcript.turns.iter().all(|turn| !turn.text.is_empty()));

        let tailed = backend.transcript(id, 1).unwrap();
        assert_eq!(tailed.turns.len(), 1);
        assert!(tailed.truncated);
    }
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
            "00000000-0000-0000-0000-000000000002",
        ),
        (Box::new(PiBackend::new(fixtures("pi"))), "malformed"),
    ];

    for (backend, id) in cases {
        let transcript = backend.transcript(id, 10).unwrap();
        assert_eq!(transcript.turns.len(), 2);
        assert_eq!(
            transcript.notes,
            vec!["Skipped 1 unparseable line.".to_owned()]
        );
    }
}

#[test]
fn pi_reports_entries_outside_the_active_leaf_path() {
    let transcript = PiBackend::new(fixtures("pi"))
        .transcript("session-pi", 10)
        .unwrap();

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
    let transcript = CodexBackend::new(fixtures("codex"))
        .transcript("00000000-0000-0000-0000-000000000001", 10)
        .unwrap();
    let model = transcript.session.model.unwrap();

    assert_eq!(model.id, "gpt-fixture");
    assert_eq!(model.variant.as_deref(), Some("high"));
}

#[test]
fn deferred_opencode_backend_is_unavailable_without_erroring() {
    let backend = OpenCodeBackend;

    assert!(!backend.available());
    assert!(backend.list(10).unwrap().is_empty());
}

#[test]
fn transcript_reads_are_capped_at_four_megabytes() {
    let root = std::env::temp_dir().join(format!("tapes-bounds-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("bounded.jsonl");
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

    let transcript = ClaudeBackend::new(&root).transcript("bounded", 10).unwrap();

    assert!(transcript.truncated);
    assert_eq!(transcript.turns.len(), 2);
    fs::remove_dir_all(root).unwrap();
}
