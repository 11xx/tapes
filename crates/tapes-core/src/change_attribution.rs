//! A session's consumption divided among the commits of one repository, from
//! what its recording and the repository show.
//!
//! The items are the repository's commits. Beside them stand two buckets: a
//! session's work at a place after its last commit there is *uncommitted*,
//! per worktree and branch, and what no evidence places is *unattributed*,
//! each part with its [`Reason`]. A session's time in a directory that exists
//! outside the repository is unattributed as [`Reason::Elsewhere`]: no
//! working-stretch estimate reaches into it.
//!
//! Evidence is tiered by what establishes it:
//!
//! - **bound**: the session created the commit. Its recording names the sha,
//!   and the sha is the commit or carries its stable patch id.
//! - **matched**: an edit whose added lines the commit adds, for at least
//!   [`Params::edit_overlap`] of them over at least [`Params::edit_lines`],
//!   made no later than [`Params::edit_authoring`] after the commit's
//!   committer time, since a later edit applied the commit rather than wrote
//!   it; and a worktree span, the session's stay in a worktree or on a branch
//!   whose commits are known.
//! - **estimated**: a request within [`Params::proximity`] of the commit's
//!   author time, made while the session was at one of the repository's
//!   places; and a revision an inspection names.
//!
//! A sha that names the commit only through its added lines drops a tier.
//! Evidence for a commit is admitted only inside the commit's window, from
//! [`Params::lookback`] before the earlier of its author and committer times
//! to [`Params::lookahead`] after the later.
//!
//! Every tier takes part, and each share names the tier and basis that placed
//! it. Worktree spans are interval evidence. Creations and edits are point
//! evidence: an item's events span it from the first to the last, and cut the
//! working stretches no span covers. Estimated evidence only cuts working
//! stretches; it spans nothing. One piece of evidence naming several commits
//! gives each an even share of what it places.
//!
//! A worktree span is divided by the commits known at its place, in order:
//! requests up to a commit's creation serve that commit, and those after the
//! session's last commit there serve the place's uncommitted bucket until the
//! stay ends or the branch's next commit arrives. A stay with no commit
//! inside it serves the place's next known commit. A commit is known at a
//! place when the session created it there, or when the repository's history
//! names it on the place's branch: a live branch, or a merge whose subject
//! names the branch. No commit is known on the default branch; its commits
//! only end uncommitted work.
//!
//! A segment several items cover is split evenly among them. A child session
//! is first placed by its own evidence, its request times excepted: they are
//! its parent's, so a child has no proximity evidence. What its own evidence
//! leaves unplaced inherits its parent's allocation at the same time, with
//! basis `inherited`.
//!
//! The partition of a session runs over every commit its evidence admits, so
//! a commit's amount never depends on which commits a view selects.
//!
//! Amounts count each request once. A Claude recording's recorded total
//! counts its subagents' requests, which are members with amounts of their
//! own, so the part of the total no request explains is measured against
//! the requests of the whole family. Cache writes keep the TTL each request
//! recorded, through every even share, so each share is priced at its own
//! rates.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::Duration;
use serde::Serialize;

use crate::attribution::{
    even_split, Allocation, Basis, Event, Evidence, Owner, Piece as EnginePiece, Plan, Reason,
    Requests, Span, Tier,
};
use crate::span::Instant;

mod gather;
mod repository;

pub use gather::{
    attribute, attribute_within, gather, Gathered, Member, MemberRule, Membership,
    RepositoryAttribution,
};
pub use repository::{normalize_remote, Repository};

/// The thresholds evidence is admitted and weighed by.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// How long before the earlier of a commit's author and committer times
    /// its evidence is admitted.
    pub lookback: Duration,
    /// How long after the later of the two its evidence is admitted.
    pub lookahead: Duration,
    /// The idle gap that cuts a session into working stretches.
    pub idle: Duration,
    /// The least share of an edit's added lines a commit must add.
    pub edit_overlap: f64,
    /// The fewest distinct added lines an edit must have to match.
    pub edit_lines: usize,
    /// How long after a commit's committer time an edit may still be
    /// evidence of writing it: a later edit applied the commit rather than
    /// wrote it, and the tolerance holds a call that edits and commits at
    /// once. `None` admits an edit anywhere in the commit's window.
    pub edit_authoring: Option<Duration>,
    /// How close to a commit's author time a request must lie.
    pub proximity: Duration,
    /// The least share of a session's timed requests a stay a tool argument
    /// names must hold to count as presence.
    pub presence_share: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            lookback: Duration::minutes(60),
            lookahead: Duration::minutes(30),
            idle: Duration::minutes(45),
            edit_overlap: 0.5,
            edit_lines: 2,
            edit_authoring: Some(Duration::minutes(1)),
            proximity: Duration::minutes(30),
            presence_share: 0.25,
        }
    }
}

/// What a share is given to: a commit, or a place's uncommitted work.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Work {
    Commit {
        sha: String,
    },
    /// A session's work in a worktree, on a branch, after its last commit
    /// there.
    Uncommitted {
        worktree: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
}

impl Work {
    pub fn commit(sha: impl Into<String>) -> Self {
        Self::Commit { sha: sha.into() }
    }

    /// The commit's sha, for a commit.
    pub fn sha(&self) -> Option<&str> {
        match self {
            Self::Commit { sha } => Some(sha),
            Self::Uncommitted { .. } => None,
        }
    }
}

/// What the engine divides among: one item, or the commits one piece of
/// evidence names together, which share evenly what it places.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    One(Work),
    Several(Vec<String>),
}

impl Key {
    fn commits(mut shas: Vec<String>) -> Option<Self> {
        shas.sort();
        shas.dedup();
        match shas.len() {
            0 => None,
            1 => Some(Self::One(Work::commit(shas.pop()?))),
            _ => Some(Self::Several(shas)),
        }
    }
}

/// The kind of evidence that admitted an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// The session created the commit.
    Created,
    /// An edit added lines the commit adds.
    Edited,
    /// The session stayed in a worktree or on a branch whose commits are known.
    Worktree,
    /// A request lies near the commit's author time.
    Proximity,
    /// An inspection named the commit.
    Inspected,
}

/// How an observed sha came to name a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    /// The sha is the commit.
    Exact,
    /// The observed object carries the commit's stable patch id.
    PatchId,
    /// The commit adds most of the observed object's added lines.
    Digests,
}

/// One piece of evidence the partition admitted: digests, shas, paths and
/// times, never transcript text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Admitted {
    pub kind: Kind,
    pub tier: Tier,
    pub at: Instant,
    /// A span's end, where it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Instant>,
    pub work: Vec<Work>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
    /// The sha the recording named, where it differs from the commit's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
}

/// One commit, as the partition weighs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFacts {
    pub sha: String,
    pub author: Instant,
    pub committer: Instant,
    pub merge: bool,
    /// Line digests of the lines it adds, sorted, each once.
    pub added: Vec<u64>,
}

impl CommitFacts {
    /// The interval inside which evidence for the commit is admitted.
    pub fn window(&self, params: &Params) -> (Instant, Instant) {
        (
            self.author.min(self.committer) - params.lookback,
            self.author.max(self.committer) + params.lookahead,
        )
    }

    fn admits(&self, at: Instant, params: &Params) -> bool {
        let (from, to) = self.window(params);
        from <= at && at <= to
    }
}

/// What the repository says, as the partition reads it.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// Every commit evidence may name, oldest first.
    pub commits: Vec<CommitFacts>,
    /// The repository's worktree directories, live or known from the
    /// commits made in them. A path belongs to the deepest that holds it.
    pub places: Vec<PathBuf>,
    /// The branch each live worktree has checked out.
    pub worktree_branches: BTreeMap<PathBuf, String>,
    /// The commits the repository's history names on each branch.
    pub branch_commits: BTreeMap<String, BTreeSet<String>>,
    /// The commits each live branch's first-parent history received, the
    /// default branch included.
    pub arrivals: BTreeMap<String, BTreeSet<String>>,
}

impl Facts {
    /// The worktree directory holding `path`.
    pub fn place_of(&self, path: &Path) -> Option<&PathBuf> {
        self.places
            .iter()
            .filter(|place| path.starts_with(place))
            .max_by_key(|place| place.components().count())
    }

    fn index(&self) -> Index<'_> {
        let mut digests: HashMap<u64, Vec<usize>> = HashMap::new();
        for (position, commit) in self.commits.iter().enumerate() {
            for digest in &commit.added {
                digests.entry(*digest).or_default().push(position);
            }
        }
        let by_sha = self
            .commits
            .iter()
            .enumerate()
            .map(|(position, commit)| (commit.sha.as_str(), position))
            .collect();
        Index {
            facts: self,
            digests,
            by_sha,
        }
    }
}

struct Index<'a> {
    facts: &'a Facts,
    digests: HashMap<u64, Vec<usize>>,
    by_sha: HashMap<&'a str, usize>,
}

impl Index<'_> {
    fn commit(&self, sha: &str) -> Option<&CommitFacts> {
        self.by_sha.get(sha).map(|at| &self.facts.commits[*at])
    }

    /// The commits that add at least `share` of `added`, each once.
    fn adding(&self, added: &[u64], share: f64) -> Vec<&CommitFacts> {
        let distinct: BTreeSet<u64> = added.iter().copied().collect();
        if distinct.is_empty() {
            return Vec::new();
        }
        let mut found: BTreeMap<usize, usize> = BTreeMap::new();
        for digest in &distinct {
            for position in self.digests.get(digest).into_iter().flatten() {
                *found.entry(*position).or_default() += 1;
            }
        }
        found
            .into_iter()
            .filter(|(_, count)| *count as f64 >= share * distinct.len() as f64)
            .map(|(position, _)| &self.facts.commits[position])
            .collect()
    }
}

/// Where a session's recording placed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub at: Option<Instant>,
    pub cwd: PathBuf,
    pub branch: Option<String>,
    /// The directory exists and the repository does not hold it.
    pub outside: bool,
}

/// A commit a session's recording names, as the repository resolved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeenCommit {
    pub at: Option<Instant>,
    /// The call whose result named it.
    pub call: usize,
    pub created: bool,
    pub branch: Option<String>,
    /// As the recording named it.
    pub observed: String,
    /// The repository's commit it names, and how.
    pub resolved: Option<(String, Via)>,
}

/// A directory a tool call's arguments named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    pub at: Instant,
    pub path: PathBuf,
}

/// An edit a session made: when, to which path, and the digests of the
/// lines it added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditInput {
    pub at: Instant,
    pub path: PathBuf,
    pub added: Vec<u64>,
}

/// What one session's recording shows, ready to be partitioned.
#[derive(Clone, Debug, Default)]
pub struct SessionEvidence {
    /// The stamp of each counted request in recording order, none for an
    /// untimed one.
    pub requests: Vec<Option<Instant>>,
    /// The first stamp of each working stretch at [`Params::idle`], absent
    /// where the read keeps no timeline.
    pub starts: Option<Vec<Instant>>,
    pub locations: Vec<Location>,
    pub commits: Vec<SeenCommit>,
    pub mentions: Vec<Mention>,
    pub edits: Vec<EditInput>,
    /// The session inherits from a parent, whose evidence the time of its
    /// requests is: a child's requests near a commit are not its own
    /// evidence for it.
    pub inherits: bool,
}

impl SessionEvidence {
    fn timed(&self) -> Vec<Instant> {
        let mut timed: Vec<Instant> = self.requests.iter().flatten().copied().collect();
        timed.sort();
        timed
    }

    /// The stretches of the timeline the session spent outside the
    /// repository, oldest first.
    fn elsewhere(&self) -> Vec<Piece> {
        let mut marks: Vec<&Location> = self.locations.iter().collect();
        marks.sort_by_key(|location| location.at);
        let mut outside: Vec<Piece> = Vec::new();
        for (index, location) in marks.iter().enumerate() {
            if !location.outside {
                continue;
            }
            let to = marks.get(index + 1).and_then(|next| next.at);
            match outside.last_mut() {
                Some(last) if last.to == location.at => last.to = to,
                _ => outside.push(Piece {
                    from: location.at,
                    to,
                }),
            }
        }
        outside.retain(|piece| piece.from.zip(piece.to).is_none_or(|(from, to)| from < to));
        outside
    }
}

/// One disjoint part `[from, to)` of a session's timeline, an absent bound
/// being the timeline's beginning or end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Piece {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Instant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Instant>,
}

impl Piece {
    fn holds(&self, at: Instant) -> bool {
        self.from.is_none_or(|from| at >= from) && self.to.is_none_or(|to| at < to)
    }

    pub fn bounds(&self) -> crate::span::SpanBounds {
        crate::span::SpanBounds::Time {
            from: self.from,
            to: self.to,
        }
    }

    fn meet(&self, other: &Piece) -> Option<Piece> {
        let from = match (self.from, other.from) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, None) | (None, a) => a,
        };
        let to = match (self.to, other.to) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, None) | (None, a) => a,
        };
        from.zip(to)
            .is_none_or(|(from, to)| from < to)
            .then_some(Piece { from, to })
    }
}

/// One even share: the `index`th of `shares`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Split {
    pub index: usize,
    pub shares: usize,
}

/// One part of a session given to its owner: the piece of the timeline it
/// is read from, none for the requests no time places, and the even shares
/// of that piece it takes, outermost first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub piece: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub split: Vec<Split>,
    pub owner: Owner<Work>,
}

/// A session divided: its pieces, each part's owner, and the evidence that
/// placed them. The parts add up to the whole session.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Partition {
    pub pieces: Vec<Piece>,
    pub entries: Vec<Entry>,
    pub evidence: Vec<Admitted>,
    /// Working stretches were known, so the estimate could cut them.
    pub stretches: bool,
}

/// One stay of a session at a place in the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stay {
    from: Option<Instant>,
    to: Option<Instant>,
    place: Option<PathBuf>,
    branch: Option<String>,
    mentioned: bool,
}

/// Where the session was, from its recorded locations and from the
/// directories its tool calls named. A named directory moves the session
/// there until its recording next shows a location; a directory whose named
/// stays hold less than [`Params::presence_share`] of its timed requests is
/// passing mention, not presence.
fn stays(evidence: &SessionEvidence, facts: &Facts, params: &Params) -> Vec<Stay> {
    let timed = evidence.timed();
    let all = fold_stays(evidence, facts, &|_| true);
    let mut held: BTreeMap<&PathBuf, usize> = BTreeMap::new();
    for stay in all.iter().filter(|stay| stay.mentioned) {
        let piece = Piece {
            from: stay.from,
            to: stay.to,
        };
        let count = timed.iter().filter(|at| piece.holds(**at)).count();
        if let Some(place) = &stay.place {
            *held.entry(place).or_default() += count;
        }
    }
    let present: BTreeSet<PathBuf> = held
        .into_iter()
        .filter(|(_, count)| {
            !timed.is_empty() && *count as f64 >= params.presence_share * timed.len() as f64
        })
        .map(|(place, _)| place.clone())
        .collect();
    fold_stays(evidence, facts, &|place| present.contains(place))
}

fn fold_stays(
    evidence: &SessionEvidence,
    facts: &Facts,
    keep: &dyn Fn(&PathBuf) -> bool,
) -> Vec<Stay> {
    // Recorded locations come before a mention at the same instant.
    let mut marks: Vec<Mark> = Vec::new();
    for location in &evidence.locations {
        marks.push(Mark {
            at: location.at,
            mentioned: false,
            place: facts.place_of(&location.cwd).cloned(),
            branch: location.branch.clone(),
        });
    }
    for mention in &evidence.mentions {
        if let Some(place) = facts.place_of(&mention.path).filter(|place| keep(place)) {
            marks.push(Mark {
                at: Some(mention.at),
                mentioned: true,
                place: Some(place.clone()),
                branch: None,
            });
        }
    }
    marks.sort_by_key(|mark| (mark.at, mark.mentioned));
    let mut stays: Vec<Stay> = Vec::new();
    for Mark {
        at,
        mentioned,
        place,
        branch,
    } in marks
    {
        if let Some(current) = stays.last() {
            let same = current.place == place && (mentioned || current.branch == branch);
            if same {
                continue;
            }
        }
        if let Some(current) = stays.last_mut() {
            current.to = at;
        }
        stays.push(Stay {
            from: at,
            to: None,
            place,
            branch,
            mentioned,
        });
    }
    // A stay that begins and ends at one instant holds nothing.
    stays.retain(|stay| stay.from.zip(stay.to).is_none_or(|(from, to)| from < to));
    stays
}

/// One place the recording shows the session at, from `at`.
struct Mark {
    at: Option<Instant>,
    mentioned: bool,
    place: Option<PathBuf>,
    branch: Option<String>,
}

/// A commit the session created, and where it stayed then.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Made {
    at: Instant,
    sha: String,
    /// The branch the creation named.
    branch: Option<String>,
    place: PathBuf,
    /// The branch the stay recorded.
    stayed: Option<String>,
}

/// A span over `[from, to)` cut to the commit's window.
fn windowed(
    commit: &CommitFacts,
    from: Option<Instant>,
    to: Instant,
    params: &Params,
) -> Option<(Instant, Instant)> {
    let (low, high) = commit.window(params);
    let from = from.map_or(low, |from| from.max(low));
    let to = to.min(high);
    (from < to).then_some((from, to))
}

/// The engine's inputs and what admitted them.
struct Inputs {
    spans: Vec<Span<Key>>,
    events: Vec<Event<Key>>,
    cues: Vec<Event<Key>>,
    admitted: Vec<Admitted>,
}

fn one_ns() -> Duration {
    Duration::nanoseconds(1)
}

/// Turn a session's evidence into spans, events and cues.
fn inputs(evidence: &SessionEvidence, facts: &Facts, params: &Params) -> Inputs {
    let index = facts.index();
    let mut inputs = Inputs {
        spans: Vec::new(),
        events: Vec::new(),
        cues: Vec::new(),
        admitted: Vec::new(),
    };
    // A sha resolved through its lines alone is one tier weaker.
    let weaken = |tier: Tier, via: Via| match (tier, via) {
        (Tier::Bound, Via::Digests) => Tier::Matched,
        (_, Via::Digests) => Tier::Estimated,
        (tier, _) => tier,
    };

    // Creations and inspections, one piece of evidence per call.
    let mut calls: BTreeMap<(usize, bool), Vec<&SeenCommit>> = BTreeMap::new();
    for seen in &evidence.commits {
        calls
            .entry((seen.call, seen.created))
            .or_default()
            .push(seen);
    }
    for ((_, created), seen) in &calls {
        let mut by_tier: BTreeMap<Tier, (Instant, Vec<String>, Via, Vec<String>)> = BTreeMap::new();
        for one in seen {
            let (Some(at), Some((sha, via))) = (one.at, &one.resolved) else {
                continue;
            };
            let Some(commit) = index.commit(sha).filter(|commit| commit.admits(at, params)) else {
                continue;
            };
            let base = if *created {
                Tier::Bound
            } else {
                Tier::Estimated
            };
            let tier = weaken(base, *via);
            let entry = by_tier
                .entry(tier)
                .or_insert_with(|| (at, Vec::new(), *via, Vec::new()));
            entry.1.push(commit.sha.clone());
            entry.2 = entry.2.max(*via);
            if one.observed != commit.sha {
                entry.3.push(one.observed.clone());
            }
        }
        for (tier, (at, shas, via, observed)) in by_tier {
            let Some(work) = Key::commits(shas.clone()) else {
                continue;
            };
            let event = Event { at, work, tier };
            let kind = if *created {
                inputs.events.push(event);
                Kind::Created
            } else {
                inputs.cues.push(event);
                Kind::Inspected
            };
            inputs.admitted.push(Admitted {
                kind,
                tier,
                at,
                to: None,
                work: sorted_commits(shas),
                via: Some(via),
                observed: observed.first().cloned(),
            });
        }
    }

    // Edits whose added lines a commit adds.
    for edit in &evidence.edits {
        let distinct: BTreeSet<u64> = edit.added.iter().copied().collect();
        if distinct.len() < params.edit_lines {
            continue;
        }
        let shas: Vec<String> = index
            .adding(&edit.added, params.edit_overlap)
            .into_iter()
            .filter(|commit| commit.admits(edit.at, params))
            .filter(|commit| {
                params
                    .edit_authoring
                    .is_none_or(|tolerance| edit.at <= commit.committer + tolerance)
            })
            .map(|commit| commit.sha.clone())
            .collect();
        let Some(work) = Key::commits(shas.clone()) else {
            continue;
        };
        inputs.events.push(Event {
            at: edit.at,
            work,
            tier: Tier::Matched,
        });
        inputs.admitted.push(Admitted {
            kind: Kind::Edited,
            tier: Tier::Matched,
            at: edit.at,
            to: None,
            work: sorted_commits(shas),
            via: None,
            observed: None,
        });
    }

    // Worktree spans.
    worktree_spans(evidence, facts, &index, params, &mut inputs);

    // Requests near a commit's author time.
    // Only a request the session made inside the repository is near a
    // commit in any sense that matters.
    let inside: Vec<Piece> = stays(evidence, facts, params)
        .into_iter()
        .filter(|stay| stay.place.is_some())
        .map(|stay| Piece {
            from: stay.from,
            to: stay.to,
        })
        .collect();
    let timed: Vec<Instant> = if evidence.inherits {
        Vec::new()
    } else {
        evidence
            .timed()
            .into_iter()
            .filter(|at| inside.iter().any(|piece| piece.holds(*at)))
            .collect()
    };
    for commit in facts.commits.iter().filter(|commit| !commit.merge) {
        let at = commit.author;
        let start = timed.partition_point(|request| *request < at - params.proximity);
        if timed
            .get(start)
            .is_some_and(|request| *request <= at + params.proximity)
        {
            inputs.cues.push(Event {
                at,
                work: Key::One(Work::commit(commit.sha.clone())),
                tier: Tier::Estimated,
            });
            inputs.admitted.push(Admitted {
                kind: Kind::Proximity,
                tier: Tier::Estimated,
                at,
                to: None,
                work: vec![Work::commit(commit.sha.clone())],
                via: None,
                observed: None,
            });
        }
    }
    inputs
}

fn sorted_commits(mut shas: Vec<String>) -> Vec<Work> {
    shas.sort();
    shas.dedup();
    shas.into_iter().map(Work::commit).collect()
}

/// The spans of the session's stays at places whose commits are known.
fn worktree_spans(
    evidence: &SessionEvidence,
    facts: &Facts,
    index: &Index<'_>,
    params: &Params,
    inputs: &mut Inputs,
) {
    let stays = stays(evidence, facts, params);
    // The commits the session created, by the place it stayed at then.
    let mut created: Vec<Made> = Vec::new();
    for seen in evidence.commits.iter().filter(|seen| seen.created) {
        let (Some(at), Some((sha, via))) = (seen.at, &seen.resolved) else {
            continue;
        };
        if *via == Via::Digests || index.commit(sha).is_none() {
            continue;
        }
        let Some(stay) = stays.iter().find(|stay| {
            Piece {
                from: stay.from,
                to: stay.to,
            }
            .holds(at)
        }) else {
            continue;
        };
        let Some(place) = &stay.place else {
            continue;
        };
        created.push(Made {
            at,
            sha: sha.clone(),
            branch: seen.branch.clone(),
            place: place.clone(),
            stayed: stay.branch.clone(),
        });
    }
    created.sort();
    for stay in &stays {
        let Some(place) = &stay.place else {
            continue;
        };
        let branch = stay
            .branch
            .clone()
            .or_else(|| facts.worktree_branches.get(place).cloned());
        let piece = Piece {
            from: stay.from,
            to: stay.to,
        };
        // The session's own creations at this place and branch.
        let own: Vec<&Made> = created
            .iter()
            .filter(|made| made.place == *place && made.stayed == stay.branch)
            .collect();
        let known: Vec<&CommitFacts> = branch
            .as_ref()
            .and_then(|branch| facts.branch_commits.get(branch))
            .into_iter()
            .flatten()
            .filter_map(|sha| index.commit(sha))
            .filter(|commit| !own.iter().any(|made| made.sha == commit.sha))
            .collect();
        let mut points: BTreeMap<Instant, BTreeSet<&str>> = BTreeMap::new();
        for made in own.iter().filter(|made| piece.holds(made.at)) {
            points.entry(made.at).or_default().insert(&made.sha);
        }
        for commit in known.iter().filter(|commit| piece.holds(commit.committer)) {
            points
                .entry(commit.committer)
                .or_default()
                .insert(&commit.sha);
        }
        let mut cursor = stay.from;
        for (at, shas) in &points {
            let end = *at + one_ns();
            for sha in shas {
                let Some(commit) = index.commit(sha) else {
                    continue;
                };
                if let Some((from, to)) = windowed(commit, cursor, end, params) {
                    push_span(inputs, Work::commit(commit.sha.clone()), from, Some(to));
                }
            }
            cursor = Some(end);
        }
        // After the session's last commit at this place, up to the stay's
        // last division.
        let last_own = own
            .iter()
            .rfind(|made| cursor.is_some_and(|cursor| made.at < cursor));
        if let Some(made) = last_own {
            let from = cursor.or(stay.from);
            // The branch's next commit ends the uncommitted work.
            let arrived = branch
                .as_ref()
                .and_then(|branch| facts.arrivals.get(branch))
                .into_iter()
                .flatten()
                .filter_map(|sha| index.commit(sha))
                .filter(|commit| !own.iter().any(|made| made.sha == commit.sha))
                .map(|commit| commit.committer)
                .filter(|at| from.is_none_or(|from| *at > from))
                .min();
            let to = match (stay.to, arrived) {
                (Some(to), Some(arrived)) => Some(to.min(arrived)),
                (to, arrived) => to.or(arrived),
            };
            let nonempty = from.zip(to).is_none_or(|(from, to)| from < to);
            if let (Some(from), true) = (from, nonempty) {
                let work = Work::Uncommitted {
                    worktree: place.clone(),
                    branch: branch.clone().or_else(|| made.branch.clone()),
                };
                push_span(inputs, work, from, to);
            }
        } else if let Some(to) = stay.to {
            let next = known
                .iter()
                .filter(|commit| commit.committer >= to)
                .min_by_key(|commit| (commit.committer, commit.sha.clone()));
            if let Some(commit) = next {
                if let Some((from, to)) = windowed(commit, cursor, to, params) {
                    push_span(inputs, Work::commit(commit.sha.clone()), from, Some(to));
                }
            }
        }
    }
}

fn push_span(inputs: &mut Inputs, work: Work, from: Instant, to: Option<Instant>) {
    inputs.spans.push(Span {
        work: Key::One(work.clone()),
        run: None,
        from,
        to,
        tier: Tier::Matched,
    });
    inputs.admitted.push(Admitted {
        kind: Kind::Worktree,
        tier: Tier::Matched,
        at: from,
        to,
        work: vec![work],
        via: None,
        observed: None,
    });
}

/// Divide one session by its own evidence.
pub fn partition(evidence: &SessionEvidence, facts: &Facts, params: &Params) -> Partition {
    let inputs = inputs(evidence, facts, params);
    let plan = Plan::with_cues(
        &inputs.spans,
        &inputs.events,
        &inputs.cues,
        evidence.starts.as_deref(),
    );
    let zero = Requests {
        segments: vec![0; plan.segments.len()],
        windows: vec![0; plan.windows.len()],
        remainder: 0,
    };
    let attribution = plan
        .attribute(&zero, Some(Allocation::Even), true)
        .expect("the counts have the plan's shape");
    let even = attribution
        .allocated
        .as_ref()
        .expect("an allocation was asked for");
    // The estimate, with every segment several items cover split evenly.
    let mut parts: Vec<(EnginePiece, Owner<Key>)> = attribution
        .estimated
        .parts
        .iter()
        .filter(|(_, owner)| !matches!(owner, Owner::Unattributed(Reason::Overlap)))
        .map(|(part, owner)| (part.piece, owner.clone()))
        .collect();
    parts.extend(
        even.parts
            .iter()
            .filter(|(part, _)| matches!(part.piece, EnginePiece::Even { .. }))
            .map(|(part, owner)| (part.piece, owner.clone())),
    );

    let mut pieces: Vec<Piece> = Vec::new();
    let mut positions: BTreeMap<(bool, usize), usize> = BTreeMap::new();
    let mut piece_of = |window: bool, index: usize| -> usize {
        *positions.entry((window, index)).or_insert_with(|| {
            pieces.push(if window {
                let window = &plan.windows[index];
                Piece {
                    from: window.from,
                    to: window.to,
                }
            } else {
                let segment = &plan.segments[index];
                Piece {
                    from: segment.from,
                    to: segment.to,
                }
            });
            pieces.len() - 1
        })
    };
    let mut entries: Vec<Entry> = Vec::new();
    for (piece, owner) in parts {
        let (piece, split) = match piece {
            EnginePiece::Segment(index) => (Some(piece_of(false, index)), Vec::new()),
            EnginePiece::Window(index) => (Some(piece_of(true, index)), Vec::new()),
            EnginePiece::Even {
                segment,
                index,
                shares,
            } => (
                Some(piece_of(false, segment)),
                vec![Split { index, shares }],
            ),
            EnginePiece::Remainder => (None, Vec::new()),
        };
        match owner {
            Owner::Unattributed(reason) => entries.push(Entry {
                piece,
                split,
                owner: Owner::Unattributed(reason),
            }),
            Owner::Item {
                work: Key::One(work),
                evidence,
            } => entries.push(Entry {
                piece,
                split,
                owner: Owner::Item { work, evidence },
            }),
            Owner::Item {
                work: Key::Several(shas),
                evidence,
            } => {
                let shares = shas.len();
                for (index, sha) in shas.into_iter().enumerate() {
                    let mut split = split.clone();
                    split.push(Split { index, shares });
                    entries.push(Entry {
                        piece,
                        split,
                        owner: Owner::Item {
                            work: Work::commit(sha),
                            evidence: Evidence {
                                tier: evidence.tier,
                                basis: Basis::Even,
                            },
                        },
                    });
                }
            }
        }
    }
    let entries = cut_elsewhere(&mut pieces, entries, &evidence.elsewhere());
    let mut admitted = inputs.admitted;
    admitted.sort_by(|a, b| {
        (a.at, a.kind, &a.work, a.tier, a.to).cmp(&(b.at, b.kind, &b.work, b.tier, b.to))
    });
    admitted.dedup();
    Partition {
        pieces,
        entries,
        evidence: admitted,
        stretches: evidence.starts.is_some(),
    }
}

/// Give the parts of working-stretch estimates that lie where the session
/// worked outside the repository to [`Reason::Elsewhere`]: a stretch's
/// evidence speaks for the repository only while the session was in it.
fn cut_elsewhere(pieces: &mut Vec<Piece>, entries: Vec<Entry>, elsewhere: &[Piece]) -> Vec<Entry> {
    if elsewhere.is_empty() {
        return entries;
    }
    let mut cut = Vec::new();
    for entry in entries {
        let estimated = matches!(
            &entry.owner,
            Owner::Item { evidence, .. } if evidence.basis == Basis::WorkingStretch
        );
        let Some(at) = entry.piece.filter(|_| estimated) else {
            cut.push(entry);
            continue;
        };
        let own = pieces[at];
        let mut away: Vec<Piece> = elsewhere
            .iter()
            .filter_map(|piece| own.meet(piece))
            .collect();
        if away.is_empty() {
            cut.push(entry);
            continue;
        }
        let mut place = |piece: Piece| match pieces.iter().position(|held| *held == piece) {
            Some(position) => position,
            None => {
                pieces.push(piece);
                pieces.len() - 1
            }
        };
        for piece in away.clone() {
            cut.push(Entry {
                piece: Some(place(piece)),
                split: entry.split.clone(),
                owner: Owner::Unattributed(Reason::Elsewhere),
            });
        }
        for piece in uncovered(own, &mut away) {
            cut.push(Entry {
                piece: Some(place(piece)),
                split: entry.split.clone(),
                owner: entry.owner.clone(),
            });
        }
    }
    cut
}

impl Partition {
    /// Keep only what lies inside `within`: each piece cut to it, and the
    /// entries of the pieces it meets. Requests no time places lie in no
    /// window.
    pub fn cut_to(&mut self, within: Piece) {
        let mut pieces = Vec::new();
        let mut moved: BTreeMap<usize, usize> = BTreeMap::new();
        for (position, piece) in self.pieces.iter().enumerate() {
            if let Some(inside) = piece.meet(&within) {
                moved.insert(position, pieces.len());
                pieces.push(inside);
            }
        }
        self.entries = std::mem::take(&mut self.entries)
            .into_iter()
            .filter_map(|entry| {
                let piece = *moved.get(&entry.piece?)?;
                Some(Entry {
                    piece: Some(piece),
                    ..entry
                })
            })
            .collect();
        self.pieces = pieces;
    }

    /// Give what this partition leaves unattributed in time to whatever
    /// `parent` gives the same time, with basis `inherited`; what the parent
    /// leaves unattributed there stays unattributed.
    pub fn inherit(&mut self, parent: &Partition) {
        let mut pieces: Vec<Piece> = Vec::new();
        let mut entries: Vec<Entry> = Vec::new();
        let place = |pieces: &mut Vec<Piece>, piece: Piece| -> usize {
            match pieces.iter().position(|held| *held == piece) {
                Some(position) => position,
                None => {
                    pieces.push(piece);
                    pieces.len() - 1
                }
            }
        };
        // The parent's owned time, piece by piece.
        let mut owned: BTreeMap<usize, Vec<&Entry>> = BTreeMap::new();
        for entry in &parent.entries {
            if let (Some(piece), Owner::Item { .. }) = (entry.piece, &entry.owner) {
                owned.entry(piece).or_default().push(entry);
            }
        }
        for entry in &self.entries {
            let inheritable = !matches!(entry.owner, Owner::Unattributed(Reason::Elsewhere));
            let (Some(at), Owner::Unattributed(reason), true) =
                (entry.piece, &entry.owner, inheritable)
            else {
                let piece = entry.piece.map(|at| place(&mut pieces, self.pieces[at]));
                entries.push(Entry {
                    piece,
                    split: entry.split.clone(),
                    owner: entry.owner.clone(),
                });
                continue;
            };
            let own = self.pieces[at];
            let mut covered: Vec<Piece> = Vec::new();
            for (parent_piece, given) in &owned {
                let Some(meet) = own.meet(&parent.pieces[*parent_piece]) else {
                    continue;
                };
                covered.push(meet);
                let position = place(&mut pieces, meet);
                for given in given {
                    let Owner::Item { work, evidence } = &given.owner else {
                        continue;
                    };
                    let mut split = entry.split.clone();
                    split.extend(given.split.iter().copied());
                    entries.push(Entry {
                        piece: Some(position),
                        split,
                        owner: Owner::Item {
                            work: work.clone(),
                            evidence: Evidence {
                                tier: evidence.tier,
                                basis: Basis::Inherited,
                            },
                        },
                    });
                }
            }
            for rest in uncovered(own, &mut covered) {
                let position = place(&mut pieces, rest);
                entries.push(Entry {
                    piece: Some(position),
                    split: entry.split.clone(),
                    owner: Owner::Unattributed(*reason),
                });
            }
        }
        self.pieces = pieces;
        self.entries = entries;
    }

    /// The requests each entry holds, counted from the session's request
    /// stamps: an even share holds its share of its piece's requests.
    pub fn requests(&self, requests: &[Option<Instant>]) -> Vec<u64> {
        let mut held = vec![0_u64; self.pieces.len()];
        let mut untimed = 0_u64;
        for request in requests {
            match request {
                Some(at) => {
                    if let Some(position) = self.pieces.iter().position(|piece| piece.holds(*at)) {
                        held[position] += 1;
                    }
                }
                None => untimed += 1,
            }
        }
        self.entries
            .iter()
            .map(|entry| {
                let whole = entry.piece.map_or(untimed, |piece| held[piece]);
                entry.split.iter().fold(whole, |count, split| {
                    even_split(count, split.shares, split.index)
                })
            })
            .collect()
    }
}

/// The parts of `piece` that none of `covered`, disjoint parts of it, holds.
fn uncovered(piece: Piece, covered: &mut [Piece]) -> Vec<Piece> {
    covered.sort();
    let mut rest = Vec::new();
    let mut cursor = piece.from;
    for held in covered.iter() {
        if let Some(start) = held.from {
            if cursor.is_none_or(|cursor| cursor < start) {
                rest.push(Piece {
                    from: cursor,
                    to: Some(start),
                });
            }
        }
        match held.to {
            Some(end) => cursor = Some(end),
            None => return rest,
        }
    }
    if cursor.zip(piece.to).is_none_or(|(from, to)| from < to) {
        rest.push(Piece {
            from: cursor,
            to: piece.to,
        });
    }
    rest
}

/// Tokens one share holds, by counter. `cache_write` counts every cache
/// write; `ephemeral_5m` and `ephemeral_1h` the part of it the requests
/// recorded at each TTL, and the rest was written at no recorded TTL, so a
/// share's writes are priced at their own rates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub ephemeral_5m: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub ephemeral_1h: u64,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

impl Counts {
    /// A figure's counters, its cache writes split by `ttl` where the split
    /// accounts for every one of them, and otherwise at no recorded TTL.
    pub fn of(tokens: &crate::model::Tokens, ttl: Option<&crate::model::CacheWrites>) -> Self {
        let cache_write = tokens.cache_write.unwrap_or(0);
        let split = ttl.filter(|split| split.total() == cache_write);
        Self {
            input: tokens.input.unwrap_or(0),
            output: tokens.output.unwrap_or(0),
            reasoning: tokens.reasoning.unwrap_or(0),
            cache_read: tokens.cache_read.unwrap_or(0),
            cache_write,
            ephemeral_5m: split.and_then(|split| split.ephemeral_5m).unwrap_or(0),
            ephemeral_1h: split.and_then(|split| split.ephemeral_1h).unwrap_or(0),
        }
    }

    /// The cache writes no request recorded a TTL for.
    pub fn ttl_unrecorded(&self) -> u64 {
        self.cache_write
            .saturating_sub(self.ephemeral_5m)
            .saturating_sub(self.ephemeral_1h)
    }

    /// The cache writes by TTL.
    pub fn cache_writes(&self) -> crate::model::CacheWrites {
        let some = |count: u64| (count > 0).then_some(count);
        crate::model::CacheWrites {
            ephemeral_5m: some(self.ephemeral_5m),
            ephemeral_1h: some(self.ephemeral_1h),
            ttl_unrecorded: some(self.ttl_unrecorded()),
        }
    }

    /// The counters as a figure records them.
    pub fn tokens(&self) -> crate::model::Tokens {
        crate::model::Tokens {
            input: Some(self.input),
            output: Some(self.output),
            reasoning: Some(self.reasoning),
            cache_read: Some(self.cache_read),
            cache_write: Some(self.cache_write),
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
        self.reasoning += other.reasoning;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.ephemeral_5m += other.ephemeral_5m;
        self.ephemeral_1h += other.ephemeral_1h;
    }

    /// One even share. Each TTL's writes are split on their own, and the
    /// share's cache writes are the parts it takes, so every TTL's writes
    /// add up over the shares.
    fn even_share(&self, split: Split) -> Self {
        let share = |count| even_split(count, split.shares, split.index);
        let (five, hour, rest) = (
            share(self.ephemeral_5m),
            share(self.ephemeral_1h),
            share(self.ttl_unrecorded()),
        );
        Self {
            input: share(self.input),
            output: share(self.output),
            reasoning: share(self.reasoning),
            cache_read: share(self.cache_read),
            cache_write: five + hour + rest,
            ephemeral_5m: five,
            ephemeral_1h: hour,
        }
    }

    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

/// What one part of a session consumed: its requests, their tokens by
/// model, and by model the part of a recorded total no request explains
/// that its time holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Amount {
    pub requests: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, Counts>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unexplained: BTreeMap<String, Counts>,
}

impl Amount {
    pub fn add(&mut self, other: &Self) {
        self.requests += other.requests;
        for (model, counts) in &other.models {
            self.models.entry(model.clone()).or_default().add(counts);
        }
        for (model, counts) in &other.unexplained {
            self.unexplained
                .entry(model.clone())
                .or_default()
                .add(counts);
        }
    }

    fn even_share(&self, split: Split) -> Self {
        let share = |map: &BTreeMap<String, Counts>| {
            map.iter()
                .map(|(model, counts)| (model.clone(), counts.even_share(split)))
                .collect()
        };
        Self {
            requests: even_split(self.requests, split.shares, split.index),
            models: share(&self.models),
            unexplained: share(&self.unexplained),
        }
    }

    /// Every counter of every model, requests and unexplained parts summed.
    pub fn tokens(&self) -> Counts {
        let mut sum = Counts::default();
        for counts in self.models.values().chain(self.unexplained.values()) {
            sum.add(counts);
        }
        sum
    }

    pub fn is_zero(&self) -> bool {
        self.requests == 0
            && self.models.values().all(Counts::is_zero)
            && self.unexplained.values().all(Counts::is_zero)
    }
}

/// The amounts a read took for a partition: one per piece, the requests no
/// piece holds, and the whole session, or the window the pieces were cut
/// to.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Amounts {
    pub pieces: Vec<Amount>,
    pub remainder: Amount,
    pub whole: Amount,
    /// The pieces were cut to a window, which `whole` is.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub within: bool,
}

/// One owner's part of a session by one tier and basis.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Share {
    pub work: Work,
    pub evidence: Evidence,
    pub amount: Amount,
}

/// A session by its harness and native id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SessionKey {
    pub harness: String,
    pub id: String,
}

/// One session's consumption, divided.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionAttribution {
    pub session: SessionKey,
    /// The session whose allocation this one inherits where its own evidence
    /// places nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionKey>,
    /// The agent that served it, `harness:model#effort` as its recording
    /// names them.
    pub agent: String,
    /// Why its recording is part of the repository's evidence.
    pub rules: BTreeSet<MemberRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Instant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<Instant>,
    /// The billing its whole recording establishes.
    #[serde(skip)]
    pub billing: Option<crate::pricing::Billing>,
    pub partition: Partition,
    /// What the read took, absent where the session could not be read in
    /// pieces.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amounts: Option<Amounts>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl SessionAttribution {
    /// The amount of each entry.
    pub fn entry_amounts(&self) -> Option<Vec<Amount>> {
        let amounts = self.amounts.as_ref()?;
        Some(
            self.partition
                .entries
                .iter()
                .map(|entry| {
                    let whole = match entry.piece {
                        Some(piece) => amounts.pieces.get(piece).cloned().unwrap_or_default(),
                        None => amounts.remainder.clone(),
                    };
                    entry
                        .split
                        .iter()
                        .fold(whole, |amount, split| amount.even_share(*split))
                })
                .collect(),
        )
    }

    /// Every owner's share, by owner and then evidence, strongest first.
    pub fn shares(&self) -> Vec<Share> {
        let Some(amounts) = self.entry_amounts() else {
            return Vec::new();
        };
        let mut held: BTreeMap<(Work, Evidence), Amount> = BTreeMap::new();
        for (entry, amount) in self.partition.entries.iter().zip(amounts) {
            if let Owner::Item { work, evidence } = &entry.owner {
                held.entry((work.clone(), *evidence))
                    .or_default()
                    .add(&amount);
            }
        }
        held.into_iter()
            .map(|((work, evidence), amount)| Share {
                work,
                evidence,
                amount,
            })
            .collect()
    }

    /// What no owner holds, by reason.
    pub fn unattributed(&self) -> BTreeMap<Reason, Amount> {
        let mut held: BTreeMap<Reason, Amount> = BTreeMap::new();
        let Some(amounts) = self.entry_amounts() else {
            return held;
        };
        for (entry, amount) in self.partition.entries.iter().zip(amounts) {
            if let Owner::Unattributed(reason) = entry.owner {
                if !amount.is_zero() || reason != Reason::RequestTimeUnrecorded {
                    held.entry(reason).or_default().add(&amount);
                }
            }
        }
        held
    }

    /// The commits `selected` holds, their shares in this session; the
    /// shares of every other commit, apart; and the uncommitted buckets.
    pub fn select(&self, selected: &BTreeSet<String>) -> Selection {
        let mut selection = Selection::default();
        for share in self.shares() {
            match share.work.sha() {
                Some(sha) if selected.contains(sha) => selection.selected.push(share),
                Some(_) => selection.unselected.push(share),
                None => selection.uncommitted.push(share),
            }
        }
        selection
    }
}

/// A session's shares as a view selects them.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Selection {
    pub selected: Vec<Share>,
    pub unselected: Vec<Share>,
    pub uncommitted: Vec<Share>,
}
