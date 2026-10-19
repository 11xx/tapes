//! Spans of one session conserve: the spans that share no request and the
//! remainder no span holds add up to the whole session, per counter and per
//! disjoint category, within the spans' accounting basis. A span names its
//! boundary requests, and a request or record it can neither place nor rule
//! out qualifies it instead of silently leaving it complete.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::span::{SpanBounds, SPANS_SCHEMA};
use agent_tapes_core::usage::{SpanUsage, UsageView};
use agent_tapes_core::{usage_full_with_backends, usage_spans_with_backends, Selection};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

fn fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn opencode_program() -> PathBuf {
    static XDG: OnceLock<PathBuf> = OnceLock::new();
    let root = XDG.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("tapes-span-xdg-{}", std::process::id()));
        let opencode = root.join("opencode");
        fs::create_dir_all(&opencode).unwrap();
        File::create(opencode.join("opencode.db")).unwrap();
        root
    });
    std::env::set_var("XDG_DATA_HOME", root);
    fixtures("opencode").join("opencode2")
}

fn spans(backend: Box<dyn Backend>, id: &str, bounds: &[SpanBounds]) -> SpanUsage {
    usage_spans_with_backends(&[backend], Selection::Id(id), bounds).unwrap()
}

fn full(backend: Box<dyn Backend>, id: &str) -> UsageView {
    usage_full_with_backends(&[backend], Selection::Id(id)).unwrap()
}

fn at(stamp: &str) -> DateTime<Utc> {
    stamp.parse().unwrap()
}

fn time(from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> SpanBounds {
    SpanBounds::Time { from, to }
}

fn ordinals(first: Option<usize>, last: Option<usize>) -> SpanBounds {
    SpanBounds::Requests { first, last }
}

/// Every amount a figure carries: its native counters, its disjoint
/// categories, its cost, and a pi consumption's counters and cost. The
/// disjoint total appears only on a completely measured figure, so it is
/// compared through its categories.
fn amounts(view: &UsageView) -> BTreeMap<String, f64> {
    fn collect(prefix: &str, value: &Value, into: &mut BTreeMap<String, f64>) {
        match value {
            Value::Number(number) => {
                into.insert(prefix.to_owned(), number.as_f64().unwrap());
            }
            Value::Object(object) => {
                for (key, value) in object {
                    if !matches!(key.as_str(), "missing" | "convention" | "completeness") {
                        collect(&format!("{prefix}.{key}"), value, into);
                    }
                }
            }
            _ => {}
        }
    }
    let value = serde_json::to_value(view).unwrap();
    let mut into = BTreeMap::new();
    for field in ["tokens", "disjoint", "cost"] {
        collect(field, &value[field], &mut into);
    }
    for field in ["tokens", "cost"] {
        collect(
            &format!("consumption.{field}"),
            &value["consumption"][field],
            &mut into,
        );
    }
    into.remove("disjoint.total");
    into
}

/// Whether a figure names the amount at `key` incompletely measured, so
/// what it holds there is a known subtotal.
fn qualifies(view: &UsageView, key: &str) -> bool {
    let value = serde_json::to_value(view).unwrap();
    let (figure, counter) = key.rsplit_once('.').unwrap();
    let (completeness, counter) = match figure {
        "tokens" => (&value["measurement_completeness"], counter),
        "cost" => (&value["measurement_completeness"], "cost"),
        "disjoint" => (&value["disjoint"]["measurement_completeness"], counter),
        "disjoint.cache_write" => (
            &value["disjoint"]["measurement_completeness"],
            "cache_write",
        ),
        "consumption.cost" => (&value["consumption"]["measurement_completeness"], "cost"),
        _ => (&value["consumption"]["measurement_completeness"], counter),
    };
    completeness.get(counter).is_some()
}

/// The parts add up to the whole at every amount the whole measures, or a
/// part names that amount incompletely measured and the parts hold a known
/// subtotal of it.
fn assert_sums(parts: &[&UsageView], whole: &UsageView, label: &str) {
    let mut summed = BTreeMap::<String, f64>::new();
    for part in parts {
        for (key, amount) in amounts(part) {
            *summed.entry(key).or_default() += amount;
        }
    }
    // A counter the whole does not measure, such as one a Codex total
    // stopped writing, has no total for the parts to conserve.
    for (key, right) in amounts(whole) {
        let left = summed.get(&key).copied().unwrap_or_default();
        let qualified = parts.iter().any(|part| {
            part.span.as_ref().is_some_and(|span| span.requests > 0) && qualifies(part, &key)
        });
        if qualified {
            assert!(
                left <= right + 1e-9,
                "{label}: {key} holds a known subtotal {left} above the whole's {right}"
            );
            continue;
        }
        assert!(
            (left - right).abs() < 1e-9,
            "{label}: {key} sums to {left} across the parts but the whole holds {right}"
        );
    }
}

fn reasons(view: &UsageView, counter: &str) -> Vec<String> {
    serde_json::to_value(&view.measurement_completeness).unwrap()[counter]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .map(|reason| reason.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

type Case = (&'static str, fn() -> Box<dyn Backend>, &'static str);

fn cases() -> Vec<Case> {
    vec![
        (
            "claude",
            || Box::new(ClaudeBackend::new(fixtures("claude"))),
            "session-claude",
        ),
        (
            "claude resumed",
            || Box::new(ClaudeBackend::new(fixtures("claude-resumed"))),
            "30000000-0000-4000-8000-000000000001",
        ),
        (
            "codex",
            || Box::new(CodexBackend::new(fixtures("codex"))),
            "00000000-0000-0000-0000-000000000001",
        ),
        (
            "codex resumed",
            || Box::new(CodexBackend::new(fixtures("codex-resumed"))),
            "40000000-0000-7000-8000-000000000001",
        ),
        (
            "pi",
            || Box::new(PiBackend::new(fixtures("pi"))),
            "session-pi",
        ),
        (
            "pi turn durations",
            || Box::new(PiBackend::new(fixtures("pi"))),
            "session-turn-durations",
        ),
        (
            "opencode",
            || Box::new(OpenCodeBackend::new(opencode_program())),
            "ses_000000fixtureSpanSession",
        ),
    ]
}

/// Claim: splitting any fixture session at a boundary between requests, by
/// time or by ordinal, gives two spans that add up to the whole, and the
/// whole in the spans' basis is the whole-session answer wherever the two
/// share a basis. Rejects a split that drops, repeats or misplaces a
/// request, such as one that counts each record of a Claude request.
#[test]
fn a_split_at_every_request_boundary_adds_up_to_the_whole() {
    for (label, backend, id) in cases() {
        let all = spans(backend(), id, &[ordinals(None, None)]);
        let session_requests = all.spans[0].span.as_ref().unwrap().session_requests;
        assert!(session_requests > 0, "{label}: the fixture places requests");
        let answer = full(backend(), id);
        if answer
            .accounting
            .as_ref()
            .map(|accounting| accounting.basis)
            == all
                .whole
                .accounting
                .as_ref()
                .map(|accounting| accounting.basis)
        {
            assert_sums(&[&all.whole], &answer, &format!("{label}: whole"));
        }
        let mut stamps = Vec::new();
        for ordinal in 1..=session_requests {
            let one = spans(backend(), id, &[ordinals(Some(ordinal), Some(ordinal))]);
            let placed = one.spans[0].span.as_ref().unwrap();
            assert_eq!(
                placed.requests, 1,
                "{label}: request {ordinal} is placed once"
            );
            stamps.push(placed.first.as_ref().unwrap().timestamp.unwrap());
        }
        for boundary in 0..=session_requests {
            let split = spans(
                backend(),
                id,
                &[
                    ordinals(None, Some(boundary)),
                    ordinals(Some(boundary + 1), None),
                ],
            );
            assert_eq!(split.remainder.span.as_ref().unwrap().requests, 0);
            assert_sums(
                &[&split.spans[0], &split.spans[1], &split.remainder],
                &split.whole,
                &format!("{label}: requests split after {boundary}"),
            );
            let instant = stamps
                .get(boundary)
                .copied()
                .unwrap_or_else(|| stamps[session_requests - 1] + Duration::milliseconds(1));
            let split = spans(
                backend(),
                id,
                &[time(None, Some(instant)), time(Some(instant), None)],
            );
            let before = split.spans[0].span.as_ref().unwrap();
            let after = split.spans[1].span.as_ref().unwrap();
            assert_eq!(
                before.requests + after.requests,
                session_requests,
                "{label}: a time split at {instant} places every request once"
            );
            assert_sums(
                &[&split.spans[0], &split.spans[1], &split.remainder],
                &split.whole,
                &format!("{label}: time split at {instant}"),
            );
        }
    }
}

/// Claim: a Claude session resumed twice splits into one span per run whose
/// figures add up to the whole session, each naming its first and last
/// request by ordinal, timestamp and request id, and the runs' recorded
/// totals are withheld rather than divided. Rejects a span that answers a
/// run's cost-state, or the newest one, in place of its requests.
#[test]
fn a_claude_session_resumed_twice_splits_into_its_runs() {
    let id = "30000000-0000-4000-8000-000000000001";
    let backend =
        || -> Box<dyn Backend> { Box::new(ClaudeBackend::new(fixtures("claude-resumed"))) };
    let runs = spans(
        backend(),
        id,
        &[
            time(None, Some(at("2026-01-05T11:00:00Z"))),
            time(
                Some(at("2026-01-05T11:00:00Z")),
                Some(at("2026-01-05T13:00:00Z")),
            ),
            time(Some(at("2026-01-05T13:00:00Z")), None),
        ],
    );
    assert_eq!(runs.schema, SPANS_SCHEMA);
    let named = runs
        .spans
        .iter()
        .map(|view| {
            let span = view.span.as_ref().unwrap();
            (
                span.requests,
                span.first.as_ref().unwrap().ordinal,
                span.first.as_ref().unwrap().id.clone().unwrap(),
                span.first.as_ref().unwrap().timestamp.unwrap(),
                span.last.as_ref().unwrap().ordinal,
                span.last.as_ref().unwrap().id.clone().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        named,
        [
            (
                2,
                1,
                "req_fixture_1".to_owned(),
                at("2026-01-05T10:00:01Z"),
                2,
                "req_fixture_2".to_owned()
            ),
            (
                2,
                3,
                "req_fixture_3".to_owned(),
                at("2026-01-05T12:00:01Z"),
                4,
                "req_fixture_4".to_owned()
            ),
            (
                1,
                5,
                "req_fixture_5".to_owned(),
                at("2026-01-05T14:00:01Z"),
                5,
                "req_fixture_5".to_owned()
            ),
        ]
    );
    assert_eq!(runs.spans[0].tokens.as_ref().unwrap().output, Some(200));
    assert_eq!(runs.spans[1].tokens.as_ref().unwrap().output, Some(250));
    assert_eq!(runs.spans[2].tokens.as_ref().unwrap().output, Some(300));
    let whole = full(backend(), id);
    assert_sums(
        &[
            &runs.spans[0],
            &runs.spans[1],
            &runs.spans[2],
            &runs.remainder,
        ],
        &whole,
        "claude runs",
    );
    for view in &runs.spans {
        assert!(
            view.recorded_total.is_none(),
            "a span holds no recorded total"
        );
        assert!(view
            .notes
            .iter()
            .any(|note| note.contains("3 recorded totals (cost-state)")));
    }
}

/// Claim: a Codex session resumed once splits into its two runs by the
/// difference of the cumulative totals at the boundary, the resumed
/// process's restated total counts no request, and each run names its
/// boundary requests by response id. Rejects a run figure that is the
/// cumulative total itself, or that counts the restated total.
#[test]
fn a_codex_session_resumed_once_splits_into_its_runs() {
    let id = "40000000-0000-7000-8000-000000000001";
    let backend = || -> Box<dyn Backend> { Box::new(CodexBackend::new(fixtures("codex-resumed"))) };
    let boundary = at("2026-01-06T11:00:00Z");
    let runs = spans(
        backend(),
        id,
        &[time(None, Some(boundary)), time(Some(boundary), None)],
    );
    let first = runs.spans[0].span.as_ref().unwrap();
    let second = runs.spans[1].span.as_ref().unwrap();
    assert_eq!(first.session_requests, 4);
    assert_eq!(
        (
            first.requests,
            first.first.as_ref().unwrap().id.as_deref(),
            first.last.as_ref().unwrap().id.as_deref()
        ),
        (2, Some("resp_fixture_1"), Some("resp_fixture_2"))
    );
    assert_eq!(
        (
            second.requests,
            second.first.as_ref().unwrap().id.as_deref(),
            second.last.as_ref().unwrap().id.as_deref()
        ),
        (2, Some("resp_fixture_3"), Some("resp_fixture_4"))
    );
    let tokens = runs.spans[1].tokens.as_ref().unwrap();
    assert_eq!((tokens.input, tokens.output), (Some(5400), Some(120)));
    let whole = full(backend(), id);
    assert_sums(
        &[&runs.spans[0], &runs.spans[1], &runs.remainder],
        &whole,
        "codex runs",
    );
}

/// Claim: a Codex counter that restarts inside a span qualifies that span
/// `since-reset` and its figure never goes negative, while a span before the
/// restart stays whole. Rejects a span computed as a plain difference across
/// the restart.
#[test]
fn a_codex_counter_reset_inside_a_span_qualifies_it_since_reset() {
    let id = "40000000-0000-7000-8000-000000000002";
    let boundary = at("2026-01-07T11:00:00Z");
    let runs = spans(
        Box::new(CodexBackend::new(fixtures("codex-resumed"))),
        id,
        &[time(None, Some(boundary)), time(Some(boundary), None)],
    );
    assert!(!reasons(&runs.spans[0], "output").contains(&"since-reset".to_owned()));
    assert!(reasons(&runs.spans[1], "output").contains(&"since-reset".to_owned()));
    let tokens = runs.spans[1].tokens.as_ref().unwrap();
    assert_eq!((tokens.input, tokens.output), (Some(5400), Some(120)));
}

/// Claim: the multi-span call answers spans that leave requests between them
/// with a remainder holding exactly those requests, so spans and remainder
/// add up to the whole; overlapping spans count a shared request in each and
/// report it as overlapped. Rejects a remainder that drops unplaced requests
/// and an overlap that goes unreported.
#[test]
fn several_spans_answer_with_a_remainder_and_flag_overlap() {
    let id = "30000000-0000-4000-8000-000000000001";
    let backend =
        || -> Box<dyn Backend> { Box::new(ClaudeBackend::new(fixtures("claude-resumed"))) };
    let apart = spans(
        backend(),
        id,
        &[
            time(None, Some(at("2026-01-05T11:00:00Z"))),
            time(Some(at("2026-01-05T13:00:00Z")), None),
        ],
    );
    let remainder = apart.remainder.span.as_ref().unwrap();
    assert_eq!(remainder.bounds, None);
    assert_eq!(
        (
            remainder.requests,
            remainder.first.as_ref().unwrap().ordinal,
            remainder.last.as_ref().unwrap().ordinal
        ),
        (2, 3, 4)
    );
    assert_eq!(apart.overlapped, 0);
    assert_sums(
        &[&apart.spans[0], &apart.spans[1], &apart.remainder],
        &apart.whole,
        "spans apart",
    );

    let shared = spans(
        backend(),
        id,
        &[ordinals(None, Some(4)), ordinals(Some(3), None)],
    );
    assert_eq!(shared.overlapped, 2);
    for view in &shared.spans {
        assert_eq!(view.span.as_ref().unwrap().overlapped, 2);
    }
    assert_eq!(shared.spans[0].span.as_ref().unwrap().requests, 4);
    assert_eq!(shared.spans[1].span.as_ref().unwrap().requests, 3);
    assert_eq!(shared.remainder.span.as_ref().unwrap().requests, 0);
}

/// Claim: an unreadable record the read could not cover leaves every span
/// it may belong to incomplete (`unreadable-record`, counted under
/// `unplaced`), whichever way the spans are bounded, while a span it cannot
/// belong to stays complete. Rejects a span reported complete over a part
/// of the recording the read could not decode.
#[test]
fn a_span_the_read_cannot_cover_is_not_complete() {
    let id = "30000000-0000-4000-8000-000000000002";
    let backend =
        || -> Box<dyn Backend> { Box::new(ClaudeBackend::new(fixtures("claude-resumed"))) };
    let runs = spans(
        backend(),
        id,
        &[
            time(None, Some(at("2026-01-05T11:00:00Z"))),
            time(
                Some(at("2026-01-05T11:00:00Z")),
                Some(at("2026-01-05T13:00:00Z")),
            ),
            time(Some(at("2026-01-05T13:00:00Z")), None),
            ordinals(Some(2), Some(2)),
            ordinals(Some(5), Some(5)),
        ],
    );
    let unreadable = "unreadable-record".to_owned();
    for (index, covered) in [(0, false), (1, false), (2, true), (3, false), (4, true)] {
        let view = &runs.spans[index];
        assert_eq!(
            !reasons(view, "input").contains(&unreadable),
            covered,
            "span {index} completeness: {:?}",
            view.measurement_completeness
        );
        assert_eq!(view.span.as_ref().unwrap().unplaced, usize::from(!covered));
    }
}

/// Claim: a request without a usable timestamp is held by the remainder, so
/// a time split still adds up to the whole, and it qualifies each span it
/// may belong to `unestablished-scope`. Rejects a request dropped from every
/// figure, or a span reported complete beside it.
#[test]
fn a_request_without_a_timestamp_is_held_by_the_remainder() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "tapes-span-{}-{}-{nonce}",
        std::process::id(),
        TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
    ));
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let id = "30000000-0000-4000-8000-000000000003";
    let request = |uuid: &str, request: &str, stamp: Option<&str>, output: u64| {
        let mut record = json!({
            "type": "assistant", "sessionId": id, "uuid": uuid, "requestId": request,
            "cwd": "/fixtures/project",
            "message": {"role": "assistant", "model": "claude-fixture", "content": [{"type": "text", "text": "ok"}],
                        "usage": {"input_tokens": 1, "output_tokens": output, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
        });
        if let Some(stamp) = stamp {
            record["timestamp"] = json!(stamp);
        }
        record.to_string()
    };
    let lines = [
        request("a1", "req-1", Some("2026-01-05T10:00:01Z"), 10),
        request("a2", "req-2", None, 20),
        request("a3", "req-3", Some("2026-01-05T10:00:05Z"), 40),
    ];
    fs::write(project.join(format!("{id}.jsonl")), lines.join("\n") + "\n").unwrap();
    let split = at("2026-01-05T10:00:03Z");
    let answer = spans(
        Box::new(ClaudeBackend::new(&root)),
        id,
        &[time(None, Some(split)), time(Some(split), None)],
    );
    let remainder = answer.remainder.span.as_ref().unwrap();
    assert_eq!(
        (
            remainder.requests,
            remainder.first.as_ref().unwrap().ordinal
        ),
        (1, 2)
    );
    assert_eq!(answer.remainder.tokens.as_ref().unwrap().output, Some(20));
    for view in &answer.spans {
        assert!(reasons(view, "output").contains(&"unestablished-scope".to_owned()));
    }
    assert_sums(
        &[&answer.spans[0], &answer.spans[1], &answer.remainder],
        &answer.whole,
        "timestamp-less request",
    );
    let _ = fs::remove_dir_all(&root);
}
