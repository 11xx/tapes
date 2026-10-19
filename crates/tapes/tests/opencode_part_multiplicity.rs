//! Every unreadable record of an OpenCode database counts once in a read's
//! skipped-record note, bounded or whole (`skipped`), while a whole read marks one
//! `malformed-record` gap per message position that holds any. Two unreadable
//! parts of one message, or a message whose data does not decode beside its
//! own unreadable part, are two records in one gap.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const SESSION: &str = "ses_part_multiplicity_fixture";

struct Root(PathBuf);

impl Root {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tapes-opencode-part-multiplicity-{name}-{}-{}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("home/.local/share/opencode")).unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode/opencode2");
        std::os::unix::fs::symlink(fixture, path.join("opencode")).unwrap();
        Self(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Every distinct `sqlite3` on PATH: output modes differ between versions.
fn sqlite3_binaries() -> Vec<PathBuf> {
    let mut binaries: Vec<PathBuf> = Vec::new();
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let Ok(binary) = directory.join("sqlite3").canonicalize() else {
            continue;
        };
        if binary.is_file() && !binaries.contains(&binary) {
            binaries.push(binary);
        }
    }
    if binaries.is_empty() {
        eprintln!("skipping OpenCode database test: sqlite3 is absent from PATH");
    }
    binaries
}

fn use_sqlite3(root: &Path, sqlite3: &Path) {
    let link = root.join("sqlite3");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(sqlite3, &link).unwrap();
    fs::set_permissions(&link, fs::Permissions::from_mode(0o755)).ok();
}

/// A stable OpenCode store with one user and one assistant message, each with
/// a healthy text part, and the rows `extra` adds. The message and part tables
/// copy a real store's schema.
fn create_database(root: &Path, sqlite3: &Path, extra: &str) {
    let database = root.join("home/.local/share/opencode/opencode.db");
    let _ = fs::remove_file(&database);
    let script = format!(
        r#"
CREATE TABLE `session` (`id` text PRIMARY KEY, `parent_id` text, `directory` text NOT NULL,
  `title` text NOT NULL, `model` text, `cost` real, `tokens_input` integer,
  `tokens_output` integer, `tokens_reasoning` integer, `tokens_cache_read` integer,
  `tokens_cache_write` integer, `metadata` text, `time_created` integer NOT NULL, `time_updated` integer NOT NULL);
CREATE TABLE `message` (`id` text PRIMARY KEY, `session_id` text NOT NULL,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);
CREATE INDEX `message_session_time_created_id_idx` ON `message` (`session_id`,`time_created`,`id`);
CREATE TABLE `part` (`id` text PRIMARY KEY, `message_id` text NOT NULL, `session_id` text NOT NULL,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);
CREATE INDEX `part_session_idx` ON `part` (`session_id`);
CREATE INDEX `part_message_id_id_idx` ON `part` (`message_id`,`id`);
INSERT INTO session VALUES ('{SESSION}', NULL, '/fixtures/database', 'Database fixture session',
  '{{"id":"fixture-db-model"}}', 0, 0, 0, 0, 0, 0, NULL, 1784663000000, 1784664200000);
INSERT INTO message VALUES
  ('msg_multi_user', '{SESSION}', 1784663100000, 1784663100000,
   '{{"role":"user","time":{{"created":1784663100000}}}}'),
  ('msg_multi_assistant', '{SESSION}', 1784663110000, 1784663110000,
   '{{"role":"assistant","time":{{"created":1784663110000}}}}');
INSERT INTO part VALUES
  ('prt_multi_user', 'msg_multi_user', '{SESSION}', 1784663100001, 1784663100001,
   '{{"type":"text","text":"Healthy question.","time":{{"start":1784663100001}}}}'),
  ('prt_multi_answer', 'msg_multi_assistant', '{SESSION}', 1784663110001, 1784663110001,
   '{{"type":"text","text":"Healthy answer.","time":{{"start":1784663110001}}}}');
{extra}
"#
    );
    let mut create = Command::new(sqlite3)
        .arg(&database)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    create
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    assert!(create.wait().unwrap().success());
}

fn read(root: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .args(args)
        .env_remove("PI_SESSION_FILE")
        .env_remove("XDG_DATA_HOME")
        .env("HOME", root.join("home"))
        .env("PATH", root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn texts(read: &Value) -> Vec<String> {
    read["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["text"].as_str().unwrap().to_owned())
        .collect()
}

fn unreadable_reason(usage: &Value) -> bool {
    usage["measurement_completeness"]["input"]
        .as_array()
        .is_some_and(|reasons| reasons.iter().any(|reason| reason == "unreadable-record"))
}

/// The session with `extra` reads every healthy turn and figure of the
/// session without it, and names `records` unreadable records in `gaps`
/// message-position gaps.
fn assert_records_beside_gaps(name: &str, extra: &str, records: usize, gaps: usize) {
    let binaries = sqlite3_binaries();
    let root = Root::new(name);
    for sqlite3 in binaries {
        use_sqlite3(&root.0, &sqlite3);
        create_database(&root.0, &sqlite3, "");
        let healthy_full = read(&root.0, &["show", SESSION, "--full", "--json"]);
        let healthy_usage = read(&root.0, &["usage", SESSION, "--json"]);
        create_database(&root.0, &sqlite3, extra);
        let label = format!("{name} via {}", sqlite3.display());

        let bounded = read(&root.0, &["show", SESSION, "--tail", "5000", "--json"]);
        let full = read(&root.0, &["show", SESSION, "--full", "--json"]);
        assert_eq!(texts(&bounded), texts(&healthy_full), "{label}");
        assert_eq!(texts(&full), texts(&healthy_full), "{label}");

        let notes = bounded["notes"].to_string();
        assert!(
            notes.contains(&format!("Skipped {records} unreadable records")),
            "{label}: {notes}"
        );
        let marked = full["read"]["gaps"].as_array().unwrap();
        assert_eq!(marked.len(), gaps, "{label}: {}", full["read"]);
        assert!(marked.iter().all(|gap| gap["reason"] == "malformed-record"));

        for args in [
            &["usage", SESSION, "--json"][..],
            &["usage", SESSION, "--full", "--json"][..],
        ] {
            let usage = read(&root.0, args);
            assert_eq!(usage["tokens"], healthy_usage["tokens"], "{label} {args:?}");
            assert!(unreadable_reason(&usage), "{label} {args:?}: {usage}");
            let notes = usage["notes"].to_string();
            assert!(
                notes.contains(&format!("Skipped {records} unreadable records")),
                "{label} {args:?}: {notes}"
            );
        }
    }
}

#[test]
fn two_undecodable_parts_of_one_message_are_two_records_in_one_gap() {
    let extra = format!(
        "INSERT INTO part VALUES \
         ('prt_multi_broken_a', 'msg_multi_assistant', '{SESSION}', 1784663110002, 1784663110002, '{{not json'), \
         ('prt_multi_broken_b', 'msg_multi_assistant', '{SESSION}', 1784663110003, 1784663110003, '{{not json');"
    );
    assert_records_beside_gaps("two-parts", &extra, 2, 1);
}

#[test]
fn two_keyless_parts_of_one_message_are_two_records_in_one_gap() {
    let extra = format!(
        "INSERT INTO part VALUES \
         (NULL, 'msg_multi_assistant', '{SESSION}', 1784663110002, 1784663110002, \
          '{{\"type\":\"text\",\"text\":\"Keyless part.\"}}'), \
         (NULL, 'msg_multi_assistant', '{SESSION}', 1784663110003, 1784663110003, \
          '{{\"type\":\"text\",\"text\":\"Keyless part.\"}}');"
    );
    assert_records_beside_gaps("keyless-parts", &extra, 2, 1);
}

#[test]
fn an_undecodable_message_with_an_undecodable_part_is_two_records_in_one_gap() {
    let extra = format!(
        "INSERT INTO message VALUES ('msg_multi_broken', '{SESSION}', 1784663105000, \
         1784663105000, '{{not json'); \
         INSERT INTO part VALUES ('prt_multi_broken', 'msg_multi_broken', '{SESSION}', \
         1784663105001, 1784663105001, '{{not json');"
    );
    assert_records_beside_gaps("message-and-part", &extra, 2, 1);
}
