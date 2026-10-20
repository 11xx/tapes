//! `tapes prices update` reads the vendors' pricing pages and LiteLLM's
//! JSON into a `tapes-price-table/2` table that keeps each price's history
//! as validity windows, and fails closed per source.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use agent_tapes_core::price_update::{
    self, merge, parse_anthropic, parse_litellm, parse_openai, FetchRecord, Fetched, ANTHROPIC,
    OPENAI,
};
use agent_tapes_core::pricing::{BillingKind, CategoryPrices, PriceTable, PromptCategory};
use chrono::{DateTime, TimeZone, Utc};

fn sources() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prices/sources")
}

fn source(name: &str) -> String {
    fs::read_to_string(sources().join(name)).unwrap()
}

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, 12, 0, 0).unwrap()
}

/// A copy of the source fixtures that a test may edit.
fn copied_sources() -> tempfile::TempDir {
    let copy = tempfile::tempdir().unwrap();
    for name in ["anthropic.md", "openai.md", "litellm.json"] {
        fs::copy(sources().join(name), copy.path().join(name)).unwrap();
    }
    copy
}

fn priced(id: &str, input: f64) -> Fetched {
    Fetched {
        id: id.to_owned(),
        per_million: CategoryPrices {
            uncached_input: Some(input),
            output: Some(input * 5.0),
            ..CategoryPrices::default()
        },
        long_context: None,
        source: "https://example.com/pricing.md".to_owned(),
    }
}

#[test]
fn anthropic_prompt_size_pair_is_one_row_with_a_long_context_term() {
    // Claim: Haiku 5.5's "up to" and "over 100,000 tokens" rows become one
    // row whose long-context term starts above 100000 prompt tokens counted
    // over uncached input, cache reads and cache writes. Rejects two rows for
    // one id, a term counting fewer categories, and the over row's prices as
    // the base rate.
    let rows = parse_anthropic(&source("anthropic.md"), ANTHROPIC.url).unwrap();
    let haiku: Vec<_> = rows
        .iter()
        .filter(|row| row.id == "claude-haiku-5-5")
        .collect();
    assert_eq!(haiku.len(), 1);
    let haiku = haiku[0];
    assert_eq!(haiku.per_million.uncached_input, Some(0.10));
    assert_eq!(haiku.per_million.cache_write_5m, Some(0.125));
    assert_eq!(haiku.per_million.cache_write_1h, Some(0.20));
    assert_eq!(haiku.per_million.cache_read, Some(0.01));
    assert_eq!(haiku.per_million.output, Some(0.50));
    let term = haiku.long_context.as_ref().expect("a long-context term");
    assert_eq!(term.prompt_tokens, 100_000);
    assert!(!term.inclusive);
    assert_eq!(
        term.counts,
        vec![
            PromptCategory::UncachedInput,
            PromptCategory::CacheRead,
            PromptCategory::CacheWrite
        ]
    );
    assert_eq!(term.billing, vec![BillingKind::Api]);
    let long = term.per_million.as_ref().unwrap();
    assert_eq!(long.uncached_input, Some(0.50));
    assert_eq!(long.output, Some(2.50));

    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "claude-mythos-5-1",
            "claude-opus-5-5",
            "claude-opus-4-1",
            "claude-sonnet-4-5",
            "claude-haiku-5-5",
            "claude-haiku-4-5"
        ]
    );
    let opus = rows.iter().find(|row| row.id == "claude-opus-5-5").unwrap();
    assert_eq!(opus.per_million.cache_read, Some(0.20), "<sup> is stripped");
    assert!(opus.long_context.is_none());
}

#[test]
fn anthropic_pair_missing_a_half_or_an_unknown_parenthetical_fails() {
    // Claim: a prompt-size pair missing its other half, or a parenthetical
    // tapes does not read, fails the source. Rejects a lone half priced as
    // a plain row.
    let page = source("anthropic.md");
    let lone: String = page
        .lines()
        .filter(|line| !line.contains("for prompts over"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(parse_anthropic(&lone, ANTHROPIC.url).is_err());
    let odd = page.replace("Claude Haiku 4.5 ", "Claude Haiku 4.5 (preview)");
    assert!(parse_anthropic(&odd, ANTHROPIC.url).is_err());
}

#[test]
fn openai_long_context_starts_above_272k_counting_input_and_cache_reads() {
    // Claim: an OpenAI row with long-context columns carries a term above
    // 272000 prompt tokens counted over uncached input and cache reads, with
    // its 30m and 12h writes unpriced and named in its source. Rejects a
    // term counting cache writes, a threshold other than the page's, and a
    // row priced from a Batch table.
    let rows = parse_openai(&source("openai.md"), OPENAI.url).unwrap();
    let luna = rows.iter().find(|row| row.id == "gpt-6-luna").unwrap();
    assert_eq!(luna.per_million.uncached_input, Some(0.10));
    assert_eq!(luna.per_million.output, Some(0.50));
    assert_eq!(luna.per_million.cache_read, Some(0.01));
    assert_eq!(luna.per_million.cache_write_5m, None);
    assert_eq!(luna.per_million.cache_write_1h, None);
    assert!(luna.source.contains("30m and 12h cache writes"));
    let term = luna.long_context.as_ref().expect("a long-context term");
    assert_eq!(term.prompt_tokens, 272_000);
    assert_eq!(
        term.counts,
        vec![PromptCategory::UncachedInput, PromptCategory::CacheRead]
    );
    assert_eq!(term.billing, vec![BillingKind::Api]);
    assert_eq!(term.per_million.as_ref().unwrap().output, Some(0.75));

    let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-4o",
            "o3",
            "babbage-002",
            "gpt-5.6-cyber"
        ],
        "one row per model, the context-length note stripped, other headings ignored"
    );
    let cyber = rows.iter().find(|row| row.id == "gpt-5.6-cyber").unwrap();
    assert!(cyber.long_context.is_none(), "all-`-` long columns");
    let pro = rows.iter().find(|row| row.id == "gpt-5.5-pro").unwrap();
    assert_eq!(pro.per_million.cache_read, None);
}

#[test]
fn openai_without_the_272k_sentence_or_with_a_conflicting_price_fails() {
    // Claim: a page that does not say where long context starts, or that
    // prices one model differently in two tables, fails the source. Rejects
    // a guessed threshold and a silently chosen price.
    let page = source("openai.md");
    let unsaid = page.replace(
        "Short context: ≤272K input tokens. Long context: >272K input tokens.",
        "",
    );
    let error = parse_openai(&unsaid, OPENAI.url).unwrap_err();
    assert!(format!("{error:#}").contains("long context starts"));
    let conflicting = page.replacen(
        "| gpt-5.6-sol | $4.00 | $20.00 | $0.40",
        "| gpt-5.6-sol | $4.50 | $20.00 | $0.40",
        1,
    );
    assert!(parse_openai(&conflicting, OPENAI.url).is_err());
}

#[test]
fn litellm_takes_untiered_chat_models_no_official_source_priced() {
    // Claim: LiteLLM prices bare chat ids without a tiered `_above_` price,
    // per token times a million, and never an id an official source priced.
    // Rejects provider-prefixed ids, embeddings, base rates for tiered
    // models, and LiteLLM overriding a vendor page.
    let official: BTreeSet<String> = ["claude-haiku-4-5".to_owned()].into();
    let rows = parse_litellm(&source("litellm.json"), &official).unwrap();
    let ids: BTreeSet<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        ["claude-sonnet-4-5-20250929", "deepseek-chat", "gpt-4o"].into()
    );
    let sonnet = rows
        .iter()
        .find(|row| row.id == "claude-sonnet-4-5-20250929")
        .unwrap();
    assert_eq!(
        sonnet.per_million,
        CategoryPrices {
            uncached_input: Some(3.0),
            cache_read: Some(0.3),
            cache_write_5m: Some(3.75),
            cache_write_1h: Some(6.0),
            output: Some(15.0),
        }
    );
    assert_eq!(sonnet.source, "LiteLLM");
}

#[test]
fn history_keeps_unchanged_rows_splits_changed_ones_and_opens_new_ids_backwards() {
    // Claim: an unchanged price keeps its one row; a changed price closes
    // the row in effect at the update instant and opens a new one there; a
    // new id's row has no start, so it prices earlier sessions. Rejects a
    // split that drops the old row, a duplicate row for an unchanged price,
    // and a new id that only prices sessions after the fetch.
    let (rows, first) = merge(&[], &[priced("a", 1.0), priced("b", 2.0)], at(1));
    assert_eq!(first.added, 2);
    assert!(rows.iter().all(|row| row.effective_from.is_none()));

    let (rows, second) = merge(&rows, &[priced("a", 1.0), priced("b", 3.0)], at(10));
    assert_eq!((second.unchanged, second.changed), (1, 1));
    let a: Vec<_> = rows.iter().filter(|row| row.id == "a").collect();
    assert_eq!(a.len(), 1);
    let b: Vec<_> = rows.iter().filter(|row| row.id == "b").collect();
    assert_eq!(b.len(), 2);
    assert_eq!(b[0].per_million.uncached_input, Some(2.0));
    assert_eq!(b[0].effective_from, None);
    assert_eq!(b[0].effective_until, Some(at(10)));
    assert_eq!(b[1].per_million.uncached_input, Some(3.0));
    assert_eq!(b[1].effective_from, Some(at(10)));
    assert_eq!(b[1].effective_until, None);

    let text = PriceTable::to_toml("USD", &rows).unwrap();
    let table = PriceTable::parse(&text).unwrap();
    let in_effect = |id: &str, when: DateTime<Utc>| {
        table
            .rows(id)
            .find(|row| {
                row.effective_from.is_none_or(|from| from <= when)
                    && row.effective_until.is_none_or(|until| when < until)
            })
            .and_then(|row| row.per_million.uncached_input)
    };
    assert_eq!(in_effect("b", at(5)), Some(2.0));
    assert_eq!(in_effect("b", at(15)), Some(3.0));

    let (again, third) = merge(&rows, &[priced("b", 3.0)], at(20));
    assert_eq!(third.unchanged, 1);
    assert_eq!(again, rows, "a missing id keeps its rows; nothing splits");
}

#[test]
fn a_dated_id_matches_the_row_named_without_its_date() {
    // Claim: a recorded `claude-sonnet-4-5-20250929` that no row names is
    // priced by the `claude-sonnet-4-5` row, while an exact row wins and a
    // suffix that is not eight digits matches nothing. Rejects dated ids
    // going unpriced and the stripped row shadowing an exact one.
    let (rows, _) = merge(
        &[],
        &[
            priced("claude-sonnet-4-5", 3.0),
            priced("claude-opus-4-1", 15.0),
            priced("claude-opus-4-1-20250805", 14.0),
        ],
        at(1),
    );
    let table = PriceTable::parse(&PriceTable::to_toml("USD", &rows).unwrap()).unwrap();
    let price = |id: &str| table.price(id).map(|row| row.per_million.uncached_input);
    assert_eq!(price("claude-sonnet-4-5-20250929"), Some(Some(3.0)));
    assert_eq!(price("claude-opus-4-1-20250805"), Some(Some(14.0)));
    assert_eq!(price("claude-sonnet-4-5-2025092"), None);
    assert_eq!(price("claude-sonnet-4-5-latest"), None);
}

#[test]
fn a_written_table_round_trips_through_parse() {
    // Claim: the table tapes writes is a `tapes-price-table/2` document that
    // `PriceTable::parse` reads back to the same rows, long-context terms and
    // validity windows included. Rejects writing `LongContext`'s own
    // serialization, which the parser refuses.
    let mut fetched = parse_anthropic(&source("anthropic.md"), ANTHROPIC.url).unwrap();
    fetched.extend(parse_openai(&source("openai.md"), OPENAI.url).unwrap());
    let (rows, _) = merge(&[], &fetched, at(1));
    let mut changed = fetched.clone();
    changed[0].per_million.output = Some(99.0);
    let (rows, _) = merge(&rows, &changed, at(10));
    let text = PriceTable::to_toml("USD", &rows).unwrap();
    assert!(text.contains("schema = \"tapes-price-table/2\""));
    let table = PriceTable::parse(&text).unwrap();
    assert_eq!(table.models, rows);
    assert_eq!(PriceTable::to_toml("USD", &table.models).unwrap(), text);
}

#[test]
fn a_changed_header_fails_its_source_keeps_its_rows_and_the_others_update() {
    // Claim: a source whose table header changed contributes nothing, keeps
    // the rows it gave earlier, and is reported failed, while the other
    // sources still update. Rejects a failed source wiping its rows and one
    // failure stopping every source.
    let copy = copied_sources();
    let data = tempfile::tempdir().unwrap();
    let first = price_update::update(data.path(), Some(copy.path()), at(1)).unwrap();
    assert!(!first.failed(), "{:?}", first.sources);

    let page = fs::read_to_string(copy.path().join("anthropic.md")).unwrap();
    fs::write(
        copy.path().join("anthropic.md"),
        page.replace("| Output tokens ", "| Output tokens (base) "),
    )
    .unwrap();
    let openai = fs::read_to_string(copy.path().join("openai.md")).unwrap();
    fs::write(
        copy.path().join("openai.md"),
        openai.replace("| o3 | $2.00 |", "| o3 | $1.00 |"),
    )
    .unwrap();
    let second = price_update::update(data.path(), Some(copy.path()), at(10)).unwrap();
    assert!(second.failed());
    let anthropic = &second.sources[0];
    assert_eq!(anthropic.name, "anthropic");
    assert!(!anthropic.ok);
    assert!(anthropic.error.as_deref().unwrap().contains("header"));
    assert!(second.sources[1].ok && second.sources[2].ok);

    let table = PriceTable::read(&data.path().join("prices.toml")).unwrap();
    assert_eq!(table.rows("claude-haiku-5-5").count(), 1);
    assert_eq!(table.rows("o3").count(), 2, "OpenAI still updated");
    let record = FetchRecord::read(&data.path().join("prices-fetch.json")).unwrap();
    assert_eq!(record.attempted_at, at(10));
    let kept = &record.sources["anthropic"];
    assert!(!kept.ok);
    assert_eq!(kept.fetched_at, Some(at(1)), "its last success is kept");
    assert_eq!(record.sources["openai"].fetched_at, Some(at(10)));
}

#[test]
fn litellm_never_prices_a_dated_form_of_an_official_id() {
    // Claim: LiteLLM's `claude-sonnet-4-5-20250929` is skipped when an
    // official source priced `claude-sonnet-4-5`, so the dated id falls back
    // to the official row and follows its price changes. Rejects a LiteLLM
    // row shadowing the official one for the dated id.
    let official: BTreeSet<String> = ["claude-sonnet-4-5".to_owned()].into();
    let rows = parse_litellm(&source("litellm.json"), &official).unwrap();
    assert!(rows
        .iter()
        .all(|row| row.id != "claude-sonnet-4-5-20250929"));
}
