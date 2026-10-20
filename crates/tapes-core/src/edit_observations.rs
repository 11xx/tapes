//! The file edits a session made, as the digests of the lines they added and
//! removed.
//!
//! A commit's changed lines and a recording's edited lines digest through the
//! same function ([`line_digest`]), so an edit compares with a commit directly
//! and no line text is kept. Each harness records an edit in its own shape, so
//! a result is folded into an [`EditScan`] while its record is parsed, and the
//! event projection combines that with what the paired call said (see
//! [`EditCall`]) once the two meet.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::commit_observations::{CommitCall, Outcome};
use crate::git_reader::line_digest;
use crate::shell_commands::{self, basename, git_subcommand};

/// The most line digests one edit keeps on each side.
pub const MAX_DIGESTS_PER_SIDE: usize = 1024;
/// The most edits one result reports.
pub const MAX_CALL_EDITS: usize = 32;
/// The most edits one session reports; later ones are counted, not kept.
pub const MAX_SESSION_EDITS: usize = 2048;
/// The most line digests a session's kept edits hold together; an edit past it
/// is kept without digests and says so.
pub const MAX_SESSION_DIGESTS: usize = 262_144;
const MAX_PATH_BYTES: usize = 4096;

/// What an edit did to its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditOp {
    /// An edit tool changed part of an existing file.
    Edit,
    /// A write tool put a file's content, or a patch added a file.
    Write,
    /// A patch deleted a file.
    Delete,
    /// A shell call changed the file.
    Shell,
    /// A shell call created the file.
    ShellCreate,
    /// A shell call deleted the file.
    ShellDelete,
}

/// Which record shape gave the lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditShape {
    /// Hunks of numbered `+`/`-`/context lines the harness recorded.
    StructuredPatch,
    /// The call's old and new text, diffed here.
    OldNew,
    /// The whole content written.
    Content,
    /// A unified diff the harness recorded.
    UnifiedDiff,
    /// A patch in the `*** Begin Patch` format the call carried.
    ApplyPatch,
    /// Claude's per-file hunks for a shell call.
    BashEditDiff,
}

/// The tool and record shape an edit came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EditSource {
    pub tool: String,
    pub shape: EditShape,
}

/// The tool call an edit came from.
pub type EditCall = CommitCall;

/// One file a session changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EditObservation {
    /// When the call's result was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// As the recording names it.
    pub path: String,
    pub op: EditOp,
    /// [`line_digest`]s of the added lines, in order.
    pub added: Vec<u64>,
    /// [`line_digest`]s of the removed lines, in order.
    pub removed: Vec<u64>,
    /// Digests were cut at a cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    pub source: EditSource,
    pub call: EditCall,
}

/// A shell call that moved the working tree, whose recorded diff is not an
/// edit the session wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TreeMovement {
    pub files: usize,
    /// Files the recording did not list.
    #[serde(skip_serializing_if = "is_zero")]
    pub more_files: usize,
    pub added_lines: usize,
    pub removed_lines: usize,
}

/// The edits one tool result yields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EventEdits {
    pub observations: Vec<EditObservation>,
    /// More edits were seen than the per-call cap keeps.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// The shell call ran a git command that moves the working tree, so its
    /// recorded diff is no authored edit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_movement: Option<TreeMovement>,
    /// Files the recording of a shell call's diff did not list.
    #[serde(skip_serializing_if = "is_zero")]
    pub more_files: usize,
    /// The recording marks the diff as shared with other changes.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,
    /// The call wrote files by a command the diff does not describe.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unrecognized: bool,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// What a read's edits cost against the session caps, and what was set aside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EditCounts {
    pub kept: usize,
    /// Edits dropped for the per-session cap.
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted: usize,
    /// Edits kept without digests for the per-session digest cap.
    #[serde(skip_serializing_if = "is_zero")]
    pub digests_omitted: usize,
    /// Results whose own edits hit the per-call cap.
    #[serde(skip_serializing_if = "is_zero")]
    pub truncated_calls: usize,
    /// Shell results that were tree movements.
    #[serde(skip_serializing_if = "is_zero")]
    pub tree_movements: usize,
    /// Files those movements changed.
    #[serde(skip_serializing_if = "is_zero")]
    pub tree_movement_files: usize,
    /// Shell results that wrote files by commands no diff describes.
    #[serde(skip_serializing_if = "is_zero")]
    pub unrecognized: usize,
    #[serde(skip)]
    digests: usize,
}

impl EditCounts {
    /// Whether the read has anything to report beyond the edits it kept.
    pub fn is_empty(&self) -> bool {
        self.omitted == 0
            && self.digests_omitted == 0
            && self.truncated_calls == 0
            && self.tree_movements == 0
            && self.unrecognized == 0
    }

    /// Admit one result's edits against the session caps.
    pub(crate) fn admit(&mut self, edits: &mut EventEdits) {
        let room = MAX_SESSION_EDITS - self.kept;
        if edits.observations.len() > room {
            self.omitted += edits.observations.len() - room;
            edits.observations.truncate(room);
        }
        for edit in &mut edits.observations {
            let size = edit.added.len() + edit.removed.len();
            if self.digests + size > MAX_SESSION_DIGESTS {
                edit.added.clear();
                edit.removed.clear();
                edit.truncated = true;
                self.digests_omitted += 1;
            } else {
                self.digests += size;
            }
        }
        self.kept += edits.observations.len();
        if edits.truncated {
            self.truncated_calls += 1;
        }
        if let Some(movement) = &edits.tree_movement {
            self.tree_movements += 1;
            self.tree_movement_files += movement.files;
        }
        if edits.unrecognized {
            self.unrecognized += 1;
        }
    }
}

/// Every edit of one session, in recording order, with what the caps set aside.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SessionEdits {
    pub observations: Vec<EditObservation>,
    #[serde(skip_serializing_if = "EditCounts::is_empty")]
    pub counts: EditCounts,
}

// ------------------------------------------------------------------ lines

/// The digests of the lines one edit added and removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Lines {
    added: Vec<u64>,
    removed: Vec<u64>,
    truncated: bool,
    /// Every added and removed line, digested or not.
    added_total: usize,
    removed_total: usize,
}

impl Lines {
    fn add(&mut self, text: &str) {
        self.added_total += 1;
        Self::push(&mut self.added, &mut self.truncated, text);
    }

    fn remove(&mut self, text: &str) {
        self.removed_total += 1;
        Self::push(&mut self.removed, &mut self.truncated, text);
    }

    fn push(side: &mut Vec<u64>, truncated: &mut bool, text: &str) {
        if let Some(digest) = line_digest(text) {
            if side.len() < MAX_DIGESTS_PER_SIDE {
                side.push(digest);
            } else {
                *truncated = true;
            }
        }
    }

    /// One diff line: its first character says what it is.
    fn diff_line(&mut self, line: &str) {
        match line.as_bytes().first() {
            Some(b'+') => self.add(&line[1..]),
            Some(b'-') => self.remove(&line[1..]),
            _ => {}
        }
    }

    /// The text `new` has where `old` had other: the lines between their
    /// common beginning and common end.
    fn old_new(old: &str, new: &str) -> Self {
        let old: Vec<&str> = old.lines().collect();
        let new: Vec<&str> = new.lines().collect();
        let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let tail = old[head..]
            .iter()
            .rev()
            .zip(new[head..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let mut lines = Self::default();
        old[head..old.len() - tail]
            .iter()
            .for_each(|line| lines.remove(line));
        new[head..new.len() - tail]
            .iter()
            .for_each(|line| lines.add(line));
        lines
    }

    /// Every line of `text`, as added.
    fn content(text: &str) -> Self {
        let mut lines = Self::default();
        text.lines().for_each(|line| lines.add(line));
        lines
    }

    /// Every line of `text`, as removed.
    fn deleted(text: &str) -> Self {
        let mut lines = Self::default();
        text.lines().for_each(|line| lines.remove(line));
        lines
    }

    /// The hunks of a unified diff, past its file headers.
    fn unified(text: &str) -> Self {
        let mut lines = Self::default();
        let mut in_hunk = false;
        for line in text.lines() {
            if line.starts_with("@@") {
                in_hunk = true;
            } else if in_hunk {
                lines.diff_line(line);
            }
        }
        lines
    }

    /// Hunks as Claude records them: `{lines: ["+added", "-removed", " kept"]}`.
    fn hunks(hunks: &Value) -> Self {
        let mut lines = Self::default();
        for hunk in hunks.as_array().into_iter().flatten() {
            for line in hunk["lines"].as_array().into_iter().flatten() {
                if let Some(line) = line.as_str() {
                    lines.diff_line(line);
                }
            }
        }
        lines
    }
}

/// One file's lines before they are an observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileDraft {
    path: String,
    op: EditOp,
    shape: EditShape,
    lines: Lines,
}

impl FileDraft {
    fn new(path: &str, op: EditOp, shape: EditShape, lines: Lines) -> Option<Self> {
        (!path.is_empty() && path.len() <= MAX_PATH_BYTES).then(|| Self {
            path: path.to_owned(),
            op,
            shape,
            lines,
        })
    }
}

/// A patch in the `*** Begin Patch` format, as one draft per file it names.
fn apply_patch(text: &str) -> Vec<FileDraft> {
    fn finish(drafts: &mut Vec<FileDraft>, current: Option<(String, EditOp, Lines)>) {
        if let Some((path, op, lines)) = current {
            drafts.extend(FileDraft::new(&path, op, EditShape::ApplyPatch, lines));
        }
    }
    let mut drafts = Vec::new();
    let mut current: Option<(String, EditOp, Lines)> = None;
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            finish(&mut drafts, current.take());
            current = Some((path.trim().to_owned(), EditOp::Edit, Lines::default()));
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            finish(&mut drafts, current.take());
            current = Some((path.trim().to_owned(), EditOp::Write, Lines::default()));
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            finish(&mut drafts, current.take());
            current = Some((path.trim().to_owned(), EditOp::Delete, Lines::default()));
        } else if line.starts_with("*** ") || line.starts_with("@@") {
            continue;
        } else if let Some((_, _, lines)) = current.as_mut() {
            lines.diff_line(line);
        }
    }
    finish(&mut drafts, current);
    drafts
}

// ---------------------------------------------------------------- results

/// A shell call's recorded diff: Claude's `bashEditDiff`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShellDiff {
    files: Vec<FileDraft>,
    more_files: usize,
    shared: bool,
}

/// What a result's record says about edits, kept while the whole record is in
/// hand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EditScan {
    outcome: Outcome,
    /// Files the harness itself recorded the changes of.
    drafts: Vec<FileDraft>,
    shell: Option<ShellDiff>,
    /// The drafts are a runtime's own record of a file change (Codex's
    /// `FileChange` item), which supersedes the patch text its call carried.
    runtime: bool,
}

impl EditScan {
    pub(crate) fn outcome_only(outcome: Outcome) -> Self {
        Self {
            outcome,
            ..Self::default()
        }
    }

    pub(crate) fn is_runtime(&self) -> bool {
        self.runtime && !self.drafts.is_empty()
    }

    /// A Claude tool result: `structured` is the record's `toolUseResult`.
    pub(crate) fn claude(structured: Option<&Value>, is_error: Option<bool>) -> Self {
        let mut scan = Self::outcome_only(Outcome::from_error_flag(is_error));
        let Some(result) = structured.filter(|result| result.is_object()) else {
            return scan;
        };
        if let Some(diff) = result["bashEditDiff"].as_object() {
            let mut shell = ShellDiff {
                more_files: diff
                    .get("moreFiles")
                    .and_then(Value::as_u64)
                    .map_or(0, |count| count as usize),
                shared: diff.get("shared").and_then(Value::as_bool) == Some(true),
                ..ShellDiff::default()
            };
            for file in diff
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let op = if file["created"].as_bool() == Some(true) {
                    EditOp::ShellCreate
                } else if file["deleted"].as_bool() == Some(true) {
                    EditOp::ShellDelete
                } else {
                    EditOp::Shell
                };
                shell.files.extend(FileDraft::new(
                    file["filePath"].as_str().unwrap_or_default(),
                    op,
                    EditShape::BashEditDiff,
                    Lines::hunks(&file["hunks"]),
                ));
            }
            scan.shell = Some(shell);
            return scan;
        }
        let path = ["filePath", "notebook_path", "notebookPath"]
            .into_iter()
            .find_map(|key| result[key].as_str())
            .unwrap_or_default();
        let (op, shape, lines) = if result["structuredPatch"]
            .as_array()
            .is_some_and(|hunks| !hunks.is_empty())
        {
            let op = if result["type"].as_str() == Some("update") {
                EditOp::Write
            } else {
                EditOp::Edit
            };
            (
                op,
                EditShape::StructuredPatch,
                Lines::hunks(&result["structuredPatch"]),
            )
        } else if let Some(content) = result["content"]
            .as_str()
            .filter(|_| result["type"].as_str() == Some("create"))
        {
            (EditOp::Write, EditShape::Content, Lines::content(content))
        } else if let (Some(old), Some(new)) =
            (result["oldString"].as_str(), result["newString"].as_str())
        {
            (EditOp::Edit, EditShape::OldNew, Lines::old_new(old, new))
        } else {
            return scan;
        };
        scan.drafts.extend(FileDraft::new(path, op, shape, lines));
        scan
    }

    /// A Pi tool result: an edit records its unified patch in `details.patch`.
    pub(crate) fn pi(message: &Value) -> Self {
        let mut scan = Self::outcome_only(Outcome::from_error_flag(message["isError"].as_bool()));
        if let Some(patch) = message["details"]["patch"].as_str() {
            let path = patch
                .lines()
                .find_map(|line| line.strip_prefix("+++ "))
                .unwrap_or_default();
            scan.drafts.extend(FileDraft::new(
                path.trim(),
                EditOp::Edit,
                EditShape::UnifiedDiff,
                Lines::unified(patch),
            ));
        }
        scan
    }

    /// A Codex tool result's text: a patch application states its ending.
    pub(crate) fn codex_text(text: &str) -> Self {
        let head = text.lines().take(6).collect::<Vec<_>>();
        let outcome = if head
            .iter()
            .any(|line| line.starts_with("apply_patch verification failed"))
        {
            Outcome::Failed
        } else if let Some(code) = head
            .iter()
            .find_map(|line| line.strip_prefix("Exit code: "))
            .and_then(|code| code.trim().parse::<i64>().ok())
        {
            Outcome::from_exit(Some(code))
        } else if let Some(code) = head
            .iter()
            .find_map(|line| line.strip_prefix("Process exited with code "))
            .and_then(|code| code.trim().parse::<i64>().ok())
        {
            Outcome::from_exit(Some(code))
        } else {
            Outcome::Unknown
        };
        Self::outcome_only(outcome)
    }

    /// A Codex `FileChange` item: the runtime's own record of each file it
    /// changed, `{path: {type: update|add|delete, ...}}`.
    pub(crate) fn codex_file_change(item: &Value) -> Self {
        let mut scan = Self::outcome_only(match item["status"].as_str() {
            Some("completed") => Outcome::Succeeded,
            Some("failed") => Outcome::Failed,
            _ => Outcome::Unknown,
        });
        scan.runtime = true;
        for (path, change) in item["changes"].as_object().into_iter().flatten() {
            let (op, shape, lines) = match change["type"].as_str() {
                Some("update") => (
                    EditOp::Edit,
                    EditShape::UnifiedDiff,
                    Lines::unified(change["unified_diff"].as_str().unwrap_or_default()),
                ),
                Some("add") => (
                    EditOp::Write,
                    EditShape::Content,
                    Lines::content(change["content"].as_str().unwrap_or_default()),
                ),
                Some("delete") => (
                    EditOp::Delete,
                    EditShape::Content,
                    Lines::deleted(change["content"].as_str().unwrap_or_default()),
                ),
                _ => continue,
            };
            scan.drafts.extend(FileDraft::new(path, op, shape, lines));
        }
        scan
    }
}

// ------------------------------------------------------------------ calls

/// Git subcommands that move the working tree to other content: the diff such
/// a command leaves is not an edit the session wrote.
const TREE_MOVERS: [&str; 11] = [
    "merge",
    "pull",
    "checkout",
    "switch",
    "rebase",
    "reset",
    "restore",
    "stash",
    "cherry-pick",
    "am",
    "revert",
];

/// What a shell call's command says about changing files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShellFacts {
    moves_tree: bool,
    writes_files: bool,
}

/// What a call's recorded arguments say about the edits it would make.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EditFacts {
    tool: String,
    /// Files the call's own arguments describe the change of.
    drafts: Vec<FileDraft>,
    shell: Option<ShellFacts>,
}

impl EditFacts {
    pub(crate) fn of_call(name: Option<&str>, arguments: &str) -> Option<Self> {
        let tool = name?;
        let drafts = match tool {
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "edit" | "write" | "apply_patch" => {
                call_drafts(tool, arguments)
            }
            _ if shell_commands::is_shell_tool(Some(tool)) => {
                return Some(Self {
                    tool: tool.to_owned(),
                    drafts: Vec::new(),
                    shell: Some(shell_facts(arguments)),
                })
            }
            _ => return None,
        };
        Some(Self {
            tool: tool.to_owned(),
            drafts,
            shell: None,
        })
    }
}

/// A file edit tool's arguments as drafts. Where the recording gives no
/// number for a changed line, the old and new text are diffed here.
fn call_drafts(tool: &str, arguments: &str) -> Vec<FileDraft> {
    let Ok(input) = serde_json::from_str::<Value>(arguments) else {
        // A patch tool's argument is the patch text itself.
        return if tool == "apply_patch" {
            apply_patch(arguments)
        } else {
            Vec::new()
        };
    };
    let text = |key: &str| input[key].as_str().unwrap_or_default();
    let path = ["file_path", "path", "notebook_path"]
        .into_iter()
        .find_map(|key| input[key].as_str())
        .unwrap_or_default();
    let one = |op, shape, lines| FileDraft::new(path, op, shape, lines).into_iter().collect();
    match tool {
        "Edit" => one(
            EditOp::Edit,
            EditShape::OldNew,
            Lines::old_new(text("old_string"), text("new_string")),
        ),
        "MultiEdit" => one(
            EditOp::Edit,
            EditShape::OldNew,
            sum(input["edits"].as_array().into_iter().flatten().map(|edit| {
                Lines::old_new(
                    edit["old_string"].as_str().unwrap_or_default(),
                    edit["new_string"].as_str().unwrap_or_default(),
                )
            })),
        ),
        "Write" | "write" => one(
            EditOp::Write,
            EditShape::Content,
            Lines::content(text("content")),
        ),
        "NotebookEdit" => one(
            EditOp::Edit,
            EditShape::Content,
            Lines::content(text("new_source")),
        ),
        "edit" => one(
            EditOp::Edit,
            EditShape::OldNew,
            sum(input["edits"].as_array().into_iter().flatten().map(|edit| {
                Lines::old_new(
                    edit["oldText"].as_str().unwrap_or_default(),
                    edit["newText"].as_str().unwrap_or_default(),
                )
            })),
        ),
        "apply_patch" => ["input", "patch"]
            .into_iter()
            .find_map(|key| input[key].as_str())
            .map(apply_patch)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The lines of several edits of one file, as one.
fn sum(parts: impl Iterator<Item = Lines>) -> Lines {
    let mut total = Lines::default();
    for part in parts {
        total.added_total += part.added_total;
        total.removed_total += part.removed_total;
        total.truncated |= part.truncated;
        for (side, digests) in [
            (&mut total.added, part.added),
            (&mut total.removed, part.removed),
        ] {
            for digest in digests {
                if side.len() < MAX_DIGESTS_PER_SIDE {
                    side.push(digest);
                } else {
                    total.truncated = true;
                }
            }
        }
    }
    total
}

/// Source files a redirection must name to count as a write: the extension
/// separates an edit from a scratch file or a log.
const SOURCE_EXTENSIONS: [&str; 24] = [
    "rs", "py", "toml", "md", "json", "ts", "tsx", "js", "go", "hs", "c", "h", "cpp", "sh", "yml",
    "yaml", "nix", "cabal", "css", "html", "org", "lock", "cfg", "ini",
];

/// A redirection target that is scratch space rather than a project file: a
/// temporary or cache directory, or a path built from a variable that cannot
/// be resolved here.
fn is_scratch(target: &str) -> bool {
    target.starts_with('$')
        || ["/tmp/", "/.cache/", "/.local/cache/", "/scratchpad/"]
            .iter()
            .any(|marker| target.contains(marker))
}

/// Whether a path a command writes is a project source or document.
fn is_source_file(path: &str) -> bool {
    !is_scratch(path)
        && path
            .rsplit_once('.')
            .is_some_and(|(_, extension)| SOURCE_EXTENSIONS.contains(&extension))
}

fn shell_facts(arguments: &str) -> ShellFacts {
    let mut facts = ShellFacts::default();
    for command in shell_commands::commands(arguments) {
        if let Some((sub, args)) = git_subcommand(&command.words) {
            facts.moves_tree |= TREE_MOVERS.contains(&sub);
            facts.writes_files |= sub == "apply" && !args.contains(&"--check");
        }
        let program = command.words.first().map_or("", |word| basename(word));
        let flagged =
            |predicate: fn(&str) -> bool| command.words[1..].iter().any(|word| predicate(word));
        facts.writes_files |= match program {
            "sed" => flagged(|w| {
                w == "--in-place"
                    || w.starts_with("--in-place=")
                    || (w.starts_with('-') && !w.starts_with("--") && w.contains('i'))
            }),
            "perl" => flagged(|w| w.starts_with('-') && !w.starts_with("--") && w.contains('i')),
            "tee" => command.words[1..]
                .iter()
                .any(|word| !word.starts_with('-') && is_source_file(word)),
            "patch" | "apply_patch" => true,
            _ => false,
        };
        facts.writes_files |= command
            .redirects
            .iter()
            .any(|target| is_source_file(target));
    }
    facts
}

// -------------------------------------------------------------- observing

impl EditFacts {
    /// The edits a paired result yields. `tool` is the call's recorded tool
    /// name, and `superseded` says the harness recorded the same change again
    /// as a runtime item that wins.
    pub(crate) fn observe(
        facts: Option<&Self>,
        tool: Option<&str>,
        scan: &EditScan,
        superseded: bool,
        call: &EditCall,
        at: Option<DateTime<Utc>>,
    ) -> Option<EventEdits> {
        if scan.outcome == Outcome::Failed {
            return None;
        }
        let tool = tool
            .or(facts.map(|facts| facts.tool.as_str()))
            .unwrap_or("");
        let mut edits = EventEdits::default();
        let mut drafts: Vec<&FileDraft> = Vec::new();
        let shell = facts.and_then(|facts| facts.shell);
        if let Some(diff) = &scan.shell {
            edits.more_files = diff.more_files;
            edits.shared = diff.shared;
            if shell.is_some_and(|shell| shell.moves_tree) {
                edits.tree_movement = Some(TreeMovement {
                    files: diff.files.len(),
                    more_files: diff.more_files,
                    added_lines: diff.files.iter().map(|file| file.lines.added_total).sum(),
                    removed_lines: diff.files.iter().map(|file| file.lines.removed_total).sum(),
                });
            } else {
                drafts.extend(&diff.files);
            }
            edits.unrecognized =
                diff.files.is_empty() && shell.is_some_and(|shell| shell.writes_files);
        } else if !scan.drafts.is_empty() {
            drafts.extend(&scan.drafts);
        } else if scan.outcome == Outcome::Succeeded && !superseded {
            if let Some(facts) = facts {
                drafts.extend(&facts.drafts);
            }
        }
        if scan.shell.is_none() && drafts.is_empty() {
            edits.unrecognized = shell.is_some_and(|shell| shell.writes_files);
        }
        edits.truncated = drafts.len() > MAX_CALL_EDITS;
        edits.observations = drafts
            .into_iter()
            .take(MAX_CALL_EDITS)
            .map(|draft| EditObservation {
                at,
                path: draft.path.clone(),
                op: draft.op,
                added: draft.lines.added.clone(),
                removed: draft.lines.removed.clone(),
                truncated: draft.lines.truncated,
                source: EditSource {
                    tool: tool.to_owned(),
                    shape: draft.shape,
                },
                call: call.clone(),
            })
            .collect();
        let silent = edits.observations.is_empty()
            && !edits.truncated
            && edits.tree_movement.is_none()
            && edits.more_files == 0
            && !edits.shared
            && !edits.unrecognized;
        (!silent).then_some(edits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digests(lines: &[&str]) -> Vec<u64> {
        lines.iter().filter_map(|line| line_digest(line)).collect()
    }

    #[test]
    fn hunks_digest_their_added_and_removed_lines_and_ignore_context() {
        let lines = Lines::hunks(
            &json!([{"lines": [" keep", "-old line", "+new line", "+}", "\\ No newline at end of file"]}]),
        );
        assert_eq!(lines.added, digests(&["new line"]));
        assert_eq!(lines.removed, digests(&["old line"]));
        assert_eq!((lines.added_total, lines.removed_total), (2, 1));
    }

    #[test]
    fn old_and_new_text_differ_only_where_they_differ() {
        let lines = Lines::old_new("a\nb\nc\nd\n", "a\nB\nC\nd\n");
        assert_eq!(lines.removed, digests(&["b", "c"]));
        assert_eq!(lines.added, digests(&["B", "C"]));
    }

    #[test]
    fn a_unified_diff_is_read_past_its_file_headers() {
        let patch = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-one\n+two\n same\n";
        let lines = Lines::unified(patch);
        assert_eq!(lines.removed, digests(&["one"]));
        assert_eq!(lines.added, digests(&["two"]));
    }

    #[test]
    fn a_patch_gives_one_draft_per_file() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n context\n-old\n+new\n*** Add File: b.rs\n+fresh\n+start\n*** Delete File: c.rs\n*** End Patch\n";
        let drafts = apply_patch(patch);
        let seen: Vec<_> = drafts.iter().map(|d| (d.path.as_str(), d.op)).collect();
        assert_eq!(
            seen,
            [
                ("a.rs", EditOp::Edit),
                ("b.rs", EditOp::Write),
                ("c.rs", EditOp::Delete)
            ]
        );
        assert_eq!(drafts[0].lines.added, digests(&["new"]));
        assert_eq!(drafts[1].lines.added, digests(&["fresh", "start"]));
    }

    #[test]
    fn digests_stop_at_the_per_edit_cap_and_say_so() {
        let text = (0..MAX_DIGESTS_PER_SIDE + 5)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        let lines = Lines::content(&text);
        assert_eq!(lines.added.len(), MAX_DIGESTS_PER_SIDE);
        assert!(lines.truncated);
        assert_eq!(lines.added_total, MAX_DIGESTS_PER_SIDE + 5);
    }

    #[test]
    fn shell_commands_are_told_apart_by_what_they_do_to_the_tree_and_files() {
        let facts = |command: &str| {
            let arguments = json!({ "command": command }).to_string();
            shell_facts(&arguments)
        };
        assert!(facts("git checkout main").moves_tree);
        assert!(facts("cd x && git -C y rebase --continue").moves_tree);
        assert!(facts("git stash pop").moves_tree);
        assert!(!facts("git diff HEAD").moves_tree);
        assert!(facts("sed -i 's/a/b/' f.rs").writes_files);
        assert!(facts("sed -ni '1p' f.rs").writes_files);
        assert!(!facts("sed -n '1p' f.rs").writes_files);
        assert!(facts("cat > notes.md <<'E'\nbody\nE").writes_files);
        assert!(!facts("cat > /tmp/x.md <<'E'\nbody\nE").writes_files);
        assert!(!facts("cat > $S/notes.md <<'E'\nbody\nE").writes_files);
        assert!(!facts("cat > ~/.cache/p/notes.md <<'E'\nbody\nE").writes_files);
        assert!(!facts("cargo build 2>&1 | tail").writes_files);
        assert!(facts("perl -pi -e 's/a/b/' f.py").writes_files);
        assert!(facts("git apply fix.patch").writes_files);
        assert!(facts("make | tee build/out.rs").writes_files);
        assert!(!facts("make 2>&1 | tee /tmp/build.log").writes_files);
    }
}
