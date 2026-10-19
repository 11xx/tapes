//! Usage inside a window of time over a selection of sessions. Each selected
//! session contributes the span of its own requests inside the window, so a
//! session that straddles an edge contributes what it consumed inside and
//! nothing else, and the window's figures are the sum of those shares under
//! the summary's usual rules. A quota window is a window whose edges come
//! from the provider's own quota observations in the recordings.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::model::Session;
use crate::span::{PlacedRequest, SpanBounds};

/// A window of time; an absent edge is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WindowBounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
}

impl WindowBounds {
    pub fn span(self) -> SpanBounds {
        SpanBounds::Time {
            from: self.from,
            to: self.to,
        }
    }

    /// Whether a session active from `started` to `last` may have activity
    /// inside the window; an unknown end may.
    pub fn may_hold(self, started: Option<DateTime<Utc>>, last: Option<DateTime<Utc>>) -> bool {
        self.from
            .is_none_or(|from| last.is_none_or(|last| last >= from))
            && self
                .to
                .is_none_or(|to| started.is_none_or(|started| started < to))
    }
}

/// How a window's edges were established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowBoundary {
    /// The caller named the edges.
    Requested,
    /// A quota observation recorded the window's reset and its length.
    Recorded,
    /// Part of the window was derived: a length from the limit's name rather
    /// than a recorded number, or a window following a recorded one.
    Inferred,
}

/// Which of a provider's quota windows: the short one (Codex primary,
/// Claude five-hour) or the long one (Codex secondary, Claude seven-day).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuotaKind {
    Primary,
    Secondary,
}

/// Where a quota window's length came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum QuotaLength {
    /// The observation recorded its window's length in minutes.
    Recorded,
    /// The observation named its limit (`five_hour`, `seven_day`), and the
    /// length is the one that name states.
    LimitName { name: String },
}

/// One quota observation a recording holds: when its window resets and how
/// long the window is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuotaObservation {
    pub quota: QuotaKind,
    pub resets_at: DateTime<Utc>,
    pub window_minutes: u64,
    pub length: QuotaLength,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

impl QuotaObservation {
    /// The window the observation describes: the length before its reset.
    pub fn window(&self) -> (DateTime<Utc>, DateTime<Utc>) {
        let length = Duration::minutes(i64::try_from(self.window_minutes).unwrap_or(i64::MAX / 60));
        (self.resets_at - length, self.resets_at)
    }
}

/// The observation a quota window was resolved from, and the session whose
/// recording holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QuotaSource {
    pub harness: String,
    pub session: String,
    pub observation: QuotaObservation,
    /// Why part of the window is inferred, where it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred: Option<&'static str>,
}

/// The instant a quota window is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaAt {
    /// The window the newest observation describes, or the one that follows
    /// it when that one has ended.
    Current,
    At(DateTime<Utc>),
}

/// The quota observations a session's read holds: Codex's newest
/// `rate_limits` windows, Claude's newest limit of each kind.
pub fn quota_observations(session: &Session) -> Vec<QuotaObservation> {
    let Some(detail) = session.usage_detail.as_ref() else {
        return Vec::new();
    };
    let mut observations = detail.quota_observations.clone();
    if let Some(limits) = &detail.rate_limits {
        for (quota, window) in [
            (QuotaKind::Primary, &limits.primary),
            (QuotaKind::Secondary, &limits.secondary),
        ] {
            if let Some((resets_at, window_minutes)) = window
                .as_ref()
                .and_then(|window| window.resets_at.zip(window.window_minutes))
            {
                observations.push(QuotaObservation {
                    quota,
                    resets_at,
                    window_minutes,
                    length: QuotaLength::Recorded,
                    observed_at: limits.observed_at,
                });
            }
        }
    }
    observations
}

/// Resolve a quota window from `observations`, each with its harness and
/// session. `current` reads the newest observation alone; an instant reads
/// the newest observation whose window holds it. A window an observation
/// describes is recorded when its length was recorded and inferred when its
/// length came from the limit's name; the window right after an observed
/// one, for an instant past its reset, is inferred. Anything further is
/// refused.
pub fn resolve_quota_window(
    observations: &[(String, String, QuotaObservation)],
    quota: QuotaKind,
    at: QuotaAt,
    now: DateTime<Utc>,
) -> anyhow::Result<(WindowBounds, WindowBoundary, QuotaSource)> {
    let mut chosen = observations
        .iter()
        .filter(|(_, _, observation)| observation.quota == quota)
        .collect::<Vec<_>>();
    // Newest observation first: the account's limits change, so an older
    // observation's window speaks for the limit as it then was.
    chosen.sort_by_key(|(_, _, observation)| {
        std::cmp::Reverse((observation.observed_at, observation.resets_at))
    });
    let kind = match quota {
        QuotaKind::Primary => "primary",
        QuotaKind::Secondary => "secondary",
    };
    if chosen.is_empty() {
        anyhow::bail!(
            "no recording in scope holds a {kind} quota observation; read the window with --window <from>..<to>"
        );
    }
    let (instant, candidates) = match at {
        QuotaAt::Current => (now, &chosen[..1]),
        QuotaAt::At(instant) => (instant, &chosen[..]),
    };
    let source =
        |harness: &str, session: &str, observation: &QuotaObservation, inferred| QuotaSource {
            harness: harness.to_owned(),
            session: session.to_owned(),
            observation: observation.clone(),
            inferred,
        };
    let containing = candidates.iter().find(|(_, _, observation)| {
        let (start, end) = observation.window();
        start <= instant && instant < end
    });
    if let Some((harness, session, observation)) = containing {
        let (from, to) = observation.window();
        let (boundary, inferred) = match observation.length {
            QuotaLength::Recorded => (WindowBoundary::Recorded, None),
            QuotaLength::LimitName { .. } => (
                WindowBoundary::Inferred,
                Some("the window's length is the one its limit's name states"),
            ),
        };
        return Ok((
            WindowBounds {
                from: Some(from),
                to: Some(to),
            },
            boundary,
            source(harness, session, observation, inferred),
        ));
    }
    let following = candidates.iter().find(|(_, _, observation)| {
        let (start, end) = observation.window();
        end <= instant && instant < end + (end - start)
    });
    if let Some((harness, session, observation)) = following {
        let (start, end) = observation.window();
        return Ok((
            WindowBounds {
                from: Some(end),
                to: Some(end + (end - start)),
            },
            WindowBoundary::Inferred,
            source(
                harness,
                session,
                observation,
                Some("the window follows the observed one, from its reset for the same length"),
            ),
        ));
    }
    match at {
        QuotaAt::Current => anyhow::bail!(
            "the newest {kind} quota observation's window ended before the one now running began; \
             read the window with --window <from>..<to>"
        ),
        QuotaAt::At(_) => anyhow::bail!(
            "no {kind} quota observation describes the window holding {}, nor the one after it; \
             read the window with --window <from>..<to>",
            instant.to_rfc3339()
        ),
    }
}

/// How a recording names the session it descends from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParentEvidence {
    /// A Claude subagent's recording, kept under its parent's.
    Subagent,
    /// A Codex spawn's `parent_thread_id`.
    Spawn,
    /// A declared fork origin, whose history the session copies.
    ForkOrigin,
    /// A `tapes-parent` marker opening the first prompt, written by the tool
    /// that launched the session. The session holds no copy of its parent.
    DeclaredLaunchParent,
}

/// The session a recording names as its parent, as its own records state it,
/// and how: a Claude subagent's parent, a Codex spawn's `parent_thread_id`, a
/// declared fork origin, or the launcher a `tapes-parent` marker names. A fork
/// copies its origin's first prompt, marker included, so its origin outranks
/// the marker.
pub fn named_parent(session: &Session) -> Option<(String, ParentEvidence)> {
    parent_in_harness(session).map(|(_, parent, evidence)| (parent, evidence))
}

/// As [`named_parent`], with the harness whose store holds the parent: the
/// session's own for a subagent, a spawn or a fork origin, and the one a
/// marker names for its launcher.
pub fn parent_in_harness(session: &Session) -> Option<(String, String, ParentEvidence)> {
    let own = || session.harness().to_owned();
    if let Some((parent, _)) = session.id.split_once("::") {
        return Some((own(), parent.to_owned(), ParentEvidence::Subagent));
    }
    let detail = session.usage_detail.as_ref()?;
    let spawn = || {
        detail
            .declared_parent
            .clone()
            .map(|parent| (own(), parent, ParentEvidence::Spawn))
    };
    let origin = || {
        detail
            .copy_evidence
            .as_ref()
            .and_then(|copies| copies.origins.first().cloned())
            .map(|origin| (own(), origin, ParentEvidence::ForkOrigin))
    };
    let launcher = || {
        detail.launch_parent.clone().map(|parent| {
            (
                parent.harness,
                parent.id,
                ParentEvidence::DeclaredLaunchParent,
            )
        })
    };
    spawn().or_else(origin).or_else(launcher)
}

/// The session a recording names as its parent; see [`named_parent`].
pub fn declared_parent(session: &Session) -> Option<String> {
    named_parent(session).map(|(parent, _)| parent)
}

/// One session's share of a window: the requests its span placed inside it,
/// and how its descent was established.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowShare {
    pub id: String,
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<crate::model::Launch>,
    /// The parent the session's recording names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// How the recording names `parent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_evidence: Option<ParentEvidence>,
    /// The session the chain of named parents ends at.
    pub root: String,
    /// Whether the store holds the session the chain ends at.
    pub root_resolved: bool,
    pub requests: usize,
    pub session_requests: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub unplaced: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<PlacedRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<PlacedRequest>,
    /// The share's consumption in disjoint categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disjoint: Option<crate::model::DisjointTokens>,
    /// The allowance the share spent, one entry per pool window.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowance: Vec<crate::allowance::Allowance>,
    /// Why the share carries no allowance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowance_absent: Option<&'static str>,
}

impl WindowShare {
    /// The share's known disjoint tokens, the measure shares are ordered by.
    pub fn consumed(&self) -> u64 {
        self.disjoint.as_ref().map_or(0, |disjoint| {
            [
                disjoint.uncached_input,
                disjoint.cache_read,
                disjoint.cache_write.map(|writes| writes.total()),
                disjoint.output,
            ]
            .into_iter()
            .flatten()
            .sum()
        })
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// The window a summary answers, how its edges were established, and each
/// session's share of it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowRecord {
    #[serde(flatten)]
    pub bounds: WindowBounds,
    pub boundary: WindowBoundary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<QuotaSource>,
    /// The sessions the window holds requests of, the largest share first.
    pub sessions: Vec<WindowShare>,
    /// Selected sessions whose span inside the window placed no request.
    pub empty: usize,
    /// Each allowance pool the window's observations report on, and how its
    /// observed rise divides.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowance: Vec<crate::allowance::PoolAllowance>,
    /// What a pool stands for where the recordings name no account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowance_assumption: Option<&'static str>,
}
