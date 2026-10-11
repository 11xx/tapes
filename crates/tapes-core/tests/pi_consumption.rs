//! pi keeps every branch a rewind leaves behind. The session's counters sum
//! the active branch; the consumption view sums every retained branch, each
//! consumption event once, and both say what they selected and covered.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::pi::PiBackend;
use agent_tapes_core::backend::{Backend, MIN_READ_BYTES};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

const REWOUND: &str = "2026-01-05T10-00-00-000Z_session-rewound.jsonl";

fn fixtures(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(path)
}

struct Temp(PathBuf);

impl Temp {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tapes-pi-consumption-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The rewound fixture's lines, rewritten by `edit` into a store of its own.
fn rewound_variant(label: &str, edit: impl FnOnce(&mut Vec<String>)) -> Temp {
    let temp = Temp::new(label);
    let mut lines = fs::read_to_string(fixtures("rewound/pi").join(REWOUND))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    edit(&mut lines);
    fs::write(temp.0.join(REWOUND), lines.join("\n") + "\n").unwrap();
    temp
}

fn line_index(lines: &[String], id: &str) -> usize {
    let needle = format!(r#""id":"{id}""#);
    lines
        .iter()
        .position(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("fixture entry {id} is missing"))
}

fn bounded(backend: PiBackend, id: &str) -> Value {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(backend)];
    serde_json::to_value(usage_with_backends(&backends, Selection::Id(id)).unwrap()).unwrap()
}

fn full(backend: PiBackend, id: &str) -> Value {
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(backend)];
    serde_json::to_value(usage_full_with_backends(&backends, Selection::Id(id)).unwrap()).unwrap()
}

/// Both reading paths of one store.
fn both(root: &Path, id: &str) -> [(&'static str, Value); 2] {
    [
        ("bounded", bounded(PiBackend::new(root), id)),
        ("full", full(PiBackend::new(root), id)),
    ]
}

fn active_tokens() -> Value {
    json!({
        "input": 400,
        "output": 40,
        "reasoning": 8,
        "cache_read": 200,
        "cache_write": 20
    })
}

fn consumed_tokens() -> Value {
    json!({
        "input": 1030,
        "output": 103,
        "reasoning": 20,
        "cache_read": 500,
        "cache_write": 50
    })
}

/// The fixture drafts one approach over two requests, rewinds to the first
/// reply with a branch summary, and drafts a second approach. The session's
/// counters are the active branch's requests; consumption adds the abandoned
/// requests and the summary the rewind paid for.
#[test]
fn a_rewound_recording_reports_active_and_consumption_totals_apart() {
    for (path, view) in both(&fixtures("rewound/pi"), "session-rewound") {
        assert_eq!(view["tokens"], active_tokens(), "{path}: {view}");
        assert_eq!(view["cost"]["usd"], json!(0.5), "{path}: {view}");
        assert_eq!(
            view["accounting"],
            json!({
                "basis": "summed-requests",
                "coverage": "session",
                "selection": "active-branch"
            }),
            "{path}: {view}"
        );
        let consumption = &view["consumption"];
        assert_eq!(consumption["tokens"], consumed_tokens(), "{path}");
        assert_eq!(consumption["cost"]["usd"], 1.3125, "{path}");
        assert_eq!(
            consumption["measurement_coverage"]["reasoning"],
            json!({"measured":4,"missing":1,"unpriced":0}),
            "{path}"
        );
        assert_eq!(consumption["events"]["counted"], 5, "{path}");
        assert_eq!(consumption["events"]["active_branch"], 3, "{path}");
        assert_eq!(consumption["events"]["other_branches"], 2, "{path}");
        assert_eq!(consumption["identity"]["provider_response"], 4, "{path}");
    }
}

/// A re-read derives the same figures, and an observation of an event the
/// read already counted is a repeat: the same entry written twice, or another
/// entry carrying the same provider response.
#[test]
fn rereading_or_reimporting_evidence_adds_no_consumption() {
    let root = fixtures("rewound/pi");
    for (path, first) in both(&root, "session-rewound") {
        let again = if path == "bounded" {
            bounded(PiBackend::new(&root), "session-rewound")
        } else {
            full(PiBackend::new(&root), "session-rewound")
        };
        assert_eq!(first["consumption"], again["consumption"], "{path}");
    }

    let store = rewound_variant("repeats", |lines| {
        let duplicate = line_index(lines, "assistant-2a");
        let copy = lines[duplicate].clone();
        lines.insert(duplicate + 1, copy);
        let imported = line_index(lines, "assistant-3a");
        let copy = lines[imported].replace(r#""id":"assistant-3a""#, r#""id":"imported-3a""#);
        lines.insert(imported + 1, copy);
    });
    for (path, view) in both(&store.0, "session-rewound") {
        let consumption = &view["consumption"];
        assert_eq!(consumption["tokens"], consumed_tokens(), "{path}: {view}");
        assert_eq!(consumption["cost"]["usd"], json!(1.3125), "{path}");
        assert_eq!(
            consumption["events"],
            json!({
                "counted": 5,
                "active_branch": 3,
                "other_branches": 2,
                "repeats": 2
            }),
            "{path}: {view}"
        );
        assert_eq!(view["tokens"], active_tokens(), "{path}: {view}");
    }
}

/// A record the read could not decode may have been consumption on any
/// branch. Off the active branch it leaves the active figure whole and the
/// consumption figure incomplete; on it, the active branch is cut short.
#[test]
fn an_unreadable_record_limits_only_the_figures_it_could_belong_to() {
    let abandoned = rewound_variant("unreadable-abandoned", |lines| {
        let index = line_index(lines, "assistant-3a");
        lines[index] = r#"{"type":"message","id":"assistant-3a","#.to_owned();
    });
    for (path, view) in both(&abandoned.0, "session-rewound") {
        assert_eq!(view["tokens"], active_tokens(), "{path}: {view}");
        assert_eq!(view["accounting"]["coverage"], "session", "{path}: {view}");
        let consumption = &view["consumption"];
        assert_eq!(consumption["accounting"]["coverage"], "session", "{path}");
        assert_eq!(consumption["events"]["counted"], 4, "{path}: {view}");
        assert_eq!(consumption["tokens"]["input"], 630, "{path}: {view}");
        assert_eq!(
            consumption["incomplete"],
            json!([
                "1 record could not be read",
                "reasoning is a known subtotal: 3/4 events measured, 1 missing, 0 unpriced"
            ]),
            "{path}: {view}"
        );
    }

    let active = rewound_variant("unreadable-active", |lines| {
        let index = line_index(lines, "user-2b");
        lines[index] = r#"{"type":"message","id":"user-2b","#.to_owned();
    });
    for (path, view) in both(&active.0, "session-rewound") {
        assert_eq!(
            view["accounting"]["coverage"], "read-window",
            "{path}: {view}"
        );
        assert_eq!(
            view["accounting"]["selection"], "active-branch",
            "{path}: {view}"
        );
        assert_eq!(
            view["consumption"]["incomplete"],
            json!([
                "1 record could not be read",
                "reasoning is a known subtotal: 4/5 events measured, 1 missing, 0 unpriced"
            ]),
            "{path}: {view}"
        );
    }

    let trailing = rewound_variant("unreadable-trailing", |lines| {
        lines.push(r#"{"type":"message","id":"assistant-3b","#.to_owned());
    });
    for (path, view) in both(&trailing.0, "session-rewound") {
        assert_eq!(
            view["accounting"]["coverage"], "read-window",
            "{path}: an unreadable last record may have been the leaf: {view}"
        );
    }
}

/// pi writes a request that failed or was aborted with the usage it had when
/// it stopped, so the figure counts the attempt but is not its measurement.
#[test]
fn a_failed_request_leaves_consumption_partial() {
    let store = rewound_variant("aborted", |lines| {
        let index = line_index(lines, "assistant-3a");
        lines[index] = lines[index].replace(r#""stopReason":"stop""#, r#""stopReason":"aborted""#);
    });
    for (path, view) in both(&store.0, "session-rewound") {
        let consumption = &view["consumption"];
        assert_eq!(consumption["events"]["counted"], 5, "{path}: {view}");
        assert_eq!(
            consumption["incomplete"],
            json!([
                "1 request stopped on an error or an abort; its recorded usage is partial",
                "reasoning is a known subtotal: 4/5 events measured, 1 missing, 0 unpriced"
            ]),
            "{path}: {view}"
        );
    }
}

/// A fork copies its parent's entries, identities included, so events the
/// fork shares with its parent are witnessed in both recordings. Without
/// response identities only a union by recording can tell them apart.
#[test]
fn a_fork_names_the_recording_its_events_may_repeat() {
    for (path, view) in both(&fixtures("pi"), "session-pi") {
        let consumption = &view["consumption"];
        assert!(
            !view.to_string().contains("private-owner"),
            "{path}: {view}"
        );
        assert!(
            !view.to_string().contains("encoded-project"),
            "{path}: {view}"
        );
        assert!(
            !view.to_string().contains("2025-12-31T10-00-00-000Z"),
            "{path}: {view}"
        );
        assert_eq!(
            consumption["identity"],
            json!({
                "provider_response": 0,
                "recording_entry": 3,
                "unidentified": 0,
                "forked_from": "e0581212-d966-4d00-91c8-235b728a28c1"
            }),
            "{path}: {view}"
        );
        assert_eq!(
            consumption["tokens"],
            json!({
                "input": 1040,
                "output": 2060,
                "reasoning": 3010,
                "cache_read": 4012,
                "cache_write": 5014
            }),
            "{path}: {view}"
        );
        assert_eq!(consumption["cost"]["usd"], json!(99.375), "{path}");
        assert_eq!(view["cost"]["usd"], json!(0.375), "{path}");
    }
}

/// A rewind to before the first entry starts a new root, so the active
/// branch can lie wholly inside a bounded tail while the branch it replaced
/// lies before it. The bounded read reports the active branch complete and
/// consumption bounded; the whole read covers both.
#[test]
fn a_bounded_read_holds_the_whole_active_branch_but_not_all_consumption() {
    let temp = Temp::new("new-root");
    let id = "session-new-root";
    let usage = |input: u64, cost: f64| {
        format!(
            r#""usage":{{"input":{input},"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":{},"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":{cost}}}}}"#,
            input + 1
        )
    };
    let assistant = |entry: &str, parent: &str, input: u64, cost: f64| {
        format!(
            r#"{{"type":"message","id":"{entry}","parentId":"{parent}","timestamp":"2026-01-06T10:00:02Z","message":{{"role":"assistant","timestamp":0,"provider":"fixture-provider","model":"pi-model-a","responseId":"resp-{entry}","stopReason":"stop",{},"content":[{{"type":"text","text":"Reply."}}]}}}}"#,
            usage(input, cost)
        )
    };
    let mut lines = vec![
        format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-06T10:00:00Z","cwd":"/fixtures/project"}}"#
        ),
        r#"{"type":"message","id":"first-user","parentId":null,"timestamp":"2026-01-06T10:00:01Z","message":{"role":"user","timestamp":0,"content":[{"type":"text","text":"Start."}]}}"#.to_owned(),
        assistant("first-assistant", "first-user", 1000, 2.0),
    ];
    let filler = "x".repeat(1024);
    let mut parent = "first-assistant".to_owned();
    for index in 0..96 {
        let entry = format!("filler-{index}");
        lines.push(format!(
            r#"{{"type":"message","id":"{entry}","parentId":"{parent}","timestamp":"2026-01-06T10:00:03Z","message":{{"role":"user","timestamp":0,"content":[{{"type":"text","text":"{filler}"}}]}}}}"#
        ));
        parent = entry;
    }
    lines.push(
        r#"{"type":"message","id":"second-user","parentId":null,"timestamp":"2026-01-06T10:01:00Z","message":{"role":"user","timestamp":0,"content":[{"type":"text","text":"Start over."}]}}"#
            .to_owned(),
    );
    lines.push(assistant("second-assistant", "second-user", 10, 0.5));
    fs::write(
        temp.0.join(format!("2026-01-06T10-00-00-000Z_{id}.jsonl")),
        lines.join("\n") + "\n",
    )
    .unwrap();

    let view = bounded(PiBackend::new(&temp.0).with_read_bytes(MIN_READ_BYTES), id);
    assert_eq!(view["truncated"], true, "{view}");
    assert_eq!(
        view["accounting"],
        json!({
            "basis": "summed-requests",
            "coverage": "session",
            "selection": "active-branch"
        }),
        "{view}"
    );
    assert_eq!(view["tokens"]["input"], 10, "{view}");
    let consumption = &view["consumption"];
    assert_eq!(
        consumption["accounting"]["coverage"], "read-window",
        "{view}"
    );
    assert_eq!(consumption["tokens"]["input"], 10, "{view}");
    assert_eq!(consumption["events"]["counted"], 1, "{view}");

    let view = full(PiBackend::new(&temp.0), id);
    assert_eq!(view["accounting"]["coverage"], "session", "{view}");
    assert_eq!(view["tokens"]["input"], 10, "{view}");
    let consumption = &view["consumption"];
    assert_eq!(consumption["accounting"]["coverage"], "session", "{view}");
    assert_eq!(consumption["tokens"]["input"], 1010, "{view}");
    assert_eq!(consumption["cost"]["usd"], json!(2.5), "{view}");
    assert_eq!(
        consumption["events"],
        json!({
            "counted": 2,
            "active_branch": 1,
            "other_branches": 1,
            "repeats": 0
        }),
        "{view}"
    );
}

#[test]
fn missing_dimensions_qualify_each_known_subtotal() {
    let store = rewound_variant("missing-dimensions", |lines| {
        let index = line_index(lines, "assistant-3a");
        let mut entry: Value = serde_json::from_str(&lines[index]).unwrap();
        let usage = entry["message"]["usage"].as_object_mut().unwrap();
        for field in ["input", "output", "reasoning", "cacheRead", "cacheWrite"] {
            usage.remove(field);
        }
        lines[index] = entry.to_string();
    });
    for (path, view) in both(&store.0, "session-rewound") {
        let consumption = &view["consumption"];
        for field in ["input", "output", "cache_read", "cache_write"] {
            assert_eq!(
                consumption["measurement_coverage"][field]["measured"], 4,
                "{path}: {view}"
            );
            assert_eq!(
                consumption["measurement_coverage"][field]["missing"], 1,
                "{path}: {view}"
            );
        }
        assert_eq!(
            consumption["measurement_coverage"]["reasoning"]["measured"], 3,
            "{path}"
        );
        assert_eq!(
            consumption["measurement_coverage"]["reasoning"]["missing"], 2,
            "{path}"
        );
        assert_eq!(consumption["tokens"]["input"], 630, "{path}");
        assert_eq!(view["tokens"], active_tokens(), "{path}");
        assert!(
            consumption["incomplete"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str().unwrap().contains("input")),
            "{view}"
        );
    }
}

#[test]
fn zero_cost_events_with_usage_are_unpriced_before_aggregation() {
    for all in [false, true] {
        let store = rewound_variant("unpriced", |lines| {
            for line in lines {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                let selected = all || entry["id"] == "assistant-3a";
                let usage = if entry["type"] == "message" {
                    &mut entry["message"]["usage"]
                } else {
                    &mut entry["usage"]
                };
                if selected && usage.is_object() {
                    usage["cost"] =
                        json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0});
                }
                *line = entry.to_string();
            }
        });
        for (path, view) in both(&store.0, "session-rewound") {
            let consumption = &view["consumption"];
            assert_eq!(
                consumption["cost"]["usd"],
                if all { 0.0 } else { 0.8125 },
                "{path}"
            );
            assert_eq!(
                consumption["measurement_coverage"]["cost"]["unpriced"],
                if all { 5 } else { 1 },
                "{path}: {view}"
            );
            assert!(
                consumption["incomplete"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v.as_str().unwrap().contains("unpriced")),
                "{path}: {view}"
            );
        }
    }
}

#[test]
fn identity_reports_the_unestablished_provider_account_namespace() {
    for (path, view) in both(&fixtures("rewound/pi"), "session-rewound") {
        assert_eq!(
            view["consumption"]["identity"]["provider_account_namespace"], "unestablished",
            "{path}: {view}"
        );
    }
}

#[test]
fn fork_lineage_normalizes_paths_and_preserves_uncertainty() {
    let store = rewound_variant("fork-lineage", |_| {});
    let parent_file = store.0.join(REWOUND);
    let child_file = store.0.join("2026-01-06T10-00-00-000Z_child.jsonl");
    for (reference, expected, resolved) in [
        (
            parent_file.to_string_lossy().into_owned(),
            Some("session-rewound"),
            true,
        ),
        ("session-rewound".to_owned(), Some("session-rewound"), true),
        (
            "/outside/2026-01-05T10-00-00-000Z_missing-parent.jsonl".to_owned(),
            Some("missing-parent"),
            false,
        ),
        (
            "/outside/2026-01-05T10-00-00-000Z_session-rewound.jsonl".to_owned(),
            Some("session-rewound"),
            false,
        ),
        ("/private-owner/unrecognized.jsonl".to_owned(), None, false),
    ] {
        let header = json!({"type":"session","version":3,"id":"child","parentSession":reference});
        fs::write(
            &child_file,
            format!(
                "{header}
"
            ),
        )
        .unwrap();
        let backend = PiBackend::new(&store.0);
        let child = backend.locate("child").unwrap().unwrap();
        for lineage in [
            backend.lineage(&child).unwrap(),
            backend.stream_lineage(&child).unwrap(),
        ] {
            assert_eq!(
                lineage.parent.as_ref().map(|p| p.native_id.as_str()),
                expected
            );
            assert_eq!(
                lineage.parent.as_ref().is_some_and(|p| p.resolved),
                resolved
            );
            assert!(!serde_json::to_string(&lineage)
                .unwrap()
                .contains("private-owner"));
            if expected.is_none() {
                assert!(!lineage.notes.is_empty());
            }
        }
    }
}

#[test]
fn active_accounting_names_excluded_entry_classes() {
    for (path, view) in both(&fixtures("rewound/pi"), "session-rewound") {
        assert!(
            view["notes"].as_array().unwrap().iter().any(|v| v
                .as_str()
                .unwrap()
                .contains("compaction, branch-summary, tool-result, and usage entries")),
            "{path}: {view}"
        );
    }
}

#[test]
fn total_only_usage_with_zero_cost_is_unpriced() {
    let store = rewound_variant("total-only", |lines| {
        let index = line_index(lines, "assistant-3a");
        let mut entry: Value = serde_json::from_str(&lines[index]).unwrap();
        entry["message"]["usage"] = json!({"totalTokens":660,"cost":{"total":0}});
        lines[index] = entry.to_string();
    });
    for (path, view) in both(&store.0, "session-rewound") {
        assert_eq!(view["consumption"]["cost"]["usd"], 0.8125, "{path}");
        assert_eq!(
            view["consumption"]["measurement_coverage"]["cost"]["unpriced"], 1,
            "{path}: {view}"
        );
        assert_eq!(
            view["consumption"]["measurement_coverage"]["cost"]["measured"], 4,
            "{path}"
        );
    }
}

#[test]
fn missing_prices_retain_only_the_known_cost_subtotal() {
    for all in [false, true] {
        let store = rewound_variant("missing-prices", |lines| {
            for line in lines {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                let selected = all || entry["id"] == "assistant-3a";
                let usage = if entry["type"] == "message" {
                    &mut entry["message"]["usage"]
                } else {
                    &mut entry["usage"]
                };
                if selected {
                    if let Some(usage) = usage.as_object_mut() {
                        usage.remove("cost");
                    }
                }
                *line = entry.to_string();
            }
        });
        for (path, view) in both(&store.0, "session-rewound") {
            let consumption = &view["consumption"];
            if all {
                assert!(consumption.get("cost").is_none(), "{path}: {view}");
            } else {
                assert_eq!(consumption["cost"]["usd"], 0.8125, "{path}");
            }
            assert_eq!(
                consumption["measurement_coverage"]["cost"]["missing"],
                if all { 5 } else { 1 },
                "{path}"
            );
            assert_eq!(
                consumption["measurement_coverage"]["cost"]["unpriced"], 0,
                "{path}"
            );
        }
    }
}

#[test]
fn an_unrecognized_fork_locator_reports_uncertainty_without_the_path() {
    let store = rewound_variant("unknown-parent", |lines| {
        let mut header: Value = serde_json::from_str(&lines[0]).unwrap();
        header["parentSession"] = json!("/private-owner/encoded-project/unrecognized.jsonl");
        lines[0] = header.to_string();
    });
    for (path, view) in both(&store.0, "session-rewound") {
        let identity = &view["consumption"]["identity"];
        assert!(identity.get("forked_from").is_none(), "{path}: {view}");
        assert_eq!(
            identity["fork_parent_unestablished"], true,
            "{path}: {view}"
        );
        assert!(!view.to_string().contains("private-owner"), "{path}");
        assert!(!view.to_string().contains("encoded-project"), "{path}");
        assert!(!view.to_string().contains("unrecognized.jsonl"), "{path}");
    }
}
