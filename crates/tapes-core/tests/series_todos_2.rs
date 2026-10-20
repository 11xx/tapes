//! A Claude id lookup answers from identities a walk reads in filenames and
//! headers, so a miss does not depend on unrelated live recordings, and a
//! recording that changes under a read is read again. A pi recording whose
//! requests all went through a ChatGPT sign-in is billed by subscription,
//! and one whose requests all name one other provider is billed as that
//! provider's plan rule says.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{claude::ClaudeBackend, pi::PiBackend, Backend, Query};
use agent_tapes_core::pricing::{self, PriceTable};
use agent_tapes_core::{resolve_session, usage_full_with_backends, usage_with_backends, Selection};
use agent_tapes_discovery::ResolveError;
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

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
            "tapes-series-todos-2-{name}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
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

// --- pi billing --------------------------------------------------------------

const GPT_TABLE: &str = r#"
schema = "tapes-price-table/2"
currency = "USD"

[[models]]
id = "gpt-6-fixture"
effective = 2026-10-19
source = "fixture"
per_million = { uncached_input = 2.0, cache_read = 0.2, cache_write_5m = 2.5, output = 10.0 }

[models.long_context]
above_prompt_tokens = 272000
prompt_tokens = ["uncached_input", "cache_read", "cache_write"]
multipliers = { uncached_input = 2.0, cache_read = 2.0, cache_write_5m = 2.0, output = 1.5 }
"#;

const PI_ID: &str = "019f0000-0000-7000-8000-0000000000f1";

// A 300K-prompt request (100K uncached, 200K read, 1K output) at base and
// surcharged rates, and a 50K one (10K uncached, 40K read, 500 output)
// below the threshold.
const LARGE_BASE: f64 = (200_000.0 + 40_000.0 + 10_000.0) / 1e6;
const LARGE_SURCHARGED: f64 = (400_000.0 + 80_000.0 + 15_000.0) / 1e6;
const SMALL: f64 = (20_000.0 + 8_000.0 + 5_000.0) / 1e6;

fn pi_usage(input: u64, read: u64, output: u64) -> Value {
    json!({"input": input, "output": output, "cacheRead": read, "cacheWrite": 0, "reasoning": 0,
           "totalTokens": input + read + output,
           "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0}})
}

/// A pi session, as pi writes one, of a large and a small request through
/// `provider`'s `api`, with `extra` entries after them.
fn pi_session(store: &Store, provider: &str, api: &str, extra: &[Value]) {
    pi_session_of(store, [provider, provider], api, extra);
}

/// A pi session whose large request goes through the first provider and
/// whose small one goes through the second.
fn pi_session_of(store: &Store, providers: [&str; 2], api: &str, extra: &[Value]) {
    let provider = providers[0];
    let assistant = |id: &str, parent: &str, provider: &str, usage: Value| {
        json!({"type": "message", "id": id, "parentId": parent,
               "timestamp": "2026-10-19T10:00:30.000Z",
               "message": {"role": "assistant", "content": [{"type": "text", "text": "Done."}],
                           "api": api, "provider": provider, "model": "gpt-6-fixture",
                           "usage": usage, "stopReason": "stop", "timestamp": 1_791_000_000_000_u64,
                           "responseId": format!("resp_{id}")}})
    };
    let user = |id: &str, parent: &str| {
        json!({"type": "message", "id": id, "parentId": parent,
               "timestamp": "2026-10-19T10:00:01.000Z",
               "message": {"role": "user", "content": [{"type": "text", "text": "Survey."}],
                           "timestamp": 1_791_000_000_000_u64}})
    };
    let mut lines = vec![
        json!({"type": "session", "version": 3, "id": PI_ID,
               "timestamp": "2026-10-19T10:00:00.000Z", "cwd": "/fixtures/project"}),
        json!({"type": "model_change", "id": "model-1", "parentId": null,
               "timestamp": "2026-10-19T10:00:00.000Z", "provider": provider,
               "modelId": "gpt-6-fixture"}),
        user("user-1", "model-1"),
        assistant(
            "assistant-1",
            "user-1",
            provider,
            pi_usage(100_000, 200_000, 1_000),
        ),
        user("user-2", "assistant-1"),
        assistant(
            "assistant-2",
            "user-2",
            providers[1],
            pi_usage(10_000, 40_000, 500),
        ),
    ];
    lines.extend_from_slice(extra);
    let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
    let directory = store.0.join("--fixtures-project--");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join(format!("2026-10-19T10-00-00-000Z_{PI_ID}.jsonl")),
        body,
    )
    .unwrap();
}

/// The session's estimate from the bounded and the whole usage view, and
/// as a listing prices it.
fn pi_estimates(store: &Store) -> [Value; 3] {
    pi_estimates_under(store, GPT_TABLE)
}

/// [`pi_estimates`] under the price table `table`.
fn pi_estimates_under(store: &Store, table: &str) -> [Value; 3] {
    let table = PriceTable::parse(table).unwrap();
    let backend = || -> Box<dyn Backend> { Box::new(PiBackend::new(&store.0)) };
    let mut view = usage_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    pricing::price_view(&mut view, &table);
    let mut whole = usage_full_with_backends(&[backend()], Selection::Id(PI_ID)).unwrap();
    pricing::price_view(&mut whole, &table);
    let mut listed = backend()
        .list(&Query::unscoped(5))
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.id == PI_ID)
        .unwrap();
    listed.price(&table);
    [
        serde_json::to_value(view).unwrap()["list_price_estimate"].clone(),
        serde_json::to_value(whole).unwrap()["list_price_estimate"].clone(),
        serde_json::to_value(&listed.list_price_estimate).unwrap(),
    ]
}

fn close(left: &Value, right: f64) -> bool {
    left.as_f64()
        .is_some_and(|left| (left - right).abs() < 1e-9)
}

/// Claim: a pi session whose requests all went through the
/// `openai-codex-responses` API, a ChatGPT sign-in, is billed by
/// subscription, so its request above 272K pays base rates; the estimate is
/// complete and names that billing and its evidence. Rejects a pi session
/// priced between base and surcharged rates as billing-unestablished.
#[test]
fn a_pi_chatgpt_session_above_the_threshold_pays_base_rates() {
    let store = Store::new("pi-chatgpt");
    pi_session(&store, "openai-codex", "openai-codex-responses", &[]);
    for estimate in pi_estimates(&store) {
        assert!(
            close(&estimate["amount"], LARGE_BASE + SMALL),
            "{estimate:#}"
        );
        assert_eq!(
            estimate["billing"],
            json!([{"terms": "subscription", "plan": "chatgpt-subscription",
                    "evidence": "pi `openai-codex-responses` requests, a ChatGPT sign-in"}])
        );
        assert!(
            estimate.get("measurement_completeness").is_none(),
            "{estimate:#}"
        );
    }
}

/// Claim: a pi session establishes no billing when a token-carrying event
/// is not a ChatGPT sign-in's request: an `anthropic` request, which may be
/// an OAuth sign-in or an API key, or a compaction, which names no API. Its
/// request above 272K is bounded billing-unestablished. Rejects a
/// subscription read from some of a session's requests.
#[test]
fn a_pi_session_with_any_other_token_event_establishes_no_billing() {
    let compaction = json!({"type": "compaction", "id": "compaction-1", "parentId": "assistant-2",
        "timestamp": "2026-10-19T10:05:00.000Z", "summary": "Summary.",
        "firstKeptEntryId": "user-2", "tokensBefore": 300_000, "details": {},
        "usage": pi_usage(5_000, 0, 500), "fromHook": false});
    for (label, provider, api, extra) in [
        ("pi-anthropic", "anthropic", "anthropic-messages", vec![]),
        (
            "pi-compaction",
            "openai-codex",
            "openai-codex-responses",
            vec![compaction],
        ),
    ] {
        let store = Store::new(label);
        pi_session(&store, provider, api, &extra);
        for estimate in pi_estimates(&store) {
            assert!(estimate.get("amount").is_none(), "{label}: {estimate:#}");
            assert!(
                estimate["at_most"].as_f64().unwrap() - estimate["at_least"].as_f64().unwrap()
                    > (LARGE_SURCHARGED - LARGE_BASE) / 2.0,
                "{label}: {estimate:#}"
            );
            assert_eq!(estimate["billing"][0]["terms"], "unestablished", "{label}");
        }
    }
}

// --- plan rules --------------------------------------------------------------

/// [`GPT_TABLE`] as a `tapes-price-table/3` table whose long-context term
/// names `api` billing, with `plans` rows after it.
fn plan_table(plans: &str) -> String {
    GPT_TABLE
        .replace("tapes-price-table/2", "tapes-price-table/3")
        .replace("multipliers = {", "billing = [\"api\"]\nmultipliers = {")
        + plans
}

/// Claim: a pi session whose every request names provider `opencode-go`, a
/// subscription billed at API rates, is plan `opencode-go`, whose built-in
/// rule pays `api` terms: its request above 272K pays the long-context rates
/// for the whole request, and the estimate is complete and names the plan.
/// Rejects a provider plan priced between base and surcharged rates, or a
/// surcharge waived because the plan is a subscription.
#[test]
fn a_pi_opencode_go_session_above_the_threshold_pays_the_surcharge() {
    let store = Store::new("pi-opencode-go");
    pi_session(&store, "opencode-go", "openai-completions", &[]);
    for estimate in pi_estimates_under(&store, &plan_table("")) {
        assert!(
            close(&estimate["amount"], LARGE_SURCHARGED + SMALL),
            "{estimate:#}"
        );
        assert_eq!(
            estimate["billing"],
            json!([{"terms": "api", "plan": "opencode-go",
                    "evidence": "pi requests all name provider `opencode-go`"}])
        );
        assert!(
            estimate.get("measurement_completeness").is_none(),
            "{estimate:#}"
        );
    }
}

/// Claim: a `tapes-price-table/3` table's `[[plans]]` row overrides the
/// built-in rule of its id, so `opencode-go` billed as `subscription` pays
/// base rates above the threshold, and the estimate's terms are the
/// resolved ones. Rejects a table rule ignored in favour of the built-in one.
#[test]
fn a_table_plan_rule_overrides_the_built_in_one() {
    let store = Store::new("pi-opencode-go-rule");
    pi_session(&store, "opencode-go", "openai-completions", &[]);
    let table = plan_table("\n[[plans]]\nid = \"opencode-go\"\nbilling = \"subscription\"\n");
    for estimate in pi_estimates_under(&store, &table) {
        assert!(
            close(&estimate["amount"], LARGE_BASE + SMALL),
            "{estimate:#}"
        );
        assert_eq!(
            estimate["billing"][0]["terms"], "subscription",
            "{estimate:#}"
        );
        assert_eq!(
            estimate["billing"][0]["plan"], "opencode-go",
            "{estimate:#}"
        );
    }
}

/// Claim: a pi session whose requests name two providers establishes no
/// plan, so its request above 272K is bounded billing-unestablished.
/// Rejects the first provider's plan taken for the whole session.
#[test]
fn a_pi_session_mixing_providers_establishes_no_plan() {
    let store = Store::new("pi-mixed-providers");
    pi_session_of(
        &store,
        ["opencode-go", "fixture-provider"],
        "openai-completions",
        &[],
    );
    for estimate in pi_estimates_under(&store, &plan_table("")) {
        assert!(estimate.get("amount").is_none(), "{estimate:#}");
        assert_eq!(
            estimate["billing"][0]["terms"], "unestablished",
            "{estimate:#}"
        );
        assert!(estimate["billing"][0].get("plan").is_none(), "{estimate:#}");
    }
}

/// Claim: plan rules parse only in a `tapes-price-table/3` table, each id
/// once, with `billing` `api` or `subscription` and no other key. Rejects a
/// `/2` table that quietly takes plan rules, a duplicate rule whose order
/// decides, and a misspelled billing taken for some default.
#[test]
fn a_table_refuses_a_misplaced_or_malformed_plan_rule() {
    let rule =
        |id: &str, billing: &str| format!("[[plans]]\nid = \"{id}\"\nbilling = \"{billing}\"\n");
    let refused = |text: String, expected: &str| {
        let error = format!("{:#}", PriceTable::parse(&text).unwrap_err());
        assert!(error.contains(expected), "{expected}: {error}");
    };
    refused(
        format!("{GPT_TABLE}{}", rule("opencode-go", "api")),
        "plans need schema `tapes-price-table/3`",
    );
    refused(
        plan_table(&format!(
            "{}{}",
            rule("opencode-go", "api"),
            rule("opencode-go", "subscription")
        )),
        "plans[1] names `opencode-go`, which plans[0] already names",
    );
    refused(
        plan_table(&rule("opencode-go", "metered")),
        "unknown variant `metered`",
    );
    refused(
        plan_table("[[plans]]\nid = \"opencode-go\"\nbilling = \"api\"\nrate = 1\n"),
        "unknown field `rate`",
    );
    let table = PriceTable::parse(&plan_table(&rule("fixture-plan", "subscription"))).unwrap();
    assert_eq!(
        table.plan_billing("fixture-plan"),
        Some(pricing::BillingKind::Subscription)
    );
    assert_eq!(
        table.plan_billing("opencode-go"),
        Some(pricing::BillingKind::Api)
    );
}
