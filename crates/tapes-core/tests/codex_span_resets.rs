//! A Codex session whose cumulative total restarts, as a resumed `codex
//! exec` process does, still has span figures that conserve: the spans and
//! the remainder add up to the whole in the spans' basis, per counter.

use std::path::PathBuf;

use agent_tapes_core::backend::{codex::CodexBackend, Backend};
use agent_tapes_core::model::{AccountingCoverage, Tokens};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::{SpanUsage, UsageView};
use agent_tapes_core::{usage_full_with_backends, usage_spans_with_backends, Selection};

const ID: &str = "50000000-0000-7000-8000-000000000008";

fn backend() -> Box<dyn Backend> {
    Box::new(CodexBackend::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-span-resets/sessions"),
    ))
}

fn spans(bounds: &[SpanBounds]) -> SpanUsage {
    usage_spans_with_backends(&[backend()], Selection::Id(ID), bounds).unwrap()
}

fn requests(first: Option<usize>, last: Option<usize>) -> SpanBounds {
    SpanBounds::Requests { first, last }
}

fn counters(view: &UsageView) -> [u64; 5] {
    let tokens = view.tokens.clone().unwrap_or(Tokens {
        input: None,
        output: None,
        reasoning: None,
        cache_read: None,
        cache_write: None,
    });
    [
        tokens.input,
        tokens.output,
        tokens.reasoning,
        tokens.cache_read,
        tokens.cache_write,
    ]
    .map(|counter| counter.unwrap_or(0))
}

fn disjoint(view: &UsageView) -> [Option<u64>; 3] {
    view.disjoint.as_ref().map_or([None; 3], |disjoint| {
        [
            disjoint.uncached_input,
            disjoint.cache_read,
            disjoint.output,
        ]
    })
}

/// Claim: splitting a session whose cumulative total restarted, at every
/// request boundary, gives spans and a remainder that add up to `whole` per
/// counter and per disjoint category, and that whole is every run's
/// consumption, qualified `since-reset`. Rejects a whole that is only the
/// total recorded since the restart, which the spans exceed.
#[test]
fn spans_across_a_counter_restart_add_up_to_the_whole() {
    let all = spans(&[requests(None, None)]);
    let session_requests = all.spans[0].span.as_ref().unwrap().session_requests;
    assert_eq!(session_requests, 4);
    assert_eq!(counters(&all.whole), [5100, 190, 50, 2800, 0]);
    assert_eq!(disjoint(&all.whole)[1..], [Some(2800), Some(190)]);
    assert_eq!(
        all.whole.accounting.as_ref().unwrap().coverage,
        AccountingCoverage::SinceReset
    );
    for boundary in 0..=session_requests {
        let split = spans(&[
            requests(None, Some(boundary)),
            requests(Some(boundary + 1), None),
        ]);
        let parts = [&split.spans[0], &split.spans[1], &split.remainder];
        let summed = parts.iter().fold([0; 5], |mut sum, part| {
            for (sum, counter) in sum.iter_mut().zip(counters(part)) {
                *sum += counter;
            }
            sum
        });
        assert_eq!(
            summed,
            counters(&split.whole),
            "split after request {boundary}: spans and remainder against whole"
        );
        // A category the whole withholds, such as uncached input derived
        // from counters qualified `since-reset`, has no total to conserve.
        let whole = disjoint(&split.whole);
        let summed = parts.iter().fold([Some(0); 3], |mut sum, part| {
            for (sum, category) in sum.iter_mut().zip(disjoint(part)) {
                *sum = sum
                    .zip(category.or(Some(0)))
                    .map(|(sum, category)| sum + category);
            }
            sum
        });
        for (category, whole) in summed.into_iter().zip(whole) {
            if let Some(whole) = whole {
                assert_eq!(
                    category,
                    Some(whole),
                    "split after request {boundary}: disjoint categories against whole"
                );
            }
        }
    }
}

/// Claim: the whole-session answer keeps the harness's own total recorded
/// since the restart, under `since-reset`, while the span answer names how
/// its whole differs from it. Rejects a whole-session answer rebuilt from
/// the spans' moves, and a span whole that silently departs from it.
#[test]
fn the_whole_session_answer_keeps_the_total_recorded_since_the_restart() {
    let full = usage_full_with_backends(&[backend()], Selection::Id(ID)).unwrap();
    assert_eq!(counters(&full), [2600, 70, 20, 1900, 0]);
    assert_eq!(
        full.accounting.as_ref().unwrap().coverage,
        AccountingCoverage::SinceReset
    );
    let all = spans(&[requests(None, None)]);
    assert!(
        all.whole
            .notes
            .iter()
            .any(|note| note.contains("restarted") && note.contains("usage --full")),
        "the span whole names how it differs from the recorded total: {:?}",
        all.whole.notes
    );
}
