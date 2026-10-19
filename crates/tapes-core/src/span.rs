//! Usage of spans inside one session. A span selects the session's counted
//! requests by time or by ordinal; every request a read counts is placed in
//! each span that holds it, or in the remainder when none does. The figures
//! conserve: over spans that share no request, the spans and the remainder
//! add up to the whole session, per counter, within the spans' accounting
//! basis. A request or a record the read cannot place for or against a span
//! qualifies that span rather than silently leaving it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::measurement::{MeasurementCompleteness, MeasurementReason};
use crate::model::Session;

pub const SPANS_SCHEMA: &str = "tapes-usage-spans/5";

/// An instant a time span is bounded by.
pub type Instant = DateTime<Utc>;

/// An RFC 3339 timestamp with an offset, as a span bound.
pub fn parse_instant(value: &str) -> Option<Instant> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|instant| instant.with_timezone(&Utc))
}

/// Which of a session's counted requests a span selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SpanBounds {
    /// Requests whose timestamp lies in `[from, to)`; an absent bound is open.
    Time {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<DateTime<Utc>>,
    },
    /// The session's counted requests `first..=last`, 1-based in recording
    /// order; an absent end is open.
    Requests {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last: Option<usize>,
    },
}

impl SpanBounds {
    /// Whether the span holds the request, or `None` when its timestamp is
    /// unknown and the span selects by time.
    fn holds(&self, ordinal: usize, timestamp: Option<DateTime<Utc>>) -> Option<bool> {
        match *self {
            Self::Time { from, to } => {
                let at = timestamp?;
                Some(from.is_none_or(|from| at >= from) && to.is_none_or(|to| at < to))
            }
            Self::Requests { first, last } => Some(
                first.is_none_or(|first| ordinal >= first)
                    && last.is_none_or(|last| ordinal <= last),
            ),
        }
    }

    /// Whether a record lying between two neighbouring placed positions may
    /// lie inside the span, and whether it certainly does.
    fn between(&self, before: &Position, after: Option<&Position>) -> (bool, bool) {
        match *self {
            Self::Time { from, to } => {
                let low = before.timestamp;
                let high = after.and_then(|after| after.timestamp);
                let may = to.is_none_or(|to| low.is_none_or(|low| low < to))
                    && from.is_none_or(|from| high.is_none_or(|high| high >= from));
                let sure = from.is_none_or(|from| low.is_some_and(|low| low >= from))
                    && to.is_none_or(|to| high.is_some_and(|high| high < to));
                (may, sure && may)
            }
            Self::Requests { first, last } => {
                let low = before.ordinal;
                let high = low + 1;
                let may =
                    first.is_none_or(|first| first <= high) && last.is_none_or(|last| last >= low);
                let sure = low > 0
                    && after.is_some()
                    && first.is_none_or(|first| first <= low)
                    && last.is_none_or(|last| last >= high);
                (may, sure)
            }
        }
    }
}

/// One counted request as a span names it: its ordinal among the session's
/// counted requests, its timestamp, and the harness's own identifier for it
/// where the harness records one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedRequest {
    pub ordinal: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// What one span figure holds: the bounds asked for, the first and last
/// request placed in it, and how many it placed. The remainder has no bounds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpanPlacement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<SpanBounds>,
    pub requests: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<PlacedRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<PlacedRequest>,
    /// Requests of this span that another span holds too.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub overlapped: usize,
    /// Requests and records the read could neither place in this figure nor
    /// rule out of it; each qualifies the figure's completeness.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unplaced: usize,
    /// The counted requests of the whole session.
    pub session_requests: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl SpanPlacement {
    fn add(&mut self, request: &PlacedRequest) {
        self.requests += 1;
        if self.first.is_none() {
            self.first = Some(request.clone());
        }
        self.last = Some(request.clone());
    }
}

/// Every counted request of one read, in recording order, as the span
/// placer counts them: its ordinal, its timestamp where it has one, and the
/// harness's own id where it records one.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RequestTimeline {
    pub requests: Vec<PlacedRequest>,
}

impl RequestTimeline {
    /// The requests no usable timestamp places in time.
    pub fn untimed(&self) -> usize {
        self.requests
            .iter()
            .filter(|request| request.timestamp.is_none())
            .count()
    }
}

/// The most requests a read keeps in its timeline. A read that counts more
/// withholds the timeline rather than keep part of it.
pub const MAX_TIMELINE_REQUESTS: usize = 100_000;

/// Why a read asked for its timeline holds none: it counted more requests
/// than a timeline keeps.
pub const TIMELINE_REQUESTS_WITHHELD: &str =
    "the read counted more requests than a timeline keeps (100000)";

/// What a placer keeps of the requests it places.
enum Kept {
    /// Nothing: no caller asked for the timeline.
    Off,
    /// Every request placed so far, within the bound.
    Requests(Vec<PlacedRequest>),
    /// More requests than the bound; none are kept.
    Withheld,
}

/// The most recent placed position a record follows.
#[derive(Clone, Default)]
struct Position {
    ordinal: usize,
    timestamp: Option<DateTime<Utc>>,
}

/// A record whose figure is undecided until the next request is placed.
struct Pending {
    before: Position,
    reason: MeasurementReason,
}

/// Places a session's requests and records in spans, in recording order.
/// Figure `spans.len()` is the remainder.
pub(crate) struct Placer {
    spans: Vec<SpanBounds>,
    placements: Vec<SpanPlacement>,
    qualified: Vec<MeasurementCompleteness>,
    position: Position,
    pending: Vec<Pending>,
    overlapped: usize,
    /// The requests placed, in order, as far as the placer keeps them.
    kept: Kept,
}

impl Placer {
    pub(crate) fn new(spans: &[SpanBounds]) -> Self {
        let mut placements = spans
            .iter()
            .map(|bounds| SpanPlacement {
                bounds: Some(*bounds),
                ..SpanPlacement::default()
            })
            .collect::<Vec<_>>();
        placements.push(SpanPlacement::default());
        Self {
            spans: spans.to_vec(),
            qualified: vec![MeasurementCompleteness::default(); placements.len()],
            placements,
            position: Position::default(),
            pending: Vec::new(),
            overlapped: 0,
            kept: Kept::Requests(Vec::new()),
        }
    }

    /// A placer that keeps its requests' timeline only when `timeline` asks
    /// for it.
    pub(crate) fn keeping(mut self, timeline: bool) -> Self {
        if !timeline {
            self.kept = Kept::Off;
        }
        self
    }

    /// How many figures the placer fills: one per span and the remainder.
    pub(crate) fn figures(&self) -> usize {
        self.placements.len()
    }

    pub(crate) fn remainder(&self) -> usize {
        self.spans.len()
    }

    /// Place the session's next counted request, returning the figures that
    /// hold it. A request a time span cannot place for want of a timestamp
    /// is held by the remainder and qualifies the spans it may belong to.
    pub(crate) fn request(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        id: Option<&str>,
    ) -> Vec<usize> {
        let ordinal = self.position.ordinal + 1;
        let request = PlacedRequest {
            ordinal,
            timestamp,
            id: id.filter(|id| !id.is_empty()).map(str::to_owned),
        };
        let next = Position {
            ordinal,
            timestamp: timestamp.or(self.position.timestamp),
        };
        self.resolve(Some(&next));
        let mut held = Vec::new();
        let mut undecided = false;
        for (index, span) in self.spans.iter().enumerate() {
            match span.holds(ordinal, timestamp) {
                Some(true) => held.push(index),
                Some(false) => {}
                None => undecided = true,
            }
        }
        if held.len() > 1 {
            self.overlapped += 1;
            for index in &held {
                self.placements[*index].overlapped += 1;
            }
        }
        if held.is_empty() {
            held.push(self.remainder());
        }
        for index in &held {
            self.placements[*index].add(&request);
        }
        if let Kept::Requests(requests) = &mut self.kept {
            if requests.len() < MAX_TIMELINE_REQUESTS {
                requests.push(request);
            } else {
                self.kept = Kept::Withheld;
            }
        }
        if undecided {
            self.pending.push(Pending {
                before: self.position.clone(),
                reason: MeasurementReason::UnestablishedScope,
            });
        }
        self.position = next;
        held
    }

    /// Place a record that is not a counted request but qualifies the figure
    /// that holds it, such as a failed attempt or usage no counter reads.
    /// Returns the figures that certainly hold it; the figures it may belong
    /// to are qualified with `reason` once its neighbours are known.
    pub(crate) fn record(
        &mut self,
        timestamp: Option<DateTime<Utc>>,
        reason: MeasurementReason,
    ) -> Vec<usize> {
        let mut held = Vec::new();
        let mut undecided = false;
        for (index, span) in self.spans.iter().enumerate() {
            match (span, timestamp) {
                (SpanBounds::Time { .. }, Some(_)) => {
                    if span.holds(0, timestamp) == Some(true) {
                        held.push(index);
                    }
                }
                _ => undecided = true,
            }
        }
        if undecided {
            self.pending.push(Pending {
                before: self.position.clone(),
                reason,
            });
            return held;
        }
        if held.is_empty() {
            held.push(self.remainder());
        }
        held
    }

    /// A record the read could not decode, at the current position.
    pub(crate) fn unreadable(&mut self) {
        self.pending.push(Pending {
            before: self.position.clone(),
            reason: MeasurementReason::UnreadableRecord,
        });
    }

    fn resolve(&mut self, after: Option<&Position>) {
        for pending in std::mem::take(&mut self.pending) {
            let mut contained = false;
            for (index, span) in self.spans.iter().enumerate() {
                let (may, sure) = span.between(&pending.before, after);
                contained |= sure;
                if may {
                    self.qualified[index].qualify_all(pending.reason);
                    self.placements[index].unplaced += 1;
                }
            }
            if !contained {
                let remainder = self.spans.len();
                self.qualified[remainder].qualify_all(pending.reason);
                self.placements[remainder].unplaced += 1;
            }
        }
    }

    /// The placements and the qualifications each figure carries, one per
    /// span and then the remainder.
    pub(crate) fn finish(mut self) -> Placed {
        self.resolve(None);
        let requests = self.position.ordinal;
        for placement in &mut self.placements {
            placement.session_requests = requests;
        }
        Placed {
            placements: self.placements,
            qualified: self.qualified,
            overlapped: self.overlapped,
            kept: self.kept,
        }
    }
}

/// What a placer established, figure by figure.
pub(crate) struct Placed {
    placements: Vec<SpanPlacement>,
    qualified: Vec<MeasurementCompleteness>,
    overlapped: usize,
    kept: Kept,
}

impl Placed {
    /// Every request placed, in order: none when the placer kept no
    /// timeline, and the reason when it outgrew its bound.
    pub(crate) fn timeline(self) -> Result<Option<RequestTimeline>, &'static str> {
        match self.kept {
            Kept::Off => Ok(None),
            Kept::Requests(requests) => Ok(Some(RequestTimeline { requests })),
            Kept::Withheld => Err(TIMELINE_REQUESTS_WITHHELD),
        }
    }

    /// How many figures were placed: one per span and the remainder.
    pub(crate) fn figures(&self) -> usize {
        self.placements.len()
    }
}

/// A backend's span read: one session figure per span and the remainder,
/// the whole session in the same basis, and what placed each figure.
#[derive(Debug)]
pub struct SpanSessions {
    pub spans: Vec<(Session, SpanPlacement)>,
    pub remainder: (Session, SpanPlacement),
    /// The whole session in the spans' accounting basis.
    pub whole: Session,
    /// Requests more than one span holds.
    pub overlapped: usize,
    /// Every request the figures place, in recording order, unless the
    /// read counted more than [`MAX_TIMELINE_REQUESTS`].
    pub timeline: Option<RequestTimeline>,
    pub notes: Vec<String>,
}

impl SpanSessions {
    /// Give every figure the billing `session`'s recording establishes:
    /// each is part of the one recording.
    pub(crate) fn billed_as(mut self, session: &Session) -> Self {
        let billing = session
            .usage_detail
            .as_ref()
            .and_then(|detail| detail.billing.clone());
        if let Some(billing) = billing {
            for figure in self
                .spans
                .iter_mut()
                .map(|(figure, _)| figure)
                .chain([&mut self.remainder.0, &mut self.whole])
            {
                figure
                    .usage_detail
                    .get_or_insert_with(Default::default)
                    .billing
                    .get_or_insert_with(|| billing.clone());
            }
        }
        self
    }
}

impl SpanSessions {
    /// Join the figures a backend computed with what its placer established.
    /// `figures` holds one session per span and then the remainder.
    pub(crate) fn assemble(
        placed: Placed,
        figures: Vec<Session>,
        whole: Session,
        notes: Vec<String>,
    ) -> Self {
        let Placed {
            placements,
            qualified,
            overlapped,
            kept,
        } = placed;
        let timeline = match kept {
            Kept::Requests(requests) => Some(RequestTimeline { requests }),
            Kept::Off | Kept::Withheld => None,
        };
        let mut joined = figures
            .into_iter()
            .zip(placements)
            .zip(qualified)
            .map(|((mut session, placement), qualified)| {
                // What the placer could not place qualifies every figure the
                // span holds, a retained-branch consumption included.
                if !qualified.is_empty() {
                    let detail = session.usage_detail.get_or_insert_with(Default::default);
                    detail.measurement_completeness.merge(&qualified);
                    if let Some(consumption) = &mut detail.consumption {
                        consumption.measurement_completeness.merge(&qualified);
                    }
                }
                (session, placement)
            })
            .collect::<Vec<_>>();
        let remainder = joined.pop().expect("a placer always fills the remainder");
        Self {
            spans: joined,
            remainder,
            whole,
            overlapped,
            timeline,
            notes,
        }
    }
}

/// A span figure keeps the whole session's identity: the span names its own
/// requests, and the session it belongs to stays the one asked about.
pub(crate) fn keep_identity(figure: &mut Session, whole: &Session) {
    figure.id.clone_from(&whole.id);
    figure.source.clone_from(&whole.source);
    figure.metadata.clone_from(&whole.metadata);
    figure.title.clone_from(&whole.title);
    figure.derived_title.clone_from(&whole.derived_title);
    figure.directory.clone_from(&whole.directory);
    figure.started_at = whole.started_at;
    figure.last_activity_at = whole.last_activity_at;
    figure.launch = whole.launch;
}
