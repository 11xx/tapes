use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/content-carriers")
}

#[test]
fn attachment_fixture_records_each_carrier_and_field_state() {
    let text = std::fs::read_to_string(
        fixture_root().join("project/20000000-0000-4000-8000-000000000001.jsonl"),
    )
    .unwrap();
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for kind in [
        "file",
        "compact_file_reference",
        "instructions",
        "environment",
        "prompt_snapshot",
    ] {
        for state in ["present", "missing", "malformed"] {
            let id = format!("{kind}-{state}");
            let record = records.iter().find(|record| record["uuid"] == id).unwrap();
            assert_eq!(record["attachment"]["type"], kind);
            assert!(record.get("message").is_none());
        }
    }
    let file = &records[0]["attachment"]["content"];
    assert_eq!(file["type"], "text");
    assert!(file["file"]["content"].is_string());
    assert_eq!(file["file"]["startLine"], 3);
    assert_eq!(file["file"]["numLines"], 1);
}

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::content::{ContentAvailability, ContentPart};
use agent_tapes_core::model::{Turn, TurnKind};

const ATTACHMENTS: &str = "20000000-0000-4000-8000-000000000001";

fn reference(turn: &Turn) -> &agent_tapes_core::content::ArtifactReference {
    match &turn.parts[0] {
        ContentPart::FileReference { reference, .. } => reference,
        part => panic!("expected file reference, got {part:?}"),
    }
}

#[test]
fn attached_files_are_ambient_and_agree_across_read_modes() {
    let backend = ClaudeBackend::new(fixture_root());
    let session = backend.locate(ATTACHMENTS).unwrap().unwrap();
    let transcript = backend.transcript(&session, usize::MAX).unwrap();
    assert_eq!(transcript.turns.len(), 6);
    assert!(session.derived_title.is_none());
    assert!(transcript
        .turns
        .iter()
        .all(|turn| turn.kind == TurnKind::Ambient && !turn.kind.in_exchange()));
    let file = &transcript.turns[0];
    let artifact = reference(file);
    assert_eq!(artifact.path.as_deref(), Some("src/example.rs"));
    assert_eq!(
        artifact.body.as_ref().unwrap().text,
        "Synthetic attached text.\n"
    );
    assert_eq!(
        artifact.body_availability,
        Some(ContentAvailability::RetainedBody)
    );
    assert_eq!(artifact.body_lines.as_ref().unwrap().start, 3);
    assert_eq!(artifact.body_lines.as_ref().unwrap().end, 3);
    assert_eq!(artifact.total_lines, Some(8));
    assert_eq!(artifact.source, file.parts[0].record_ref().cloned());
    for turn in &transcript.turns[1..] {
        assert!(reference(turn).body.is_none());
        assert_eq!(
            reference(turn).body_availability,
            Some(ContentAvailability::ReferenceOnly)
        );
    }
    let inventory = agent_tapes_core::content::inventory(&transcript.turns).unwrap();
    assert_eq!(inventory.records, 6);
    assert_eq!(inventory.references, 6);
    assert_eq!(inventory.retained_body, 1);
    assert_eq!(inventory.reference_only, 5);
    let json = serde_json::to_value(&transcript).unwrap();
    let unmapped = json["read"]["unmapped"].to_string();
    assert!(!unmapped.contains("attachment/file"));
    assert!(!unmapped.contains("attachment/compact_file_reference"));
    assert!(unmapped.contains("attachment/instructions"));
    assert!(unmapped.contains("attachment/environment"));
    assert!(unmapped.contains("attachment/prompt_snapshot"));

    let mut streamed = Vec::new();
    backend
        .stream_transcript(&session, None, &mut |turn| {
            streamed.push(turn);
            Ok(())
        })
        .unwrap();
    let page = backend.history_page(&session, None, 64 * 1024).unwrap();
    for other in [&streamed, &page.turns] {
        assert_eq!(other.len(), transcript.turns.len());
        for (expected, actual) in transcript.turns.iter().zip(other.iter()) {
            assert_eq!(expected.text, actual.text);
            assert_eq!(expected.kind, actual.kind);
            assert_eq!(expected.coverage, actual.coverage);
            let mut expected_ref = reference(expected).clone();
            let mut actual_ref = reference(actual).clone();
            assert!(actual_ref.source.is_some());
            expected_ref.source = None;
            actual_ref.source = None;
            assert_eq!(expected_ref, actual_ref);
        }
    }
}

const PERSISTED: &str = "20000000-0000-4000-8000-000000000002";

#[test]
fn persisted_results_keep_previews_pairing_and_external_body_coverage() {
    let backend = ClaudeBackend::new(fixture_root());
    let session = backend.locate(PERSISTED).unwrap().unwrap();
    let transcript = backend.transcript(&session, usize::MAX).unwrap();
    let valid = transcript
        .turns
        .iter()
        .find(|turn| turn.native_id.as_deref() == Some("result-present"))
        .unwrap();
    assert_eq!(valid.text, "Synthetic output preview.\n...");
    assert_eq!(valid.parts.len(), 2);
    let artifact = match &valid.parts[1] {
        ContentPart::FileReference { reference, .. } => reference,
        _ => panic!("missing persisted reference"),
    };
    assert_eq!(artifact.path.as_deref(), Some("tool-results/result001.txt"));
    assert_eq!(artifact.recorded_size.as_deref(), Some("51.6KB"));
    assert_eq!(artifact.tool_call_id.as_deref(), Some("tool-present"));
    assert!(artifact.bytes.is_none());
    assert!(artifact.body.is_none());
    assert_eq!(
        artifact.body_availability,
        Some(ContentAvailability::ReferenceOnly)
    );
    assert_eq!(artifact.source, valid.parts[1].record_ref().cloned());
    assert_eq!(
        valid.coverage.as_ref().unwrap().availability,
        ContentAvailability::ReferenceOnly
    );
    let tool = valid.tool.as_ref().unwrap();
    assert_eq!(tool.call_id.as_deref(), Some("tool-present"));
    assert_eq!(tool.output.as_ref().unwrap().preview, valid.text);
    assert_eq!(tool.artifact_references.len(), 1);
    assert_eq!(
        tool.artifact_references[0].source.as_ref(),
        valid.record_ref.as_ref()
    );
    for turn in &transcript.turns {
        if turn.native_id.as_deref() == Some("result-second") {
            assert!(turn.text.is_empty());
            assert_eq!(turn.tool.as_ref().unwrap().artifact_references.len(), 1);
        } else if turn.native_id.as_deref() != Some("result-present") {
            assert!(turn
                .parts
                .iter()
                .all(|part| !matches!(part, ContentPart::FileReference { .. })));
            if let Some(tool) = &turn.tool {
                assert!(tool.artifact_references.is_empty());
            }
        }
    }
    let inventory = agent_tapes_core::content::inventory(&transcript.turns).unwrap();
    assert_eq!(inventory.references, 2);
    assert_eq!(inventory.reference_only, 2);
    let events = agent_tapes_core::event::project(transcript.clone(), usize::MAX);
    let result = events
        .events
        .iter()
        .find(|record| {
            record.event.call_id.as_deref() == Some("tool-present")
                && !record.event.artifact_references.is_empty()
        })
        .unwrap();
    assert_eq!(result.event.artifact_references.len(), 1);
    assert_eq!(result.event.output.as_ref().unwrap().preview, valid.text);
    assert_eq!(events.pairs.complete, 7);

    let mut streamed = Vec::new();
    backend
        .stream_transcript(&session, None, &mut |turn| {
            streamed.push(turn);
            Ok(())
        })
        .unwrap();
    let page = backend.history_page(&session, None, 64 * 1024).unwrap();
    for other in [&streamed, &page.turns] {
        assert_eq!(other.len(), transcript.turns.len());
        for (expected, actual) in transcript.turns.iter().zip(other.iter()) {
            assert_eq!(expected.text, actual.text);
            assert_eq!(expected.kind, actual.kind);
            assert_eq!(expected.coverage, actual.coverage);
            assert_eq!(
                expected.tool.as_ref().map(|tool| &tool.output),
                actual.tool.as_ref().map(|tool| &tool.output)
            );
            assert_eq!(expected.parts.len(), actual.parts.len());
        }
    }
}
