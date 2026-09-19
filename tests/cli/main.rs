use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const CODEX_SESSION_ONE: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"
);
const CODEX_SESSION_TWO: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T11-00-00-10000000-0000-0000-0000-000000000002.jsonl"
);
const CODEX_SESSION_NO_MODEL: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T13-00-00-20000000-0000-0000-0000-000000000004.jsonl"
);
const CODEX_SESSION_OTHER_MODEL: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T14-00-00-30000000-0000-0000-0000-000000000005.jsonl"
);
const CODEX_SESSION_INTERACTIVE: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T15-00-00-50000000-0000-7000-8000-000000000006.jsonl"
);
const CODEX_SESSION_TERMINAL_ONLY: &str = concat!(
    "{\"timestamp\":\"2026-01-01T15:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"00000000-0000-0000-0000-000000000006\",\"session_id\":\"00000000-0000-0000-0000-000000000006\",\"cwd\":\"/fixtures/project\",\"model_provider\":\"openai\"}}\n",
    "{\"timestamp\":\"2026-01-01T15:00:01Z\",\"type\":\"turn_context\",\"payload\":{\"cwd\":\"/fixtures/project\",\"model\":\"gpt-fixture\",\"effort\":\"high\",\"turn_id\":\"turn-terminal-only\"}}\n",
    "{\"timestamp\":\"2026-01-01T15:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-terminal-only\",\"outcome\":\"error\",\"error\":{\"codex_error_info\":\"usage_limit_exceeded\",\"message\":\"synthetic quota message\"},\"duration_ms\":1250}}\n",
    "{\"timestamp\":\"2026-01-01T15:00:03Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":17,\"output_tokens\":3,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0}},\"rate_limits\":null}}\n"
);
const CODEX_SESSION_INVOCATIONS: &str = concat!(
    "{\"timestamp\":\"2026-01-01T16:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"00000000-0000-0000-0000-000000000007\",\"session_id\":\"00000000-0000-0000-0000-000000000007\",\"cwd\":\"/fixtures/project\",\"model_provider\":\"openai\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:01Z\",\"type\":\"turn_context\",\"payload\":{\"cwd\":\"/fixtures/project\",\"model\":\"gpt-fixture\",\"effort\":\"high\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec\",\"arguments\":\"{\\\"cmd\\\":\\\"git status && cargo test\\\",\\\"path\\\":\\\"/canary\\\",\\\"digest\\\":\\\"digest-a\\\",\\\"bytes\\\":4,\\\"why\\\":\\\"verify the fixture\\\"}\",\"call_id\":\"call-shell\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:03Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call-shell\",\"output\":\"{\\\"path\\\":\\\"/canary\\\",\\\"digest\\\":\\\"digest-a\\\",\\\"bytes\\\":4}\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:03.100Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec\",\"arguments\":\"{\\\"cmd\\\":\\\"echo one\\\\necho two # trailing\\\"}\",\"call_id\":\"call-shell-boundaries\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:03.200Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec\",\"arguments\":\"{\\\"cmd\\\":\\\"printf '%s' ''\\\"}\",\"call_id\":\"call-shell-empty\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:04Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"orchestrator\",\"arguments\":\"await tools.exec_command({cmd: \\\"python -m pytest\\\", why: \\\"run tests\\\"});\",\"call_id\":\"call-js\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:05Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"orchestrator\",\"arguments\":\"const cmd = \\\"git status\\\"; tools.exec_command({cmd});\",\"call_id\":\"call-dynamic\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:05.100Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"orchestrator\",\"arguments\":\"await tools.exec_command({cmd: \\\"echo actual\\\", metadata: {cmd: \\\"touch imaginary\\\"}});\",\"call_id\":\"call-js-nested\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:05.200Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"orchestrator\",\"arguments\":\"await tools.exec_command({cmd: \\\"echo \\\" + variable});\",\"call_id\":\"call-js-concat\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:05.300Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"orchestrator\",\"arguments\":\"await tools.exec_command({cmd: \\\"echo \\\\u0041\\\"});\",\"call_id\":\"call-js-unicode\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:05.400Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec\",\"arguments\":\"{\\\"argv\\\":[\\\"echo\\\",7,\\\"status\\\"]}\",\"call_id\":\"call-argv-invalid\"}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:06Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_started\",\"item\":{\"type\":\"CommandExecution\",\"id\":\"runtime-1\",\"process_id\":\"process-1\",\"command\":[\"sh\",\"-c\",\"echo ok\"],\"cwd\":\"/fixtures/project\",\"parsed_cmd\":[{\"kind\":\"word\"},{\"kind\":\"word\"},{\"kind\":\"word\"}],\"source\":\"fixture\",\"status\":\"in_progress\"}}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:06.100Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"CommandExecution\",\"id\":\"runtime-1\",\"process_id\":\"process-1\",\"command\":[\"sh\",\"-c\",\"echo ok\"],\"cwd\":\"/fixtures/project\",\"parsed_cmd\":[{\"kind\":\"word\"},{\"kind\":\"word\"},{\"kind\":\"word\"}],\"source\":\"fixture\",\"status\":\"completed\",\"stdout\":\"ok\",\"duration\":{\"wall_ms\":1}}}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:06.150Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"FileChange\",\"id\":\"file-change-1\",\"changes\":[],\"status\":\"completed\",\"stdout\":\"\",\"stderr\":\"\"}}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:06.200Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"AgentMessage\",\"id\":\"agent-message-1\",\"content\":[{\"type\":\"output_text\",\"text\":\"ordinary reply\"}]}}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:06.300Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"Reasoning\",\"id\":\"reasoning-1\",\"raw_content\":\"opaque\",\"summary_text\":\"reasoning\"}}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:06.400Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"id\":\"user-message-1\",\"content\":\"ordinary request\"}}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:06.500Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"ContextCompaction\",\"id\":\"compaction-1\"}}}\n",
    "{\"timestamp\":\"2026-01-01T16:00:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n"
);

fn tapes() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
}

fn fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("tapes-cli-live-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
        CODEX_SESSION_ONE,
    )
    .unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T11-00-00-10000000-0000-0000-0000-000000000002.jsonl"),
        CODEX_SESSION_TWO,
    )
    .unwrap();
    (root.clone(), root.join("home"))
}

/// A removed per-change worktree leaves sessions whose recorded directory no
/// longer exists. A scoped listing cannot prove their project, so it leaves
/// them out, says how many directories it left out, and names the flags that
/// reach them.
#[test]
fn scoped_listing_reports_directories_it_cannot_place() {
    let root = std::env::temp_dir().join(format!("tapes-cli-unplaced-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let initialized = Command::new("git")
        .arg("-C")
        .arg(&project)
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(initialized.success());
    let removed = root.join("project-removed-worktree");
    let sessions = root.join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    for (stamp, id, cwd) in [
        ("10-00-00", "00000000-0000-0000-0000-00000000000a", &project),
        ("11-00-00", "00000000-0000-0000-0000-00000000000b", &removed),
    ] {
        let meta = serde_json::json!({
            "timestamp": "2026-01-01T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": id, "session_id": id, "timestamp": "2026-01-01T10:00:00Z", "cwd": cwd, "model_provider": "openai"}
        });
        let message = serde_json::json!({
            "timestamp": "2026-01-01T10:00:01Z",
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}
        });
        fs::write(
            sessions.join(format!("rollout-2026-01-01T{stamp}-{id}.jsonl")),
            format!("{meta}\n{message}\n"),
        )
        .unwrap();
    }
    let list = |args: &[&str]| {
        let output = tapes()
            .args(args)
            .current_dir(&project)
            .env("CODEX_HOME", root.join("codex"))
            .env("HOME", root.join("home"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let scoped: Value =
        serde_json::from_slice(&list(&["list", "--here", "--harness", "codex", "--json"])).unwrap();
    let ids = scoped["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["00000000-0000-0000-0000-00000000000a"]);
    assert_eq!(scoped["unplaced"]["directories"], 1, "{scoped}");
    assert_eq!(
        scoped["unplaced"]["examples"][0],
        removed.display().to_string()
    );

    let human = String::from_utf8(list(&["list", "--here", "--harness", "codex"])).unwrap();
    assert!(human.contains("Not placed"), "{human}");
    assert!(human.contains(&removed.display().to_string()), "{human}");
    assert!(human.contains("--global --directory"), "{human}");

    let global: Value = serde_json::from_slice(&list(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--directory",
        "project-removed-worktree",
        "--json",
    ]))
    .unwrap();
    assert_eq!(
        global["sessions"][0]["id"],
        "00000000-0000-0000-0000-00000000000b"
    );
    assert!(global.get("unplaced").is_none(), "{global}");
    fs::remove_dir_all(root).unwrap();
}

/// The turns of a whole show that a kind predicate keeps, and the count of
/// every other turn by kind, as `projection.omitted` states them.
fn split_turns(turns: &[Value], keeps: impl Fn(&str) -> bool) -> (Vec<Value>, Value) {
    let mut kept = Vec::new();
    let mut omitted = serde_json::Map::new();
    for turn in turns {
        let kind = turn["kind"].as_str().unwrap();
        if keeps(kind) {
            kept.push(turn.clone());
        } else {
            let count = omitted.get(kind).and_then(Value::as_u64).unwrap_or(0) + 1;
            omitted.insert(kind.to_owned(), count.into());
        }
    }
    (kept, Value::Object(omitted))
}

/// What a turn is and where it sat, without the read evidence a bounded and a
/// whole read record differently.
fn turn_identities(turns: &Value) -> Vec<(u64, String, String)> {
    turns
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| {
            (
                turn["ordinal"].as_u64().unwrap(),
                turn["kind"].as_str().unwrap().to_owned(),
                turn["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// `--only` and `--omit` keep the named turn kinds with the ordinals and
/// timestamps a whole show gives them, and count every other turn by kind so
/// its absence reads as the projection. `--exchange` names one such
/// selection; a bounded read, a whole read, and `--tail` agree on them.
#[test]
fn only_and_omit_keep_turn_kinds_and_count_the_rest() {
    let (codex_home, home) = fixture_store("turn-kinds");
    let id = "00000000-0000-0000-0000-000000000001";
    let invoke = |args: &[&str]| {
        tapes()
            .args(args)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", &home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };
    let run = |args: &[&str]| {
        let output = invoke(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let json = |args: &[&str]| -> Value { serde_json::from_str(&run(args)).unwrap() };

    let whole = json(&["show", id, "--json"]);
    assert!(whole.get("projection").is_none(), "{whole}");
    let turns = whole["turns"].as_array().unwrap().clone();

    let (exchanged, exchange_omitted) =
        split_turns(&turns, |kind| matches!(kind, "operator" | "assistant"));
    assert!(exchanged.len() >= 2, "{whole}");
    assert!(exchange_omitted.get("tool").is_some(), "{whole}");
    assert!(exchange_omitted.get("reasoning").is_some(), "{whole}");
    let exchange = json(&["show", id, "--exchange", "--json"]);
    assert_eq!(exchange["turns"], Value::Array(exchanged.clone()));
    assert_eq!(
        exchange["projection"],
        serde_json::json!({"kept": ["operator", "assistant"], "omitted": exchange_omitted})
    );
    let spelled = json(&[
        "show",
        id,
        "--only",
        "assistant",
        "--only",
        "operator",
        "--json",
    ]);
    assert_eq!(spelled["turns"], exchange["turns"]);
    assert_eq!(spelled["projection"], exchange["projection"]);

    let (without, omitted) = split_turns(&turns, |kind| !matches!(kind, "tool" | "reasoning"));
    assert_eq!(
        omitted.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["reasoning", "tool"]
    );
    let projection = serde_json::json!({
        "kept": ["operator", "assistant", "control", "ambient", "notice", "unknown"],
        "omitted": omitted
    });
    let omitting = json(&["show", id, "--omit", "tool,reasoning", "--json"]);
    assert_eq!(omitting["turns"], Value::Array(without.clone()));
    assert_eq!(omitting["projection"], projection);

    let newest = json(&[
        "show",
        id,
        "--omit",
        "tool,reasoning",
        "--tail",
        "1",
        "--json",
    ]);
    let last = without.last().unwrap();
    assert_eq!(newest["turns"], serde_json::json!([last]));
    assert_eq!(
        newest["truncation"]["window"],
        serde_json::json!({
            "returned": 1,
            "omitted": without.len() - 1,
            "omitted_from": "head",
            "bound": 1,
            "ordinals": {"first": last["ordinal"], "last": last["ordinal"]}
        })
    );
    assert_eq!(newest["projection"], projection);

    let human = run(&["show", id, "--omit", "tool,reasoning"]);
    assert!(
        human.contains(
            "Note: Only operator, assistant, control, ambient, notice, unknown turns are shown; \
             turns omitted by kind: "
        ),
        "{human}"
    );
    assert!(human.contains("Drop --omit to see them."), "{human}");
    assert!(!human.contains("fixture_tool"), "{human}");

    let (operators, operator_omitted) = split_turns(&turns, |kind| kind == "operator");
    let only = json(&["show", id, "--only", "operator", "--json"]);
    assert_eq!(only["turns"], Value::Array(operators.clone()));
    assert_eq!(
        only["projection"],
        serde_json::json!({"kept": ["operator"], "omitted": operator_omitted})
    );
    let human = run(&["show", id, "--only", "operator"]);
    assert!(
        human.contains("Note: Only operator turns are shown;"),
        "{human}"
    );
    assert!(human.contains("Drop --only to see them."), "{human}");
    assert!(!human.contains("Fixture inspected."), "{human}");
    let human = run(&["show", id, "--exchange"]);
    assert!(human.contains("Drop --exchange to see them."), "{human}");

    let streamed = json(&["show", id, "--full", "--omit", "tool,reasoning", "--json"]);
    assert_eq!(
        turn_identities(&streamed["turns"]),
        turn_identities(&Value::Array(without.clone()))
    );
    assert_eq!(streamed["projection"], projection);
    let streamed = json(&[
        "show",
        id,
        "--full",
        "--omit",
        "tool,reasoning",
        "--tail",
        "1",
        "--json",
    ]);
    assert_eq!(
        turn_identities(&streamed["turns"]),
        turn_identities(&serde_json::json!([last]))
    );
    // Kept turns keep their source ordinals, so the window names those rather
    // than positions among the kept turns.
    assert_eq!(
        streamed["truncation"]["window"],
        newest["truncation"]["window"]
    );
    assert_eq!(streamed["projection"], projection);
    let human = run(&["show", id, "--full", "--only", "operator"]);
    assert!(
        human.contains("Note: Only operator turns are shown;"),
        "{human}"
    );
    assert!(human.contains("Drop --only to see them."), "{human}");
    assert!(!human.contains("Fixture inspected."), "{human}");

    for refused in [
        &["show", id, "--only", "tool", "--omit", "reasoning"][..],
        &["show", id, "--exchange", "--only", "tool"],
        &["show", id, "--exchange", "--omit", "tool"],
        &["export", id, "--only", "tool", "--omit", "reasoning"],
    ] {
        let output = invoke(refused);
        assert_eq!(output.status.code(), Some(2), "{refused:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
            "{refused:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!invoke(&["export", id, "--exchange"]).status.success());
    assert!(!invoke(&["show", id, "--only", "prompt"]).status.success());
    let _ = fs::remove_dir_all(&codex_home);
}

/// `--omit` narrows every file of a bundle: the JSON and the trace hold every
/// kept turn and count the rest, and the context file holds the exchange kinds
/// the selection keeps. A bulk export applies it to each bundle.
#[test]
fn export_omit_narrows_every_bundle_file_and_counts_the_omitted_turns() {
    let (codex_home, home) = fixture_store("export-omit");
    let id = "00000000-0000-0000-0000-000000000001";
    let run = |args: &[&str]| {
        let output = tapes()
            .args(args)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", &home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let whole: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    let (kept, omitted) = split_turns(whole["turns"].as_array().unwrap(), |kind| {
        kind != "reasoning"
    });
    assert!(omitted.get("reasoning").is_some(), "{whole}");

    let file = |directory: &Path, suffix: &str| {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                let name = path.to_string_lossy();
                name.contains(id) && name.ends_with(suffix)
            })
            .unwrap()
    };
    let directory = codex_home.join("bundle");
    run(&[
        "export",
        id,
        "--omit",
        "reasoning",
        "--bundle",
        directory.to_str().unwrap(),
    ]);
    let bundle: Value =
        serde_json::from_str(&fs::read_to_string(file(&directory, ".json")).unwrap()).unwrap();
    assert_eq!(bundle["turns"], Value::Array(kept));
    assert_eq!(
        bundle["projection"],
        serde_json::json!({
            "kept": ["operator", "assistant", "tool", "control", "ambient", "notice", "unknown"],
            "omitted": omitted
        })
    );
    let trace = fs::read_to_string(file(&directory, ".trace.md")).unwrap();
    assert!(
        trace.contains(
            "- projection: kept operator, assistant, tool, control, ambient, notice, unknown; \
             omitted 1 reasoning"
        ),
        "{trace}"
    );
    assert!(
        trace.contains("Only the kept turn kinds are traced"),
        "{trace}"
    );
    assert!(trace.contains("fixture_tool"), "{trace}");
    assert!(!trace.contains("Consider the fixture."), "{trace}");
    let context = fs::read_to_string(file(&directory, ".context.md")).unwrap();
    assert!(context.contains("Inspect the fixture."), "{context}");
    assert!(context.contains("Fixture inspected."), "{context}");
    assert!(!context.contains("fixture_tool"), "{context}");

    let tools = codex_home.join("tools");
    run(&[
        "export",
        id,
        "--only",
        "tool",
        "--bundle",
        tools.to_str().unwrap(),
    ]);
    let context = fs::read_to_string(file(&tools, ".context.md")).unwrap();
    assert!(!context.contains("\n## "), "{context}");

    let bulk = codex_home.join("bulk");
    run(&[
        "export",
        "--global",
        "--harness",
        "codex",
        "--omit",
        "reasoning",
        "--bundle",
        bulk.to_str().unwrap(),
    ]);
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(bulk.join("manifest.json")).unwrap()).unwrap();
    let sessions = manifest["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{manifest}");
    for session in sessions {
        let bundle: Value = serde_json::from_str(
            &fs::read_to_string(session["files"]["json"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(
            bundle["projection"]["kept"]
                .as_array()
                .unwrap()
                .iter()
                .all(|kind| kind != "reasoning"),
            "{bundle}"
        );
        assert!(
            bundle["turns"]
                .as_array()
                .unwrap()
                .iter()
                .all(|turn| turn["kind"] != "reasoning"),
            "{bundle}"
        );
    }
    let _ = fs::remove_dir_all(&codex_home);
}

/// A bundle's context file is the exchange `show --exchange` returns, turn for
/// turn, leaving out the context and notices the harness wrote as user turns.
#[test]
fn a_bundle_context_holds_the_turns_show_exchange_returns() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-context-exchange-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let codex_home = root.path().join("codex");
    let sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-0000000000c1";
    let timestamp = "2026-01-01T10:00:00Z";
    let item = |payload: Value| {
        serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": payload})
            .to_string()
            + "\n"
    };
    let user = |text: &str| {
        item(
            serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}),
        )
    };
    let mut body = serde_json::json!({"timestamp": timestamp, "type": "session_meta", "payload": {"id": id, "session_id": id, "timestamp": timestamp, "cwd": "/fixtures/project", "source": "cli", "model_provider": "openai"}}).to_string() + "\n";
    body += &user("Inspect the fixture.");
    body += &user("<environment_context>\n  <cwd>/fixtures/project</cwd>\n</environment_context>");
    body += &user("<turn_aborted>\nThe user interrupted the previous turn.\n</turn_aborted>");
    body += &item(
        serde_json::json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "Consider it."}]}),
    );
    body += &item(
        serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Fixture inspected."}]}),
    );
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    let run = |args: &[&str]| {
        let output = tapes()
            .args(args)
            .env("CODEX_HOME", &codex_home)
            .env("HOME", &home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let whole: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    let kinds = whole["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"ambient"), "{kinds:?}");
    assert!(kinds.contains(&"notice"), "{kinds:?}");

    let exchange: Value =
        serde_json::from_slice(&run(&["show", id, "--exchange", "--json"])).unwrap();
    let exchanged = exchange["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| format!("{} #{}", turn["role"].as_str().unwrap(), turn["ordinal"]))
        .collect::<Vec<_>>();

    let directory = root.path().join("bundle");
    run(&["export", id, "--bundle", directory.to_str().unwrap()]);
    let context = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".context.md"))
        .unwrap();
    let context = fs::read_to_string(context).unwrap();
    let headed = context
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .map(|heading| heading.split(" — ").next().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(headed, exchanged, "{context}");
}

/// An interactive Codex rollout carries its operator's messages beside the
/// instructions, skills, and notices the harness writes into the same user
/// role. The exchange keeps the operator's messages under both the bounded
/// and the whole read, and files the harness's own items by what they are.
#[test]
fn exchange_keeps_the_operator_turns_of_an_interactive_codex_rollout() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-interactive-{}", std::process::id())),
    );
    let codex_home = root.path().join("codex");
    let sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let name = "rollout-2026-01-01T15-00-00-50000000-0000-7000-8000-000000000006.jsonl";
    fs::write(sessions.join(name), CODEX_SESSION_INTERACTIVE).unwrap();
    let id = "50000000-0000-7000-8000-000000000006";
    let json = |arguments: &[&str]| -> Value {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &root.path().join("home"),
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let exchange = |turns: &Value| {
        turns
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| {
                (
                    turn["kind"].as_str().unwrap().to_owned(),
                    turn["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>()
    };
    let kept = [
        ("operator", "Summarize the fixture project."),
        ("assistant", "The fixture project holds one crate."),
        ("operator", "$fixture-skill review the crate."),
        ("operator", "Keep the review short."),
        ("assistant", "The crate is sound."),
    ]
    .map(|(kind, text)| (kind.to_owned(), text.to_owned()))
    .to_vec();

    for arguments in [
        vec!["show", id, "--exchange", "--json"],
        vec!["show", id, "--full", "--exchange", "--json"],
    ] {
        let shown = json(&arguments);
        assert_eq!(exchange(&shown["turns"]), kept, "{arguments:?}: {shown}");
        let omitted = &shown["projection"]["omitted"];
        assert!(omitted.get("unknown").is_none(), "{arguments:?}: {omitted}");
        assert_eq!(omitted["notice"], 3, "{arguments:?}: {omitted}");
    }

    let whole = json(&["show", id, "--json"]);
    let user = whole["turns"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|turn| turn["role"] == "user")
        .map(|turn| turn["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        user,
        ["ambient", "operator", "operator", "ambient", "notice", "operator", "notice", "notice"]
    );
}

/// The file tail is the reader's own bound, so a caller can set it; the read
/// evidence and the truncation report name the bound that was in force.
#[test]
fn read_bytes_sets_the_file_tail_a_transcript_read_takes() {
    let root = std::env::temp_dir().join(format!("tapes-cli-read-bytes-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-00000000dddd";
    let mut body = format!(
        "{{\"timestamp\":\"2026-01-01T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/fixtures/project\"}}}}\n"
    );
    let padding = "x".repeat(400);
    for index in 0..400 {
        body.push_str(&format!(
            "{{\"timestamp\":\"2026-01-01T10:{:02}:{:02}Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"turn {index} {padding}\"}}]}}}}\n",
            index / 60,
            index % 60
        ));
    }
    assert!(body.len() > 128 * 1024);
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        &body,
    )
    .unwrap();
    let run = |args: &[&str]| {
        tapes()
            .args(args)
            .env("HOME", root.join("home"))
            .env("CODEX_HOME", &root)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };
    let json = |args: &[&str]| -> Value {
        let output = run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let whole = json(&["show", id, "--tail", "1000", "--json"]);
    assert_eq!(whole["turns"].as_array().unwrap().len(), 400);
    assert_eq!(whole["read"]["configured_bound"], 4 * 1024 * 1024);

    let narrow = json(&[
        "show",
        id,
        "--tail",
        "1000",
        "--read-bytes",
        "64k",
        "--json",
    ]);
    assert_eq!(narrow["read"]["configured_bound"], 64 * 1024);
    assert_eq!(
        narrow["truncation"]["source"],
        serde_json::json!([{"kind": "file-tail", "bytes": 64 * 1024}])
    );
    let kept = narrow["turns"].as_array().unwrap().len();
    assert!(kept > 0 && kept < 400, "{kept}");

    let refused = run(&["show", id, "--read-bytes", "1KiB"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("read bytes must be between 64KiB and 1GiB"),
        "{stderr}"
    );

    let unparsed = run(&["show", id, "--read-bytes", "64 furlongs"]);
    assert!(!unparsed.status.success());
    let stderr = String::from_utf8_lossy(&unparsed.stderr);
    assert!(stderr.contains("unknown size unit"), "{stderr}");

    let _ = fs::remove_dir_all(&root);
}

fn terminal_only_fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-terminal-only-{name}-{}",
        std::process::id()
    ));
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T15-00000000-0000-0000-0000-000000000006.jsonl"),
        CODEX_SESSION_TERMINAL_ONLY,
    )
    .unwrap();
    (root.clone(), root.join("home"))
}

fn invocation_fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-invocations-{name}-{}",
        std::process::id()
    ));
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T16-00000000-0000-0000-0000-000000000007.jsonl"),
        CODEX_SESSION_INVOCATIONS,
    )
    .unwrap();
    (root.clone(), root.join("home"))
}

fn parser_context_fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-parser-context-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-000000000008";
    let scripts = [
        ("continuation", "echo \\\n next"),
        ("assignment", "FLAG=1 echo next"),
        ("shell-control", "if false; then echo next; fi"),
        (
            "arrow",
            "const f = () => tools.exec_command({cmd:'echo next'});",
        ),
        (
            "short-circuit",
            "false && tools.exec_command({cmd:'echo next'});",
        ),
    ];
    let mut body = String::new();
    for value in [
        serde_json::json!({
            "timestamp": "2026-01-01T16:30:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "session_id": id,
                "source": "exec",
                "cwd": "/fixtures/project"
            }
        }),
        serde_json::json!({
            "timestamp": "2026-01-01T16:30:01Z",
            "type": "turn_context",
            "payload": {
                "cwd": "/fixtures/project",
                "model": "gpt-fixture",
                "effort": "high"
            }
        }),
    ] {
        body.push_str(&value.to_string());
        body.push('\n');
    }
    for (call_id, script) in scripts {
        let value = serde_json::json!({
            "timestamp": "2026-01-01T16:30:02Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "exec",
                "arguments": script,
                "call_id": format!("call-context-{call_id}")
            }
        });
        body.push_str(&value.to_string());
        body.push('\n');
    }
    fs::write(
        sessions.join(format!("rollout-2026-01-01T16-30-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    (root.clone(), root.join("home"))
}

fn invocation_bound_fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-invocation-bound-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-000000000009";
    let long_program = "p".repeat(3_000);
    let long_subcommand = "s".repeat(3_000);
    let long_argument = "a".repeat(3_000);
    let calls = [
        (
            "bound-program",
            "exec",
            serde_json::json!({"argv":[long_program, "sub"]}).to_string(),
        ),
        (
            "bound-subcommand",
            "exec",
            serde_json::json!({"argv":["echo", long_subcommand.clone()]}).to_string(),
        ),
        (
            "bound-shell",
            "exec",
            serde_json::json!({"cmd":format!("echo {long_subcommand}")}).to_string(),
        ),
        (
            "bound-javascript",
            "orchestrator",
            format!("tools.exec_command({{cmd:'echo {long_subcommand}'}});"),
        ),
        (
            "bounded-argument",
            "exec",
            serde_json::json!({"argv":["echo","next",long_argument]}).to_string(),
        ),
    ];
    let mut body = String::new();
    for value in [
        serde_json::json!({
            "timestamp": "2026-01-01T17:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "session_id": id,
                "source": "exec",
                "cwd": "/fixtures/project"
            }
        }),
        serde_json::json!({
            "timestamp": "2026-01-01T17:00:01Z",
            "type": "turn_context",
            "payload": {
                "cwd": "/fixtures/project",
                "model": "gpt-fixture",
                "effort": "high"
            }
        }),
    ] {
        body.push_str(&value.to_string());
        body.push('\n');
    }
    for (suffix, name, arguments) in calls {
        let value = serde_json::json!({
            "timestamp": "2026-01-01T17:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": name,
                "arguments": arguments,
                "call_id": format!("call-{suffix}")
            }
        });
        body.push_str(&value.to_string());
        body.push('\n');
    }
    fs::write(
        sessions.join(format!("rollout-2026-01-01T17-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    (root.clone(), root.join("home"))
}

fn filter_fixture_store(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("tapes-cli-filter-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let files = [
        (
            "rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl",
            CODEX_SESSION_ONE,
            1_800_000_000,
        ),
        (
            "rollout-2026-01-01T11-00-00-10000000-0000-0000-0000-000000000002.jsonl",
            CODEX_SESSION_TWO,
            1_800_000_001,
        ),
        (
            "rollout-2026-01-01T13-00-00-20000000-0000-0000-0000-000000000004.jsonl",
            CODEX_SESSION_NO_MODEL,
            1_800_000_002,
        ),
        (
            "rollout-2026-01-01T14-00-00-30000000-0000-0000-0000-000000000005.jsonl",
            CODEX_SESSION_OTHER_MODEL,
            1_800_000_003,
        ),
    ];
    for (filename, contents, modified) in files {
        let path = sessions.join(filename);
        fs::write(&path, contents).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(modified))
            .unwrap();
    }
    (root.clone(), root.join("home"))
}

fn fake_status(root: &Path, body: &str, exit: i32) -> (PathBuf, PathBuf) {
    let bin = root.join("bin");
    let snapshot = root.join("snapshot.json");
    let calls = root.join("calls");
    fs::create_dir_all(&bin).unwrap();
    fs::write(&snapshot, body).unwrap();
    let program = bin.join("harness-status");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' call >> '{}'\ncat '{}'\nexit {}\n",
            calls.display(),
            snapshot.display(),
            exit
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    (bin, calls)
}

fn fake_stalled_status(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let bin = root.join("bin");
    let calls = root.join("calls");
    let parent_pid = root.join("parent.pid");
    let child_pid = root.join("child.pid");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join("harness-status");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\nsleep 5 &\nchild=$!\nprintf '%s\\n' \"$child\" > '{}'\nprintf '%s\\n' call >> '{}'\nwait \"$child\"\n",
            parent_pid.display(),
            child_pid.display(),
            calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    (bin, calls, parent_pid, child_pid)
}

fn fake_exited_stalled_status(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let bin = root.join("bin");
    let calls = root.join("calls");
    let parent_pid = root.join("parent.pid");
    let child_pid = root.join("child.pid");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join("harness-status");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\nsleep 5 &\nchild=$!\nprintf '%s\\n' \"$child\" > '{}'\nprintf '%s\\n' call >> '{}'\nexit 0\n",
            parent_pid.display(),
            child_pid.display(),
            calls.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    (bin, calls, parent_pid, child_pid)
}

fn process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let end_of_command = stat.rfind(") ")?;
    stat[end_of_command + 2..].chars().next()
}

fn assert_process_gone(pid_file: &Path) {
    let pid: u32 = fs::read_to_string(pid_file)
        .unwrap_or_else(|error| panic!("read {}: {error}", pid_file.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("parse {}: {error}", pid_file.display()));
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match process_state(pid) {
            None => return,
            Some(state) if Instant::now() >= deadline => {
                panic!(
                    "pid {pid} from {} remains in state {state}",
                    pid_file.display()
                )
            }
            Some(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn with_fixture_env(command: &mut Command, codex_home: &Path, home: &Path, bin: &Path) {
    command
        .env("CODEX_HOME", codex_home)
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
}

fn session<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == id)
        .unwrap()
}

fn fixture_command(name: &str, args: &[&str]) -> std::process::Output {
    let (codex_home, home) = fixture_store(name);
    let mut command = tapes();
    command.args(args);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    command.output().unwrap()
}

fn supplied_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/input")
        .join(name)
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(path: PathBuf) -> Self {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct OpenCodeAlias {
    path: PathBuf,
}

impl OpenCodeAlias {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OpenCodeAlias {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn titleless_opencode_program(root: &Path) -> OpenCodeAlias {
    opencode_program(root, "opencode2")
}

fn opencode_program(root: &Path, name: &str) -> OpenCodeAlias {
    let program = root.join(name);
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/opencode/opencode2");
    std::os::unix::fs::symlink(fixture, &program).unwrap();
    OpenCodeAlias { path: program }
}

fn malformed_opencode_program(root: &Path) -> OpenCodeAlias {
    let program = root.join("opencode");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/opencode/opencode-malformed-row");
    std::os::unix::fs::symlink(fixture, &program).unwrap();
    OpenCodeAlias { path: program }
}

/// Bare `tapes` is a guide request, not a usage error; every mistyped
/// invocation still fails at clap's exit code 2.
#[test]
fn bare_invocation_guides_while_misuse_still_fails() {
    let guide = tapes().output().unwrap();
    assert!(guide.status.success());
    let text = String::from_utf8_lossy(&guide.stdout);
    for command in [
        "tapes list",
        "tapes show",
        "tapes events",
        "tapes stats",
        "tapes brief",
        "tapes export",
    ] {
        assert!(text.contains(command), "guide omits `{command}`");
    }

    for arguments in [vec!["bogus"], vec!["show"], vec!["list", "--nope"]] {
        let misuse = tapes().args(&arguments).output().unwrap();
        assert_eq!(misuse.status.code(), Some(2), "{arguments:?} exited wrong");
    }
}

#[test]
fn list_help_exits_successfully() {
    let output = tapes().args(["list", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("full model identity"), "{help}");
    assert!(help.contains("id (variant)"), "{help}");
    assert!(help.contains("directory path"), "{help}");
    assert!(help.contains("last_activity_at"), "{help}");
    assert!(
        help.contains("newest first by default, or oldest"),
        "{help}"
    );
    assert!(
        help.contains("RFC 3339 timestamps with an offset"),
        "{help}"
    );
    assert!(help.contains("last 32 normalized turns"), "{help}");
    assert!(help.contains("unsearched"), "{help}");
    assert!(help.contains("--input <PATH>"), "{help}");
    assert!(help.contains("chatgpt-exporter"), "{help}");
}

#[test]
fn supplied_single_conversation_reaches_list_show_and_export() {
    let input = supplied_fixture("chatgpt-export.json");
    let input = input.to_str().unwrap();
    let listed = tapes()
        .args([
            "list",
            "--input",
            input,
            "--input-format",
            "chatgpt-exporter",
            "--source-scope",
            "fixture-account",
            "--json",
        ])
        .env("HOME", "/definitely/missing")
        .env("PATH", "/definitely/missing")
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["schema"], "tapes-list/5");
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "supplied-1");
    assert_eq!(listed["sessions"][0]["source"]["kind"], "supplied-export");
    assert_eq!(
        listed["sessions"][0]["source"]["scope"],
        serde_json::json!({"value":"fixture-account","authority":"declared"})
    );
    let occurrence = listed["sessions"][0]["occurrence"].as_str().unwrap();

    let shown = tapes()
        .args([
            "show",
            "--occurrence",
            occurrence,
            "--input",
            input,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["session"]["id"], "supplied-1");
    assert_eq!(shown["turns"][0]["kind"], "operator");
    assert_eq!(
        shown["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["question", "answer"]
    );
    assert_eq!(shown["turns"][0]["record_ref"]["span"]["start"], 4);
    assert_eq!(shown["turns"][1]["record_ref"]["pointer"], "/messages/1");

    let brief = tapes()
        .args(["brief", "supplied-1", "--input", input, "--json"])
        .output()
        .unwrap();
    assert!(brief.status.success());
    let brief: Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert_eq!(brief["ending"]["last_operator"]["ordinal"], 0);
    assert_eq!(brief["tail"][0]["kind"], "operator");

    let titled = tapes()
        .args([
            "show",
            "--title",
            "Bounded input",
            "--input",
            input,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(titled.status.success());
    let titled: Value = serde_json::from_slice(&titled.stdout).unwrap();
    assert_eq!(titled["session"]["id"], "supplied-1");

    let bundle_root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-export-{}", std::process::id())),
    );
    let exported = tapes()
        .args(["export", "supplied-1", "--input", input, "--bundle"])
        .arg(bundle_root.path())
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let entries = fs::read_dir(bundle_root.path())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert!(entries
        .iter()
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".context.md")));
    assert!(entries
        .iter()
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".json")));
    assert!(entries
        .iter()
        .any(|entry| entry.file_name().to_string_lossy().ends_with(".trace.md")));
}

/// The official export's voice and reasoning records keep their content
/// outside the `type`-tagged parts other producers write: object parts name
/// their kind `content_type`, and reasoning holds its text beside `parts`.
#[test]
fn supplied_openai_voice_and_reasoning_records_project_their_content() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-voice-{}", std::process::id())),
    );
    let input = root.path().join("conversations-000.json");
    let message = |id: &str, role: &str, content: Value| serde_json::json!({"id": id, "author": {"role": role}, "content": content});
    let node = |id: &str, parent: Option<&str>, message: Value| serde_json::json!({"id": id, "parent": parent, "message": message});
    fs::write(
        &input,
        serde_json::to_vec(&serde_json::json!([{
            "conversation_id": "voice-1",
            "title": "Voice",
            "current_node": "image",
            "mapping": {
                "root": node("root", None, Value::Null),
                "spoken": node("spoken", Some("root"), message("spoken-message", "user", serde_json::json!({
                    "content_type": "multimodal_text",
                    "parts": [
                        {"content_type": "audio_transcription", "text": "operator speech", "direction": "in", "decoding_id": null},
                        {"content_type": "real_time_user_audio_video_asset_pointer", "audio_asset_pointer": {"content_type": "audio_asset_pointer", "asset_pointer": "sediment://file_in", "size_bytes": 11, "format": "wav"}, "frames_asset_pointers": []}
                    ]
                }))),
                "thoughts": node("thoughts", Some("spoken"), message("thoughts-message", "assistant", serde_json::json!({
                    "content_type": "thoughts",
                    "thoughts": [{"summary": "Weighing", "content": "private weighing", "chunks": [], "finished": true}],
                    "source_analysis_msg_id": "analysis"
                }))),
                "recap": node("recap", Some("thoughts"), message("recap-message", "assistant", serde_json::json!({
                    "content_type": "reasoning_recap",
                    "content": "Thought for 3s"
                }))),
                "answer": node("answer", Some("recap"), message("answer-message", "assistant", serde_json::json!({
                    "content_type": "multimodal_text",
                    "parts": [
                        {"content_type": "audio_transcription", "text": "assistant speech", "direction": "out", "decoding_id": null},
                        {"content_type": "audio_asset_pointer", "asset_pointer": "sediment://file_out", "size_bytes": 13, "format": "wav"}
                    ]
                }))),
                "image": node("image", Some("answer"), message("image-message", "user", serde_json::json!({
                    "content_type": "multimodal_text",
                    "parts": [
                        {"content_type": "image_asset_pointer", "asset_pointer": "sediment://file_image", "size_bytes": 17, "width": 1, "height": 1},
                        "caption"
                    ]
                })))
            }
        }]))
        .unwrap(),
    )
    .unwrap();

    let shown = tapes()
        .args([
            "show",
            "voice-1",
            "--input",
            input.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let turns = shown["turns"].as_array().unwrap();
    let summary = turns
        .iter()
        .map(|turn| {
            (
                turn["role"].as_str().unwrap(),
                turn["kind"].as_str().unwrap(),
                turn["text"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            ("user", "operator", "operator speech"),
            ("reasoning", "reasoning", "Weighing\nprivate weighing"),
            ("reasoning", "reasoning", "Thought for 3s"),
            ("assistant", "assistant", "assistant speech"),
            ("user", "operator", "caption"),
        ]
    );
    assert_eq!(shown["content"]["unknown"], 0, "{}", shown["content"]);
    assert_eq!(shown["content"]["references"], 3, "{}", shown["content"]);
    assert_eq!(turns[0]["parts"][0]["kind"], "transcription");
    assert_eq!(turns[0]["parts"][0]["native_kind"], "audio_transcription");
    assert_eq!(
        turns[0]["parts"][1]["reference"]["uri"],
        "sediment://file_in"
    );
    assert_eq!(turns[1]["parts"][1]["native_kind"], "thoughts");
    assert_eq!(
        turns[1]["parts"][1]["source_field"],
        "message.content.thoughts[0].content"
    );
    assert_eq!(turns[3]["parts"][1]["kind"], "media-reference");
    assert_eq!(turns[4]["parts"][0]["reference"]["bytes"], 17);
}

/// ChatGPT Exporter keeps the mapping graph rather than flattening it into a
/// convenience message array. The chosen path is a transcript projection;
/// sibling messages and their edges remain source evidence.
#[test]
fn supplied_chatgpt_exporter_raw_graph_preserves_branches_and_auto_provenance() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-raw-graph-{}", std::process::id())),
    );
    let input = root.path().join("export.data");
    fs::write(
        &input,
        serde_json::to_vec(&serde_json::json!({
            "id": "raw-graph",
            "current_node": "answer-a",
            "mapping": {
                "root": {"id":"root","parent":null,"message":null},
                "question": {"id":"question","parent":"root","message":{"id":"question-message","author":{"role":"user"},"content":{"parts":["question"]}}},
                "answer-a": {"id":"answer-a","parent":"question","message":{"id":"answer-a-message","author":{"role":"assistant"},"content":{"parts":["selected answer"]}}},
                "answer-b": {"id":"answer-b","parent":"question","message":{"id":"answer-b-message","author":{"role":"assistant"},"content":{"parts":["alternate answer"]}}}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let input = input.to_str().unwrap();

    let listed = tapes()
        .args(["list", "--input", input, "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert!(listed["sessions"][0]["source"].get("producer").is_none());
    assert_eq!(listed["sessions"][0]["source"]["origin"], "openai");
    assert_eq!(
        listed["sessions"][0]["source"]["representation"],
        "chatgpt-exporter-conversation"
    );

    let explicitly_openai = tapes()
        .args([
            "list",
            "--input",
            input,
            "--input-format",
            "openai",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(explicitly_openai.status.success());
    let explicitly_openai: Value = serde_json::from_slice(&explicitly_openai.stdout).unwrap();
    assert_eq!(
        explicitly_openai["sessions"][0]["source"]["producer"],
        "OpenAI export"
    );
    assert_eq!(
        explicitly_openai["sessions"][0]["source"]["producer_authority"],
        "declared"
    );

    let shown = tapes()
        .args(["show", "raw-graph", "--input", input, "--json"])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert!(shown["turns"][0]["record_ref"]["domain"]
        .as_str()
        .unwrap()
        .starts_with("openai:"));
    assert_eq!(
        shown["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["question", "selected answer"]
    );
    assert_eq!(
        shown["graph"]["selected_path"],
        serde_json::json!(["root", "question", "answer-a"])
    );
    assert_eq!(shown["graph"]["nodes"].as_array().unwrap().len(), 4);
    assert!(shown["graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["message"]["text"] == "alternate answer"));
    assert_eq!(shown["graph"]["edges"].as_array().unwrap().len(), 3);

    let no_current = root.path().join("no-current.json");
    fs::write(
        &no_current,
        serde_json::to_vec(&serde_json::json!({
            "id": "raw-no-current",
            "mapping": {
                "root": {"id":"root","parent":null,"message":null},
                "message": {"id":"message","parent":"root","message":{"id":"message-id","author":{"role":"assistant"},"content":{"parts":["retained but unselected"]}}}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let no_current = no_current.to_str().unwrap();
    let shown = tapes()
        .args(["show", "raw-no-current", "--input", no_current, "--json"])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert!(shown["turns"].as_array().unwrap().is_empty());
    assert_eq!(shown["graph"]["nodes"].as_array().unwrap().len(), 2);
    assert!(shown["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note == "canonical branch is unknown because current_node is absent"));

    let mut mapping = serde_json::Map::new();
    mapping.insert(
        "root".to_owned(),
        serde_json::json!({"id":"root","parent":null,"message":null}),
    );
    let mut parent = "root".to_owned();
    for index in 0..140 {
        let id = format!("node-{index}");
        mapping.insert(
            id.clone(),
            serde_json::json!({
                "id": id,
                "parent": parent.clone(),
                "message": {
                    "id": format!("message-{index}"),
                    "author": {"role":"assistant"},
                    "content": {"parts": [format!("long path {index}")]}
                }
            }),
        );
        parent = id;
    }
    let long_path = root.path().join("long-path.json");
    fs::write(
        &long_path,
        serde_json::to_vec(&serde_json::json!({
            "id": "raw-long-path",
            "current_node": parent,
            "mapping": mapping
        }))
        .unwrap(),
    )
    .unwrap();
    let long_path = long_path.to_str().unwrap();
    let shown = tapes()
        .args([
            "show",
            "raw-long-path",
            "--input",
            long_path,
            "--tail",
            "200",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["turns"].as_array().unwrap().len(), 140);
    assert_eq!(
        shown["graph"]["selected_path"].as_array().unwrap().len(),
        141
    );
}

/// A collection cursor describes the ordered supplied-input observation, not
/// only the file that produced the row carrying it. A changed source refuses
/// the cursor instead of returning an empty successful page.
#[test]
fn supplied_occurrence_continuation_crosses_files_and_rejects_changes() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-continuation-{}", std::process::id())),
    );
    let first = root.path().join("a.json");
    let second = root.path().join("b.json");
    fs::write(
        &first,
        serde_json::to_vec(&serde_json::json!([{"id":"first","mapping":{"root":{"parent":null,"message":null},"first":{"parent":"root","message":{"author":{"role":"assistant"},"content":{"parts":["first"]}}}},"current_node":"first"}]))
            .unwrap(),
    )
    .unwrap();
    fs::write(
        &second,
        serde_json::to_vec(&serde_json::json!([{"id":"second","mapping":{"root":{"parent":null,"message":null},"second":{"parent":"root","message":{"author":{"role":"assistant"},"content":{"parts":["second"]}}}},"current_node":"second"}]))
            .unwrap(),
    )
    .unwrap();
    let first = first.to_str().unwrap();
    let second = second.to_str().unwrap();

    let listed = tapes()
        .args([
            "list", "--input", first, "--input", second, "--limit", "1", "--json",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let cursor = listed["sessions"][0]["occurrence"].as_str().unwrap();
    let continued = tapes()
        .args([
            "list",
            "--input",
            first,
            "--input",
            second,
            "--after-occurrence",
            cursor,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(continued.status.success());
    let continued: Value = serde_json::from_slice(&continued.stdout).unwrap();
    assert_eq!(continued["sessions"][0]["id"], "second");

    fs::write(second, b"[]").unwrap();
    let changed = tapes()
        .args([
            "list",
            "--input",
            first,
            "--input",
            second,
            "--after-occurrence",
            cursor,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!changed.status.success());
    assert!(
        String::from_utf8_lossy(&changed.stderr).contains("different supplied input observation")
    );
}

#[test]
fn supplied_duplicate_ids_require_an_occurrence_and_never_use_installed_stores() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-duplicate-{}", std::process::id())),
    );
    let input = root.path().join("renamed.data");
    fs::write(
        &input,
        serde_json::to_vec(&serde_json::json!([
            {"id":"duplicate","title":"one","messages":[{"role":"user","content":"first"}]},
            {"id":"duplicate","title":"two","messages":[{"role":"user","content":"second"}]}
        ]))
        .unwrap(),
    )
    .unwrap();
    let input = input.to_str().unwrap();

    let ambiguous = tapes()
        .args(["show", "duplicate", "--input", input, "--json"])
        .env("HOME", "/definitely/missing")
        .env("PATH", "/definitely/missing")
        .output()
        .unwrap();
    assert!(!ambiguous.status.success());
    let error = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(error.contains("occurs 2 times"), "{error}");
    assert!(error.contains("input:v2:"), "{error}");

    let listed = tapes()
        .args(["list", "--input", input, "--json"])
        .output()
        .unwrap();
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let occurrences = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["occurrence"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(occurrences.len(), 2);

    let selected = tapes()
        .args(["show", "--occurrence"])
        .arg(&occurrences[1])
        .args(["--input", input, "--json"])
        .output()
        .unwrap();
    assert!(selected.status.success());
    let selected: Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(selected["turns"][0]["text"], "second");
}

#[test]
fn supplied_zip_reads_conversations_and_retains_associated_report_evidence() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-zip-{}", std::process::id())),
    );
    let archive_path = root.path().join("renamed-container.data");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    archive
        .start_file("renamed-conversation.json", options)
        .unwrap();
    archive
        .write_all(
            br#"{"id":"associated-1","current_node":"node","mapping":{"root":{"id":"root","parent":null,"message":null},"node":{"id":"node","parent":"root","message":{"id":"message-1","author":{"role":"user"},"content":{"content_type":"text","parts":["hello"]}}}}}"#,
        )
        .unwrap();
    archive
        .start_file("empty-conversation.json", options)
        .unwrap();
    archive
        .write_all(
            br#"{"id":"empty-outer","mapping":{"root":{"id":"root","parent":null,"message":null}}}"#,
        )
        .unwrap();
    archive.start_file("file-report.dat", options).unwrap();
    archive
        .write_all(
            br#"{"backing_conversation_id":"associated-1","widget_session_id":"report-1","widget_state":{"status":"completed","report_message":{"id":"report-message","author":{"role":"assistant"},"content":{"parts":[{"type":"text","text":"private report body"}],"content_references":[{"type":"attribution","url":"https://example.test/source","start_idx":0,"end_idx":18}]}}}}"#,
        )
        .unwrap();
    archive.start_file("grouped-report.dat", options).unwrap();
    archive
        .write_all(
            br#"{"backing_conversation_id":"associated-1","widget_session_id":"grouped-report","widget_state":{"status":"completed","report_message":{"content":{"parts":["grouped body"],"content_references":{"type":"grouped_webpages","start_idx":0,"end_idx":6,"items":[{"url":"https://example.invalid/grouped-target","title":"Grouped source"}]}}}}}"#,
        )
        .unwrap();
    let sources = (0..65)
        .map(|index| {
            serde_json::json!({
                "url": format!("https://example.invalid/bounded-{index}"),
                "title": format!("Bounded source {index}")
            })
        })
        .collect::<Vec<_>>();
    archive
        .start_file("bounded-grouped-report.dat", options)
        .unwrap();
    archive
        .write_all(
            &serde_json::to_vec(&serde_json::json!({
                "backing_conversation_id":"associated-1",
                "widget_session_id":"bounded-grouped-report",
                "widget_state": {
                    "report_message": {
                        "content": {
                            "content_references": {
                                "type":"grouped_webpages",
                                "items": sources
                            }
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
    let mut object_sources = serde_json::Map::new();
    object_sources.insert(
        "holding".to_owned(),
        serde_json::Value::Array(
            (0..64)
                .map(|index| {
                    serde_json::json!({
                        "url": format!("https://example.invalid/object-{index}"),
                        "title": format!("Object source {index}")
                    })
                })
                .collect(),
        ),
    );
    object_sources.insert(
        "z".to_owned(),
        serde_json::json!([{
            "url": "https://example.invalid/object-last",
            "title": "Object last"
        }]),
    );
    archive
        .start_file("object-grouped-report.dat", options)
        .unwrap();
    archive
        .write_all(
            &serde_json::to_vec(&serde_json::json!({
                "backing_conversation_id":"associated-1",
                "widget_session_id":"object-grouped-report",
                "widget_state": {
                    "report_message": {
                        "content": {
                            "content_references": {
                                "type":"grouped_webpages",
                                "items": object_sources
                            }
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
    let mut object_citations = serde_json::Map::new();
    object_citations.insert(
        "holding".to_owned(),
        serde_json::Value::Array(
            (0..64)
                .map(|index| {
                    serde_json::json!({
                        "url": format!("https://example.invalid/citation-{index}"),
                        "title": format!("Citation source {index}")
                    })
                })
                .collect(),
        ),
    );
    object_citations.insert(
        "z".to_owned(),
        serde_json::json!([{
            "url": "https://example.invalid/citation-last",
            "title": "Citation last"
        }]),
    );
    archive
        .start_file("object-citation-report.dat", options)
        .unwrap();
    archive
        .write_all(
            &serde_json::to_vec(&serde_json::json!({
                "backing_conversation_id":"associated-1",
                "widget_session_id":"object-citation-report",
                "widget_state": {
                    "report_message": {
                        "content": {
                            "content_references": object_citations
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
    archive.start_file("empty-report.dat", options).unwrap();
    archive
        .write_all(
            br#"{"backing_conversation_id":"empty-outer","widget_session_id":"empty-report","widget_state":{"status":"completed","report_message":{"content":{"parts":["body without a turn"]}}}}"#,
        )
        .unwrap();
    archive.start_file("orphan-report.dat", options).unwrap();
    archive
        .write_all(
            br#"{"backing_conversation_id":"missing-outer","widget_session_id":"orphan-report","widget_state":{"status":"completed","report_message":{"author":{"role":"assistant"},"content":{"parts":["orphan body"]}}}}"#,
        )
        .unwrap();
    archive.start_file("unrelated.xlsx", options).unwrap();
    archive
        .write_all(b"not-json-and-not-a-conversation")
        .unwrap();
    archive.finish().unwrap();

    let archive = archive_path.to_str().unwrap();
    let shown = tapes()
        .args(["show", "associated-1", "--input", archive, "--json"])
        .output()
        .unwrap();
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let output = String::from_utf8_lossy(&shown.stdout);
    assert!(output.contains("private report body"), "{output}");
    assert!(output.contains("https://example.test/source"), "{output}");
    let output: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(output["turns"].as_array().unwrap().len(), 1);
    let parts = output["turns"][0]["parts"].as_array().unwrap();
    let report = parts
        .iter()
        .find(|part| part["native_kind"] == "openai-library-report")
        .unwrap();
    assert_eq!(report["reference"]["identity"], "report-1");
    assert_eq!(report["reference"]["backing"], "associated-1");
    assert_eq!(report["reference"]["citation_count"], 1);
    assert_eq!(report["reference"]["body"]["text"], "private report body");
    assert_eq!(report["reference"]["body_availability"], "retained-body");
    assert_eq!(
        report["reference"]["citations"][0]["uri"]["text"],
        "https://example.test/source"
    );
    assert_eq!(output["content"]["references"], 5);

    let grouped = parts
        .iter()
        .find(|part| part["reference"]["identity"] == "grouped-report")
        .unwrap();
    assert_eq!(
        grouped["reference"]["citations"][0]["sources"][0]["uri"]["text"],
        "https://example.invalid/grouped-target"
    );
    assert_eq!(
        grouped["reference"]["citations"][0]["sources"][0]["title"]["text"],
        "Grouped source"
    );
    let bounded = parts
        .iter()
        .find(|part| part["reference"]["identity"] == "bounded-grouped-report")
        .unwrap();
    assert_eq!(bounded["reference"]["citations"][0]["omitted_sources"], 1);
    let object_grouped = parts
        .iter()
        .find(|part| part["reference"]["identity"] == "object-grouped-report")
        .unwrap();
    assert_eq!(
        object_grouped["reference"]["citations"][0]["sources"]
            .as_array()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(
        object_grouped["reference"]["citations"][0]["omitted_sources"],
        1
    );
    let object_citation = parts
        .iter()
        .find(|part| part["reference"]["identity"] == "object-citation-report")
        .unwrap();
    assert_eq!(object_citation["reference"]["citation_count"], 64);
    assert_eq!(object_citation["reference"]["omitted_citations"], 1);

    let bundle_root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-report-export-{}", std::process::id())),
    );
    let exported = tapes()
        .args(["export", "associated-1", "--input", archive, "--bundle"])
        .arg(bundle_root.path())
        .output()
        .unwrap();
    assert!(exported.status.success());
    let json_path = fs::read_dir(bundle_root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let bundle_json: Value = serde_json::from_slice(&fs::read(json_path).unwrap()).unwrap();
    assert_eq!(
        bundle_json["artifacts"][0]["body"]["text"],
        "private report body"
    );
    assert_eq!(
        bundle_json["artifacts"][0]["citations"][0]["uri"]["text"],
        "https://example.test/source"
    );
    let grouped_bundle = bundle_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["identity"] == "grouped-report")
        .unwrap();
    assert_eq!(
        grouped_bundle["citations"][0]["sources"][0]["uri"]["text"],
        "https://example.invalid/grouped-target"
    );
    let object_grouped_bundle = bundle_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["identity"] == "object-grouped-report")
        .unwrap();
    assert_eq!(object_grouped_bundle["citations"][0]["omitted_sources"], 1);
    let object_citation_bundle = bundle_json["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["identity"] == "object-citation-report")
        .unwrap();
    assert_eq!(object_citation_bundle["omitted_citations"], 1);

    let listed = tapes()
        .args(["list", "--input", archive, "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["artifacts"].as_array().unwrap().len(), 7);
    assert!(listed["unsearched"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value.as_str().unwrap().contains("orphan-report.dat")));

    let brief = tapes()
        .args(["brief", "associated-1", "--input", archive, "--json"])
        .output()
        .unwrap();
    assert!(brief.status.success());
    let brief: Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert_eq!(brief["schema"], "tapes-brief/6");
    let grouped_brief_part = brief["tail"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|entry| entry["parts"].as_array().into_iter().flatten())
        .find(|part| part["reference"]["identity"] == "grouped-report")
        .unwrap();
    assert_eq!(
        grouped_brief_part["reference"]["citations"][0]["sources"][0]["uri"]["text"],
        "https://example.invalid/grouped-target"
    );

    let endings = tapes()
        .args(["endings", "--input", archive, "--text", "--json"])
        .output()
        .unwrap();
    assert!(endings.status.success());
    let endings: Value = serde_json::from_slice(&endings.stdout).unwrap();
    assert_eq!(endings["schema"], "tapes-endings/6");
    let grouped_ending_part = endings["endings"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|ending| ending["tail"].as_array().into_iter().flatten())
        .flat_map(|entry| entry["parts"].as_array().into_iter().flatten())
        .find(|part| part["reference"]["identity"] == "grouped-report")
        .unwrap();
    assert_eq!(
        grouped_ending_part["reference"]["citations"][0]["sources"][0]["uri"]["text"],
        "https://example.invalid/grouped-target"
    );

    let empty = tapes()
        .args(["show", "empty-outer", "--input", archive, "--json"])
        .output()
        .unwrap();
    assert!(empty.status.success());
    let empty: Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert!(empty["turns"].as_array().unwrap().is_empty());
    assert_eq!(empty["artifacts"][0]["body"]["text"], "body without a turn");
}

/// Citation descriptors are bounded independently of the enclosing record and
/// preserve their original lengths when UTF-8 strings are shortened.
#[test]
fn supplied_citation_descriptors_are_bounded_in_show_and_export() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-input-descriptor-bound-{}",
        std::process::id()
    )));
    let conversation = root.path().join("conversation.json");
    fs::write(
        &conversation,
        serde_json::to_vec(&serde_json::json!({
            "id":"bounded-descriptor",
            "current_node":"node",
            "mapping": {
                "root":{"parent":null,"message":null},
                "node":{"parent":"root","message":{"author":{"role":"assistant"},"content":{"parts":["answer"]}}}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let archive_path = root.path().join("descriptor.zip");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    archive.start_file("conversation.json", options).unwrap();
    archive
        .write_all(&fs::read(&conversation).unwrap())
        .unwrap();
    let long_uri = format!("https://example.invalid/{}", "u".repeat(5_000));
    let long_title = "🧪".repeat(5_000);
    archive
        .start_file("descriptor-report.dat", options)
        .unwrap();
    archive
        .write_all(
            &serde_json::to_vec(&serde_json::json!({
                "backing_conversation_id":"bounded-descriptor",
                "widget_session_id":"descriptor-report",
                "widget_state": {
                    "report_message": {
                        "content": {
                            "content_references": [{
                                "type":"source",
                                "url":long_uri,
                                "title":long_title
                            }]
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
    archive.finish().unwrap();
    let archive = archive_path.to_str().unwrap();

    let shown = tapes()
        .args(["show", "bounded-descriptor", "--input", archive, "--json"])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["schema"], "tapes-session/10");
    let citation = &shown["artifacts"][0]["citations"][0];
    assert_eq!(citation["uri"]["chars"], 5_024);
    assert_eq!(
        citation["uri"]["text"].as_str().unwrap().chars().count(),
        4_096
    );
    assert_eq!(citation["uri"]["truncated"], true);
    assert_eq!(citation["title"]["chars"], 5_000);
    assert_eq!(
        citation["title"]["text"].as_str().unwrap().chars().count(),
        1_024
    );
    assert_eq!(citation["title"]["truncated"], true);
    assert_eq!(shown["artifacts"][0]["descriptor_truncated"], true);

    let bundle_root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-input-descriptor-export-{}",
        std::process::id()
    )));
    let exported = tapes()
        .args([
            "export",
            "bounded-descriptor",
            "--input",
            archive,
            "--bundle",
        ])
        .arg(bundle_root.path())
        .output()
        .unwrap();
    assert!(exported.status.success());
    let json_path = fs::read_dir(bundle_root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let bundle_json: Value = serde_json::from_slice(&fs::read(json_path).unwrap()).unwrap();
    assert_eq!(bundle_json["schema"], "tapes-session/10");
    assert_eq!(
        bundle_json["artifacts"][0]["citations"][0]["title"]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1_024
    );
    assert_eq!(bundle_json["artifacts"][0]["descriptor_truncated"], true);
}

#[test]
fn supplied_input_routes_every_nonhistorical_view_and_keeps_history_explicit() {
    let input = supplied_fixture("chatgpt-export.json");
    let input = input.to_str().unwrap();
    for (command, args) in [
        ("events", vec!["supplied-1"]),
        ("lineage", vec!["supplied-1"]),
        ("stats", vec!["supplied-1"]),
        ("usage", vec!["supplied-1"]),
        ("brief", vec!["supplied-1"]),
    ] {
        let output = tapes()
            .arg(command)
            .args(args)
            .args(["--input", input, "--json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _: Value = serde_json::from_slice(&output.stdout).unwrap();
    }

    let endings = tapes()
        .args(["endings", "--input", input, "--json"])
        .output()
        .unwrap();
    assert!(endings.status.success());
    let endings: Value = serde_json::from_slice(&endings.stdout).unwrap();
    assert_eq!(endings["endings"].as_array().unwrap().len(), 1);

    let bulk_root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-bulk-{}", std::process::id())),
    );
    let bulk = tapes()
        .args(["export", "--input", input, "--bundle"])
        .arg(bulk_root.path())
        .output()
        .unwrap();
    assert!(
        bulk.status.success(),
        "{}",
        String::from_utf8_lossy(&bulk.stderr)
    );
    assert!(bulk_root.path().join("manifest.json").is_file());
    let manifest: Value =
        serde_json::from_slice(&fs::read(bulk_root.path().join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["sessions"].as_array().unwrap().len(), 1);

    let latest = tapes()
        .args(["show", "--latest", "--input", input, "--json"])
        .output()
        .unwrap();
    assert!(!latest.status.success());
    assert!(String::from_utf8_lossy(&latest.stderr).contains("--latest"));

    let mixed_scope = tapes()
        .args(["list", "--input", input, "--global", "--json"])
        .output()
        .unwrap();
    assert!(!mixed_scope.status.success());
    assert!(String::from_utf8_lossy(&mixed_scope.stderr).contains("conflicts"));

    let page = tapes()
        .args(["page", "supplied-1", "--input", input, "--json"])
        .output()
        .unwrap();
    assert!(!page.status.success());
    assert!(String::from_utf8_lossy(&page.stderr).contains("not supported by page"));

    let capped = tapes()
        .args([
            "show",
            "supplied-1",
            "--input",
            input,
            "--output-bytes",
            "1024",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!capped.status.success());
    assert!(String::from_utf8_lossy(&capped.stderr).contains("output-bytes"));
}

#[test]
fn supplied_jsonl_keeps_structural_gaps_and_reaches_a_later_record() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-input-budget-{}", std::process::id())),
    );
    let input = root.path().join("renamed-records.data");
    let oversized = "x".repeat(5_000);
    let first = serde_json::json!({
        "id": "oversized",
        "messages": [{"role":"user","content": oversized}]
    });
    let second = serde_json::json!({
        "id": "reachable",
        "messages": [{"role":"assistant","content":"after the gap"}]
    });
    fs::write(
        &input,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        ),
    )
    .unwrap();
    let input = input.to_str().unwrap();
    let listed = tapes()
        .args(["list", "--input", input, "--record-bytes", "1024", "--json"])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["scanned"], 2);
    assert!(!listed["scan_truncated"].as_bool().unwrap());
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], "reachable");
    assert!(listed["unsearched"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| diagnostic.as_str().unwrap().contains("oversized")));
    let occurrence = listed["sessions"][0]["occurrence"].as_str().unwrap();

    let shown = tapes()
        .args(["show", "--occurrence"])
        .arg(occurrence)
        .args(["--input", input, "--record-bytes", "1024", "--json"])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["turns"][0]["text"], "after the gap");
    assert_eq!(shown["read"]["gaps"].as_array().unwrap().len(), 1);
}

#[test]
fn perplexity_envelope_preserves_entry_fields_and_separate_response_evidence() {
    let input = supplied_fixture("perplexity-export.json");
    let input = input.to_str().unwrap();
    let listed = tapes()
        .args(["list", "--input", input, "--input-format", "auto", "--json"])
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(listed["scanned"], 2);
    assert_eq!(listed["sessions"][0]["source"]["origin"], "perplexity");
    assert_eq!(listed["sessions"][0]["metadata"]["mode"], "research");
    assert!(listed["sessions"][0]["metadata"].get("status").is_none());

    let shown = tapes()
        .args(["show", "perplexity-1", "--input", input, "--json"])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let turns = shown["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 6);
    assert_eq!(turns[0]["text"], "Draw a diagram");
    assert!(turns[1]["text"].as_str().unwrap().contains("graph TD"));
    assert_eq!(turns[0]["ts"], "2026-09-12T10:01:00Z");
    assert!(turns[1].get("ts").is_none());
    assert_eq!(turns[2]["text"], "");
    assert_eq!(turns[2]["parts"][0]["kind"], "text");
    assert_eq!(turns[3]["parts"][0]["native_kind"], "null");
    assert_eq!(
        turns[3]["coverage"]["omitted_reason"],
        "source field was null"
    );
    assert_eq!(turns[5]["parts"][0]["native_kind"], "non-string");
    assert!(shown["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str().unwrap().contains("valid timestamp")));
    assert_eq!(turns[0]["record_ref"]["pointer"], "/entries/0/query");
    assert_eq!(turns[1]["record_ref"]["pointer"], "/entries/0/answer");
    assert_eq!(turns[0]["metadata"]["engine"], "research");
    assert_eq!(turns[0]["metadata"]["status"], "COMPLETED");
    assert_eq!(turns[0]["metadata"]["label"], "first");
    assert_eq!(
        turns[0]["metadata"]["source_fields"]["status"],
        "/entries/0/query_status"
    );
    assert_eq!(turns[2]["metadata"], turns[3]["metadata"]);

    let usage = tapes()
        .args(["usage", "perplexity-1", "--input", input, "--json"])
        .output()
        .unwrap();
    let usage: Value = serde_json::from_slice(&usage.stdout).unwrap();
    assert_eq!(usage["session"]["metadata"]["collection"], "collection-1");

    let brief = tapes()
        .args(["brief", "perplexity-1", "--input", input, "--json"])
        .output()
        .unwrap();
    let brief: Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert!(brief["session"]["metadata"].get("engine").is_none());
    assert_eq!(brief["tail"][0]["metadata"]["engine"], "research");
}

#[test]
fn perplexity_zip_ignores_an_unrelated_workbook_and_declared_wrong_format_does_not_fallback() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-perplexity-zip-{}", std::process::id())),
    );
    let archive_path = root.path().join("renamed-export.bin");
    let file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    archive
        .start_file("conversation-part.json", options)
        .unwrap();
    archive
        .write_all(
            fs::read(supplied_fixture("perplexity-export.json"))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
    archive.start_file("profile.xlsx", options).unwrap();
    archive.write_all(b"profile workbook bytes").unwrap();
    archive.finish().unwrap();

    let archive = archive_path.to_str().unwrap();
    let listed = tapes()
        .args([
            "list",
            "--input",
            archive,
            "--input-format",
            "perplexity",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(listed["unsearched"].as_array().unwrap().len(), 0);

    let wrong = tapes()
        .args([
            "list",
            "--input",
            archive,
            "--input-format",
            "openai",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("openai"));
}

#[test]
fn show_help_exits_successfully() {
    assert!(tapes().args(["show", "--help"]).status().unwrap().success());
}

#[test]
fn events_help_explains_pairing_filters_and_the_default_bound() {
    let output = tapes().args(["events", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(
        help.contains("Pairing is exact within the bounded read"),
        "{help}"
    );
    assert!(
        help.contains("every turn the bounded reader reaches"),
        "{help}"
    );
    assert!(help.contains("--name <NAME>"), "{help}");
    assert!(help.contains("--call-id <ID>"), "{help}");
    assert!(help.contains("--program <PROGRAM>"), "{help}");
    assert!(help.contains("tapes-events/6"), "{help}");
}

#[test]
fn export_help_explains_the_selection_form_and_its_manifest() {
    let output = tapes().args(["export", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("manifest.json"), "{help}");
    assert!(help.contains("`failed`"), "{help}");
    assert!(help.contains("the same order"), "{help}");
    assert!(help.contains("--since <TIMESTAMP>"), "{help}");
    assert!(help.contains("--sort <ORDER>"), "{help}");
    assert!(help.contains("--search <SUBSTRING>"), "{help}");
}

/// Selecting a session stays explicit: an id or `--latest`, never a silent
/// default, and never two scopes at once.
#[test]
fn selection_and_scope_flags_are_mutually_exclusive() {
    for arguments in [
        vec!["show", "some-id", "--latest"],
        vec!["show", "some-id", "--project", "/tmp"],
        vec!["show", "some-id", "--exclude", "other-id"],
        vec!["show", "some-id", "--harness", "codex"],
        vec!["export", "some-id", "--global"],
        vec!["events", "some-id", "--latest"],
        vec!["show", "--exclude", "some-id"],
        vec!["show", "--latest", "--here", "--global"],
        vec!["export", "--latest", "--project", "/tmp", "--global"],
        vec!["export", "some-id", "--since", "2026-01-01"],
        vec!["export", "some-id", "--sort", "oldest"],
        vec!["export", "--latest", "--search", "parser"],
        vec!["export"],
        vec!["export", "--bundle", "/tmp"],
        vec!["list", "--here", "--global"],
    ] {
        let misuse = tapes().args(&arguments).output().unwrap();
        assert_eq!(misuse.status.code(), Some(2), "{arguments:?} was accepted");
    }
}

#[test]
fn events_json_answers_call_counts_and_incomplete_calls_without_raw_text() {
    let id = "00000000-0000-0000-0000-000000000001";
    let output = fixture_command("events-json", &["events", id, "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-events/6");
    assert_eq!(value["session"]["id"], id);
    assert!(value.get("truncation").is_none(), "{value}");
    assert_eq!(
        value["pairs"],
        serde_json::json!({"complete": 1, "incomplete": 1})
    );
    let events = value["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|event| event.get("text").is_none()));
    assert_eq!(
        events
            .iter()
            .filter(|event| { event["kind"] == "tool-call" && event["name"] == "fixture_tool" })
            .count(),
        1
    );
    let incomplete = events
        .iter()
        .find(|event| event["incomplete"] == "no-result-in-read")
        .unwrap();
    assert_eq!(incomplete["call_id"], "call-pending");
    assert!(incomplete.get("pair").is_none(), "{incomplete}");
    assert!(incomplete.get("duration_ms").is_none(), "{incomplete}");
    assert_eq!(events[0]["duration_ms"], 1_000);
    assert!(events[1].get("duration_ms").is_none(), "{}", events[1]);
}

#[test]
fn terminal_only_recording_exposes_a_structured_stop_observation() {
    let (codex_home, home) = terminal_only_fixture_store("observation");
    let mut command = tapes();
    command.args(["show", "00000000-0000-0000-0000-000000000006", "--json"]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 0);
    assert_eq!(value["terminal"]["record_type"], "event_msg");
    assert_eq!(value["terminal"]["payload_type"], "task_complete");
    assert_eq!(value["terminal"]["turn_id"], "turn-terminal-only");
    assert_eq!(value["terminal"]["outcome"], "error");
    assert_eq!(value["terminal"]["code"], "usage_limit_exceeded");
    assert_eq!(
        value["terminal"]["message"],
        serde_json::json!({
            "text": "synthetic quota message",
            "chars": 23
        })
    );
    assert_eq!(value["terminal"]["duration_ms"], 1250);
    assert_eq!(
        value["session"]["tokens"],
        serde_json::json!({
            "input": 17,
            "output": 3,
            "cache_read": 0,
            "cache_write": 0
        })
    );
    assert_eq!(
        value["session"]["accounting"],
        serde_json::json!({"basis":"recorded-total","coverage":"session"})
    );
    assert_eq!(value["trailing_record"]["kind"], "event_msg");
    assert_eq!(
        value["text_tail"]["empty_reason"],
        "empty-complete-projection"
    );

    let mut zero_tail = tapes();
    zero_tail.args([
        "show",
        "00000000-0000-0000-0000-000000000006",
        "--tail",
        "0",
        "--json",
    ]);
    with_fixture_env(
        &mut zero_tail,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let zero_tail = zero_tail.output().unwrap();
    assert!(zero_tail.status.success());
    let zero_tail: Value = serde_json::from_slice(&zero_tail.stdout).unwrap();
    assert_eq!(
        zero_tail["text_tail"]["empty_reason"],
        "zero-requested-tail"
    );
    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn events_filters_after_pairing_and_tail_uses_show_ordinals() {
    let id = "00000000-0000-0000-0000-000000000001";
    let named = fixture_command(
        "events-name",
        &["events", id, "--name", "fixture_tool", "--json"],
    );
    let named: Value = serde_json::from_slice(&named.stdout).unwrap();
    assert_eq!(named["events"].as_array().unwrap().len(), 1);
    assert_eq!(named["events"][0]["kind"], "tool-call");
    assert_eq!(
        named["pairs"],
        serde_json::json!({"complete": 1, "incomplete": 0})
    );

    let wrong_case = fixture_command(
        "events-name-case",
        &["events", id, "--name", "Fixture_Tool", "--json"],
    );
    let wrong_case: Value = serde_json::from_slice(&wrong_case.stdout).unwrap();
    assert!(wrong_case["events"].as_array().unwrap().is_empty());

    let pending = fixture_command(
        "events-call-id",
        &["events", id, "--call-id", "call-pending", "--json"],
    );
    let pending: Value = serde_json::from_slice(&pending.stdout).unwrap();
    assert_eq!(pending["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        pending["pairs"],
        serde_json::json!({"complete": 0, "incomplete": 1})
    );

    let shown = fixture_command("events-show-tail", &["show", id, "--tail", "1", "--json"]);
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let events = fixture_command("events-tail", &["events", id, "--tail", "1", "--json"]);
    let events: Value = serde_json::from_slice(&events.stdout).unwrap();
    assert_eq!(
        events["truncation"]["window"],
        shown["truncation"]["window"]
    );
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    assert_eq!(events["events"][0]["ordinal"], shown["turns"][0]["ordinal"]);

    let human = fixture_command("events-human", &["events", id, "--tail", "1"]);
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(
        human.contains("[tool-call #5 2026-01-01T10:00:06Z]"),
        "{human}"
    );
    assert!(
        human.contains("fixture_pending call-pending completed no-result-in-read"),
        "{human}"
    );
    assert!(human.contains("Showing the last 1 of 6 turns"), "{human}");
}

#[test]
fn events_expose_nested_declarations_and_qualified_artifact_consumption() {
    let (codex_home, home) = invocation_fixture_store("projection");
    let id = "00000000-0000-0000-0000-000000000007";
    let mut command = tapes();
    command.args(["events", id, "--json"]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema"], "tapes-events/6");
    let events = value["events"].as_array().unwrap();
    let shell_call = events
        .iter()
        .find(|event| event["call_id"] == "call-shell" && event["kind"] == "tool-call")
        .unwrap();
    let invocations = shell_call["invocations"].as_array().unwrap();
    assert_eq!(
        invocations
            .iter()
            .map(|invocation| invocation["program"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["git", "cargo"]
    );
    assert_eq!(invocations[0]["coverage"], "conditional-declaration");
    assert_eq!(invocations[0]["intent"]["text"], "verify the fixture");
    assert!(invocations[0].get("witnessed_result").is_none());
    assert!(invocations[0].get("duration_ms").is_none());
    assert_eq!(shell_call["duration_ms"], 1_000);
    assert_eq!(shell_call["artifact_references"][0]["path"], "/canary");
    assert_eq!(
        shell_call["artifact_consumptions"][0]["status"],
        "matching-consumption-observed"
    );
    assert!(shell_call["artifact_consumptions"][0]["consumer"].is_object());

    let shell_boundaries = events
        .iter()
        .find(|event| event["call_id"] == "call-shell-boundaries")
        .unwrap();
    assert_eq!(shell_boundaries["invocations"].as_array().unwrap().len(), 2);
    assert_eq!(shell_boundaries["invocations"][0]["subcommand"], "one");
    assert_eq!(shell_boundaries["invocations"][1]["subcommand"], "two");
    assert!(shell_boundaries["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|invocation| {
            invocation
                .get("arguments")
                .is_none_or(|arguments| arguments.as_array().is_some_and(Vec::is_empty))
        }));

    let shell_empty = events
        .iter()
        .find(|event| event["call_id"] == "call-shell-empty")
        .unwrap();
    assert_eq!(shell_empty["invocations"][0]["program"], "printf");
    assert_eq!(shell_empty["invocations"][0]["subcommand"], "%s");
    assert_eq!(shell_empty["invocations"][0]["arguments"][0]["text"], "");

    let js_call = events
        .iter()
        .find(|event| event["call_id"] == "call-js" && event["kind"] == "tool-call")
        .unwrap();
    assert_eq!(js_call["invocations"][0]["program"], "python");
    assert!(js_call["invocations"][0]["span"].is_object());
    assert_eq!(js_call["invocations"][0]["intent"]["text"], "run tests");

    let dynamic = events
        .iter()
        .find(|event| event["call_id"] == "call-dynamic")
        .unwrap();
    assert_eq!(dynamic["invocations"][0]["coverage"], "unsupported");
    assert!(dynamic["invocations"][0]["unsupported_reason"]
        .as_object()
        .is_some());

    let nested = events
        .iter()
        .find(|event| event["call_id"] == "call-js-nested")
        .unwrap();
    assert_eq!(nested["invocations"].as_array().unwrap().len(), 1);
    assert_eq!(nested["invocations"][0]["program"], "echo");
    assert_eq!(nested["invocations"][0]["subcommand"], "actual");

    let concatenated = events
        .iter()
        .find(|event| event["call_id"] == "call-js-concat")
        .unwrap();
    assert_eq!(concatenated["invocations"][0]["coverage"], "unsupported");
    assert!(concatenated["invocations"][0]["unsupported_reason"]
        .as_object()
        .is_some());

    let unicode = events
        .iter()
        .find(|event| event["call_id"] == "call-js-unicode")
        .unwrap();
    assert_eq!(unicode["invocations"][0]["program"], "echo");
    assert_eq!(unicode["invocations"][0]["subcommand"], "A");

    let invalid_argv = events
        .iter()
        .find(|event| event["call_id"] == "call-argv-invalid")
        .unwrap();
    assert_eq!(invalid_argv["invocations"][0]["coverage"], "unsupported");
    assert!(invalid_argv["invocations"][0]["unsupported_reason"]
        .as_object()
        .is_some());

    let runtime = events
        .iter()
        .find(|event| event["call_id"] == "runtime-1" && event["kind"] == "tool-call")
        .unwrap();
    assert_eq!(runtime["invocations"][0]["origin"], "structured-runtime");
    assert_eq!(runtime["invocations"][0]["program"], "sh");
    assert_eq!(
        runtime["invocations"][0]["source_field"],
        "payload.item.command"
    );
    assert_eq!(runtime["name"], "CommandExecution");
    assert!(runtime["invocations"][0]["witnessed_result"].is_object());
    assert!(events
        .iter()
        .any(|event| event["name"] == "FileChange" && event["kind"] == "tool-result"));
    assert!(events.iter().all(|event| {
        !matches!(
            event["name"].as_str(),
            Some("AgentMessage" | "Reasoning" | "UserMessage" | "ContextCompaction")
        )
    }));

    let mut stats = tapes();
    stats.args(["stats", id, "--json"]);
    with_fixture_env(
        &mut stats,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let stats = stats.output().unwrap();
    assert!(stats.status.success());
    let stats: Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(stats["tools"]["calls"], 10);
    assert_eq!(stats["tools"]["results"], 3);
    assert_eq!(stats["tools"]["paired"], 2);

    let mut filtered = tapes();
    filtered.args(["events", id, "--program", "cargo", "--json"]);
    with_fixture_env(
        &mut filtered,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let filtered = filtered.output().unwrap();
    assert!(filtered.status.success());
    let filtered: Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(filtered["events"].as_array().unwrap().len(), 2);
    assert!(filtered["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["kind"] == "tool-call" && event["call_id"] == "call-shell"));

    let mut wrong_name = tapes();
    wrong_name.args(["events", id, "--name", "git", "--json"]);
    with_fixture_env(
        &mut wrong_name,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let wrong_name = wrong_name.output().unwrap();
    assert!(wrong_name.status.success());
    let wrong_name: Value = serde_json::from_slice(&wrong_name.stdout).unwrap();
    assert!(wrong_name["events"].as_array().unwrap().is_empty());

    let bundle_root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-invocation-bundle-{}", std::process::id())),
    );
    let mut export = tapes();
    export
        .args(["export", id, "--bundle"])
        .arg(bundle_root.path());
    with_fixture_env(
        &mut export,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let export = export.output().unwrap();
    assert!(export.status.success());
    let bundle_json = fs::read_dir(bundle_root.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    let bundle: Value = serde_json::from_slice(&fs::read(&bundle_json).unwrap()).unwrap();
    assert!(bundle["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["invocations"].is_array()));
    let trace = fs::read_dir(bundle_root.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|extension| extension == "md")
                && path.to_string_lossy().contains("trace")
        })
        .unwrap();
    assert!(String::from_utf8_lossy(&fs::read(trace).unwrap()).contains("invocation:"));
    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn events_and_stats_qualify_ambiguous_invocation_contexts() {
    let (codex_home, home) = parser_context_fixture_store("context");
    let id = "00000000-0000-0000-0000-000000000008";
    let mut events = tapes();
    events.args(["events", id, "--json"]);
    with_fixture_env(
        &mut events,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let events = events.output().unwrap();
    assert!(events.status.success());
    let events: Value = serde_json::from_slice(&events.stdout).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 5);

    let continuation = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["call_id"] == "call-context-continuation")
        .unwrap();
    assert_eq!(continuation["invocations"].as_array().unwrap().len(), 1);
    assert_eq!(continuation["invocations"][0]["program"], "echo");
    assert_eq!(continuation["invocations"][0]["subcommand"], "next");
    assert!(continuation["invocations"][0].get("arguments").is_none());

    for call_id in [
        "call-context-assignment",
        "call-context-shell-control",
        "call-context-arrow",
        "call-context-short-circuit",
    ] {
        let event = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["call_id"] == call_id)
            .unwrap();
        assert_eq!(
            event["invocations"].as_array().unwrap().len(),
            1,
            "{call_id}"
        );
        assert_eq!(
            event["invocations"][0]["coverage"], "unsupported",
            "{call_id}"
        );
        assert!(
            event["invocations"][0].get("program").is_none(),
            "{call_id}"
        );
    }

    let mut stats = tapes();
    stats.args(["stats", id, "--json"]);
    with_fixture_env(
        &mut stats,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let stats = stats.output().unwrap();
    assert!(stats.status.success());
    let stats: Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(stats["tools"]["calls"], 5);
    assert_eq!(stats["tools"]["results"], 0);
    assert_eq!(stats["tools"]["paired"], 0);
    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn events_qualify_shortened_invocation_names_and_bound_arguments() {
    let (codex_home, home) = invocation_bound_fixture_store("names");
    let id = "00000000-0000-0000-0000-000000000009";
    let mut command = tapes();
    command.args(["events", id, "--json"]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["events"].as_array().unwrap().len(), 5);

    for suffix in [
        "bound-program",
        "bound-subcommand",
        "bound-shell",
        "bound-javascript",
    ] {
        let event = value["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["call_id"] == format!("call-{suffix}"))
            .unwrap();
        assert_eq!(
            event["invocations"].as_array().unwrap().len(),
            1,
            "{suffix}"
        );
        assert_eq!(
            event["invocations"][0]["coverage"], "unsupported",
            "{suffix}"
        );
        assert!(event["invocations"][0].get("program").is_none(), "{suffix}");
        assert!(
            event["invocations"][0]["unsupported_reason"]
                .as_object()
                .is_some(),
            "{suffix}"
        );
    }

    let bounded = value["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["call_id"] == "call-bounded-argument")
        .unwrap();
    assert_eq!(bounded["invocations"][0]["coverage"], "static-literal");
    assert_eq!(bounded["invocations"][0]["program"], "echo");
    assert_eq!(bounded["invocations"][0]["subcommand"], "next");
    assert_eq!(
        bounded["invocations"][0]["arguments"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(bounded["invocations"][0]["arguments"][0]["chars"], 3_000);
    assert_eq!(
        bounded["invocations"][0]["arguments"][0]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        2_048
    );
    assert_eq!(bounded["invocations"][0]["arguments"][0]["truncated"], true);
    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_without_stores_is_empty_and_successful() {
    let temporary_home = std::env::temp_dir().join(format!("tapes-test-{}", std::process::id()));
    let output = tapes()
        .arg("list")
        .env("HOME", temporary_home)
        .env("PATH", "/definitely/missing")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("No harnesses available."));
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("Unavailable: claude, codex, opencode, pi"));
    assert!(output.stderr.is_empty());
}

#[test]
fn list_joins_one_bounded_status_snapshot_and_maps_states() {
    let (codex_home, home) = fixture_store("list");
    let snapshot = r#"{"threads":[
      {"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"},
      {"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"attention"}
    ]}"#;
    let (bin, calls) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        session(&value, "00000000-0000-0000-0000-000000000001")["live"],
        "working"
    );
    assert_eq!(
        session(&value, "10000000-0000-0000-0000-000000000002")["live"],
        "idle"
    );
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn human_list_keeps_id_column_exact() {
    let (codex_home, home) = fixture_store("human-list");
    let snapshot = r#"{"threads":[
      {"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"},
      {"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"idle"}
    ]}"#;
    let (bin, _) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args(["list", "--global"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = text.lines().filter_map(|line| {
        let fields: Vec<_> = line.split('\t').collect();
        (fields.len() == 7).then_some(fields)
    });
    assert_eq!(
        text.lines().next(),
        Some("ID\tLIVE\tHARNESS\tMODEL\tTITLE\tDIRECTORY\tLAST ACTIVITY")
    );
    let first = rows
        .by_ref()
        .find(|row| row[0] != "ID")
        .expect("first session row");
    let second = rows.find(|row| row[0] != "ID").expect("second session row");
    assert!(rows.next().is_none(), "unexpected extra tabular row");
    assert_eq!(first[0], "10000000-0000-0000-0000-000000000002");
    assert_eq!(first[1], "idle");
    assert_eq!(second[0], "00000000-0000-0000-0000-000000000001");
    assert_eq!(second[1], "working");
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn list_filters_metadata_case_insensitively_and_before_limit() {
    let (codex_home, home) = filter_fixture_store("metadata");
    let command = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        command.output().unwrap()
    };

    let model = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--model",
        "GPT-FIXTURE",
        "--json",
    ]);
    assert!(
        model.status.success(),
        "{}",
        String::from_utf8_lossy(&model.stderr)
    );
    let model_value: Value = serde_json::from_slice(&model.stdout).unwrap();
    let model_ids = model_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        model_ids,
        vec![
            "10000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000001"
        ]
    );

    let without_filter = command(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(without_filter.status.success());
    let all: Value = serde_json::from_slice(&without_filter.stdout).unwrap();
    assert_eq!(all["sessions"].as_array().unwrap().len(), 4);
    let no_model = session(&all, "20000000-0000-0000-0000-000000000004");
    assert!(no_model.get("model").is_none());

    let directory = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--directory",
        "OTHER-PROJECT",
        "--json",
    ]);
    assert!(directory.status.success());
    let directory_value: Value = serde_json::from_slice(&directory.stdout).unwrap();
    assert_eq!(directory_value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        directory_value["sessions"][0]["id"],
        "30000000-0000-0000-0000-000000000005"
    );

    let limited = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--model",
        "gpt",
        "--limit",
        "1",
        "--json",
    ]);
    assert!(limited.status.success());
    let limited_value: Value = serde_json::from_slice(&limited.stdout).unwrap();
    assert_eq!(limited_value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        limited_value["sessions"][0]["id"],
        "10000000-0000-0000-0000-000000000002"
    );
    assert_eq!(limited_value["scanned"], 3);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_filters_activity_before_limit_and_serializes_the_window() {
    let (codex_home, home) = filter_fixture_store("activity");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T10:00:00+00:00",
        "--until",
        "2026-01-01T10:30:00Z",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["sort"], "newest");
    assert_eq!(
        value["activity"],
        serde_json::json!({
            "since": "2026-01-01T10:00:00Z",
            "until": "2026-01-01T10:30:00Z"
        })
    );
    assert_eq!(
        value["sessions"][0]["id"],
        "00000000-0000-0000-0000-000000000001"
    );
    assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(value["scanned"], 4);
    assert_eq!(value["scan_truncated"], false);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_sort_oldest_orders_recorded_activity_and_keeps_activity_absent() {
    let (codex_home, home) = filter_fixture_store("sort");
    let command = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        command.output().unwrap()
    };

    let newest = command(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(newest.status.success());
    let newest_value: Value = serde_json::from_slice(&newest.stdout).unwrap();
    let newest_ids = newest_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        newest_ids,
        vec![
            "30000000-0000-0000-0000-000000000005",
            "20000000-0000-0000-0000-000000000004",
            "10000000-0000-0000-0000-000000000002",
            "00000000-0000-0000-0000-000000000001"
        ]
    );
    assert_eq!(newest_value["sort"], "newest");
    assert!(newest_value.get("activity").is_none());

    let oldest = command(&[
        "list",
        "--global",
        "--harness",
        "codex",
        "--sort",
        "oldest",
        "--json",
    ]);
    assert!(oldest.status.success());
    let oldest_value: Value = serde_json::from_slice(&oldest.stdout).unwrap();
    let oldest_ids = oldest_value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        oldest_ids,
        vec![
            "00000000-0000-0000-0000-000000000001",
            "10000000-0000-0000-0000-000000000002",
            "20000000-0000-0000-0000-000000000004",
            "30000000-0000-0000-0000-000000000005"
        ]
    );
    assert_eq!(oldest_value["sort"], "oldest");
    assert!(oldest_value.get("activity").is_none());

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_accepts_date_only_activity_bounds_and_omits_absent_bound() {
    let (codex_home, home) = filter_fixture_store("date-only");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["activity"]["since"], "2026-01-01T00:00:00Z");
    assert!(value["activity"].get("until").is_none());
    assert_eq!(
        value["sessions"][0]["id"],
        "30000000-0000-0000-0000-000000000005"
    );

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn missing_recorded_activity_is_preserved_and_blocks_latest_selection() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-missing-activity-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "60000000-0000-0000-0000-000000000001";
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/fixture\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"timestamp-free\"}}]}}}}\n"
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
        command.output().unwrap()
    };
    let listed = run(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let row = session(&listed, id);
    assert!(row.get("started_at").is_none());
    assert!(row.get("last_activity_at").is_none());
    assert_eq!(row["source"]["recorded_harness"], "codex");

    let latest = run(&[
        "show",
        "--global",
        "--harness",
        "codex",
        "--latest",
        "--json",
    ]);
    assert!(!latest.status.success());
    assert!(
        String::from_utf8_lossy(&latest.stderr).contains("no recorded activity timestamp"),
        "{}",
        String::from_utf8_lossy(&latest.stderr)
    );
}

#[test]
fn non_text_message_remains_as_content_evidence() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-content-parts-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "61000000-0000-0000-0000-000000000001";
    let body = format!(
        "{{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T10:00:00Z\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/fixture\"}}}}\n{{\"type\":\"response_item\",\"timestamp\":\"2026-01-01T10:00:01Z\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"image\",\"uri\":\"/unreadable-canary\"}}]}}}}\n"
    );
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    let mut command = tapes();
    command.args(["show", id, "--json"]);
    with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(value["turns"][0]["parts"][0]["kind"], "media-reference");
    assert_eq!(
        value["turns"][0]["parts"][0]["reference"]["uri"],
        "/unreadable-canary"
    );
    assert_eq!(value["content"]["references"], 1);
    assert_eq!(
        value["turns"][0]["parts"][0]["reference"]["source"]["part_index"],
        0
    );
    assert!(value["turns"][0]["parts"][0]["reference"]["source"]["span"].is_object());

    let mut brief = tapes();
    brief.args(["brief", id, "--json"]);
    with_fixture_env(&mut brief, &codex, &root.path().join("home"), root.path());
    let brief = brief.output().unwrap();
    assert!(brief.status.success());
    let brief: Value = serde_json::from_slice(&brief.stdout).unwrap();
    assert_eq!(brief["content"]["references"], 1);
}

#[test]
fn list_rejects_a_non_half_open_activity_window() {
    let cases = [
        ("2026-01-01T10:00:00Z", "2026-01-01T10:00:00Z"),
        ("2026-01-01T11:00:00Z", "2026-01-01T10:00:00Z"),
    ];
    for (since, until) in cases {
        let (codex_home, home) = filter_fixture_store("invalid-window");
        let mut command = tapes();
        command.args([
            "list",
            "--global",
            "--harness",
            "codex",
            "--since",
            since,
            "--until",
            until,
        ]);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("--since must be earlier than --until"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(codex_home).unwrap();
    }
}

#[test]
fn list_searches_recent_fixture_turns_before_the_limit_and_survives_a_bad_line() {
    let (codex_home, home) = filter_fixture_store("search");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--search",
        "VALID",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("last 32 normalized turns"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["sessions"][0]["id"],
        "10000000-0000-0000-0000-000000000002"
    );
    assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(value["scanned"], 4);
    assert_eq!(value["scan_truncated"], false);
    assert!(value["unsearched"].as_array().unwrap().is_empty());

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn liveness_tolerates_unknown_states() {
    let (codex_home, home) = fixture_store("unknown-state");
    let snapshot = r#"{"threads":[
      {"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"},
      {"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"waiting-for-future"}
    ]}"#;
    let (bin, _) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        session(&value, "00000000-0000-0000-0000-000000000001")["live"],
        "working"
    );
    assert!(session(&value, "10000000-0000-0000-0000-000000000002")["live"].is_null());
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn liveness_times_out_stalled_authority() {
    let (codex_home, home) = fixture_store("timeout");
    let (bin, calls, parent_pid, child_pid) = fake_stalled_status(&codex_home);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let started = Instant::now();
    let output = command.output().unwrap();
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "stalled authority took {elapsed:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    assert_process_gone(&parent_pid);
    assert_process_gone(&child_pid);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn liveness_reaps_exited_authority_with_inherited_pipe() {
    let (codex_home, home) = fixture_store("exited-timeout");
    let (bin, calls, parent_pid, child_pid) = fake_exited_stalled_status(&codex_home);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let started = Instant::now();
    let output = command.output().unwrap();
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "exited stalled authority took {elapsed:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    assert_process_gone(&parent_pid);
    assert_process_gone(&child_pid);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn oversized_status_is_optional() {
    let (codex_home, home) = fixture_store("oversized");
    let oversized = format!("{{\"threads\":[]}}{}", "x".repeat(64 * 1024));
    let (bin, calls) = fake_status(&codex_home, &oversized, 0);
    let mut command = tapes();
    command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let started = Instant::now();
    let output = command.output().unwrap();
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "oversized authority took {elapsed:?}"
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn show_human_header_marks_matching_live_state() {
    let (codex_home, home) = fixture_store("show");
    let snapshot = r#"{"threads":[{"id":"00000000-0000-0000-0000-000000000001","harness":"codex","state":"working"}]}"#;
    let (bin, calls) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args([
        "show",
        "00000000-0000-0000-0000-000000000001",
        "--tail",
        "1",
    ]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("# codex 00000000-0000-0000-0000-000000000001 [working]\n"));
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn show_json_carries_matching_live_state() {
    let (codex_home, home) = fixture_store("show-json");
    let snapshot = r#"{"threads":[{"id":"10000000-0000-0000-0000-000000000002","harness":"codex","state":"idle"}]}"#;
    let (bin, calls) = fake_status(&codex_home, snapshot, 0);
    let mut command = tapes();
    command.args([
        "show",
        "10000000-0000-0000-0000-000000000002",
        "--tail",
        "1",
        "--json",
    ]);
    with_fixture_env(&mut command, &codex_home, &home, &bin);
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["live"], "idle");
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn show_names_a_trailing_record_in_human_and_json_output() {
    let (codex_home, home) = fixture_store("trailing-record");

    let mut human_command = tapes();
    human_command.args([
        "show",
        "00000000-0000-0000-0000-000000000001",
        "--tail",
        "1",
    ]);
    with_fixture_env(&mut human_command, &codex_home, &home, &codex_home);
    let human = human_command.output().unwrap();
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("The newest trailing record is `event_msg` at 2026-01-01T10:00:07Z"),
        "{text}"
    );

    let mut json_command = tapes();
    json_command.args([
        "show",
        "00000000-0000-0000-0000-000000000001",
        "--tail",
        "1",
        "--json",
    ]);
    with_fixture_env(&mut json_command, &codex_home, &home, &codex_home);
    let json_output = json_command.output().unwrap();
    assert!(json_output.status.success());
    let value: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(
        value["trailing_record"],
        serde_json::json!({
            "kind": "event_msg",
            "timestamp": "2026-01-01T10:00:07Z"
        })
    );

    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn show_omits_an_absent_trailing_record_in_human_and_json_output() {
    let (codex_home, home) = fixture_store("no-trailing-record");

    let mut human_command = tapes();
    human_command.args([
        "show",
        "10000000-0000-0000-0000-000000000002",
        "--tail",
        "1",
    ]);
    with_fixture_env(&mut human_command, &codex_home, &home, &codex_home);
    let human = human_command.output().unwrap();
    assert!(human.status.success());
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(!text.contains("trailing record"), "{text}");

    let mut json_command = tapes();
    json_command.args([
        "show",
        "10000000-0000-0000-0000-000000000002",
        "--tail",
        "1",
        "--json",
    ]);
    with_fixture_env(&mut json_command, &codex_home, &home, &codex_home);
    let json_output = json_command.output().unwrap();
    assert!(json_output.status.success());
    let value: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert!(value.get("trailing_record").is_none());

    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn unusable_status_is_optional_and_export_omits_volatile_state() {
    let (codex_home, home) = fixture_store("optional");
    let (bin, calls) = fake_status(&codex_home, "{not-json", 17);
    let mut list_command = tapes();
    list_command.args(["list", "--global", "--json"]);
    with_fixture_env(&mut list_command, &codex_home, &home, &bin);
    let list_output = list_command.output().unwrap();

    assert!(list_output.status.success());
    let value: Value = serde_json::from_slice(&list_output.stdout).unwrap();
    assert!(session(&value, "00000000-0000-0000-0000-000000000001")["live"].is_null());
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);

    let bundle = codex_home.join("bundle");
    let mut export_command = tapes();
    export_command
        .args(["export", "00000000-0000-0000-0000-000000000001", "--bundle"])
        .arg(&bundle);
    with_fixture_env(&mut export_command, &codex_home, &home, &bin);
    let export_output = export_command.output().unwrap();
    assert!(
        export_output.status.success(),
        "{}",
        String::from_utf8_lossy(&export_output.stderr)
    );
    assert_eq!(fs::read_to_string(&calls).unwrap().lines().count(), 1);
    let json_path = fs::read_dir(&bundle)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .unwrap();
    assert!(!fs::read_to_string(json_path).unwrap().contains("\"live\""));
    let _ = fs::remove_dir_all(codex_home);
}

#[test]
fn piped_help_does_not_panic() {
    let mut child = tapes()
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();

    assert!(!String::from_utf8_lossy(&output.stderr).contains("panic"));
}

#[test]
fn exporting_an_unknown_session_writes_no_bundle_and_fails() {
    let directory = std::env::temp_dir().join(format!("tapes-cli-export-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let output = tapes()
        .args(["export", "definitely-not-a-session"])
        .arg("--bundle")
        .arg(&directory)
        .env("HOME", std::env::temp_dir().join("tapes-cli-empty-home"))
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "the manifest is the only stdout");
    assert!(!directory.exists(), "a failed export leaves nothing behind");
}

#[test]
fn human_renderers_show_derived_titles_and_whole_second_timestamps() {
    let root = std::env::temp_dir().join(format!("tapes-cli-human-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let id = "00000000-0000-0000-0000-0000000000aa";
    let path = root
        .join("sessions/2026/01/01")
        .join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        concat!(
            r#"{"timestamp":"2026-01-01T10:00:00.123456789Z","type":"session_meta","payload":{"id":"00000000-0000-0000-0000-0000000000aa","cwd":"/fixtures/project"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:00:01.123456789Z","type":"turn_context","payload":{"cwd":"/fixtures/project","model":"gpt-fixture","effort":"high"}}"#,
            "\n",
            r##"{"timestamp":"2026-01-01T10:00:02.987654321Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"\n  <recommended_plugins>\n- Fixture helper\n</recommended_plugins>\n\n  # AGENTS.md instructions\n\n<INSTRUCTIONS>\nFollow the repository instructions before acting.\n</INSTRUCTIONS>\n\n<environment_context>\n  <cwd>/fixtures/project</cwd>\n</environment_context>\n\nInspect the fixture."}]}}"##,
            "\n",
            r#"{"timestamp":"2026-01-01T10:00:03.123456789Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixture inspected."}]}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T10:00:06.123456789Z","type":"event","payload":{}}"#,
            "\n"
        ),
    )
    .unwrap();

    let command = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", root.join("home"))
            .env("CODEX_HOME", &root)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };

    let list = command(&["list", "--harness", "codex"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_text = String::from_utf8_lossy(&list.stdout);
    assert!(list_text.contains("~Inspect the fixture."), "{list_text}");
    assert!(
        !list_text.contains("# AGENTS.md instructions"),
        "{list_text}"
    );
    assert!(!list_text.contains("recommended_plugins"), "{list_text}");
    assert!(list_text.contains("2026-01-01T10:00:06Z"), "{list_text}");
    assert!(!list_text.contains(".123456789Z"), "{list_text}");
    assert!(!list_text.contains("+00:00"), "{list_text}");

    let list_json = command(&["list", "--harness", "codex", "--json"]);
    assert!(list_json.status.success());
    let list_value: serde_json::Value = serde_json::from_slice(&list_json.stdout).unwrap();
    let session = &list_value["sessions"][0];
    assert!(session.get("title").is_none());
    assert_eq!(session["derived_title"], "Inspect the fixture.");
    assert_eq!(session["derived_title_truncated"], false);
    assert_eq!(
        session["last_activity_at"],
        "2026-01-01T10:00:06.123456789Z"
    );

    let show = command(&["show", id]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show_text = String::from_utf8_lossy(&show.stdout);
    assert!(
        show_text.contains("[user #0 2026-01-01T10:00:02Z]"),
        "{show_text}"
    );
    assert!(!show_text.contains(".987654321Z"), "{show_text}");
    assert!(!show_text.contains("+00:00"), "{show_text}");

    let show_json = command(&["show", id, "--json"]);
    assert!(show_json.status.success());
    let show_value: serde_json::Value = serde_json::from_slice(&show_json.stdout).unwrap();
    assert_eq!(
        show_value["turns"][0]["ts"],
        "2026-01-01T10:00:02.987654321Z"
    );
    assert!(show_value["session"].get("title").is_none());
    assert_eq!(
        show_value["session"]["derived_title"],
        "Inspect the fixture."
    );
    assert_eq!(show_value["session"]["derived_title_truncated"], false);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn list_json_marks_derived_title_truncation_without_changing_the_title_text() {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-derived-title-marker-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
        CODEX_SESSION_ONE,
    )
    .unwrap();
    let long_id = "00000000-0000-0000-0000-0000000000bb";
    let long_request = "word ".repeat(96);
    fs::write(
        sessions.join(format!(
            "rollout-2026-01-01T09-00-00-{long_id}.jsonl"
        )),
        format!(
            concat!(
                r#"{{"timestamp":"2026-01-01T09:00:00Z","type":"session_meta","payload":{{"id":"{long_id}","cwd":"/fixtures/project"}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-01-01T09:00:01Z","type":"turn_context","payload":{{"cwd":"/fixtures/project","model":"gpt-fixture","effort":"low"}}}}"#,
                "\n",
                r#"{{"timestamp":"2026-01-01T09:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{long_request}"}}]}}}}"#,
                "\n"
            ),
            long_id = long_id,
            long_request = long_request
        ),
    )
    .unwrap();

    let mut command = tapes();
    command.args(["list", "--global", "--harness", "codex", "--json"]);
    command
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", &root)
        .env("PATH", "/definitely/missing");
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let complete = session(&value, "00000000-0000-0000-0000-000000000001");
    assert_eq!(complete["derived_title"], "Inspect the fixture.");
    assert_eq!(complete["derived_title_truncated"], false);

    let shortened = session(&value, long_id);
    assert_eq!(shortened["derived_title_truncated"], true);
    assert_eq!(
        shortened["derived_title"].as_str().unwrap().chars().count(),
        96
    );
    assert!(shortened["derived_title"].as_str().unwrap().ends_with('…'));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn opencode_titleless_cli_keeps_metadata_absent_without_message_title_derivation() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-titleless-{}",
        std::process::id()
    )));
    let program = titleless_opencode_program(root.path());
    let alias_path = program.path().to_owned();
    let id = "ses_titleless_fixture";

    let command = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", root.path().join("home"))
            .env("PATH", root.path())
            .output()
            .unwrap()
    };

    let list = command(&["list", "--harness", "opencode", "--json"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_value: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let listed = &list_value["sessions"][0];
    assert_eq!(listed["id"], id);
    assert!(listed.get("title").is_none());
    assert!(listed.get("derived_title").is_none());
    assert!(listed.get("derived_title_truncated").is_none());

    let show = command(&["show", id, "--json"]);
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let show_value: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert!(show_value["session"].get("title").is_none());
    assert!(show_value["session"].get("derived_title").is_none());
    assert!(show_value["session"]
        .get("derived_title_truncated")
        .is_none());

    let bundle = root.path().join("bundle");
    let bundle_argument = bundle.to_str().unwrap().to_owned();
    let export = command(&["export", id, "--bundle", &bundle_argument]);
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    let context = std::fs::read_dir(&bundle)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "md"))
        .map(|path| std::fs::read_to_string(path).unwrap())
        .unwrap();
    assert!(!context.contains("- title:"), "{context}");

    drop(program);
    assert!(
        !alias_path.exists(),
        "titleless OpenCode alias was not removed"
    );
}

#[test]
fn opencode2_session_resolves_when_stable_cli_is_also_installed() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode2-with-stable-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");
    let id = "ses_000000fixtureSharedSession";

    let output = tapes()
        .args(["show", id, "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["turns"][0]["text"], "Inspect the fixture.");
}

#[test]
fn opencode_listing_is_the_deduplicated_union_of_database_and_api_sessions() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-composition-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");

    let output = tapes()
        .args(["list", "--harness", "opencode", "--global", "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut ids = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            "ses_000000fixtureSharedSession",
            "ses_api_only_fixture",
            "ses_database_only_fixture"
        ]
    );
    assert_eq!(
        value["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|session| session["id"] == "ses_000000fixtureSharedSession")
            .count(),
        1
    );
}

#[test]
fn malformed_opencode_database_row_is_unreadable_without_breaking_listing() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-malformed-row-{}",
        std::process::id()
    )));
    let _stable = malformed_opencode_program(root.path());

    let list = |json: bool| {
        let mut arguments = vec!["list", "--harness", "opencode", "--global"];
        if json {
            arguments.push("--json");
        }
        tapes()
            .args(arguments)
            .env("HOME", root.path().join("home"))
            .env("PATH", root.path())
            .output()
            .unwrap()
    };

    let json = list(true);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: Value = serde_json::from_slice(&json.stdout).unwrap();
    let mut ids = value["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            "ses_000000fixtureSharedSession",
            "ses_database_only_fixture"
        ]
    );
    let unreadable = value["unreadable"].as_array().unwrap();
    assert_eq!(value["scanned"], 3);
    assert_eq!(unreadable.len(), 2, "{unreadable:?}");
    assert!(unreadable[0]
        .as_str()
        .unwrap()
        .ends_with("opencode.db: 1 of 3 session rows unreadable"));
    assert!(unreadable[1]
        .as_str()
        .unwrap()
        .contains("ses_truncated_fixture"));
    assert!(unreadable[1]
        .as_str()
        .unwrap()
        .contains("EOF while parsing a string"));
    // A corrupt row says nothing about the harness, which was read fine.
    assert!(
        value["unavailable"].as_array().unwrap().is_empty(),
        "{value}"
    );

    let human = list(false);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human_text = String::from_utf8_lossy(&human.stdout);
    assert!(
        human_text.contains("ses_database_only_fixture"),
        "{human_text}"
    );
    assert!(
        human_text.contains("ses_000000fixtureSharedSession"),
        "{human_text}"
    );
    assert!(
        human_text.contains("Unreadable: opencode session ses_truncated_fixture:"),
        "{human_text}"
    );
    assert!(
        human_text.contains("EOF while parsing a string"),
        "{human_text}"
    );
    assert!(
        !human_text.contains("No harnesses available."),
        "{human_text}"
    );

    let show = tapes()
        .args(["show", "ses_truncated_fixture", "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(!show.status.success());
    let error = String::from_utf8_lossy(&show.stderr);
    assert!(error.contains("ses_truncated_fixture"), "{error}");
    assert!(error.contains("EOF while parsing a string"), "{error}");
}

/// The stable store takes precedence for an id both OpenCode stores answer.
/// When its rows cannot be parsed, no read answers from the opencode2
/// projection in its place: an exact read fails naming the store that failed
/// and the store that answered, and a listing reports the id as unreadable.
#[test]
fn an_unreadable_stable_opencode_store_is_not_answered_from_opencode2() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-unreadable-store-{}",
        std::process::id()
    )));
    let stable = root.path().join("opencode");
    std::os::unix::fs::symlink(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/opencode/opencode-unreadable-rows"),
        &stable,
    )
    .unwrap();
    let _stable = OpenCodeAlias { path: stable };
    let _beta = opencode_program(root.path(), "opencode2");
    let shared = "ses_000000fixtureSharedSession";
    let bundle = root.path().join("bundle");
    let bundle = bundle.to_str().unwrap();
    let home = root.path().join("home");
    let run = |args: &[&str]| {
        tapes()
            .args(args)
            .env("HOME", &home)
            .env_remove("XDG_DATA_HOME")
            .env("PATH", root.path())
            .output()
            .unwrap()
    };
    let store = home.join(".local/share/opencode/opencode.db");
    let store = store.to_str().unwrap();

    for args in [
        vec!["show", shared, "--json"],
        vec!["show", shared, "--full", "--json"],
        vec!["events", shared, "--json"],
        vec!["usage", shared, "--json"],
        vec!["stats", shared, "--json"],
        vec!["brief", shared, "--json"],
        vec!["export", shared, "--bundle", bundle],
    ] {
        let output = run(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} answered: {stdout}");
        assert!(output.stdout.is_empty(), "{args:?} answered: {stdout}");
        assert!(error.contains(store), "{args:?}: {error}");
        assert!(error.contains("EOF while parsing"), "{args:?}: {error}");
        assert!(
            error.contains(&format!("opencode2:/api/session/{shared}")),
            "{args:?}: {error}"
        );
    }

    let listed = |args: &[&str]| -> Value {
        let output = run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let names_shared = |value: &Value| {
        value["unreadable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic.as_str().unwrap().contains(shared))
    };

    let list = listed(&["list", "--harness", "opencode", "--global", "--json"]);
    let ids = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["ses_api_only_fixture"], "{list}");
    assert!(names_shared(&list), "{list}");

    let endings = listed(&["endings", "--harness", "opencode", "--global", "--json"]);
    let ended = endings["endings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|ending| ending["session"]["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ended, vec!["ses_api_only_fixture"], "{endings}");
    assert!(names_shared(&endings), "{endings}");
}

/// A listing names the stable OpenCode store it could not read and never
/// lists, from opencode2, a session that store may hold: rows whose ids the
/// transport hid are named from the store's ids alone, and a store that
/// cannot be listed at all withholds every opencode2 session, counted.
#[test]
fn a_listing_names_the_opencode_store_it_could_not_read() {
    for (fixture, expected, diagnostics) in [
        (
            "opencode-quoted-rows",
            vec!["ses_api_only_fixture"],
            vec![
                "2 of 2 session rows unreadable",
                "ses_000000fixtureSharedSession",
            ],
        ),
        (
            "opencode-list-fails",
            vec![],
            vec![
                "could not be listed",
                "exited with exit status: 1",
                "withheld 2 sessions another store listed",
            ],
        ),
    ] {
        let root = TemporaryDirectory::new(
            std::env::temp_dir().join(format!("tapes-cli-{fixture}-{}", std::process::id())),
        );
        let stable = root.path().join("opencode");
        std::os::unix::fs::symlink(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../tests/fixtures/opencode/{fixture}")),
            &stable,
        )
        .unwrap();
        let _stable = OpenCodeAlias { path: stable };
        let _beta = opencode_program(root.path(), "opencode2");
        let home = root.path().join("home");
        let output = tapes()
            .args(["list", "--harness", "opencode", "--global", "--json"])
            .env("HOME", &home)
            .env_remove("XDG_DATA_HOME")
            .env("PATH", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{fixture}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let list: Value = serde_json::from_slice(&output.stdout).unwrap();
        let ids = list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, expected, "{fixture}: {list}");
        let unreadable = list["unreadable"].to_string();
        let store = home.join(".local/share/opencode/opencode.db");
        assert!(
            unreadable.contains(store.to_str().unwrap()),
            "{fixture}: {unreadable}"
        );
        for diagnostic in diagnostics {
            assert!(unreadable.contains(diagnostic), "{fixture}: {unreadable}");
        }
    }
}

#[test]
fn database_prefilter_failure_is_visible_while_the_safe_fallback_runs() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-prefilter-failure-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let mut command = tapes();
    command.args([
        "list",
        "--harness",
        "opencode",
        "--global",
        "--search",
        "PREFILTERERROR",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &root.path().join("codex"),
        &root.path().join("home"),
        root.path(),
    );

    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["sessions"].as_array().unwrap().is_empty());
    assert_eq!(value["unsearched"].as_array().unwrap().len(), 1);
    assert!(value["unsearched"][0]
        .as_str()
        .unwrap()
        .contains("opencode search prefilter failed"));
}

#[test]
fn opencode_database_transcripts_preserve_normalized_turn_roles_and_text() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-database-transcript-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");
    let id = "ses_database_only_fixture";

    let output = tapes()
        .args(["show", id, "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["id"], id);
    let turns = value["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 4);
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(turns[0]["text"], "Inspect the database fixture.");
    assert_eq!(turns[1]["role"], "reasoning");
    assert_eq!(turns[1]["text"], "Reason through the database projection.");
    assert_eq!(turns[2]["role"], "tool");
    let tool_text = turns[2]["text"].as_str().unwrap();
    assert!(tool_text.contains("\"tool\":\"read\""), "{tool_text}");
    assert!(tool_text.contains("Database fixture read."), "{tool_text}");
    assert_eq!(turns[3]["role"], "assistant");
    assert_eq!(turns[3]["text"], "Database-backed answer.");

    let output = tapes()
        .args(["events", id, "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 2);
    assert_eq!(events["events"][0]["call_id"], "call_database_fixture");
    assert_eq!(events["events"][0]["duration_ms"], 999);
    assert_eq!(events["events"][1]["kind"], "tool-result");
    assert_eq!(events["events"][1]["ts"], "2026-07-21T19:45:08.999Z");
}

/// The bounded reader keeps a file's tail, so an oversized session would
/// otherwise report the first retained record as its start. `show --json`
/// reports the header's recorded start and still marks the window truncated.
#[test]
fn show_reports_the_recorded_start_of_an_oversized_session() {
    let root =
        std::env::temp_dir().join(format!("tapes-cli-oversized-start-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-00000000bbbb";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    use std::io::Write;
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for _ in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T12:00:00Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);
    assert!(fs::metadata(&path).unwrap().len() > 4 * 1024 * 1024);

    let mut command = tapes();
    command.args(["show", id, "--tail", "1", "--json"]);
    command
        .env("HOME", root.join("home"))
        .env("CODEX_HOME", &root)
        .env("PATH", "/definitely/missing");
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["session"]["started_at"], "2026-01-01T10:00:00Z");
    assert_eq!(value["session"]["last_activity_at"], "2026-01-01T12:00:00Z");
    assert_eq!(value["session"]["directory"], "/fixtures/project");
    assert_eq!(value["truncated"], true);
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["truncation"]["source"],
        serde_json::json!([{ "kind": "file-tail", "bytes": 4_194_304 }])
    );
    assert_eq!(value["truncation"]["window"]["returned"], 1);
    assert_eq!(value["truncation"]["window"]["bound"], 1);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn show_preserves_aligned_tail_records_and_reports_only_real_partial_prefixes() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-tail-alignment-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let bound = 4 * 1024 * 1024;
    let header = |id: &str| {
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": "2026-01-01T10:00:00Z",
                "type": "session_meta",
                "payload": {"id": id, "source": "exec", "cwd": "/fixtures/project"}
            })
        )
    };
    let exact_id = "80000000-0000-0000-0000-000000000001";
    let exact_header = header(exact_id);
    let exact_turn = format!(
        "{}\n",
        serde_json::json!({
            "timestamp": "2026-01-01T10:00:01Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "EXACT_BOUNDARY_MESSAGE"}]
            }
        })
    );
    let padding_overhead = serde_json::json!({"padding": ""}).to_string().len() + 1;
    let exact_padding = format!(
        "{}\n",
        serde_json::json!({
            "padding": "x".repeat(bound - exact_turn.len() - padding_overhead)
        })
    );
    assert_eq!(exact_turn.len() + exact_padding.len(), bound);
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{exact_id}.jsonl")),
        format!("{exact_header}{exact_turn}{exact_padding}"),
    )
    .unwrap();

    let partial_id = "80000000-0000-0000-0000-000000000002";
    let partial_header = header(partial_id);
    let partial = format!(
        "{}\n",
        serde_json::json!({
            "padding": "x".repeat(bound + 128 - padding_overhead)
        })
    );
    let later = format!(
        "{}\n",
        serde_json::json!({
            "timestamp": "2026-01-01T10:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "AFTER_PARTIAL_RECORD"}]
            }
        })
    );
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{partial_id}.jsonl")),
        format!("{partial_header}{partial}{later}"),
    )
    .unwrap();

    let show = |id: &str| {
        let mut command = tapes();
        command.args(["show", id, "--json"]);
        with_fixture_env(
            &mut command,
            &codex,
            &root.path().join("home"),
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };

    let exact = show(exact_id);
    assert!(exact["turns"]
        .as_array()
        .unwrap()
        .iter()
        .any(|turn| turn["text"] == "EXACT_BOUNDARY_MESSAGE"));
    let exact_ranges = exact["read"]["ranges"].as_array().unwrap();
    let exact_tail = exact_ranges
        .iter()
        .find(|range| range["kind"] == "tail")
        .unwrap();
    let exact_alignment = exact_ranges
        .iter()
        .find(|range| range["kind"] == "alignment")
        .unwrap();
    assert_eq!(exact_alignment["span"]["end"], exact_tail["span"]["start"]);
    assert!(exact["read"].get("gaps").is_none_or(|gaps| {
        gaps.as_array().is_some_and(|gaps| {
            gaps.iter()
                .all(|gap| gap["reason"] != "discarded-partial-record")
        })
    }));

    let partial = show(partial_id);
    assert!(partial["turns"]
        .as_array()
        .unwrap()
        .iter()
        .any(|turn| turn["text"] == "AFTER_PARTIAL_RECORD"));
    assert!(partial["read"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gap| gap["reason"] == "discarded-partial-record"));
}

/// The recorded case: a 147-turn session shown through the default window
/// returns 100 turns and says 47 earlier ones were windowed out, with no
/// source bound; a wide enough window clears it; export reads every turn.
#[test]
fn show_distinguishes_the_turn_window_from_source_truncation() {
    let root = std::env::temp_dir().join(format!(
        "tapes-cli-window-provenance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let sessions = root.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "00000000-0000-0000-0000-00000000cccc";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let mut file = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    use std::io::Write;
    writeln!(
        file,
        r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    for index in 0..147 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-01-01T10:00:{:02}Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"turn {index}"}}]}}}}"#,
            index % 60
        )
        .unwrap();
    }
    drop(file);

    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        command
            .env("HOME", root.join("home"))
            .env("CODEX_HOME", &root)
            .env("PATH", "/definitely/missing");
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 100);
    assert_eq!(value["truncated"], true);
    assert_eq!(
        value["truncation"],
        serde_json::json!({
            "window": { "returned": 100, "omitted": 47, "omitted_from": "head", "bound": 100, "ordinals": { "first": 47, "last": 146 } }
        })
    );

    let human = String::from_utf8(run(&["show", id])).unwrap();
    assert!(
        human.contains(
            "Showing the last 100 of 147 turns; 47 earlier turns fall outside the 100-turn window"
        ),
        "{human}"
    );
    assert!(human.contains("--tail 147"), "{human}");

    let value: Value =
        serde_json::from_slice(&run(&["show", id, "--tail", "200", "--json"])).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 147);
    assert_eq!(value["truncated"], false);
    assert!(value.get("truncation").is_none(), "{value}");

    let bundle = root.join("bundle");
    let listing =
        String::from_utf8(run(&["export", id, "--bundle", bundle.to_str().unwrap()])).unwrap();
    let json_path = listing
        .lines()
        .find(|line| line.contains(".json"))
        .and_then(|line| line.split('\t').next())
        .unwrap();
    let exported: Value = serde_json::from_str(&fs::read_to_string(json_path).unwrap()).unwrap();
    assert_eq!(exported["turns"].as_array().unwrap().len(), 147);
    assert_eq!(exported["truncated"], false);
    assert!(exported.get("truncation").is_none(), "{exported}");

    fs::remove_dir_all(root).unwrap();
}

/// The database projection cuts long part text; the transcript names that
/// bound and the count of parts it cut rather than a bare flag.
#[test]
fn opencode_database_reads_name_cut_turn_text_as_a_source_bound() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-cut-text-{}",
        std::process::id()
    )));
    let _stable = opencode_program(root.path(), "opencode");
    let _beta = opencode_program(root.path(), "opencode2");

    let output = tapes()
        .args(["show", "ses_database_only_fixture", "--json"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["truncated"], true);
    assert_eq!(
        value["truncation"],
        serde_json::json!({
            "source": [{ "kind": "turn-text", "turns": 1, "chars": 4000 }]
        })
    );

    let human = tapes()
        .args(["show", "ses_database_only_fixture"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("1 turn carries text cut at 4000 characters"),
        "{text}"
    );
    assert!(!text.contains("Use --tail"), "{text}");
}

/// A consumer re-finds a turn from the session id and the ordinal alone, so
/// ordinals count the whole normalized sequence even under a window, the
/// window names the ordinals it holds, and the session names where it was
/// read from.
#[test]
fn show_gives_every_turn_a_coordinate_a_consumer_can_write_down() {
    let (codex_home, home) = fixture_store("source-reference");
    let id = "00000000-0000-0000-0000-000000000001";
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let whole: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    let turns = whole["turns"].as_array().unwrap();
    for (index, turn) in turns.iter().enumerate() {
        assert_eq!(turn["ordinal"], index, "{turn}");
    }
    assert!(
        whole["session"]["source"]["location"]["locator"]
            .as_str()
            .unwrap()
            .ends_with(&format!("{id}.jsonl")),
        "{}",
        whole["session"]
    );
    assert!(whole.get("truncation").is_none());

    let last = turns.len() - 1;
    let windowed: Value =
        serde_json::from_slice(&run(&["show", id, "--tail", "1", "--json"])).unwrap();
    assert_eq!(windowed["turns"][0]["ordinal"], last);
    assert_eq!(windowed["turns"][0]["text"], turns[last]["text"]);
    assert_eq!(
        windowed["truncation"]["window"]["ordinals"],
        serde_json::json!({ "first": last, "last": last })
    );

    let again: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    assert_eq!(
        again["turns"], whole["turns"],
        "ordinals are stable across reads"
    );

    let human = String::from_utf8(run(&["show", id])).unwrap();
    assert!(human.contains("[user #0 "), "{human}");
    assert!(human.contains(&format!(" #{last} ")), "{human}");

    let listed: Value =
        serde_json::from_slice(&run(&["list", "--global", "--harness", "codex", "--json"]))
            .unwrap();
    assert_eq!(
        session(&listed, id)["source"]["location"]["locator"],
        whole["session"]["source"]["location"]["locator"]
    );

    fs::remove_dir_all(codex_home).unwrap();
}

/// `show --tail 1` on a session whose whole message projection exceeds the
/// transport bound returns the one turn, names the page bound, and never asks
/// for the unpaged projection.
#[test]
fn show_tail_on_an_oversized_opencode_session_stays_bounded() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-oversized-{}",
        std::process::id()
    )));
    let program = opencode_program(root.path(), "opencode2");
    let id = "ses_oversized_fixture";
    let run = |args: &[&str]| {
        let output = tapes()
            .args(args)
            .env("HOME", root.path().join("home"))
            .env("PATH", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value =
        serde_json::from_slice(&run(&["show", id, "--tail", "1", "--json"])).unwrap();
    assert_eq!(value["turns"].as_array().unwrap().len(), 1);
    assert_eq!(value["truncated"], true);
    assert!(value["truncation"].get("source").is_none(), "{value}");
    assert_eq!(
        value["truncation"]["window"],
        serde_json::json!({
            "returned": 1, "omitted": 7, "omitted_from": "head", "bound": 1,
            "ordinals": { "first": 7, "last": 7 }, "omitted_exact": false
        })
    );

    let human = String::from_utf8(run(&["show", id, "--tail", "1"])).unwrap();
    assert!(
        human.contains("Showing the last 1 of at least 8 turns; the read stopped once the 1-turn window was full"),
        "{human}"
    );
    assert!(!human.contains("were fetched from the store"), "{human}");

    let calls = fs::read_to_string(format!("{}.calls", program.path().display())).unwrap();
    assert!(
        calls
            .lines()
            .filter(|line| line.contains("/message"))
            .all(|line| line.contains("limit=")),
        "{calls}"
    );
}

/// A window wide enough to reach a message the transport cannot carry still
/// renders every turn before it, and the human output names where and why the
/// read stopped.
#[test]
fn show_renders_what_precedes_a_message_the_transport_cannot_carry() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-oversized-giant-{}",
        std::process::id()
    )));
    let _program = opencode_program(root.path(), "opencode2");
    let output = tapes()
        .args(["show", "ses_giant_message_fixture", "--tail", "120"])
        .env("HOME", root.path().join("home"))
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("[assistant #99 "), "{text}");
    assert!(
        text.contains("Only the newest 100 messages were fetched from the store"),
        "{text}"
    );
    assert!(
        text.contains("is larger than the 8 MiB transport bound; the read stopped before it"),
        "{text}"
    );
}

/// Run tapes against an OpenCode store rooted at `root`, expecting success.
fn opencode_stdout(root: &Path, args: &[&str]) -> String {
    let output = tapes()
        .args(args)
        .env("HOME", root.join("home"))
        .env_remove("XDG_DATA_HOME")
        .env("PATH", root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A bounded read of a 1,200-message session stops at the 1,000-message
/// ceiling; `--full` streams every message, and the newest turns it streams
/// are the bounded read's turns, normalized the same way.
fn assert_opencode_full_read_passes_the_ceiling(bounded: &Value, full: &Value) {
    let bounded_turns = bounded["turns"].as_array().unwrap();
    assert_eq!(bounded_turns.len(), 1000);
    assert!(
        bounded["truncation"]["source"]
            .as_array()
            .unwrap()
            .contains(
                &serde_json::json!({"kind": "record-page", "records": 1000, "of": "messages"})
            ),
        "{}",
        bounded["truncation"]
    );

    let full_turns = full["turns"].as_array().unwrap();
    assert_eq!(full["read"]["coordinate_domain"], "opencode-message");
    assert_eq!(full["read"]["source_length"], 1200);
    assert_eq!(full["read"]["projection_options"][0], "full");
    let offset = full_turns.len() - bounded_turns.len();
    for (index, turn) in full_turns.iter().enumerate() {
        assert_eq!(turn["ordinal"], index);
    }
    for (bounded_turn, full_turn) in bounded_turns.iter().zip(&full_turns[offset..]) {
        for field in ["role", "kind", "text", "native_id", "ts", "parts"] {
            assert_eq!(bounded_turn[field], full_turn[field], "{field}");
        }
    }
}

/// `show --full` pages an OpenCode API session oldest first past the message
/// ceiling a bounded read keeps.
#[test]
fn show_full_streams_every_message_of_an_opencode_api_session() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-opencode-many-{}", std::process::id())),
    );
    let _program = opencode_program(root.path(), "opencode2");
    let id = "ses_many_fixture";
    let json = |args: &[&str]| -> Value {
        serde_json::from_str(&opencode_stdout(root.path(), args)).unwrap()
    };

    let bounded = json(&["show", id, "--tail", "5000", "--json"]);
    let full = json(&["show", id, "--full", "--json"]);
    assert_opencode_full_read_passes_the_ceiling(&bounded, &full);
    let texts = full["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["text"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let expected = (0..1200)
        .map(|index| match index % 2 {
            0 => format!("Request {index}"),
            _ => format!("Answer {index}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, expected);
    assert!(full.get("truncation").is_none(), "{}", full["truncation"]);

    let human = opencode_stdout(root.path(), &["show", id, "--full"]);
    assert!(human.contains("Request 0\n"), "{human}");
    assert!(
        human.contains("the whole recording was streamed; 1200 messages"),
        "{human}"
    );
}

/// Every distinct `sqlite3` on PATH. sqlite3 output modes differ between
/// versions, so a database test reads its store through each one installed
/// rather than only the first.
fn sqlite3_binaries() -> Vec<PathBuf> {
    let mut binaries: Vec<PathBuf> = Vec::new();
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap()) {
        let Ok(binary) = directory.join("sqlite3").canonicalize() else {
            continue;
        };
        if binary.is_file() && !binaries.contains(&binary) {
            binaries.push(binary);
        }
    }
    assert!(
        !binaries.is_empty(),
        "the OpenCode database tests drive a real sqlite3"
    );
    binaries
}

/// Make `sqlite3` under `root` the given binary, the one tapes runs when
/// `root` is its PATH.
fn use_sqlite3(root: &Path, sqlite3: &Path) {
    let link = root.join("sqlite3");
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(sqlite3, link).unwrap();
    eprintln!(
        "reading the OpenCode database through {}",
        sqlite3.display()
    );
}

/// A stable OpenCode store under `root` holding one session row and the rows
/// `inserts` adds. The message and part tables copy a real store's schema;
/// the session table holds the columns the reader selects.
fn create_opencode_database(root: &Path, id: &str, inserts: &str) {
    let store = root.join("home/.local/share/opencode");
    fs::create_dir_all(&store).unwrap();
    let schema = format!(
        r#"
CREATE TABLE `session` (`id` text PRIMARY KEY, `parent_id` text, `directory` text NOT NULL,
  `title` text NOT NULL, `model` text, `cost` real, `tokens_input` integer,
  `tokens_output` integer, `tokens_reasoning` integer, `tokens_cache_read` integer,
  `tokens_cache_write` integer, `time_created` integer NOT NULL, `time_updated` integer NOT NULL);
CREATE TABLE `message` (`id` text PRIMARY KEY, `session_id` text NOT NULL,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);
CREATE INDEX `message_session_time_created_id_idx` ON `message` (`session_id`,`time_created`,`id`);
CREATE TABLE `part` (`id` text PRIMARY KEY, `message_id` text NOT NULL, `session_id` text NOT NULL,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);
CREATE INDEX `part_session_idx` ON `part` (`session_id`);
CREATE INDEX `part_message_id_id_idx` ON `part` (`message_id`,`id`);
INSERT INTO session VALUES ('{id}', NULL, '/fixtures/database', 'Database fixture session',
  '{{"id":"fixture-db-model"}}', 0, 0, 0, 0, 0, 0, 1784663000000, 1784664200000);
{inserts}
"#
    );
    let mut create = Command::new(&sqlite3_binaries()[0])
        .arg(store.join("opencode.db"))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    create
        .stdin
        .take()
        .unwrap()
        .write_all(schema.as_bytes())
        .unwrap();
    assert!(create.wait().unwrap().success());
}

/// `show --full` pages a stable OpenCode database oldest first past the
/// message ceiling, through messages that share a creation time across a
/// page and a message whose parts span several part pages, and reports the
/// part text the projection still cuts.
#[test]
fn show_full_streams_every_message_of_an_opencode_database_session() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-database-full-{}",
        std::process::id()
    )));
    let _program = opencode_program(root.path(), "opencode");
    let id = "ses_many_database_fixture";
    // Every third message shares a creation time, message 1 carries 300
    // parts of one creation time, and the last part is longer than the
    // projection keeps.
    let inserts = r#"
WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 1199)
INSERT INTO message SELECT printf('msg_many_%04d', i), 'ses_many_database_fixture',
  1784663000000 + (i / 3) * 1000, 1784663000000 + (i / 3) * 1000,
  json_object('role', CASE WHEN i % 2 = 0 THEN 'user' ELSE 'assistant' END,
              'time', json_object('created', 1784663000000 + (i / 3) * 1000))
FROM n;
WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 1199)
INSERT INTO part SELECT printf('prt_many_%04d', i), printf('msg_many_%04d', i),
  'ses_many_database_fixture', 1784663000000 + (i / 3) * 1000, 1784663000000 + (i / 3) * 1000,
  json_object('type', 'text',
              'text', CASE WHEN i % 2 = 0 THEN printf('Request %d', i) ELSE printf('Answer %d', i) END
                      || CASE WHEN i = 1199 THEN ' ' || replace(hex(zeroblob(4500)), '00', 'x') ELSE '' END,
              'time', json_object('start', 1784663000000 + (i / 3) * 1000))
FROM n WHERE i <> 1;
WITH RECURSIVE n(j) AS (SELECT 0 UNION ALL SELECT j + 1 FROM n WHERE j < 299)
INSERT INTO part SELECT printf('prt_many_0001_%03d', j), 'msg_many_0001',
  'ses_many_database_fixture', 1784663000000, 1784663000000,
  json_object('type', 'text', 'text', printf('Answer 1 part %03d', j),
              'time', json_object('start', 1784663000000))
FROM n;
"#;
    create_opencode_database(root.path(), id, inserts);
    let json = |args: &[&str]| -> Value {
        serde_json::from_str(&opencode_stdout(root.path(), args)).unwrap()
    };
    let mut expected = Vec::new();
    for index in 0..1200 {
        match index {
            1 => expected.extend((0..300).map(|part| format!("Answer 1 part {part:03}"))),
            _ if index % 2 == 0 => expected.push(format!("Request {index}")),
            _ => expected.push(format!("Answer {index}")),
        }
    }
    let (expected_last, expected) = expected.split_last().unwrap();

    for sqlite3 in sqlite3_binaries() {
        use_sqlite3(root.path(), &sqlite3);
        let bounded = json(&["show", id, "--tail", "5000", "--json"]);
        let full = json(&["show", id, "--full", "--json"]);
        assert_opencode_full_read_passes_the_ceiling(&bounded, &full);
        let texts = full["turns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|turn| turn["text"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let (last, texts) = texts.split_last().unwrap();
        assert_eq!(texts, expected);
        assert!(last.starts_with(&format!("{expected_last} x")), "{last}");
        assert_eq!(last.chars().count(), 4000);
        assert_eq!(
            full["truncation"],
            serde_json::json!({"source": [{"kind": "turn-text", "turns": 1, "chars": 4000}]})
        );

        let human = opencode_stdout(root.path(), &["show", id, "--full", "--tail", "1"]);
        assert!(
            human.contains("1 turn carries text cut at 4000 characters"),
            "{human}"
        );
        assert!(
            human.contains("the whole recording was streamed; 1200 messages"),
            "{human}"
        );
    }
}

/// A bounded read of a stable OpenCode database whose newest 5,000 parts
/// together outgrow the 8 MiB transport bound returns that newest window and
/// reports both record ceilings, the same session a whole read pages through.
#[test]
fn show_reads_the_newest_parts_of_a_part_heavy_opencode_database_session() {
    let root = TemporaryDirectory::new(std::env::temp_dir().join(format!(
        "tapes-cli-opencode-database-part-heavy-{}",
        std::process::id()
    )));
    let _program = opencode_program(root.path(), "opencode");
    let id = "ses_part_heavy_database_fixture";
    // 1,200 assistant messages of six 2,000-character parts each: the newest
    // 1,000 messages hold 6,000 parts, and 5,001 projected rows are about
    // 10.4 MiB.
    let inserts = r#"
WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 1199)
INSERT INTO message SELECT printf('msg_heavy_%04d', i), 'ses_part_heavy_database_fixture',
  1784663000000 + i * 1000, 1784663000000 + i * 1000,
  json_object('role', 'assistant', 'time', json_object('created', 1784663000000 + i * 1000))
FROM n;
WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < 1199),
  m(j) AS (SELECT 0 UNION ALL SELECT j + 1 FROM m WHERE j < 5)
INSERT INTO part SELECT printf('prt_heavy_%04d_%d', i, j), printf('msg_heavy_%04d', i),
  'ses_part_heavy_database_fixture', 1784663000000 + i * 1000 + j, 1784663000000 + i * 1000 + j,
  json_object('type', 'text',
              'text', printf('Part %04d %d ', i, j) || replace(hex(zeroblob(2000)), '00', 'x'),
              'time', json_object('start', 1784663000000 + i * 1000 + j))
FROM n, m;
"#;
    create_opencode_database(root.path(), id, inserts);

    for sqlite3 in sqlite3_binaries() {
        use_sqlite3(root.path(), &sqlite3);
        let bounded: Value = serde_json::from_str(&opencode_stdout(
            root.path(),
            &["show", id, "--tail", "5000", "--json"],
        ))
        .unwrap();
        assert_eq!(
            bounded["truncation"]["source"],
            serde_json::json!([
                {"kind": "record-page", "records": 1000, "of": "messages"},
                {"kind": "record-page", "records": 5000, "of": "parts"}
            ])
        );
        let turns = bounded["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 5000);
        // The newest 5,000 parts are all of messages 367 to 1199 and the two
        // newest parts of message 366.
        assert!(
            turns[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("Part 0366 4 "),
            "{}",
            turns[0]["text"]
        );
        assert!(
            turns[4999]["text"]
                .as_str()
                .unwrap()
                .starts_with("Part 1199 5 "),
            "{}",
            turns[4999]["text"]
        );
    }
}

/// The normalized totals reach every surface unchanged: list, show, and the
/// export bundle carry the same counters, with absent ones omitted.
#[test]
fn codex_token_totals_agree_across_list_show_and_export() {
    let (codex_home, home) = fixture_store("codex-tokens");
    let id = "00000000-0000-0000-0000-000000000001";
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let expected = serde_json::json!({
        "input": 1200,
        "output": 300,
        "cache_read": 1000,
        "cache_write": 0
    });
    let expected_accounting = serde_json::json!({
        "basis": "recorded-total",
        "coverage": "session"
    });

    let shown: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
    assert_eq!(shown["session"]["tokens"], expected);
    assert_eq!(shown["session"]["accounting"], expected_accounting);
    assert!(shown["session"].get("cost").is_none());

    let listed: Value =
        serde_json::from_slice(&run(&["list", "--global", "--harness", "codex", "--json"]))
            .unwrap();
    assert_eq!(session(&listed, id)["tokens"], expected);
    assert_eq!(session(&listed, id)["accounting"], expected_accounting);

    let bundle = codex_home.join("bundle");
    let listing =
        String::from_utf8(run(&["export", id, "--bundle", bundle.to_str().unwrap()])).unwrap();
    let json_path = listing
        .lines()
        .find(|line| line.contains(".json"))
        .and_then(|line| line.split('\t').next())
        .unwrap();
    let exported: Value = serde_json::from_str(&fs::read_to_string(json_path).unwrap()).unwrap();
    assert_eq!(exported["session"]["tokens"], expected);
    assert_eq!(exported["session"]["accounting"], expected_accounting);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn list_sort_oldest_keeps_the_oldest_sessions_under_the_limit() {
    let (codex_home, home) = filter_fixture_store("sort-oldest-limit");
    let mut command = tapes();
    command.args([
        "list",
        "--global",
        "--harness",
        "codex",
        "--sort",
        "oldest",
        "--limit",
        "1",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["sessions"][0]["id"],
        "00000000-0000-0000-0000-000000000001"
    );

    fs::remove_dir_all(codex_home).unwrap();
}

const CLAUDE_SESSION: &str = include_str!("../fixtures/claude/project/session-claude.jsonl");
const PI_SESSION: &str = include_str!("../fixtures/pi/2026-01-01T10-00-00-000Z_session-pi.jsonl");
const CLAUDE_SUBAGENT: &str =
    include_str!("../fixtures/claude/project/session-claude/subagents/agent-fixture.jsonl");
const CLAUDE_SUBAGENT_META: &str =
    include_str!("../fixtures/claude/project/session-claude/subagents/agent-fixture.meta.json");

/// `show --full` reads a Claude recording past the bounded tail and writes
/// every turn from the recording's first; a flag it cannot honor refuses.
#[test]
fn show_full_streams_a_claude_recording_past_the_read_bound() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-full-read-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    let record = |uuid: &str, role: &str, content: Value| {
        serde_json::json!({
            "type": role,
            "sessionId": "full-claude",
            "uuid": uuid,
            "timestamp": "2026-01-01T10:00:00Z",
            "cwd": "/fixtures/project",
            "message": {"role": role, "content": content}
        })
        .to_string()
    };
    let filler = "x".repeat(100 * 1024);
    let mut body = record("first", "user", Value::from("opening request")) + "\n";
    for index in 0..48 {
        body += &record(
            &format!("filler-{index}"),
            "assistant",
            serde_json::json!([{"type": "text", "text": filler}]),
        );
        body.push('\n');
    }
    body += &record("last", "user", Value::from("closing request"));
    body.push('\n');
    fs::write(project.join("full-claude.jsonl"), &body).unwrap();
    let codex_home = root.path().join("codex");
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    fs::write(
        codex_sessions
            .join("rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
        CODEX_SESSION_ONE,
    )
    .unwrap();
    let run = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };
    let stdout = |arguments: &[&str]| {
        let output = run(arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let bounded = stdout(&["show", "full-claude"]);
    assert!(!bounded.contains("opening request"));
    assert!(bounded.contains("closing request"));

    let full = stdout(&["show", "full-claude", "--full"]);
    assert!(full.contains(" #0 2026-01-01T10:00:00Z]\nopening request"));
    assert!(full.contains(" #49 2026-01-01T10:00:00Z]\nclosing request"));
    assert!(full.contains("closing request"));
    assert!(full.contains("the whole recording was streamed"));
    assert!(!full.contains("Showing the last"));

    let tailed = stdout(&["show", "full-claude", "--full", "--tail", "1"]);
    assert!(tailed.contains("closing request"), "{tailed}");
    assert!(!tailed.contains("opening request"));
    assert!(
        tailed.contains("Showing the last 1 of 50 turns"),
        "{tailed}"
    );

    let json: Value =
        serde_json::from_str(&stdout(&["show", "full-claude", "--full", "--json"])).unwrap();
    assert_eq!(json["schema"], "tapes-session/10");
    assert_eq!(json["turns"].as_array().unwrap().len(), 50);
    assert_eq!(json["turns"][0]["text"], "opening request");
    assert_eq!(json["turns"][49]["ordinal"], 49);
    assert_eq!(json["read"]["source_length"], body.len() as u64);
    assert_eq!(json["read"]["projection_options"][0], "full");
    // The read names the revision it opened at, the one every turn cites.
    assert_eq!(
        json["read"]["source_revision"], json["turns"][0]["record_ref"]["revision"],
        "{}",
        json["read"]
    );
    assert!(json["read"]["source_revision"].is_string());
    assert!(json.get("truncation").is_none(), "{}", json["truncation"]);
    let tailed: Value = serde_json::from_str(&stdout(&[
        "show",
        "full-claude",
        "--full",
        "--json",
        "--tail",
        "2",
    ]))
    .unwrap();
    assert_eq!(tailed["turns"].as_array().unwrap().len(), 2);
    assert_eq!(tailed["truncation"]["window"]["omitted"], 48);
    assert!(
        !run(&["show", "full-claude", "--full", "--read-bytes", "1m"])
            .status
            .success()
    );
}

/// A whole read streams a Codex or Pi recording past the read bound. Pi needs
/// facts from across the file to project a turn, keeping only the last
/// entry's branch, and gets that right while it streams.
#[test]
fn show_full_streams_codex_and_pi_recordings_past_the_read_bound() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-full-codex-pi-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let codex_home = root.path().join("codex");
    let filler = "x".repeat(100 * 1024);
    let timestamp = "2026-01-01T10:00:00Z";

    let codex_id = "00000000-0000-0000-0000-0000000000f1";
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    let codex_user = |text: &str| {
        format!(
            "{}\n",
            serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}})
        )
    };
    let mut codex = serde_json::json!({"timestamp": timestamp, "type": "session_meta", "payload": {"id": codex_id, "session_id": codex_id, "timestamp": timestamp, "cwd": "/fixtures/project", "source": "cli", "model_provider": "openai"}}).to_string() + "\n";
    codex += &codex_user("opening request");
    for _ in 0..48 {
        codex += &serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": filler}]}}).to_string();
        codex.push('\n');
    }
    codex += &codex_user("closing request");
    fs::write(
        codex_sessions.join(format!("rollout-2026-01-01T10-00-00-{codex_id}.jsonl")),
        codex,
    )
    .unwrap();

    let pi_sessions = home.join(".pi/agent/sessions/--fixtures-project--");
    fs::create_dir_all(&pi_sessions).unwrap();
    let pi_message = |id: &str, parent: Option<&str>, role: &str, text: &str| {
        serde_json::json!({"type": "message", "id": id, "parentId": parent, "timestamp": timestamp, "message": {"role": role, "timestamp": 1767261600000u64, "content": [{"type": "text", "text": text}]}}).to_string() + "\n"
    };
    let mut pi = serde_json::json!({"type": "session", "version": 3, "id": "pi-full", "timestamp": timestamp, "cwd": "/fixtures/project"}).to_string() + "\n";
    pi += &pi_message("m0", None, "user", "opening request");
    pi += &pi_message("abandoned", Some("m0"), "assistant", "abandoned answer");
    let mut parent = "m0".to_owned();
    for index in 1..=48 {
        let id = format!("m{index}");
        pi += &pi_message(&id, Some(&parent), "assistant", &filler);
        parent = id;
    }
    pi += &pi_message("last", Some(&parent), "user", "closing request");
    fs::write(
        pi_sessions.join("2026-01-01T10-00-00-000Z_pi-full.jsonl"),
        pi,
    )
    .unwrap();

    let show = |arguments: &[&str]| {
        let output = tapes()
            .args(arguments)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let bounded = show(&["show", codex_id]);
    assert!(!bounded.contains("opening request"));
    let full = show(&["show", codex_id, "--full"]);
    assert!(
        full.contains("[user #0 2026-01-01T10:00:00Z]\nopening request"),
        "{}",
        &full[..full.len().min(300)]
    );
    assert!(full.contains("[user #49 2026-01-01T10:00:00Z]\nclosing request"));
    assert!(full.contains("the whole recording was streamed"));

    let bounded = show(&["show", "pi-full"]);
    assert!(!bounded.contains("opening request"));
    let full = show(&["show", "pi-full", "--full"]);
    assert!(full.contains("opening request"));
    assert!(full.contains("closing request"));
    assert!(!full.contains("abandoned answer"));
    assert!(
        full.contains("1 entry belongs to an abandoned branch."),
        "{}",
        &full[full.len().saturating_sub(600)..]
    );
}

/// `export --full` bundles a whole Claude or Codex recording past the read
/// bound: all three files hold every turn, the JSON turns are the ones `show
/// --full --json` writes, and its events are the ones `events` pairs over a
/// read wide enough to hold the file. `--omit` narrows all three, a bulk export
/// streams each selected session, and what `--full` cannot honor refuses.
#[test]
fn export_full_bundles_claude_and_codex_recordings_past_the_read_bound() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-export-full-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let codex_home = root.path().join("codex");
    let filler = "x".repeat(100 * 1024);
    let timestamp = "2026-01-01T10:00:00Z";

    let claude_id = "export-full-claude";
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    // A user record Claude vouches for as typed is an operator turn, which the
    // exchange in `.context.md` holds.
    let claude = |uuid: &str, role: &str, content: Value| {
        let mut record = serde_json::json!({"type": role, "sessionId": claude_id, "uuid": uuid, "timestamp": timestamp, "cwd": "/fixtures/project", "message": {"role": role, "content": content}});
        if role == "user" && record["message"]["content"].is_string() {
            record["promptSource"] = Value::from("typed");
        }
        record.to_string() + "\n"
    };
    let mut body = claude("first", "user", Value::from("opening request"))
        + &claude(
            "call",
            "assistant",
            serde_json::json!([{"type": "tool_use", "id": "tool-early", "name": "fixture_tool", "input": {"path": "early"}}]),
        )
        + &claude(
            "result",
            "user",
            serde_json::json!([{"type": "tool_result", "tool_use_id": "tool-early", "content": "early result"}]),
        );
    for index in 0..48 {
        body += &claude(
            &format!("filler-{index}"),
            "assistant",
            serde_json::json!([{"type": "text", "text": filler}]),
        );
    }
    body += &claude("last", "user", Value::from("closing request"));
    fs::write(project.join(format!("{claude_id}.jsonl")), &body).unwrap();

    let codex_id = "00000000-0000-0000-0000-0000000000f2";
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    let codex_item = |payload: Value| {
        serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": payload})
            .to_string()
            + "\n"
    };
    let codex_user = |text: &str| {
        codex_item(
            serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}),
        )
    };
    let mut codex = serde_json::json!({"timestamp": timestamp, "type": "session_meta", "payload": {"id": codex_id, "session_id": codex_id, "timestamp": timestamp, "cwd": "/fixtures/project", "source": "cli", "model_provider": "openai"}}).to_string() + "\n";
    codex += &codex_user("opening request");
    codex += &codex_item(
        serde_json::json!({"type": "function_call", "name": "fixture_tool", "arguments": "{\"path\":\"early\"}", "call_id": "call-early"}),
    );
    codex += &codex_item(
        serde_json::json!({"type": "function_call_output", "call_id": "call-early", "output": "early result"}),
    );
    for _ in 0..48 {
        codex += &codex_item(
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": filler}]}),
        );
    }
    codex += &codex_user("closing request");
    fs::write(
        codex_sessions.join(format!("rollout-2026-01-01T10-00-00-{codex_id}.jsonl")),
        codex,
    )
    .unwrap();

    let run = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };
    let stdout = |arguments: &[&str]| {
        let output = run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let bundle = |directory: &Path, id: &str| {
        let file = |suffix: &str| {
            let path = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    let name = path.to_string_lossy();
                    name.contains(id) && name.ends_with(suffix)
                })
                .unwrap_or_else(|| panic!("no {suffix} for {id} in {}", directory.display()));
            fs::read_to_string(path).unwrap()
        };
        let json: Value = serde_json::from_str(&file(".json")).unwrap();
        (file(".context.md"), json, file(".trace.md"))
    };

    for id in [claude_id, codex_id] {
        let directory = root.path().join(format!("bounded-{id}"));
        stdout(&["export", id, "--bundle", directory.to_str().unwrap()]);
        let (context, _, _) = bundle(&directory, id);
        // A turn's text is followed by a blank line; the derived title in the
        // header names the opening request too.
        assert!(!context.contains("opening request\n\n"), "{id}");

        let directory = root.path().join(format!("full-{id}"));
        let manifest = stdout(&[
            "export",
            id,
            "--full",
            "--bundle",
            directory.to_str().unwrap(),
        ]);
        assert_eq!(manifest.lines().count(), 3, "{manifest}");
        let (context, json, trace) = bundle(&directory, id);
        for file in [&context, &trace] {
            assert!(file.contains("opening request\n\n"), "{id}");
            assert!(file.contains("closing request"), "{id}");
        }
        assert!(
            context.contains("- source read: the whole recording was streamed;"),
            "{context:.600}"
        );
        assert!(trace.contains("## tool: fixture_tool"), "{id}");
        let shown: Value =
            serde_json::from_str(&stdout(&["show", id, "--full", "--json"])).unwrap();
        assert!(shown["turns"].as_array().unwrap().len() > 50, "{id}");
        assert_eq!(json["turns"], shown["turns"], "{id}");
        assert_eq!(json["read"]["projection_options"][0], "full");
        let events: Value =
            serde_json::from_str(&stdout(&["events", id, "--read-bytes", "64m", "--json"]))
                .unwrap();
        assert!(
            events["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["pair"].is_object()),
            "{id}: {}",
            events["events"]
        );
        assert_eq!(json["events"], events["events"], "{id}");

        let directory = root.path().join(format!("omit-{id}"));
        stdout(&[
            "export",
            id,
            "--full",
            "--omit",
            "tool",
            "--bundle",
            directory.to_str().unwrap(),
        ]);
        let (context, json, trace) = bundle(&directory, id);
        assert!(
            json["turns"]
                .as_array()
                .unwrap()
                .iter()
                .all(|turn| turn["kind"] != "tool"),
            "{id}"
        );
        assert!(
            json["projection"]["omitted"]["tool"].as_u64() > Some(0),
            "{id}"
        );
        assert!(!trace.contains("## tool:"), "{id}");
        assert!(trace.contains("opening request"), "{id}");
        assert!(context.contains("- projection: kept"), "{id}");
        assert!(context.contains("closing request"), "{id}");
    }

    let directory = root.path().join("bulk");
    stdout(&[
        "export",
        "--global",
        "--harness",
        "claude",
        "--full",
        "--bundle",
        directory.to_str().unwrap(),
    ]);
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(directory.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(
        manifest["sessions"].as_array().unwrap().len(),
        1,
        "{manifest}"
    );
    assert_eq!(manifest["failed"], serde_json::json!([]));
    let (context, _, _) = bundle(&directory, claude_id);
    assert!(context.contains("opening request"));

    let conflicting = run(&["export", claude_id, "--full", "--read-bytes", "1m"]);
    assert_eq!(conflicting.status.code(), Some(2));
    let input = supplied_fixture("chatgpt-export.json");
    let supplied = run(&[
        "export",
        "supplied-1",
        "--full",
        "--input",
        input.to_str().unwrap(),
        "--input-format",
        "chatgpt-exporter",
    ]);
    assert!(!supplied.status.success());
    assert!(
        String::from_utf8_lossy(&supplied.stderr).contains("--scan-bytes"),
        "{}",
        String::from_utf8_lossy(&supplied.stderr)
    );

    let _opencode = opencode_program(root.path(), "opencode2");
    let refused = root.path().join("opencode");
    let mut opencode = tapes();
    opencode.args([
        "export",
        "ses_000000fixtureSharedSession",
        "--full",
        "--bundle",
        refused.to_str().unwrap(),
    ]);
    with_fixture_env(&mut opencode, &codex_home, &home, root.path());
    let opencode = opencode.output().unwrap();
    assert!(!opencode.status.success());
    assert!(
        String::from_utf8_lossy(&opencode.stderr)
            .contains("opencode sessions cannot be read whole twice"),
        "{}",
        String::from_utf8_lossy(&opencode.stderr)
    );
    assert_eq!(
        fs::read_dir(&refused).map_or(0, |entries| entries.count()),
        0
    );
}

/// `events --full` and `stats --full` stream a Claude or Codex recording past
/// the read bound twice and answer what a bounded read wide enough to hold the
/// file answers, apart from its read evidence and text tail: the same events,
/// pairs, windows, and filters, a pair whose call falls outside the window
/// included, and the same counts with coverage `session`. A selection streams
/// each session, and what `--full` cannot honor refuses without writing.
#[test]
fn events_and_stats_full_stream_claude_and_codex_recordings_past_the_read_bound() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-events-full-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let codex_home = root.path().join("codex");
    let filler = "x".repeat(100 * 1024);
    let timestamp = |second: usize| format!("2026-01-01T10:{:02}:{:02}Z", second / 60, second % 60);

    let claude_id = "events-full-claude";
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    let claude = |second: usize, uuid: &str, role: &str, content: Value| {
        serde_json::json!({"type": role, "sessionId": claude_id, "uuid": uuid, "timestamp": timestamp(second), "cwd": "/fixtures/project", "message": {"role": role, "content": content}}).to_string() + "\n"
    };
    let mut body = claude(0, "first", "user", Value::from("opening request"))
        + &claude(
            1,
            "call",
            "assistant",
            serde_json::json!([{"type": "tool_use", "id": "tool-early", "name": "fixture_tool", "input": {"path": "early"}}]),
        )
        + &claude(
            3,
            "result",
            "user",
            serde_json::json!([{"type": "tool_result", "tool_use_id": "tool-early", "content": "early result", "is_error": true}]),
        );
    for index in 0..48 {
        body += &claude(
            4 + index,
            &format!("filler-{index}"),
            "assistant",
            serde_json::json!([{"type": "text", "text": filler}]),
        );
    }
    body += &claude(60, "last", "user", Value::from("closing request"));
    fs::write(project.join(format!("{claude_id}.jsonl")), &body).unwrap();

    let codex_id = "00000000-0000-0000-0000-0000000000f3";
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    let codex_item = |second: usize, payload: Value| {
        serde_json::json!({"timestamp": timestamp(second), "type": "response_item", "payload": payload})
            .to_string()
            + "\n"
    };
    let codex_user = |second: usize, text: &str| {
        codex_item(
            second,
            serde_json::json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}),
        )
    };
    let mut codex = serde_json::json!({"timestamp": timestamp(0), "type": "session_meta", "payload": {"id": codex_id, "session_id": codex_id, "timestamp": timestamp(0), "cwd": "/fixtures/project", "source": "cli", "model_provider": "openai"}}).to_string() + "\n";
    codex += &codex_user(0, "opening request");
    codex += &codex_item(
        1,
        serde_json::json!({"type": "function_call", "name": "fixture_tool", "arguments": "{\"path\":\"early\"}", "call_id": "call-early"}),
    );
    codex += &codex_item(
        2,
        serde_json::json!({"type": "function_call_output", "call_id": "call-early", "output": "early result"}),
    );
    // A call whose result lands after the filler, so a narrow window holds
    // the result and not the call that declares its program.
    codex += &codex_item(
        3,
        serde_json::json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"git status\"}", "call_id": "call-late"}),
    );
    for index in 0..48 {
        codex += &codex_item(
            4 + index,
            serde_json::json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": filler}]}),
        );
    }
    codex += &codex_item(
        58,
        serde_json::json!({"type": "function_call_output", "call_id": "call-late", "output": "clean"}),
    );
    codex += &codex_user(60, "closing request");
    fs::write(
        codex_sessions.join(format!("rollout-2026-01-01T10-00-00-{codex_id}.jsonl")),
        &codex,
    )
    .unwrap();

    let run = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };
    let json = |arguments: &[&str]| -> Value {
        let output = run(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    // What a whole read and a bounded read wide enough to hold the file share.
    let comparable = |mut value: Value| {
        let object = value.as_object_mut().unwrap();
        object.remove("read");
        object.remove("text_tail");
        value
    };

    for (id, length) in [(claude_id, body.len()), (codex_id, codex.len())] {
        let bounded = json(&["events", id, "--json"]);
        assert_eq!(
            bounded["truncation"]["source"][0]["kind"], "file-tail",
            "{id}"
        );
        for selection in [
            &[][..],
            &["--name", "fixture_tool"],
            &["--call-id", "call-early"],
            &["--tail", "2"],
        ] {
            let full = json(&[&["events", id, "--full", "--json"], selection].concat());
            let wide =
                json(&[&["events", id, "--read-bytes", "64m", "--json"], selection].concat());
            assert_eq!(full["read"]["projection_options"][0], "full", "{id}");
            assert_eq!(full["read"]["source_length"], length as u64, "{id}");
            assert_eq!(comparable(full), comparable(wide), "{id} {selection:?}");
        }
        let full = json(&["events", id, "--full", "--name", "fixture_tool", "--json"]);
        assert_eq!(full["events"].as_array().unwrap().len(), 1, "{id}: {full}");
        assert_eq!(
            full["pairs"],
            serde_json::json!({"complete": 1, "incomplete": 0})
        );

        let full = json(&["stats", id, "--full", "--json"]);
        let wide = json(&["stats", id, "--read-bytes", "64m", "--json"]);
        assert_eq!(full["coverage"]["turns"], "session", "{id}: {full}");
        assert_eq!(
            full["tools"]["paired"].as_u64(),
            wide["tools"]["paired"].as_u64()
        );
        assert!(full["tools"]["paired"].as_u64() > Some(0), "{id}: {full}");
        assert_eq!(full["read"]["projection_options"][0], "full", "{id}");
        assert_eq!(comparable(full), comparable(wide), "{id}");
        let bounded = json(&["stats", id, "--json"]);
        assert_eq!(bounded["coverage"]["turns"], "read-window", "{id}");
    }

    // The late call declares `git`; its result shares the call id. A window
    // holding only the result selects nothing, as a bounded read does.
    for (selection, events) in [
        (&["--program", "git"][..], 2),
        (&["--program", "git", "--tail", "2"], 0),
    ] {
        let full = json(&[&["events", codex_id, "--full", "--json"], selection].concat());
        let wide = json(
            &[
                &["events", codex_id, "--read-bytes", "64m", "--json"],
                selection,
            ]
            .concat(),
        );
        assert_eq!(
            full["events"].as_array().unwrap().len(),
            events,
            "{selection:?}: {full}"
        );
        assert_eq!(comparable(full), comparable(wide), "{selection:?}");
    }
    let window = json(&["events", codex_id, "--full", "--tail", "2", "--json"]);
    assert_eq!(window["events"][0]["call_id"], "call-late", "{window}");
    assert!(window["events"][0]["pair"].is_object(), "{window}");
    assert_eq!(
        window["pairs"],
        serde_json::json!({"complete": 1, "incomplete": 0})
    );

    let human = run(&["events", claude_id, "--full", "--tail", "2"]);
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("Showing the last 2 of 52 turns"), "{human}");
    assert!(
        human.contains("the whole recording was streamed twice; source length"),
        "{human}"
    );

    let summary = json(&[
        "stats",
        "--global",
        "--harness",
        "claude",
        "--full",
        "--json",
    ]);
    assert_eq!(summary["read"], 1, "{summary}");
    let one = json(&["stats", claude_id, "--full", "--json"]);
    assert_eq!(summary["sessions"][0]["tools"], one["tools"]);
    assert_eq!(summary["sessions"][0]["coverage"], one["coverage"]);
    // Claude can record every kind, so each kind the one session lacks is
    // one the summary reports no session held.
    let claude = &summary["kinds_by_harness"]["claude"];
    assert_eq!(claude["declared"], one["kinds"], "{summary}");
    assert_eq!(claude["turns"], one["turns"], "{summary}");
    let unobserved = [
        "operator",
        "assistant",
        "reasoning",
        "tool",
        "control",
        "ambient",
        "notice",
        "unknown",
    ]
    .into_iter()
    .filter(|kind| one["turns"][kind] == 0)
    .collect::<Vec<_>>();
    assert!(!unobserved.is_empty());
    assert_eq!(
        claude["unobserved"],
        serde_json::json!(unobserved),
        "{summary}"
    );

    for command in ["events", "stats"] {
        let conflicting = run(&[command, claude_id, "--full", "--read-bytes", "1m"]);
        assert_eq!(conflicting.status.code(), Some(2), "{command}");
        let input = supplied_fixture("chatgpt-export.json");
        let supplied = run(&[
            command,
            "supplied-1",
            "--full",
            "--input",
            input.to_str().unwrap(),
            "--input-format",
            "chatgpt-exporter",
        ]);
        assert!(!supplied.status.success(), "{command}");
        assert!(
            String::from_utf8_lossy(&supplied.stderr).contains("--scan-bytes"),
            "{command}: {}",
            String::from_utf8_lossy(&supplied.stderr)
        );

        let _opencode = opencode_program(root.path(), "opencode2");
        let mut opencode = tapes();
        opencode.args([
            command,
            "ses_000000fixtureSharedSession",
            "--full",
            "--json",
        ]);
        with_fixture_env(&mut opencode, &codex_home, &home, root.path());
        let opencode = opencode.output().unwrap();
        assert!(!opencode.status.success(), "{command}");
        assert!(
            String::from_utf8_lossy(&opencode.stderr)
                .contains("opencode sessions cannot be read whole twice"),
            "{command}: {}",
            String::from_utf8_lossy(&opencode.stderr)
        );
        assert!(opencode.stdout.is_empty(), "{command}");
    }
}

/// Every turn says what it is, and a user turn the harness recorded its own
/// command in says so in the heading a reader judges an ending by.
#[test]
fn show_types_every_turn_and_names_a_control_turn_in_human_output() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-turn-kinds-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    let pi_sessions = home.join(".pi/agent/sessions");
    fs::create_dir_all(&pi_sessions).unwrap();
    fs::write(
        pi_sessions.join("2026-01-01T10-00-00-000Z_session-pi.jsonl"),
        PI_SESSION,
    )
    .unwrap();
    let codex_home = root.path().join("codex");
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    fs::write(
        codex_sessions
            .join("rollout-2026-01-01T10-00-00-00000000-0000-0000-0000-000000000001.jsonl"),
        CODEX_SESSION_ONE,
    )
    .unwrap();

    let _opencode = opencode_program(root.path(), "opencode2");

    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(&mut command, &codex_home, &home, root.path());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    for id in [
        "session-claude",
        "session-pi",
        "00000000-0000-0000-0000-000000000001",
        "ses_000000fixtureSharedSession",
    ] {
        let value: Value = serde_json::from_slice(&run(&["show", id, "--json"])).unwrap();
        let turns = value["turns"].as_array().unwrap();
        assert!(!turns.is_empty(), "{id} has no turns");
        for turn in turns {
            let kind = turn["kind"].as_str().unwrap_or_else(|| panic!("{turn}"));
            assert!(
                [
                    "operator",
                    "assistant",
                    "reasoning",
                    "tool",
                    "control",
                    "ambient",
                    "notice",
                    "unknown"
                ]
                .contains(&kind),
                "{id} carries an unnamed kind: {turn}"
            );
            let role = turn["role"].as_str().unwrap();
            if role != "user" {
                assert_eq!(kind, role, "{id}: {turn}");
            }
        }
    }

    let human = String::from_utf8(run(&["show", "session-claude"])).unwrap();
    assert!(human.contains("[user/control #10 "), "{human}");
    assert!(human.contains("[user/notice #12 "), "{human}");
    assert!(human.contains("[user #5 "), "{human}");
}

#[test]
fn lineage_help_names_the_schema_and_the_rule_it_follows() {
    let output = tapes().args(["lineage", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("tapes-lineage/2"), "{help}");
    assert!(
        help.contains("A relationship exists only where a record states it"),
        "{help}"
    );
    assert!(help.contains("referred to, never"), "{help}");
}

/// The command reports the relationships a store recorded, keeps a reference
/// the store cannot resolve, and never reads a child's transcript.
#[test]
fn lineage_reports_a_subagent_reference_and_an_unresolved_parent() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-lineage-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    let subagents = project.join("session-claude/subagents");
    fs::create_dir_all(&subagents).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    fs::write(subagents.join("agent-fixture.jsonl"), CLAUDE_SUBAGENT).unwrap();
    fs::write(
        subagents.join("agent-fixture.meta.json"),
        CLAUDE_SUBAGENT_META,
    )
    .unwrap();
    let pi_sessions = home.join(".pi/agent/sessions");
    fs::create_dir_all(&pi_sessions).unwrap();
    fs::write(
        pi_sessions.join("2026-01-01T10-00-00-000Z_session-pi.jsonl"),
        PI_SESSION,
    )
    .unwrap();
    let codex_home = root.path().join("codex");
    fs::create_dir_all(codex_home.join("sessions")).unwrap();

    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value =
        serde_json::from_slice(&run(&["lineage", "session-claude", "--json"])).unwrap();
    assert_eq!(value["schema"], "tapes-lineage/2");
    assert_eq!(value["session"]["id"], "session-claude");
    let children = value["lineage"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "{value}");
    assert_eq!(children[0]["reference"], "fixture");
    assert_eq!(children[0]["role"], "Explore");
    assert_eq!(children[0]["disposition"], "completed");
    assert_eq!(children[0]["resolved"], true);
    assert!(value["lineage"].get("parent").is_none(), "{value}");
    // The parent refers to the child; nothing the child recorded is in here.
    assert!(
        !String::from_utf8(run(&["lineage", "session-claude", "--json"]))
            .unwrap()
            .contains("Subagent fixture."),
        "{value}"
    );

    let human = String::from_utf8(run(&["lineage", "session-claude"])).unwrap();
    assert!(human.contains("child: Explore fixture"), "{human}");
    assert!(human.contains("status completed"), "{human}");

    let value: Value = serde_json::from_slice(&run(&["lineage", "session-pi", "--json"])).unwrap();
    assert_eq!(value["lineage"]["parent"]["native_id"], "session-pi-parent");
    assert_eq!(value["lineage"]["parent"]["resolved"], false);
    assert_eq!(value["lineage"]["children"].as_array().unwrap().len(), 0);

    let human = String::from_utf8(run(&["lineage", "session-pi"])).unwrap();
    assert!(human.contains("parent: session-pi-parent"), "{human}");
    assert!(human.contains("unresolved"), "{human}");
}

/// A spawn recorded before the tail window is outside a bounded lineage read;
/// `lineage --full` streams the whole recording and names it.
#[test]
fn lineage_full_names_a_child_reference_recorded_before_the_tail_window() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-lineage-full-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    let timestamp = "2026-01-01T10:00:00Z";
    let filler = "x".repeat(100 * 1024);
    let claude_record = |uuid: &str, role: &str, extra: Value, content: Value| {
        let mut record = serde_json::json!({"type": role, "sessionId": "lineage-full", "uuid": uuid, "timestamp": timestamp, "cwd": "/fixtures/project", "message": {"role": role, "content": content}});
        if let Value::Object(extra) = extra {
            record.as_object_mut().unwrap().extend(extra);
        }
        record.to_string() + "\n"
    };
    let mut claude = claude_record(
        "spawn",
        "assistant",
        Value::Null,
        serde_json::json!([{"type": "tool_use", "id": "tool-early", "name": "Agent", "input": {"subagent_type": "Explore"}}]),
    );
    claude += &claude_record(
        "spawned",
        "user",
        serde_json::json!({"toolUseResult": {"status": "completed", "agentId": "early-agent", "agentType": "Explore"}}),
        serde_json::json!([{"type": "tool_result", "tool_use_id": "tool-early", "content": "done"}]),
    );
    for index in 0..48 {
        claude += &claude_record(
            &format!("filler-{index}"),
            "assistant",
            Value::Null,
            serde_json::json!([{"type": "text", "text": filler}]),
        );
    }
    fs::write(project.join("lineage-full.jsonl"), &claude).unwrap();

    let codex_home = root.path().join("codex");
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    let codex_id = "00000000-0000-0000-0000-0000000000f3";
    let mut codex = serde_json::json!({"timestamp": timestamp, "type": "session_meta", "payload": {"id": codex_id, "timestamp": timestamp, "cwd": "/fixtures/project", "source": "cli"}}).to_string() + "\n";
    codex += &serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type": "function_call", "name": "spawn_agent", "call_id": "call-spawn", "arguments": "{\"task_name\":\"worker\"}"}}).to_string();
    codex.push('\n');
    codex += &serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type": "function_call_output", "call_id": "call-spawn", "output": "{\"task_name\":\"/root/worker\"}"}}).to_string();
    codex.push('\n');
    for _ in 0..48 {
        codex += &serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": filler}]}}).to_string();
        codex.push('\n');
    }
    fs::write(
        codex_sessions.join(format!("rollout-2026-01-01T10-00-00-{codex_id}.jsonl")),
        codex,
    )
    .unwrap();

    let run = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };
    let json = |arguments: &[&str]| -> Value {
        let output = run(arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let bounded = json(&["lineage", "lineage-full", "--json"]);
    assert_eq!(bounded["lineage"]["children"], serde_json::json!([]));
    assert_eq!(bounded["truncated"], true);
    let full = json(&["lineage", "lineage-full", "--full", "--json"]);
    let children = full["lineage"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "{full}");
    assert_eq!(children[0]["reference"], "early-agent");
    assert_eq!(children[0]["role"], "Explore");
    assert_eq!(children[0]["disposition"], "completed");
    assert_eq!(children[0]["resolved"], false);
    assert_eq!(full["truncated"], false);
    assert!(full.get("truncation").is_none(), "{full}");

    let bounded = json(&["lineage", codex_id, "--json"]);
    assert_eq!(bounded["lineage"]["children"], serde_json::json!([]));
    let full = json(&["lineage", codex_id, "--full", "--json"]);
    let children = full["lineage"]["children"].as_array().unwrap();
    assert_eq!(children.len(), 1, "{full}");
    assert_eq!(children[0]["reference"], "/root/worker");
    assert_eq!(children[0]["group"], "/root");
    assert_eq!(full["truncated"], false);

    assert!(
        !run(&["lineage", "lineage-full", "--full", "--read-bytes", "1m"])
            .status
            .success()
    );
    let refused = run(&[
        "lineage",
        "lineage-full",
        "--full",
        "--input",
        "/definitely/missing.json",
    ]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--scan-bytes"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn usage_help_names_the_schema_and_what_the_figures_mean() {
    let output = tapes().args(["usage", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("tapes-usage/5"), "{help}");
    assert!(
        help.contains("basis and coverage decide whether figures may be summed"),
        "{help}"
    );
    assert!(help.contains("quota is a separate fact"), "{help}");
    assert!(help.contains("tapes-usage-summary/3"), "{help}");
    assert!(help.contains("--by"), "{help}");
}

/// Neither form was asked for, so the error names both rather than guessing
/// which one was meant.
#[test]
fn usage_without_a_session_or_a_selection_names_both_forms() {
    let output = tapes().arg("usage").output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("--latest"), "{error}");
    assert!(error.contains("--global"), "{error}");
}

/// The usage view counts the same normalized turns `show` renders, by role.
#[test]
fn usage_json_reports_recorded_facts_and_turn_counts_show_agrees_with() {
    let id = "00000000-0000-0000-0000-000000000001";
    let output = fixture_command("usage-json", &["usage", id, "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-usage/5");
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["session"]["source"]["recorded_harness"], "codex");
    assert_eq!(
        value["tokens"],
        serde_json::json!({
            "input": 1200,
            "output": 300,
            "cache_read": 1000,
            "cache_write": 0
        })
    );
    assert_eq!(
        value["accounting"],
        serde_json::json!({ "basis": "recorded-total", "coverage": "session" })
    );
    assert_eq!(value["context_window"], 272_000);
    assert_eq!(value["rate_limits"]["primary"]["used_percent"], 12.0);
    assert_eq!(value["rate_limits"]["secondary"]["window_minutes"], 10_080);
    assert_eq!(
        value["rate_limits"]["primary"]["resets_at"],
        "2026-01-01T14:00:00Z"
    );
    assert_eq!(value["rate_limits"]["plan"], "fixture");
    assert_eq!(value["rate_limits"]["credits"]["balance"], "0");
    assert_eq!(value["rate_limits"]["credits"]["has_credits"], false);
    assert_eq!(value["rate_limits"]["spend_control_reached"], false);
    for absent in ["cost", "durations_ms", "by_model", "truncation"] {
        assert!(value.get(absent).is_none(), "{absent} in {value}");
    }

    let shown = fixture_command("usage-show", &["show", id, "--json"]);
    let shown: Value = serde_json::from_slice(&shown.stdout).unwrap();
    let turns = shown["turns"].as_array().unwrap();
    let counted = |role: &str| turns.iter().filter(|turn| turn["role"] == role).count();
    assert_eq!(value["turns"]["user"], counted("user"));
    assert_eq!(value["turns"]["assistant"], counted("assistant"));
    assert_eq!(value["turns"]["tool"], counted("tool"));
    assert_eq!(value["turns"]["reasoning"], counted("reasoning"));
    assert_eq!(value["turns"]["total"], turns.len());
    assert_eq!(value["turns"]["coverage"], "session");
}

/// `usage --full` counts every turn of a recording past the read bound and
/// folds the session's counters from every record: a Claude request sum
/// covers the whole session, and a Codex total recorded before the tail
/// window is still reported.
#[test]
fn usage_full_counts_every_turn_and_folds_the_whole_recordings_counters() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-usage-full-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    let timestamp = "2026-01-01T10:00:00Z";
    let filler = "x".repeat(100 * 1024);
    let claude_user = |uuid: &str, text: &str| {
        serde_json::json!({"type": "user", "sessionId": "usage-full", "uuid": uuid, "timestamp": timestamp, "cwd": "/fixtures/project", "message": {"role": "user", "content": text}}).to_string() + "\n"
    };
    let mut claude = claude_user("first", "opening request");
    for index in 0..48 {
        claude += &serde_json::json!({"type": "assistant", "sessionId": "usage-full", "uuid": format!("filler-{index}"), "requestId": format!("request-{index}"), "timestamp": timestamp, "cwd": "/fixtures/project", "message": {"role": "assistant", "model": "claude-fixture", "content": [{"type": "text", "text": filler}], "usage": {"input_tokens": 1, "output_tokens": 2}}}).to_string();
        claude.push('\n');
    }
    claude += &claude_user("last", "closing request");
    fs::write(project.join("usage-full.jsonl"), &claude).unwrap();

    let codex_home = root.path().join("codex");
    let codex_sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&codex_sessions).unwrap();
    let codex_id = "00000000-0000-0000-0000-0000000000f2";
    let codex_user = |text: &str| {
        serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}}).to_string() + "\n"
    };
    let mut codex = serde_json::json!({"timestamp": timestamp, "type": "session_meta", "payload": {"id": codex_id, "timestamp": timestamp, "cwd": "/fixtures/project", "source": "cli"}}).to_string() + "\n";
    codex += &codex_user("opening request");
    codex += &serde_json::json!({"timestamp": timestamp, "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": 500, "output_tokens": 20}, "model_context_window": 272000}}}).to_string();
    codex.push('\n');
    for _ in 0..48 {
        codex += &serde_json::json!({"timestamp": timestamp, "type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": filler}]}}).to_string();
        codex.push('\n');
    }
    codex += &codex_user("closing request");
    fs::write(
        codex_sessions.join(format!("rollout-2026-01-01T10-00-00-{codex_id}.jsonl")),
        &codex,
    )
    .unwrap();

    let run = |arguments: &[&str]| {
        tapes()
            .args(arguments)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("PATH", "/definitely/missing")
            .output()
            .unwrap()
    };
    let json = |arguments: &[&str]| -> Value {
        let output = run(arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let bounded = json(&["usage", "usage-full", "--json"]);
    assert_eq!(bounded["turns"]["coverage"], "read-window", "{bounded}");
    assert!(
        bounded["turns"]["total"].as_u64().unwrap() < 50,
        "{bounded}"
    );
    assert_eq!(bounded["accounting"]["coverage"], "read-window");

    let full = json(&["usage", "usage-full", "--full", "--json"]);
    assert_eq!(full["schema"], "tapes-usage/5");
    assert_eq!(full["turns"]["total"], 50, "{full}");
    assert_eq!(full["turns"]["user"], 2);
    assert_eq!(full["turns"]["assistant"], 48);
    assert_eq!(full["turns"]["coverage"], "session");
    assert_eq!(full["content"]["records"], 50);
    assert_eq!(
        full["tokens"],
        serde_json::json!({"input": 48, "output": 96})
    );
    assert_eq!(
        full["accounting"],
        serde_json::json!({"basis": "summed-requests", "coverage": "session"})
    );
    assert_eq!(full["session"]["model"]["id"], "claude-fixture");
    assert_eq!(full["read"]["source_length"], claude.len() as u64);
    assert_eq!(full["read"]["projection_options"][0], "full");
    assert_eq!(full["truncated"], false);
    assert!(full.get("truncation").is_none(), "{full}");

    let human = run(&["usage", "usage-full", "--full"]);
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("covering the whole session"), "{human}");
    assert!(
        human.contains("the whole recording was streamed; source length"),
        "{human}"
    );

    let bounded = json(&["usage", codex_id, "--json"]);
    assert!(bounded.get("tokens").is_none(), "{bounded}");
    let full = json(&["usage", codex_id, "--full", "--json"]);
    assert_eq!(full["turns"]["total"], 50, "{full}");
    assert_eq!(
        full["tokens"],
        serde_json::json!({"input": 500, "output": 20})
    );
    assert_eq!(full["context_window"], 272_000);

    let refused = run(&["usage", "usage-full", "--full", "--read-bytes", "1m"]);
    assert!(!refused.status.success());
    let refused = run(&["usage", "--global", "--full"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("one session"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let refused = run(&[
        "usage",
        "usage-full",
        "--full",
        "--input",
        "/definitely/missing.json",
    ]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--scan-bytes"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

/// Human output states each recorded fact once and invents no line for a
/// fact the harness did not record.
#[test]
fn a_claude_cost_state_below_the_recorded_requests_is_not_the_session_total() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-cost-state-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/-fixtures-project");
    fs::create_dir_all(&project).unwrap();
    let recording = |id: &str, cost_state: Value| {
        let user = serde_json::json!({"type": "user", "sessionId": id, "uuid": "user", "timestamp": "2026-01-01T10:00:00Z", "cwd": "/fixtures/project", "message": {"role": "user", "content": "Inspect the fixture."}});
        let assistant = serde_json::json!({"type": "assistant", "sessionId": id, "uuid": "assistant", "requestId": "request", "timestamp": "2026-01-01T10:00:01Z", "cwd": "/fixtures/project", "message": {"role": "assistant", "model": "claude-fixture", "content": [{"type": "text", "text": "Fixture inspected."}], "usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_input_tokens": 30, "cache_creation_input_tokens": 40, "output_tokens_details": {"thinking_tokens": 5}}}});
        let mut cost_state = cost_state;
        cost_state["type"] = Value::from("cost-state");
        cost_state["sessionId"] = Value::from(id);
        fs::write(
            project.join(format!("{id}.jsonl")),
            format!("{user}\n{assistant}\n{cost_state}\n"),
        )
        .unwrap();
    };
    recording(
        "zeroed-cost-state",
        serde_json::json!({"totalCostUSD": 0, "totalDuration": 1000, "modelUsage": {}}),
    );
    recording(
        "covering-cost-state",
        serde_json::json!({"totalCostUSD": 2.5, "modelUsage": {"claude-fixture": {"inputTokens": 100, "outputTokens": 200, "cacheReadInputTokens": 300, "cacheCreationInputTokens": 400}}}),
    );
    let usage = |id: &str, full: bool| -> Value {
        let mut command = tapes();
        command.args(["usage", id, "--json"]);
        if full {
            command.arg("--full");
        }
        with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    for full in [false, true] {
        let zeroed = usage("zeroed-cost-state", full);
        assert_eq!(zeroed["accounting"]["basis"], "summed-requests", "{zeroed}");
        assert_eq!(
            zeroed["tokens"],
            serde_json::json!({"input": 10, "output": 20, "reasoning": 5, "cache_read": 30, "cache_write": 40}),
            "{zeroed}"
        );
        assert!(zeroed.get("cost").is_none(), "{zeroed}");
        assert!(zeroed.get("durations_ms").is_none(), "{zeroed}");

        // A cost-state without a thinking count still covers the requests.
        let covering = usage("covering-cost-state", full);
        assert_eq!(
            covering["accounting"]["basis"], "recorded-total",
            "{covering}"
        );
        assert_eq!(covering["tokens"]["output"], 200, "{covering}");
        assert_eq!(covering["cost"]["usd"], 2.5, "{covering}");
    }
}

#[test]
fn usage_human_output_names_the_recorded_facts_only() {
    let id = "00000000-0000-0000-0000-000000000001";
    let output = fixture_command("usage-human", &["usage", id]);
    let rendered = String::from_utf8(output.stdout).unwrap();

    assert!(
        rendered.starts_with(
            "session: codex 00000000-0000-0000-0000-000000000001 gpt-fixture (high)\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("tokens: input 1200, output 300, cache read 1000, cache write 0\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("accounting: a recorded total, covering the whole session\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "turns: 6 total, 1 user, 1 assistant, 3 tool, 1 reasoning, covering the whole session\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("context window: 272000 tokens\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "rate limit primary: 12% used of a 300-minute window, resets 2026-01-01T14:00:00Z\n"
        ),
        "{rendered}"
    );
    assert!(rendered.contains("plan: fixture\n"), "{rendered}");
    for absent in ["cost:", "durations:", "model ", "Note:"] {
        assert!(!rendered.contains(absent), "{absent} in {rendered}");
    }
}

/// The summary sums the same counters the listing reports per session, and
/// `counted` says how many sessions were behind each sum.
#[test]
fn usage_summary_sums_the_listings_own_counters_and_counts_the_sessions_behind_them() {
    let (codex_home, home) = filter_fixture_store("usage-summary");
    let command = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        command.output().unwrap()
    };

    let listed = command(&["list", "--global", "--harness", "codex", "--json"]);
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let listed: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let counted_session = session(&listed, "00000000-0000-0000-0000-000000000001");

    let output = command(&[
        "usage",
        "--global",
        "--harness",
        "codex",
        "--by",
        "harness,model",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-usage-summary/3");
    assert_eq!(value["selection"]["scope"], "global");
    assert_eq!(value["selection"]["harness"], "codex");
    assert_eq!(value["scanned"], 4);
    assert_eq!(value["scan_truncated"], false);

    let groups = value["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    // A session that recorded no model is keyed by that absence.
    assert_eq!(groups[0]["key"], serde_json::json!({ "harness": "codex" }));
    assert_eq!(groups[0]["sessions"], 1);
    assert!(groups[0].get("tokens").is_none(), "{value}");

    let gpt = &groups[1];
    assert_eq!(
        gpt["key"],
        serde_json::json!({ "harness": "codex", "model": "gpt-fixture" })
    );
    assert_eq!(gpt["sessions"], 2);
    assert_eq!(gpt["tokens"], counted_session["tokens"]);
    assert_eq!(
        gpt["counted"],
        serde_json::json!({
            "input": 1,
            "output": 1,
            "reasoning": 0,
            "cache_read": 1,
            "cache_write": 1,
            "cost": 0
        })
    );
    assert_eq!(
        gpt["coverage"],
        serde_json::json!({
            "recorded_total": 1,
            "summed_session": 0,
            "summed_read_window": 0,
            "no_accounting": 1
        })
    );
    assert!(gpt.get("cost").is_none(), "{value}");

    assert_eq!(value["totals"]["sessions"], 4);
    assert_eq!(value["totals"]["tokens"], counted_session["tokens"]);
    assert_eq!(value["totals"]["counted"]["input"], 1);
    assert_eq!(value["totals"]["coverage"]["no_accounting"], 3);

    fs::remove_dir_all(codex_home).unwrap();
}

/// The activity window narrows the summed set the same way it narrows a
/// listing, and sessions whose harness recorded no counters are reported as
/// such rather than as zeroes.
#[test]
fn usage_summary_narrows_by_activity_and_reports_sessions_with_no_accounting() {
    let (codex_home, home) = filter_fixture_store("usage-window");
    let mut command = tapes();
    command.args([
        "usage",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T12:00:00Z",
        "--by",
        "model",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(
        value["selection"]["activity"],
        serde_json::json!({ "since": "2026-01-01T12:00:00Z" })
    );
    let groups = value["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["key"], serde_json::json!({}));
    assert_eq!(
        groups[1]["key"],
        serde_json::json!({ "model": "other-fixture" })
    );
    for group in groups {
        assert!(group.get("tokens").is_none(), "{group}");
        assert!(group.get("cost").is_none(), "{group}");
        assert_eq!(group["coverage"]["no_accounting"], 1);
    }
    assert_eq!(value["totals"]["sessions"], 2);
    assert_eq!(value["totals"]["coverage"]["no_accounting"], 2);
    assert!(value["totals"].get("tokens").is_none(), "{value}");

    fs::remove_dir_all(codex_home).unwrap();
}

/// The table prints one row per group and a closing total, and a sum drawn
/// from fewer sessions than the row holds says so.
#[test]
fn usage_summary_human_output_says_how_many_sessions_are_behind_each_sum() {
    let (codex_home, home) = filter_fixture_store("usage-table");
    let mut command = tapes();
    command.args(["usage", "--global", "--harness", "codex"]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    let rendered = String::from_utf8(output.stdout).unwrap();
    let lines = rendered.lines().collect::<Vec<_>>();

    assert_eq!(
        lines[0],
        "HARNESS\tMODEL\tSESSIONS\tINPUT\tOUTPUT\tREASONING\tCACHE READ\tCACHE WRITE\tCOST\t\
         COVERAGE (RECORDED/SUMMED/WINDOW/NONE)"
    );
    assert_eq!(
        lines[2],
        "codex\tgpt-fixture\t2\t1200 (1 of 2)\t300 (1 of 2)\t\t1000 (1 of 2)\t0 (1 of 2)\t\t1/0/0/1"
    );
    assert_eq!(
        lines[4],
        "TOTAL\t\t4\t1200 (1 of 4)\t300 (1 of 4)\t\t1000 (1 of 4)\t0 (1 of 4)\t\t1/0/0/3"
    );

    fs::remove_dir_all(codex_home).unwrap();
}

/// The selection is exactly what `list` returns for the same flags, in the
/// same order, and each session keeps its own bounded bundle.
#[test]
fn export_over_an_activity_window_writes_one_bundle_per_session_and_a_manifest() {
    let (codex_home, home) = filter_fixture_store("export-window");
    let bundle = codex_home.join("bundle");
    let mut command = tapes();
    command.args([
        "export",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T11:00:00Z",
        "--until",
        "2026-01-01T14:00:00Z",
        "--sort",
        "oldest",
        "--bundle",
    ]);
    command.arg(&bundle);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema"], "tapes-export-manifest/5");
    assert_eq!(
        manifest["selection"],
        serde_json::json!({
            "scope": "global",
            "harness": "codex",
            "activity": {
                "since": "2026-01-01T11:00:00Z",
                "until": "2026-01-01T14:00:00Z"
            },
            "sort": "oldest",
            "limit": 20
        })
    );
    let exported = manifest["sessions"].as_array().unwrap();
    assert_eq!(
        exported
            .iter()
            .map(|session| session["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "10000000-0000-0000-0000-000000000002",
            "20000000-0000-0000-0000-000000000004"
        ]
    );
    assert!(manifest["failed"].as_array().unwrap().is_empty());
    assert_eq!(manifest["scan_truncated"], false);

    // Two sessions exported in one run keep separate bundles: the stem carries
    // the session id, so sharing an export second cannot collide.
    let mut stdout_paths = Vec::new();
    for session in exported {
        for kind in ["context", "json", "trace"] {
            let path = PathBuf::from(session["files"][kind].as_str().unwrap());
            assert!(path.is_file(), "{} is missing", path.display());
            stdout_paths.push(path);
        }
    }
    assert_eq!(fs::read_dir(&bundle).unwrap().count(), 7);

    let stdout = String::from_utf8(output.stdout).unwrap();
    let printed = stdout
        .lines()
        .map(|line| PathBuf::from(line.split('\t').next().unwrap()))
        .collect::<Vec<_>>();
    stdout_paths.push(bundle.join("manifest.json"));
    assert_eq!(printed, stdout_paths);

    fs::remove_dir_all(codex_home).unwrap();
}

/// A limit bounds the selection the way it bounds a listing, and a selection
/// that holds nothing is a fact rather than a failure.
#[test]
fn export_honours_the_limit_and_writes_a_manifest_for_an_empty_selection() {
    let (codex_home, home) = filter_fixture_store("export-limit");
    let run = |arguments: &[&str], bundle: &Path| {
        let mut command = tapes();
        command.args(arguments).arg("--bundle").arg(bundle);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        command.output().unwrap()
    };

    let limited_bundle = codex_home.join("limited");
    let limited = run(
        &["export", "--global", "--harness", "codex", "--limit", "1"],
        &limited_bundle,
    );
    assert!(
        limited.status.success(),
        "{}",
        String::from_utf8_lossy(&limited.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(limited_bundle.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["selection"]["limit"], 1);
    assert_eq!(
        manifest["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["30000000-0000-0000-0000-000000000005"]
    );
    assert_eq!(fs::read_dir(&limited_bundle).unwrap().count(), 4);

    let empty_bundle = codex_home.join("empty");
    let empty = run(
        &[
            "export",
            "--global",
            "--harness",
            "codex",
            "--since",
            "2030-01-01",
        ],
        &empty_bundle,
    );
    assert!(
        empty.status.success(),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(empty_bundle.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["sessions"].as_array().unwrap().is_empty());
    assert!(manifest["failed"].as_array().unwrap().is_empty());
    assert_eq!(fs::read_dir(&empty_bundle).unwrap().count(), 1);
    let stdout = String::from_utf8(empty.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "{stdout}");

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn endings_help_names_the_schema_and_what_it_does_not_do() {
    let output = tapes().args(["endings", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(help.contains("tapes-endings/6"), "{help}");
    assert!(help.contains("never on their text"), "{help}");
    assert!(help.contains("labels no session complete"), "{help}");
    assert!(help.contains("--tail <N>"), "{help}");
    assert!(help.contains("[default: 12]"), "{help}");
    assert!(help.contains("--text"), "{help}");
}

/// The activity window chooses the set before a transcript is opened, so the
/// report holds exactly the sessions `list` would return for the same flags.
#[test]
fn endings_applies_the_activity_window_before_reading_any_transcript() {
    let (codex_home, home) = filter_fixture_store("endings-window");
    let mut command = tapes();
    command.args([
        "endings",
        "--global",
        "--harness",
        "codex",
        "--since",
        "2026-01-01T12:00:00Z",
        "--json",
    ]);
    with_fixture_env(
        &mut command,
        &codex_home,
        &home,
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(value["schema"], "tapes-endings/6");
    assert_eq!(
        value["selection"]["activity"],
        serde_json::json!({ "since": "2026-01-01T12:00:00Z" })
    );
    let endings = value["endings"].as_array().unwrap();
    assert_eq!(
        endings
            .iter()
            .map(|ending| ending["session"]["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "30000000-0000-0000-0000-000000000005",
            "20000000-0000-0000-0000-000000000004"
        ]
    );
    for ending in endings {
        assert_eq!(ending["source"]["schema"], "tapes-endings/6");
        assert_eq!(ending["source"]["source"]["recorded_harness"], "codex");
        assert_eq!(ending["facts"], serde_json::json!(["assistant-close"]));
        // The structural report carries no transcript text of its own.
        assert!(ending.get("tail").is_none(), "{ending}");
    }
    assert!(value["unread"].as_array().unwrap().is_empty());
    assert_eq!(value["scan_truncated"], false);

    fs::remove_dir_all(codex_home).unwrap();
}

/// The text tail is a glimpse of the exchange: an entry longer than the bound
/// is cut and says so, and the human render prints it beneath its session.
#[test]
fn endings_text_tail_is_bounded_and_marks_what_it_cut() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-endings-text-{}", std::process::id())),
    );
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let codex_home = root.path().join("codex");
    let sessions = codex_home.join("sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    let id = "40000000-0000-0000-0000-000000000006";
    let long = "word ".repeat(200);
    let long = long.trim_end();
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        format!(
            r#"{{"timestamp":"2026-01-01T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}
{{"timestamp":"2026-01-01T10:00:01Z","type":"turn_context","payload":{{"cwd":"/fixtures/project","model":"gpt-fixture"}}}}
{{"timestamp":"2026-01-01T10:00:02Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Inspect the fixture."}}]}}}}
{{"timestamp":"2026-01-01T10:00:03Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{long}"}}]}}}}
"#
        ),
    )
    .unwrap();

    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let value: Value =
        serde_json::from_slice(&run(&["endings", "--global", "--text", "--json"])).unwrap();
    let tail = value["endings"][0]["tail"].as_array().unwrap();
    assert_eq!(tail.len(), 2, "{tail:#?}");
    assert_eq!(tail[0]["kind"], "operator");
    assert_eq!(tail[0]["text"], "Inspect the fixture.");
    assert_eq!(tail[0]["truncated"], false);
    assert_eq!(tail[1]["kind"], "assistant");
    assert_eq!(tail[1]["text"].as_str().unwrap().chars().count(), 400);
    assert_eq!(tail[1]["truncated"], true);

    let human = String::from_utf8(run(&["endings", "--global", "--text"])).unwrap();
    let lines = human.lines().collect::<Vec<_>>();
    assert!(
        lines[0].starts_with(&format!(
            "{id} codex 2026-01-01T10:00:03Z assistant assistant-close"
        )),
        "{human}"
    );
    assert_eq!(lines[1], "  [user #0 2026-01-01T10:00:02Z]");
    assert_eq!(lines[2], "    Inspect the fixture.");
    assert_eq!(
        lines[3],
        "  [assistant #1 2026-01-01T10:00:03Z cut at 400 characters]"
    );

    let without_text = String::from_utf8(run(&["endings", "--global"])).unwrap();
    assert!(
        !without_text
            .lines()
            .any(|line| line.starts_with("  [") || line.starts_with("    ")),
        "{without_text}"
    );
}

const CODEX_SESSION_TITLE: &str = include_str!(
    "../fixtures/codex/rollout-2026-01-01T12-00-00-10000000-0000-0000-0000-000000000003.jsonl"
);

/// A recording whose every counted figure is chosen: a call repeated under
/// one id, a result whose call is outside the pair, a tool the harness
/// recorded an error on, two complete pairs with known durations, recorded
/// token counters, and one child.
const STATS_PARENT: &str = concat!(
    r#"{"timestamp":"2026-02-02T09:00:00Z","type":"session_meta","payload":{"id":"44444444-0000-0000-0000-000000000001","cwd":"/fixtures/project"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:01Z","type":"turn_context","payload":{"cwd":"/fixtures/project","model":"gpt-fixture","effort":"high"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Measure the fixture."}]}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:02.500Z","type":"event_msg","payload":{"type":"user_message","message":"Measure the fixture."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:03Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Consider the fixture."}]}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:04Z","type":"response_item","payload":{"type":"function_call","name":"read","arguments":"{}","call_id":"call-a"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:05Z","type":"response_item","payload":{"type":"function_call","name":"read","arguments":"{}","call_id":"call-a"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:07Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-a","output":"Read complete."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:09Z","type":"response_item","payload":{"type":"custom_tool_call","name":"probe","input":"probe fixture","call_id":"call-b","status":"error"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:11Z","type":"response_item","payload":{"type":"custom_tool_call","name":"probe","input":"probe fixture","call_id":"call-c","status":"completed"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:12Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-c","output":"Probe complete."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:13Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","arguments":"{\"task_name\":\"explore\",\"model\":\"gpt-fixture\"}","call_id":"call-d"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:14Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-d","output":"{\"task_name\":\"/root/explore\",\"agents\":[{\"agent_name\":\"/root/explore\",\"agent_status\":\"completed\"}]}"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:15Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-z","output":"Orphan result."}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:16Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Fixture measured."}]}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:16.500Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2500,"cached_input_tokens":1000,"cache_write_input_tokens":500,"output_tokens":400,"total_tokens":4400},"model_context_window":272000},"rate_limits":null}}"#,
    "\n",
);

const STATS_CHILD: &str = concat!(
    r#"{"timestamp":"2026-02-02T09:00:13Z","type":"session_meta","payload":{"id":"44444444-0000-0000-0000-000000000002","cwd":"/fixtures/project","parent_thread_id":"44444444-0000-0000-0000-000000000001","agent_path":"/root/explore","agent_nickname":"explorer"}}"#,
    "\n",
    r#"{"timestamp":"2026-02-02T09:00:14Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Explored."}]}}"#,
    "\n",
);

fn stats_store(name: &str) -> TemporaryDirectory {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-stats-{name}-{}", std::process::id())),
    );
    let sessions = root.path().join("codex/sessions/2026/02/02");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir_all(root.path().join("home")).unwrap();
    fs::write(
        sessions.join("rollout-2026-02-02T09-00-00-44444444-0000-0000-0000-000000000001.jsonl"),
        STATS_PARENT,
    )
    .unwrap();
    fs::write(
        sessions.join("rollout-2026-02-02T09-00-13-44444444-0000-0000-0000-000000000002.jsonl"),
        STATS_CHILD,
    )
    .unwrap();
    root
}

fn stats_command(root: &Path, arguments: &[&str]) -> std::process::Output {
    let mut command = tapes();
    command.args(arguments);
    with_fixture_env(
        &mut command,
        &root.join("codex"),
        &root.join("home"),
        Path::new("/definitely/missing"),
    );
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn stats_help_names_the_schema_and_what_the_figures_cover() {
    let output = tapes().args(["stats", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("tapes-stats/6"), "{help}");
    assert!(help.contains("complete pairs only"), "{help}");
    assert!(
        help.contains("share of recorded token counts rather than of cost"),
        "{help}"
    );
    assert!(help.contains("Nothing is judged"), "{help}");
}

/// Every figure in the answer is a figure the recording chose, so the whole
/// object is asserted rather than a sample of it.
#[test]
fn stats_json_counts_a_chosen_recording_exactly() {
    let root = stats_store("exact");
    let id = "44444444-0000-0000-0000-000000000001";
    let output = stats_command(root.path(), &["stats", id, "--json"]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut comparable = value.clone();
    let read = comparable.as_object_mut().unwrap().remove("read").unwrap();
    let text_tail = comparable
        .as_object_mut()
        .unwrap()
        .remove("text_tail")
        .unwrap();
    comparable.as_object_mut().unwrap().remove("content");
    assert_eq!(read["source_length"], 2722);
    assert_eq!(read["configured_bound"], 4 * 1024 * 1024);
    assert_eq!(read["ranges"][0]["kind"], "tail");
    assert_eq!(
        read["ranges"][0]["span"],
        serde_json::json!({"start": 0, "end": 2722})
    );
    assert_eq!(read["records"].as_array().unwrap().len(), 16);
    assert_eq!(text_tail["returned"], 2);

    assert_eq!(
        comparable,
        serde_json::json!({
            "schema": "tapes-stats/6",
            "session": {
                "id": id,
                "source": {
                    "kind": "installed-recording",
                    "origin": "codex",
                    "recorded_harness": "codex",
                    "representation": "codex-recording",
                    "producer": "codex",
                    "location": {
                        "locator": root
                            .path()
                            .join("codex/sessions/2026/02/02/rollout-2026-02-02T09-00-00-44444444-0000-0000-0000-000000000001.jsonl")
                            .display()
                            .to_string()
                    }
                },
                "model": { "id": "gpt-fixture", "variant": "high" },
                "started_at": "2026-02-02T09:00:00Z",
                "last_activity_at": "2026-02-02T09:00:16.500Z",
                "directory": "/fixtures/project",
            },
            "coverage": { "turns": "session", "pairs": "complete-only" },
            "turns": {
                "operator": 1,
                "assistant": 1,
                "tool": 9,
                "reasoning": 1,
                "control": 0,
                "ambient": 0,
                "notice": 0,
                "unknown": 0,
                "total": 12
            },
            "kinds": {
                "recordable": ["operator", "assistant", "reasoning", "tool", "ambient", "notice"],
                "user_default": {
                    "kind": "operator",
                    "basis": "Codex wraps the text it puts in the user role in elements of its own, so a user message holding anything else is the operator's"
                }
            },
            "tools": {
                "calls": 5,
                "results": 4,
                "paired": 3,
                "incomplete": {
                    "no-result-in-read": 2,
                    "call-before-read-bound": 0,
                    "call-not-recorded": 1
                },
                "by_name": [
                    {
                        "name": "probe",
                        "calls": 2,
                        "paired": 1,
                        "errors": 1,
                        "duration_ms": { "total": 1_000, "max": 1_000, "count": 1 }
                    },
                    {
                        "name": "read",
                        "calls": 2,
                        "paired": 1,
                        "errors": 0,
                        "duration_ms": { "total": 2_000, "max": 2_000, "count": 1 }
                    },
                    {
                        "name": "spawn_agent",
                        "calls": 1,
                        "paired": 1,
                        "errors": 0,
                        "duration_ms": { "total": 1_000, "max": 1_000, "count": 1 }
                    }
                ],
                "errors": 1
            },
            "durations_ms": {
                "recorded_span": 14_000,
                "in_tool": 4_000,
                "between_turns_max": 2_000,
                "count_with_timestamps": 12
            },
            "usage": {
                "tokens": {
                    "input": 2500,
                    "output": 400,
                    "cache_read": 1000,
                    "cache_write": 500
                },
                "accounting": { "basis": "recorded-total", "coverage": "session" },
                "cache_read_ratio": 0.25,
                "cache_write_ratio": 0.125
            },
            "lineage": {
                "children": 1,
                "resolved": 1,
                "by_disposition": { "completed": 1 }
            },
            "warnings": ["incomplete-pairs"]
        }),
        "{value:#}"
    );
}

/// The counts are the `events` projection's own, so the two commands answer
/// the same question about the same read.
#[test]
fn stats_tool_counts_are_the_events_projections_counts() {
    let root = stats_store("events");
    let id = "44444444-0000-0000-0000-000000000001";
    let stats: Value =
        serde_json::from_slice(&stats_command(root.path(), &["stats", id, "--json"]).stdout)
            .unwrap();
    let events: Value =
        serde_json::from_slice(&stats_command(root.path(), &["events", id, "--json"]).stdout)
            .unwrap();

    let records = events["events"].as_array().unwrap();
    let counted = |kind: &str| {
        records
            .iter()
            .filter(|record| record["kind"] == kind)
            .count()
    };
    assert_eq!(stats["tools"]["calls"], counted("tool-call"));
    assert_eq!(stats["tools"]["results"], counted("tool-result"));
    assert_eq!(stats["tools"]["paired"], events["pairs"]["complete"]);
    let incomplete = |reason: &str| {
        records
            .iter()
            .filter(|record| record["incomplete"] == reason)
            .count()
    };
    for reason in [
        "no-result-in-read",
        "call-before-read-bound",
        "call-not-recorded",
    ] {
        assert_eq!(
            stats["tools"]["incomplete"][reason],
            incomplete(reason),
            "{reason}"
        );
    }
    // The tool distribution comes from the typed events rather than from the
    // harness envelope each tool turn keeps as its text.
    let named = |name: &str| {
        records
            .iter()
            .filter(|record| record["kind"] == "tool-call" && record["name"] == name)
            .count()
    };
    for tool in stats["tools"]["by_name"].as_array().unwrap() {
        assert_eq!(
            tool["calls"],
            named(tool["name"].as_str().unwrap()),
            "{tool}"
        );
    }
}

/// A read that stopped at a source bound counted only what it reached, and
/// says so in the coverage and in a warning.
#[test]
fn stats_of_an_oversized_recording_reports_the_read_window() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-stats-oversized-{}", std::process::id())),
    );
    let sessions = root.path().join("codex/sessions/2026/02/02");
    fs::create_dir_all(&sessions).unwrap();
    let id = "44444444-0000-0000-0000-0000000000ff";
    let mut file = std::io::BufWriter::new(
        fs::File::create(sessions.join(format!("rollout-2026-02-02T09-00-00-{id}.jsonl"))).unwrap(),
    );
    use std::io::Write;
    writeln!(
        file,
        r#"{{"timestamp":"2026-02-02T09:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/fixtures/project"}}}}"#
    )
    .unwrap();
    let filler = "x".repeat(4096);
    for _ in 0..1200 {
        writeln!(
            file,
            r#"{{"timestamp":"2026-02-02T10:00:00Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{filler}"}}]}}}}"#
        )
        .unwrap();
    }
    drop(file);

    let value: Value =
        serde_json::from_slice(&stats_command(root.path(), &["stats", id, "--json"]).stdout)
            .unwrap();
    assert_eq!(value["coverage"]["turns"], "read-window");
    assert_eq!(
        value["coverage"]["truncation"]["source"],
        serde_json::json!([{ "kind": "file-tail", "bytes": 4_194_304 }])
    );
    assert_eq!(value["warnings"], serde_json::json!(["read-window"]));

    let human = String::from_utf8(stats_command(root.path(), &["stats", id]).stdout).unwrap();
    assert!(
        human.contains("coverage: turns cover the bounded read window"),
        "{human}"
    );
    assert!(human.contains("warnings: read-window"), "{human}");
    assert!(
        human.contains("Only the final 4 MiB of the recording was read"),
        "{human}"
    );
}

/// Human output states each group once and invents no line for a group the
/// recording holds nothing for.
#[test]
fn stats_human_output_prints_no_absent_group() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-stats-human-{}", std::process::id())),
    );
    let sessions = root.path().join("codex/sessions/2026/01/01");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("rollout-2026-01-01T12-00-00-10000000-0000-0000-0000-000000000003.jsonl"),
        CODEX_SESSION_TITLE,
    )
    .unwrap();

    let rendered = String::from_utf8(
        stats_command(
            root.path(),
            &["stats", "10000000-0000-0000-0000-000000000003"],
        )
        .stdout,
    )
    .unwrap();

    assert!(
        rendered.starts_with(
            "session: codex 10000000-0000-0000-0000-000000000003 gpt-fixture (high)\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "coverage: turns cover the whole session, durations cover complete pairs only\n"
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("turns: 3 total, 1 operator, 1 assistant, 1 ambient\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("tools: 0 calls, 0 results, 0 paired, 0 errors\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("durations: recorded span 2000ms, longest gap between turns 1000ms, 3 turns with timestamps\n"),
        "{rendered}"
    );
    for absent in [
        "incomplete:",
        "NAME\tCALLS",
        "tokens:",
        "cost:",
        "accounting:",
        "cache of input",
        "children:",
        "warnings:",
        "Note:",
    ] {
        assert!(!rendered.contains(absent), "{absent} in {rendered}");
    }
}

/// The stats read is the export-shaped one, so its turn counts are the whole
/// bounded read's and agree with what `usage` counts by role.
#[test]
fn stats_and_usage_count_the_same_bounded_read() {
    let root = stats_store("usage");
    let id = "44444444-0000-0000-0000-000000000001";
    let stats: Value =
        serde_json::from_slice(&stats_command(root.path(), &["stats", id, "--json"]).stdout)
            .unwrap();
    let usage: Value =
        serde_json::from_slice(&stats_command(root.path(), &["usage", id, "--json"]).stdout)
            .unwrap();

    assert_eq!(stats["turns"]["total"], usage["turns"]["total"]);
    assert_eq!(stats["turns"]["tool"], usage["turns"]["tool"]);
    assert_eq!(stats["turns"]["assistant"], usage["turns"]["assistant"]);
    assert_eq!(stats["coverage"]["turns"], usage["turns"]["coverage"]);
    assert_eq!(stats["usage"]["tokens"], usage["tokens"]);
    assert_eq!(stats["usage"]["accounting"], usage["accounting"]);
}

#[test]
fn brief_help_names_the_schema_and_the_half_it_reads() {
    let output = tapes().args(["brief", "--help"]).output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);

    assert!(help.contains("tapes-brief/6"), "{help}");
    assert!(help.contains("reads the recording alone"), "{help}");
    assert!(help.contains("--tail <N>"), "{help}");
    assert!(help.contains("[default: 12]"), "{help}");
}

/// The human brief is one screen in a fixed reading order: which session this
/// is, where it worked, where it stopped, what it left open, what it last
/// said.
#[test]
fn brief_renders_the_continuation_in_reading_order() {
    let (codex_home, home) = fixture_store("brief-order");
    let id = "00000000-0000-0000-0000-000000000001";
    let run = |arguments: &[&str]| {
        let mut command = tapes();
        command.args(arguments);
        with_fixture_env(
            &mut command,
            &codex_home,
            &home,
            Path::new("/definitely/missing"),
        );
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let human = String::from_utf8(run(&["brief", id])).unwrap();
    let lines = human.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[0],
        format!("session: codex {id} ~Inspect the fixture."),
        "{human}"
    );
    // The recorded directory is not on this machine, which is stated rather
    // than left blank, and nothing about a commit is claimed for it.
    assert_eq!(lines[1], "working set: /fixtures/project (gone)", "{human}");
    assert_eq!(lines[2], "ending: tool call-without-result", "{human}");
    assert_eq!(
        lines[3], "in flight: call fixture_pending call-pending #5 2026-01-01T10:00:06Z",
        "{human}"
    );
    assert_eq!(lines[4], "tail:", "{human}");
    assert_eq!(lines[5], "[user #0 2026-01-01T10:00:02Z]", "{human}");
    assert_eq!(lines[6], "Inspect the fixture.", "{human}");
    assert_eq!(lines[7], "[assistant #4 2026-01-01T10:00:06Z]", "{human}");
    assert_eq!(lines[8], "Fixture inspected.", "{human}");
    assert_eq!(lines.len(), 9, "{human}");

    // The window bounds the rendered exchange alone.
    let narrow = String::from_utf8(run(&["brief", id, "--tail", "1"])).unwrap();
    assert!(!narrow.contains("[user #0"), "{narrow}");
    assert!(narrow.contains("[assistant #4"), "{narrow}");

    let value: Value = serde_json::from_slice(&run(&["brief", id, "--json"])).unwrap();
    assert_eq!(value["schema"], "tapes-brief/6");
    assert_eq!(value["session"]["id"], id);
    assert_eq!(value["working_set"]["directory_exists"], false);
    assert!(value["working_set"].get("git").is_none(), "{value}");
    assert_eq!(
        value["ending"]["facts"],
        serde_json::json!(["call-without-result"])
    );
    assert_eq!(
        value["in_flight"]["calls_without_result"][0]["call_id"],
        "call-pending"
    );
    assert_eq!(value["usage"]["tokens"]["input"], 1200);
    assert_eq!(value["tail"].as_array().unwrap().len(), 2);

    fs::remove_dir_all(codex_home).unwrap();
}

#[test]
fn selection_stats_agree_with_individual_reads() {
    let (path, home) = fixture_store("stats-summary");
    let root = TemporaryDirectory { path };
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, root.path(), &home, root.path());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let summary = run(&["stats", "--global", "--harness", "codex", "--json"]);
    assert_eq!(summary["schema"], "tapes-stats-summary/4");
    assert_eq!(summary["selected"], 2);
    assert_eq!(summary["read"], 2);
    let mut totals = [0_u64; 4];
    for row in summary["sessions"].as_array().unwrap() {
        let one = run(&["stats", row["session"]["id"].as_str().unwrap(), "--json"]);
        assert_eq!(row["tools"], one["tools"]);
        assert_eq!(row["coverage"], one["coverage"]);
        for (index, key) in ["calls", "results", "paired", "errors"].iter().enumerate() {
            totals[index] += one["tools"][key].as_u64().unwrap();
        }
    }
    for (index, key) in ["calls", "results", "paired", "errors"].iter().enumerate() {
        assert_eq!(summary["by_harness"]["codex"][key], totals[index]);
    }
}

#[test]
fn selection_stats_cli_reports_partial_and_total_read_failure() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-stats-failure-{}", std::process::id())),
    );
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/opencode/opencode2");
    let program = root.path().join("opencode2");
    let calls = root.path().join("calls");
    for (pattern, reads, success) in [
        ("/api/session/ses_api_only_fixture/message*", 1, true),
        ("/api/session/*/message*", 0, false),
    ] {
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$4\" >> '{}'\ncase \"$4\" in {pattern}) exit 7;; esac\nexec '{}' \"$@\"\n", calls.display(),fixture.display());
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = tapes();
        command.args(["stats", "--global", "--harness", "opencode", "--json"]);
        with_fixture_env(
            &mut command,
            &root.path().join("codex"),
            &root.path().join("home"),
            root.path(),
        );
        let output = command.output().unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["selected"], 2);
        assert_eq!(report["read"], reads);
        assert_eq!(report["failed"].as_array().unwrap().len(), 2 - reads);
        assert_eq!(report["sessions"].as_array().unwrap().len(), reads);
    }
    let calls = fs::read_to_string(calls).unwrap();
    assert!(
        !calls.contains("parent"),
        "summary must not request child lineage: {calls}"
    );
}

#[test]
fn recorded_title_selects_every_single_session_view_and_refuses_hidden_ambiguity() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-title-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    fs::create_dir_all(&project).unwrap();
    let body = format!(
        "{}\n{{\"type\":\"ai-title\",\"aiTitle\":\"Exact Title\"}}\n",
        CLAUDE_SESSION
    );
    fs::write(project.join("session-claude.jsonl"), &body).unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
        command.output().unwrap()
    };
    for view in ["show", "brief", "usage", "stats", "lineage", "events"] {
        let output = run(&[
            view,
            "--title",
            "Exact Title",
            "--global",
            "--harness",
            "claude",
            "--json",
        ]);
        assert!(
            output.status.success(),
            "{view}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["session"]["id"], "session-claude", "{view}");
    }
    let output = run(&[
        "export",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
        "--bundle",
        root.path().join("bundles").to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let miss = run(&[
        "show",
        "--title",
        "exact title",
        "--global",
        "--harness",
        "claude",
    ]);
    assert!(!miss.status.success());
    assert!(String::from_utf8_lossy(&miss.stderr).contains("was not found"));
    for i in 0..25 {
        let content = body
            .replace("session-claude", &format!("session-{i}"))
            .replace("Exact Title", &format!("filler-{i}"));
        fs::write(project.join(format!("session-{i}.jsonl")), content).unwrap();
    }
    fs::write(
        project.join("duplicate.jsonl"),
        body.replace("session-claude", "duplicate"),
    )
    .unwrap();
    let ambiguous = run(&[
        "show",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
    ]);
    let diagnostic = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(!ambiguous.status.success());
    assert!(
        diagnostic.contains("ambiguous")
            && diagnostic.contains("duplicate")
            && diagnostic.contains("session-claude"),
        "{diagnostic}"
    );
    fs::remove_file(project.join("duplicate.jsonl")).unwrap();
    fs::write(project.join("unreadable.jsonl"), "{malformed\n").unwrap();
    let unreadable = run(&[
        "show",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
    ]);
    assert!(!unreadable.status.success());
    assert!(String::from_utf8_lossy(&unreadable.stderr).contains("incomplete lookup"));
    fs::remove_file(project.join("unreadable.jsonl")).unwrap();

    let hidden = format!(
        "{}{}\n",
        body.replace("session-claude", "hidden"),
        format!(
            "{{\"type\":\"padding\",\"text\":\"{}\"}}\n",
            "x".repeat(4096)
        )
        .repeat(1100)
    );
    fs::write(project.join("hidden.jsonl"), hidden).unwrap();
    let incomplete = run(&[
        "show",
        "--title",
        "Exact Title",
        "--global",
        "--harness",
        "claude",
    ]);
    let diagnostic = String::from_utf8_lossy(&incomplete.stderr);
    assert!(!incomplete.status.success());
    assert!(
        diagnostic.contains("incomplete lookup") && diagnostic.contains("hidden"),
        "{diagnostic}"
    );
}

#[test]
fn show_preserves_exec_operator_evidence_outside_the_source_tail() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-cli-exec-header-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let id = "40000000-0000-0000-0000-000000000001";
    let header = serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T10:00:00Z",
        "payload":{"id":id,"source":"exec","cwd":"/fixture"}});
    let padding = serde_json::json!({"type":"padding","text":"x".repeat(4096)});
    let prompt = serde_json::json!({"type":"response_item","timestamp":"2026-01-01T10:01:00Z",
        "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"finish the bounded task"}]}});
    let body = format!(
        "{header}\n{}{prompt}\n",
        format!("{padding}\n").repeat(1100)
    );
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    let mut command = tapes();
    command.args(["show", id, "--json"]);
    with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["turns"].as_array().unwrap().len(), 1);
    assert_eq!(view["turns"][0]["kind"], "operator");
    assert_eq!(view["truncation"]["source"][0]["kind"], "file-tail");
}

#[test]
fn history_pages_cover_records_once_and_reject_changed_sources() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-history-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let id = "50000000-0000-0000-0000-000000000001";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let header = serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T10:00:00Z","payload":{"id":id,"source":"exec","cwd":"/fixture"}});
    let mut body = format!("{header}\n");
    for i in 0..40 {
        let turn = serde_json::json!({"type":"response_item","timestamp":"2026-01-01T10:01:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("historical request {i:02}")}]}});
        body.push_str(&format!("{turn}\n"));
    }
    fs::write(&path, &body).unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
        command.output().unwrap()
    };
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let mut args = vec!["page", id, "--bytes", "1024", "--json"];
        if let Some(cursor) = cursor.as_deref() {
            args.extend(["--cursor", cursor]);
        }
        let output = run(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let page: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(page["bytes_read"].as_u64().unwrap() <= 1024);
        assert_eq!(page["skipped_records"], 0);
        assert_eq!(page["skipped_fragment_bytes"], 0);
        assert_eq!(page["read"]["coordinate_domain"], "file-byte-range");
        assert_eq!(
            page["read"]["source_length"],
            fs::metadata(&path).unwrap().len()
        );
        assert!(!page["read"]["records"].as_array().unwrap().is_empty());
        for turn in page["turns"].as_array().unwrap() {
            assert_eq!(turn["kind"], "operator");
            assert!(
                seen.insert(turn["text"].as_str().unwrap().to_owned()),
                "duplicate {turn}"
            );
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        pages += 1;
        assert!(pages < 50);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 40);
    let small: Value =
        serde_json::from_slice(&run(&["page", id, "--bytes", "1024", "--json"]).stdout).unwrap();
    let larger: Value =
        serde_json::from_slice(&run(&["page", id, "--bytes", "2048", "--json"]).stdout).unwrap();
    assert_eq!(small["turns"][0]["ordinal"], 0);
    assert_eq!(larger["turns"][0]["ordinal"], 0);
    let common = small["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|turn| {
            let text = turn["text"].as_str()?;
            let other = larger["turns"]
                .as_array()?
                .iter()
                .find(|candidate| candidate["text"] == text)?;
            Some((turn, other))
        })
        .expect("the page sizes overlap a record");
    assert_eq!(common.0["record_ref"], common.1["record_ref"]);
    assert_eq!(common.0["record_ref"]["part_index"], 0);
    assert!(
        common.0["record_ref"]["span"]["end"].as_u64().unwrap()
            > common.0["record_ref"]["span"]["start"].as_u64().unwrap()
    );
    let output = run(&["page", id, "--bytes", "1024", "--json"]);
    let first_cursor = serde_json::from_slice::<Value>(&output.stdout).unwrap()["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let output = run(&[
        "history-search",
        id,
        "--bytes",
        "1024",
        "--pages",
        "1",
        "--search",
        "request 00",
        "--json",
    ]);
    let limited: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(limited["matches"].as_array().unwrap().is_empty());
    assert!(limited["next_cursor"].is_string());
    let output = run(&[
        "history-search",
        id,
        "--bytes",
        "1024",
        "--pages",
        "32",
        "--search",
        "request 00",
        "--json",
    ]);
    let complete: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(complete["matches"].as_array().unwrap().len(), 1);
    assert!(
        complete["matches"][0]["page_end"].as_u64().unwrap()
            > complete["matches"][0]["page_start"].as_u64().unwrap()
    );
    assert!(complete["next_cursor"].is_null());
    fs::write(&path, format!("{body}\n")).unwrap();
    let changed = run(&["page", id, "--cursor", &first_cursor, "--json"]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("source changed"));
}

#[test]
fn metadata_history_recovers_midfile_models_and_reports_record_gaps() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-history-models-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let id = "60000000-0000-0000-0000-000000000001";
    let header = serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T10:00:00Z","payload":{"id":id,"source":"exec","cwd":"/fixture"}});
    let model = serde_json::json!({"type":"turn_context","timestamp":"2026-01-01T10:01:00Z","payload":{"model":"recorded-model","effort":"medium"}});
    let padding = format!(
        "{{\"type\":\"padding\",\"text\":\"{}\"}}\n",
        "x".repeat(4096)
    );
    let body = format!(
        "{header}\n{}{model}\n{}malformed\n",
        padding.repeat(20),
        padding.repeat(1100)
    );
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        body,
    )
    .unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let shown = run(&["show", id, "--json"]);
    assert!(shown["session"]["model"].is_null());
    let metadata = run(&[
        "metadata", id, "--bytes", "1048576", "--pages", "8", "--json",
    ]);
    assert!(metadata["next_cursor"].is_null());
    assert_eq!(metadata["skipped_records"], 1);
    assert!(metadata.get("context_bytes").is_none());
    assert_eq!(metadata["observations"][0]["model"]["id"], "recorded-model");
    let tiny = run(&["metadata", id, "--bytes", "1024", "--pages", "2", "--json"]);
    assert!(tiny["skipped_fragment_bytes"].as_u64().unwrap() > 0);
    assert!(tiny["next_cursor"].is_string());
}

#[test]
fn history_page_preserves_a_record_exactly_aligned_with_the_byte_budget() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-page-alignment-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let id = "80000000-0000-0000-0000-000000000001";
    let header = serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T10:00:00Z","payload":{"id":id,"source":"exec","cwd":"/fixture"}});
    let mut record = serde_json::json!({"type":"response_item","timestamp":"2026-01-01T10:01:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}});
    let padding = 1023 - record.to_string().len();
    record["payload"]["content"][0]["text"] = Value::String("x".repeat(padding));
    let line = format!("{record}\n");
    assert_eq!(line.len(), 1024);
    fs::write(
        sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl")),
        format!("{header}\n{line}{line}"),
    )
    .unwrap();
    let mut command = tapes();
    command.args(["page", id, "--bytes", "1024", "--json"]);
    with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
    let output = command.output().unwrap();
    assert!(output.status.success());
    let page: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page["turns"].as_array().unwrap().len(), 1);
    assert_eq!(page["skipped_fragment_bytes"], 0);
    assert_eq!(page["bytes_read"], 1024);
    assert_eq!(page["alignment_bytes"], 1);
}

#[test]
fn codex_history_pages_read_only_their_range_and_detect_same_size_edits() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-page-context-{}", std::process::id())),
    );
    let codex = root.path().join("codex");
    let sessions = codex.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let id = "90000000-0000-0000-0000-000000000001";
    let path = sessions.join(format!("rollout-2026-01-01T10-00-00-{id}.jsonl"));
    let header = serde_json::json!({"type":"session_meta","timestamp":"2026-01-01T10:00:00Z","payload":{"id":id,"source":"cli","cwd":"/fixture"}});
    let user = |text: &str| serde_json::json!({"type":"response_item","timestamp":"2026-01-01T10:01:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}});
    let ambient = user("<environment_context>\n  <cwd>/fixture</cwd>\n</environment_context>");
    let notice = user("<turn_aborted>\nThe user interrupted the previous turn.\n</turn_aborted>");
    let operator = user("corroborated request");
    let evidence = serde_json::json!({"type":"event_msg","timestamp":"2026-01-01T10:01:00Z","payload":{"type":"user_message","message":"corroborated request"}});
    let mut body = format!(
        "{header}\n{}\n{ambient}\n{notice}\n{operator}\n",
        format!("{{\"padding\":\"{}\"}}\n", "x".repeat(4000)).repeat(20)
    );
    let mut newer = format!("{evidence}\n");
    newer.push_str(&" ".repeat(1023 - newer.len()));
    newer.push('\n');
    assert_eq!(newer.len(), 1024);
    body.push_str(&newer);
    fs::write(&path, &body).unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &codex, &root.path().join("home"), root.path());
        command.output().unwrap()
    };
    let json = |output: std::process::Output| -> Value {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let mut cursor: Option<String> = None;
    let mut first_cursor = None;
    let mut paged_kinds = Vec::new();
    for _ in 0..8 {
        let mut args = vec!["page", id, "--bytes", "1024", "--json"];
        if let Some(cursor) = cursor.as_deref() {
            args.extend(["--cursor", cursor]);
        }
        let page = json(run(&args));
        assert_eq!(page["skipped_records"], 0);
        assert!(page.get("context_bytes").is_none());
        let read = &page["read"];
        assert_eq!(
            read["projection_options"],
            serde_json::json!(["transcript"])
        );
        let ranges = read["ranges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|range| range["kind"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ranges, ["alignment", "tail"]);
        assert!(read.get("context_records").is_none());
        for turn in page["turns"].as_array().unwrap() {
            if turn["role"] == "user" {
                paged_kinds.push((turn["record_ref"]["span"].clone(), turn["kind"].clone()));
            }
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        first_cursor.get_or_insert_with(|| cursor.clone().unwrap());
        if paged_kinds.len() == 3 {
            break;
        }
    }
    let shown = json(run(&["show", id, "--json"]));
    let shown_kinds = shown["turns"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|turn| turn["role"] == "user")
        .map(|turn| (turn["record_ref"]["span"].clone(), turn["kind"].clone()))
        .collect::<Vec<_>>();
    paged_kinds.sort_by_key(|(span, _)| span["start"].as_u64());
    assert_eq!(paged_kinds, shown_kinds);
    assert_eq!(
        shown_kinds
            .iter()
            .map(|(_, kind)| kind.clone())
            .collect::<Vec<_>>(),
        ["ambient", "notice", "operator"]
    );

    let cursor = first_cursor.unwrap();
    let original = fs::metadata(&path).unwrap().modified().unwrap();
    let changed_body = body.replace("corroborated request", "CORROBORATED REQUEST");
    assert_eq!(changed_body.len(), body.len());
    fs::write(&path, changed_body).unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(original)
        .unwrap();
    let changed = run(&["page", id, "--cursor", &cursor, "--json"]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("source changed"));
}

#[test]
fn lineage_bounds_subagent_metadata_and_reports_the_gap() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-meta-bound-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    let children = project.join("session-claude/subagents");
    fs::create_dir_all(&children).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    fs::write(children.join("agent-fixture.jsonl"), CLAUDE_SUBAGENT).unwrap();
    fs::write(
        children.join("agent-fixture.meta.json"),
        serde_json::json!({"model":"x".repeat(100000)}).to_string(),
    )
    .unwrap();
    let mut command = tapes();
    command.args(["lineage", "session-claude", "--json"]);
    with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
    let output = command.output().unwrap();
    assert!(output.status.success());
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    let child = view["lineage"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|child| child["reference"] == "fixture")
        .unwrap();
    assert!(child["model"] == "claude-fixture-sonnet");
    assert!(view["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|note| note.as_str().unwrap().contains("exceeds 65536 bytes")));
}

#[test]
fn child_reads_are_qualified_and_never_become_parent_activity() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-child-read-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    let children = project.join("session-claude/subagents");
    fs::create_dir_all(&children).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    fs::write(children.join("agent-fixture.jsonl"), CLAUDE_SUBAGENT).unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
        command.output().unwrap()
    };
    let output = run(&[
        "child",
        "session-claude",
        "--reference",
        "fixture",
        "--tail",
        "0",
        "--json",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(view["schema"], "tapes-child/3");
    assert_eq!(view["parent"]["id"], "session-claude");
    assert_eq!(
        view["transcript"]["session"]["id"],
        "session-claude::fixture"
    );
    assert!(view["transcript"]["turns"].as_array().unwrap().is_empty());
    assert_eq!(view["usage"]["turns"]["total"], 1);
    assert_eq!(view["ending"]["session"]["id"], "session-claude::fixture");
    let listing = run(&["list", "--harness", "claude", "--json"]);
    let listed: Value = serde_json::from_slice(&listing.stdout).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert!(!run(&[
        "child",
        "session-claude",
        "--reference",
        "missing",
        "--json"
    ])
    .status
    .success());
    assert!(!run(&[
        "child",
        "session-claude",
        "--reference",
        "../fixture",
        "--json"
    ])
    .status
    .success());
}

#[test]
fn child_rejects_mixed_native_parent_ids() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-child-identity-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    let children = project.join("session-claude/subagents");
    fs::create_dir_all(&children).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    fs::write(
        children.join("agent-fixture.jsonl"),
        format!(
            "{}\n{}",
            CLAUDE_SUBAGENT,
            CLAUDE_SUBAGENT.replace("session-claude", "different-parent")
        ),
    )
    .unwrap();
    let mut command = tapes();
    command.args([
        "child",
        "session-claude",
        "--reference",
        "fixture",
        "--json",
    ]);
    with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
    let output = command.output().unwrap();
    assert!(
        !output.status.success(),
        "mixed parent identities must refuse"
    );
}

/// `child --full` streams a subagent recording past the read bound: usage
/// counts every turn, the transcript and ending keep the newest --tail turns,
/// and a parent identity no bounded window reaches is still checked.
#[test]
fn child_full_streams_a_subagent_recording_past_the_read_bound() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-child-full-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    let children = project.join("session-claude/subagents");
    fs::create_dir_all(&children).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    let filler = "x".repeat(100 * 1024);
    let record = |parent: &str, uuid: &str, role: &str, content: Value| {
        serde_json::json!({"type": role, "sessionId": parent, "uuid": uuid, "timestamp": "2026-01-01T10:00:01Z", "cwd": "/fixtures/project", "message": {"role": role, "content": content}}).to_string() + "\n"
    };
    let body = |foreign_at: Option<usize>| {
        let mut body = record(
            "session-claude",
            "first",
            "user",
            Value::from("child opening"),
        );
        for index in 0..48 {
            let parent = if foreign_at == Some(index) {
                "other-parent"
            } else {
                "session-claude"
            };
            body += &record(
                parent,
                &format!("filler-{index}"),
                "assistant",
                serde_json::json!([{"type": "text", "text": filler}]),
            );
        }
        body + &record(
            "session-claude",
            "last",
            "assistant",
            serde_json::json!([{"type": "text", "text": "child closing"}]),
        )
    };
    let transcript = children.join("agent-big.jsonl");
    fs::write(&transcript, body(None)).unwrap();
    let run = |args: &[&str]| {
        let mut command = tapes();
        command.args(args);
        with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
        command.output().unwrap()
    };
    let json = |args: &[&str]| -> Value {
        let output = run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let child = ["child", "session-claude", "--reference", "big"];

    let bounded = json(&[&child[..], &["--json"]].concat());
    assert_eq!(bounded["usage"]["turns"]["coverage"], "read-window");
    assert!(bounded["usage"]["turns"]["total"].as_u64().unwrap() < 50);

    let full = json(&[&child[..], &["--full", "--tail", "2", "--json"]].concat());
    assert_eq!(full["schema"], "tapes-child/3");
    assert_eq!(full["usage"]["turns"]["total"], 50, "{}", full["usage"]);
    assert_eq!(full["usage"]["turns"]["coverage"], "session");
    assert_eq!(full["usage"]["read"]["projection_options"][0], "full");
    let turns = full["transcript"]["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1]["text"], "child closing");
    assert_eq!(turns[1]["ordinal"], 49);
    assert_eq!(full["transcript"]["truncation"]["window"]["omitted"], 48);
    assert_eq!(full["transcript"]["session"]["id"], "session-claude::big");
    assert_eq!(
        full["transcript"]["session"]["derived_title"],
        "child opening"
    );
    assert_eq!(full["ending"]["source"]["coverage"], "window");

    // A foreign parent identity between the opening probe and the tail window.
    fs::write(&transcript, body(Some(2))).unwrap();
    assert!(run(&[&child[..], &["--json"]].concat()).status.success());
    let refused = run(&[&child[..], &["--full", "--json"]].concat());
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("different or invalid native parent ID"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    assert!(
        !run(&[&child[..], &["--full", "--read-bytes", "1m"]].concat())
            .status
            .success()
    );
    let refused = run(&[
        &child[..],
        &["--full", "--input", "/definitely/missing.json"],
    ]
    .concat());
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--scan-bytes"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn usage_summary_partitions_incompatible_accounting_without_a_grand_sum() {
    let root = TemporaryDirectory::new(
        std::env::temp_dir().join(format!("tapes-usage-domains-{}", std::process::id())),
    );
    let home = root.path().join("home");
    let project = home.join(".claude/projects/fixture");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("session-claude.jsonl"), CLAUDE_SESSION).unwrap();
    let pi = home.join(".pi/agent/sessions");
    fs::create_dir_all(&pi).unwrap();
    fs::write(
        pi.join("2026-01-01T10-00-00-000Z_session-pi.jsonl"),
        PI_SESSION,
    )
    .unwrap();
    let mut command = tapes();
    command.args(["usage", "--global", "--json"]);
    with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
    let output = command.output().unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["totals"]["mixed_accounting"], true);
    assert!(report["totals"].get("tokens").is_none());
    assert!(report["totals"].get("cost").is_none());
    assert_eq!(report["partitions"].as_array().unwrap().len(), 2);
    let recorded = format!("{}\n{{\"type\":\"cost-state\",\"modelUsage\":{{\"claude-fixture\":{{\"inputTokens\":100,\"outputTokens\":200,\"cacheReadInputTokens\":300,\"cacheCreationInputTokens\":400}}}},\"totalCostUSD\":1.0}}\n", CLAUDE_SESSION.replace("session-claude","recorded"));
    fs::write(project.join("recorded.jsonl"), recorded).unwrap();
    let mut command = tapes();
    command.args(["usage", "--global", "--harness", "claude", "--json"]);
    with_fixture_env(&mut command, &root.path().join("codex"), &home, root.path());
    let output = command.output().unwrap();
    assert!(output.status.success());
    let same_harness: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(same_harness["totals"]["mixed_accounting"], true);
    assert_eq!(same_harness["partitions"].as_array().unwrap().len(), 2);
    assert!(same_harness["groups"][0].get("tokens").is_none());

    for row in report["partitions"].as_array().unwrap() {
        assert!(row.get("tokens").is_some());
    }
}
