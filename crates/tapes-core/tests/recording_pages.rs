//! Claims: native pages meet exactly, stay bounded by page work, retain complete
//! content and call identity, and defer partial appends. Prefix rereads, lost
//! blocks, preview-only results, filter underfill and torn items are rejected.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use agent_tapes_core::recording::{Kind, Limits, Position, Recording, Stop};
use agent_tapes_core::NativeStore;
use serde_json::{json, Value};

const ID: &str = "00000000-0000-0000-0000-000000000001";
struct Fixture {
    _root: tempfile::TempDir,
    path: PathBuf,
    store: NativeStore,
    harness: &'static str,
}
impl Fixture {
    fn new(harness: &'static str, count: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let filename = match harness {
            "claude" => format!("{ID}.jsonl"),
            "codex" => format!("rollout-2026-01-01T10-00-00-{ID}.jsonl"),
            _ => format!("2026-01-01T10-00-00-000Z_{ID}.jsonl"),
        };
        let path = root.path().join(filename);
        let store = match harness {
            "claude" => NativeStore::claude_file(&path),
            "codex" => NativeStore::codex_file(&path),
            _ => NativeStore::pi_file(&path),
        };
        let header = match harness {
            "claude" => json!({"type":"session-owner", "sessionId":ID}),
            "codex" => json!({"type":"session_meta", "payload":{"id":ID}}),
            _ => json!({"type":"session","id":ID,"version":3}),
        };
        fs::write(&path, format!("{header}\n")).unwrap();
        let fixture = Self {
            _root: root,
            path,
            store,
            harness,
        };
        let mut file = OpenOptions::new().append(true).open(&fixture.path).unwrap();
        for n in 0..count {
            writeln!(
                file,
                "{}",
                fixture.message(n, &format!("message-{n} {}", "x".repeat(100)))
            )
            .unwrap();
        }
        fixture
    }
    fn reader(&self) -> Recording {
        Recording::resolve(std::slice::from_ref(&self.store), ID).unwrap()
    }
    fn message(&self, n: usize, text: &str) -> Value {
        let content = json!([{"type":"text","text":text}]);
        match self.harness {
            "claude" => {
                json!({"type":"assistant","sessionId":ID,"uuid":format!("m{n}"),"timestamp":"2026-01-01T10:00:01Z","message":{"role":"assistant","content":content}})
            }
            "codex" => {
                json!({"type":"response_item","timestamp":"2026-01-01T10:00:01Z","payload":{"type":"message","id":format!("m{n}"),"role":"assistant","content":[{"type":"output_text","text":text}]}})
            }
            _ => {
                json!({"type":"message","id":format!("m{n}"),"parentId":format!("m{}",n.saturating_sub(1)),"timestamp":"2026-01-01T10:00:01Z","message":{"role":"assistant","content":content}})
            }
        }
    }
    fn append(&self, value: &Value) {
        writeln!(
            OpenOptions::new().append(true).open(&self.path).unwrap(),
            "{value}"
        )
        .unwrap();
    }
    fn tool(&self, result: bool) -> Value {
        let payload = "y".repeat(10_000);
        match self.harness {
            "claude" => {
                json!({"type":if result {"user"}else{"assistant"},"sessionId":ID,"uuid":if result {"result"}else{"call"},"message":{"role":if result {"user"}else{"assistant"},"content":[if result { json!({"type":"tool_result","tool_use_id":"call-1","content":payload}) } else { json!({"type":"tool_use","id":"call-1","name":"read","input":{"path":"fixture"}}) }]}})
            }
            "codex" => {
                json!({"type":"response_item","payload":if result {json!({"type":"function_call_output","call_id":"call-1","output":payload})} else {json!({"type":"function_call","call_id":"call-1","name":"read","arguments":"{\"path\":\"fixture\"}"})}})
            }
            _ => {
                json!({"type":"message","id":if result {"result"}else{"call"},"parentId":"m0","message":if result {json!({"role":"toolResult","toolCallId":"call-1","content":[{"type":"text","text":payload}]})}else{json!({"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"fixture"}}]})}})
            }
        }
    }
}
fn limits(items: usize) -> Limits {
    Limits {
        items,
        ..Limits::default()
    }
}
fn texts(page: &agent_tapes_core::recording::Page) -> Vec<String> {
    page.items.iter().map(|i| i.text.clone()).collect()
}

#[test]
fn tail_and_before_read_page_bytes_instead_of_the_prefix_per_native_backend() {
    for harness in ["claude", "codex", "pi"] {
        let small = Fixture::new(harness, 100);
        let large = Fixture::new(harness, 20_000);
        let a = small.reader().tail(&limits(3)).unwrap();
        let b = large.reader().tail(&limits(3)).unwrap();
        let before = large.reader().before(&b.start, &limits(3)).unwrap();
        println!(
            "{harness}: source={} tail100={} tail20000={} before={}",
            fs::metadata(&large.path).unwrap().len(),
            a.bytes_read,
            b.bytes_read,
            before.bytes_read
        );
        assert_eq!(b.items.len(), 3);
        assert_eq!(before.items.len(), 3);
        assert!(
            b.bytes_read < 40_000 && before.bytes_read < 40_000,
            "page read scaled with prefix"
        );
        assert!(b.bytes_read.abs_diff(a.bytes_read) < 8192);
    }
}

#[test]
fn directions_meet_after_reopening_and_growth_without_gaps_duplicates_or_changed_positions() {
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 0);
        for n in 0..6 {
            let mut row = fixture.message(n, "part-a");
            let content = if harness == "codex" {
                &mut row["payload"]["content"]
            } else {
                &mut row["message"]["content"]
            };
            *content = if harness == "codex" {
                json!([{"type":"output_text","text":format!("a{n}")},{"type":"output_text","text":format!("b{n}")}])
            } else {
                json!([{"type":"text","text":format!("a{n}")},{"type":"thinking","thinking":"private"},{"type":"text","text":format!("b{n}")}])
            };
            fixture.append(&row);
        }
        let bounded = |items| Limits {
            kinds: vec![Kind::Assistant],
            ..limits(items)
        };
        let tail = fixture.reader().tail(&bounded(3)).unwrap();
        let cursor = Position::parse(tail.start.as_str()).unwrap();
        let older = fixture.reader().before(&cursor, &bounded(3)).unwrap();
        fixture.append(&fixture.message(12, "appended"));
        let repeat = fixture
            .reader()
            .forwards(&Position::parse(older.start.as_str()).unwrap(), &bounded(6))
            .unwrap();
        assert_eq!(
            texts(&repeat),
            [texts(&older), texts(&tail)].concat(),
            "directions failed to meet"
        );
        assert_eq!(
            repeat.items[3].start, tail.items[0].start,
            "append changed item position"
        );
        let newer = fixture.reader().forwards(&tail.end, &bounded(3)).unwrap();
        assert_eq!(texts(&newer), ["appended"]);
        let back = fixture.reader().before(&newer.end, &bounded(1)).unwrap();
        assert_eq!(texts(&newer), texts(&back));
    }
}

#[test]
fn later_results_name_earlier_calls_and_retain_bodies_larger_than_tool_previews() {
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 1);
        fixture.append(&fixture.tool(false));
        fixture.append(&fixture.tool(true));
        let page = fixture.reader().tail(&limits(1)).unwrap();
        assert_eq!(page.items[0].kind, Kind::ToolResult);
        assert_eq!(page.items[0].call_id.as_deref(), Some("call-1"));
        assert!(
            page.items[0]
                .content
                .to_string()
                .contains(&"y".repeat(10_000)),
            "result was only a preview"
        );
        let call = fixture.reader().before(&page.start, &limits(1)).unwrap();
        assert_eq!(call.items[0].kind, Kind::ToolCall);
        assert_eq!(call.items[0].call_id, page.items[0].call_id);
        if harness == "codex" {
            fixture.append(&json!({"type":"response_item","payload":{"type":"web_search_call","id":"web-fixture","status":"completed","action":{"type":"search","query":"fixture"}}}));
            let web = fixture.reader().tail(&limits(1)).unwrap();
            assert_eq!(
                web.items[0].tool.as_ref().unwrap().name.as_deref(),
                Some("web_search_call")
            );
        }
    }
}

#[test]
fn kind_filter_fills_pages_across_skipped_tools_in_both_directions() {
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 5);
        for n in 5..15 {
            fixture.append(&fixture.tool(true));
            fixture.append(&fixture.message(n, &format!("wanted-{n}")));
        }
        let options = Limits {
            kinds: vec![Kind::User, Kind::Assistant],
            ..limits(4)
        };
        let tail = fixture.reader().tail(&options).unwrap();
        assert_eq!(
            texts(&tail),
            ["wanted-11", "wanted-12", "wanted-13", "wanted-14"]
        );
        assert!(tail.items.iter().all(|i| i.kind == Kind::Assistant));
        let older = fixture.reader().before(&tail.start, &options).unwrap();
        assert_eq!(older.items.len(), 4);
        let forward = fixture.reader().forwards(&older.start, &options).unwrap();
        assert_eq!(texts(&older), texts(&forward));
    }
}

#[test]
fn oversized_items_refuse_without_clipping_and_byte_limited_pages_resume() {
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 5);
        let page = fixture.reader().tail(&limits(1)).unwrap();
        let size = page.items[0].budget_bytes().unwrap();
        let too_small = Limits {
            bytes: size - 1,
            ..limits(1)
        };
        assert!(fixture
            .reader()
            .tail(&too_small)
            .unwrap_err()
            .to_string()
            .contains("content was not clipped"));
        let bounded = fixture
            .reader()
            .tail(&Limits {
                bytes: size * 2,
                ..limits(5)
            })
            .unwrap();
        assert!(bounded.item_bytes <= size * 2);
        assert!(matches!(bounded.stop, Stop::Bytes));
        assert_eq!(
            texts(
                &fixture
                    .reader()
                    .forwards(&bounded.start, &limits(bounded.items.len()))
                    .unwrap()
            ),
            texts(&bounded)
        );
    }
}

#[test]
fn incomplete_append_is_deferred_even_when_it_is_valid_json_without_a_newline() {
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 3);
        let page = fixture.reader().tail(&limits(1)).unwrap();
        let value = fixture
            .message(3, "committed only after newline")
            .to_string();
        let mut file = OpenOptions::new().append(true).open(&fixture.path).unwrap();
        file.write_all(value.as_bytes()).unwrap();
        let deferred = fixture.reader().forwards(&page.end, &limits(3)).unwrap();
        assert!(
            deferred.items.is_empty(),
            "torn/uncommitted append was exposed"
        );
        assert_eq!(deferred.end, page.end);
        assert_eq!(deferred.pending_bytes, value.len() as u64);
        file.write_all(b"\n").unwrap();
        assert_eq!(
            texts(
                &fixture
                    .reader()
                    .forwards(&deferred.end, &limits(3))
                    .unwrap()
            ),
            ["committed only after newline"]
        );
    }
}

#[test]
fn multi_part_records_page_each_part_once_and_keep_internal_positions() {
    let fixture = Fixture::new("claude", 1);
    let mut message = fixture.message(1, "part-a");
    message["message"]["content"] = json!([{"type":"text","text":"part-a"},{"type":"thinking","thinking":"part-b"},{"type":"text","text":"part-c"}]);
    fixture.append(&message);
    let c = fixture.reader().tail(&limits(1)).unwrap();
    let b = fixture.reader().before(&c.start, &limits(1)).unwrap();
    let a = fixture.reader().before(&b.start, &limits(1)).unwrap();
    let all = fixture.reader().forwards(&a.start, &limits(3)).unwrap();
    assert_eq!(texts(&all), ["part-a", "part-b", "part-c"]);
    assert_eq!(all.items[1].start, b.items[0].start);
    assert_eq!(all.items[2].start, c.items[0].start);
    let mut user = message;
    user["message"]["role"] = json!("user");
    user["message"]["content"] = json!([{"type":"text","text":"<local-command-stdout>fixture</local-command-stdout>"},{"type":"text","text":"another part"}]);
    fixture.append(&user);
    let user_page = fixture.reader().tail(&limits(2)).unwrap();
    assert!(
        user_page
            .items
            .iter()
            .all(|item| item.kind == Kind::Unknown),
        "splitting content changed the native envelope classification"
    );
}

struct Database {
    _root: tempfile::TempDir,
    file: PathBuf,
    store: NativeStore,
}
impl Database {
    fn new(count: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("opencode.db");
        let store = NativeStore::opencode_stable_at("opencode", root.path());
        let db = Self {
            _root: root,
            file,
            store,
        };
        let mut sql = include_str!("fixtures/opencode/recording-pages.sql").to_owned();
        sql.push_str("BEGIN;\n");
        for n in 0..count {
            sql.push_str(&Self::insert(
                n,
                &json!({"type":"text","text":format!("db-{n}")}),
            ));
        }
        sql.push_str("COMMIT;");
        db.sql(&sql);
        db
    }
    fn sql(&self, sql: &str) {
        use std::process::{Command, Stdio};
        let mut child = Command::new("sqlite3")
            .arg(&self.file)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(sql.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn insert(n: usize, content: &Value) -> String {
        let mut content = content.clone();
        if matches!(content["type"].as_str(), Some("text" | "reasoning"))
            && content.get("time").is_none()
        {
            content["time"] = json!({"start": n, "end": n+1});
        }
        format!(
            "INSERT INTO part VALUES('p{n:06}','msg_fixture','ses_fixture',{n},{n},'{}');\n",
            content.to_string().replace('\'', "''")
        )
    }
    fn reader(&self) -> Recording {
        Recording::resolve(std::slice::from_ref(&self.store), "ses_fixture").unwrap()
    }
}

#[test]
fn database_keysets_meet_across_growth_with_complete_message_fields() {
    let small = Database::new(100);
    let small_tail = small.reader().tail(&limits(3)).unwrap();
    let db = Database::new(1200);
    let tail = db.reader().tail(&limits(3)).unwrap();
    assert_eq!(texts(&tail), ["db-1197", "db-1198", "db-1199"]);
    assert!(
        tail.bytes_read.abs_diff(small_tail.bytes_read) < 100,
        "database response scaled with prefix"
    );
    let older = db.reader().before(&tail.start, &limits(3)).unwrap();
    db.sql(&Database::insert(
        1200,
        &json!({"type":"text","text":"z".repeat(20_000)}),
    ));
    let forward = db
        .reader()
        .forwards(&Position::parse(older.start.as_str()).unwrap(), &limits(6))
        .unwrap();
    assert_eq!(texts(&forward), [texts(&older), texts(&tail)].concat());
    let appended = db.reader().forwards(&tail.end, &limits(1)).unwrap();
    assert_eq!(
        appended.items[0].text.len(),
        20_000,
        "database text was clipped"
    );
    assert_eq!(
        appended.items[0].content["text"].as_str().unwrap().len(),
        20_000
    );
    assert_eq!(tail.parents.len(), 1);
    println!(
        "opencode: tail1200_transport={} before_transport={} append20000_transport={}",
        tail.bytes_read, older.bytes_read, appended.bytes_read
    );
}

#[test]
fn database_filter_skips_large_tools_and_complete_results_keep_call_identity() {
    let db = Database::new(2);
    db.sql(&Database::insert(2, &json!({"type":"tool","callID":"call-1","tool":"read","state":{"status":"completed","input":{"path":"fixture"},"output":"y".repeat(20_000)}})));
    db.sql(&Database::insert(3, &json!({"type":"text","text":"last"})));
    let prose = db
        .reader()
        .tail(&Limits {
            kinds: vec![Kind::Assistant],
            ..limits(3)
        })
        .unwrap();
    assert_eq!(texts(&prose), ["db-0", "db-1", "last"]);
    assert!(
        prose.bytes_read < 10_000,
        "filter materialized unwanted tool output"
    );
    let results = db
        .reader()
        .tail(&Limits {
            kinds: vec![Kind::ToolResult],
            ..limits(1)
        })
        .unwrap();
    assert_eq!(results.items[0].call_id.as_deref(), Some("call-1"));
    assert_eq!(
        results.items[0].content["state"]["output"]
            .as_str()
            .unwrap()
            .len(),
        20_000
    );
    let calls = db.reader().before(&results.start, &limits(1)).unwrap();
    assert_eq!(calls.items[0].kind, Kind::ToolCall);
    let error = db
        .reader()
        .tail(&Limits {
            kinds: vec![Kind::ToolResult],
            bytes: 1000,
            ..limits(1)
        })
        .unwrap_err();
    assert!(error.to_string().contains("content was not clipped"));
    db.sql(
        "UPDATE part SET data = '{\"type\":\"text\",\"text\":\"rewritten\"}' WHERE id = 'p000002';",
    );
    assert!(db
        .reader()
        .forwards(&results.end, &limits(1))
        .unwrap_err()
        .to_string()
        .contains("changed"));
}

#[test]
fn backend_fixtures_retain_parent_links_and_report_malformed_records() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let claude = NativeStore::claude_file(fixtures.join("claude/project/session-claude.jsonl"));
    let reader = Recording::resolve(&[claude], "session-claude").unwrap();
    let child = reader.child("fixture").unwrap().tail(&limits(2)).unwrap();
    assert_eq!(child.parents[0].reference, "session-claude");
    let pi = NativeStore::pi_file(fixtures.join("pi/2026-01-01T10-00-00-000Z_session-pi.jsonl"));
    let page = Recording::resolve(&[pi], "session-pi")
        .unwrap()
        .tail(&limits(100))
        .unwrap();
    assert!(page.parents.iter().any(|p| p.relation == "forked-from"));
    assert!(page.items.iter().any(|i| i.text == "Abandoned branch."));
    assert!(page
        .items
        .iter()
        .any(|i| i.parent_id.as_deref() == Some("assistant-1")));
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 2);
        OpenOptions::new()
            .append(true)
            .open(&fixture.path)
            .unwrap()
            .write_all(b"malformed\n")
            .unwrap();
        let page = fixture.reader().tail(&limits(2)).unwrap();
        assert_eq!(page.malformed_records, 1);
        assert_eq!(page.items.len(), 2);
    }
}

#[test]
fn positions_reject_replacement_truncation_and_edits_inside_a_positioned_record() {
    let fixture = Fixture::new("claude", 3);
    let mut message = fixture.message(4, "first");
    message["message"]["content"] =
        json!([{"type":"text","text":"first"},{"type":"text","text":"second"}]);
    fixture.append(&message);
    let page = fixture.reader().tail(&limits(1)).unwrap();
    let source = fs::read_to_string(&fixture.path).unwrap();
    fs::write(&fixture.path, source.replace("second", "edited")).unwrap();
    assert!(fixture
        .reader()
        .before(&page.start, &limits(1))
        .unwrap_err()
        .to_string()
        .contains("changed"));
    fs::write(&fixture.path, &source).unwrap();
    let cursor = fixture.reader().tail(&limits(1)).unwrap().end;
    fs::write(
        &fixture.path,
        source.lines().next().unwrap().to_owned() + "\n",
    )
    .unwrap();
    assert!(fixture
        .reader()
        .forwards(&cursor, &limits(1))
        .unwrap_err()
        .to_string()
        .contains("truncated"));
}

#[test]
fn concurrent_chunked_appends_never_publish_partial_items() {
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 1);
        let path = fixture.path.clone();
        let message = fixture.message(2, &"é".repeat(500)).to_string();
        let (half_tx, half_rx) = std::sync::mpsc::sync_channel(0);
        let (finish_tx, finish_rx) = std::sync::mpsc::sync_channel(0);
        let writer = std::thread::spawn(move || {
            let mut out = OpenOptions::new().append(true).open(path).unwrap();
            out.write_all(&message.as_bytes()[..message.len() / 2])
                .unwrap();
            half_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
            out.write_all(&message.as_bytes()[message.len() / 2..])
                .unwrap();
            out.write_all(b"\n").unwrap();
        });
        let original = fixture.reader().tail(&limits(1)).unwrap();
        half_rx.recv().unwrap();
        let partial = fixture
            .reader()
            .forwards(&original.end, &limits(1))
            .unwrap();
        assert!(
            partial.items.is_empty(),
            "chunked append produced a torn item"
        );
        finish_tx.send(()).unwrap();
        writer.join().unwrap();
        let complete = fixture.reader().forwards(&partial.end, &limits(1)).unwrap();
        assert_eq!(complete.items[0].text, "é".repeat(500));
    }
}

