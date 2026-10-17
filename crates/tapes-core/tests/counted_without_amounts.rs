//! A figure that counted events keeps every reason that applies to it, even
//! when it measured no amount; a figure that counted nothing stays empty.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::measurement::{Measured, MeasurementCompleteness, MeasurementReason};
use agent_tapes_core::model::{AccountingCoverage, CostCoverage};
use agent_tapes_core::usage::{self, GroupBy, MeasurementCoverage};
use agent_tapes_core::{
    brief_with_backends, show_with_backends, stats_full_with_backends, stats_with_backends,
    usage_full_with_backends, usage_with_backends, Selection,
};
use serde_json::{json, Value};

const COUNTERS: [&str; 6] = [
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-counted-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn reasons(completeness: &Value, counter: &str) -> Vec<String> {
    completeness[counter]
        .as_array()
        .map(|reasons| {
            reasons
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

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
fn a_figure_whose_coverage_counted_events_is_qualified_without_an_event_count() {
    let coverage: BTreeMap<String, MeasurementCoverage> = COUNTERS[..5]
        .iter()
        .map(|counter| {
            (
                (*counter).to_owned(),
                MeasurementCoverage {
                    measured: 0,
                    missing: 2,
                    unpriced: 0,
                },
            )
        })
        .collect();
    let completeness = MeasurementCompleteness::assess(Measured {
        coverage: Some(&coverage),
        scope: Some(AccountingCoverage::ReadWindow),
        ..Measured::default()
    });
    assert!(has(
        &completeness,
        "input",
        MeasurementReason::MissingCounter
    ));
    assert!(has(&completeness, "input", MeasurementReason::ReadWindow));
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

fn claude_store(label: &str, sessions: &[(&str, bool)]) -> Store {
    let store = Store::new(label);
    for (id, with_usage) in sessions {
        let mut lines = vec![json!({
            "type": "user", "sessionId": id, "uuid": format!("{id}-user"),
            "timestamp": "2026-01-01T10:00:00Z", "cwd": format!("/fixtures/{id}"),
            "message": {"role": "user", "content": "fixture"}
        })];
        for index in 0..2 {
            let mut message = json!({"role": "assistant", "model": "fixture-model", "content": []});
            if *with_usage {
                message["usage"] = json!({
                    "input_tokens": 10, "output_tokens": 4,
                    "cache_read_input_tokens": 3, "cache_creation_input_tokens": 5,
                    "output_tokens_details": {"thinking_tokens": 2}
                });
            }
            lines.push(json!({
                "type": "assistant", "sessionId": id,
                "requestId": format!("{id}-{index}"), "uuid": format!("{id}-{index}"),
                "timestamp": "2026-01-01T10:00:01Z", "cwd": format!("/fixtures/{id}"),
                "message": message
            }));
        }
        store.write(&format!("fixture-project/{id}.jsonl"), &lines);
    }
    store
}

/// Every counter missing, and nothing else: the read covered the session.
fn assert_every_counter_missing(label: &str, completeness: &Value) {
    for counter in &COUNTERS[..5] {
        assert_eq!(
            reasons(completeness, counter),
            ["missing-counter"],
            "{label} × {counter}: {completeness}"
        );
    }
    assert_eq!(
        reasons(completeness, "cost"),
        ["missing-price"],
        "{label} × cost: {completeness}"
    );
}

#[test]
fn a_claude_session_whose_requests_all_lack_usage_is_qualified_everywhere() {
    let store = claude_store("claude-empty", &[("empty", false), ("healthy", true)]);
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(&store.0))];
    for full in [false, true] {
        let view = if full {
            usage_full_with_backends(&backends, Selection::Id("empty")).unwrap()
        } else {
            usage_with_backends(&backends, Selection::Id("empty")).unwrap()
        };
        let view = serde_json::to_value(view).unwrap();
        assert!(view.get("tokens").is_none(), "{view}");
        assert!(view.get("cost").is_none(), "{view}");
        assert_eq!(
            view["accounting"],
            json!({"basis": "summed-requests", "coverage": "session"}),
            "{view}"
        );
        assert_every_counter_missing(
            &format!("usage full={full}"),
            &view["measurement_completeness"],
        );

        let stats = if full {
            stats_full_with_backends(&backends, Selection::Id("empty")).unwrap()
        } else {
            stats_with_backends(&backends, Selection::Id("empty")).unwrap()
        };
        let stats = serde_json::to_value(stats).unwrap();
        assert_every_counter_missing(
            &format!("stats full={full}"),
            &stats["usage"]["measurement_completeness"],
        );
    }
    let shown =
        serde_json::to_value(show_with_backends(&backends, Selection::Id("empty"), 12).unwrap())
            .unwrap();
    assert_every_counter_missing(
        "session wire",
        &shown["session"]["measurement_completeness"],
    );
    let brief =
        serde_json::to_value(brief_with_backends(&backends, Selection::Id("empty"), 12).unwrap())
            .unwrap();
    assert_every_counter_missing("brief", &brief["usage"]["measurement_completeness"]);

    let healthy =
        serde_json::to_value(usage_with_backends(&backends, Selection::Id("healthy")).unwrap())
            .unwrap();
    assert!(
        healthy
            .get("measurement_completeness")
            .is_none_or(|completeness| {
                COUNTERS[..5]
                    .iter()
                    .all(|counter| reasons(completeness, counter).is_empty())
            }),
        "{healthy}"
    );
    assert_eq!(healthy["tokens"]["input"], 20, "{healthy}");

    let sessions: Vec<_> = ["empty", "healthy"]
        .map(|id| backends[0].locate(id).unwrap().unwrap())
        .to_vec();
    let summary = serde_json::to_value(usage::aggregate(&sessions, &[GroupBy::Directory])).unwrap();
    assert_every_counter_missing(
        "summary totals",
        &summary["totals"]["measurement_completeness"],
    );
    assert_eq!(summary["totals"]["tokens"]["input"], 20, "{summary}");
    let group = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["key"]["directory"] == "/fixtures/healthy")
        .unwrap();
    assert!(
        COUNTERS[..5]
            .iter()
            .all(|counter| reasons(&group["measurement_completeness"], counter).is_empty()),
        "{group}"
    );
}

fn codex_unattributed(extra: &[Value], full: bool) -> Value {
    let id = "70000000-0000-0000-0000-0000000000c1";
    let store = Store::new("codex-unattributed");
    // No `turn_context`: the response is attributed to no model.
    let mut lines = vec![
        json!({"type": "session_meta", "payload": {"id": id, "timestamp": "2026-01-01T10:00:00Z"}}),
        json!({"type": "token_usage_record", "payload": {"response_id": "response-0", "usage": {
            "input_tokens": 10, "output_tokens": 4, "reasoning_output_tokens": 2,
            "cached_input_tokens": 3, "cache_write_input_tokens": 5}}}),
    ];
    lines.extend_from_slice(extra);
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &lines,
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(&store.0))];
    let view = if full {
        usage_full_with_backends(&backends, Selection::Id(id)).unwrap()
    } else {
        usage_with_backends(&backends, Selection::Id(id)).unwrap()
    };
    serde_json::to_value(view).unwrap()
}

#[test]
fn codex_unattributed_usage_follows_unrecognized_usage() {
    let unknown =
        json!({"type": "event_msg", "payload": {"type": "mystery", "usage": {"input_tokens": 7}}});
    let lifecycle = json!({"type": "event_msg", "payload": {"type": "task_started"}});
    for full in [false, true] {
        let figure = codex_unattributed(std::slice::from_ref(&unknown), full);
        let attribution = &figure["attribution"];
        assert_eq!(attribution["unattributed"], 1, "{figure}");
        assert_eq!(attribution["unattributed_tokens"]["input"], 10, "{figure}");
        for counter in &COUNTERS[..5] {
            let found = reasons(&attribution["unattributed_completeness"], counter);
            assert!(
                found.contains(&"unrecognized-usage".to_owned()),
                "full={full} × {counter}: {attribution}"
            );
            assert!(
                !found.contains(&"incomplete-attribution".to_owned()),
                "full={full} × {counter}: {attribution}"
            );
        }

        let whole = codex_unattributed(std::slice::from_ref(&lifecycle), full);
        for counter in &COUNTERS[..5] {
            assert!(
                reasons(&whole["attribution"]["unattributed_completeness"], counter).is_empty(),
                "full={full} × {counter}: {whole}"
            );
        }
    }
}
