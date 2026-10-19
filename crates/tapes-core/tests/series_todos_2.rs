//! A Claude id lookup answers from identities a walk reads in filenames and
//! headers, so a miss does not depend on unrelated live recordings, and a
//! recording that changes under a read is read again.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, Backend};
use agent_tapes_core::resolve_session;
use agent_tapes_discovery::ResolveError;

const LIVE: &str = "6f1d2c3b-4a59-4e68-9b7a-1c2d3e4f5a6b";
const TARGET: &str = "0a9b8c7d-6e5f-4a3b-8c2d-1e0f9a8b7c6d";
const MISSING: &str = "deadbeef-0000-4000-8000-000000000000";

struct Store(PathBuf);

impl Store {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-series-todos-2-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("-fixtures-project")).unwrap();
        Self(root)
    }

    fn recording(&self, id: &str) -> PathBuf {
        self.0.join("-fixtures-project").join(format!("{id}.jsonl"))
    }

    /// A Claude recording of `id` holding `turns` request/answer pairs.
    fn write(&self, id: &str, turns: usize) -> PathBuf {
        let path = self.recording(id);
        let mut file = fs::File::create(&path).unwrap();
        for turn in 0..turns {
            file.write_all(user(id, turn).as_bytes()).unwrap();
            file.write_all(assistant(id, turn).as_bytes()).unwrap();
        }
        path
    }

    fn backend(&self) -> Vec<Box<dyn Backend>> {
        vec![Box::new(ClaudeBackend::new(&self.0))]
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn user(id: &str, turn: usize) -> String {
    format!(
        "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"uuid\":\"user-{turn}\",\"parentUuid\":null,\"timestamp\":\"2026-01-01T10:00:00Z\",\"cwd\":\"/fixtures/project\",\"message\":{{\"role\":\"user\",\"content\":\"Request {turn} {pad}\"}}}}\n",
        pad = "x".repeat(400)
    )
}

fn assistant(id: &str, turn: usize) -> String {
    format!(
        "{{\"type\":\"assistant\",\"sessionId\":\"{id}\",\"uuid\":\"assistant-{turn}\",\"parentUuid\":\"user-{turn}\",\"timestamp\":\"2026-01-01T10:00:01Z\",\"cwd\":\"/fixtures/project\",\"message\":{{\"role\":\"assistant\",\"model\":\"claude-fixture\",\"stop_reason\":\"end_turn\",\"content\":[{{\"type\":\"text\",\"text\":\"Answer {turn}\"}}]}}}}\n"
    )
}

/// Appends a record to `path` every few hundred microseconds, as a live
/// session does, until the guard finishes or drops.
struct Appender {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<usize>>,
}

impl Appender {
    fn start(path: &Path, id: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let path = path.to_owned();
        let line = user(id, 0);
        let thread = std::thread::spawn(move || {
            let mut file = OpenOptions::new().append(true).open(path).unwrap();
            let mut appended = 0;
            while !flag.load(Ordering::Relaxed) {
                file.write_all(line.as_bytes()).unwrap();
                appended += 1;
                std::thread::sleep(Duration::from_micros(300));
            }
            appended
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> usize {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap()
    }
}

impl Drop for Appender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Claim: a lookup for an id no recording holds answers not-found while an
/// unrelated recording is being appended to. Rejects a lookup that opens
/// every recording whole and reports the unrelated one's change as
/// incomplete coverage instead of absence.
#[test]
fn a_missing_id_is_not_found_while_an_unrelated_recording_grows() {
    let store = Store::new("missing");
    // Past the default 4 MiB read, so a whole read of it takes head and tail.
    let live = store.write(LIVE, 12_000);
    store.write(TARGET, 2);
    let appender = Appender::start(&live, LIVE);
    for query in [MISSING, "deadbeef"] {
        for _ in 0..3 {
            match resolve_session(&store.backend(), query) {
                Err(ResolveError::NotFound { truncated, .. }) => assert!(!truncated),
                other => panic!("{query}: expected not-found, got {other:?}"),
            }
        }
    }
    let resolved = resolve_session(&store.backend(), &TARGET[..8]).unwrap();
    assert_eq!(resolved.session.id, TARGET);
    assert!(appender.finish() > 0);
}

/// Appends one record to `path` on the first read after its `opens`th
/// open, from inside that read's window, and answers whether it did.
fn append_inside_read(path: &Path, id: &str, opens: usize) -> JoinHandle<bool> {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    let line = user(id, 0);
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let watch = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    assert!(watch >= 0);
    let added =
        unsafe { libc::inotify_add_watch(watch, name.as_ptr(), libc::IN_OPEN | libc::IN_ACCESS) };
    assert!(added >= 0);
    std::thread::spawn(move || {
        let mut seen_opens = 0;
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(30);
        let appended = 'watch: loop {
            let mut poll = libc::pollfd {
                fd: watch,
                events: libc::POLLIN,
                revents: 0,
            };
            if Instant::now() >= deadline || unsafe { libc::poll(&mut poll, 1, 1_000) } < 0 {
                break false;
            }
            if poll.revents & libc::POLLIN == 0 {
                continue;
            }
            let read = unsafe { libc::read(watch, buffer.as_mut_ptr().cast(), buffer.len()) };
            assert!(read > 0);
            let mut offset = 0;
            while offset < read as usize {
                let event = unsafe {
                    std::ptr::read_unaligned(
                        buffer[offset..].as_ptr().cast::<libc::inotify_event>(),
                    )
                };
                offset += size_of::<libc::inotify_event>() + event.len as usize;
                if event.mask & libc::IN_OPEN != 0 {
                    seen_opens += 1;
                }
                if event.mask & libc::IN_ACCESS != 0 && seen_opens >= opens {
                    file.write_all(line.as_bytes()).unwrap();
                    break 'watch true;
                }
            }
        };
        unsafe { libc::close(watch) };
        appended
    })
}

/// Claim: a recording appended to while it is read is read again, so the
/// lookup that names it resolves. Rejects a single read that reports the
/// change as a failure.
#[test]
fn a_recording_that_changes_mid_read_is_read_again() {
    let store = Store::new("retry");
    // Past the default 4 MiB read, so the read spans a head and a tail.
    let target = store.write(TARGET, 12_000);
    let before = fs::metadata(&target).unwrap().len();
    // A prefix lookup opens the recording once for its header and once for
    // the whole read.
    let watcher = append_inside_read(&target, TARGET, 2);
    let resolved = resolve_session(&store.backend(), &TARGET[..8]);
    assert!(
        watcher.join().unwrap(),
        "the read never reached the recording"
    );
    assert!(fs::metadata(&target).unwrap().len() > before);
    assert_eq!(resolved.unwrap().session.id, TARGET);
}

/// Claim: a walk that could not inspect a candidate never claims absence,
/// because that candidate's header may hold the queried id. Rejects a
/// not-found answer over an unreadable recording.
#[test]
fn an_uninspectable_candidate_keeps_a_miss_from_reading_as_absence() {
    let store = Store::new("unreadable");
    store.write(TARGET, 2);
    let hidden = store.write(LIVE, 2);
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&hidden).is_ok() {
        eprintln!("skipped: permissions do not bind this user");
        return;
    }
    let outcome = resolve_session(&store.backend(), MISSING);
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o600)).unwrap();
    match outcome {
        Err(ResolveError::NotFound { .. }) | Ok(_) => {
            panic!("expected incomplete coverage, got {outcome:?}")
        }
        Err(_) => {}
    }
}
