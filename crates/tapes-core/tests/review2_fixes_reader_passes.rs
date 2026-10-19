//! A context reading the lineage of many Codex sessions opens each rollout
//! once for its header, the open discovery makes, besides the session's
//! own read of its recording; the child index reads no header again.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{codex::CodexBackend, Backend};
use serde_json::json;

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-review2-passes-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("2026/01/09")).unwrap();
        Self(root)
    }

    fn rollouts(&self) -> PathBuf {
        self.0.join("2026/01/09")
    }

    fn write(&self, hour: u32, id: &str, parent: Option<&str>) {
        let mut payload = json!({"id": id, "session_id": id,
            "timestamp": format!("2026-01-09T{hour:02}:00:00Z"), "cwd": "/fixtures/project"});
        if let Some(parent) = parent {
            payload["parent_thread_id"] = json!(parent);
            payload["agent_path"] = json!(format!("/root/{id}"));
        }
        let lines = [
            json!({"timestamp": format!("2026-01-09T{hour:02}:00:00Z"), "type": "session_meta",
                   "payload": payload}),
            json!({"timestamp": format!("2026-01-09T{hour:02}:00:01Z"), "type": "response_item",
                   "payload": {"type": "message", "role": "user",
                               "content": [{"type": "input_text", "text": "Inspect the fixture."}]}}),
        ];
        let body = lines.map(|line| line.to_string()).join("\n") + "\n";
        fs::write(
            self.rollouts()
                .join(format!("rollout-2026-01-09T{hour:02}-00-00-{id}.jsonl")),
            body,
        )
        .unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The opens of the files in one directory, as the kernel reports them.
struct Opens(i32);

impl Opens {
    fn watch(directory: &Path) -> Self {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "inotify_init1");
        let path = CString::new(directory.as_os_str().as_bytes()).unwrap();
        let watch = unsafe { libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_OPEN) };
        assert!(watch >= 0, "inotify_add_watch");
        Self(fd)
    }

    /// The opens reported since the last call, by file name.
    fn drain(&self) -> BTreeMap<String, usize> {
        let mut opens = BTreeMap::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = unsafe { libc::read(self.0, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read <= 0 {
                return opens;
            }
            let mut offset = 0;
            while offset < read as usize {
                let event = unsafe {
                    std::ptr::read_unaligned(
                        buffer[offset..].as_ptr().cast::<libc::inotify_event>(),
                    )
                };
                let name_start = offset + size_of::<libc::inotify_event>();
                let name = &buffer[name_start..name_start + event.len as usize];
                let name = String::from_utf8_lossy(name)
                    .trim_end_matches('\0')
                    .to_owned();
                if !name.is_empty() {
                    *opens.entry(name).or_insert(0) += 1;
                }
                offset = name_start + event.len as usize;
            }
        }
    }
}

impl Drop for Opens {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

const PARENT: &str = "01a30000-0000-7000-8000-000000000001";
const CHILDREN: [&str; 2] = [
    "01a30000-0000-7000-8000-000000000002",
    "01a30000-0000-7000-8000-000000000003",
];
const OTHERS: [&str; 3] = [
    "01a30000-0000-7000-8000-000000000004",
    "01a30000-0000-7000-8000-000000000005",
    "01a30000-0000-7000-8000-000000000006",
];

/// Claim: reading the lineage of every session of a Codex store through one
/// backend opens each rollout at most twice, once for the header discovery
/// reads and once for the session's own recording, and still finds each
/// parent's children. Rejects a child index that opens every header again
/// after discovery has, three opens per rollout.
#[test]
fn lineage_reads_open_each_rollout_header_once() {
    let store = Store::new("opens");
    store.write(10, PARENT, None);
    for (hour, child) in (11..).zip(CHILDREN) {
        store.write(hour, child, Some(PARENT));
    }
    for (hour, other) in (13..).zip(OTHERS) {
        store.write(hour, other, None);
    }
    let backend = CodexBackend::new(&store.0);
    let ids = std::iter::once(PARENT)
        .chain(CHILDREN)
        .chain(OTHERS)
        .collect::<Vec<_>>();
    let sessions = ids
        .iter()
        .map(|id| backend.locate(id).unwrap().unwrap())
        .collect::<Vec<_>>();

    let opens = Opens::watch(&store.rollouts());
    let mut found = BTreeMap::new();
    for session in &sessions {
        let mut children = backend
            .lineage(session)
            .unwrap()
            .children
            .into_iter()
            .filter_map(|child| child.session_id)
            .collect::<Vec<_>>();
        children.sort();
        found.insert(session.id.clone(), children);
    }
    let opened = opens.drain();

    assert_eq!(
        found[PARENT],
        CHILDREN.map(str::to_owned).to_vec(),
        "the parent's children"
    );
    assert!(found
        .iter()
        .filter(|(id, _)| id.as_str() != PARENT)
        .all(|(_, children)| children.is_empty()));
    assert_eq!(
        opened.len(),
        ids.len(),
        "every rollout is opened: {opened:?}"
    );
    for (name, count) in &opened {
        assert!(
            *count <= 2,
            "{name} opened {count} times by {} lineage reads: {opened:?}",
            ids.len()
        );
    }
}
