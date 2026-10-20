//! A caller holding one model share's disjoint categories, with the reasons
//! each carries, prices it through `PricedShare` exactly as tapes prices
//! that share inside a whole figure: reasons included, request by request
//! under a row with terms, and with a category the share left undetermined
//! keeping that reason rather than gaining `missing-counter`.

use agent_tapes_core::measurement::{MeasurementCompleteness, MeasurementReason};
use agent_tapes_core::model::{DisjointTokens, Model, TokenConvention, Tokens};
use agent_tapes_core::pricing::{
    Billing, ListPriceEstimate, PriceTable, PricedFigure, PricedShare, RequestTerms, RequestUses,
};
use agent_tapes_core::usage::ModelUsage;
use chrono::{DateTime, Utc};

const PLAIN_TABLE: &str = r#"
schema = "tapes-price-table/1"
currency = "USD"

[[models]]
id = "gpt-a-fixture"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.5, output = 10.0 }

[[models]]
id = "gpt-b-fixture"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.25, output = 5.0 }
"#;

const TERMS_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "gpt-a-fixture"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.5, output = 10.0 }

[models.long_context]
above_prompt_tokens = 272000
prompt_tokens = ["uncached_input", "cache_read", "cache_write"]
billing = ["api"]
multipliers = { uncached_input = 2.0, cache_read = 2.0, cache_write_5m = 2.0, output = 1.5 }

[[models]]
id = "gpt-b-fixture"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.25, output = 5.0 }
"#;

fn tokens(input: u64, cache_read: u64, output: u64) -> Tokens {
    Tokens {
        input: Some(input),
        output: Some(output),
        reasoning: None,
        cache_read: Some(cache_read),
        cache_write: Some(0),
    }
}

fn share(model: &str, tokens: Tokens) -> ModelUsage {
    ModelUsage {
        model: model.to_owned(),
        variant: None,
        tokens: Some(tokens),
        cost: None,
        cost_coverage: None,
        request_count: None,
        measurement_coverage: Default::default(),
        measurement_incomplete: Vec::new(),
        measurement_completeness: MeasurementCompleteness::default(),
        list_price_estimate: None,
        cache_write_by_ttl: None,
    }
}

/// A share's disjoint categories as tapes derives them inside a figure.
fn disjoint_of(model: &ModelUsage, convention: TokenConvention) -> DisjointTokens {
    DisjointTokens::from_native(model.tokens.as_ref().unwrap(), convention, None)
        .with_completeness(&model.measurement_completeness)
}

/// Each share of a two-model figure as the figure's estimate prices it.
fn figure_shares(
    shares: &[ModelUsage],
    convention: TokenConvention,
    table: &PriceTable,
    terms: RequestTerms<'_>,
) -> Vec<Option<ListPriceEstimate>> {
    let parts = shares
        .iter()
        .map(|model| disjoint_of(model, convention))
        .collect::<Vec<_>>();
    let whole = DisjointTokens::sum(&parts).unwrap();
    PricedFigure {
        disjoint: Some(&whole),
        convention: Some(convention),
        model: None,
        selections: &[],
        observation: None,
        by_model: Some(shares),
        terms,
    }
    .estimate(table)
    .by_model
}

fn priced(
    disjoint: &DisjointTokens,
    model: &str,
    table: &PriceTable,
    terms: RequestTerms<'_>,
) -> Option<ListPriceEstimate> {
    PricedShare {
        disjoint,
        model,
        variant: None,
        sole: false,
        terms,
    }
    .estimate(table)
    .finish()
}

fn reasons(estimate: &ListPriceEstimate, category: &str) -> Vec<MeasurementReason> {
    estimate
        .measurement_completeness
        .iter()
        .find(|(held, _)| *held == category)
        .map(|(_, reasons)| reasons.iter().copied().collect())
        .unwrap_or_default()
}

fn at(stamp: &str) -> DateTime<Utc> {
    stamp.parse().unwrap()
}

/// Claim: a share whose uncached input its counters leave undetermined is
/// priced with that category `undetermined` and nothing else, as the figure
/// prices the same share. Rejects an entry that restates the share through
/// native counters, where the absent input gains `missing-counter`.
#[test]
fn an_undetermined_category_keeps_the_share_reason_alone() {
    let table = PriceTable::parse(PLAIN_TABLE).unwrap();
    let convention = TokenConvention::of("codex").unwrap();
    // Input counted inside it fewer tokens than the cache reads, which
    // contradicts the convention and determines no uncached input.
    let shares = [
        share("gpt-a-fixture", tokens(100, 200, 50)),
        share("gpt-b-fixture", tokens(1_000, 400, 80)),
    ];
    let undetermined = disjoint_of(&shares[0], convention);
    assert_eq!(undetermined.uncached_input, None);

    let figure = figure_shares(&shares, convention, &table, RequestTerms::default());
    let entry = priced(
        &undetermined,
        "gpt-a-fixture",
        &table,
        RequestTerms::default(),
    )
    .expect("the share's other categories are priced");

    assert_eq!(
        reasons(&entry, "uncached_input"),
        vec![MeasurementReason::Undetermined],
        "an undetermined category keeps the share's reason alone: {:?}",
        entry.measurement_completeness
    );
    assert!(entry
        .measurement_completeness
        .iter()
        .all(|(_, reasons)| !reasons.contains(&MeasurementReason::MissingCounter)));
    assert_eq!(figure[0].as_ref(), Some(&entry));
}

/// Claim: a share the caller holds as disjoint categories carries its own
/// reasons through, so a reason no native counter would show survives.
/// Rejects an entry that rebuilds the share's completeness from counters.
#[test]
fn a_share_keeps_the_reasons_it_carries() {
    let table = PriceTable::parse(PLAIN_TABLE).unwrap();
    let convention = TokenConvention::of("claude").unwrap();
    let mut held = share("gpt-a-fixture", tokens(1_000, 2_000, 300));
    held.measurement_completeness
        .qualify("output", MeasurementReason::IncompleteAttribution);
    let disjoint = disjoint_of(&held, convention);

    let figure = figure_shares(
        &[held, share("gpt-b-fixture", tokens(500, 0, 100))],
        convention,
        &table,
        RequestTerms::default(),
    );
    let entry = priced(&disjoint, "gpt-a-fixture", &table, RequestTerms::default()).unwrap();

    assert_eq!(
        reasons(&entry, "output"),
        vec![MeasurementReason::IncompleteAttribution]
    );
    assert_eq!(figure[0].as_ref(), Some(&entry));
}

/// Claim: a category the share holds no amount for and gives no reason is a
/// `missing-counter`, as a figure's share is. Rejects an entry that prices
/// an absent category as complete.
#[test]
fn an_absent_category_with_no_reason_is_a_missing_counter() {
    let table = PriceTable::parse(PLAIN_TABLE).unwrap();
    let convention = TokenConvention::of("claude").unwrap();
    let mut disjoint = disjoint_of(&share("gpt-a-fixture", tokens(1_000, 0, 300)), convention);
    disjoint.cache_read = None;

    let entry = priced(&disjoint, "gpt-a-fixture", &table, RequestTerms::default()).unwrap();

    assert_eq!(
        reasons(&entry, "cache_read"),
        vec![MeasurementReason::MissingCounter]
    );
}

/// Claim: under a row with a long-context term, the share is priced request
/// by request over its own requests, at each request's rate, exactly as the
/// figure prices that share. Rejects an entry that prices only plain rows,
/// or prices the share whole between the term's rates.
#[test]
fn a_share_under_a_row_with_terms_is_priced_by_its_requests() {
    let table = PriceTable::parse(TERMS_TABLE).unwrap();
    let convention = TokenConvention::of("claude").unwrap();
    let model_a = Model {
        id: "gpt-a-fixture".to_owned(),
        variant: None,
    };
    let model_b = Model {
        id: "gpt-b-fixture".to_owned(),
        variant: None,
    };
    let request = |input, cache_read, output| {
        DisjointTokens::from_native(&tokens(input, cache_read, output), convention, None)
    };
    let mut requests = RequestUses::default();
    // One prompt past the 272K threshold, one below it.
    requests.push(
        Some(at("2026-10-19T10:00:00Z")),
        Some(&model_a),
        &request(100_000, 200_000, 1_000),
    );
    requests.push(
        Some(at("2026-10-19T10:05:00Z")),
        Some(&model_a),
        &request(10_000, 0, 1_000),
    );
    requests.push(
        Some(at("2026-10-19T10:10:00Z")),
        Some(&model_b),
        &request(5_000, 0, 500),
    );
    let billing = Billing::api("fixture");
    let terms = RequestTerms {
        requests: Some(&requests),
        billing: Some(&billing),
        from: Some(at("2026-10-19T10:00:00Z")),
        to: Some(at("2026-10-19T10:10:00Z")),
    };
    let shares = [
        share("gpt-a-fixture", tokens(110_000, 200_000, 2_000)),
        share("gpt-b-fixture", tokens(5_000, 0, 500)),
    ];
    let disjoint = disjoint_of(&shares[0], convention);

    let figure = figure_shares(&shares, convention, &table, terms);
    let entry = priced(&disjoint, "gpt-a-fixture", &table, terms).unwrap();

    // 100K * 4.0 + 200K * 0.4 + 1K * 15.0 past the threshold, and
    // 10K * 2.0 + 1K * 10.0 below it, per million.
    let expected = 0.4 + 0.08 + 0.015 + 0.02 + 0.01;
    let amount = entry.amount.expect("every request's rate is known");
    assert!(
        (amount - expected).abs() < 1e-9,
        "{amount} is not {expected}"
    );
    assert!(entry.is_complete(), "{:?}", entry.measurement_completeness);
    assert_eq!(entry.billing, vec![billing.clone()]);
    assert_eq!(figure[0].as_ref(), Some(&entry));
}

/// Claim: a share under a row with terms whose uncached input it left
/// undetermined keeps that reason alone there too, and is bounded between
/// the term's rates for the requests it cannot hold one by one. Rejects an
/// entry that adds `missing-counter` to the category under terms.
#[test]
fn an_undetermined_category_under_terms_keeps_the_share_reason_alone() {
    let table = PriceTable::parse(TERMS_TABLE).unwrap();
    let convention = TokenConvention::of("codex").unwrap();
    let shares = [
        share("gpt-a-fixture", tokens(100, 200_000, 50)),
        share("gpt-b-fixture", tokens(1_000, 400, 80)),
    ];
    let disjoint = disjoint_of(&shares[0], convention);
    let billing = Billing::api("fixture");
    let terms = RequestTerms {
        requests: None,
        billing: Some(&billing),
        from: Some(at("2026-10-19T10:00:00Z")),
        to: Some(at("2026-10-19T10:10:00Z")),
    };

    let figure = figure_shares(&shares, convention, &table, terms);
    let entry = priced(&disjoint, "gpt-a-fixture", &table, terms).unwrap();

    assert_eq!(
        reasons(&entry, "uncached_input"),
        vec![MeasurementReason::Undetermined],
        "{:?}",
        entry.measurement_completeness
    );
    assert_eq!(figure[0].as_ref(), Some(&entry));
}
