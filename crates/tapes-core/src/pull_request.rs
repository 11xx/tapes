//! A pull request as the forge reports it, and the commits of the local
//! repository it selects.
//!
//! The forge knows which commits a pull request held and when it lived; the
//! GitHub CLI, `gh`, supplies that, and nothing else is fetched. The
//! repository is only read: no fetch, no write.
//!
//! The commits a pull request selects depend on how it ended:
//!
//! - **open**, or **closed** without a merge: the listed commits that exist
//!   locally;
//! - **merge commit**: the merge commit has two parents and its second
//!   parent reaches every listed commit that exists locally. The listed
//!   commits are selected; the merge commit authors nothing and is not;
//! - **rebase**: the merge commit has one parent, and the last `n` commits
//!   along first parents ending at it, `n` being the number of listed
//!   commits, carry the listed subjects in order. Those commits are
//!   selected;
//! - **squash**: the merge commit has one parent and is not a rebase. The
//!   merge commit is selected, with any listed commit that still exists
//!   locally;
//! - **unrecognized**: the merge is not in the repository, or its shape fits
//!   no rule above. The listed commits that exist locally are selected.
//!
//! A squash or rebase makes the pull request's work again under new shas,
//! after the work happened. Those commits therefore admit evidence over the
//! pull request's lifetime, from its creation to its merge, besides their
//! own windows (see [`crate::change_attribution::Repository::set_lifetimes`]).

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant as Clock};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::git_reader::{CommitSummary, GitReader, Selector};
use crate::span::Instant;

/// The fields `gh pr view` is asked for.
pub const FIELDS: &str = "number,state,commits,baseRefName,headRefName,mergeCommit,createdAt,\
                          mergedAt,closedAt,title";

/// Bytes read from `gh` before it is cut off.
const OUTPUT_BYTES: u64 = 8 * 1024 * 1024;
/// Bytes of `gh`'s own error kept to name what failed.
const ERROR_BYTES: u64 = 2 * 1024;
/// How long `gh` may take to answer.
const TIMEOUT: Duration = Duration::from_secs(60);

/// How the forge says a pull request stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum State {
    Open,
    Closed,
    Merged,
}

/// One commit of a pull request, as listed.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    pub oid: String,
    #[serde(default)]
    pub message_headline: String,
}

/// The commit the forge made by merging.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MergeCommit {
    pub oid: String,
}

/// What `gh pr view --json` reports.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub state: State,
    pub commits: Vec<Listed>,
    pub base_ref_name: String,
    pub head_ref_name: String,
    #[serde(default)]
    pub merge_commit: Option<MergeCommit>,
    pub created_at: Instant,
    #[serde(default)]
    pub merged_at: Option<Instant>,
    #[serde(default)]
    pub closed_at: Option<Instant>,
    #[serde(default)]
    pub title: String,
}

impl PullRequest {
    pub fn parse(json: &[u8]) -> Result<Self> {
        serde_json::from_slice(json).context("gh pr view printed JSON tapes could not read")
    }

    /// When the pull request ended: its merge, else its close; none while
    /// open.
    pub fn ended_at(&self) -> Option<Instant> {
        match self.state {
            State::Open => None,
            State::Merged => self.merged_at.or(self.closed_at),
            State::Closed => self.closed_at,
        }
    }
}

/// Ask `gh` for pull request `number` of the repository holding
/// `directory`, as `gh pr view NUMBER --json ...`.
pub fn fetch(directory: &Path, number: u64) -> Result<PullRequest> {
    let mut child = match Command::new("gh")
        .args(["pr", "view", &number.to_string(), "--json", FIELDS])
        .current_dir(directory)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
            "--pr reads the pull request through the GitHub CLI, `gh`, which is not installed: \
             install gh, or pass the pull request's commits with --commits-from FILE (for \
             example `git log --format=%H BASE..HEAD > FILE`)"
        ),
        Err(error) => return Err(error).context("failed to run gh"),
    };
    let out = child.stdout.take().expect("stdout is piped");
    let err = child.stderr.take().expect("stderr is piped");
    let stdout = std::thread::spawn(move || read_bounded(out, OUTPUT_BYTES));
    let stderr = std::thread::spawn(move || read_bounded(err, ERROR_BYTES));
    let started = Clock::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "gh pr view {number} gave no answer in {} seconds",
                TIMEOUT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (stdout, truncated) = stdout.join().map_err(|_| anyhow!("gh output thread"))??;
    let (stderr, _) = stderr.join().map_err(|_| anyhow!("gh error thread"))??;
    if !status.success() {
        let said = String::from_utf8_lossy(&stderr);
        let said = said.trim();
        bail!(
            "gh pr view {number} failed ({status}){}{}: --pr needs gh installed, authenticated \
             and run where the repository has a GitHub remote naming that pull request; pass \
             the commits with --commits-from FILE instead",
            if said.is_empty() { "" } else { ": " },
            said
        );
    }
    if truncated {
        bail!("gh pr view {number} printed more than {OUTPUT_BYTES} bytes");
    }
    PullRequest::parse(&stdout)
}

fn read_bounded(mut source: impl Read, limit: u64) -> Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    (&mut source).take(limit + 1).read_to_end(&mut kept)?;
    let truncated = kept.len() as u64 > limit;
    kept.truncate(limit as usize);
    // Drain the rest so the child is never blocked on a full pipe.
    std::io::copy(&mut source, &mut std::io::sink())?;
    Ok((kept, truncated))
}

/// How a pull request's commits were found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    Open,
    Closed,
    MergeCommit,
    Squash,
    Rebase,
    Unrecognized,
}

impl Method {
    /// The rule that selects the commits, as the output names it.
    pub fn rule(self) -> &'static str {
        match self {
            Self::Open => "open: the listed commits that exist locally",
            Self::Closed => "closed unmerged: the listed commits that exist locally",
            Self::MergeCommit => {
                "merged with a merge commit: the listed commits, not the merge commit"
            }
            Self::Squash => {
                "squash-merged: the squash commit, with the listed commits that still exist \
                 locally"
            }
            Self::Rebase => {
                "rebase-merged: the rebased commits ending at the merge commit, their subjects \
                 matching the listed ones in order"
            }
            Self::Unrecognized => "no merge rule fits: the listed commits that exist locally",
        }
    }
}

/// The pull request, as a change view reports it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PullRequestView {
    pub number: u64,
    pub state: State,
    pub method: Method,
    /// The rule that selected the commits.
    pub rule: &'static str,
    /// Why no rule fit, where none did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unrecognized: Option<String>,
    pub base: String,
    pub head: String,
    pub title: String,
    pub created_at: Instant,
    /// When it was merged, else closed; absent while open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Instant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_commit: Option<String>,
    /// The commits the forge lists.
    pub listed: usize,
    /// Listed commits the repository does not hold, by sha.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// The commits that admit evidence over the pull request's lifetime.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub widened: Vec<String>,
}

/// The commits a pull request selects.
pub struct PullRequestSelection {
    pub view: PullRequestView,
    pub commits: Vec<CommitSummary>,
    /// The span the widened commits admit evidence over.
    pub lifetime: Option<(Instant, Instant)>,
}

/// The commits of `reader`'s repository that pull request `pr` selects.
pub fn select(reader: &GitReader, pr: &PullRequest) -> Result<PullRequestSelection> {
    let listed: Vec<String> = pr
        .commits
        .iter()
        .map(|commit| commit.oid.to_ascii_lowercase())
        .collect();
    let held = reader.existing_commits(&listed.iter().cloned().collect())?;
    let local: Vec<String> = listed
        .iter()
        .filter(|sha| held.contains(*sha))
        .cloned()
        .collect();
    let missing: Vec<String> = listed
        .iter()
        .filter(|sha| !held.contains(*sha))
        .cloned()
        .collect();
    let merge_commit = pr
        .merge_commit
        .as_ref()
        .map(|commit| commit.oid.to_ascii_lowercase());

    let (method, shas, widened, unrecognized) = match pr.state {
        State::Open => (Method::Open, local, Vec::new(), None),
        State::Closed => (Method::Closed, local, Vec::new(), None),
        State::Merged => match merged(reader, pr, &listed, &local, merge_commit.as_deref())? {
            Merged::Rule {
                method,
                shas,
                widened,
            } => (method, shas, widened, None),
            Merged::NoRule(why) => (Method::Unrecognized, local, Vec::new(), Some(why)),
        },
    };
    let mut seen = BTreeSet::new();
    let shas: Vec<String> = shas
        .into_iter()
        .filter(|sha| seen.insert(sha.clone()))
        .collect();
    let commits = reader.describe(&shas)?;
    let view = PullRequestView {
        number: pr.number,
        state: pr.state,
        method,
        rule: method.rule(),
        unrecognized,
        base: pr.base_ref_name.clone(),
        head: pr.head_ref_name.clone(),
        title: pr.title.clone(),
        created_at: pr.created_at,
        ended_at: pr.ended_at(),
        merge_commit,
        listed: listed.len(),
        missing,
        widened: widened.clone(),
    };
    if commits.is_empty() {
        bail!(
            "pull request #{} selects no commits ({}{}): the repository holds none of the {} \
             listed commits{}; fetch the branch yourself, or pass the commits with \
             --commits-from FILE",
            pr.number,
            method.rule(),
            view.unrecognized
                .as_deref()
                .map(|why| format!("; {why}"))
                .unwrap_or_default(),
            view.listed,
            if method == Method::Squash {
                " and the squash commit is not among them"
            } else {
                ""
            }
        );
    }
    let lifetime = match (widened.is_empty(), pr.ended_at()) {
        (false, Some(ended)) => Some((pr.created_at, ended)),
        _ => None,
    };
    Ok(PullRequestSelection {
        view,
        commits,
        lifetime,
    })
}

/// What a merged pull request's merge commit shows.
enum Merged {
    /// A rule fits: the commits to select, and those among them that stand
    /// for the pull request's work under new shas.
    Rule {
        method: Method,
        shas: Vec<String>,
        widened: Vec<String>,
    },
    NoRule(String),
}

fn merged(
    reader: &GitReader,
    pr: &PullRequest,
    listed: &[String],
    local: &[String],
    merge_commit: Option<&str>,
) -> Result<Merged> {
    let Some(merge_commit) = merge_commit else {
        return Ok(Merged::NoRule(
            "gh reports no merge commit for the merged pull request".to_owned(),
        ));
    };
    let held = reader.existing_commits(&BTreeSet::from([merge_commit.to_owned()]))?;
    if !held.contains(merge_commit) {
        return Ok(Merged::NoRule(format!(
            "the merge commit {merge_commit} is not in the repository, which tapes never fetches"
        )));
    }
    let merge = reader
        .describe(&[merge_commit.to_owned()])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("git returned no metadata for {merge_commit}"))?;
    match merge.parents.as_slice() {
        [first, second] => {
            let side = reader
                .summaries(&Selector::Range {
                    from: first.clone(),
                    to: second.clone(),
                })
                .with_context(|| format!("the commits merge {merge_commit} brought"))?;
            let brought: BTreeSet<&str> = side
                .commits
                .iter()
                .map(|commit| commit.sha.as_str())
                .collect();
            if !local.is_empty() && local.iter().all(|sha| brought.contains(sha.as_str())) {
                Ok(Merged::Rule {
                    method: Method::MergeCommit,
                    shas: local.to_vec(),
                    widened: Vec::new(),
                })
            } else {
                Ok(Merged::NoRule(format!(
                    "the merge commit {merge_commit} has two parents, but its second parent does \
                     not reach the listed commits"
                )))
            }
        }
        [_] => {
            let count = listed.len();
            let chain = reader.first_parent_chain(merge_commit, count)?;
            let rebased = count > 0
                && chain.len() == count
                && chain
                    .iter()
                    .zip(&pr.commits)
                    .all(|(commit, listed)| commit.subject == listed.message_headline);
            if rebased {
                let shas: Vec<String> = chain.iter().map(|commit| commit.sha.clone()).collect();
                Ok(Merged::Rule {
                    method: Method::Rebase,
                    widened: shas.clone(),
                    shas,
                })
            } else {
                let mut shas: Vec<String> = local
                    .iter()
                    .filter(|sha| sha.as_str() != merge_commit)
                    .cloned()
                    .collect();
                shas.push(merge_commit.to_owned());
                Ok(Merged::Rule {
                    method: Method::Squash,
                    shas,
                    widened: vec![merge_commit.to_owned()],
                })
            }
        }
        parents => Ok(Merged::NoRule(format!(
            "the merge commit {merge_commit} has {} parents",
            parents.len()
        ))),
    }
}
