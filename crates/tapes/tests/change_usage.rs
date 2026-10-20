//! `tapes usage` prices a change: the commits a revision, range, branch or
//! list selects, by commit, agent and role, from the recordings of the
//! sessions that made, inspected or worked near them. Each test drives the
//! binary over a synthetic repository and the stores of three harnesses,
//! then names the caller-visible claim it holds and the wrong behaviour it
//! rejects.

#[path = "../../tapes-core/tests/support/recordings.rs"]
mod recordings;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

use recordings::*;

const DELEGATE_A: &str = "c1a00000-0000-4000-8000-0000000000a1";
const DELEGATE_B: &str = "019b0000-0000-7000-8000-0000000000b1";
const ON_MAIN: &str = "019b0000-0000-7000-8000-0000000000f1";
const LIVE: &str = "c1a00000-0000-4000-8000-0000000000c1";
const REVIEWER: &str = "019b0000-0000-7000-8000-0000000000e1";
const LEAD: &str = "c1a00000-0000-4000-8000-0000000000d1";
const EDITOR: &str = "a0d0000000000000e";
const CACHED: &str = "c1a00000-0000-4000-8000-0000000000a7";

/// The commits the scenario made, by the name expectations use.
struct Change {
    sandbox: Sandbox,
    names: BTreeMap<&'static str, String>,
}

impl Change {
    fn sha(&self, name: &str) -> &str {
        &self.names[name]
    }

    fn name(&self, sha: &str) -> &'static str {
        self.names
            .iter()
            .find(|(_, full)| full.as_str() == sha)
            .map(|(name, _)| *name)
            .unwrap_or_else(|| panic!("no commit named {sha}"))
    }
}

fn refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// A day of work in one repository:
///
/// - 09:00 a Claude delegate in worktree `wt-a` on `feat-a` edits and
///   commits a1 and a2; at 09:40 `feat-a` is merged without fast-forward as
///   `Merge branch 'feat-a'`, then its branch and worktree are removed. From
///   09:30 a Codex reviewer in the main checkout runs `git show a1`.
/// - 10:00 a Codex delegate in `wt-b` on `feat-b` commits b1, merged the
///   same way at 10:40 and removed.
/// - 11:00 a pi session in the main checkout edits and commits f1 straight
///   onto `main`.
/// - 12:00 a Claude session in `wt-c` on `feat-c` commits c1; the branch
///   stays live and unmerged.
/// - 13:00 a Claude lead in `wt-d` on `feat-d` spawns an editor subagent that
///   writes what d1 adds, commits d1, and records a `cost-state` whose total
///   counts its subagent's requests; `feat-d` is fast-forwarded onto `main`.
/// - 14:00 a Claude session in `wt-t` on `feat-t` writes 5-minute and
///   1-hour cache entries and commits t1; the branch stays live.
fn change() -> Change {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let base = sandbox.git(&repo, None, &["rev-parse", "HEAD"]);

    let wt_a = sandbox.worktree("wt-a", "feat-a");
    let a1_lines = lines("alpha");
    let a2_lines = lines("beta");
    let a1 = sandbox.commit(&wt_a, at("09:10:00"), "a.rs", &refs(&a1_lines), "a1");
    let a2 = sandbox.commit(&wt_a, at("09:25:00"), "a.rs", &refs(&a2_lines), "a2");
    sandbox.git(
        &repo,
        Some(at("09:40:00")),
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge branch 'feat-a'",
            "feat-a",
        ],
    );
    let merge_a = sandbox.git(&repo, None, &["rev-parse", "HEAD"]);
    sandbox.retire(&wt_a, "feat-a");

    let mut delegate_a = Claude::lead(DELEGATE_A, &wt_a, "feat-a");
    delegate_a.prompt(at("09:00:00"));
    delegate_a.reply(at("09:00:05"));
    delegate_a.edit(at("09:05:00"), &wt_a.join("a.rs"), &a1_lines);
    delegate_a.commit(at("09:10:00"), &a1, "committed", "a1");
    delegate_a.edit(at("09:20:00"), &wt_a.join("a.rs"), &a2_lines);
    delegate_a.commit(at("09:25:00"), &a2, "committed", "a2");
    delegate_a.reply(at("09:26:00"));
    delegate_a.write(&sandbox, &wt_a);

    let mut reviewer = Codex::new(REVIEWER, at("09:30:00"), &repo, "main");
    reviewer.reply(at("09:30:05"));
    reviewer.exec(
        at("09:33:00"),
        &format!("git show {}", short(&a1)),
        &format!("commit {a1}\nAuthor: Fixture Author <author@example.invalid>\n\n    a1\n"),
    );
    reviewer.reply(at("09:36:00"));
    reviewer.write(&sandbox);

    let wt_b = sandbox.worktree("wt-b", "feat-b");
    let b1 = sandbox.commit(&wt_b, at("10:15:00"), "b.rs", &refs(&lines("gamma")), "b1");
    sandbox.git(
        &repo,
        Some(at("10:40:00")),
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge branch 'feat-b'",
            "feat-b",
        ],
    );
    let merge_b = sandbox.git(&repo, None, &["rev-parse", "HEAD"]);
    sandbox.retire(&wt_b, "feat-b");
    let mut delegate_b = Codex::new(DELEGATE_B, at("10:00:00"), &wt_b, "feat-b");
    delegate_b.reply(at("10:00:05"));
    delegate_b.reply(at("10:05:00"));
    delegate_b.commit(at("10:15:00"), "feat-b", &b1, "b1");
    delegate_b.reply(at("10:16:00"));
    delegate_b.write(&sandbox);

    let f1_lines = lines("delta");
    let f1 = sandbox.commit(&repo, at("11:10:00"), "f.rs", &refs(&f1_lines), "f1");
    let mut on_main = Pi::new(ON_MAIN, at("11:00:00"), &repo);
    on_main.reply(at("11:00:05"));
    on_main.edit(at("11:05:00"), &repo.join("f.rs"), &f1_lines);
    on_main.commit(at("11:10:00"), "main", &f1, "f1");
    on_main.write(&sandbox);

    let wt_c = sandbox.worktree("wt-c", "feat-c");
    let c1 = sandbox.commit(
        &wt_c,
        at("12:10:00"),
        "c.rs",
        &refs(&lines("epsilon")),
        "c1",
    );
    let mut live = Claude::lead(LIVE, &wt_c, "feat-c");
    live.prompt(at("12:00:00"));
    live.reply(at("12:00:05"));
    live.commit(at("12:10:00"), &c1, "committed", "c1");
    live.write(&sandbox, &wt_c);

    let wt_d = sandbox.worktree("wt-d", "feat-d");
    let d1_lines = lines("zeta");
    let d1 = sandbox.commit(&wt_d, at("13:20:00"), "d.rs", &refs(&d1_lines), "d1");
    sandbox.git(&repo, None, &["merge", "-q", "--ff-only", "feat-d"]);
    sandbox.retire(&wt_d, "feat-d");
    let mut lead = Claude::lead(LEAD, &wt_d, "feat-d");
    lead.prompt(at("13:00:00"));
    lead.reply(at("13:00:05"));
    lead.spawn(at("13:02:00"), at("13:07:00"), EDITOR);
    lead.reply(at("13:15:00"));
    lead.commit(at("13:20:00"), &d1, "committed", "d1");
    let mut editor = Claude::agent(&lead, EDITOR);
    editor.prompt(at("13:02:30"));
    editor.reply(at("13:03:00"));
    editor.edit(at("13:05:00"), &wt_d.join("d.rs"), &d1_lines);
    editor.reply(at("13:06:00"));
    lead.cost_state_covering(&[&editor]);
    lead.write(&sandbox, &wt_d);
    editor.write(&sandbox, &wt_d);

    let wt_t = sandbox.worktree("wt-t", "feat-t");
    let t1_lines = lines("eta");
    let t1 = sandbox.commit(&wt_t, at("14:10:00"), "t.rs", &refs(&t1_lines), "t1");
    let mut cached = Claude::lead(CACHED, &wt_t, "feat-t");
    cached.cache_writes = Some((1_000_000, 2_000_000));
    cached.prompt(at("14:00:00"));
    cached.reply(at("14:00:05"));
    cached.edit(at("14:05:00"), &wt_t.join("t.rs"), &t1_lines);
    cached.commit(at("14:10:00"), &t1, "committed", "t1");
    cached.write(&sandbox, &wt_t);

    let names = BTreeMap::from([
        ("base", base),
        ("a1", a1),
        ("a2", a2),
        ("Ma", merge_a),
        ("b1", b1),
        ("Mb", merge_b),
        ("f1", f1),
        ("c1", c1),
        ("d1", d1),
        ("t1", t1),
    ]);
    Change { sandbox, names }
}

// ---------------------------------------------------------------- driving

/// `tapes usage ARGS` in `dir`, reading only the sandbox's stores.
fn tapes_in(change: &Change, dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let sandbox = &change.sandbox;
    let mut command = Command::new(env!("CARGO_BIN_EXE_tapes"));
    command
        .current_dir(dir)
        .env_clear()
        .env("HOME", sandbox.dir("home"))
        .env("PATH", "/usr/bin:/bin")
        .env("CLAUDE_CONFIG_DIR", sandbox.dir("claude"))
        .env("CODEX_HOME", sandbox.dir("codex"))
        .env("PI_CODING_AGENT_DIR", sandbox.dir("pi"))
        .env("XDG_CONFIG_HOME", sandbox.dir("xdg"))
        .env("XDG_DATA_HOME", sandbox.dir("xdg-data"))
        .env("GIT_CONFIG_GLOBAL", sandbox.dir("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("usage")
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn tapes(change: &Change, args: &[&str]) -> Output {
    tapes_in(change, &change.sandbox.repo(), args, None)
}

fn answer(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn json(change: &Change, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    answer(tapes(change, &args))
}

fn refusal(change: &Change, args: &[&str]) -> String {
    let output = tapes(change, args);
    assert!(
        !output.status.success(),
        "{args:?} answered: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// What a tally counts: requests and each model's native counters, the
/// parts no request explains included.
#[derive(Clone, Debug, Default, PartialEq)]
struct Figure {
    requests: u64,
    tokens: BTreeMap<(String, String), BTreeMap<String, u64>>,
}

impl Figure {
    fn of(tally: &Value) -> Self {
        let mut figure = Self {
            requests: tally["requests"].as_u64().unwrap(),
            ..Self::default()
        };
        for (field, prefix) in [("models", ""), ("unexplained", "unexplained ")] {
            for model in tally[field].as_array().into_iter().flatten() {
                let counters = figure
                    .tokens
                    .entry((
                        model["harness"].as_str().unwrap().to_owned(),
                        model["model"].as_str().unwrap().to_owned(),
                    ))
                    .or_default();
                for (name, value) in model["tokens"].as_object().unwrap() {
                    if let Some(value) = value.as_u64() {
                        *counters.entry(format!("{prefix}{name}")).or_default() += value;
                    }
                }
            }
        }
        figure
    }

    fn add(&mut self, other: &Self) {
        self.requests += other.requests;
        for (model, counters) in &other.tokens {
            let into = self.tokens.entry(model.clone()).or_default();
            for (name, value) in counters {
                *into.entry(name.clone()).or_default() += value;
            }
        }
    }

    fn sum<'a>(tallies: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut sum = Self::default();
        for tally in tallies {
            sum.add(&Self::of(tally));
        }
        sum.tokens.retain(|_, counters| {
            counters.retain(|_, value| *value != 0);
            !counters.is_empty()
        });
        sum
    }
}

fn items(answer: &Value, field: &str) -> Vec<Value> {
    answer[field].as_array().cloned().unwrap_or_default()
}

/// Each commit's rows, by the scenario's name for it.
fn by_commit(change: &Change, answer: &Value) -> BTreeMap<&'static str, Vec<Value>> {
    let mut rows: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for row in items(answer, "rows") {
        let sha = row["key"]["commit"].as_str().unwrap().to_owned();
        rows.entry(change.name(&sha)).or_default().push(row);
    }
    rows
}

fn session<'a>(answer: &'a Value, id: &str) -> &'a Value {
    answer["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"].as_str().unwrap().ends_with(id))
        .unwrap_or_else(|| panic!("{id} is not in the view: {}", answer["sessions"]))
}

fn row_for<'a>(answer: &'a Value, sha: &str, id: &str) -> Option<&'a Value> {
    answer["rows"].as_array().unwrap().iter().find(|row| {
        row["key"]["commit"] == sha
            && row["key"]["session"]["id"]
                .as_str()
                .is_some_and(|session| session.ends_with(id))
    })
}

// ------------------------------------------------------------------ tests

/// Claim: every selector that names the same commits gives each of them
/// the same rows, and a commit's rows do not change with what else is
/// selected. Rejects a division made over the selection alone, which would
/// move a session's share of an unselected commit onto a selected one.
#[test]
fn every_selector_gives_a_commit_the_same_rows() {
    let change = change();
    let (a1, a2) = (change.sha("a1"), change.sha("a2"));
    let list = change.sandbox.dir("commits.txt");
    fs::write(&list, format!("# feat-a\n{a1}\n\n{a2}\n")).unwrap();

    let commits = json(&change, &["--commit", a1, "--commit", a2]);
    let views = [
        json(
            &change,
            &[
                "--range",
                &format!("{}..{}", change.sha("base"), change.sha("Ma")),
            ],
        ),
        json(&change, &["--branch", "feat-a"]),
        json(&change, &["--commits-from", list.to_str().unwrap()]),
        answer(tapes_in(
            &change,
            &change.sandbox.repo(),
            &["--commits-from", "-", "--json"],
            Some(&format!("{a1}\n{a2}\n")),
        )),
        json(
            &change,
            &["--range", &format!("{}..main", change.sha("base"))],
        ),
    ];
    let expected = by_commit(&change, &commits);
    assert_eq!(
        expected.keys().copied().collect::<Vec<_>>(),
        ["a1", "a2"],
        "{}",
        commits["rows"]
    );
    assert!(
        commits["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["key"]["role"] == "reviewer"),
        "the reviewer of a1 holds a row: {}",
        commits["rows"]
    );
    for view in &views {
        let rows = by_commit(&change, view);
        for name in ["a1", "a2"] {
            assert_eq!(
                rows[name], expected[name],
                "{name} in {}",
                view["selection"]
            );
        }
    }
}

/// Claim: a lead whose recorded total counts its subagents' requests is
/// counted once: the change view's sessions add up to the family view of
/// the same lead, and the part no request explains is only the model no
/// request names. Rejects a lead whose unexplained part repeats the
/// requests its subagent's rows already count.
#[test]
fn a_lead_and_its_subagents_count_each_request_once() {
    let change = change();
    let view = json(
        &change,
        &["--commit", change.sha("d1"), "--by", "commit,session"],
    );
    let family = json(&change, &[LEAD, "--family", "--harness", "claude"]);

    let lead = session(&view, LEAD);
    let editor = session(&view, EDITOR);
    let both = Figure::sum([&lead["whole"], &editor["whole"]]);
    let unexplained: Vec<&str> = lead["whole"]["unexplained"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["model"].as_str().unwrap())
        .collect();
    assert_eq!(unexplained, ["claude-fixture-small"], "{}", lead["whole"]);

    let counted = |counter: &str| -> u64 {
        both.tokens
            .values()
            .map(|counters| counters.get(counter).copied().unwrap_or(0))
            .sum()
    };
    let family = &family["totals"];
    for name in ["input", "output", "cache_read"] {
        assert_eq!(
            counted(name) + counted(&format!("unexplained {name}")),
            family["tokens"][name].as_u64().unwrap(),
            "{name}: {family}"
        );
    }
    assert_eq!(
        counted("unexplained input"),
        family["unexplained"]["by_model"][0]["tokens"]["input"]
            .as_u64()
            .unwrap()
    );
}

/// Claim: a list-price estimate prices 5-minute and 1-hour cache writes at
/// their own rates where the recording splits them. Rejects pricing every
/// cache write at the 5-minute rate, or leaving the split unrecorded.
#[test]
fn cache_writes_are_priced_by_their_ttl() {
    let change = change();
    let prices = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tapes-core/tests/fixtures/prices/prices.toml");
    let view = json(
        &change,
        &[
            "--commit",
            change.sha("t1"),
            "--prices",
            prices.to_str().unwrap(),
        ],
    );
    let rows = &view["totals"]["rows"];
    let model = &rows["models"][0];
    assert_eq!(model["model"], "claude-fixture", "{rows}");
    let requests = rows["requests"].as_u64().unwrap();
    assert!(requests > 0);
    assert_eq!(
        model["cache_write"]["ephemeral_5m"].as_u64(),
        Some(1_000_000 * requests),
        "{model}"
    );
    assert_eq!(
        model["cache_write"]["ephemeral_1h"].as_u64(),
        Some(2_000_000 * requests),
        "{model}"
    );
    let tokens = &model["tokens"];
    let count = |name: &str| tokens[name].as_u64().unwrap_or(0) as f64;
    let expected = (count("input") * 3.0
        + count("cache_read") * 0.3
        + count("output") * 15.0
        + 1_000_000.0 * requests as f64 * 3.75
        + 2_000_000.0 * requests as f64 * 6.0)
        / 1_000_000.0;
    let amount = rows["list_price_estimate"]["amount"].as_f64().unwrap();
    assert!(
        (amount - expected).abs() < 1e-9,
        "{amount} is not {expected}: {}",
        rows["list_price_estimate"]
    );
}

/// Claim: `--by merge` groups commits by the first-parent merge that
/// brought them, a commit made on the mainline being its own group.
/// Rejects grouping by branch name or folding a fast-forward commit into a
/// neighbouring merge.
#[test]
fn by_merge_groups_by_the_mainline_merge() {
    let change = change();
    let view = json(
        &change,
        &[
            "--commit",
            change.sha("a1"),
            "--commit",
            change.sha("a2"),
            "--commit",
            change.sha("b1"),
            "--commit",
            change.sha("f1"),
            "--by",
            "merge",
        ],
    );
    let groups: Vec<(String, &str)> = items(&view, "rows")
        .iter()
        .map(|row| {
            let merge = &row["key"]["merge"];
            (
                merge["kind"].as_str().unwrap().to_owned(),
                change.name(merge["sha"].as_str().unwrap()),
            )
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("merge".to_owned(), "Ma"),
            ("merge".to_owned(), "Mb"),
            ("mainline".to_owned(), "f1"),
        ],
        "{}",
        view["rows"]
    );
}

/// Claim: `--branch` reads a live branch's commits beyond the default
/// branch, a deleted branch's commits through the merge naming it, and
/// refuses a name that is neither, naming the selectors that still work.
/// Rejects an empty answer for a merged branch and a guess for an unknown
/// one.
#[test]
fn branch_reads_live_then_merged_else_refuses() {
    let change = change();
    let live = json(&change, &["--branch", "feat-c"]);
    assert_eq!(live["selection"]["resolved"], "live");
    let shas: Vec<&str> = items(&live, "commits")
        .iter()
        .map(|commit| change.name(commit["sha"].as_str().unwrap()))
        .collect();
    assert_eq!(shas, ["c1"]);

    let merged = json(&change, &["--branch", "feat-b"]);
    assert_eq!(merged["selection"]["resolved"], "merged");
    assert_eq!(merged["selection"]["merges"][0], change.sha("Mb"));
    let shas: Vec<&str> = items(&merged, "commits")
        .iter()
        .map(|commit| change.name(commit["sha"].as_str().unwrap()))
        .collect();
    assert_eq!(shas, ["b1"]);

    let stderr = refusal(&change, &["--branch", "feat-gone"]);
    assert!(
        stderr.contains("--range") && stderr.contains("--commits-from"),
        "{stderr}"
    );
}

/// Claim: one session's reverse view, `<SESSION> --by commit`, gives each
/// commit what a range view over the same commits gives that session, a
/// lead's recorded total included. Rejects a reverse view divided
/// differently from the commit view, such as one read as a window over the
/// session's activity, which loses the part of a recorded total no request
/// explains.
#[test]
fn a_session_view_matches_the_range_view() {
    let change = change();
    let merged_a = format!("{}..{}", change.sha("base"), change.sha("Ma"));
    for (id, selection, commits) in [
        (DELEGATE_A, ["--range", merged_a.as_str()], 2),
        (LEAD, ["--commit", change.sha("d1")], 1),
    ] {
        let reverse = json(&change, &[id, "--by", "commit"]);
        let mut args = selection.to_vec();
        args.extend(["--by", "commit,session"]);
        let range = json(&change, &args);
        let rows = items(&reverse, "rows");
        assert_eq!(rows.len(), commits, "{id}: {}", reverse["rows"]);
        for row in &rows {
            let sha = row["key"]["commit"].as_str().unwrap();
            let same = row_for(&range, sha, id).expect("the range view holds the row");
            assert_eq!(
                Figure::of(row),
                Figure::of(same),
                "{id} {}",
                change.name(sha)
            );
            assert_eq!(row["evidence"], same["evidence"]);
        }
        assert_eq!(
            Figure::of(&session(&reverse, id)["whole"]),
            Figure::of(&session(&range, id)["whole"]),
            "{id}"
        );
    }
}

/// Claim: every session's rows, uncommitted buckets, unattributed parts and
/// unselected shares add up to its whole; the view's totals add up the
/// same way; and what unselected commits hold stays out of the rows'
/// total. Rejects an unselected share counted into the change's cost.
#[test]
fn totals_conserve_each_session_and_leave_unselected_out() {
    let change = change();
    let view = json(&change, &["--commit", change.sha("a2")]);
    let unselected = items(&view, "unselected");
    assert!(
        unselected
            .iter()
            .any(|share| share["commit"] == change.sha("a1")),
        "a1 is the delegate's other commit: {}",
        view["unselected"]
    );
    for session in items(&view, "sessions") {
        let parts = Figure::sum([
            &session["rows"],
            &session["uncommitted"],
            &session["unattributed"],
            &session["unselected"],
        ]);
        assert_eq!(parts, Figure::sum([&session["whole"]]), "{}", session["id"]);
    }
    let totals = &view["totals"];
    assert_eq!(
        Figure::sum(items(&view, "rows").iter()),
        Figure::sum([&totals["rows"]])
    );
    assert_eq!(
        Figure::sum(items(&view, "unselected").iter()),
        Figure::sum([&totals["unselected"]])
    );
    assert_eq!(
        Figure::sum([
            &totals["rows"],
            &totals["uncommitted"],
            &totals["unattributed"],
            &totals["unselected"],
        ]),
        Figure::sum([&totals["sessions"]])
    );
    let unselected_requests = totals["unselected"]["requests"].as_u64().unwrap();
    assert!(unselected_requests > 0);
    let rows_requests: u64 = items(&view, "rows")
        .iter()
        .map(|row| row["requests"].as_u64().unwrap())
        .sum();
    assert_eq!(totals["rows"]["requests"].as_u64().unwrap(), rows_requests);
}

/// Claim: `--tiers bound` places only what bound evidence places; what the
/// other tiers alone would place is unattributed as `tiers not asked`,
/// naming them, and every session still adds up to its whole. Rejects a
/// cascade that lets a left-out tier place, and a restricted view that
/// drops the left-out part from the session.
#[test]
fn tiers_restrict_placement_and_name_what_they_leave_out() {
    let change = change();
    let a1 = change.sha("a1");
    let every = json(&change, &["--commit", a1, "--by", "commit,session"]);
    let bound = json(
        &change,
        &["--commit", a1, "--by", "commit,session", "--tiers", "bound"],
    );
    assert_eq!(
        every["tiers"],
        serde_json::json!(["bound", "matched", "estimated"])
    );
    assert_eq!(bound["tiers"], serde_json::json!(["bound"]));

    let reviewed = row_for(&every, a1, REVIEWER).expect("the reviewer holds a share of a1");
    assert!(reviewed["requests"].as_u64().unwrap() > 0);
    assert!(row_for(&bound, a1, REVIEWER).is_none(), "{}", bound["rows"]);
    assert_ne!(
        Figure::of(&every["totals"]["rows"]),
        Figure::of(&bound["totals"]["rows"])
    );

    let reviewer = session(&bound, REVIEWER);
    assert_eq!(
        Figure::of(&reviewer["whole"]),
        Figure::of(&session(&every, REVIEWER)["whole"])
    );
    let not_asked: Vec<&Value> = bound["unattributed"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|part| part["reason"] == "tiers not asked")
        .collect();
    assert!(!not_asked.is_empty(), "{}", bound["unattributed"]);
    for part in &not_asked {
        assert_eq!(
            part["excluded_tiers"],
            serde_json::json!(["matched", "estimated"])
        );
    }
    for view in [&every, &bound] {
        for session in items(view, "sessions") {
            let parts = Figure::sum([
                &session["rows"],
                &session["uncommitted"],
                &session["unattributed"],
                &session["unselected"],
            ]);
            assert_eq!(parts, Figure::sum([&session["whole"]]), "{}", session["id"]);
        }
    }
}

/// Claim: a change view refuses what it cannot answer, saying what to run
/// instead. Rejects an empty answer standing in for a refusal.
#[test]
fn change_views_refuse_what_they_cannot_answer() {
    let change = change();
    let a1 = change.sha("a1");
    let outside = change.sandbox.dir("outside");
    fs::create_dir_all(&outside).unwrap();
    let output = tapes_in(&change, &outside, &["--commit", "HEAD"], None);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("not inside a Git repository"), "{stderr}");
    assert!(stderr.contains("--project"), "{stderr}");

    for (args, says) in [
        (vec!["--commit", "no-such-revision"], "no-such-revision"),
        (vec!["--range", "HEAD..HEAD"], "names no commits"),
        (vec!["--range", "HEAD"], "A..B"),
        (vec!["--commit", a1, "--family"], "--family"),
        (vec!["--commit", a1, "--global"], "--global"),
        (
            vec![
                "--commit",
                a1,
                "--window",
                "2026-01-12T09:00:00Z..2026-01-12T10:00:00Z",
            ],
            "--window",
        ),
        (vec![DELEGATE_A, "--commit", a1], "session"),
        (vec!["--commit", a1, "--by", "model"], "--by"),
        (vec!["--explain"], "a change view needs commits"),
        (vec![DELEGATE_A, "--by", "model"], "<SESSION> --by commit"),
    ] {
        let stderr = refusal(&change, &args);
        assert!(stderr.contains(says), "{args:?}: {stderr}");
    }
    let output = tapes_in(
        &change,
        &change.sandbox.repo(),
        &["--commits-from", "-"],
        Some("# none\n\n"),
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("names no revisions"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
