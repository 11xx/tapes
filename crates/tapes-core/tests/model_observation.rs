use std::path::{Path, PathBuf};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::model::{ModelSelectionSpan, Role};
use chrono::{TimeZone, Utc};

fn root(harness: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/model-observation")
        .join(harness)
}

fn backend(harness: &str, bytes: u64) -> Box<dyn Backend> {
    match harness {
        "claude" => Box::new(ClaudeBackend::new(root(harness)).with_read_bytes(bytes)),
        "pi" => Box::new(PiBackend::new(root(harness)).with_read_bytes(bytes)),
        _ => unreachable!(),
    }
}

fn id(harness: &str, suffix: &str) -> String {
    format!(
        "{}0000000-0000-4000-8000-000000000{suffix}",
        if harness == "claude" { "a" } else { "b" }
    )
}

fn span(span: &ModelSelectionSpan, effort: &str, first: usize, last: usize) {
    assert_eq!(span.model.id, "model-fixture");
    assert_eq!(span.model.variant.as_deref(), Some(effort));
    assert_eq!(
        span.first.native_id.as_deref(),
        Some(format!("assistant-{first}").as_str())
    );
    assert_eq!(
        span.last.native_id.as_deref(),
        Some(format!("assistant-{last}").as_str())
    );
    assert_eq!(
        span.first.timestamp,
        Some(
            Utc.with_ymd_and_hms(2026, 1, 5, 10, 0, first as u32 * 2)
                .unwrap()
        )
    );
    assert_eq!(
        span.last.timestamp,
        Some(
            Utc.with_ymd_and_hms(2026, 1, 5, 10, 0, last as u32 * 2)
                .unwrap()
        )
    );
}

#[test]
fn claude_and_pi_selections_and_assistant_turns() {
    for harness in ["claude", "pi"] {
        let backend = backend(harness, u64::MAX);
        let session = backend.locate(&id(harness, "001")).unwrap().unwrap();
        let status = session.model_observation.as_ref().unwrap();
        assert!(status.mixed && status.head_read && !status.attribution_uncertain);
        assert_eq!(status.distinct_observed, Some(2));
        assert_eq!(
            session.model.as_ref().unwrap().variant.as_deref(),
            Some("xhigh")
        );
        assert_eq!(session.model_selections.len(), 2);
        span(&session.model_selections[0], "high", 1, 2);
        span(&session.model_selections[1], "xhigh", 3, 4);
        assert_eq!(
            session.newest_model_selection(),
            session.model_selections.last()
        );
        let transcript = backend.transcript(&session, usize::MAX).unwrap();
        let turns = transcript
            .turns
            .iter()
            .filter(|t| t.role == Role::Assistant)
            .collect::<Vec<_>>();
        assert_eq!(turns.len(), 4);
        for (turn, effort) in turns.iter().zip(["high", "high", "xhigh", "xhigh"]) {
            assert_eq!(
                turn.model.as_ref().unwrap().variant.as_deref(),
                Some(effort)
            );
        }
        let mut streamed = Vec::new();
        let read = backend
            .stream_transcript(&session, None, &mut |turn| {
                streamed.push(turn);
                Ok(())
            })
            .unwrap();
        let whole = backend.stream_session(&session, &read).unwrap();
        assert_eq!(whole.model_selections, session.model_selections);
        assert_eq!(
            streamed.iter().map(|t| &t.model).collect::<Vec<_>>(),
            transcript
                .turns
                .iter()
                .map(|t| &t.model)
                .collect::<Vec<_>>()
        );
        let json = serde_json::to_value(&transcript).unwrap();
        assert_eq!(json["turns"][3]["model"]["variant"], "xhigh");
    }
}

#[test]
fn single_selection_and_repeated_selection_spans() {
    for harness in ["claude", "pi"] {
        let backend = backend(harness, u64::MAX);
        let single = backend.locate(&id(harness, "002")).unwrap().unwrap();
        let status = single.model_observation.as_ref().unwrap();
        assert!(!status.mixed && !status.attribution_uncertain && status.head_read);
        assert_eq!(status.distinct_observed, Some(1));
        assert_eq!(single.model_selections.len(), 1);
        span(&single.model_selections[0], "high", 1, 2);
        let repeated = backend.locate(&id(harness, "003")).unwrap().unwrap();
        assert_eq!(
            repeated
                .model_observation
                .as_ref()
                .unwrap()
                .distinct_observed,
            Some(2)
        );
        assert_eq!(repeated.model_selections.len(), 3);
        span(&repeated.model_selections[0], "high", 1, 1);
        span(&repeated.model_selections[1], "xhigh", 2, 2);
        span(&repeated.model_selections[2], "high", 3, 3);
    }
}

#[test]
fn tail_bound_does_not_claim_a_complete_single_selection() {
    for harness in ["claude", "pi"] {
        let path = if harness == "claude" {
            root(harness)
                .join("project")
                .join(format!("{}.jsonl", id(harness, "001")))
        } else {
            root(harness).join(format!(
                "2026-01-05T10-00-00-000Z_{}.jsonl",
                id(harness, "001")
            ))
        };
        let text = std::fs::read_to_string(path).unwrap();
        let start = if harness == "claude" {
            text.find("{\"type\":\"assistant\",\"timestamp\":\"2026-01-05T10:00:06Z\"")
                .unwrap()
        } else {
            text.find("{\"type\":\"thinking_level_change\",\"id\":\"thinking-3\"")
                .unwrap()
        };
        let backend = backend(harness, (text.len() - start) as u64);
        let session = backend.locate(&id(harness, "001")).unwrap().unwrap();
        let status = session.model_observation.as_ref().unwrap();
        assert!(!status.head_read && status.attribution_uncertain && !status.mixed);
        assert_eq!(status.distinct_observed, Some(1));
        assert_eq!(session.model_selections.len(), 1);
        span(&session.model_selections[0], "xhigh", 3, 4);
        let read = backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap();
        let whole = backend.stream_session(&session, &read).unwrap();
        assert!(whole.model_observation.as_ref().unwrap().head_read);
        assert_eq!(whole.model_selections.len(), 2);
    }
}

#[test]
fn codex_turn_context_spans_preserve_record_coordinates() {
    let backend =
        CodexBackend::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex"));
    let session = backend
        .locate("00000000-0000-0000-0000-000000000001")
        .unwrap()
        .unwrap();
    let selection = session.newest_model_selection().unwrap();
    assert_eq!(&selection.model, session.model.as_ref().unwrap());
    assert_eq!(
        selection.first.timestamp,
        Some(Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 1).unwrap())
    );
    assert_eq!(selection.last, selection.first);
}
