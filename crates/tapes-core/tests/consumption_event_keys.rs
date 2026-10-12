use std::path::Path;

use agent_tapes_core::backend::{pi::PiBackend, Backend};
use agent_tapes_core::usage::{ConsumptionKeyOptions, UsageOptions};
use agent_tapes_core::{usage_with_options_with_backends, Selection};

#[test]
fn public_usage_options_bound_keys_without_shortening_measurements() {
    for full in [false, true] {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-consumption-keys");
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(root))];
        let view = usage_with_options_with_backends(
            &backends,
            Selection::Id("keys-parent"),
            UsageOptions {
                reconcile: false,
                full,
                series: None,
                event_keys: Some(ConsumptionKeyOptions { limit: 1 }),
            },
        )
        .unwrap();
        let keys = view.consumption_event_keys.unwrap();
        assert_eq!(keys.observed, Some(3));
        assert_eq!(keys.returned, 1);
        assert_eq!(keys.omissions.row_cap, 2);
        assert_eq!(keys.keys[0].response_id.as_deref(), Some("response-parent"));
        assert_eq!(view.consumption.unwrap().cost.unwrap().usd, 0.75);
        assert_eq!(view.cost.unwrap().usd, 0.75);
        assert_eq!(view.schema, "tapes-usage/11");
    }
}

#[test]
fn invalid_key_limits_refuse_before_session_resolution() {
    for limit in [0, 10_001] {
        let result = usage_with_options_with_backends(
            &[],
            Selection::Id("absent"),
            UsageOptions {
                reconcile: false,
                full: false,
                series: None,
                event_keys: Some(ConsumptionKeyOptions { limit }),
            },
        );
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("between 1 and 10000"));
    }
}

#[test]
fn bounded_measurements_and_keys_share_the_transcript_frontier() {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("tapes-key-frontier-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let file = "2026-01-15T10-00-00-000Z_keys-parent.jsonl";
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pi-consumption-keys")
        .join(file);
    let path = root.join(file);
    fs::copy(fixture, &path).unwrap();
    let backend = PiBackend::new(&root);
    let cached = backend.locate("keys-parent").unwrap().unwrap();
    let extra = serde_json::json!({"type":"usage","id":"later-usage","parentId":"parent-request","timestamp":"2026-01-15T10:00:04Z","kind":"fixture","provider":"fixture-provider","model":"fixture-model","usage":{"input":100,"output":10,"reasoning":0,"cacheRead":0,"cacheWrite":0,"totalTokens":110,"cost":{"total":1}}});
    let contents = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{contents}{extra}\n")).unwrap();
    let transcript = backend.transcript(&cached, usize::MAX).unwrap();
    let keys = backend
        .consumption_event_keys(
            &transcript.session,
            transcript.read.as_ref(),
            ConsumptionKeyOptions { limit: 200 },
        )
        .unwrap();
    let view = agent_tapes_core::usage::usage(&transcript);
    fs::remove_dir_all(root).unwrap();
    assert_eq!(keys.observed, Some(4));
    assert_eq!(view.consumption.as_ref().unwrap().events.counted, 4);
    assert_eq!(view.cost.unwrap().usd, 1.75);
    assert_eq!(cached.cost.unwrap().usd, 0.75);
}

mod amounts {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use agent_tapes_core::backend::{pi::PiBackend, Backend};
    use agent_tapes_core::consumption_union::UnionWithheld;
    use agent_tapes_core::usage::{ConsumptionEventKeys, ConsumptionKeyOptions, UsageOptions};
    use agent_tapes_core::{usage_with_options_with_backends, Selection};
    use serde_json::json;

    const PARENT: &str = "2026-01-15T10-00-00-000Z_keys-parent.jsonl";
    const FORK: &str = "2026-01-15T11-00-00-000Z_keys-fork.jsonl";

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi-consumption-keys")
    }

    fn keys_in(root: &Path, id: &str, limit: usize, full: bool) -> ConsumptionEventKeys {
        let backends: Vec<Box<dyn Backend>> = vec![Box::new(PiBackend::new(root))];
        usage_with_options_with_backends(
            &backends,
            Selection::Id(id),
            UsageOptions {
                reconcile: false,
                full,
                series: None,
                event_keys: Some(ConsumptionKeyOptions { limit }),
            },
        )
        .unwrap()
        .consumption_event_keys
        .unwrap()
    }

    /// The keys of a session in a store whose fork names its parent's
    /// recording.
    fn keys(id: &str, limit: usize, full: bool) -> ConsumptionEventKeys {
        let store = Store::new(|_| {});
        keys_in(&store.0, id, limit, full)
    }

    /// A copy of the fixtures that `edit` may change.
    struct Store(PathBuf);

    impl Store {
        fn new(edit: impl FnOnce(&Path)) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("tapes-key-amounts-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            for file in [PARENT, FORK] {
                fs::copy(fixtures().join(file), root.join(file)).unwrap();
            }
            // A fork's header names the recording it was copied from.
            let fork = root.join(FORK);
            let header = fs::read_to_string(&fork).unwrap().replace(
                "/fixtures/2026-01-15T10-00-00-000Z_keys-parent.jsonl",
                &root.join(PARENT).display().to_string(),
            );
            fs::write(fork, header).unwrap();
            edit(&root);
            Self(root)
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn each_key_carries_its_event_measurement() {
        for full in [false, true] {
            let parent = keys("keys-parent", 200, full);
            let shared = &parent.keys[0];
            assert_eq!(shared.response_id.as_deref(), Some("response-shared"));
            let measurement = shared.measurement.as_ref().unwrap();
            let disjoint = measurement.disjoint.as_ref().unwrap();
            assert_eq!(disjoint.uncached_input, Some(10));
            assert_eq!(disjoint.output, Some(1));
            assert_eq!(disjoint.total, Some(11));
            assert_eq!(measurement.cost.as_ref().unwrap().usd, 0.125);
            assert!(measurement.cost_coverage.is_none());
            assert!(measurement.measurement_completeness.is_empty());
            let value = serde_json::to_value(shared).unwrap();
            assert_eq!(
                value["measurement"]["cost"]["kind"],
                "harness-calculated-estimate"
            );
            assert_eq!(parent.session_id.as_deref(), Some("keys-parent"));
            assert_eq!(
                keys("keys-fork", 200, full).forked_from.as_deref(),
                Some("keys-parent")
            );
        }
    }

    #[test]
    fn a_parent_and_its_fork_give_the_exact_union_amount() {
        for full in [false, true] {
            let union = keys("keys-parent", 200, full).union(&keys("keys-fork", 200, full));
            assert!(union.withheld.is_empty(), "{union:?}");
            let amounts = union.amounts.unwrap();
            // Three parent events and the fork's own request, with the two
            // shared events counted once.
            assert_eq!(amounts.events, 4);
            assert_eq!(amounts.cost.as_ref().unwrap().usd, 1.25);
            assert_eq!(amounts.disjoint.total, Some(110));
            assert_eq!(amounts.disjoint.uncached_input, Some(100));
            assert_eq!(amounts.disjoint.output, Some(10));
            assert!(amounts.measurement_completeness.is_empty());
            // The order of the two selections does not matter.
            let swapped = keys("keys-fork", 200, full).union(&keys("keys-parent", 200, full));
            assert_eq!(swapped.amounts, Some(amounts));
        }
    }

    #[test]
    fn an_omitted_key_withholds_the_exact_union() {
        for (parent_limit, fork_limit) in [(2, 200), (200, 2)] {
            let parent = keys("keys-parent", parent_limit, false);
            let fork = keys("keys-fork", fork_limit, false);
            let union = parent.union(&fork);
            assert!(union.amounts.is_none(), "{union:?}");
            assert!(union.withheld.contains(&UnionWithheld::OmittedKeys));
            // The known amounts still bound the union.
            let cost = union.bounds.cost.unwrap();
            assert!(cost.at_least >= 0.5, "{cost:?}");
            assert!(cost.at_most.is_none(), "{cost:?}");
        }
    }

    #[test]
    fn selections_of_unknown_lineage_withhold_the_union_and_bound_it() {
        let parent = keys("keys-parent", 200, false);
        let mut stranger = keys("keys-fork", 200, false);
        stranger.session_id = Some("keys-stranger".to_owned());
        stranger.forked_from = None;
        let union = parent.union(&stranger);
        assert_eq!(union.withheld, [UnionWithheld::UnestablishedLineage]);
        assert!(union.amounts.is_none());
        // Each selection keeps its own figure, and the union lies between the
        // larger of the two and their sum.
        let cost = union.bounds.cost.unwrap();
        assert_eq!(cost.at_least, 0.875);
        assert_eq!(cost.at_most, Some(0.75 + 0.875));
        let input = union.bounds.uncached_input.unwrap();
        assert_eq!(input.at_least, 70);
        assert_eq!(input.at_most, Some(60 + 70));
    }

    #[test]
    fn amounts_of_different_bases_never_join_one_union() {
        use agent_tapes_core::model::AccountingBasis;
        let parent = keys("keys-parent", 200, false);
        let mut fork = keys("keys-fork", 200, false);
        fork.basis = AccountingBasis::RecordedTotal;
        let union = parent.union(&fork);
        assert!(union.amounts.is_none(), "{union:?}");
        assert_eq!(union.withheld, [UnionWithheld::MixedBasis]);
    }

    #[test]
    fn a_selection_without_a_session_id_establishes_no_lineage() {
        let mut parent = keys("keys-parent", 200, false);
        let fork = keys("keys-fork", 200, false);
        parent.session_id = None;
        let union = parent.union(&fork);
        assert_eq!(union.withheld, [UnionWithheld::UnestablishedLineage]);
    }

    #[test]
    fn an_unreadable_selection_withholds_the_union_and_keeps_its_peer() {
        let store = Store::new(|_| {});
        let parent = keys_in(&store.0, "keys-parent", 200, false);
        // The fork's recording becomes unreadable after the session resolved.
        let backend = PiBackend::new(&store.0);
        let session = backend.locate("keys-fork").unwrap().unwrap();
        fs::remove_file(store.0.join(FORK)).unwrap();
        let gone = backend
            .consumption_event_keys(&session, None, ConsumptionKeyOptions { limit: 200 })
            .unwrap();
        assert_eq!(gone.unavailable, Some("source-unreadable-or-changed"));
        let union = parent.union(&gone);
        assert!(union.amounts.is_none(), "{union:?}");
        assert!(union.withheld.contains(&UnionWithheld::OmittedKeys));
        // The healthy parent's own amounts stay on its keys.
        assert_eq!(
            parent.keys[0]
                .measurement
                .as_ref()
                .unwrap()
                .cost
                .as_ref()
                .unwrap()
                .usd,
            0.125
        );
    }

    #[test]
    fn a_response_without_a_verified_copy_withholds_the_union() {
        // The fork records the parent's response as an entry of its own, so
        // the equal response identifiers establish nothing across recordings.
        let store = Store::new(|root| {
            let path = root.join(FORK);
            let body = fs::read_to_string(&path)
                .unwrap()
                .replace("\"id\":\"shared-request\"", "\"id\":\"renamed-request\"")
                .replace(
                    "\"parentId\":\"shared-request\"",
                    "\"parentId\":\"renamed-request\"",
                );
            fs::write(path, body).unwrap();
        });
        let union = keys_in(&store.0, "keys-parent", 200, false).union(&keys_in(
            &store.0,
            "keys-fork",
            200,
            false,
        ));
        assert!(union.amounts.is_none(), "{union:?}");
        assert_eq!(union.withheld, [UnionWithheld::UnestablishedIdentity]);
    }

    #[test]
    fn a_changed_copy_withholds_the_union() {
        let store = Store::new(|root| {
            let path = root.join(FORK);
            let body = fs::read_to_string(&path)
                .unwrap()
                .replace("\"total\":0.25", "\"total\":0.5");
            fs::write(path, body).unwrap();
        });
        let union = keys_in(&store.0, "keys-parent", 200, false).union(&keys_in(
            &store.0,
            "keys-fork",
            200,
            false,
        ));
        assert!(union.amounts.is_none(), "{union:?}");
        assert!(union.withheld.contains(&UnionWithheld::UnverifiedCopies));
    }

    #[test]
    fn an_unavailable_parent_withholds_the_union_and_keeps_the_fork_figure() {
        let store = Store::new(|root| fs::remove_file(root.join(PARENT)).unwrap());
        let fork = keys_in(&store.0, "keys-fork", 200, false);
        assert_eq!(fork.returned, 3);
        // The fork's own keys and amounts stay readable.
        assert_eq!(
            fork.keys[2]
                .measurement
                .as_ref()
                .unwrap()
                .cost
                .as_ref()
                .unwrap()
                .usd,
            0.5
        );
        let parent = keys("keys-parent", 200, false);
        let union = parent.union(&fork);
        assert!(union.amounts.is_none());
        assert!(union.withheld.contains(&UnionWithheld::UnverifiedCopies));
    }

    #[test]
    fn an_event_missing_a_counter_or_a_price_qualifies_the_union_and_its_total() {
        let store = Store::new(|root| {
            let path = root.join(PARENT);
            // The parent's own request records no cache counters and writes a
            // zero price beside usage.
            let body = fs::read_to_string(&path)
                .unwrap()
                .replace(
                    "\"usage\":{\"input\":30,\"output\":3,\"reasoning\":0,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":33,\"cost\":{\"total\":0.375}}",
                    "\"usage\":{\"input\":30,\"output\":3,\"reasoning\":0,\"totalTokens\":33,\"cost\":{\"total\":0}}",
                );
            fs::write(path, body).unwrap();
        });
        let parent = keys_in(&store.0, "keys-parent", 200, false);
        let event = parent.keys[2].measurement.as_ref().unwrap();
        assert_eq!(event.cost, None);
        assert_eq!(
            event.cost_coverage.as_ref().unwrap().zero_written_requests,
            1
        );
        let reasons = |counter: &str| -> Vec<String> {
            serde_json::to_value(&event.measurement_completeness).unwrap()[counter]
                .as_array()
                .map(|reasons| {
                    reasons
                        .iter()
                        .map(|reason| reason.as_str().unwrap().to_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(reasons("cache_read"), ["missing-counter"]);
        assert!(reasons("cost").contains(&"unpriced".to_owned()));

        let fork = keys_in(&store.0, "keys-fork", 200, false);
        let union = parent.union(&fork);
        // Membership is exact, and the amounts are known subtotals.
        assert!(union.withheld.is_empty(), "{union:?}");
        let amounts = union.amounts.unwrap();
        assert_eq!(amounts.disjoint.total, None);
        assert_eq!(amounts.disjoint.uncached_input, Some(100));
        assert_eq!(amounts.cost.as_ref().unwrap().usd, 0.875);
        assert!(!amounts
            .measurement_completeness
            .is_complete(&["cache_read", "cost"]));
        assert!(amounts.disjoint.missing.contains(&"cache_read"));
        assert_eq!(
            amounts
                .cost_coverage
                .as_ref()
                .unwrap()
                .zero_written_requests,
            1
        );
        // The upper bound of a counter some event lacks is unmeasured.
        let bounds = serde_json::to_value(&union.bounds).unwrap();
        assert!(bounds["cost"]["at_most"].is_null(), "{bounds}");
    }

    #[test]
    fn a_key_and_its_measurement_carry_no_recording_locator() {
        let fork = keys("keys-fork", 200, true);
        let text = serde_json::to_string(&fork).unwrap();
        for private in ["/fixtures", "tests/fixtures", ".jsonl", "cwd"] {
            assert!(!text.contains(private), "{private} in {text}");
        }
        let union = serde_json::to_string(&keys("keys-parent", 200, true).union(&fork)).unwrap();
        for private in ["/fixtures", ".jsonl"] {
            assert!(!union.contains(private), "{private} in {union}");
        }
    }

    #[test]
    fn measurements_are_omitted_with_their_keys() {
        let fork = keys("keys-fork", 1, false);
        assert_eq!(fork.returned, 1);
        assert_eq!(fork.omissions.row_cap, 2);
        let value = serde_json::to_value(&fork).unwrap();
        let measured = value["keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|key| key.get("measurement").is_some())
            .count();
        assert_eq!(measured, 1);
        let _ = json!(null);
    }
}
