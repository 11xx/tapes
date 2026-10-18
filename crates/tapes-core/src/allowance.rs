//! Allowance percent apportioned over the sessions that spent it. A provider
//! reports how much of a quota window an account has used; each recording
//! that saw the report holds an observation of it. Between two consecutive
//! observations of one pool's window, the rise in used percent is
//! apportioned over the disjoint tokens each session spent inside that
//! interval. The result is an estimate from observations, reported beside
//! tokens and any price and never in their place.
//!
//! Observed percents are whole numbers, read as the true percent rounded to
//! the nearest whole, so an apportioned figure is a bound: the estimate
//! apportions the rises as observed, and the bounds hold for every true
//! percent the observations allow, which never falls within one window
//! period. A run of equal observations hides at most one unit of rise, so a
//! set's figure is uncertain by about one unit per run its weight changes in.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::window::QuotaKind;

pub const ALLOWANCE_KIND: &str = "apportioned-allowance";
pub const RATES_SCHEMA: &str = "tapes-allowance-rates/1";

/// The allowance one observation reports on: one quota window of one plan
/// of one account, as the recordings name them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct PoolKey {
    pub harness: String,
    /// The account the recording names, where it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub window: QuotaKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
}

/// One observation of a pool's used percent.
#[derive(Clone, Debug, PartialEq)]
pub struct RateObservation {
    pub at: DateTime<Utc>,
    pub pool: PoolKey,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

/// One request's disjoint tokens, at the time it was placed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimedUse {
    pub at: DateTime<Utc>,
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl TimedUse {
    pub fn total(&self) -> u64 {
        self.uncached_input + self.cache_read + self.cache_write + self.output
    }
}

/// What a recording holds for apportionment: its requests' disjoint tokens
/// in time and the allowance observations it saw.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timeline {
    pub requests: Vec<TimedUse>,
    pub observations: Vec<RateObservation>,
}

/// How a pool's percent is weighted over the tokens spent in an interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Weighting {
    /// Uncached input, cache read, cache write and output, summed.
    DisjointTotal,
    /// Output alone, which includes reasoning.
    Output,
}

/// A percent as observations bound it: `estimate` apportions the observed
/// rises as written, and the bounds allow for their rounding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct PercentBounds {
    pub estimate: f64,
    pub at_least: f64,
    pub at_most: f64,
}

impl std::ops::Add for PercentBounds {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            estimate: self.estimate + other.estimate,
            at_least: self.at_least + other.at_least,
            at_most: self.at_most + other.at_most,
        }
    }
}

/// The allowance a set of sessions spent from one pool window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Allowance {
    pub kind: &'static str,
    pub window: QuotaKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Percent of the window, weighted by disjoint total.
    #[serde(flatten)]
    pub percent: PercentBounds,
    /// The same apportionment weighted by output alone, a sensitivity bound
    /// on the weighting.
    pub output_weighted: PercentBounds,
    pub coverage: AllowanceCoverage,
}

/// What the apportionment rests on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AllowanceCoverage {
    /// Observations of the pool inside the window.
    pub observations: usize,
    /// Intervals between consecutive observations of one window period.
    pub intervals: usize,
    /// Observations below the percent already observed in their period,
    /// snapshots older than an earlier one, which are skipped.
    #[serde(skip_serializing_if = "is_zero")]
    pub stale_observations: usize,
    /// Requests of the set that fell in no interval: before the first
    /// observation, after the last, or across a reset. Their allowance is
    /// not counted, so a set with any is a known subtotal.
    #[serde(skip_serializing_if = "is_zero")]
    pub uncovered_requests: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// One pool's observed allowance over a window and where it went.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PoolAllowance {
    pub pool: PoolKey,
    /// The rise in used percent the observations show, summed over the
    /// window's intervals.
    pub observed: PercentBounds,
    /// Shares of sessions the selection holds.
    pub attributed: PercentBounds,
    /// Shares of the pool's other sessions on this machine.
    pub outside_selection: PercentBounds,
    /// Rises in intervals where no session read spent a token: another
    /// machine, another client, or a session the listing did not reach.
    pub without_recorded_use: PercentBounds,
    pub coverage: AllowanceCoverage,
    /// The member listing reached its bound, so sessions it left out may
    /// have spent from the pool and their share sits with the members read.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub members_truncated: bool,
}

/// One session of a pool, with whether the selection holds it.
pub struct PoolMember<'a> {
    pub id: &'a str,
    pub selected: bool,
    pub requests: &'a [TimedUse],
}

/// The intervals of a pool: chains of consecutive observations within one
/// window period, each interval's rise, and each member's tokens in it.
pub struct Apportionment {
    pub pool: PoolKey,
    observations: usize,
    /// Observations below the running percent of their period.
    stale: usize,
    /// Per chain, per observation: whether its percent is a rounded whole.
    rounded: Vec<Vec<bool>>,
    /// Per chain, per observation: the observed percent.
    percents: Vec<Vec<f64>>,
    /// Per chain, per interval: the observed rise.
    rises: Vec<Vec<f64>>,
    /// Per chain, per interval: (start, end].
    bounds: Vec<Vec<(DateTime<Utc>, DateTime<Utc>)>>,
    /// Per member id, per chain, per interval: (disjoint total, output).
    weights: BTreeMap<String, Vec<Vec<(f64, f64)>>>,
    totals: Vec<Vec<(f64, f64)>>,
    uncovered: BTreeMap<String, usize>,
    selected: BTreeMap<String, bool>,
}

impl Apportionment {
    /// Split the pool's observations into chains, one per window period: a
    /// new chain at a new `resets_at` (beyond a minute's jitter), or at a
    /// falling percent where no reset is recorded. Inside a period a lower
    /// percent is a stale snapshot and is skipped. Every member's requests
    /// are placed in the interval that holds them.
    pub fn new(
        pool: PoolKey,
        observations: &[RateObservation],
        members: &[PoolMember<'_>],
    ) -> Self {
        let mut ordered = observations
            .iter()
            .filter(|observation| observation.pool == pool)
            .collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            left.at
                .cmp(&right.at)
                .then(left.used_percent.total_cmp(&right.used_percent))
        });
        ordered.dedup_by(|later, earlier| {
            later.at == earlier.at && later.used_percent == earlier.used_percent
        });
        let mut chains: Vec<Vec<&RateObservation>> = Vec::new();
        let mut stale = 0;
        for observation in &ordered {
            let Some(last) = chains.last().and_then(|chain| chain.last()) else {
                chains.push(vec![observation]);
                continue;
            };
            let period = same_period(last.resets_at, observation.resets_at);
            if period && observation.used_percent >= last.used_percent {
                chains.last_mut().unwrap().push(observation);
            } else if period && last.resets_at.is_some() {
                // A lower percent inside one period is a snapshot older than
                // the one before it: another session's report lagging.
                stale += 1;
            } else {
                chains.push(vec![observation]);
            }
        }
        let rounded = chains
            .iter()
            .map(|chain| {
                chain
                    .iter()
                    .map(|observation| observation.used_percent.fract() == 0.0)
                    .collect()
            })
            .collect();
        let percents = chains
            .iter()
            .map(|chain| {
                chain
                    .iter()
                    .map(|observation| observation.used_percent)
                    .collect()
            })
            .collect();
        let rises = chains
            .iter()
            .map(|chain| {
                chain
                    .windows(2)
                    .map(|pair| pair[1].used_percent - pair[0].used_percent)
                    .collect()
            })
            .collect();
        let bounds: Vec<Vec<_>> = chains
            .iter()
            .map(|chain| {
                chain
                    .windows(2)
                    .map(|pair| (pair[0].at, pair[1].at))
                    .collect()
            })
            .collect();
        let mut weights = BTreeMap::new();
        let mut uncovered = BTreeMap::new();
        let mut selected = BTreeMap::new();
        let mut totals: Vec<Vec<(f64, f64)>> = bounds
            .iter()
            .map(|chain| vec![(0.0, 0.0); chain.len()])
            .collect();
        for member in members {
            let mut placed: Vec<Vec<(f64, f64)>> = bounds
                .iter()
                .map(|chain| vec![(0.0, 0.0); chain.len()])
                .collect();
            let mut missed = 0;
            for request in member.requests {
                let slot = bounds.iter().enumerate().find_map(|(chain, intervals)| {
                    intervals
                        .iter()
                        .position(|(start, end)| *start < request.at && request.at <= *end)
                        .map(|interval| (chain, interval))
                });
                match slot {
                    Some((chain, interval)) => {
                        let cell = &mut placed[chain][interval];
                        cell.0 += request.total() as f64;
                        cell.1 += request.output as f64;
                        let total = &mut totals[chain][interval];
                        total.0 += request.total() as f64;
                        total.1 += request.output as f64;
                    }
                    None => missed += 1,
                }
            }
            weights.insert(member.id.to_owned(), placed);
            uncovered.insert(member.id.to_owned(), missed);
            selected.insert(member.id.to_owned(), member.selected);
        }
        Self {
            pool,
            stale,
            observations: ordered.len() - stale,
            rounded,
            percents,
            rises,
            bounds,
            weights,
            totals,
            uncovered,
            selected,
        }
    }

    fn intervals(&self) -> usize {
        self.bounds.iter().map(Vec::len).sum()
    }

    /// The bounds of a set whose weight in each interval `share` gives.
    ///
    /// Each whole observed percent `u` holds a true percent in
    /// `[u - 0.5, u + 0.5]`, and the true percent never falls within a chain,
    /// so a run of equal observations (a plateau) hides at most one unit of
    /// rise and plateaus bound independently. Writing the set's figure as
    /// `sum_j t_j c_j` with `c_j = w_(j-1) - w_j`, a plateau starting at `a`
    /// contributes `(u - 0.5) S_a` plus, at least, the least and, at most,
    /// the greatest of zero and its suffix sums `S_k`. An observation with a
    /// fractional percent is exact.
    fn bound(&self, share: impl Fn(usize, usize) -> f64) -> PercentBounds {
        let mut point = 0.0;
        let mut low = 0.0;
        let mut high = 0.0;
        for (chain, rises) in self.rises.iter().enumerate() {
            let weights = (0..rises.len())
                .map(|interval| share(chain, interval))
                .collect::<Vec<_>>();
            let weight = |index: isize| {
                usize::try_from(index)
                    .ok()
                    .and_then(|index| weights.get(index))
                    .copied()
                    .unwrap_or(0.0)
            };
            let coefficients = (0..self.rounded[chain].len())
                .map(|index| weight(index as isize - 1) - weight(index as isize))
                .collect::<Vec<_>>();
            for (interval, rise) in rises.iter().enumerate() {
                point += weights[interval] * rise;
            }
            let percents = &self.percents[chain];
            let mut start = 0;
            while start < percents.len() {
                let mut end = start;
                if self.rounded[chain][start] {
                    while end + 1 < percents.len()
                        && self.rounded[chain][end + 1]
                        && percents[end + 1] == percents[start]
                    {
                        end += 1;
                    }
                }
                let half = if self.rounded[chain][start] { 0.5 } else { 0.0 };
                let suffix = |from: usize| coefficients[from..=end].iter().sum::<f64>();
                let base = (percents[start] - half) * suffix(start);
                let (mut least, mut most) = (0.0_f64, 0.0_f64);
                if half > 0.0 {
                    for from in start..=end {
                        least = least.min(suffix(from));
                        most = most.max(suffix(from));
                    }
                }
                low += base + least;
                high += base + most;
                start = end + 1;
            }
        }
        PercentBounds {
            estimate: point,
            at_least: low.max(0.0),
            at_most: high,
        }
    }

    fn weight_of(&self, ids: &[&str], chain: usize, interval: usize, weighting: Weighting) -> f64 {
        let total = self.totals[chain][interval];
        let total = match weighting {
            Weighting::DisjointTotal => total.0,
            Weighting::Output => total.1,
        };
        if total == 0.0 {
            return 0.0;
        }
        ids.iter()
            .filter_map(|id| self.weights.get(*id))
            .map(|placed| match weighting {
                Weighting::DisjointTotal => placed[chain][interval].0,
                Weighting::Output => placed[chain][interval].1,
            })
            .sum::<f64>()
            / total
    }

    /// The allowance the sessions `ids` spent from this pool.
    pub fn allowance(&self, ids: &[&str]) -> Allowance {
        Allowance {
            kind: ALLOWANCE_KIND,
            window: self.pool.window,
            window_minutes: self.pool.window_minutes,
            plan: self.pool.plan.clone(),
            percent: self.bound(|chain, interval| {
                self.weight_of(ids, chain, interval, Weighting::DisjointTotal)
            }),
            output_weighted: self
                .bound(|chain, interval| self.weight_of(ids, chain, interval, Weighting::Output)),
            coverage: AllowanceCoverage {
                observations: self.observations,
                intervals: self.intervals(),
                stale_observations: self.stale,
                uncovered_requests: ids.iter().filter_map(|id| self.uncovered.get(*id)).sum(),
            },
        }
    }

    /// The pool's observed rise and how it divides: the selection's shares,
    /// the pool's other sessions, and rises no session read spent tokens in.
    pub fn pool(&self) -> PoolAllowance {
        let selected = self
            .selected
            .iter()
            .filter(|(_, selected)| **selected)
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        let outside = self
            .selected
            .iter()
            .filter(|(_, selected)| !**selected)
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>();
        let idle = |chain: usize, interval: usize| {
            if self.totals[chain][interval].0 == 0.0 {
                1.0
            } else {
                0.0
            }
        };
        PoolAllowance {
            pool: self.pool.clone(),
            observed: self.bound(|_, _| 1.0),
            attributed: self.bound(|chain, interval| {
                self.weight_of(&selected, chain, interval, Weighting::DisjointTotal)
            }),
            outside_selection: self.bound(|chain, interval| {
                self.weight_of(&outside, chain, interval, Weighting::DisjointTotal)
            }),
            without_recorded_use: self.bound(idle),
            members_truncated: false,
            coverage: AllowanceCoverage {
                observations: self.observations,
                intervals: self.intervals(),
                stale_observations: self.stale,
                uncovered_requests: selected
                    .iter()
                    .filter_map(|id| self.uncovered.get(*id))
                    .sum(),
            },
        }
    }
}

/// Whether two observations' resets name one window period: the same reset,
/// within the minute a provider's reported reset jitters by, or none
/// recorded on either.
fn same_period(left: Option<DateTime<Utc>>, right: Option<DateTime<Utc>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => (left - right).num_seconds().abs() <= 60,
        (None, None) => true,
        _ => false,
    }
}

/// Why a harness's sessions carry no apportioned allowance.
pub fn absent_reason(harness: &str) -> Option<&'static str> {
    match harness {
        "codex" => None,
        "claude" => Some(
            "claude records no used percent: its quota observations exist only on refused requests",
        ),
        _ => Some("the harness records no allowance observations"),
    }
}

/// Quantiles of a sample by the nearest-rank method: the median averages the
/// two middle values of an even sample, and the 90th percentile is the
/// smallest value with at least 90% of the sample at or below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Quantiles {
    pub count: usize,
    pub median: f64,
    pub p90: f64,
    pub max: f64,
}

impl Quantiles {
    pub fn of(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let count = sorted.len();
        let median = if count % 2 == 1 {
            sorted[count / 2]
        } else {
            (sorted[count / 2 - 1] + sorted[count / 2]) / 2.0
        };
        let rank = (0.9 * count as f64).ceil() as usize;
        Some(Self {
            count,
            median,
            p90: sorted[rank.clamp(1, count) - 1],
            max: sorted[count - 1],
        })
    }
}

/// A ratio of tokens to percent, bounded by the percent's bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct TokensPerPercent {
    pub estimate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_least: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_most: Option<f64>,
}

impl TokensPerPercent {
    pub fn of(tokens: u64, percent: PercentBounds) -> Option<Self> {
        let ratio = |percent: f64| (percent > 0.0).then(|| tokens as f64 / percent);
        Some(Self {
            estimate: ratio(percent.estimate)?,
            at_least: ratio(percent.at_most),
            at_most: ratio(percent.at_least),
        })
    }
}

/// Tokens per one percent of the window, by disjoint category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct TokensPerPercentSplit {
    pub total: TokensPerPercent,
    pub uncached_input: TokensPerPercent,
    pub cache_read: TokensPerPercent,
    pub output: TokensPerPercent,
    pub reasoning: TokensPerPercent,
}

/// The percent statistics of a group's lanes: each statistic over the
/// lanes' estimates and over their lower and upper bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct LanePercent {
    pub estimate: Quantiles,
    pub at_least: Quantiles,
    pub at_most: Quantiles,
}

/// When a rate was measured: the first and last request of its lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct MeasuredOver {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// One group's allowance rates: what one lane (a session) of it costs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RateGroup {
    pub key: crate::usage::GroupKey,
    /// The length of the window the percents are of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    pub lanes: usize,
    pub percent_per_lane: LanePercent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_percent: Option<TokensPerPercentSplit>,
    /// Median minutes from a lane's first to its last request.
    pub median_duration_minutes: f64,
    pub measured_over: MeasuredOver,
}

/// Allowance rates per lane over a lookback.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AllowanceRates {
    pub schema: &'static str,
    pub lookback: crate::window::WindowBounds,
    pub window: QuotaKind,
    pub weighting: Weighting,
    pub groups: Vec<RateGroup>,
    /// The pools whose observations the rates rest on.
    pub pools: Vec<PoolKey>,
    /// Sessions left out: still running, or holding no allowance.
    pub excluded_running: usize,
    pub excluded_without_allowance: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_absent: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
}
