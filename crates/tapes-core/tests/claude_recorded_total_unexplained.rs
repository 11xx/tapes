//! A Claude `cost-state` totals a session's consumption per model and can
//! count usage that none of its request records shows: a lead whose
//! plugin-steered subagents write only zero-usage mirror rows records a
//! Haiku total no request explains. The records are cumulative, write no
//! timestamp, and sit between timestamped records, often written twice in a
//! row. tapes names the unexplained part per model beside `by_model`, places
//! each total's increase in the interval the records around it bound, and
//! prices the cache writes the requests explain at their own TTLs.

use std::fs;
use std::path::PathBuf;

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::Backend;
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::span::SpanBounds;
use agent_tapes_core::usage::{GroupBy, UsageOptions};
use agent_tapes_core::window::WindowBounds;
use agent_tapes_core::{
    usage_full_with_backends, usage_spans_with_backends, usage_window_with_backends,
    usage_with_options_with_backends, Selection, SessionSelection, WindowAsk,
};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The lead session.
const LEAD: &str = "5d0c7a92-3e41-4b8f-9a26-c17e0b4d8f53";
/// A subagent of the lead whose own Haiku requests its total counts.
const AGENT: &str = "a4f81c2e7b9d03a56";
/// A headless session whose total holds a few more cache writes than its
/// requests recorded.
const HEADLESS: &str = "8b3e5f17-0a6c-4d92-b1e4-7c2f9a0d6e38";
/// When the lead's runs started, as its cost-states write it.
const START_TIME: i64 = 1_792_576_800_000;

const TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "claude-opus-5-5"
effective = 2026-10-18
source = "fixture"
per_million = { uncached_input = 4.0, cache_read = 0.2, cache_write_5m = 5.0, cache_write_1h = 8.0, output = 20.0 }

[[models]]
id = "claude-haiku-5-5"
effective = 2026-10-18
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.25, cache_write_1h = 2.0, output = 5.0 }
"#;

struct Store {
    root: TempDir,
}

impl Store {
    fn new() -> Self {
        let store = Self {
            root: TempDir::with_prefix("tapes-claude-recorded-total-unexplained-").unwrap(),
        };
        fs::create_dir_all(store.project()).unwrap();
        store
    }

    fn projects(&self) -> PathBuf {
        self.root.path().join("projects")
    }

    fn project(&self) -> PathBuf {
        self.projects().join("-fixtures-project")
    }

    fn write(&self, path: PathBuf, records: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![Box::new(ClaudeBackend::new(self.projects()))]
    }

    /// The lead's recording and, when `with_agent`, its subagent's.
    ///
    /// - 10:01 an Opus request writes 400 one-hour cache tokens; the
    ///   recording's records then end at 10:10 and its total is written
    ///   twice: Opus as its request, and Haiku that no request explains.
    /// - 10:40 a resumed run's Opus request writes 500 more; at 10:45 the
    ///   subagent makes a Haiku request; the records end at 11:20 and the
    ///   total grows by the subagent's request, more Haiku no request
    ///   explains, and 100 Opus cache writes no request recorded.
    fn lead(with_agent: bool) -> Self {
        let store = Self::new();
        store.write(
            store.project().join(format!("{LEAD}.jsonl")),
            &[
                json!({"type": "mode", "mode": "default", "sessionId": LEAD}),
                prompt(LEAD, "lead-user-1", "2026-10-20T10:00:00.000Z"),
                opus(
                    LEAD,
                    "req_lead_1",
                    "2026-10-20T10:01:00.000Z",
                    [2, 1000, 0, 400, 100, 10],
                ),
                turn_duration(LEAD, "2026-10-20T10:10:00.000Z"),
                cost_state(
                    LEAD,
                    [[2, 100, 10, 1000, 400], [10, 200, 0, 3000, 1000]],
                    600_000,
                ),
                cost_state(
                    LEAD,
                    [[2, 100, 10, 1000, 400], [10, 200, 0, 3000, 1000]],
                    600_027,
                ),
                prompt(LEAD, "lead-user-2", "2026-10-20T10:30:00.000Z"),
                opus(
                    LEAD,
                    "req_lead_2",
                    "2026-10-20T10:40:00.000Z",
                    [3, 2000, 0, 500, 200, 20],
                ),
                turn_duration(LEAD, "2026-10-20T11:20:00.000Z"),
                cost_state(
                    LEAD,
                    [[5, 300, 30, 3000, 1000], [36, 650, 0, 9500, 3100]],
                    3_600_000,
                ),
                cost_state(
                    LEAD,
                    [[5, 300, 30, 3000, 1000], [36, 650, 0, 9500, 3100]],
                    3_600_019,
                ),
            ],
        );
        if with_agent {
            let subagents = store.project().join(LEAD).join("subagents");
            store.write(
                subagents.join(format!("agent-{AGENT}.jsonl")),
                &[
                    json!({"type": "user", "sessionId": LEAD, "agentId": AGENT,
                        "isSidechain": true, "uuid": "agent-user", "parentUuid": null,
                        "timestamp": "2026-10-20T10:44:00.000Z", "cwd": "/fixtures/project",
                        "entrypoint": "cli",
                        "message": {"role": "user", "content": "Run the delegated slice."}}),
                    mirror_row("agent-mirror-1", "2026-10-20T10:44:30.000Z"),
                    haiku_request("2026-10-20T10:45:00.000Z"),
                    mirror_row("agent-mirror-2", "2026-10-20T10:46:00.000Z"),
                ],
            );
            fs::write(
                subagents.join(format!("agent-{AGENT}.meta.json")),
                json!({"agentType": "delegated-agents:run", "pluginSteered": true,
                    "description": "delegated fixture slice", "toolUseId": "toolu_plugin_fixture",
                    "spawnDepth": 1, "requestShape": "background",
                    "requestNonInteractive": true, "finished": true})
                .to_string(),
            )
            .unwrap();
        }
        store
    }

    fn whole(&self, id: &str) -> Value {
        let mut view = usage_full_with_backends(&self.backends(), Selection::Id(id)).unwrap();
        pricing::price_view(&mut view, &PriceTable::parse(TABLE).unwrap());
        serde_json::to_value(view).unwrap()
    }

    fn spans(&self, spans: &[SpanBounds]) -> Value {
        serde_json::to_value(
            usage_spans_with_backends(&self.backends(), Selection::Id(LEAD), spans).unwrap(),
        )
        .unwrap()
    }

    fn family(&self) -> Value {
        let view = usage_with_options_with_backends(
            &self.backends(),
            Selection::Id(LEAD),
            UsageOptions {
                full: true,
                series: None,
                reconcile: true,
                event_keys: None,
            },
        )
        .unwrap();
        serde_json::to_value(view).unwrap()
    }

    fn window(&self, from: &str, to: Option<&str>) -> Value {
        let summary = usage_window_with_backends(
            &self.backends(),
            &SessionSelection {
                limit: Some(50),
                ..Default::default()
            },
            &[GroupBy::Session],
            WindowAsk::Bounds(WindowBounds {
                from: Some(from.parse().unwrap()),
                to: to.map(|to| to.parse().unwrap()),
            }),
            None,
        )
        .unwrap();
        serde_json::to_value(summary).unwrap()
    }
}

fn prompt(session: &str, uuid: &str, at: &str) -> Value {
    json!({"type": "user", "sessionId": session, "uuid": uuid, "parentUuid": null,
        "timestamp": at, "cwd": "/fixtures/project", "entrypoint": "cli",
        "message": {"role": "user", "content": "Carry on with the series."}})
}

fn turn_duration(session: &str, at: &str) -> Value {
    json!({"type": "system", "subtype": "turn_duration", "sessionId": session,
        "uuid": format!("duration-{at}"), "timestamp": at, "cwd": "/fixtures/project",
        "durationMs": 1000})
}

/// An Opus request as Claude Code records an API response: its cache writes
/// split by TTL under `cache_creation`.
fn opus(session: &str, request: &str, at: &str, counters: [u64; 6]) -> Value {
    let [input, read, write_5m, write_1h, output, thinking] = counters;
    json!({"type": "assistant", "sessionId": session, "uuid": format!("assistant-{request}"),
        "parentUuid": null, "requestId": request, "timestamp": at, "cwd": "/fixtures/project",
        "message": {"id": format!("msg-{request}"), "role": "assistant",
            "model": "claude-opus-5-5", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "Done."}],
            "usage": {"input_tokens": input, "cache_read_input_tokens": read,
                "cache_creation_input_tokens": write_5m + write_1h, "output_tokens": output,
                "output_tokens_details": {"thinking_tokens": thinking},
                "cache_creation": {"ephemeral_5m_input_tokens": write_5m,
                    "ephemeral_1h_input_tokens": write_1h},
                "service_tier": "standard"}}})
}

/// The subagent's own Haiku request, which its parent's total counts.
fn haiku_request(at: &str) -> Value {
    json!({"type": "assistant", "sessionId": LEAD, "agentId": AGENT, "isSidechain": true,
        "uuid": "agent-assistant", "parentUuid": "agent-user", "requestId": "req_agent_1",
        "timestamp": at, "cwd": "/fixtures/project",
        "message": {"id": "msg-agent-1", "role": "assistant", "model": "claude-haiku-5-5",
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "Started."}],
            "usage": {"input_tokens": 6, "cache_read_input_tokens": 500,
                "cache_creation_input_tokens": 100, "output_tokens": 50,
                "cache_creation": {"ephemeral_5m_input_tokens": 100,
                    "ephemeral_1h_input_tokens": 0},
                "service_tier": "standard"}}})
}

/// A plugin-steered subagent's mirror row: it names the subagent's model
/// and carries no request id and only zero usage.
fn mirror_row(uuid: &str, at: &str) -> Value {
    json!({"type": "assistant", "sessionId": LEAD, "agentId": AGENT, "isSidechain": true,
        "uuid": uuid, "parentUuid": "agent-user", "timestamp": at, "cwd": "/fixtures/project",
        "message": {"id": format!("msg-{uuid}"), "role": "assistant",
            "model": "claude-haiku-5-5", "stop_reason": null,
            "content": [{"type": "text", "text": "Working."}],
            "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "output_tokens_details": null,
                "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                "service_tier": null,
                "cache_creation": {"ephemeral_1h_input_tokens": 0,
                    "ephemeral_5m_input_tokens": 0},
                "inference_geo": null, "iterations": null, "speed": null}}})
}

/// A cumulative cost-state over Opus and then Haiku, each as input, output,
/// thinking, cache read and cache write; it writes no timestamp.
fn cost_state(session: &str, models: [[u64; 5]; 2], duration: u64) -> Value {
    let usage = |[input, output, thinking, read, write]: [u64; 5]| {
        json!({"inputTokens": input, "outputTokens": output, "thinkingTokens": thinking,
            "cacheReadInputTokens": read, "cacheCreationInputTokens": write,
            "webSearchRequests": 0, "costUSD": 0.25})
    };
    json!({"type": "cost-state", "sessionId": session, "totalCostUSD": 0.5,
        "totalAPIDuration": 4000, "totalAPIDurationWithoutRetries": 3900,
        "totalToolDuration": 2000, "totalLinesAdded": 0, "totalLinesRemoved": 0,
        "totalDuration": duration, "startTime": START_TIME, "hasUnknownModelCost": false,
        "modelUsage": {"claude-opus-5-5": usage(models[0]), "claude-haiku-5-5": usage(models[1])}})
}

/// A model's entry in an `unexplained` list.
fn entry<'a>(list: &'a Value, model: &str) -> Option<&'a Value> {
    list.as_array()?
        .iter()
        .find(|entry| entry["model"] == model)
}

/// A model's unexplained tokens as input, output, cache read and cache
/// write, or none where the list does not name it.
fn counted(list: &Value, model: &str) -> Option<[u64; 4]> {
    let tokens = &entry(list, model)?["tokens"];
    Some(
        ["input", "output", "cache_read", "cache_write"].map(|name| tokens[name].as_u64().unwrap()),
    )
}

fn time_span(from: Option<&str>, to: Option<&str>) -> SpanBounds {
    SpanBounds::Time {
        from: from.map(|from| from.parse().unwrap()),
        to: to.map(|to| to.parse().unwrap()),
    }
}

fn close(value: &Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|value| (value - expected).abs() < 1e-12)
}

/// Claim: the whole-session usage view names, beside `by_model`, each
/// model's part of the recorded total the session's own requests do not
/// explain — all 36/650/9500/3100 Haiku tokens, which only the subagent's
/// mirror rows and its own request stand beside, and the 100 Opus cache
/// writes no request recorded — while the session's tokens stay the recorded
/// total. Rejects a view with no unexplained part, one that counts the
/// zero-usage mirror rows as explaining it, and one that folds it into the
/// figure's tokens.
#[test]
fn the_whole_view_names_each_models_unexplained_part_beside_by_model() {
    let store = Store::lead(true);
    let view = store.whole(LEAD);
    assert_eq!(view["accounting"]["basis"], "recorded-total", "{view:#}");
    assert_eq!(view["tokens"]["cache_write"], 4100, "{view:#}");
    let unexplained = &view["unexplained"];
    assert_eq!(unexplained["explained_by"], "session-requests", "{view:#}");
    assert_eq!(
        counted(&unexplained["by_model"], "claude-haiku-5-5"),
        Some([36, 650, 9500, 3100]),
        "{unexplained:#}"
    );
    assert_eq!(
        counted(&unexplained["by_model"], "claude-opus-5-5"),
        Some([0, 0, 0, 100]),
        "{unexplained:#}"
    );
    assert!(unexplained.get("placement").is_none(), "{unexplained:#}");
}

/// Claim: the cache writes a recorded total's requests explain are priced
/// at their recorded TTLs and only what they leave is a range. The lead's
/// Opus share records 1000 cache writes, of which its requests wrote 900 for
/// an hour: `disjoint` splits them 900 one-hour and 100 without a TTL, and
/// the share is priced as a range only $0.0003 wide, the 100 writes between
/// the five-minute and the one-hour price. Rejects the whole 1000 written
/// `ttl_unrecorded` and priced as a $0.003 range.
#[test]
fn the_requests_ttls_price_the_writes_they_explain() {
    let store = Store::lead(false);
    let view = store.whole(LEAD);
    assert_eq!(
        view["disjoint"]["cache_write"],
        json!({"ephemeral_5m": 0, "ephemeral_1h": 900, "ttl_unrecorded": 3200}),
        "{view:#}"
    );
    let opus = entry(&view["by_model"], "claude-opus-5-5").unwrap();
    let estimate = &opus["list_price_estimate"];
    // 5 x 4 + 3000 x 0.2 + 900 x 8 + 300 x 20 per million, and the 100
    // unexplained writes at 5 to 8.
    assert!(close(&estimate["at_least"], 0.01432), "{estimate:#}");
    assert!(close(&estimate["at_most"], 0.01462), "{estimate:#}");
}

/// Claim: a headless session whose total holds 1460 more one-hour cache
/// writes than its requests recorded — 419,363 written, 420,823 recorded —
/// prices the requests' writes exactly and only the 1460 as a range,
/// $0.00438 wide. Rejects all 420,823 writes priced as a $1.26 range.
#[test]
fn a_small_shortfall_leaves_a_range_of_cents() {
    let store = Store::new();
    let usage = json!({"inputTokens": 40, "outputTokens": 9000, "thinkingTokens": 900,
        "cacheReadInputTokens": 2_000_000, "cacheCreationInputTokens": 420_823,
        "webSearchRequests": 0, "costUSD": 4.5});
    store.write(
        store.project().join(format!("{HEADLESS}.jsonl")),
        &[
            json!({"type": "queue-operation", "operation": "enqueue", "sessionId": HEADLESS,
                "timestamp": "2026-10-20T08:00:00.000Z"}),
            prompt(HEADLESS, "headless-user", "2026-10-20T08:00:01.000Z"),
            opus(
                HEADLESS,
                "req_headless_1",
                "2026-10-20T08:01:00.000Z",
                [20, 1_000_000, 0, 300_000, 5000, 500],
            ),
            opus(
                HEADLESS,
                "req_headless_2",
                "2026-10-20T08:20:00.000Z",
                [20, 1_000_000, 0, 119_363, 4000, 400],
            ),
            json!({"type": "cost-state", "sessionId": HEADLESS, "totalCostUSD": 4.5,
                "totalAPIDuration": 90_000, "totalAPIDurationWithoutRetries": 90_000,
                "totalToolDuration": 30_000, "totalLinesAdded": 12, "totalLinesRemoved": 3,
                "totalDuration": 1_200_000, "startTime": START_TIME,
                "hasUnknownModelCost": false, "modelUsage": {"claude-opus-5-5": usage}}),
        ],
    );
    let view = store.whole(HEADLESS);
    assert_eq!(
        view["disjoint"]["cache_write"],
        json!({"ephemeral_5m": 0, "ephemeral_1h": 419_363, "ttl_unrecorded": 1460}),
        "{view:#}"
    );
    let estimate = &view["list_price_estimate"];
    let spread = estimate["at_most"].as_f64().unwrap() - estimate["at_least"].as_f64().unwrap();
    assert!((spread - 0.00438).abs() < 1e-9, "{estimate:#}");
}

/// Claim: each unexplained model share is priced by its own row with its
/// cache writes at no known TTL: the lead's 3100 unexplained Haiku tokens of
/// cache write between $0.003875 and $0.0062, beside 36 input, 650 output
/// and 9500 cache read. Rejects an unpriced unexplained part, and one priced
/// by the session's Opus row.
#[test]
fn the_unexplained_part_is_priced_by_its_model_as_a_range() {
    let store = Store::lead(false);
    let view = store.whole(LEAD);
    let haiku = entry(&view["unexplained"]["by_model"], "claude-haiku-5-5").unwrap();
    let estimate = &haiku["list_price_estimate"];
    // 36 x 1 + 9500 x 0.1 + 650 x 5 per million, and 3100 writes at 1.25
    // to 2.
    assert!(close(&estimate["at_least"], 0.008111), "{estimate:#}");
    assert!(close(&estimate["at_most"], 0.010436), "{estimate:#}");
    assert_eq!(
        estimate["models"][0]["id"], "claude-haiku-5-5",
        "{estimate:#}"
    );
}

/// Claim: a span holds each total's increase by the interval the records
/// around it bound. The first total's 10/200/3000/1000 Haiku lies wholly in
/// a span to 10:10, which holds it `bounded`; a span from 10:10 to 10:45
/// holds half of the second interval (10:10 to 11:20), prorated and
/// `estimated` with its basis; and the remainder holds the rest, so the
/// spans and the remainder add up to the whole. Rejects a span with no
/// unexplained part, one that takes the whole session's part, and an
/// estimate that does not say so.
#[test]
fn spans_place_each_totals_increase_in_its_interval() {
    let store = Store::lead(true);
    let answer = store.spans(&[
        time_span(Some("2026-10-20T09:00:00Z"), Some("2026-10-20T10:10:00Z")),
        time_span(Some("2026-10-20T10:10:00Z"), Some("2026-10-20T10:45:00Z")),
    ]);
    let first = &answer["spans"][0]["unexplained"];
    assert_eq!(first["placement"]["status"], "bounded", "{first:#}");
    assert_eq!(
        counted(&first["by_model"], "claude-haiku-5-5"),
        Some([10, 200, 3000, 1000]),
        "{first:#}"
    );
    assert!(
        entry(&first["by_model"], "claude-opus-5-5").is_none(),
        "{first:#}"
    );
    let second = &answer["spans"][1]["unexplained"];
    assert_eq!(second["placement"]["status"], "estimated", "{second:#}");
    assert_eq!(
        second["placement"]["basis"], "cost-state-interval-prorated-by-time",
        "{second:#}"
    );
    assert_eq!(
        counted(&second["by_model"], "claude-haiku-5-5"),
        Some([13, 225, 3250, 1050]),
        "{second:#}"
    );
    assert_eq!(
        counted(&second["by_model"], "claude-opus-5-5"),
        Some([0, 0, 0, 50]),
        "{second:#}"
    );
    let remainder = &answer["remainder"]["unexplained"];
    assert_eq!(
        counted(&remainder["by_model"], "claude-haiku-5-5"),
        Some([13, 225, 3250, 1050]),
        "{remainder:#}"
    );
    let whole = &answer["whole"]["unexplained"];
    assert_eq!(
        counted(&whole["by_model"], "claude-haiku-5-5"),
        Some([36, 650, 9500, 3100]),
        "{whole:#}"
    );
    // The span's own figure sums its request alone, the 10:40 request's 500
    // cache writes.
    assert_eq!(
        answer["spans"][1]["tokens"]["cache_write"], 500,
        "{answer:#}"
    );
}

/// Claim: an increase no timestamped record bounds is placed in no span: a
/// total written before the recording's first timestamp leaves its 100 Haiku
/// output unplaced, named under the span's `unplaced` with reason
/// `unbounded` and held by the remainder, while the next total's increase is
/// placed. Rejects a span that takes the unbounded increase, and a split that
/// drops it.
#[test]
fn an_unbounded_increase_stays_with_the_remainder() {
    let store = Store::new();
    store.write(
        store.project().join(format!("{LEAD}.jsonl")),
        &[
            json!({"type": "mode", "mode": "default", "sessionId": LEAD}),
            cost_state(LEAD, [[0, 0, 0, 0, 0], [0, 100, 0, 0, 0]], 1000),
            prompt(LEAD, "lead-user-1", "2026-10-20T10:00:00.000Z"),
            opus(
                LEAD,
                "req_lead_1",
                "2026-10-20T10:01:00.000Z",
                [2, 1000, 0, 400, 100, 10],
            ),
            turn_duration(LEAD, "2026-10-20T10:10:00.000Z"),
            cost_state(LEAD, [[2, 100, 10, 1000, 400], [0, 300, 0, 0, 0]], 600_000),
        ],
    );
    let answer = store.spans(&[time_span(Some("2026-10-20T09:00:00Z"), None)]);
    let span = &answer["spans"][0]["unexplained"];
    assert_eq!(
        counted(&span["by_model"], "claude-haiku-5-5"),
        Some([0, 200, 0, 0]),
        "{span:#}"
    );
    assert_eq!(
        counted(&span["unplaced"], "claude-haiku-5-5"),
        Some([0, 100, 0, 0]),
        "{span:#}"
    );
    assert_eq!(span["unplaced_reasons"], json!(["unbounded"]), "{span:#}");
    let remainder = &answer["remainder"]["unexplained"];
    assert_eq!(
        counted(&remainder["by_model"], "claude-haiku-5-5"),
        Some([0, 100, 0, 0]),
        "{remainder:#}"
    );
}

/// Claim: a family read measures the lead's total against its subagent's
/// requests too: `--reconcile` names 30/600/9000/3000 Haiku tokens, the
/// subagent's 6/50/500/100 request explained, under `family-requests`.
/// Rejects the session-only figure, which counts the subagent's request as
/// unexplained.
#[test]
fn a_family_read_explains_the_subagents_requests() {
    let store = Store::lead(true);
    let view = store.family();
    let unexplained = &view["unexplained"];
    assert_eq!(
        unexplained["explained_by"], "family-requests",
        "{unexplained:#}"
    );
    assert_eq!(
        counted(&unexplained["by_model"], "claude-haiku-5-5"),
        Some([30, 600, 9000, 3000]),
        "{unexplained:#}"
    );
}

/// Claim: a window holds the subagent's request as a share of its own, so
/// the lead's share places only what the family leaves unexplained inside
/// the window: from 10:10 on, 20/400/6000/2000 Haiku and the 100 Opus
/// writes, whole, since the window holds the second interval whole; and the
/// window's total names the same part. Rejects a lead share that counts the
/// subagent's request again, and a window with no unexplained part.
#[test]
fn a_window_places_the_familys_unexplained_part_once() {
    let store = Store::lead(true);
    let summary = store.window("2026-10-20T10:10:00Z", None);
    let lead = summary["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"] == LEAD)
        .unwrap_or_else(|| panic!("no lead share: {summary:#}"));
    let unexplained = &lead["unexplained"];
    assert_eq!(
        unexplained["explained_by"], "family-requests",
        "{unexplained:#}"
    );
    assert_eq!(
        unexplained["placement"]["status"], "bounded",
        "{unexplained:#}"
    );
    assert_eq!(
        counted(&unexplained["by_model"], "claude-haiku-5-5"),
        Some([20, 400, 6000, 2000]),
        "{unexplained:#}"
    );
    assert_eq!(
        counted(&unexplained["by_model"], "claude-opus-5-5"),
        Some([0, 0, 0, 100]),
        "{unexplained:#}"
    );
    assert_eq!(
        counted(
            &summary["totals"]["unexplained"]["by_model"],
            "claude-haiku-5-5"
        ),
        Some([20, 400, 6000, 2000]),
        "{summary:#}"
    );
}
