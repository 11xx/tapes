//! Synthetic repositories and recordings for the change-attribution tests:
//! a git sandbox beside the stores of three harnesses, and builders that write
//! Claude, Codex and Pi recordings under each harness's own naming.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_tapes_core::backend::{
    claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend,
};
use agent_tapes_core::change_attribution::{
    attribute, gather, Params, Repository, RepositoryAttribution,
};
use agent_tapes_core::git_reader::GitReader;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use tempfile::TempDir;

pub type Time = DateTime<Utc>;

/// An instant of the fixture day.
pub fn at(time: &str) -> Time {
    format!("2026-01-12T{time}Z").parse().unwrap()
}

pub fn stamp(time: Time) -> String {
    time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

pub fn second(time: Time) -> Time {
    time + Duration::seconds(1)
}

pub fn short(sha: &str) -> &str {
    &sha[..7]
}

/// The lines a commit adds: distinct enough to digest apart.
pub fn lines(name: &str) -> Vec<String> {
    (1..=3)
        .map(|n| format!("pub const {}_{n}: u32 = {n};", name.to_uppercase()))
        .collect()
}

// ------------------------------------------------------------------ git

/// A repository with an `origin` remote and one base commit on `main`, and
/// the stores of three harnesses beside it. git sees no user or system
/// configuration.
pub struct Sandbox {
    pub root: TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
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

    pub fn dir(&self, name: &str) -> PathBuf {
        fs::canonicalize(self.root.path()).unwrap().join(name)
    }

    pub fn repo(&self) -> PathBuf {
        self.dir("repo")
    }

    pub fn command(&self, dir: &Path) -> Command {
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
    pub fn git(&self, dir: &Path, when: Option<Time>, args: &[&str]) -> String {
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
    pub fn rewrite(&self, dir: &Path, when: Time, args: &[&str]) {
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
    pub fn commit(
        &self,
        dir: &Path,
        when: Time,
        file: &str,
        added: &[&str],
        subject: &str,
    ) -> String {
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
    pub fn worktree(&self, name: &str, branch: &str) -> PathBuf {
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
    pub fn retire(&self, worktree: &Path, branch: &str) {
        self.git(
            &self.repo(),
            None,
            &["worktree", "remove", worktree.to_str().unwrap()],
        );
        self.git(&self.repo(), None, &["branch", "-q", "-D", branch]);
    }

    pub fn backends(&self) -> Vec<Box<dyn Backend>> {
        vec![
            Box::new(ClaudeBackend::new(self.dir("claude/projects"))),
            Box::new(CodexBackend::new(self.dir("codex/sessions"))),
            Box::new(PiBackend::new(self.dir("pi/sessions"))),
        ]
    }

    pub fn repository(&self) -> Repository {
        let reader = GitReader::at(&self.repo())
            .unwrap()
            .expect("git is installed")
            .with_env("GIT_CONFIG_GLOBAL", self.dir("gitconfig"))
            .with_env("GIT_CONFIG_NOSYSTEM", "1");
        Repository::with_reader(reader, &self.repo()).unwrap()
    }

    /// The attribution of the sessions active in `[from, to]`.
    pub fn attribute(&self, from: Time, to: Time) -> RepositoryAttribution {
        let backends = self.backends();
        let repository = self.repository();
        let gathered = gather(&backends, &repository, from, to).unwrap();
        attribute(&backends, &repository, &gathered, &Params::default()).unwrap()
    }

    pub fn day(&self) -> RepositoryAttribution {
        self.attribute(at("07:00:00"), at("18:00:00"))
    }

    pub fn write(&self, path: PathBuf, records: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }
}

// ----------------------------------------------------------- recordings

/// A Claude Code recording: a lead under its project directory, or a
/// subagent under its lead's `subagents` directory.
pub struct Claude {
    pub session: String,
    pub agent: Option<String>,
    pub cwd: PathBuf,
    pub branch: Option<String>,
    pub records: Vec<Value>,
    pub parent: Option<String>,
    pub serial: usize,
    /// The 5-minute and 1-hour cache writes each later request records.
    pub cache_writes: Option<(u64, u64)>,
}

impl Claude {
    pub fn lead(session: &str, cwd: &Path, branch: &str) -> Self {
        Self {
            session: session.to_owned(),
            agent: None,
            cwd: cwd.to_owned(),
            branch: Some(branch.to_owned()),
            records: Vec::new(),
            parent: None,
            serial: 0,
            cache_writes: None,
        }
    }

    pub fn agent(lead: &Self, agent: &str) -> Self {
        Self {
            agent: Some(agent.to_owned()),
            ..Self::lead(&lead.session, &lead.cwd, lead.branch.as_deref().unwrap())
        }
    }

    pub fn on(&mut self, branch: &str) {
        self.branch = Some(branch.to_owned());
    }

    pub fn step_into(&mut self, cwd: &Path, branch: Option<&str>) {
        self.cwd = cwd.to_owned();
        self.branch = branch.map(str::to_owned);
    }

    pub fn push(&mut self, kind: &str, when: Time, fields: Value) -> String {
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

    pub fn prompt(&mut self, when: Time) {
        self.push(
            "user",
            when,
            json!({"message": {"role": "user", "content": "Carry on."}}),
        );
    }

    pub fn assistant(&mut self, when: Time, content: Value) {
        let request = format!(
            "{}-{}",
            self.agent.as_deref().unwrap_or("lead"),
            self.serial + 1
        );
        let mut usage = json!({"input_tokens": 10 + self.serial as u64,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 100, "output_tokens": 7});
        if let Some((five, hour)) = self.cache_writes {
            usage["cache_creation_input_tokens"] = json!(five + hour);
            usage["cache_creation"] =
                json!({"ephemeral_5m_input_tokens": five, "ephemeral_1h_input_tokens": hour});
        }
        self.push(
            "assistant",
            when,
            json!({"requestId": format!("req_{request}"), "message": {
                "id": format!("msg_{request}"), "role": "assistant", "model": "claude-fixture",
                "content": content, "stop_reason": "end_turn", "usage": usage}}),
        );
    }

    pub fn reply(&mut self, when: Time) {
        self.assistant(when, json!([{"type": "text", "text": "Done."}]));
    }

    pub fn call(&mut self, when: Time, name: &str, input: Value, text: &str, result: Value) {
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

    pub fn bash(&mut self, when: Time, command: &str, stdout: &str) {
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
    pub fn commit(&mut self, when: Time, sha: &str, kind: &str, subject: &str) {
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

    pub fn edit(&mut self, when: Time, file: &Path, added: &[String]) {
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

    pub fn spawn(&mut self, when: Time, done: Time, agent: &str) {
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
    pub fn cost_state(&mut self) {
        self.cost_state_covering(&[]);
    }

    /// A `cost-state` record totalling the session's requests, those of
    /// `children`, its subagents, and a model no request explains, as a
    /// lead's recorded total counts its subagents' requests.
    pub fn cost_state_covering(&mut self, children: &[&Claude]) {
        let assistants: Vec<&Value> = self
            .records
            .iter()
            .chain(children.iter().flat_map(|child| child.records.iter()))
            .filter(|record| record["type"] == "assistant")
            .collect();
        let requests: u64 = assistants
            .iter()
            .map(|record| record["message"]["usage"]["input_tokens"].as_u64().unwrap())
            .sum();
        let count = assistants.len() as u64;
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
    pub fn project(sandbox: &Sandbox, cwd: &Path) -> PathBuf {
        let name: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        sandbox.dir("claude/projects").join(name)
    }

    /// Write a lead at its project, or a subagent under `lead`'s.
    pub fn write(&self, sandbox: &Sandbox, lead_cwd: &Path) {
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
pub struct Codex {
    pub id: String,
    pub started: Time,
    pub cwd: PathBuf,
    pub records: Vec<Value>,
    pub total: u64,
    pub serial: usize,
}

impl Codex {
    pub fn new(id: &str, started: Time, cwd: &Path, branch: &str) -> Self {
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

    pub fn push(&mut self, when: Time, record: Value) {
        let mut record = record;
        record["timestamp"] = json!(stamp(when));
        self.records.push(record);
    }

    pub fn request(&mut self, when: Time) {
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

    pub fn reply(&mut self, when: Time) {
        self.push(
            when,
            json!({"type": "response_item", "payload": {"type": "message",
            "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}}),
        );
        self.request(when);
    }

    pub fn exec(&mut self, when: Time, cmd: &str, output: &str) {
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

    pub fn commit(&mut self, when: Time, branch: &str, sha: &str, subject: &str) {
        self.exec(
            when,
            &format!("git add -A && git commit -m \"{subject}\""),
            &format!(
                "[{branch} {}] {subject}\n 1 file changed, 3 insertions(+)\n",
                short(sha)
            ),
        );
    }

    pub fn write(&self, sandbox: &Sandbox) {
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
pub struct Pi {
    pub id: String,
    pub started: Time,
    pub cwd: PathBuf,
    pub records: Vec<Value>,
    pub parent: Option<String>,
    pub serial: usize,
}

impl Pi {
    pub fn new(id: &str, started: Time, cwd: &Path) -> Self {
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

    pub fn message(&mut self, when: Time, message: Value) {
        self.serial += 1;
        let id = format!("m{}", self.serial);
        let mut message = message;
        message["timestamp"] = json!(when.timestamp_millis());
        self.records
            .push(json!({"type": "message", "id": id, "parentId": self.parent,
            "timestamp": stamp(when), "message": message}));
        self.parent = Some(id);
    }

    pub fn assistant(&mut self, when: Time, content: Value) {
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

    pub fn reply(&mut self, when: Time) {
        self.assistant(when, json!([{"type": "text", "text": "Done."}]));
    }

    pub fn call(
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

    pub fn commit(&mut self, when: Time, branch: &str, sha: &str, subject: &str) {
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

    pub fn edit(&mut self, when: Time, file: &Path, added: &[String]) {
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

    pub fn write(&self, sandbox: &Sandbox) {
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
