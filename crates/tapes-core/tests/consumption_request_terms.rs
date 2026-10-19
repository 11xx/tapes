//! A pi recording's retained-branch `consumption` prices a
//! `tapes-price-table/2` time-of-day term request by request at each event's
//! own time, as the active-branch figure does, whether its events lie on
//! the active branch alone or on abandoned branches too. An event whose
//! time the recording does not hold is priced between the rates it could
//! pay, with `request-terms-unknown`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{pi::PiBackend, Backend};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-consumption-terms-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write_lines(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(&path, body.join("\n") + "\n").unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SESSION_ID: &str = "019f0000-0000-7000-8000-0000000000f1";

const DEEPSEEK_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "deepseek-v4-pro"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 1.32, cache_read = 0.044, output = 3.96 }

[[models.time_of_day]]
from = "16:30"
until = "00:30"
per_million = { uncached_input = 0.66, cache_read = 0.022, output = 1.98 }
"#;

/// One request at peak rates: 10k uncached x 1.32 + 90k read x 0.044 + 2k
/// output x 3.96 per million. Off-peak is half of it.
const PEAK: f64 = (13_200.0 + 3_960.0 + 7_920.0) / 1e6;
const OFF_PEAK: f64 = PEAK / 2.0;

fn user(id: &str, parent: &str, stamp: &str) -> Value {
    json!({"type": "message", "id": id, "parentId": parent, "timestamp": stamp,
           "message": {"role": "user", "timestamp": 1_791_000_000_000_u64,
                       "content": [{"type": "text", "text": "Continue."}]}})
}

/// A DeepSeek request; `stamp` is the entry's time, absent when the
/// recording holds none for it.
fn assistant(id: &str, parent: &str, stamp: Option<&str>) -> Value {
    let mut entry = json!({"type": "message", "id": id, "parentId": parent,
           "message": {"role": "assistant", "timestamp": 1_791_000_000_000_u64,
                       "provider": "deepseek", "model": "deepseek-v4-pro", "stopReason": "stop",
                       "usage": {"input": 10_000, "output": 2_000, "cacheRead": 90_000,
                                 "cacheWrite": 0, "totalTokens": 102_000,
                                 "cost": {"total": 0.0251}},
                       "content": [{"type": "text", "text": "Done."}]}});
    if let Some(stamp) = stamp {
        entry["timestamp"] = json!(stamp);
    }
    entry
}

/// Write a pi recording opened at 10:00 UTC whose entries follow its header
/// and model change, and return the consumption and figure estimates of
/// its bounded and whole usage views.
fn estimates(label: &str, entries: &[Value]) -> Vec<(Value, Value, Value)> {
    let scratch = Scratch::new(label);
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": SESSION_ID,
               "timestamp": "2026-10-19T10:00:00.000Z", "cwd": "/fixtures/project"}),
        json!({"type": "model_change", "id": "model-1", "parentId": null,
               "timestamp": "2026-10-19T10:00:00.000Z", "provider": "deepseek",
               "modelId": "deepseek-v4-pro"}),
    ];
    lines.extend_from_slice(entries);
    scratch.write_lines(
        &format!("--fixtures-project--/2026-10-19T10-00-00-000Z_{SESSION_ID}.jsonl"),
        &lines,
    );
    let table = PriceTable::parse(DEEPSEEK_TABLE).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(PiBackend::new(&scratch.0)) };
    [false, true]
        .into_iter()
        .map(|full| {
            let mut view = if full {
                usage_full_with_backends(&[backend()], Selection::Id(SESSION_ID))
            } else {
                usage_with_backends(&[backend()], Selection::Id(SESSION_ID))
            }
            .unwrap();
            pricing::price_view(&mut view, &table);
            let view = serde_json::to_value(view).unwrap();
            (
                view["consumption"]["list_price_estimate"].clone(),
                view["list_price_estimate"].clone(),
                view["consumption"]["events"].clone(),
            )
        })
        .collect()
}

fn close(value: &Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|value| (value - expected).abs() < 1e-9)
}

/// Claim: a consumption holding only the active branch's requests, one at
/// 10:00 and one at 18:00 UTC, is priced exactly as the figure over the
/// same requests: peak for the first, off-peak for the second, with no
/// reason. Rejects pricing it between peak and off-peak rates with
/// `request-terms-unknown` while the figure reads exact.
#[test]
fn a_consumption_with_no_other_branch_is_priced_as_its_figure() {
    for (consumption, figure, events) in estimates(
        "single-branch",
        &[
            user("user-1", "model-1", "2026-10-19T10:00:01.000Z"),
            assistant("assistant-1", "user-1", Some("2026-10-19T10:00:30.000Z")),
            user("user-2", "assistant-1", "2026-10-19T18:00:01.000Z"),
            assistant("assistant-2", "user-2", Some("2026-10-19T18:00:30.000Z")),
        ],
    ) {
        assert_eq!(events["other_branches"], 0, "{events:#}");
        assert!(close(&figure["amount"], PEAK + OFF_PEAK), "{figure:#}");
        assert_eq!(consumption, figure, "{consumption:#}");
    }
}

/// Claim: a consumption holding an abandoned branch's request made at
/// 12:00 UTC beside the active branch's 10:00 and 18:00 requests is priced
/// by each request's own time: two at peak, one off-peak, with no reason.
/// Rejects pricing it between peak and off-peak rates with
/// `request-terms-unknown`, and pricing the abandoned request at the
/// active branch's rates.
#[test]
fn a_consumption_with_another_branch_reads_each_requests_time() {
    for (consumption, figure, events) in estimates(
        "other-branch",
        &[
            user("user-1", "model-1", "2026-10-19T10:00:01.000Z"),
            assistant("assistant-1", "user-1", Some("2026-10-19T10:00:30.000Z")),
            user("user-2", "assistant-1", "2026-10-19T12:00:01.000Z"),
            assistant("assistant-2", "user-2", Some("2026-10-19T12:00:30.000Z")),
            user("user-3", "assistant-1", "2026-10-19T18:00:01.000Z"),
            assistant("assistant-3", "user-3", Some("2026-10-19T18:00:30.000Z")),
        ],
    ) {
        assert_eq!(events["other_branches"], 1, "{events:#}");
        assert!(close(&figure["amount"], PEAK + OFF_PEAK), "{figure:#}");
        assert!(
            close(&consumption["amount"], 2.0 * PEAK + OFF_PEAK),
            "{consumption:#}"
        );
        assert!(
            consumption.get("measurement_completeness").is_none(),
            "{consumption:#}"
        );
    }
}

/// Claim: an abandoned branch's request the recording holds no time for
/// leaves the consumption a range by that request alone, between its
/// off-peak and peak rates, its categories qualified
/// `request-terms-unknown`, while the timed requests keep their own rates.
/// Rejects an exact amount for an untimed request, and widening the range
/// over the timed requests as well.
#[test]
fn an_untimed_request_on_another_branch_leaves_a_range_with_its_reason() {
    for (consumption, _, events) in estimates(
        "untimed-branch",
        &[
            user("user-1", "model-1", "2026-10-19T10:00:01.000Z"),
            assistant("assistant-1", "user-1", Some("2026-10-19T10:00:30.000Z")),
            user("user-2", "assistant-1", "2026-10-19T12:00:01.000Z"),
            assistant("assistant-2", "user-2", None),
            user("user-3", "assistant-1", "2026-10-19T18:00:01.000Z"),
            assistant("assistant-3", "user-3", Some("2026-10-19T18:00:30.000Z")),
        ],
    ) {
        assert_eq!(events["other_branches"], 1, "{events:#}");
        assert!(consumption.get("amount").is_none(), "{consumption:#}");
        assert!(
            close(&consumption["at_least"], PEAK + 2.0 * OFF_PEAK),
            "{consumption:#}"
        );
        assert!(
            close(&consumption["at_most"], 2.0 * PEAK + OFF_PEAK),
            "{consumption:#}"
        );
        for category in ["uncached_input", "cache_read", "output"] {
            assert_eq!(
                consumption["measurement_completeness"][category],
                json!(["request-terms-unknown"]),
                "{consumption:#}"
            );
        }
    }
}
