//! A session's consumption divided among the work its evidence names.
//!
//! Evidence comes as spans and events. A span is interval evidence: work from
//! its start to its end, open while it has none. An event is point evidence:
//! the session acted on an item at an instant, and its events on one item
//! span that item from the first to the last, inclusive. The session's
//! timeline is cut at every span edge into disjoint segments, each read once,
//! so no request is counted twice. A segment one item covers is that item's
//! share; one several items cover, and one no item covers, is nobody's, with
//! a fixed [`Reason`].
//!
//! Beside that conserving partition stand two other layers. An estimate cuts
//! each segment no item covers by the session's working stretches and its
//! own events in them. An allocation gives what no single item holds to items
//! by a stated [`Allocation`]. Every layer is computed over every item the
//! session has evidence for, and a view only selects among the shares, so an
//! item's share never depends on what else is selected.
//!
//! The module never reads a figure itself. A [`Plan`] names the bounds to
//! read; the caller reads one figure per bound, of any type implementing
//! [`Figure`], and [`Figures::parts`] hands back the figures a share or a
//! remainder is made of, for the caller to add up with the primitives at the
//! end of this module.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::span::{Instant, Placer, RequestTimeline, SpanBounds};

/// What establishes a piece of evidence, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Bound,
    Matched,
    Estimated,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Matched => "matched",
            Self::Estimated => "estimated",
        }
    }
}

/// How a share was given to its item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Basis {
    /// A segment one item's interval evidence alone covers.
    Span,
    /// A segment one item's events, first to last, alone cover.
    EventWindow,
    /// Part of a segment no item covers, given by the session's own event in
    /// the same working stretch.
    WorkingStretch,
    /// A child's part of its parent's share, by request time.
    Inherited,
    /// An even share of a segment several items cover.
    Even,
    /// What lies between items, given to the next item.
    Following,
}

impl Basis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Span => "span",
            Self::EventWindow => "event-window",
            Self::WorkingStretch => "working-stretch",
            Self::Inherited => "inherited",
            Self::Even => "even",
            Self::Following => "following",
        }
    }
}

/// The tier and basis a share carries. Ordered strongest first: by tier, then
/// interval evidence before event windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Evidence {
    pub tier: Tier,
    pub basis: Basis,
}

/// Why part of a session is in no selected item's share. Ordered as the
/// reasons are listed: the segment reasons by name, then untimed requests,
/// then the selection, then the recorded total.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    AfterLastItem,
    BeforeFirstItem,
    BetweenItems,
    Overlap,
    RequestTimeUnrecorded,
    UnselectedItem,
    RecordedTotalNotSplittable,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overlap => "overlap",
            Self::BeforeFirstItem => "before first item",
            Self::BetweenItems => "between items",
            Self::AfterLastItem => "after last item",
            Self::RequestTimeUnrecorded => "request time unrecorded",
            Self::RecordedTotalNotSplittable => "recorded total not splittable",
            Self::UnselectedItem => "unselected item",
        }
    }
}

impl Serialize for Reason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// How what no single item holds is allocated beside the conserving figures:
/// a segment several items cover evenly among them, or what lies between
/// items to the next item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Allocation {
    Even,
    Following,
}

impl Allocation {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "even" => Some(Self::Even),
            "following" => Some(Self::Following),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Even => "even",
            Self::Following => "following",
        }
    }
}

/// Which layer of a divided session a figure takes: the segments an item
/// alone covers; those and the estimate parts given to it; or an allocation.
/// An even allocation adds even shares of overlapped segments to the
/// conserving shares; a following allocation adds, to the estimated shares,
/// what lies between items.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Take {
    Conserving,
    Estimated,
    Allocated(Allocation),
}

/// Interval evidence: the session worked on `work` from `from` to `to`,
/// exclusive, open while it has no end. `run` names the run the span is,
/// where a ledger dispatched one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Span<K> {
    pub work: K,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    pub from: Instant,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Instant>,
    pub tier: Tier,
}

/// Point evidence: the session acted on `work` at `at`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Event<K> {
    pub at: Instant,
    pub work: K,
    pub tier: Tier,
}

/// One item covering a segment, with the strongest evidence that covers it
/// there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Cover<K> {
    pub work: K,
    pub evidence: Evidence,
}

/// One disjoint segment `[from, to)` of a session's timeline, an absent bound
/// being the timeline's beginning or end, and what covers all of it: its
/// items in key order, and its runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Segment<K> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Instant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Instant>,
    pub work: Vec<Cover<K>>,
    pub runs: Vec<String>,
}

impl<K> Segment<K> {
    pub fn bounds(&self) -> SpanBounds {
        SpanBounds::Time {
            from: self.from,
            to: self.to,
        }
    }

    /// The one item that covers the segment, if exactly one does.
    pub fn alone(&self) -> Option<&Cover<K>> {
        match &self.work[..] {
            [one] => Some(one),
            _ => None,
        }
    }
}

/// One part `[from, to)` of segment `segment`, which no item covers, as the
/// session's working stretches and its own events in them cut it, and the
/// item and tier of the event that serves it: none for a stretch with no
/// event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Window<K> {
    pub segment: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Instant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Instant>,
    pub work: Option<(K, Tier)>,
}

impl<K> Window<K> {
    pub fn bounds(&self) -> SpanBounds {
        SpanBounds::Time {
            from: self.from,
            to: self.to,
        }
    }
}

/// The span an item's events give it: its first to its last event,
/// inclusive, so the end is one nanosecond past the last. Its tier is the
/// weakest of those events, since the window rests on all of them.
fn event_spans<K: Ord + Clone>(events: &[Event<K>]) -> Vec<Edge<K>> {
    let mut windows: BTreeMap<&K, (Instant, Instant, Tier)> = BTreeMap::new();
    for event in events {
        windows
            .entry(&event.work)
            .and_modify(|(first, last, tier)| {
                *first = (*first).min(event.at);
                *last = (*last).max(event.at);
                *tier = (*tier).max(event.tier);
            })
            .or_insert((event.at, event.at, event.tier));
    }
    windows
        .into_iter()
        .map(|(work, (first, last, tier))| Edge {
            work: work.clone(),
            run: None,
            from: first,
            to: Some(last + chrono::Duration::nanoseconds(1)),
            evidence: Evidence {
                tier,
                basis: Basis::EventWindow,
            },
        })
        .collect()
}

/// A span or an item's event window, as the timeline is cut by it.
struct Edge<K> {
    work: K,
    run: Option<String>,
    from: Instant,
    to: Option<Instant>,
    evidence: Evidence,
}

impl<K> Edge<K> {
    fn covers(&self, from: Option<Instant>, to: Option<Instant>) -> bool {
        from.is_some_and(|from| self.from <= from)
            && self.to.is_none_or(|end| to.is_some_and(|to| to <= end))
    }
}

/// The session's timeline cut at every edge of its spans and of its items'
/// event windows: the segment before the first edge, one between each pair,
/// and the one after the last, each with what covers all of it.
pub fn segments<K: Ord + Clone>(spans: &[Span<K>], events: &[Event<K>]) -> Vec<Segment<K>> {
    let edges: Vec<Edge<K>> = spans
        .iter()
        .map(|span| Edge {
            work: span.work.clone(),
            run: span.run.clone(),
            from: span.from,
            to: span.to,
            evidence: Evidence {
                tier: span.tier,
                basis: Basis::Span,
            },
        })
        .chain(event_spans(events))
        .collect();
    let cuts: BTreeSet<Instant> = edges
        .iter()
        .flat_map(|edge| std::iter::once(edge.from).chain(edge.to))
        .collect();
    let mut bounds: Vec<Option<Instant>> = vec![None];
    bounds.extend(cuts.into_iter().map(Some));
    bounds.push(None);
    bounds
        .windows(2)
        .map(|pair| {
            let (from, to) = (pair[0], pair[1]);
            let mut work: BTreeMap<K, Evidence> = BTreeMap::new();
            let mut runs: BTreeSet<String> = BTreeSet::new();
            for edge in edges.iter().filter(|edge| edge.covers(from, to)) {
                work.entry(edge.work.clone())
                    .and_modify(|held| *held = (*held).min(edge.evidence))
                    .or_insert(edge.evidence);
                runs.extend(edge.run.clone());
            }
            Segment {
                from,
                to,
                work: work
                    .into_iter()
                    .map(|(work, evidence)| Cover { work, evidence })
                    .collect(),
                runs: runs.into_iter().collect(),
            }
        })
        .collect()
}

/// The later of two starts, an absent one being the timeline's beginning.
fn later(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, None) | (None, a) => a,
    }
}

/// The earlier of two ends, an absent one being the timeline's end.
fn earlier(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) | (None, a) => a,
    }
}

fn nonempty(from: Option<Instant>, to: Option<Instant>) -> bool {
    from.zip(to).is_none_or(|(from, to)| from < to)
}

/// One part `[from, to)` of a working stretch and the item and tier that
/// serve it.
type Cut<K> = (Option<Instant>, Option<Instant>, Option<(K, Tier)>);

/// The estimate parts of every segment no item covers. Working stretch `i`
/// holds the requests from its first stamp, `starts[i]`, to the next
/// stretch's, the first from the timeline's beginning and the last to its
/// end, as [`crate::stretch`] places requests in stretches, and the events in
/// that window are the stretch's. Within a stretch, the requests up to and
/// including an event serve that event's item, and those after its last
/// event serve the same item; a stretch with no event serves none. Events at
/// one instant are taken in key order. A segment any item covers is left to
/// its evidence.
pub fn estimate_windows<K: Ord + Clone>(
    segments: &[Segment<K>],
    starts: &[Instant],
    events: &[Event<K>],
) -> Vec<Window<K>> {
    let mut ordered: Vec<&Event<K>> = events.iter().collect();
    ordered.sort();
    let mut pieces: Vec<Cut<K>> = Vec::new();
    for index in 0..starts.len() {
        let from = (index > 0).then(|| starts[index]);
        let to = starts.get(index + 1).copied();
        let inside: Vec<&&Event<K>> = ordered
            .iter()
            .filter(|event| {
                from.is_none_or(|from| event.at >= from) && to.is_none_or(|to| event.at < to)
            })
            .collect();
        let Some(last) = inside.last() else {
            pieces.push((from, to, None));
            continue;
        };
        let mut cursor = from;
        for event in &inside {
            let cut = Some(event.at + chrono::Duration::nanoseconds(1));
            pieces.push((cursor, cut, Some((event.work.clone(), event.tier))));
            cursor = cut;
        }
        pieces.push((cursor, to, Some((last.work.clone(), last.tier))));
    }
    let mut windows: Vec<Window<K>> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        if !segment.work.is_empty() {
            continue;
        }
        for (from, to, work) in &pieces {
            let from = later(segment.from, *from);
            let to = earlier(segment.to, *to);
            if !nonempty(from, to) {
                continue;
            }
            match windows.last_mut() {
                Some(last) if last.segment == index && &last.work == work && last.to == from => {
                    last.to = to;
                }
                _ => windows.push(Window {
                    segment: index,
                    from,
                    to,
                    work: work.clone(),
                }),
            }
        }
    }
    windows
}

/// How a session is read: its segments, and the estimate windows that cut
/// those no item covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Plan<K> {
    pub segments: Vec<Segment<K>>,
    pub windows: Vec<Window<K>>,
}

/// How many requests each part of a plan holds: one count per segment, one
/// per window, and the requests no segment holds, whose time the recording
/// leaves unrecorded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Requests {
    pub segments: Vec<u64>,
    pub windows: Vec<u64>,
    pub remainder: u64,
}

impl<K: Ord + Clone> Plan<K> {
    /// The plan of one session's evidence. `starts` are the first stamps of
    /// its working stretches at the idle gap the estimate uses, in order, as
    /// [`crate::usage::stretches`] gives them; with none, or with no event to
    /// estimate by, the plan has no windows.
    pub fn new(spans: &[Span<K>], events: &[Event<K>], starts: Option<&[Instant]>) -> Self {
        Self::with_cues(spans, events, &[], starts)
    }

    /// As [`Plan::new`], with cues beside the events. A cue is point evidence
    /// that only estimates: it cuts the working stretch it falls in exactly as
    /// an event does, and gives its item no event window, so it covers no
    /// segment.
    pub fn with_cues(
        spans: &[Span<K>],
        events: &[Event<K>],
        cues: &[Event<K>],
        starts: Option<&[Instant]>,
    ) -> Self {
        let segments = segments(spans, events);
        let windows = match starts {
            Some(starts) if !events.is_empty() || !cues.is_empty() => {
                let cutting: Vec<Event<K>> = events.iter().chain(cues).cloned().collect();
                estimate_windows(&segments, starts, &cutting)
            }
            _ => Vec::new(),
        };
        Self { segments, windows }
    }
}

impl<K> Plan<K> {
    /// The bounds a read takes one figure for: every segment, then every
    /// window. The read's remainder holds the requests no segment holds.
    pub fn bounds(&self) -> Vec<SpanBounds> {
        self.segments
            .iter()
            .map(Segment::bounds)
            .chain(self.windows.iter().map(Window::bounds))
            .collect()
    }

    /// Whether the estimate gives any part to an item.
    pub fn estimates(&self) -> bool {
        self.windows.iter().any(|window| window.work.is_some())
    }

    /// The requests of `timeline` each part of the plan holds, placed as a
    /// span read places them.
    pub fn place(&self, timeline: &RequestTimeline) -> Requests {
        let bounds = self.bounds();
        let mut placer = Placer::new(&bounds).keeping(false);
        for request in &timeline.requests {
            placer.request(request.timestamp, request.id.as_deref());
        }
        let counts = placer.finish().requests();
        let count = |index: usize| counts.get(index).copied().unwrap_or(0) as u64;
        let segments = self.segments.len();
        Requests {
            segments: (0..segments).map(count).collect(),
            windows: (segments..bounds.len()).map(count).collect(),
            remainder: count(bounds.len()),
        }
    }

    /// Why segment `index` is no single item's, or `None` when one item
    /// covers it.
    pub fn reason(&self, index: usize) -> Option<Reason> {
        match self.segments[index].work.len() {
            1 => None,
            0 => {
                let covered = |segment: &Segment<K>| !segment.work.is_empty();
                if !self.segments[..index].iter().any(covered) {
                    Some(Reason::BeforeFirstItem)
                } else if !self.segments[index + 1..].iter().any(covered) {
                    Some(Reason::AfterLastItem)
                } else {
                    Some(Reason::BetweenItems)
                }
            }
            _ => Some(Reason::Overlap),
        }
    }

    /// The first segment after `index` that one item alone covers.
    fn next_item(&self, index: usize) -> Option<&Cover<K>> {
        self.segments[index + 1..].iter().find_map(Segment::alone)
    }

    /// One run's parts: the segments that run alone covers.
    pub fn run_share(&self, run: &str, requests: &Requests) -> Vec<Part> {
        self.segments
            .iter()
            .enumerate()
            .filter(|(_, segment)| matches!(&segment.runs[..], [one] if one == run))
            .map(|(index, _)| Part {
                piece: Piece::Segment(index),
                requests: requests.segments.get(index).copied().unwrap_or(0),
            })
            .collect()
    }
}

/// One piece of a session's figure, by where the read holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Piece {
    Segment(usize),
    Window(usize),
    /// Share `index` of `shares` even shares of a segment.
    Even {
        segment: usize,
        index: usize,
        shares: usize,
    },
    /// The requests no segment holds.
    Remainder,
}

/// A piece and the requests it holds; an even share holds every request of
/// its segment, a share of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Part {
    pub piece: Piece,
    pub requests: u64,
}

/// Whom a part of a layer is given to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Owner<K> {
    Item { work: K, evidence: Evidence },
    Unattributed(Reason),
}

/// One item's share in one layer by one tier and basis, in read order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Share<K> {
    pub work: K,
    pub evidence: Evidence,
    pub parts: Vec<Part>,
}

/// What a layer gives no item, for one reason, in read order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Remainder {
    pub reason: Reason,
    pub requests: u64,
    pub parts: Vec<Part>,
}

/// One layer of a divided session: every piece of the session's figure, in
/// read order, each given to an item or left unattributed, so the layer's
/// parts add up to the whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Layer<K> {
    pub take: Take,
    pub parts: Vec<(Part, Owner<K>)>,
}

impl<K: Ord + Clone> Layer<K> {
    /// Every item's shares, by item and then evidence, strongest first.
    pub fn shares(&self) -> Vec<Share<K>> {
        let mut held: BTreeMap<(&K, Evidence), Vec<Part>> = BTreeMap::new();
        for (part, owner) in &self.parts {
            if let Owner::Item { work, evidence } = owner {
                held.entry((work, *evidence)).or_default().push(*part);
            }
        }
        held.into_iter()
            .map(|((work, evidence), parts)| Share {
                work: work.clone(),
                evidence,
                parts,
            })
            .collect()
    }

    /// What no item holds, by reason. A segment reason is listed wherever a
    /// segment carries it, holding requests or not; untimed requests are
    /// listed where there are any.
    pub fn remainders(&self) -> Vec<Remainder> {
        let mut held: BTreeMap<Reason, Vec<Part>> = BTreeMap::new();
        for (part, owner) in &self.parts {
            if let Owner::Unattributed(reason) = owner {
                held.entry(*reason).or_default().push(*part);
            }
        }
        held.into_iter()
            .map(|(reason, parts)| Remainder {
                reason,
                requests: parts.iter().map(|part| part.requests).sum(),
                parts,
            })
            .filter(|remainder| {
                remainder.reason != Reason::RequestTimeUnrecorded || remainder.requests > 0
            })
            .collect()
    }

    /// The parts whose owner `keep` holds, in read order.
    pub fn parts_of(&self, keep: impl Fn(&Owner<K>) -> bool) -> Vec<Part> {
        self.parts
            .iter()
            .filter(|(_, owner)| keep(owner))
            .map(|(part, _)| *part)
            .collect()
    }

    /// The parts of the items `selected` holds, in read order.
    pub fn figure(&self, selected: &BTreeSet<K>) -> Vec<Part> {
        self.parts_of(|owner| matches!(owner, Owner::Item { work, .. } if selected.contains(work)))
    }

    /// The parts no item holds, in read order.
    pub fn unattributed(&self) -> Vec<Part> {
        self.parts_of(|owner| matches!(owner, Owner::Unattributed(_)))
    }

    /// The reasons of the unattributed parts that hold a request.
    pub fn reasons(&self) -> Vec<Reason> {
        self.remainders()
            .into_iter()
            .filter(|remainder| remainder.requests > 0)
            .map(|remainder| remainder.reason)
            .collect()
    }

    /// A view of the layer that selects `selected`: their parts, and apart
    /// from them, the shares of every item it does not select.
    pub fn select(&self, selected: &BTreeSet<K>) -> Selection<K> {
        Selection {
            figure: self.figure(selected),
            unselected: self
                .shares()
                .into_iter()
                .filter(|share| !selected.contains(&share.work))
                .collect(),
        }
    }
}

/// A layer as a view selects it: the selected items' parts, which make up
/// its figure, and the unselected items' shares, outside that figure for
/// [`Reason::UnselectedItem`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Selection<K> {
    pub figure: Vec<Part>,
    pub unselected: Vec<Share<K>>,
}

/// A divided session's layers: the conserving partition, the estimate, and
/// the allocation where one is asked for. A plan without windows estimates
/// nothing, so its estimated layer holds the conserving shares alone.
/// `splittable` is false where the whole in the segments' basis is not the
/// session's own figure, so part of that figure is in no segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attribution<K> {
    pub conserving: Layer<K>,
    pub estimated: Layer<K>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocated: Option<Layer<K>>,
    pub splittable: bool,
}

impl<K: Ord + Clone> Attribution<K> {
    /// The layer `take` names, where the session has it.
    pub fn layer(&self, take: Take) -> Option<&Layer<K>> {
        match take {
            Take::Conserving => Some(&self.conserving),
            Take::Estimated => Some(&self.estimated),
            Take::Allocated(_) => self.allocated.as_ref().filter(|layer| layer.take == take),
        }
    }

    /// What the conserving figure of `selected` leaves out, with why: the
    /// unattributed parts and the unselected items' shares, or `None` when
    /// nothing is left out.
    pub fn outside(&self, selected: &BTreeSet<K>) -> Option<(Vec<Part>, Vec<Reason>)> {
        let layer = &self.conserving;
        let mut reasons = layer.reasons();
        let unselected = |owner: &Owner<K>| matches!(owner, Owner::Item { work, .. } if !selected.contains(work));
        if layer
            .parts
            .iter()
            .any(|(part, owner)| part.requests > 0 && unselected(owner))
        {
            reasons.push(Reason::UnselectedItem);
        }
        if !self.splittable {
            reasons.push(Reason::RecordedTotalNotSplittable);
        }
        if reasons.is_empty() {
            return None;
        }
        let parts = layer
            .parts
            .iter()
            .filter(|(_, owner)| match owner {
                Owner::Item { work, .. } => !selected.contains(work),
                Owner::Unattributed(_) => true,
            })
            .map(|(part, _)| *part)
            .collect();
        Some((parts, reasons))
    }

    /// What the conserving partition gives no item, with why, or `None` when
    /// it gives every part to an item.
    pub fn unattributed(&self) -> Option<(Vec<Part>, Vec<Reason>)> {
        let mut reasons = self.conserving.reasons();
        if !self.splittable {
            reasons.push(Reason::RecordedTotalNotSplittable);
        }
        (!reasons.is_empty()).then(|| (self.conserving.unattributed(), reasons))
    }
}

impl<K: Ord + Clone> Plan<K> {
    /// The session's layers from the requests each part of the plan holds,
    /// with the allocation `method` asks for: none, even or following.
    /// `None` when `requests` does not hold one count per segment and per
    /// window.
    pub fn attribute(
        &self,
        requests: &Requests,
        method: Option<Allocation>,
        splittable: bool,
    ) -> Option<Attribution<K>> {
        if requests.segments.len() != self.segments.len()
            || requests.windows.len() != self.windows.len()
        {
            return None;
        }
        Some(Attribution {
            conserving: self.layer(Take::Conserving, requests),
            estimated: self.layer(Take::Estimated, requests),
            allocated: method.map(|method| self.layer(Take::Allocated(method), requests)),
            splittable,
        })
    }

    fn layer(&self, take: Take, requests: &Requests) -> Layer<K> {
        let mut parts: Vec<(Part, Owner<K>)> = Vec::new();
        let item = |cover: &Cover<K>, basis: Option<Basis>| Owner::Item {
            work: cover.work.clone(),
            evidence: Evidence {
                tier: cover.evidence.tier,
                basis: basis.unwrap_or(cover.evidence.basis),
            },
        };
        for (index, segment) in self.segments.iter().enumerate() {
            let whole = Part {
                piece: Piece::Segment(index),
                requests: requests.segments[index],
            };
            if let Some(one) = segment.alone() {
                parts.push((whole, item(one, None)));
                continue;
            }
            let reason = self.reason(index).expect("a segment no single item covers");
            let estimating = matches!(
                take,
                Take::Estimated | Take::Allocated(Allocation::Following)
            );
            match take {
                Take::Allocated(Allocation::Even) if segment.work.len() > 1 => {
                    let shares = segment.work.len();
                    for (share, cover) in segment.work.iter().enumerate() {
                        let part = Part {
                            piece: Piece::Even {
                                segment: index,
                                index: share,
                                shares,
                            },
                            requests: whole.requests,
                        };
                        parts.push((part, item(cover, Some(Basis::Even))));
                    }
                }
                _ if estimating => {
                    let next = (take == Take::Allocated(Allocation::Following)
                        && reason == Reason::BetweenItems)
                        .then(|| self.next_item(index))
                        .flatten();
                    let leftover = || match next {
                        Some(cover) => item(cover, Some(Basis::Following)),
                        None => Owner::Unattributed(reason),
                    };
                    let windows: Vec<usize> = self
                        .windows
                        .iter()
                        .enumerate()
                        .filter(|(_, window)| window.segment == index)
                        .map(|(window, _)| window)
                        .collect();
                    if windows.is_empty() {
                        parts.push((whole, leftover()));
                        continue;
                    }
                    for window in windows {
                        let part = Part {
                            piece: Piece::Window(window),
                            requests: requests.windows[window],
                        };
                        let owner = match &self.windows[window].work {
                            Some((work, tier)) => Owner::Item {
                                work: work.clone(),
                                evidence: Evidence {
                                    tier: *tier,
                                    basis: Basis::WorkingStretch,
                                },
                            },
                            None => leftover(),
                        };
                        parts.push((part, owner));
                    }
                }
                _ => parts.push((whole, Owner::Unattributed(reason))),
            }
        }
        // Even shares are read after every whole segment.
        parts.sort_by_key(|(part, _)| matches!(part.piece, Piece::Even { .. }));
        parts.push((
            Part {
                piece: Piece::Remainder,
                requests: requests.remainder,
            },
            Owner::Unattributed(Reason::RequestTimeUnrecorded),
        ));
        Layer { take, parts }
    }
}

/// A figure a read takes per bound, and the even share of it each of
/// several items holds.
pub trait Figure: Clone {
    /// The `index`th of `shares` even shares; see [`even_split`].
    fn even_share(&self, shares: usize, index: usize) -> Self;
}

/// The figures one read took for a plan: one per segment, one per window,
/// and the remainder where the read has one.
#[derive(Clone, Debug, PartialEq)]
pub struct Figures<F> {
    pub segments: Vec<F>,
    pub windows: Vec<F>,
    pub remainder: Option<F>,
}

impl<F: Figure> Figures<F> {
    /// The figures `parts` are made of, each with the requests it holds, in
    /// the parts' order; a remainder the read does not have adds nothing.
    /// `None` when a part names a figure the read does not hold.
    pub fn parts(&self, parts: &[Part]) -> Option<Vec<(Cow<'_, F>, u64)>> {
        let mut held = Vec::with_capacity(parts.len());
        for part in parts {
            let figure = match part.piece {
                Piece::Segment(index) => Cow::Borrowed(self.segments.get(index)?),
                Piece::Window(index) => Cow::Borrowed(self.windows.get(index)?),
                Piece::Even {
                    segment,
                    index,
                    shares,
                } => Cow::Owned(self.segments.get(segment)?.even_share(shares, index)),
                Piece::Remainder => match &self.remainder {
                    Some(remainder) => Cow::Borrowed(remainder),
                    None => continue,
                },
            };
            held.push((figure, part.requests));
        }
        Some(held)
    }
}

/// The `index`th of `shares` even shares of `count` tokens. Tokens stay
/// whole: each share is the quotient, and the first shares take one token
/// each of the remainder, so the shares add up to the count.
pub fn even_split(count: u64, shares: usize, index: usize) -> u64 {
    let shares = shares.max(1) as u64;
    count / shares + u64::from((index as u64) < count % shares)
}

/// One of `shares` even shares of an amount of money.
pub fn even_amount(amount: f64, shares: usize) -> f64 {
    amount / shares.max(1) as u64 as f64
}

/// `sum` with a part's counter added. A part with no request that recorded
/// no counter adds zero; a part with requests that recorded none leaves the
/// sum absent, as does an absent sum or an overflow.
pub fn add_count(sum: Option<u64>, part: Option<u64>, requests: u64) -> Option<u64> {
    let part = match part {
        None if requests == 0 => 0,
        part => part?,
    };
    sum?.checked_add(part)
}

/// Parts' counters added up from zero, each with the requests its part
/// holds; see [`add_count`].
pub fn sum_counts(parts: impl IntoIterator<Item = (Option<u64>, u64)>) -> Option<u64> {
    parts.into_iter().try_fold(0, |sum, (part, requests)| {
        add_count(Some(sum), part, requests)
    })
}

/// Parts' amounts of money added up from zero, in order. A part with no
/// request that recorded none adds nothing; a part with requests that
/// recorded none leaves the sum absent.
pub fn sum_amounts(parts: impl IntoIterator<Item = (Option<f64>, u64)>) -> Option<f64> {
    parts
        .into_iter()
        .try_fold(0.0, |sum, (part, requests)| match part {
            None if requests == 0 => Some(sum),
            amount => Some(sum + amount?),
        })
}

/// The gap reason that says only that a part recorded nothing in a category,
/// which a part holding no request does not carry into a sum.
pub const ABSENT_CATEGORY: &str = "recorded category is absent";

/// Add a part's gap reasons for category `name` to `gaps`, each once and
/// kept sorted. A part holding no request adds no [`ABSENT_CATEGORY`].
pub fn merge_reasons(
    gaps: &mut BTreeMap<String, Vec<String>>,
    name: &str,
    reasons: &[String],
    requests: u64,
) {
    for reason in reasons {
        if requests == 0 && reason == ABSENT_CATEGORY {
            continue;
        }
        let held = gaps.entry(name.to_string()).or_default();
        if !held.contains(reason) {
            held.push(reason.clone());
            held.sort();
        }
    }
}

/// How parts stand against their whole: the categories they do not add up
/// to, and those one side leaves unknown, which cannot be compared. A
/// category neither side records is neither.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Conservation {
    pub differs: Vec<String>,
    pub uncompared: Vec<String>,
}

impl Conservation {
    /// Compare category `name`: what the parts add up to against the whole.
    pub fn compare(&mut self, name: impl Into<String>, parts: Option<u64>, whole: Option<u64>) {
        match (parts, whole) {
            (Some(parts), Some(whole)) if parts != whole => self.differs.push(name.into()),
            (Some(_), None) | (None, Some(_)) => self.uncompared.push(name.into()),
            _ => {}
        }
    }

    /// `not-conserved` where a category differs, else `unestablished` where
    /// one cannot be compared, else `conserved`.
    pub fn status(&self) -> &'static str {
        if !self.differs.is_empty() {
            "not-conserved"
        } else if !self.uncompared.is_empty() {
            "unestablished"
        } else {
            "conserved"
        }
    }
}
