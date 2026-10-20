//! `tapes usage --pr N` prices a pull request: the commits its merge method
//! selects, read from the forge's `gh pr view` and the local repository,
//! priced from the recordings of the sessions that did the work. Each test
//! drives the binary over a synthetic repository, the stores of three
//! harnesses and a fixture `gh` on `PATH` that prints canned JSON and
//! records its arguments, then names the caller-visible claim it holds and
//! the wrong behaviour it rejects.

#[path = "../../tapes-core/tests/support/recordings.rs"]
mod recordings;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use chrono::Duration;
use serde_json::{json, Value};

use recordings::*;

const SQUASHED: &str = "c1a00000-0000-4000-8000-0000000000b1";
const REBASED: &str = "c1a00000-0000-4000-8000-0000000000b2";
const MERGED: &str = "c1a00000-0000-4000-8000-0000000000b3";
const OPENED: &str = "c1a00000-0000-4000-8000-0000000000b4";

/// The fixture `gh`: it records its arguments and prints the canned JSON of
/// the pull request it was asked for, or fails with the message in
/// `gh.fail`.
const GH: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FIXTURE_DIR/gh.calls"
if [ -e "$FIXTURE_DIR/gh.fail" ]; then
  cat "$FIXTURE_DIR/gh.fail" >&2
  exit 1
fi
exec cat "$FIXTURE_DIR/pr-$3.json"
"#;

/// A `git` that records its arguments, then runs the real one.
const GIT: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FIXTURE_DIR/git.calls"
exec /usr/bin/git "$@"
"#;

/// An instant `days` after the fixture day.
fn day(days: i64, time: &str) -> Time {
    at(time) + Duration::days(days)
}

fn refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// An instant as the JSON views write it.
fn rfc(time: Time) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A synthetic repository and its forge: the fixture `gh`, a recording
/// `git`, and the commits the scenario made by the name expectations use.
struct Scene {
    sandbox: Sandbox,
    names: BTreeMap<&'static str, String>,
    forge: PathBuf,
}

impl Scene {
    fn new(sandbox: Sandbox, names: BTreeMap<&'static str, String>) -> Self {
        let forge = sandbox.dir("forge");
        for (directory, gh) in [("bin", true), ("bin-without-gh", false)] {
            let bin = forge.join(directory);
            fs::create_dir_all(&bin).unwrap();
            executable(&bin.join("git"), GIT);
            if gh {
                executable(&bin.join("gh"), GH);
            }
        }
        Self {
            sandbox,
            names,
            forge,
        }
    }

    fn sha(&self, name: &str) -> &str {
        &self.names[name]
    }

    fn name(&self, sha: &str) -> &str {
        self.names
            .iter()
            .find(|(_, full)| full.as_str() == sha)
            .map(|(name, _)| *name)
            .unwrap_or_else(|| panic!("no commit named {sha}"))
    }

    /// What `gh pr view N` prints.
    fn pull_request(&self, number: u64, canned: Value) {
        fs::write(
            self.forge.join(format!("pr-{number}.json")),
            canned.to_string(),
        )
        .unwrap();
    }

    fn calls(&self, program: &str) -> Vec<String> {
        fs::read_to_string(self.forge.join(format!("{program}.calls")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A `PATH` on which the fixture `gh` comes first.
    fn with_gh(&self) -> String {
        format!("{}:/usr/bin:/bin", self.forge.join("bin").display())
    }

    /// A `PATH` holding the recording `git` alone, so `gh` is not
    /// installed.
    fn without_gh(&self) -> String {
        self.forge.join("bin-without-gh").display().to_string()
    }

    fn tapes(&self, args: &[&str]) -> Output {
        self.tapes_on(&self.with_gh(), args)
    }

    /// `tapes usage ARGS` with `path` as `PATH`, reading only the
    /// sandbox's stores.
    fn tapes_on(&self, path: &str, args: &[&str]) -> Output {
        let sandbox = &self.sandbox;
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env("TAPES_OFFLINE", "1")
            .current_dir(sandbox.repo())
            .env_clear()
            .env("HOME", sandbox.dir("home"))
            .env("PATH", path)
            .env("FIXTURE_DIR", &self.forge)
            .env("CLAUDE_CONFIG_DIR", sandbox.dir("claude"))
            .env("CODEX_HOME", sandbox.dir("codex"))
            .env("PI_CODING_AGENT_DIR", sandbox.dir("pi"))
            .env("XDG_CONFIG_HOME", sandbox.dir("xdg"))
            .env("XDG_DATA_HOME", sandbox.dir("xdg-data"))
            .env("GIT_CONFIG_GLOBAL", sandbox.dir("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("usage")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.push("--json");
        args.push("--detail");
        let output = self.tapes(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refusal(&self, path: &str, args: &[&str]) -> String {
        let output = self.tapes_on(path, args);
        assert!(
            !output.status.success(),
            "{args:?} answered: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// The commits a view selected, by name.
    fn selected(&self, view: &Value) -> Vec<&str> {
        view["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|commit| self.name(commit["sha"].as_str().unwrap()))
            .collect()
    }

    /// Requests each commit's rows hold, by name.
    fn requests(&self, view: &Value) -> BTreeMap<&str, u64> {
        let mut by_commit = BTreeMap::new();
        for row in view["rows"].as_array().unwrap() {
            let sha = row["key"]["commit"].as_str().unwrap();
            *by_commit.entry(self.name(sha)).or_default() += row["requests"].as_u64().unwrap();
        }
        by_commit
    }

    /// The tier and basis pairs the rows of a commit rest on.
    fn labels(&self, view: &Value, name: &str) -> BTreeSet<(String, String)> {
        view["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["key"]["commit"] == self.sha(name))
            .flat_map(|row| row["evidence"].as_array().unwrap().clone())
            .map(|label| {
                (
                    label["tier"].as_str().unwrap().to_owned(),
                    label["basis"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// The pull request `gh` would print, merged by `how`.
    fn canned(&self, number: u64, state: &str, listed: &[(&str, &str)], rest: Value) -> Value {
        let mut canned = json!({
            "number": number,
            "state": state,
            "commits": listed
                .iter()
                .map(|(oid, subject)| json!({"oid": oid, "messageHeadline": subject}))
                .collect::<Vec<_>>(),
            "baseRefName": "main",
            "headRefName": "feature",
            "mergeCommit": null,
            "createdAt": rfc(day(1, "09:30:00")),
            "mergedAt": null,
            "closedAt": null,
            "title": "Add the feature",
        });
        for (key, value) in rest.as_object().unwrap() {
            canned[key] = value.clone();
        }
        canned
    }
}

/// Make the commits unreachable, then drop them from the object store, as
/// a deleted branch and a garbage collection leave a repository.
fn forget(sandbox: &Sandbox) {
    let repo = sandbox.repo();
    sandbox.git(&repo, None, &["reflog", "expire", "--expire=now", "--all"]);
    sandbox.git(&repo, None, &["gc", "-q", "--prune=now"]);
}

fn holds(sandbox: &Sandbox, sha: &str) -> bool {
    sandbox
        .command(&sandbox.repo())
        .args(["cat-file", "-e", sha])
        .output()
        .unwrap()
        .status
        .success()
}

/// A session works on `feature` in the main checkout on day 1 and commits
/// f1 and f2; five days later `feature` is squash-merged into `main` as one
/// commit, `S`, and the branch and its commits are deleted.
fn squashed() -> Scene {
    squashed_keeping(false)
}

/// As [`squashed`], with the branch's commits left in the object store when
/// `keep` is set, as before a garbage collection.
fn squashed_keeping(keep: bool) -> Scene {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let f1_lines = lines("alpha");
    let f2_lines = lines("beta");
    sandbox.git(&repo, None, &["checkout", "-q", "-b", "feature"]);
    let f1 = sandbox.commit(
        &repo,
        day(1, "10:10:00"),
        "feature.rs",
        &refs(&f1_lines),
        "f1",
    );
    let f2 = sandbox.commit(
        &repo,
        day(1, "10:25:00"),
        "feature.rs",
        &refs(&f2_lines),
        "f2",
    );
    sandbox.git(&repo, None, &["checkout", "-q", "main"]);
    sandbox.git(&repo, None, &["merge", "-q", "--squash", "feature"]);
    sandbox.git(
        &repo,
        Some(day(6, "12:00:00")),
        &["commit", "-q", "-m", "Add the feature (#7)"],
    );
    let squash = sandbox.git(&repo, None, &["rev-parse", "HEAD"]);
    sandbox.git(&repo, None, &["branch", "-q", "-D", "feature"]);
    if !keep {
        forget(&sandbox);
    }

    let mut session = Claude::lead(SQUASHED, &repo, "feature");
    session.prompt(day(1, "10:00:00"));
    session.reply(day(1, "10:00:05"));
    session.edit(day(1, "10:05:00"), &repo.join("feature.rs"), &f1_lines);
    session.commit(day(1, "10:10:00"), &f1, "committed", "f1");
    session.edit(day(1, "10:20:00"), &repo.join("feature.rs"), &f2_lines);
    session.commit(day(1, "10:25:00"), &f2, "committed", "f2");
    session.reply(day(1, "10:26:00"));
    session.write(&sandbox, &repo);

    let scene = Scene::new(
        sandbox,
        BTreeMap::from([("f1", f1), ("f2", f2), ("S", squash)]),
    );
    scene.pull_request(
        7,
        scene.canned(
            7,
            "MERGED",
            &[(scene.sha("f1"), "f1"), (scene.sha("f2"), "f2")],
            json!({
                "mergeCommit": {"oid": scene.sha("S")},
                "mergedAt": rfc(day(6, "12:00:00")),
                "closedAt": rfc(day(6, "12:00:00")),
            }),
        ),
    );
    scene
}

/// A Claude delegate in worktree `wt-m` commits m1 and m2 on `feat-m`; the
/// branch is merged without fast-forward as `M` and removed.
fn merged() -> Scene {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let wt = sandbox.worktree("wt-m", "feat-m");
    let m1_lines = lines("gamma");
    let m2_lines = lines("delta");
    let m1 = sandbox.commit(&wt, at("10:10:00"), "m.rs", &refs(&m1_lines), "m1");
    let m2 = sandbox.commit(&wt, at("10:25:00"), "m.rs", &refs(&m2_lines), "m2");
    sandbox.git(
        &repo,
        Some(at("11:00:00")),
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge pull request #5 from org/feat-m",
            "feat-m",
        ],
    );
    let merge = sandbox.git(&repo, None, &["rev-parse", "HEAD"]);
    sandbox.retire(&wt, "feat-m");

    let mut delegate = Claude::lead(MERGED, &wt, "feat-m");
    delegate.prompt(at("10:00:00"));
    delegate.reply(at("10:00:05"));
    delegate.edit(at("10:05:00"), &wt.join("m.rs"), &m1_lines);
    delegate.commit(at("10:10:00"), &m1, "committed", "m1");
    delegate.edit(at("10:20:00"), &wt.join("m.rs"), &m2_lines);
    delegate.commit(at("10:25:00"), &m2, "committed", "m2");
    delegate.reply(at("10:26:00"));
    delegate.write(&sandbox, &wt);

    let scene = Scene::new(
        sandbox,
        BTreeMap::from([("m1", m1), ("m2", m2), ("M", merge)]),
    );
    scene.pull_request(
        5,
        scene.canned(
            5,
            "MERGED",
            &[(scene.sha("m1"), "m1"), (scene.sha("m2"), "m2")],
            json!({
                "headRefName": "feat-m",
                "createdAt": rfc(at("09:30:00")),
                "mergeCommit": {"oid": scene.sha("M")},
                "mergedAt": rfc(at("11:00:00")),
                "closedAt": rfc(at("11:00:00")),
            }),
        ),
    );
    scene
}

/// A session commits r1 and r2 on `feat-r` in the main checkout; `main`
/// moves on, and the forge rebase-merges the branch: `main` gains r1' and
/// r2', and the original commits are deleted.
fn rebased() -> Scene {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let r1_lines = lines("epsilon");
    let r2_lines = lines("zeta");
    sandbox.git(&repo, None, &["checkout", "-q", "-b", "feat-r"]);
    let r1 = sandbox.commit(&repo, at("10:10:00"), "r.rs", &refs(&r1_lines), "r1");
    let r2 = sandbox.commit(&repo, at("10:25:00"), "r.rs", &refs(&r2_lines), "r2");
    sandbox.git(&repo, None, &["checkout", "-q", "main"]);
    sandbox.commit(
        &repo,
        at("11:00:00"),
        "other.rs",
        &refs(&lines("eta")),
        "other",
    );
    sandbox.git(&repo, None, &["checkout", "-q", "feat-r"]);
    sandbox.rewrite(&repo, at("12:00:00"), &["rebase", "-q", "main"]);
    sandbox.git(&repo, None, &["checkout", "-q", "main"]);
    sandbox.git(&repo, None, &["merge", "-q", "--ff-only", "feat-r"]);
    let r1_rebased = sandbox.git(&repo, None, &["rev-parse", "main~1"]);
    let r2_rebased = sandbox.git(&repo, None, &["rev-parse", "main"]);
    sandbox.git(&repo, None, &["branch", "-q", "-D", "feat-r"]);
    forget(&sandbox);

    let mut session = Claude::lead(REBASED, &repo, "feat-r");
    session.prompt(at("10:00:00"));
    session.reply(at("10:00:05"));
    session.edit(at("10:05:00"), &repo.join("r.rs"), &r1_lines);
    session.commit(at("10:10:00"), &r1, "committed", "r1");
    session.edit(at("10:20:00"), &repo.join("r.rs"), &r2_lines);
    session.commit(at("10:25:00"), &r2, "committed", "r2");
    session.reply(at("10:26:00"));
    session.write(&sandbox, &repo);

    let scene = Scene::new(
        sandbox,
        BTreeMap::from([
            ("r1", r1.clone()),
            ("r2", r2.clone()),
            ("r1'", r1_rebased),
            ("r2'", r2_rebased),
        ]),
    );
    scene.pull_request(
        9,
        scene.canned(
            9,
            "MERGED",
            &[(&r1, "r1"), (&r2, "r2")],
            json!({
                "headRefName": "feat-r",
                "createdAt": rfc(at("09:30:00")),
                "mergeCommit": {"oid": scene.sha("r2'")},
                "mergedAt": rfc(at("12:00:00")),
                "closedAt": rfc(at("12:00:00")),
            }),
        ),
    );
    scene
}

/// A delegate commits o1 on the live branch `feat-o` in worktree `wt-o`.
/// The forge lists o1 and a second commit this repository does not hold.
fn open() -> Scene {
    let sandbox = Sandbox::new();
    let wt = sandbox.worktree("wt-o", "feat-o");
    let o1_lines = lines("theta");
    let o1 = sandbox.commit(&wt, at("10:10:00"), "o.rs", &refs(&o1_lines), "o1");

    let mut delegate = Claude::lead(OPENED, &wt, "feat-o");
    delegate.prompt(at("10:00:00"));
    delegate.reply(at("10:00:05"));
    delegate.edit(at("10:05:00"), &wt.join("o.rs"), &o1_lines);
    delegate.commit(at("10:10:00"), &o1, "committed", "o1");
    delegate.reply(at("10:11:00"));
    delegate.write(&sandbox, &wt);

    let unfetched = "d".repeat(40);
    let scene = Scene::new(
        sandbox,
        BTreeMap::from([("o1", o1), ("unfetched", unfetched)]),
    );
    scene.pull_request(
        3,
        scene.canned(
            3,
            "OPEN",
            &[(scene.sha("o1"), "o1"), (scene.sha("unfetched"), "o2")],
            json!({"headRefName": "feat-o", "createdAt": rfc(at("09:30:00"))}),
        ),
    );
    scene
}

/// No call to `git` or `gh` wrote to the repository or fetched.
fn assert_read_only(scene: &Scene) {
    let forbidden = [
        "fetch",
        "pull",
        "push",
        "clone",
        "commit",
        "checkout",
        "reset",
        "gc",
        "update-ref",
        "add",
        "merge",
        "rebase",
    ];
    let calls = scene.calls("git");
    assert!(!calls.is_empty(), "tapes ran no git");
    for call in &calls {
        // `git config` reads unless it sets a value.
        assert!(
            !call.split_whitespace().any(|word| word == "config")
                || call.contains(" --get") && !call.contains(" --add"),
            "tapes ran git config to write: git {call}"
        );
        for word in call.split_whitespace() {
            assert!(
                !forbidden.contains(&word),
                "tapes ran a git command that is not a read: git {call}"
            );
        }
    }
}

// ------------------------------------------------------------------ tests

/// Claim: a squash-merged pull request whose branch and commits are gone
/// prices the squash commit from the edits the session made on the branch,
/// labelled one tier down (matched, never bound), because the pull
/// request's lifetime, days before the merge, admits those edits. Rejects
/// selecting only the listed commits, which no longer exist and so price
/// nothing, and the squash commit's own window alone, which holds none of
/// the session's work.
#[test]
fn a_squash_merged_pull_request_prices_the_squash_commit_from_its_branch_edits() {
    let scene = squashed();
    for name in ["f1", "f2"] {
        assert!(!holds(&scene.sandbox, scene.sha(name)), "{name} is gone");
    }

    let view = scene.json(&["--pr", "7"]);
    assert_eq!(view["schema"], "tapes-change-usage/4");
    assert_eq!(scene.selected(&view), ["S"]);
    let priced = scene.requests(&view);
    assert!(
        priced.get("S").copied().unwrap_or(0) > 0,
        "{}",
        view["rows"]
    );
    let labels = scene.labels(&view, "S");
    assert!(
        labels.iter().all(|(tier, _)| tier == "matched"),
        "{labels:?}"
    );
    assert!(!labels.is_empty());

    // The pull request, as the change view reports it.
    let pr = &view["selection"];
    assert_eq!(pr["kind"], "pull-request");
    assert_eq!(pr["number"], 7);
    assert_eq!(pr["state"], "MERGED");
    assert_eq!(pr["method"], "squash");
    assert_eq!(pr["base"], "main");
    assert_eq!(pr["head"], "feature");
    assert_eq!(pr["created_at"], rfc(day(1, "09:30:00")));
    assert_eq!(pr["ended_at"], rfc(day(6, "12:00:00")));
    assert_eq!(pr["merge_commit"], scene.sha("S"));
    assert_eq!(pr["widened"], json!([scene.sha("S")]));
    assert_eq!(
        pr["missing"],
        json!([scene.sha("f1"), scene.sha("f2")]),
        "the listed commits the repository lost are named by sha"
    );
    assert!(pr["rule"].as_str().unwrap().contains("squash"));

    // The commit alone has its own window, which holds none of the work.
    let alone = scene.json(&["--commit", scene.sha("S")]);
    assert_eq!(
        scene.requests(&alone).get("S").copied().unwrap_or(0),
        0,
        "{}",
        alone["rows"]
    );
    assert_ne!(alone["interval"], view["interval"]);
    assert_read_only(&scene);

    // gh was asked once, for exactly the fields the view needs.
    assert_eq!(
        scene.calls("gh"),
        ["pr view 7 --json number,state,commits,baseRefName,headRefName,mergeCommit,createdAt,mergedAt,closedAt,title"]
    );
}

/// Claim: a squash-merged pull request also selects the listed commits that
/// still exist locally, and reports none of them missing. Rejects selecting
/// the squash commit alone and dropping the commits the repository holds.
#[test]
fn a_squash_merged_pull_request_keeps_the_listed_commits_that_still_exist() {
    let scene = squashed_keeping(true);
    let view = scene.json(&["--pr", "7"]);
    assert_eq!(view["selection"]["method"], "squash");
    assert_eq!(scene.selected(&view), ["f1", "f2", "S"]);
    assert_eq!(view["selection"]["missing"], Value::Null);
    assert_eq!(view["selection"]["widened"], json!([scene.sha("S")]));
}

/// Claim: a pull request merged with a merge commit selects the commits of
/// its branch, not the merge commit, bound to the session that made them,
/// and prices them as `--commits-from` the same commits does. Rejects
/// selecting the merge commit, which authors nothing.
#[test]
fn a_merge_commit_pull_request_selects_the_branch_commits_not_the_merge() {
    let scene = merged();
    let view = scene.json(&["--pr", "5"]);
    assert_eq!(scene.selected(&view), ["m1", "m2"]);
    assert_eq!(view["selection"]["method"], "merge-commit");
    assert_eq!(view["selection"]["widened"], Value::Null);
    let priced = scene.requests(&view);
    assert!(priced["m1"] > 0 && priced["m2"] > 0, "{priced:?}");
    assert!(!priced.contains_key("M"));

    let list = scene.sandbox.dir("commits.txt");
    fs::write(&list, format!("{}\n{}\n", scene.sha("m1"), scene.sha("m2"))).unwrap();
    let listed = scene.json(&["--commits-from", list.to_str().unwrap()]);
    assert_eq!(view["rows"], listed["rows"]);
    assert_eq!(view["totals"], listed["totals"]);
    assert_eq!(view["unattributed"], listed["unattributed"]);
}

/// Claim: a rebase-merged pull request selects the rebased commits on the
/// base branch, found by the listed subjects in order, and prices them from
/// the session's edits though the original commits are gone. Rejects
/// selecting the listed commits, which no longer exist, and the merge
/// commit alone as a squash.
#[test]
fn a_rebase_merged_pull_request_selects_the_rebased_commits() {
    let scene = rebased();
    assert!(!holds(&scene.sandbox, scene.sha("r1")));
    let view = scene.json(&["--pr", "9"]);
    assert_eq!(view["selection"]["method"], "rebase");
    assert_eq!(scene.selected(&view), ["r1'", "r2'"]);
    assert_eq!(
        view["selection"]["widened"],
        json!([scene.sha("r1'"), scene.sha("r2'")])
    );
    let priced = scene.requests(&view);
    assert!(priced["r1'"] > 0 && priced["r2'"] > 0, "{priced:?}");
    assert!(
        scene
            .labels(&view, "r1'")
            .iter()
            .all(|(tier, _)| tier != "bound"),
        "an edit digest never binds"
    );
}

/// Claim: an open pull request selects its listed commits that exist
/// locally, prices them from the recordings, and reports the listed commits
/// the repository lacks by sha. Rejects refusing because one commit is
/// missing and hiding that it is.
#[test]
fn an_open_pull_request_selects_its_local_commits_and_names_the_rest() {
    let scene = open();
    let view = scene.json(&["--pr", "3"]);
    assert_eq!(scene.selected(&view), ["o1"]);
    let pr = &view["selection"];
    assert_eq!(pr["method"], "open");
    assert_eq!(pr["state"], "OPEN");
    assert_eq!(pr["head"], "feat-o");
    assert_eq!(pr["ended_at"], Value::Null);
    assert_eq!(pr["missing"], json!([scene.sha("unfetched")]));
    assert_eq!(pr["listed"], 2);
    assert!(scene.requests(&view)["o1"] > 0);
    assert_read_only(&scene);

    let text = String::from_utf8(scene.tapes(&["--pr", "3"]).stdout).unwrap();
    assert!(text.contains("Pull request #3"), "{text}");
    assert!(text.contains("method open"), "{text}");
    assert!(text.contains("1 of 2 listed commits"), "{text}");
}

/// Claim: a merged pull request whose merge is not in the repository fits
/// no rule: the output says so and selects the listed commits that exist
/// locally, without fetching. Rejects guessing a squash from a merge commit
/// that is not there.
#[test]
fn a_merge_the_repository_lacks_fits_no_rule_and_says_so() {
    let scene = open();
    scene.pull_request(
        4,
        scene.canned(
            4,
            "MERGED",
            &[(scene.sha("o1"), "o1")],
            json!({
                "headRefName": "feat-o",
                "mergeCommit": {"oid": "e".repeat(40)},
                "mergedAt": rfc(at("12:00:00")),
                "closedAt": rfc(at("12:00:00")),
            }),
        ),
    );
    let view = scene.json(&["--pr", "4"]);
    assert_eq!(view["selection"]["method"], "unrecognized");
    assert_eq!(scene.selected(&view), ["o1"]);
    let why = view["selection"]["unrecognized"].as_str().unwrap();
    assert!(why.contains("not in the repository"), "{why}");
    assert_eq!(view["selection"]["widened"], Value::Null);
    assert_read_only(&scene);
}

/// Claim: `--pr` refuses clearly when `gh` is not installed, naming the
/// remedy, when `gh` fails, naming what failed, when `gh` prints something
/// that is no pull request, and when the repository holds none of the
/// pull request's commits. Rejects an empty answer, a panic, and a refusal
/// that names nothing to do.
#[test]
fn pr_refuses_when_gh_is_missing_or_fails() {
    let scene = open();
    let missing = scene.refusal(&scene.without_gh(), &["--pr", "3"]);
    assert!(
        missing.contains("gh") && missing.contains("not installed"),
        "{missing}"
    );
    assert!(
        missing.contains("install gh") && missing.contains("--commits-from"),
        "{missing}"
    );

    fs::write(scene.forge.join("gh.fail"), "gh: not logged in\n").unwrap();
    let failed = scene.refusal(&scene.with_gh(), &["--pr", "3"]);
    assert!(
        failed.contains("gh pr view 3 failed") && failed.contains("gh: not logged in"),
        "{failed}"
    );
    assert!(failed.contains("--commits-from"), "{failed}");
    fs::remove_file(scene.forge.join("gh.fail")).unwrap();

    fs::write(scene.forge.join("pr-8.json"), "no pull request here").unwrap();
    let garbled = scene.refusal(&scene.with_gh(), &["--pr", "8"]);
    assert!(garbled.contains("could not read"), "{garbled}");

    // A pull request none of whose commits the repository holds, and which
    // is not merged, selects nothing.
    scene.pull_request(
        6,
        scene.canned(6, "OPEN", &[(&"c".repeat(40), "elsewhere")], json!({})),
    );
    let empty = scene.refusal(&scene.with_gh(), &["--pr", "6"]);
    assert!(empty.contains("selects no commits"), "{empty}");
    assert!(
        empty.contains(&"c".repeat(7)) || empty.contains("--commits-from"),
        "{empty}"
    );
}

/// Claim: `--pr` combines with no other selector, a session or a window.
/// Rejects answering one of them and ignoring the other.
#[test]
fn pr_combines_with_no_other_selection() {
    let scene = open();
    let sha = scene.sha("o1");
    for args in [
        vec!["--pr", "3", "--commit", sha],
        vec!["--pr", "3", "--range", "main..HEAD"],
        vec!["--pr", "3", "--branch", "feat-o"],
        vec!["--pr", "3", "--commits-from", "-"],
        vec!["--pr", "3", OPENED],
        vec![
            "--pr",
            "3",
            "--window",
            "2026-01-12T00:00:00Z..2026-01-13T00:00:00Z",
        ],
    ] {
        scene.refusal(&scene.with_gh(), &args);
    }
    assert!(scene.calls("gh").is_empty(), "gh ran for a refused ask");
}
