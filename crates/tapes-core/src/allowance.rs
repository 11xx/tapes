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
pub const RATES_SCHEMA: &str = "tapes-allowance-rates/6";
pub const FORECAST_SCHEMA: &str = "tapes-allowance-forecast/2";

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
/// in time, the requests it recorded no time for, the allowance observations
/// it saw, and the account it names.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Timeline {
    pub requests: Vec<TimedUse>,
    /// Requests whose record carries no usable timestamp, which no interval
    /// can hold.
    pub untimed: usize,
    pub observations: Vec<RateObservation>,
    /// The account the recording names, where it names one.
    pub account: Option<String>,
}

impl Timeline {
    /// The quota limits the recording's own observations name.
    pub fn limits(&self) -> Vec<Option<&str>> {
        let mut limits = self
            .observations
            .iter()
            .map(|observation| observation.pool.limit.as_deref())
            .collect::<Vec<_>>();
        limits.sort();
        limits.dedup();
        limits
    }
}

/// Tokens by disjoint category, summed over requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spent {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl Spent {
    fn add(&mut self, request: &TimedUse) {
        self.uncached_input += request.uncached_input;
        self.cache_read += request.cache_read;
        self.cache_write += request.cache_write;
        self.output += request.output;
        self.reasoning += request.reasoning;
    }

    pub fn total(&self) -> u64 {
        self.uncached_input + self.cache_read + self.cache_write + self.output
    }
}

impl std::ops::AddAssign for Spent {
    fn add_assign(&mut self, other: Self) {
        self.uncached_input += other.uncached_input;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.output += other.output;
        self.reasoning += other.reasoning;
    }
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
    /// observation, after the last, across a reset, or recorded without a
    /// time. Their allowance is not counted, so a set with any is a known
    /// subtotal.
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

/// One session that may spend from a pool, with whether the selection
/// holds it, the account its recording names and the quota limits its own
/// observations name.
pub struct PoolMember<'a> {
    pub id: &'a str,
    pub selected: bool,
    pub account: Option<&'a str>,
    pub limits: Vec<Option<&'a str>>,
    pub requests: &'a [TimedUse],
    /// Requests recorded without a time.
    pub untimed: usize,
}

impl PoolMember<'_> {
    /// Whether the session spends from `pool`: its recording names the
    /// pool's account (or, like the pool, none), and when its own
    /// observations name quota limits, the pool's is among them.
    pub fn spends_from(&self, pool: &PoolKey) -> bool {
        self.account == pool.account.as_deref()
            && (self.limits.is_empty() || self.limits.contains(&pool.limit.as_deref()))
    }
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
    /// Per member id, the intervals it spent in, each once, in order:
    /// (chain, interval, disjoint total, output). A member keeps a cell per
    /// interval it spent in, never one per interval of the pool.
    weights: BTreeMap<String, Vec<(usize, usize, f64, f64)>>,
    totals: Vec<Vec<(f64, f64)>>,
    /// Per member id, the tokens of its requests an interval holds.
    covered: BTreeMap<String, Spent>,
    uncovered: BTreeMap<String, usize>,
    selected: BTreeMap<String, bool>,
    /// The newest observation of the newest window period.
    latest: Option<RateObservation>,
}

impl Apportionment {
    /// Split the pool's observations into chains, one per window period: a
    /// new chain at a new `resets_at` (beyond a minute's jitter), or at a
    /// falling percent where no reset is recorded. Inside a period a lower
    /// percent is a stale snapshot and is skipped. Every member that spends
    /// from the pool has its requests placed in the interval that holds
    /// them; the others are no members of it.
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
        let mut covered = BTreeMap::new();
        let mut uncovered = BTreeMap::new();
        let mut selected = BTreeMap::new();
        let mut totals: Vec<Vec<(f64, f64)>> = bounds
            .iter()
            .map(|chain| vec![(0.0, 0.0); chain.len()])
            .collect();
        for member in members.iter().filter(|member| member.spends_from(&pool)) {
            let mut placed = BTreeMap::<(usize, usize), (f64, f64)>::new();
            let mut spent = Spent::default();
            let mut missed = member.untimed;
            for request in member.requests {
                match slot(&bounds, request.at) {
                    Some((chain, interval)) => {
                        let cell = placed.entry((chain, interval)).or_default();
                        cell.0 += request.total() as f64;
                        cell.1 += request.output as f64;
                        let total = &mut totals[chain][interval];
                        total.0 += request.total() as f64;
                        total.1 += request.output as f64;
                        spent.add(request);
                    }
                    None => missed += 1,
                }
            }
            weights.insert(
                member.id.to_owned(),
                placed
                    .into_iter()
                    .map(|((chain, interval), (total, output))| (chain, interval, total, output))
                    .collect(),
            );
            covered.insert(member.id.to_owned(), spent);
            uncovered.insert(member.id.to_owned(), missed);
            selected.insert(member.id.to_owned(), member.selected);
        }
        let latest = chains
            .last()
            .and_then(|chain| chain.last())
            .map(|observation| (*observation).clone());
        Self {
            pool,
            stale,
            latest,
            observations: ordered.len() - stale,
            rounded,
            percents,
            rises,
            bounds,
            weights,
            totals,
            covered,
            uncovered,
            selected,
        }
    }

    /// How many weight cells the members keep: one per interval each spent
    /// in, at most one per request placed.
    pub fn weight_cells(&self) -> usize {
        self.weights.values().map(Vec::len).sum()
    }

    /// The tokens of the sessions `ids` that an interval of the pool holds,
    /// the tokens its allowance is apportioned over.
    pub fn covered(&self, ids: &[&str]) -> Spent {
        let mut spent = Spent::default();
        for covered in ids.iter().filter_map(|id| self.covered.get(*id)) {
            spent += *covered;
        }
        spent
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

    /// The share of each interval's tokens the sessions `ids` spent, per
    /// chain and interval, summed from their cells.
    fn shares(&self, ids: &[&str], weighting: Weighting) -> Vec<Vec<f64>> {
        let mut shares = self
            .totals
            .iter()
            .map(|chain| vec![0.0; chain.len()])
            .collect::<Vec<_>>();
        for cells in ids.iter().filter_map(|id| self.weights.get(*id)) {
            for (chain, interval, total, output) in cells {
                shares[*chain][*interval] += match weighting {
                    Weighting::DisjointTotal => total,
                    Weighting::Output => output,
                };
            }
        }
        for (chain, intervals) in shares.iter_mut().enumerate() {
            for (interval, share) in intervals.iter_mut().enumerate() {
                let (total, output) = self.totals[chain][interval];
                let total = match weighting {
                    Weighting::DisjointTotal => total,
                    Weighting::Output => output,
                };
                *share = if total == 0.0 { 0.0 } else { *share / total };
            }
        }
        shares
    }

    /// The newest observation of the pool's newest window period: the used
    /// percent and reset a forecast starts from.
    pub fn latest(&self) -> Option<&RateObservation> {
        self.latest.as_ref()
    }

    /// The percent the sessions `ids` spent, smoothed over each window
    /// period: their share of the tokens spent in the period's intervals
    /// that held any, times the rise across those intervals. Within a period
    /// the share is constant, so a run of whole-percent steps that landed on
    /// whichever sessions happened to spend in them is spread over all of
    /// the period's spending. The bounds are those of the period's rise,
    /// from the rounding of the observations that bound it.
    pub fn smoothed(&self, ids: &[&str]) -> PercentBounds {
        let mut spent = vec![0.0; self.totals.len()];
        for cells in ids.iter().filter_map(|id| self.weights.get(*id)) {
            for (chain, _, total, _) in cells {
                spent[*chain] += total;
            }
        }
        let periods = self
            .totals
            .iter()
            .map(|intervals| intervals.iter().map(|(total, _)| total).sum::<f64>())
            .collect::<Vec<_>>();
        self.bound(|chain, interval| {
            if self.totals[chain][interval].0 == 0.0 || periods[chain] == 0.0 {
                0.0
            } else {
                spent[chain] / periods[chain]
            }
        })
    }

    /// The allowance the sessions `ids` spent from this pool.
    pub fn allowance(&self, ids: &[&str]) -> Allowance {
        Allowance {
            kind: ALLOWANCE_KIND,
            window: self.pool.window,
            window_minutes: self.pool.window_minutes,
            plan: self.pool.plan.clone(),
            percent: {
                let shares = self.shares(ids, Weighting::DisjointTotal);
                self.bound(|chain, interval| shares[chain][interval])
            },
            output_weighted: {
                let shares = self.shares(ids, Weighting::Output);
                self.bound(|chain, interval| shares[chain][interval])
            },
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
            attributed: {
                let shares = self.shares(&selected, Weighting::DisjointTotal);
                self.bound(|chain, interval| shares[chain][interval])
            },
            outside_selection: {
                let shares = self.shares(&outside, Weighting::DisjointTotal);
                self.bound(|chain, interval| shares[chain][interval])
            },
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

/// The chain and interval `(start, end]` holding `at`. Chains follow one
/// another in time and each chain's intervals are in order, so each is
/// searched by halves.
fn slot(
    bounds: &[Vec<(DateTime<Utc>, DateTime<Utc>)>],
    at: DateTime<Utc>,
) -> Option<(usize, usize)> {
    bounds.iter().enumerate().find_map(|(chain, intervals)| {
        let first = intervals.first()?;
        let last = intervals.last()?;
        if at <= first.0 || at > last.1 {
            return None;
        }
        let interval = intervals.partition_point(|(_, end)| *end < at);
        (interval < intervals.len() && intervals[interval].0 < at).then_some((chain, interval))
    })
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

/// How a lane's percent of a window is estimated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Estimator {
    /// Each interval's rise apportioned over the tokens spent in it, the
    /// attribution a window read gives.
    #[default]
    IntervalApportioned,
    /// The lane's share of each window period's tokens times the period's
    /// rise; see [`Apportionment::smoothed`].
    PeriodSmoothed,
}

/// The percent statistics of a group's lanes: each statistic over the
/// lanes' estimates and over their lower and upper bounds, under the named
/// estimator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct LanePercent {
    pub estimator: Estimator,
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

/// One group's allowance rates: what one lane (a session) of it costs under
/// one plan, limit, and window length.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RateGroup {
    pub key: crate::usage::GroupKey,
    /// The plan the percents are of, as the observations name it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The quota limit the percents are of, as the observations name it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// The length of the window the percents are of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    pub lanes: usize,
    /// Each lane's percent as a window read attributes it.
    pub percent_per_lane: LanePercent,
    /// Each lane's percent smoothed over its window periods, a steadier lane
    /// cost than the interval attribution at whole-percent resolution.
    pub smoothed_percent_per_lane: LanePercent,
    /// The tokens of each lane's requests that an interval of its pools
    /// holds.
    pub tokens_per_lane: Quantiles,
    /// Tokens per one percent: the tokens of the lanes' requests that an
    /// interval of their pools holds, over the percent apportioned on them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_percent: Option<TokensPerPercentSplit>,
    /// The lanes' requests no interval held (before the first observation,
    /// after the last, across a reset, or recorded without a time), which
    /// neither side of the rate counts.
    pub uncovered_requests: usize,
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
    pub lanes_read: LanesRead,
}

/// Which sessions rates were measured over: every completed session of the
/// lookback, up to `limit` per harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LanesRead {
    /// The most sessions of a harness read: the caller's --limit, else the
    /// lane bound.
    pub limit: usize,
    /// The limit is the caller's.
    pub explicit_limit: bool,
    /// Sessions the lookback may hold that the limit left out: those first
    /// in --sort order are kept, then the parents reached only through their
    /// subagents.
    #[serde(skip_serializing_if = "is_zero")]
    pub left_out_by_limit: usize,
    /// The listing returned as many sessions of a harness as the lane bound
    /// allows, or stopped at its scan ceiling, so sessions beyond it were
    /// not read and are not counted in `left_out_by_limit`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub lane_bound_reached: bool,
}

/// One model and variant of a planned mix, and how many lanes of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedLanes {
    pub model: String,
    pub variant: Option<String>,
    pub lanes: usize,
}

/// One planned model's lanes as the rates size them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForecastLanes {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub lanes: usize,
    /// The measured lanes the size rests on.
    pub measured_lanes: usize,
    /// Median tokens of a measured lane.
    pub median_lane_tokens: f64,
    /// The group's tokens per one percent of the window.
    pub tokens_per_percent: TokensPerPercent,
    /// One lane's percent: median lane tokens over tokens per percent,
    /// bounded by the ratio's bounds.
    pub percent_per_lane: PercentBounds,
    /// Median minutes from a measured lane's first to its last request.
    pub median_duration_minutes: f64,
    /// All of this model's planned lanes together.
    pub percent: PercentBounds,
}

/// Where a forecast's window stands: its newest observation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForecastWindow {
    pub pool: PoolKey,
    /// The length of the window the forecast sizes against, as the
    /// observations name it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    pub observed_at: DateTime<Utc>,
    pub used_percent: f64,
    /// The newest period's reset, as its newest observation reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// What is left of the window: 100 less the used percent, bounded by
    /// its rounding, or the whole window once the reported reset has passed.
    pub remaining_percent: PercentBounds,
    /// The reported reset passed before the forecast was made.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reset_since_observed: bool,
}

/// A planned lane mix against an allowance window: the percent it consumes,
/// and how long the window lasts running it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AllowanceForecast {
    pub schema: &'static str,
    pub lookback: crate::window::WindowBounds,
    pub window: QuotaKind,
    pub window_state: ForecastWindow,
    /// Which sessions the rates sizing the plan were measured over.
    pub lanes_read: LanesRead,
    pub plan: Vec<ForecastLanes>,
    /// Lanes running at once.
    pub concurrency: usize,
    /// The whole mix's percent.
    pub percent: PercentBounds,
    /// Whether the mix fits in what is left: `yes` when even its upper
    /// bound does, `no` when even its lower bound does not, else `maybe`.
    pub fits: Fit,
    /// Percent per minute while `concurrency` lanes of the mix run.
    pub percent_per_minute: PercentBounds,
    /// Minutes until the remaining percent is spent at that pace.
    pub minutes_to_exhaustion: PercentBounds,
    /// Whether that falls before the newest period's reset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exhausted_before_reset: Option<bool>,
}

/// Whether a planned mix fits in a window's remaining percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fit {
    Yes,
    Maybe,
    No,
}
