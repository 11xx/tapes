//! The git reader turns a selector into commits with their identities and
//! content digests, read-only and bounded. Each test names the caller-visible
//! claim it holds and the wrong behaviour it rejects.

use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

use agent_tapes_core::git_reader::{
    line_digest, ChangeStatus, ChangedPath, Commit, CommitSet, ContentKind, DefaultBranchSource,
    GitReader, Limits, Resolution, Role, Selector, Unresolved,
};
use chrono::{DateTime, Utc};
use tempfile::TempDir;

/// A synthetic repository whose git sees no user or system configuration.
struct Repo {
    root: TempDir,
    clock: Cell<i64>,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            root: TempDir::with_prefix("tapes-git-reader-").unwrap(),
            clock: Cell::new(1_700_000_000),
        };
        fs::write(repo.config(), "").unwrap();
        fs::create_dir(repo.dir()).unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn config(&self) -> PathBuf {
        self.root.path().join("gitconfig")
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(self.dir())
            .args(["-c", "user.name=Fixture Author"])
            .args(["-c", "user.email=author@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(["-c", "core.hooksPath=/dev/null"])
            .env("GIT_CONFIG_GLOBAL", self.config())
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.dir().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Commit everything staged or not, one tick after the previous commit.
    fn commit(&self, message: &str) -> String {
        let tick = self.clock.get() + 60;
        self.clock.set(tick);
        self.commit_at(message, tick, tick)
    }

    fn commit_at(&self, message: &str, author: i64, committer: i64) -> String {
        self.git(&["add", "-A"]);
        let status = self
            .command()
            .args(["commit", "-q", "--allow-empty", "-m", message])
            .env("GIT_AUTHOR_DATE", format!("{author} +0000"))
            .env("GIT_COMMITTER_DATE", format!("{committer} +0000"))
            .status()
            .unwrap();
        assert!(status.success());
        self.git(&["rev-parse", "HEAD"])
    }

    fn change(&self, name: &str, contents: &str, message: &str) -> String {
        self.write(name, contents);
        self.commit(message)
    }

    fn reader(&self) -> GitReader {
        GitReader::at(&self.dir())
            .unwrap()
            .expect("git is installed")
            .with_env("GIT_CONFIG_GLOBAL", self.config())
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
    }
}

fn shas(set: &CommitSet) -> Vec<&str> {
    set.commits
        .iter()
        .map(|commit| commit.sha.as_str())
        .collect()
}

fn digests(lines: &[&str]) -> Vec<u64> {
    lines.iter().filter_map(|line| line_digest(line)).collect()
}

fn path<'a>(commit: &'a Commit, name: &str) -> &'a ChangedPath {
    commit
        .paths
        .iter()
        .find(|path| path.path == name)
        .unwrap_or_else(|| panic!("{name} is not among {:?}", commit.paths))
}

#[test]
fn a_linear_range_lists_its_commits_oldest_first_with_identities_and_changes() {
    // Rejects: a range that includes its lower bound, lists newest first, drops
    // the times, or reports changes that are not the commit's own.
    let repo = Repo::new();
    let base = repo.change("src.rs", "fn one() {}\n", "base");
    let second = {
        repo.write("src.rs", "fn one() {}\nlet two = 2;\n");
        repo.commit_at("add two", 1_700_000_100, 1_700_000_200)
    };
    let third = repo.change("src.rs", "let two = 2;\nfn one() {}\n// note\n", "third");

    let set = repo
        .reader()
        .commits(&Selector::parse(&format!("{base}..{third}")))
        .unwrap();

    assert_eq!(shas(&set), [second.as_str(), third.as_str()]);
    let first = &set.commits[0];
    assert_eq!(first.parents, [base]);
    assert_eq!(first.subject, "add two");
    assert_eq!(first.author_time.timestamp(), 1_700_000_100);
    assert_eq!(first.committer_time.timestamp(), 1_700_000_200);
    assert!(!first.merge);
    assert!(first.patch_id.is_some());
    let changed = path(first, "src.rs");
    assert_eq!(changed.status, ChangeStatus::Modified);
    assert_eq!(changed.content, ContentKind::Text);
    assert_eq!(changed.added, digests(&["let two = 2;"]));
    assert!(changed.removed.is_empty());
    assert!(!set.commits_truncated && !set.metadata_truncated);
}

#[test]
fn a_symmetric_range_holds_both_sides_and_not_their_common_history() {
    // Rejects: `A...B` read as `A..B`, which would omit the left side's work,
    // or a set that includes the shared base.
    let repo = Repo::new();
    let base = repo.change("f", "base\n", "base");
    repo.git(&["checkout", "-q", "-b", "topic"]);
    let topic = repo.change("topic.txt", "topic\n", "topic work");
    repo.git(&["checkout", "-q", "main"]);
    let main = repo.change("main.txt", "main\n", "main work");

    let reader = repo.reader();
    let symmetric = reader.commits(&Selector::parse("main...topic")).unwrap();
    let mut got = shas(&symmetric);
    got.sort_unstable();
    let mut want = [topic.as_str(), main.as_str()];
    want.sort_unstable();
    assert_eq!(got, want);
    assert!(!shas(&symmetric).contains(&base.as_str()));

    let one_sided = reader.commits(&Selector::parse("main..topic")).unwrap();
    assert_eq!(shas(&one_sided), [topic.as_str()]);
}

/// A repository with `main` at the base and `topic` one commit ahead.
fn topic_repo() -> (Repo, String, String) {
    let repo = Repo::new();
    let base = repo.change("f", "base\n", "base");
    repo.git(&["checkout", "-q", "-b", "topic"]);
    let topic = repo.change("g", "topic\n", "topic work");
    (repo, base, topic)
}

#[test]
fn a_branch_is_measured_against_the_remote_head_before_anything_else() {
    // Rejects: a default branch taken from the local `main` or from
    // `init.defaultBranch` while `origin/HEAD` names another.
    let (repo, base, topic) = topic_repo();
    repo.git(&["checkout", "-q", "main"]);
    let ahead = repo.change("h", "ahead\n", "main moves on");
    // The remote's trunk sits at the base; local `main` and the configured
    // default both name branches that exist and must lose.
    repo.git(&["update-ref", "refs/remotes/origin/trunk", &base]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/trunk",
    ]);
    repo.git(&["config", "init.defaultBranch", "main"]);

    let set = repo
        .reader()
        .commits(&Selector::Branch("topic".into()))
        .unwrap();

    let default = set.default_branch.clone().unwrap();
    assert_eq!(default.name, "origin/trunk");
    assert_eq!(default.source, DefaultBranchSource::OriginHead);
    assert_eq!(shas(&set), [topic.as_str()]);
    assert_ne!(ahead, base);
}

#[test]
fn a_branch_is_measured_against_the_configured_default_when_it_exists() {
    // Rejects: `init.defaultBranch` honoured when no such branch exists, and
    // ignored in favour of `main` when it does.
    let (repo, base, topic) = topic_repo();
    repo.git(&["branch", "trunk", &base]);
    repo.git(&["config", "init.defaultBranch", "trunk"]);
    let reader = repo.reader();

    let set = reader.commits(&Selector::Branch("topic".into())).unwrap();
    let default = set.default_branch.clone().unwrap();
    assert_eq!(default.name, "trunk");
    assert_eq!(default.source, DefaultBranchSource::InitDefaultBranch);
    assert_eq!(shas(&set), [topic.as_str()]);

    repo.git(&["config", "init.defaultBranch", "absent"]);
    let fallback = reader.default_branch().unwrap().unwrap();
    assert_eq!(fallback.name, "main");
    assert_eq!(fallback.source, DefaultBranchSource::Conventional);
}

#[test]
fn a_branch_is_measured_against_main_or_master_by_convention() {
    // Rejects: a default branch that is absent when only `master` exists, or
    // `master` preferred over `main`.
    let (repo, base, topic) = topic_repo();
    repo.git(&["branch", "-m", "main", "master"]);
    let set = repo
        .reader()
        .commits(&Selector::Branch("topic".into()))
        .unwrap();
    let default = set.default_branch.clone().unwrap();
    assert_eq!(default.name, "master");
    assert_eq!(default.source, DefaultBranchSource::Conventional);
    assert_eq!(shas(&set), [topic.as_str()]);

    repo.git(&["branch", "main", &base]);
    let both = repo.reader().default_branch().unwrap().unwrap();
    assert_eq!(both.name, "main");

    repo.git(&["branch", "-D", "main", "master"]);
    assert!(repo.reader().default_branch().unwrap().is_none());
    let error = repo
        .reader()
        .commits(&Selector::Branch("topic".into()))
        .unwrap_err();
    assert!(error.to_string().contains("no default branch"), "{error}");
}

#[test]
fn a_rebased_commit_resolves_by_patch_id_while_the_old_object_exists() {
    // Rejects: a rewritten sha reported as unrelated work, and a patch id that
    // varies with the parent a commit was replayed onto.
    let (repo, _base, original) = topic_repo();
    repo.git(&["checkout", "-q", "main"]);
    repo.change("h", "main moved\n", "main moves");
    repo.git(&["checkout", "-q", "topic"]);
    repo.git(&["rebase", "-q", "main"]);
    let rebased = repo.git(&["rev-parse", "HEAD"]);
    assert_ne!(rebased, original);

    let reader = repo.reader();
    let set = reader.commits(&Selector::Branch("topic".into())).unwrap();
    assert_eq!(shas(&set), [rebased.as_str()]);

    match reader.resolve(&set, &original).unwrap() {
        Resolution::PatchEquivalent {
            patch_id,
            candidates,
        } => {
            assert_eq!(candidates, std::slice::from_ref(&rebased));
            assert_eq!(Some(patch_id), set.commits[0].patch_id);
        }
        other => panic!("expected a patch-id match, got {other:?}"),
    }
    assert_eq!(
        reader.resolve(&set, &rebased).unwrap(),
        Resolution::Exact { sha: rebased },
    );
}

#[test]
fn an_abbreviated_sha_resolves_exactly() {
    // Rejects: an abbreviation that falls through to patch-id matching or
    // fails to resolve.
    let (repo, _base, topic) = topic_repo();
    let set = repo
        .reader()
        .commits(&Selector::Branch("topic".into()))
        .unwrap();
    for length in [7, 12] {
        assert_eq!(
            repo.reader().resolve(&set, &topic[..length]).unwrap(),
            Resolution::Exact { sha: topic.clone() },
        );
    }
}

#[test]
fn an_amended_commit_resolves_only_when_its_change_is_unchanged() {
    // Rejects: a message-only amend taken for different work, and a content
    // amend taken for the same change.
    let (repo, _base, original) = topic_repo();
    repo.git(&["commit", "-q", "--amend", "-m", "reworded"]);
    let reworded = repo.git(&["rev-parse", "HEAD"]);
    let reader = repo.reader();
    let set = reader.commits(&Selector::Branch("topic".into())).unwrap();
    assert_eq!(shas(&set), [reworded.as_str()]);
    assert!(matches!(
        reader.resolve(&set, &original).unwrap(),
        Resolution::PatchEquivalent { .. }
    ));

    repo.write("g", "topic\nextra line\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "--amend", "--no-edit"]);
    let extended = repo.git(&["rev-parse", "HEAD"]);
    let set = reader.commits(&Selector::Branch("topic".into())).unwrap();
    assert_eq!(shas(&set), [extended.as_str()]);
    assert_eq!(
        reader.resolve(&set, &reworded).unwrap(),
        Resolution::Unresolved {
            reason: Unresolved::NoEquivalent
        },
    );
    // The content that survived the amend is still recognisable by digest.
    let kept = path(&set.commits[0], "g");
    assert!(kept.added.contains(&line_digest("topic").unwrap()));
    assert!(kept.added.contains(&line_digest("extra line").unwrap()));
}

#[test]
fn a_squash_commit_carries_the_added_lines_of_the_commits_it_squashed() {
    // Rejects: a squash commit that reports no changes because it has one
    // parent, or only the last squashed commit's lines.
    let repo = Repo::new();
    repo.change("f", "base\n", "base");
    repo.git(&["checkout", "-q", "-b", "topic"]);
    let first = repo.change("a.txt", "alpha\nbeta\n", "first");
    let second = repo.change("b.txt", "gamma\n", "second");
    let third = repo.change("a.txt", "alpha\nbeta\ndelta\n", "third");
    repo.git(&["checkout", "-q", "main"]);
    repo.git(&["merge", "-q", "--squash", "topic"]);
    let squash = repo.commit("squash topic");

    let reader = repo.reader();
    let squashed = reader.commit(&squash).unwrap();
    assert!(!squashed.merge);
    let squashed_lines: Vec<u64> = squashed
        .paths
        .iter()
        .flat_map(|path| path.added.iter().copied())
        .collect();
    let squashed_commits = reader
        .commits(&Selector::List(vec![first, second, third]))
        .unwrap();
    assert_eq!(squashed_commits.commits.len(), 3);
    let net: Vec<u64> = digests(&["alpha", "beta", "gamma", "delta"]);
    for digest in &net {
        assert!(squashed_lines.contains(digest), "{digest} is missing");
    }
    // Every line a squashed commit added that survives to the end is in the
    // squash; `beta` is added once in history and once in the squash.
    for commit in &squashed_commits.commits {
        for changed in &commit.paths {
            for digest in &changed.added {
                assert!(squashed_lines.contains(digest));
            }
        }
    }
}

#[test]
fn a_merge_commit_has_parents_and_no_changes_of_its_own() {
    // Rejects: a merge's diff against its first parent counted on top of the
    // merged commits' own lines, a merge given a patch id, and a merge that
    // resolves by anything but its sha.
    let repo = Repo::new();
    let base = repo.change("f", "base\n", "base");
    repo.git(&["checkout", "-q", "-b", "topic"]);
    let topic = repo.change("topic.txt", "from topic\n", "topic work");
    repo.git(&["checkout", "-q", "main"]);
    repo.change("main.txt", "from main\n", "main work");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge topic", "topic"]);
    let merge = repo.git(&["rev-parse", "HEAD"]);

    let reader = repo.reader();
    let set = reader
        .commits(&Selector::parse(&format!("{base}..main")))
        .unwrap();
    let merged = set.commits.iter().find(|c| c.sha == merge).unwrap();
    assert!(merged.merge);
    assert_eq!(merged.parents.len(), 2);
    assert!(merged.paths.is_empty());
    assert_eq!(merged.patch_id, None);
    assert!(!merged.diff_truncated);
    // The merged commit's own lines are counted once, on that commit.
    let own = set.commits.iter().find(|c| c.sha == topic).unwrap();
    assert_eq!(path(own, "topic.txt").added, digests(&["from topic"]));
    let counted: usize = set
        .commits
        .iter()
        .flat_map(|c| &c.paths)
        .map(|p| p.added.len())
        .sum();
    assert_eq!(counted, 2);

    assert_eq!(
        reader.resolve(&set, &merge).unwrap(),
        Resolution::Exact { sha: merge.clone() },
    );
    let outside = reader.commits(&Selector::Commits(vec![topic])).unwrap();
    assert_eq!(
        reader.resolve(&outside, &merge).unwrap(),
        Resolution::Unresolved {
            reason: Unresolved::NoPatchId
        },
    );
}

#[test]
fn an_unknown_revision_refuses_with_gits_own_message() {
    // Rejects: an empty set for a revision that does not exist, a rewritten
    // message, and a revision that git would read as an option.
    let repo = Repo::new();
    repo.change("f", "base\n", "base");
    let reader = repo.reader();

    for selector in [
        Selector::parse("no-such-rev"),
        Selector::parse("main..no-such-rev"),
        Selector::parse("main...no-such-rev"),
        Selector::List(vec!["no-such-rev".into()]),
    ] {
        let error = reader.commits(&selector).unwrap_err().to_string();
        assert!(
            error.starts_with("fatal: ") && error.contains("no-such-rev"),
            "{selector:?}: {error}"
        );
    }
    assert!(reader.commits(&Selector::Commits(vec![])).is_err());
    let error = reader
        .commits(&Selector::Commits(vec!["--output=/dev/null".into()]))
        .unwrap_err();
    assert!(
        error.to_string().contains("not a single revision"),
        "{error}"
    );
    assert!(reader.commit("no-such-rev").is_err());
}

#[test]
fn output_past_the_byte_cap_is_cut_and_reported() {
    // Rejects: a large diff read whole, a silent cut, a patch id computed from
    // a cut diff, and digests presented as complete after the cut.
    let repo = Repo::new();
    repo.change("f", "base\n", "base");
    let body: String = (0..400).map(|n| format!("line number {n}\n")).collect();
    let big = repo.change("big.txt", &body, "a large file");
    let small = repo.change("small.txt", "tiny line\n", "a small file");

    let capped = repo.reader().with_limits(Limits {
        output_bytes: 1_500,
        ..Limits::default()
    });
    let commit = capped.commit(&big).unwrap();
    assert!(commit.diff_truncated);
    assert_eq!(commit.patch_id, None);
    let cut = path(&commit, "big.txt");
    assert!(cut.digests_truncated);
    assert!(cut.added.len() < 400);
    assert!(
        !cut.added.is_empty(),
        "the part of the diff inside the cap is kept"
    );

    // A commit inside the cap is untouched by the cap.
    let whole = capped.commit(&small).unwrap();
    assert!(!whole.diff_truncated);
    assert!(whole.patch_id.is_some());
    assert!(!path(&whole, "small.txt").digests_truncated);

    // The cap applies to the commit metadata as well.
    let tiny = repo.reader().with_limits(Limits {
        output_bytes: 200,
        ..Limits::default()
    });
    let set = tiny.commits(&Selector::parse("main~2..main")).unwrap();
    assert!(set.metadata_truncated);
    assert!(set.commits.len() < 2);
}

/// Git's own `patch-id --stable` of one commit's diff, read alone.
fn own_patch_id(repo: &Repo, sha: &str) -> Option<String> {
    let diff = repo
        .command()
        .args(["diff-tree", "-p", "-M", "-r", "--root", sha])
        .output()
        .unwrap();
    assert!(diff.status.success());
    let mut patch_id = repo
        .command()
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    patch_id
        .stdin
        .take()
        .unwrap()
        .write_all(&diff.stdout)
        .unwrap();
    let output = patch_id.wait_with_output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

#[test]
fn a_set_reads_each_commit_as_a_read_of_that_commit_alone_does() {
    // Rejects: a set whose commits differ from the same commits read one at
    // a time: changes lost after a diff cut at the cap, changes shifted past
    // an empty commit, and patch ids that are not git's own.
    let repo = Repo::new();
    let base = repo.change("f0.txt", "base\n", "base");
    let big: String = (0..6_000).map(|n| format!("line number {n}\n")).collect();
    for n in 1..=100 {
        match n % 10 {
            3 => repo.write(&format!("big{n}.txt"), &big),
            6 => {}
            9 => fs::rename(
                repo.dir().join(format!("f{}.txt", n - 1)),
                repo.dir().join(format!("moved{n}.txt")),
            )
            .unwrap(),
            _ => repo.write(&format!("f{n}.txt"), &format!("line {n}\n")),
        }
        repo.commit(&format!("commit {n}"));
    }

    let reader = repo.reader().with_limits(Limits {
        output_bytes: 64 * 1024,
        ..Limits::default()
    });
    let set = reader
        .commits(&Selector::parse(&format!("{base}..main")))
        .unwrap();
    assert!(!set.metadata_truncated && !set.commits_truncated);
    assert_eq!(set.commits.len(), 100);
    for commit in &set.commits {
        assert_eq!(commit, &reader.commit(&commit.sha).unwrap());
        if !commit.diff_truncated {
            assert_eq!(commit.patch_id, own_patch_id(&repo, &commit.sha));
        }
    }
    let cut = set.commits.iter().filter(|commit| commit.diff_truncated);
    assert_eq!(cut.count(), 10, "each large diff passes the cap");
    let empty = set.commits.iter().filter(|commit| commit.paths.is_empty());
    assert_eq!(empty.count(), 10, "each empty commit changes nothing");
}

#[test]
fn a_read_ahead_spares_the_later_read_its_diffs_under_the_same_limits() {
    // Rejects: a later read that diffs again what was read ahead of it, one
    // that differs from a read with nothing read ahead, a stopped read ahead
    // that still diffs, and changes read under one cap handed to a reader
    // under another.
    let repo = Repo::new();
    repo.change("f", "base\n", "base");
    let body: String = (0..400).map(|n| format!("line number {n}\n")).collect();
    repo.change("big.txt", &body, "a large file");
    repo.change("small.txt", "tiny line\n", "a small file");
    let from = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    let to = from + chrono::Duration::days(1);

    let log = repo.root.path().join("git-commands.log");
    let program = repo.root.path().join("git-logged");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec git \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let diffs = || {
        fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("diff-tree"))
            .count()
    };
    let reader = GitReader::with_program(program.as_os_str(), &repo.dir())
        .unwrap()
        .expect("git is installed")
        .with_env("GIT_CONFIG_GLOBAL", repo.config())
        .with_env("GIT_CONFIG_NOSYSTEM", "1");
    let alone = repo.reader().commits_between(from, to).unwrap().commits;
    assert_eq!(alone.len(), 3);

    reader.read_ahead_between(from, to, &AtomicBool::new(true));
    assert_eq!(diffs(), 0, "a stopped read ahead begins no diff");

    reader.read_ahead_between(from, to, &AtomicBool::new(false));
    let ahead = diffs();
    assert!(ahead > 0);
    assert_eq!(reader.commits_between(from, to).unwrap().commits, alone);
    assert_eq!(diffs(), ahead, "the read takes what was read ahead");

    let capped = Limits {
        output_bytes: 1_500,
        ..Limits::default()
    };
    reader.read_ahead_between(from, to, &AtomicBool::new(false));
    let cut = reader
        .clone()
        .with_limits(capped)
        .commits_between(from, to)
        .unwrap();
    assert!(!cut.metadata_truncated);
    assert!(cut.commits.iter().any(|commit| commit.diff_truncated));
    assert_eq!(
        cut.commits,
        repo.reader()
            .with_limits(capped)
            .commits_between(from, to)
            .unwrap()
            .commits
    );
}

#[test]
fn digest_and_commit_caps_report_what_they_cut() {
    // Rejects: more digests than the per-path or per-commit cap, and a cap hit
    // that leaves no mark.
    let repo = Repo::new();
    repo.change("f", "base\n", "base");
    let many = |tag: &str| -> String { (0..10).map(|n| format!("{tag} {n}\n")).collect() };
    repo.write("a.txt", &many("alpha"));
    repo.write("b.txt", &many("beta"));
    let wide = repo.commit("two wide files");
    let extra = repo.change("c.txt", "one\n", "a third");

    let per_path = repo.reader().with_limits(Limits {
        digests_per_path: 4,
        ..Limits::default()
    });
    let commit = per_path.commit(&wide).unwrap();
    for name in ["a.txt", "b.txt"] {
        let cut = path(&commit, name);
        assert_eq!(cut.added.len(), 4);
        assert!(cut.digests_truncated);
    }

    let per_commit = repo.reader().with_limits(Limits {
        digests_per_commit: 12,
        ..Limits::default()
    });
    let commit = per_commit.commit(&wide).unwrap();
    let kept: usize = commit.paths.iter().map(|p| p.added.len()).sum();
    assert_eq!(kept, 12);
    assert!(commit.digests_truncated);
    assert!(commit.paths.iter().any(|p| p.digests_truncated));

    // The newest commits are kept, however the set was named.
    let newest = repo.reader().with_limits(Limits {
        commits: 1,
        ..Limits::default()
    });
    let named = [
        Selector::parse("main~2..main"),
        Selector::Commits(vec![wide.clone(), extra.clone()]),
    ];
    for selector in named {
        let set = newest.commits(&selector).unwrap();
        assert!(set.commits_truncated, "{selector:?}");
        assert_eq!(shas(&set), [extra.as_str()], "{selector:?}");
    }
    let listed = newest
        .commits(&Selector::List(vec![wide.clone(), extra.clone()]))
        .unwrap();
    assert!(listed.commits_truncated);
    assert_eq!(shas(&listed), [wide.as_str()]);
}

#[test]
fn line_digests_ignore_trailing_whitespace_blank_and_punctuation_only_lines() {
    // Rejects: digests that differ by trailing whitespace or line ending,
    // blank or brace-only lines that count as content, a different hash than
    // SHA-256's first eight bytes, and leading indentation discarded.
    assert_eq!(line_digest("foo"), Some(0x2c26_b46b_68ff_c68f));
    assert_eq!(line_digest("foo  \t"), line_digest("foo"));
    assert_eq!(line_digest("foo\r"), line_digest("foo"));
    assert_ne!(line_digest("  foo"), line_digest("foo"));
    for skipped in ["", "   ", "\t", "}", "  });  ", "{", "//", "---", "*/ ;"] {
        assert_eq!(line_digest(skipped), None, "{skipped:?}");
    }
    assert!(line_digest("}x").is_some());
    assert!(line_digest("é").is_some());
}

#[test]
fn the_same_line_from_a_commit_and_from_a_recording_has_one_digest() {
    // Rejects: a diff-side digest that differs from the public function, which
    // would keep recorded edits from ever matching committed ones.
    let repo = Repo::new();
    repo.change("f", "base\n", "base");
    let contents = "    let value = compute(1);   \r\n\r\n}\nfn tail() {}\n";
    let commit = repo.change("code.rs", contents, "add code");
    let set = repo
        .reader()
        .commits(&Selector::Commits(vec![commit]))
        .unwrap();
    let added = &path(&set.commits[0], "code.rs").added;
    // The recording supplies the line without the diff's markers or the
    // file's line ending.
    assert_eq!(
        added,
        &digests(&["    let value = compute(1);", "fn tail() {}"])
    );
    assert_eq!(added.len(), 2);
}

#[test]
fn trailers_keep_co_authors_and_role_hints_and_drop_the_rest() {
    // Rejects: attribution hints lost, trailer keys matched case-sensitively,
    // and unrelated trailers or body prose kept.
    let repo = Repo::new();
    repo.change("f", "base\n", "base");
    repo.write("f", "changed\n");
    let message = "feat: change f\n\nBody text mentions Planned-by: nobody.\n\n\
        Co-Authored-By: Pair Person <pair@example.invalid>\n\
        planned-by: harness-a:model-a#high\n\
        Implemented-by: harness-b:model-b\n\
        Reviewed-by: harness-c:model-c#max\n\
        Orchestrated-by: harness-a:model-a#max\n\
        Assisted-by: harness-d:model-d (scout)\n\
        Signed-off-by: Someone <someone@example.invalid>\n";
    let sha = repo.commit(message);
    let commit = repo.reader().commit(&sha).unwrap();

    assert_eq!(commit.subject, "feat: change f");
    assert_eq!(
        commit.trailers.co_authored_by,
        ["Pair Person <pair@example.invalid>"]
    );
    let roles: Vec<(Role, &str)> = commit
        .trailers
        .roles
        .iter()
        .map(|trailer| (trailer.role, trailer.value.as_str()))
        .collect();
    assert_eq!(
        roles,
        [
            (Role::PlannedBy, "harness-a:model-a#high"),
            (Role::ImplementedBy, "harness-b:model-b"),
            (Role::ReviewedBy, "harness-c:model-c#max"),
            (Role::OrchestratedBy, "harness-a:model-a#max"),
            (Role::AssistedBy, "harness-d:model-d (scout)"),
        ]
    );
}

#[test]
fn renames_binaries_and_odd_paths_are_reported_without_invented_lines() {
    // Rejects: a pure rename reported as every line removed and re-added, a
    // binary file digested as text, and a quoted path left quoted.
    let repo = Repo::new();
    repo.write("old name.txt", "kept line\nanother kept line\n");
    repo.write("quote\"d.txt", "quoted file line\n");
    fs::write(repo.dir().join("blob.bin"), [0_u8, 159, 146, 150, 0, 1, 2]).unwrap();
    repo.commit("add files");
    repo.git(&["mv", "old name.txt", "new name.txt"]);
    fs::write(repo.dir().join("blob.bin"), [0_u8, 1, 2, 3, 0, 9]).unwrap();
    repo.write("quote\"d.txt", "quoted file line\nsecond quoted line\n");
    let sha = repo.commit("rename and edit");

    let commit = repo.reader().commit(&sha).unwrap();
    let renamed = path(&commit, "new name.txt");
    assert_eq!(renamed.status, ChangeStatus::Renamed);
    assert_eq!(renamed.previous_path.as_deref(), Some("old name.txt"));
    assert!(renamed.added.is_empty() && renamed.removed.is_empty());
    let binary = path(&commit, "blob.bin");
    assert_eq!(binary.content, ContentKind::Binary);
    assert!(binary.added.is_empty());
    let quoted = path(&commit, "quote\"d.txt");
    assert_eq!(quoted.added, digests(&["second quoted line"]));
}

#[test]
fn a_commit_list_keeps_its_order_and_names_resolve_to_commits() {
    // Rejects: a list re-sorted into history order and revision names that are
    // not resolved.
    let repo = Repo::new();
    repo.change("f", "one\n", "one");
    let two = repo.change("f", "two\n", "two");
    let three = repo.change("f", "three\n", "three");
    let reader = repo.reader();

    let listed = reader
        .commits(&Selector::List(vec![three.clone(), two.clone()]))
        .unwrap();
    assert_eq!(shas(&listed), [three.as_str(), two.as_str()]);

    let named = reader
        .commits(&Selector::Commits(vec!["HEAD".into(), "HEAD~1".into()]))
        .unwrap();
    assert_eq!(shas(&named), [two.as_str(), three.as_str()]);
}

#[test]
fn an_observed_object_the_repository_no_longer_holds_is_unresolved_as_missing() {
    // Rejects: a pruned sha reported as having no equivalent change, which
    // would say the work is elsewhere when it can only not be compared, and a
    // malformed sha that errors.
    let (repo, _base, original) = topic_repo();
    repo.git(&["checkout", "-q", "main"]);
    repo.change("h", "main moved\n", "main moves");
    repo.git(&["checkout", "-q", "topic"]);
    repo.git(&["rebase", "-q", "main"]);
    let reader = repo.reader();
    let set = reader.commits(&Selector::Branch("topic".into())).unwrap();

    repo.git(&["reflog", "expire", "--expire=now", "--all"]);
    repo.git(&["gc", "-q", "--prune=now"]);
    let missing = Resolution::Unresolved {
        reason: Unresolved::Missing,
    };
    assert_eq!(reader.resolve(&set, &original).unwrap(), missing);
    assert_eq!(reader.resolve(&set, "not a sha").unwrap(), missing);
    assert_eq!(
        reader.resolve(&set, &"0123456789".repeat(4)).unwrap(),
        missing
    );
}

#[test]
fn a_missing_git_makes_the_reader_unavailable_without_an_error() {
    // Rejects: a machine without git failing a read instead of reporting the
    // reader unavailable.
    let repo = Repo::new();
    let absent = GitReader::with_program(OsStr::new("tapes-test-no-such-git"), &repo.dir());
    assert!(absent.unwrap().is_none());
}

#[test]
fn reading_leaves_the_repository_untouched() {
    // Rejects: a reader that takes locks, writes refs, or refreshes the index.
    let (repo, _base, _topic) = topic_repo();
    let snapshot = |repo: &Repo| -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    walk(&entry.path(), out);
                } else {
                    out.push((entry.path(), meta.len(), meta.modified().unwrap()));
                }
            }
        }
        let mut files = Vec::new();
        walk(&repo.dir().join(".git"), &mut files);
        files.sort();
        files
    };
    let before = snapshot(&repo);
    let reader = repo.reader();
    let set = reader.commits(&Selector::Branch("topic".into())).unwrap();
    reader.resolve(&set, "0123456789abcdef").unwrap();
    assert_eq!(snapshot(&repo), before);
}
