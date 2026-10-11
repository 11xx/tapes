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
