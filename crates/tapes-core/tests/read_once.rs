//! A listing opens only the recordings that can hold activity inside its
//! window, and a change view decodes each member's recording no more often
//! than its evidence, the replay that pairs tool calls, and its span figures
//! need. Each test names the caller-visible claim it holds and the wrong
//! behaviour it rejects.

#[path = "support/recordings.rs"]
mod recordings;

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration as StdDuration, SystemTime};

use agent_tapes_core::backend::claude::ClaudeBackend;
use agent_tapes_core::backend::codex::CodexBackend;
use agent_tapes_core::backend::pi::PiBackend;
use agent_tapes_core::backend::{Backend, Listing, Query, QueryFilters};
use agent_tapes_core::change_attribution::{attribute, gather, MemberRule, Params};
use agent_tapes_core::counters;
use agent_tapes_core::model::OperatorOrigin;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};

use recordings::*;

/// The counters are process-wide, so every test that streams a recording
/// runs one at a time.
static COUNTED: Mutex<()> = Mutex::new(());

const OLD: &str = "e1000000-0000-4000-8000-000000000001";
const RECENT: &str = "e1000000-0000-4000-8000-000000000002";
const OTHER: &str = "e1000000-0000-4000-8000-0000000000ff";
const LONG: &str = "e1000000-0000-4000-8000-000000000003";
const PARENT: &str = "e1000000-0000-4000-8000-000000000004";
const MEMBER: &str = "e1000000-0000-4000-8000-000000000005";

fn modified(path: &Path, when: DateTime<Utc>) {
    let file = fs::File::options().write(true).open(path).unwrap();
    let when = SystemTime::UNIX_EPOCH + StdDuration::from_secs(when.timestamp() as u64);
    file.set_times(fs::FileTimes::new().set_modified(when))
        .unwrap();
}

/// The one recording under `root` whose name carries `id`.
fn recording_of(root: &Path, id: &str) -> PathBuf {
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.to_string_lossy().contains(id) && path.extension().is_some() {
                return path;
            }
        }
    }
    panic!("no recording of {id} under {}", root.display());
}

fn append(path: &Path, record: &Value) {
    let mut body = fs::read_to_string(path).unwrap();
    body.push_str(&format!("{record}\n"));
    fs::write(path, body).unwrap();
}

/// The three harnesses' file stores, each able to hold a session whose
/// header names it and whose last records can be made to name another, so
/// that normalizing it fails and the failure shows in the listing.
#[derive(Clone, Copy, Debug)]
enum Harness {
    Claude,
    Codex,
    Pi,
}

const HARNESSES: [Harness; 3] = [Harness::Claude, Harness::Codex, Harness::Pi];

impl Harness {
    fn root(self, sandbox: &Sandbox) -> PathBuf {
        sandbox.dir(match self {
            Self::Claude => "claude/projects",
            Self::Codex => "codex/sessions",
            Self::Pi => "pi/sessions",
        })
    }

    fn backend(self, sandbox: &Sandbox) -> Box<dyn Backend> {
        match self {
            Self::Claude => Box::new(ClaudeBackend::new(self.root(sandbox))),
            Self::Codex => Box::new(CodexBackend::new(self.root(sandbox))),
            Self::Pi => Box::new(PiBackend::new(self.root(sandbox))),
        }
    }

    /// Record a session that worked from `started` to `ended`, and set its
    /// file's modification time to `written`.
    fn write(
        self,
        sandbox: &Sandbox,
        id: &str,
        started: Time,
        ended: Time,
        written: Time,
    ) -> PathBuf {
        let cwd = sandbox.repo();
        match self {
            Self::Claude => {
                let mut claude = Claude::lead(id, &cwd, "main");
                claude.prompt(started);
                claude.reply(ended);
                claude.write(sandbox, &cwd);
            }
            Self::Codex => {
                let mut codex = Codex::new(id, started, &cwd, "main");
                codex.reply(ended);
                codex.write(sandbox);
            }
            Self::Pi => {
                let mut pi = Pi::new(id, started, &cwd);
                pi.reply(ended);
                pi.write(sandbox);
            }
        }
        let path = recording_of(&self.root(sandbox), id);
        modified(&path, written);
        path
    }

    /// Make normalizing the recording fail: a record past the opening probe
    /// names another session. The opening is untouched, so the file is still
    /// a candidate, and only a parse of it finds the fault.
    fn corrupt(self, path: &Path, written: Time) {
        let foreign = match self {
            Self::Claude => json!({"type": "user", "sessionId": OTHER,
                "timestamp": stamp(written), "message": {"role": "user", "content": "x"}}),
            Self::Codex => json!({"type": "session_meta", "timestamp": stamp(written),
                "payload": {"id": OTHER}}),
            Self::Pi => json!({"type": "session", "id": OTHER, "timestamp": stamp(written)}),
        };
        append(
            path,
            &json!({"type": "padding", "timestamp": stamp(written), "text": "x".repeat(96 * 1024)}),
        );
        append(path, &foreign);
        modified(path, written);
    }
}

fn list(backend: &dyn Backend, since: Option<Time>) -> Listing {
    backend
        .list(&Query::unscoped_with_filters(
            100,
            QueryFilters {
                since,
                ..Default::default()
            },
        ))
        .unwrap()
}

fn ids(listing: &Listing) -> Vec<String> {
    let mut ids: Vec<String> = listing
        .sessions
        .iter()
        .map(|session| session.id.clone())
        .collect();
    ids.sort();
    ids
}

/// A listing of a window never opens a recording last written before the
/// window began, by more than a day: a file whose contents cannot be
/// normalized shows in the listing only when it was parsed.
#[test]
fn a_recording_written_before_the_window_is_skipped_without_parsing() {
    for harness in HARNESSES {
        let sandbox = Sandbox::new();
        let long_ago = at("09:00:00") - Duration::days(60);
        let old = harness.write(&sandbox, OLD, long_ago, long_ago, long_ago);
        harness.corrupt(&old, long_ago);
        harness.write(
            &sandbox,
            RECENT,
            at("09:00:00"),
            at("09:30:00"),
            at("09:30:00"),
        );
        let backend = harness.backend(&sandbox);

        let everything = list(backend.as_ref(), None);
        assert_eq!(
            everything.unavailable.len(),
            1,
            "{harness:?}: the corrupted recording is unreadable when parsed"
        );

        let window = list(backend.as_ref(), Some(at("08:00:00")));
        assert_eq!(ids(&window), [RECENT], "{harness:?}");
        assert!(
            window.unavailable.is_empty(),
            "{harness:?}: the old recording was parsed before the window dropped it: {:?}",
            window.unavailable
        );
    }
}

/// The skip leaves a day of slack: a recording written up to a day before the
/// window began is still read, so clock skew and coarse file times cannot
/// hide a session.
#[test]
fn a_recording_written_within_a_day_of_the_window_is_still_parsed() {
    for harness in HARNESSES {
        let sandbox = Sandbox::new();
        let written = at("08:00:00") - Duration::hours(20);
        let path = harness.write(&sandbox, OLD, written, written, written);
        harness.corrupt(&path, written);
        let backend = harness.backend(&sandbox);

        let window = list(backend.as_ref(), Some(at("08:00:00")));
        assert_eq!(
            window.unavailable.len(),
            1,
            "{harness:?}: a recording inside the slack must be parsed"
        );
    }
}

/// A session that began long before the window and was active inside it is
/// listed: the skip reads the file's modification time, never the time its
/// first record carries.
#[test]
fn a_session_begun_long_ago_and_active_inside_the_window_is_listed() {
    for harness in HARNESSES {
        let sandbox = Sandbox::new();
        harness.write(
            &sandbox,
            LONG,
            at("09:00:00") - Duration::days(60),
            at("09:30:00"),
            at("09:30:00"),
        );
        let backend = harness.backend(&sandbox);

        let window = list(backend.as_ref(), Some(at("08:00:00")));
        assert_eq!(ids(&window), [LONG], "{harness:?}");
    }
}

/// A file last written long ago cannot hold a subagent written later: a
/// Claude parent whose own recording is old is still nominated by a
/// subagent recording written since the window began, and a parent whose
/// subagent is newer than its own file is listed and gathered with it.
#[test]
fn a_subagent_newer_than_its_parent_file_hides_neither() {
    let _counted = COUNTED.lock().unwrap_or_else(|e| e.into_inner());
    let sandbox = Sandbox::new();
    let cwd = sandbox.repo();
    let long_ago = at("09:00:00") - Duration::days(60);
    let mut parent = Claude::lead(PARENT, &cwd, "main");
    parent.prompt(long_ago);
    parent.reply(long_ago + Duration::minutes(1));
    let mut agent = Claude::agent(&parent, "a1b2c3");
    agent.prompt(at("09:00:00"));
    agent.reply(at("09:10:00"));
    parent.write(&sandbox, &cwd);
    agent.write(&sandbox, &cwd);
    let root = Harness::Claude.root(&sandbox);
    modified(&recording_of(&root, &format!("{PARENT}.jsonl")), long_ago);
    modified(&recording_of(&root, "agent-a1b2c3.jsonl"), at("09:10:00"));
    let backend = ClaudeBackend::new(&root);

    let (nominated, failures) = backend.window_parents(at("08:00:00"));
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(ids(&Listing::from_sessions(nominated)), [PARENT]);
    assert!(
        ids(&list(&backend, Some(at("08:00:00")))).is_empty(),
        "the parent's own records end before the window"
    );

    // A parent active inside the window, whose subagent was written after it.
    let mut active = Claude::lead(MEMBER, &cwd, "main");
    active.prompt(at("09:00:00"));
    active.reply(at("09:05:00"));
    let mut helper = Claude::agent(&active, "d4e5f6");
    helper.prompt(at("09:06:00"));
    helper.reply(at("09:20:00"));
    active.write(&sandbox, &cwd);
    helper.write(&sandbox, &cwd);
    modified(
        &recording_of(&root, &format!("{MEMBER}.jsonl")),
        at("09:05:00"),
    );
    modified(&recording_of(&root, "agent-d4e5f6.jsonl"), at("09:20:00"));
    let backend = ClaudeBackend::new(&root);
    assert_eq!(ids(&list(&backend, Some(at("08:00:00")))), [MEMBER]);
    let backends = sandbox.backends();
    let gathered = gather(
        &backends,
        &sandbox.repository(),
        at("08:00:00"),
        at("18:00:00"),
    )
    .unwrap();
    let children: Vec<_> = gathered
        .members
        .iter()
        .filter(|member| member.rules.contains(&MemberRule::Child))
        .map(|member| member.session.id.clone())
        .collect();
    assert_eq!(children, [format!("{MEMBER}::d4e5f6")]);
}

/// Listing with the skip equals listing everything and keeping the sessions
/// whose newest activity is inside the window, whatever each file's
/// modification time is relative to the window.
#[test]
fn a_listing_with_the_skip_equals_the_full_listing_filtered_afterwards() {
    for harness in HARNESSES {
        let sandbox = Sandbox::new();
        let since = at("12:00:00");
        let cases = [
            // id suffix, started, ended, written
            ("10", at("01:00:00"), at("02:00:00"), at("02:00:00")),
            ("11", at("01:00:00"), at("11:59:00"), at("11:59:00")),
            ("12", at("01:00:00"), at("12:00:00"), at("12:00:00")),
            ("13", at("01:00:00"), at("02:00:00"), at("13:00:00")),
            ("14", at("11:00:00"), at("13:30:00"), at("13:30:00")),
            (
                "15",
                at("01:00:00"),
                at("12:30:00"),
                at("12:30:00") + Duration::days(40),
            ),
            (
                "16",
                at("01:00:00"),
                at("12:30:00"),
                at("12:30:00") - Duration::hours(3),
            ),
        ];
        for (suffix, started, ended, written) in cases {
            let id = format!("e1000000-0000-4000-8000-0000000001{suffix}");
            harness.write(&sandbox, &id, started, ended, written);
        }
        let backend = harness.backend(&sandbox);

        let mut expected: Vec<String> = list(backend.as_ref(), None)
            .sessions
            .into_iter()
            .filter(|session| session.last_activity_at.is_some_and(|last| last >= since))
            .map(|session| session.id)
            .collect();
        expected.sort();
        assert_eq!(expected.len(), 4, "{harness:?}");
        assert_eq!(
            ids(&list(backend.as_ref(), Some(since))),
            expected,
            "{harness:?}"
        );
    }
}

// ---------------------------------------------------------------- reads

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).unwrap().len()
}

/// Decode a Claude member's recording through the change view and report the
/// bytes streaming passes decoded beside the recording's size.
fn decoded_for_view(write: impl FnOnce(&Sandbox) -> PathBuf) -> (u64, u64, usize) {
    let sandbox = Sandbox::new();
    let path = write(&sandbox);
    let size = file_size(&path);
    let backends = sandbox.backends();
    let repository = sandbox.repository();
    let passes = counters::recording_passes();
    let decoded = counters::decoded_bytes();
    let gathered = gather(&backends, &repository, at("07:00:00"), at("18:00:00")).unwrap();
    assert_eq!(gathered.members.len(), 1, "the session is a member");
    attribute(&backends, &repository, &gathered, &Params::default()).unwrap();
    (
        counters::decoded_bytes() - decoded,
        size,
        counters::recording_passes() - passes,
    )
}

/// A change view decodes a member's recording at most three times: its
/// evidence pass, the replay that pairs its tool calls, and its span
/// figures. The pass that settles the launch decodes nothing, and the replay
/// does not make it again.
#[test]
fn a_claude_member_is_decoded_no_more_than_three_times() {
    let _counted = COUNTED.lock().unwrap_or_else(|e| e.into_inner());
    let (decoded, size, passes) = decoded_for_view(|sandbox| {
        let cwd = sandbox.repo();
        let mut claude = Claude::lead(MEMBER, &cwd, "main");
        claude.prompt(at("09:00:00"));
        claude.bash(at("09:01:00"), "git status", "clean");
        claude.reply(at("09:05:00"));
        claude.write(sandbox, &cwd);
        recording_of(&Harness::Claude.root(sandbox), MEMBER)
    });
    assert!(
        decoded <= 3 * size,
        "decoded {decoded} bytes of a {size}-byte recording"
    );
    assert!(passes <= 4, "{passes} passes over the recording");
}

/// The same bound holds for a Codex member, whose evidence read is a pass and
/// its replay, and whose spans are a third.
#[test]
fn a_codex_member_is_decoded_no_more_than_three_times() {
    let _counted = COUNTED.lock().unwrap_or_else(|e| e.into_inner());
    let (decoded, size, passes) = decoded_for_view(|sandbox| {
        let mut codex = Codex::new(MEMBER, at("09:00:00"), &sandbox.repo(), "main");
        codex.exec(at("09:01:00"), "git status", "clean");
        codex.reply(at("09:05:00"));
        codex.write(sandbox);
        recording_of(&Harness::Codex.root(sandbox), MEMBER)
    });
    assert!(
        decoded <= 3 * size,
        "decoded {decoded} bytes of a {size}-byte recording"
    );
    assert!(passes <= 3, "{passes} passes over the recording");
}

fn claude_store(lines: &[String]) -> (Sandbox, ClaudeBackend, agent_tapes_core::model::Session) {
    let sandbox = Sandbox::new();
    let project = Claude::project(&sandbox, &sandbox.repo());
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join(format!("{MEMBER}.jsonl")),
        lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let backend = ClaudeBackend::new(sandbox.dir("claude/projects"));
    let session = backend.locate(MEMBER).unwrap().unwrap();
    (sandbox, backend, session)
}

/// One typed prompt per entrypoint, as record lines.
fn prompts(entrypoints: &[&str]) -> Vec<String> {
    let mut claude = Claude::lead(MEMBER, Path::new("/work"), "main");
    for (index, entrypoint) in entrypoints.iter().enumerate() {
        claude.prompt(at("09:00:00") + Duration::minutes(index as i64));
        let prompt = claude.records.last_mut().unwrap();
        prompt["entrypoint"] = json!(entrypoint);
        prompt["promptSource"] = json!("typed");
    }
    claude.records.iter().map(Value::to_string).collect()
}

/// The origins of the turns of a first read and of its replay.
fn origins(
    backend: &ClaudeBackend,
    session: &agent_tapes_core::model::Session,
) -> [Vec<Option<OperatorOrigin>>; 2] {
    let first = RefCell::new(Vec::new());
    let read = backend
        .stream_transcript(session, None, &mut |turn| {
            first.borrow_mut().push(turn.origin);
            Ok(())
        })
        .unwrap();
    let replay = RefCell::new(Vec::new());
    backend
        .stream_transcript(session, Some(&read), &mut |turn| {
            replay.borrow_mut().push(turn.origin);
            Ok(())
        })
        .unwrap();
    [first.into_inner(), replay.into_inner()]
}

/// The launch of a whole recording decides the origin of every operator turn:
/// a headless session's prompts come from a program, and one interactive
/// record anywhere makes them a person's. A replay hands over the same
/// origins as the read it replays, and the launch is read from a record's
/// line as a JSON reader would: the last of a repeated key stands, and an
/// escaped `type` is the type it spells.
#[test]
fn a_launch_found_without_decoding_orders_every_turn_as_before() {
    let _counted = COUNTED.lock().unwrap_or_else(|e| e.into_inner());
    let (_sandbox, backend, session) = claude_store(&prompts(&["sdk-cli", "sdk-cli"]));
    for turns in origins(&backend, &session) {
        assert_eq!(turns, [Some(OperatorOrigin::Program); 2]);
    }

    let (_sandbox, backend, session) = claude_store(&prompts(&["sdk-cli", "cli"]));
    for turns in origins(&backend, &session) {
        assert!(!turns.contains(&Some(OperatorOrigin::Program)), "{turns:?}");
    }

    let mut repeated = prompts(&["sdk-cli"]);
    repeated.push(r#"{"type":"user","entrypoint":"sdk-cli","entrypoint":"cli"}"#.to_owned());
    let (_sandbox, backend, session) = claude_store(&repeated);
    for turns in origins(&backend, &session) {
        assert!(!turns.contains(&Some(OperatorOrigin::Program)), "{turns:?}");
    }

    let escaped = prompts(&["sdk-cli"])[0].replace(r#""type":"user""#, r#""type":"us\u0065r""#);
    assert!(escaped.contains(r"us\u0065r"));
    let (_sandbox, backend, session) = claude_store(&[escaped]);
    for turns in origins(&backend, &session) {
        assert_eq!(turns, [Some(OperatorOrigin::Program)]);
    }
}

/// A first read of a Claude recording decodes each record once, and its
/// replay decodes each record once: finding the launch decodes none.
#[test]
fn a_streamed_read_and_its_replay_each_decode_a_recording_once() {
    let _counted = COUNTED.lock().unwrap_or_else(|e| e.into_inner());
    let records = prompts(&["cli", "cli", "cli"]);
    let (sandbox, backend, session) = claude_store(&records);
    let size = file_size(&recording_of(&sandbox.dir("claude/projects"), MEMBER));

    let before = counters::decoded_bytes();
    let read = backend
        .stream_transcript(&session, None, &mut |_| Ok(()))
        .unwrap();
    let first = counters::decoded_bytes() - before;
    backend
        .stream_transcript(&session, Some(&read), &mut |_| Ok(()))
        .unwrap();
    let replay = counters::decoded_bytes() - before - first;

    assert!(first <= size, "first read decoded {first} of {size} bytes");
    assert!(replay <= size, "replay decoded {replay} of {size} bytes");
}
