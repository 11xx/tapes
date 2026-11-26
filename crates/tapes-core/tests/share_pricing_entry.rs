//! A caller holding one model share's disjoint categories, with the reasons
//! each carries, prices it through `PricedShare` exactly as tapes prices
//! that share inside a whole figure: reasons included, request by request
//! under a row with terms, and with a category the share left undetermined
//! keeping that reason rather than gaining `missing-counter`.

use agent_tapes_core::measurement::{MeasurementCompleteness, MeasurementReason};
use agent_tapes_core::model::{
    DisjointTokens, Model, ModelObservationStatus, ModelSelectionRecord, ModelSelectionSpan,
    TokenConvention, Tokens,
};
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

/// Claim: variant-qualified price rows take precedence (`<id>-<variant>` then
/// `<id>#<variant>`) over base `<id>` rows, and a figure with multiple
/// variants of one base model prices each variant share at its own row when
/// variant-specific rows exist, while falling back to the base row otherwise.
#[test]
fn variant_qualified_price_rows_take_precedence_and_fall_back_to_base() {
    let table = PriceTable::parse(
        r#"
schema = "tapes-price-table/1"
currency = "USD"

[[models]]
id = "gemini-fixture"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.1, cache_write_5m = 1.0, output = 2.0 }

[[models]]
id = "gemini-fixture-high"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 3.0, cache_read = 0.3, cache_write_5m = 3.0, output = 6.0 }

[[models]]
id = "gemini-fixture#high"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 9.0, cache_read = 0.9, cache_write_5m = 9.0, output = 18.0 }

[[models]]
id = "gemini-fixture#max"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 5.0, cache_read = 0.5, cache_write_5m = 5.0, output = 10.0 }
"#,
    )
    .unwrap();
    let convention = TokenConvention::of("antigravity").unwrap();
    let disjoint = DisjointTokens::from_native(&tokens(1_000_000, 0, 0), convention, None);

    let price_variant = |variant: Option<&str>| {
        PricedShare {
            disjoint: &disjoint,
            model: "gemini-fixture",
            variant,
            sole: false,
            terms: RequestTerms::default(),
        }
        .estimate(&table)
        .finish()
        .unwrap()
    };

    // `<id>-<variant>` beats `<id>#<variant>` and `<id>`.
    let high = price_variant(Some("high"));
    assert!((high.amount.unwrap() - 3.0).abs() < 1e-12);
    assert_eq!(high.models[0].id, "gemini-fixture-high");

    // `<id>#<variant>` beats `<id>` when `<id>-<variant>` is absent.
    let max = price_variant(Some("max"));
    assert!((max.amount.unwrap() - 5.0).abs() < 1e-12);
    assert_eq!(max.models[0].id, "gemini-fixture#max");

    // Unlisted variant falls back to base `<id>`.
    let low = price_variant(Some("low"));
    assert!((low.amount.unwrap() - 1.0).abs() < 1e-12);
    assert_eq!(low.models[0].id, "gemini-fixture");

    // Multi-variant figure over one base model sums distinct variant rates instead
    // of collapsing into a single base sole-model estimate.
    let mut high_share = share("gemini-fixture", tokens(1_000_000, 0, 0));
    high_share.variant = Some("high".to_owned());
    let mut low_share = share("gemini-fixture", tokens(1_000_000, 0, 0));
    low_share.variant = Some("low".to_owned());
    let shares = [high_share, low_share];
    let whole_disjoint = DisjointTokens::from_native(&tokens(2_000_000, 0, 0), convention, None);
    let rec = ModelSelectionRecord {
        timestamp: None,
        native_id: None,
    };
    let selections = [
        ModelSelectionSpan {
            model: Model {
                id: "gemini-fixture".to_owned(),
                variant: Some("high".to_owned()),
            },
            first: rec.clone(),
            last: rec.clone(),
        },
        ModelSelectionSpan {
            model: Model {
                id: "gemini-fixture".to_owned(),
                variant: Some("low".to_owned()),
            },
            first: rec.clone(),
            last: rec,
        },
    ];
    let figure = PricedFigure {
        disjoint: Some(&whole_disjoint),
        convention: Some(convention),
        model: Some(&selections[1].model),
        selections: &selections,
        observation: Some(&ModelObservationStatus {
            mixed: true,
            head_read: true,
            attribution_uncertain: false,
            distinct_observed: Some(2),
        }),
        by_model: Some(&shares),
        terms: RequestTerms::default(),
    }
    .estimate(&table);
    let whole = figure.whole.and_then(|sum| sum.finish()).unwrap();
    assert!((whole.amount.unwrap() - 4.0).abs() < 1e-12);
}

/// Claim: a figure with an explicit empty model split (`by_model: Some(&[])`)
/// and uncertain attribution withholds its estimate even when the surrounding
/// session's selections name a single base model.
#[test]
fn empty_model_split_under_uncertain_attribution_withholds_sole_model_fallback() {
    let table = PriceTable::parse(PLAIN_TABLE).unwrap();
    let convention = TokenConvention::of("antigravity").unwrap();
    let disjoint = DisjointTokens::from_native(&tokens(100, 5_000, 50), convention, None);
    let model = Model {
        id: "gpt-a-fixture".to_owned(),
        variant: Some("high".to_owned()),
    };
    let rec = ModelSelectionRecord {
        timestamp: None,
        native_id: None,
    };
    let selections = [ModelSelectionSpan {
        model: model.clone(),
        first: rec.clone(),
        last: rec,
    }];
    let figure = PricedFigure {
        disjoint: Some(&disjoint),
        convention: Some(convention),
        model: Some(&model),
        selections: &selections,
        observation: Some(&ModelObservationStatus {
            mixed: false,
            head_read: true,
            attribution_uncertain: true,
            distinct_observed: Some(1),
        }),
        by_model: Some(&[]),
        terms: RequestTerms::default(),
    }
    .estimate(&table);
    assert!(figure.whole.is_none());
}

/// Claim: a bare model share (`variant: None`) in a non-sole figure matches
/// only requests with `variant: None`, never consuming effort-variant requests
/// of the same base ID when pricing dated `/2` rows, while a sole-model figure
/// without variant-specific rows continues to match all variants of that ID.
#[test]
fn bare_model_share_does_not_consume_variant_requests_under_dated_rows() {
    let table = PriceTable::parse(
        r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "review-model"
effective = 2026-01-01
effective_until = 2026-01-02
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.0, cache_write_5m = 0.0, output = 0.0 }

[[models]]
id = "review-model"
effective = 2026-01-02
effective_from = 2026-01-02
source = "fixture"
per_million = { uncached_input = 9.0, cache_read = 0.0, cache_write_5m = 0.0, output = 0.0 }

[[models]]
id = "review-model-high"
effective = 2026-01-01
source = "fixture"
per_million = { uncached_input = 3.0, cache_read = 0.0, cache_write_5m = 0.0, output = 0.0 }
"#,
    )
    .unwrap();
    let convention = TokenConvention::of("antigravity").unwrap();
    let bare_model = Model {
        id: "review-model".to_owned(),
        variant: None,
    };
    let high_model = Model {
        id: "review-model".to_owned(),
        variant: Some("high".to_owned()),
    };
    let req_disjoint = DisjointTokens::from_native(&tokens(100, 0, 0), convention, None);
    let mut requests = RequestUses::default();
    requests.push(
        Some(at("2026-01-01T12:00:00Z")),
        Some(&bare_model),
        &req_disjoint,
    );
    requests.push(
        Some(at("2026-01-02T12:00:00Z")),
        Some(&high_model),
        &req_disjoint,
    );
    let terms = RequestTerms {
        requests: Some(&requests),
        billing: None,
        from: Some(at("2026-01-01T12:00:00Z")),
        to: Some(at("2026-01-02T12:00:00Z")),
    };

    let bare_share = share("review-model", tokens(100, 0, 0));
    let mut high_share = share("review-model", tokens(100, 0, 0));
    high_share.variant = Some("high".to_owned());
    let shares = [bare_share, high_share];
    let whole_disjoint = DisjointTokens::from_native(&tokens(200, 0, 0), convention, None);
    let rec = ModelSelectionRecord {
        timestamp: None,
        native_id: None,
    };
    let selections = [
        ModelSelectionSpan {
            model: bare_model,
            first: rec.clone(),
            last: rec.clone(),
        },
        ModelSelectionSpan {
            model: high_model.clone(),
            first: rec.clone(),
            last: rec,
        },
    ];

    let figure = PricedFigure {
        disjoint: Some(&whole_disjoint),
        convention: Some(convention),
        model: Some(&high_model),
        selections: &selections,
        observation: Some(&ModelObservationStatus {
            mixed: true,
            head_read: true,
            attribution_uncertain: false,
            distinct_observed: Some(2),
        }),
        by_model: Some(&shares),
        terms,
    }
    .estimate(&table);

    let bare_est = figure.by_model[0].as_ref().unwrap();
    assert!((bare_est.amount.unwrap() - 0.0001).abs() < 1e-12);
    assert!(
        bare_est.is_complete(),
        "{:?}",
        bare_est.measurement_completeness
    );

    let high_est = figure.by_model[1].as_ref().unwrap();
    assert!((high_est.amount.unwrap() - 0.0003).abs() < 1e-12);
    assert!(
        high_est.is_complete(),
        "{:?}",
        high_est.measurement_completeness
    );

    let whole = figure.whole.and_then(|sum| sum.finish()).unwrap();
    assert!((whole.amount.unwrap() - 0.0004).abs() < 1e-12);
    assert!(whole.is_complete(), "{:?}", whole.measurement_completeness);

    // Sole-model figure over a base-only `/2` table continues to match both
    // bare and variant requests of that base ID.
    let base_only = PriceTable::parse(
        r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "review-model"
effective = 2026-01-01
effective_until = 2026-01-02
source = "fixture"
per_million = { uncached_input = 1.0, cache_read = 0.0, cache_write_5m = 0.0, output = 0.0 }

[[models]]
id = "review-model"
effective = 2026-01-02
effective_from = 2026-01-02
source = "fixture"
per_million = { uncached_input = 9.0, cache_read = 0.0, cache_write_5m = 0.0, output = 0.0 }
"#,
    )
    .unwrap();
    let sole_figure = PricedFigure {
        disjoint: Some(&whole_disjoint),
        convention: Some(convention),
        model: Some(&high_model),
        selections: &selections,
        observation: Some(&ModelObservationStatus {
            mixed: true,
            head_read: true,
            attribution_uncertain: false,
            distinct_observed: Some(2),
        }),
        by_model: None,
        terms,
    }
    .estimate(&base_only);
    let sole_whole = sole_figure.whole.and_then(|sum| sum.finish()).unwrap();
    // 100 * 1.0 / 1e6 (Jan 1) + 100 * 9.0 / 1e6 (Jan 2) = 0.0010 USD.
    assert!((sole_whole.amount.unwrap() - 0.0010).abs() < 1e-12);
    assert!(
        sole_whole.is_complete(),
        "{:?}",
        sole_whole.measurement_completeness
    );
}
