use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

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
    dispatch(Cli::parse());
    Ok(())
}

fn dispatch(cli: Cli) {
    match cli.command {
        Command::List {
            harness,
            here,
            limit,
            json,
        } => {
            let sessions = tapes_core::list(harness.as_deref(), here, limit);
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&sessions).expect("empty session list is serializable")
                );
            } else {
                for session in sessions {
                    println!("{session}");
                }
            }
        }
        Command::Show {
            session,
            tail,
            json: _,
        } => eprintln!("{}", tapes_core::show(&session, tail)),
        Command::Export { session, bundle } => {
            eprintln!("{}", tapes_core::export(&session, bundle.as_deref()));
        }
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
