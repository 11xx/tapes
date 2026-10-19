//! Every incompleteness reason a measurement records reaches every figure
//! derived from it, and no figure that does not depend on it. The table
//! crosses each reason with each derived figure: the usage view and its
//! disjoint categories, the model split, the retained-branch consumption,
//! summary totals, groups and partitions, per-session figures and identity
//! bounds, stats, brief, and the session wire.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{pi::PiBackend, Backend};
use agent_tapes_core::model::{
    Accounting, AccountingBasis, AccountingCoverage, Cost, Session, Tokens, Transcript,
};
use agent_tapes_core::usage::{self, GroupBy, ModelUsage, RecordedUsage};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const ID: &str = "completeness-fixture";
const NATIVE: [&str; 6] = [
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];
const TOKENS: [&str; 5] = ["input", "output", "reasoning", "cache_read", "cache_write"];
/// Pi counts uncached input apart from cache and reasoning inside output, so
/// each category follows the native counter of the same name.
const CATEGORIES: [(&str, &str); 5] = [
    ("uncached_input", "input"),
    ("cache_read", "cache_read"),
    ("cache_write", "cache_write"),
    ("output", "output"),
    ("reasoning", "reasoning"),
];
const SUMMED: [&str; 4] = ["input", "cache_read", "cache_write", "output"];
const RATIO: [&str; 3] = ["input", "cache_read", "cache_write"];

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-completeness-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, lines: &[String]) {
        fs::write(
            self.0.join(format!("2026-01-16T10-00-00-000Z_{ID}.jsonl")),
            lines.join("\n") + "\n",
        )
        .unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// How a case departs from a completely measured single request.
#[derive(Clone, Copy)]
enum Shape {
    /// The request omits one native usage field.
    Omit(&'static str),
    /// The request ends on this stop reason with every counter populated.
    Stop(&'static str),
    /// A record after the request cannot be decoded.
    Unreadable,
    /// The bounded read keeps a tail that leaves the request's parent behind.
    Window,
    /// The request writes no cost.
    NoPrice,
    /// The request writes a zero cost beside its tokens.
    ZeroPrice,
    /// The session's counters cover only what followed a restart.
    SinceReset,
    /// No accounting establishes what the counters cover.
    Unscoped,
    /// The reader recorded a failed request whose consumption is unknown.
    Failed,
}

struct Case {
    name: &'static str,
    shape: Shape,
    /// The reason category every affected counter carries.
    reason: &'static str,
    /// The native counters the reason affects.
    affected: &'static [&'static str],
    /// The reason the retained-branch consumption carries, where the case
    /// reaches it.
    consumption: Option<&'static str>,
    /// Whether the whole-recording read has the case.
    full: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "omitted input",
        shape: Shape::Omit("input"),
        reason: "missing-counter",
        affected: &["input"],
        consumption: Some("missing-counter"),
        full: true,
    },
    Case {
        name: "omitted output",
        shape: Shape::Omit("output"),
        reason: "missing-counter",
        affected: &["output"],
        consumption: Some("missing-counter"),
        full: true,
    },
    Case {
        name: "omitted reasoning",
        shape: Shape::Omit("reasoning"),
        reason: "missing-counter",
        affected: &["reasoning"],
        consumption: Some("missing-counter"),
        full: true,
    },
    Case {
        name: "omitted cache read",
        shape: Shape::Omit("cacheRead"),
        reason: "missing-counter",
        affected: &["cache_read"],
        consumption: Some("missing-counter"),
        full: true,
    },
    Case {
        name: "omitted cache write",
        shape: Shape::Omit("cacheWrite"),
        reason: "missing-counter",
        affected: &["cache_write"],
        consumption: Some("missing-counter"),
        full: true,
    },
    Case {
        name: "aborted",
        shape: Shape::Stop("aborted"),
        reason: "stopped-attempt",
        affected: &NATIVE,
        consumption: Some("stopped-attempt"),
        full: true,
    },
    Case {
        name: "error",
        shape: Shape::Stop("error"),
        reason: "stopped-attempt",
        affected: &NATIVE,
        consumption: Some("stopped-attempt"),
        full: true,
    },
    Case {
        name: "unreadable",
        shape: Shape::Unreadable,
        reason: "read-window",
        affected: &NATIVE,
        consumption: Some("unreadable-record"),
        full: true,
    },
    Case {
        name: "read window",
        shape: Shape::Window,
        reason: "read-window",
        affected: &NATIVE,
        consumption: Some("read-window"),
        full: false,
    },
    Case {
        name: "no price",
        shape: Shape::NoPrice,
        reason: "missing-price",
        affected: &["cost"],
        consumption: Some("missing-price"),
        full: true,
    },
    Case {
        name: "zero price",
        shape: Shape::ZeroPrice,
        reason: "unpriced",
        affected: &["cost"],
        consumption: Some("unpriced"),
        full: true,
    },
    Case {
        name: "since reset",
        shape: Shape::SinceReset,
        reason: "since-reset",
        affected: &NATIVE,
        consumption: None,
        full: true,
    },
    Case {
        name: "unscoped",
        shape: Shape::Unscoped,
        reason: "unestablished-scope",
        affected: &NATIVE,
        consumption: None,
        full: true,
    },
    Case {
        name: "failed request",
        shape: Shape::Failed,
        reason: "stopped-attempt",
        affected: &NATIVE,
        consumption: None,
        full: true,
    },
];

fn header(directory: &str) -> String {
    json!({"type":"session","version":3,"id":ID,"timestamp":"2026-01-16T10:00:00Z","cwd":directory})
        .to_string()
}

fn request(parent: Option<&str>) -> Value {
    json!({"type":"message","id":"request","parentId":parent,"timestamp":"2026-01-16T10:00:02Z","message":{"role":"assistant","model":"fixture-model","provider":"fixture-provider","responseId":"response","stopReason":"stop","content":[{"type":"text","text":"A response."}],"usage":{"input":10,"output":5,"reasoning":2,"cacheRead":3,"cacheWrite":4,"cost":{"total":0.5}}}})
}

/// The recording for `shape`, and the read bound its bounded read uses.
fn recording(shape: Shape, directory: &str) -> (Vec<String>, Option<u64>) {
    let mut request = request(None);
    let usage = request["message"]["usage"].as_object_mut().unwrap();
    match shape {
        Shape::Omit(field) => {
            usage.remove(field);
        }
        Shape::NoPrice => {
            usage.remove("cost");
        }
        Shape::ZeroPrice => usage["cost"]["total"] = json!(0),
        Shape::Stop(reason) => request["message"]["stopReason"] = json!(reason),
        _ => {}
    }
    let mut lines = vec![header(directory)];
    let mut bound = None;
    if let Shape::Window = shape {
        lines.push(
            json!({"type":"message","id":"prompt","parentId":null,"timestamp":"2026-01-16T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"x".repeat(8192)}]}})
                .to_string(),
        );
        request["parentId"] = json!("prompt");
        let request = request.to_string();
        bound = Some(request.len() as u64 + 64);
        lines.push(request);
    } else {
        lines.push(request.to_string());
    }
    if let Shape::Unreadable = shape {
        lines.push("{ not a record".to_owned());
    }
    (lines, bound)
}

/// Session facts no Pi recording writes, set as another reader would set
/// them. Whether the case set any.
fn apply(shape: Shape, session: &mut Session) -> bool {
    match shape {
        Shape::SinceReset => {
            session.accounting.as_mut().unwrap().coverage = AccountingCoverage::SinceReset;
        }
        Shape::Unscoped => session.accounting = None,
        Shape::Failed => session.usage_detail.as_mut().unwrap().request_failures = 1,
        _ => return false,
    }
    true
}

/// Declare `session` a fork of the session `origin` ids.
fn forked_from(session: &mut agent_tapes_core::model::Session, origin: &str) {
    session
        .usage_detail
        .get_or_insert_with(Default::default)
        .copy_evidence = Some(agent_tapes_core::overlap::CopyEvidence {
        origins: vec![origin.to_owned()],
        ..Default::default()
    });
}

fn read(store: &Store, bound: Option<u64>, full: bool) -> (Transcript, Value) {
    let backend = PiBackend::new(&store.0);
    let backend = match bound {
        Some(bound) => backend.with_read_bytes(bound),
        None => backend,
    };
    let session = backend.locate(ID).unwrap().unwrap();
    if full {
        let read = backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap();
        let whole = backend.stream_session(&session, &read).unwrap();
        let view =
            serde_json::to_value(usage::streamed(&whole, Default::default(), &read)).unwrap();
        let transcript = Transcript::new(whole, Vec::new(), Default::default(), None, Vec::new());
        (transcript, view)
    } else {
        let transcript = backend.transcript(&session, usize::MAX).unwrap();
        let view = serde_json::to_value(usage::usage(&transcript)).unwrap();
        (transcript, view)
    }
}

/// A column's figure exists. A missing figure has no reasons for any
/// counter, so every cell check over it would pass without examining it.
fn assert_present(label: &str, figure: &Value) {
    assert!(
        figure.as_object().is_some_and(|figure| !figure.is_empty()),
        "{label}: no figure"
    );
}

fn reasons<'a>(figure: &'a Value, counter: &str) -> Vec<&'a str> {
    figure["measurement_completeness"][counter]
        .as_array()
        .map(|reasons| reasons.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The figure's native counters carry the case's reason exactly where the
/// case affects them, and no others are qualified.
fn assert_native(label: &str, figure: &Value, reason: &str, affected: &[&str]) {
    assert_present(label, figure);
    for counter in NATIVE {
        let found = reasons(figure, counter);
        if affected.contains(&counter) {
            assert!(
                found.contains(&reason),
                "{label} × {counter}: expected {reason} in {figure}"
            );
        } else {
            assert!(found.is_empty(), "{label} × {counter}: {figure}");
        }
    }
}

/// The figure's disjoint categories follow their native counters, and the
/// total stands only when every summed category is complete.
fn assert_disjoint(label: &str, disjoint: &Value, reason: &str, affected: &[&str]) {
    assert_present(label, disjoint);
    for (category, native) in CATEGORIES {
        let found = reasons(disjoint, category);
        let missing = disjoint["missing"].as_array().map_or_else(
            || disjoint["missing"].get(category).is_some(),
            |names| names.iter().any(|name| name == category),
        );
        if affected.contains(&native) {
            assert!(
                found.contains(&reason),
                "{label} × {category}: expected {reason} in {disjoint}"
            );
            assert!(missing, "{label} × {category} missing: {disjoint}");
        } else {
            // A category its convention cannot derive is absent, never a
            // qualified amount: Pi checks reasoning against output.
            let undetermined = found == ["undetermined"] && disjoint.get(category).is_none();
            assert!(
                found.is_empty() || undetermined,
                "{label} × {category}: {disjoint}"
            );
        }
    }
    let summed_partial = SUMMED.iter().any(|counter| affected.contains(counter));
    assert_eq!(
        disjoint.get("total").is_none(),
        summed_partial,
        "{label} × total: {disjoint}"
    );
}

#[test]
fn every_recorded_reason_qualifies_every_dependent_figure() {
    for case in CASES {
        let reads: &[bool] = if case.full { &[false, true] } else { &[false] };
        for &full in reads {
            let label = format!("{} (full={full})", case.name);
            let store = Store::new("case");
            let (lines, bound) = recording(case.shape, "/fixtures/affected");
            store.write(&lines);
            let (mut transcript, mut view) = read(&store, bound, full);
            if apply(case.shape, &mut transcript.session) {
                view = serde_json::to_value(usage::usage(&transcript)).unwrap();
            }
            let session = &transcript.session;

            assert_native(&format!("{label} usage"), &view, case.reason, case.affected);
            assert_disjoint(
                &format!("{label} usage"),
                &view["disjoint"],
                case.reason,
                case.affected,
            );
            assert_native(
                &format!("{label} model"),
                &view["by_model"][0],
                case.reason,
                case.affected,
            );
            let consumption = &view["consumption"];
            match case.consumption {
                Some(reason) => assert_native(
                    &format!("{label} consumption"),
                    consumption,
                    reason,
                    case.affected,
                ),
                None => assert_native(&format!("{label} consumption"), consumption, "", &[]),
            }

            let (aggregate, partitions) =
                usage::partitioned_aggregate(std::slice::from_ref(session), &[GroupBy::Harness]);
            let totals = serde_json::to_value(&aggregate.totals).unwrap();
            let group = serde_json::to_value(&aggregate.groups[0]).unwrap();
            let partition = serde_json::to_value(&partitions[0]).unwrap();
            for (name, tally) in [
                ("summary total", &totals),
                ("summary group", &group),
                ("summary partition", &partition),
            ] {
                assert_native(
                    &format!("{label} {name}"),
                    tally,
                    case.reason,
                    case.affected,
                );
                assert_disjoint(
                    &format!("{label} {name}"),
                    &tally["disjoint"],
                    case.reason,
                    case.affected,
                );
            }
            let figure = serde_json::to_value(usage::SessionUsageFigure::from(session)).unwrap();
            assert_native(
                &format!("{label} session figure"),
                &figure,
                case.reason,
                case.affected,
            );

            let stats = serde_json::to_value(agent_tapes_core::stats::stats(
                transcript.clone(),
                &Default::default(),
            ))
            .unwrap();
            assert_native(
                &format!("{label} stats"),
                &stats["usage"],
                case.reason,
                case.affected,
            );
            for ratio in ["cache_read_ratio", "cache_write_ratio"] {
                assert_eq!(
                    stats["usage"].get(ratio).is_none(),
                    RATIO.iter().any(|counter| case.affected.contains(counter)),
                    "{label} stats {ratio}: {stats}"
                );
            }
            let brief = serde_json::to_value(agent_tapes_core::brief::brief(
                transcript.clone(),
                Ok(Default::default()),
                12,
            ))
            .unwrap();
            assert_native(
                &format!("{label} brief"),
                &brief["usage"],
                case.reason,
                case.affected,
            );

            let restored: Transcript =
                serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
            let restored = serde_json::to_value(usage::usage(&restored)).unwrap();
            assert_native(
                &format!("{label} wire"),
                &restored,
                case.reason,
                case.affected,
            );
            assert_disjoint(
                &format!("{label} wire"),
                &restored["disjoint"],
                case.reason,
                case.affected,
            );

            // A fork may share its origin's events, so their union is
            // bounded; the upper bound needs each counter completely measured.
            let mut twin = session.clone();
            twin.id = "twin".to_owned();
            forked_from(&mut twin, &session.id);
            let unioned =
                serde_json::to_value(usage::aggregate(&[session.clone(), twin], &[])).unwrap();
            assert!(unioned["totals"].get("tokens").is_none(), "{unioned}");
            for counter in NATIVE {
                let bound = &unioned["totals"]["identity_bounds"][counter];
                let held = if counter == "cost" {
                    figure.get("cost").is_some()
                } else {
                    figure["tokens"].get(counter).is_some()
                };
                assert_eq!(
                    bound.get("at_least").is_some(),
                    held,
                    "{label} bound × {counter}: {unioned}"
                );
                if !held {
                    continue;
                }
                assert_eq!(
                    bound.get("at_most").is_none(),
                    case.affected.contains(&counter),
                    "{label} bound × {counter}: {bound}"
                );
            }
        }
    }
}

#[test]
fn an_affected_session_leaves_a_healthy_sessions_figures_whole() {
    for case in CASES {
        let affected_store = Store::new("affected");
        let (lines, bound) = recording(case.shape, "/fixtures/affected");
        affected_store.write(&lines);
        let (mut affected, _) = read(&affected_store, bound, false);
        apply(case.shape, &mut affected.session);
        let healthy_store = Store::new("healthy");
        healthy_store.write(&[header("/fixtures/healthy"), request(None).to_string()]);
        let (healthy, view) = read(&healthy_store, None, false);
        assert!(
            view.get("measurement_completeness").is_none(),
            "{}: {view}",
            case.name
        );
        let mut healthy = healthy.session;
        healthy.id = "healthy".to_owned();
        let healthy_session = healthy.clone();

        let summary = serde_json::to_value(usage::aggregate(
            &[affected.session.clone(), healthy],
            &[GroupBy::Directory],
        ))
        .unwrap();
        let group = summary["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["key"]["directory"] == "/fixtures/healthy")
            .unwrap();
        assert!(
            group.get("measurement_completeness").is_none(),
            "{}: {group}",
            case.name
        );
        assert_eq!(group["tokens"]["input"], 10, "{}: {group}", case.name);
        assert_eq!(group["disjoint"]["total"], 22, "{}: {group}", case.name);
        let figure =
            serde_json::to_value(usage::SessionUsageFigure::from(&healthy_session)).unwrap();
        assert!(
            figure.get("measurement_completeness").is_none(),
            "{}: {figure}",
            case.name
        );
        assert_eq!(figure["tokens"]["input"], 10);
    }
}

#[test]
fn a_stopped_model_does_not_qualify_an_independent_healthy_model() {
    let store = Store::new("models");
    let mut lines = vec![header("/fixtures/models")];
    for (id, parent, model, stop) in [
        ("stopped", None, "stopped-model", "aborted"),
        ("healthy", Some("stopped"), "healthy-model", "stop"),
    ] {
        let mut entry = request(parent);
        entry["id"] = json!(id);
        entry["message"]["model"] = json!(model);
        entry["message"]["responseId"] = json!(id);
        entry["message"]["stopReason"] = json!(stop);
        lines.push(entry.to_string());
    }
    store.write(&lines);
    for full in [false, true] {
        let (_, view) = read(&store, None, full);
        let models = view["by_model"].as_array().unwrap();
        let model = |name: &str| {
            models
                .iter()
                .find(|model| model["model"] == name)
                .unwrap()
                .clone()
        };
        let healthy = model("healthy-model");
        assert_eq!(healthy["tokens"]["input"], 10);
        assert_eq!(healthy["cost"]["usd"], 0.5);
        assert!(
            healthy.get("measurement_completeness").is_none(),
            "{healthy}"
        );
        assert_native(
            "stopped model",
            &model("stopped-model"),
            "stopped-attempt",
            &NATIVE,
        );
        assert_native("session", &view, "stopped-attempt", &NATIVE);
        assert!(view["disjoint"].get("total").is_none());
    }
}

#[test]
fn an_unattributed_request_qualifies_every_model_share() {
    let store = Store::new("attribution");
    store.write(&[header("/fixtures/attribution"), request(None).to_string()]);
    let (mut transcript, _) = read(&store, None, false);
    let detail = transcript.session.usage_detail.as_mut().unwrap();
    detail.attribution = Some(usage::UsageAttribution {
        basis: usage::ObservationBasis::UsageRecord,
        coverage: AccountingCoverage::Session,
        observed: 2,
        counted: 2,
        attributed: 1,
        unattributed: 1,
        leading_uncounted: 0,
        resets: 0,
        outside_total: 0,
        outside_total_tokens: None,
        outside_total_coverage: Default::default(),
        unattributed_tokens: None,
        unattributed_coverage: Default::default(),
        unattributed_completeness: Default::default(),
        incomplete: vec!["model-attribution-unknown".to_owned()],
        bounds: Vec::new(),
    });
    let view = serde_json::to_value(usage::usage(&transcript)).unwrap();
    for counter in TOKENS {
        assert!(
            reasons(&view["by_model"][0], counter).contains(&"incomplete-attribution"),
            "{counter}: {view}"
        );
        assert!(reasons(&view, counter).is_empty(), "{counter}: {view}");
    }
    assert_eq!(view["disjoint"]["total"], 22);
}

/// A cumulative cost-state kept beside the request sum splits by model, and
/// each share follows the cost-state's own coverage and the failures every
/// model shares, not the session counters' scope.
#[test]
fn a_recorded_models_share_follows_the_recorded_accounting() {
    let recorded = |coverage, cache_read| RecordedUsage {
        accounting: Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage,
            selection: None,
        },
        tokens: None,
        cost: None,
        written_cost_usd: None,
        cost_coverage: None,
        by_model: Some(vec![ModelUsage {
            list_price_estimate: None,
            cache_write_by_ttl: None,
            model: "recorded-model".to_owned(),
            variant: None,
            tokens: Some(Tokens {
                input: Some(10),
                output: Some(5),
                reasoning: Some(2),
                cache_read,
                cache_write: Some(4),
            }),
            cost: Some(Cost { usd: 0.5 }),
            cost_coverage: None,
            request_count: None,
            measurement_coverage: Default::default(),
            measurement_incomplete: Vec::new(),
            measurement_completeness: Default::default(),
        }]),
    };
    let store = Store::new("recorded-model");
    store.write(&[header("/fixtures/recorded"), request(None).to_string()]);
    let view = |coverage, cache_read, failures| {
        let (mut transcript, _) = read(&store, None, false);
        let detail = transcript.session.usage_detail.as_mut().unwrap();
        detail.recorded_total = Some(recorded(coverage, cache_read));
        detail.request_failures = failures;
        serde_json::to_value(usage::usage(&transcript)).unwrap()
    };
    let share = |view: &Value| {
        let model = view["recorded_total"]["by_model"][0].clone();
        assert_eq!(model["model"], "recorded-model", "{view}");
        model
    };

    let whole = view(AccountingCoverage::Session, Some(3), 0);
    assert!(
        share(&whole).get("measurement_completeness").is_none(),
        "{whole}"
    );

    let missing = share(&view(AccountingCoverage::Session, None, 0));
    assert_native(
        "recorded missing counter",
        &missing,
        "missing-counter",
        &["cache_read"],
    );

    let windowed = view(AccountingCoverage::ReadWindow, Some(3), 0);
    assert_native(
        "recorded read window",
        &share(&windowed),
        "read-window",
        &NATIVE,
    );
    // The session's own split covers the whole session, whatever the
    // cost-state covers.
    assert!(
        windowed["by_model"][0]
            .get("measurement_completeness")
            .is_none(),
        "{windowed}"
    );

    let stopped = view(AccountingCoverage::Session, Some(3), 1);
    assert_native(
        "recorded stopped",
        &share(&stopped),
        "stopped-attempt",
        &NATIVE,
    );
}

/// A model that counted requests and recorded no counter has no amount to
/// qualify, yet it is no complete measurement of what it counted.
#[test]
fn a_model_that_counted_requests_and_recorded_nothing_is_incomplete() {
    let store = Store::new("counted-nothing");
    store.write(&[
        header("/fixtures/counted-nothing"),
        request(None).to_string(),
    ]);
    let (mut transcript, _) = read(&store, None, false);
    let detail = transcript.session.usage_detail.as_mut().unwrap();
    detail.by_model = Some(vec![ModelUsage {
        list_price_estimate: None,
        cache_write_by_ttl: None,
        model: "counted-model".to_owned(),
        variant: None,
        tokens: None,
        cost: None,
        cost_coverage: None,
        request_count: Some(2),
        measurement_coverage: Default::default(),
        measurement_incomplete: Vec::new(),
        measurement_completeness: Default::default(),
    }]);
    let view = serde_json::to_value(usage::usage(&transcript)).unwrap();
    let model = &view["by_model"][0];
    for counter in TOKENS {
        assert!(
            reasons(model, counter).contains(&"missing-counter"),
            "{counter}: {model}"
        );
    }
}
