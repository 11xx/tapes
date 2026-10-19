//! Whether a measured figure is complete, counter by counter, and if not,
//! why. Every figure derived from a measurement — disjoint categories and
//! their total, summary sums and identity bounds, model splits, ratios —
//! asks this rather than the facts behind it, so a recorded reason reaches
//! every figure that depends on the counter it qualifies. A qualified
//! counter's amount, where present, is a known subtotal.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{
    AccountingCoverage, Cost, CostCoverage, InputConvention, OutputConvention, TokenConvention,
    Tokens,
};
use crate::usage::MeasurementCoverage;

/// The native counters a measurement carries, its harness-written cost
/// included.
pub const COUNTERS: [&str; 6] = [
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];

/// The disjoint categories, which add across harnesses.
pub const CATEGORIES: [&str; 5] = [
    "uncached_input",
    "cache_read",
    "cache_write",
    "output",
    "reasoning",
];

/// Why a counter or category is not completely measured. The set is fixed;
/// a figure's notes say the same in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeasurementReason {
    /// Some counted events did not record the counter, or the figure has no
    /// amount for it.
    MissingCounter,
    /// Some counted events, or the figure, recorded no price.
    MissingPrice,
    /// A written zero price beside nonzero usage.
    Unpriced,
    /// A request stopped on an error or an abort, or failed; its consumption
    /// is partial or unknown.
    StoppedAttempt,
    /// Records the read could not decode may have carried consumption.
    UnreadableRecord,
    /// A record the reader has no reading for carried usage-shaped fields, so
    /// consumption it witnessed is in no counter.
    UnrecognizedUsage,
    /// The figure covers only the part of the recording the read reached.
    ReadWindow,
    /// The counters restarted inside the recording; the figure covers what
    /// followed the restart.
    SinceReset,
    /// No accounting establishes what the figure covers.
    UnestablishedScope,
    /// The read could not attribute every counted request to a model, so no
    /// model's share is complete.
    IncompleteAttribution,
    /// The harness's convention does not determine the category from the
    /// counters it recorded.
    Undetermined,
    /// The price table has no row for the model that served the usage.
    UnlistedModel,
    /// Cache writes recorded no TTL, so their price lies between the
    /// 5-minute and the 1-hour price.
    TtlUnrecorded,
    /// A price-table term priced by the request, by its prompt size or its
    /// time, may apply to usage whose requests the figure does not hold one
    /// by one, so its price lies between the term's rates.
    RequestTermsUnknown,
    /// A long-context surcharge applies to per-token billing alone, and the
    /// recording establishes neither per-token nor subscription billing, so
    /// its price lies between the base and the surcharged rates.
    BillingUnestablished,
    /// No row of the model's prices is in effect at the request's time.
    OutsideValidity,
}

impl MeasurementReason {
    /// Whether the reason describes the read, or records it could place on
    /// no model, rather than the events one model served. Such a reason on a
    /// figure qualifies every share of it. A stopped attempt is not one: a
    /// reader places it on the model it names, and one naming no model is
    /// counted as a failure every share carries.
    pub fn qualifies_every_share(self) -> bool {
        matches!(
            self,
            Self::UnreadableRecord
                | Self::UnrecognizedUsage
                | Self::ReadWindow
                | Self::SinceReset
                | Self::UnestablishedScope
                | Self::IncompleteAttribution
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MissingCounter => "missing-counter",
            Self::MissingPrice => "missing-price",
            Self::Unpriced => "unpriced",
            Self::StoppedAttempt => "stopped-attempt",
            Self::UnreadableRecord => "unreadable-record",
            Self::UnrecognizedUsage => "unrecognized-usage",
            Self::ReadWindow => "read-window",
            Self::SinceReset => "since-reset",
            Self::UnestablishedScope => "unestablished-scope",
            Self::IncompleteAttribution => "incomplete-attribution",
            Self::Undetermined => "undetermined",
            Self::UnlistedModel => "unlisted-model",
            Self::TtlUnrecorded => "ttl-unrecorded",
            Self::RequestTermsUnknown => "request-terms-unknown",
            Self::BillingUnestablished => "billing-unestablished",
            Self::OutsideValidity => "outside-validity",
        }
    }
}

/// The incompletely measured counters of one figure, each with its reasons.
/// A counter absent here is completely measured.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MeasurementCompleteness(BTreeMap<String, BTreeSet<MeasurementReason>>);

/// One note per counter whose events did not all record it, saying how many
/// did.
pub fn known_subtotal_notes(coverage: &BTreeMap<String, MeasurementCoverage>) -> Vec<String> {
    coverage
        .iter()
        .filter(|(_, coverage)| coverage.missing > 0 || coverage.unpriced > 0)
        .map(|(counter, coverage)| {
            let events = coverage.measured + coverage.missing + coverage.unpriced;
            if coverage.measured == 0 {
                format!("{counter} was recorded by 0 of {events} events")
            } else {
                format!(
                    "{counter} is a known subtotal: {}/{events} events measured, {} missing, {} unpriced",
                    coverage.measured, coverage.missing, coverage.unpriced
                )
            }
        })
        .collect()
}

/// The facts a figure's own completeness follows from.
#[derive(Clone, Copy, Default)]
pub struct Measured<'a> {
    pub tokens: Option<&'a Tokens>,
    pub cost: Option<&'a Cost>,
    pub cost_coverage: Option<&'a CostCoverage>,
    /// Per-counter coverage among the events the figure sums, where the
    /// reader counted them.
    pub coverage: Option<&'a BTreeMap<String, MeasurementCoverage>>,
    /// How much of its session the figure covers; absent, unestablished.
    pub scope: Option<AccountingCoverage>,
    /// How many events the figure counted, so a figure that counted some and
    /// measured nothing still qualifies every counter. Nonzero coverage also
    /// shows counted events, for a figure that keeps no separate count.
    pub events: usize,
}

impl MeasurementCompleteness {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether every one of `counters` is completely measured.
    pub fn is_complete(&self, counters: &[&str]) -> bool {
        counters
            .iter()
            .all(|counter| !self.0.contains_key(*counter))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &BTreeSet<MeasurementReason>)> {
        self.0
            .iter()
            .map(|(counter, reasons)| (counter.as_str(), reasons))
    }

    pub fn qualify(&mut self, counter: &str, reason: MeasurementReason) {
        self.0.entry(counter.to_owned()).or_default().insert(reason);
    }

    /// Qualify every native counter, for a reason no single counter carries.
    pub fn qualify_all(&mut self, reason: MeasurementReason) {
        for counter in COUNTERS {
            self.qualify(counter, reason);
        }
    }

    /// The reasons `keep` admits, counter by counter.
    pub fn retaining(&self, keep: impl Fn(MeasurementReason) -> bool) -> Self {
        Self(
            self.0
                .iter()
                .filter_map(|(counter, reasons)| {
                    let kept = reasons
                        .iter()
                        .copied()
                        .filter(|reason| keep(*reason))
                        .collect::<BTreeSet<_>>();
                    (!kept.is_empty()).then(|| (counter.clone(), kept))
                })
                .collect(),
        )
    }

    pub fn merge(&mut self, other: &Self) {
        for (counter, reasons) in &other.0 {
            self.0
                .entry(counter.clone())
                .or_default()
                .extend(reasons.iter().copied());
        }
    }

    /// What a figure's own facts establish. A figure that counted no event
    /// and measured nothing has no counter to qualify; a summary qualifies
    /// its absence with `include_absent`. A figure that counted events
    /// carries every reason that applies to it, its scope, pricing and
    /// coverage among them, whether or not it measured an amount.
    pub fn assess(figure: Measured<'_>) -> Self {
        let mut result = Self::default();
        let counted = figure.events > 0
            || figure
                .coverage
                .is_some_and(MeasurementCoverage::counted_any);
        if figure.tokens.is_none() && figure.cost.is_none() && !counted {
            return result;
        }
        result.include_absent(figure.tokens, figure.cost);
        if let Some(pricing) = figure.cost_coverage.filter(|pricing| !pricing.complete) {
            let zero_written =
                pricing.zero_written_observations > 0 || pricing.zero_written_requests > 0;
            result.qualify(
                "cost",
                if zero_written {
                    MeasurementReason::Unpriced
                } else {
                    MeasurementReason::MissingPrice
                },
            );
        }
        for (counter, measured) in figure.coverage.into_iter().flatten() {
            if measured.missing > 0 {
                result.qualify(
                    counter,
                    if counter == "cost" {
                        MeasurementReason::MissingPrice
                    } else {
                        MeasurementReason::MissingCounter
                    },
                );
            }
            if measured.unpriced > 0 {
                result.qualify(counter, MeasurementReason::Unpriced);
            }
        }
        match figure.scope {
            Some(AccountingCoverage::Session) => {}
            Some(AccountingCoverage::ReadWindow) => {
                result.qualify_all(MeasurementReason::ReadWindow);
            }
            Some(AccountingCoverage::SinceReset) => {
                result.qualify_all(MeasurementReason::SinceReset);
            }
            None => result.qualify_all(MeasurementReason::UnestablishedScope),
        }
        result
    }

    /// Qualify each counter the figure holds no amount for.
    pub fn include_absent(&mut self, tokens: Option<&Tokens>, cost: Option<&Cost>) {
        for (counter, value) in [
            ("input", tokens.and_then(|tokens| tokens.input)),
            ("output", tokens.and_then(|tokens| tokens.output)),
            ("reasoning", tokens.and_then(|tokens| tokens.reasoning)),
            ("cache_read", tokens.and_then(|tokens| tokens.cache_read)),
            ("cache_write", tokens.and_then(|tokens| tokens.cache_write)),
        ] {
            if value.is_none() {
                self.qualify(counter, MeasurementReason::MissingCounter);
            }
        }
        if cost.is_none() {
            self.qualify("cost", MeasurementReason::MissingPrice);
        }
    }

    /// The same completeness by disjoint category under `convention`. A
    /// category carries the reasons of every native counter it is derived
    /// from.
    pub fn disjoint(&self, convention: &TokenConvention) -> Self {
        let uncached_input: &[&str] = match convention.input {
            InputConvention::ExcludesCache => &["input"],
            InputConvention::IncludesCacheRead => &["input", "cache_read", "cache_write"],
        };
        let output: &[&str] = match convention.output {
            OutputConvention::IncludesReasoning => &["output"],
            OutputConvention::ExcludesReasoning => &["output", "reasoning"],
        };
        let mut result = Self::default();
        for (category, dependencies) in [
            ("uncached_input", uncached_input),
            ("cache_read", &["cache_read"][..]),
            ("cache_write", &["cache_write"][..]),
            ("output", output),
            ("reasoning", &["reasoning"][..]),
        ] {
            for counter in dependencies {
                for reason in self.0.get(*counter).into_iter().flatten() {
                    result.qualify(category, *reason);
                }
            }
        }
        result
    }
}
