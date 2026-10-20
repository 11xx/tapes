//! A session's consumption divides among the commits of one repository, each
//! share labelled with the tier and basis that placed it, with the work after
//! a session's last commit at a place as that place's uncommitted bucket.
//! Each test builds a repository and the recordings of the sessions that
//! worked in it, then names the caller-visible claim it holds and the wrong
//! behaviour it rejects.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_tapes_core::attribution::{Basis, Tier};
use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::change_attribution::{
    attribute, gather, Amount, Kind, MemberRule, Params, Repository, RepositoryAttribution,
    SessionAttribution, Via, Work,
};
use agent_tapes_core::git_reader::GitReader;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use tempfile::TempDir;

type Time = DateTime<Utc>;

/// An instant of the fixture day.
fn at(time: &str) -> Time {
    format!("2026-01-12T{time}Z").parse().unwrap()
}

fn stamp(time: Time) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn second(time: Time) -> Time {
    time + Duration::seconds(1)
}

fn short(sha: &str) -> &str {
    &sha[..7]
}

/// The lines a commit adds: distinct enough to digest apart.
fn lines(name: &str) -> Vec<String> {
    (1..=3)
        .map(|n| format!("pub const {}_{n}: u32 = {n};", name.to_uppercase()))
        .collect()
}

// ------------------------------------------------------------------ git

/// A repository with an `origin` remote and one base commit on `main`, and
/// the stores of three harnesses beside it. git sees no user or system
/// configuration.
struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let sandbox = Self {
            root: TempDir::with_prefix("tapes-change-attribution-").unwrap(),
        };
        fs::write(sandbox.dir("gitconfig"), "").unwrap();
        fs::create_dir(sandbox.repo()).unwrap();
        sandbox.git(&sandbox.repo(), None, &["init", "-q", "-b", "main"]);
        sandbox.git(
            &sandbox.repo(),
            None,
            &[
                "remote",
                "add",
                "origin",
                "https://example.test/org/repo.git",
            ],
        );
        sandbox.commit(
            &sandbox.repo(),
            at("08:00:00"),
            "README.md",
            &["Fixture repository."],
            "base",
        );
        sandbox
    }

    fn dir(&self, name: &str) -> PathBuf {
        fs::canonicalize(self.root.path()).unwrap().join(name)
    }

    fn repo(&self) -> PathBuf {
        self.dir("repo")
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Fixture Author"])
            .args(["-c", "user.email=author@example.invalid"])
            .args(["-c", "commit.gpgsign=false"])
            .args(["-c", "core.hooksPath=/dev/null"])
            .env("GIT_CONFIG_GLOBAL", self.dir("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", self.dir("xdg"));
        command
    }

    /// Run git in `dir`, its new commits authored and committed `when`.
    fn git(&self, dir: &Path, when: Option<Time>, args: &[&str]) -> String {
        let mut command = self.command(dir);
        command.args(args);
        if let Some(when) = when {
            command.env("GIT_AUTHOR_DATE", format!("{} +0000", when.timestamp()));
            command.env("GIT_COMMITTER_DATE", format!("{} +0000", when.timestamp()));
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// Run git in `dir`, rewritten commits committed `when` and keeping
    /// their author dates.
    fn rewrite(&self, dir: &Path, when: Time, args: &[&str]) {
        let output = self
            .command(dir)
            .args(args)
            .env("GIT_COMMITTER_DATE", format!("{} +0000", when.timestamp()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Append `added` to `file` in `dir` and commit it `when`.
    fn commit(&self, dir: &Path, when: Time, file: &str, added: &[&str], subject: &str) -> String {
        let path = dir.join(file);
        let mut body = fs::read_to_string(&path).unwrap_or_default();
        for line in added {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(&path, body).unwrap();
        self.git(dir, None, &["add", "-A"]);
        self.git(dir, Some(when), &["commit", "-q", "-m", subject]);
        self.git(dir, None, &["rev-parse", "HEAD"])
    }

    /// A worktree of the repository on a new branch from `main`.
    fn worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.dir(name);
        self.git(
            &self.repo(),
            None,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                path.to_str().unwrap(),
                "main",
            ],
        );
        path
    }

    /// Remove a worktree and delete its branch, as an integration leaves them.
    fn retire(&self, worktree: &Path, branch: &str) {
        self.git(
            &self.repo(),
            None,
            &["worktree", "remove", worktree.to_str().unwrap()],
        );
        self.git(&self.repo(), None, &["branch", "-q", "-D", branch]);
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.dir("claude/projects"))),
            Box::new(CodexBackend::new(self.dir("codex/sessions"))),
            Box::new(PiBackend::new(self.dir("pi/sessions"))),
        ]
    }

    fn repository(&self) -> Repository {
        let reader = GitReader::at(&self.repo())
            .unwrap()
            .expect("git is installed")
            .with_env("GIT_CONFIG_GLOBAL", self.dir("gitconfig"))
            .with_env("GIT_CONFIG_NOSYSTEM", "1");
        Repository::with_reader(reader, &self.repo()).unwrap()
    }

    /// The attribution of the sessions active in `[from, to]`.
    fn attribute(&self, from: Time, to: Time) -> RepositoryAttribution {
        let backends = self.backends();
        let repository = self.repository();
        let gathered = gather(&backends, &repository, from, to).unwrap();
        attribute(&backends, &repository, &gathered, &Params::default()).unwrap()
    }

    fn day(&self) -> RepositoryAttribution {
        self.attribute(at("07:00:00"), at("18:00:00"))
    }

    fn write(&self, path: PathBuf, records: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }
}

// ----------------------------------------------------------- recordings

/// A Claude Code recording: a lead under its project directory, or a
/// subagent under its lead's `subagents` directory.
struct Claude {
    session: String,
    agent: Option<String>,
    cwd: PathBuf,
    branch: Option<String>,
    records: Vec<Value>,
    parent: Option<String>,
    serial: usize,
}

impl Claude {
    fn lead(session: &str, cwd: &Path, branch: &str) -> Self {
        Self {
            session: session.to_owned(),
            agent: None,
            cwd: cwd.to_owned(),
            branch: Some(branch.to_owned()),
            records: Vec::new(),
            parent: None,
            serial: 0,
        }
    }

    fn agent(lead: &Self, agent: &str) -> Self {
        Self {
            agent: Some(agent.to_owned()),
            ..Self::lead(&lead.session, &lead.cwd, lead.branch.as_deref().unwrap())
        }
    }

    fn on(&mut self, branch: &str) {
        self.branch = Some(branch.to_owned());
    }

    fn push(&mut self, kind: &str, when: Time, fields: Value) -> String {
        self.serial += 1;
        let uuid = format!(
            "{}-{kind}-{}",
            self.agent.as_deref().unwrap_or("lead"),
            self.serial
        );
        let mut record = json!({
            "parentUuid": self.parent, "isSidechain": self.agent.is_some(),
            "sessionId": self.session, "cwd": self.cwd, "userType": "external",
            "entrypoint": "cli", "version": "2.1.0", "type": kind, "uuid": uuid,
            "timestamp": stamp(when),
        });
        if let Some(branch) = &self.branch {
            record["gitBranch"] = json!(branch);
        }
        if let Some(agent) = &self.agent {
            record["agentId"] = json!(agent);
        }
        for (key, value) in fields.as_object().unwrap() {
            record[key] = value.clone();
        }
        self.records.push(record);
        self.parent = Some(uuid.clone());
        uuid
    }

    fn prompt(&mut self, when: Time) {
        self.push(
            "user",
            when,
            json!({"message": {"role": "user", "content": "Carry on."}}),
        );
    }

    fn assistant(&mut self, when: Time, content: Value) {
        let request = format!(
            "{}-{}",
            self.agent.as_deref().unwrap_or("lead"),
            self.serial + 1
        );
        self.push(
            "assistant",
            when,
            json!({"requestId": format!("req_{request}"), "message": {
                "id": format!("msg_{request}"), "role": "assistant", "model": "claude-fixture",
                "content": content, "stop_reason": "end_turn",
                "usage": {"input_tokens": 10 + self.serial as u64, "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 100, "output_tokens": 7}}}),
        );
    }

    fn reply(&mut self, when: Time) {
        self.assistant(when, json!([{"type": "text", "text": "Done."}]));
    }

    fn call(&mut self, when: Time, name: &str, input: Value, text: &str, result: Value) {
        let id = format!("toolu_{}", self.serial + 1);
        self.assistant(
            when,
            json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
        );
        self.push(
            "user",
            second(when),
            json!({"message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": text, "is_error": false}]},
                "toolUseResult": result}),
        );
    }

    fn bash(&mut self, when: Time, command: &str, stdout: &str) {
        self.call(
            when,
            "Bash",
            json!({"command": command}),
            stdout,
            json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false}),
        );
    }

    /// A commit Claude Code records structurally, as `committed` or
    /// `amended`.
    fn commit(&mut self, when: Time, sha: &str, kind: &str, subject: &str) {
        let branch = self.branch.clone().unwrap();
        let stdout = format!(
            "[{branch} {}] {subject}\n 1 file changed, 3 insertions(+)\n",
            short(sha)
        );
        let mut operation = json!({"sha": short(sha), "kind": kind});
        if kind == "committed" {
            operation["branch"] = json!(branch);
        }
        let command = if kind == "amended" {
            format!("git commit --amend -m \"{subject}\"")
        } else {
            format!("git add -A && git commit -m \"{subject}\"")
        };
        self.call(
            when,
            "Bash",
            json!({"command": command}),
            &stdout,
            json!({"stdout": stdout, "stderr": "", "interrupted": false, "isImage": false,
                "gitOperation": {"commit": operation}}),
        );
    }

    fn edit(&mut self, when: Time, file: &Path, added: &[String]) {
        let new_string = added.join("\n");
        let mut patch = vec![" // header".to_owned()];
        patch.extend(added.iter().map(|line| format!("+{line}")));
        self.call(
            when,
            "Edit",
            json!({"file_path": file, "old_string": "// header", "new_string": new_string}),
            "The file has been updated successfully.",
            json!({"filePath": file, "oldString": "// header", "newString": new_string,
                "originalFile": null, "userModified": false, "replaceAll": false,
                "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1,
                    "newLines": 1 + added.len(), "lines": patch}]}),
        );
    }

    fn spawn(&mut self, when: Time, done: Time, agent: &str) {
        let id = format!("toolu_{}", self.serial + 1);
        self.assistant(
            when,
            json!([{"type": "tool_use", "id": id, "name": "Agent",
                "input": {"description": "fixture task", "prompt": "Do the task.",
                    "subagent_type": "general-purpose"}}]),
        );
        self.push(
            "user",
            done,
            json!({"message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": "Finished.", "is_error": false}]},
                "toolUseResult": {"status": "completed", "agentId": agent,
                    "content": [{"type": "text", "text": "Finished."}]}}),
        );
    }

    /// A `cost-state` record totalling the session's requests and a model
    /// no request explains.
    fn cost_state(&mut self) {
        let requests: u64 = self
            .records
            .iter()
            .filter(|record| record["type"] == "assistant")
            .map(|record| record["message"]["usage"]["input_tokens"].as_u64().unwrap())
            .sum();
        let count = self
            .records
            .iter()
            .filter(|record| record["type"] == "assistant")
            .count() as u64;
        let usage = |input: u64, output: u64, read: u64| {
            json!({"inputTokens": input, "outputTokens": output, "thinkingTokens": 0,
                "cacheReadInputTokens": read, "cacheCreationInputTokens": 0,
                "webSearchRequests": 0, "costUSD": 0.25})
        };
        self.records.push(json!({"type": "cost-state", "sessionId": self.session,
            "totalCostUSD": 0.5, "totalAPIDuration": 4000, "totalAPIDurationWithoutRetries": 3900,
            "totalToolDuration": 2000, "totalLinesAdded": 0, "totalLinesRemoved": 0,
            "totalDuration": 600_000, "startTime": 1_768_212_000_000_i64, "hasUnknownModelCost": false,
            "modelUsage": {"claude-fixture": usage(requests, 7 * count, 100 * count),
                "claude-fixture-small": usage(301, 41, 1001)}}));
    }

    /// The project directory Claude Code names for a working directory.
    fn project(sandbox: &Sandbox, cwd: &Path) -> PathBuf {
        let name: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        sandbox.dir("claude/projects").join(name)
    }

    /// Write a lead at its project, or a subagent under `lead`'s.
    fn write(&self, sandbox: &Sandbox, lead_cwd: &Path) {
        let project = Self::project(sandbox, lead_cwd);
        match &self.agent {
            None => sandbox.write(
                project.join(format!("{}.jsonl", self.session)),
                &self.records,
            ),
            Some(agent) => {
                let subagents = project.join(&self.session).join("subagents");
                sandbox.write(
                    subagents.join(format!("agent-{agent}.jsonl")),
                    &self.records,
                );
                fs::write(
                    subagents.join(format!("agent-{agent}.meta.json")),
                    json!({"agentType": "general-purpose", "description": "fixture task",
                        "spawnDepth": 1})
                    .to_string(),
                )
                .unwrap();
            }
        }
    }
}

/// A Codex rollout.
struct Codex {
    id: String,
    started: Time,
    cwd: PathBuf,
    records: Vec<Value>,
    total: u64,
    serial: usize,
}

impl Codex {
    fn new(id: &str, started: Time, cwd: &Path, branch: &str) -> Self {
        let mut codex = Self {
            id: id.to_owned(),
            started,
            cwd: cwd.to_owned(),
            records: Vec::new(),
            total: 0,
            serial: 0,
        };
        codex.push(
            started,
            json!({"type": "session_meta", "payload": {
            "id": id, "timestamp": stamp(started), "cwd": cwd, "originator": "codex_exec",
            "cli_version": "0.80.0", "source": "exec", "model_provider": "openai",
            "git": {"commit_hash": "0000000000000000000000000000000000000000", "branch": branch,
                "repository_url": "git@example.test:org/repo.git"}}}),
        );
        codex.push(
            started,
            json!({"type": "turn_context", "payload": {
            "cwd": cwd, "model": "gpt-fixture", "effort": "high"}}),
        );
        codex.push(
            started,
            json!({"type": "response_item", "payload": {"type": "message",
            "role": "user", "content": [{"type": "input_text", "text": "Carry on."}]}}),
        );
        codex
    }

    fn push(&mut self, when: Time, record: Value) {
        let mut record = record;
        record["timestamp"] = json!(stamp(when));
        self.records.push(record);
    }

    fn request(&mut self, when: Time) {
        self.serial += 1;
        let input = 20 + self.serial as u64;
        self.total += input;
        let usage = |input: u64, requests: u64| {
            json!({"input_tokens": input, "cached_input_tokens": 0,
                "output_tokens": 5 * requests, "reasoning_output_tokens": requests,
                "total_tokens": input + 5 * requests})
        };
        let total = usage(self.total, self.serial as u64);
        self.push(
            when,
            json!({"type": "event_msg", "payload": {"type": "token_count",
            "info": {"total_token_usage": total, "last_token_usage": usage(input, 1),
                "model_context_window": 200000}, "rate_limits": null}}),
        );
    }

    fn reply(&mut self, when: Time) {
        self.push(
            when,
            json!({"type": "response_item", "payload": {"type": "message",
            "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}}),
        );
        self.request(when);
    }

    fn exec(&mut self, when: Time, cmd: &str, output: &str) {
        let call = format!("call_{}", self.serial + 1);
        let arguments = json!({"cmd": cmd, "workdir": self.cwd, "yield_time_ms": 1000}).to_string();
        self.push(
            when,
            json!({"type": "response_item", "payload": {"type": "function_call",
            "name": "exec_command", "arguments": arguments, "call_id": call}}),
        );
        self.request(when);
        self.push(second(when), json!({"type": "response_item", "payload": {
            "type": "function_call_output", "call_id": call,
            "output": format!("Chunk ID: 000000\nWall time: 0.1000 seconds\nProcess exited with code 0\nOriginal token count: 20\nOutput:\n{output}")}}));
    }

    fn commit(&mut self, when: Time, branch: &str, sha: &str, subject: &str) {
        self.exec(
            when,
            &format!("git add -A && git commit -m \"{subject}\""),
            &format!(
                "[{branch} {}] {subject}\n 1 file changed, 3 insertions(+)\n",
                short(sha)
            ),
        );
    }

    fn write(&self, sandbox: &Sandbox) {
        let day = self.started.format("%Y/%m/%d");
        let name = format!(
            "rollout-{}-{}.jsonl",
            self.started.format("%Y-%m-%dT%H-%M-%S"),
            self.id
        );
        sandbox.write(
            sandbox
                .dir("codex/sessions")
                .join(day.to_string())
                .join(name),
            &self.records,
        );
    }
}

/// A pi session file.
struct Pi {
    id: String,
    started: Time,
    cwd: PathBuf,
    records: Vec<Value>,
    parent: Option<String>,
    serial: usize,
}

impl Pi {
    fn new(id: &str, started: Time, cwd: &Path) -> Self {
        let mut pi = Self {
            id: id.to_owned(),
            started,
            cwd: cwd.to_owned(),
            records: vec![json!({"type": "session", "version": 3, "id": id,
                "timestamp": stamp(started), "cwd": cwd})],
            parent: None,
            serial: 0,
        };
        pi.message(
            started,
            json!({"role": "user", "content": [{"type": "text", "text": "Carry on."}]}),
        );
        pi
    }

    fn message(&mut self, when: Time, message: Value) {
        self.serial += 1;
        let id = format!("m{}", self.serial);
        let mut message = message;
        message["timestamp"] = json!(when.timestamp_millis());
        self.records
            .push(json!({"type": "message", "id": id, "parentId": self.parent,
            "timestamp": stamp(when), "message": message}));
        self.parent = Some(id);
    }

    fn assistant(&mut self, when: Time, content: Value) {
        let input = 30 + self.serial as u64;
        self.message(
            when,
            json!({"role": "assistant", "api": "openai-responses",
            "provider": "openai", "model": "gpt-fixture", "stopReason": "toolUse",
            "usage": {"input": input, "output": 3, "cacheRead": 50, "cacheWrite": 0,
                "totalTokens": input + 53, "cost": {"total": 0.001}},
            "content": content}),
        );
    }

    fn reply(&mut self, when: Time) {
        self.assistant(when, json!([{"type": "text", "text": "Done."}]));
    }

    fn call(
        &mut self,
        when: Time,
        name: &str,
        arguments: Value,
        text: &str,
        details: Option<Value>,
    ) {
        let call = format!("call_{0}|fc_{0}", self.serial + 1);
        self.assistant(
            when,
            json!([{"type": "toolCall", "id": call, "name": name, "arguments": arguments}]),
        );
        let mut result = json!({"role": "toolResult", "toolCallId": call, "toolName": name,
            "content": [{"type": "text", "text": text}], "isError": false});
        if let Some(details) = details {
            result["details"] = details;
        }
        self.message(second(when), result);
    }

    fn commit(&mut self, when: Time, branch: &str, sha: &str, subject: &str) {
        self.call(
            when,
            "bash",
            json!({"command": format!("git add -A && git commit -m \"{subject}\"")}),
            &format!(
                "[{branch} {}] {subject}\n 1 file changed, 3 insertions(+)\n",
                short(sha)
            ),
            None,
        );
    }

    fn edit(&mut self, when: Time, file: &Path, added: &[String]) {
        let name = file.to_string_lossy();
        let mut patch = format!(
            "--- {name}\n+++ {name}\n@@ -1,1 +1,{} @@\n // header\n",
            1 + added.len()
        );
        for line in added {
            patch.push_str(&format!("+{line}\n"));
        }
        self.call(
            when,
            "edit",
            json!({"path": file, "edits": [{"oldText": "// header\n",
                "newText": format!("// header\n{}\n", added.join("\n"))}]}),
            &format!("Successfully replaced 1 block(s) in {name}."),
            Some(json!({"diff": "", "patch": patch, "firstChangedLine": 2})),
        );
    }

    fn write(&self, sandbox: &Sandbox) {
        let directory = format!(
            "--{}--",
            self.cwd
                .to_string_lossy()
                .trim_start_matches('/')
                .replace('/', "-")
        );
        let name = format!(
            "{}_{}.jsonl",
            self.started.format("%Y-%m-%dT%H-%M-%S-000Z"),
            self.id
        );
        sandbox.write(
            sandbox.dir("pi/sessions").join(directory).join(name),
            &self.records,
        );
    }
}

// ------------------------------------------------------------ scenarios

/// The commits a scenario made, by the name its expectations use.
type Names = BTreeMap<String, String>;

const INTERACTIVE: &str = "c1a00000-0000-4000-8000-000000000001";
const DELEGATE_A: &str = "c1a00000-0000-4000-8000-000000000002";
const DELEGATE_B: &str = "019b0000-0000-7000-8000-000000000003";
const REVIEWER: &str = "019b0000-0000-7000-8000-000000000004";
const LEAD: &str = "c1a00000-0000-4000-8000-000000000005";
const EDITOR: &str = "a0e0000000000000e";
const READER: &str = "a0f0000000000000f";
const SQUASHED: &str = "c1a00000-0000-4000-8000-000000000006";
const AMENDED: &str = "c1a00000-0000-4000-8000-000000000007";
const LINGERING: &str = "c1a00000-0000-4000-8000-000000000008";
const LONG: &str = "c1a00000-0000-4000-8000-000000000009";
const ON_MAIN: &str = "c1a00000-0000-4000-8000-00000000000a";

/// An interactive session in the main checkout: on `feat-a` it edits and
/// commits c1 at 10:08 and talks on; at 10:12 it switches to `feat-b`, where
/// it commits c2 at 10:18 and c3 at 10:25 and talks on. A `cost-state`
/// totals a model no request explains.
fn interactive(sandbox: &Sandbox) -> Names {
    let repo = sandbox.repo();
    sandbox.git(&repo, None, &["branch", "feat-a"]);
    sandbox.git(&repo, None, &["branch", "feat-b"]);
    sandbox.git(&repo, None, &["switch", "-q", "feat-a"]);
    let c1_lines = lines("alpha");
    let c1 = sandbox.commit(&repo, at("10:08:00"), "a.rs", &refs(&c1_lines), "c1");
    sandbox.git(&repo, None, &["switch", "-q", "feat-b"]);
    let c2 = sandbox.commit(&repo, at("10:18:00"), "b.rs", &refs(&lines("beta")), "c2");
    let c3 = sandbox.commit(&repo, at("10:25:00"), "b.rs", &refs(&lines("gamma")), "c3");
    sandbox.git(&repo, None, &["switch", "-q", "main"]);

    let mut claude = Claude::lead(INTERACTIVE, &repo, "feat-a");
    claude.prompt(at("10:00:00"));
    claude.reply(at("10:00:05"));
    claude.edit(at("10:04:00"), &repo.join("a.rs"), &c1_lines);
    claude.commit(at("10:08:00"), &c1, "committed", "c1");
    claude.reply(at("10:09:00"));
    claude.assistant(
        at("10:12:00"),
        json!([{"type": "tool_use", "id": "toolu_switch", "name": "Bash",
            "input": {"command": "git switch feat-b"}}]),
    );
    claude.on("feat-b");
    claude.push(
        "user",
        at("10:12:01"),
        json!({"message": {"role": "user", "content": [{"type": "tool_result",
            "tool_use_id": "toolu_switch", "content": "Switched to branch 'feat-b'",
            "is_error": false}]}}),
    );
    claude.reply(at("10:15:00"));
    claude.commit(at("10:18:00"), &c2, "committed", "c2");
    claude.reply(at("10:22:00"));
    claude.commit(at("10:25:00"), &c3, "committed", "c3");
    claude.reply(at("10:28:00"));
    claude.cost_state();
    claude.write(sandbox, &repo);
    names(&[("c1", &c1), ("c2", &c2), ("c3", &c3)])
}

/// Two delegates in their own worktrees, a Claude one on `lane-a` committing
/// a1 and a2 and a Codex one on `lane-b` committing b1 and b2, integrated by
/// fast-forward with `lane-b` rebased first; the worktrees and branches are
/// then removed.
fn delegates(sandbox: &Sandbox) -> Names {
    let repo = sandbox.repo();
    let wt_a = sandbox.worktree("wt-a", "lane-a");
    let wt_b = sandbox.worktree("wt-b", "lane-b");
    let a1 = sandbox.commit(
        &wt_a,
        at("11:05:00"),
        "lane_a.rs",
        &refs(&lines("delta")),
        "a1",
    );
    let b1 = sandbox.commit(
        &wt_b,
        at("11:06:00"),
        "lane_b.rs",
        &refs(&lines("epsilon")),
        "b1",
    );
    let a2 = sandbox.commit(
        &wt_a,
        at("11:15:00"),
        "lane_a.rs",
        &refs(&lines("zeta")),
        "a2",
    );
    let b2 = sandbox.commit(
        &wt_b,
        at("11:16:00"),
        "lane_b.rs",
        &refs(&lines("eta")),
        "b2",
    );
    sandbox.git(&repo, None, &["merge", "-q", "--ff-only", "lane-a"]);
    sandbox.rewrite(&wt_b, at("11:21:00"), &["rebase", "-q", "main"]);
    sandbox.git(&repo, None, &["merge", "-q", "--ff-only", "lane-b"]);
    let b2_landed = sandbox.git(&repo, None, &["rev-parse", "main"]);
    let b1_landed = sandbox.git(&repo, None, &["rev-parse", "main~1"]);
    sandbox.retire(&wt_a, "lane-a");
    sandbox.retire(&wt_b, "lane-b");

    let mut claude = Claude::lead(DELEGATE_A, &wt_a, "lane-a");
    claude.prompt(at("11:00:00"));
    claude.reply(at("11:00:05"));
    claude.commit(at("11:05:00"), &a1, "committed", "a1");
    claude.reply(at("11:10:00"));
    claude.commit(at("11:15:00"), &a2, "committed", "a2");
    claude.write(sandbox, &wt_a);

    let mut codex = Codex::new(DELEGATE_B, at("11:01:00"), &wt_b, "lane-b");
    codex.reply(at("11:01:05"));
    codex.commit(at("11:06:00"), "lane-b", &b1, "b1");
    codex.reply(at("11:12:00"));
    codex.commit(at("11:16:00"), "lane-b", &b2, "b2");
    codex.write(sandbox);
    names(&[
        ("a1", &a1),
        ("a2", &a2),
        ("b1", &b1_landed),
        ("b2", &b2_landed),
    ])
}

/// Three commits on `main` no session made, and a Codex reviewer in the
/// main checkout that runs `git diff X1..X3`, then `git show X2`.
fn reviewer(sandbox: &Sandbox) -> Names {
    let repo = sandbox.repo();
    let x1 = sandbox.commit(&repo, at("11:40:00"), "x.rs", &refs(&lines("theta")), "x1");
    let x2 = sandbox.commit(&repo, at("11:45:00"), "x.rs", &refs(&lines("iota")), "x2");
    let x3 = sandbox.commit(&repo, at("11:50:00"), "x.rs", &refs(&lines("kappa")), "x3");

    let mut codex = Codex::new(REVIEWER, at("12:00:00"), &repo, "main");
    codex.reply(at("12:00:05"));
    codex.exec(
        at("12:02:00"),
        &format!("git diff {}..{}", short(&x1), short(&x3)),
        "diff --git a/x.rs b/x.rs\n",
    );
    codex.reply(at("12:05:00"));
    codex.exec(
        at("12:08:00"),
        &format!("git show {}", short(&x2)),
        &format!("commit {x2}\nAuthor: Fixture Author <author@example.invalid>\n\n    x2\n"),
    );
    codex.reply(at("12:10:00"));
    codex.write(sandbox);
    names(&[("x1", &x1), ("x2", &x2), ("x3", &x3)])
}

/// A Claude lead in a worktree on `lane-c` spawns an editor subagent that
/// writes the lines c4 adds and a reader subagent that only reads, then
/// commits c4 at 13:20; the lane is integrated and removed.
fn subagents(sandbox: &Sandbox) -> Names {
    let repo = sandbox.repo();
    let wt_c = sandbox.worktree("wt-c", "lane-c");
    let c4_lines = lines("lambda");
    let c4 = sandbox.commit(&wt_c, at("13:20:00"), "lane_c.rs", &refs(&c4_lines), "c4");
    sandbox.git(&repo, None, &["merge", "-q", "--ff-only", "lane-c"]);
    sandbox.retire(&wt_c, "lane-c");

    let mut lead = Claude::lead(LEAD, &wt_c, "lane-c");
    lead.prompt(at("13:00:00"));
    lead.reply(at("13:00:05"));
    lead.spawn(at("13:02:00"), at("13:07:00"), EDITOR);
    lead.spawn(at("13:08:00"), at("13:12:00"), READER);
    lead.reply(at("13:15:00"));
    lead.commit(at("13:20:00"), &c4, "committed", "c4");

    let mut editor = Claude::agent(&lead, EDITOR);
    editor.prompt(at("13:02:30"));
    editor.reply(at("13:03:00"));
    editor.edit(at("13:05:00"), &wt_c.join("lane_c.rs"), &c4_lines);
    editor.reply(at("13:06:00"));

    let mut reader = Claude::agent(&lead, READER);
    reader.prompt(at("13:08:30"));
    reader.reply(at("13:09:00"));
    reader.bash(at("13:10:00"), "cat lane_c.rs", "// header\n");
    reader.reply(at("13:11:00"));

    lead.write(sandbox, &wt_c);
    editor.write(sandbox, &wt_c);
    reader.write(sandbox, &wt_c);
    names(&[("c4", &c4)])
}

/// A pi session on `lane-s` edits and commits s1 and s2; the lane is then
/// squash-merged into `main` as S and removed.
fn squash(sandbox: &Sandbox) -> Names {
    let repo = sandbox.repo();
    let wt_s = sandbox.worktree("wt-s", "lane-s");
    let s1_lines = lines("mu");
    let s2_lines = lines("nu");
    let s1 = sandbox.commit(&wt_s, at("14:10:00"), "lane_s.rs", &refs(&s1_lines), "s1");
    let s2 = sandbox.commit(&wt_s, at("14:20:00"), "lane_s.rs", &refs(&s2_lines), "s2");
    sandbox.git(&repo, None, &["merge", "-q", "--squash", "lane-s"]);
    sandbox.git(
        &repo,
        Some(at("14:30:00")),
        &["commit", "-q", "-m", "squashed lane-s"],
    );
    let squashed = sandbox.git(&repo, None, &["rev-parse", "main"]);
    sandbox.retire(&wt_s, "lane-s");

    let mut pi = Pi::new(SQUASHED, at("14:00:00"), &wt_s);
    pi.reply(at("14:00:05"));
    pi.edit(at("14:05:00"), &wt_s.join("lane_s.rs"), &s1_lines);
    pi.commit(at("14:10:00"), "lane-s", &s1, "s1");
    pi.edit(at("14:15:00"), &wt_s.join("lane_s.rs"), &s2_lines);
    pi.commit(at("14:20:00"), "lane-s", &s2, "s2");
    pi.write(sandbox);
    names(&[("S", &squashed)])
}

/// A Claude session on `lane-r` commits r1 at 15:05 and amends its message
/// at 15:10; `main` moves on, the lane is rebased onto it at 15:30,
/// integrated and removed.
fn amended(sandbox: &Sandbox) -> Names {
    let repo = sandbox.repo();
    let wt_r = sandbox.worktree("wt-r", "lane-r");
    let r1 = sandbox.commit(
        &wt_r,
        at("15:05:00"),
        "lane_r.rs",
        &refs(&lines("xi")),
        "r1",
    );
    sandbox.rewrite(
        &wt_r,
        at("15:10:00"),
        &["commit", "-q", "--amend", "-m", "r1 reworded"],
    );
    let r1_amended = sandbox.git(&wt_r, None, &["rev-parse", "HEAD"]);
    sandbox.commit(
        &repo,
        at("15:20:00"),
        "main.rs",
        &refs(&lines("omicron")),
        "m",
    );
    sandbox.rewrite(&wt_r, at("15:30:00"), &["rebase", "-q", "main"]);
    sandbox.git(&repo, None, &["merge", "-q", "--ff-only", "lane-r"]);
    let landed = sandbox.git(&repo, None, &["rev-parse", "main"]);
    sandbox.retire(&wt_r, "lane-r");

    let mut claude = Claude::lead(AMENDED, &wt_r, "lane-r");
    claude.prompt(at("15:00:00"));
    claude.reply(at("15:00:05"));
    claude.commit(at("15:05:00"), &r1, "committed", "r1");
    claude.reply(at("15:08:00"));
    claude.commit(at("15:10:00"), &r1_amended, "amended", "r1 reworded");
    claude.reply(at("15:12:00"));
    claude.write(sandbox, &wt_r);
    names(&[("r1", &landed)])
}

/// An interactive session in the main checkout commits m1 on `main` at
/// 10:10 and talks on; at 10:20 another commit, n, lands on `main`.
fn on_main(sandbox: &Sandbox) -> Names {
    let repo = sandbox.repo();
    let m1 = sandbox.commit(&repo, at("10:10:00"), "m.rs", &refs(&lines("sigma")), "m1");
    let n = sandbox.commit(&repo, at("10:20:00"), "n.rs", &refs(&lines("tau")), "n");

    let mut claude = Claude::lead(ON_MAIN, &repo, "main");
    claude.prompt(at("10:00:00"));
    claude.reply(at("10:00:05"));
    claude.reply(at("10:05:00"));
    claude.commit(at("10:10:00"), &m1, "committed", "m1");
    claude.reply(at("10:12:00"));
    claude.reply(at("10:15:00"));
    claude.reply(at("10:25:00"));
    claude.reply(at("10:30:00"));
    claude.write(sandbox, &repo);
    names(&[("m1", &m1), ("n", &n)])
}

/// A pi session in a live worktree on `lane-u` commits u1 at 16:05 and works
/// on without committing again.
fn lingering(sandbox: &Sandbox) -> Names {
    let wt_u = sandbox.worktree("wt-u", "lane-u");
    let u1 = sandbox.commit(
        &wt_u,
        at("16:05:00"),
        "lane_u.rs",
        &refs(&lines("pi")),
        "u1",
    );

    let mut pi = Pi::new(LINGERING, at("16:00:00"), &wt_u);
    pi.reply(at("16:00:05"));
    pi.commit(at("16:05:00"), "lane-u", &u1, "u1");
    pi.reply(at("16:10:00"));
    pi.reply(at("16:15:00"));
    pi.write(sandbox);
    names(&[("u1", &u1)])
}

/// A pi session in a live worktree on `lane-l` that starts at 09:00, works
/// steadily and commits l1 at 10:30: it started before l1's window opens.
fn long(sandbox: &Sandbox) -> Names {
    let wt_l = sandbox.worktree("wt-l", "lane-l");
    let l1 = sandbox.commit(
        &wt_l,
        at("10:30:00"),
        "lane_l.rs",
        &refs(&lines("rho")),
        "l1",
    );

    let mut pi = Pi::new(LONG, at("09:00:00"), &wt_l);
    for time in ["09:00:05", "09:20:00", "09:40:00", "10:00:00", "10:20:00"] {
        pi.reply(at(time));
    }
    pi.commit(at("10:30:00"), "lane-l", &l1, "l1");
    pi.write(sandbox);
    names(&[("l1", &l1)])
}

fn refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

fn names(pairs: &[(&str, &String)]) -> Names {
    pairs
        .iter()
        .map(|(name, sha)| ((*sha).clone(), (*name).to_owned()))
        .collect()
}

// --------------------------------------------------------------- reading

fn session<'a>(attribution: &'a RepositoryAttribution, id: &str) -> &'a SessionAttribution {
    attribution
        .sessions
        .iter()
        .find(|session| session.session.id.contains(id))
        .unwrap_or_else(|| {
            panic!(
                "no session {id} among {:?}",
                attribution
                    .sessions
                    .iter()
                    .map(|session| &session.session)
                    .collect::<Vec<_>>()
            )
        })
}

fn label(work: &Work, names: &Names) -> String {
    match work {
        Work::Commit { sha } => names
            .get(sha)
            .cloned()
            .unwrap_or_else(|| short(sha).to_owned()),
        Work::Uncommitted { branch, .. } => {
            format!("uncommitted {}", branch.as_deref().unwrap_or("-"))
        }
    }
}

/// The requests of each owner of a session, unattributed parts by reason.
fn requests(session: &SessionAttribution, names: &Names) -> BTreeMap<String, u64> {
    let mut held: BTreeMap<String, u64> = BTreeMap::new();
    for share in session
        .shares()
        .into_iter()
        .filter(|share| !share.amount.is_zero())
    {
        *held.entry(label(&share.work, names)).or_default() += share.amount.requests;
    }
    for (reason, amount) in session.unattributed() {
        if amount.requests > 0 {
            *held
                .entry(format!("unattributed {}", reason.as_str()))
                .or_default() += amount.requests;
        }
    }
    held
}

/// The tiers and bases of each owner's shares.
fn labels(
    session: &SessionAttribution,
    names: &Names,
) -> BTreeMap<String, BTreeSet<(Tier, Basis)>> {
    let mut held: BTreeMap<String, BTreeSet<(Tier, Basis)>> = BTreeMap::new();
    for share in session.shares() {
        if share.amount.requests > 0 {
            held.entry(label(&share.work, names))
                .or_default()
                .insert((share.evidence.tier, share.evidence.basis));
        }
    }
    held
}

fn expect(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs
        .iter()
        .map(|(label, requests)| ((*label).to_owned(), *requests))
        .collect()
}

// ------------------------------------------------------------------ tests

/// Claim: an interactive session in the main checkout that switches branches
/// is divided by its own commits on each branch, and what it does after its
/// last commit on a branch is that branch's uncommitted work. Rejects a
/// division by place alone, which runs the `feat-a` tail into c2.
#[test]
fn interactive_main_checkout_divides_by_branch_and_commit() {
    let sandbox = Sandbox::new();
    let names = interactive(&sandbox);
    let attribution = sandbox.day();
    let session = session(&attribution, INTERACTIVE);
    assert_eq!(
        requests(session, &names),
        expect(&[
            ("c1", 3),
            ("c2", 2),
            ("c3", 2),
            ("uncommitted feat-a", 2),
            ("uncommitted feat-b", 1),
        ])
    );
    let labels = labels(session, &names);
    for commit in ["c1", "c2", "c3"] {
        assert!(
            labels[commit].contains(&(Tier::Matched, Basis::Span)),
            "{commit}: {:?}",
            labels[commit]
        );
    }
    let evidence = &session.partition.evidence;
    let bound: BTreeSet<String> = evidence
        .iter()
        .filter(|admitted| admitted.kind == Kind::Created)
        .flat_map(|admitted| admitted.work.iter().map(|work| label(work, &names)))
        .collect();
    assert_eq!(
        bound,
        BTreeSet::from(["c1".into(), "c2".into(), "c3".into()])
    );
    assert!(evidence.iter().any(|admitted| admitted.kind == Kind::Edited
        && admitted.work == vec![Work::commit(names_sha(&names, "c1"))]));
}

/// Claim: delegates whose worktrees and branches are gone are still members,
/// by the commits they made, and each is divided by its own commits; a
/// commit rebased before integration resolves to the landed commit by its
/// patch id and is still bound evidence. Rejects membership by present
/// location only, which loses both delegates.
#[test]
fn removed_worktree_delegates_divide_by_their_own_commits() {
    let sandbox = Sandbox::new();
    let names = delegates(&sandbox);
    let attribution = sandbox.day();

    let claude = session(&attribution, DELEGATE_A);
    assert_eq!(requests(claude, &names), expect(&[("a1", 2), ("a2", 2)]));
    let codex = session(&attribution, DELEGATE_B);
    assert_eq!(requests(codex, &names), expect(&[("b1", 2), ("b2", 2)]));
    let created: Vec<(Tier, Option<Via>)> = codex
        .partition
        .evidence
        .iter()
        .filter(|admitted| admitted.kind == Kind::Created)
        .map(|admitted| (admitted.tier, admitted.via))
        .collect();
    assert_eq!(created, vec![(Tier::Bound, Some(Via::PatchId)); 2]);
    assert_eq!(
        labels(codex, &names)["b1"],
        BTreeSet::from([(Tier::Matched, Basis::Span)])
    );
}

/// Claim: a reviewer's inspections are estimated evidence that only cuts its
/// working stretch: `git diff X1..X3` gives X1 and X3 even shares of the
/// requests up to it, and `git show X2` takes the rest. Rejects inspections
/// that span from one to the next, which overlap and split everything.
#[test]
fn reviewer_inspections_cut_the_working_stretch() {
    let sandbox = Sandbox::new();
    let names = reviewer(&sandbox);
    let attribution = sandbox.day();
    let session = session(&attribution, REVIEWER);
    assert_eq!(
        requests(session, &names),
        expect(&[("x1", 1), ("x2", 3), ("x3", 1)])
    );
    let labels = labels(session, &names);
    assert_eq!(
        labels["x1"],
        BTreeSet::from([(Tier::Estimated, Basis::Even)])
    );
    assert_eq!(
        labels["x2"],
        BTreeSet::from([(Tier::Estimated, Basis::WorkingStretch)])
    );
}

/// Claim: a subagent that edits is placed by its own edits; one that only
/// reads inherits its lead's allocation at the time of its requests, its
/// own proximity to a commit not counting. Rejects a reader placed by
/// proximity and an editor overridden by inheritance.
#[test]
fn subagents_place_by_own_evidence_then_inherit() {
    let sandbox = Sandbox::new();
    let names = subagents(&sandbox);
    let attribution = sandbox.day();

    let lead = session(&attribution, LEAD);
    assert!(lead.parent.is_none());
    assert_eq!(requests(lead, &names), expect(&[("c4", 5)]));

    let reader = session(&attribution, READER);
    assert_eq!(requests(reader, &names), expect(&[("c4", 3)]));
    assert_eq!(
        labels(reader, &names)["c4"],
        BTreeSet::from([(Tier::Matched, Basis::Inherited)])
    );

    let editor = session(&attribution, EDITOR);
    assert!(editor.parent.is_some());
    assert_eq!(requests(editor, &names), expect(&[("c4", 3)]));
    assert!(
        labels(editor, &names)["c4"]
            .iter()
            .all(|(tier, basis)| *tier == Tier::Matched && *basis != Basis::Inherited),
        "{:?}",
        labels(editor, &names)
    );
}

/// Claim: commits squash-merged under a new sha resolve to the squash
/// commit through the lines they add, one tier down: the session's whole
/// work lands on the squash commit, matched, never bound. Rejects a
/// digest-only resolution that keeps the bound tier.
#[test]
fn squashed_commits_resolve_by_digests_a_tier_down() {
    let sandbox = Sandbox::new();
    let names = squash(&sandbox);
    let attribution = sandbox.day();
    let session = session(&attribution, SQUASHED);
    assert_eq!(requests(session, &names), expect(&[("S", 5)]));
    assert!(
        labels(session, &names)["S"]
            .iter()
            .all(|(tier, _)| *tier == Tier::Matched),
        "{:?}",
        labels(session, &names)
    );
    let created: Vec<(Tier, Option<Via>)> = session
        .partition
        .evidence
        .iter()
        .filter(|admitted| admitted.kind == Kind::Created)
        .map(|admitted| (admitted.tier, admitted.via))
        .collect();
    assert_eq!(created, vec![(Tier::Matched, Some(Via::Digests)); 2]);
}

/// Claim: an amended and then rebased commit resolves by its stable patch
/// id to the commit that landed, keeping the bound tier, and the work after
/// the amend is the lane's uncommitted work. Rejects resolution by exact
/// sha alone, which leaves the session without its commit.
#[test]
fn amended_and_rebased_commit_resolves_by_patch_id() {
    let sandbox = Sandbox::new();
    let names = amended(&sandbox);
    let attribution = sandbox.day();
    let session = session(&attribution, AMENDED);
    assert_eq!(
        requests(session, &names),
        expect(&[("r1", 4), ("uncommitted lane-r", 1)])
    );
    let created: Vec<(Tier, Option<Via>)> = session
        .partition
        .evidence
        .iter()
        .filter(|admitted| admitted.kind == Kind::Created)
        .map(|admitted| (admitted.tier, admitted.via))
        .collect();
    assert_eq!(created, vec![(Tier::Bound, Some(Via::PatchId)); 2]);
}

/// Claim: work in a worktree after the session's last commit there is the
/// worktree's uncommitted bucket, named by its branch. Rejects running the
/// tail into the last commit.
#[test]
fn work_after_the_last_commit_is_uncommitted() {
    let sandbox = Sandbox::new();
    let names = lingering(&sandbox);
    let attribution = sandbox.day();
    let session = session(&attribution, LINGERING);
    assert_eq!(
        requests(session, &names),
        expect(&[("u1", 2), ("uncommitted lane-u", 2)])
    );
    let uncommitted = session
        .shares()
        .into_iter()
        .find_map(|share| match share.work {
            Work::Uncommitted { worktree, branch } => Some((worktree, branch)),
            Work::Commit { .. } => None,
        });
    assert_eq!(
        uncommitted,
        Some((sandbox.dir("wt-u"), Some("lane-u".to_owned())))
    );
    assert!(session
        .partition
        .evidence
        .iter()
        .any(|admitted| admitted.kind == Kind::Created));
    let member = gather(
        &sandbox.backends(),
        &sandbox.repository(),
        at("07:00:00"),
        at("18:00:00"),
    )
    .unwrap();
    let rules = &member
        .members
        .iter()
        .find(|member| member.session.id.contains(LINGERING))
        .unwrap()
        .rules;
    assert!(rules.contains(&MemberRule::Location), "{rules:?}");
}

/// Claim: a session's uncommitted work on a branch ends when the branch's
/// next commit arrives, the default branch included, though no commit on
/// the default branch divides a stay. Rejects an uncommitted bucket that
/// runs to the end of the stay.
#[test]
fn uncommitted_work_ends_when_the_branch_moves_on() {
    let sandbox = Sandbox::new();
    let names = on_main(&sandbox);
    let attribution = sandbox.day();
    let session = session(&attribution, ON_MAIN);
    assert_eq!(
        requests(session, &names),
        expect(&[("m1", 3), ("uncommitted main", 2), ("n", 2)])
    );
    assert_eq!(
        labels(session, &names)["n"],
        BTreeSet::from([(Tier::Estimated, Basis::WorkingStretch)])
    );
}

/// The amount each commit holds across every session.
fn commit_amounts(attribution: &RepositoryAttribution) -> BTreeMap<String, Amount> {
    let mut held: BTreeMap<String, Amount> = BTreeMap::new();
    for session in &attribution.sessions {
        for share in session.shares() {
            if let Some(sha) = share.work.sha() {
                held.entry(sha.to_owned()).or_default().add(&share.amount);
            }
        }
    }
    held
}

/// A commit's window as the default parameters admit evidence for it.
fn window(sandbox: &Sandbox, sha: &str) -> (Time, Time) {
    let times = sandbox.git(
        &sandbox.repo(),
        None,
        &["show", "-s", "--format=%at %ct", sha],
    );
    let mut times = times
        .split_whitespace()
        .map(|time| DateTime::from_timestamp(time.parse().unwrap(), 0).unwrap());
    let (author, committer) = (times.next().unwrap(), times.next().unwrap());
    let params = Params::default();
    (
        author.min(committer) - params.lookback,
        author.max(committer) + params.lookahead,
    )
}

/// Claim: a commit's amount is the same whether a view selects it alone,
/// within a range, or with every commit of the day: the view chooses which
/// commits it reports, never how a session divides. Rejects a gathering
/// that keeps only sessions starting inside the view, which loses a session
/// that started before a commit's window opened.
#[test]
fn a_commits_amount_does_not_depend_on_the_selection() {
    let sandbox = Sandbox::new();
    let mut names = long(&sandbox);
    names.extend(interactive(&sandbox));
    names.extend(delegates(&sandbox));
    names.extend(reviewer(&sandbox));
    let all = commit_amounts(&sandbox.day());
    assert!(all.len() >= names.len(), "{all:?}");
    for (sha, name) in &names {
        let (from, to) = window(&sandbox, sha);
        let alone = commit_amounts(&sandbox.attribute(from, to));
        assert_eq!(alone.get(sha), all.get(sha), "{name} alone");
        assert!(
            all.get(sha).is_some_and(|amount| amount.requests > 0),
            "{name}"
        );
    }
    let range = ["c1", "c2", "c3", "a1"];
    let windows: Vec<(Time, Time)> = range
        .iter()
        .map(|name| window(&sandbox, &names_sha(&names, name)))
        .collect();
    let from = windows.iter().map(|window| window.0).min().unwrap();
    let to = windows.iter().map(|window| window.1).max().unwrap();
    let in_range = commit_amounts(&sandbox.attribute(from, to));
    for name in range {
        let sha = names_sha(&names, name);
        assert_eq!(in_range.get(&sha), all.get(&sha), "{name} in range");
    }
}

/// Claim: every session's owners and unattributed parts add up to the whole
/// session, its recorded total's unexplained part and every even split
/// included. Rejects shares that drop a split's remainder or the part no
/// piece holds.
#[test]
fn shares_and_buckets_conserve_the_session() {
    let sandbox = Sandbox::new();
    interactive(&sandbox);
    reviewer(&sandbox);
    subagents(&sandbox);
    let attribution = sandbox.day();
    assert!(attribution.sessions.len() >= 5);
    let mut unexplained = false;
    for session in &attribution.sessions {
        let amounts = session.amounts.as_ref().expect("read in pieces");
        let mut sum = Amount::default();
        for amount in session.entry_amounts().unwrap() {
            sum.add(&amount);
        }
        assert_eq!(
            sum.requests, amounts.whole.requests,
            "{:?}",
            session.session
        );
        assert_eq!(
            sum.tokens(),
            amounts.whole.tokens(),
            "{:?}",
            session.session
        );
        let mut owned = Amount::default();
        for share in session.shares() {
            owned.add(&share.amount);
        }
        for amount in session.unattributed().values() {
            owned.add(amount);
        }
        assert_eq!(
            owned.tokens(),
            amounts.whole.tokens(),
            "{:?}",
            session.session
        );
        unexplained |= !amounts.whole.unexplained.is_empty();
    }
    assert!(
        unexplained,
        "a recorded total explains more than the requests"
    );
    // An even split of an odd amount: its remainder is someone's.
    let reviewer = session(&attribution, REVIEWER);
    let pieces = &reviewer.amounts.as_ref().unwrap().pieces;
    assert!(reviewer.partition.entries.iter().any(|entry| {
        !entry.split.is_empty()
            && entry
                .piece
                .is_some_and(|piece| pieces[piece].tokens().input % 2 == 1)
    }));
}

fn names_sha(names: &Names, name: &str) -> String {
    names
        .iter()
        .find(|(_, held)| held.as_str() == name)
        .map(|(sha, _)| sha.clone())
        .unwrap()
}
