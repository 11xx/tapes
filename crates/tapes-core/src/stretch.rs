//! Working stretches: a session's timestamped turns, in the order a read
//! hands them over, cut wherever the gap from one stamp to the next exceeds
//! an idle threshold. `stats` counts its stretches with this fold and
//! [`crate::usage::stretches`] lists them, so the two cannot disagree.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::span::{PlacedRequest, RequestTimeline};

/// One working stretch: its earliest and latest stamp, and the timed
/// requests it holds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stretch {
    pub first: DateTime<Utc>,
    pub last: DateTime<Utc>,
    pub requests: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_request: Option<PlacedRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_request: Option<PlacedRequest>,
}

/// Stamps folded one at a time. A gap above the threshold starts a stretch;
/// any other gap, a negative one included, continues the current one.
#[derive(Clone, Debug)]
pub(crate) struct StretchFold {
    idle_threshold: i64,
    previous: Option<DateTime<Utc>>,
    stretches: Vec<(DateTime<Utc>, DateTime<Utc>)>,
}

impl StretchFold {
    pub(crate) fn new(idle_threshold: i64) -> Self {
        Self {
            idle_threshold,
            previous: None,
            stretches: Vec::new(),
        }
    }

    pub(crate) fn idle_threshold(&self) -> i64 {
        self.idle_threshold
    }

    /// Fold one stamp, returning its gap in milliseconds from the stamp
    /// before it.
    pub(crate) fn add(&mut self, ts: DateTime<Utc>) -> Option<i64> {
        let gap = self
            .previous
            .map(|previous| ts.signed_duration_since(previous).num_milliseconds());
        match (gap, self.stretches.last_mut()) {
            (Some(gap), Some((first, last))) if gap <= self.idle_threshold => {
                *first = (*first).min(ts);
                *last = (*last).max(ts);
            }
            _ => self.stretches.push((ts, ts)),
        }
        self.previous = Some(ts);
        gap
    }

    /// How many stretches the stamps so far form.
    pub(crate) fn count(&self) -> usize {
        self.stretches.len()
    }

    /// The stretches, each holding the timed requests of `timeline` that
    /// fall in it: a request belongs to the latest stretch that began at or
    /// before it, and one that precedes every stretch to the first. Untimed
    /// requests belong to none, and without a stretch neither does any.
    pub(crate) fn finish(self, timeline: Option<&RequestTimeline>) -> Vec<Stretch> {
        let mut stretches = self
            .stretches
            .into_iter()
            .map(|(first, last)| Stretch {
                first,
                last,
                requests: 0,
                first_request: None,
                last_request: None,
            })
            .collect::<Vec<_>>();
        if stretches.is_empty() {
            return stretches;
        }
        let requests = timeline.map_or(&[][..], |timeline| &timeline.requests[..]);
        for request in requests {
            let Some(at) = request.timestamp else {
                continue;
            };
            let index = stretches
                .iter()
                .rposition(|stretch| stretch.first <= at)
                .unwrap_or(0);
            let stretch = &mut stretches[index];
            stretch.requests += 1;
            stretch.first_request.get_or_insert_with(|| request.clone());
            stretch.last_request = Some(request.clone());
        }
        stretches
    }
}
