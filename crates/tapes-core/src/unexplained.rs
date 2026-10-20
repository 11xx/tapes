//! The part of a recorded total that no request record explains.
//!
//! A Claude `cost-state` totals a run's consumption per model, and can count
//! usage that none of the recording's request records shows. Per model, that
//! part is the recorded total less the requests it covers: the recording's
//! own, and in a family read its children's as well. It is named beside the
//! per-model split and never folded into a figure summed from requests.
//!
//! Successive recorded totals are cumulative and write no timestamp, so each
//! one bounds in time what accrued since the one before it: after the newest
//! timestamped record preceding the previous total, and no later than the
//! newest one preceding it. A span holds the part of each interval that falls
//! inside it, an interval it cuts shared by the recording's own timestamped
//! records between the two totals that it holds, so a span over an idle
//! stretch of a long interval holds little of it; an interval no timestamped
//! record bounds is placed in no span and stays with the remainder.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::model::{CacheWrites, Tokens};
use crate::pricing::ListPriceEstimate;
use crate::span::SpanBounds;

/// The most recorded totals a chain keeps. Past it, the oldest two
/// intervals merge, so placement coarsens and the whole stays exact.
pub const MAX_MARKS: usize = 4096;

/// The most related requests a family read keeps. A family with more
/// leaves its requests unknown, and its part is withheld.
pub const MAX_FAMILY_REQUESTS: usize = 100_000;

/// The most record timestamps a chain keeps to share cut intervals by.
/// Past it, cut intervals are shared by duration instead.
pub const MAX_STAMPS: usize = 1_000_000;

/// How a cut interval is shared by the records it holds, as a placement
/// names it.
pub const PLACEMENT_BASIS: &str = "cost-state-interval-prorated-by-records";

/// How a cut interval is shared by duration, where the chain kept no record
/// timestamps.
pub const PLACEMENT_BASIS_TIME: &str = "cost-state-interval-prorated-by-time";

const PLACEMENT_STATEMENT: &str = "each recorded total's increase accrued after the newest \
     timestamped record preceding the total before it and no later than the newest one \
     preceding it; a figure holding every timestamped record written between the two totals \
     holds the increase whole, and one holding some of them holds that share of it";

const PLACEMENT_STATEMENT_TIME: &str = "each recorded total's increase accrued after the \
     newest timestamped record preceding the total before it and no later than the newest one \
     preceding it; a figure holding that interval whole holds the increase whole, and a figure \
     cutting it holds the share of its duration it covers";

/// Counters in the order input, output, reasoning, cache read, cache write.
/// An unrecorded counter counts as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts(pub [u64; 5]);

impl Counts {
    pub fn of(tokens: &Tokens) -> Self {
        Self(
            [
                tokens.input,
                tokens.output,
                tokens.reasoning,
                tokens.cache_read,
                tokens.cache_write,
            ]
            .map(|count| count.unwrap_or(0)),
        )
    }

    pub fn tokens(self) -> Tokens {
        let [input, output, reasoning, cache_read, cache_write] = self.0.map(Some);
        Tokens {
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
        }
    }

    pub fn add(&mut self, other: Self) {
        for (total, count) in self.0.iter_mut().zip(other.0) {
            *total = total.saturating_add(count);
        }
    }

    pub fn is_zero(self) -> bool {
        self.0.iter().all(|count| *count == 0)
    }
}

/// One recorded total, in recording order.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    /// The newest timestamp any record before it wrote.
    pub after: Option<DateTime<Utc>>,
    /// The start of the run the total counts, as the harness writes it.
    pub run: Option<i64>,
    pub models: BTreeMap<String, Counts>,
}

/// What some requests of one model spent, with their cache writes by TTL.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spent {
    pub counts: Counts,
    pub writes: CacheWrites,
}

impl Spent {
    fn add(&mut self, other: &Self) {
        self.counts.add(other.counts);
        self.writes.add(&other.writes);
    }
}

/// A request of a related recording, placed against the totals by its time.
#[derive(Clone, Debug, PartialEq)]
pub struct TimedRequest {
    pub id: Option<String>,
    pub at: Option<DateTime<Utc>>,
    pub model: String,
    pub counts: Counts,
}

/// A recording's recorded totals with the requests written between them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordedChain {
    /// The recording's earliest timestamp.
    pub opened: Option<DateTime<Utc>>,
    pub marks: Vec<Mark>,
    /// `explained[k]` holds the recording's own requests first written after
    /// mark `k - 1` and before mark `k`; the last entry follows the newest
    /// mark, so no total counts it.
    pub explained: Vec<BTreeMap<String, Spent>>,
    /// `stamps[k]` holds the timestamps of the records written between the
    /// same two marks; none past [`MAX_STAMPS`].
    pub stamps: Option<Vec<Vec<DateTime<Utc>>>>,
}

/// Folds a recording's timestamps, recorded totals and requests into its
/// chain, one record at a time.
#[derive(Debug)]
pub(crate) struct ChainBuilder {
    opened: Option<DateTime<Utc>>,
    newest: Option<DateTime<Utc>>,
    marks: Vec<Mark>,
    explained: Vec<BTreeMap<String, Spent>>,
    stamps: Option<Vec<Vec<DateTime<Utc>>>>,
    kept_stamps: usize,
    /// Marks merged away past [`MAX_MARKS`].
    dropped: usize,
}

impl Default for ChainBuilder {
    fn default() -> Self {
        Self {
            opened: None,
            newest: None,
            marks: Vec::new(),
            explained: vec![BTreeMap::new()],
            stamps: Some(vec![Vec::new()]),
            kept_stamps: 0,
            dropped: 0,
        }
    }
}

impl ChainBuilder {
    /// A record written at `at`.
    pub(crate) fn stamp(&mut self, at: Option<DateTime<Utc>>) {
        let Some(at) = at else { return };
        self.opened = Some(self.opened.map_or(at, |opened| opened.min(at)));
        self.newest = Some(self.newest.map_or(at, |newest| newest.max(at)));
        if self.kept_stamps == MAX_STAMPS {
            self.stamps = None;
        }
        if let Some(stamps) = &mut self.stamps {
            stamps
                .last_mut()
                .expect("a chain always has an open epoch")
                .push(at);
            self.kept_stamps += 1;
        }
    }

    /// A recorded total of `models`, counting the run that started at `run`.
    pub(crate) fn mark(&mut self, run: Option<i64>, models: BTreeMap<String, Counts>) {
        if self.marks.len() == MAX_MARKS {
            self.marks.remove(0);
            let oldest = self.explained.remove(0);
            for (model, spent) in oldest {
                self.explained[0].entry(model).or_default().add(&spent);
            }
            if let Some(stamps) = &mut self.stamps {
                let oldest = stamps.remove(0);
                stamps[0].splice(0..0, oldest);
            }
            self.dropped += 1;
        }
        self.marks.push(Mark {
            after: self.newest,
            run,
            models,
        });
        self.explained.push(BTreeMap::new());
        if let Some(stamps) = &mut self.stamps {
            stamps.push(Vec::new());
        }
    }

    /// How many totals the recording has written so far: the epoch of a
    /// request first written now.
    pub(crate) fn epoch(&self) -> usize {
        self.dropped + self.marks.len()
    }

    /// A request of `model`, first written in `epoch`.
    pub(crate) fn explain(&mut self, epoch: usize, model: &str, spent: Spent) {
        let index = epoch
            .saturating_sub(self.dropped)
            .min(self.explained.len() - 1);
        self.explained[index]
            .entry(model.to_owned())
            .or_default()
            .add(&spent);
    }

    /// The chain, or none when the recording wrote no total.
    pub(crate) fn finish(self) -> Option<RecordedChain> {
        (!self.marks.is_empty()).then_some(RecordedChain {
            opened: self.opened,
            marks: self.marks,
            explained: self.explained,
            stamps: self.stamps,
        })
    }
}

/// Whose requests a figure's unexplained part is measured against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExplainedBy {
    /// The recording's own requests.
    SessionRequests,
    /// The recording's own requests and its children's.
    FamilyRequests,
}

/// How a span or window figure holds its unexplained part.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Placement {
    /// Every interval it holds lies wholly inside it.
    Bounded,
    /// It cuts an interval and holds a prorated share of it.
    Estimated {
        basis: &'static str,
        statement: &'static str,
    },
}

impl Placement {
    fn estimated(by_records: bool) -> Self {
        if by_records {
            Self::Estimated {
                basis: PLACEMENT_BASIS,
                statement: PLACEMENT_STATEMENT,
            }
        } else {
            Self::Estimated {
                basis: PLACEMENT_BASIS_TIME,
                statement: PLACEMENT_STATEMENT_TIME,
            }
        }
    }

    fn merge(left: Option<Self>, right: Option<Self>) -> Option<Self> {
        match (left, right) {
            (Some(left @ Self::Estimated { .. }), Some(right @ Self::Estimated { .. })) => {
                Some(if left == right {
                    left
                } else {
                    Self::Estimated {
                        basis: "cost-state-interval-prorated",
                        statement: "the figures added share their cut intervals on different \
                             bases, by records and by duration",
                    }
                })
            }
            (Some(estimated @ Self::Estimated { .. }), _)
            | (_, Some(estimated @ Self::Estimated { .. })) => Some(estimated),
            (Some(Self::Bounded), _) | (_, Some(Self::Bounded)) => Some(Self::Bounded),
            (None, None) => None,
        }
    }
}

/// Why a span could not place part of the unexplained usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnplacedReason {
    /// A recorded total that no timestamped record precedes, or a recording
    /// with no timestamp to open its first interval.
    Unbounded,
    /// The span selects requests by ordinal, which places nothing in time.
    OrdinalSpan,
}

/// One model's tokens that the requests do not explain.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UnexplainedModel {
    pub model: String,
    pub tokens: Tokens,
    /// The tokens priced by the model's row, their cache writes at no
    /// recorded TTL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_price_estimate: Option<ListPriceEstimate>,
}

/// The part of a recorded total that the requests do not explain, per
/// model. A span or window figure says how it was placed and names, under
/// `unplaced`, what the split could not place; the remainder holds that.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Unexplained {
    /// Absent on a sum over figures measured against different requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explained_by: Option<ExplainedBy>,
    pub by_model: Vec<UnexplainedModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unplaced: Vec<UnexplainedModel>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unplaced_reasons: Vec<UnplacedReason>,
}

fn models(counts: &BTreeMap<String, Counts>) -> Vec<UnexplainedModel> {
    counts
        .iter()
        .filter(|(_, counts)| !counts.is_zero())
        .map(|(model, counts)| UnexplainedModel {
            model: model.clone(),
            tokens: counts.tokens(),
            list_price_estimate: None,
        })
        .collect()
}

fn add_models(into: &mut BTreeMap<String, Counts>, models: &BTreeMap<String, Counts>) {
    for (model, counts) in models {
        into.entry(model.clone()).or_default().add(*counts);
    }
}

impl Unexplained {
    /// Whether it names no tokens at all.
    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty() && self.unplaced.is_empty()
    }

    /// Every entry's tokens, the placed ones and the unplaced ones.
    pub fn entries_mut(&mut self) -> impl Iterator<Item = &mut UnexplainedModel> {
        self.by_model.iter_mut().chain(self.unplaced.iter_mut())
    }

    /// The parts of several figures added, model by model; none when none
    /// of them holds any.
    pub fn sum<'a>(parts: impl IntoIterator<Item = &'a Unexplained>) -> Option<Unexplained> {
        let mut placed = BTreeMap::new();
        let mut unplaced = BTreeMap::new();
        let mut reasons = Vec::new();
        let mut placement = None;
        let mut explained_by: Option<Option<ExplainedBy>> = None;
        for part in parts {
            explained_by = Some(match explained_by {
                None => part.explained_by,
                Some(held) => held.filter(|held| part.explained_by == Some(*held)),
            });
            for (into, entries) in [
                (&mut placed, &part.by_model),
                (&mut unplaced, &part.unplaced),
            ] {
                for entry in entries {
                    into.entry(entry.model.clone())
                        .or_insert_with(Counts::default)
                        .add(Counts::of(&entry.tokens));
                }
            }
            placement = Placement::merge(placement, part.placement);
            reasons.extend(part.unplaced_reasons.iter().copied());
        }
        let explained_by = explained_by?;
        reasons.sort();
        reasons.dedup();
        Some(Unexplained {
            explained_by,
            by_model: models(&placed),
            placement,
            unplaced: models(&unplaced),
            unplaced_reasons: reasons,
        })
    }
}

/// One interval between recorded totals, with what accrued in it per model
/// beyond the requests that explain it.
#[derive(Clone, Debug, PartialEq)]
pub struct Interval {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    /// The timestamps of the recording's records written in the interval,
    /// where the chain kept them.
    pub stamps: Option<Vec<DateTime<Utc>>>,
    pub models: BTreeMap<String, Counts>,
}

/// Where the unexplained part of a recorded total accrued.
#[derive(Clone, Debug, PartialEq)]
pub struct Accrual {
    pub explained_by: ExplainedBy,
    pub intervals: Vec<Interval>,
}

/// The unexplained part of each span, of the remainder no span holds, and
/// of the whole.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedUnexplained {
    pub spans: Vec<Unexplained>,
    pub remainder: Unexplained,
    pub whole: Unexplained,
}

impl RecordedChain {
    /// The first mark of the newest run: a total restarts where the run it
    /// names changes or any of its counters falls below the one before it.
    fn run_start(&self) -> usize {
        (1..self.marks.len())
            .rev()
            .find(|index| {
                let (before, mark) = (&self.marks[index - 1], &self.marks[*index]);
                let other_run =
                    matches!((before.run, mark.run), (Some(left), Some(right)) if left != right);
                other_run
                    || before.models.iter().any(|(model, counts)| {
                        let now = mark.models.get(model).copied().unwrap_or_default();
                        counts
                            .0
                            .iter()
                            .zip(now.0)
                            .any(|(before, now)| now < *before)
                    })
            })
            .unwrap_or(0)
    }

    /// Per model, what the newest total's own requests spent: those first
    /// written in its run, before it.
    pub fn covered(&self) -> BTreeMap<String, Spent> {
        let mut covered = BTreeMap::<String, Spent>::new();
        for epoch in &self.explained[self.run_start()..self.marks.len()] {
            for (model, spent) in epoch {
                covered.entry(model.clone()).or_default().add(spent);
            }
        }
        covered
    }

    /// Where the newest total's unexplained part accrued, explained by the
    /// recording's own requests and, where given, `family`: requests of
    /// related recordings, each counted once by id and placed by its time
    /// against the totals. A related request later than the newest total's
    /// bound is outside it, and one with no time is counted by the newest.
    pub fn accrual(&self, family: Option<&[TimedRequest]>) -> Accrual {
        let start = self.run_start();
        let newest = self.marks.len() - 1;
        let floor = start
            .checked_sub(1)
            .and_then(|index| self.marks[index].after);
        let mut related = vec![BTreeMap::<String, Counts>::new(); self.marks.len()];
        let mut seen = HashSet::new();
        for request in family.unwrap_or_default() {
            if request
                .id
                .as_ref()
                .is_some_and(|id| !seen.insert(id.clone()))
            {
                continue;
            }
            let epoch = match request.at {
                None => Some(newest),
                Some(at) if floor.is_some_and(|floor| at <= floor) => None,
                Some(at) => (start..self.marks.len())
                    .find(|index| self.marks[*index].after.is_some_and(|bound| at <= bound)),
            };
            if let Some(epoch) = epoch {
                related[epoch]
                    .entry(request.model.clone())
                    .or_default()
                    .add(request.counts);
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for ((mark, explained), related) in self.marks[start..]
            .iter()
            .zip(&self.explained[start..])
            .zip(&related[start..])
        {
            names.extend(mark.models.keys().cloned());
            names.extend(explained.keys().cloned());
            names.extend(related.keys().cloned());
        }
        let mut intervals = (start..self.marks.len())
            .map(|index| Interval {
                from: match index.checked_sub(1) {
                    Some(before) => self.marks[before].after.or(self.opened),
                    None => self.opened,
                },
                to: self.marks[index].after,
                stamps: self.stamps.as_ref().map(|stamps| stamps[index].clone()),
                models: BTreeMap::new(),
            })
            .collect::<Vec<_>>();
        for name in &names {
            for counter in 0..5 {
                let recorded = |index: usize| {
                    self.marks[index]
                        .models
                        .get(name)
                        .map_or(0, |counts| counts.0[counter]) as i128
                };
                let explained = |index: usize| {
                    let own = self.explained[index]
                        .get(name)
                        .map_or(0, |spent| spent.counts.0[counter]);
                    let related = related[index]
                        .get(name)
                        .map_or(0, |counts| counts.0[counter]);
                    own as i128 + related as i128
                };
                let total = (start..self.marks.len()).map(explained).sum::<i128>();
                let unexplained = (recorded(newest) - total).max(0);
                let (mut held, mut placed) = (0i128, 0i128);
                for (offset, index) in (start..self.marks.len()).enumerate() {
                    held += explained(index);
                    let next = (recorded(index) - held).max(placed).min(unexplained);
                    let increase = (next - placed) as u64;
                    placed = next;
                    if increase > 0 {
                        intervals[offset].models.entry(name.clone()).or_default().0[counter] +=
                            increase;
                    }
                }
            }
        }
        Accrual {
            explained_by: if family.is_some() {
                ExplainedBy::FamilyRequests
            } else {
                ExplainedBy::SessionRequests
            },
            intervals,
        }
    }
}

impl Accrual {
    fn total(&self) -> BTreeMap<String, Counts> {
        let mut total = BTreeMap::new();
        for interval in &self.intervals {
            add_models(&mut total, &interval.models);
        }
        total
    }

    /// The whole session's unexplained part; none when the requests explain
    /// all of the total.
    pub fn whole(&self) -> Option<Unexplained> {
        let by_model = models(&self.total());
        (!by_model.is_empty()).then_some(Unexplained {
            explained_by: Some(self.explained_by),
            by_model,
            placement: None,
            unplaced: Vec::new(),
            unplaced_reasons: Vec::new(),
        })
    }

    /// Place every interval's part in the spans that hold it. A time span
    /// holds the part of each interval inside it, whole where the interval
    /// lies inside it and, where it cuts the interval, prorated by the
    /// recording's records in the interval it holds, or by duration where the
    /// chain kept no record timestamps;
    /// what no span holds is the remainder's. An interval no timestamped
    /// record bounds, and every interval for a span selecting by ordinal,
    /// stays whole with the remainder and is named `unplaced` in the span.
    /// Over spans that share no time, the spans and the remainder add up to
    /// the whole per model and counter.
    pub fn place(&self, spans: &[SpanBounds]) -> Option<PlacedUnexplained> {
        let whole = self.whole()?;
        let figures = spans.len() + 1;
        let remainder = spans.len();
        let mut placed = vec![BTreeMap::<String, Counts>::new(); figures];
        let mut unplaced = vec![BTreeMap::<String, Counts>::new(); figures];
        let mut reasons = vec![Vec::<UnplacedReason>::new(); figures];
        let mut cut = vec![false; figures];
        let mut held_any = vec![false; figures];
        let mut by_records = vec![true; figures];
        let times = spans
            .iter()
            .map(|span| match *span {
                SpanBounds::Time { from, to } => Some((from, to)),
                SpanBounds::Requests { .. } => None,
            })
            .collect::<Vec<_>>();
        for interval in &self.intervals {
            if interval.models.values().all(|counts| counts.is_zero()) {
                continue;
            }
            let (Some(low), Some(high)) = (interval.from, interval.to) else {
                add_models(&mut placed[remainder], &interval.models);
                reasons[remainder].push(UnplacedReason::Unbounded);
                for index in 0..spans.len() {
                    add_models(&mut unplaced[index], &interval.models);
                    reasons[index].push(UnplacedReason::Unbounded);
                }
                continue;
            };
            for (index, time) in times.iter().enumerate() {
                if time.is_none() {
                    add_models(&mut unplaced[index], &interval.models);
                    reasons[index].push(UnplacedReason::OrdinalSpan);
                }
            }
            let segments = match &interval.stamps {
                Some(stamps) => record_segments(stamps, high, &times),
                None => segments(low, high, &times),
            };
            let interval_by_records = interval.stamps.is_some();
            let weights = segments
                .iter()
                .map(|segment| segment.weight)
                .collect::<Vec<_>>();
            let mut holds = vec![0usize; figures];
            for segment in &segments {
                if segment.holders.is_empty() {
                    holds[remainder] += 1;
                }
                for holder in &segment.holders {
                    holds[*holder] += 1;
                }
            }
            for (figure, count) in holds.iter().enumerate() {
                if *count > 0 {
                    held_any[figure] = true;
                    if *count < segments.len() {
                        cut[figure] = true;
                        by_records[figure] &= interval_by_records;
                    }
                }
            }
            for (model, counts) in &interval.models {
                let mut shares = vec![Counts::default(); segments.len()];
                for counter in 0..5 {
                    for (share, amount) in shares
                        .iter_mut()
                        .zip(apportion(counts.0[counter], &weights))
                    {
                        share.0[counter] = amount;
                    }
                }
                for (segment, share) in segments.iter().zip(shares) {
                    let holders = if segment.holders.is_empty() {
                        vec![remainder]
                    } else {
                        segment.holders.clone()
                    };
                    for holder in holders {
                        placed[holder].entry(model.clone()).or_default().add(share);
                    }
                }
            }
        }
        let mut figures = (0..figures)
            .map(|index| {
                let mut reasons = std::mem::take(&mut reasons[index]);
                reasons.sort();
                reasons.dedup();
                Unexplained {
                    explained_by: Some(self.explained_by),
                    by_model: models(&placed[index]),
                    placement: Some(if cut[index] && held_any[index] {
                        Placement::estimated(by_records[index])
                    } else {
                        Placement::Bounded
                    }),
                    unplaced: models(&unplaced[index]),
                    unplaced_reasons: reasons,
                }
            })
            .collect::<Vec<_>>();
        let remainder = figures
            .pop()
            .expect("a placement always fills the remainder");
        Some(PlacedUnexplained {
            spans: figures,
            remainder,
            whole,
        })
    }
}

/// A time span's bounds, either open; `None` for a span by ordinal.
type TimeBounds = Option<(Option<DateTime<Utc>>, Option<DateTime<Utc>>)>;

/// A stretch of an interval and the spans that hold all of it.
struct Segment {
    weight: u64,
    holders: Vec<usize>,
}

/// The interval `[low, high]` cut at every time span's bound inside it, each
/// stretch weighted by its duration in milliseconds. An instant interval is
/// one stretch, held by the spans that hold the instant.
fn segments(low: DateTime<Utc>, high: DateTime<Utc>, times: &[TimeBounds]) -> Vec<Segment> {
    if high <= low {
        let holders = times
            .iter()
            .enumerate()
            .filter_map(|(index, time)| {
                let (from, to) = (*time)?;
                (from.is_none_or(|from| from <= high) && to.is_none_or(|to| high < to))
                    .then_some(index)
            })
            .collect();
        return vec![Segment { weight: 1, holders }];
    }
    let mut points = vec![low, high];
    for (from, to) in times.iter().flatten() {
        for point in [from, to].into_iter().flatten() {
            if low < *point && *point < high {
                points.push(*point);
            }
        }
    }
    points.sort();
    points.dedup();
    points
        .windows(2)
        .map(|pair| {
            let (start, end) = (pair[0], pair[1]);
            let holders = times
                .iter()
                .enumerate()
                .filter_map(|(index, time)| {
                    let (from, to) = (*time)?;
                    (from.is_none_or(|from| from <= start) && to.is_none_or(|to| end <= to))
                        .then_some(index)
                })
                .collect();
            Segment {
                weight: (end - start).num_milliseconds().max(0) as u64,
                holders,
            }
        })
        .collect()
}

/// The recording's records in an interval grouped by the time spans holding
/// each, every group weighted by its count. An interval no record falls in
/// is the instant `high`, held by the spans that hold it.
fn record_segments(
    stamps: &[DateTime<Utc>],
    high: DateTime<Utc>,
    times: &[TimeBounds],
) -> Vec<Segment> {
    let holders_at = |at: DateTime<Utc>| {
        times
            .iter()
            .enumerate()
            .filter_map(|(index, time)| {
                let (from, to) = (*time)?;
                (from.is_none_or(|from| from <= at) && to.is_none_or(|to| at < to)).then_some(index)
            })
            .collect::<Vec<_>>()
    };
    if stamps.is_empty() {
        return vec![Segment {
            weight: 1,
            holders: holders_at(high),
        }];
    }
    let mut groups = BTreeMap::<Vec<usize>, u64>::new();
    for at in stamps {
        *groups.entry(holders_at(*at)).or_default() += 1;
    }
    groups
        .into_iter()
        .map(|(holders, weight)| Segment { weight, holders })
        .collect()
}

/// `total` divided over `weights` in proportion, whole tokens summing to it
/// exactly: each part rounded down, and what that leaves given one by one to
/// the parts with the largest fractions, earlier parts first on a tie.
fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    let sum = weights
        .iter()
        .map(|weight| u128::from(*weight))
        .sum::<u128>();
    let mut shares = vec![0u64; weights.len()];
    if sum == 0 {
        if let Some(first) = shares.first_mut() {
            *first = total;
        }
        return shares;
    }
    let exact = |index: usize| u128::from(total) * u128::from(weights[index]);
    for (index, share) in shares.iter_mut().enumerate() {
        *share = (exact(index) / sum) as u64;
    }
    let mut left = total - shares.iter().sum::<u64>();
    let mut order = (0..weights.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| std::cmp::Reverse(exact(*index) % sum));
    for index in order {
        if left == 0 {
            break;
        }
        shares[index] += 1;
        left -= 1;
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apportion_conserves_and_follows_weights() {
        assert_eq!(apportion(10, &[1, 1, 1]), vec![4, 3, 3]);
        assert_eq!(apportion(7, &[0, 0]), vec![7, 0]);
        assert_eq!(apportion(100, &[3, 1]), vec![75, 25]);
        let shares = apportion(1_000_003, &[7, 13, 29, 1]);
        assert_eq!(shares.iter().sum::<u64>(), 1_000_003);
    }
}
