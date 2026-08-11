use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use crate::model::{Role, Session, Transcript, SESSION_SCHEMA};

/// One exported file: where it landed and how big it is.
pub struct BundleFile {
    pub path: PathBuf,
    pub bytes: u64,
}

/// The three files an export writes, in the order a rescuer should read them.
pub struct Bundle {
    pub context: BundleFile,
    pub json: BundleFile,
    pub trace: BundleFile,
}

impl Bundle {
    pub fn files(&self) -> [&BundleFile; 3] {
        [&self.context, &self.json, &self.trace]
    }
}

/// The commit a session's working directory sat on, when that is knowable.
#[derive(Debug, Serialize)]
pub struct GitContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Serialize)]
struct BundleJson<'a> {
    schema: &'static str,
    session: &'a Session,
    turns: &'a [crate::model::Turn],
    truncated: bool,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    notes: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    git: Option<&'a GitContext>,
}

pub fn export(transcript: &Transcript, directory: &Path) -> Result<Bundle> {
    let prefix = directory.join(bundle_stem(&transcript.session));
    let mut session = transcript.session.clone();
    session.live = None;
    let git = git_context(session.directory.as_deref());

    let json = serde_json::to_string_pretty(&BundleJson {
        schema: SESSION_SCHEMA,
        session: &session,
        turns: &transcript.turns,
        truncated: transcript.truncated,
        notes: &transcript.notes,
        git: git.as_ref(),
    })
    .context("failed to serialize the bundle JSON")?;

    Ok(Bundle {
        context: write_atomically(&prefix, "context.md", &render_context(transcript))?,
        json: write_atomically(&prefix, "json", &json)?,
        trace: write_atomically(&prefix, "trace.md", &render_trace(transcript))?,
    })
}

/// A prefix that sorts by export time and still names its session.
fn bundle_stem(session: &Session) -> String {
    let id = session
        .id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!(
        "{}-{}-{id}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        session.harness
    )
}

/// Write to a temporary name in the target directory, then rename, so a
/// half-written bundle never looks complete.
fn write_atomically(prefix: &Path, extension: &str, body: &str) -> Result<BundleFile> {
    let mut path = prefix.as_os_str().to_owned();
    path.push(".");
    path.push(extension);
    let path = PathBuf::from(path);

    let mut temporary = path.clone().into_os_string();
    temporary.push(".partial");
    let temporary = PathBuf::from(temporary);

    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
    }
    fs::write(&temporary, body)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("failed to place {}", path.display()))?;

    Ok(BundleFile {
        path,
        bytes: body.len() as u64,
    })
}

/// Operator turns and assistant-visible text only — small enough to read whole.
fn render_context(transcript: &Transcript) -> String {
    let mut out = String::new();
    write_header(&mut out, transcript, "context");
    for turn in &transcript.turns {
        let speaker = match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool | Role::Reasoning => continue,
        };
        write_turn_heading(&mut out, speaker, turn.ts);
        out.push_str(turn.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

/// Every turn, reasoning and tool chronology included, for free-text search.
fn render_trace(transcript: &Transcript) -> String {
    let mut out = String::new();
    write_header(&mut out, transcript, "trace");
    for turn in &transcript.turns {
        match turn.role {
            Role::User => write_turn_heading(&mut out, "user", turn.ts),
            Role::Assistant => write_turn_heading(&mut out, "assistant", turn.ts),
            Role::Reasoning => write_turn_heading(&mut out, "reasoning", turn.ts),
            Role::Tool => {
                let label = tool_label(&turn.text);
                write_turn_heading(&mut out, &format!("tool: {label}"), turn.ts);
            }
        }
        out.push_str(turn.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

fn write_header(out: &mut String, transcript: &Transcript, kind: &str) {
    let session = &transcript.session;
    let _ = writeln!(out, "# {} {} ({kind})", session.harness, session.id);
    let _ = writeln!(out);
    if let Some(title) = &session.title {
        let _ = writeln!(out, "- title: {title}");
    }
    if let Some(model) = &session.model {
        match &model.variant {
            Some(variant) => {
                let _ = writeln!(out, "- model: {} ({variant})", model.id);
            }
            None => {
                let _ = writeln!(out, "- model: {}", model.id);
            }
        }
    }
    if let Some(directory) = &session.directory {
        let _ = writeln!(out, "- directory: {}", directory.display());
    }
    let _ = writeln!(
        out,
        "- last activity: {}",
        session.last_activity_at.to_rfc3339()
    );
    if transcript.truncated {
        let _ = writeln!(out, "- truncated: this is a window, not the whole session");
    }
    for note in &transcript.notes {
        let _ = writeln!(out, "- note: {note}");
    }
    let _ = writeln!(out);
}

fn write_turn_heading(out: &mut String, speaker: &str, ts: Option<chrono::DateTime<Utc>>) {
    match ts {
        Some(ts) => {
            let _ = writeln!(out, "## {speaker} — {}", ts.to_rfc3339());
        }
        None => {
            let _ = writeln!(out, "## {speaker}");
        }
    }
    let _ = writeln!(out);
}

/// Tool turns carry the harness's own JSON envelope. Lead with whatever names
/// the tool so the trace is scannable; the envelope stays beneath it.
fn tool_label(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return "unnamed".to_owned();
    };
    for key in ["name", "toolName", "tool_name"] {
        if let Some(name) = value[key].as_str() {
            return name.to_owned();
        }
    }
    for key in ["tool_use_id", "call_id", "callID", "toolCallId"] {
        if value[key].is_string() {
            return "result".to_owned();
        }
    }
    "unnamed".to_owned()
}

fn git_context(directory: Option<&Path>) -> Option<GitContext> {
    let directory = directory?;
    if !directory.is_dir() {
        return None;
    }
    let head = git(directory, &["rev-parse", "HEAD"]);
    let branch =
        git(directory, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|branch| branch != "HEAD");
    (head.is_some() || branch.is_some()).then_some(GitContext { head, branch })
}

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::model::{LiveState, Model, Turn};

    fn transcript() -> Transcript {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Transcript {
            session: Session {
                id: "ses_abc".into(),
                harness: "opencode".into(),
                model: Some(Model {
                    id: "kimi-k3".into(),
                    variant: Some("max".into()),
                }),
                title: Some("A rescue".into()),
                directory: None,
                started_at: ts,
                last_activity_at: ts,
                live: None,
                cost: None,
                tokens: None,
            },
            turns: vec![
                Turn {
                    role: Role::User,
                    text: "fix the parser".into(),
                    ts: Some(ts),
                },
                Turn {
                    role: Role::Reasoning,
                    text: "the parser drops empty lines".into(),
                    ts: Some(ts),
                },
                Turn {
                    role: Role::Tool,
                    text: r#"{"name":"shell","input":{"command":"cargo test"}}"#.into(),
                    ts: Some(ts),
                },
                Turn {
                    role: Role::Assistant,
                    text: "fixed it".into(),
                    ts: Some(ts),
                },
            ],
            truncated: false,
            notes: vec!["1 entry belongs to an abandoned branch.".into()],
        }
    }

    /// Each test gets its own directory: they run in parallel and every one of
    /// them counts and then removes the directory's contents.
    fn export_into_temporary_directory(test: &str) -> (PathBuf, Bundle) {
        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let bundle = export(&transcript(), &directory).unwrap();
        (directory, bundle)
    }

    #[test]
    fn export_writes_three_files_sharing_one_prefix() {
        let (directory, bundle) = export_into_temporary_directory("three-files");

        let names = bundle
            .files()
            .map(|file| file.path.file_name().unwrap().to_str().unwrap().to_owned());
        let stem = names[0].strip_suffix(".context.md").unwrap().to_owned();
        assert_eq!(names[1], format!("{stem}.json"));
        assert_eq!(names[2], format!("{stem}.trace.md"));
        assert!(stem.ends_with("-opencode-ses_abc"));
        for file in bundle.files() {
            assert!(file.path.is_file(), "{} is missing", file.path.display());
        }
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 3);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn context_holds_operator_turns_and_excludes_reasoning_and_tools() {
        let (directory, bundle) = export_into_temporary_directory("context");
        let context = fs::read_to_string(&bundle.context.path).unwrap();

        assert!(context.contains("fix the parser"));
        assert!(context.contains("fixed it"));
        assert!(!context.contains("the parser drops empty lines"));
        assert!(!context.contains("cargo test"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn trace_holds_the_whole_chronology_and_names_its_tools() {
        let (directory, bundle) = export_into_temporary_directory("trace");
        let trace = fs::read_to_string(&bundle.trace.path).unwrap();

        assert!(trace.contains("## reasoning"));
        assert!(trace.contains("the parser drops empty lines"));
        assert!(trace.contains("## tool: shell"));
        assert!(trace.contains("cargo test"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn json_carries_the_schema_and_the_transcript_notes() {
        let (directory, bundle) = export_into_temporary_directory("json");
        let value: Value =
            serde_json::from_str(&fs::read_to_string(&bundle.json.path).unwrap()).unwrap();

        assert_eq!(value["schema"], SESSION_SCHEMA);
        assert_eq!(value["session"]["id"], "ses_abc");
        assert_eq!(value["turns"].as_array().unwrap().len(), 4);
        assert_eq!(value["notes"][0], "1 entry belongs to an abandoned branch.");
        assert!(value.get("git").is_none());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn json_omits_volatile_live_state() {
        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-{}-live-state", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let mut transcript = transcript();
        transcript.session.live = Some(LiveState::Working);
        let bundle = export(&transcript, &directory).unwrap();
        let value: Value =
            serde_json::from_str(&fs::read_to_string(&bundle.json.path).unwrap()).unwrap();

        assert!(value["session"].get("live").is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn no_partial_files_survive_a_completed_export() {
        let (directory, _) = export_into_temporary_directory("atomic");

        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            assert!(
                !path.to_string_lossy().ends_with(".partial"),
                "{} survived",
                path.display()
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tool_labels_come_from_whichever_key_the_harness_uses() {
        assert_eq!(tool_label(r#"{"name":"shell"}"#), "shell");
        assert_eq!(tool_label(r#"{"tool_use_id":"toolu_1"}"#), "result");
        assert_eq!(tool_label(r#"{"call_id":"call_1"}"#), "result");
        assert_eq!(tool_label("not json"), "unnamed");
    }
}
