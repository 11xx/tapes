use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_tapes_core::backend::{claude::ClaudeBackend, Backend};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    path: PathBuf,
    backend: ClaudeBackend,
}

impl Fixture {
    fn new(bytes: &[u8]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "tapes-append-probe-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("project/fixture-session.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        let backend = ClaudeBackend::new(&root);
        Self {
            root,
            path,
            backend,
        }
    }

    fn append(&self, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    fn overwrite(&self, offset: u64) {
        let mut file = OpenOptions::new().write(true).open(&self.path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"y").unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn recording(records: usize) -> Vec<u8> {
    let line = format!("{{\"type\":\"user\",\"sessionId\":\"fixture-session\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n", "x".repeat(128));
    line.repeat(records).into_bytes()
}

#[test]
fn strict_cursor_rejects_every_source_mutation_and_replay_checks_unread_bytes() {
    for case in [
        "append",
        "read-edit",
        "unread-edit",
        "overwrite-append",
        "truncate",
        "replace",
        "partial-append",
    ] {
        let original = recording(64);
        let fixture = Fixture::new(&original);
        let session = fixture.backend.locate("fixture-session").unwrap().unwrap();
        let page = fixture.backend.history_page(&session, None, 1024).unwrap();
        let cursor = page.next_cursor.as_deref().unwrap();
        let pin = fixture
            .backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap();
        assert!(fixture
            .backend
            .history_page(&session, Some(cursor), 1024)
            .is_ok());
        match case {
            "append" => fixture.append(&recording(1)),
            "partial-append" => fixture.append(b"{\"type\":"),
            "read-edit" => fixture.overwrite(original.len() as u64 - 10),
            "unread-edit" => fixture.overwrite(110),
            "overwrite-append" => {
                fixture.overwrite(110);
                fixture.append(&recording(1));
            }
            "truncate" => OpenOptions::new()
                .write(true)
                .open(&fixture.path)
                .unwrap()
                .set_len(2048)
                .unwrap(),
            "replace" => {
                let replacement = fixture.root.join("replacement");
                fs::write(&replacement, &original).unwrap();
                fs::rename(replacement, &fixture.path).unwrap();
            }
            _ => unreachable!(),
        }
        let error = fixture
            .backend
            .history_page(&session, Some(cursor), 1024)
            .unwrap_err();
        assert!(
            error.to_string().contains("history source changed"),
            "{case}: {error}"
        );
        let mut turns = 0;
        let replay = fixture
            .backend
            .stream_transcript(&session, Some(&pin), &mut |_| {
                turns += 1;
                Ok(())
            });
        if matches!(case, "append" | "partial-append") {
            let replay = replay.unwrap();
            assert_eq!(replay.source_length, original.len() as u64);
            assert_eq!(turns, 64);
        } else {
            assert!(replay.is_err(), "{case}");
        }
    }
}

#[test]
fn strict_pages_report_partial_malformed_and_page_oversized_records() {
    for suffix in [b"{\"type\":".as_slice(), b"not-json\n".as_slice()] {
        let mut bytes = recording(16);
        bytes.extend_from_slice(suffix);
        let fixture = Fixture::new(&bytes);
        let session = fixture.backend.locate("fixture-session").unwrap().unwrap();
        let page = fixture.backend.history_page(&session, None, 1024).unwrap();
        assert_eq!(page.skipped_records, 1);
        assert!(page
            .read
            .gaps
            .iter()
            .any(|gap| gap.reason == "malformed-record"));
        let pin = fixture
            .backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap();
        assert_eq!(pin.skipped, 1);
        assert!(fixture
            .backend
            .stream_transcript(&session, Some(&pin), &mut |_| Ok(()))
            .is_ok());
        fixture.append(b"}\n");
        assert!(fixture
            .backend
            .history_page(&session, page.next_cursor.as_deref(), 1024)
            .is_err());
    }
    let mut bytes = recording(8);
    bytes.extend_from_slice(format!("{{\"padding\":\"{}\"}}\n", "x".repeat(4096)).as_bytes());
    bytes.extend_from_slice(&recording(8));
    let fixture = Fixture::new(&bytes);
    let session = fixture.backend.locate("fixture-session").unwrap().unwrap();
    let mut cursor = None;
    let mut fragments = 0;
    let mut turns = 0;
    loop {
        let page = fixture
            .backend
            .history_page(&session, cursor.as_deref(), 1024)
            .unwrap();
        assert!(page.bytes_read <= 1024);
        assert!(page.alignment_bytes <= 1);
        fragments += page.skipped_fragment_bytes;
        turns += page.turns.len();
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert!(fragments >= 4096);
    assert_eq!(turns, 16);
}

#[test]
fn streamed_replay_hashes_oversized_gap_bytes() {
    let fixture = Fixture::new(&recording(8));
    let mut file = OpenOptions::new().append(true).open(&fixture.path).unwrap();
    let block = vec![b'x'; 1024 * 1024];
    for _ in 0..65 {
        file.write_all(&block).unwrap();
    }
    file.write_all(b"\n").unwrap();
    file.write_all(&recording(8)).unwrap();
    drop(file);
    let session = fixture.backend.locate("fixture-session").unwrap().unwrap();
    let pin = fixture
        .backend
        .stream_transcript(&session, None, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(pin.skipped, 1);
    assert_eq!(pin.gaps[0].reason, "oversized-record");
    assert!(fixture
        .backend
        .stream_transcript(&session, Some(&pin), &mut |_| Ok(()))
        .is_ok());
    fixture.overwrite(pin.gaps[0].span.start + 100);
    assert!(fixture
        .backend
        .stream_transcript(&session, Some(&pin), &mut |_| Ok(()))
        .is_err());
}
