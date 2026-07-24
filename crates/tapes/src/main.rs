use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tapes_core::model::{Role, Session, Transcript};

#[derive(Parser)]
#[command(name = "tapes", about = "Read and export coding-agent sessions")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List available sessions.
    List {
        /// Restrict results to one harness.
        #[arg(long)]
        harness: Option<String>,
        /// Restrict results to the current repository.
        #[arg(long)]
        here: bool,
        /// Return at most this many sessions.
        #[arg(long)]
        limit: Option<usize>,
        /// Render results as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one session.
    Show {
        /// Session identifier.
        session: String,
        /// Show only the final number of messages.
        #[arg(long)]
        tail: Option<usize>,
        /// Render the session as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Export one session.
    Export {
        /// Session identifier.
        session: String,
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
    match cli.command {
        Command::List {
            harness,
            here,
            limit,
            json,
        } => {
            let result = tapes_core::list(harness.as_deref(), here, limit)?;
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                print_session_list(&result.sessions);
                print_availability_note(&result);
            }
        }
        Command::Show {
            session,
            tail,
            json,
        } => {
            let transcript = tapes_core::show(&session, tail)?;
            if json {
                println!("{}", serde_json::to_string(&transcript)?);
            } else {
                print_transcript(&transcript);
            }
        }
        Command::Export { session, bundle } => {
            eprintln!("{}", tapes_core::export(&session, bundle.as_deref()));
        }
    }
    Ok(())
}

fn print_session_list(sessions: &[Session]) {
    if sessions.is_empty() {
        return;
    }
    println!("ID\tHARNESS\tMODEL\tTITLE\tDIRECTORY\tLAST ACTIVITY");
    for session in sessions {
        let model = session.model.as_ref().map_or_else(String::new, |model| {
            model.variant.as_ref().map_or_else(
                || model.id.clone(),
                |variant| format!("{} ({variant})", model.id),
            )
        });
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            session.id,
            session.harness,
            model,
            session.title.as_deref().unwrap_or_default(),
            session
                .directory
                .as_deref()
                .map_or_else(String::new, |path| path.display().to_string()),
            session.last_activity_at.to_rfc3339()
        );
    }
}

fn print_availability_note(result: &tapes_core::SessionList) {
    if result.unavailable.is_empty() {
        return;
    }
    if result.unavailable.len() == 4 {
        println!("No harnesses available.");
    }
    println!("Unavailable: {}", result.unavailable.join(", "));
}

fn print_transcript(transcript: &Transcript) {
    for turn in &transcript.turns {
        let role = match turn.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
            Role::Reasoning => "reasoning",
        };
        if let Some(ts) = turn.ts {
            println!("[{role} {}]\n{}", ts.to_rfc3339(), turn.text);
        } else {
            println!("[{role}]\n{}", turn.text);
        }
    }
    for note in &transcript.notes {
        println!("Note: {note}");
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
