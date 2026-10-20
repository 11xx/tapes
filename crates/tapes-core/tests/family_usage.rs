//! A session's family is the session and everything it caused: its
//! subagents, the sessions that name a member in a `tapes-parent` marker in
//! any harness, recursively, and with `--continuations` the session a
//! continued one carried over from. Each member is counted once.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::family::{family_usage_with_backends, FamilyOptions};
use agent_tapes_core::{usage_full_with_backends, Selection};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Store(PathBuf);

impl Store {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-family-usage-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, relative: &str, lines: &[Value]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }

    fn claude(&self, id: &str, lines: &[Value]) {
        self.write(
            &format!("claude/projects/-fixtures-project/{id}.jsonl"),
            lines,
        );
    }

    fn claude_subagent(&self, parent: &str, agent: &str, lines: &[Value]) {
        self.write(
            &format!("claude/projects/-fixtures-project/{parent}/subagents/agent-{agent}.jsonl"),
            lines,
        );
        fs::write(
            self.0.join(format!(
                "claude/projects/-fixtures-project/{parent}/subagents/agent-{agent}.meta.json"
            )),
            r#"{"agentType":"fixture-reader","description":"Read the fixture","spawnDepth":1}"#,
        )
        .unwrap();
    }

    fn codex(&self, id: &str, minute: u32, lines: &[Value]) {
        self.write(
            &format!("codex/2026/01/10/rollout-2026-01-10T10-{minute:02}-00-{id}.jsonl"),
            lines,
        );
    }

    fn pi(&self, id: &str, minute: u32, lines: &[Value]) {
        self.write(
            &format!("pi/--fixtures-project--/2026-01-10T10-{minute:02}-00-000Z_{id}.jsonl"),
            lines,
        );
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.0.join("claude/projects"))),
            Box::new(CodexBackend::new(self.0.join("codex"))),
            Box::new(PiBackend::new(self.0.join("pi"))),
        ]
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const LEAD: &str = "30000000-0000-4000-8000-0000000000f1";
const AGENT: &str = "a0123456789abcdef";
const CODEX_CHILD: &str = "01a10000-0000-7000-8000-0000000000f2";
const PI_GRANDCHILD: &str = "01a20000-0000-7000-8000-0000000000f3";
const FOREIGN: &str = "01a10000-0000-7000-8000-0000000000f4";
const OTHER_PARENT: &str = "30000000-0000-4000-8000-0000000000ff";
const EARLIER: &str = "30000000-0000-4000-8000-0000000000e1";
const CONTINUING: &str = "30000000-0000-4000-8000-0000000000e2";

fn stamp(minute: u32, second: u32) -> String {
    format!("2026-01-10T10:{minute:02}:{second:02}Z")
}

fn claude_user(id: &str, uuid: &str, at: String, prompt: &str) -> Value {
    json!({"type": "user", "sessionId": id, "uuid": uuid, "timestamp": at,
           "cwd": "/fixtures/project", "entrypoint": "cli",
           "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}})
}

fn claude_request(id: &str, uuid: &str, request: &str, at: String, output: u64) -> Value {
    json!({"type": "assistant", "sessionId": id, "uuid": uuid, "requestId": request,
           "timestamp": at, "cwd": "/fixtures/project", "entrypoint": "cli",
           "message": {"role": "assistant", "id": format!("msg_{request}"),
                       "model": "claude-fixture", "stop_reason": "end_turn",
                       "content": [{"type": "text", "text": "ok"}],
                       "usage": {"input_tokens": 1, "output_tokens": output,
                                 "cache_read_input_tokens": 10,
                                 "cache_creation_input_tokens": 2}}})
}

fn claude_lead() -> Vec<Value> {
    vec![
        claude_user(LEAD, "lead-user", stamp(0, 1), "Lead the fixture work."),
        claude_request(LEAD, "lead-reply", "req_lead", stamp(0, 30), 5),
        claude_request(LEAD, "lead-end", "req_lead_end", stamp(40, 0), 6),
    ]
}

fn claude_agent() -> Vec<Value> {
    let mut user = claude_user(LEAD, "agent-user", stamp(5, 0), "Read the fixture.");
    let mut reply = claude_request(LEAD, "agent-reply", "req_agent", stamp(5, 30), 7);
    for record in [&mut user, &mut reply] {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(AGENT);
    }
    vec![user, reply]
}

/// A Codex exec rollout whose second user message is `prompt`, then one
/// request at `minute`.
fn codex_session(id: &str, minute: u32, prompt: &str, output: u64) -> Vec<Value> {
    let at = stamp(minute, 0);
    let total = json!({"input_tokens": 100, "cached_input_tokens": 0, "output_tokens": output,
                       "reasoning_output_tokens": 0, "total_tokens": 100 + output});
    vec![
        json!({"timestamp": at, "type": "session_meta",
               "payload": {"id": id, "session_id": id, "timestamp": at, "cwd": "/fixtures/project",
                           "source": "exec", "originator": "codex_exec"}}),
        json!({"timestamp": at, "type": "turn_context",
               "payload": {"cwd": "/fixtures/project", "model": "gpt-fixture"}}),
        json!({"timestamp": at, "type": "response_item",
               "payload": {"type": "message", "role": "user", "content": [
                   {"type": "input_text", "text":
                       "<environment_context>\n  <cwd>/fixtures/project</cwd>\n</environment_context>"}]}}),
        json!({"timestamp": at, "type": "response_item",
               "payload": {"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": prompt}]}}),
        json!({"timestamp": stamp(minute + 5, 0), "type": "event_msg",
               "payload": {"type": "token_count",
                           "info": {"total_token_usage": total, "last_token_usage": total,
                                    "model_context_window": 272000}}}),
    ]
}

fn pi_session(id: &str, minute: u32, prompt: &str, output: u64) -> Vec<Value> {
    vec![
        json!({"type": "session", "version": 3, "id": id, "timestamp": stamp(minute, 0),
               "cwd": "/fixtures/project"}),
        json!({"type": "message", "id": "p-user", "parentId": null, "timestamp": stamp(minute, 1),
               "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}}),
        json!({"type": "message", "id": "p-reply", "parentId": "p-user",
               "timestamp": stamp(minute, 10),
               "message": {"role": "assistant", "provider": "fixture-provider",
                           "model": "fixture-model", "responseId": "response-1",
                           "stopReason": "stop",
                           "usage": {"input": 20, "output": output, "cacheRead": 3,
                                     "cacheWrite": 4, "totalTokens": 27 + output,
                                     "cost": {"total": 0.5}},
                           "content": [{"type": "text", "text": "ok"}]}}),
    ]
}

fn marker(harness: &str, id: &str) -> String {
    format!("tapes-parent: {harness}:{id}\nInspect the fixture.")
}

/// The synthetic family: a Claude lead with one subagent, a launch-marked
/// Codex child, a launch-marked Pi grandchild under the Codex child, and a
/// Codex session whose marker names another parent.
fn family_store() -> Store {
    let store = Store::new("family");
    store.claude(LEAD, &claude_lead());
    store.claude_subagent(LEAD, AGENT, &claude_agent());
    store.codex(
        CODEX_CHILD,
        10,
        &codex_session(CODEX_CHILD, 10, &marker("claude", LEAD), 9),
    );
    store.pi(
        PI_GRANDCHILD,
        11,
        &pi_session(PI_GRANDCHILD, 11, &marker("codex", CODEX_CHILD), 11),
    );
    store.codex(
        FOREIGN,
        14,
        &codex_session(FOREIGN, 14, &marker("claude", OTHER_PARENT), 13),
    );
    store
}

fn family(store: &Store, continuations: bool) -> Value {
    let family = family_usage_with_backends(
        &store.backends(),
        Selection::Id(LEAD),
        FamilyOptions {
            continuations,
            prices: None,
        },
    )
    .unwrap();
    serde_json::to_value(family).unwrap()
}

/// The four disjoint categories of a figure: uncached input, cache read,
/// cache write and output.
fn categories(figure: &Value) -> [u64; 4] {
    let disjoint = &figure["disjoint"];
    let writes = disjoint["cache_write"]
        .as_object()
        .map(|writes| writes.values().filter_map(Value::as_u64).sum::<u64>())
        .unwrap_or(0);
    [
        disjoint["uncached_input"].as_u64().unwrap_or(0),
        disjoint["cache_read"].as_u64().unwrap_or(0),
        writes,
        disjoint["output"].as_u64().unwrap_or(0),
    ]
}

fn disjoint_total(figure: &Value) -> u64 {
    categories(figure).iter().sum()
}

/// The family's totals per accounting basis, added across its partitions.
fn partition_total(family: &Value) -> u64 {
    family["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(disjoint_total)
        .sum()
}

fn member<'a>(family: &'a Value, id: &str) -> &'a Value {
    family["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == id)
        .unwrap_or_else(|| panic!("{id} is not a member: {family:#}"))
}

fn full(store: &Store, id: &str) -> Value {
    serde_json::to_value(usage_full_with_backends(&store.backends(), Selection::Id(id)).unwrap())
        .unwrap()
}

/// Claim: the family of a Claude session prices the session, its subagent,
/// the Codex session whose first prompt names it, and the Pi session whose
/// first prompt names that Codex session, each once, with the roles lead,
/// subagent and delegate; its totals equal the sum of the members' own
/// figures. Rejects a family that stops at the launcher's own recording or
/// at the first generation, a member counted twice, and totals that differ
/// from their members' sum.
#[test]
fn a_family_prices_every_member_once_across_harnesses() {
    let store = family_store();
    let family = family(&store, false);
    assert_eq!(family["schema"], "tapes-family-usage/1");

    let members: Vec<(String, String, String)> = family["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| {
            (
                session["harness"].as_str().unwrap().to_owned(),
                session["id"].as_str().unwrap().to_owned(),
                session["role"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        members,
        vec![
            ("claude".to_owned(), LEAD.to_owned(), "lead".to_owned()),
            (
                "claude".to_owned(),
                format!("{LEAD}::{AGENT}"),
                "subagent".to_owned()
            ),
            (
                "codex".to_owned(),
                CODEX_CHILD.to_owned(),
                "delegate".to_owned()
            ),
            (
                "pi".to_owned(),
                PI_GRANDCHILD.to_owned(),
                "delegate".to_owned()
            ),
        ],
        "{family:#}"
    );
    assert_eq!(member(&family, CODEX_CHILD)["link"], "launch-marker");
    assert_eq!(member(&family, CODEX_CHILD)["parent"]["id"], LEAD);
    assert_eq!(member(&family, PI_GRANDCHILD)["parent"]["id"], CODEX_CHILD);
    assert_eq!(member(&family, PI_GRANDCHILD)["parent"]["harness"], "codex");

    // Each member's own recorded consumption: the lead's two requests, the
    // subagent's one, the Codex child's and the Pi grandchild's.
    let own = [
        disjoint_total(&full(&store, LEAD)),
        // 1 uncached + 10 cache read + 2 cache write + 7 output
        20,
        disjoint_total(&full(&store, CODEX_CHILD)),
        disjoint_total(&full(&store, PI_GRANDCHILD)),
    ];
    assert_eq!(own[0], (1 + 10 + 2 + 5) + (1 + 10 + 2 + 6));
    let summed: u64 = own.iter().sum();
    assert_eq!(partition_total(&family), summed, "{family:#}");
    assert_eq!(family["totals"]["sessions"], 4, "{family:#}");

    // Agents are keyed by harness, model and effort, per role.
    let agents: Vec<(String, String)> = family["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| {
            (
                agent["agent"].as_str().unwrap().to_owned(),
                agent["role"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(agents.contains(&("claude:claude-fixture".to_owned(), "lead".to_owned())));
    assert!(agents.contains(&("claude:claude-fixture".to_owned(), "subagent".to_owned())));
    assert!(agents.contains(&("codex:gpt-fixture".to_owned(), "delegate".to_owned())));
    assert!(
        agents.contains(&(
            "pi:fixture-provider/fixture-model".to_owned(),
            "delegate".to_owned()
        )) || agents
            .iter()
            .any(|(agent, role)| agent.starts_with("pi:") && role == "delegate"),
        "{agents:?}"
    );
}

/// Claim: a session whose marker names a different parent is not a member,
/// and the search reports it examined that session. Rejects a family that
/// adds every session active in the interval, or one that reports no
/// coverage.
#[test]
fn a_session_naming_another_parent_is_not_a_member() {
    let store = family_store();
    let family = family(&store, false);
    assert!(
        family["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["id"] != FOREIGN),
        "{family:#}"
    );
    let search = &family["search"];
    assert!(
        search["candidates_examined"].as_u64().unwrap() >= 3,
        "{search:#}"
    );
    assert!(
        search["candidates_listed"].as_u64().unwrap() >= 4,
        "{search:#}"
    );
    assert_eq!(search["complete"], true, "{search:#}");
}

/// Claim: a launched session that began after the family's last member
/// stopped is outside every interval, so its opening is not read and it
/// joins no family, however its marker reads. Rejects a search that reads
/// every session listed.
#[test]
fn a_session_that_began_outside_every_interval_is_not_read() {
    let store = family_store();
    let late = "01a10000-0000-7000-8000-0000000000f5";
    store.write(
        &format!("codex/2026/01/10/rollout-2026-01-10T11-30-00-{late}.jsonl"),
        &{
            let mut lines = codex_session(late, 30, &marker("claude", LEAD), 3);
            for line in &mut lines {
                let at = line["timestamp"].as_str().unwrap().replace("T10:", "T11:");
                line["timestamp"] = json!(at);
                if line["type"] == "session_meta" {
                    line["payload"]["timestamp"] = json!(at);
                }
            }
            lines
        },
    );
    let family = family(&store, false);
    assert!(
        family["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|session| session["id"] != late),
        "{family:#}"
    );
    assert_eq!(family["search"]["outside_interval"], 1, "{family:#}");
}

/// A Claude request id recorded in the parent's recording and again in its
/// subagent's is one request.
#[test]
fn a_request_recorded_in_two_recordings_of_one_family_is_counted_once() {
    let store = Store::new("shared");
    store.claude(LEAD, &claude_lead());
    let mut agent = claude_agent();
    agent.push(claude_request(
        LEAD,
        "agent-shared",
        "req_lead",
        stamp(5, 40),
        5,
    ));
    for record in agent.iter_mut().skip(2) {
        record["isSidechain"] = json!(true);
        record["agentId"] = json!(AGENT);
    }
    store.claude_subagent(LEAD, AGENT, &agent);
    let family = family(&store, false);
    let subagent = member(&family, &format!("{LEAD}::{AGENT}"));
    assert_eq!(subagent["shared_requests"], 1, "{family:#}");
    // The lead's two requests plus the subagent's one of its own.
    assert_eq!(
        partition_total(&family),
        (1 + 10 + 2 + 5) + (1 + 10 + 2 + 6) + 20,
        "{family:#}"
    );
}

fn continuing(lines_after: Vec<Value>, logical_parent: &str) -> Vec<Value> {
    let mut lines = vec![json!({"type": "system", "subtype": "compact_boundary",
        "logicalParentUuid": logical_parent, "uuid": "boundary", "parentUuid": null,
        "sessionId": CONTINUING, "timestamp": stamp(20, 0), "cwd": "/fixtures/project",
        "content": "Conversation compacted"})];
    lines.extend(lines_after);
    lines
}

/// Claim: with continuations, a session whose recording opens with a compact
/// boundary naming a record of exactly one other recording is linked to it,
/// and that session's family joins; without the flag the earlier session is
/// not a member. Rejects a link guessed from time or proximity.
#[test]
fn a_continued_session_is_linked_through_its_compact_boundary() {
    let store = Store::new("continuation");
    store.claude(
        EARLIER,
        &[
            claude_user(EARLIER, "earlier-user", stamp(0, 1), "Start."),
            claude_request(EARLIER, "earlier-reply", "req_earlier", stamp(0, 30), 4),
        ],
    );
    store.claude(
        CONTINUING,
        &continuing(
            vec![claude_request(
                CONTINUING,
                "continuing-reply",
                "req_continuing",
                stamp(21, 0),
                8,
            )],
            "earlier-reply",
        ),
    );
    let ask = |continuations| {
        serde_json::to_value(
            family_usage_with_backends(
                &store.backends(),
                Selection::Id(CONTINUING),
                FamilyOptions {
                    continuations,
                    prices: None,
                },
            )
            .unwrap(),
        )
        .unwrap()
    };
    let without = ask(false);
    assert_eq!(
        without["sessions"].as_array().unwrap().len(),
        1,
        "{without:#}"
    );
    let with = ask(true);
    assert_eq!(with["continuations"][0]["status"], "linked", "{with:#}");
    assert_eq!(with["continuations"][0]["from"]["id"], EARLIER);
    assert_eq!(member(&with, EARLIER)["role"], "lead");
    assert_eq!(member(&with, EARLIER)["link"], "continuation");
    assert_eq!(
        partition_total(&with),
        (1 + 10 + 2 + 4) + (1 + 10 + 2 + 8),
        "{with:#}"
    );
}

/// Claim: a boundary whose record no searched recording holds is reported as
/// not found and no session is linked. Rejects guessing the nearest earlier
/// session.
#[test]
fn a_continuation_that_cannot_be_established_is_reported_not_guessed() {
    let store = Store::new("unlinked");
    store.claude(
        EARLIER,
        &[claude_user(EARLIER, "earlier-user", stamp(0, 1), "Start.")],
    );
    store.claude(
        CONTINUING,
        &continuing(
            vec![claude_request(
                CONTINUING,
                "continuing-reply",
                "req_continuing",
                stamp(21, 0),
                8,
            )],
            "no-such-record",
        ),
    );
    let family = serde_json::to_value(
        family_usage_with_backends(
            &store.backends(),
            Selection::Id(CONTINUING),
            FamilyOptions {
                continuations: true,
                prices: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        family["continuations"][0]["status"], "not-found",
        "{family:#}"
    );
    assert_eq!(
        family["sessions"].as_array().unwrap().len(),
        1,
        "{family:#}"
    );
}

fn opus(session: &str, request: &str, at: &str, counters: [u64; 5]) -> Value {
    let [input, read, write, output, thinking] = counters;
    json!({"type": "assistant", "sessionId": session, "uuid": format!("assistant-{request}"),
        "parentUuid": null, "requestId": request, "timestamp": at, "cwd": "/fixtures/project",
        "message": {"id": format!("msg-{request}"), "role": "assistant",
            "model": "claude-opus-5-5", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "Done."}],
            "usage": {"input_tokens": input, "cache_read_input_tokens": read,
                "cache_creation_input_tokens": write, "output_tokens": output,
                "output_tokens_details": {"thinking_tokens": thinking},
                "cache_creation": {"ephemeral_5m_input_tokens": 0,
                    "ephemeral_1h_input_tokens": write},
                "service_tier": "standard"}}})
}

fn cost_state(session: &str, models: [[u64; 5]; 2]) -> Value {
    let usage = |[input, output, thinking, read, write]: [u64; 5]| {
        json!({"inputTokens": input, "outputTokens": output, "thinkingTokens": thinking,
            "cacheReadInputTokens": read, "cacheCreationInputTokens": write,
            "webSearchRequests": 0, "costUSD": 0.25})
    };
    json!({"type": "cost-state", "sessionId": session, "totalCostUSD": 0.5,
        "totalAPIDuration": 4000, "totalAPIDurationWithoutRetries": 3900,
        "totalToolDuration": 2000, "totalLinesAdded": 0, "totalLinesRemoved": 0,
        "totalDuration": 600_000, "startTime": 1_792_576_800_000_i64,
        "hasUnknownModelCost": false,
        "modelUsage": {"claude-opus-5-5": usage(models[0]), "claude-haiku-5-5": usage(models[1])}})
}

/// Claim: a Claude recording whose accounting is a recorded total counts its
/// subagents' requests, so a subagent recording is listed with its own figure
/// and marked as counted in the parent's total, and the family's totals hold
/// the parent's recorded total once. Rejects totals that add the parent's
/// recorded total and the subagent's requests, which the recorded total
/// already holds.
#[test]
fn a_recorded_total_that_counts_a_subagent_is_not_added_to_it_again() {
    let store = Store::new("recorded-total");
    let at = "2026-01-10T10:01:00.000Z";
    store.claude(
        LEAD,
        &[
            claude_user(LEAD, "lead-user", stamp(0, 1), "Lead the fixture work."),
            opus(LEAD, "req_lead", at, [2, 1000, 400, 100, 10]),
            // Opus as its request; the Haiku total is the subagent's.
            cost_state(LEAD, [[2, 100, 10, 1000, 400], [6, 50, 0, 500, 100]]),
            cost_state(LEAD, [[2, 100, 10, 1000, 400], [6, 50, 0, 500, 100]]),
        ],
    );
    let mut agent = claude_agent();
    agent[1]["message"]["model"] = json!("claude-haiku-5-5");
    agent[1]["message"]["usage"] = json!({"input_tokens": 6, "output_tokens": 50,
        "cache_read_input_tokens": 500, "cache_creation_input_tokens": 100});
    store.claude_subagent(LEAD, AGENT, &agent);

    let family = family(&store, false);
    let subagent = member(&family, &format!("{LEAD}::{AGENT}"));
    assert_eq!(subagent["counted_in_parent_total"], true, "{family:#}");
    assert!(member(&family, LEAD)
        .get("counted_in_parent_total")
        .is_none());
    assert_eq!(
        family["partitions"][0]["accounting"]["basis"], "recorded-total",
        "{family:#}"
    );
    assert_eq!(
        partition_total(&family),
        disjoint_total(&full(&store, LEAD)),
        "{family:#}"
    );
    assert_eq!(family["totals"]["sessions"], 1, "{family:#}");
}
