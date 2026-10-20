//! What a repository says about a set of commits: which commits a selector
//! names, when each was made, what each changed, and enough content identity
//! to recognise the same change under another sha.
//!
//! Everything here runs `git` read-only and bounded. A command's output is
//! read up to a byte cap and then cut off; a commit set is capped in length;
//! and the line digests kept per path and per commit are capped. Each cut is
//! reported on the value it affected, never silent. No line text is kept: a
//! changed line is reduced to a [`line_digest`], which is also what a reader
//! of edits in a recording uses, so both sides compare identical values.
//!
//! # Merge commits
//!
//! A merge commit is a member of the set with its parents, times, subject and
//! trailers, but it carries no changed paths, no digests and no patch id. The
//! commits it merged are members of their own, and counting the merge's diff
//! against its first parent as well would count their lines twice. A merge is
//! therefore resolved by sha alone ([`Resolution::Unresolved`] with
//! [`Unresolved::NoPatchId`] otherwise). Changes made only in the merge itself,
//! such as a conflict resolution, are not reported.
//!
//! # Renames
//!
//! A rename is one changed path, the new one, with the old one named in
//! `previous_path`; a pure rename has no digests, because it changed no line.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Bytes of a failed command's stderr kept as its message.
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// The role trailers whose values are kept as hints about who did the work.
const ROLE_TRAILERS: [(&str, Role); 5] = [
    ("planned-by", Role::PlannedBy),
    ("implemented-by", Role::ImplementedBy),
    ("reviewed-by", Role::ReviewedBy),
    ("orchestrated-by", Role::OrchestratedBy),
    ("assisted-by", Role::AssistedBy),
];

/// The bounds on one read.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bytes read from any one `git` command before it is cut off.
    pub output_bytes: usize,
    /// Commits in a set. A larger selection keeps the newest.
    pub commits: usize,
    /// Line digests kept for one changed path.
    pub digests_per_path: usize,
    /// Line digests kept for one commit, across its paths.
    pub digests_per_commit: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            output_bytes: 8 * 1024 * 1024,
            commits: 2_000,
            digests_per_path: 50_000,
            digests_per_commit: 200_000,
        }
    }
}

/// Which commits to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Each named revision is one commit, taken as a set in history order.
    Commits(Vec<String>),
    /// A list of commits kept in the order given.
    List(Vec<String>),
    /// `from..to`: the commits reachable from `to` and not from `from`.
    Range { from: String, to: String },
    /// `left...right`: the commits reachable from either side and not both.
    Symmetric { left: String, right: String },
    /// The commits on a branch that the repository's default branch lacks.
    Branch(String),
}

impl Selector {
    /// `A...B` is a symmetric range, `A..B` a range, anything else one commit.
    pub fn parse(text: &str) -> Self {
        if let Some((left, right)) = text.split_once("...") {
            Self::Symmetric {
                left: left.to_owned(),
                right: right.to_owned(),
            }
        } else if let Some((from, to)) = text.split_once("..") {
            Self::Range {
                from: from.to_owned(),
                to: to.to_owned(),
            }
        } else {
            Self::Commits(vec![text.to_owned()])
        }
    }
}

/// How the default branch was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultBranchSource {
    /// The remote's `HEAD`, as `origin/HEAD` records it.
    OriginHead,
    /// The branch `init.defaultBranch` names, because it exists.
    InitDefaultBranch,
    /// `main` or `master`, whichever exists first.
    Conventional,
}

/// The branch a repository's work is measured against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DefaultBranch {
    /// A revision naming it: `origin/main` for the remote's head, else the
    /// local branch name.
    pub name: String,
    pub source: DefaultBranchSource,
}

/// The ordered commits a selector named, with what was cut.
#[derive(Debug, Clone, Serialize)]
pub struct CommitSet {
    /// Oldest first, except for [`Selector::List`], which keeps its order.
    pub commits: Vec<Commit>,
    /// The default branch a [`Selector::Branch`] was measured against.
    pub default_branch: Option<DefaultBranch>,
    /// Commits the selection held beyond `Limits::commits`; the newest are
    /// kept.
    pub commits_truncated: bool,
    /// The commit metadata read hit the byte cap, so later commits are absent.
    pub metadata_truncated: bool,
}

/// The commits a selector named, without their changes.
#[derive(Debug, Clone, Serialize)]
pub struct Summaries {
    /// Ordered as [`CommitSet::commits`] is.
    pub commits: Vec<CommitSummary>,
    pub default_branch: Option<DefaultBranch>,
    pub commits_truncated: bool,
    pub metadata_truncated: bool,
}

/// One commit's identity, times and subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitSummary {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_time: DateTime<Utc>,
    pub committer_time: DateTime<Utc>,
    pub subject: String,
    pub merge: bool,
}

impl From<Metadata> for CommitSummary {
    fn from(metadata: Metadata) -> Self {
        Self {
            merge: metadata.parents.len() > 1,
            sha: metadata.sha,
            parents: metadata.parents,
            author_time: metadata.author_time,
            committer_time: metadata.committer_time,
            subject: metadata.subject,
        }
    }
}

/// The shas a selector names, before their metadata is read.
struct Listed {
    shas: Vec<String>,
    default_branch: Option<DefaultBranch>,
    truncated: bool,
}

/// One commit, its identities and its changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_time: DateTime<Utc>,
    pub committer_time: DateTime<Utc>,
    pub subject: String,
    pub trailers: Trailers,
    /// The commit has more than one parent, so it carries no changes of its
    /// own (see the module documentation).
    pub merge: bool,
    pub paths: Vec<ChangedPath>,
    /// `git patch-id --stable` of the commit's change. Absent for a merge, an
    /// empty change, and a change whose diff was cut at the byte cap.
    pub patch_id: Option<String>,
    /// The commit's diff hit the byte cap: later paths are absent or carry
    /// partial digests.
    pub diff_truncated: bool,
    /// The commit hit `Limits::digests_per_commit`.
    pub digests_truncated: bool,
}

/// The attribution hints a commit message carries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Trailers {
    pub co_authored_by: Vec<String>,
    pub roles: Vec<RoleTrailer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleTrailer {
    pub role: Role,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    PlannedBy,
    ImplementedBy,
    ReviewedBy,
    OrchestratedBy,
    AssistedBy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
    TypeChanged,
    Renamed,
}

/// What a path holds, which decides whether it has lines to digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Text,
    Binary,
    /// A submodule pointer: a commit id, not content.
    Submodule,
}

/// One path a commit changed, with the digests of its changed lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedPath {
    pub path: String,
    pub status: ChangeStatus,
    /// The path before a rename.
    pub previous_path: Option<String>,
    pub content: ContentKind,
    /// [`line_digest`]s of the added lines, in diff order.
    pub added: Vec<u64>,
    /// [`line_digest`]s of the removed lines, in diff order.
    pub removed: Vec<u64>,
    /// Digests of this path were cut at a limit, or its diff was not fully
    /// read.
    pub digests_truncated: bool,
}

/// How an observed sha relates to a commit set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    /// The sha is a member of the set.
    Exact {
        sha: String,
    },
    /// A member carries the same change under another sha, as a rebase,
    /// cherry-pick or message-only amend leaves it. Every such member is
    /// listed, in set order.
    PatchEquivalent {
        patch_id: String,
        candidates: Vec<String>,
    },
    Unresolved {
        reason: Unresolved,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unresolved {
    /// The repository no longer holds the object, so its change cannot be
    /// compared.
    Missing,
    /// The object has no patch id: a merge, or a commit that changed nothing
    /// (or whose diff exceeded the byte cap).
    NoPatchId,
    /// The object exists and no member of the set carries its change.
    NoEquivalent,
}

/// One live worktree: its directory and the branch it has checked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// A branch a merge commit's subject names, and the commits it merged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergedBranch {
    pub merge: String,
    pub branch: String,
    pub commits: Vec<String>,
}

/// The branch a default merge subject names, without a remote's prefix.
fn merged_branch_name(subject: &str) -> Option<String> {
    let quoted = |rest: &str| rest.split_once('\'').map(|(name, _)| name.to_owned());
    if let Some(rest) = subject.strip_prefix("Merge branch '") {
        return quoted(rest);
    }
    if let Some(rest) = subject.strip_prefix("Merge remote-tracking branch '") {
        return quoted(rest).map(|name| match name.split_once('/') {
            Some((_, branch)) => branch.to_owned(),
            None => name,
        });
    }
    let rest = subject.strip_prefix("Merge pull request #")?;
    let (_, from) = rest.split_once(" from ")?;
    let name = from.split_whitespace().next()?;
    Some(
        name.split_once('/')
            .map_or(name, |(_, branch)| branch)
            .to_owned(),
    )
}

/// A bounded, read-only view of one repository.
#[derive(Debug, Clone)]
pub struct GitReader {
    program: OsString,
    directory: PathBuf,
    environment: Vec<(OsString, OsString)>,
    limits: Limits,
}

impl GitReader {
    /// A reader for the repository containing `directory`. `None` when `git`
    /// is not installed: the reader is unavailable, which is a fact about the
    /// machine, not a failure.
    pub fn at(directory: &Path) -> Result<Option<Self>> {
        Self::with_program(OsStr::new("git"), directory)
    }

    /// As [`GitReader::at`], running the named `git` executable.
    pub fn with_program(program: &OsStr, directory: &Path) -> Result<Option<Self>> {
        let probe = Command::new(program)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match probe {
            Ok(status) if status.success() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to run git"),
        }
        Ok(Some(Self {
            program: program.to_owned(),
            directory: directory.to_owned(),
            environment: Vec::new(),
            limits: Limits::default(),
        }))
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Set a variable in the environment of every `git` command, such as
    /// `GIT_CONFIG_GLOBAL`.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.environment.push((key.into(), value.into()));
        self
    }

    /// The branch work is measured against: the remote's head when
    /// `origin/HEAD` points at a branch that exists, else the branch
    /// `init.defaultBranch` names when it exists, else `main`, else `master`.
    pub fn default_branch(&self) -> Result<Option<DefaultBranch>> {
        let head = self.run_lenient(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?;
        if head.success {
            let target = text(&head.bytes);
            if let Some(branch) = target.trim().strip_prefix("refs/remotes/") {
                if self.commit_exists(&format!("refs/remotes/{branch}"))? {
                    return Ok(Some(DefaultBranch {
                        name: branch.to_owned(),
                        source: DefaultBranchSource::OriginHead,
                    }));
                }
            }
        }
        let configured = self.run_lenient(&["config", "--get", "init.defaultBranch"])?;
        if configured.success {
            let name = text(&configured.bytes).trim().to_owned();
            if !name.is_empty() && self.commit_exists(&format!("refs/heads/{name}"))? {
                return Ok(Some(DefaultBranch {
                    name,
                    source: DefaultBranchSource::InitDefaultBranch,
                }));
            }
        }
        for name in ["main", "master"] {
            if self.commit_exists(&format!("refs/heads/{name}"))? {
                return Ok(Some(DefaultBranch {
                    name: name.to_owned(),
                    source: DefaultBranchSource::Conventional,
                }));
            }
        }
        Ok(None)
    }

    /// The commits a selector names, with their changes. An unknown revision
    /// refuses with git's own message.
    pub fn commits(&self, selector: &Selector) -> Result<CommitSet> {
        let listed = self.list(selector)?;
        let (metadata, metadata_truncated) = self.metadata(&listed.shas)?;
        let commits = metadata
            .into_iter()
            .map(|metadata| self.with_changes(metadata))
            .collect::<Result<Vec<_>>>()?;
        Ok(CommitSet {
            commits,
            default_branch: listed.default_branch,
            commits_truncated: listed.truncated,
            metadata_truncated,
        })
    }

    /// The commits a selector names, as [`GitReader::commits`] lists them,
    /// without reading their changes.
    pub fn summaries(&self, selector: &Selector) -> Result<Summaries> {
        let listed = self.list(selector)?;
        let (metadata, metadata_truncated) = self.metadata(&listed.shas)?;
        Ok(Summaries {
            commits: metadata.into_iter().map(CommitSummary::from).collect(),
            default_branch: listed.default_branch,
            commits_truncated: listed.truncated,
            metadata_truncated,
        })
    }

    /// Identity, times and subject of each of `shas`, in order, without
    /// their changes.
    pub fn describe(&self, shas: &[String]) -> Result<Vec<CommitSummary>> {
        let (metadata, _) = self.metadata(shas)?;
        Ok(metadata.into_iter().map(CommitSummary::from).collect())
    }

    fn list(&self, selector: &Selector) -> Result<Listed> {
        let mut default_branch = None;
        let mut listing: Vec<String> = vec!["rev-list".into(), "--topo-order".into()];
        let walk: Vec<String> = match selector {
            Selector::Commits(revs) => {
                listing.extend(["--reverse".into(), "--no-walk".into()]);
                require_revisions(revs.iter())?
            }
            Selector::List(revs) => {
                listing.push("--no-walk=unsorted".into());
                require_revisions(revs.iter())?
            }
            Selector::Range { from, to } => {
                listing.push("--reverse".into());
                require_revisions([from, to])?;
                vec![format!("{from}..{to}")]
            }
            Selector::Symmetric { left, right } => {
                listing.push("--reverse".into());
                require_revisions([left, right])?;
                vec![format!("{left}...{right}")]
            }
            Selector::Branch(branch) => {
                listing.push("--reverse".into());
                require_revisions([branch])?;
                let base = self.default_branch()?.ok_or_else(|| {
                    anyhow!(
                        "no default branch: origin/HEAD, init.defaultBranch, main and master \
                         are all absent"
                    )
                })?;
                let range = format!("{}..{branch}", base.name);
                default_branch = Some(base);
                vec![range]
            }
        };
        // One more than the cap, so that a larger selection is noticed. A walk
        // is cut by git, newest first; `--max-count` would also switch off
        // `--no-walk`, so a list of named commits is cut afterwards.
        let keep = self.limits.commits;
        let walking = !matches!(selector, Selector::Commits(_) | Selector::List(_));
        if walking {
            listing.push(format!("--max-count={}", keep + 1));
        }
        listing.push("--end-of-options".into());
        listing.extend(walk);
        let listed = self.run(&listing, None)?;
        let mut shas: Vec<String> = text(&listed.bytes).lines().map(str::to_owned).collect();
        let commits_truncated = shas.len() > keep;
        if commits_truncated {
            if matches!(selector, Selector::List(_)) {
                shas.truncate(keep);
            } else {
                // Oldest first, so the surplus is the oldest at the front.
                let surplus = shas.len() - keep;
                shas.drain(..surplus);
            }
        }
        Ok(Listed {
            shas,
            default_branch,
            truncated: commits_truncated,
        })
    }

    /// One commit with its changes, for any revision that names a commit.
    pub fn commit(&self, revision: &str) -> Result<Commit> {
        require_revisions([revision])?;
        let args: Vec<String> = ["rev-parse", "--verify", "--end-of-options"]
            .into_iter()
            .map(str::to_owned)
            .chain([format!("{revision}^{{commit}}")])
            .collect();
        let named = self.run(&args, None)?;
        let sha = text(&named.bytes).trim().to_owned();
        let (metadata, _) = self.metadata(std::slice::from_ref(&sha))?;
        let metadata = metadata
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("git returned no metadata for {sha}"))?;
        self.with_changes(metadata)
    }

    /// Match a sha a session saw against a set: by sha, else by the change it
    /// carries while the object still exists, else not at all.
    pub fn resolve(&self, set: &CommitSet, observed: &str) -> Result<Resolution> {
        let observed = observed.trim().to_ascii_lowercase();
        let is_hash = (4..=64).contains(&observed.len())
            && observed.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !is_hash {
            return Ok(Resolution::Unresolved {
                reason: Unresolved::Missing,
            });
        }
        let member = |sha: &str| set.commits.iter().find(|commit| commit.sha == sha);
        if observed.len() >= 7 {
            if let Some(commit) = set
                .commits
                .iter()
                .find(|commit| commit.sha.starts_with(&observed))
            {
                return Ok(Resolution::Exact {
                    sha: commit.sha.clone(),
                });
            }
        }
        let probe = format!("{observed}^{{commit}}");
        let found = self.run_lenient(&["rev-parse", "--verify", "--quiet", &probe])?;
        if !found.success {
            return Ok(Resolution::Unresolved {
                reason: Unresolved::Missing,
            });
        }
        let sha = text(&found.bytes).trim().to_owned();
        if let Some(commit) = member(&sha) {
            return Ok(Resolution::Exact {
                sha: commit.sha.clone(),
            });
        }
        let (metadata, _) = self.metadata(std::slice::from_ref(&sha))?;
        let Some(metadata) = metadata.into_iter().next() else {
            return Ok(Resolution::Unresolved {
                reason: Unresolved::Missing,
            });
        };
        let Some(patch_id) = self.with_changes(metadata)?.patch_id else {
            return Ok(Resolution::Unresolved {
                reason: Unresolved::NoPatchId,
            });
        };
        let candidates: Vec<String> = set
            .commits
            .iter()
            .filter(|commit| commit.patch_id.as_deref() == Some(patch_id.as_str()))
            .map(|commit| commit.sha.clone())
            .collect();
        Ok(if candidates.is_empty() {
            Resolution::Unresolved {
                reason: Unresolved::NoEquivalent,
            }
        } else {
            Resolution::PatchEquivalent {
                patch_id,
                candidates,
            }
        })
    }

    /// The commits the repository's branches, tags, remote branches and
    /// `HEAD` reach whose interval from the earlier to the later of their
    /// author and committer times meets `[from, to]`, oldest first, with their
    /// changes. Git cuts the walk by committer time alone, so the walk reaches
    /// back a day before `from` and each commit is then kept by both times.
    /// References a tool keeps for itself outside those namespaces are not
    /// walked.
    pub fn commits_between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<CommitSet> {
        let since = (from - chrono::Duration::days(1)).timestamp();
        let listing: Vec<String> = [
            "rev-list",
            "--topo-order",
            "--reverse",
            "--branches",
            "--tags",
            "--remotes",
            "HEAD",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([format!("--since=@{since}")])
        .collect();
        let listed = self.run_capped(
            &listing.iter().map(String::as_str).collect::<Vec<_>>(),
            None,
        )?;
        // A repository with no commit has no `HEAD` to walk.
        if !listed.success && !listed.truncated {
            return Ok(CommitSet {
                commits: Vec::new(),
                default_branch: None,
                commits_truncated: false,
                metadata_truncated: false,
            });
        }
        let shas: Vec<String> = text(&listed.bytes)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let (metadata, metadata_truncated) = self.metadata(&shas)?;
        let mut kept: Vec<Metadata> = metadata
            .into_iter()
            .filter(|commit| {
                commit.author_time.max(commit.committer_time) >= from
                    && commit.author_time.min(commit.committer_time) <= to
            })
            .collect();
        let commits_truncated = listed.truncated || kept.len() > self.limits.commits;
        if kept.len() > self.limits.commits {
            let surplus = kept.len() - self.limits.commits;
            kept.drain(..surplus);
        }
        let commits = kept
            .into_iter()
            .map(|metadata| self.with_changes(metadata))
            .collect::<Result<Vec<_>>>()?;
        Ok(CommitSet {
            commits,
            default_branch: None,
            commits_truncated,
            metadata_truncated,
        })
    }

    /// The live worktrees, the main one first: each one's directory and the
    /// branch it has checked out, none for a detached head.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let output = self.run_lenient(&["worktree", "list", "--porcelain"])?;
        output.require_success()?;
        let mut worktrees: Vec<Worktree> = Vec::new();
        for line in text(&output.bytes).lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                worktrees.push(Worktree {
                    path: PathBuf::from(path),
                    branch: None,
                });
            } else if let Some(branch) = line.strip_prefix("branch ") {
                if let Some(worktree) = worktrees.last_mut() {
                    worktree.branch = Some(
                        branch
                            .strip_prefix("refs/heads/")
                            .unwrap_or(branch)
                            .to_owned(),
                    );
                }
            }
        }
        Ok(worktrees)
    }

    /// The URLs of the repository's remotes, each once.
    pub fn remote_urls(&self) -> Result<Vec<String>> {
        let output = self.run_lenient(&["config", "--get-regexp", r"^remote\..*\.url$"])?;
        let mut urls: Vec<String> = text(&output.bytes)
            .lines()
            .filter_map(|line| line.split_once(' ').map(|(_, url)| url.trim().to_owned()))
            .filter(|url| !url.is_empty())
            .collect();
        urls.sort();
        urls.dedup();
        Ok(urls)
    }

    /// The commits a live branch holds that the default branch lacks, or
    /// `None` when no local branch has that name.
    pub fn branch_commits(&self, branch: &str) -> Result<Option<Vec<String>>> {
        if require_revisions([branch]).is_err()
            || !self.commit_exists(&format!("refs/heads/{branch}"))?
        {
            return Ok(None);
        }
        let Some(base) = self.default_branch()? else {
            return Ok(None);
        };
        let range = format!("{}..refs/heads/{branch}", base.name);
        let listed = self.run(
            &[
                "rev-list".to_owned(),
                format!("--max-count={}", self.limits.commits),
                "--end-of-options".to_owned(),
                range,
            ],
            None,
        )?;
        Ok(Some(
            text(&listed.bytes).lines().map(str::to_owned).collect(),
        ))
    }

    /// The commits a live branch's first-parent history holds that were
    /// committed at or after `since`, newest first, or `None` when no local
    /// branch has that name.
    pub fn first_parent_commits(
        &self,
        branch: &str,
        since: DateTime<Utc>,
    ) -> Result<Option<Vec<String>>> {
        if require_revisions([branch]).is_err()
            || !self.commit_exists(&format!("refs/heads/{branch}"))?
        {
            return Ok(None);
        }
        let listed = self.run(
            &[
                "rev-list".to_owned(),
                "--first-parent".to_owned(),
                format!("--max-count={}", self.limits.commits),
                format!("--since=@{}", since.timestamp()),
                "--end-of-options".to_owned(),
                format!("refs/heads/{branch}"),
            ],
            None,
        )?;
        Ok(Some(
            text(&listed.bytes).lines().map(str::to_owned).collect(),
        ))
    }

    /// What the merge commits committed at or after `since` say they merged:
    /// for each merge whose subject is one git or a forge writes by default
    /// (`Merge branch 'name'`, `Merge remote-tracking branch 'origin/name'`,
    /// `Merge pull request #n from owner/name`), the branch name and the
    /// commits its second parent brought that its first parent lacked.
    pub fn merged_branches(&self, since: DateTime<Utc>) -> Result<Vec<MergedBranch>> {
        let output = self.run_lenient(&[
            "log",
            "--branches",
            "--tags",
            "--remotes",
            "--merges",
            "--no-notes",
            "-z",
            &format!("--since=@{}", since.timestamp()),
            "--format=%H%x1f%P%x1f%s",
        ])?;
        if !output.success {
            return Ok(Vec::new());
        }
        let mut merged = Vec::new();
        for record in text(&output.bytes).split('\0') {
            let mut fields = record.trim_start_matches('\n').splitn(3, '\u{1f}');
            let (Some(sha), Some(parents), Some(subject)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let parents: Vec<&str> = parents.split_whitespace().collect();
            let [first, second] = parents[..] else {
                continue;
            };
            let Some(branch) = merged_branch_name(subject) else {
                continue;
            };
            let listed = self.run(
                &[
                    "rev-list".to_owned(),
                    format!("--max-count={}", self.limits.commits),
                    "--end-of-options".to_owned(),
                    format!("{first}..{second}"),
                ],
                None,
            )?;
            merged.push(MergedBranch {
                merge: sha.to_owned(),
                branch,
                commits: text(&listed.bytes).lines().map(str::to_owned).collect(),
            });
        }
        Ok(merged)
    }

    /// What the merges any branch, tag or remote branch reaches say they
    /// merged from `branch`, as [`GitReader::merged_branches`] reads a merge
    /// subject, over the whole history.
    pub fn merges_naming(&self, branch: &str) -> Result<Vec<MergedBranch>> {
        require_revisions([branch])?;
        let grep = format!("--grep={branch}");
        let output = self.run_lenient(&[
            "log",
            "--branches",
            "--tags",
            "--remotes",
            "--merges",
            "--no-notes",
            "--fixed-strings",
            &grep,
            "-z",
            "--format=%H%x1f%P%x1f%s",
        ])?;
        if !output.success {
            return Ok(Vec::new());
        }
        let mut merged = Vec::new();
        for record in text(&output.bytes).split('\0') {
            let mut fields = record.trim_start_matches('\n').splitn(3, '\u{1f}');
            let (Some(sha), Some(parents), Some(subject)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let parents: Vec<&str> = parents.split_whitespace().collect();
            let [first, second] = parents[..] else {
                continue;
            };
            if merged_branch_name(subject).as_deref() != Some(branch) {
                continue;
            }
            let listed = self.run(
                &[
                    "rev-list".to_owned(),
                    format!("--max-count={}", self.limits.commits),
                    "--end-of-options".to_owned(),
                    format!("{first}..{second}"),
                ],
                None,
            )?;
            merged.push(MergedBranch {
                merge: sha.to_owned(),
                branch: branch.to_owned(),
                commits: text(&listed.bytes).lines().map(str::to_owned).collect(),
            });
        }
        Ok(merged)
    }

    /// Whether a local branch of that name exists.
    pub fn has_branch(&self, branch: &str) -> Result<bool> {
        if require_revisions([branch]).is_err() {
            return Ok(false);
        }
        self.commit_exists(&format!("refs/heads/{branch}"))
    }

    /// The directory of the working tree holding the reader's directory, or
    /// `None` when no repository holds it.
    pub fn toplevel(&self) -> Result<Option<PathBuf>> {
        let output = self.run_lenient(&["rev-parse", "--show-toplevel"])?;
        Ok(output
            .success
            .then(|| PathBuf::from(text(&output.bytes).trim()))
            .filter(|path| !path.as_os_str().is_empty()))
    }

    /// The first-parent mainline merge or commit that brought each of
    /// `shas` to `mainline`, a revision naming the mainline's tip: a commit
    /// on the first-parent history is its own, and any other commit belongs
    /// to the oldest mainline merge whose second parents reach it and whose
    /// first parent does not. Mainline commits committed before `since` are
    /// not walked, and a commit no walked mainline commit brought is absent.
    pub fn mainline_arrivals(
        &self,
        mainline: &str,
        shas: &BTreeSet<String>,
        since: DateTime<Utc>,
    ) -> Result<BTreeMap<String, String>> {
        let listed = self.run(
            &[
                "rev-list".to_owned(),
                "--first-parent".to_owned(),
                "--parents".to_owned(),
                "--reverse".to_owned(),
                format!("--since=@{}", since.timestamp()),
                "--end-of-options".to_owned(),
                mainline.to_owned(),
            ],
            None,
        )?;
        let mut arrived: BTreeMap<String, String> = BTreeMap::new();
        for line in text(&listed.bytes).lines() {
            if arrived.len() == shas.len() {
                break;
            }
            let mut fields = line.split_whitespace();
            let Some(commit) = fields.next() else {
                continue;
            };
            if shas.contains(commit) {
                arrived
                    .entry(commit.to_owned())
                    .or_insert(commit.to_owned());
            }
            if fields.count() < 2 {
                continue;
            }
            let brought = self.run(
                &[
                    "rev-list".to_owned(),
                    format!("--max-count={}", self.limits.commits),
                    "--end-of-options".to_owned(),
                    format!("{commit}^1..{commit}"),
                ],
                None,
            )?;
            for sha in text(&brought.bytes).lines() {
                if shas.contains(sha) {
                    arrived.entry(sha.to_owned()).or_insert(commit.to_owned());
                }
            }
        }
        Ok(arrived)
    }

    /// Which of `names`, full or abbreviated hexadecimal shas, name a commit
    /// the repository holds. Anything else names none.
    pub fn existing_commits(
        &self,
        names: &std::collections::BTreeSet<String>,
    ) -> Result<std::collections::BTreeSet<String>> {
        let asked: Vec<&String> = names
            .iter()
            .filter(|name| {
                (4..=64).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .collect();
        if asked.is_empty() {
            return Ok(Default::default());
        }
        let mut input = String::new();
        for name in &asked {
            input.push_str(name);
            input.push('\n');
        }
        let output = self.run_capped(
            &["cat-file", "--batch-check=%(objecttype)"],
            Some(input.into_bytes()),
        )?;
        output.require_success()?;
        Ok(asked
            .into_iter()
            .zip(text(&output.bytes).lines())
            .filter(|(_, kind)| *kind == "commit")
            .map(|(name, _)| name.clone())
            .collect())
    }

    fn commit_exists(&self, reference: &str) -> Result<bool> {
        let probe = format!("{reference}^{{commit}}");
        Ok(self
            .run_lenient(&["rev-parse", "--verify", "--quiet", &probe])?
            .success)
    }

    /// Identity, times, subject and trailers for `shas`, in order. The second
    /// value says the byte cap cut the list short.
    fn metadata(&self, shas: &[String]) -> Result<(Vec<Metadata>, bool)> {
        if shas.is_empty() {
            return Ok((Vec::new(), false));
        }
        let format = "--format=%H%x1f%P%x1f%at%x1f%ct%x1f%s%x1f%(trailers:only=true,unfold=true)";
        let mut input = shas.join("\n");
        input.push('\n');
        let output = self.run_capped(
            &[
                "-c",
                "log.showSignature=false",
                "log",
                "--no-walk=unsorted",
                "--stdin",
                "--no-notes",
                "--encoding=UTF-8",
                "-z",
                format,
            ],
            Some(input.into_bytes()),
        )?;
        output.require_success()?;
        let mut bytes = output.bytes.as_slice();
        if output.truncated {
            // The last record may be cut; records end at a NUL.
            let complete = bytes
                .iter()
                .rposition(|&byte| byte == 0)
                .map_or(0, |at| at + 1);
            bytes = &bytes[..complete];
        }
        let metadata = bytes
            .split(|&byte| byte == 0)
            .filter(|record| !record.is_empty())
            .map(parse_metadata)
            .collect::<Result<Vec<_>>>()?;
        Ok((metadata, output.truncated))
    }

    fn with_changes(&self, metadata: Metadata) -> Result<Commit> {
        let merge = metadata.parents.len() > 1;
        let mut changes = Changes::default();
        if !merge {
            let output = self.run_capped(
                &[
                    "diff-tree",
                    "-p",
                    "--raw",
                    "-M",
                    "-r",
                    "--root",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--end-of-options",
                    &metadata.sha,
                ],
                None,
            )?;
            output.require_success()?;
            changes = parse_diff(&output.bytes, output.truncated, &self.limits);
            if !output.truncated && !changes.paths.is_empty() {
                changes.patch_id = self.patch_id(&output.bytes)?;
            }
        }
        Ok(Commit {
            sha: metadata.sha,
            parents: metadata.parents,
            author_time: metadata.author_time,
            committer_time: metadata.committer_time,
            subject: metadata.subject,
            trailers: metadata.trailers,
            merge,
            paths: changes.paths,
            patch_id: changes.patch_id,
            diff_truncated: changes.diff_truncated,
            digests_truncated: changes.digests_truncated,
        })
    }

    fn patch_id(&self, diff: &[u8]) -> Result<Option<String>> {
        let output = self.run(
            &["patch-id".to_owned(), "--stable".to_owned()],
            Some(diff.to_vec()),
        )?;
        Ok(text(&output.bytes)
            .split_whitespace()
            .next()
            .map(str::to_owned))
    }

    /// Run `git` and fail with its message unless it succeeds. A cut-off read
    /// is not a failure but is not complete either, so callers that cannot
    /// take one use [`GitReader::run_capped`].
    fn run(&self, args: &[String], stdin: Option<Vec<u8>>) -> Result<Output> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = self.run_capped(&args, stdin)?;
        output.require_success()?;
        if output.truncated {
            bail!(
                "git {} output exceeded {} bytes",
                args.first().copied().unwrap_or_default(),
                self.limits.output_bytes
            );
        }
        Ok(output)
    }

    fn run_lenient(&self, args: &[&str]) -> Result<Output> {
        let output = self.run_capped(args, None)?;
        if output.truncated {
            bail!("git {} output exceeded the byte cap", args[0]);
        }
        Ok(output)
    }

    /// Run `git`, reading its stdout up to the byte cap and killing it when
    /// the cap is passed.
    fn run_capped(&self, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Output> {
        let mut command = Command::new(&self.program);
        command
            .arg("-C")
            .arg(&self.directory)
            .arg("--no-pager")
            .args(["-c", "core.quotePath=false"])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .envs(self.environment.iter().map(|(k, v)| (k, v)))
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("failed to run git")?;
        let writer = stdin.map(|bytes| {
            let mut pipe = child.stdin.take().expect("piped stdin is present");
            // A cut-off read closes the pipe early; the write error is moot.
            thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            })
        });
        let mut stderr = child.stderr.take().expect("piped stderr is present");
        let diagnostics = thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buffer = [0_u8; 4096];
            while let Ok(count) = stderr.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let room = MAX_STDERR_BYTES.saturating_sub(kept.len()).min(count);
                kept.extend_from_slice(&buffer[..room]);
            }
            kept
        });
        let cap = self.limits.output_bytes;
        let mut bytes = Vec::new();
        let read = child
            .stdout
            .take()
            .expect("piped stdout is present")
            .take(cap as u64 + 1)
            .read_to_end(&mut bytes);
        let truncated = bytes.len() > cap;
        if truncated || read.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().context("failed to wait for git")?;
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        let stderr = diagnostics.join().unwrap_or_default();
        read.context("failed to read git output")?;
        bytes.truncate(cap);
        Ok(Output {
            bytes,
            truncated,
            success: status.success(),
            stderr: text(&stderr).trim().to_owned(),
        })
    }
}

struct Output {
    bytes: Vec<u8>,
    truncated: bool,
    success: bool,
    stderr: String,
}

impl Output {
    /// Refuse with git's own message. A cut-off read was killed on purpose,
    /// so its exit status says nothing.
    fn require_success(&self) -> Result<()> {
        if self.success || self.truncated {
            return Ok(());
        }
        if self.stderr.is_empty() {
            bail!("git failed without a message");
        }
        bail!("{}", self.stderr)
    }
}

struct Metadata {
    sha: String,
    parents: Vec<String>,
    author_time: DateTime<Utc>,
    committer_time: DateTime<Utc>,
    subject: String,
    trailers: Trailers,
}

fn parse_metadata(record: &[u8]) -> Result<Metadata> {
    let record = text(record);
    let mut fields = record.splitn(6, '\u{1f}');
    let mut next = |name: &str| {
        fields
            .next()
            .ok_or_else(|| anyhow!("git log record lacks {name}"))
    };
    let sha = next("a sha")?.trim_start_matches('\n').to_owned();
    let parents = next("parents")?
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let author_time = timestamp(next("an author time")?)?;
    let committer_time = timestamp(next("a committer time")?)?;
    let subject = next("a subject")?.to_owned();
    let trailers = parse_trailers(next("trailers")?);
    Ok(Metadata {
        sha,
        parents,
        author_time,
        committer_time,
        subject,
        trailers,
    })
}

fn timestamp(seconds: &str) -> Result<DateTime<Utc>> {
    seconds
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .ok_or_else(|| anyhow!("git log gave an unreadable time {seconds:?}"))
}

fn parse_trailers(block: &str) -> Trailers {
    let mut trailers = Trailers::default();
    for line in block.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        if key == "co-authored-by" {
            trailers.co_authored_by.push(value);
        } else if let Some((_, role)) = ROLE_TRAILERS.iter().find(|(name, _)| *name == key) {
            trailers.roles.push(RoleTrailer { role: *role, value });
        }
    }
    trailers
}

#[derive(Default)]
struct Changes {
    paths: Vec<ChangedPath>,
    patch_id: Option<String>,
    diff_truncated: bool,
    digests_truncated: bool,
}

/// Read `git diff-tree -p --raw -M` output: the raw entries name the paths,
/// and the patch sections that follow, in the same order, carry their lines.
fn parse_diff(bytes: &[u8], truncated: bool, limits: &Limits) -> Changes {
    let mut bytes = bytes;
    if truncated {
        // A cut line cannot be digested truthfully; drop it.
        let complete = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |at| at + 1);
        bytes = &bytes[..complete];
    }
    let mut changes = Changes {
        diff_truncated: truncated,
        ..Changes::default()
    };
    let mut section: Option<usize> = None;
    let mut sections = 0_usize;
    let mut in_hunk = false;
    let mut commit_digests = 0_usize;
    for line in bytes.split(|&byte| byte == b'\n') {
        if section.is_none() && line.first() == Some(&b':') {
            changes.paths.extend(parse_raw(line));
            continue;
        }
        if line.starts_with(b"diff --git ") {
            section = Some(sections);
            sections += 1;
            in_hunk = false;
            continue;
        }
        let Some(path) = section.and_then(|at| changes.paths.get_mut(at)) else {
            continue;
        };
        if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
            path.content = ContentKind::Binary;
            continue;
        }
        if line.starts_with(b"@@ ") {
            in_hunk = true;
            continue;
        }
        if !in_hunk || path.content != ContentKind::Text {
            continue;
        }
        let added = match line.first() {
            Some(b'+') => true,
            Some(b'-') => false,
            _ => continue,
        };
        let Some(digest) = line_digest(&String::from_utf8_lossy(&line[1..])) else {
            continue;
        };
        if path.added.len() + path.removed.len() >= limits.digests_per_path {
            path.digests_truncated = true;
        } else if commit_digests >= limits.digests_per_commit {
            path.digests_truncated = true;
            changes.digests_truncated = true;
        } else {
            let kept = if added {
                &mut path.added
            } else {
                &mut path.removed
            };
            kept.push(digest);
            commit_digests += 1;
        }
    }
    if truncated {
        // The section being read when the cap hit is partial, and the ones
        // after it were never reached.
        let first_unfinished = sections.saturating_sub(1);
        for path in changes.paths.iter_mut().skip(first_unfinished) {
            path.digests_truncated = true;
        }
    }
    changes
}

/// One `:<mode> <mode> <sha> <sha> <status>\t<path>[\t<path>]` raw entry.
fn parse_raw(line: &[u8]) -> Option<ChangedPath> {
    let line = text(line);
    let (meta, paths) = line.split_once('\t')?;
    let mut meta = meta.trim_start_matches(':').split(' ');
    let modes = (meta.next()?, meta.next()?);
    let status = meta.nth(2)?;
    let mut paths = paths.split('\t').map(unquote);
    let first = paths.next()?;
    let second = paths.next();
    let (status, path, previous_path) = match status.chars().next()? {
        'A' => (ChangeStatus::Added, first, None),
        'D' => (ChangeStatus::Deleted, first, None),
        'T' => (ChangeStatus::TypeChanged, first, None),
        'R' | 'C' => (ChangeStatus::Renamed, second?, Some(first)),
        _ => (ChangeStatus::Modified, first, None),
    };
    let content = if modes.0 == "160000" || modes.1 == "160000" {
        ContentKind::Submodule
    } else {
        ContentKind::Text
    };
    Some(ChangedPath {
        path,
        status,
        previous_path,
        content,
        added: Vec::new(),
        removed: Vec::new(),
        digests_truncated: false,
    })
}

/// Undo git's C-style quoting of a path that holds a control character, a
/// quote or a backslash.
fn unquote(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|path| path.strip_suffix('"'))
    else {
        return path.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match chars.next() {
            Some(b'a') => bytes.push(7),
            Some(b'b') => bytes.push(8),
            Some(b'f') => bytes.push(12),
            Some(b'n') => bytes.push(b'\n'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'v') => bytes.push(11),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(next @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(next - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The digest of one line of text, or `None` when the line carries nothing to
/// compare: it is blank or made of punctuation alone, such as a closing brace.
///
/// A line is normalised by trimming trailing whitespace, then hashed with
/// SHA-256; the digest is the first eight bytes read as a big-endian `u64`.
/// Leading whitespace is part of the line. Readers of a commit's diff and
/// readers of edits in a recording both digest through this function, so equal
/// lines from different sources give equal digests.
pub fn line_digest(line: &str) -> Option<u64> {
    let line = line.trim_end();
    if line
        .chars()
        .all(|character| character.is_ascii_punctuation() || character.is_whitespace())
    {
        return None;
    }
    let hash = Sha256::digest(line.as_bytes());
    let mut head = [0_u8; 8];
    head.copy_from_slice(&hash[..8]);
    Some(u64::from_be_bytes(head))
}

/// Revisions that cannot be read by git as options or negations.
fn require_revisions<S: AsRef<str>>(revisions: impl IntoIterator<Item = S>) -> Result<Vec<String>> {
    let mut checked = Vec::new();
    let mut any = false;
    for revision in revisions {
        let revision = revision.as_ref();
        any = true;
        if revision.is_empty() || revision.starts_with(['-', '^']) || revision.contains("..") {
            bail!("{revision:?} is not a single revision");
        }
        checked.push(revision.to_owned());
    }
    if !any {
        bail!("no revision was given");
    }
    Ok(checked)
}

fn text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}
