//! Usage pricing reads the config table merged over the fetched data table,
//! and a pricing command refreshes the data table only when it is due.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::price_update::{
    refresh_due, FetchRecord, RefreshDecision, SourceRecord, FETCH_SCHEMA,
};
use agent_tapes_core::pricing::{lookup_defaults, PriceLookup, PriceOrigin, PriceTable};
use chrono::{DateTime, Duration, TimeZone, Utc};

const DATA: &str = r#"schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "model-a"
aliases = ["model-a-latest"]
effective = 2026-10-01
source = "fetched"
[models.per_million]
uncached_input = 1.0

[[models]]
id = "model-b"
effective = 2026-10-01
source = "fetched"
[models.per_million]
uncached_input = 2.0
"#;

const CONFIG: &str = r#"schema = "tapes-price-table/1"
currency = "USD"

[[models]]
id = "model-a"
effective = 2026-10-02
source = "mine"
[models.per_million]
uncached_input = 9.0
"#;

fn write(root: &Path, name: &str, text: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, text).unwrap();
    path
}

fn loaded(found: PriceLookup) -> (PriceTable, PriceOrigin) {
    match found {
        PriceLookup::Loaded(loaded) => (loaded.table, loaded.origin),
        other => panic!("no table: {other:?}"),
    }
}

#[test]
fn a_config_row_wins_its_id_and_aliases_and_data_rows_price_the_rest() {
    // Claim: the config row for model-a answers model-a and the alias only
    // the data row named, while the data row prices model-b. Rejects the
    // data table overriding the config table, and a data row surviving
    // under an alias the config row's id claimed.
    let root = tempfile::tempdir().unwrap();
    let config = write(root.path(), "config.toml", CONFIG);
    let data = write(root.path(), "data.toml", DATA);
    let lookup = lookup_defaults(Some(&config), Some(&data));
    assert!(lookup.warnings.is_empty(), "{:?}", lookup.warnings);
    let (table, origin) = loaded(lookup.found);
    assert_eq!(origin, PriceOrigin::ConfigData);
    assert_eq!(table.price("model-a").unwrap().source, "mine");
    assert!(table.price("model-a-latest").is_none());
    assert_eq!(table.price("model-b").unwrap().source, "fetched");
    assert_eq!(table.schema, "tapes-price-table/2");
}

#[test]
fn a_config_table_in_another_currency_is_used_alone_with_a_warning() {
    // Claim: tables in different currencies are not merged; the config table
    // prices alone and one warning names both currencies. Rejects mixing
    // currencies in one table, or a silent drop of the data table.
    let root = tempfile::tempdir().unwrap();
    let config = write(
        root.path(),
        "config.toml",
        &CONFIG.replace("\"USD\"", "\"EUR\""),
    );
    let data = write(root.path(), "data.toml", DATA);
    let lookup = lookup_defaults(Some(&config), Some(&data));
    assert_eq!(lookup.warnings.len(), 1);
    assert!(lookup.warnings[0].contains("EUR") && lookup.warnings[0].contains("USD"));
    let (table, origin) = loaded(lookup.found);
    assert_eq!(origin, PriceOrigin::Config);
    assert!(table.price("model-b").is_none());
}

#[test]
fn an_invalid_data_table_is_set_aside_with_a_warning() {
    // Claim: a data table that does not parse leaves the config table
    // pricing alone, or no table, with a warning naming its path. Rejects
    // the invalid data table withholding every estimate, or going unreported.
    let root = tempfile::tempdir().unwrap();
    let config = write(root.path(), "config.toml", CONFIG);
    let data = write(root.path(), "data.toml", "schema = 1\n");
    let lookup = lookup_defaults(Some(&config), Some(&data));
    assert_eq!(lookup.warnings.len(), 1);
    assert!(lookup.warnings[0].contains(data.to_str().unwrap()));
    let (table, origin) = loaded(lookup.found);
    assert_eq!(origin, PriceOrigin::Config);
    assert_eq!(table.price("model-a").unwrap().source, "mine");

    let alone = lookup_defaults(Some(&root.path().join("absent.toml")), Some(&data));
    assert_eq!(alone.warnings.len(), 1);
    assert!(matches!(alone.found, PriceLookup::Absent));
}

#[test]
fn either_table_alone_is_used_as_it_is() {
    // Claim: with one of the two tables present, it prices alone and says
    // which it is. Rejects absence when only the data table exists.
    let root = tempfile::tempdir().unwrap();
    let data = write(root.path(), "data.toml", DATA);
    let absent = root.path().join("absent.toml");
    let (_, origin) = loaded(lookup_defaults(Some(&absent), Some(&data)).found);
    assert_eq!(origin, PriceOrigin::Data);
    let config = write(root.path(), "config.toml", CONFIG);
    let (_, origin) = loaded(lookup_defaults(Some(&config), Some(&absent)).found);
    assert_eq!(origin, PriceOrigin::Config);
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).unwrap()
}

fn record(success_ago: Duration, attempt_ago: Duration) -> FetchRecord {
    FetchRecord {
        schema: FETCH_SCHEMA.to_owned(),
        attempted_at: now() - attempt_ago,
        sources: BTreeMap::from([(
            "anthropic".to_owned(),
            SourceRecord {
                url: "https://example.com/pricing.md".to_owned(),
                ok: success_ago == attempt_ago,
                fetched_at: Some(now() - success_ago),
                error: None,
                models: 1,
            },
        )]),
    }
}

#[test]
fn a_refresh_is_due_only_past_a_day_of_no_success_and_an_hour_of_no_attempt() {
    // Claim: a success under 24 hours old keeps the table fresh; a stale
    // table waits an hour after the last attempt; an absent table with no
    // record is due. Rejects a refresh on every run, and a stale table
    // retried before the hour is out.
    let minutes = Duration::minutes;
    let fresh = record(minutes(23 * 60 + 59), minutes(23 * 60 + 59));
    assert!(!refresh_due(Some(&fresh), true, now()));
    let retried = record(Duration::hours(24), minutes(59));
    assert!(!refresh_due(Some(&retried), true, now()));
    let waited = record(Duration::hours(24), minutes(61));
    assert!(refresh_due(Some(&waited), true, now()));
    assert!(refresh_due(None, false, now()));
    assert!(refresh_due(Some(&fresh), false, now()));
    assert!(matches!(
        RefreshDecision::at(Some(&retried), true, now()),
        RefreshDecision::WaitingSince { .. }
    ));
    assert!(matches!(
        RefreshDecision::at(Some(&waited), true, now()),
        RefreshDecision::StaleSince { .. }
    ));
    assert_eq!(
        RefreshDecision::at(None, false, now()),
        RefreshDecision::Absent
    );
}

#[test]
fn a_claimed_attempt_holds_off_a_concurrent_refresh_and_keeps_successes() {
    // Claim: claiming an attempt before fetching writes the record with the
    // new attempt and every source's last success, so a command starting
    // meanwhile finds no refresh due. Rejects concurrent commands each
    // fetching, and a claim that forgets when a source last succeeded.
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join(agent_tapes_core::price_update::FETCH_FILE);
    let stale = record(Duration::hours(30), Duration::hours(30));
    fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
    assert!(refresh_due(Some(&stale), true, now()));
    agent_tapes_core::price_update::claim_attempt(data.path(), now()).unwrap();
    let claimed = FetchRecord::read(&path).unwrap();
    assert_eq!(claimed.attempted_at, now());
    assert_eq!(claimed.sources, stale.sources);
    assert!(!refresh_due(Some(&claimed), true, now()));
}
