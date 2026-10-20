//! The repository side of an attribution: its worktrees and remotes, the
//! commits evidence may name, and which commit an observed sha names.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{CommitFacts, Params, Via};
use crate::git_reader::{CommitSet, GitReader, Resolution, Worktree};
use crate::scope::Scope;
use crate::span::Instant;

/// One repository, read through `git`, read-only.
pub struct Repository {
    reader: GitReader,
    scope: Scope,
    /// The main worktree's directory.
    pub root: PathBuf,
    /// The live worktrees, the main one first.
    pub worktrees: Vec<Worktree>,
    /// The remotes' URLs, normalized by [`normalize_remote`].
    pub remotes: BTreeSet<String>,
    /// The local name of the branch work is measured against.
    pub default_branch: Option<String>,
    resolved: RefCell<HashMap<String, Option<(String, Via)>>>,
}

/// The commits evidence may name, as a set and as the partition weighs
/// them.
pub(crate) struct Universe {
    pub set: CommitSet,
    pub facts: Vec<CommitFacts>,
}

impl Repository {
    /// The repository holding `directory`, `None` when `git` is not
    /// installed.
    pub fn open(directory: &Path) -> Result<Option<Self>> {
        let Some(reader) = GitReader::at(directory)? else {
            return Ok(None);
        };
        let worktrees = reader.worktrees()?;
        let root = worktrees
            .first()
            .map(|worktree| worktree.path.clone())
            .unwrap_or_else(|| directory.to_owned());
        let scope = Scope::at(&root)?;
        let remotes = reader
            .remote_urls()?
            .iter()
            .filter_map(|url| normalize_remote(url))
            .collect();
        let default_branch =
            reader
                .default_branch()?
                .map(|branch| match branch.name.split_once('/') {
                    Some((_, name))
                        if branch.source == crate::git_reader::DefaultBranchSource::OriginHead =>
                    {
                        name.to_owned()
                    }
                    _ => branch.name,
                });
        Ok(Some(Self {
            reader,
            scope,
            root,
            worktrees,
            remotes,
            default_branch,
            resolved: RefCell::new(HashMap::new()),
        }))
    }

    pub fn reader(&self) -> &GitReader {
        &self.reader
    }

    /// Whether a directory belongs to the repository: `Some` for one that
    /// exists, as git places it; `None` for one that is gone, which nothing
    /// left on disk can place.
    pub fn holds(&self, directory: &Path) -> Option<bool> {
        directory.exists().then(|| self.scope.contains(directory))
    }

    /// Whether a remote URL a recording names is one of the repository's.
    pub fn has_remote(&self, url: &str) -> bool {
        normalize_remote(url).is_some_and(|url| self.remotes.contains(&url))
    }

    /// The commits whose windows meet `[from, to]`.
    pub(crate) fn universe(&self, from: Instant, to: Instant, params: &Params) -> Result<Universe> {
        let set = self
            .reader
            .commits_between(from - params.lookahead, to + params.lookback)?;
        let facts = set
            .commits
            .iter()
            .map(|commit| {
                let mut added: Vec<u64> = commit
                    .paths
                    .iter()
                    .flat_map(|path| path.added.iter().copied())
                    .collect();
                added.sort_unstable();
                added.dedup();
                CommitFacts {
                    sha: commit.sha.clone(),
                    author: commit.author_time,
                    committer: commit.committer_time,
                    merge: commit.merge,
                    added,
                }
            })
            .collect();
        Ok(Universe { set, facts })
    }

    /// The commits the repository's history names on each of `branches`:
    /// those a live branch holds beyond the default branch, and those a
    /// merge committed at or after `since` says it brought from the branch.
    /// The default branch itself is named by neither.
    pub(crate) fn branch_commits(
        &self,
        branches: &BTreeSet<String>,
        since: Instant,
    ) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let mut known: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let wanted = |branch: &str| {
            branches.contains(branch) && self.default_branch.as_deref() != Some(branch)
        };
        for branch in branches.iter().filter(|branch| wanted(branch)) {
            if let Some(commits) = self.reader.branch_commits(branch)? {
                known.entry(branch.clone()).or_default().extend(commits);
            }
        }
        for merged in self.reader.merged_branches(since)? {
            if wanted(&merged.branch) {
                known
                    .entry(merged.branch)
                    .or_default()
                    .extend(merged.commits);
            }
        }
        Ok(known)
    }

    /// The commit of `universe` an observed sha names: the commit itself, or
    /// a commit with its stable patch id while the observed object exists,
    /// or else the commit that adds at least [`Params::edit_overlap`] of the
    /// lines the observed object adds. Several candidates resolve to the
    /// newest.
    pub(crate) fn resolve(
        &self,
        universe: &Universe,
        observed: &str,
        params: &Params,
    ) -> Result<Option<(String, Via)>> {
        if let Some(known) = self.resolved.borrow().get(observed) {
            return Ok(known.clone());
        }
        let resolved = match self.reader.resolve(&universe.set, observed)? {
            Resolution::Exact { sha } => Some((sha, Via::Exact)),
            Resolution::PatchEquivalent { candidates, .. } => {
                candidates.last().map(|sha| (sha.clone(), Via::PatchId))
            }
            Resolution::Unresolved { reason } => match reason {
                crate::git_reader::Unresolved::Missing => None,
                _ => self.by_digests(universe, observed, params)?,
            },
        };
        self.resolved
            .borrow_mut()
            .insert(observed.to_owned(), resolved.clone());
        Ok(resolved)
    }

    fn by_digests(
        &self,
        universe: &Universe,
        observed: &str,
        params: &Params,
    ) -> Result<Option<(String, Via)>> {
        let Ok(commit) = self.reader.commit(observed) else {
            return Ok(None);
        };
        let added: BTreeSet<u64> = commit
            .paths
            .iter()
            .flat_map(|path| path.added.iter().copied())
            .collect();
        if added.len() < params.edit_lines {
            return Ok(None);
        }
        let best = universe
            .facts
            .iter()
            .filter(|candidate| candidate.sha != commit.sha && !candidate.merge)
            .map(|candidate| {
                let shared = candidate
                    .added
                    .iter()
                    .filter(|digest| added.contains(digest))
                    .count();
                (shared, candidate)
            })
            .filter(|(shared, _)| *shared as f64 >= params.edit_overlap * added.len() as f64)
            .max_by_key(|(shared, candidate)| {
                (*shared, candidate.committer, candidate.sha.clone())
            });
        Ok(best.map(|(_, candidate)| (candidate.sha.clone(), Via::Digests)))
    }

    /// Which of `names` name a commit the repository holds.
    pub(crate) fn names_commits(&self, names: &BTreeSet<String>) -> Result<BTreeSet<String>> {
        self.reader.existing_commits(names)
    }
}

/// A remote URL as a host and a path, so the HTTPS and SSH forms of one
/// remote compare equal: no scheme, no user, no `.git` suffix, no trailing
/// slash, lower-cased host.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest.to_owned(),
        // scp-like `user@host:path`.
        None => {
            let (host, path) = url.split_once(':')?;
            if host.contains('/') {
                return None;
            }
            format!("{host}/{path}")
        }
    };
    let rest = match rest.split_once('@') {
        Some((user, after)) if !user.contains('/') => after.to_owned(),
        _ => rest,
    };
    let (host, path) = rest.split_once('/')?;
    let host = host.split(':').next()?.to_ascii_lowercase();
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    (!host.is_empty() && !path.is_empty()).then(|| format!("{host}/{path}"))
}
