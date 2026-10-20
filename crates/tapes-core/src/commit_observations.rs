//! Commits a session made or inspected, taken from its shell tool calls and
//! their results.
//!
//! A result's whole text is in hand only while its record is parsed, so each
//! backend folds it into a small [`ResultScan`] there: the commit shas and
//! subjects it prints, and whether it reads as a failure. When the event
//! projection pairs that result with its call, the call's command text (see
//! [`CommandFacts`]) says which of those lines mean a commit was made and
//! which only show one.
//!
//! Only shas, commit subjects and branch names leave this module, never other
//! output text.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::event::{javascript_tokens, JsTokenKind};

/// The most observations one tool call reports.
pub const MAX_CALL_OBSERVATIONS: usize = 32;
/// The most observations one session reports; later ones are counted, not kept.
pub const MAX_SESSION_OBSERVATIONS: usize = 1024;
/// The most commit lines kept from one result.
const MAX_SCANNED_ENTRIES: usize = 128;
const MAX_SUBJECT_CHARS: usize = 256;
/// The longest command text read for git subcommands.
const MAX_COMMAND_CHARS: usize = 256 * 1024;
const MAX_COMMANDS: usize = 512;
const MAX_SHELL_DEPTH: usize = 3;
const MAX_BRANCH_BYTES: usize = 256;
const MIN_SHA_LEN: usize = 7;
const MAX_SHA_LEN: usize = 64;

/// How a commit's sha was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommitFound {
    /// The harness's own structured record of the git operation, which names
    /// the commit and its branch.
    GitOperation,
    /// Git's own `[<branch> <sha>] <subject>` line after a commit-creating command.
    SummaryLine,
    /// A sha printed by a later command of the same call, such as `git log -1`
    /// or `git rev-parse HEAD`, after a commit-creating command that printed none.
    ChainedOutput,
    /// A sha the call's command names as a revision, as in `git show <sha>`.
    Invocation,
}

/// The tool call an observation came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitCall {
    pub ordinal: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

/// One commit a session made (`created`) or inspected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitObservation {
    /// When the call's result was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// As printed or named: abbreviated or full.
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The branch a summary line or the harness's record names, when it
    /// names one: a detached head has none, and a chained print names none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub created: bool,
    pub found: CommitFound,
    pub call: CommitCall,
}

/// The commit observations one tool result yields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EventCommits {
    pub observations: Vec<CommitObservation>,
    /// More commits were seen than the per-call cap keeps.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// A commit-creating command ran without failing, and nothing in the
    /// result names the commit it made.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub created_unnamed: bool,
}

/// What a whole read's observations cost against the per-session cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CommitCounts {
    pub kept: usize,
    /// Observations dropped for the per-session cap.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted: usize,
    /// Results whose own observations hit the per-call cap.
    #[serde(skip_serializing_if = "is_zero")]
    pub truncated_calls: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl CommitCounts {
    pub fn is_empty(&self) -> bool {
        self.omitted == 0 && self.truncated_calls == 0
    }

    /// Admit one result's observations against the session cap.
    pub(crate) fn admit(&mut self, commits: &mut EventCommits) {
        let room = MAX_SESSION_OBSERVATIONS - self.kept;
        if commits.observations.len() > room {
            self.omitted += commits.observations.len() - room;
            commits.observations.truncate(room);
        }
        self.kept += commits.observations.len();
        if commits.truncated {
            self.truncated_calls += 1;
        }
    }
}

// ---------------------------------------------------------------- results

/// Whether the command that produced a result succeeded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Outcome {
    #[default]
    Unknown,
    Succeeded,
    Failed,
}

impl Outcome {
    pub(crate) fn from_exit(code: Option<i64>) -> Self {
        match code {
            Some(0) => Self::Succeeded,
            Some(_) => Self::Failed,
            None => Self::Unknown,
        }
    }

    pub(crate) fn from_error_flag(is_error: Option<bool>) -> Self {
        match is_error {
            Some(true) => Self::Failed,
            Some(false) => Self::Succeeded,
            None => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// `[<branch> <sha>] <subject>`
    Summary,
    /// `commit <sha>` from `git log` or `git show`.
    Header,
    /// A line led by a sha, as `--oneline`, `rev-parse` and `rev-list` print.
    Led,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Printed {
    shown: Shown,
    sha: String,
    subject: Option<String>,
    branch: Option<String>,
}

/// A commit the harness recorded as made, in a structured field beside the
/// result's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitOperation {
    sha: String,
    branch: Option<String>,
}

impl GitOperation {
    /// Claude's `toolUseResult.gitOperation.commit`: `{sha, kind, branch?}`
    /// with a kind of `committed` or `amended`.
    pub(crate) fn of_claude(structured: &Value) -> Option<Self> {
        let commit = &structured["gitOperation"]["commit"];
        let sha = commit["sha"].as_str().filter(|sha| is_sha(sha))?;
        matches!(commit["kind"].as_str(), Some("committed" | "amended")).then(|| Self {
            sha: sha.to_owned(),
            branch: commit["branch"].as_str().and_then(bounded_name),
        })
    }
}

/// The commit lines of one result and how it ended, kept while the whole text
/// is in hand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResultScan {
    entries: Vec<Printed>,
    truncated: bool,
    failure_text: bool,
    no_new_commit: bool,
    outcome: Outcome,
    operation: Option<GitOperation>,
}

const FAILURE_PHRASES: [&str; 7] = [
    "nothing to commit",
    "no changes added to commit",
    "nothing added to commit",
    "Aborting commit",
    "Automatic merge failed",
    "error: could not apply",
    "CONFLICT (",
];

impl ResultScan {
    /// This scan, with the commit the harness recorded beside the text.
    pub(crate) fn with_operation(mut self, operation: Option<GitOperation>) -> Self {
        self.operation = operation;
        self
    }

    pub(crate) fn has_operation(&self) -> bool {
        self.operation.is_some()
    }

    pub(crate) fn of(text: &str, outcome: Outcome) -> Self {
        let mut scan = Self {
            outcome,
            ..Self::default()
        };
        // The entry whose subject the next indented line after a blank one gives.
        let mut awaiting_subject: Option<(usize, bool)> = None;
        for line in text.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some((index, in_body)) = awaiting_subject {
                if line.is_empty() {
                    awaiting_subject = Some((index, true));
                    continue;
                }
                if in_body {
                    if let Some(subject) = line.strip_prefix("    ") {
                        scan.entries[index].subject = bounded_subject(subject);
                    }
                    awaiting_subject = None;
                } else if !line.starts_with(' ') && !line.contains(':') {
                    awaiting_subject = None;
                }
            }
            if line.len() < 200 {
                if FAILURE_PHRASES.iter().any(|phrase| line.contains(phrase))
                    || line.starts_with("fatal: ")
                    || line.starts_with("error: ")
                {
                    scan.failure_text = true;
                }
                if matches!(
                    line,
                    "Fast-forward" | "Already up to date." | "Already up-to-date."
                ) {
                    scan.no_new_commit = true;
                }
            }
            let Some(entry) = printed_line(line) else {
                continue;
            };
            if scan.entries.len() == MAX_SCANNED_ENTRIES {
                scan.truncated = true;
                continue;
            }
            if entry.shown == Shown::Header {
                awaiting_subject = Some((scan.entries.len(), false));
            }
            scan.entries.push(entry);
        }
        scan
    }
}

fn printed_line(line: &str) -> Option<Printed> {
    if line.starts_with('[') {
        return summary_line(line);
    }
    if let Some(rest) = line.strip_prefix("commit ") {
        let sha = rest.split([' ', '\t']).next()?;
        return (sha.len() >= 40 && is_sha(sha)).then(|| Printed {
            shown: Shown::Header,
            sha: sha.to_owned(),
            subject: None,
            branch: None,
        });
    }
    let (sha, rest) = match line.find([' ', '\t']) {
        Some(at) => (&line[..at], line[at + 1..].trim()),
        None => (line, ""),
    };
    is_sha(sha).then(|| Printed {
        shown: Shown::Led,
        sha: sha.to_owned(),
        subject: bounded_subject(rest),
        branch: None,
    })
}

fn summary_line(line: &str) -> Option<Printed> {
    let close = line.find(']')?;
    // `[<branch> <sha>]`, `[<branch> (root-commit) <sha>]`, `[detached HEAD <sha>]`.
    let (head, sha) = line[1..close].rsplit_once(' ')?;
    if head.is_empty() || !is_sha(sha) {
        return None;
    }
    let branch = head
        .strip_suffix(" (root-commit)")
        .unwrap_or(head)
        .to_owned();
    Some(Printed {
        shown: Shown::Summary,
        sha: sha.to_owned(),
        subject: bounded_subject(&line[close + 1..]),
        branch: (branch != "detached HEAD")
            .then_some(branch)
            .and_then(|branch| bounded_name(&branch)),
    })
}

fn bounded_name(name: &str) -> Option<String> {
    (!name.is_empty() && name.len() <= MAX_BRANCH_BYTES).then(|| name.to_owned())
}

fn bounded_subject(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(MAX_SUBJECT_CHARS).collect())
}

/// A lowercase hex object name. An abbreviation must hold a digit: an
/// all-letter run is as likely a word (`defaced`) as a sha.
fn is_sha(word: &str) -> bool {
    (MIN_SHA_LEN..=MAX_SHA_LEN).contains(&word.len())
        && word
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && (word.len() >= 40 || word.bytes().any(|byte| byte.is_ascii_digit()))
}

/// The text of a tool result: a string, or the text parts of an array joined
/// at line boundaries.
pub(crate) fn result_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut joined = String::new();
            for text in parts.iter().filter_map(|part| part["text"].as_str()) {
                if !joined.is_empty() && !joined.ends_with('\n') {
                    joined.push('\n');
                }
                joined.push_str(text);
            }
            Some(joined)
        }
        _ => None,
    }
}

/// A result's text with the JSON objects Codex nests inside its text parts
/// unwrapped, and the process exit status those objects record. A shell
/// result is such an object, `{"output": "...", "exit_code": 0, ...}`, inside
/// a string.
pub(crate) fn decoded_result(value: &Value) -> Option<(String, Outcome)> {
    let mut decoded = Decoded::default();
    match value {
        Value::String(text) => decoded.text(text, 0),
        Value::Array(parts) => {
            for text in parts.iter().filter_map(|part| part["text"].as_str()) {
                decoded.text(text, 0);
            }
        }
        _ => return None,
    }
    let outcome = if decoded.failed {
        Outcome::Failed
    } else if decoded.succeeded {
        Outcome::Succeeded
    } else {
        Outcome::Unknown
    };
    Some((decoded.joined, outcome))
}

#[derive(Default)]
struct Decoded {
    joined: String,
    succeeded: bool,
    failed: bool,
}

impl Decoded {
    const MAX_DEPTH: usize = 4;
    /// The members of a nested object that hold the output a command wrote.
    const OUTPUT_KEYS: [&'static str; 7] = [
        "output",
        "stdout",
        "stderr",
        "formatted_output",
        "aggregated_output",
        "value",
        "result",
    ];

    fn push(&mut self, text: &str) {
        if !self.joined.is_empty() && !self.joined.ends_with('\n') {
            self.joined.push('\n');
        }
        self.joined.push_str(text);
    }

    fn text(&mut self, text: &str, depth: usize) {
        let trimmed = text.trim_start();
        if depth < Self::MAX_DEPTH && trimmed.starts_with(['{', '[']) {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                self.walk(&value, depth + 1);
                return;
            }
        }
        self.push(text);
    }

    fn walk(&mut self, value: &Value, depth: usize) {
        match value {
            Value::String(text) => self.text(text, depth),
            Value::Array(items) => items.iter().for_each(|item| self.walk(item, depth)),
            Value::Object(object) => {
                let exit = match &object.get("exit_code") {
                    Some(Value::Number(code)) => code.as_i64(),
                    Some(Value::String(code)) => code.trim().parse().ok(),
                    _ => None,
                };
                match exit {
                    Some(0) => self.succeeded = true,
                    Some(_) => self.failed = true,
                    None => {}
                }
                for key in Self::OUTPUT_KEYS {
                    if let Some(member) = object.get(key) {
                        self.walk(member, depth);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The scan of a result's text, when the value holds text.
pub(crate) fn scan_result(value: &Value, outcome: Outcome) -> Option<ResultScan> {
    result_text(value).map(|text| ResultScan::of(&text, outcome))
}

// ------------------------------------------------------------------ calls

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sep {
    Start,
    And,
    Or,
    Seq,
    Pipe,
}

#[derive(Clone, Debug)]
struct Command {
    after: Sep,
    words: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Creates {
    Commit,
    Merge,
    CherryPick,
    Revert,
    Am,
    RebaseContinue,
}

/// What a call's command text says about commits: whether it creates one,
/// which later commands print one, and which shas it names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandFacts {
    creates: Vec<Creates>,
    /// Every printing command, in script order.
    blocks: Vec<Block>,
    printing: bool,
    /// Every printing command asks for `<sha> <subject>` lines.
    subjects_on_led_lines: bool,
    named: Vec<String>,
}

/// One printing command and the commit-creating commands that ran since the
/// previous one. A printer that shows the commits at `HEAD` newest first lists
/// the commits just made at its head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Block {
    /// Commit-creating commands since the previous printer.
    run: usize,
    /// It prints the commits at `HEAD` newest first, unfiltered, as `git log`,
    /// `git show` and `git rev-parse HEAD` do.
    from_head: bool,
    /// It runs only if every commit-creating command of its run succeeded.
    guarded: bool,
    /// The most commit lines it prints, when its command bounds them.
    limit: Option<usize>,
}

const SHELLS: [&str; 5] = ["bash", "sh", "zsh", "dash", "ksh"];
const PRINTING: [&str; 7] = [
    "log",
    "show",
    "rev-parse",
    "rev-list",
    "merge-base",
    "reflog",
    "show-ref",
];
const NAMING: [&str; 8] = [
    "show",
    "diff",
    "log",
    "diff-tree",
    "range-diff",
    "cat-file",
    "format-patch",
    "rev-list",
];
const PASSTHROUGH: [&str; 12] = [
    "head", "tail", "cat", "tee", "cut", "sed", "awk", "sort", "uniq", "column", "nl", "less",
];
const SHELL_TOOLS: [&str; 8] = [
    "bash",
    "shell",
    "exec",
    "exec_command",
    "shell_command",
    "commandexecution",
    "container.exec",
    "local_shell",
];

impl CommandFacts {
    /// The facts of a tool call's recorded arguments, when it is a shell tool
    /// whose command names git.
    pub(crate) fn of_call(name: Option<&str>, arguments: &str) -> Option<Self> {
        if !name.is_some_and(|name| SHELL_TOOLS.contains(&name.to_ascii_lowercase().as_str())) {
            return None;
        }
        if !arguments.contains("git") || arguments.chars().count() > MAX_COMMAND_CHARS {
            return None;
        }
        let mut commands = Vec::new();
        for script in scripts(arguments) {
            scan_shell(&script, 0, Sep::Seq, &mut commands);
            if commands.len() >= MAX_COMMANDS {
                break;
            }
        }
        let facts = Self::from_commands(&commands);
        (!facts.creates.is_empty() || facts.printing || !facts.named.is_empty()).then_some(facts)
    }

    fn from_commands(commands: &[Command]) -> Self {
        let mut facts = Self::default();
        let mut run: Vec<usize> = Vec::new();
        let mut printers = 0usize;
        let mut all_subjects = true;
        for (position, command) in commands.iter().enumerate() {
            let Some((sub, args)) = git_subcommand(&command.words) else {
                continue;
            };
            if let Some(kind) = creates(sub, &args) {
                facts.creates.push(kind);
                run.push(position);
            }
            if PRINTING.contains(&sub) && !consumed(commands, position) {
                printers += 1;
                let from_head = prints_head(sub, &args);
                facts.blocks.push(Block {
                    run: run.len(),
                    from_head,
                    guarded: run.first().is_none_or(|first| {
                        commands[first + 1..=position]
                            .iter()
                            .all(|command| matches!(command.after, Sep::And | Sep::Pipe))
                    }),
                    limit: print_limit(commands, position, sub, &args, from_head),
                });
                run.clear();
                all_subjects &= asks_for_subjects(sub, &args);
            }
            if NAMING.contains(&sub) {
                for sha in named_shas(&args) {
                    if !facts.named.iter().any(|named| related(named, &sha)) {
                        facts.named.push(sha);
                    }
                }
            }
        }
        facts.printing = printers > 0;
        facts.subjects_on_led_lines = facts.printing && all_subjects;
        facts
    }

    /// The commits made that the lines printed after them name. Printers
    /// print in script order, so a result's commit lines divide among them by
    /// their bounds; each printer that shows `HEAD` newest first begins with
    /// the commits made since the printer before it.
    fn chained(&self, scan: &ResultScan, named: &[Draft], succeeded: bool) -> Vec<Draft> {
        let lines = scan
            .entries
            .iter()
            .filter(|entry| entry.shown != Shown::Summary)
            .collect::<Vec<_>>();
        let mut drafts: Vec<Draft> = Vec::new();
        let mut offset = 0usize;
        for block in &self.blocks {
            if block.run > 0 && block.from_head && (block.guarded || succeeded) {
                let end = block.limit.map_or(lines.len(), |limit| offset + limit);
                let shown = lines.get(offset..end.min(lines.len())).unwrap_or_default();
                for entry in shown.iter().take(block.run) {
                    let known = named
                        .iter()
                        .chain(&drafts)
                        .any(|d| related(&d.sha, &entry.sha));
                    if !known {
                        drafts.push(Draft::of(
                            entry,
                            true,
                            CommitFound::ChainedOutput,
                            self.subjects_on_led_lines,
                        ));
                    }
                }
            }
            // A printer with no bound leaves the next one's lines unplaced.
            match block.limit {
                Some(limit) if offset + limit <= lines.len() => offset += limit,
                _ => break,
            }
        }
        drafts
    }

    /// The observations a paired result yields.
    pub(crate) fn observe(
        &self,
        scan: &ResultScan,
        call: &CommitCall,
        at: Option<DateTime<Utc>>,
    ) -> Option<EventCommits> {
        let made = self.creates.len();
        let mut created: Vec<Draft> = Vec::new();
        // The harness's own record of the commit is the strongest form.
        if let Some(operation) = &scan.operation {
            Draft::add(
                &mut created,
                Draft {
                    sha: operation.sha.clone(),
                    subject: None,
                    branch: operation.branch.clone(),
                    created: true,
                    found: CommitFound::GitOperation,
                },
            );
        }
        // A summary line is evidence of a commit only from a call that ran a
        // commit-creating command.
        for entry in scan
            .entries
            .iter()
            .filter(|entry| made > 0 && entry.shown == Shown::Summary)
        {
            Draft::add(
                &mut created,
                Draft::of(entry, true, CommitFound::SummaryLine, true),
            );
        }
        let mut created_unnamed = false;
        if made > created.len() {
            let succeeded = scan.outcome != Outcome::Failed && !scan.failure_text;
            let merged_nothing = scan.no_new_commit && self.creates == [Creates::Merge];
            for draft in self.chained(scan, &created, succeeded) {
                Draft::add(&mut created, draft);
            }
            created_unnamed = succeeded && !merged_nothing && created.len() < made;
        }
        // Commits only seen are the ones the call names as revisions; a sha
        // the result merely prints is not one, but it gives the longer name
        // and the subject of a commit already named.
        let mut seen: Vec<Draft> = Vec::new();
        if scan.outcome != Outcome::Failed {
            for sha in &self.named {
                if !created.iter().any(|draft| related(&draft.sha, sha)) {
                    seen.push(Draft {
                        sha: sha.clone(),
                        subject: None,
                        branch: None,
                        created: false,
                        found: CommitFound::Invocation,
                    });
                }
            }
        }
        for entry in scan
            .entries
            .iter()
            .filter(|entry| entry.shown != Shown::Summary)
        {
            let printed = Draft::of(
                entry,
                false,
                CommitFound::Invocation,
                self.subjects_on_led_lines,
            );
            if let Some(named) = seen
                .iter_mut()
                .find(|named| related(&named.sha, &entry.sha))
            {
                named.absorb(printed);
            }
        }
        created.append(&mut seen);
        let truncated =
            created.len() > MAX_CALL_OBSERVATIONS || (scan.truncated && created_unnamed);
        created.truncate(MAX_CALL_OBSERVATIONS);
        if created.is_empty() && !created_unnamed && !truncated {
            return None;
        }
        Some(EventCommits {
            observations: created
                .into_iter()
                .map(|draft| CommitObservation {
                    at,
                    sha: draft.sha,
                    subject: draft.subject,
                    branch: draft.branch,
                    created: draft.created,
                    found: draft.found,
                    call: call.clone(),
                })
                .collect(),
            truncated,
            created_unnamed,
        })
    }
}

struct Draft {
    sha: String,
    subject: Option<String>,
    branch: Option<String>,
    created: bool,
    found: CommitFound,
}

impl Draft {
    fn of(entry: &Printed, created: bool, found: CommitFound, led_subjects: bool) -> Self {
        let subject = match entry.shown {
            Shown::Led if !led_subjects => None,
            _ => entry.subject.clone(),
        };
        Self {
            sha: entry.sha.clone(),
            subject,
            branch: entry.branch.clone(),
            created,
            found,
        }
    }

    /// Add a sighting to the list, folding it into the draft it names.
    fn add(drafts: &mut Vec<Draft>, draft: Draft) {
        match drafts
            .iter_mut()
            .find(|kept| related(&kept.sha, &draft.sha))
        {
            Some(kept) => kept.absorb(draft),
            None => drafts.push(draft),
        }
    }

    /// Fold a second sighting of the same commit into this one: a commit made
    /// stays made, the longer name is kept, and a subject or branch is kept
    /// if either has it.
    fn absorb(&mut self, other: Draft) {
        if other.created && !self.created {
            self.created = true;
            self.found = other.found;
        }
        if other.sha.len() > self.sha.len() {
            self.sha = other.sha;
        }
        if self.subject.is_none() {
            self.subject = other.subject;
        }
        if self.branch.is_none() {
            self.branch = other.branch;
        }
    }
}

/// Two names for one commit: one abbreviates the other.
fn related(left: &str, right: &str) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Whether the command at `position` feeds a later filter that keeps its
/// output from the result.
fn consumed(commands: &[Command], position: usize) -> bool {
    commands.get(position + 1).is_some_and(|next| {
        next.after == Sep::Pipe
            && !next
                .words
                .first()
                .is_some_and(|program| PASSTHROUGH.contains(&basename(program)))
    })
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// The subcommand and arguments of a `git` command, past its global options.
fn git_subcommand(words: &[String]) -> Option<(&str, Vec<&str>)> {
    let (program, rest) = words.split_first()?;
    if basename(program) != "git" {
        return None;
    }
    let mut rest = rest.iter().map(String::as_str);
    while let Some(word) = rest.next() {
        match word {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path"
            | "--super-prefix" | "--config-env" => {
                rest.next();
            }
            word if word.starts_with('-') => {}
            sub => return Some((sub, rest.collect())),
        }
    }
    None
}

fn creates(sub: &str, args: &[&str]) -> Option<Creates> {
    let has = |flags: &[&str]| args.iter().any(|arg| flags.contains(arg));
    let suspended = has(&["--abort", "--quit", "--skip", "--dry-run"]);
    if suspended {
        return None;
    }
    match sub {
        "commit" => Some(Creates::Commit),
        "merge" => (!has(&["--squash", "--no-commit"])).then_some(Creates::Merge),
        "cherry-pick" => (!has(&["-n", "--no-commit"])).then_some(Creates::CherryPick),
        "revert" => (!has(&["-n", "--no-commit"])).then_some(Creates::Revert),
        "am" => Some(Creates::Am),
        "rebase" => has(&["--continue"]).then_some(Creates::RebaseContinue),
        _ => None,
    }
}

/// Whether the printing command shows the commits at `HEAD`, newest first and
/// unfiltered: no other revision, path, filter or reordering.
fn prints_head(sub: &str, args: &[&str]) -> bool {
    const FILTERS: [&str; 14] = [
        "--all",
        "--branches",
        "--remotes",
        "--author",
        "--committer",
        "--grep",
        "--since",
        "--until",
        "--after",
        "--before",
        "--skip",
        "--reverse",
        "--merges",
        "--follow",
    ];
    if args.contains(&"--") {
        return false;
    }
    let revs = args
        .iter()
        .filter(|arg| !arg.starts_with('-') && arg.parse::<u64>().is_err())
        .collect::<Vec<_>>();
    match sub {
        "log" | "show" => {
            revs.iter().all(|rev| **rev == "HEAD")
                && !args.iter().any(|arg| {
                    FILTERS.iter().any(|filter| arg.starts_with(filter))
                        || arg.starts_with("-S")
                        || arg.starts_with("-G")
                })
        }
        "rev-parse" => revs.as_slice() == [&"HEAD"],
        _ => false,
    }
}

/// The most commit lines the printing command at `position` writes: its own
/// bound on the commits it lists, and a `head` it is piped into.
fn print_limit(
    commands: &[Command],
    position: usize,
    sub: &str,
    args: &[&str],
    from_head: bool,
) -> Option<usize> {
    let mut limit = match sub {
        "rev-parse" | "show" => from_head.then_some(1),
        _ => commit_count(args),
    };
    for next in &commands[position + 1..] {
        if next.after != Sep::Pipe {
            break;
        }
        let words = next.words.iter().map(String::as_str).collect::<Vec<_>>();
        match words.first().map(|program| basename(program)) {
            Some("cat" | "tee") => {}
            Some("head") => {
                let lines = line_count(&words[1..]).unwrap_or(10);
                limit = Some(limit.map_or(lines, |limit| limit.min(lines)));
            }
            _ => return None,
        }
    }
    limit
}

/// A `git log` bound: `-N`, `-n N`, `-nN`, `--max-count=N`, `--max-count N`.
fn commit_count(args: &[&str]) -> Option<usize> {
    args.iter().enumerate().find_map(|(index, arg)| {
        if let Some(count) = arg.strip_prefix("--max-count=") {
            return count.parse().ok();
        }
        match *arg {
            "-n" | "--max-count" => args.get(index + 1)?.parse().ok(),
            arg => arg
                .strip_prefix("-n")
                .or_else(|| arg.strip_prefix('-'))?
                .parse()
                .ok(),
        }
    })
}

/// The line count of a `head` invocation's arguments.
fn line_count(args: &[&str]) -> Option<usize> {
    commit_count(args).filter(|_| !args.is_empty())
}

/// Whether the printing command prints `<sha> <subject>` per commit.
fn asks_for_subjects(sub: &str, args: &[&str]) -> bool {
    sub == "log"
        && args.iter().any(|arg| {
            matches!(
                *arg,
                "--oneline"
                    | "--pretty=oneline"
                    | "--format=oneline"
                    | "--format=%h %s"
                    | "--format=%H %s"
                    | "--pretty=format:%h %s"
            )
        })
}

/// The commit names a command spells as hex, in its revision arguments: each
/// end of an `A..B` or `A...B` range, without `^`, `~n` or `:path` suffixes.
fn named_shas(args: &[&str]) -> Vec<String> {
    let mut shas: Vec<String> = Vec::new();
    for arg in args.iter().take_while(|arg| **arg != "--") {
        if arg.starts_with('-') {
            continue;
        }
        let arg = arg.split(':').next().unwrap_or(arg);
        for end in arg.split("...").flat_map(|part| part.split("..")) {
            let end = end.trim_start_matches('^');
            let end = end.split(['~', '^']).next().unwrap_or(end);
            if is_sha(end) && !shas.iter().any(|sha| related(sha, end)) {
                shas.push(end.to_owned());
            }
        }
    }
    shas
}

// ----------------------------------------------------------- script text

/// The shell scripts a shell tool call's recorded arguments hold.
fn scripts(arguments: &str) -> Vec<String> {
    if arguments.contains("tools.exec_command") {
        return javascript_commands(arguments);
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value_scripts(&value),
        Err(_) => Vec::new(),
    }
}

fn value_scripts(value: &Value) -> Vec<String> {
    match value {
        Value::String(script) => vec![script.clone()],
        Value::Array(argv) => argv_script(argv).into_iter().collect(),
        Value::Object(object) => ["command", "cmd", "argv"]
            .into_iter()
            .find_map(|key| object.get(key))
            .map(value_scripts)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// An argv as one script: a shell's `-c` operand, or the words themselves.
fn argv_script(argv: &[Value]) -> Option<String> {
    let words = argv.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
    if let Some(script) = shell_operand(&words) {
        return Some(script.to_owned());
    }
    Some(
        words
            .iter()
            .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// The script a shell invoked with `-c` (or `-lc` and the like) runs.
fn shell_operand<'a>(words: &[&'a str]) -> Option<&'a str> {
    let (program, rest) = words.split_first()?;
    if !SHELLS.contains(&basename(program)) {
        return None;
    }
    let flag = rest.iter().position(|word| {
        word.starts_with('-') && !word.starts_with("--") && word[1..].contains('c')
    })?;
    rest.get(flag + 1).copied()
}

/// Each literal `cmd`/`command` string of a Codex script's `exec_command` calls.
fn javascript_commands(text: &str) -> Vec<String> {
    let Ok(tokens) = javascript_tokens(text) else {
        return Vec::new();
    };
    tokens
        .windows(3)
        .filter_map(|window| {
            let key = match &window[0].kind {
                JsTokenKind::Identifier(key) | JsTokenKind::String(key) => key.as_str(),
                JsTokenKind::Punctuation(_) => return None,
            };
            match (&window[1].kind, &window[2].kind) {
                (JsTokenKind::Punctuation(':'), JsTokenKind::String(script))
                    if matches!(key, "cmd" | "command") =>
                {
                    Some(script.clone())
                }
                _ => None,
            }
        })
        .collect()
}

/// A tolerant split of shell text into commands. It follows quoting, comments,
/// heredoc bodies, redirections and `sh -c` operands, and treats command
/// substitutions as opaque words. It never evaluates anything, so a command it
/// cannot read exactly is still split at the separators it can see.
fn scan_shell(text: &str, depth: usize, first: Sep, out: &mut Vec<Command>) {
    let mut scanner = Scanner {
        chars: text.chars().collect(),
        at: 0,
        words: Vec::new(),
        word: String::new(),
        in_word: false,
        sep: first,
        heredocs: Vec::new(),
        depth,
        out,
    };
    scanner.run();
}

struct Scanner<'a> {
    chars: Vec<char>,
    at: usize,
    words: Vec<String>,
    word: String,
    in_word: bool,
    sep: Sep,
    /// Heredoc delimiters whose bodies start at the next line, with whether
    /// leading tabs are stripped from the closing line.
    heredocs: Vec<(String, bool)>,
    depth: usize,
    out: &'a mut Vec<Command>,
}

impl Scanner<'_> {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn run(&mut self) {
        while self.at < self.chars.len() && self.out.len() < MAX_COMMANDS {
            let c = self.chars[self.at];
            match c {
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => {}
                        Some(next) => {
                            self.word.push(next);
                            self.in_word = true;
                        }
                        None => {}
                    }
                    self.at += 2;
                }
                '\'' => self.single_quoted(),
                '"' => self.double_quoted(),
                '`' => self.opaque('`', '`'),
                '$' if self.peek(1) == Some('(') => {
                    self.at += 1;
                    self.opaque('(', ')');
                }
                '$' if self.peek(1) == Some('{') => {
                    self.at += 1;
                    self.opaque('{', '}');
                }
                '#' if !self.in_word => {
                    while self.at < self.chars.len() && self.chars[self.at] != '\n' {
                        self.at += 1;
                    }
                }
                '<' | '>' => self.redirection(),
                '&' => match self.peek(1) {
                    Some('&') => self.end(Sep::And, 2),
                    Some('>') => self.at += 1,
                    _ => self.end(Sep::Seq, 1),
                },
                '|' => match self.peek(1) {
                    Some('|') => self.end(Sep::Or, 2),
                    Some('&') => self.end(Sep::Pipe, 2),
                    _ => self.end(Sep::Pipe, 1),
                },
                ';' | '(' | ')' => self.end(Sep::Seq, 1),
                '{' | '}' if !self.in_word => self.end(Sep::Seq, 1),
                '\n' => {
                    self.end(Sep::Seq, 1);
                    self.heredoc_bodies();
                }
                c if c.is_whitespace() => {
                    self.flush_word();
                    self.at += 1;
                }
                c => {
                    self.word.push(c);
                    self.in_word = true;
                    self.at += 1;
                }
            }
        }
        self.end(Sep::Seq, 0);
    }

    fn flush_word(&mut self) {
        if self.in_word {
            self.words.push(std::mem::take(&mut self.word));
            self.in_word = false;
        }
    }

    /// Finish the command being read; the next one follows `next`.
    fn end(&mut self, next: Sep, width: usize) {
        self.at += width;
        self.flush_word();
        if self.words.is_empty() {
            if self.sep == Sep::Start || next != Sep::Seq {
                self.sep = next;
            }
            return;
        }
        let words = std::mem::take(&mut self.words);
        self.push_command(words);
        self.sep = next;
    }

    fn push_command(&mut self, mut words: Vec<String>) {
        const PREFIXES: [&str; 12] = [
            "if", "then", "else", "elif", "do", "while", "until", "!", "time", "command", "exec",
            "nohup",
        ];
        while let Some(first) = words.first() {
            let assignment = first.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
                    && !name.starts_with(|c: char| c.is_ascii_digit())
            });
            if assignment || PREFIXES.contains(&first.as_str()) || first == "env" {
                words.remove(0);
            } else if first == "timeout" && words.len() > 2 {
                words.drain(..2);
            } else {
                break;
            }
        }
        if words.is_empty() {
            return;
        }
        let refs = words.iter().map(String::as_str).collect::<Vec<_>>();
        let operand = shell_operand(&refs).or(match refs.as_slice() {
            ["eval", script] => Some(script),
            _ => None,
        });
        if let Some(script) = operand {
            if self.depth < MAX_SHELL_DEPTH {
                let script = script.to_owned();
                let mut inner = Vec::new();
                scan_shell(&script, self.depth + 1, self.sep, &mut inner);
                self.out.extend(inner);
                return;
            }
        }
        self.out.push(Command {
            after: self.sep,
            words,
        });
    }

    fn single_quoted(&mut self) {
        self.in_word = true;
        self.at += 1;
        while let Some(&c) = self.chars.get(self.at) {
            self.at += 1;
            if c == '\'' {
                return;
            }
            self.word.push(c);
        }
    }

    fn double_quoted(&mut self) {
        self.in_word = true;
        self.at += 1;
        while let Some(&c) = self.chars.get(self.at) {
            match c {
                '"' => {
                    self.at += 1;
                    return;
                }
                '\\' => {
                    if let Some(&next) = self.chars.get(self.at + 1) {
                        if next != '\n' {
                            self.word.push(next);
                        }
                    }
                    self.at += 2;
                }
                '$' if self.peek(1) == Some('(') => {
                    self.at += 1;
                    self.opaque('(', ')');
                }
                c => {
                    self.word.push(c);
                    self.at += 1;
                }
            }
        }
    }

    /// Skip a balanced `open … close` group (a substitution) starting at its
    /// opening character, leaving a placeholder that is no sha, path or
    /// subcommand. A group whose two ends are one character (a backtick) closes
    /// at the second.
    fn opaque(&mut self, open: char, close: char) {
        self.in_word = true;
        self.word.push('\u{1}');
        let mut depth = 0usize;
        while let Some(&c) = self.chars.get(self.at) {
            self.at += 1;
            if c == '\\' {
                self.at += 1;
            } else if open == close {
                depth += 1;
                if depth == 2 {
                    return;
                }
            } else if c == open {
                depth += 1;
            } else if c == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return;
                }
            }
        }
    }

    fn redirection(&mut self) {
        if self.in_word && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.in_word = false;
        }
        let c = self.chars[self.at];
        if c == '<' && self.peek(1) == Some('<') && self.peek(2) != Some('<') {
            self.at += 2;
            let strip_tabs = self.peek(0) == Some('-');
            if strip_tabs {
                self.at += 1;
            }
            let tag = self.operand();
            if !tag.is_empty() {
                self.heredocs.push((tag, strip_tabs));
            }
            return;
        }
        while matches!(self.peek(0), Some('<' | '>' | '&' | '|')) {
            self.at += 1;
        }
        let _ = self.operand();
    }

    /// Read one word after an operator, without adding it to the command.
    fn operand(&mut self) -> String {
        while self.peek(0).is_some_and(|c| c == ' ' || c == '\t') {
            self.at += 1;
        }
        let mut word = String::new();
        while let Some(c) = self.peek(0) {
            match c {
                '\'' | '"' => {
                    self.at += 1;
                    while let Some(inner) = self.peek(0) {
                        self.at += 1;
                        if inner == c {
                            break;
                        }
                        word.push(inner);
                    }
                }
                '\\' => {
                    self.at += 1;
                }
                c if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '<' | '>') => {
                    break
                }
                c => {
                    word.push(c);
                    self.at += 1;
                }
            }
        }
        word
    }

    /// Skip the bodies of the heredocs opened on the line just ended.
    fn heredoc_bodies(&mut self) {
        for (tag, strip_tabs) in std::mem::take(&mut self.heredocs) {
            loop {
                if self.at >= self.chars.len() {
                    return;
                }
                let end = self.chars[self.at..]
                    .iter()
                    .position(|c| *c == '\n')
                    .map_or(self.chars.len(), |offset| self.at + offset);
                let line = self.chars[self.at..end].iter().collect::<String>();
                self.at = (end + 1).min(self.chars.len());
                let line = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if line == tag {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(command: &str) -> CommandFacts {
        let arguments = serde_json::json!({ "command": command }).to_string();
        CommandFacts::of_call(Some("Bash"), &arguments).unwrap_or_default()
    }

    #[test]
    fn a_heredoc_message_hides_the_words_it_holds() {
        let facts = facts("cat > m <<'E'\ngit commit -m not-this\nE\ngit status");
        assert!(facts.creates.is_empty());
    }

    #[test]
    fn a_commit_after_quoted_and_redirected_words_is_found() {
        let facts = facts(
            "cd /w && export A=1 && git add -A 2>&1 && git -C /w commit -q -F m && git log -1 --oneline",
        );
        assert_eq!(facts.creates, [Creates::Commit]);
        assert_eq!(
            facts.blocks,
            [Block {
                run: 1,
                from_head: true,
                guarded: true,
                limit: Some(1)
            }]
        );
    }

    #[test]
    fn a_shell_operand_is_read_as_commands() {
        let facts = facts("bash -lc 'git commit -m x; git rev-parse HEAD'");
        assert_eq!(facts.creates, [Creates::Commit]);
        assert_eq!(
            facts.blocks,
            [Block {
                run: 1,
                from_head: true,
                guarded: false,
                limit: Some(1)
            }]
        );
    }

    #[test]
    fn a_log_piped_into_a_filter_prints_nothing() {
        let facts = facts("git commit -q -m x && git log -1 --format=%B | grep -F x");
        assert!(facts.blocks.is_empty());
    }

    #[test]
    fn a_printer_shows_the_head_when_nothing_selects_otherwise_and_says_how_much() {
        let blocks = |command: &str| {
            facts(command)
                .blocks
                .iter()
                .map(|block| (block.run, block.from_head, block.limit))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            blocks("git commit -q -m a; git log --oneline -3"),
            [(1, true, Some(3))]
        );
        assert_eq!(
            blocks("git commit -q -m a; git log --oneline | head -2"),
            [(1, true, Some(2))]
        );
        assert_eq!(
            blocks("git commit -q -m a; git log -n 5 | head -2"),
            [(1, true, Some(2))]
        );
        assert_eq!(
            blocks("git commit -q -m a; git log --oneline"),
            [(1, true, None)]
        );
        assert_eq!(
            blocks("git commit -q -m a; git rev-parse origin/main"),
            [(1, false, None)]
        );
        assert_eq!(
            blocks("git commit -q -m a; git log --oneline main"),
            [(1, false, None)]
        );
        assert_eq!(
            blocks("git commit -q -m a; git log --author=x -3"),
            [(1, false, Some(3))]
        );
        assert_eq!(
            blocks("git commit -q -m a; git log --oneline -1 -- src"),
            [(1, false, Some(1))]
        );
        assert_eq!(
            blocks("git commit -q -m a; git commit -q -m b; git log -2"),
            [(2, true, Some(2))]
        );
        assert_eq!(
            blocks("git rev-parse HEAD; git commit -q -m a; git log -1"),
            [(0, true, Some(1)), (1, true, Some(1))]
        );
    }

    #[test]
    fn commit_free_forms_do_not_create() {
        for command in [
            "git commit --dry-run",
            "git merge --abort",
            "git merge --squash topic",
            "git rebase main",
            "git commit-tree HEAD^{tree}",
            "git cherry-pick --abort",
        ] {
            assert!(facts(command).creates.is_empty(), "{command}");
        }
    }

    #[test]
    fn revisions_are_named_by_hex_only() {
        assert_eq!(
            named_shas(&["--stat", "abc1234..def5678", "HEAD~1", "--", "deadbee1"]),
            ["abc1234", "def5678"]
        );
        assert_eq!(
            named_shas(&["abc1234^{commit}", "9f8e7d6c~3"]),
            ["abc1234", "9f8e7d6c"]
        );
    }

    #[test]
    fn words_that_spell_hex_are_no_sha() {
        assert!(!is_sha("defaced"));
        assert!(is_sha("def4ced"));
        assert!(is_sha(&"a".repeat(40)));
    }
}
