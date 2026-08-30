mod guide;
mod liveness;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use tapes_core::bundle::Bundle;
use tapes_core::model::{human_timestamp, human_title, LiveState, Role, Session, Transcript};
use tapes_core::{Selection, Where};

#[derive(Parser)]
#[command(
    name = "tapes",
    about = "Read and export coding-agent sessions",
    after_help = "Run `tapes` with no arguments for the workflow guide."
)]
struct Cli {
    /// Absent prints the workflow guide: what tapes owns, the order the
    /// commands are used in, and how to judge what comes back.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Which project's sessions a command may see.
#[derive(Args)]
struct ScopeArgs {
    /// Restrict to the project containing the current directory: every
    /// worktree of its repository, or the directory's subtree when it is not
    /// in one.
    #[arg(long, conflicts_with_all = ["project", "global"])]
    here: bool,
    /// Restrict to the project containing this path.
    #[arg(long, conflicts_with = "global")]
    project: Option<PathBuf>,
    /// Look at every session on the machine.
    #[arg(long)]
    global: bool,
}

impl ScopeArgs {
    fn within(&self) -> Where<'_> {
        match (&self.project, self.here) {
            (Some(path), _) => Where::Project(path),
            (None, true) => Where::Here,
            (None, false) => Where::Global,
        }
    }

    /// `--latest` answers "the session I was just in", so it looks at this
    /// project unless told otherwise. `list` keeps the opposite default: it
    /// is the survey, and a survey that hides other projects would be a
    /// surprise.
    fn within_or_here(&self) -> Where<'_> {
        match (&self.project, self.global) {
            (Some(path), _) => Where::Project(path),
            (None, true) => Where::Global,
            (None, false) => Where::Here,
        }
    }
}

/// Which session a command acts on: one named, or the latest in scope.
/// A named session is looked up by id across every store, so every flag that
/// narrows a *search* is a contradiction beside one — and silently ignoring
/// them would answer a question the caller did not ask.
#[derive(Args)]
struct SelectionArgs {
    /// Session identifier, full or an unambiguous prefix.
    #[arg(
        required_unless_present = "latest",
        conflicts_with_all = ["latest", "exclude", "harness", "here", "project", "global"]
    )]
    session: Option<String>,
    /// Take the most recent session in scope instead of naming one. A
    /// caller asking from inside a live session is usually itself the most
    /// recent one in its own project, so reaching an older session takes
    /// `--exclude <own-id>`.
    #[arg(long)]
    latest: bool,
    /// Pass over this session when taking the latest. Repeatable. An agent
    /// asking from inside its own session passes its own id here.
    #[arg(long, requires = "latest")]
    exclude: Vec<String>,
    /// With --latest, take the most recent session of one harness.
    #[arg(long, requires = "latest")]
    harness: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

impl SelectionArgs {
    fn selection(&self) -> Selection<'_> {
        match &self.session {
            Some(session) => Selection::Id(session),
            None => Selection::Latest {
                within: self.scope.within_or_here(),
                harness: self.harness.as_deref(),
                exclude: &self.exclude,
            },
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// List available sessions. Human output keeps the exact session id in
    /// the ID column and puts any present-tense state in a separate LIVE
    /// column. If harness-status is unavailable, malformed, oversized, or
    /// slower than its 250 ms deadline, LIVE is blank.
    List {
        /// Restrict results to one harness.
        #[arg(long)]
        harness: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Take at most this many sessions from each harness [default: 20].
        /// The scope and metadata filters apply first, so a bound never hides
        /// a match.
        #[arg(long)]
        limit: Option<usize>,
        /// Match case-insensitively against the full model identity shown in
        /// the MODEL column: the id and, when present, its `id (variant)`
        /// spelling. Sessions without a model never match.
        #[arg(long, value_name = "SUBSTRING")]
        model: Option<String>,
        /// Match case-insensitively against the recorded directory path.
        /// Sessions without a directory never match.
        #[arg(long, value_name = "SUBSTRING")]
        directory: Option<String>,
        /// Render results as JSON. Matching sessions may include an optional
        /// `live` field supplied by harness-status.
        #[arg(long)]
        json: bool,
    },
    /// Show one session. The human header marks a matching live session when
    /// harness-status is reachable.
    Show {
        #[command(flatten)]
        selection: SelectionArgs,
        /// Show only the final number of messages.
        #[arg(long)]
        tail: Option<usize>,
        /// Render the session as JSON. The session may include an optional
        /// `live` field supplied by harness-status.
        #[arg(long)]
        json: bool,
    },
    /// Export one session.
    Export {
        #[command(flatten)]
        selection: SelectionArgs,
        /// Directory for the exported bundle.
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    reset_sigpipe();
    dispatch(Cli::parse())
}

fn dispatch(cli: Cli) -> Result<()> {
    let Some(command) = cli.command else {
        guide::print();
        return Ok(());
    };
    match command {
        Command::List {
            harness,
            scope,
            limit,
            model,
            directory,
            json,
        } => {
            let mut result = tapes_core::list_with_filters(
                harness.as_deref(),
                scope.within(),
                limit,
                model.as_deref(),
                directory.as_deref(),
            )?;
            liveness::annotate(&mut result.sessions);
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                print_session_list(&result.sessions);
                print_availability_note(&result);
            }
        }
        Command::Show {
            selection,
            tail,
            json,
        } => {
            let by_latest = selection.latest;
            let mut transcript = tapes_core::show(selection.selection(), tail)?;
            liveness::annotate(std::slice::from_mut(&mut transcript.session));
            if json {
                println!("{}", serde_json::to_string(&transcript)?);
            } else {
                print_transcript(&transcript, by_latest);
            }
        }
        Command::Export { selection, bundle } => {
            let bundle = tapes_core::export(selection.selection(), bundle.as_deref())?;
            print_manifest(&bundle);
        }
    }
    Ok(())
}

fn print_session_list(sessions: &[Session]) {
    if sessions.is_empty() {
        return;
    }
    println!("ID\tLIVE\tHARNESS\tMODEL\tTITLE\tDIRECTORY\tLAST ACTIVITY");
    for session in sessions {
        let model = session
            .model
            .as_ref()
            .map_or_else(String::new, |model| model.identity());
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            session.id,
            live_label(session),
            session.harness,
            model,
            human_title(session),
            session
                .directory
                .as_deref()
                .map_or_else(String::new, |path| path.display().to_string()),
            human_timestamp(session.last_activity_at)
        );
    }
}

fn live_label(session: &Session) -> &'static str {
    match session.live {
        Some(LiveState::Working) => "working",
        Some(LiveState::Idle) => "idle",
        None => "",
    }
}

fn print_availability_note(result: &tapes_core::SessionList) {
    if result.scan_truncated {
        println!(
            "The search stopped early after {} candidates; older sessions were not inspected. \
             Raise --limit to widen the scan, or `tapes export` a session once it is found.",
            result.scanned
        );
    }
    for session in &result.unreadable {
        println!("Unreadable: {session}");
    }
    if result.unavailable.is_empty() {
        return;
    }
    if result.unavailable.len() == 4 {
        println!("No harnesses available.");
    }
    println!("Unavailable: {}", result.unavailable.join(", "));
}

/// The manifest is the whole stdout contract for `export`: three paths, three
/// sizes, in the order a rescuer should read them.
fn print_manifest(bundle: &Bundle) {
    for file in bundle.files() {
        println!("{}\t{}", file.path.display(), human_bytes(file.bytes));
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    match bytes {
        bytes if bytes >= MIB => format!("{:.1} MiB", bytes as f64 / MIB as f64),
        bytes if bytes >= KIB => format!("{:.1} KiB", bytes as f64 / KIB as f64),
        bytes => format!("{bytes} B"),
    }
}

fn print_transcript(transcript: &Transcript, by_latest: bool) {
    print!("{}", render_transcript(transcript, by_latest));
}

/// The human render carries the same two partiality signals the JSON does.
/// `--tail` is the recommended first probe, so a window that does not say it
/// is one would be read as the whole session. A session reached by `--latest`
/// names itself, because the caller did not name it and may have been handed
/// its own session.
fn render_transcript(transcript: &Transcript, by_latest: bool) -> String {
    let mut out = String::new();
    if transcript.session.live.is_some() || by_latest {
        out.push_str(&format!(
            "# {} {}{}\n",
            transcript.session.harness,
            transcript.session.id,
            live_marker(&transcript.session)
        ));
    }
    for turn in &transcript.turns {
        let role = match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Reasoning => "reasoning",
        };
        if let Some(ts) = turn.ts {
            out.push_str(&format!(
                "[{role} {}]\n{}\n",
                human_timestamp(ts),
                turn.text
            ));
        } else {
            out.push_str(&format!("[{role}]\n{}\n", turn.text));
        }
    }
    if let Some(newest) = transcript.turns.iter().rev().find_map(|turn| turn.ts) {
        if transcript.session.last_activity_at > newest {
            out.push_str(&format!(
                "Note: The store records activity at {}, after the newest turn rendered here ({}). \
                 What came later is a record `show` does not render as a turn.\n",
                human_timestamp(transcript.session.last_activity_at),
                human_timestamp(newest)
            ));
        }
    }
    if by_latest {
        out.push_str(&format!(
            "Note: --latest picked the newest session in scope. Pass --exclude {} to reach the one before it.\n",
            transcript.session.id
        ));
    }
    if transcript.truncated {
        out.push_str(
            "Note: Truncated — earlier turns are not shown. Use --tail N for a larger window, \
             or `tapes export` for the whole session.\n",
        );
    }
    for note in &transcript.notes {
        out.push_str(&format!("Note: {note}\n"));
    }
    out
}

fn live_marker(session: &Session) -> String {
    match session.live {
        Some(LiveState::Working) => " [working]".to_owned(),
        Some(LiveState::Idle) => " [idle]".to_owned(),
        None => String::new(),
    }
}

fn reset_sigpipe() {
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;

    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use tapes_core::model::Turn;

    fn transcript(truncated: bool) -> Transcript {
        Transcript {
            session: Session {
                id: "s1".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                title: None,
                derived_title: None,
                directory: None,
                started_at: Utc::now(),
                last_activity_at: Utc::now(),
                live: None,
                cost: None,
                tokens: None,
            },
            turns: vec![Turn {
                role: Role::User,
                text: "fix the parser".to_owned(),
                ts: None,
            }],
            truncated,
            notes: vec!["Skipped 1 unparseable line.".to_owned()],
        }
    }

    #[test]
    fn the_human_render_says_when_it_is_a_window() {
        let windowed = render_transcript(&transcript(true), false);
        assert!(windowed.contains("Truncated"), "{windowed}");
        assert!(windowed.contains("--tail N"), "{windowed}");
        assert!(windowed.contains("tapes export"), "{windowed}");
        assert!(windowed.contains("Skipped 1 unparseable line."));

        let whole = render_transcript(&transcript(false), false);
        assert!(!whole.contains("Truncated"), "{whole}");
        assert!(whole.contains("Skipped 1 unparseable line."));
        assert!(whole.starts_with("[user]"), "{whole}");
    }

    /// `list` reads activity from the newest record in the store, `show`
    /// renders only turns, and a session whose last record is harness state
    /// leaves the two disagreeing. The render says so rather than leaving a
    /// reader to open the raw store.
    #[test]
    fn a_stamp_past_the_newest_rendered_turn_is_named() {
        let newest_turn = "2026-08-23T23:24:52Z".parse::<DateTime<Utc>>().unwrap();
        let mut session = transcript(false);
        session.turns[0].ts = Some(newest_turn);
        session.session.last_activity_at = "2026-08-23T23:27:56Z".parse().unwrap();
        let rendered = render_transcript(&session, false);
        assert!(rendered.contains("does not render as a turn"), "{rendered}");
        assert!(rendered.contains("23:27:56"), "{rendered}");
        assert!(rendered.contains("23:24:52"), "{rendered}");

        session.session.last_activity_at = newest_turn;
        let agreeing = render_transcript(&session, false);
        assert!(
            !agreeing.contains("does not render as a turn"),
            "{agreeing}"
        );
    }

    /// A caller that did not name a session is told which one it got and how
    /// to step past it, since the newest session in a project is usually the
    /// caller itself.
    #[test]
    fn latest_names_the_session_it_picked_and_how_to_skip_it() {
        let picked = render_transcript(&transcript(false), true);
        assert!(picked.starts_with("# claude s1"), "{picked}");
        assert!(picked.contains("--exclude s1"), "{picked}");

        let named = render_transcript(&transcript(false), false);
        assert!(!named.contains("--exclude"), "{named}");
    }
}
