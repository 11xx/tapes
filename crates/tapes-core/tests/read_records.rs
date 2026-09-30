use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::pi::PiBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::event::{project, EventKind};
use agent_tapes_core::{capture_with_backends, export_with_backends, ExportRead, Selection};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/read-records")
}

fn backend(harness: &str, root: &Path, bound: u64) -> Box<dyn Backend> {
    match harness {
        "claude" => Box::new(ClaudeBackend::new(root).with_read_bytes(bound)),
        "codex" => Box::new(CodexBackend::new(root).with_read_bytes(bound)),
        "pi" => Box::new(PiBackend::new(root).with_read_bytes(bound)),
        _ => unreachable!(),
    }
}

fn cases() -> [(&'static str, &'static str, &'static str); 3] {
    [
        (
            "claude",
            "session-read-claude",
            "project/session-read-claude.jsonl",
        ),
        (
            "codex",
            "90000000-0000-7000-8000-000000000001",
            "rollout-2026-01-01T10-00-00-90000000-0000-7000-8000-000000000001.jsonl",
        ),
        (
            "pi",
            "session-read-pi",
            "2026-01-01T10-00-00-000Z_session-read-pi.jsonl",
        ),
    ]
}

#[test]
fn read_locators_survive_export_and_capture_is_unpinned() {
    let output = "fixture line\n";
    let digest = format!("sha256:{:x}", Sha256::digest(output.as_bytes()));
    for (harness, id, _) in cases() {
        let backends = vec![backend(
            harness,
            &fixture_root().join(harness),
            4 * 1024 * 1024,
        )];
        let report = capture_with_backends(&backends, Selection::Id(id)).unwrap();
        assert_eq!(report.schema, "tapes-capture/1");
        assert_eq!(report.session, id);
        assert_eq!(report.harness, harness);
        assert_eq!(report.state, "unpinned");
        assert!(report.store_path_class.ends_with("jsonl"));

        let session = backends[0].locate(id).unwrap().unwrap();
        let events = project(
            backends[0].transcript(&session, usize::MAX).unwrap(),
            usize::MAX,
        );
        let identities = events
            .events
            .iter()
            .map(|record| record.event_id.as_ref().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(identities.len(), events.events.len(), "{harness}");
        let call = events
            .events
            .iter()
            .find(|record| record.event.kind == EventKind::ToolCall)
            .unwrap();
        assert!(call.native_id.is_some(), "{harness}");
        assert!(call
            .event_id
            .as_ref()
            .is_some_and(|id| id.starts_with("sha256:")));
        assert_eq!(call.event.read.as_ref().unwrap().path, "src/x.rs");
        assert_eq!(
            call.event
                .read
                .as_ref()
                .unwrap()
                .lines
                .as_ref()
                .unwrap()
                .start,
            10
        );
        assert_eq!(
            call.event
                .read
                .as_ref()
                .unwrap()
                .lines
                .as_ref()
                .unwrap()
                .end,
            40
        );
        assert_eq!(call.event.read.as_ref().unwrap().whole, None);
        assert_eq!(call.event.read.as_ref().unwrap().succeeded, Some(true));
        assert_eq!(
            call.event.read.as_ref().unwrap().sha256.as_deref(),
            Some(digest.as_str())
        );
        let second = events
            .events
            .iter()
            .find(|record| {
                record.event.kind == EventKind::ToolCall
                    && record.event.call_id.as_deref()
                        == Some(if harness == "codex" {
                            "call-read-2"
                        } else {
                            "tool-read-2"
                        })
            })
            .unwrap();
        let second_read = second.event.read.as_ref().unwrap();
        if harness == "pi" {
            assert_eq!(second_read.path, "src/unknown.rs");
            assert_eq!(second_read.lines, None);
            assert_eq!(second_read.whole, None);
            assert_eq!(second_read.succeeded, Some(false));
            assert_eq!(second_read.sha256, None);
        } else {
            assert_eq!(second_read.path, "src/whole.rs");
            assert_eq!(second_read.lines, None);
            assert_eq!(second_read.whole, (harness == "codex").then_some(true));
            assert_eq!(second_read.succeeded, Some(true));
            assert_eq!(
                second_read.sha256.as_deref(),
                Some(format!("sha256:{:x}", Sha256::digest(b"whole fixture\n")).as_str())
            );
        }
        if harness == "codex" {
            assert!(call
                .native_id
                .as_deref()
                .unwrap()
                .starts_with("record-sha256:"));
            assert!(call.record_ref.as_ref().unwrap().native_id.is_none());
        }

        let directory = std::env::temp_dir().join(format!(
            "tapes-read-export-{harness}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let bundle = export_with_backends(
            &backends,
            Selection::Id(id),
            Some(&directory),
            None,
            ExportRead::Bounded,
        )
        .unwrap();
        let exported: Value =
            serde_json::from_slice(&fs::read(&bundle.json.path).unwrap()).unwrap();
        assert_eq!(exported["schema"], "tapes-session/14");
        let exported_call = exported["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["kind"] == "tool-call")
            .unwrap();
        assert_eq!(exported_call["read"]["sha256"], digest);
        assert_eq!(
            capture_with_backends(&backends, Selection::Id(id))
                .unwrap()
                .state,
            "unpinned"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn event_identity_survives_distinct_file_windows() {
    for (harness, id, relative) in cases() {
        let source = fixture_root().join(harness).join(relative);
        let content = fs::read_to_string(source).unwrap();
        let (first, rest) = content.split_once('\n').unwrap();
        let root = std::env::temp_dir().join(format!(
            "tapes-read-window-{harness}-{}",
            std::process::id()
        ));
        let target = root.join(relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            format!(
                "{first}\n{{\"type\":\"fixture-padding\",\"data\":\"{}\"}}\n{rest}",
                "x".repeat(128 * 1024)
            ),
        )
        .unwrap();
        let read = |bound, label: &str| {
            let backends = vec![backend(harness, &root, bound)];
            let session = backends[0].locate(id).unwrap().unwrap();
            let events = project(
                backends[0].transcript(&session, usize::MAX).unwrap(),
                usize::MAX,
            );
            let call = events
                .events
                .into_iter()
                .find(|event| event.event.kind == EventKind::ToolCall)
                .unwrap();
            let bundle_dir = root.join(label);
            fs::create_dir_all(&bundle_dir).unwrap();
            let bundle = export_with_backends(
                &backends,
                Selection::Id(id),
                Some(&bundle_dir),
                None,
                ExportRead::Bounded,
            )
            .unwrap();
            let exported: Value =
                serde_json::from_slice(&fs::read(bundle.json.path).unwrap()).unwrap();
            let exported_call = exported["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["kind"] == "tool-call")
                .unwrap();
            assert_eq!(
                exported_call["event_id"],
                call.event_id.as_ref().unwrap().as_str()
            );
            (
                call.native_id.unwrap(),
                call.event_id.unwrap(),
                call.event.read.unwrap(),
            )
        };
        let narrow = read(64 * 1024, "narrow");
        let wide = read(256 * 1024, "wide");
        assert_eq!(narrow, wide, "{harness}");
        fs::remove_dir_all(root).unwrap();
    }
}
