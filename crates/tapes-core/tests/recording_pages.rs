//! Claims: native pages meet exactly, stay bounded by page work, retain complete
//! content and call identity, and defer partial appends. Prefix rereads, lost
//! blocks, preview-only results, filter underfill and torn items are rejected.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use agent_tapes_core::recording::{Kind, Limits, Position, Recording, Refusal, Stop};
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
        assert!(matches!(deferred.stop, Stop::Pending));
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

// non-text OpenCode parts are unknown; neither the SQL nor the item kind
// can admit step/patch metadata as prose.
#[test]
fn database_prose_excludes_step_and_patch_rows_and_kinds_agree() {
    let db = Database::new(2);
    for (n, kind) in [(2, "step-start"), (3, "step-finish"), (4, "patch")] {
        db.sql(&Database::insert(n, &json!({"type":kind})));
    }
    db.sql(&Database::insert(
        5,
        &json!({"type":"text","text":"answer"}),
    ));
    db.sql(&Database::insert(6, &json!({"marker":"untyped"})));
    let prose = db
        .reader()
        .tail(&Limits {
            kinds: vec![Kind::User, Kind::Assistant],
            ..limits(3)
        })
        .unwrap();
    assert_eq!(texts(&prose), ["db-0", "db-1", "answer"]);
    let all = db.reader().tail(&limits(10)).unwrap();
    let unknown = db
        .reader()
        .tail(&Limits {
            kinds: vec![Kind::Unknown],
            ..limits(10)
        })
        .unwrap();
    assert_eq!(unknown.items.len(), 4);
    assert_eq!(
        all.items.iter().filter(|i| i.kind == Kind::Unknown).count(),
        4
    );
    assert_eq!(
        all.items
            .iter()
            .filter(|i| i.kind == Kind::Assistant)
            .count(),
        3
    );
    assert!(all
        .items
        .iter()
        .all(|i| i.parent_id.as_deref() == Some("msg_parent")
            && i.message_id.as_deref() == Some("msg_fixture")));
}

// the first incomplete part blocks later parts, including filtered tools.
// Ordinary completion must follow once, with no cursor on mutable content.
#[test]
fn database_follow_defers_streaming_text_and_running_tools_until_complete() {
    let db = Database::new(1);
    db.sql("UPDATE message SET data=json_remove(data,'$.time.completed');");
    db.sql(&Database::insert(
        1,
        &json!({"type":"text","text":"Hel","time":{"start":1}}),
    ));
    db.sql(&Database::insert(2, &json!({"type":"text","text":"later"})));
    let tail = db.reader().tail(&limits(10)).unwrap();
    assert_eq!(texts(&tail), ["db-0"]);
    assert_eq!(tail.pending.as_deref(), Some("p000001"));
    assert!(matches!(tail.stop, Stop::Pending));
    db.sql("UPDATE part SET data='{\"type\":\"text\",\"text\":\"Hello\",\"time\":{\"start\":1,\"end\":2}}',time_updated=2 WHERE id='p000001';");
    let complete = db.reader().forwards(&tail.end, &limits(10)).unwrap();
    assert_eq!(texts(&complete), ["Hello", "later"]);
    db.sql(&Database::insert(3,&json!({"type":"tool","callID":"call-live","tool":"bash","state":{"status":"running","input":{"command":"fixture"}}})));
    let running = db.reader().forwards(&complete.end, &limits(10)).unwrap();
    assert!(running.items.is_empty());
    assert_eq!(running.end, complete.end);
    assert_eq!(running.pending.as_deref(), Some("p000003"));
    db.sql("UPDATE part SET data='{\"type\":\"tool\",\"callID\":\"call-live\",\"tool\":\"bash\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"fixture\"},\"output\":\"done\"}}' WHERE id='p000003';");
    let tool = db.reader().forwards(&running.end, &limits(10)).unwrap();
    assert_eq!(tool.items.len(), 2);
    assert_eq!(tool.items[0].kind, Kind::ToolCall);
    assert_eq!(tool.items[1].kind, Kind::ToolResult);
    assert!(db
        .reader()
        .forwards(&tool.end, &limits(10))
        .unwrap()
        .items
        .is_empty());
    // The rule holds before the first completed part as well.
    let empty = Database::new(0);
    empty.sql("UPDATE message SET data=json_remove(data,'$.time.completed');");
    empty.sql(&Database::insert(
        0,
        &json!({"type":"text","text":"partial","time":{"start":0}}),
    ));
    let page = empty.reader().tail(&limits(1)).unwrap();
    assert!(page.items.is_empty());
    empty.sql("UPDATE part SET data='{\"type\":\"text\",\"text\":\"whole\",\"time\":{\"start\":0,\"end\":1}}' WHERE id='p000000';");
    assert_eq!(
        texts(&empty.reader().forwards(&page.end, &limits(1)).unwrap()),
        ["whole"]
    );
    let large = Database::new(0);
    large.sql(&Database::insert(
        0,
        &json!({"type":"text","text":"a".repeat(30_000)}),
    ));
    let saved = large.reader().tail(&limits(1)).unwrap();
    large.sql(&Database::insert(
        1,
        &json!({"type":"text","text":"b".repeat(30_000)}),
    ));
    let resumed = large
        .reader()
        .forwards(
            &saved.end,
            &Limits {
                scan_bytes: 40_000,
                ..limits(1)
            },
        )
        .unwrap();
    assert_eq!(
        resumed.items.len(),
        1,
        "completed cursor row consumed the next page's scan budget"
    );
    assert_eq!(resumed.items[0].text, "b".repeat(30_000));
}

// the schema has one flat vocabulary, and the byte budget charges the
// native payload once instead of charging normalized copies of the same body.
#[test]
fn flat_items_expose_fields_once_and_budget_each_native_payload_once() {
    let fixture = Fixture::new("claude", 1);
    fixture.append(&fixture.tool(true));
    let page = fixture.reader().tail(&limits(1)).unwrap();
    let item = &page.items[0];
    let json = serde_json::to_value(item).unwrap();
    for forbidden in ["turn", "position", "parts", "ordinal", "coverage"] {
        assert!(json.get(forbidden).is_none(), "unexpected {forbidden}");
    }
    assert!(json.get("start").is_some());
    assert!(item.text.is_empty());
    assert!(
        item.budget_bytes().unwrap() < 12_000,
        "payload charged more than once"
    );
    assert!(serde_json::to_vec(item).unwrap().len() > 20_000);
    assert_eq!(
        item.tool
            .as_ref()
            .unwrap()
            .output
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .len(),
        10_000
    );
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude/project/session-claude.jsonl");
    let child = Recording::resolve(&[NativeStore::claude_file(fixtures)], "session-claude")
        .unwrap()
        .child("fixture")
        .unwrap()
        .tail(&limits(1))
        .unwrap();
    assert_eq!(child.recording, "session-claude::fixture");
}

// native message arrays have no implicit 128-part text ceiling.
#[test]
fn codex_message_over_128_parts_returns_every_text_part() {
    let fixture = Fixture::new("codex", 0);
    let mut message = fixture.message(0, "placeholder");
    message["payload"]["content"] = json!((0..140)
        .map(|n| json!({"type":"output_text","text":format!("part-{n}")}))
        .collect::<Vec<_>>());
    fixture.append(&message);
    let page = fixture
        .reader()
        .tail(&Limits {
            kinds: vec![Kind::Assistant],
            ..limits(200)
        })
        .unwrap();
    assert_eq!(
        texts(&page),
        (0..140).map(|n| format!("part-{n}")).collect::<Vec<_>>()
    );
}

// callers match public refusal variants, skip an oversized item using
// its end, and reject a same-inode rewrite even when its last 64 bytes agree.
#[test]
fn typed_refusals_allow_skipping_and_reject_stale_boundaries_and_renames() {
    let fixture = Fixture::new("claude", 2);
    fixture.append(&fixture.message(3, &"z".repeat(20_000)));
    let error = fixture
        .reader()
        .tail(&Limits {
            bytes: 1000,
            ..limits(1)
        })
        .unwrap_err();
    let Refusal::Oversized { needed, start, end } = error.downcast_ref::<Refusal>().unwrap() else {
        panic!("wrong refusal")
    };
    assert!(*needed > 20_000);
    let item = fixture
        .reader()
        .forwards(
            start,
            &Limits {
                bytes: *needed,
                ..limits(1)
            },
        )
        .unwrap();
    assert_eq!(item.items[0].text.len(), 20_000);
    assert!(fixture
        .reader()
        .forwards(end, &limits(1))
        .unwrap()
        .items
        .is_empty());
    let small = Limits {
        scan_bytes: 10,
        ..Limits::default()
    };
    assert!(matches!(
        fixture
            .reader()
            .tail(&small)
            .unwrap_err()
            .downcast_ref::<Refusal>(),
        Some(Refusal::ScanBudget { .. })
    ));
    let original = fs::read_to_string(&fixture.path).unwrap();
    let tail = fixture.reader().tail(&limits(1)).unwrap();
    fs::write(
        &fixture.path,
        original.replacen("message-1", "MESSAGE-1", 1)
            + &format!("{}\n", fixture.message(4, "appended")),
    )
    .unwrap();
    assert!(matches!(
        fixture
            .reader()
            .before(&tail.start, &limits(1))
            .unwrap_err()
            .downcast_ref::<Refusal>(),
        Some(Refusal::Changed { .. })
    ));
    fs::write(&fixture.path, &original).unwrap();
    let tail = fixture.reader().tail(&limits(1)).unwrap();
    let next = fixture.path.with_extension("replacement");
    fs::write(&next, &original).unwrap();
    fs::rename(next, &fixture.path).unwrap();
    assert!(matches!(
        fixture
            .reader()
            .forwards(&tail.end, &limits(1))
            .unwrap_err()
            .downcast_ref::<Refusal>(),
        Some(Refusal::Replaced { .. })
    ));
}

// whitespace-only JSONL separators are valid separators, not bad records.
#[test]
fn blank_lines_are_not_malformed_records() {
    let fixture = Fixture::new("claude", 2);
    OpenOptions::new()
        .append(true)
        .open(&fixture.path)
        .unwrap()
        .write_all(b"\n \t\r\nnot json\n")
        .unwrap();
    let page = fixture.reader().tail(&limits(20)).unwrap();
    assert_eq!(page.malformed_records, 1);
}

// Both directions meet at internal gaps under kind filters
// and scan stops, rather than merely agreeing at single-part file boundaries.
#[test]
fn multipart_filtered_meeting_and_scan_stops_preserve_every_selected_item() {
    for harness in ["claude", "codex", "pi"] {
        let fixture = Fixture::new(harness, 0);
        for n in 0..20 {
            let mut row = fixture.message(n, "a");
            let content = if harness == "codex" {
                &mut row["payload"]["content"]
            } else {
                &mut row["message"]["content"]
            };
            *content = if harness == "codex" {
                json!([{"type":"output_text","text":format!("a{n} {}","x".repeat(1000))},{"type":"output_text","text":format!("b{n} {}","x".repeat(1000))}])
            } else {
                json!([{"type":"text","text":format!("a{n} {}","x".repeat(1000))},{"type":"thinking","thinking":"private"},{"type":"text","text":format!("b{n} {}","x".repeat(1000))}])
            };
            fixture.append(&row);
        }
        let filter = vec![Kind::Assistant];
        let full = fixture
            .reader()
            .tail(&Limits {
                kinds: filter.clone(),
                ..limits(100)
            })
            .unwrap();
        let options = Limits {
            items: 100,
            scan_bytes: 20_000,
            kinds: filter,
            ..Limits::default()
        };
        let mut at = full.start.clone();
        let mut forward = Vec::new();
        let mut forward_scans = 0;
        for _ in 0..100 {
            let p = fixture.reader().forwards(&at, &options).unwrap();
            forward.extend(texts(&p));
            if matches!(p.stop, Stop::End) {
                break;
            }
            forward_scans += usize::from(matches!(p.stop, Stop::ScanBytes));
            assert_ne!(p.end, at);
            at = p.end;
        }
        assert_eq!(forward, texts(&full));
        let mut at = full.end.clone();
        let mut backward = Vec::new();
        let mut backward_scans = 0;
        for _ in 0..100 {
            let p = fixture.reader().before(&at, &options).unwrap();
            let mut t = texts(&p);
            t.extend(backward);
            backward = t;
            if matches!(p.stop, Stop::End) {
                break;
            }
            backward_scans += usize::from(matches!(p.stop, Stop::ScanBytes));
            assert_ne!(p.start, at);
            at = p.start;
        }
        assert_eq!(backward, texts(&full));
        assert!(forward_scans > 0 && backward_scans > 0);
        for (index, item) in full.items.iter().enumerate() {
            let before = fixture
                .reader()
                .before(
                    &item.start,
                    &Limits {
                        kinds: vec![Kind::Assistant],
                        ..limits(100)
                    },
                )
                .unwrap();
            let after = fixture
                .reader()
                .forwards(
                    &item.start,
                    &Limits {
                        kinds: vec![Kind::Assistant],
                        ..limits(100)
                    },
                )
                .unwrap();
            assert_eq!([texts(&before), texts(&after)].concat(), texts(&full));
            assert_eq!(
                texts(&before),
                texts(&full)[..index],
                "before ignored an internal gap"
            );
            assert_eq!(
                texts(&after),
                texts(&full)[index..],
                "forwards ignored an internal gap"
            );
        }
    }
}

// Linux accounts bytes read independently of Window.bytes_read. Each child
// runs this test alone so unrelated tests cannot alter its /proc/self/io delta.
#[test]
fn kernel_read_counts_bound_tail_and_before_independently() {
    if !PathBuf::from("/proc/self/io").exists() {
        return;
    }
    let counter = || {
        fs::read_to_string("/proc/self/io")
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("rchar: ")
                    .map(|n| n.parse::<u64>().unwrap())
            })
            .unwrap()
    };
    if let Ok(harness) = std::env::var("TAPES_PAGE_IO_CHILD") {
        let harness = match harness.as_str() {
            "claude" => "claude",
            "codex" => "codex",
            _ => "pi",
        };
        let n = std::env::var("TAPES_PAGE_IO_RECORDS")
            .unwrap()
            .parse()
            .unwrap();
        let fixture = Fixture::new(harness, n);
        let reader = fixture.reader();
        let before = counter();
        let tail = reader.tail(&limits(3)).unwrap();
        let tail_bytes = counter() - before;
        let before = counter();
        let older = reader.before(&tail.start, &limits(3)).unwrap();
        let older_bytes = counter() - before;
        println!(
            "KERNEL_PAGE_BYTES {harness} {n} {tail_bytes} {older_bytes} reported={} {}",
            tail.bytes_read, older.bytes_read
        );
        assert!(
            tail_bytes < 50_000 && older_bytes < 50_000,
            "actual source reads scaled with prefix"
        );
        return;
    }
    for harness in ["claude", "codex", "pi"] {
        for count in [100, 20_000] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "kernel_read_counts_bound_tail_and_before_independently",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("TAPES_PAGE_IO_CHILD", harness)
                .env("TAPES_PAGE_IO_RECORDS", count.to_string())
                .output()
                .unwrap();
            print!("{}", String::from_utf8_lossy(&result.stdout));
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}

// native indexes may scan/sort the session, but a page must not spawn one
// query process per item. This wrapper executes the actual SQLite statements.
#[test]
fn database_batches_keep_sqlite_process_count_independent_of_item_count() {
    use std::os::unix::fs::PermissionsExt;
    let db = Database::new(2000);
    let count = db._root.path().join("queries");
    let program = db._root.path().join("opencode-counted");
    let shell_quote =
        |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    fs::write(&program,format!("#!/bin/bash\nprintf '.\\n' >> {}\nexec sqlite3 -readonly -batch -list -header {} \"$4\"\n",shell_quote(&count),shell_quote(&db.file))).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let store = NativeStore::opencode_stable_at(program, db._root.path());
    let r = Recording::resolve(&[store], "ses_fixture").unwrap();
    let calls = || fs::read_to_string(&count).unwrap().lines().count();
    let before = calls();
    r.tail(&limits(1)).unwrap();
    let one = calls() - before;
    let before = calls();
    let forty = r.tail(&limits(40)).unwrap();
    let many = calls() - before;
    let before = calls();
    r.forwards(&forty.start, &limits(40)).unwrap();
    let resume = calls() - before;
    println!("SQLITE_PAGE_PROCESSES one={one} forty={many} resume={resume}");
    assert_eq!(one, many, "query processes scaled with item count");
    assert!(many <= 3 && resume <= 4);
    let plan=std::process::Command::new("sqlite3").arg(&db.file).arg("EXPLAIN QUERY PLAN SELECT id FROM part WHERE session_id='ses_fixture' ORDER BY time_created,id LIMIT 40").output().unwrap();
    let plan = String::from_utf8(plan.stdout).unwrap();
    assert!(
        plan.contains("part_session_idx") && plan.contains("TEMP B-TREE"),
        "fixture lacks the real schema's sort cost: {plan}"
    );
    println!("REAL_INDEX_PLAN {plan}");
}

// A crashed turn cannot conceal resumed turns. Only the newest, live message
// may hold the stream; completed/errored messages release incomplete parts too.
#[test]
fn database_abandoned_parts_release_later_turns_with_never_finished_status() {
    let db = Database::new(0);
    db.sql("UPDATE message SET data='{\"role\":\"user\"}' WHERE id='msg_fixture'; INSERT INTO message VALUES('msg_a','ses_fixture',2,2,'{\"role\":\"assistant\",\"time\":{\"created\":2}}');");
    db.sql(&Database::insert(
        0,
        &json!({"type":"text","text":"question"}),
    ));
    db.sql(&Database::insert(
        1,
        &json!({"type":"reasoning","text":"half a thought","time":{"start":1}}),
    ));
    db.sql(&Database::insert(2,&json!({"type":"tool","tool":"bash","callID":"abandoned","state":{"status":"running","input":{}}})));
    db.sql("UPDATE part SET message_id='msg_a' WHERE id IN ('p000001','p000002');");
    let tail = db.reader().tail(&limits(20)).unwrap();
    assert_eq!(texts(&tail), ["question"]);
    assert!(matches!(tail.stop, Stop::Pending));
    db.sql("INSERT INTO message VALUES('msg_user2','ses_fixture',50,50,'{\"role\":\"user\"}'); INSERT INTO message VALUES('msg_b','ses_fixture',51,51,'{\"role\":\"assistant\",\"time\":{\"completed\":52}}');");
    db.sql(&Database::insert(
        10,
        &json!({"type":"text","text":"second question"}),
    ));
    db.sql(&Database::insert(
        11,
        &json!({"type":"text","text":"second answer"}),
    ));
    db.sql("UPDATE part SET message_id='msg_user2' WHERE id='p000010'; UPDATE part SET message_id='msg_b' WHERE id='p000011';");
    let resumed = db.reader().forwards(&tail.end, &limits(20)).unwrap();
    assert_eq!(
        texts(&resumed),
        ["half a thought", "", "", "second question", "second answer"]
    );
    let wire = serde_json::to_value(&resumed).unwrap();
    assert_eq!(wire["items"][0]["status"], "never-finished");
    for i in [1, 2] {
        assert_eq!(wire["items"][i]["tool"]["status"], "never-finished");
    }
    assert!(matches!(resumed.stop, Stop::End));
    assert_eq!(db.reader().tail(&limits(20)).unwrap().items.len(), 6);
    for terminal in [json!({"completed":2}), json!({"error":{"name":"Aborted"}})] {
        let fresh = Database::new(0);
        let data = if terminal.get("completed").is_some() {
            json!({"role":"assistant","time":terminal})
        } else {
            json!({"role":"assistant","error":terminal["error"]})
        };
        fresh.sql(&format!("UPDATE message SET data='{}';", data));
        fresh.sql(&Database::insert(
            0,
            &json!({"type":"reasoning","text":"retained","time":{"start":0}}),
        ));
        let page = fresh.reader().tail(&limits(10)).unwrap();
        assert_eq!(texts(&page), ["retained"]);
        assert_eq!(
            serde_json::to_value(&page.items[0]).unwrap()["status"],
            "never-finished"
        );
    }
}

// Message grouping is harness-independent, including mixed user/tool-result
// records. Reasoning and tool records with their own IDs are not Codex messages.
#[test]
fn message_ids_group_parts_of_native_messages_on_every_backend() {
    for harness in ["claude", "codex", "pi"] {
        let f = Fixture::new(harness, 0);
        let text =
            |t: &str| json!({"type":if harness=="codex" {"output_text"}else{"text"},"text":t});
        let mut a = f.message(1, "a");
        if harness == "codex" {
            a["payload"]["content"] = json!([text("a"), text("b")]);
        } else {
            a["message"]["content"] = json!([text("a"), text("b")]);
        }
        if harness == "claude" {
            a["message"]["id"] = json!("api-message");
        }
        f.append(&a);
        let assistant = f.reader().tail(&limits(10)).unwrap();
        let expected = if harness == "claude" {
            "api-message"
        } else {
            "m1"
        };
        assert_eq!(assistant.items.len(), 2);
        assert!(
            assistant
                .items
                .iter()
                .all(|i| i.message_id.as_deref() == Some(expected)),
            "{harness} assistant grouping"
        );
        assert_ne!(assistant.items[0].start, assistant.items[1].start);
        assert_eq!(assistant.items[0].native_id, assistant.items[1].native_id);
        let mut user = f.message(2, "u");
        match harness {
            "codex" => {
                user["payload"]["role"] = json!("user");
                user["payload"]["content"] = json!([text("u"), text("v")]);
            }
            _ => {
                user["type"] = json!(if harness == "claude" {
                    "user"
                } else {
                    "message"
                });
                user["message"]["role"] = json!("user");
                user["message"]["content"] = json!([text("u"), text("v")]);
            }
        }
        if harness == "claude" {
            user["message"]["content"][1] =
                json!({"type":"tool_result","tool_use_id":"c","content":"result"});
        }
        f.append(&user);
        let page = f.reader().forwards(&assistant.end, &limits(10)).unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(
            page.items
                .iter()
                .all(|i| i.message_id.as_deref() == Some("m2")),
            "{harness} user grouping"
        );
        if harness == "codex" {
            f.append(&json!({"type":"response_item","payload":{"type":"reasoning","id":"rs_fixture","summary":[{"type":"summary_text","text":"thinking"}]}}));
            let mut tool = f.tool(false);
            tool["payload"]["id"] = json!("ctc_fixture");
            f.append(&tool);
            assert!(f
                .reader()
                .forwards(&page.end, &limits(10))
                .unwrap()
                .items
                .iter()
                .all(|i| i.message_id.is_none()));
        }
    }
    let db = Database::new(2);
    db.sql(&Database::insert(2,&json!({"type":"tool","tool":"bash","callID":"c","state":{"status":"completed","input":{},"output":"done"}})));
    let page = db.reader().tail(&limits(10)).unwrap();
    assert_eq!(page.items.len(), 4);
    assert!(page
        .items
        .iter()
        .all(|i| i.message_id.as_deref() == Some("msg_fixture")));
}

// An idle poll after a large result costs an anchor, not a whole-record read.
// /proc rchar measures actual process reads independently of Page.bytes_read.
#[test]
fn kernel_read_counts_bound_idle_follow_after_large_records() {
    if !PathBuf::from("/proc/self/io").exists() {
        return;
    }
    if std::env::var_os("TAPES_ANCHOR_IO_CHILD").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kernel_read_counts_bound_idle_follow_after_large_records",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TAPES_ANCHOR_IO_CHILD", "1")
            .output()
            .unwrap();
        print!("{}", String::from_utf8_lossy(&out.stdout));
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let counter = || {
        fs::read_to_string("/proc/self/io")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("rchar: ").map(|n| n.parse::<u64>().unwrap()))
            .unwrap()
    };
    let f = Fixture::new("claude", 1);
    let mut tool = f.tool(true);
    tool["message"]["content"][0]["content"] = json!("R".repeat(6 * 1024 * 1024));
    f.append(&tool);
    let prose = Limits {
        kinds: vec![Kind::User, Kind::Assistant],
        ..limits(40)
    };
    let r = f.reader();
    let tail = r.tail(&prose).unwrap();
    let initial = counter();
    let idle = r.forwards(&tail.end, &prose).unwrap();
    let idle_bytes = counter() - initial;
    assert!(idle.items.is_empty());
    println!(
        "KERNEL_IDLE_BYTES {idle_bytes} reported={}",
        idle.bytes_read
    );
    assert!(
        idle_bytes < 32_000,
        "idle poll reread large preceding record: {idle_bytes}"
    );
    tool["message"]["content"][0]["content"] = json!("V".repeat(3 * 1024 * 1024));
    f.append(&tool);
    f.append(&f.message(9, "after"));
    let initial = counter();
    let followed = r.forwards(&tail.end, &prose).unwrap();
    let follow_bytes = counter() - initial;
    println!(
        "KERNEL_FOLLOW_BYTES {follow_bytes} reported={}",
        followed.bytes_read
    );
    assert_eq!(texts(&followed), ["after"]);
    assert!(
        follow_bytes < 7 * 1024 * 1024,
        "follow reread preceding record"
    );
}

// Source failures remain machine-readable; disappearance differs from a
// truncated record at an internal part gap. Known oversize scans give a hint.
#[test]
fn page_failures_are_typed_and_known_scan_budget_has_needed_size() {
    let f = Fixture::new("claude", 1);
    let r = f.reader();
    fs::remove_file(&f.path).unwrap();
    let error = r.tail(&limits(1)).unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<Refusal>(),
            Some(Refusal::Replaced { .. })
        ),
        "{error:?}"
    );
    let f = Fixture::new("claude", 0);
    let mut message = f.message(1, "a");
    message["message"]["content"] =
        json!([{"type":"text","text":"a"},{"type":"text","text":"longer text"}]);
    f.append(&message);
    let r = f.reader();
    let tail = r.tail(&limits(1)).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&f.path)
        .unwrap()
        .set_len(fs::metadata(&f.path).unwrap().len() - 10)
        .unwrap();
    let error = r.forwards(&tail.start, &limits(1)).unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<Refusal>(),
            Some(Refusal::Changed { .. })
        ),
        "{error:?}"
    );
    let db = Database::new(1);
    db.sql(&Database::insert(
        1,
        &json!({"type":"text","text":"x".repeat(2000)}),
    ));
    let error = db
        .reader()
        .tail(&Limits {
            scan_bytes: 1000,
            ..limits(1)
        })
        .unwrap_err();
    let wire = serde_json::to_value(error.downcast_ref::<Refusal>().unwrap()).unwrap();
    assert_eq!(wire["kind"], "scan-budget");
    assert!(wire["needed"].as_u64().unwrap() > 2000);
    let enough = db
        .reader()
        .tail(&Limits {
            scan_bytes: wire["needed"].as_u64().unwrap() as usize,
            ..limits(1)
        })
        .unwrap();
    assert_eq!(enough.items[0].text.len(), 2000);
    let r = db.reader();
    db.sql("DROP TABLE part;");
    let error = r.tail(&limits(1)).unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<Refusal>(),
            Some(Refusal::Unavailable { .. })
        ),
        "{error:?}"
    );
}

// Tools views can show shared status, labels, metadata, caller and search
// arguments without consulting backend-specific native JSON paths.
#[test]
fn normalized_tools_expose_shared_status_and_view_fields() {
    for harness in ["claude", "codex", "pi"] {
        let f = Fixture::new(harness, 0);
        f.append(&f.tool(false));
        let call = f.reader().tail(&limits(1)).unwrap();
        assert_eq!(
            serde_json::to_value(&call.items[0]).unwrap()["tool"]["status"],
            "running"
        );
        let mut result = f.tool(true);
        match harness {
            "claude" => result["message"]["content"][0]["is_error"] = json!(true),
            "pi" => {
                result["message"]["isError"] = json!(true);
                result["message"]["details"] = json!({"exitCode":1});
            }
            _ => result["payload"]["status"] = json!("failed"),
        }
        f.append(&result);
        let failed = f.reader().tail(&limits(1)).unwrap();
        assert_eq!(
            serde_json::to_value(&failed.items[0]).unwrap()["tool"]["status"],
            "failed",
            "{harness}"
        );
        if harness == "pi" {
            assert_eq!(
                serde_json::to_value(&failed.items[0]).unwrap()["tool"]["metadata"]["exitCode"],
                1
            );
        }
        let mut completed = f.tool(true);
        if harness == "codex" {
            completed["payload"]["status"] = json!("completed");
        }
        f.append(&completed);
        assert_eq!(
            serde_json::to_value(&f.reader().tail(&limits(1)).unwrap().items[0]).unwrap()["tool"]
                ["status"],
            "completed"
        );
    }
    let f = Fixture::new("codex", 0);
    f.append(&json!({"type":"response_item","payload":{"type":"web_search_call","id":"search","status":"completed","action":{"type":"search","query":"fixture query","queries":["fixture query"]}}}));
    let item = &f.reader().tail(&limits(1)).unwrap().items[0];
    let wire = serde_json::to_value(item).unwrap();
    assert_eq!(wire["tool"]["arguments"]["query"], "fixture query");
    assert_eq!(wire["tool"]["status"], "completed");
    let f = Fixture::new("claude", 0);
    let mut call = f.tool(false);
    call["message"]["content"][0]["caller"] = json!({"type":"direct"});
    f.append(&call);
    assert_eq!(
        serde_json::to_value(&f.reader().tail(&limits(1)).unwrap().items[0]).unwrap()["tool"]
            ["caller"]["type"],
        "direct"
    );
    let db = Database::new(0);
    db.sql(&Database::insert(0,&json!({"type":"tool","tool":"bash","callID":"c","state":{"status":"error","title":"Fixture tool","metadata":{"exit":1},"input":{},"error":"failed"}})));
    let page = db.reader().tail(&limits(2)).unwrap();
    let wire = serde_json::to_value(&page.items[0]).unwrap();
    assert_eq!(wire["tool"]["status"], "failed");
    assert_eq!(wire["tool"]["title"], "Fixture tool");
    assert_eq!(wire["tool"]["metadata"]["exit"], 1);
    assert_eq!(wire["content"]["state"]["status"], "error");
    assert!(page.items[0].budget_bytes().unwrap() < 2000);
}

// New JSON kinds are retained as unknown, while a typo in a filter is invalid.
#[test]
fn json_readers_accept_future_kinds_and_stops_without_accepting_filter_typos() {
    assert_eq!(
        serde_json::from_str::<Kind>("\"future-kind\"").unwrap(),
        Kind::Unknown
    );
    assert!("future-kind".parse::<Kind>().is_err());
    assert!(matches!(
        serde_json::from_str::<Stop>("\"future-stop\"").unwrap(),
        Stop::Unknown
    ));
}
