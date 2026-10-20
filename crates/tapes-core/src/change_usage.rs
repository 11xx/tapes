//! What a change cost: the consumption [`crate::change_attribution`] divides
//! among a repository's commits, answered for the commits a view selects, by
//! commit, agent and role, each amount naming the tier and basis that placed
//! it, and priced where a table is given.
//!
//! A view selects commits by revision, range, branch or list, or asks for
//! one session's partition or a window's consumption across every commit.
//! The sessions in a commit view are those holding a share of a selected
//! commit, or that would hold one if every evidence tier took part. Each of
//! them is divided over every commit its evidence admits, so what a session
//! gives a commit does not depend on what else was selected, though a wider
//! selection reads the sessions of a wider interval; the shares
//! its evidence gives commits the view did not select are reported apart as
//! `unselected` and left out of the rows' total. A session's rows, its
//! uncommitted buckets, its unattributed parts and its unselected shares add
//! up to its whole. Where only some tiers are asked for, only their
//! evidence places consumption, and what only the others would place is
//! unattributed as `tiers not asked`.
//!
//! A session's role for a commit comes from its own evidence for that
//! commit: `author` when it created the commit, made an edit the commit
//! holds, or worked in the commit's worktree; `reviewer` when it inspected
//! the commit and has no authoring evidence for it; `proximate` when only
//! time places it near the commit. A share a child inherits takes the role
//! its parent holds for the commit.
//!
//! Evidence files place the sessions their `binding` and `event` records
//! name, alone, as [`crate::change_attribution`] says, and bring in those
//! whose work the view selects wherever their recordings began. A record's
//! role is the session's role for its work's commits: `reviewer` where the
//! records say so, and `author` otherwise. Work of which the repository
//! holds no commit stands apart as `labelled`, out of the rows' total like
//! `unselected`. With only the records asked for, the sessions they name
//! are the view's only sessions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::attribution::{Basis, Owner, Reason, Tier};
use crate::attribution_evidence::{EvidenceFile, FileReport};
use crate::backend::Backend;
use crate::change_attribution::evidence::{
    self, Conflict, Disagreement, ParentConflict, Records, Unresolved, Wanted,
};
use crate::change_attribution::{
    attribute_with, gather, Admitted, Amount, Counts, Gathered, Item, Kind, Labelled, Membership,
    Params, Piece, Placement, Repository, RepositoryAttribution, SessionAttribution, SessionKey,
    Tiers, Work,
};
use crate::git_reader::{CommitSummary, Selector};
use crate::model::{CacheWrites, DisjointTokens, TokenConvention, Tokens};
use crate::pricing::{EstimateSum, ListPriceEstimate, PriceTable, PricedShare, RequestTerms};
use crate::pull_request::{PullRequestSelection, PullRequestView};
use crate::span::Instant;
use crate::usage::DisjointSum;

pub const SCHEMA: &str = "tapes-change-usage/4";

/// What a change view's rows are grouped by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dimension {
    Commit,
    /// The first-parent mainline merge that brought the commit, or the
    /// commit itself where it is on the mainline.
    Merge,
    Agent,
    Role,
    Tier,
    Session,
}

/// The rows a view gives when none are asked for.
pub const DEFAULT_DIMENSIONS: [Dimension; 3] =
    [Dimension::Commit, Dimension::Agent, Dimension::Role];

/// What a session did for a commit, by its own evidence for the commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// It created the commit, made an edit the commit holds, or worked in
    /// the commit's worktree.
    Author,
    /// It inspected the commit and holds no authoring evidence for it.
    Reviewer,
    /// Only time places it near the commit.
    Proximate,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Author => "author",
            Self::Reviewer => "reviewer",
            Self::Proximate => "proximate",
        }
    }

    fn of(kinds: &BTreeSet<Kind>) -> Self {
        if kinds
            .iter()
            .any(|kind| matches!(kind, Kind::Created | Kind::Edited | Kind::Worktree))
        {
            Self::Author
        } else if kinds.contains(&Kind::Inspected) {
            Self::Reviewer
        } else {
            Self::Proximate
        }
    }
}

/// Which commits a view selects, or which consumption it divides.
pub enum ChangeSelection<'a> {
    /// Each revision names one commit.
    Commits(Vec<String>),
    /// `A..B` or `A...B`, as `git log` reads it.
    Range(String),
    /// A live branch's commits beyond the default branch, or the commits
    /// the merges naming a merged branch brought.
    Branch(String),
    /// Revisions read from `source`, a file or standard input.
    List {
        source: String,
        revisions: Vec<String>,
    },
    /// The commits a pull request selects, by the rule its merge method
    /// gives.
    PullRequest(Box<PullRequestSelection>),
    /// One session's partition across every commit.
    Session(crate::Selection<'a>),
    /// The consumption of `[from, to)` across every commit, `to` now where
    /// it is open.
    Window { from: Instant, to: Option<Instant> },
}

/// A change view to answer.
pub struct Request<'a> {
    pub selection: ChangeSelection<'a>,
    /// The dimensions rows are grouped by, in order.
    pub by: Vec<Dimension>,
    pub tiers: Tiers,
    /// List the evidence behind each row.
    pub explain: bool,
    pub prices: Option<&'a PriceTable>,
    /// Evidence files whose records place the sessions they name.
    pub evidence: Option<&'a [EvidenceFile]>,
    /// Only the sessions the evidence files' records name take part.
    pub evidence_only: bool,
}

/// How the view was asked, as it resolved.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Asked {
    Commits {
        revisions: Vec<String>,
    },
    Range {
        range: String,
    },
    Branch {
        branch: String,
        resolved: BranchResolution,
        /// The merges that name a merged branch.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        merges: Vec<String>,
    },
    CommitsFrom {
        source: String,
        revisions: usize,
    },
    PullRequest(PullRequestView),
    Session {
        session: SessionKey,
    },
    Window {
        from: Instant,
        to: Instant,
    },
}

/// How a branch named its commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BranchResolution {
    /// A local branch, measured against the default branch.
    Live,
    /// Merges whose subjects name the branch.
    Merged,
}

/// The repository a view reads.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RepositoryView {
    pub root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

/// One selected commit.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommitView {
    pub sha: String,
    pub subject: String,
    pub author_time: Instant,
    pub committer_time: Instant,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub merge: bool,
}

impl From<&CommitSummary> for CommitView {
    fn from(commit: &CommitSummary) -> Self {
        Self {
            sha: commit.sha.clone(),
            subject: commit.subject.clone(),
            author_time: commit.author_time,
            committer_time: commit.committer_time,
            merge: commit.merge,
        }
    }
}

/// The group a commit's merge dimension names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MergeGroup {
    /// A merge on the first-parent mainline that brought the commit.
    Merge { sha: String, subject: String },
    /// The commit is itself on the mainline, as a fast-forward integration
    /// leaves it.
    Mainline { sha: String, subject: String },
    /// No mainline commit the walk reached brought it.
    Unmerged,
}

/// One model's native counters within one harness, as its recordings name
/// them. A counter a recording does not measure is absent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelTokens {
    pub harness: String,
    pub model: String,
    pub tokens: Tokens,
    /// The cache writes by TTL, where any were written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<CacheWrites>,
}

/// What a set of shares consumed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tally {
    pub requests: u64,
    /// The sessions it holds shares of.
    pub sessions: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelTokens>,
    /// The disjoint categories, which add across harnesses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disjoint: Option<DisjointSum>,
    /// Per model, the part of a recorded total no request explains that its
    /// time holds, kept apart from every figure summed from requests.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unexplained: Vec<ModelTokens>,
    /// The requests' tokens priced at list price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_price_estimate: Option<ListPriceEstimate>,
    /// The unexplained parts priced at list price, their cache writes at no
    /// recorded TTL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unexplained_list_price_estimate: Option<ListPriceEstimate>,
}

/// A tally of the shares one tier and basis placed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvidenceTally {
    pub tier: Tier,
    pub basis: Basis,
    #[serde(flatten)]
    pub tally: Tally,
}

/// What a row is keyed by: the dimensions the view groups by.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct RowKey {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The label of the work an evidence record names that the commit's
    /// share is part of.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionKey>,
}

/// One piece of evidence behind a row, and the session it is from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Explained {
    pub session: SessionKey,
    #[serde(flatten)]
    pub evidence: Admitted,
}

/// One row of selected commits' consumption.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub key: RowKey,
    /// The commit's subject, where the row names a commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(flatten)]
    pub tally: Tally,
    /// The same amount by the tier and basis that placed each share.
    pub evidence: Vec<EvidenceTally>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub explain: Vec<Explained>,
}

/// One place's work after its sessions' last commit there.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Bucket {
    pub worktree: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(flatten)]
    pub tally: Tally,
    pub evidence: Vec<EvidenceTally>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub explain: Vec<Explained>,
}

/// What no item holds, for one reason.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Unattributed {
    pub reason: Reason,
    /// The tiers left out, for what only they would place.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_tiers: Vec<Tier>,
    #[serde(flatten)]
    pub tally: Tally,
}

/// The shares a commit the view did not select holds of the view's
/// sessions.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Unselected {
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(flatten)]
    pub tally: Tally,
}

/// Work evidence records name of which the repository holds no commit, and
/// what the view's sessions gave it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LabelledWork {
    #[serde(flatten)]
    pub work: Labelled,
    #[serde(flatten)]
    pub tally: Tally,
    pub evidence: Vec<EvidenceTally>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub explain: Vec<Explained>,
}

/// The view's figures added up. `rows`, `uncommitted`, `unattributed`,
/// `unselected` and `labelled` add up to `sessions`, the whole of every
/// session in the view; the change's own figure is `rows`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Totals {
    pub rows: Tally,
    pub uncommitted: Tally,
    pub unattributed: Tally,
    pub unselected: Tally,
    pub labelled: Tally,
    pub sessions: Tally,
    /// The rows by the tier and basis that placed them.
    pub evidence: Vec<EvidenceTally>,
}

/// One session in the view, divided.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionView {
    pub harness: String,
    pub id: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionKey>,
    pub rules: Vec<crate::change_attribution::MemberRule>,
    /// What placed its consumption: its recording, the evidence records
    /// that name it, or its parent's allocation alone.
    pub placement: Placement,
    /// The evidence files whose records place it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_files: Vec<String>,
    pub whole: Tally,
    pub rows: Tally,
    pub uncommitted: Tally,
    pub unattributed: Tally,
    pub unselected: Tally,
    pub labelled: Tally,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// One evidence file and what its records did in the view.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvidenceFileView {
    #[serde(flatten)]
    pub report: FileReport,
    /// Its bindings and events that name a session in the view.
    pub placed: PlacedRecords,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PlacedRecords {
    pub binding: usize,
    pub event: usize,
}

/// The evidence files a view read and what they did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EvidenceView {
    pub files: Vec<EvidenceFileView>,
    pub producers: Vec<String>,
    /// Only the sessions the records name took part.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub only: bool,
    /// Sessions in the view placed by records, and placed by inheriting
    /// from one.
    pub placed: usize,
    pub inherited: usize,
    /// Sessions looked up by id in a store.
    pub looked_up: usize,
    /// Members a lineage record made a child of another member.
    pub lineage_applied: usize,
    pub unresolved: Vec<Unresolved>,
    pub conflicts: Vec<Conflict>,
    pub disagreements: Vec<Disagreement>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parent_conflicts: Vec<ParentConflict>,
}

/// What the gathering listed and read.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MembershipView {
    pub listed: usize,
    pub read: usize,
    pub members: usize,
    /// Sessions whose recordings could not be read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<SessionKey>,
    /// Gone directories the members' commits and remotes placed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub placed: Vec<PathBuf>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub listing_bounded: bool,
}

/// A change view's answer.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChangeUsage {
    pub schema: &'static str,
    pub repository: RepositoryView,
    pub selection: Asked,
    /// The evidence tiers that took part.
    pub tiers: Vec<Tier>,
    pub by: Vec<Dimension>,
    /// The selected commits, oldest first; every commit a share names in a
    /// session or window view.
    pub commits: Vec<CommitView>,
    /// The interval whose sessions were read.
    pub interval: Piece,
    pub rows: Vec<Row>,
    pub uncommitted: Vec<Bucket>,
    pub unattributed: Vec<Unattributed>,
    pub unselected: Vec<Unselected>,
    pub labelled: Vec<LabelledWork>,
    pub totals: Totals,
    pub sessions: Vec<SessionView>,
    pub membership: MembershipView,
    /// The evidence files read, where any were named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<EvidenceView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Answer a change view over `repository` from the stores `backends` read.
pub fn change_usage(
    backends: &[Box<dyn Backend>],
    repository: &Repository,
    request: Request<'_>,
) -> Result<ChangeUsage> {
    let params = Params {
        tiers: request.tiers,
        ..Params::default()
    };
    let reader = repository.reader();
    // Only a pull request's squashed or rebased commits admit evidence
    // beyond their own windows.
    repository.set_lifetimes(match &request.selection {
        ChangeSelection::PullRequest(pr) => pr.lifetime.map_or_else(BTreeMap::new, |lifetime| {
            pr.view
                .widened
                .iter()
                .map(|sha| (sha.clone(), lifetime))
                .collect()
        }),
        _ => BTreeMap::new(),
    });
    let mut notes = Vec::new();
    // The selected commits, and the interval whose sessions hold their
    // evidence.
    let (asked, selected, only, within) = match &request.selection {
        ChangeSelection::Commits(revisions) => {
            let summaries = reader
                .summaries(&Selector::Commits(revisions.clone()))
                .context("a --commit revision names no commit")?;
            note_truncation(
                &mut notes,
                summaries.commits_truncated,
                summaries.metadata_truncated,
            );
            (
                Asked::Commits {
                    revisions: revisions.clone(),
                },
                Some(summaries.commits),
                None,
                None,
            )
        }
        ChangeSelection::Range(range) => {
            let selector = Selector::parse(range);
            if matches!(selector, Selector::Commits(_)) {
                bail!("--range takes A..B or A...B; `{range}` names one revision, which --commit selects");
            }
            let summaries = reader
                .summaries(&selector)
                .with_context(|| format!("the range {range} could not be read"))?;
            note_truncation(
                &mut notes,
                summaries.commits_truncated,
                summaries.metadata_truncated,
            );
            if summaries.commits.is_empty() {
                bail!("the range {range} names no commits");
            }
            (
                Asked::Range {
                    range: range.clone(),
                },
                Some(summaries.commits),
                None,
                None,
            )
        }
        ChangeSelection::Branch(branch) => {
            let (resolved, merges, commits) = branch_commits(repository, branch, &mut notes)?;
            (
                Asked::Branch {
                    branch: branch.clone(),
                    resolved,
                    merges,
                },
                Some(commits),
                None,
                None,
            )
        }
        ChangeSelection::List { source, revisions } => {
            if revisions.is_empty() {
                bail!("{source} names no revisions: give one revision per line");
            }
            let summaries = reader
                .summaries(&Selector::Commits(revisions.clone()))
                .with_context(|| format!("a revision {source} lists names no commit"))?;
            note_truncation(
                &mut notes,
                summaries.commits_truncated,
                summaries.metadata_truncated,
            );
            (
                Asked::CommitsFrom {
                    source: source.clone(),
                    revisions: revisions.len(),
                },
                Some(summaries.commits),
                None,
                None,
            )
        }
        ChangeSelection::PullRequest(pr) => (
            Asked::PullRequest(pr.view.clone()),
            Some(pr.commits.clone()),
            None,
            None,
        ),
        ChangeSelection::Session(selection) => {
            let resolved = selection.resolve(backends)?;
            let key = SessionKey {
                harness: resolved.session.harness().to_owned(),
                id: resolved.session.id.clone(),
            };
            let (Some(from), Some(to)) = (
                resolved.session.started_at,
                resolved
                    .session
                    .last_activity_at
                    .or(resolved.session.started_at),
            ) else {
                bail!(
                    "{} session {} records no activity times, so its consumption cannot be \
                     placed among commits",
                    key.harness,
                    key.id
                );
            };
            (
                Asked::Session {
                    session: key.clone(),
                },
                None,
                Some((key, from, to)),
                None,
            )
        }
        ChangeSelection::Window { from, to } => {
            let to = to.unwrap_or_else(chrono::Utc::now);
            if to <= *from {
                bail!("the window ends before it starts");
            }
            (
                Asked::Window { from: *from, to },
                None,
                None,
                Some(Piece {
                    from: Some(*from),
                    to: Some(to),
                }),
            )
        }
    };
    if let Some(selected) = &selected {
        if selected.is_empty() {
            bail!("the selection names no commits");
        }
    }
    let (from, to) = match (&selected, &only, within) {
        (Some(selected), _, _) => {
            let from = selected
                .iter()
                .map(|commit| commit.author_time.min(commit.committer_time))
                .min()
                .expect("the selection holds a commit")
                - params.lookback;
            let to = selected
                .iter()
                .map(|commit| commit.author_time.max(commit.committer_time))
                .max()
                .expect("the selection holds a commit")
                + params.lookahead;
            (from, to)
        }
        (None, Some((_, from, to)), _) => (*from, *to),
        (
            None,
            None,
            Some(Piece {
                from: Some(from),
                to: Some(to),
            }),
        ) => (from, to),
        _ => unreachable!("every selection names commits, a session or a window"),
    };

    let (from, to) = match &request.selection {
        ChangeSelection::PullRequest(pr) => match pr.lifetime {
            Some((begun, ended)) => (
                from.min(begun - params.lookback),
                to.max(ended + params.lookahead),
            ),
            None => (from, to),
        },
        _ => (from, to),
    };

    let records = request
        .evidence
        .map(|files| Records::new(files, repository))
        .transpose()?;
    let mut gathered = if request.evidence_only {
        Gathered {
            members: Vec::new(),
            membership: Membership::default(),
        }
    } else {
        gather(backends, repository, from, to)?
    };
    let selected_shas: BTreeSet<String> = selected
        .iter()
        .flatten()
        .map(|commit| commit.sha.clone())
        .collect();
    let evidence_membership = records.as_ref().map(|records| {
        let wanted = match (&selected, &only) {
            (Some(_), _) => Wanted::Commits(&selected_shas),
            (None, Some((key, _, _))) => Wanted::Session(key.clone()),
            (None, None) => Wanted::Window { from, to },
        };
        evidence::extend(
            backends,
            &mut gathered,
            records,
            &wanted,
            request.evidence_only,
        )
    });
    let attribution = attribute_with(
        backends,
        repository,
        &gathered,
        &params,
        within,
        records.as_ref(),
    )?;
    if gathered.membership.listing_bounded {
        notes.push(
            "a harness's listing reached its bound, so sessions active in the interval may be \
             missing"
                .to_owned(),
        );
    }
    if attribution.commits_truncated {
        notes.push(
            "more commits met the sessions' interval than a read keeps; the oldest were left out"
                .to_owned(),
        );
    }

    let chosen: Option<BTreeSet<String>> = selected
        .as_ref()
        .map(|commits| commits.iter().map(|commit| commit.sha.clone()).collect());
    // The sessions the view holds.
    let in_view: Vec<&SessionAttribution> = match (&chosen, &only) {
        (Some(chosen), _) => attribution
            .sessions
            .iter()
            .filter(|session| {
                // A session only a left-out tier places on a selected commit
                // is in the view, its part unattributed for those tiers.
                session.shares().iter().any(|share| {
                    share.work.sha().is_some_and(|sha| chosen.contains(sha))
                        && !share.amount.is_zero()
                }) || session
                    .placed_by_every_tier
                    .as_ref()
                    .is_some_and(|placed| placed.iter().any(|sha| chosen.contains(sha)))
            })
            .collect(),
        (None, Some((key, _, _))) => {
            let found: Vec<&SessionAttribution> = attribution
                .sessions
                .iter()
                .filter(|session| session.session == *key)
                .collect();
            if found.is_empty() {
                bail!(
                    "{} session {} holds no evidence for the repository at {}: none of its \
                     locations, commits, remote or named paths lie in it",
                    key.harness,
                    key.id,
                    repository.root.display()
                );
            }
            found
        }
        _ => attribution
            .sessions
            .iter()
            .filter(|session| {
                session
                    .amounts
                    .as_ref()
                    .is_some_and(|amounts| !amounts.whole.is_zero())
            })
            .collect(),
    };

    // Every commit a share of the view names, with its facts.
    let named: BTreeSet<String> = in_view
        .iter()
        .flat_map(|session| session.shares())
        .filter_map(|share| share.work.sha().map(str::to_owned))
        .collect();
    let mut described: BTreeMap<String, CommitSummary> = selected
        .iter()
        .flatten()
        .map(|commit| (commit.sha.clone(), commit.clone()))
        .collect();
    let missing: Vec<String> = named
        .iter()
        .filter(|sha| !described.contains_key(*sha))
        .cloned()
        .collect();
    if !missing.is_empty() {
        for commit in reader.describe(&missing)? {
            described.insert(commit.sha.clone(), commit);
        }
    }
    let row_commits: Vec<CommitSummary> = match &selected {
        Some(selected) => selected.clone(),
        None => {
            let mut commits: Vec<CommitSummary> = named
                .iter()
                .filter_map(|sha| described.get(sha).cloned())
                .collect();
            commits.sort_by(|a, b| (a.committer_time, &a.sha).cmp(&(b.committer_time, &b.sha)));
            commits
        }
    };
    let chosen: BTreeSet<String> = row_commits
        .iter()
        .map(|commit| commit.sha.clone())
        .collect();

    let by = if request.by.is_empty() {
        DEFAULT_DIMENSIONS.to_vec()
    } else {
        request.by.clone()
    };
    let groups = if by.contains(&Dimension::Merge) {
        merge_groups(repository, &row_commits, &mut described)?
    } else {
        BTreeMap::new()
    };

    let roles = Roles::of(&attribution);
    let labels = by.contains(&Dimension::Commit);
    let mut builder = Builder {
        by: &by,
        prices: request.prices,
        explain: request.explain,
        chosen: &chosen,
        groups: &groups,
        roles: &roles,
        labels,
        records: records.as_ref(),
        rows: BTreeMap::new(),
        buckets: BTreeMap::new(),
        unattributed: BTreeMap::new(),
        unselected: BTreeMap::new(),
        labelled: BTreeMap::new(),
        totals: TotalsAcc::default(),
        sessions: Vec::new(),
    };
    for session in &in_view {
        builder.add(session);
    }
    let position: BTreeMap<&str, usize> = row_commits
        .iter()
        .enumerate()
        .map(|(index, commit)| (commit.sha.as_str(), index))
        .collect();
    let mut rows: Vec<Row> = builder
        .rows
        .into_iter()
        .map(|(key, acc)| {
            let subject = key
                .commit
                .as_ref()
                .and_then(|sha| described.get(sha))
                .map(|commit| commit.subject.clone());
            Row {
                subject,
                tally: acc.tally.finish(),
                evidence: finish_evidence(acc.evidence),
                explain: acc.explain,
                key,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let at = |row: &Row| {
            row.key
                .commit
                .as_deref()
                .and_then(|sha| position.get(sha).copied())
                .unwrap_or(usize::MAX)
        };
        let merge_at = |row: &Row| match &row.key.merge {
            Some(MergeGroup::Merge { sha, .. } | MergeGroup::Mainline { sha, .. }) => {
                described.get(sha).map(|commit| commit.committer_time)
            }
            _ => None,
        };
        (at(a), merge_at(a).is_none(), merge_at(a), &a.key).cmp(&(
            at(b),
            merge_at(b).is_none(),
            merge_at(b),
            &b.key,
        ))
    });
    let uncommitted = builder
        .buckets
        .into_iter()
        .map(|((worktree, branch), acc)| Bucket {
            worktree,
            branch,
            tally: acc.tally.finish(),
            evidence: finish_evidence(acc.evidence),
            explain: acc.explain,
        })
        .collect();
    let excluded = request.tiers.excluded();
    let unattributed = builder
        .unattributed
        .into_iter()
        .map(|(reason, acc)| Unattributed {
            reason,
            excluded_tiers: if reason == Reason::TiersNotAsked {
                excluded.clone()
            } else {
                Vec::new()
            },
            tally: acc.finish(),
        })
        .collect();
    let mut unselected: Vec<Unselected> = builder
        .unselected
        .into_iter()
        .map(|(sha, acc)| Unselected {
            subject: described.get(&sha).map(|commit| commit.subject.clone()),
            commit: sha,
            tally: acc.finish(),
        })
        .collect();
    unselected.sort_by(|a, b| {
        let time = |sha: &str| described.get(sha).map(|commit| commit.committer_time);
        (time(&a.commit), &a.commit).cmp(&(time(&b.commit), &b.commit))
    });
    let labelled = builder
        .labelled
        .into_iter()
        .map(|(work, acc)| LabelledWork {
            work,
            tally: acc.tally.finish(),
            evidence: finish_evidence(acc.evidence),
            explain: acc.explain,
        })
        .collect();
    let evidence = records.as_ref().map(|records| {
        let membership = evidence_membership.unwrap_or_default();
        let placed: BTreeSet<SessionKey> = in_view
            .iter()
            .filter(|session| session.placement == Placement::Evidence)
            .map(|session| session.session.clone())
            .collect();
        let applied = records.applied(&placed);
        let mut producers: Vec<String> = records
            .files()
            .iter()
            .map(|(_, producer)| producer.clone())
            .collect();
        producers.sort();
        producers.dedup();
        EvidenceView {
            files: records
                .reports()
                .iter()
                .zip(applied)
                .map(|(report, (binding, event))| EvidenceFileView {
                    report: report.clone(),
                    placed: PlacedRecords { binding, event },
                })
                .collect(),
            producers,
            only: request.evidence_only,
            placed: placed.len(),
            inherited: in_view
                .iter()
                .filter(|session| session.placement == Placement::Inherited)
                .count(),
            looked_up: membership.looked_up,
            lineage_applied: membership.lineage_applied,
            unresolved: membership.unresolved,
            conflicts: attribution.conflicts.clone(),
            disagreements: records.disagreements().to_vec(),
            parent_conflicts: membership.parent_conflicts,
        }
    });
    if let Some(evidence) = &evidence {
        if !evidence.unresolved.is_empty() {
            notes.push(format!(
                "{} sessions evidence records name are held by no installed store and add nothing",
                evidence.unresolved.len()
            ));
        }
        if !evidence.conflicts.is_empty() {
            notes.push(format!(
                "{} commits a session's recording shows it creating are bound to it by none of the \
                 records that place it",
                evidence.conflicts.len()
            ));
        }
        if !evidence.disagreements.is_empty() {
            notes.push(format!(
                "records name different commits for {} labels; each label's commits are every \
                 commit its records name",
                evidence.disagreements.len()
            ));
        }
    }

    Ok(ChangeUsage {
        schema: SCHEMA,
        repository: RepositoryView {
            root: repository.root.clone(),
            default_branch: repository.default_branch.clone(),
        },
        selection: asked,
        tiers: request.tiers.named(),
        by: by.clone(),
        commits: row_commits.iter().map(CommitView::from).collect(),
        interval: Piece {
            from: Some(from),
            to: Some(to),
        },
        rows,
        uncommitted,
        unattributed,
        unselected,
        labelled,
        totals: builder.totals.finish(),
        sessions: builder.sessions,
        membership: MembershipView {
            listed: gathered.membership.listed,
            read: gathered.membership.read,
            members: gathered.members.len(),
            unreadable: gathered
                .membership
                .unreadable
                .iter()
                .map(|(key, _)| key.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            placed: gathered.membership.placed.clone(),
            listing_bounded: gathered.membership.listing_bounded,
        },
        evidence,
        notes,
    })
}

fn note_truncation(notes: &mut Vec<String>, commits: bool, metadata: bool) {
    if commits {
        notes.push(
            "the selection held more commits than a read keeps; the oldest were left out"
                .to_owned(),
        );
    }
    if metadata {
        notes.push("the selection's commit metadata reached the read's byte bound; later commits are absent".to_owned());
    }
}

/// A branch's commits: a live branch's beyond the default branch, else what
/// the merges naming it brought.
fn branch_commits(
    repository: &Repository,
    branch: &str,
    notes: &mut Vec<String>,
) -> Result<(BranchResolution, Vec<String>, Vec<CommitSummary>)> {
    let reader = repository.reader();
    if reader.has_branch(branch)? {
        let summaries = reader.summaries(&Selector::Branch(branch.to_owned()))?;
        note_truncation(
            notes,
            summaries.commits_truncated,
            summaries.metadata_truncated,
        );
        if !summaries.commits.is_empty() {
            return Ok((BranchResolution::Live, Vec::new(), summaries.commits));
        }
    }
    let merges = reader.merges_naming(branch)?;
    let commits: Vec<String> = merges
        .iter()
        .flat_map(|merged| merged.commits.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if commits.is_empty() {
        let live = if reader.has_branch(branch)? {
            "is a local branch with no commits the default branch lacks"
        } else {
            "is no local branch"
        };
        bail!(
            "--branch {branch} {live}, and no merge subject names it; select its commits with \
             --range A..B or --commits-from FILE"
        );
    }
    let summaries = reader.summaries(&Selector::Commits(commits))?;
    note_truncation(
        notes,
        summaries.commits_truncated,
        summaries.metadata_truncated,
    );
    Ok((
        BranchResolution::Merged,
        merges.into_iter().map(|merged| merged.merge).collect(),
        summaries.commits,
    ))
}

/// The mainline group of each commit: the first-parent merge of the
/// default branch that brought it, or itself where it lies on that history.
fn merge_groups(
    repository: &Repository,
    commits: &[CommitSummary],
    described: &mut BTreeMap<String, CommitSummary>,
) -> Result<BTreeMap<String, MergeGroup>> {
    let reader = repository.reader();
    let Some(default) = repository.default_branch.clone() else {
        return Ok(commits
            .iter()
            .map(|commit| (commit.sha.clone(), MergeGroup::Unmerged))
            .collect());
    };
    let mainline = if reader.has_branch(&default)? {
        format!("refs/heads/{default}")
    } else {
        reader
            .default_branch()?
            .map(|branch| branch.name)
            .ok_or_else(|| anyhow!("the default branch {default} cannot be read"))?
    };
    let shas: BTreeSet<String> = commits.iter().map(|commit| commit.sha.clone()).collect();
    let since = commits
        .iter()
        .map(|commit| commit.committer_time.min(commit.author_time))
        .min()
        .map_or_else(chrono::Utc::now, |at| at - chrono::Duration::days(1));
    let arrived = reader.mainline_arrivals(&mainline, &shas, since)?;
    let merges: Vec<String> = arrived
        .values()
        .filter(|sha| !described.contains_key(*sha))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !merges.is_empty() {
        for commit in reader.describe(&merges)? {
            described.insert(commit.sha.clone(), commit);
        }
    }
    Ok(commits
        .iter()
        .map(|commit| {
            let group = match arrived.get(&commit.sha) {
                Some(by) if *by == commit.sha && commit.merge => MergeGroup::Merge {
                    sha: by.clone(),
                    subject: commit.subject.clone(),
                },
                Some(by) if *by == commit.sha => MergeGroup::Mainline {
                    sha: by.clone(),
                    subject: commit.subject.clone(),
                },
                Some(by) => MergeGroup::Merge {
                    sha: by.clone(),
                    subject: described
                        .get(by)
                        .map(|merge| merge.subject.clone())
                        .unwrap_or_default(),
                },
                None => MergeGroup::Unmerged,
            };
            (commit.sha.clone(), group)
        })
        .collect())
}

/// What a session's evidence for one commit says about its role: the kinds
/// of its recording's evidence, and the roles of the records naming it.
#[derive(Default)]
struct Acts {
    kinds: BTreeSet<Kind>,
    /// Whether records name the commit, and whether each says `reviewer`.
    records: Vec<bool>,
}

impl Acts {
    fn role(&self) -> Role {
        if !self.records.is_empty() {
            return if self.records.iter().all(|reviewer| *reviewer) {
                Role::Reviewer
            } else {
                Role::Author
            };
        }
        Role::of(&self.kinds)
    }
}

/// Each session's role for each commit its own evidence names.
struct Roles {
    own: BTreeMap<SessionKey, BTreeMap<String, Acts>>,
    parents: BTreeMap<SessionKey, SessionKey>,
}

impl Roles {
    fn of(attribution: &RepositoryAttribution) -> Self {
        let mut own: BTreeMap<SessionKey, BTreeMap<String, Acts>> = BTreeMap::new();
        let mut parents = BTreeMap::new();
        for session in &attribution.sessions {
            let acts = own.entry(session.session.clone()).or_default();
            for admitted in &session.partition.evidence {
                for work in &admitted.work {
                    if let Some(sha) = work.sha() {
                        let act = acts.entry(sha.to_owned()).or_default();
                        match &admitted.record {
                            Some(record) => {
                                act.records.push(record.role.as_deref() == Some("reviewer"))
                            }
                            None => {
                                act.kinds.insert(admitted.kind);
                            }
                        }
                    }
                }
            }
            if let Some(parent) = &session.parent {
                parents.insert(session.session.clone(), parent.clone());
            }
        }
        Self { own, parents }
    }

    /// The role of `session` for `sha`, for a share placed by `basis`: an
    /// inherited share takes the role its parent holds, up the chain.
    fn role(&self, session: &SessionKey, sha: &str, basis: Basis) -> Role {
        let mut at = session;
        let mut inherited = basis == Basis::Inherited;
        for _ in 0..=self.parents.len() {
            let acts = self.own.get(at).and_then(|acts| acts.get(sha));
            if !inherited {
                return acts.map_or(Role::Proximate, Acts::role);
            }
            match self.parents.get(at) {
                Some(parent) => at = parent,
                None => return acts.map_or(Role::Proximate, Acts::role),
            }
            inherited = self.own.get(at).and_then(|acts| acts.get(sha)).is_none();
        }
        Role::Proximate
    }
}

/// One share's facts, as a tally adds them.
struct Contribution<'a> {
    session: &'a SessionAttribution,
    convention: Option<TokenConvention>,
    amount: &'a Amount,
}

/// A tally being added up.
#[derive(Default)]
struct TallyAcc {
    requests: u64,
    sessions: BTreeSet<SessionKey>,
    models: BTreeMap<(String, String), (Counts, [bool; 5])>,
    unexplained: BTreeMap<(String, String), Counts>,
    conventions: BTreeMap<String, TokenConvention>,
    estimate: Option<EstimateSum>,
    unexplained_estimate: Option<EstimateSum>,
}

/// Which native counters a recording measures, in `Tokens` order.
fn measured(session: &SessionAttribution) -> [bool; 5] {
    match &session.measured {
        Some(tokens) => [
            tokens.input.is_some(),
            tokens.output.is_some(),
            tokens.reasoning.is_some(),
            tokens.cache_read.is_some(),
            tokens.cache_write.is_some(),
        ],
        None => [true; 5],
    }
}

fn masked(counts: &Counts, mask: [bool; 5]) -> Tokens {
    Tokens {
        input: mask[0].then_some(counts.input),
        output: mask[1].then_some(counts.output),
        reasoning: mask[2].then_some(counts.reasoning),
        cache_read: mask[3].then_some(counts.cache_read),
        cache_write: mask[4].then_some(counts.cache_write),
    }
}

impl TallyAcc {
    fn add(&mut self, contribution: &Contribution<'_>, prices: Option<&PriceTable>) {
        let Contribution {
            session,
            convention,
            amount,
        } = contribution;
        if amount.is_zero() {
            return;
        }
        let harness = &session.session.harness;
        self.requests += amount.requests;
        self.sessions.insert(session.session.clone());
        if let Some(convention) = convention {
            self.conventions.insert(harness.clone(), *convention);
        }
        let mask = measured(session);
        let terms = RequestTerms {
            requests: None,
            billing: session.billing.as_ref(),
            from: session.started_at,
            to: session.last_activity_at,
        };
        for (model, counts) in &amount.models {
            let entry = self
                .models
                .entry((harness.clone(), model.clone()))
                .or_insert((Counts::default(), [true; 5]));
            entry.0.add(counts);
            for (held, own) in entry.1.iter_mut().zip(mask) {
                *held &= own;
            }
            if let (Some(table), Some(convention)) = (prices, convention) {
                let writes = counts.cache_writes();
                let disjoint =
                    DisjointTokens::from_native(&masked(counts, mask), *convention, Some(&writes));
                let sum = PricedShare {
                    disjoint: &disjoint,
                    model,
                    variant: None,
                    sole: false,
                    terms,
                }
                .estimate(table);
                self.estimate
                    .get_or_insert_with(EstimateSum::default)
                    .merge(sum);
            }
        }
        for (model, counts) in &amount.unexplained {
            self.unexplained
                .entry((harness.clone(), model.clone()))
                .or_default()
                .add(counts);
            if let (Some(table), Some(convention)) = (prices, convention) {
                let writes = CacheWrites {
                    ttl_unrecorded: Some(counts.cache_write),
                    ..CacheWrites::default()
                };
                let disjoint =
                    DisjointTokens::from_native(&counts.tokens(), *convention, Some(&writes));
                let sum = PricedShare {
                    disjoint: &disjoint,
                    model,
                    variant: None,
                    sole: false,
                    terms,
                }
                .estimate(table);
                self.unexplained_estimate
                    .get_or_insert_with(EstimateSum::default)
                    .merge(sum);
            }
        }
    }

    fn merge(&mut self, other: &TallyAcc) {
        self.requests += other.requests;
        self.sessions.extend(other.sessions.iter().cloned());
        for (key, (counts, mask)) in &other.models {
            let entry = self
                .models
                .entry(key.clone())
                .or_insert((Counts::default(), [true; 5]));
            entry.0.add(counts);
            for (held, own) in entry.1.iter_mut().zip(mask) {
                *held &= *own;
            }
        }
        for (key, counts) in &other.unexplained {
            self.unexplained.entry(key.clone()).or_default().add(counts);
        }
        self.conventions.extend(
            other
                .conventions
                .iter()
                .map(|(key, value)| (key.clone(), *value)),
        );
        for (mine, theirs) in [
            (&mut self.estimate, &other.estimate),
            (&mut self.unexplained_estimate, &other.unexplained_estimate),
        ] {
            if let Some(theirs) = theirs {
                mine.get_or_insert_with(EstimateSum::default)
                    .merge(theirs.clone());
            }
        }
    }

    fn finish(&self) -> Tally {
        let mut disjoint = DisjointSum {
            sessions: self.sessions.len(),
            ..DisjointSum::default()
        };
        let mut models = Vec::new();
        for ((harness, model), (counts, mask)) in &self.models {
            let tokens = masked(counts, *mask);
            let writes = counts.cache_writes();
            if let Some(convention) = self.conventions.get(harness) {
                disjoint.add_disjoint(
                    harness,
                    &DisjointTokens::from_native(&tokens, *convention, Some(&writes)),
                );
            }
            models.push(ModelTokens {
                harness: harness.clone(),
                model: model.clone(),
                tokens,
                cache_write: (counts.cache_write > 0).then_some(writes),
            });
        }
        let unexplained = self
            .unexplained
            .iter()
            .filter(|(_, counts)| !counts.is_zero())
            .map(|((harness, model), counts)| ModelTokens {
                harness: harness.clone(),
                model: model.clone(),
                tokens: counts.tokens(),
                cache_write: None,
            })
            .collect();
        Tally {
            requests: self.requests,
            sessions: self.sessions.len(),
            models,
            disjoint: (!self.models.is_empty())
                .then(|| disjoint.finish())
                .flatten(),
            unexplained,
            list_price_estimate: self.estimate.clone().and_then(EstimateSum::finish),
            unexplained_list_price_estimate: self
                .unexplained_estimate
                .clone()
                .and_then(EstimateSum::finish),
        }
    }
}

/// A row or bucket being added up: its tally, by evidence, and what
/// evidence names it.
#[derive(Default)]
struct RowAcc {
    tally: TallyAcc,
    evidence: BTreeMap<(Tier, Basis), TallyAcc>,
    explain: Vec<Explained>,
    explained: BTreeSet<SessionKey>,
}

impl RowAcc {
    fn add(
        &mut self,
        contribution: &Contribution<'_>,
        tier: Tier,
        basis: Basis,
        prices: Option<&PriceTable>,
    ) {
        self.tally.add(contribution, prices);
        self.evidence
            .entry((tier, basis))
            .or_default()
            .add(contribution, prices);
    }

    /// List the evidence `session` admitted for `item`, once per session.
    fn explain(&mut self, session: &SessionAttribution, item: &Item) {
        if !self.explained.insert(session.session.clone()) {
            return;
        }
        for admitted in &session.partition.evidence {
            let names = match item {
                Item::Work { work, label } => {
                    admitted.work.contains(work)
                        && admitted
                            .record
                            .as_ref()
                            .is_none_or(|record| record.label == *label)
                }
                Item::Labelled(labelled) => {
                    admitted.work.is_empty()
                        && admitted.record.as_ref().is_some_and(|record| {
                            record.label == labelled.label && record.unheld == labelled.commits
                        })
                }
            };
            if names {
                self.explain.push(Explained {
                    session: session.session.clone(),
                    evidence: admitted.clone(),
                });
            }
        }
    }
}

fn finish_evidence(evidence: BTreeMap<(Tier, Basis), TallyAcc>) -> Vec<EvidenceTally> {
    evidence
        .into_iter()
        .map(|((tier, basis), acc)| EvidenceTally {
            tier,
            basis,
            tally: acc.finish(),
        })
        .collect()
}

#[derive(Default)]
struct TotalsAcc {
    rows: TallyAcc,
    uncommitted: TallyAcc,
    unattributed: TallyAcc,
    unselected: TallyAcc,
    labelled: TallyAcc,
    sessions: TallyAcc,
    evidence: BTreeMap<(Tier, Basis), TallyAcc>,
}

impl TotalsAcc {
    fn finish(self) -> Totals {
        Totals {
            rows: self.rows.finish(),
            uncommitted: self.uncommitted.finish(),
            unattributed: self.unattributed.finish(),
            unselected: self.unselected.finish(),
            labelled: self.labelled.finish(),
            sessions: self.sessions.finish(),
            evidence: finish_evidence(self.evidence),
        }
    }
}

struct Builder<'a> {
    by: &'a [Dimension],
    prices: Option<&'a PriceTable>,
    explain: bool,
    chosen: &'a BTreeSet<String>,
    groups: &'a BTreeMap<String, MergeGroup>,
    roles: &'a Roles,
    /// Rows keep the label of the work a commit's share is part of.
    labels: bool,
    records: Option<&'a Records>,
    rows: BTreeMap<RowKey, RowAcc>,
    buckets: BTreeMap<(PathBuf, Option<String>), RowAcc>,
    unattributed: BTreeMap<Reason, TallyAcc>,
    unselected: BTreeMap<String, TallyAcc>,
    labelled: BTreeMap<Labelled, RowAcc>,
    totals: TotalsAcc,
    sessions: Vec<SessionView>,
}

impl Builder<'_> {
    fn add(&mut self, session: &SessionAttribution) {
        let convention = TokenConvention::of(&session.session.harness);
        let mut own = SessionAccs::default();
        let notes = session
            .notes
            .iter()
            .map(|note| match note.split_once(": ") {
                Some((head, _)) if head == "the recording could not be read in pieces" => {
                    head.to_owned()
                }
                _ => note.clone(),
            })
            .collect();
        if let (Some(entries), Some(amounts)) = (session.entry_amounts(), &session.amounts) {
            let whole = Contribution {
                session,
                convention,
                amount: &amounts.whole,
            };
            own.whole.add(&whole, self.prices);
            for (entry, amount) in session.partition.entries.iter().zip(&entries) {
                if amount.is_zero() {
                    continue;
                }
                let contribution = Contribution {
                    session,
                    convention,
                    amount,
                };
                match &entry.owner {
                    Owner::Item {
                        work: item @ Item::Labelled(labelled),
                        evidence,
                    } => {
                        let work = self.labelled.entry(labelled.clone()).or_default();
                        work.add(&contribution, evidence.tier, evidence.basis, self.prices);
                        if self.explain {
                            work.explain(session, item);
                        }
                        own.labelled.add(&contribution, self.prices);
                    }
                    Owner::Item {
                        work: item @ Item::Work { work, label },
                        evidence,
                    } => match work {
                        Work::Commit { sha } if self.chosen.contains(sha) => {
                            let mut key = self.key(session, sha, evidence.tier, evidence.basis);
                            if self.labels {
                                key.label.clone_from(label);
                            }
                            let row = self.rows.entry(key).or_default();
                            row.add(&contribution, evidence.tier, evidence.basis, self.prices);
                            if self.explain {
                                row.explain(session, item);
                            }
                            self.totals
                                .evidence
                                .entry((evidence.tier, evidence.basis))
                                .or_default()
                                .add(&contribution, self.prices);
                            own.rows.add(&contribution, self.prices);
                        }
                        Work::Commit { sha } => {
                            self.unselected
                                .entry(sha.clone())
                                .or_default()
                                .add(&contribution, self.prices);
                            own.unselected.add(&contribution, self.prices);
                        }
                        Work::Uncommitted { worktree, branch } => {
                            let bucket = self
                                .buckets
                                .entry((worktree.clone(), branch.clone()))
                                .or_default();
                            bucket.add(&contribution, evidence.tier, evidence.basis, self.prices);
                            if self.explain {
                                bucket.explain(session, item);
                            }
                            own.uncommitted.add(&contribution, self.prices);
                        }
                    },
                    Owner::Unattributed(reason) => {
                        self.unattributed
                            .entry(*reason)
                            .or_default()
                            .add(&contribution, self.prices);
                        own.unattributed.add(&contribution, self.prices);
                    }
                }
            }
        }
        self.totals.rows.merge(&own.rows);
        self.totals.uncommitted.merge(&own.uncommitted);
        self.totals.unattributed.merge(&own.unattributed);
        self.totals.unselected.merge(&own.unselected);
        self.totals.labelled.merge(&own.labelled);
        self.totals.sessions.merge(&own.whole);
        self.sessions.push(SessionView {
            harness: session.session.harness.clone(),
            id: session.session.id.clone(),
            agent: session.agent.clone(),
            parent: session.parent.clone(),
            rules: session.rules.iter().copied().collect(),
            placement: session.placement,
            evidence_files: match (session.placement, self.records) {
                (Placement::Evidence, Some(records)) => records.files_of(&session.session),
                _ => Vec::new(),
            },
            whole: own.whole.finish(),
            rows: own.rows.finish(),
            uncommitted: own.uncommitted.finish(),
            unattributed: own.unattributed.finish(),
            unselected: own.unselected.finish(),
            labelled: own.labelled.finish(),
            notes,
        });
    }

    fn key(&self, session: &SessionAttribution, sha: &str, tier: Tier, basis: Basis) -> RowKey {
        let mut key = RowKey::default();
        for dimension in self.by {
            match dimension {
                Dimension::Commit => key.commit = Some(sha.to_owned()),
                Dimension::Merge => {
                    key.merge = Some(
                        self.groups
                            .get(sha)
                            .cloned()
                            .unwrap_or(MergeGroup::Unmerged),
                    );
                }
                Dimension::Agent => key.agent = Some(session.agent.clone()),
                Dimension::Role => {
                    key.role = Some(self.roles.role(&session.session, sha, basis));
                }
                Dimension::Tier => key.tier = Some(tier),
                Dimension::Session => key.session = Some(session.session.clone()),
            }
        }
        key
    }
}

#[derive(Default)]
struct SessionAccs {
    whole: TallyAcc,
    rows: TallyAcc,
    uncommitted: TallyAcc,
    unattributed: TallyAcc,
    unselected: TallyAcc,
    labelled: TallyAcc,
}
