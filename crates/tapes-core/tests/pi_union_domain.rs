//! A Pi consumption union claims an upper bound or an exact amount only where
//! every event's consumption is measured, and only within the accounting
//! domain its keys were selected from.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{pi::PiBackend, Backend};
use agent_tapes_core::consumption_union::UnionWithheld;
use agent_tapes_core::measurement::MeasurementReason;
use agent_tapes_core::usage::{ConsumptionEventKeys, ConsumptionKeyOptions, UsageOptions};
use agent_tapes_core::{usage_with_options_with_backends, Selection};

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
            "tapes-pi-union-{label}-{}-{nonce}",
            std::process::id()
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
