//! A window over a selection sums each session's span inside it, groups the
//! shares by session, launch and descent, and takes quota windows from the
//! recordings' own quota observations.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, codex::CodexBackend, Backend};
use agent_tapes_core::pricing::PriceTable;
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::window::{
    resolve_quota_window, QuotaAt, QuotaKind, QuotaLength, QuotaObservation, WindowBoundary,
    WindowBounds,
};
use agent_tapes_core::{
    usage_full_with_backends, usage_spans_with_backends, usage_window_with_backends, Selection,
    SessionSelection, WindowAsk,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-window-{label}-{}-{nonce}",
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

fn at(stamp: &str) -> DateTime<Utc> {
    stamp.parse().unwrap()
}

fn window(from: Option<&str>, to: Option<&str>) -> WindowBounds {
    WindowBounds {
        from: from.map(at),
        to: to.map(at),
    }
}

fn selection() -> SessionSelection<'static> {
    SessionSelection {
        limit: Some(50),
        ..Default::default()
    }
}

/// A Claude session of one request per minute stamp, each with its own
/// counters.
fn claude_session(id: &str, minutes: &[(&str, u64)]) -> Vec<Value> {
    let mut lines = vec![json!({
        "type": "user", "sessionId": id, "uuid": format!("{id}-user"), "entrypoint": "cli",
        "timestamp": format!("2026-01-10T{}:00Z", minutes[0].0), "cwd": "/fixtures/project",
        "message": {"role": "user", "content": "Window fixture."}
    })];
    for (index, (minute, output)) in minutes.iter().enumerate() {
        lines.push(json!({
            "type": "assistant", "sessionId": id, "uuid": format!("{id}-a{index}"), "entrypoint": "cli",
            "requestId": format!("{id}-r{index}"), "timestamp": format!("2026-01-10T{minute}:30Z"),
            "cwd": "/fixtures/project",
            "message": {"role": "assistant", "model": "claude-fixture", "content": [{"type": "text", "text": "ok"}],
                        "usage": {"input_tokens": 1, "output_tokens": output, "cache_read_input_tokens": 10 * output,
                                  "cache_creation_input_tokens": 2}}
        }));
    }
    lines
}

/// One session inside the window, one crossing each edge.
fn straddling_store() -> Store {
    let store = Store::new("straddle");
    let sessions = [
        (
            "30000000-0000-4000-8000-0000000000c1",
            vec![("10:30", 5), ("10:35", 7)],
        ),
        (
            "30000000-0000-4000-8000-0000000000c2",
            vec![("09:50", 11), ("10:05", 13), ("10:10", 17)],
        ),
        (
            "30000000-0000-4000-8000-0000000000c3",
            vec![("10:50", 19), ("11:05", 23)],
        ),
    ];
    for (id, minutes) in sessions {
        store.write(
            &format!("projects/-fixtures-project/{id}.jsonl"),
            &claude_session(id, &minutes),
        );
    }
    store
}

fn claude(store: &Store) -> Vec<Box<dyn Backend>> {
    vec![Box::new(ClaudeBackend::new(store.0.join("projects")))]
}

fn counters(value: &Value) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for counter in ["input", "output", "cache_read", "cache_write"] {
        if let Some(amount) = value["tokens"][counter].as_u64() {
            out.insert(counter.to_owned(), amount);
        }
    }
    out
}

fn add(into: &mut BTreeMap<String, u64>, from: &BTreeMap<String, u64>) {
    for (counter, amount) in from {
        *into.entry(counter.clone()).or_default() += amount;
    }
}

/// Claim: a window's total equals the sum of each session's span inside it,
/// per counter: a session inside contributes whole, and a session crossing
/// an edge contributes only its requests inside. Rejects a window that adds
/// whole sessions, or drops a session that crosses an edge.
#[test]
fn a_window_sums_the_spans_inside_it() {
    let store = straddling_store();
    let bounds = window(Some("2026-01-10T10:00:00Z"), Some("2026-01-10T11:00:00Z"));
    let summary = usage_window_with_backends(
        &claude(&store),
        &selection(),
        &[GroupBy::Session],
        WindowAsk::Bounds(bounds),
        None,
    )
    .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    assert_eq!(summary["schema"], "tapes-usage-summary/8");
    assert_eq!(summary["window"]["boundary"], "requested");
    let mut expected = BTreeMap::new();
    for id in [
        "30000000-0000-4000-8000-0000000000c1",
        "30000000-0000-4000-8000-0000000000c2",
        "30000000-0000-4000-8000-0000000000c3",
    ] {
        let span = usage_spans_with_backends(&claude(&store), Selection::Id(id), &[bounds.span()])
            .unwrap();
        add(
            &mut expected,
            &counters(&serde_json::to_value(&span.spans[0]).unwrap()),
        );
    }
    assert_eq!(counters(&summary["totals"]), expected);
    // Inside: 5 + 7; crossing the start: 13 + 17; crossing the end: 19.
    assert_eq!(expected["output"], 61);
    let requests = summary["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|share| {
            (
                share["id"].as_str().unwrap().to_owned(),
                share["requests"].as_u64().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(requests.values().sum::<u64>(), 5);
    assert_eq!(
        summary["window"]["sessions"][0]["id"],
        "30000000-0000-4000-8000-0000000000c2"
    );
    let by_launch = usage_window_with_backends(
        &claude(&store),
        &selection(),
        &[GroupBy::Launch],
        WindowAsk::Bounds(bounds),
        None,
    )
    .unwrap();
    let by_launch = serde_json::to_value(by_launch).unwrap();
    assert_eq!(by_launch["groups"].as_array().unwrap().len(), 1);
    assert_eq!(by_launch["groups"][0]["key"]["launch"], "interactive");
    assert_eq!(counters(&by_launch["groups"][0]), expected);
}

/// Claim: adjacent windows that split the timeline give each session shares
/// that add up to its whole figure, per counter. Rejects a window edge that
/// counts a request on both sides or neither.
#[test]
fn adjacent_windows_conserve_each_session() {
    let store = straddling_store();
    let windows = [
        window(None, Some("2026-01-10T10:00:00Z")),
        window(Some("2026-01-10T10:00:00Z"), Some("2026-01-10T11:00:00Z")),
        window(Some("2026-01-10T11:00:00Z"), None),
    ];
    let mut shares: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    for bounds in windows {
        let summary = usage_window_with_backends(
            &claude(&store),
            &selection(),
            &[GroupBy::Session],
            WindowAsk::Bounds(bounds),
            None,
        )
        .unwrap();
        for group in serde_json::to_value(&summary).unwrap()["groups"]
            .as_array()
            .unwrap()
        {
            let id = group["key"]["session"].as_str().unwrap().to_owned();
            add(shares.entry(id).or_default(), &counters(group));
        }
    }
    assert_eq!(shares.len(), 3);
    for (id, summed) in shares {
        let whole = usage_full_with_backends(&claude(&store), Selection::Id(&id)).unwrap();
        assert_eq!(
            summed,
            counters(&serde_json::to_value(whole).unwrap()),
            "{id}"
        );
    }
}

fn codex_session(id: &str, header: Value, stamp: &str, output: u64) -> Vec<Value> {
    let mut meta =
        json!({"id": id, "timestamp": stamp, "cwd": "/fixtures/project", "source": "exec"});
    for (key, value) in header.as_object().unwrap() {
        meta[key] = value.clone();
    }
    let total = json!({"input_tokens": 100, "cached_input_tokens": 0, "cache_write_input_tokens": 0,
                       "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": 100 + output});
    vec![
        json!({"timestamp": stamp, "type": "session_meta", "payload": meta}),
        json!({"timestamp": stamp, "type": "turn_context", "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
        json!({"timestamp": stamp, "type": "response_item",
               "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Parent fixture."}]}}),
        json!({"timestamp": stamp, "type": "event_msg",
               "payload": {"type": "token_count", "info": {"total_token_usage": total, "last_token_usage": total, "model_context_window": 272000},
                           "rate_limits": {"limit_id": "codex", "primary": {"used_percent": 12.0, "window_minutes": 300, "resets_at": 1768050000}, "secondary": null}}}),
    ]
}

/// Codex rollouts: a parent, a child it spawned, a fork of it, an unrelated
/// session, and a child whose parent the store does not hold.
fn family_store() -> Store {
    let store = Store::new("family");
    let parent = "40000000-0000-7000-8000-0000000000d1";
    let rollouts = [
        (parent, json!({}), 10),
        (
            "40000000-0000-7000-8000-0000000000d2",
            json!({"parent_thread_id": parent}),
            20,
        ),
        (
            "40000000-0000-7000-8000-0000000000d3",
            json!({"forked_from_id": parent}),
            30,
        ),
        ("40000000-0000-7000-8000-0000000000d4", json!({}), 40),
        (
            "40000000-0000-7000-8000-0000000000d5",
            json!({"parent_thread_id": "40000000-0000-7000-8000-0000000000ff"}),
            50,
        ),
    ];
    for (minute, (id, header, output)) in rollouts.into_iter().enumerate() {
        let stamp = format!("2026-01-10T10:0{minute}:00Z");
        store.write(
            &format!("2026/01/10/rollout-2026-01-10T10-0{minute}-00-{id}.jsonl"),
            &codex_session(id, header, &stamp, output),
        );
    }
    store
}

fn codex(store: &Store) -> Vec<Box<dyn Backend>> {
    vec![Box::new(CodexBackend::new(&store.0))]
}

/// Claim: `--by parent` groups a parent with the child it spawned and its
/// fork, keeps an unrelated session apart, and keeps a child whose parent
/// the store does not hold under that parent's id, marked unresolved, never
/// joined to a guessed one. Rejects grouping by proximity, or dropping the
/// unresolved parent.
#[test]
fn parent_grouping_follows_named_parents_only() {
    let store = family_store();
    let summary = usage_window_with_backends(
        &codex(&store),
        &selection(),
        &[GroupBy::Parent],
        WindowAsk::Bounds(window(Some("2026-01-10T09:00:00Z"), None)),
        None,
    )
    .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    let groups = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["key"]["parent"].as_str().unwrap().to_owned(),
                group["sessions"].as_u64().unwrap(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        groups,
        BTreeMap::from([
            ("40000000-0000-7000-8000-0000000000d1".to_owned(), 3),
            ("40000000-0000-7000-8000-0000000000d4".to_owned(), 1),
            ("40000000-0000-7000-8000-0000000000ff".to_owned(), 1),
        ])
    );
    let orphan = summary["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"] == "40000000-0000-7000-8000-0000000000d5")
        .unwrap();
    assert_eq!(orphan["root_resolved"], false);
    assert_eq!(orphan["parent"], "40000000-0000-7000-8000-0000000000ff");
}

/// Claim: a Codex quota window is the recorded `rate_limits` window, its
/// reset and length as observed, and the answer names the observation; an
/// instant past that window and the one after it refuses. Rejects a window
/// guessed from session activity.
#[test]
fn a_codex_quota_window_is_the_recorded_one() {
    let store = family_store();
    let selection = SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        ..Default::default()
    };
    let summary = usage_window_with_backends(
        &codex(&store),
        &selection,
        &[GroupBy::Session],
        WindowAsk::Quota {
            quota: QuotaKind::Primary,
            at: QuotaAt::At(at("2026-01-10T11:00:00Z")),
        },
        None,
    )
    .unwrap();
    let window = serde_json::to_value(summary).unwrap()["window"].clone();
    // resets_at 1768050000 is 2026-01-10T13:00:00Z; 300 minutes before it.
    assert_eq!(window["from"], "2026-01-10T08:00:00Z");
    assert_eq!(window["to"], "2026-01-10T13:00:00Z");
    assert_eq!(window["boundary"], "recorded");
    assert_eq!(window["quota"]["observation"]["length"]["kind"], "recorded");
    assert_eq!(window["sessions"].as_array().unwrap().len(), 5);
    let refused = usage_window_with_backends(
        &codex(&store),
        &selection,
        &[GroupBy::Session],
        WindowAsk::Quota {
            quota: QuotaKind::Primary,
            at: QuotaAt::At(at("2026-01-11T12:00:00Z")),
        },
        None,
    )
    .err()
    .unwrap();
    assert!(
        format!("{refused:#}").contains("read the window with --window"),
        "{refused:#}"
    );
}

/// Claim: `current` reads the newest observation, recorded while its window
/// runs and inferred, as the window after it, once it has reset; an older
/// observation of another limit never answers for the current window.
/// Rejects choosing the observation that resets last over the newest one.
#[test]
fn the_current_quota_window_comes_from_the_newest_observation() {
    let observation = |observed: &str, resets: &str, minutes: u64| QuotaObservation {
        quota: QuotaKind::Primary,
        resets_at: at(resets),
        window_minutes: minutes,
        length: QuotaLength::Recorded,
        observed_at: Some(at(observed)),
    };
    let observations = vec![
        (
            "codex".to_owned(),
            "older".to_owned(),
            observation("2026-01-09T00:00:00Z", "2026-02-01T00:00:00Z", 43_200),
        ),
        (
            "codex".to_owned(),
            "newest".to_owned(),
            observation("2026-01-10T10:00:00Z", "2026-01-10T13:00:00Z", 300),
        ),
    ];
    let (bounds, boundary, source) = resolve_quota_window(
        &observations,
        QuotaKind::Primary,
        QuotaAt::Current,
        at("2026-01-10T12:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        (bounds.from, bounds.to),
        (
            Some(at("2026-01-10T08:00:00Z")),
            Some(at("2026-01-10T13:00:00Z"))
        )
    );
    assert_eq!(
        (boundary, source.session.as_str()),
        (WindowBoundary::Recorded, "newest")
    );
    let (bounds, boundary, _) = resolve_quota_window(
        &observations,
        QuotaKind::Primary,
        QuotaAt::Current,
        at("2026-01-10T14:00:00Z"),
    )
    .unwrap();
    assert_eq!(
        (bounds.from, bounds.to),
        (
            Some(at("2026-01-10T13:00:00Z")),
            Some(at("2026-01-10T18:00:00Z"))
        )
    );
    assert_eq!(boundary, WindowBoundary::Inferred);
    assert!(resolve_quota_window(
        &observations,
        QuotaKind::Primary,
        QuotaAt::Current,
        at("2026-01-11T00:00:00Z")
    )
    .is_err());
}

/// Claim: a Claude recording records a limit only on a refused request,
/// naming the limit and its reset but not its length, so its quota window
/// is inferred from the limit's name; a store whose recordings hold no
/// quota observation refuses and points at `--window`. Rejects a Claude
/// window reported as recorded, or one invented without an observation.
#[test]
fn a_claude_quota_window_is_inferred_from_the_limit_name() {
    let store = straddling_store();
    let id = "30000000-0000-4000-8000-0000000000c4";
    let mut lines = claude_session(id, &[("10:20", 3)]);
    lines.push(json!({
        "type": "assistant", "sessionId": id, "uuid": format!("{id}-limited"), "entrypoint": "cli",
        "timestamp": "2026-01-10T10:21:00Z", "cwd": "/fixtures/project", "isApiErrorMessage": true,
        "error": "rate_limit", "apiErrorStatus": 429,
        "quotaLimits": {"status": "rejected", "resetsAt": 1768046400, "rateLimitType": "five_hour"},
        "message": {"role": "assistant", "model": "<synthetic>", "content": [{"type": "text", "text": "limit reached"}]}
    }));
    let selection = SessionSelection {
        harness: Some("claude"),
        limit: Some(50),
        ..Default::default()
    };
    let quota = WindowAsk::Quota {
        quota: QuotaKind::Primary,
        at: QuotaAt::At(at("2026-01-10T10:30:00Z")),
    };
    let refused = usage_window_with_backends(&claude(&store), &selection, &[], quota, None)
        .err()
        .unwrap();
    assert!(
        format!("{refused:#}").contains("holds a primary quota observation"),
        "{refused:#}"
    );
    store.write(&format!("projects/-fixtures-project/{id}.jsonl"), &lines);
    let summary =
        usage_window_with_backends(&claude(&store), &selection, &[], quota, None).unwrap();
    let window = serde_json::to_value(summary).unwrap()["window"].clone();
    // resetsAt 1768046400 is 2026-01-10T12:00:00Z; five hours before it.
    assert_eq!(window["from"], "2026-01-10T07:00:00Z");
    assert_eq!(window["boundary"], "inferred");
    assert_eq!(
        window["quota"]["observation"]["length"],
        json!({"kind": "limit-name", "name": "five_hour"})
    );
}

/// Claim: `--prices` prices a window read as any summary: the window's
/// estimate is the sum of its sessions' span estimates. Rejects pricing
/// whole sessions instead of their spans.
#[test]
fn prices_compose_with_windows() {
    let store = straddling_store();
    let table = PriceTable::parse(
        "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n[[models]]\nid = \"claude-fixture\"\neffective = 2026-01-01\nsource = \"fixture\"\nper_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.0, cache_write_1h = 1.0, output = 10.0 }\n",
    )
    .unwrap();
    let bounds = window(Some("2026-01-10T10:00:00Z"), Some("2026-01-10T11:00:00Z"));
    let summary = usage_window_with_backends(
        &claude(&store),
        &selection(),
        &[GroupBy::Session],
        WindowAsk::Bounds(bounds),
        Some(&table),
    )
    .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    // Five requests inside: 5 input, 61 output, 610 cache reads, 10 writes.
    let expected = (5.0 * 1.0 + 610.0 * 0.1 + 10.0 * 1.0 + 61.0 * 10.0) / 1e6;
    let low = summary["totals"]["list_price_estimate"]["at_least"]
        .as_f64()
        .or(summary["totals"]["list_price_estimate"]["amount"].as_f64())
        .unwrap();
    assert!(
        (low - expected).abs() < 1e-12,
        "{}",
        summary["totals"]["list_price_estimate"]
    );
}
