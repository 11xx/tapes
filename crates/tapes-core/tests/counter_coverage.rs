//! Every reader's summed counters say how many counted events recorded them,
//! through the one per-counter coverage and completeness every derived figure
//! asks. A counter some counted events did not record is a qualified known
//! subtotal, and a session that recorded every counter stays whole beside it.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::measurement::{Measured, MeasurementCompleteness};
use agent_tapes_core::usage::{self, GroupBy};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

const TOKENS: [&str; 5] = ["input", "output", "reasoning", "cache_read", "cache_write"];

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-coverage-{label}-{}-{nonce}",
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

/// What a request records: every counter, or the named subset.
fn counters(recorded: &[&str]) -> Value {
    let all = [
        ("input", 10),
        ("output", 4),
        ("reasoning", 2),
        ("cache_read", 3),
        ("cache_write", 5),
    ];
    let mut usage = serde_json::Map::new();
    for (name, value) in all {
        if recorded.contains(&name) {
            usage.insert(name.to_owned(), json!(value));
        }
    }
    Value::Object(usage)
}

/// A partial session: three requests recording every counter, input and
/// output and cache write, and nothing.
fn partial() -> [Vec<&'static str>; 3] {
    [
        TOKENS.to_vec(),
        vec!["input", "output", "cache_write"],
        Vec::new(),
    ]
}

fn claude_usage(recorded: &[&str]) -> Option<Value> {
    let counter = |name: &str| recorded.contains(&name);
    let usage = counters(recorded);
    if recorded.is_empty() {
        return None;
    }
    let mut native = json!({});
    for (name, field) in [
        ("input", "input_tokens"),
        ("output", "output_tokens"),
        ("cache_read", "cache_read_input_tokens"),
        ("cache_write", "cache_creation_input_tokens"),
    ] {
        if counter(name) {
            native[field] = usage[name].clone();
        }
    }
    if counter("reasoning") {
        native["output_tokens_details"] = json!({"thinking_tokens": usage["reasoning"]});
    }
    Some(native)
}

fn claude_store(label: &str, sessions: &[(&str, Vec<Vec<&str>>)]) -> Store {
    let store = Store::new(label);
    for (id, requests) in sessions {
        let mut lines = vec![json!({
            "type": "user", "sessionId": id, "uuid": "user",
            "timestamp": "2026-01-01T10:00:00Z",
            "message": {"role": "user", "content": "fixture"}
        })];
        for (index, recorded) in requests.iter().enumerate() {
            let mut message = json!({"role": "assistant", "model": "fixture-model", "content": []});
            if let Some(usage) = claude_usage(recorded) {
                message["usage"] = usage;
            }
            lines.push(json!({
                "type": "assistant", "sessionId": id,
                "requestId": format!("{id}-{index}"),
                "uuid": format!("{id}-{index}"),
                "message": message
            }));
        }
        store.write(&format!("fixture-project/{id}.jsonl"), &lines);
    }
    store
}

fn pi_store(label: &str, sessions: &[(&str, Vec<Vec<&str>>)]) -> Store {
    let store = Store::new(label);
    for (id, requests) in sessions {
        let mut lines = vec![json!({
            "type": "session", "version": 3, "id": id,
            "timestamp": "2026-01-01T10:00:00Z"
        })];
        let mut parent = Value::Null;
        for (index, recorded) in requests.iter().enumerate() {
            let entry = format!("{id}-{index}");
            let usage = counters(recorded);
            let mut message = json!({"role": "assistant", "model": "fixture-model", "content": []});
            if !recorded.is_empty() {
                message["usage"] = json!({
                    "input": usage.get("input"), "output": usage.get("output"),
                    "reasoning": usage.get("reasoning"),
                    "cacheRead": usage.get("cache_read"),
                    "cacheWrite": usage.get("cache_write"),
                    "cost": {"total": 0.1}
                });
            }
            lines.push(json!({
                "type": "message", "id": entry, "parentId": parent, "message": message
            }));
            parent = json!(entry);
        }
        store.write(&format!("2026-01-01T10-00-00-000Z_{id}.jsonl"), &lines);
    }
    store
}

fn codex_store(label: &str, id: &str, requests: &[Vec<&str>]) -> Store {
    let store = Store::new(label);
    let mut lines = vec![
        json!({"type": "session_meta", "payload": {"id": id, "timestamp": "2026-01-01T10:00:00Z"}}),
        json!({"type": "turn_context", "payload": {"model": "fixture-model"}}),
    ];
    for (index, recorded) in requests.iter().enumerate() {
        let usage = counters(recorded);
        let mut payload = json!({"response_id": format!("response-{index}")});
        if !recorded.is_empty() {
            payload["usage"] = json!({
                "input_tokens": usage.get("input"),
                "output_tokens": usage.get("output"),
                "reasoning_output_tokens": usage.get("reasoning"),
                "cached_input_tokens": usage.get("cache_read"),
                "cache_write_input_tokens": usage.get("cache_write"),
            });
        }
        lines.push(json!({"type": "token_usage_record", "payload": payload}));
    }
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &lines,
    );
    store
}

fn view(backend: Box<dyn Backend>, id: &str, full: bool) -> Value {
    let backends = vec![backend];
    let view = if full {
        usage_full_with_backends(&backends, Selection::Id(id)).unwrap()
    } else {
        usage_with_backends(&backends, Selection::Id(id)).unwrap()
    };
    serde_json::to_value(view).unwrap()
}

fn coverage(measured: usize, missing: usize) -> Value {
    json!({"measured": measured, "missing": missing, "unpriced": 0})
}

fn reasons<'a>(figure: &'a Value, counter: &str) -> Vec<&'a str> {
    figure["measurement_completeness"][counter]
        .as_array()
        .map(|reasons| reasons.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The coverage the partial session's requests record, counter by counter.
fn assert_partial_coverage(label: &str, figure: &Value) {
    for (counter, measured, missing) in [
        ("input", 2, 1),
        ("output", 2, 1),
        ("reasoning", 1, 2),
        ("cache_read", 1, 2),
        ("cache_write", 2, 1),
    ] {
        assert_eq!(
            figure["measurement_coverage"][counter],
            coverage(measured, missing),
            "{label} × {counter}: {figure}"
        );
        assert_eq!(
            reasons(figure, counter),
            ["missing-counter"],
            "{label} × {counter}: {figure}"
        );
    }
    assert!(
        figure["measurement_incomplete"]
            .as_array()
            .is_some_and(|notes| notes.len() >= TOKENS.len()),
        "{label}: {figure}"
    );
}

fn assert_whole(label: &str, figure: &Value, events: usize) {
    for counter in TOKENS {
        assert_eq!(
            figure["measurement_coverage"][counter],
            coverage(events, 0),
            "{label} × {counter}: {figure}"
        );
        assert!(reasons(figure, counter).is_empty(), "{label}: {figure}");
    }
}

#[test]
fn claude_and_pi_session_sums_report_per_counter_coverage() {
    for harness in ["claude", "pi"] {
        for full in [false, true] {
            let sessions = [
                ("partial", partial().to_vec()),
                ("whole", vec![TOKENS.to_vec(), TOKENS.to_vec()]),
            ];
            let store = if harness == "claude" {
                claude_store("sums", &sessions)
            } else {
                pi_store("sums", &sessions)
            };
            let backend = |store: &Store| -> Box<dyn Backend> {
                if harness == "claude" {
                    Box::new(ClaudeBackend::new(&store.0))
                } else {
                    Box::new(PiBackend::new(&store.0))
                }
            };
            let label = format!("{harness} full={full}");

            let partial = view(backend(&store), "partial", full);
            assert_eq!(partial["tokens"]["input"], 20, "{label}: {partial}");
            assert_partial_coverage(&label, &partial);
            for category in ["uncached_input", "cache_write", "output"] {
                assert_eq!(
                    reasons(&partial["disjoint"], category),
                    ["missing-counter"],
                    "{label} × {category}: {partial}"
                );
            }
            assert!(partial["disjoint"].get("total").is_none(), "{label}");

            let whole = view(backend(&store), "whole", full);
            assert_whole(&label, &whole, 2);
            assert_eq!(whole["disjoint"]["total"], 44, "{label}: {whole}");
        }
    }
}

#[test]
fn a_codex_model_split_reports_per_counter_coverage() {
    let id = "70000000-0000-0000-0000-000000000008";
    // Two requests with usage, and a third record that wrote none.
    let store = codex_store("codex", id, &partial());
    for full in [false, true] {
        let figure = view(Box::new(CodexBackend::new(&store.0)), id, full);
        let model = &figure["by_model"][0];
        let label = format!("codex full={full}");
        assert_eq!(model["tokens"]["input"], 20, "{label}: {figure}");
        for (counter, measured, missing) in [
            ("input", 2, 0),
            ("output", 2, 0),
            ("reasoning", 1, 1),
            ("cache_read", 1, 1),
            ("cache_write", 2, 0),
        ] {
            assert_eq!(
                model["measurement_coverage"][counter],
                coverage(measured, missing),
                "{label} × {counter}: {model}"
            );
            assert_eq!(
                reasons(model, counter).contains(&"missing-counter"),
                missing > 0,
                "{label} × {counter}: {model}"
            );
        }
        // The record that wrote no usage is not an amount of zero: the
        // read's attribution names it and qualifies every model share.
        assert!(
            figure["attribution"]["incomplete"]
                .as_array()
                .is_some_and(|reasons| !reasons.is_empty()),
            "{label}: {figure}"
        );
        for counter in TOKENS {
            assert!(
                reasons(model, counter).contains(&"incomplete-attribution"),
                "{label} × {counter}: {model}"
            );
        }
    }
}

#[test]
fn a_recorded_total_counts_as_one_event_per_counter() {
    let id = "70000000-0000-0000-0000-000000000009";
    let store = Store::new("codex-total");
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &[
            json!({"type": "session_meta", "payload": {"id": id, "timestamp": "2026-01-01T10:00:00Z"}}),
            json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 30, "output_tokens": 10, "cache_write_input_tokens": 0}
            }}}),
        ],
    );
    for full in [false, true] {
        let figure = view(Box::new(CodexBackend::new(&store.0)), id, full);
        for (counter, measured, missing) in [
            ("input", 1, 0),
            ("output", 1, 0),
            ("reasoning", 0, 1),
            ("cache_read", 0, 1),
            ("cache_write", 1, 0),
        ] {
            assert_eq!(
                figure["measurement_coverage"][counter],
                coverage(measured, missing),
                "full={full} × {counter}: {figure}"
            );
        }
    }
}

#[test]
fn a_claude_recorded_total_counts_the_models_it_sums() {
    let store = Store::new("claude-total");
    store.write(
        "fixture-project/recorded.jsonl",
        &[
            json!({"type": "user", "sessionId": "recorded", "uuid": "user",
                   "timestamp": "2026-01-01T10:00:00Z",
                   "message": {"role": "user", "content": "fixture"}}),
            json!({"type": "assistant", "sessionId": "recorded", "requestId": "one", "uuid": "one",
                   "message": {"role": "assistant", "model": "fixture-model", "content": [],
                               "usage": {"input_tokens": 10, "output_tokens": 4}}}),
            json!({"type": "cost-state", "sessionId": "recorded", "totalCostUSD": 0.5,
                   "modelUsage": {
                       "model-a": {"inputTokens": 20, "outputTokens": 8, "thinkingTokens": 2, "costUSD": 0.3},
                       "model-b": {"inputTokens": 20, "outputTokens": 8, "costUSD": 0.2}
                   }}),
        ],
    );
    let figure = view(Box::new(ClaudeBackend::new(&store.0)), "recorded", false);
    assert_eq!(figure["accounting"]["basis"], "recorded-total", "{figure}");
    assert_eq!(
        figure["measurement_coverage"]["reasoning"],
        coverage(1, 1),
        "{figure}"
    );
    assert_eq!(
        figure["measurement_coverage"]["input"],
        coverage(2, 0),
        "{figure}"
    );
    assert_eq!(
        reasons(&figure, "reasoning"),
        ["missing-counter"],
        "{figure}"
    );
    assert!(reasons(&figure, "input").is_empty(), "{figure}");
}

#[test]
fn a_summary_sums_event_coverage_and_withholds_what_a_partial_session_qualifies() {
    let store = claude_store(
        "summary",
        &[
            ("partial", partial().to_vec()),
            ("whole", vec![TOKENS.to_vec(), TOKENS.to_vec()]),
        ],
    );
    let backend = ClaudeBackend::new(&store.0);
    let sessions = ["partial", "whole"]
        .map(|id| backend.locate(id).unwrap().unwrap())
        .to_vec();
    let summary = serde_json::to_value(usage::aggregate(&sessions, &[GroupBy::Harness])).unwrap();
    for (label, figure) in [
        ("totals", &summary["totals"]),
        ("group", &summary["groups"][0]),
    ] {
        for (counter, measured, missing) in [
            ("input", 4, 1),
            ("output", 4, 1),
            ("reasoning", 3, 2),
            ("cache_read", 3, 2),
            ("cache_write", 4, 1),
        ] {
            assert_eq!(
                figure["measurement_coverage"][counter],
                coverage(measured, missing),
                "{label} × {counter}: {figure}"
            );
            assert_eq!(
                reasons(figure, counter),
                ["missing-counter"],
                "{label} × {counter}: {figure}"
            );
        }
    }
}

#[test]
fn a_summary_withholds_event_coverage_wherever_its_tokens_are_withheld() {
    // Pi sessions may share events, so no exact union exists; each session's
    // own coverage stays in its figure.
    let store = pi_store(
        "overlap",
        &[
            ("first", partial().to_vec()),
            ("second", vec![TOKENS.to_vec()]),
        ],
    );
    let backend = PiBackend::new(&store.0);
    let sessions = ["first", "second"]
        .map(|id| backend.locate(id).unwrap().unwrap())
        .to_vec();
    let summary = serde_json::to_value(usage::aggregate(&sessions, &[GroupBy::Harness])).unwrap();
    for (label, figure) in [
        ("totals", &summary["totals"]),
        ("group", &summary["groups"][0]),
    ] {
        assert!(figure.get("tokens").is_none(), "{label}: {figure}");
        assert!(
            figure.get("measurement_coverage").is_none(),
            "{label}: {figure}"
        );
        assert_eq!(
            figure["session_figures"][0]["measurement_coverage"]["input"],
            coverage(2, 1),
            "{label}: {figure}"
        );
        assert_eq!(
            figure["session_figures"][1]["measurement_coverage"]["input"],
            coverage(1, 0),
            "{label}: {figure}"
        );
    }
}

#[test]
fn a_summary_never_adds_event_coverage_across_accounting_bases() {
    let store = claude_store(
        "bases",
        &[
            ("summed", partial().to_vec()),
            ("recorded", vec![TOKENS.to_vec()]),
        ],
    );
    let path = store.0.join("fixture-project/recorded.jsonl");
    let mut recording = fs::read_to_string(&path).unwrap();
    recording.push_str(
        &json!({"type": "cost-state", "sessionId": "recorded", "totalCostUSD": 0.5,
                "modelUsage": {"model-a": {"inputTokens": 20, "outputTokens": 8,
                                           "cacheReadInputTokens": 10, "cacheCreationInputTokens": 10,
                                           "thinkingTokens": 1}}})
        .to_string(),
    );
    recording.push('\n');
    fs::write(&path, recording).unwrap();
    let backend = ClaudeBackend::new(&store.0);
    let sessions = ["summed", "recorded"]
        .map(|id| backend.locate(id).unwrap().unwrap())
        .to_vec();
    assert_eq!(
        sessions[1].accounting.as_ref().unwrap().basis,
        agent_tapes_core::model::AccountingBasis::RecordedTotal
    );
    let (aggregate, partitions) = usage::partitioned_aggregate(&sessions, &[GroupBy::Harness]);
    let totals = serde_json::to_value(&aggregate.totals).unwrap();
    assert_eq!(totals["mixed_accounting"], true, "{totals}");
    assert!(totals.get("measurement_coverage").is_none(), "{totals}");
    assert_eq!(partitions.len(), 2);
    for partition in partitions {
        let partition = serde_json::to_value(&partition).unwrap();
        assert_eq!(partition["sessions"], 1, "{partition}");
        assert!(
            partition["measurement_coverage"]["input"].is_object(),
            "{partition}"
        );
    }
}

#[test]
fn a_summary_counts_the_events_of_sessions_with_no_amount_as_missing() {
    // Three requests that wrote no counter, beside one that wrote `input`.
    let store = claude_store(
        "no-amount",
        &[
            ("empty", vec![Vec::new(), Vec::new(), Vec::new()]),
            ("one", vec![vec!["input"]]),
        ],
    );
    let backend = ClaudeBackend::new(&store.0);
    let sessions = ["empty", "one"]
        .map(|id| backend.locate(id).unwrap().unwrap())
        .to_vec();
    let summary = serde_json::to_value(usage::aggregate(&sessions, &[GroupBy::Harness])).unwrap();
    for (label, figure) in [
        ("totals", &summary["totals"]),
        ("group", &summary["groups"][0]),
    ] {
        assert_eq!(
            figure["measurement_coverage"]["input"],
            coverage(1, 3),
            "{label}: {figure}"
        );
        assert_eq!(
            figure["measurement_coverage"]["output"],
            coverage(0, 4),
            "{label}: {figure}"
        );
        assert_eq!(reasons(figure, "input"), ["missing-counter"], "{label}");
    }
}

#[test]
fn a_session_of_counter_less_requests_reports_every_counter_missing() {
    for harness in ["claude", "pi"] {
        let sessions = [("empty", vec![Vec::new(), Vec::new()])];
        let store = if harness == "claude" {
            claude_store("empty", &sessions)
        } else {
            pi_store("empty", &sessions)
        };
        let backend: Box<dyn Backend> = if harness == "claude" {
            Box::new(ClaudeBackend::new(&store.0))
        } else {
            Box::new(PiBackend::new(&store.0))
        };
        let figure = view(backend, "empty", false);
        for counter in TOKENS {
            assert_eq!(
                figure["measurement_coverage"][counter],
                coverage(0, 2),
                "{harness} × {counter}: {figure}"
            );
        }
        assert_eq!(
            figure["measurement_coverage"]["cost"],
            if harness == "pi" {
                coverage(0, 2)
            } else {
                Value::Null
            },
            "{harness}: {figure}"
        );
    }
}

#[test]
fn a_figure_that_counted_events_and_measured_nothing_qualifies_every_counter() {
    let completeness = MeasurementCompleteness::assess(Measured {
        events: 2,
        scope: Some(agent_tapes_core::model::AccountingCoverage::Session),
        ..Measured::default()
    });
    assert!(!completeness.is_complete(&["input"]));
    for counter in TOKENS {
        assert!(!completeness.is_complete(&[counter]), "{counter}");
    }
    assert!(!completeness.is_complete(&["cost"]));
    let none = MeasurementCompleteness::assess(Measured::default());
    assert!(none.is_empty(), "a figure with no events has no counter");
}

#[test]
fn codex_unattributed_tokens_report_per_counter_coverage() {
    let id = "70000000-0000-0000-0000-00000000000b";
    // No `turn_context`: no request can be attributed to a model.
    let store = Store::new("codex-unattributed");
    let mut lines = vec![
        json!({"type": "session_meta", "payload": {"id": id, "timestamp": "2026-01-01T10:00:00Z"}}),
    ];
    for (index, recorded) in partial().iter().take(2).enumerate() {
        let usage = counters(recorded);
        let mut payload = json!({"response_id": format!("response-{index}")});
        if !recorded.is_empty() {
            payload["usage"] = json!({
                "input_tokens": usage.get("input"),
                "output_tokens": usage.get("output"),
                "reasoning_output_tokens": usage.get("reasoning"),
                "cached_input_tokens": usage.get("cache_read"),
                "cache_write_input_tokens": usage.get("cache_write"),
            });
        }
        lines.push(json!({"type": "token_usage_record", "payload": payload}));
    }
    store.write(
        &format!("2026/01/01/rollout-2026-01-01T10-00-00-{id}.jsonl"),
        &lines,
    );
    let figure = view(Box::new(CodexBackend::new(&store.0)), id, false);
    let attribution = &figure["attribution"];
    assert_eq!(attribution["unattributed"], 2, "{figure}");
    assert_eq!(attribution["unattributed_tokens"]["input"], 20, "{figure}");
    for (counter, measured, missing) in [
        ("input", 2, 0),
        ("output", 2, 0),
        ("reasoning", 1, 1),
        ("cache_read", 1, 1),
        ("cache_write", 2, 0),
    ] {
        assert_eq!(
            attribution["unattributed_coverage"][counter],
            coverage(measured, missing),
            "{counter}: {attribution}"
        );
        let qualified =
            json!({"measurement_completeness": attribution["unattributed_completeness"]});
        assert_eq!(
            reasons(&qualified, counter),
            if missing > 0 {
                vec!["missing-counter"]
            } else {
                Vec::new()
            },
            "{counter}: {attribution}"
        );
    }
}

#[test]
fn a_pi_consumption_without_counters_keeps_the_coverage_every_reader_reports() {
    let store = pi_store("pi-empty", &[("empty", vec![Vec::new(), Vec::new()])]);
    let figure = view(Box::new(PiBackend::new(&store.0)), "empty", true);
    let consumption = &figure["consumption"];
    for counter in TOKENS {
        assert_eq!(
            consumption["measurement_coverage"][counter],
            coverage(0, 2),
            "{counter}: {consumption}"
        );
        assert_eq!(
            figure["measurement_coverage"][counter], consumption["measurement_coverage"][counter],
            "{counter}: {figure}"
        );
        assert_eq!(
            reasons(consumption, counter),
            ["missing-counter"],
            "{counter}"
        );
    }
}
