//! One whole-recording read serves a session's request timeline, its working
//! stretches and its spans: the timeline counts requests as the span placer
//! does, the stretches cut the read's turns as `stats --idle` does, and
//! neither streams the recording again.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend, StreamedTranscript,
};
use agent_tapes_core::model::Session;
use agent_tapes_core::span::{PlacedRequest, SpanBounds};
use agent_tapes_core::{counters, stats_full_with_backends_and_idle, usage, Selection};

/// Tests that count recording passes hold this, since every read in the
/// process moves the counter.
static COUNTED: Mutex<()> = Mutex::new(());

fn counted() -> std::sync::MutexGuard<'static, ()> {
    COUNTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

type Case = (&'static str, fn() -> Box<dyn Backend>, &'static str, usize);

/// Recordings and the requests their span read counts: Claude runs resumed
/// twice, a Codex counter that restarts, a Codex compaction and a request no
/// token count followed, and a pi session.
fn cases() -> Vec<Case> {
    vec![
        (
            "claude resumed",
            || Box::new(ClaudeBackend::new(fixtures("claude-resumed"))),
            "30000000-0000-4000-8000-000000000001",
            5,
        ),
        (
            "codex restart",
            || Box::new(CodexBackend::new(fixtures("codex-span-resets/sessions"))),
            "50000000-0000-7000-8000-000000000008",
            4,
        ),
        (
            "codex compaction",
            || Box::new(CodexBackend::new(fixtures("codex-span-resets/sessions"))),
            "50000000-0000-7000-8000-000000000009",
            5,
        ),
        (
            "pi",
            || Box::new(PiBackend::new(fixtures("pi"))),
            "session-pi",
            0,
        ),
    ]
}

fn read(backend: &dyn Backend, id: &str) -> (Session, StreamedTranscript) {
    let session = backend.locate(id).unwrap().unwrap();
    let read = backend
        .stream_transcript(&session, None, &mut |_| Ok(()))
        .unwrap();
    (session, read)
}

/// Claim: the timeline a read carries lists the same requests, under the
/// same ordinals, timestamps and ids, as the span read that replays it
/// places, for Claude, a Codex restart, a Codex compaction and pi. Rejects a
/// timeline that counts requests its own way, such as one that reads only
/// the moves of Codex's cumulative total and so misses the compaction.
#[test]
fn the_timeline_counts_requests_as_the_span_placer_does() {
    let _counted = counted();
    for (label, backend, id, expected) in cases() {
        let backend = backend();
        let (session, read) = read(backend.as_ref(), id);
        let timeline = usage::timeline(&read).unwrap_or_else(|| panic!("{label}: a timeline"));
        let spans = backend.stream_spans(&session, &read, &[]).unwrap();
        assert_eq!(timeline, &spans.timeline, "{label}");
        let ordinals = timeline
            .requests
            .iter()
            .map(|request| request.ordinal)
            .collect::<Vec<_>>();
        assert_eq!(
            ordinals,
            (1..=spans.remainder.1.session_requests).collect::<Vec<_>>(),
            "{label}"
        );
        if expected > 0 {
            assert_eq!(timeline.requests.len(), expected, "{label}");
        } else {
            assert!(!timeline.requests.is_empty(), "{label}: the fixture counts");
        }
    }
}

/// Claim: the stretches the library cuts from a read are the ones `stats
/// --full --idle` counts on the same recording at the same threshold, from
/// a threshold below every gap to one above them all. Rejects stretches cut
/// from other stamps than the turns', such as the requests'.
#[test]
fn stretches_are_the_ones_stats_counts() {
    let _counted = counted();
    for (label, backend, id, _) in cases() {
        let (_, read) = read(backend().as_ref(), id);
        for idle in [0, 1_000, 60_000, 45 * 60_000, 86_400_000] {
            let stats =
                stats_full_with_backends_and_idle(&[backend()], Selection::Id(id), idle).unwrap();
            let counted = stats
                .durations_ms
                .as_ref()
                .and_then(|durations| durations.stretches);
            let stretches = usage::stretches(&read, idle);
            // stats states no stretch count for a single stamp.
            if let Some(counted) = counted {
                assert_eq!(stretches.len(), counted, "{label} at {idle} ms");
            } else {
                assert!(stretches.len() <= 1, "{label} at {idle} ms");
            }
        }
    }
    // The resumed Claude runs sit two hours apart: three stretches at 45
    // minutes, holding the runs' requests.
    let backend = ClaudeBackend::new(fixtures("claude-resumed"));
    let (_, read) = read(&backend, "30000000-0000-4000-8000-000000000001");
    let stretches = usage::stretches(&read, 45 * 60_000);
    let held = stretches
        .iter()
        .map(|stretch| stretch.requests)
        .collect::<Vec<_>>();
    assert_eq!(held, [2, 2, 1]);
    assert_eq!(
        stretches[1]
            .first_request
            .as_ref()
            .and_then(|request| request.id.as_deref()),
        Some("req_fixture_3")
    );
}

/// A copy of a fixture recording under a fresh store root, removed when
/// dropped.
struct Store(PathBuf);

impl Store {
    fn copy(fixture: &Path, relative: &str, edit: impl Fn(&str) -> String) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-request-timeline-{}-{nonce}",
            std::process::id()
        ));
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = fs::read_to_string(fixture.join(relative)).unwrap();
        let edited = body.lines().map(&edit).collect::<Vec<_>>().join("\n") + "\n";
        fs::write(path, edited).unwrap();
        Self(root)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claim: a request whose record carries no usable timestamp is counted in
/// the timeline as untimed, keeps its ordinal, and is held by no stretch,
/// while every timed request is held by exactly one. Rejects a timeline that
/// drops the request and a stretch that adopts it.
#[test]
fn an_untimed_request_is_counted_and_held_by_no_stretch() {
    let _counted = counted();
    let relative = "project/30000000-0000-4000-8000-000000000001.jsonl";
    let store = Store::copy(&fixtures("claude-resumed"), relative, |line| {
        if line.contains("\"requestId\":\"req_fixture_4\"") {
            let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
            value.as_object_mut().unwrap().remove("timestamp");
            value.to_string()
        } else {
            line.to_owned()
        }
    });
    let backend = ClaudeBackend::new(&store.0);
    let (session, read) = read(&backend, "30000000-0000-4000-8000-000000000001");
    let timeline = usage::timeline(&read).unwrap();
    assert_eq!(timeline.requests.len(), 5);
    assert_eq!(timeline.untimed(), 1);
    assert_eq!(
        timeline.requests[3],
        PlacedRequest {
            ordinal: 4,
            timestamp: None,
            id: Some("req_fixture_4".to_owned()),
        }
    );
    let spans = backend.stream_spans(&session, &read, &[]).unwrap();
    assert_eq!(timeline, &spans.timeline);
    let stretches = usage::stretches(&read, 45 * 60_000);
    assert_eq!(
        stretches
            .iter()
            .map(|stretch| stretch.requests)
            .sum::<usize>(),
        4
    );
    assert!(stretches.iter().all(|stretch| {
        [&stretch.first_request, &stretch.last_request]
            .into_iter()
            .flatten()
            .all(|request| request.timestamp.is_some())
    }));
}

/// Claim: the timeline and the stretches come from the read without
/// streaming the recording again, so one read and one span pass serve all
/// three, as many passes as the spans alone take. Rejects a timeline or
/// stretches that read the recording themselves.
#[test]
fn one_read_serves_the_timeline_the_stretches_and_the_spans() {
    let _counted = counted();
    for (label, backend, id, _) in cases() {
        let backend = backend();
        let session = backend.locate(id).unwrap().unwrap();
        let before = counters::recording_passes();
        let read = backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap();
        let read_passes = counters::recording_passes() - before;
        let before = counters::recording_passes();
        let timeline = usage::timeline(&read).unwrap().clone();
        let stretches = usage::stretches(&read, 45 * 60_000);
        assert_eq!(
            counters::recording_passes() - before,
            0,
            "{label}: the timeline and stretches stream nothing"
        );
        let first = timeline
            .requests
            .first()
            .and_then(|request| request.timestamp);
        let cut = stretches.get(1).map(|stretch| stretch.first).or(first);
        let before = counters::recording_passes();
        let spans = backend
            .stream_spans(
                &session,
                &read,
                &[SpanBounds::Time {
                    from: None,
                    to: cut,
                }],
            )
            .unwrap();
        assert_eq!(
            counters::recording_passes() - before,
            1,
            "{label}: the spans replay the read once"
        );
        assert!(read_passes >= 1, "{label}");
        assert_eq!(spans.timeline, timeline, "{label}");
    }
}
