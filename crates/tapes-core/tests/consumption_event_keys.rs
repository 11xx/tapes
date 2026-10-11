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
