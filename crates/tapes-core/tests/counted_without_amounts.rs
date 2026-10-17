//! A figure that counted events keeps every reason that applies to it, even
//! when it measured no amount; a figure that counted nothing stays empty.

use std::collections::BTreeMap;

use agent_tapes_core::measurement::{Measured, MeasurementCompleteness, MeasurementReason};
use agent_tapes_core::model::{AccountingCoverage, CostCoverage};

const COUNTERS: [&str; 6] = [
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];

fn has(completeness: &MeasurementCompleteness, counter: &str, reason: MeasurementReason) -> bool {
    completeness
        .iter()
        .any(|(name, reasons)| name == counter && reasons.contains(&reason))
}

#[test]
fn a_counted_figure_with_no_amount_keeps_its_scope_reason() {
    for (scope, reason) in [
        (
            Some(AccountingCoverage::ReadWindow),
            MeasurementReason::ReadWindow,
        ),
        (
            Some(AccountingCoverage::SinceReset),
            MeasurementReason::SinceReset,
        ),
        (None, MeasurementReason::UnestablishedScope),
    ] {
        let completeness = MeasurementCompleteness::assess(Measured {
            events: 1,
            scope,
            ..Measured::default()
        });
        for counter in &COUNTERS[..5] {
            assert!(
                has(&completeness, counter, MeasurementReason::MissingCounter),
                "{scope:?} × {counter}: {completeness:?}"
            );
            assert!(
                has(&completeness, counter, reason),
                "{scope:?} × {counter}: {completeness:?}"
            );
        }
        assert!(has(&completeness, "cost", MeasurementReason::MissingPrice));
    }
}

#[test]
fn a_counted_figure_with_a_written_zero_price_keeps_unpriced() {
    let pricing = CostCoverage {
        complete: false,
        zero_written_requests: 1,
        zero_written_observations: 1,
    };
    let completeness = MeasurementCompleteness::assess(Measured {
        cost_coverage: Some(&pricing),
        scope: Some(AccountingCoverage::Session),
        events: 1,
        ..Measured::default()
    });
    assert!(
        has(&completeness, "cost", MeasurementReason::Unpriced),
        "{completeness:?}"
    );
    assert!(has(
        &completeness,
        "input",
        MeasurementReason::MissingCounter
    ));
}

#[test]
fn a_figure_that_counted_nothing_stays_empty() {
    for scope in [None, Some(AccountingCoverage::ReadWindow)] {
        let completeness = MeasurementCompleteness::assess(Measured {
            scope,
            coverage: Some(&BTreeMap::new()),
            ..Measured::default()
        });
        assert!(completeness.is_empty(), "{scope:?}: {completeness:?}");
    }
}
