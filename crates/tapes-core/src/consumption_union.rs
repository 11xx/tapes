//! The union of two consumption selections' amounts, computed from their
//! event keys and per-event measurements. A union is exact only when every
//! key of both selections is present and measured, both reads cover their
//! recordings and hold no usage outside the keys, and the selections' overlap is established by verified copy
//! evidence rather than by matching identifiers. Otherwise the exact amounts
//! are withheld, each selection keeps its own figure, and the union is
//! bounded.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::measurement::MeasurementCompleteness;
use crate::model::{
    AccountingCoverage, AccountingSelection, CacheWrites, Cost, CostCoverage, DisjointTokens,
};
use crate::usage::{ConsumptionEventKey, ConsumptionEventKeys, QuantityBounds};

/// Why a union's exact amounts are withheld. The set is fixed and carries no
/// recording locator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnionWithheld {
    /// A selection omitted keys, or its keys were unavailable.
    OmittedKeys,
    /// A selection's read covers only part of its recording, or could not
    /// decode every record.
    IncompleteRead,
    /// A key carries no measurement.
    UnmeasuredEvent,
    /// A selection's read holds records carrying usage that no reading
    /// counts, or did not establish that it holds none, so some of its
    /// consumption has no key.
    UnrecognizedUsage,
    /// The selections are of different accounting bases or token conventions.
    MixedBasis,
    /// The selections cover different branch domains: one the active branch,
    /// the other every retained branch.
    MixedSelection,
    /// Neither selection is established as the other's fork parent, so the
    /// events they share are unknown.
    UnestablishedLineage,
    /// The fork's copies of its parent's events are not all verified.
    UnverifiedCopies,
    /// A key of one selection names a provider response another selection's
    /// key names, and no verified copy establishes them as one event.
    UnestablishedIdentity,
    /// More than two selections are related, and a union is computed for a
    /// parent and one fork.
    MoreThanTwoSelections,
    /// A related selection lies outside the set being summed, so the union
    /// of the members within it is not the union of their events.
    PartialSelection,
}

/// The amounts of a union that is exact in membership. A category or the cost
/// that some event did not record is a known subtotal, named in
/// `measurement_completeness`; `disjoint.total` is present only when every
/// summed category is complete.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsumptionAmounts {
    /// Distinct consumption events the union holds.
    pub events: usize,
    pub disjoint: DisjointTokens,
    /// Known cost subtotal over the events that recorded a price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
}

/// At-least and at-most bounds per comparable quantity over the union of two
/// selections, whatever their overlap. A selection with an omitted key or an
/// unmeasured category leaves the upper bound absent.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ConsumptionUnionBounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncached_input: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<QuantityBounds<f64>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsumptionUnion {
    /// Why the exact amounts are withheld; empty when `amounts` is present.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<UnionWithheld>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amounts: Option<ConsumptionAmounts>,
    pub bounds: ConsumptionUnionBounds,
}

impl ConsumptionEventKeys {
    /// The union of this selection's consumption with `other`'s.
    pub fn union(&self, other: &Self) -> ConsumptionUnion {
        let mut withheld = BTreeSet::new();
        for selection in [self, other] {
            if !selection.is_whole() {
                withheld.insert(UnionWithheld::OmittedKeys);
            }
            if selection.coverage != Some(AccountingCoverage::Session)
                || selection.unreadable_records != Some(0)
            {
                withheld.insert(UnionWithheld::IncompleteRead);
            }
            if selection.unrecognized_usage != Some(0) {
                withheld.insert(UnionWithheld::UnrecognizedUsage);
            }
            if selection.keys.iter().any(|key| {
                key.measurement
                    .as_ref()
                    .is_none_or(|m| m.disjoint.is_none())
            }) {
                withheld.insert(UnionWithheld::UnmeasuredEvent);
            }
        }
        if self.basis != other.basis {
            withheld.insert(UnionWithheld::MixedBasis);
        }
        if self.selection != other.selection {
            withheld.insert(UnionWithheld::MixedSelection);
        }
        let bounds = bounds(self, other);
        let events = if names(self.forked_from.as_deref(), other.session_id.as_deref()) {
            distinct_events(other, self, &mut withheld)
        } else if names(other.forked_from.as_deref(), self.session_id.as_deref()) {
            distinct_events(self, other, &mut withheld)
        } else {
            withheld.insert(UnionWithheld::UnestablishedLineage);
            Vec::new()
        };
        let amounts = if withheld.is_empty() {
            amounts(&events)
        } else {
            None
        };
        if amounts.is_none() && withheld.is_empty() {
            withheld.insert(UnionWithheld::MixedBasis);
        }
        ConsumptionUnion {
            withheld: withheld.into_iter().collect(),
            amounts,
            bounds,
        }
    }

    /// Whether every counted event has its key here: nothing was omitted or
    /// unavailable.
    fn is_whole(&self) -> bool {
        let omissions = &self.omissions;
        self.unavailable.is_none()
            && self.observed == Some(self.returned)
            && omissions.row_cap
                + omissions.byte_budget
                + omissions.oversized_key
                + omissions.unidentified
                + omissions.private_or_invalid_identity
                + omissions.unavailable_source
                == 0
    }
}

/// Whether a fork's recorded parent is this session, both established.
fn names(parent: Option<&str>, session: Option<&str>) -> bool {
    parent.is_some() && parent == session
}

/// The parent's events and the fork's own, each once, or nothing when the
/// fork's copies of its parent's events are not established. A verified copy
/// of a parent event off the parent's active branch is in the fork's active
/// domain alone, so an active-branch union counts it from the fork.
fn distinct_events<'a>(
    parent: &'a ConsumptionEventKeys,
    fork: &'a ConsumptionEventKeys,
    withheld: &mut BTreeSet<UnionWithheld>,
) -> Vec<&'a ConsumptionEventKey> {
    if fork.copy_coverage != Some(crate::usage::ConsumptionCopyCoverage::Session)
        || fork.copy_conflicts > 0
    {
        withheld.insert(UnionWithheld::UnverifiedCopies);
    }
    let mut events: Vec<&ConsumptionEventKey> = parent.keys.iter().collect();
    for key in &fork.keys {
        match &key.copy_of {
            Some(copy) => {
                let held = parent.keys.iter().any(|candidate| {
                    candidate.recording_entry.as_ref() == Some(copy)
                        && candidate.entry_digest == key.entry_digest
                });
                let off_branch = parent.selection == AccountingSelection::ActiveBranch
                    && fork.selection == AccountingSelection::ActiveBranch;
                if !held && off_branch {
                    events.push(key);
                } else if !held {
                    withheld.insert(UnionWithheld::UnverifiedCopies);
                }
            }
            None => {
                let same_entry = key.recording_entry.as_ref().is_some_and(|entry| {
                    parent.keys.iter().any(|candidate| {
                        candidate
                            .recording_entry
                            .as_ref()
                            .is_some_and(|held| held.entry_id == entry.entry_id)
                    })
                });
                if same_entry {
                    withheld.insert(UnionWithheld::UnverifiedCopies);
                }
                let same_response = key.response_id.is_some()
                    && parent.keys.iter().any(|candidate| {
                        candidate.provider == key.provider
                            && candidate.response_id == key.response_id
                    });
                if same_response {
                    withheld.insert(UnionWithheld::UnestablishedIdentity);
                }
                events.push(key);
            }
        }
    }
    events
}

fn amounts(events: &[&ConsumptionEventKey]) -> Option<ConsumptionAmounts> {
    let measurements = events
        .iter()
        .map(|key| key.measurement.as_ref())
        .collect::<Option<Vec<_>>>()?;
    let disjoint = DisjointTokens::sum(
        measurements
            .iter()
            .map(|measurement| measurement.disjoint.as_ref())
            .collect::<Option<Vec<_>>>()?,
    )?;
    let priced = measurements
        .iter()
        .filter_map(|measurement| measurement.cost.as_ref())
        .map(|cost| cost.usd)
        .collect::<Vec<_>>();
    let mut completeness = MeasurementCompleteness::default();
    let mut cost_coverage: Option<CostCoverage> = None;
    for measurement in &measurements {
        completeness.merge(&measurement.measurement_completeness);
        if let Some(coverage) = &measurement.cost_coverage {
            let total = cost_coverage.get_or_insert_with(CostCoverage::default);
            total.zero_written_requests += coverage.zero_written_requests;
            total.zero_written_observations += coverage.zero_written_observations;
        }
    }
    Some(ConsumptionAmounts {
        events: events.len(),
        disjoint,
        cost: (!priced.is_empty()).then(|| Cost {
            usd: priced.iter().sum(),
        }),
        cost_coverage,
        measurement_completeness: completeness,
    })
}

/// A selection's known sum of one quantity, and whether it is the whole
/// quantity: every key present and every event's amount recorded.
fn known<T: Copy>(
    selection: &ConsumptionEventKeys,
    amount: impl Fn(&ConsumptionEventKey) -> Option<T>,
    zero: T,
    add: impl Fn(T, T) -> T,
) -> (Option<T>, bool) {
    let mut sum = None;
    let mut whole = selection.is_whole()
        && selection.coverage == Some(AccountingCoverage::Session)
        && selection.unreadable_records == Some(0)
        && selection.unrecognized_usage == Some(0);
    for key in &selection.keys {
        match amount(key) {
            Some(value) => sum = Some(add(sum.unwrap_or(zero), value)),
            None => whole = false,
        }
    }
    (sum, whole)
}

fn category_bounds(
    first: &ConsumptionEventKeys,
    second: &ConsumptionEventKeys,
    category: &'static str,
    amount: impl Fn(&DisjointTokens) -> Option<u64> + Copy,
) -> Option<QuantityBounds<u64>> {
    let of = |key: &ConsumptionEventKey| {
        let disjoint = key.measurement.as_ref()?.disjoint.as_ref()?;
        disjoint
            .measurement_completeness
            .is_complete(&[category])
            .then(|| amount(disjoint))
            .flatten()
    };
    let known_of = |selection: &ConsumptionEventKeys| {
        // A qualified amount still bounds from below.
        let (sum, whole) = known(
            selection,
            |key| {
                let disjoint = key.measurement.as_ref()?.disjoint.as_ref()?;
                amount(disjoint)
            },
            0,
            u64::saturating_add,
        );
        let complete = selection.keys.iter().all(|key| of(key).is_some());
        (sum, whole && complete)
    };
    let (first_sum, first_whole) = known_of(first);
    let (second_sum, second_whole) = known_of(second);
    let at_least = first_sum.into_iter().chain(second_sum).max()?;
    Some(QuantityBounds {
        at_least,
        at_most: (first_whole && second_whole).then(|| {
            first_sum
                .unwrap_or(0)
                .saturating_add(second_sum.unwrap_or(0))
        }),
    })
}

fn bounds(first: &ConsumptionEventKeys, second: &ConsumptionEventKeys) -> ConsumptionUnionBounds {
    let cost_of = |selection: &ConsumptionEventKeys| {
        let (sum, mut whole) = known(
            selection,
            |key| key.measurement.as_ref()?.cost.as_ref().map(|cost| cost.usd),
            0.0,
            |left, right| left + right,
        );
        // A recorded price that is partial still bounds from below.
        whole &= selection.keys.iter().all(|key| {
            key.measurement.as_ref().is_some_and(|m| {
                m.cost.is_some() && m.measurement_completeness.is_complete(&["cost"])
            })
        });
        (sum, whole)
    };
    let (first_cost, first_whole) = cost_of(first);
    let (second_cost, second_whole) = cost_of(second);
    ConsumptionUnionBounds {
        uncached_input: category_bounds(first, second, "uncached_input", |d| d.uncached_input),
        cache_read: category_bounds(first, second, "cache_read", |d| d.cache_read),
        cache_write: category_bounds(first, second, "cache_write", |d| {
            d.cache_write.as_ref().map(CacheWrites::total)
        }),
        output: category_bounds(first, second, "output", |d| d.output),
        cost: first_cost
            .into_iter()
            .chain(second_cost)
            .reduce(f64::max)
            .map(|at_least| QuantityBounds {
                at_least,
                at_most: (first_whole && second_whole)
                    .then(|| first_cost.unwrap_or(0.0) + second_cost.unwrap_or(0.0)),
            }),
    }
}
