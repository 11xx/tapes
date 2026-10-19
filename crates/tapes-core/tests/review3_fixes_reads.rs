//! Reads stay inside the bounds and relationships they claim in the cases an
//! independent review named: a launch marker names a parent in the harness
//! it names, a recording's final record is whole without a trailing line
//! feed, an external parent's store-declared fork outranks a marker it
//! copied, rates grouped without parents resolve no parent, a keep bound
//! holds for parents reached through their children, and a discovery page
//! charges the lineage metadata it retains.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, opencode::OpenCodeBackend, pi::PiBackend, Backend,
    Listing, Query, StreamOptions, StreamedTranscript,
};
use agent_tapes_core::lineage::Lineage;
use agent_tapes_core::model::{AccountingBasis, KindDeclaration, Session, Transcript, Turn};
use agent_tapes_core::span::{SpanBounds, SpanSessions};
use agent_tapes_core::usage::{GroupBy, UsageObservationOptions, UsageObservationResult};
use agent_tapes_core::window::{QuotaKind, WindowBounds};
use agent_tapes_core::{
    allowance_rates_with_backends, usage_summary_with_backends, usage_window_with_backends,
    NativeStore, SessionSelection, WindowAsk,
};
use serde_json::{json, Value};

/// A temporary store root, removed when dropped.
struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-review3-reads-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write_text(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn write(&self, relative: &str, lines: &[Value]) -> PathBuf {
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        self.write_text(relative, &(body.join("\n") + "\n"))
    }

    fn claude(&self, id: &str, minute: u32, prompt: &str) {
        self.write(
            &format!("claude/projects/-fixtures-project/{id}.jsonl"),
            &claude_session(id, minute, prompt),
        );
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn at(stamp: &str) -> chrono::DateTime<chrono::Utc> {
    stamp.parse().unwrap()
}

/// A headless Claude session whose first user message is `prompt`, then one
/// request at `minute`.
fn claude_session(id: &str, minute: u32, prompt: &str) -> Vec<Value> {
    let stamp = |second: u32| format!("2026-01-10T10:{minute:02}:{second:02}Z");
    vec![
        json!({"type": "queue-operation", "operation": "enqueue", "sessionId": id,
               "timestamp": stamp(0)}),
        json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"), "entrypoint": "sdk-cli",
               "timestamp": stamp(1), "cwd": "/fixtures/project",
               "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}}),
        json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"), "entrypoint": "sdk-cli",
               "requestId": format!("{id}-request"), "timestamp": stamp(30), "cwd": "/fixtures/project",
               "message": {"role": "assistant", "model": "claude-fixture",
                           "content": [{"type": "text", "text": "ok"}],
                           "usage": {"input_tokens": 1, "output_tokens": 5, "cache_read_input_tokens": 10,
                                     "cache_creation_input_tokens": 2}}}),
    ]
}

/// A Codex exec rollout's records: its header, with `parent` as its
/// `parent_thread_id` where given, its first prompt, and one request, all
/// at `stamp`.
fn codex_session(id: &str, stamp: &str, parent: Option<&str>, prompt: &str) -> Vec<Value> {
    let total = json!({"input_tokens": 100, "cached_input_tokens": 0, "output_tokens": 7,
                       "reasoning_output_tokens": 0, "total_tokens": 107});
    let mut meta = json!({"id": id, "session_id": id, "timestamp": stamp, "cwd": "/fixtures/project",
                          "source": "exec", "originator": "codex_exec"});
    if let Some(parent) = parent {
        meta["parent_thread_id"] = json!(parent);
    }
    vec![
        json!({"timestamp": stamp, "type": "session_meta", "payload": meta}),
        json!({"timestamp": stamp, "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
        json!({"timestamp": stamp, "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": prompt}]}}),
        json!({"timestamp": stamp, "type": "event_msg",
               "payload": {"type": "token_count",
                           "info": {"total_token_usage": total, "last_token_usage": total,
                                    "model_context_window": 272000}}}),
    ]
}

/// Where a Codex rollout of `id` starting at `stamp` lives under its root.
fn codex_path(id: &str, stamp: &str) -> String {
    let date = &stamp[..10];
    let time = stamp[11..19].replace(':', "-");
    format!(
        "{}/rollout-{date}T{time}-{id}.jsonl",
        date.replace('-', "/")
    )
}

fn marker(harness: &str, id: &str) -> String {
    format!("tapes-parent: {harness}:{id}\nInspect the fixture.")
}

fn share<'a>(read: &'a Value, id: &str) -> &'a Value {
    read["window"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|share| share["id"] == id)
        .unwrap_or_else(|| panic!("no share of {id}: {:#}", read["window"]))
}

fn parent_groups(summary: &Value) -> Vec<(String, u64)> {
    summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["key"]["parent"].as_str().unwrap().to_owned(),
                group["sessions"].as_u64().unwrap(),
            )
        })
        .collect()
}

const CLAUDE_NAMED: &str = "30000000-0000-4000-8000-0000000000a1";
const CLAUDE_ROOT: &str = "30000000-0000-4000-8000-0000000000a2";
const CODEX_CHILD: &str = "01a10000-0000-7000-8000-0000000000a3";

/// Claim: a marker naming `pi:<id>` names a Pi session, so where only a
/// Claude session holds that id the child's root is the Pi id, unresolved.
/// Rejects resolving the marker's id in every harness, which roots the
/// child at the Claude session's own launcher, resolved.
#[test]
fn a_marker_names_its_parent_in_the_harness_it_names() {
    let store = Store::new("marker-harness");
    store.claude(CLAUDE_ROOT, 1, "Lead the fixture work.");
    store.claude(CLAUDE_NAMED, 2, &marker("claude", CLAUDE_ROOT));
    let stamp = "2026-01-10T10:03:00Z";
    store.write(
        &format!("codex/{}", codex_path(CODEX_CHILD, stamp)),
        &codex_session(CODEX_CHILD, stamp, None, &marker("pi", CLAUDE_NAMED)),
    );
    fs::create_dir_all(store.0.join("pi")).unwrap();
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(ClaudeBackend::new(store.0.join("claude/projects"))),
        Box::new(CodexBackend::new(store.0.join("codex"))),
        Box::new(PiBackend::new(store.0.join("pi"))),
    ];
    let selection = SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        ..Default::default()
    };
    let read = serde_json::to_value(
        usage_window_with_backends(
            &backends,
            &selection,
            &[GroupBy::Parent],
            WindowAsk::Bounds(WindowBounds {
                from: Some(at("2026-01-10T09:00:00Z")),
                to: None,
            }),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    let child = share(&read, CODEX_CHILD);
    assert_eq!(child["parent"], CLAUDE_NAMED, "{child}");
    assert_eq!(child["root"], CLAUDE_NAMED, "{child}");
    assert_eq!(child["root_resolved"], false, "{child}");
    assert_eq!(
        parent_groups(&read),
        vec![(CLAUDE_NAMED.to_owned(), 1)],
        "{read:#}"
    );
}

const LAUNCHER: &str = "30000000-0000-4000-8000-0000000000b1";
const UNTERMINATED: &str = "30000000-0000-4000-8000-0000000000b2";

/// Claim: a recording whose final record is its marked first prompt,
/// complete but without a trailing line feed, names its launcher, as the
/// recording's opening reads that record. Rejects a first-prompt read that
/// drops the final record, which groups the child under itself.
#[test]
fn a_final_first_prompt_without_a_line_feed_names_its_launcher() {
    let store = Store::new("unterminated");
    store.claude(LAUNCHER, 1, "Lead the fixture work.");
    let records = claude_session(UNTERMINATED, 2, &marker("claude", LAUNCHER));
    let body: Vec<String> = records[..2].iter().map(Value::to_string).collect();
    store.write_text(
        &format!("claude/projects/-fixtures-project/{UNTERMINATED}.jsonl"),
        &body.join("\n"),
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(
        store.0.join("claude/projects"),
    ))];
    let selection = SessionSelection {
        limit: Some(50),
        ..Default::default()
    };
    let summary = serde_json::to_value(
        usage_summary_with_backends(&backends, &selection, &[GroupBy::Parent]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parent_groups(&summary),
        vec![(LAUNCHER.to_owned(), 2)],
        "{summary:#}"
    );
}

const OPENCODE_CHILD: &str = "ses_000000fixtureSharedSession";
const OPENCODE_EXTERNAL: &str = "ses_database_only_fixture";
const OPENCODE_MISSING_ORIGIN: &str = "ses_missing_origin_fixture";
const MARKED_PARENT: &str = "30000000-0000-4000-8000-0000000000c1";

/// The OpenCode fixture program over a database whose session table lists
/// only `OPENCODE_CHILD`, a child of `OPENCODE_EXTERNAL`, which forks the
/// missing `OPENCODE_MISSING_ORIGIN` and whose first prompt is a copied
/// marker naming the Claude session `MARKED_PARENT`.
fn opencode_fork_store(store: &Store) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
    let program = store.0.join("opencode");
    fs::create_dir_all(&program).unwrap();
    fs::copy(fixtures.join("opencode2"), program.join("opencode2")).unwrap();
    let child = json!({"id": OPENCODE_CHILD, "title": "Child fixture session",
        "directory": "/fixtures/project", "time_created": 1_784_662_770_000_u64,
        "time_updated": 1_784_662_790_000_u64, "model": "{\"id\":\"fixture-model\"}", "cost": 0.5,
        "tokens_input": 100, "tokens_output": 40, "tokens_reasoning": 0, "tokens_cache_read": 0,
        "tokens_cache_write": 0});
    let external = json!({"id": OPENCODE_EXTERNAL, "title": "External fixture session",
        "directory": "/fixtures/project", "time_created": 1_784_662_700_000_u64,
        "time_updated": 1_784_662_710_000_u64, "model": "{\"id\":\"fixture-model\"}", "cost": 0.5,
        "tokens_input": 100, "tokens_output": 40, "tokens_reasoning": 0, "tokens_cache_read": 0,
        "tokens_cache_write": 0});
    for (file, row) in [
        ("database-sessions.json", child.clone()),
        ("database-locate-shared.json", child),
        ("database-locate-database-only.json", external),
        (
            "database-lineage-shared.json",
            json!({"id": OPENCODE_CHILD, "parent_id": OPENCODE_EXTERNAL, "fork_session_id": null}),
        ),
        (
            "database-lineage-database-only.json",
            json!({"id": OPENCODE_EXTERNAL, "parent_id": null,
                   "fork_session_id": OPENCODE_MISSING_ORIGIN}),
        ),
        (
            "database-first-prompt-database-only.json",
            json!({"session_id": OPENCODE_EXTERNAL,
                   "first_prompt": marker("claude", MARKED_PARENT)}),
        ),
    ] {
        fs::write(program.join(file), format!("{row}\n")).unwrap();
    }
    fs::create_dir_all(program.join("bin")).unwrap();
    std::os::unix::fs::symlink(program.join("opencode2"), program.join("bin/opencode")).unwrap();
    let data = store.0.join("data/opencode");
    fs::create_dir_all(&data).unwrap();
    fs::File::create(data.join("opencode.db")).unwrap();
    std::env::set_var("XDG_DATA_HOME", store.0.join("data"));
    program.join("bin/opencode")
}

/// Claim: a parent outside the selection is read with the relationships its
/// store declares beside its recording, so an OpenCode parent that forks a
/// missing session roots its child at that fork origin, unresolved, though
/// its first prompt carries a marker it copied naming a present session.
/// Rejects reading the external parent's marker without its store-declared
/// fork origin, which roots the child at the session the copied marker names.
#[test]
fn an_external_parent_s_declared_fork_outranks_the_marker_it_copied() {
    let store = Store::new("external-fork");
    store.claude(MARKED_PARENT, 1, "Lead the fixture work.");
    let program = opencode_fork_store(&store);
    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(OpenCodeBackend::new(&program)),
        Box::new(ClaudeBackend::new(store.0.join("claude/projects"))),
    ];
    let selection = SessionSelection {
        harness: Some("opencode"),
        limit: Some(50),
        ..Default::default()
    };
    let summary = serde_json::to_value(
        usage_summary_with_backends(&backends, &selection, &[GroupBy::Parent]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        parent_groups(&summary),
        vec![(OPENCODE_MISSING_ORIGIN.to_owned(), 1)],
        "{summary:#}"
    );
}

/// A Codex backend that counts the relative lookups and first-prompt reads
/// asked of it.
struct CountedCodex {
    inner: CodexBackend,
    lookups: Rc<Cell<usize>>,
    first_prompts: Rc<Cell<usize>>,
}

impl Backend for CountedCodex {
    fn kinds(&self) -> KindDeclaration {
        self.inner.kinds()
    }
    fn history_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> anyhow::Result<agent_tapes_core::history::Page> {
        self.inner.history_page(session, cursor, bytes)
    }
    fn metadata_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> anyhow::Result<agent_tapes_core::history::Page> {
        self.inner.metadata_page(session, cursor, bytes)
    }
    fn harness(&self) -> &'static str {
        self.inner.harness()
    }
    fn accounting_bases(&self) -> &'static [AccountingBasis] {
        self.inner.accounting_bases()
    }
    fn available(&self) -> bool {
        self.inner.available()
    }
    fn list_titles(&self, query: &Query) -> anyhow::Result<Listing> {
        self.inner.list_titles(query)
    }
    fn list(&self, query: &Query) -> anyhow::Result<Listing> {
        self.inner.list(query)
    }
    fn list_with_search(
        &self,
        query: &Query,
        needle: &str,
        tail: usize,
    ) -> anyhow::Result<Listing> {
        self.inner.list_with_search(query, needle, tail)
    }
    fn locate(&self, id: &str) -> anyhow::Result<Option<Session>> {
        self.inner.locate(id)
    }
    fn locate_relative(&self, id: &str) -> anyhow::Result<Option<Session>> {
        self.lookups.set(self.lookups.get() + 1);
        self.inner.locate_relative(id)
    }
    fn transcript(&self, session: &Session, tail: usize) -> anyhow::Result<Transcript> {
        self.inner.transcript(session, tail)
    }
    fn supports_full_search(&self) -> bool {
        self.inner.supports_full_search()
    }
    fn stream_transcript_with(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        options: StreamOptions,
        turn: &mut dyn FnMut(Turn) -> anyhow::Result<()>,
    ) -> anyhow::Result<StreamedTranscript> {
        self.inner
            .stream_transcript_with(session, replay, options, turn)
    }
    fn first_prompt_markers(
        &self,
        sessions: &[&Session],
    ) -> Vec<Option<anyhow::Result<agent_tapes_core::launch_parent::LaunchMarker>>> {
        self.first_prompts
            .set(self.first_prompts.get() + sessions.len());
        self.inner.first_prompt_markers(sessions)
    }
    fn lineage(&self, session: &Session) -> anyhow::Result<Lineage> {
        self.inner.lineage(session)
    }
    fn stream_lineage(&self, session: &Session) -> anyhow::Result<Lineage> {
        self.inner.stream_lineage(session)
    }
    fn stream_session(
        &self,
        session: &Session,
        read: &StreamedTranscript,
    ) -> anyhow::Result<Session> {
        self.inner.stream_session(session, read)
    }
    fn allowance_timeline(
        &self,
        session: &Session,
    ) -> anyhow::Result<Option<agent_tapes_core::allowance::Timeline>> {
        self.inner.allowance_timeline(session)
    }
    fn stream_spans(
        &self,
        session: &Session,
        read: &StreamedTranscript,
        spans: &[SpanBounds],
    ) -> anyhow::Result<SpanSessions> {
        self.inner.stream_spans(session, read, spans)
    }
    fn usage_observations(
        &self,
        session: &Session,
        read: Option<&agent_tapes_core::model::ReadEvidence>,
        options: UsageObservationOptions,
    ) -> anyhow::Result<UsageObservationResult> {
        self.inner.usage_observations(session, read, options)
    }
}

const SPAWN_PARENT: &str = "01a10000-0000-7000-8000-0000000000d1";
const SPAWN_CHILD: &str = "01a10000-0000-7000-8000-0000000000d2";

/// Relative lookups and first-prompt reads that rates grouped by `by` ask
/// of a Codex store holding a spawn inside the lookback whose parent lies
/// before it.
fn rates_reads(by: GroupBy) -> (usize, usize) {
    let store = Store::new("rates-parents");
    let parent_stamp = "2026-01-10T08:00:00Z";
    store.write(
        &codex_path(SPAWN_PARENT, parent_stamp),
        &codex_session(SPAWN_PARENT, parent_stamp, None, "Lead the fixture work."),
    );
    let child_stamp = "2026-01-10T10:00:00Z";
    store.write(
        &codex_path(SPAWN_CHILD, child_stamp),
        &codex_session(
            SPAWN_CHILD,
            child_stamp,
            Some(SPAWN_PARENT),
            "Inspect the fixture.",
        ),
    );
    let lookups = Rc::new(Cell::new(0));
    let first_prompts = Rc::new(Cell::new(0));
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(CountedCodex {
        inner: CodexBackend::new(&store.0),
        lookups: lookups.clone(),
        first_prompts: first_prompts.clone(),
    })];
    let selection = SessionSelection {
        harness: Some("codex"),
        limit: Some(50),
        ..Default::default()
    };
    allowance_rates_with_backends(
        &backends,
        &selection,
        &[by],
        WindowBounds {
            from: Some(at("2026-01-10T09:00:00Z")),
            to: Some(at("2026-01-10T12:00:00Z")),
        },
        QuotaKind::Primary,
    )
    .unwrap();
    (lookups.get(), first_prompts.get())
}

/// Claim: rates grouped by model resolve no parent of a lane, so a spawn
/// whose parent lies outside the lookback asks the store neither for that
/// parent nor for its first prompt; grouped by parent, they do. Rejects
/// resolving every lane's parent chain whatever the grouping.
#[test]
fn rates_grouped_without_parents_resolve_no_parent() {
    assert_eq!(rates_reads(GroupBy::Model), (0, 0));
    let (lookups, first_prompts) = rates_reads(GroupBy::Parent);
    assert!(lookups > 0, "a grouping by parent resolves the parent");
    assert!(first_prompts > 0, "a grouping by parent reads its marker");
}

const QUIET_PARENT: &str = "30000000-0000-4000-8000-0000000000e1";
const QUIET_AGENT: &str = "a00000000000000e1";

fn claude_request(id: &str, agent: Option<&str>, entry: &str, stamp: &str) -> Value {
    let mut record = json!({
        "sessionId": id, "cwd": "/fixtures/project", "entrypoint": "cli", "version": "2.1.0",
        "type": "assistant", "uuid": entry, "parentUuid": null, "requestId": format!("req_{entry}"),
        "timestamp": stamp,
        "message": {"id": format!("msg_{entry}"), "role": "assistant", "model": "claude-fixture",
            "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                      "output_tokens": 20},
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "Done."}]}
    });
    if let Some(agent) = agent {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(agent);
    }
    record
}

/// Claim: a caller's --limit bounds the sessions rates read, parents
/// reached only through their children included, so `--limit 0` reads no
/// lane and counts the parent as left out. Rejects adding a parent the
/// listing left out after the bound was applied, which reads it and its
/// subagent as lanes.
#[test]
fn a_keep_bound_holds_for_parents_reached_through_children() {
    let store = Store::new("keep-parents");
    store.write(
        &format!("project/{QUIET_PARENT}.jsonl"),
        &[claude_request(
            QUIET_PARENT,
            None,
            "parent-1",
            "2026-01-12T09:59:00Z",
        )],
    );
    store.write(
        &format!("project/{QUIET_PARENT}/subagents/agent-{QUIET_AGENT}.jsonl"),
        &[claude_request(
            QUIET_PARENT,
            Some(QUIET_AGENT),
            "child-1",
            "2026-01-12T10:05:00Z",
        )],
    );
    let backends: Vec<Box<dyn Backend>> = vec![Box::new(ClaudeBackend::new(&store.0))];
    let rates = |limit| {
        let selection = SessionSelection {
            harness: Some("claude"),
            limit: Some(limit),
            ..Default::default()
        };
        serde_json::to_value(
            allowance_rates_with_backends(
                &backends,
                &selection,
                &[GroupBy::Session],
                WindowBounds {
                    from: Some(at("2026-01-12T10:00:00Z")),
                    to: Some(at("2026-01-12T11:00:00Z")),
                },
                QuotaKind::Primary,
            )
            .unwrap(),
        )
        .unwrap()
    };
    let bounded = rates(0);
    assert!(bounded.get("excluded_absent").is_none(), "{bounded:#}");
    assert_eq!(bounded["excluded_running"], 0, "{bounded:#}");
    assert_eq!(bounded["lanes_read"]["left_out_by_limit"], 1, "{bounded:#}");
    let kept = rates(1);
    assert_eq!(
        kept["excluded_absent"].as_array().map(Vec::len),
        Some(1),
        "the subagent is the kept parent's lane: {kept:#}"
    );
    assert!(
        kept["lanes_read"].get("left_out_by_limit").is_none(),
        "{kept:#}"
    );
}

const LINEAGE_HEADERS: usize = 64;
const NICKNAME_BYTES: usize = 512 * 1024;

/// Claim: a discovery page charges the Codex lineage metadata it retains
/// against its 16 MiB bound, so 64 child headers each naming a 512 KiB
/// nickname end the page incomplete at that bound with at most 16 MiB of
/// metadata held. Rejects a page that keeps over 32 MiB of metadata and
/// reports itself complete.
#[test]
fn a_discovery_page_charges_the_lineage_metadata_it_retains() {
    let store = Store::new("lineage-budget");
    let nickname = "n".repeat(NICKNAME_BYTES);
    for index in 0..LINEAGE_HEADERS {
        let id = format!("01a10000-0000-7000-8000-{index:012x}");
        let stamp = format!("2026-01-10T10:{:02}:{:02}Z", index / 60, index % 60);
        let header = json!({"timestamp": stamp, "type": "session_meta",
            "payload": {"id": id, "timestamp": stamp, "cwd": "/fixtures/project",
                        "parent_thread_id": SPAWN_PARENT, "agent_path": "/root/worker",
                        "agent_nickname": nickname}});
        store.write(&codex_path(&id, &stamp), &[header]);
    }
    let page = NativeStore::codex(&store.0).candidates(LINEAGE_HEADERS * 2);
    let retained: usize = page
        .records
        .iter()
        .filter_map(|record| record.metadata())
        .map(|metadata| metadata.to_string().len())
        .sum();
    assert!(
        retained <= 16 * 1024 * 1024,
        "{} records retain {retained} bytes of metadata",
        page.records.len()
    );
    assert!(!page.complete, "a page cut at its bound is incomplete");
    assert!(
        page.failures
            .iter()
            .any(|failure| failure.to_string().contains("16 MiB retained data")),
        "{:?}",
        page.failures
    );
}
