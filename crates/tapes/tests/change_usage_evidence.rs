//! Evidence files place a change's consumption: a session a `binding` or
//! `event` record names is placed by its records alone, its work's part
//! shared among the commits the repository holds, and work it holds no
//! commit of reported apart. Each test drives the binary over a synthetic
//! repository and harness stores, then names the caller-visible claim it
//! holds and the wrong behaviour it rejects.

#[path = "../../tapes-core/tests/support/recordings.rs"]
mod recordings;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use agent_tapes_core::attribution::{Allocation, Event, Owner, Piece, Plan, Requests, Tier};
use agent_tapes_core::attribution_evidence::{read_file, Malformed};
use serde_json::{json, Value};

use recordings::*;

const BOUND: &str = "c1a00000-0000-4000-8000-0000000000b1";
const WHOLE: &str = "c1a00000-0000-4000-8000-0000000000b2";
const PARTICIPANT: &str = "c1a00000-0000-4000-8000-0000000000b3";
const LABELLED: &str = "019b0000-0000-7000-8000-0000000000b4";
const SPLIT: &str = "c1a00000-0000-4000-8000-0000000000b5";
const STOCK: &str = "c1a00000-0000-4000-8000-0000000000b6";
const LEAD: &str = "c1a00000-0000-4000-8000-0000000000b7";
const SUBAGENT: &str = "a0b0000000000000b";
const LAUNCHER: &str = "c1a00000-0000-4000-8000-0000000000b8";
const LAUNCHED: &str = "019b0000-0000-7000-8000-0000000000b9";
const MISSING: &str = "c1a00000-0000-4000-8000-00000000dead";
const OUTSIDE: &str = "0123456789abcdef0123456789abcdef01234567";

// ---------------------------------------------------------------- driving

/// `tapes usage ARGS` in the repository, reading only the sandbox's stores.
fn tapes(sandbox: &Sandbox, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
        .current_dir(sandbox.repo())
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

fn json_of(sandbox: &Sandbox, args: &[&str]) -> Value {
    let mut args = args.to_vec();
    args.push("--json");
    args.push("--detail");
    let output = tapes(sandbox, &args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// An evidence file of `records` from a neutral producer.
fn evidence(sandbox: &Sandbox, name: &str, records: &[Value]) -> PathBuf {
    let path = sandbox.dir(name);
    let mut body = json!({"schema": "tapes-attribution-evidence/1",
        "producer": "fixture-producer/1.0.0", "written_at": "2026-01-12T18:00:00Z"})
    .to_string();
    body.push('\n');
    for record in records {
        body.push_str(&record.to_string());
        body.push('\n');
    }
    fs::write(&path, body).unwrap();
    path
}

fn session_ref(harness: &str, id: &str) -> Value {
    json!({"harness": harness, "id": id})
}

fn binding(harness: &str, id: &str, work: Value, tier: &str, span: Option<(Time, Time)>) -> Value {
    let mut record = json!({"kind": "binding", "session": session_ref(harness, id),
        "work": work, "tier": tier});
    if let Some((from, to)) = span {
        record["span"] = json!({"from": stamp(from), "to": stamp(to)});
    }
    record
}

fn event(harness: &str, id: &str, work: Value, at_: Time) -> Value {
    json!({"kind": "event", "session": session_ref(harness, id), "work": work,
        "tier": "bound", "at": stamp(at_), "role": "integrator"})
}

/// The session's entry in the view's `sessions`.
fn session<'a>(view: &'a Value, id: &str) -> &'a Value {
    view["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == id)
        .unwrap_or_else(|| panic!("no session {id} in {}", view["sessions"]))
}

fn has_session(view: &Value, id: &str) -> bool {
    view["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|session| session["id"] == id)
}

/// The requests each commit's rows give `id`, by commit.
fn rows_of(view: &Value, id: &str) -> BTreeMap<String, u64> {
    let mut held = BTreeMap::new();
    for row in view["rows"].as_array().unwrap() {
        if row["key"]["session"]["id"] == id {
            *held
                .entry(row["key"]["commit"].as_str().unwrap().to_owned())
                .or_default() += row["requests"].as_u64().unwrap();
        }
    }
    held
}

/// The bases of `id`'s rows.
fn bases_of(view: &Value, id: &str) -> Vec<String> {
    let mut bases: Vec<String> = view["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["key"]["session"]["id"] == id)
        .flat_map(|row| row["evidence"].as_array().unwrap().iter())
        .map(|evidence| {
            format!(
                "{}/{}",
                evidence["tier"].as_str().unwrap(),
                evidence["basis"].as_str().unwrap()
            )
        })
        .collect();
    bases.sort();
    bases.dedup();
    bases
}

fn requests(tally: &Value) -> u64 {
    tally["requests"].as_u64().unwrap()
}

/// Every native counter of a tally, the parts no request explains included.
fn counters(tally: &Value) -> BTreeMap<String, u64> {
    let mut held = BTreeMap::new();
    for field in ["models", "unexplained"] {
        for model in tally[field].as_array().into_iter().flatten() {
            for (name, value) in model["tokens"].as_object().unwrap() {
                if let Some(value) = value.as_u64() {
                    *held.entry(format!("{field} {name}")).or_default() += value;
                }
            }
        }
    }
    held
}

fn refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// The arguments selecting each of `shas` with --commit.
fn commits<'a>(shas: &[&'a str]) -> Vec<&'a str> {
    shas.iter().flat_map(|sha| ["--commit", *sha]).collect()
}

// ---------------------------------------------------------------- scenarios

/// A Claude session in worktree `wt-x` on `feat-x` that edits and commits
/// x1 at 09:10 and x2 at 09:40, its seven requests at 09:00:05, 09:05,
/// 09:10, 09:15, 09:30, 09:40 and 09:45. Its recording alone splits it over
/// both commits.
fn two_commit_session() -> (Sandbox, String, String) {
    let sandbox = Sandbox::new();
    let wt = sandbox.worktree("wt-x", "feat-x");
    let x1_lines = lines("xray");
    let x2_lines = lines("yankee");
    let x1 = sandbox.commit(&wt, at("09:10:00"), "x.rs", &refs(&x1_lines), "x1");
    let x2 = sandbox.commit(&wt, at("09:40:00"), "x.rs", &refs(&x2_lines), "x2");
    let mut session = Claude::lead(BOUND, &wt, "feat-x");
    session.prompt(at("09:00:00"));
    session.reply(at("09:00:05"));
    session.edit(at("09:05:00"), &wt.join("x.rs"), &x1_lines);
    session.commit(at("09:10:00"), &x1, "committed", "x1");
    session.reply(at("09:15:00"));
    session.edit(at("09:30:00"), &wt.join("x.rs"), &x2_lines);
    session.commit(at("09:40:00"), &x2, "committed", "x2");
    session.reply(at("09:45:00"));
    session.write(&sandbox, &wt);
    (sandbox, x1, x2)
}

/// Claim: a session a binding names is placed by its binding's span alone:
/// the four requests inside `[09:00, 09:20)` are x1's, bound by span, and
/// the three after it are unattributed, though its recording shows it
/// creating x2. Rejects mixing the recording's evidence into a session a
/// record names, which gives x2 a row.
#[test]
fn a_binding_outranks_the_recording() {
    let (sandbox, x1, x2) = two_commit_session();
    let selection = commits(&[&x1, &x2]);
    let mut args = selection.clone();
    args.extend(["--by", "commit,session"]);
    let stock = json_of(&sandbox, &args);
    assert_eq!(
        rows_of(&stock, BOUND).get(&x2).copied(),
        Some(3),
        "the recording alone places x2"
    );

    let file = evidence(
        &sandbox,
        "bound.jsonl",
        &[binding(
            "claude",
            BOUND,
            json!({"label": "x-change", "commits": [x1]}),
            "bound",
            Some((at("09:00:00"), at("09:20:00"))),
        )],
    );
    args.extend(["--evidence", file.to_str().unwrap()]);
    let view = json_of(&sandbox, &args);
    assert_eq!(rows_of(&view, BOUND), BTreeMap::from([(x1.clone(), 4)]));
    assert_eq!(bases_of(&view, BOUND), ["bound/span"]);
    let placed = session(&view, BOUND);
    assert_eq!(placed["placement"], "evidence");
    assert_eq!(requests(&placed["unattributed"]), 3);
    assert_eq!(requests(&placed["whole"]), 7);
}

/// Claim: a session a binding places that its recording shows creating a
/// commit the binding does not name is reported as a conflict naming the
/// session, the commit and the file, and the binding still places it; the
/// view answers. Rejects a silent disagreement and a refusal.
#[test]
fn a_creation_no_record_binds_is_a_conflict() {
    let (sandbox, x1, x2) = two_commit_session();
    let file = evidence(
        &sandbox,
        "bound.jsonl",
        &[binding(
            "claude",
            BOUND,
            json!({"commits": [x1]}),
            "bound",
            Some((at("09:00:00"), at("09:20:00"))),
        )],
    );
    let mut args = commits(&[&x1, &x2]);
    args.extend(["--evidence", file.to_str().unwrap()]);
    let view = json_of(&sandbox, &args);
    let conflicts = view["evidence"]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    assert_eq!(conflicts[0]["session"]["id"], BOUND);
    assert_eq!(conflicts[0]["commit"], x2.as_str());
    assert_eq!(conflicts[0]["files"], json!([file.to_str().unwrap()]));
    assert_eq!(view["evidence"]["placed"], 1);
    let text = String::from_utf8(
        tapes(&sandbox, &[&args[..], &["--explain", "--detail"]].concat()).stdout,
    )
    .unwrap();
    assert!(text.contains(&format!("created {}", &x2[..10])), "{text}");
    assert!(
        text.contains("from ") && text.contains("(fixture-producer/1.0.0)"),
        "{text}"
    );
}

/// Claim: a binding without a span places every request of its session,
/// even a session whose recording lies outside the repository and the
/// selection's interval: it is looked up by id and its whole is the
/// commit's. Rejects reading only listed sessions, and a whole-session
/// binding that places nothing.
#[test]
fn a_binding_without_a_span_places_the_whole_session() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let w1 = sandbox.commit(
        &repo,
        at("09:00:00"),
        "w.rs",
        &refs(&lines("whiskey")),
        "w1",
    );
    let outside = sandbox.dir("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    let mut pi = Pi::new(WHOLE, at("13:00:00"), &outside);
    for time in ["13:00:00", "13:20:00", "15:30:00", "16:00:00"] {
        pi.reply(at(time));
    }
    pi.write(&sandbox);
    let file = evidence(
        &sandbox,
        "whole.jsonl",
        &[binding(
            "pi",
            WHOLE,
            json!({"commits": [w1]}),
            "declared",
            None,
        )],
    );
    let view = json_of(
        &sandbox,
        &[
            "--commit",
            &w1,
            "--by",
            "commit,session",
            "--evidence",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(rows_of(&view, WHOLE), BTreeMap::from([(w1.clone(), 4)]));
    assert_eq!(bases_of(&view, WHOLE), ["matched/span"]);
    let placed = session(&view, WHOLE);
    assert_eq!(requests(&placed["whole"]), 4);
    assert_eq!(requests(&placed["unattributed"]), 0);
    assert!(placed["rules"]
        .as_array()
        .unwrap()
        .contains(&json!("evidence")));
    assert_eq!(view["evidence"]["looked_up"], 1);

    // A window over its activity finds it through its binding too.
    let window = json_of(
        &sandbox,
        &[
            "--window",
            "2026-01-12T12:00:00Z..2026-01-12T17:00:00Z",
            "--by",
            "commit,session",
            "--evidence",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(rows_of(&window, WHOLE), BTreeMap::from([(w1, 4)]));
}

/// Claim: a participant's events divide it among two changes exactly as the
/// attribution engine divides the same events: each change's events span
/// it first to last, uncovered requests go by working stretch to the event
/// they lead up to or follow, and a stretch with no event is unattributed.
/// Rejects any placement of events other than the engine's.
#[test]
fn events_divide_a_participant_as_the_engine_does() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let a1 = sandbox.commit(&repo, at("08:30:00"), "a.rs", &refs(&lines("alpha")), "a1");
    let b1 = sandbox.commit(&repo, at("08:40:00"), "b.rs", &refs(&lines("bravo")), "b1");
    let times = [
        "09:00:00", "09:05:00", "09:10:00", "09:15:00", "09:20:00", "09:25:00", "09:30:00",
        "09:35:00", "09:40:00", "11:00:00", "11:05:00", "11:10:00",
    ];
    let mut participant = Claude::lead(PARTICIPANT, &repo, "main");
    participant.prompt(at("08:59:00"));
    for time in times {
        participant.reply(at(time));
    }
    participant.write(&sandbox, &repo);
    let events = [
        ("a", "09:12:00"),
        ("a", "09:22:00"),
        ("b", "09:31:00"),
        ("b", "09:38:00"),
    ];
    let work = |name: &str| match name {
        "a" => json!({"label": "change-a", "commits": [a1]}),
        _ => json!({"label": "change-b", "commits": [b1]}),
    };
    let records: Vec<Value> = events
        .iter()
        .map(|(name, time)| event("claude", PARTICIPANT, work(name), at(time)))
        .collect();
    let file = evidence(&sandbox, "events.jsonl", &records);
    let view = json_of(
        &sandbox,
        &[
            "--commit",
            &a1,
            "--commit",
            &b1,
            "--by",
            "commit,session",
            "--evidence",
            file.to_str().unwrap(),
        ],
    );

    // The engine, given the same events and working stretches.
    let stamps: Vec<Time> = times.iter().map(|time| at(time)).collect();
    let mut starts = vec![stamps[0]];
    for pair in stamps.windows(2) {
        if pair[1] - pair[0] > chrono::Duration::minutes(45) {
            starts.push(pair[1]);
        }
    }
    let engine_events: Vec<Event<String>> = events
        .iter()
        .map(|(name, time)| Event {
            at: at(time),
            work: (*name).to_owned(),
            tier: Tier::Bound,
        })
        .collect();
    let plan = Plan::new(&[], &engine_events, Some(&starts));
    let holds = |from: Option<Time>, to: Option<Time>, stamp: Time| {
        from.is_none_or(|from| stamp >= from) && to.is_none_or(|to| stamp < to)
    };
    let counts = Requests {
        segments: plan
            .segments
            .iter()
            .map(|segment| {
                stamps
                    .iter()
                    .filter(|s| holds(segment.from, segment.to, **s))
                    .count() as u64
            })
            .collect(),
        windows: plan
            .windows
            .iter()
            .map(|window| {
                stamps
                    .iter()
                    .filter(|s| holds(window.from, window.to, **s))
                    .count() as u64
            })
            .collect(),
        remainder: 0,
    };
    let divided = plan
        .attribute(&counts, Some(Allocation::Even), true)
        .unwrap();
    let mut engine: BTreeMap<String, u64> = BTreeMap::new();
    let mut unplaced = 0;
    let even = divided.allocated.as_ref().unwrap();
    for (part, owner) in divided.estimated.parts.iter().chain(
        even.parts
            .iter()
            .filter(|(part, _)| matches!(part.piece, Piece::Even { .. })),
    ) {
        let count = match part.piece {
            Piece::Even { index, shares, .. } => {
                agent_tapes_core::attribution::even_split(part.requests, shares, index)
            }
            _ => part.requests,
        };
        match owner {
            Owner::Item { work, .. } => *engine.entry(work.clone()).or_default() += count,
            Owner::Unattributed(agent_tapes_core::attribution::Reason::Overlap) => {}
            Owner::Unattributed(_) => unplaced += count,
        }
    }
    assert_eq!(
        (engine["a"], engine["b"], unplaced),
        (5, 4, 3),
        "the engine's division of the fixture"
    );
    assert_eq!(
        rows_of(&view, PARTICIPANT),
        BTreeMap::from([(a1.clone(), engine["a"]), (b1.clone(), engine["b"])])
    );
    assert_eq!(
        requests(&session(&view, PARTICIPANT)["unattributed"]),
        unplaced
    );
    let rows = view["rows"].as_array().unwrap();
    let labels: Vec<&str> = rows
        .iter()
        .filter_map(|row| row["key"]["label"].as_str())
        .collect();
    assert_eq!(labels, ["change-a", "change-b"]);
}

/// A Codex session in the main checkout, its five requests at 10:00,
/// 10:10, 10:20, 10:40 and 10:50, and k1 committed on `main` at 09:00.
fn labelled_scenario() -> (Sandbox, String) {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let k1 = sandbox.commit(&repo, at("09:00:00"), "k.rs", &refs(&lines("kilo")), "k1");
    let mut codex = Codex::new(LABELLED, at("09:59:00"), &repo, "main");
    for time in ["10:00:00", "10:10:00", "10:20:00", "10:40:00", "10:50:00"] {
        codex.reply(at(time));
    }
    codex.write(&sandbox);
    (sandbox, k1)
}

/// Claim: work a binding names of which the repository holds no commit
/// lands in `labelled`, out of the total: the session's three requests on
/// k1 are the change's figure, its two on the other work are labelled, and
/// rows, uncommitted, unattributed, unselected and labelled add up to the
/// sessions' wholes, request by request and counter by counter. Rejects
/// counting labelled work into the change and dropping it.
#[test]
fn work_outside_the_repository_is_labelled_and_out_of_the_total() {
    let (sandbox, k1) = labelled_scenario();
    let file = evidence(
        &sandbox,
        "labelled.jsonl",
        &[
            binding(
                "codex",
                LABELLED,
                json!({"label": "local", "commits": [k1]}),
                "bound",
                Some((at("09:55:00"), at("10:30:00"))),
            ),
            binding(
                "codex",
                LABELLED,
                json!({"label": "other-change", "commits": [OUTSIDE]}),
                "bound",
                Some((at("10:30:00"), at("11:00:00"))),
            ),
        ],
    );
    let view = json_of(
        &sandbox,
        &[
            "--commit",
            &k1,
            "--by",
            "commit,session",
            "--evidence",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(rows_of(&view, LABELLED), BTreeMap::from([(k1.clone(), 3)]));
    let labelled = view["labelled"].as_array().unwrap();
    assert_eq!(labelled.len(), 1, "{labelled:?}");
    assert_eq!(labelled[0]["label"], "other-change");
    assert_eq!(labelled[0]["commits"], json!([OUTSIDE]));
    assert_eq!(requests(&labelled[0]), 2);
    let totals = &view["totals"];
    assert_eq!(requests(&totals["rows"]), 3);
    assert_eq!(requests(&totals["labelled"]), 2);
    assert_eq!(requests(&session(&view, LABELLED)["labelled"]), 2);
    let parts = [
        "rows",
        "uncommitted",
        "unattributed",
        "unselected",
        "labelled",
    ];
    let sum: u64 = parts.iter().map(|part| requests(&totals[part])).sum();
    assert_eq!(sum, requests(&totals["sessions"]));
    let mut held: BTreeMap<String, u64> = BTreeMap::new();
    for part in parts {
        for (name, value) in counters(&totals[part]) {
            *held.entry(name).or_default() += value;
        }
    }
    let mut whole = counters(&totals["sessions"]);
    whole.retain(|_, value| *value > 0);
    held.retain(|_, value| *value > 0);
    assert_eq!(held, whole);
}

/// Claim: a session no installed store holds is listed unresolved, with its
/// files and a fixed reason, and adds nothing; records naming one label
/// with different commits are reported with each set and its file; the
/// view still answers. Rejects a refusal, a silent drop, and a silent merge.
#[test]
fn unresolved_sessions_and_disagreeing_labels_are_reported() {
    let (sandbox, k1) = labelled_scenario();
    let repo = sandbox.repo();
    let k2 = sandbox.commit(
        &repo,
        at("09:05:00"),
        "k2.rs",
        &refs(&lines("kilo-two")),
        "k2",
    );
    let first = evidence(
        &sandbox,
        "first.jsonl",
        &[
            binding(
                "codex",
                LABELLED,
                json!({"label": "local", "commits": [k1]}),
                "bound",
                Some((at("09:55:00"), at("10:30:00"))),
            ),
            binding("claude", MISSING, json!({"commits": [k1]}), "bound", None),
        ],
    );
    let second = evidence(
        &sandbox,
        "second.jsonl",
        &[event(
            "codex",
            LABELLED,
            json!({"label": "local", "commits": [k1, k2]}),
            at("10:45:00"),
        )],
    );
    let view = json_of(
        &sandbox,
        &[
            "--commit",
            &k1,
            "--by",
            "commit,session",
            "--evidence",
            first.to_str().unwrap(),
            "--evidence",
            second.to_str().unwrap(),
        ],
    );
    let unresolved = view["evidence"]["unresolved"].as_array().unwrap();
    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert_eq!(unresolved[0]["session"]["id"], MISSING);
    assert_eq!(unresolved[0]["reason"], "not-found");
    assert_eq!(unresolved[0]["files"], json!([first.to_str().unwrap()]));
    assert!(!has_session(&view, MISSING));
    let disagreements = view["evidence"]["disagreements"].as_array().unwrap();
    assert_eq!(disagreements.len(), 1, "{disagreements:?}");
    assert_eq!(disagreements[0]["label"], "local");
    let mut sets: Vec<(Vec<String>, Vec<String>)> = disagreements[0]["sets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|set| {
            (
                serde_json::from_value(set["commits"].clone()).unwrap(),
                serde_json::from_value(set["files"].clone()).unwrap(),
            )
        })
        .collect();
    sets.sort();
    let mut both = vec![k1.clone(), k2.clone()];
    both.sort();
    let mut expected = vec![
        (vec![k1.clone()], vec![first.to_str().unwrap().to_owned()]),
        (both, vec![second.to_str().unwrap().to_owned()]),
    ];
    expected.sort();
    assert_eq!(sets, expected);
    assert!(view["notes"].as_array().unwrap().iter().any(|note| note
        .as_str()
        .unwrap()
        .contains("held by no installed store")));
}

/// Claim: a commit's amount under evidence does not depend on what else is
/// selected: a binding for a two-commit change shares its five requests
/// three and two, `--commit y1` alone gives y1 the same row as selecting
/// both, y2's part stands as unselected, and each row keeps the work's
/// label. Rejects sharing the work among the selected commits only.
#[test]
fn selecting_one_commit_of_a_change_takes_only_its_part() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let y1 = sandbox.commit(&repo, at("09:00:00"), "y1.rs", &refs(&lines("yoke")), "y1");
    let y2 = sandbox.commit(&repo, at("09:10:00"), "y2.rs", &refs(&lines("yolk")), "y2");
    let mut split = Claude::lead(SPLIT, &repo, "main");
    split.prompt(at("09:59:00"));
    for time in ["10:00:00", "10:05:00", "10:10:00", "10:15:00", "10:20:00"] {
        split.reply(at(time));
    }
    split.write(&sandbox, &repo);
    let file = evidence(
        &sandbox,
        "split.jsonl",
        &[binding(
            "claude",
            SPLIT,
            json!({"label": "two-commit", "commits": [y1, y2]}),
            "bound",
            Some((at("09:58:00"), at("10:30:00"))),
        )],
    );
    let path = file.to_str().unwrap();
    let alone = json_of(
        &sandbox,
        &[
            "--commit",
            &y1,
            "--by",
            "commit,session",
            "--evidence",
            path,
        ],
    );
    let both = json_of(
        &sandbox,
        &[
            "--commit",
            &y1,
            "--commit",
            &y2,
            "--by",
            "commit,session",
            "--evidence",
            path,
        ],
    );
    let both_rows = rows_of(&both, SPLIT);
    assert_eq!(both_rows.values().sum::<u64>(), 5);
    assert_eq!(rows_of(&alone, SPLIT).get(&y1), both_rows.get(&y1));
    let row = |view: &Value, sha: &str| {
        view["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["key"]["commit"] == sha)
            .cloned()
            .unwrap()
    };
    assert_eq!(counters(&row(&alone, &y1)), counters(&row(&both, &y1)));
    assert_eq!(row(&alone, &y1)["key"]["label"], "two-commit");
    let unselected = alone["unselected"].as_array().unwrap();
    assert_eq!(unselected.len(), 1);
    assert_eq!(unselected[0]["commit"], y2.as_str());
    assert_eq!(requests(&unselected[0]), both_rows[&y2]);
}

/// Claim: --evidence-only leaves out every session no record names: a
/// session the recordings place on z1 takes part with --evidence and is
/// gone with --evidence-only, which lists nothing. Rejects mixing listed
/// sessions into a ledger's figure.
#[test]
fn evidence_only_leaves_out_sessions_no_record_names() {
    let sandbox = Sandbox::new();
    let wt = sandbox.worktree("wt-z", "feat-z");
    let z1_lines = lines("zulu");
    let z1 = sandbox.commit(&wt, at("09:10:00"), "z.rs", &refs(&z1_lines), "z1");
    let mut stock = Claude::lead(STOCK, &wt, "feat-z");
    stock.prompt(at("09:00:00"));
    stock.reply(at("09:00:05"));
    stock.edit(at("09:05:00"), &wt.join("z.rs"), &z1_lines);
    stock.commit(at("09:10:00"), &z1, "committed", "z1");
    stock.write(&sandbox, &wt);
    let outside = sandbox.dir("elsewhere");
    fs::create_dir_all(&outside).unwrap();
    let mut named = Pi::new(WHOLE, at("12:00:00"), &outside);
    named.reply(at("12:00:05"));
    named.reply(at("12:10:00"));
    named.write(&sandbox);
    let file = evidence(
        &sandbox,
        "only.jsonl",
        &[binding(
            "pi",
            WHOLE,
            json!({"commits": [z1]}),
            "bound",
            None,
        )],
    );
    let path = file.to_str().unwrap();
    let with = json_of(&sandbox, &["--commit", &z1, "--evidence", path]);
    assert!(has_session(&with, STOCK) && has_session(&with, WHOLE));
    let only = json_of(
        &sandbox,
        &["--commit", &z1, "--evidence", path, "--evidence-only"],
    );
    assert!(!has_session(&only, STOCK), "{}", only["sessions"]);
    assert!(has_session(&only, WHOLE));
    assert_eq!(only["membership"]["listed"], 0);
    assert_eq!(only["evidence"]["only"], true);
    assert_eq!(requests(&only["totals"]["rows"]), 2);
}

/// Claim: the subagents of a session records place inherit from it alone:
/// a subagent the recordings place on q1, by its lead's worktree span at
/// the time it ran, takes the lead's binding to q2 by inheritance, all of
/// it on q2. Rejects placing the descendants of a record-placed session by
/// the recordings.
#[test]
fn a_record_placed_sessions_subagents_inherit_from_it_alone() {
    let sandbox = Sandbox::new();
    let wt = sandbox.worktree("wt-q", "feat-q");
    let q1 = sandbox.commit(&wt, at("09:20:00"), "q1.rs", &refs(&lines("quebec")), "q1");
    let q2 = sandbox.commit(&wt, at("09:30:00"), "q2.rs", &refs(&lines("quartz")), "q2");
    let mut lead = Claude::lead(LEAD, &wt, "feat-q");
    lead.prompt(at("09:00:00"));
    lead.reply(at("09:00:05"));
    lead.spawn(at("09:02:00"), at("09:12:00"), SUBAGENT);
    lead.commit(at("09:20:00"), &q1, "committed", "q1");
    lead.reply(at("09:25:00"));
    let mut subagent = Claude::agent(&lead, SUBAGENT);
    subagent.prompt(at("09:02:30"));
    subagent.reply(at("09:03:00"));
    subagent.reply(at("09:05:00"));
    subagent.reply(at("09:10:00"));
    lead.write(&sandbox, &wt);
    subagent.write(&sandbox, &wt);
    let id = format!("{LEAD}::{SUBAGENT}");
    let selection = [
        "--commit",
        q1.as_str(),
        "--commit",
        q2.as_str(),
        "--by",
        "commit,session",
    ];
    let stock = json_of(&sandbox, &selection);
    assert_eq!(
        rows_of(&stock, &id),
        BTreeMap::from([(q1.clone(), 3)]),
        "the recordings alone place it on q1"
    );

    let file = evidence(
        &sandbox,
        "lead.jsonl",
        &[binding(
            "claude",
            LEAD,
            json!({"commits": [q2]}),
            "bound",
            None,
        )],
    );
    let view = json_of(
        &sandbox,
        &[&selection[..], &["--evidence", file.to_str().unwrap()]].concat(),
    );
    assert_eq!(rows_of(&view, LEAD), BTreeMap::from([(q2.clone(), 4)]));
    assert_eq!(rows_of(&view, &id), BTreeMap::from([(q2, 3)]));
    assert_eq!(bases_of(&view, &id), ["bound/inherited"]);
    assert_eq!(session(&view, &id)["placement"], "inherited");
    assert_eq!(view["evidence"]["inherited"], 1);
}

/// Claim: a lineage record makes a markerless session a child of its
/// launcher in a change view, so the child inherits what its launcher's
/// worktree places at the same time. Rejects ignoring lineage records in a
/// change view, which leaves the child out.
#[test]
fn a_lineage_record_makes_a_child_inherit_in_a_change_view() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo();
    let wt = sandbox.worktree("wt-m", "feat-m");
    let m1_lines = lines("mike");
    let m1 = sandbox.commit(&wt, at("10:50:00"), "m.rs", &refs(&m1_lines), "m1");
    let mut launcher = Claude::lead(LAUNCHER, &wt, "feat-m");
    launcher.prompt(at("10:00:00"));
    for time in ["10:00:05", "10:10:00", "10:20:00", "10:30:00", "10:40:00"] {
        launcher.reply(at(time));
    }
    launcher.edit(at("10:45:00"), &wt.join("m.rs"), &m1_lines);
    launcher.commit(at("10:50:00"), &m1, "committed", "m1");
    launcher.write(&sandbox, &wt);
    let mut launched = Codex::new(LAUNCHED, at("10:04:00"), &repo, "main");
    for time in ["10:05:00", "10:08:00", "10:12:00"] {
        launched.reply(at(time));
    }
    launched.write(&sandbox);
    let selection = ["--commit", m1.as_str(), "--by", "commit,session"];
    let stock = json_of(&sandbox, &selection);
    assert!(
        !has_session(&stock, LAUNCHED),
        "nothing in its recording places it"
    );

    let file = evidence(
        &sandbox,
        "lineage.jsonl",
        &[
            json!({"kind": "lineage", "parent": session_ref("claude", LAUNCHER),
            "child": session_ref("codex", LAUNCHED), "tier": "bound"}),
        ],
    );
    let view = json_of(
        &sandbox,
        &[&selection[..], &["--evidence", file.to_str().unwrap()]].concat(),
    );
    assert_eq!(rows_of(&view, LAUNCHED), BTreeMap::from([(m1, 3)]));
    assert_eq!(bases_of(&view, LAUNCHED), ["matched/inherited"]);
    assert_eq!(session(&view, LAUNCHED)["parent"]["id"], LAUNCHER);
    assert_eq!(view["evidence"]["lineage_applied"], 1);
}

/// Claim: `event` is a known record kind: a valid one is read and counted,
/// one without `at` or with an unparseable `at` is malformed with its line
/// number, and an identical one counts once. Rejects counting events as an
/// unknown kind.
#[test]
fn event_records_are_read_and_validated() {
    let sandbox = Sandbox::new();
    let work = json!({"label": "auth-rework"});
    let mut missing = event("claude", BOUND, work.clone(), at("09:00:00"));
    missing.as_object_mut().unwrap().remove("at");
    let mut unparseable = event("claude", BOUND, work.clone(), at("09:00:00"));
    unparseable["at"] = json!("yesterday");
    let valid = event("claude", BOUND, work, at("09:00:00"));
    let file = evidence(
        &sandbox,
        "events.jsonl",
        &[valid.clone(), missing, unparseable, valid],
    );
    let read = read_file(Path::new(&file)).unwrap();
    assert_eq!((read.report.event.read, read.report.event.used), (2, 1));
    assert_eq!(read.report.duplicates, 1);
    assert!(read.report.unknown_kinds.is_empty());
    let malformed: Vec<(usize, Malformed)> = read
        .report
        .malformed
        .iter()
        .map(|line| (line.line, line.reason))
        .collect();
    assert_eq!(
        malformed,
        [(3, Malformed::MissingField), (4, Malformed::InvalidInstant)]
    );
    assert_eq!(read.events.len(), 1);
    assert_eq!(read.events[0].at, at("09:00:00"));
    assert_eq!(read.events[0].role.as_deref(), Some("integrator"));
}

/// Claim: without --evidence a change view carries no evidence section,
/// places every session by its recording and holds no labelled work, under
/// tapes-change-usage/4. Rejects evidence output, or placement, leaking
/// into a view that named no file.
#[test]
fn without_evidence_the_view_is_the_recordings_alone() {
    let (sandbox, x1, x2) = two_commit_session();
    let view = json_of(&sandbox, &commits(&[&x1, &x2]));
    assert_eq!(view["schema"], "tapes-change-usage/4");
    assert!(view.get("evidence").is_none());
    assert_eq!(view["labelled"], json!([]));
    for session in view["sessions"].as_array().unwrap() {
        assert_eq!(session["placement"], "recording");
    }
    let output = tapes(&sandbox, &["--commit", &x1, "--evidence-only"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--evidence"));
}
