//! What one session and everything it caused consumed.
//!
//! A session's family is the set of sessions whose consumption it caused,
//! each counted once by its harness and native id:
//!
//! - the session itself;
//! - its lineage descendants: a Claude parent's subagent recordings, a Codex
//!   spawn named by its `parent_thread_id`, and a session that declares a
//!   member as its fork origin;
//! - every session whose first prompt opens with a `tapes-parent` marker
//!   naming a member, in any harness, recursively;
//! - on request, the session a continued Claude session carried over from,
//!   together with that session's family.
//!
//! Descent comes from what recordings state. A candidate for a launch marker
//! is searched by time alone: a launched session's first activity lies inside
//! its launcher's recorded interval, in any project, so the search lists
//! every session active since the family opened and reads the opening of only
//! those that began inside a member's interval. The search reports what it
//! listed, what it read and what bound left it incomplete.
//!
//! A member is measured as a window measures a share: the span of its own
//! requests. A Claude recording's recorded total counts its subagents'
//! requests, which the family holds as members of their own, so the part of
//! that total no request explains is reported apart, measured against the
//! family's requests. A request recorded under one id in a parent's and its
//! subagent's recordings is counted in the earlier one only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::backend::{open_regular_file, session_file_path, Backend, StreamOptions};
use crate::model::{AccountingBasis, Session};
use crate::pricing::PriceTable;
use crate::span::SpanBounds;
use crate::usage::{self, AccountingPartition, GroupBy, GroupKey, UsageTally};
use crate::window::{self, ParentEvidence};
use crate::{ListFilters, ListSort, Selection};

pub const FAMILY_SCHEMA: &str = "tapes-family-usage/1";

/// The most sessions a family holds; a search that finds more stops and says
/// so.
const MAX_MEMBERS: usize = 2_000;

/// The most sessions of each harness the launch-marker search lists.
const POOL_LIMIT: usize = 5_000;

/// The most bytes a continuation search reads across candidate recordings.
const CONTINUATION_BYTES: u64 = 2 << 30;

/// The longest record a continuation search decodes.
const MAX_RECORD_BYTES: u64 = 16 << 20;

/// The most records a recording's opening is read through for a compact
/// boundary.
const OPENING_RECORDS: usize = 64;

/// The most earlier sessions a chain of continuations is followed through.
const MAX_CONTINUATIONS: usize = 16;

/// What a member was to the family's root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// The session asked about, and a session it continued.
    Lead,
    /// A Claude subagent recording or a Codex spawn.
    Subagent,
    /// A session a launcher started with a `tapes-parent` marker.
    Delegate,
    /// A session that declares a member as its fork origin.
    Fork,
}

/// How a member was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Link {
    Root,
    /// The session a compact boundary of the continuing one names.
    Continuation,
    /// A recording kept under its parent's subagents directory.
    SubagentRecording,
    /// A Codex `parent_thread_id`.
    Spawn,
    /// A declared fork origin.
    ForkOrigin,
    /// A `tapes-parent` marker opening the first prompt.
    LaunchMarker,
}

/// A session by its harness and native id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct SessionKey {
    pub harness: String,
    pub id: String,
}

impl SessionKey {
    fn of(session: &Session) -> Self {
        Self {
            harness: session.harness().to_owned(),
            id: session.id.clone(),
        }
    }
}

/// One member's consumption.
#[derive(Debug, Serialize)]
pub struct FamilySession {
    pub harness: String,
    pub id: String,
    pub role: Role,
    /// `harness:model#effort`, with whatever part the recording does not
    /// state left out.
    pub agent: String,
    pub link: Link,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionKey>,
    /// Requests this recording holds that an earlier member's recording holds
    /// under the same request id, left out of its figure.
    #[serde(skip_serializing_if = "is_zero")]
    pub shared_requests: usize,
    /// The parent's recorded total counts this subagent recording's
    /// requests, so the figure is shown and not added to the totals again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub counted_in_parent_total: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub usage: UsageTally,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// The consumption of the members that share an agent and a role.
#[derive(Debug, Serialize)]
pub struct FamilyAgent {
    pub agent: String,
    pub role: Role,
    pub key: GroupKey,
    #[serde(flatten)]
    pub usage: UsageTally,
}

/// A member the family names without a figure, and why.
#[derive(Debug, Serialize)]
pub struct WithheldMember {
    pub harness: String,
    pub id: String,
    pub role: Role,
    pub reason: WithheldReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WithheldReason {
    /// The recording could not be read whole, or changed while it was read.
    RecordingUnreadableOrChanged,
    /// The recording states no consumption of its own.
    NoMeasuredConsumption,
}

/// What the search for launched sessions listed, read and left out.
#[derive(Debug, Default, Serialize)]
pub struct FamilySearch {
    /// The span of time the search covered: from the family's earliest member
    /// opening to the latest member activity, open where a member is live.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// Sessions of every harness active since `since`, as a listing returns
    /// them.
    pub candidates_listed: usize,
    /// Candidates whose opening records were read for a marker.
    pub candidates_examined: usize,
    /// Candidates that began outside every member's interval, whose openings
    /// were not read.
    pub outside_interval: usize,
    /// Candidates examined that recorded no start, so no interval ruled them
    /// out.
    pub untimed: usize,
    /// Why the search may have missed a member; empty when it is complete.
    pub incomplete: Vec<SearchIncomplete>,
    /// Candidates whose opening could not be read: harness and id.
    pub unreadable: Vec<SessionKey>,
    /// Stores the listing could not read, as the listing names them.
    pub unavailable: Vec<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum SearchIncomplete {
    /// A harness's listing reached its bound.
    ListingBound { harness: String, bound: usize },
    /// The listing stopped at its scan ceiling.
    ScanTruncated,
    /// A store the listing reads could not be read.
    StoreUnavailable,
    /// Candidate openings could not be read.
    OpeningsUnreadable { count: usize },
    /// The family reached the member bound.
    MemberBound { bound: usize },
    /// A member's subagent recordings could not all be enumerated.
    SubagentsUnenumerated { count: usize },
}

/// What a continued session's compact boundary says about its predecessor.
#[derive(Debug, Serialize)]
pub struct Continuation {
    /// The continuing session.
    #[serde(flatten)]
    pub session: SessionKey,
    pub status: ContinuationStatus,
    /// The session the boundary's record belongs to, when one was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<SessionKey>,
    /// Recordings read for the record, and bytes read across them.
    pub recordings_examined: usize,
    pub bytes_examined: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContinuationStatus {
    /// Exactly one recording holds the boundary's record as its own.
    Linked,
    /// The recording opens with no compact boundary.
    NotContinued,
    /// The harness records no compact boundary.
    UnsupportedHarness,
    /// The recording could not be read for a compact boundary.
    OpeningUnreadable,
    /// No searched recording holds the record.
    NotFound,
    /// More than one recording holds the record as its own.
    Ambiguous,
}

/// The family of one session, measured.
#[derive(Debug, Serialize)]
pub struct FamilyUsage {
    pub schema: &'static str,
    pub root: SessionKey,
    pub continuations_requested: bool,
    /// One row per agent and role.
    pub agents: Vec<FamilyAgent>,
    /// One row per member, the root first.
    pub sessions: Vec<FamilySession>,
    /// Every member that took a figure, summed per the summary's partitions,
    /// withholding and overlap rules.
    pub totals: UsageTally,
    pub partitions: Vec<AccountingPartition>,
    /// The partitions' list-price estimates added, where `totals` withholds
    /// its own because the members' accounting differs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priced_across_bases: Option<PricedAcrossBases>,
    pub search: FamilySearch,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub continuations: Vec<Continuation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<WithheldMember>,
}

/// List-price estimates of the accounting partitions, added. Each partition's
/// figure is its own sessions' consumption priced; they are separate sessions,
/// so their prices add even where their token counters do not.
#[derive(Debug, PartialEq, Serialize)]
pub struct PricedAcrossBases {
    pub currency: String,
    pub at_least: f64,
    pub at_most: f64,
    /// Partitions the sum holds.
    pub partitions: usize,
    /// Partitions with sessions that carry no estimate, so the sum is a
    /// lower bound.
    pub unpriced_partitions: usize,
}

fn priced_across_bases(partitions: &[AccountingPartition]) -> Option<PricedAcrossBases> {
    let mut sum: Option<PricedAcrossBases> = None;
    let mut unpriced = 0;
    for partition in partitions {
        let Some(estimate) = &partition.tally.list_price_estimate else {
            unpriced += usize::from(partition.tally.disjoint.is_some());
            continue;
        };
        let least = estimate.at_least.or(estimate.amount)?;
        let most = estimate.at_most.or(estimate.amount)?;
        let held = sum.get_or_insert_with(|| PricedAcrossBases {
            currency: estimate.currency.clone(),
            at_least: 0.0,
            at_most: 0.0,
            partitions: 0,
            unpriced_partitions: 0,
        });
        if held.currency != estimate.currency {
            return None;
        }
        held.at_least += least;
        held.at_most += most;
        held.partitions += 1;
    }
    sum.map(|mut sum| {
        sum.unpriced_partitions = unpriced;
        sum
    })
}

/// What a family read is asked for.
#[derive(Clone, Copy, Default)]
pub struct FamilyOptions<'a> {
    /// Follow a continued session back to the session it carried over from.
    pub continuations: bool,
    pub prices: Option<&'a PriceTable>,
}

/// When a member's recording began and last had activity; an absent end is
/// open.
type Interval = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

struct Member {
    session: Session,
    backend: usize,
    role: Role,
    link: Link,
    parent: Option<SessionKey>,
}

#[derive(Default)]
struct Roster {
    members: Vec<Member>,
    index: HashMap<SessionKey, usize>,
}

impl Roster {
    fn contains(&self, key: &SessionKey) -> bool {
        self.index.contains_key(key)
    }

    fn add(&mut self, member: Member) -> bool {
        let key = SessionKey::of(&member.session);
        if self.index.contains_key(&key) {
            return false;
        }
        self.index.insert(key, self.members.len());
        self.members.push(member);
        true
    }

    /// The span of time the members' recordings cover.
    fn intervals(&self) -> Vec<Interval> {
        self.members
            .iter()
            .map(|member| (member.session.started_at, member.session.last_activity_at))
            .collect()
    }
}

/// Measure the family of the session `selection` names.
pub fn family_usage_with_backends(
    backends: &[Box<dyn Backend>],
    selection: Selection<'_>,
    options: FamilyOptions<'_>,
) -> Result<FamilyUsage> {
    let resolved = selection.resolve(backends)?;
    let root = SessionKey::of(&resolved.session);
    let mut roster = Roster::default();
    roster.add(Member {
        session: resolved.session,
        backend: resolved.backend_index,
        role: Role::Lead,
        link: Link::Root,
        parent: None,
    });
    let mut continuations = Vec::new();
    if options.continuations {
        follow_continuations(backends, &mut roster, &mut continuations);
    }
    let search = expand(backends, &mut roster);
    let measured = measure(backends, &roster, options.prices);
    Ok(FamilyUsage {
        schema: FAMILY_SCHEMA,
        root,
        continuations_requested: options.continuations,
        agents: measured.agents,
        sessions: measured.sessions,
        totals: measured.totals,
        priced_across_bases: priced_across_bases(&measured.partitions),
        partitions: measured.partitions,
        search,
        continuations,
        withheld: measured.withheld,
    })
}

// ---------------------------------------------------------------- discovery

/// Add the members the recordings name: subagent recordings of every member,
/// and the sessions the time-windowed search finds naming a member as their
/// parent, until no new member appears.
fn expand(backends: &[Box<dyn Backend>], roster: &mut Roster) -> FamilySearch {
    let mut search = FamilySearch::default();
    let mut unenumerated = 0;
    let mut subagents_read = 0;
    // Subagent recordings are read for each member once, as it joins.
    let read_children = |roster: &mut Roster, from: usize, unenumerated: &mut usize| {
        for position in from..roster.members.len() {
            let (children, failures) = backends[roster.members[position].backend]
                .window_children(&roster.members[position].session);
            *unenumerated += failures.len();
            let parent = SessionKey::of(&roster.members[position].session);
            let backend = roster.members[position].backend;
            for child in children {
                if roster.members.len() >= MAX_MEMBERS {
                    return;
                }
                roster.add(Member {
                    session: child,
                    backend,
                    role: Role::Subagent,
                    link: Link::SubagentRecording,
                    parent: Some(parent.clone()),
                });
            }
        }
    };
    read_children(roster, subagents_read, &mut unenumerated);
    subagents_read = roster.members.len();

    let since = roster
        .members
        .iter()
        .filter_map(|member| member.session.started_at)
        .min();
    search.since = since;
    let listed = since.map(|since| {
        crate::list_scoped(
            backends,
            None,
            None,
            POOL_LIMIT,
            &ListFilters {
                since: Some(since),
                ..Default::default()
            },
            ListSort::Oldest,
        )
    });
    let mut pool = match listed {
        Some(Ok(listed)) => {
            search.candidates_listed = listed.sessions.len();
            search.unavailable = listed.unavailable;
            if listed.scan_truncated {
                search.incomplete.push(SearchIncomplete::ScanTruncated);
            }
            let mut per_harness: BTreeMap<String, usize> = BTreeMap::new();
            for origin in &listed.origins {
                *per_harness
                    .entry(backends[*origin].harness().to_owned())
                    .or_default() += 1;
            }
            for (harness, count) in per_harness {
                if count >= POOL_LIMIT {
                    search.incomplete.push(SearchIncomplete::ListingBound {
                        harness,
                        bound: POOL_LIMIT,
                    });
                }
            }
            if !search.unavailable.is_empty() || !listed.unreadable.is_empty() {
                search.incomplete.push(SearchIncomplete::StoreUnavailable);
            }
            listed
                .sessions
                .into_iter()
                .zip(listed.origins)
                .map(|(session, backend)| Candidate {
                    session,
                    backend,
                    examined: false,
                })
                .collect::<Vec<_>>()
        }
        Some(Err(_)) => {
            search.incomplete.push(SearchIncomplete::StoreUnavailable);
            Vec::new()
        }
        None => Vec::new(),
    };
    pool.retain(|candidate| !roster.contains(&SessionKey::of(&candidate.session)));

    loop {
        // A marker lives in the first prompt: read it only for candidates
        // that began inside a member's interval.
        let intervals = roster.intervals();
        let mut eligible: Vec<usize> = Vec::new();
        for (position, candidate) in pool.iter().enumerate() {
            if candidate.examined
                || roster.contains(&SessionKey::of(&candidate.session))
                || window::declared_parent(&candidate.session).is_some()
            {
                continue;
            }
            if began_inside(&candidate.session, &intervals) {
                eligible.push(position);
            }
        }
        if !eligible.is_empty() {
            let failures = read_markers(backends, &mut pool, &eligible);
            for position in &eligible {
                pool[*position].examined = true;
            }
            search.candidates_examined += eligible.len();
            search.untimed += eligible
                .iter()
                .filter(|position| pool[**position].session.started_at.is_none())
                .count();
            for failure in failures {
                if !search.unreadable.contains(&failure) {
                    search.unreadable.push(failure);
                }
            }
        }
        // What a listing already carries (a spawn's `parent_thread_id`, a
        // declared fork origin, whenever the candidate began) and the
        // markers just read name a parent; one that is a member joins.
        let mut grew = false;
        for candidate in &pool {
            if let Some((harness, id, evidence)) = window::parent_in_harness(&candidate.session) {
                grew |= adopt(roster, candidate, &SessionKey { harness, id }, evidence);
            }
        }
        if roster.members.len() >= MAX_MEMBERS {
            search
                .incomplete
                .push(SearchIncomplete::MemberBound { bound: MAX_MEMBERS });
            break;
        }
        if !grew {
            break;
        }
        pool.retain(|candidate| !roster.contains(&SessionKey::of(&candidate.session)));
        read_children(roster, subagents_read, &mut unenumerated);
        subagents_read = roster.members.len();
    }
    search.outside_interval = pool.iter().filter(|candidate| !candidate.examined).count();
    if !search.unreadable.is_empty() {
        search
            .incomplete
            .push(SearchIncomplete::OpeningsUnreadable {
                count: search.unreadable.len(),
            });
    }
    if unenumerated > 0 {
        search
            .incomplete
            .push(SearchIncomplete::SubagentsUnenumerated {
                count: unenumerated,
            });
    }
    search.complete = search.incomplete.is_empty();
    search
}

struct Candidate {
    session: Session,
    backend: usize,
    examined: bool,
}

/// Whether the candidate began inside some member's interval, or recorded no
/// start that could rule it out.
fn began_inside(candidate: &Session, intervals: &[Interval]) -> bool {
    let Some(began) = candidate.started_at else {
        return true;
    };
    intervals.iter().any(|(start, end)| {
        start.is_none_or(|start| began >= start) && end.is_none_or(|end| began <= end)
    })
}

/// Join `candidate` to the family when `parent` is a member.
fn adopt(
    roster: &mut Roster,
    candidate: &Candidate,
    parent: &SessionKey,
    evidence: ParentEvidence,
) -> bool {
    if !roster.contains(parent) || roster.members.len() >= MAX_MEMBERS {
        return false;
    }
    let (role, link) = match evidence {
        ParentEvidence::Subagent => (Role::Subagent, Link::SubagentRecording),
        ParentEvidence::Spawn => (Role::Subagent, Link::Spawn),
        ParentEvidence::ForkOrigin => (Role::Fork, Link::ForkOrigin),
        ParentEvidence::DeclaredLaunchParent => (Role::Delegate, Link::LaunchMarker),
    };
    roster.add(Member {
        session: candidate.session.clone(),
        backend: candidate.backend,
        role,
        link,
        parent: Some(parent.clone()),
    })
}

/// Declare, for the candidates at `positions`, the origins their stores
/// state and the launch marker their first prompt opens with: one batched
/// question per backend each. Candidates whose opening could not be read.
fn read_markers(
    backends: &[Box<dyn Backend>],
    pool: &mut [Candidate],
    positions: &[usize],
) -> Vec<SessionKey> {
    let mut unreadable = Vec::new();
    for (index, backend) in backends.iter().enumerate() {
        let own: Vec<usize> = positions
            .iter()
            .copied()
            .filter(|position| pool[*position].backend == index)
            .collect();
        if own.is_empty() {
            continue;
        }
        let held: Vec<&Session> = own
            .iter()
            .map(|position| &pool[*position].session)
            .collect();
        if let Ok(declared) = backend.declared_origins(&held) {
            for (position, declared) in own.iter().zip(declared) {
                pool[*position].session.declare_origins(declared);
            }
        }
        let unrelated: Vec<usize> = own
            .into_iter()
            .filter(|position| window::declared_parent(&pool[*position].session).is_none())
            .collect();
        let held: Vec<&Session> = unrelated
            .iter()
            .map(|position| &pool[*position].session)
            .collect();
        let markers = backend.first_prompt_markers(&held);
        for (position, marker) in unrelated.iter().zip(markers) {
            match marker {
                Some(Ok(marker)) => pool[*position].session.declare_launch_parent(marker),
                Some(Err(_)) => unreadable.push(SessionKey::of(&pool[*position].session)),
                None => {}
            }
        }
    }
    unreadable
}

// ------------------------------------------------------------ continuations

/// Follow the root, and each session found, back through the compact
/// boundary its recording opens with.
fn follow_continuations(
    backends: &[Box<dyn Backend>],
    roster: &mut Roster,
    report: &mut Vec<Continuation>,
) {
    let mut current = 0;
    for _ in 0..MAX_CONTINUATIONS {
        let member = &roster.members[current];
        let key = SessionKey::of(&member.session);
        let found = continued_from(backends, member);
        let linked = found.linked.clone();
        report.push(Continuation {
            session: key.clone(),
            status: found.status,
            from: linked.as_ref().map(|(_, session)| SessionKey::of(session)),
            recordings_examined: found.recordings,
            bytes_examined: found.bytes,
        });
        let Some((backend, session)) = linked else {
            return;
        };
        let added = roster.add(Member {
            session,
            backend,
            role: Role::Lead,
            link: Link::Continuation,
            parent: Some(key),
        });
        if !added {
            return;
        }
        current = roster.members.len() - 1;
    }
}

struct Continued {
    status: ContinuationStatus,
    linked: Option<(usize, Session)>,
    recordings: usize,
    bytes: u64,
}

impl Continued {
    fn without_link(status: ContinuationStatus) -> Self {
        Self {
            status,
            linked: None,
            recordings: 0,
            bytes: 0,
        }
    }
}

/// The session a continuing Claude session carried over from.
///
/// A continuing session's recording opens, before any user or assistant
/// record, with a `compact_boundary` system record whose `logicalParentUuid`
/// is the `uuid` of a record in the earlier session's recording. No record
/// names the earlier session. The link holds when exactly one recording in
/// the continuing one's own directory holds a record with that `uuid`
/// carrying that recording's own session id; none or several establish
/// nothing.
fn continued_from(backends: &[Box<dyn Backend>], member: &Member) -> Continued {
    if member.session.harness() != "claude" {
        return Continued::without_link(ContinuationStatus::UnsupportedHarness);
    }
    let Some(path) = session_file_path(&member.session) else {
        return Continued::without_link(ContinuationStatus::OpeningUnreadable);
    };
    let target = match boundary_parent(path) {
        Ok(Some(target)) => target,
        Ok(None) => return Continued::without_link(ContinuationStatus::NotContinued),
        Err(_) => return Continued::without_link(ContinuationStatus::OpeningUnreadable),
    };
    let mut examined = Continued::without_link(ContinuationStatus::NotFound);
    let mut holders: Vec<String> = Vec::new();
    for (id, candidate) in siblings(path) {
        if examined.bytes >= CONTINUATION_BYTES {
            break;
        }
        examined.recordings += 1;
        if holds_record(&candidate, &id, &target, &mut examined.bytes).unwrap_or(false) {
            holders.push(id);
        }
    }
    match holders.len() {
        0 => {}
        1 => match crate::resolve_session_scoped(backends, &holders[0], Some("claude")) {
            Ok(found) => {
                examined.status = ContinuationStatus::Linked;
                examined.linked = Some((found.backend_index, found.session));
            }
            Err(_) => examined.status = ContinuationStatus::NotFound,
        },
        _ => examined.status = ContinuationStatus::Ambiguous,
    }
    examined
}

/// The other session recordings kept beside the one at `path`, as session id
/// and path, the most recently written first.
fn siblings(path: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Some(directory) = path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, String, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let candidate = entry.path();
            if candidate == path || candidate.extension()? != "jsonl" {
                return None;
            }
            let id = candidate.file_stem()?.to_str()?.to_owned();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, id, candidate))
        })
        .collect();
    found.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    found.truncate(POOL_LIMIT);
    found
        .into_iter()
        .map(|(_, id, candidate)| (id, candidate))
        .collect()
}

/// The `logicalParentUuid` of the compact boundary a recording opens with,
/// when it opens with one before any user or assistant record.
fn boundary_parent(path: &Path) -> Result<Option<String>> {
    let file = open_regular_file(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    for _ in 0..OPENING_RECORDS {
        line.clear();
        if bounded_line(&mut reader, &mut line)?.is_none() {
            return Ok(None);
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match value["type"].as_str() {
            Some("user" | "assistant") => return Ok(None),
            Some("system") if value["subtype"] == "compact_boundary" => {
                return Ok(value["logicalParentUuid"].as_str().map(str::to_owned));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// Whether the recording at `path`, of session `id`, holds a record whose
/// own `uuid` is `target` and whose `sessionId` is `id`. Counts the bytes
/// read.
fn holds_record(path: &Path, id: &str, target: &str, bytes: &mut u64) -> Result<bool> {
    let file = open_regular_file(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let needle = target.as_bytes();
    let mut held = false;
    while let Some(read) = bounded_line(&mut reader, &mut line)? {
        *bytes += read;
        if !contains(&line, needle) {
            continue;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            held |= value["uuid"] == target && value["sessionId"] == id;
        }
    }
    Ok(held)
}

/// One line of at most [`MAX_RECORD_BYTES`] into `line`, the bytes read, or
/// `None` at the end. A longer line is read through and left empty.
fn bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> Result<Option<u64>> {
    line.clear();
    let mut total = 0u64;
    loop {
        let mut limited = reader.by_ref().take(MAX_RECORD_BYTES + 1);
        let read = limited.read_until(b'\n', line)? as u64;
        total += read;
        if read == 0 {
            return Ok((total > 0).then_some(total));
        }
        if line.last() == Some(&b'\n') {
            return Ok(Some(total));
        }
        if line.len() as u64 > MAX_RECORD_BYTES {
            // Discard the rest of an oversized record.
            line.clear();
            let mut sink = Vec::new();
            loop {
                sink.clear();
                let more = reader.by_ref().take(1 << 20).read_until(b'\n', &mut sink)? as u64;
                total += more;
                if more == 0 || sink.last() == Some(&b'\n') {
                    return Ok(Some(total));
                }
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

// -------------------------------------------------------------- measurement

struct Measured {
    agents: Vec<FamilyAgent>,
    sessions: Vec<FamilySession>,
    totals: UsageTally,
    partitions: Vec<AccountingPartition>,
    withheld: Vec<WithheldMember>,
}

/// A member's figure: the one `usage --full` reports for its recording, and
/// the part of it not already counted by an earlier member of its Claude
/// family.
struct Figure {
    whole: Session,
    counted: Session,
    shared: usize,
}

/// One member's figure over its whole recording. `seen` holds the request ids
/// the earlier recordings of its Claude family recorded; a request this
/// recording records under one of them is left out of the counted figure.
fn member_figure(
    backend: &dyn Backend,
    member: &Member,
    seen: Option<&mut HashSet<String>>,
) -> Result<Figure> {
    let read = backend.stream_transcript_with(
        &member.session,
        None,
        StreamOptions { timeline: true },
        &mut |_| Ok(()),
    )?;
    let whole = backend.stream_session(&member.session, &read)?;
    let mut shared = Vec::new();
    if let (Some(seen), Some(timeline)) = (seen, read.requests.as_ref()) {
        for request in &timeline.requests {
            if let Some(id) = &request.id {
                if !seen.insert(id.clone()) {
                    shared.push(request.ordinal);
                }
            }
        }
    }
    let counted = if shared.is_empty() {
        whole.clone()
    } else {
        // The requests no span holds are the recording's own.
        let spans: Vec<SpanBounds> = shared
            .iter()
            .map(|ordinal| SpanBounds::Requests {
                first: Some(*ordinal),
                last: Some(*ordinal),
            })
            .collect();
        backend
            .stream_spans(&member.session, &read, &spans)?
            .billed_as_whole()
            .remainder
            .0
    };
    Ok(Figure {
        whole,
        counted,
        shared: shared.len(),
    })
}

/// A Claude recording and the requests its subagent recordings add to it.
#[derive(Default)]
struct ClaudeFamily {
    /// The parent's recorded totals, where its figure places a part of them
    /// no request explains.
    chain: Option<crate::unexplained::RecordedChain>,
    requests: Vec<crate::unexplained::TimedRequest>,
    subagents: usize,
    /// A subagent whose requests are not all known: one that could not be
    /// read, or whose requests passed the bound.
    incomplete: bool,
    /// Request ids the family's recordings have recorded so far.
    seen: HashSet<String>,
    /// The parent's accounting is a recorded total, which counts its
    /// subagents' requests.
    total_counts_subagents: bool,
}

impl ClaudeFamily {
    fn observe_parent(&mut self, whole: &Session) {
        let detail = whole.usage_detail.as_ref();
        if detail.is_some_and(|detail| detail.unexplained.is_some()) {
            self.chain = detail.and_then(|detail| detail.recorded_chain.clone());
        }
        self.total_counts_subagents = whole
            .accounting
            .as_ref()
            .is_some_and(|accounting| accounting.basis == AccountingBasis::RecordedTotal);
    }

    fn observe_subagent(&mut self, whole: &Session) {
        self.subagents += 1;
        if self.chain.is_none() || self.incomplete {
            return;
        }
        match whole
            .usage_detail
            .as_ref()
            .and_then(|detail| detail.timed_requests.as_ref())
        {
            Some(requests)
                if self.requests.len() + requests.len()
                    <= crate::unexplained::MAX_FAMILY_REQUESTS =>
            {
                self.requests.extend(requests.iter().cloned());
            }
            Some(_) => self.incomplete = true,
            None if whole.tokens.is_some() => self.incomplete = true,
            None => {}
        }
    }

    fn unreadable_subagent(&mut self) {
        self.subagents += 1;
        self.incomplete = true;
    }
}

/// The label of the agent a figure was served by.
fn agent_label(session: &Session) -> String {
    let mut label = session.harness().to_owned();
    if let Some(model) = &session.model {
        label.push(':');
        label.push_str(&model.id);
        if let Some(variant) = &model.variant {
            label.push('#');
            label.push_str(variant);
        }
    }
    label
}

fn measure(
    backends: &[Box<dyn Backend>],
    roster: &Roster,
    prices: Option<&PriceTable>,
) -> Measured {
    struct Taken {
        position: usize,
        figure: Session,
        shared: usize,
        /// The parent's recorded total already counts this recording.
        in_parent_total: bool,
    }
    let mut taken: Vec<Taken> = Vec::new();
    let mut withheld = Vec::new();
    let mut families: HashMap<SessionKey, ClaudeFamily> = HashMap::new();
    let mut parent_figure: HashMap<SessionKey, usize> = HashMap::new();
    for (position, member) in roster.members.iter().enumerate() {
        let backend = backends[member.backend].as_ref();
        let key = SessionKey::of(&member.session);
        let claude = member.session.harness() == "claude";
        let subagent = claude && member.link == Link::SubagentRecording;
        let owner = if subagent {
            member.parent.clone()
        } else if claude {
            Some(key.clone())
        } else {
            None
        };
        let family = owner
            .as_ref()
            .map(|owner| families.entry(owner.clone()).or_default());
        let seen = family.map(|family| &mut family.seen);
        match member_figure(backend, member, seen) {
            Ok(figure) => {
                let mut in_parent_total = false;
                if let Some(family) = owner.as_ref().and_then(|owner| families.get_mut(owner)) {
                    if subagent {
                        family.observe_subagent(&figure.whole);
                        in_parent_total = family.total_counts_subagents;
                    } else {
                        family.observe_parent(&figure.whole);
                        parent_figure.insert(key, taken.len());
                    }
                }
                taken.push(Taken {
                    position,
                    figure: figure.counted,
                    shared: figure.shared,
                    in_parent_total,
                });
            }
            Err(_) => {
                if subagent {
                    if let Some(family) = owner.as_ref().and_then(|owner| families.get_mut(owner)) {
                        family.unreadable_subagent();
                    }
                }
                withheld.push(WithheldMember {
                    harness: member.session.harness().to_owned(),
                    id: member.session.id.clone(),
                    role: member.role,
                    reason: WithheldReason::RecordingUnreadableOrChanged,
                });
            }
        }
    }
    // A Claude recording's recorded total counts its subagents' requests: the
    // part no request of the family explains is measured against all of them.
    for (owner, family) in families {
        let (Some(chain), Some(position), true) = (
            family.chain,
            parent_figure.get(&owner),
            family.subagents > 0,
        ) else {
            continue;
        };
        taken[*position]
            .figure
            .usage_detail
            .get_or_insert_with(Default::default)
            .unexplained = (!family.incomplete)
            .then(|| chain.accrual(Some(&family.requests)).whole())
            .flatten();
    }
    for taken in &taken {
        if taken.figure.tokens.is_none() && taken.figure.cost.is_none() {
            let member = &roster.members[taken.position];
            withheld.push(WithheldMember {
                harness: member.session.harness().to_owned(),
                id: member.session.id.clone(),
                role: member.role,
                reason: WithheldReason::NoMeasuredConsumption,
            });
        }
    }

    let relations = usage::SessionRelations::default();
    let every: Vec<Session> = taken.iter().map(|taken| taken.figure.clone()).collect();
    let overlap = crate::overlap::Overlap::of(&every, &mut |_| None);
    let (aggregate, _) = usage::partitioned_aggregate_related(
        &every,
        &[GroupBy::Session],
        &overlap,
        prices,
        &relations,
    );
    let mut by_session: HashMap<SessionKey, UsageTally> = aggregate
        .groups
        .into_iter()
        .filter_map(|group| {
            Some((
                SessionKey {
                    harness: group.key.session_harness?,
                    id: group.key.session?,
                },
                group.tally,
            ))
        })
        .collect();
    let sessions: Vec<FamilySession> = taken
        .iter()
        .filter_map(|taken| {
            let member = &roster.members[taken.position];
            let key = SessionKey::of(&member.session);
            let usage = by_session.remove(&key)?;
            Some(FamilySession {
                harness: key.harness,
                id: key.id,
                role: member.role,
                agent: agent_label(&taken.figure),
                link: member.link,
                parent: member.parent.clone(),
                shared_requests: taken.shared,
                counted_in_parent_total: taken.in_parent_total,
                started_at: member.session.started_at,
                last_activity_at: member.session.last_activity_at,
                usage,
            })
        })
        .collect();

    // Rows and totals add the figures not already inside a parent's total.
    let counted: Vec<&Taken> = taken
        .iter()
        .filter(|taken| !taken.in_parent_total)
        .collect();
    let figures: Vec<Session> = counted.iter().map(|taken| taken.figure.clone()).collect();
    let mut agents = Vec::new();
    for role in [Role::Lead, Role::Subagent, Role::Delegate, Role::Fork] {
        let of_role: Vec<Session> = counted
            .iter()
            .filter(|taken| roster.members[taken.position].role == role)
            .map(|taken| taken.figure.clone())
            .collect();
        if of_role.is_empty() {
            continue;
        }
        let overlap = crate::overlap::Overlap::of(&of_role, &mut |_| None);
        let (grouped, _) = usage::partitioned_aggregate_related(
            &of_role,
            &[GroupBy::Harness, GroupBy::Model, GroupBy::Variant],
            &overlap,
            prices,
            &relations,
        );
        for group in grouped.groups {
            let mut label = group.key.harness.clone().unwrap_or_default();
            if let Some(model) = &group.key.model {
                label.push(':');
                label.push_str(model);
                if let Some(variant) = &group.key.variant {
                    label.push('#');
                    label.push_str(variant);
                }
            }
            agents.push(FamilyAgent {
                agent: label,
                role,
                key: group.key,
                usage: group.tally,
            });
        }
    }
    let overlap = crate::overlap::Overlap::of(&figures, &mut |_| None);
    let (everything, partitions) =
        usage::partitioned_aggregate_related(&figures, &[], &overlap, prices, &relations);
    Measured {
        agents,
        sessions,
        totals: everything.totals,
        partitions,
        withheld,
    }
}
