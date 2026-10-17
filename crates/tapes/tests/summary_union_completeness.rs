//! A Pi parent and fork add to a summary by their exact union, and the
//! union's figures carry the reasons any of its distinct events is
//! incompletely measured: in JSON on the totals, groups and partitions, and
//! in brackets on the human COST cell. A healthy session beside the pair
//! keeps its own completeness.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const PARENT: &str = "2026-01-15T10-00-00-000Z_0b1e6f3a-0000-4000-8000-00000000a001.jsonl";
const FORK: &str = "2026-01-15T11-00-00-000Z_0b1e6f3a-0000-4000-8000-00000000b002.jsonl";
const HEALTHY: &str = "2026-01-15T12-00-00-000Z_0b1e6f3a-0000-4000-8000-00000000c003.jsonl";

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-union-completeness-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, directory: &str, name: &str, lines: &[Value]) -> PathBuf {
        let path = self.0.join("sessions").join(directory).join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(&path, body).unwrap();
        path
    }

    fn usage(&self, json: bool) -> String {
        let root = self.0.display().to_string();
        let mut args = vec![
            "usage",
            "--native-store",
            "pi",
            "--native-root",
            root.as_str(),
            "--global",
            "--by",
            "directory",
        ];
        if json {
            args.push("--json");
        }
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .args(&args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(Path::new(&self.0));
    }
}

fn header(id: &str, hour: u32, cwd: &str, parent: Option<&Path>) -> Value {
    let mut line = json!({"type": "session", "version": 3, "id": id,
                          "timestamp": format!("2026-01-15T{hour:02}:00:00Z"), "cwd": cwd});
    if let Some(parent) = parent {
        line["parentSession"] = json!(parent.display().to_string());
    }
    line
}

fn request(id: &str, parent: Option<&str>, input: u64, cost: Option<f64>, stop: &str) -> Value {
    let mut usage = json!({"input": input, "output": 1, "reasoning": 0, "cacheRead": 0,
                           "cacheWrite": 0, "totalTokens": input + 1});
    if let Some(cost) = cost {
        usage["cost"] = json!({"total": cost});
    }
    json!({"type": "message", "id": id, "parentId": parent, "timestamp": "2026-01-15T10:00:01Z",
           "message": {"role": "assistant", "provider": "fixture-provider", "model": "fixture-model",
                       "responseId": format!("response-{id}"), "stopReason": stop,
                       "usage": usage, "content": [{"type": "text", "text": "A response."}]}})
}

/// A priced parent, a fork whose own request is `fork_request`, and an
/// unrelated priced session in another directory.
fn store(label: &str, fork_request: Value) -> Root {
    let root = Root::new(label);
    let shared = request("shared-request", None, 10, Some(0.125), "stop");
    let parent = root.write(
        "--fixtures-pair--",
        PARENT,
        &[
            header(
                "0b1e6f3a-0000-4000-8000-00000000a001",
                10,
                "/fixtures/pair",
                None,
            ),
            shared.clone(),
            request(
                "parent-request",
                Some("shared-request"),
                30,
                Some(0.375),
                "stop",
            ),
        ],
    );
    root.write(
        "--fixtures-pair--",
        FORK,
        &[
            header(
                "0b1e6f3a-0000-4000-8000-00000000b002",
                11,
                "/fixtures/pair",
                Some(&parent),
            ),
            shared,
            fork_request,
        ],
    );
    root.write(
        "--fixtures-healthy--",
        HEALTHY,
        &[
            header(
                "0b1e6f3a-0000-4000-8000-00000000c003",
                12,
                "/fixtures/healthy",
                None,
            ),
            request("healthy-request", None, 5, Some(0.25), "stop"),
        ],
    );
    root
}

fn group<'a>(value: &'a Value, directory: &str) -> &'a Value {
    value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["key"]["directory"] == directory)
        .unwrap_or_else(|| panic!("no group {directory}: {value}"))
}

fn assert_union_reason(label: &str, fork_request: Value, cost_reason: &str) {
    let root = store(label, fork_request);
    let value: Value = serde_json::from_str(&root.usage(true)).unwrap();
    let pair = group(&value, "/fixtures/pair");
    assert_eq!(pair["overlap"]["related"][0]["union"], "exact", "{pair}");
    for figure in [&value["totals"], pair, &value["partitions"][0]] {
        let reasons = figure["measurement_completeness"]["cost"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: no cost reason on {figure}"));
        assert!(reasons.contains(&json!(cost_reason)), "{label}: {figure}");
    }
    let healthy = group(&value, "/fixtures/healthy");
    assert!(
        healthy.get("measurement_completeness").is_none(),
        "{label}: {healthy}"
    );
    assert_eq!(healthy["cost"]["usd"], 0.25, "{label}: {healthy}");

    let human = root.usage(false);
    let row = |prefix: &str| {
        human
            .lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("{label}: no row {prefix}: {human}"))
            .to_owned()
    };
    for prefix in ["/fixtures/pair\t", "TOTAL\t"] {
        let row = row(prefix);
        let cell = row.split("USD [").nth(1).unwrap_or_default();
        assert!(cell.contains(cost_reason), "{label}: {human}");
    }
    let healthy = row("/fixtures/healthy\t");
    assert!(healthy.contains("0.2500 USD\t"), "{label}: {healthy}");
    assert!(!healthy.contains('['), "{label}: {healthy}");
}

#[test]
fn an_exact_union_with_an_unpriced_event_names_the_missing_price() {
    assert_union_reason(
        "missing",
        request("fork-request", Some("shared-request"), 40, None, "stop"),
        "missing-price",
    );
}

#[test]
fn an_exact_union_with_a_zero_written_price_names_it_unpriced() {
    assert_union_reason(
        "zero",
        request(
            "fork-request",
            Some("shared-request"),
            40,
            Some(0.0),
            "stop",
        ),
        "unpriced",
    );
}

#[test]
fn an_exact_union_with_a_stopped_event_names_the_stopped_attempt() {
    assert_union_reason(
        "stopped",
        request(
            "fork-request",
            Some("shared-request"),
            40,
            Some(0.5),
            "aborted",
        ),
        "stopped-attempt",
    );
}
