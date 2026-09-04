mod guide;
mod liveness;

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tapes_core::bundle::{Bundle, BundleFile};
use tapes_core::endings::{Ending, EndingsReport, DEFAULT_ENDINGS_TAIL};
use tapes_core::event::{EventKind, EventRecord, EventTranscript, Incomplete};
use tapes_core::lineage::{ChildRef, LineageView};
use tapes_core::model::{
    human_bytes, human_speaker, human_timestamp, human_title, speaker, Accounting, AccountingBasis,
    AccountingCoverage, Cost, LiveState, Session, SourceBound, Tokens, Transcript, Truncation,
};
use tapes_core::stats::{
    Coverage, LineageStats, StatsView, TimeStats, ToolNameStats, ToolStats, TurnKindCounts,
    UsageStats, Warning,
};
use tapes_core::usage::{
    Durations, GroupBy, GroupKey, ModelUsage, RateLimits, RateWindow, TurnCoverage, UsageTally,
    UsageView,
};
use tapes_core::{BulkExport, Selection, UsageSummary, Where};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SortArg {
    Newest,
    Oldest,
}

impl From<SortArg> for tapes_core::ListSort {
    fn from(sort: SortArg) -> Self {
        match sort {
            SortArg::Newest => Self::Newest,
            SortArg::Oldest => Self::Oldest,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ByArg {
    Harness,
    Model,
    Variant,
    Directory,
}

impl From<ByArg> for GroupBy {
    fn from(by: ByArg) -> Self {
        match by {
            ByArg::Harness => Self::Harness,
            ByArg::Model => Self::Model,
            ByArg::Variant => Self::Variant,
            ByArg::Directory => Self::Directory,
        }
    }
}

/// The grouping a summary uses: what was asked for, or the harness and model
/// split that answers "what spent this" without being asked.
fn grouping(by: &[ByArg]) -> Vec<GroupBy> {
    if by.is_empty() {
        return vec![GroupBy::Harness, GroupBy::Model];
    }
    let mut dimensions = Vec::new();
    for dimension in by.iter().copied().map(GroupBy::from) {
        if !dimensions.contains(&dimension) {
            dimensions.push(dimension);
        }
    }
    dimensions
}

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
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is at or after this timestamp. RFC 3339 timestamps with an offset
        /// and bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = tapes_core::parse_activity_timestamp)]
        since: Option<tapes_core::ActivityTimestamp>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is before this timestamp. RFC 3339 timestamps with an offset and
        /// bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = tapes_core::parse_activity_timestamp)]
        until: Option<tapes_core::ActivityTimestamp>,
        /// Order by `last_activity_at`: newest first by default, or oldest
        /// first. The order decides which sessions --limit keeps: each
        /// harness's newest matches, or its oldest, which inspects every
        /// candidate the scan reaches. Equal timestamps are ordered by
        /// session id, then harness.
        #[arg(long, value_enum, default_value_t = SortArg::Newest)]
        sort: SortArg,
        /// Match case-insensitively against the last 32 normalized turns in
        /// each candidate session. The fixed tail keeps listing bounded; a
        /// match outside it is not considered. Search is applied before
        /// --limit, and a failed bounded read is reported as unsearched. A
        /// short preflight notice is written to stderr before scanning. If the
        /// local OpenCode2 API server cannot start or list candidates, search
        /// falls back to CLI API GETs and records that stage's diagnostic.
        #[arg(long, value_name = "SUBSTRING")]
        search: Option<String>,
        /// Render results as JSON. Matching sessions may include optional
        /// `live` and `accounting` fields; accounting states the basis and
        /// coverage of any recorded cost or token counters.
        #[arg(long)]
        json: bool,
    },
    /// Show one session. Every turn carries a `kind` naming what the harness
    /// recorded it as, and a user turn holding a harness command, notice, or
    /// attached context is headed `user/<kind>` rather than `user`. The human
    /// header marks a matching live session when harness-status is reachable,
    /// and a verified trailing record is named when the store ends after its
    /// last rendered turn. Activity comparisons use the whole-second
    /// timestamps shown to the reader.
    Show {
        #[command(flatten)]
        selection: SelectionArgs,
        /// Show only the final number of messages.
        #[arg(long)]
        tail: Option<usize>,
        /// Render the session as JSON. The session may include optional
        /// `live`, `accounting`, and `trailing_record` fields supplied by its
        /// authorities.
        #[arg(long)]
        json: bool,
    },
    /// Project typed tool calls and results from one session. Pairing is exact
    /// within the bounded read; an unpaired event names which read boundary
    /// prevented a complete pair.
    Events {
        #[command(flatten)]
        selection: SelectionArgs,
        /// Return events on turns within the final N-turn ordinal window. The
        /// default is every turn the bounded reader reaches.
        #[arg(long)]
        tail: Option<usize>,
        /// Match the recorded tool name exactly. Repeatable.
        #[arg(long, value_name = "NAME")]
        name: Vec<String>,
        /// Match the recorded tool call identifier exactly. Repeatable.
        #[arg(long, value_name = "ID")]
        call_id: Vec<String>,
        /// Render the versioned tapes-events/1 object as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Which sessions a recording names as its relatives: the session it was
    /// spawned from or forked from, and the children its own store records,
    /// each with the role, model, timestamps, and outcome the harness wrote.
    /// A relationship exists only where a record states it; nothing is
    /// inferred from directories, titles, or timestamps. A reference the
    /// store cannot resolve is kept and marked. A child is referred to, never
    /// absorbed: read its transcript with `show` under its own id.
    Lineage {
        #[command(flatten)]
        selection: SelectionArgs,
        /// Render the versioned tapes-lineage/1 object as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Count what one session's recording holds: turns by kind, tool calls
    /// by name with their paired durations and error counts, the recorded
    /// clock, the session's own token counters with the share of
    /// `input + cache_read + cache_write` its cache accounts for, and the
    /// children its store names. Every figure is a count of records the
    /// harness wrote, and every total says what it covers: turn coverage is
    /// `read-window` when a source bound withheld turns, durations come from
    /// complete pairs only, and a cache ratio is a share of recorded token
    /// counts rather than of cost. Nothing is judged, ranked, or explained.
    Stats {
        #[command(flatten)]
        selection: SelectionArgs,
        /// Render the versioned tapes-stats/1 object as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Where quota went. A session named by id or reached with --latest
    /// answers that session as tapes-usage/1: its recorded tokens, cost, and
    /// turn counts, plus whatever else its harness recorded — a context
    /// window, a provider quota window, wall-clock durations, a per-model
    /// split. A scope or listing filter instead answers the whole selection
    /// as tapes-usage-summary/1, grouped by --by and summing each counter
    /// over the sessions that recorded it. The accounting basis and coverage
    /// decide whether figures may be summed; cost is only what the harness
    /// recorded, and quota is a separate fact about the account rather than
    /// these sessions.
    Usage {
        /// Session identifier, full or an unambiguous prefix.
        #[arg(conflicts_with_all = ["latest", "exclude", "harness", "here", "project", "global"])]
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
        /// Restrict to one harness: the most recent session of it with
        /// --latest, every selected session of it otherwise.
        #[arg(long, conflicts_with = "session")]
        harness: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Take at most this many sessions from each harness [default: 20].
        #[arg(long, conflicts_with_all = ["session", "latest"])]
        limit: Option<usize>,
        /// Match case-insensitively against the full model identity, `id` or
        /// `id (variant)`. Sessions without a model never match.
        #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest"])]
        model: Option<String>,
        /// Match case-insensitively against the recorded directory path.
        /// Sessions without a directory never match.
        #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest"])]
        directory: Option<String>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is at or after this timestamp. RFC 3339 timestamps with an offset
        /// and bare YYYY-MM-DD dates are accepted.
        #[arg(
            long,
            value_name = "TIMESTAMP",
            value_parser = tapes_core::parse_activity_timestamp,
            conflicts_with_all = ["session", "latest"]
        )]
        since: Option<tapes_core::ActivityTimestamp>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is before this timestamp. RFC 3339 timestamps with an offset and
        /// bare YYYY-MM-DD dates are accepted.
        #[arg(
            long,
            value_name = "TIMESTAMP",
            value_parser = tapes_core::parse_activity_timestamp,
            conflicts_with_all = ["session", "latest"]
        )]
        until: Option<tapes_core::ActivityTimestamp>,
        /// Order the selection by `last_activity_at` before --limit takes
        /// from it: newest first by default, or oldest first.
        #[arg(long, value_enum, value_name = "ORDER", conflicts_with_all = ["session", "latest"])]
        sort: Option<SortArg>,
        /// Match case-insensitively against the last 32 normalized turns in
        /// each candidate session. The fixed tail keeps the selection bounded;
        /// a match outside it is not considered.
        #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest"])]
        search: Option<String>,
        /// Group the summed sessions by this dimension. Repeatable and
        /// comma-separated; groups are keyed in the order given
        /// [default: harness,model]. `model` is the model id and `variant`
        /// the reasoning effort or tier qualifying it, so a session that
        /// recorded no variant is grouped under that absence.
        #[arg(
            long,
            value_enum,
            value_name = "DIMENSION",
            value_delimiter = ',',
            conflicts_with_all = ["session", "latest"]
        )]
        by: Vec<ByArg>,
        /// Render the versioned tapes-usage/1 object, or tapes-usage-summary/1
        /// for a selection, as JSON.
        #[arg(long)]
        json: bool,
    },
    /// What each session of a selection ends on, one bounded record each, as
    /// tapes-endings/1. The selection uses the flags `list` and `export` take,
    /// applied before any transcript is opened; each selected session then
    /// costs one bounded read of its newest turns and one lineage read. Every
    /// fact rests on the normalized turn kinds and typed tool events of the
    /// turns that were read, never on their text: an unanswered request, a
    /// harness command or notice recorded last, a call the read never saw a
    /// result for, results no turn narrates, a closing assistant turn. What
    /// the read left unestablished is named beside them, and `source` is the
    /// coordinate to write down when filing a follow-up. The report infers no
    /// reason for an ending and labels no session complete.
    Endings {
        /// Restrict results to one harness.
        #[arg(long)]
        harness: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Take at most this many sessions from each harness [default: 20].
        #[arg(long)]
        limit: Option<usize>,
        /// Match case-insensitively against the full model identity, `id` or
        /// `id (variant)`. Sessions without a model never match.
        #[arg(long, value_name = "SUBSTRING")]
        model: Option<String>,
        /// Match case-insensitively against the recorded directory path.
        /// Sessions without a directory never match.
        #[arg(long, value_name = "SUBSTRING")]
        directory: Option<String>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is at or after this timestamp. RFC 3339 timestamps with an offset
        /// and bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = tapes_core::parse_activity_timestamp)]
        since: Option<tapes_core::ActivityTimestamp>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is before this timestamp. RFC 3339 timestamps with an offset and
        /// bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = tapes_core::parse_activity_timestamp)]
        until: Option<tapes_core::ActivityTimestamp>,
        /// Order the selection by `last_activity_at` before --limit takes from
        /// it: newest first by default, or oldest first.
        #[arg(long, value_enum, value_name = "ORDER", default_value_t = SortArg::Newest)]
        sort: SortArg,
        /// Match case-insensitively against the last 32 normalized turns in
        /// each candidate session. The fixed tail keeps the selection bounded;
        /// a match outside it is not considered.
        #[arg(long, value_name = "SUBSTRING")]
        search: Option<String>,
        /// Read this many of each session's newest turns. The window bounds
        /// the structural read and the optional text tail alike; a window that
        /// omitted turns is reported as `tail-window`.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_ENDINGS_TAIL)]
        tail: usize,
        /// Include the bounded text of each read operator and assistant turn,
        /// cut at 400 characters. The harness's own commands, notices, and
        /// attached context stay out of it.
        #[arg(long)]
        text: bool,
        /// Render the versioned tapes-endings/1 object as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Export sessions as bundles. One session by id or `--latest`, or every
    /// session a selection holds: the listing flags choose the same set
    /// `list` would return, in the same order, and a `manifest.json` beside
    /// the bundles records that selection and every bundle it produced. Each
    /// session keeps its own bounded bundle; a session whose store cannot be
    /// read is recorded in the manifest's `failed` and does not stop the run,
    /// which fails only when every selected session did.
    Export {
        /// Session identifier, full or an unambiguous prefix.
        #[arg(
            required_unless_present_any = ["latest", "here", "project", "global", "harness", "model", "directory", "since", "until", "search"],
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
        /// Restrict to one harness: the most recent session of it with
        /// --latest, every selected session of it otherwise.
        #[arg(long, conflicts_with = "session")]
        harness: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Take at most this many sessions from each harness [default: 20].
        #[arg(long, conflicts_with_all = ["session", "latest"])]
        limit: Option<usize>,
        /// Match case-insensitively against the full model identity, `id` or
        /// `id (variant)`. Sessions without a model never match.
        #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest"])]
        model: Option<String>,
        /// Match case-insensitively against the recorded directory path.
        /// Sessions without a directory never match.
        #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest"])]
        directory: Option<String>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is at or after this timestamp. RFC 3339 timestamps with an offset
        /// and bare YYYY-MM-DD dates are accepted.
        #[arg(
            long,
            value_name = "TIMESTAMP",
            value_parser = tapes_core::parse_activity_timestamp,
            conflicts_with_all = ["session", "latest"]
        )]
        since: Option<tapes_core::ActivityTimestamp>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is before this timestamp. RFC 3339 timestamps with an offset and
        /// bare YYYY-MM-DD dates are accepted.
        #[arg(
            long,
            value_name = "TIMESTAMP",
            value_parser = tapes_core::parse_activity_timestamp,
            conflicts_with_all = ["session", "latest"]
        )]
        until: Option<tapes_core::ActivityTimestamp>,
        /// Order the selection by `last_activity_at`: newest first by
        /// default, or oldest first. Bundles are written, and the manifest
        /// lists them, in this order.
        #[arg(long, value_enum, value_name = "ORDER", conflicts_with_all = ["session", "latest"])]
        sort: Option<SortArg>,
        /// Match case-insensitively against the last 32 normalized turns in
        /// each candidate session. The fixed tail keeps the selection bounded;
        /// a match outside it is not considered.
        #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest"])]
        search: Option<String>,
        /// Directory for the exported bundles and their manifest.
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
            since,
            until,
            sort,
            search,
            json,
        } => {
            if since
                .zip(until)
                .is_some_and(|(since, until)| since >= until)
            {
                return Err(anyhow!("--since must be earlier than --until"));
            }
            if search.is_some() {
                eprintln!(
                    "Searching the last 32 normalized turns of each candidate session before applying --limit."
                );
            }
            let mut result = tapes_core::list_with_options(
                harness.as_deref(),
                scope.within(),
                limit,
                tapes_core::ListFilters {
                    model: model.as_deref(),
                    directory: directory.as_deref(),
                    since,
                    until,
                    search: search.as_deref(),
                },
                sort.into(),
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
        Command::Events {
            selection,
            tail,
            name,
            call_id,
            json,
        } => {
            let by_latest = selection.latest;
            let mut events = tapes_core::events(selection.selection(), tail)?;
            liveness::annotate(std::slice::from_mut(&mut events.session));
            events.retain(&name, &call_id);
            if json {
                println!("{}", serde_json::to_string(&events)?);
            } else {
                print_events(&events, by_latest);
            }
        }
        Command::Lineage { selection, json } => {
            let lineage = tapes_core::lineage(selection.selection())?;
            if json {
                println!("{}", serde_json::to_string(&lineage)?);
            } else {
                print!("{}", render_lineage(&lineage));
            }
        }
        Command::Stats { selection, json } => {
            let stats = tapes_core::stats(selection.selection())?;
            if json {
                println!("{}", serde_json::to_string(&stats)?);
            } else {
                print!("{}", render_stats(&stats));
            }
        }
        Command::Usage {
            session,
            latest,
            exclude,
            harness,
            scope,
            limit,
            model,
            directory,
            since,
            until,
            sort,
            search,
            by,
            json,
        } => {
            let one = if let Some(session) = &session {
                Some(Selection::Id(session))
            } else if latest {
                Some(Selection::Latest {
                    within: scope.within_or_here(),
                    harness: harness.as_deref(),
                    exclude: &exclude,
                })
            } else {
                None
            };
            if let Some(one) = one {
                let usage = tapes_core::usage(one)?;
                if json {
                    println!("{}", serde_json::to_string(&usage)?);
                } else {
                    print!("{}", render_usage(&usage));
                }
            } else {
                let selected = scope.here
                    || scope.global
                    || scope.project.is_some()
                    || harness.is_some()
                    || model.is_some()
                    || directory.is_some()
                    || since.is_some()
                    || until.is_some()
                    || search.is_some()
                    || limit.is_some()
                    || sort.is_some()
                    || !by.is_empty();
                if !selected {
                    return Err(anyhow!(
                        "usage answers one session or a selection of them: name a session id, \
                         pass --latest, or select a set with --here, --project <path>, --global, \
                         or a listing filter"
                    ));
                }
                if since
                    .zip(until)
                    .is_some_and(|(since, until)| since >= until)
                {
                    return Err(anyhow!("--since must be earlier than --until"));
                }
                if search.is_some() {
                    eprintln!(
                        "Searching the last 32 normalized turns of each candidate session before applying --limit."
                    );
                }
                let by = grouping(&by);
                let summary = tapes_core::usage_summary(
                    &tapes_core::SessionSelection {
                        within: scope.within(),
                        harness: harness.as_deref(),
                        limit,
                        filters: tapes_core::ListFilters {
                            model: model.as_deref(),
                            directory: directory.as_deref(),
                            since,
                            until,
                            search: search.as_deref(),
                        },
                        sort: sort.unwrap_or(SortArg::Newest).into(),
                    },
                    &by,
                )?;
                if json {
                    println!("{}", serde_json::to_string(&summary)?);
                } else {
                    print!("{}", render_usage_summary(&summary, &by));
                    print_diagnostics(
                        summary.scan_truncated,
                        summary.scanned,
                        &summary.unreadable,
                        &summary.unsearched,
                        &summary.unavailable,
                    );
                }
            }
        }
        Command::Endings {
            harness,
            scope,
            limit,
            model,
            directory,
            since,
            until,
            sort,
            search,
            tail,
            text,
            json,
        } => {
            if since
                .zip(until)
                .is_some_and(|(since, until)| since >= until)
            {
                return Err(anyhow!("--since must be earlier than --until"));
            }
            if search.is_some() {
                eprintln!(
                    "Searching the last 32 normalized turns of each candidate session before applying --limit."
                );
            }
            let mut report = tapes_core::endings::endings(
                &tapes_core::SessionSelection {
                    within: scope.within(),
                    harness: harness.as_deref(),
                    limit,
                    filters: tapes_core::ListFilters {
                        model: model.as_deref(),
                        directory: directory.as_deref(),
                        since,
                        until,
                        search: search.as_deref(),
                    },
                    sort: sort.into(),
                },
                tail,
                text,
            )?;
            liveness::annotate_endings(&mut report.endings);
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print!("{}", render_endings(&report));
                for session in &report.unread {
                    println!(
                        "Unread: {} session {}: {}",
                        session.harness, session.id, session.error
                    );
                }
                print_diagnostics(
                    report.scan_truncated,
                    report.scanned,
                    &report.unreadable,
                    &report.unsearched,
                    &report.unavailable,
                );
            }
        }
        Command::Export {
            session,
            latest,
            exclude,
            harness,
            scope,
            limit,
            model,
            directory,
            since,
            until,
            sort,
            search,
            bundle,
        } => {
            if let Some(session) = &session {
                print_manifest(&tapes_core::export(
                    Selection::Id(session),
                    bundle.as_deref(),
                )?);
            } else if latest {
                print_manifest(&tapes_core::export(
                    Selection::Latest {
                        within: scope.within_or_here(),
                        harness: harness.as_deref(),
                        exclude: &exclude,
                    },
                    bundle.as_deref(),
                )?);
            } else {
                if since
                    .zip(until)
                    .is_some_and(|(since, until)| since >= until)
                {
                    return Err(anyhow!("--since must be earlier than --until"));
                }
                if search.is_some() {
                    eprintln!(
                        "Searching the last 32 normalized turns of each candidate session before applying --limit."
                    );
                }
                let export = tapes_core::export_selection(
                    &tapes_core::SessionSelection {
                        within: scope.within(),
                        harness: harness.as_deref(),
                        limit,
                        filters: tapes_core::ListFilters {
                            model: model.as_deref(),
                            directory: directory.as_deref(),
                            since,
                            until,
                            search: search.as_deref(),
                        },
                        sort: sort.unwrap_or(SortArg::Newest).into(),
                    },
                    bundle.as_deref(),
                )?;
                print_selection_manifest(&export);
                if export.every_session_failed() {
                    return Err(anyhow!(
                        "no selected session could be exported; {} names each failure",
                        export.manifest_file.path.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// One line per session: what it is, what it ends on, and what the read could
/// not establish. A fact the read did not establish has no name on the line,
/// and an empty fact list is the answer that the read established none.
fn render_endings(report: &EndingsReport) -> String {
    report.endings.iter().map(render_ending).collect()
}

fn render_ending(ending: &Ending) -> String {
    let mut out = format!(
        "{} {} {} {}",
        ending.session.id,
        ending.session.harness,
        human_timestamp(ending.session.last_activity_at),
        ending
            .last_turn
            .as_ref()
            .map_or_else(|| "-".to_owned(), |turn| speaker(&turn.role, turn.kind))
    );
    if !ending.facts.is_empty() {
        out.push(' ');
        out.push_str(
            &ending
                .facts
                .iter()
                .map(|fact| fact.label())
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    if !ending.incomplete.is_empty() {
        out.push_str(&format!(
            " [incomplete: {}]",
            ending
                .incomplete
                .iter()
                .map(|limit| limit.label())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(children) = ending
        .lineage
        .as_ref()
        .map(|lineage| lineage.children)
        .filter(|children| *children > 0)
    {
        out.push_str(&format!(" [children: {children}]"));
    }
    out.push('\n');
    for entry in ending.tail.iter().flatten() {
        let heading = [
            Some(speaker(&entry.role, entry.kind)),
            Some(format!("#{}", entry.ordinal)),
            entry.ts.map(human_timestamp),
            entry.truncated.then(|| "cut at 400 characters".to_owned()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        out.push_str(&format!("  [{heading}]\n"));
        for line in entry.text.lines() {
            out.push_str(&format!("    {line}\n"));
        }
    }
    out
}

/// One line for the session, one for a recorded parent or fork, and one per
/// child. A relationship the store does not record has no line, and an empty
/// child list is the answer that the store records none.
fn render_lineage(lineage: &LineageView) -> String {
    let mut out = format!(
        "session: {} {}",
        lineage.session.harness, lineage.session.id
    );
    if let Some(model) = &lineage.session.model {
        out.push_str(&format!(" {}", model.identity()));
    }
    out.push('\n');
    if let Some(parent) = &lineage.lineage.parent {
        out.push_str(&format!(
            "parent: {} ({}){}\n",
            parent.native_id,
            parent.source,
            if parent.resolved { "" } else { " unresolved" }
        ));
    }
    if let Some(forked_from) = &lineage.lineage.forked_from {
        out.push_str(&format!("forked from: {forked_from}\n"));
    }
    for child in &lineage.lineage.children {
        out.push_str(&render_child(child));
    }
    render_truncation_notes(&mut out, &lineage.truncation);
    render_notes(&mut out, &lineage.notes);
    out
}

fn render_child(child: &ChildRef) -> String {
    let mut rendered = format!("child: {}", child.role.as_deref().unwrap_or("-"));
    rendered.push_str(&format!(" {}", child.reference));
    if let Some(session_id) = &child.session_id {
        if session_id != &child.reference {
            rendered.push_str(&format!(" ({session_id})"));
        }
    }
    if let Some(model) = &child.model {
        rendered.push_str(&format!(" {model}"));
    }
    for (label, stamp) in [
        ("spawned", child.spawned_at),
        ("completed", child.completed_at),
    ] {
        rendered.push_str(&format!(
            " {label} {}",
            stamp.map_or_else(|| "-".to_owned(), human_timestamp)
        ));
    }
    rendered.push_str(&format!(
        " status {}",
        child.disposition.as_deref().unwrap_or("-")
    ));
    if !child.resolved {
        rendered.push_str(" unresolved");
    }
    rendered.push('\n');
    rendered
}

/// One line per group, in the order the JSON carries them, and a group the
/// read holds nothing for has no line at all. The tool table is the only
/// multi-line group.
fn render_stats(stats: &StatsView) -> String {
    let mut out = format!("session: {} {}", stats.session.harness, stats.session.id);
    if let Some(model) = &stats.session.model {
        out.push_str(&format!(" {}", model.identity()));
    }
    out.push('\n');
    out.push_str(&format!("coverage: {}\n", render_coverage(&stats.coverage)));
    out.push_str(&format!("turns: {}\n", render_turn_kinds(&stats.turns)));
    out.push_str(&format!("tools: {}\n", render_tools(&stats.tools)));
    if stats.tools.incomplete.total() > 0 {
        out.push_str(&format!(
            "incomplete: {}\n",
            render_incomplete(&stats.tools)
        ));
    }
    if !stats.tools.by_name.is_empty() {
        out.push_str("NAME\tCALLS\tPAIRED\tERRORS\tTOTAL MS\tMAX MS\tTIMED\n");
        for tool in &stats.tools.by_name {
            out.push_str(&render_tool_name(tool));
        }
    }
    if let Some(durations) = &stats.durations_ms {
        out.push_str(&format!("durations: {}\n", render_time(durations)));
    }
    if let Some(usage) = &stats.usage {
        render_usage_stats(&mut out, usage);
    }
    if let Some(lineage) = &stats.lineage {
        out.push_str(&format!("children: {}\n", render_children(lineage)));
    }
    if !stats.warnings.is_empty() {
        out.push_str(&format!(
            "warnings: {}\n",
            stats
                .warnings
                .iter()
                .map(|warning| warning_label(*warning))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    render_truncation_notes(&mut out, &stats.coverage.truncation);
    out
}

fn render_coverage(coverage: &Coverage) -> String {
    let turns = match coverage.turns {
        TurnCoverage::Session => "turns cover the whole session",
        TurnCoverage::ReadWindow => "turns cover the bounded read window",
    };
    format!("{turns}, durations cover complete pairs only")
}

/// The total, then the kinds the read reached. A kind with no turn is a kind
/// the session did not record.
fn render_turn_kinds(turns: &TurnKindCounts) -> String {
    let counted = [
        ("operator", turns.operator),
        ("assistant", turns.assistant),
        ("tool", turns.tool),
        ("reasoning", turns.reasoning),
        ("control", turns.control),
        ("ambient", turns.ambient),
        ("notice", turns.notice),
        ("unknown", turns.unknown),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(name, count)| format!("{count} {name}"))
    .collect::<Vec<_>>();
    std::iter::once(format!("{} total", turns.total))
        .chain(counted)
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_tools(tools: &ToolStats) -> String {
    format!(
        "{} calls, {} results, {} paired, {} errors",
        tools.calls, tools.results, tools.paired, tools.errors
    )
}

fn render_incomplete(tools: &ToolStats) -> String {
    [
        ("no-result-in-read", tools.incomplete.no_result_in_read),
        (
            "call-before-read-bound",
            tools.incomplete.call_before_read_bound,
        ),
        ("call-not-recorded", tools.incomplete.call_not_recorded),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .map(|(name, count)| format!("{count} {name}"))
    .collect::<Vec<_>>()
    .join(", ")
}

fn render_tool_name(tool: &ToolNameStats) -> String {
    let (total, max, count) = tool.duration_ms.map_or_else(
        || (String::new(), String::new(), String::new()),
        |durations| {
            (
                durations.total.to_string(),
                durations.max.to_string(),
                durations.count.to_string(),
            )
        },
    );
    format!(
        "{}\t{}\t{}\t{}\t{total}\t{max}\t{count}\n",
        tool.name, tool.calls, tool.paired, tool.errors
    )
}

fn render_time(durations: &TimeStats) -> String {
    [
        ("recorded span", durations.recorded_span),
        ("in tool", durations.in_tool),
        ("longest gap between turns", durations.between_turns_max),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name} {value}ms")))
    .chain(std::iter::once(format!(
        "{} turns with timestamps",
        durations.count_with_timestamps
    )))
    .collect::<Vec<_>>()
    .join(", ")
}

fn render_usage_stats(out: &mut String, usage: &UsageStats) {
    if let Some(tokens) = &usage.tokens {
        out.push_str(&format!("tokens: {}\n", render_tokens(tokens)));
    }
    if let Some(cost) = &usage.cost {
        out.push_str(&format!("cost: {}\n", render_cost(cost)));
    }
    if let Some(accounting) = &usage.accounting {
        out.push_str(&format!("accounting: {}\n", render_accounting(accounting)));
    }
    let cache = [
        ("read", usage.cache_read_ratio),
        ("write", usage.cache_write_ratio),
    ]
    .into_iter()
    .filter_map(|(name, ratio)| ratio.map(|ratio| format!("{name} {ratio:.4}")))
    .collect::<Vec<_>>();
    if !cache.is_empty() {
        out.push_str(&format!(
            "cache of input plus cache read plus cache write: {}\n",
            cache.join(", ")
        ));
    }
}

fn render_children(lineage: &LineageStats) -> String {
    let dispositions = lineage
        .by_disposition
        .iter()
        .map(|(disposition, count)| format!("{count} {disposition}"))
        .collect::<Vec<_>>();
    std::iter::once(format!("{} recorded", lineage.children))
        .chain(std::iter::once(format!("{} resolved", lineage.resolved)))
        .chain(dispositions)
        .collect::<Vec<_>>()
        .join(", ")
}

fn warning_label(warning: Warning) -> &'static str {
    match warning {
        Warning::ReadWindow => "read-window",
        Warning::TailWindow => "tail-window",
        Warning::KindUnknown => "kind-unknown",
        Warning::IncompletePairs => "incomplete-pairs",
        Warning::NoTimestamps => "no-timestamps",
    }
}

/// One line per fact, and a fact the harness did not record has no line.
/// The closing notes are `show`'s, because the read behind them is the same.
fn render_usage(usage: &UsageView) -> String {
    let mut out = format!("session: {} {}", usage.session.harness, usage.session.id);
    if let Some(model) = &usage.session.model {
        out.push_str(&format!(" {}", model.identity()));
    }
    out.push('\n');
    if let Some(tokens) = &usage.tokens {
        out.push_str(&format!("tokens: {}\n", render_tokens(tokens)));
    }
    if let Some(cost) = &usage.cost {
        out.push_str(&format!("cost: {}\n", render_cost(cost)));
    }
    if let Some(accounting) = &usage.accounting {
        out.push_str(&format!("accounting: {}\n", render_accounting(accounting)));
    }
    let turns = &usage.turns;
    out.push_str(&format!(
        "turns: {} total, {} user, {} assistant, {} tool, {} reasoning, covering {}\n",
        turns.total,
        turns.user,
        turns.assistant,
        turns.tool,
        turns.reasoning,
        match turns.coverage {
            TurnCoverage::Session => "the whole session",
            TurnCoverage::ReadWindow => "the bounded read window",
        }
    ));
    if let Some(context_window) = usage.context_window {
        out.push_str(&format!("context window: {context_window} tokens\n"));
    }
    if let Some(rate_limits) = &usage.rate_limits {
        render_rate_limits(&mut out, rate_limits);
    }
    if let Some(durations) = &usage.durations_ms {
        out.push_str(&format!("durations: {}\n", render_durations(durations)));
    }
    for model in usage.by_model.iter().flatten() {
        out.push_str(&render_model_usage(model));
    }
    render_truncation_notes(&mut out, &usage.truncation);
    render_notes(&mut out, &usage.notes);
    out
}

fn render_tokens(tokens: &Tokens) -> String {
    [
        ("input", tokens.input),
        ("output", tokens.output),
        ("reasoning", tokens.reasoning),
        ("cache read", tokens.cache_read),
        ("cache write", tokens.cache_write),
    ]
    .into_iter()
    .filter_map(|(name, count)| count.map(|count| format!("{name} {count}")))
    .collect::<Vec<_>>()
    .join(", ")
}

/// Four decimal places, so a fraction of a cent is neither rounded away nor
/// dressed up as more precision than the harness recorded.
fn render_cost(cost: &Cost) -> String {
    format!("{:.4} USD", cost.usd)
}

fn render_accounting(accounting: &Accounting) -> String {
    let basis = match accounting.basis {
        AccountingBasis::RecordedTotal => "a recorded total",
        AccountingBasis::SummedRequests => "a sum of per-request records",
    };
    let coverage = match accounting.coverage {
        AccountingCoverage::Session => "the whole session",
        AccountingCoverage::ReadWindow => "the bounded read window",
    };
    format!("{basis}, covering {coverage}")
}

fn render_rate_limits(out: &mut String, limits: &RateLimits) {
    for (name, window) in [
        ("primary", &limits.primary),
        ("secondary", &limits.secondary),
    ] {
        if let Some(window) = window {
            out.push_str(&format!(
                "rate limit {name}: {}\n",
                render_rate_window(window)
            ));
        }
    }
    if let Some(plan) = &limits.plan {
        out.push_str(&format!("plan: {plan}\n"));
    }
}

fn render_rate_window(window: &RateWindow) -> String {
    let mut rendered = format!("{}% used", window.used_percent);
    if let Some(minutes) = window.window_minutes {
        rendered.push_str(&format!(" of a {minutes}-minute window"));
    }
    if let Some(resets_at) = window.resets_at {
        rendered.push_str(&format!(", resets {}", human_timestamp(resets_at)));
    }
    rendered
}

fn render_durations(durations: &Durations) -> String {
    [
        ("api", durations.api),
        ("api without retries", durations.api_without_retries),
        ("tool", durations.tool),
        ("total", durations.total),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name} {value}ms")))
    .collect::<Vec<_>>()
    .join(", ")
}

fn render_model_usage(model: &ModelUsage) -> String {
    let mut rendered = format!("model {}:", model.model);
    if let Some(tokens) = &model.tokens {
        rendered.push_str(&format!(" {}", render_tokens(tokens)));
    }
    if let Some(cost) = &model.cost {
        rendered.push_str(&format!("; {}", render_cost(cost)));
    }
    rendered.push('\n');
    rendered
}

/// One row per group and a closing total, each counter cell naming how many
/// of the row's sessions recorded it whenever that is fewer than all of
/// them. A counter no session recorded has no figure to print.
fn render_usage_summary(summary: &UsageSummary, by: &[GroupBy]) -> String {
    let key_columns: Vec<&str> = if by.is_empty() {
        vec!["GROUP"]
    } else {
        by.iter().map(key_column).collect()
    };
    let mut out = key_columns.join("\t");
    out.push_str(
        "\tSESSIONS\tINPUT\tOUTPUT\tREASONING\tCACHE READ\tCACHE WRITE\tCOST\t\
         COVERAGE (RECORDED/SUMMED/WINDOW/NONE)\n",
    );
    for group in &summary.groups {
        let mut cells: Vec<String> = by
            .iter()
            .map(|dimension| key_cell(&group.key, *dimension).unwrap_or_default())
            .collect();
        cells.resize(key_columns.len(), String::new());
        cells.extend(tally_cells(&group.tally));
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    let mut total = vec!["TOTAL".to_owned()];
    total.resize(key_columns.len(), String::new());
    total.extend(tally_cells(&summary.totals));
    out.push_str(&total.join("\t"));
    out.push('\n');
    out
}

fn key_column(dimension: &GroupBy) -> &'static str {
    match dimension {
        GroupBy::Harness => "HARNESS",
        GroupBy::Model => "MODEL",
        GroupBy::Variant => "VARIANT",
        GroupBy::Directory => "DIRECTORY",
    }
}

fn key_cell(key: &GroupKey, dimension: GroupBy) -> Option<String> {
    match dimension {
        GroupBy::Harness => key.harness.clone(),
        GroupBy::Model => key.model.clone(),
        GroupBy::Variant => key.variant.clone(),
        GroupBy::Directory => key.directory.clone(),
    }
}

fn tally_cells(tally: &UsageTally) -> Vec<String> {
    let tokens = tally.tokens.as_ref();
    let sessions = tally.sessions;
    let counted = &tally.counted;
    let coverage = &tally.coverage;
    vec![
        sessions.to_string(),
        counter_cell(
            tokens.and_then(|tokens| tokens.input),
            counted.input,
            sessions,
        ),
        counter_cell(
            tokens.and_then(|tokens| tokens.output),
            counted.output,
            sessions,
        ),
        counter_cell(
            tokens.and_then(|tokens| tokens.reasoning),
            counted.reasoning,
            sessions,
        ),
        counter_cell(
            tokens.and_then(|tokens| tokens.cache_read),
            counted.cache_read,
            sessions,
        ),
        counter_cell(
            tokens.and_then(|tokens| tokens.cache_write),
            counted.cache_write,
            sessions,
        ),
        sum_cell(tally.cost.as_ref().map(render_cost), counted.cost, sessions),
        format!(
            "{}/{}/{}/{}",
            coverage.recorded_total,
            coverage.summed_session,
            coverage.summed_read_window,
            coverage.no_accounting
        ),
    ]
}

fn counter_cell(sum: Option<u64>, counted: usize, sessions: usize) -> String {
    sum_cell(sum.map(|sum| sum.to_string()), counted, sessions)
}

/// A sum covers the sessions that recorded the counter. When that is fewer
/// than the row holds, the cell says how many, so the figure is not read as
/// every session's.
fn sum_cell(sum: Option<String>, counted: usize, sessions: usize) -> String {
    match sum {
        None => String::new(),
        Some(sum) if counted < sessions => format!("{sum} ({counted} of {sessions})"),
        Some(sum) => sum,
    }
}

fn print_events(events: &EventTranscript, by_latest: bool) {
    let mut out = String::new();
    if events.session.live.is_some() || by_latest {
        out.push_str(&format!(
            "# {} {}{}\n",
            events.session.harness,
            events.session.id,
            live_marker(&events.session)
        ));
    }
    for event in &events.events {
        out.push_str(&render_event(event));
        out.push('\n');
    }
    if by_latest {
        render_latest_note(&mut out, &events.session);
    }
    render_truncation_notes(&mut out, &events.truncation);
    render_notes(&mut out, &events.notes);
    print!("{out}");
}

fn render_event(record: &EventRecord) -> String {
    let kind = match record.event.kind {
        EventKind::ToolCall => "tool-call",
        EventKind::ToolResult => "tool-result",
    };
    let heading = record.ts.map_or_else(
        || format!("[{kind} #{}]", record.ordinal),
        |ts| format!("[{kind} #{} {}]", record.ordinal, human_timestamp(ts)),
    );
    let outcome = match (&record.duration_ms, &record.incomplete, &record.pair) {
        (Some(duration), _, _) => format!("{duration}ms"),
        (_, Some(incomplete), _) => incomplete_label(incomplete).to_owned(),
        (_, _, Some(_)) => "paired".to_owned(),
        _ => "-".to_owned(),
    };
    format!(
        "{heading} {} {} {} {outcome}",
        record.event.name.as_deref().unwrap_or("-"),
        record.event.call_id.as_deref().unwrap_or("-"),
        record.event.status.as_deref().unwrap_or("-")
    )
}

fn incomplete_label(incomplete: &Incomplete) -> &'static str {
    match incomplete {
        Incomplete::NoResultInRead => "no-result-in-read",
        Incomplete::CallBeforeReadBound => "call-before-read-bound",
        Incomplete::CallNotRecorded => "call-not-recorded",
    }
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
    print_diagnostics(
        result.scan_truncated,
        result.scanned,
        &result.unreadable,
        &result.unsearched,
        &result.unavailable,
    );
}

/// What a listing could not reach, stated the same way wherever a listing
/// backs the answer: a set that stopped short is a view, not the store.
fn print_diagnostics(
    scan_truncated: bool,
    scanned: usize,
    unreadable: &[String],
    unsearched: &[String],
    unavailable: &[String],
) {
    if scan_truncated {
        println!(
            "The search stopped early after {scanned} candidates; older sessions were not inspected. \
             Raise --limit to widen the scan, or `tapes export` a session once it is found."
        );
    }
    for session in unreadable {
        println!("Unreadable: {session}");
    }
    for session in unsearched {
        println!("Unsearched: {session}");
    }
    if unavailable.is_empty() {
        return;
    }
    if unavailable.len() == 4 {
        println!("No harnesses available.");
    }
    println!("Unavailable: {}", unavailable.join(", "));
}

/// The manifest is the whole stdout contract for `export`: three paths, three
/// sizes, in the order a rescuer should read them.
fn print_manifest(bundle: &Bundle) {
    for file in bundle.files() {
        print_bundle_file(file);
    }
}

/// A selection prints each bundle's manifest in selection order, then the one
/// file that spans them.
fn print_selection_manifest(export: &BulkExport) {
    for bundle in &export.bundles {
        print_manifest(bundle);
    }
    print_bundle_file(&export.manifest_file);
}

fn print_bundle_file(file: &BundleFile) {
    println!("{}\t{}", file.path.display(), human_bytes(file.bytes));
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
        let role = human_speaker(turn);
        if let Some(ts) = turn.ts {
            out.push_str(&format!(
                "[{role} #{} {}]\n{}\n",
                turn.ordinal,
                human_timestamp(ts),
                turn.text
            ));
        } else {
            out.push_str(&format!("[{role} #{}]\n{}\n", turn.ordinal, turn.text));
        }
    }
    if transcript.session.start_uncertain {
        out.push_str(&format!(
            "Note: The recorded start could not be read; {} is the earliest record reached, and the \
             session began at or before it.\n",
            human_timestamp(transcript.session.started_at)
        ));
    }
    render_activity_note(&mut out, transcript);
    if by_latest {
        render_latest_note(&mut out, &transcript.session);
    }
    render_truncation_notes(&mut out, &transcript.truncation);
    render_notes(&mut out, &transcript.notes);
    out
}

fn render_latest_note(out: &mut String, session: &Session) {
    out.push_str(&format!(
        "Note: --latest picked the newest session in scope. Pass --exclude {} to reach the one before it.\n",
        session.id
    ));
}

fn render_notes(out: &mut String, notes: &[String]) {
    for note in notes {
        out.push_str(&format!("Note: {note}\n"));
    }
}

/// Each cause of truncation gets its own note, and each note recommends only
/// a recovery that reaches the omitted content: a window is reopened wider, a
/// source bound is the reader's own limit and nothing through `tapes` passes
/// it.
fn render_truncation_notes(out: &mut String, truncation: &Truncation) {
    if let Some(window) = &truncation.window {
        let total = window.returned + window.omitted;
        if window.omitted_exact {
            out.push_str(&format!(
                "Note: Showing the last {} of {total} turns; {} earlier turns fall outside the {}-turn window. \
                 Use --tail {total} to see them, or `tapes export` for every turn the reader can reach.\n",
                window.returned, window.omitted, window.bound
            ));
        } else {
            out.push_str(&format!(
                "Note: Showing the last {} of at least {total} turns; the read stopped once the {}-turn window \
                 was full, so older turns were not fetched and ordinals count from the oldest fetched. \
                 A larger --tail or `tapes export` fetches further back.\n",
                window.returned, window.bound
            ));
        }
    }
    for bound in &truncation.source {
        match bound {
            SourceBound::FileTail { bytes } => out.push_str(&format!(
                "Note: Only the final {} of the recording was read; earlier records are beyond \
                 what --tail or `tapes export` can reach.\n",
                human_bytes(*bytes)
            )),
            SourceBound::RecordPage { records, of } => out.push_str(&format!(
                "Note: Only the newest {records} {of} were fetched from the store; anything \
                 older was not read, and no window reaches it.\n"
            )),
            SourceBound::TurnText { turns, chars } => {
                let (noun, verb) = if *turns == 1 {
                    ("turn", "carries")
                } else {
                    ("turns", "carry")
                };
                out.push_str(&format!(
                    "Note: {turns} {noun} {verb} text cut at {chars} characters by the store read.\n"
                ));
            }
        }
    }
}

fn render_activity_note(out: &mut String, transcript: &Transcript) {
    let newest_turn = transcript.turns.iter().rev().find_map(|turn| turn.ts);
    let activity_after_newest_turn = newest_turn.as_ref().is_some_and(|newest_turn| {
        transcript.session.last_activity_at.timestamp() > newest_turn.timestamp()
    });
    let Some(trailing_record) = transcript.trailing_record.as_ref() else {
        if activity_after_newest_turn {
            if let Some(newest_turn) = newest_turn {
                out.push_str(&format!(
                    "Note: The store records activity at {}, after the newest turn rendered here ({}). \
                     What came later is a record `show` does not render as a turn.\n",
                    human_timestamp(transcript.session.last_activity_at),
                    human_timestamp(newest_turn)
                ));
            }
        }
        return;
    };

    let label = match trailing_record.timestamp {
        Some(timestamp) => format!(
            "`{}` at {}",
            trailing_record.kind,
            human_timestamp(timestamp)
        ),
        None => format!("`{}` (timestamp unavailable)", trailing_record.kind),
    };
    if let Some(newest_turn) = newest_turn {
        if activity_after_newest_turn {
            out.push_str(&format!(
                "Note: The store records activity at {}, after the newest turn rendered here ({}). \
                 The newest trailing record is {label}; `show` does not render it as a turn.\n",
                human_timestamp(transcript.session.last_activity_at),
                human_timestamp(newest_turn)
            ));
            return;
        }
    }
    out.push_str(&format!(
        "Note: The newest trailing record is {label}; `show` does not render it as a turn.\n"
    ));
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
    use tapes_core::model::{End, OrdinalRange, Role, TrailingRecord, Turn, TurnKind, TurnWindow};

    fn transcript(truncated: bool) -> Transcript {
        Transcript {
            session: Session {
                id: "s1".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                title: None,
                derived_title: None,
                derived_title_truncated: None,
                directory: None,
                started_at: Utc::now(),
                last_activity_at: Utc::now(),
                live: None,
                cost: None,
                tokens: None,
                accounting: None,
                store: None,
                start_uncertain: false,
                usage_detail: None,
            },
            turns: vec![Turn {
                role: Role::User,
                kind: TurnKind::Operator,
                text: "fix the parser".to_owned(),
                ts: None,
                ordinal: 0,
                native_id: None,
                tool: None,
            }],
            truncated,
            truncation: Truncation {
                window: truncated.then_some(TurnWindow {
                    returned: 1,
                    omitted: 2,
                    omitted_from: End::Head,
                    bound: 1,
                    ordinals: Some(OrdinalRange { first: 2, last: 2 }),
                    omitted_exact: true,
                }),
                source: Vec::new(),
            },
            trailing_record: None,
            notes: vec!["Skipped 1 unparseable line.".to_owned()],
        }
    }

    #[test]
    fn the_human_render_says_when_it_is_a_window() {
        let windowed = render_transcript(&transcript(true), false);
        assert!(
            windowed.contains(
                "Showing the last 1 of 3 turns; 2 earlier turns fall outside the 1-turn window"
            ),
            "{windowed}"
        );
        assert!(windowed.contains("--tail 3"), "{windowed}");
        assert!(windowed.contains("tapes export"), "{windowed}");
        assert!(windowed.contains("Skipped 1 unparseable line."));

        let whole = render_transcript(&transcript(false), false);
        assert!(!whole.contains("Showing the last"), "{whole}");
        assert!(whole.contains("Skipped 1 unparseable line."));
        assert!(whole.starts_with("[user #0]"), "{whole}");
    }

    #[test]
    fn an_uncertain_start_is_named_as_a_floor() {
        let mut floor = transcript(false);
        floor.session.start_uncertain = true;
        let rendered = render_transcript(&floor, false);
        assert!(
            rendered.contains("The recorded start could not be read"),
            "{rendered}"
        );
        assert!(!render_transcript(&transcript(false), false).contains("could not be read"));
    }

    /// A source bound is the reader's own limit, so its note names what was
    /// read and does not offer a wider window as the remedy.
    #[test]
    fn a_source_bound_is_named_without_offering_a_window_as_the_remedy() {
        let mut bounded = transcript(false);
        bounded.truncated = true;
        bounded.truncation.source = vec![
            SourceBound::FileTail {
                bytes: 4 * 1024 * 1024,
            },
            SourceBound::RecordPage {
                records: 1000,
                of: "messages".to_owned(),
            },
            SourceBound::TurnText {
                turns: 1,
                chars: 4000,
            },
        ];
        let rendered = render_transcript(&bounded, false);
        assert!(
            rendered.contains("Only the final 4 MiB of the recording was read"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Only the newest 1000 messages were fetched from the store"),
            "{rendered}"
        );
        assert!(
            rendered.contains("1 turn carries text cut at 4000 characters"),
            "{rendered}"
        );
        assert!(!rendered.contains("Use --tail"), "{rendered}");
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

    #[test]
    fn subsecond_activity_in_the_same_rendered_second_is_not_named() {
        let newest_turn = "2026-08-23T23:24:52Z".parse::<DateTime<Utc>>().unwrap();
        let mut transcript = transcript(false);
        transcript.turns[0].ts = Some(newest_turn);
        transcript.session.last_activity_at =
            "2026-08-23T23:24:52.400Z".parse::<DateTime<Utc>>().unwrap();

        let same_second = render_transcript(&transcript, false);
        assert!(
            !same_second.contains("The store records activity"),
            "{same_second}"
        );

        transcript.session.last_activity_at =
            "2026-08-23T23:24:53Z".parse::<DateTime<Utc>>().unwrap();
        let next_second = render_transcript(&transcript, false);
        assert!(
            next_second.contains("The store records activity"),
            "{next_second}"
        );
    }

    #[test]
    fn a_verified_trailing_record_names_its_kind_and_timestamp() {
        let newest_turn = "2026-08-23T23:24:52Z".parse::<DateTime<Utc>>().unwrap();
        let trailing_timestamp = "2026-08-23T23:27:56Z".parse::<DateTime<Utc>>().unwrap();
        let mut transcript = transcript(false);
        transcript.turns[0].ts = Some(newest_turn);
        transcript.session.last_activity_at = trailing_timestamp;
        transcript.trailing_record = Some(TrailingRecord {
            kind: "event_msg".to_owned(),
            timestamp: Some(trailing_timestamp),
        });

        let rendered = render_transcript(&transcript, false);
        assert!(
            rendered.contains("The newest trailing record is `event_msg` at 2026-08-23T23:27:56Z"),
            "{rendered}"
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
