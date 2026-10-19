//! A Pi consumption union claims an upper bound or an exact amount only where
//! every event's consumption is measured, and only within the accounting
//! domain its keys were selected from.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{pi::PiBackend, Backend};
use agent_tapes_core::consumption_union::UnionWithheld;
use agent_tapes_core::measurement::MeasurementReason;
use agent_tapes_core::model::AccountingSelection;
use agent_tapes_core::usage::{ConsumptionEventKeys, ConsumptionKeyOptions, UsageOptions};
use agent_tapes_core::{
    usage_summary_with_backends, usage_with_options_with_backends, Selection, SessionSelection,
};
use serde_json::Value;

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const PARENT: &str = "2026-01-15T10-00-00-000Z_keys-parent.jsonl";
const FORK: &str = "2026-01-15T11-00-00-000Z_keys-fork.jsonl";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-consumption-keys")
}

/// A copy of the parent/fork fixtures whose fork names the copied parent.
struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-pi-union-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        for file in [PARENT, FORK] {
            fs::copy(fixtures().join(file), root.join(file)).unwrap();
        }
        let store = Self(root);
        store.edit(FORK, |text| {
            text.replace(
                "/fixtures/2026-01-15T10-00-00-000Z_keys-parent.jsonl",
                &store.0.join(PARENT).display().to_string(),
            )
        });
        store
    }

    fn edit(&self, file: &str, change: impl FnOnce(String) -> String) {
        let path = self.0.join(file);
        let text = fs::read_to_string(&path).unwrap();
        fs::write(path, change(text)).unwrap();
    }

    /// Insert `line` after the line holding `after`.
    fn insert(&self, file: &str, after: &str, line: &str) {
        self.edit(file, |text| {
            let mut lines: Vec<&str> = text.lines().collect();
            let at = lines.iter().position(|held| held.contains(after)).unwrap();
            lines.insert(at + 1, line);
            lines.join("\n") + "\n"
        });
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![Box::new(PiBackend::new(&self.0))]
    }

    /// `id`'s keys over `selection`'s branches, against the read a usage
    /// view of the session makes.
    fn selected(
        &self,
        id: &str,
        full: bool,
        selection: AccountingSelection,
    ) -> ConsumptionEventKeys {
        let backend = PiBackend::new(&self.0);
        let session = backend.locate(id).unwrap().unwrap();
        let read = full.then(|| {
            backend
                .stream_transcript(&session, None, &mut |_| Ok(()))
                .unwrap()
                .read_evidence(None)
        });
        backend
            .selected_consumption_event_keys(
                &session,
                read.as_ref(),
                ConsumptionKeyOptions { limit: 200 },
                selection,
            )
            .unwrap()
    }

    fn summary(&self) -> Value {
        let selection = SessionSelection {
            harness: Some("pi"),
            limit: Some(20),
            ..Default::default()
        };
        serde_json::to_value(
            usage_summary_with_backends(&self.backends(), &selection, &[]).unwrap(),
        )
        .unwrap()
    }

    fn keys(&self, id: &str, full: bool) -> ConsumptionEventKeys {
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(&self.0))];
        usage_with_options_with_backends(
            &backends,
            Selection::Id(id),
            UsageOptions {
                reconcile: false,
                full,
                series: None,
                event_keys: Some(ConsumptionKeyOptions { limit: 200 }),
            },
        )
        .unwrap()
        .consumption_event_keys
        .unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn unrelated(mut keys: ConsumptionEventKeys) -> ConsumptionEventKeys {
    keys.session_id = Some("keys-stranger".to_owned());
    keys.forked_from = None;
    keys
}

#[test]
fn a_stopped_request_with_a_price_bounds_cost_only_from_below() {
    let store = Store::new("stopped");
    store.edit(FORK, |text| {
        text.replace(
            r#""responseId":"response-fork","stopReason":"stop""#,
            r#""responseId":"response-fork","stopReason":"aborted""#,
        )
    });
    for full in [false, true] {
        let parent = store.keys("keys-parent", full);
        let fork = store.keys("keys-fork", full);
        for (label, first, second) in [
            ("parent-first", &parent, &fork),
            ("fork-first", &fork, &parent),
        ] {
            let union = first.union(second);
            let cost = union.bounds.cost.as_ref().unwrap();
            assert_eq!(cost.at_least, 0.875, "{label} full={full}");
            assert!(cost.at_most.is_none(), "{label} full={full}: {cost:?}");
            // The exact membership keeps the partial price as a qualified
            // subtotal.
            let amounts = union.amounts.as_ref().expect("exact membership");
            assert_eq!(amounts.cost.as_ref().unwrap().usd, 1.25);
            let reasons: Vec<_> = amounts
                .measurement_completeness
                .iter()
                .filter(|(counter, _)| *counter == "cost")
                .flat_map(|(_, reasons)| reasons.iter().copied())
                .collect();
            assert_eq!(reasons, [MeasurementReason::StoppedAttempt], "{label}");

            let stranger = if std::ptr::eq(first, &fork) {
                unrelated(fork.clone()).union(&parent)
            } else {
                parent.union(&unrelated(fork.clone()))
            };
            assert_eq!(stranger.withheld, [UnionWithheld::UnestablishedLineage]);
            let cost = stranger.bounds.cost.unwrap();
            assert_eq!(cost.at_least, 0.875);
            assert!(cost.at_most.is_none(), "{label} full={full}: {cost:?}");
        }
        // The healthy parent's events stay measured and complete.
        for key in &parent.keys {
            let measurement = key.measurement.as_ref().unwrap();
            assert!(measurement.measurement_completeness.is_empty());
            assert!(measurement.cost.is_some());
        }
    }
}

/// A Pi entry whose usage no reading counts, on the leaf's branch when it
/// follows the leaf and on an abandoned branch when the leaf follows it.
const MYSTERY_ACTIVE: (&str, &str) = (
    r#""id":"parent-request""#,
    r#"{"type":"mystery","id":"unknown","parentId":"parent-request","timestamp":"2026-01-15T10:00:04Z","usage":{"input":7,"cost":{"total":0.2}}}"#,
);
const MYSTERY_ABANDONED: (&str, &str) = (
    r#""id":"shared-request""#,
    r#"{"type":"mystery","id":"unknown","parentId":"shared-request","timestamp":"2026-01-15T10:00:01Z","usage":{"input":7,"cost":{"total":0.2}}}"#,
);

#[test]
fn unrecognized_usage_withholds_the_exact_union_and_its_upper_bounds() {
    for (branch, (after, line)) in [("active", MYSTERY_ACTIVE), ("abandoned", MYSTERY_ABANDONED)] {
        let store = Store::new("unrecognized");
        store.insert(PARENT, after, line);
        let clean = Store::new("recognized");
        for full in [false, true] {
            let label = format!("{branch} full={full}");
            let parent = store.keys("keys-parent", full);
            let fork = store.keys("keys-fork", full);
            assert_eq!(parent.unrecognized_usage, Some(1), "{label}");
            assert_eq!(fork.unrecognized_usage, Some(0), "{label}");
            let value = serde_json::to_value(&parent).unwrap();
            assert_eq!(value["unrecognized_usage"], 1, "{label}: {value}");
            for union in [parent.union(&fork), fork.union(&parent)] {
                assert!(union.amounts.is_none(), "{label}: {union:?}");
                assert_eq!(
                    union.withheld,
                    [UnionWithheld::UnrecognizedUsage],
                    "{label}"
                );
                let json = serde_json::to_value(&union).unwrap();
                assert_eq!(json["withheld"][0], "unrecognized-usage", "{label}");
                let bounds = &union.bounds;
                let healthy = clean
                    .keys("keys-parent", full)
                    .union(&clean.keys("keys-fork", full))
                    .bounds;
                for (name, bound, peer) in [
                    (
                        "uncached_input",
                        &bounds.uncached_input,
                        &healthy.uncached_input,
                    ),
                    ("output", &bounds.output, &healthy.output),
                ] {
                    let bound = bound.as_ref().unwrap();
                    assert!(bound.at_most.is_none(), "{label} {name}: {bounds:?}");
                    // The lower bounds are the healthy selections' own.
                    assert_eq!(bound.at_least, peer.as_ref().unwrap().at_least, "{name}");
                }
                let cost = bounds.cost.as_ref().unwrap();
                assert!(cost.at_most.is_none(), "{label}: {cost:?}");
                assert_eq!(cost.at_least, healthy.cost.unwrap().at_least);
            }
            // Every healthy event keeps its key and its measurement.
            assert_eq!(parent.keys, clean.keys("keys-parent", full).keys, "{label}");
        }
    }
}

/// A measured parent request on a branch the parent rewound past.
const ABANDONED_REQUEST: &str = r#"{"type":"message","id":"parent-abandoned","parentId":"shared-request","timestamp":"2026-01-15T10:00:01Z","message":{"role":"assistant","provider":"fixture-provider","model":"fixture-model","responseId":"response-abandoned","stopReason":"stop","usage":{"input":50,"output":5,"reasoning":0,"cacheRead":0,"cacheWrite":0,"totalTokens":55,"cost":{"total":0.625}},"content":[{"type":"text","text":"An abandoned response."}]}}"#;

/// An unrelated session whose leaf rewound past a large measured request.
const OTHER: &str = "2026-01-15T12-00-00-000Z_keys-other.jsonl";
const OTHER_LINES: &str = concat!(
    r#"{"type":"session","version":3,"id":"keys-other","timestamp":"2026-01-15T12:00:00Z","cwd":"/fixtures/project"}"#,
    "\n",
    r#"{"type":"message","id":"other-root","parentId":null,"timestamp":"2026-01-15T12:00:01Z","message":{"role":"assistant","provider":"fixture-provider","model":"fixture-model","responseId":"response-other-root","stopReason":"stop","usage":{"input":2,"output":1,"reasoning":0,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"cost":{"total":0.25}},"content":[]}}"#,
    "\n",
    r#"{"type":"message","id":"other-abandoned","parentId":"other-root","timestamp":"2026-01-15T12:00:02Z","message":{"role":"assistant","provider":"fixture-provider","model":"fixture-model","responseId":"response-other-abandoned","stopReason":"stop","usage":{"input":100,"output":100,"reasoning":0,"cacheRead":0,"cacheWrite":0,"totalTokens":200,"cost":{"total":8}},"content":[]}}"#,
    "\n",
    r#"{"type":"message","id":"other-leaf","parentId":"other-root","timestamp":"2026-01-15T12:00:03Z","message":{"role":"assistant","provider":"fixture-provider","model":"fixture-model","responseId":"response-other-leaf","stopReason":"stop","usage":{"input":4,"output":1,"reasoning":0,"cacheRead":0,"cacheWrite":0,"totalTokens":5,"cost":{"total":0.25}},"content":[]}}"#,
    "\n",
);

#[test]
fn an_overlap_union_adds_active_branch_amounts_beside_unrelated_sessions() {
    let store = Store::new("domain-summary");
    store.insert(PARENT, r#""id":"shared-request""#, ABANDONED_REQUEST);
    fs::write(store.0.join(OTHER), OTHER_LINES).unwrap();
    let value = store.summary();
    let totals = &value["totals"];
    let related = &totals["overlap"]["related"];
    assert_eq!(related[0]["union"], "exact", "{totals}");
    // The pair's active branches hold four events; the unrelated session's
    // active branch holds two. Neither abandoned request is counted.
    assert_eq!(totals["sessions"], 3, "{totals}");
    assert_eq!(totals["cost"]["usd"], 1.25 + 0.5, "{totals}");
    assert_eq!(totals["disjoint"]["uncached_input"], 100 + 6, "{totals}");
    assert_eq!(totals["disjoint"]["total"], 110 + 8, "{totals}");
    let partitions = value["partitions"].as_array().unwrap();
    assert!(!partitions.is_empty(), "{value}");
    for partition in partitions {
        assert_eq!(
            partition["accounting"]["selection"], "active-branch",
            "{partition}"
        );
    }
}

#[test]
fn keys_name_their_branch_domain_and_unions_stay_within_it() {
    let store = Store::new("domain-keys");
    store.insert(PARENT, r#""id":"shared-request""#, ABANDONED_REQUEST);
    for full in [false, true] {
        let label = format!("full={full}");
        let active = |id| store.selected(id, full, AccountingSelection::ActiveBranch);
        let (parent, fork) = (active("keys-parent"), active("keys-fork"));
        assert_eq!(parent.selection, AccountingSelection::ActiveBranch);
        assert_eq!(parent.observed, Some(3), "{label}");
        assert!(parent
            .keys
            .iter()
            .all(|key| key.response_id.as_deref() != Some("response-abandoned")));
        for union in [parent.union(&fork), fork.union(&parent)] {
            let amounts = union.amounts.expect(&label);
            assert_eq!(amounts.events, 4, "{label}");
            assert_eq!(amounts.cost.unwrap().usd, 1.25, "{label}");
            assert_eq!(amounts.disjoint.total, Some(110), "{label}");
        }
        // The retained surface names its selection and holds the abandoned
        // request.
        let retained = store.keys("keys-parent", full);
        assert_eq!(retained.selection, AccountingSelection::RetainedBranches);
        let json = serde_json::to_value(&retained).unwrap();
        assert_eq!(json["selection"], "retained-branches", "{label}");
        let union = retained.union(&store.keys("keys-fork", full));
        assert_eq!(union.amounts.unwrap().events, 5, "{label}");
        let mixed = retained.union(&fork);
        assert!(mixed.amounts.is_none(), "{label}");
        assert_eq!(mixed.withheld, [UnionWithheld::MixedSelection], "{label}");
    }
}

#[test]
fn a_fork_copy_of_an_entry_its_parent_rewound_past_counts_once() {
    // The parent rewinds past the compaction its fork copied.
    let store = Store::new("domain-rewound");
    store.edit(PARENT, |text| {
        text.replace(
            r#""id":"parent-request","parentId":"shared-compaction""#,
            r#""id":"parent-request","parentId":"shared-request""#,
        )
    });
    for full in [false, true] {
        let active = |id| store.selected(id, full, AccountingSelection::ActiveBranch);
        let (parent, fork) = (active("keys-parent"), active("keys-fork"));
        assert_eq!(parent.observed, Some(2));
        for union in [parent.union(&fork), fork.union(&parent)] {
            let amounts = union.amounts.expect("exact active union");
            assert_eq!(amounts.events, 4);
            assert_eq!(amounts.disjoint.total, Some(110));
            assert_eq!(amounts.cost.unwrap().usd, 1.25);
        }
    }
}

#[test]
fn unrecognized_usage_off_the_active_branch_leaves_the_active_union_exact() {
    let store = Store::new("domain-unrecognized");
    store.insert(PARENT, MYSTERY_ABANDONED.0, MYSTERY_ABANDONED.1);
    for full in [false, true] {
        let active = |id| store.selected(id, full, AccountingSelection::ActiveBranch);
        let parent = active("keys-parent");
        assert_eq!(parent.unrecognized_usage, Some(0));
        let union = parent.union(&active("keys-fork"));
        assert_eq!(union.amounts.expect("exact").events, 4);
    }
    // On the active branch it withholds the active union too.
    let store = Store::new("domain-unrecognized-active");
    store.insert(PARENT, MYSTERY_ACTIVE.0, MYSTERY_ACTIVE.1);
    let parent = store.selected("keys-parent", false, AccountingSelection::ActiveBranch);
    let union =
        parent.union(&store.selected("keys-fork", false, AccountingSelection::ActiveBranch));
    assert_eq!(union.withheld, [UnionWithheld::UnrecognizedUsage]);
}
