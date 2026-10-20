use agent_tapes_core::EventSink;
mod guide;
mod integration;
mod liveness;
mod remote;
mod self_token;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use agent_tapes_core::brief::{Brief, OpenCall, OpenChild, DEFAULT_BRIEF_TAIL, TAIL_TEXT_CHARS};
use agent_tapes_core::bundle::{Bundle, BundleFile, GitContext};
use agent_tapes_core::endings::{Ending, EndingsReport, DEFAULT_ENDINGS_TAIL};
use agent_tapes_core::event::{EventKind, EventRecord, EventTranscript, Incomplete};
use agent_tapes_core::lineage::{ChildRef, LineageView};
use agent_tapes_core::measurement::{MeasurementCompleteness, COUNTERS};
use agent_tapes_core::model::{
    human_bytes, human_speaker, human_timestamp, human_title, speaker, Accounting, AccountingBasis,
    AccountingCoverage, AccountingSelection, CacheWrites, Cost, CostCoverage, DisjointTokens,
    InputConvention, LiveState, OutputConvention, Projection, Session, SourceBound,
    SourceDescriptor, TokenConvention, Tokens, Transcript, Truncation, Turn, TurnKind,
    TurnSelection, SUMMED_CATEGORIES,
};
use agent_tapes_core::overlap::Disjointness;
use agent_tapes_core::pricing::{ListPriceEstimate, PriceLookup};
use agent_tapes_core::reconcile::{CounterStatus, Reconciliation, SourceRole};
use agent_tapes_core::span::{Instant, SpanBounds, SpanPlacement};
use agent_tapes_core::stats::{
    AssistantTurnDurations, Coverage, LineageStats, StatsView, TimeStats, ToolNameStats, ToolStats,
    TurnKindCounts, UsageStats, Warning,
};
use agent_tapes_core::stats_summary::SessionRead;
use agent_tapes_core::unexplained::{ExplainedBy, Placement, Unexplained, UnexplainedModel};
use agent_tapes_core::usage::{
    Consumption, ConsumptionEventKeys, ConsumptionEventMeasurement, ConsumptionKeyOptions,
    DisjointSum, Durations, GroupBy, GroupKey, ModelUsage, ObservationClassification, RateLimits,
    RateWindow, RecordedUsage, RequestDimensions, TurnCoverage, UsageAttribution, UsageObservation,
    UsageObservationOptions, UsageOptions, UsageTally, UsageView,
};
use agent_tapes_core::window::{QuotaAt, QuotaKind, WindowBounds, WindowRecord};
use agent_tapes_core::WindowAsk;
use agent_tapes_core::{BulkExport, Selection, UsageSummary, Where};
use anyhow::{anyhow, Result};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use agent_tapes_core::backend::Backend;
use agent_tapes_core::byte_size::ByteSize;
use agent_tapes_core::input::{
    InputBackend, InputFormat, InputOptions, DEFAULT_DECODED_BYTES, DEFAULT_OUTPUT_BYTES,
    DEFAULT_RECORD_BYTES, DEFAULT_RESIDENT_BYTES, DEFAULT_SCAN_BYTES,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SortArg {
    Newest,
    Oldest,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LaunchArg {
    Interactive,
    Headless,
}

impl From<LaunchArg> for agent_tapes_core::model::Launch {
    fn from(launch: LaunchArg) -> Self {
        match launch {
            LaunchArg::Interactive => Self::Interactive,
            LaunchArg::Headless => Self::Headless,
        }
    }
}

fn launch_label(launch: LaunchArg) -> &'static str {
    match launch {
        LaunchArg::Interactive => "interactive",
        LaunchArg::Headless => "headless",
    }
}

impl From<SortArg> for agent_tapes_core::ListSort {
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
    Session,
    Launch,
    Parent,
    Commit,
    Merge,
    Agent,
    Role,
    Tier,
}

impl ByArg {
    /// The dimension of a change view this names, if it names one.
    fn change(self) -> Option<agent_tapes_core::change_usage::Dimension> {
        use agent_tapes_core::change_usage::Dimension;
        match self {
            Self::Commit => Some(Dimension::Commit),
            Self::Merge => Some(Dimension::Merge),
            Self::Agent => Some(Dimension::Agent),
            Self::Role => Some(Dimension::Role),
            Self::Tier => Some(Dimension::Tier),
            Self::Session => Some(Dimension::Session),
            Self::Harness
            | Self::Model
            | Self::Variant
            | Self::Directory
            | Self::Launch
            | Self::Parent => None,
        }
    }

    /// Whether only a change view groups by it.
    fn change_only(self) -> bool {
        self.change().is_some() && self != Self::Session
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum TierArg {
    Bound,
    Matched,
    Estimated,
}

impl From<TierArg> for agent_tapes_core::attribution::Tier {
    fn from(tier: TierArg) -> Self {
        match tier {
            TierArg::Bound => Self::Bound,
            TierArg::Matched => Self::Matched,
            TierArg::Estimated => Self::Estimated,
        }
    }
}

impl From<ByArg> for GroupBy {
    /// A summary's dimension; a change view's own dimensions refuse before
    /// a summary is grouped.
    fn from(by: ByArg) -> Self {
        match by {
            ByArg::Harness => Self::Harness,
            ByArg::Model => Self::Model,
            ByArg::Variant => Self::Variant,
            ByArg::Directory => Self::Directory,
            ByArg::Session => Self::Session,
            ByArg::Launch => Self::Launch,
            ByArg::Parent => Self::Parent,
            ByArg::Commit | ByArg::Merge | ByArg::Agent | ByArg::Role | ByArg::Tier => {
                unreachable!("a change view's dimension never groups a summary")
            }
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

const DEFAULT_SERIES_ROWS: usize = 200;
const MAX_SERIES_ROWS: usize = 10_000;

fn parse_series(value: Option<Option<usize>>) -> Result<Option<UsageObservationOptions>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let limit = value.unwrap_or(DEFAULT_SERIES_ROWS);
    if !(1..=MAX_SERIES_ROWS).contains(&limit) {
        return Err(anyhow!(
            "--series rows must be between 1 and {MAX_SERIES_ROWS}"
        ));
    }
    Ok(Some(UsageObservationOptions { limit }))
}

fn parse_event_keys(value: Option<Option<usize>>) -> Result<Option<ConsumptionKeyOptions>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let limit = value.unwrap_or(DEFAULT_SERIES_ROWS);
    if !(1..=MAX_SERIES_ROWS).contains(&limit) {
        return Err(anyhow!(
            "--event-keys must be between 1 and {MAX_SERIES_ROWS}"
        ));
    }
    Ok(Some(ConsumptionKeyOptions { limit }))
}

fn parse_window(value: &str) -> std::result::Result<WindowBounds, String> {
    let (from, to) = value
        .split_once("..")
        .ok_or_else(|| format!("`{value}` is not a window FROM..TO"))?;
    let edge = |text: &str| -> std::result::Result<Option<Instant>, String> {
        (!text.is_empty()).then(|| parse_instant(text)).transpose()
    };
    let bounds = WindowBounds {
        from: edge(from)?,
        to: edge(to)?,
    };
    if bounds
        .from
        .zip(bounds.to)
        .is_some_and(|(from, to)| from >= to)
    {
        return Err(format!("window {value} ends before it starts"));
    }
    Ok(bounds)
}

/// A planned lane mix, `MODEL[#VARIANT]=LANES,...`.
#[derive(Clone, Debug)]
struct ForecastPlan(Vec<agent_tapes_core::allowance::PlannedLanes>);

fn parse_forecast(value: &str) -> std::result::Result<ForecastPlan, String> {
    value
        .split(',')
        .map(|entry| {
            let (named, lanes) = entry
                .trim()
                .rsplit_once('=')
                .ok_or_else(|| format!("{entry:?} is not MODEL[#VARIANT]=LANES"))?;
            let lanes = lanes
                .parse::<usize>()
                .ok()
                .filter(|lanes| *lanes > 0)
                .ok_or_else(|| format!("{entry:?} plans no whole positive number of lanes"))?;
            let (model, variant) = match named.split_once('#') {
                Some((model, variant)) => (model, Some(variant.to_owned())),
                None => (named, None),
            };
            if model.is_empty() || variant.as_deref().is_some_and(str::is_empty) {
                return Err(format!("{entry:?} names no model or an empty variant"));
            }
            Ok(agent_tapes_core::allowance::PlannedLanes {
                model: model.to_owned(),
                variant,
                lanes,
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map(ForecastPlan)
}

fn parse_quota_at(value: &str) -> std::result::Result<QuotaAt, String> {
    if value == "current" {
        return Ok(QuotaAt::Current);
    }
    parse_instant(value).map(QuotaAt::At)
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum QuotaArg {
    Primary,
    Secondary,
}

impl From<QuotaArg> for QuotaKind {
    fn from(quota: QuotaArg) -> Self {
        match quota {
            QuotaArg::Primary => Self::Primary,
            QuotaArg::Secondary => Self::Secondary,
        }
    }
}

fn parse_instant(value: &str) -> std::result::Result<Instant, String> {
    agent_tapes_core::span::parse_instant(value)
        .ok_or_else(|| format!("`{value}` is not an RFC 3339 timestamp with an offset"))
}

/// `FIRST..LAST`, 1-based and inclusive, either end open.
fn parse_request_range(value: &str) -> std::result::Result<SpanBounds, String> {
    let (first, last) = value
        .split_once("..")
        .ok_or_else(|| format!("`{value}` is not a request range FIRST..LAST"))?;
    let end = |text: &str| -> std::result::Result<Option<usize>, String> {
        if text.is_empty() {
            return Ok(None);
        }
        match text.parse::<usize>() {
            Ok(0) | Err(_) => Err(format!(
                "`{text}` is not a request ordinal; ordinals count from 1"
            )),
            Ok(ordinal) => Ok(Some(ordinal)),
        }
    };
    let (first, last) = (end(first)?, end(last)?);
    if let (Some(first), Some(last)) = (first, last) {
        if first > last {
            return Err(format!("request range {value} ends before it starts"));
        }
    }
    Ok(SpanBounds::Requests { first, last })
}

#[derive(Parser)]
#[command(
    name = "tapes",
    version,
    about = "Read and export coding-agent sessions",
    after_help = "Run `tapes` with no arguments for the workflow guide.\n\nA read that fails for a reason that can pass, such as a busy store, exits 75."
)]
struct Cli {
    /// Absent prints the workflow guide: what tapes owns, the order the
    /// commands are used in, and how to judge what comes back.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Which project's sessions a command may see.
#[derive(Args, Default)]
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

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum InputFormatArg {
    #[default]
    Auto,
    Openai,
    ChatgptExporter,
    Perplexity,
}

impl From<InputFormatArg> for InputFormat {
    fn from(format: InputFormatArg) -> Self {
        match format {
            InputFormatArg::Auto => Self::Auto,
            InputFormatArg::Openai => Self::Openai,
            InputFormatArg::ChatgptExporter => Self::ChatgptExporter,
            InputFormatArg::Perplexity => Self::Perplexity,
        }
    }
}

/// A caller-supplied export is a separate source collection. Its budgets are
/// enforced before decoding records and its opaque occurrence coordinate is
/// required whenever one native id occurs more than once.
#[derive(Args, Clone, Debug, Default)]
struct InputArgs {
    /// Select exactly one materialized native store; never consult installed stores.
    /// Conflicts with --input and --harness naming another harness.
    /// OpenCode uses an API or database and cannot be selected by a root.
    #[arg(
        long,
        value_name = "claude|codex|pi",
        requires = "native_root",
        conflicts_with = "input"
    )]
    native_store: Option<String>,
    /// Harness configuration root: Claude reads PATH/projects; Codex and Pi read
    /// PATH/sessions. No harness environment is read or changed. `self` resolves
    /// only within this store; --latest and --title default to this store unless
    /// --here, --project, or --global gives a scope. No live state is attached.
    #[arg(long, value_name = "PATH", requires = "native_store")]
    native_root: Option<PathBuf>,
    /// Select this exact native recording instead of enumerating the root.
    /// Use the source.location.locator returned by list. The path is opened
    /// read-only and its recorded session identity is checked. Missing,
    /// unreadable, or mismatched files refuse without selecting another source.
    #[arg(
        long,
        value_name = "PATH",
        requires = "native_store",
        conflicts_with = "input"
    )]
    native_file: Option<PathBuf>,
    /// Read one or more files, directories, or ZIP archives as supplied
    /// conversation exports. Supplying this option never consults installed
    /// harness stores. Source and archive files must be regular opened files;
    /// FIFO writers are never awaited.
    #[arg(long = "input", value_name = "PATH")]
    input: Vec<PathBuf>,
    /// Detect the supplied representation structurally, or require the named
    /// representation explicitly. Explicit producer labels are marked as
    /// declared; ambiguous auto-detection leaves producer provenance absent.
    #[arg(long, value_enum, default_value_t = InputFormatArg::Auto)]
    input_format: InputFormatArg,
    /// Record the caller's scope label as declared source metadata.
    #[arg(long, value_name = "SCOPE")]
    source_scope: Option<String>,
    /// Maximum compressed or source bytes inspected across supplied inputs.
    #[arg(long, default_value_t = ByteSize::new(DEFAULT_SCAN_BYTES), value_name = "BYTES")]
    scan_bytes: ByteSize,
    /// Maximum uncompressed JSON bytes inspected across supplied inputs.
    #[arg(long, default_value_t = ByteSize::new(DEFAULT_DECODED_BYTES), value_name = "BYTES")]
    decoded_bytes: ByteSize,
    /// Maximum one JSON record or ZIP member decoded into memory.
    #[arg(long, default_value_t = ByteSize::new(DEFAULT_RECORD_BYTES), value_name = "BYTES")]
    record_bytes: ByteSize,
    /// Maximum serialized transcript response retained for a supplied input.
    #[arg(long, default_value_t = ByteSize::new(DEFAULT_OUTPUT_BYTES), value_name = "BYTES")]
    output_bytes: ByteSize,
    /// Maximum aggregate bytes reserved for normalized supplied-input data.
    /// This is independent of result limits and prevents a collection read
    /// from retaining an unbounded set of normalized records.
    #[arg(long, default_value_t = ByteSize::new(DEFAULT_RESIDENT_BYTES), value_name = "BYTES")]
    resident_bytes: ByteSize,
    /// Select one opaque occurrence coordinate from a supplied collection.
    #[arg(long, value_name = "COORDINATE")]
    occurrence: Option<String>,
    /// Continue a collection listing after this opaque occurrence coordinate.
    #[arg(long, value_name = "COORDINATE")]
    after_occurrence: Option<String>,
}

impl InputArgs {
    fn supplied(&self) -> bool {
        !self.input.is_empty()
    }

    fn options(&self) -> Result<InputOptions> {
        let mut options = InputOptions::new(self.input.clone(), self.input_format.into());
        options.source_scope = self.source_scope.clone();
        options.occurrence = self.occurrence.clone();
        options.after_occurrence = self.after_occurrence.clone();
        options.scan_bytes = self.scan_bytes.get();
        options.decoded_bytes = self.decoded_bytes.get();
        options.record_bytes = self.record_bytes.get();
        options.output_bytes = self.output_bytes.get();
        options.resident_bytes = self.resident_bytes.get();
        options.validate()?;
        Ok(options)
    }

    fn native(&self) -> bool {
        self.native_store.is_some()
    }

    fn native_backends(&self, bytes: u64) -> Result<Vec<Box<dyn Backend>>> {
        let root = self
            .native_root
            .as_ref()
            .ok_or_else(|| anyhow!("--native-store requires --native-root"))?;
        use agent_tapes_core::NativeStore;
        let store = match (self.native_store.as_deref(), self.native_file.as_ref()) {
            (Some("claude"), Some(path)) => NativeStore::claude_file(path),
            (Some("codex"), Some(path)) => NativeStore::codex_file(path),
            (Some("pi"), Some(path)) => NativeStore::pi_file(path),
            (Some("claude"), None) => NativeStore::claude(root.join("projects")),
            (Some("codex"), None) => NativeStore::codex(root.join("sessions")),
            (Some("pi"), None) => NativeStore::pi(root.join("sessions")),
            (Some("opencode"), _) => return Err(anyhow!("OpenCode uses an API or database, not a configuration root; --native-store opencode is unsupported")),
            _ => return Err(anyhow!("--native-store must be claude, codex, or pi")),
        };
        agent_tapes_core::backend::backends_from_native_root(store, root.clone(), bytes)
    }

    fn backends(&self) -> Result<Vec<Box<dyn Backend>>> {
        if self.native() {
            return self.native_backends(agent_tapes_core::backend::DEFAULT_READ_BYTES);
        }
        if !self.supplied() {
            return Ok(agent_tapes_core::backend::backends());
        }
        Ok(vec![Box::new(InputBackend::new(self.options()?)?)])
    }

    fn validate_collection(&self) -> Result<()> {
        if self.after_occurrence.is_some() && !self.supplied() {
            return Err(anyhow!("--after-occurrence requires --input"));
        }
        if self.occurrence.is_some() && !self.supplied() {
            return Err(anyhow!("--occurrence requires --input"));
        }
        if self.occurrence.is_some() && self.after_occurrence.is_some() {
            return Err(anyhow!("--occurrence conflicts with --after-occurrence"));
        }
        Ok(())
    }

    fn validate_bulk(&self) -> Result<()> {
        self.validate_collection()?;
        if self.occurrence.is_some() {
            return Err(anyhow!(
                "--occurrence selects one record and is not available for collection views"
            ));
        }
        Ok(())
    }
}

fn refuse_full_supplied(input: &InputArgs) -> Result<()> {
    if input.supplied() {
        return Err(anyhow!(
            "--full reads installed recordings; a supplied input is bounded by \
             --scan-bytes, --decoded-bytes, and --record-bytes"
        ));
    }
    Ok(())
}

fn reject_installed_selection(
    input: &InputArgs,
    harness: Option<&str>,
    scope: &ScopeArgs,
) -> Result<()> {
    if input.native() && harness.is_some_and(|h| Some(h) != input.native_store.as_deref()) {
        return Err(anyhow!(
            "--native-store conflicts with --harness naming another harness"
        ));
    }
    if !input.supplied() {
        return Ok(());
    }
    if harness.is_some() || scope.here || scope.project.is_some() || scope.global {
        return Err(anyhow!(
            "--input conflicts with installed-store selectors --harness, --here, --project, and --global"
        ));
    }
    Ok(())
}

/// How much of each installed recording a transcript read takes.
/// The price table a command prices its reported costs with.
#[derive(Args, Clone, Debug, Default)]
struct PriceArgs {
    /// Price each figure this command reports a harness cost for with this
    /// tapes-price-table/1 or /2 file, adding `list_price_estimate` beside
    /// that cost, never in its place, exactly as `usage --prices` prices it.
    /// Without the flag the table is the file TAPES_PRICES names, then
    /// $XDG_CONFIG_HOME/tapes/prices.toml (or ~/.config/tapes/prices.toml);
    /// with none, no estimate is given. A table named here or by
    /// TAPES_PRICES that is missing or invalid refuses; an invalid default
    /// table is reported on stderr and leaves the estimate absent. A
    /// replica's answer under --remote is not priced.
    #[arg(long, value_name = "FILE")]
    prices: Option<PathBuf>,
}

impl PriceArgs {
    /// The table to price with, as `usage` finds it.
    fn table(&self) -> Result<Option<agent_tapes_core::pricing::PriceTable>> {
        price_table(self.prices.as_deref())
    }
}

/// The price table `named` or the environment selects, warning about an
/// invalid default table, which leaves estimates absent.
fn price_table(named: Option<&Path>) -> Result<Option<agent_tapes_core::pricing::PriceTable>> {
    let prices = agent_tapes_core::pricing::lookup(named)?;
    if let PriceLookup::DefaultInvalid { path, error } = &prices {
        eprintln!(
            "warning: the default price table {} is not used, so no list-price estimate is given: {error}",
            path.display()
        );
    }
    Ok(prices.table().cloned())
}

#[derive(Args, Clone, Copy, Debug, Default)]
struct ReadArgs {
    /// Read at most this much from the end of each installed Claude, Codex,
    /// or Pi recording [default: 4MiB], between 64KiB and 1GiB. The window is
    /// held in memory whole. Supplied inputs keep their own byte budgets.
    #[arg(long, value_name = "BYTES")]
    read_bytes: Option<ByteSize>,
}

impl ReadArgs {
    fn refuse_supplied(&self, input: &InputArgs) -> Result<()> {
        if self.read_bytes.is_some() && input.supplied() {
            return Err(anyhow!(
                "--read-bytes bounds installed recordings; a supplied input is bounded by \
                 --scan-bytes, --decoded-bytes, and --record-bytes"
            ));
        }
        Ok(())
    }

    fn backends(&self, input: &InputArgs) -> Result<Vec<Box<dyn Backend>>> {
        if input.native() {
            return input.native_backends(
                self.read_bytes
                    .map_or(agent_tapes_core::backend::DEFAULT_READ_BYTES, ByteSize::get),
            );
        }
        match self.read_bytes {
            Some(bytes) => agent_tapes_core::backend::backends_with_read_bytes(bytes.get()),
            None => Ok(agent_tapes_core::backend::backends()),
        }
    }
}

/// Which turn kinds a transcript keeps. Kinds are named by the labels turns
/// carry: operator, assistant, reasoning, tool, control, ambient, notice, and
/// unknown.
#[derive(Args, Clone, Debug, Default)]
struct TurnKindArgs {
    /// Keep only turns of these kinds; repeatable or comma-separated. Kept
    /// turns carry their timestamps and original ordinals, and every other
    /// turn is counted by kind under `projection`.
    #[arg(long, value_name = "KIND", value_delimiter = ',', value_parser = turn_kind_parser(), conflicts_with = "omit")]
    only: Vec<TurnKind>,
    /// Keep operator turns sent by person, program, or harness. Origin requires
    /// positive sender evidence; turns without an origin never match.
    #[arg(long, value_name = "ORIGIN", value_parser = origin_parser())]
    origin: Option<agent_tapes_core::model::OperatorOrigin>,
    /// Keep every turn but those of these kinds; repeatable or
    /// comma-separated. Omitted turns are counted by kind under `projection`.
    #[arg(long, value_name = "KIND", value_delimiter = ',', value_parser = turn_kind_parser())]
    omit: Vec<TurnKind>,
}

impl TurnKindArgs {
    /// The selection the flags ask for, with the flag that asked for it.
    fn selection(&self) -> Option<(TurnSelection, &'static str)> {
        let (mut selection, flag) = if !self.only.is_empty() {
            (TurnSelection::only(self.only.iter().copied()), "--only")
        } else if !self.omit.is_empty() {
            (TurnSelection::omit(self.omit.iter().copied()), "--omit")
        } else if self.origin.is_some() {
            (TurnSelection::only(TurnKind::ALL), "--origin")
        } else {
            return None;
        };
        if let Some(origin) = self.origin {
            selection = selection.with_origin(origin);
        }
        Some((selection, flag))
    }
}

fn origin_parser(
) -> impl clap::builder::TypedValueParser<Value = agent_tapes_core::model::OperatorOrigin> {
    use agent_tapes_core::model::OperatorOrigin;
    use clap::builder::TypedValueParser as _;
    clap::builder::PossibleValuesParser::new(["person", "program", "harness"]).map(|label| {
        match label.as_str() {
            "person" => OperatorOrigin::Person,
            "program" => OperatorOrigin::Program,
            "harness" => OperatorOrigin::Harness,
            _ => unreachable!("origin parser admits only origin labels"),
        }
    })
}

fn turn_kind_parser() -> impl clap::builder::TypedValueParser<Value = TurnKind> {
    use clap::builder::TypedValueParser as _;
    clap::builder::PossibleValuesParser::new(TurnKind::ALL.map(TurnKind::label))
        .map(|label| TurnKind::from_label(&label).expect("the parser admits only turn-kind labels"))
}

/// The fields of a selector that accept a session id, so `self` can be
/// replaced with the caller's own id before the selector is read.
trait SelfSelectable {
    fn ids_mut(&mut self) -> (&mut Option<String>, &mut Vec<String>);
    fn input_supplied(&self) -> bool;
    fn input(&self) -> &InputArgs;
}

/// Replace `self` in a selector with the caller's own session id, reporting
/// what it became. A supplied input holds no session of the caller's, and a
/// replica holds sessions of the machine that answered, so `self` refuses
/// there.
fn apply_self(selector: &mut dyn SelfSelectable, remote: Option<&str>) -> Result<()> {
    let supplied = selector.input_supplied();
    let (session, exclude) = selector.ids_mut();
    let named = session.as_deref() == Some(self_token::TOKEN)
        || exclude.iter().any(|id| id == self_token::TOKEN);
    if !named {
        return Ok(());
    }
    if let Some(destination) = remote {
        return Err(anyhow!(
            "self names the caller's own session on this machine; replica {destination} holds the sessions of the machine that answered it, so name an id that replica lists"
        ));
    }
    if supplied {
        return Err(anyhow!(
            "self names the caller's own installed session and selects nothing from a supplied input"
        ));
    }
    let backends = selector.input().backends()?;
    let resolved = self_token::resolve(&backends)?;
    let (session, exclude) = selector.ids_mut();
    eprintln!("Note: {}.", resolved.report());
    for id in session.iter_mut().chain(exclude.iter_mut()) {
        if id == self_token::TOKEN {
            id.clone_from(&resolved.id);
        }
    }
    Ok(())
}

impl SelfSelectable for SelectionArgs {
    fn ids_mut(&mut self) -> (&mut Option<String>, &mut Vec<String>) {
        (&mut self.session, &mut self.exclude)
    }

    fn input_supplied(&self) -> bool {
        self.input.supplied()
    }

    fn input(&self) -> &InputArgs {
        &self.input
    }
}

impl SelfSelectable for SessionQueryArgs {
    fn ids_mut(&mut self) -> (&mut Option<String>, &mut Vec<String>) {
        (&mut self.session, &mut self.exclude)
    }

    fn input_supplied(&self) -> bool {
        self.input.supplied()
    }

    fn input(&self) -> &InputArgs {
        &self.input
    }
}

impl Command {
    /// The selector this command reads its session from, where it has one.
    fn selector_mut(&mut self) -> Option<&mut dyn SelfSelectable> {
        match self {
            Command::Show { selection, .. }
            | Command::Page { selection, .. }
            | Command::HistorySearch { selection, .. }
            | Command::Metadata { selection, .. }
            | Command::Child { selection, .. }
            | Command::Lineage { selection, .. }
            | Command::Brief { selection, .. }
            | Command::Workset { selection, .. } => Some(selection),
            Command::Events {
                selection: query, ..
            }
            | Command::Timeline { query, .. }
            | Command::Stats { query, .. }
            | Command::Usage { query, .. }
            | Command::Export { query, .. } => Some(query),
            _ => None,
        }
    }

    /// The replica this command reads from, where it names one.
    fn remote_destination(&self) -> Option<&str> {
        match self {
            Command::List { remote, .. }
            | Command::Show { remote, .. }
            | Command::Export { remote, .. } => remote.as_deref(),
            _ => None,
        }
    }
}

fn id_selection<'a>(id: &'a str, harness: Option<&'a str>) -> Selection<'a> {
    match harness {
        Some(harness) => Selection::IdInHarness { id, harness },
        None => Selection::Id(id),
    }
}

/// Select one session by ID, exact recorded title, or latest activity.
/// ID resolution honors harness restrictions; title and latest selection also
/// honor project scope.
#[derive(Args)]
struct SelectionArgs {
    /// Session identifier, full or a prefix whose bounded native discovery proves unique, or `self` for the
    /// session the caller runs in, as its harness's exported variable names it.
    /// Identity mismatches refuse prefix fallback; a correct exact match wins
    /// even after another candidate's read failure. The filename/header rule
    /// also applies to Pi's explicit recording file. Recording reads accept only
    /// regular opened files and never wait for FIFO writers. Identity-bearing
    /// records must match the selected native session in the opening and bounded
    /// tail, or throughout a full read. Repeated directory recording IDs choose the newest
    /// modification time, then the lexically first locator, as listing does.
    /// --native-file selects one locator explicitly. Listing prefilters with uncertain native
    /// identity defer to normalization and its diagnostics.
    #[arg(
        required_unless_present_any = ["latest", "title", "occurrence"],
        conflicts_with_all = ["latest", "title", "occurrence", "exclude", "here", "project", "global"]
    )]
    session: Option<String>,
    /// Match the recorded title exactly in scope; incomplete or ambiguous lookup refuses.
    #[arg(long, conflicts_with_all = ["session", "latest", "occurrence", "exclude"])]
    title: Option<String>,
    /// Take the most recent session in scope instead of naming one. A
    /// caller asking from inside a live session is usually itself the most
    /// recent one in its own project, so reaching an older session takes
    /// `--exclude self`.
    #[arg(long)]
    latest: bool,
    /// Pass over this session when taking the latest. Repeatable. An agent
    /// asking from inside its own session passes `self` here.
    #[arg(long, requires = "latest")]
    exclude: Vec<String>,
    /// Restrict ID, prefix, title, or latest lookup to one harness.
    /// Incomplete prefix lookups identify the stores searched and their completeness.
    #[arg(long)]
    harness: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
    #[command(flatten)]
    input: InputArgs,
}

impl SelectionArgs {
    fn validate_input(&self) -> Result<()> {
        self.input.validate_collection()?;
        reject_installed_selection(&self.input, self.harness.as_deref(), &self.scope)?;
        if self.input.supplied() && self.latest {
            return Err(anyhow!(
                "--latest is not available for caller-supplied inputs; name an id, exact title, or --occurrence"
            ));
        }
        if self.input.after_occurrence.is_some() {
            return Err(anyhow!(
                "--after-occurrence is only available for collection selections"
            ));
        }
        Ok(())
    }

    fn within_or_here(&self) -> Where<'_> {
        if self.input.supplied()
            || (self.input.native() && !self.scope.here && self.scope.project.is_none())
        {
            Where::Global
        } else {
            self.scope.within_or_here()
        }
    }

    fn selection(&self) -> Selection<'_> {
        match (
            self.input.occurrence.as_deref(),
            self.session.as_deref(),
            self.title.as_deref(),
        ) {
            (Some(occurrence), _, _) => Selection::Occurrence(occurrence),
            (None, Some(session), _) => id_selection(session, self.harness.as_deref()),
            (None, _, Some(title)) => Selection::Title {
                title,
                within: self.within_or_here(),
                harness: self.harness.as_deref(),
            },
            (None, None, None) => Selection::Latest {
                within: self.within_or_here(),
                harness: self.harness.as_deref(),
                exclude: &self.exclude,
            },
        }
    }

    fn latest_warnings(&self, backends: &[Box<dyn Backend>]) -> Result<Vec<String>> {
        if !self.latest {
            return Ok(Vec::new());
        }
        agent_tapes_core::selection_warnings_with_backends(backends, self.selection())
    }
}

/// The common single-session and session-set selectors for aggregate-capable views.
#[derive(Args)]
#[group(multiple = true)]
struct SessionQueryArgs {
    /// Session identifier, full or a prefix whose bounded native discovery proves unique, or `self` for the
    /// session the caller runs in, as its harness's exported variable names it.
    /// Identity mismatches refuse prefix fallback; a correct exact match wins
    /// even after another candidate's read failure. The filename/header rule
    /// also applies to Pi's explicit recording file. Recording reads accept only
    /// regular opened files and never wait for FIFO writers. Identity-bearing
    /// records must match the selected native session in the opening and bounded
    /// tail, or throughout a full read. Repeated directory recording IDs choose the newest
    /// modification time, then the lexically first locator, as listing does.
    /// --native-file selects one locator explicitly. Listing prefilters with uncertain native
    /// identity defer to normalization and its diagnostics.
    #[arg(conflicts_with_all = ["title", "latest", "occurrence", "exclude", "here", "project", "global"])]
    session: Option<String>,
    /// Match the recorded title exactly in scope; incomplete or ambiguous lookup refuses.
    #[arg(long, conflicts_with_all = ["session", "latest", "occurrence", "exclude", "limit", "model", "launch", "directory", "since", "until", "sort", "search"])]
    title: Option<String>,
    /// Take the most recent session in scope instead of naming one. A
    /// caller asking from inside a live session is usually itself the most
    /// recent one in its own project, so reaching an older session takes
    /// `--exclude self`.
    #[arg(long)]
    latest: bool,
    /// Pass over this session when taking the latest. Repeatable. An agent
    /// asking from inside its own session passes `self` here.
    #[arg(long, requires = "latest")]
    exclude: Vec<String>,
    /// Restrict ID, prefix, title, latest, or set selection to one harness.
    #[arg(long)]
    harness: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
    #[command(flatten)]
    input: InputArgs,
    /// Take at most this many sessions from each harness [default: 20].
    #[arg(long, conflicts_with_all = ["session", "latest", "title", "occurrence"])]
    limit: Option<usize>,
    /// Match case-insensitively against the full model identity, `id` or
    /// `id (variant)`. Sessions without a model never match.
    #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest", "title", "occurrence"])]
    model: Option<String>,
    /// Keep sessions the harness records as interactive or headless. Pi
    /// records one only with `tapes install pi`, OpenCode with `tapes install opencode`, and sessions
    /// without a recognized marker never match.
    #[arg(long, value_enum, value_name = "MODE", conflicts_with_all = ["session", "latest", "title", "occurrence"])]
    launch: Option<LaunchArg>,
    /// Match case-insensitively against the recorded directory path.
    /// Sessions without a directory never match.
    #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest", "title", "occurrence"])]
    directory: Option<String>,
    /// Keep sessions whose newest recorded activity, `last_activity_at`,
    /// is at or after this timestamp. RFC 3339 timestamps with an offset
    /// and bare YYYY-MM-DD dates are accepted.
    #[arg(
        long,
        value_name = "TIMESTAMP",
        value_parser = agent_tapes_core::parse_activity_timestamp,
        conflicts_with_all = ["session", "latest", "title", "occurrence"]
    )]
    since: Option<agent_tapes_core::ActivityTimestamp>,
    /// Keep sessions whose newest recorded activity, `last_activity_at`,
    /// is before this timestamp. RFC 3339 timestamps with an offset and
    /// bare YYYY-MM-DD dates are accepted.
    #[arg(
        long,
        value_name = "TIMESTAMP",
        value_parser = agent_tapes_core::parse_activity_timestamp,
        conflicts_with_all = ["session", "latest", "title", "occurrence"]
    )]
    until: Option<agent_tapes_core::ActivityTimestamp>,
    /// Order the selection by `last_activity_at` before --limit takes
    /// from it: newest first by default, or oldest first.
    #[arg(long, value_enum, value_name = "ORDER", conflicts_with_all = ["session", "latest", "title", "occurrence"])]
    sort: Option<SortArg>,
    /// Match case-insensitively against the last 32 normalized turns in
    /// each candidate session. The fixed tail keeps the selection bounded;
    /// a match outside it is not considered.
    #[arg(long, value_name = "SUBSTRING", conflicts_with_all = ["session", "latest", "title", "occurrence"])]
    search: Option<String>,
}

impl SessionQueryArgs {
    fn validate_input(&self) -> Result<()> {
        self.input.validate_collection()?;
        reject_installed_selection(&self.input, self.harness.as_deref(), &self.scope)?;
        if self.input.supplied() && self.latest {
            return Err(anyhow!(
                "--latest is not available for caller-supplied inputs; name an id, exact title, or --occurrence"
            ));
        }
        if self.input.occurrence.is_some()
            && (self.limit.is_some()
                || self.model.is_some()
                || self.launch.is_some()
                || self.directory.is_some()
                || self.since.is_some()
                || self.until.is_some()
                || self.sort.is_some()
                || self.search.is_some())
        {
            return Err(anyhow!("--occurrence conflicts with collection filters"));
        }
        if self.input.after_occurrence.is_some()
            && (self.session.is_some() || self.title.is_some() || self.latest)
        {
            return Err(anyhow!(
                "--after-occurrence is only available for collection selections"
            ));
        }
        Ok(())
    }

    fn within_or_here(&self) -> Where<'_> {
        if self.input.supplied()
            || (self.input.native() && !self.scope.here && self.scope.project.is_none())
        {
            Where::Global
        } else {
            self.scope.within_or_here()
        }
    }

    fn single(&self) -> Option<Selection<'_>> {
        if let Some(occurrence) = self.input.occurrence.as_deref() {
            return Some(Selection::Occurrence(occurrence));
        }
        if let Some(id) = self.session.as_deref() {
            return Some(id_selection(id, self.harness.as_deref()));
        }
        if let Some(title) = self.title.as_deref() {
            return Some(Selection::Title {
                title,
                within: self.within_or_here(),
                harness: self.harness.as_deref(),
            });
        }
        self.latest.then_some(Selection::Latest {
            within: self.within_or_here(),
            harness: self.harness.as_deref(),
            exclude: &self.exclude,
        })
    }

    fn latest_warnings(&self, backends: &[Box<dyn Backend>]) -> Result<Vec<String>> {
        let Some(selection) = self.single() else {
            return Ok(Vec::new());
        };
        if !self.latest {
            return Ok(Vec::new());
        }
        agent_tapes_core::selection_warnings_with_backends(backends, selection)
    }

    fn has_set_selection(&self) -> bool {
        self.scope.here
            || self.scope.global
            || self.scope.project.is_some()
            || self.input.supplied()
            || self.input.native()
            || self.input.after_occurrence.is_some()
            || self.harness.is_some()
            || self.limit.is_some()
            || self.model.is_some()
            || self.launch.is_some()
            || self.directory.is_some()
            || self.since.is_some()
            || self.until.is_some()
            || self.sort.is_some()
            || self.search.is_some()
    }

    fn set(&self) -> agent_tapes_core::SessionSelection<'_> {
        agent_tapes_core::SessionSelection {
            within: if self.input.supplied() {
                Where::Global
            } else {
                self.scope.within()
            },
            harness: if self.input.supplied() {
                None
            } else {
                self.harness.as_deref()
            },
            limit: self.limit,
            filters: agent_tapes_core::ListFilters {
                model: self.model.as_deref(),
                launch: self.launch.map(Into::into),
                directory: self.directory.as_deref(),
                since: self.since,
                until: self.until,
                search: self.search.as_deref(),
                search_turns: None,
            },
            sort: self.sort.unwrap_or(SortArg::Newest).into(),
        }
    }

    fn validate_window(&self) -> Result<()> {
        if self
            .since
            .zip(self.until)
            .is_some_and(|(since, until)| since >= until)
        {
            return Err(anyhow!("--since must be earlier than --until"));
        }
        Ok(())
    }

    fn describe_search(&self) {
        if self.search.is_some() {
            eprintln!("Searching a bounded window of normalized turns in each candidate session before applying --limit.");
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Read a persisted tool output using its opaque show/events reference.
    /// Only native Claude recordings support confined no-follow reads.
    /// JSON uses tapes-artifact/1 with bytes, truncated, sha256, encoding and body.
    Artifact {
        session: String,
        #[arg(long)]
        reference: String,
        /// Returned byte budget: default 64 KiB, maximum 4 MiB.
        #[arg(long, default_value = "64KiB")]
        bytes: ByteSize,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        input: InputArgs,
    },
    /// Report one recording's custody as tapes-capture/2. Installed harness
    /// stores are unpinned; explicit native sources report caller-selected-native-store.
    /// Selection and export do not assert immutable native bytes.
    Capture {
        #[command(flatten)]
        input: InputArgs,
        /// Exact session id, an unambiguous prefix, or `self`.
        /// Identity mismatches refuse prefix fallback; a correct exact match wins
        /// even after another candidate's read failure. The filename/header rule
        /// also applies to Pi's explicit recording file. Recording reads accept only
        /// regular opened files and never wait for FIFO writers. Identity-bearing
        /// records must match the selected native session in the opening and bounded
        /// tail, or throughout a full read. Repeated directory recording IDs choose the newest
        /// modification time, then the lexically first locator, as listing does.
        /// --native-file selects one locator explicitly. Listing prefilters with uncertain native
        /// identity defer to normalization and its diagnostics.
        session: String,
        /// Restrict ID or prefix lookup to one harness.
        #[arg(long)]
        harness: Option<String>,
        /// Render the versioned report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List available sessions. Human output keeps the exact session id in
    /// the ID column and puts any present-tense state in a separate LIVE
    /// column. If agent-status is unavailable, malformed, oversized, or
    /// slower than its 250 ms deadline, LIVE is blank. Native recording probes
    /// accept only regular opened files and never wait for FIFO writers.
    /// Identity-bearing records must match the discovered native session in
    /// a recording's opening and in its retained tail; a recording whose tail
    /// names another session is reported unavailable, never listed under the
    /// opening's identity. Uncertain prefilters defer to normalization and its
    /// diagnostics.
    List {
        /// Restrict results to one harness.
        #[arg(long)]
        harness: Option<String>,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Take at most this many sessions from each harness [default: 20].
        /// Scope and filters apply first. With --search --full, this bounds
        /// sessions read globally before content matching; withheld candidates
        /// are reported by full_search.selection_truncated.
        #[arg(long)]
        limit: Option<usize>,
        /// Match case-insensitively against the full model identity shown in
        /// the MODEL column: the id and, when present, its `id (variant)`
        /// spelling. Sessions without a model never match.
        #[arg(long, value_name = "SUBSTRING")]
        model: Option<String>,
        /// Keep sessions the harness records as interactive or headless.
        /// Pi records one only with `tapes install pi`, OpenCode with `tapes install opencode`, and
        /// sessions without a recognized marker never match.
        #[arg(long, value_enum, value_name = "MODE")]
        launch: Option<LaunchArg>,
        /// Match case-insensitively against the recorded directory path.
        /// Sessions without a directory never match.
        #[arg(long, value_name = "SUBSTRING")]
        directory: Option<String>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is at or after this timestamp. RFC 3339 timestamps with an offset
        /// and bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = agent_tapes_core::parse_activity_timestamp)]
        since: Option<agent_tapes_core::ActivityTimestamp>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is before this timestamp. RFC 3339 timestamps with an offset and
        /// bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = agent_tapes_core::parse_activity_timestamp)]
        until: Option<agent_tapes_core::ActivityTimestamp>,
        /// Order by `last_activity_at`: newest first by default, or oldest
        /// first. The order decides which sessions --limit keeps: each
        /// harness's newest matches, or its oldest, which inspects every
        /// candidate the scan reaches. Equal timestamps are ordered by
        /// session id, then harness.
        #[arg(long, value_enum, default_value_t = SortArg::Newest)]
        sort: SortArg,
        /// Match case-insensitively against normalized turns. Bounded searches
        /// precede --limit; --search-turns selects their turn window. Partial
        /// bounded nonmatches report partial coverage. --full searches whole
        /// recordings after --limit. Read failures are unsearched, and a
        /// preflight notice goes to stderr.
        #[arg(long, value_name = "SUBSTRING")]
        search: Option<String>,
        /// Number of normalized turns to search in each bounded read. `all`
        /// searches every retained turn; source byte and record bounds apply.
        #[arg(long, value_name = "N|all", default_value = "32", requires = "search", value_parser = parse_search_turns)]
        search_turns: usize,
        /// Stream whole recordings after metadata and scope filters. --limit
        /// bounds sessions read globally. full_search reports selection
        /// truncation and coverage for matches and nonmatches; coordinates
        /// carry zero-based ordinal, kind, timestamp and record reference.
        /// whole_recording_read requires no gaps, skipped records or bounds.
        #[arg(long, requires = "search", conflicts_with = "search_turns")]
        full: bool,
        /// Maximum matching turn coordinates retained per session in a whole
        /// search. Overflow sets matches_truncated without stopping the read.
        #[arg(long, default_value = "100", requires = "full")]
        search_matches: usize,

        #[command(flatten)]
        input: InputArgs,
        /// Render results as JSON. Matching sessions may include optional
        /// `launch`, `live`, and `accounting` fields; accounting states the
        /// basis and coverage of any recorded cost or token counters.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        prices: PriceArgs,
        /// Answer from a named replica over ssh: run its own tapes with the
        /// same arguments and read its JSON. Every session then names that
        /// replica, its live state is whatever the replica reported, and
        /// scope paths resolve on the replica. A replica that cannot answer
        /// is reported unavailable with the cause and the destination, never
        /// as an empty listing.
        #[arg(long, value_name = "SSH-DESTINATION", conflicts_with = "input")]
        remote: Option<String>,
    },
    /// Show one session. The session carries a source descriptor and optional
    /// activity timestamps. Claude compaction boundaries are notices with typed
    /// context-event measurements and preserved anchors; use --full or history
    /// pages to retrieve retained pre-compaction messages. Claude file attachments
    /// are ambient file references with bounded text bodies and recorded line
    /// ranges; compact references have no body. Referenced files are never opened.
    /// Instructions are ambient file references; environment snapshots and changes
    /// are ambient notices with bounded text. Prompt snapshots retain only prompt
    /// character and tool counts and the recorded systemTurns boolean, without bodies.
    /// Attachment bodies retain at most 64Ki characters per field; cuts carry
    /// read-bound availability and omitted_reason. Repeated records stay separate.
    /// Other attachment types remain unrecognized. Claude api-request-blob records,
    /// which restate a sent request's system prompt and messages, become no turn.
    /// Complete persisted-output tool results show their preview and a reference-only
    /// file with its recorded size label and opaque artifact reference; this view
    /// never reads the external body. Use artifact for an explicit confined read.
    /// Every turn carries a
    /// `kind` naming what the harness
    /// recorded it as, and a user turn holding a harness command, notice, or
    /// attached context is headed `user/<kind>` rather than `user`. The human
    /// header names a recognized launch mode and marks a matching live session
    /// when agent-status is reachable,
    /// and a verified trailing record is named when the store ends after its
    /// last rendered turn. Activity comparisons use the whole-second
    /// timestamps shown to the reader. Claude subagent count notes report
    /// failed directory enumeration instead of a count. JSON adds `locations`:
    /// the distinct places the recording shows the session working, as
    /// changes only, in recording order, each `at` (record time), `cwd`, and
    /// the `branch`, `head_commit` and `repository_url` the harness recorded
    /// (a field it did not record is absent). `coverage` is `whole` for a
    /// --full read and `from-window-start` for a bounded tail read, whose first
    /// place is where the window opens. At most the newest 1024 are kept and
    /// `omitted` counts the older ones dropped. Claude records a directory and
    /// branch on every message; Codex a directory, branch, commit and
    /// repository on its header and a directory on each turn; Pi and OpenCode
    /// only the session's directory.
    Show {
        /// Cap each shown turn's text at N Unicode characters. JSON includes
        /// text_truncated: true and the original text_chars on capped turns;
        /// human output names the omitted character count. Native content
        /// parts and other metadata retain their own bounds. Applies to
        /// bounded, --full, supplied-input, and remote reads.
        #[arg(long, value_name = "N")]
        turn_chars: Option<usize>,
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        read: ReadArgs,
        /// Show only the final number of messages. Under --exchange, --only,
        /// or --omit it counts kept turns.
        #[arg(long)]
        tail: Option<usize>,
        /// Keep only the exchange: operator requests and the agent's visible
        /// text. Shorthand for --only operator,assistant.
        #[arg(long, conflicts_with_all = ["only", "omit"])]
        exchange: bool,
        #[command(flatten)]
        kinds: TurnKindArgs,
        /// Read the whole recording instead of its bounded tail, writing each
        /// turn as it is read so memory follows one record rather than the
        /// file. Every turn is shown unless --tail is given; with --json the
        /// same tapes-session object is written turn by turn. Its read evidence
        /// includes a SHA-256 source_prefix_digest of bytes [0, source_length)
        /// when a native file streaming pin is established. Installed Claude,
        /// Codex, Pi, and OpenCode sessions; OpenCode pages every message
        /// oldest first, and its stable database keeps cutting part text at
        /// 4000 characters (2000 for tool payloads), reported as a source bound,
        /// and a database row it cannot read is a `malformed-record` gap.
        #[arg(long, conflicts_with = "read_bytes")]
        full: bool,
        /// Render the session as JSON. The session may include optional
        /// `live`, `accounting`, and `trailing_record` fields supplied by its
        /// authorities. Claude, Pi and Codex include `model_observation` and
        /// ordered `model_selections` with first/last timestamp and native id.
        /// `head_read: false` marks partial selection coverage. Claude and Pi
        /// assistant turns carry their record's model and effort (`variant`).
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        prices: PriceArgs,
        /// Read the session from a named replica over ssh: run its own tapes
        /// with the same arguments and read its JSON. The session then names
        /// that replica, its live state is whatever the replica reported, and
        /// scope paths resolve on the replica. A replica that cannot answer
        /// refuses with the cause and the destination, never as a completed
        /// session.
        #[arg(long, value_name = "SSH-DESTINATION", conflicts_with = "input")]
        remote: Option<String>,
    },
    /// Read one bounded page of older Claude or Codex history as tapes-page/12,
    /// including absolute record references and read evidence. A page decodes
    /// only the records inside its byte range, so a turn's kind matches the one
    /// a whole read gives it. Resume with the returned cursor.
    Page {
        #[command(flatten)]
        selection: SelectionArgs,
        #[arg(long)]
        cursor: Option<String>,
        /// Hash a fixed prefix up to 1GiB in one streaming pass with bounded memory.
        /// Later pages verify their chunks, ignore appends beyond the pinned end,
        /// and refuse replacement, truncation or an edit in a chunk they reach.
        /// Continue with --cursor alone; a strict walk must restart to acquire a pin.
        /// Use --bytes equal to the reported chunk size to limit verification reads
        /// (at most 3 × 4MiB per page). Chunks already passed are not rechecked;
        /// changes during hashing are part of its observation. A pin preserves no
        /// bytes through a rewrite: read a captured copy with --native-store instead.
        #[arg(long, conflicts_with = "cursor")]
        pin: bool,
        /// Maximum payload bytes per page, between 1KiB and 4MiB.
        /// The one alignment byte read before the page is counted separately.
        #[arg(long, default_value_t = ByteSize::new(agent_tapes_core::history::DEFAULT_BYTES as u64))]
        bytes: ByteSize,
        #[arg(long)]
        json: bool,
    },
    /// Search normalized historical turns within an explicit page budget, retaining coverage gaps.
    /// JSON uses tapes-history-search/11; matches retain source references, native IDs,
    /// kinds and timestamps. Page bounds and ordinals are page-local coordinates.
    HistorySearch {
        #[command(flatten)]
        selection: SelectionArgs,
        #[arg(long)]
        cursor: Option<String>,
        /// Hash a fixed prefix up to 1GiB in one streaming pass with bounded memory.
        /// Later pages verify their chunks, ignore appends beyond the pinned end,
        /// and refuse replacement, truncation or an edit in a chunk they reach.
        /// Continue with --cursor alone; a strict walk must restart to acquire a pin.
        /// Use --bytes equal to the reported chunk size to limit verification reads
        /// (at most 3 × 4MiB per page). Chunks already passed are not rechecked;
        /// changes during hashing are part of its observation. A pin preserves no
        /// bytes through a rewrite: read a captured copy with --native-store instead.
        #[arg(long, conflicts_with = "cursor")]
        pin: bool,
        #[arg(long, default_value_t = ByteSize::new(agent_tapes_core::history::DEFAULT_BYTES as u64))]
        bytes: ByteSize,
        /// Maximum pages to read, between 1 and 32.
        #[arg(long, default_value_t = 1)]
        pages: usize,
        #[arg(long)]
        search: String,
        #[arg(long)]
        json: bool,
    },
    /// Recover recorded model observations outside the usual source tail as a
    /// models-only tapes-page/12 projection, with explicit history coverage.
    /// JSON uses tapes-metadata-history/11.
    Metadata {
        #[command(flatten)]
        selection: SelectionArgs,
        #[arg(long)]
        cursor: Option<String>,
        /// Hash a fixed prefix up to 1GiB in one streaming pass with bounded memory.
        /// Later pages verify their chunks, ignore appends beyond the pinned end,
        /// and refuse replacement, truncation or an edit in a chunk they reach.
        /// Continue with --cursor alone; a strict walk must restart to acquire a pin.
        /// Use --bytes equal to the reported chunk size to limit verification reads
        /// (at most 3 × 4MiB per page). Chunks already passed are not rechecked;
        /// changes during hashing are part of its observation. A pin preserves no
        /// bytes through a rewrite: read a captured copy with --native-store instead.
        #[arg(long, conflicts_with = "cursor")]
        pin: bool,
        #[arg(long, default_value_t = ByteSize::new(agent_tapes_core::history::DEFAULT_BYTES as u64))]
        bytes: ByteSize,
        #[arg(long, default_value_t = 1)]
        pages: usize,
        #[arg(long)]
        json: bool,
    },
    /// Read a Claude child's own transcript, accounting and ending as tapes-child/11 under its parent session.
    /// The transcript's `locations` are the places the child's own recording shows, as show reports them.
    Child {
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        read: ReadArgs,
        /// The exact child reference reported by lineage.
        #[arg(long)]
        reference: String,
        /// Maximum transcript turns to render; usage and ending cover the bounded source read.
        #[arg(long, default_value_t = 40)]
        tail: usize,
        /// Stream the child's whole recording instead of its bounded tail.
        /// Usage counts every turn and folds its counters from every record,
        /// and every record's native parent identity and agent id is checked;
        /// the transcript and ending keep only the newest --tail turns.
        #[arg(long, conflicts_with = "read_bytes")]
        full: bool,
        #[arg(long)]
        json: bool,
    },
    /// Project typed tool calls and results from a session or selection. Pairing is exact
    /// within the bounded read; an unpaired event names which read boundary
    /// prevented a complete pair. Claude subagent count notes report failed
    /// directory enumeration instead of a count.
    Events {
        #[command(flatten)]
        selection: SessionQueryArgs,
        #[command(flatten)]
        read: ReadArgs,
        /// Return events on turns within the final N-turn ordinal window. The
        /// default is every turn the bounded reader reaches.
        #[arg(long)]
        tail: Option<usize>,
        /// Match the recorded tool name exactly. Repeatable.
        #[arg(long, visible_alias = "tool", value_name = "NAME")]
        name: Vec<String>,
        /// Match a case-insensitive substring of the recorded shell command,
        /// or serialized input for other tools. Uses complete retained input,
        /// including text beyond the argument preview, and keeps paired results.
        #[arg(long, value_name = "SUBSTRING")]
        command: Option<String>,
        /// Return tapes-events-selection/4 per-session call counts and partial-read
        /// status without event records. Explicit selections use this schema with
        /// nested tapes-events/16 objects when --count is absent.
        #[arg(long)]
        count: bool,
        /// Match the recorded tool call identifier exactly. Repeatable.
        #[arg(long, value_name = "ID")]
        call_id: Vec<String>,
        /// Match a declared or structured nested program exactly. Repeatable;
        /// this never aliases the recorded outer tool name.
        #[arg(long, value_name = "PROGRAM")]
        program: Vec<String>,
        /// Stream the whole recording instead of its bounded tail, writing
        /// each event as it is paired so memory follows the calls awaiting a
        /// result rather than the file; with --json the same tapes-events/16
        /// object is written event by event. The recording is read twice, the
        /// second read replaying the first; --tail, --name, --call-id, and
        /// --program, --tool, and --command select as they do over a bounded read. Installed Claude,
        /// Codex, and Pi sessions; OpenCode refuses, since its messages are
        /// updated in place and a second read may not repeat the first.
        #[arg(long, conflicts_with = "read_bytes")]
        full: bool,
        /// Render the versioned tapes-events/16 object as JSON. `turn_ordinal`
        /// names the source turn at the same coordinate printed by show;
        /// `ordinal` retains that coordinate for both call and result. Completed Codex
        /// CommandExecution events include integer `exit` when recorded; other
        /// events omit it without structured process status. Each read call
        /// includes its recorded path and range, outcome, and returned-text
        /// digest. Claude persisted-output results carry a reference-only file
        /// with a recorded size label and keep the preview as visible output.
        /// Optional content_sha256 names undecorated file bytes of the
        /// returned range only when the recorded format establishes them.
        /// Unknown formats and truncated output omit it. An unpaired event's `incomplete` is `no-result-in-read` when
        /// the read ended before a result, `call-before-read-bound` when a
        /// source bound can hide the call, or `call-not-recorded` when the read
        /// reached the recording's start and holds no call. A shell result
        /// carries `commits` when its call made or inspected any: each
        /// observation has `at`, `sha` (as printed or named), `subject` and
        /// `branch` when the output or the harness gave one, `created`,
        /// `found` (`git-operation`, `summary-line`, `chained-output` or
        /// `invocation`) and `call`; the result says `truncated` past 32
        /// observations and `created_unnamed` when a commit-creating command
        /// ran without failing and nothing names its commit. Inspections are
        /// the hex shas a command names as revisions, never shas a result
        /// merely prints. The top-level `commits` {kept, omitted,
        /// truncated_calls} appears when a cap dropped any. A result that changed
        /// files carries `edits`: observations with `path`, `op`, `added` and
        /// `removed` (the git reader's line digests, no line text), `source`
        /// {tool, shape} and `call`, `truncated` past 32 edits, `tree_movement`
        /// when a shell call moved the working tree and its recorded diff is no
        /// authored edit, `more_files`, and `unrecognized` when a shell call wrote
        /// files with no recorded diff. The top-level `edits` {kept, omitted,
        /// digests_omitted, truncated_calls, tree_movements, tree_movement_files,
        /// unrecognized} appears when any but `kept` is nonzero.
        #[arg(long)]
        json: bool,
        /// Publish each tool call's complete recorded argument text in
        /// `arguments.preview` instead of the 200-character prefix. Nothing
        /// else changes: `arguments.chars` still states the whole length, the
        /// object's members are the same, and so is its schema version. Apply
        /// it to a bounded read or to --full.
        #[arg(long)]
        full_arguments: bool,
    },
    /// Which sessions a recording names as its relatives: the session it was
    /// spawned from or forked from, and the children its own store records,
    /// each with the role, model, timestamps, and outcome the harness wrote.
    /// A relationship exists only where a record states it; nothing is
    /// inferred from directories, titles, or timestamps. A reference the
    /// store cannot resolve is kept and marked. A child is referred to, never
    /// absorbed: use `show` for an ordinary child session ID, or
    /// `child PARENT --reference CHILD` for a Claude subagent. A Claude
    /// subagent run in the background is stamped completed at the parent's
    /// latest task notification naming its `Agent` call, with the
    /// notification's status as its disposition and that record in `source`;
    /// a shell task's notification names no child. Where the parent's records
    /// state no end, because a plugin launched the subagent or the
    /// notification lies outside the read, the subagent is stamped completed
    /// from its own recording, citing its final record in `source`: whatever
    /// that record is when the subagent's meta record says it finished, and,
    /// for a plugin-launched one that takes no further input, an assistant
    /// record that stopped on `end_turn`. Known trailing
    /// housekeeping, including cost-state and system turn_duration/stop_hook_summary,
    /// is skipped; unknown records and read gaps do not establish completion. Otherwise
    /// it has no completion stamp, which does not say it is still running.
    /// Endings first inspect a 64 KiB tail and bounded opening, expanding to
    /// --read-bytes when no complete substantive tail record is available.
    /// Child counts enumerate transcript filenames without reading metadata.
    /// Child-ending opening and tail identity records must name the session
    /// that spawned the child, and a record that names an agent must name the
    /// child itself; missing or conflicting identity fails. A Claude session
    /// that continued another while a subagent ran keeps that subagent's
    /// recording as a link into the earlier session's subagent directory: the
    /// child is named with `carried_from`, the session it was spawned under,
    /// whose records it carries beside the later ones the continuing session
    /// wrote under its own id, and its consumption is that session's. A child
    /// whose records name a session its location does not explain fails.
    /// Only regular opened recording and
    /// metadata files supply evidence; FIFO reads never wait for a writer.
    /// Failed Claude subagent enumeration or a required child-ending read fails
    /// lineage with its source and I/O diagnostic. An absent subagent directory
    /// establishes no stored child transcripts. Codex candidate header probe
    /// failures appear in notes with their source and I/O diagnostic on bounded
    /// and full reads; an unreadable header does not establish absent relatives.
    Lineage {
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        read: ReadArgs,
        /// Stream the whole recording record by record instead of its bounded
        /// tail, naming every child reference and parent fact it holds, with
        /// memory following the references kept rather than the file.
        /// Installed Claude, Codex, Pi, and OpenCode sessions.
        #[arg(long, conflicts_with = "read_bytes")]
        full: bool,
        /// Render the versioned tapes-lineage/5 object as JSON.
        #[arg(long)]
        json: bool,
    },
    /// A payload-free per-turn timeline for one session or a selection.
    /// Uses the same selection, scope and read bounds as stats.
    /// Claude subagent count notes report failed enumeration instead of a count.
    Timeline {
        #[command(flatten)]
        query: SessionQueryArgs,
        #[command(flatten)]
        read: ReadArgs,
        /// Stream every turn of each selected recording instead of its bounded read.
        #[arg(long, conflicts_with = "read_bytes")]
        full: bool,
        /// Emit tapes-timeline/1: sessions containing records with only ordinal,
        /// kind and ts. Ordinals are the zero-based coordinates printed by show;
        /// missing timestamps are omitted. Human output is one line per turn.
        #[arg(long)]
        json: bool,
    },
    /// Count what one session's recording holds: turns by kind beside the
    /// kinds its harness can record at all, tool calls
    /// by name with their paired durations and error counts, the recorded
    /// clock, the session's own token counters with the share of
    /// `input + cache_read + cache_write` its cache accounts for, the model
    /// and effort split its harness recorded, and the
    /// children its store names. Every figure is a count of records the
    /// harness wrote, and every total says what it covers: turn coverage is
    /// `read-window` when a source bound withheld turns, durations come from
    /// complete pairs only, and a cache ratio is a share of recorded token
    /// counts rather than of cost. A `completeness:` line after the token
    /// and cost lines names each incomplete counter's reasons. A model
    /// response is one source record —
    /// the model's text, its reasoning, or a tool call it made — and its
    /// duration is the interval from the previous turn's source record to its
    /// own, so a record that normalizes to several turns is one duration and
    /// a tool-result record is none; the distribution over those intervals is
    /// `assistant_turns_ms` with its count, median, p90, and max. Nothing is
    /// judged, ranked, or explained.
    /// A scope or listing filter selects multiple sessions and returns
    /// tapes-stats-summary/8: recorded tools grouped by harness and name,
    /// with per-session read coverage, pairing counts and failures, and per
    /// harness the recordable kinds no session read held. Child turns are
    /// excluded from parent totals. Failed Claude subagent enumeration or a
    /// required child-ending read fails single-session stats; a selection
    /// retains the failure for that session.
    Stats {
        #[command(flatten)]
        query: SessionQueryArgs,
        /// Gaps above this duration separate working stretches. Active time sums
        /// nonnegative timestamp gaps at or below it within the read coverage.
        #[arg(long, default_value = "45m", value_parser = parse_idle)]
        idle: i64,
        #[command(flatten)]
        read: ReadArgs,
        #[command(flatten)]
        prices: PriceArgs,
        /// Stream the whole recording instead of its bounded tail, twice: the
        /// first read counts turns and observes tool events, and the second
        /// replays it to pair and count them, so memory follows the calls
        /// awaiting a result rather than the file. Counters and children are
        /// read from every record, so turn coverage is `session`. A selection
        /// streams each selected session the same way. Installed Claude,
        /// Codex, and Pi sessions; OpenCode refuses, since its messages are
        /// updated in place and a second read may not repeat the first.
        #[arg(long, conflicts_with = "read_bytes")]
        full: bool,
        /// Render the versioned tapes-stats/16 object, or tapes-stats-summary/8
        /// for a selection, as JSON. `warnings` names the limits of the read:
        /// `read-window`, `tail-window`, `kind-unknown`, `kind-undeclared` (a
        /// turn of a kind its harness declares it cannot record, so the
        /// declaration or the reader is wrong), `incomplete-pairs`, and
        /// `no-timestamps`. The median and p90 of `assistant_turns_ms` are the
        /// samples at rank ceil(count / 2) and ceil(count * 9 / 10). A
        /// selection none of whose sessions could be read still writes its
        /// report, then exits non-zero.
        #[arg(long)]
        json: bool,
    },
    /// Where quota went. A session named by id or reached with --latest
    /// answers that session as tapes-usage/17: its recorded tokens, cost, and
    /// turn counts, plus whatever else its harness recorded — a context
    /// window, a provider quota window, wall-clock durations, a per-model
    /// split, the price-relevant dimensions its requests recorded. --from,
    /// --to and --requests answer one span of that session in the same shape
    /// with a `span` object; spans and the remainder no span holds add up to
    /// the whole session within one accounting basis. --prices, TAPES_PRICES
    /// or the default price table add a `list_price_estimate` beside each
    /// harness-written cost, priced from the caller's table. --window and
    /// --quota-window answer a selection's consumption inside a window of
    /// time, each session contributing its own span inside it. A scope or
    /// listing filter instead answers the whole selection as
    /// tapes-usage-summary/16, grouped by --by and summing each counter over
    /// the sessions that recorded it. Native tokens follow each harness's own
    /// convention and add only within it; the disjoint categories (uncached
    /// input, cache read, cache write by TTL, output) add across harnesses.
    /// The accounting basis and coverage decide whether figures may be
    /// summed; cost is the harness's own estimate, and quota is a separate
    /// fact about the account rather than these sessions. Claude subagent
    /// count notes report failed directory enumeration instead of a count.
    /// Every session's `measurement_coverage` counts, per native counter,
    /// the events its sum covers by whether each recorded the counter: the
    /// requests of a request sum, the one event of a harness-written total,
    /// the models of a Claude cost-state total. A counter some events lack
    /// is a known subtotal, never complete, and `measurement_incomplete`
    /// says how many recorded it; a summary adds the events of its sessions.
    /// A pi session's counters select its active branch and count usage on
    /// assistant requests, tool results, compactions, branch summaries, and
    /// usage entries. Every session's
    /// `measurement_completeness` names each incomplete counter with fixed
    /// reasons (a stopped or failed request, missing counters or prices, an
    /// unreadable record, which for OpenCode is a database message or part row
    /// that cannot be read (data that does not decode, a missing id or integer
    /// `time_created`, a part naming no message), an unrecognized record that
    /// carries its harness's
    /// usage fields, a read window), and every derived figure follows
    /// it: disjoint categories and their total, model splits, summaries,
    /// identity bounds, brief and stats. Text output prints the reasons
    /// too: a `completeness:` line per incomplete figure, `known subtotal;
    /// <reasons>` on a disjoint category, `total withheld:` naming what
    /// withheld a disjoint total, and the reasons in brackets on a summary
    /// row's counter and COST cells. A pi session's `consumption`
    /// sums every retained branch, each consumption event once, with
    /// per-counter measurement coverage for known
    /// subtotals, event split, observed identity (provider account namespace
    /// unestablished), and incompleteness. Consumption cost is absent when no
    /// counted event is priced; `cost_coverage` qualifies missing and unpriced
    /// events alongside the per-counter measurement counts. Its cost carries
    /// the same estimate kind and missing pricing basis as every other cost.
    /// Zero-written consumption events count as observations; assistant
    /// messages establish request counts. Fork parents retain complete native
    /// ids, including dots and underscores,
    /// within a 128-byte bound; recording paths and timestamp-prefixed basenames
    /// stay private. Other bare references, such as parent.jsonl, are ids.
    /// Sessions a recording relates form overlap groups within a summary: one
    /// declares the other as its fork origin or parent (Codex `forked_from_id`,
    /// Pi's parent header, OpenCode `fork_session_id` and `parent_id`), both
    /// are forks of the same source, selected or not, or both recorded the
    /// same request id or message uuid, which is how a Claude fork shows. An
    /// overlap group of two with consumption event keys adds by their exact
    /// union over the sessions' active branches; otherwise the summary
    /// withholds its exact totals,
    /// keeps `session_figures`, and reports per-counter `identity_bounds`
    /// within compatible accounting partitions, from the largest member of
    /// each group to the sum. An unmeasured or partial counter has no upper
    /// bound. `overlap.related` names each group, its evidence, and whether
    /// its union is exact. Sessions no evidence relates are summed, and
    /// `overlap.disjointness` says that is inferred from absent copy evidence,
    /// not established, or from sampled copy evidence when a long recording's
    /// id check kept only a sample of its ids, with `overlap.sampled` naming
    /// those sessions. `--family` instead answers one session and everything
    /// it caused as tapes-family-usage/2: the session, its subagents, the
    /// sessions that name a member in a `tapes-parent` launch marker in any
    /// harness, and with `--evidence` the sessions an evidence file launches
    /// from a member, and with `--continuations` the session it continued.
    Usage {
        #[command(flatten)]
        query: SessionQueryArgs,
        #[command(flatten)]
        read: ReadArgs,
        /// Group the summed sessions by this dimension. Repeatable and
        /// comma-separated; groups are keyed in the order given
        /// [default: harness,model]. `model` is the model id and `variant`
        /// the reasoning effort or tier qualifying it, so a session that
        /// recorded no variant is grouped under that absence. `session`
        /// groups each session alone, its key naming it as `session` and
        /// `session_harness` so sessions sharing an id in different harnesses
        /// are different groups, and `launch` by the interactive or
        /// headless launch its recording states. `parent` groups a session
        /// under the end of the chain of parents its recordings name: a Codex
        /// spawn's `parent_thread_id`, a declared fork origin (Codex, pi,
        /// OpenCode), a Claude subagent's parent, or the launcher a first
        /// prompt names on its first line as `tapes-parent:
        /// <harness>:<session-id>` (Claude, Codex, pi, OpenCode; the guide
        /// gives the exact form). A session that names none is its own group, and a
        /// parent the store of its harness does not hold is a group of its
        /// own under that id, never joined to a guessed one. A group's key
        /// names its root as `parent` and `parent_harness`, so roots sharing
        /// an id in different harnesses are different groups, and the table
        /// writes a session or root id more than one harness holds as
        /// `<harness>:<id>`. A launched session is never an
        /// overlap group with its launcher. A change view groups its rows by
        /// `commit`, `merge`, `agent`, `role`, `tier` and `session` alone
        /// [default: commit,agent,role]; `merge` keys a commit by the
        /// first-parent merge of the default branch that brought it, or by
        /// itself where it lies on that history, as a fast-forward leaves it.
        /// `<SESSION> --by commit` divides one session across the
        /// repository's commits, and `--window FROM..TO --by commit` a
        /// window's consumption; a summary refuses the change dimensions.
        #[arg(long, value_enum, value_name = "DIMENSION", value_delimiter = ',')]
        by: Vec<ByArg>,
        /// Stream one session's whole recording instead of its bounded tail:
        /// turns are counted as they stream, and tokens, cost, context window,
        /// quota, and model are folded from every record. Turns and a complete
        /// request-observation read cover the session; recorded-total
        /// accounting keeps its own epoch coverage, including `since-reset`,
        /// and Codex attribution reports its observation coverage separately.
        /// Codex attribution counts every usage record, and
        /// `attribution.outside_total` counts the requests the recorded total
        /// never moved for (a remote compaction, or a request no token count
        /// followed), with `outside_total_tokens` their sum, so the shares
        /// less those are the recorded total's movement. pi's active-branch coverage stays `read-window` when an unreadable
        /// record cuts the branch or follows its last entry, and its
        /// consumption covers the session with any unreadable record named
        /// under `incomplete`. Installed Claude, Codex, Pi, and OpenCode
        /// sessions; a selection refuses it.
        #[arg(long, conflicts_with_all = ["read_bytes", "by"])]
        full: bool,
        /// Retain a bounded recent suffix of Codex accounting observations.
        /// With no value, 200 rows are retained; the accepted range is 1..=10000.
        /// Rows are raw token observations, not guaranteed request identities,
        /// each with its own request counters restated as `disjoint`;
        /// source gaps are retained and counted once per distinct gap. Rows
        /// dropped by the row cap, the 8 MiB serialized budget, or their own
        /// size are counted under `omissions`; aggregates and attribution still
        /// cover the whole read.
        #[arg(
            long,
            value_name = "N",
            num_args = 0..=1,
            default_missing_value = "200",
            conflicts_with = "read_bytes"
        )]
        series: Option<Option<usize>>,
        /// Reconcile one Claude session's recorded total with its request
        /// observations, reading the parent and subagent recordings whole.
        /// The newest cost-state retains its tokens and dollar facts under
        /// recorded-total/session accounting. Requests deduplicate across a
        /// validated native family: children under the parent's subagents
        /// directory must positively witness matching native parent identity
        /// and name no agent but their own. A subagent carried over from the
        /// session that spawned it, which this one continued, belongs to that
        /// session's family and is named in a note here.
        /// Per-recording figures remain visible. Foreign, unvalidated or
        /// unenumerated children, and sources outside the family, withhold
        /// request-namespace and leave the union unestablished. Failed
        /// assistant records are unmeasured even without a usage object.
        /// Comparable counters retain signed recorded-minus-identified
        /// differences: zero reconciles, a negative value conflicts, and a
        /// surplus or incomplete measurement is a discrepancy. Resets,
        /// unresolved witnesses, missing counters or an unmatched frontier
        /// withhold differences. Child requests are ordered by time against
        /// the total's timestamp or, for a cost-state, which writes none, the
        /// newest parent record written before it; untimed or later child
        /// observations cannot contradict an earlier total. `unexplained`
        /// is measured against the family's requests.
        /// Each source names the revision, length and SHA-256 of its bytes.
        /// A source that could not be used carries `error` and an
        /// unenumerated family a note, each a session id and a fixed reason
        /// (foreign-identity, no-identity, source-unreadable-or-changed,
        /// subagents-unenumerated), never the underlying error text.
        #[arg(long, conflicts_with = "by")]
        reconcile: bool,
        /// Price one session with everything it caused, as
        /// tapes-family-usage/2: the session, its Claude subagents and Codex
        /// spawns, every session in any harness whose first prompt opens
        /// with a `tapes-parent: <harness>:<session-id>` line naming a member
        /// (recursively), and sessions that declare a member as their fork
        /// origin, and with --evidence the children an evidence file's lineage
        /// records launch from a member, each counted once by harness and id. A Claude request
        /// recorded under one id in a parent's and its subagent's recordings
        /// counts once. Rows per agent (`harness:model#effort`) and role
        /// (`lead`, `subagent`, `delegate`, `fork`), one row per session, and
        /// totals per accounting basis, priced with --prices; the part of a
        /// Claude recorded total no request of the family explains is
        /// reported apart as `unexplained`. Launched sessions are found by
        /// time: every session of every harness active since the family
        /// opened is listed, and only those that began inside a member's
        /// recorded interval have their opening records read for the marker.
        /// `search` reports what was listed and examined and what left it
        /// incomplete (listing bound, unavailable store, unreadable opening,
        /// member bound); a launched session that began outside every
        /// member's interval is not found by the search, though an evidence
        /// file that names it finds it by id. A member whose recording could not
        /// be read, or states no consumption, is listed under `withheld`
        /// with a fixed reason. Names one installed session by id, --latest
        /// or --title.
        #[arg(
            long,
            conflicts_with_all = ["by", "series", "reconcile", "event_keys", "from", "to", "requests", "window", "quota_window", "forecast", "allowance_rates", "full"]
        )]
        family: bool,
        /// With --family, follow a continued Claude session back to the
        /// session it carried over from, and add that session's family. A
        /// continuing recording opens with a `compact_boundary` record whose
        /// `logicalParentUuid` is the `uuid` of a record in the earlier
        /// recording, which no record names by session id; the link holds
        /// when exactly one recording beside the continuing one holds that
        /// record as its own, and otherwise `continuations` reports it as
        /// not-found or ambiguous and nothing is linked.
        #[arg(long, requires = "family")]
        continuations: bool,
        /// With --family, read a `tapes-attribution-evidence/1` file, or
        /// every `*.jsonl` file directly inside a directory, and add the
        /// sessions its `lineage` records launch from a member. Repeatable.
        /// A child is looked up by harness and id in its harness's store,
        /// wherever its recording began, and joins as a `delegate` with link
        /// `evidence`, naming the file, producer, tier and the record's label
        /// and subject. A session whose own recording names a parent (a
        /// subagent directory, a spawn, a fork origin, a `tapes-parent`
        /// marker) is placed by its recording; a file fills only what
        /// recordings leave silent. Claims naming different parents for one
        /// session are reported under `evidence.conflicts` and the session
        /// counts once, under its recording's parent or else the first claimed
        /// parent that became a member. A child no installed store holds is
        /// listed under `evidence.unresolved` and adds nothing. `binding`
        /// records are read and counted, and nothing uses them yet. Nothing
        /// is read unless named: no default directory, no environment
        /// variable. A missing path, and a file whose first line is not a
        /// header of the schema, refuse. Malformed lines are reported with
        /// their line numbers and a fixed reason, never their text.
        #[arg(long = "evidence", value_name = "PATH", requires = "family")]
        evidence: Vec<PathBuf>,
        /// Retain a recent suffix of Pi consumption event keys. Default: 200;
        /// range: 1..=10000; serialized key budget: 8 MiB. Omitted identities
        /// are counted, while measurements cover the whole read. Provider
        /// account equality is unestablished. Recording coordinates are local;
        /// copy_of verifies a matching consumption entry in the named parent.
        /// Keys contain ids and digests, without recording paths or diagnostics,
        /// and cover every retained branch (selection: retained-branches).
        /// Copy coverage and conflicts qualify cross-recording equality.
        /// Unavailable key sources retain measurements and use a fixed reason.
        /// Each key carries its event's own measurement: disjoint token
        /// categories, cost with kind and coverage, and the reasons any is
        /// incomplete. A measurement is omitted with its key, carries no
        /// locator, and an unrecorded amount is absent, never zero. A union of
        /// two selections is exact only when every key of both is present and
        /// measured, no record carries usage outside the keys
        /// (unrecognized_usage), and the copies are verified; otherwise it is
        /// withheld and bounded. A partial price bounds only from below.
        #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "200", conflicts_with_all = ["read_bytes", "series"])]
        event_keys: Option<Option<usize>>,
        /// Answer the span of one session from this RFC 3339 instant: the
        /// counted requests at or after it and before --to, either bound
        /// open. The whole recording is streamed and each request placed by
        /// its own timestamp: a Claude request by its first record, a Codex
        /// request by its token count or, where the cumulative total never
        /// moved for it, by its usage record, a pi consumption event by its
        /// entry, an OpenCode assistant message by its creation time. The answer has
        /// the whole-session shape, its tokens, disjoint categories, cost,
        /// measurement coverage and completeness covering only the span, and
        /// adds `span`: the requested bounds, the `first` and `last` request
        /// placed (ordinal, timestamp, and the harness's request id where it
        /// records one), how many `requests` it placed, and the session's
        /// `session_requests`. Spans conserve: the span before an instant
        /// and the span from it add up to the whole session per counter,
        /// within one accounting basis, and requests no span holds form the
        /// remainder. A Codex span sums its requests (`summed-requests`): each
        /// token count's move of the cumulative total and the own usage of
        /// each request the total never moved for (a remote compaction), so
        /// its spans add up to the thread's total, `since-reset` where a
        /// counter restarted, while `usage --full` keeps the recorded total.
        /// A Claude cost-state or an OpenCode session
        /// total covers more than any span, so their spans sum requests and
        /// the recorded total is withheld with a note. A Claude span carries
        /// apart its share of the part of the newest total no request
        /// explains (`unexplained`): each cost-state's increase falls between
        /// the newest timestamped records before it and before the previous
        /// one, held whole by a span holding that interval (`bounded`) and
        /// prorated by the recording's records between the two totals the
        /// span holds where it cuts it (`estimated`, with its basis); an increase no record bounds, or any for a --requests
        /// span, stays with the remainder and is named `unplaced`. A pi span keeps the
        /// active-branch counters and the retained-branch `consumption`
        /// apart. A request
        /// without a usable timestamp is held by the remainder, and a record
        /// that cannot be placed for or against the span (an unreadable
        /// record, a failed attempt or unrecognized usage between requests)
        /// is counted under `unplaced`; each qualifies the span's
        /// completeness (unestablished-scope, unreadable-record,
        /// stopped-attempt, unrecognized-usage). Turn counts describe the
        /// whole read. A selection refuses span flags.
        #[arg(
            long,
            value_name = "RFC3339",
            value_parser = parse_instant,
            conflicts_with_all = ["by", "read_bytes", "series", "reconcile", "event_keys"]
        )]
        from: Option<Instant>,
        /// End the span before this RFC 3339 instant; see --from.
        #[arg(
            long,
            value_name = "RFC3339",
            value_parser = parse_instant,
            conflicts_with_all = ["by", "read_bytes", "series", "reconcile", "event_keys"]
        )]
        to: Option<Instant>,
        /// Answer the span of one session's counted requests FIRST..LAST,
        /// 1-based and inclusive in recording order, either end open (`3..`,
        /// `..5`). Counted requests are the ones the whole-session figure
        /// sums: Claude request ids, Codex token counts that advance the
        /// cumulative total and usage records it never moved for, pi
        /// consumption events counted once, OpenCode
        /// assistant messages. The answer is the one --from describes.
        #[arg(
            long,
            value_name = "FIRST..LAST",
            value_parser = parse_request_range,
            conflicts_with_all = ["by", "read_bytes", "series", "reconcile", "event_keys", "from", "to"]
        )]
        requests: Option<SpanBounds>,
        /// Answer the consumption inside a window of time, FROM..TO in RFC
        /// 3339 with either end open, over the sessions the scope and filters
        /// select. --since and --until select sessions by their newest
        /// activity; --window cuts consumption at its edges: every selected
        /// session with activity inside the window contributes only the span
        /// of its own requests inside it (the span `usage --from/--to` reads),
        /// read whole, so a session that straddles an edge contributes what it
        /// consumed inside and nothing else. A Claude parent's subagent
        /// recordings contribute their own spans as `<parent>::<agent>`, a
        /// subagent a continued session carries over contributing once, under
        /// the session that spawned it, a
        /// parent whose own records end before the window reached through
        /// subagent recordings written since it opened and since --since,
        /// kept under --since only when it or a subagent has recorded
        /// activity since it, under the other filters, --search over its own turns included,
        /// and beyond --limit. Shares sum to the window's figures under
        /// the summary's partitions and withholding, and a request a span
        /// cannot place qualifies its share as a span does. A Claude parent's
        /// share places the part of its recorded total its family's requests
        /// do not explain as a span places it (`unexplained`), and groups and
        /// the total add those parts apart from their tokens. Related sessions
        /// are never added by an exact union. `window` names the edges, how
        /// they were established
        /// (`requested`, `recorded`, `inferred`), and each session's share,
        /// largest first: its requests inside the window, its launch, its
        /// named `parent`, how its recording names it (`parent_evidence`:
        /// `subagent`, `spawn`, `fork-origin` or `declared-launch-parent`),
        /// the `root` its chain of parents ends at and that root's
        /// `root_harness`, and its disjoint categories. --limit still bounds how many sessions each
        /// harness lists. Each share, group and total also carries
        /// `allowance`: the percent of each quota window it spent, apportioned
        /// over the disjoint tokens each session spent between consecutive
        /// observations of the pool's used percent, among the sessions of the
        /// pool's own account and harness, an estimate with bounds for the
        /// observations' whole-percent rounding. A group or total holds each
        /// session once, by its harness and id, so each rise is apportioned
        /// once whatever the grouping; `window.allowance`
        /// shows each pool's observed rise and how it divides. Claude records
        /// no used percent, so its shares say so instead.
        #[arg(
            long,
            value_name = "FROM..TO",
            value_parser = parse_window,
            conflicts_with_all = ["session", "latest", "title", "occurrence", "full", "from", "to", "requests", "series", "reconcile", "event_keys"]
        )]
        window: Option<WindowBounds>,
        /// Answer the quota window holding an instant (`current`, or an RFC
        /// 3339 instant) as --window does, its edges taken from the
        /// recordings' own quota observations; requires --harness codex or
        /// --harness claude. Codex records each `rate_limits` window's reset
        /// and length, so its window is `recorded`. Claude records a limit
        /// only on a request it refused (`quotaLimits`), naming the limit and
        /// its reset but not its length, so its window, five hours or seven
        /// days by that name, is `inferred`. `current` takes the newest
        /// observation's window, or, once that window has reset, the one
        /// after it for the same length, which is `inferred`; an instant no
        /// observation's window or the one after it holds refuses, and the
        /// window is then read with --window. `window.quota` names the
        /// observation and the session whose recording holds it. Like
        /// --window it answers a selection: a session named by id,
        /// --latest or --title refuses it.
        #[arg(
            long,
            value_name = "current|RFC3339",
            value_parser = parse_quota_at,
            conflicts_with_all = ["window", "session", "latest", "title", "occurrence", "full", "from", "to", "requests", "series", "reconcile", "event_keys"],
            requires = "harness",
            group = "quota_reader"
        )]
        quota_window: Option<QuotaAt>,
        /// Which quota window --quota-window, --allowance-rates or --forecast
        /// reads: `primary`, Codex's short window and Claude's five-hour
        /// limit, or `secondary`, Codex's long window and Claude's seven-day
        /// limit.
        #[arg(long, value_enum, default_value = "primary", requires = "quota_reader")]
        quota: QuotaArg,
        /// Report allowance rates per lane instead of a summary: every
        /// completed session (last activity at least fifteen minutes before
        /// the read) with requests inside the lookback is a lane, its percent
        /// of the --quota window apportioned as a window read apportions it,
        /// grouped by --by (`model,variant` reads cost per model and effort)
        /// and by the `plan`, `limit` and `window_minutes` of the pools it
        /// was measured in, each group naming them, since a percent is of one
        /// capacity. The lookback is --window, else --since..--until. Each
        /// group gives
        /// lanes, percent per lane (median, p90 by nearest rank, max) over the
        /// lanes' estimates and their bounds, tokens per one percent by
        /// disjoint category (the tokens an interval of the lanes' pools
        /// holds over the percent apportioned on them: the estimate, and
        /// bounds from the percent's bounds), `uncovered_requests` (requests
        /// no interval held, on neither side of the rate), the median minutes
        /// from a lane's first to its last
        /// request, and `measured_over`, the first and last request measured,
        /// because rates drift with models and pricing. Percent per lane is
        /// given twice, each naming its `estimator`: `percent_per_lane` is
        /// `interval-apportioned`, the attribution a window read gives, and
        /// `smoothed_percent_per_lane` is `period-smoothed`, the lane's share
        /// of each window period's tokens times the period's rise, with
        /// bounds from the rounding of the observations that bound it, a
        /// steadier lane cost where whole-percent steps land on whichever
        /// lanes spent in them. A session either estimator gives a percent is
        /// a lane of both, so both sum to the same group total.
        /// `tokens_per_lane` gives the lanes' tokens. Claude lanes are left out with the reason: Claude records no used
        /// percent. Lanes are read from up to 10000 sessions per harness, or
        /// --limit's, taken in --sort order and then from the parents reached
        /// only through their subagents that --since keeps; `lanes_read`
        /// names that limit, the sessions it left out, and whether the
        /// listing reached it.
        #[arg(
            long,
            conflicts_with_all = ["session", "latest", "title", "occurrence", "full", "from", "to", "requests", "series", "reconcile", "event_keys", "quota_window", "prices"],
            group = "quota_reader"
        )]
        allowance_rates: bool,
        /// Forecast a planned lane mix against the --quota window of
        /// --harness, as tapes-allowance-forecast/2: MODEL[#VARIANT]=LANES,
        /// comma-separated (`gpt-6.1-sol#xhigh=2,gpt-6.1-sol#low=4`). The
        /// rates of --allowance-rates grouped by model and variant, over
        /// --window, else --since..--until, else the last 14 days, size each
        /// lane as its group's median lane tokens over the group's tokens
        /// per one percent, carrying the ratio's bounds. The window is the
        /// one the pools' newest observation is of, and its length is named
        /// and never assumed: only lanes measured in pools of its plan, its
        /// limit and its length size the plan. The newest observation gives the remaining
        /// percent (100 less its used percent, bounded by its rounding, or
        /// the whole window once its reported reset has passed) and the
        /// newest period's reset. The answer gives the mix's percent, whether
        /// it fits what is left (`yes`, `maybe`, `no`), the percent per
        /// minute with --concurrency lanes running (each lane's percent over
        /// its group's median duration), the minutes until the window is
        /// spent at that pace, and whether that falls before the reset. A
        /// bound the measurements leave open is null. A model and variant
        /// with no measured lanes refuses, naming it; a Claude forecast
        /// refuses, since Claude records no used percent. `lanes_read` names
        /// the sessions the rates were read from, as --allowance-rates does.
        #[arg(
            long,
            value_name = "MODEL[#VARIANT]=LANES,...",
            value_parser = parse_forecast,
            conflicts_with_all = ["session", "latest", "title", "occurrence", "full", "from", "to", "requests", "series", "reconcile", "event_keys", "quota_window", "prices", "allowance_rates"],
            requires = "harness",
            group = "quota_reader"
        )]
        forecast: Option<ForecastPlan>,
        /// How many of the forecast's lanes run at once [default: all of
        /// them].
        #[arg(long, value_name = "N", requires = "forecast", value_parser = clap::value_parser!(u64).range(1..))]
        concurrency: Option<u64>,
        #[command(flatten)]
        change: Box<ChangeAsk>,
        /// Price the answer with this tapes-price-table/1 or /2 file (the
        /// guide gives both formats): each figure gains `list_price_estimate`
        /// beside any cost its harness wrote, never in its place. A /2 row may
        /// carry a long-context rate for the whole request above a prompt
        /// size, for the billing it names (per-token API billing unless it
        /// says otherwise), UTC time-of-day rates, limited to days of the
        /// week where a window names them, and an
        /// effective_from..effective_until window; those terms are priced
        /// request by request from the requests the read kept, and between
        /// their rates, with request-terms-unknown, where the figure's
        /// requests are not known one by one. The estimate's `billing` names
        /// the billing each recording establishes and its evidence, a span
        /// or window share billed as its whole recording, unestablished
        /// where the whole recording establishes none, and a bounded read
        /// that leaves part of its recording unread unestablished, its
        /// evidence naming what the read window shows, or that it shows
        /// none: a subscription
        /// pays no surcharge, and unestablished billing is priced between
        /// base and surcharged rates with billing-unestablished. A request no
        /// row is in effect for, or usage of no known time a row may not
        /// cover, is outside-validity. Without the flag the table is the file
        /// TAPES_PRICES names, then $XDG_CONFIG_HOME/tapes/prices.toml (or
        /// ~/.config/tapes/prices.toml). A table named here or by
        /// TAPES_PRICES that is missing or invalid refuses; an invalid default
        /// table is reported on stderr and leaves the estimate absent, as no
        /// table does. tapes holds no prices of its own. Each served model's
        /// share is priced by its own row; a figure that names one model is
        /// that model's share whole, and one that names several, or whose
        /// uncertain attribution leaves part of it out of its model shares,
        /// is priced share by share from its recorded split; pi consumption
        /// is split by the model each event names, priced as the session
        /// figure is on the active branch alone and by that split with
        /// events on other branches, and every consumption event by the
        /// request at its own time. The
        /// estimate carries
        /// `kind: list-price-estimate`, the currency, an `amount` (or
        /// `at_least` and `at_most` where cache writes recorded no TTL), the
        /// table's schema, each row it used by `id`, `effective` date and
        /// `source`, and `measurement_completeness` over the priced
        /// categories (uncached_input, cache_read, cache_write, output): a
        /// counter the figure measured incompletely keeps its reasons, and
        /// unlisted-model, incomplete-attribution, missing-price,
        /// ttl-unrecorded, request-terms-unknown, billing-unestablished and
        /// outside-validity name what the table or the split could not price. It
        /// is absent when nothing could be priced. A summary adds estimates
        /// where its disjoint categories add and names why it withholds them
        /// under `list_price_withheld` (mixed-accounting,
        /// identity-unestablished, exact-union).
        #[arg(long, value_name = "FILE")]
        prices: Option<PathBuf>,
        /// Render the versioned tapes-usage/17 object, tapes-usage-summary/16
        /// for a selection, tapes-change-usage/1 for a change view, or
        /// tapes-family-usage/2 under --family, as JSON. `session.model_observation` says
        /// whether more than one model and effort was observed (`mixed`) and
        /// whether a gap, partial read or budget kept the split incomplete
        /// (`attribution_uncertain`); the session model is then only the latest
        /// selection, not the owner of the whole total. `disjoint` restates
        /// `tokens` as `uncached_input`, `cache_read`, `cache_write`
        /// (`ephemeral_5m`, `ephemeral_1h`, `ttl_unrecorded`), and `output`,
        /// with `reasoning` a share of output, under the native `convention`;
        /// `total` needs all four, and `missing` names what the counters do
        /// not determine. `requests` tallies Claude requests' recorded cache
        /// TTLs, service tier, speed, inference geo, and web search and fetch
        /// counts, with `missing` counting requests per unrecorded dimension.
        /// Compatible observations retain a dimension when a later record
        /// omits it. TTL splits must match the selected cache-write count;
        /// conflicting values are withheld and counted under `conflicting`.
        /// A Claude cost-state total, and each model's share of it, splits
        /// the cache writes its requests explain by the TTLs they recorded,
        /// wherever those writes are no more than its own, and holds the rest
        /// `ttl_unrecorded`. `unexplained`, beside `by_model`, names per model
        /// the part of the total its requests do not explain, kept apart from
        /// every figure summed from requests: `explained_by` is
        /// `session-requests`, or `family-requests` under --reconcile and in a
        /// window read, where subagent requests explain it too; a span or
        /// window figure adds its `placement`, `unplaced` and
        /// `unplaced_reasons`, and a summary adds its shares' parts.
        /// At most 16 TTL-count witnesses are held per request, with excess
        /// observations counted under `omitted_values`.
        /// A newest Claude cost-state below its request sum is set aside:
        /// accounting notes name the total and reason, while recorded_total
        /// retains its counters and dollar facts under its own accounting.
        /// Synthetic assistant records do not count as requests, and neither
        /// does a Claude record with no request id and every usage counter
        /// zero (a mirror row of work done elsewhere); a recording of only
        /// those notes it holds no consumption of its own. Positive failure
        /// evidence is counted separately with consumption unknown.
        /// Incoming session reports and direct aggregation inputs classify
        /// costs against their tokens. Session JSON retains cost_coverage.
        /// A written zero beside nonzero tokens is unpriced: cost is absent,
        /// cost_coverage is incomplete and counts zero-written requests when
        /// request evidence exists, otherwise cumulative observations.
        /// A mixed priced/unpriced sum is a known priced subtotal.
        /// A `cost` carries `kind: harness-calculated-estimate` and names
        /// `pricing_basis` missing. A summary keeps one `partitions` row per
        /// harness and accounting basis and coverage; a group or total whose
        /// counters span more than one of them sets `mixed_accounting: true`
        /// and omits `tokens` and `cost` rather than add incomparable
        /// figures. Its `disjoint` sums across harnesses, naming each
        /// `conventions` entry and counting `missing` sessions per category,
        /// and is omitted only where accounting basis or coverage differ.
        /// `accounting.selection`
        /// is `active-branch` where the counters sum one of several
        /// conversation paths; `consumption` then carries the
        /// `retained-branches` figure beside them.
        #[arg(long)]
        json: bool,
    },
    /// What a continuation of one session needs from its recording, as
    /// tapes-brief/14: where the work stopped, the working directory and the
    /// commit it sits on, the calls the read never saw a result for, the
    /// children whose outcome the store does not record, and a bounded tail
    /// of the exchange. It reads the recording alone and judges nothing —
    /// what a project's journal records about the same work is the other half
    /// of a continuation, and the caller joins them. Failed Claude subagent
    /// enumeration or a required child-ending read is reported as unavailable
    /// lineage, with the diagnostic; an empty child list proves nothing then.
    Brief {
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        read: ReadArgs,
        #[command(flatten)]
        prices: PriceArgs,
        /// Render this many of the session's newest operator and assistant
        /// turns, each cut at 600 characters. Every other kind of turn stays
        /// out of the tail.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_BRIEF_TAIL)]
        tail: usize,
        /// Render the versioned tapes-brief/14 object as JSON. Each `in_flight`
        /// list holds at most 20 entries, with a note when the bound cut it.
        #[arg(long)]
        json: bool,
    },
    /// Freeze one session's working set and plan its lanes, writing
    /// DIR/workset.json as tapes-workset/1 and one lane file per lane,
    /// DIR/lanes/<lane-id>.md. A DIR that already holds workset.json or
    /// lanes/ is refused and left as it is. The root session and the
    /// relatives its own whole-recording lineage resolves (parent,
    /// forked-from session, children; one level, never a parent's parent or
    /// a child's children) are each read in full, one at a time, and cut into
    /// lanes: contiguous ranges of turn ordinals sized by the lane projection,
    /// the per-turn rendering a lane carries. The projection caps each turn's
    /// text at 16KiB for operator and assistant turns and 2KiB for every
    /// other kind, marking what it left out; lanes count the omitted bytes by
    /// kind and coverage totals them. A lane holds as many turns as fit in
    /// --lane-bytes and is never cut between turns of one source record, so
    /// file-byte ranges tile; a record or turn whose capped rendering alone
    /// exceeds the bound gets a lane of its own. A lane file holds a header naming the lane, its source and its
    /// turn range; a marked margin of read-only context, the last two
    /// operator turns earlier in the same source each with its first
    /// assistant reply (saying so when no operator turn precedes the lane);
    /// and a marked lane section whose body is exactly the lane's projected
    /// turns, `projected_bytes` long. Only lane-section turns are cited.
    /// A turn text line beginning with `## `, `=== `, `[… `, `tool: `,
    /// `artifact: ` or a backslash gains a leading backslash, so transcript
    /// text cannot forge a turn heading, a section marker, an omission
    /// marker, or a tool or artifact line.
    /// Each source is keyed `s0` (the root), `s1`, ...; a turn is cited
    /// `<key>#<ordinal>`, and a lane is `<key>-<n>` with its `file` relative
    /// to DIR. Each source records its
    /// locator, source length, revision and prefix digest where the read
    /// establishes them, a content digest (SHA-256 over each turn's whole
    /// rendering, with the turn count it covers), live state when the session-status authority reports
    /// one, its lanes with the file-byte range each covers where every turn
    /// carries a record span, and coverage: turns by kind, records decoded
    /// and skipped, bytes no turn covers, compaction boundaries and injected
    /// notices, unresolved lineage references, and failed child enumeration
    /// with its diagnostic. A figure the source cannot supply is omitted. A
    /// relative that cannot be read is listed with its diagnostic and no
    /// lanes or lane files; only an unreadable root fails. Installed
    /// recordings only.
    Workset {
        #[command(flatten)]
        selection: SelectionArgs,
        /// Directory to write workset.json and lanes/ into; created when
        /// missing, refused when it already holds either.
        #[arg(long, value_name = "DIR")]
        bundle: PathBuf,
        /// Upper bound on one lane's projected bytes, from 8KiB to 4MiB.
        #[arg(
            long,
            value_name = "BYTES",
            default_value_t = ByteSize::new(agent_tapes_core::workset::DEFAULT_LANE_BYTES)
        )]
        lane_bytes: ByteSize,
    },
    /// Check a frozen workset against its sources and resolve the workset
    /// references cited from its lanes, as tapes-locate/1. Each source is
    /// re-opened the way the workset read it (a Claude child through its
    /// root) and replayed pinned to its frozen length, revision and prefix
    /// digest where it has a file, with its content digest recomputed over
    /// the frozen turns, one pass per source serving every reference into
    /// it. A source is `unchanged`; `grown` when content was appended after
    /// frozen content that still matches; `changed` when it is shorter, its
    /// frozen prefix or frozen content differs, or it has neither to
    /// compare; or `unreadable`, with its diagnostic. The workset is
    /// `stale` when any source is `changed`. With --refs, each line of FILE
    /// is a JSON object whose `ref` is `<key>#<ordinal>` and whose optional
    /// `lane` is a lane id; other fields are ignored. Each reference is
    /// `resolved` (kind, origin, timestamp, the lane holding it, and its
    /// record reference), `unknown` (no such key, or past the frozen turn
    /// count, even in a grown source), `outside-lane` (not in the cited lane;
    /// a margin turn always is), or `source-changed`; a blank, non-JSON or
    /// `ref`-less line is `malformed`, by line number. Exit 0 whenever the
    /// report is produced; a missing or unparseable workset.json or an
    /// unreadable refs file exits 1.
    Locate {
        /// The bundle directory `tapes workset` wrote.
        #[arg(long, value_name = "DIR")]
        workset: PathBuf,
        /// JSON lines of cited references; at most 100000, each line at most
        /// 64KiB.
        #[arg(long, value_name = "FILE")]
        refs: Option<PathBuf>,
        /// Return each resolved turn's whole projected text, without the
        /// lane projection's per-kind cap, up to this many bytes (at most
        /// 1MiB; 16MiB across the report). 0 returns no text.
        #[arg(long, value_name = "BYTES", default_value_t = ByteSize::new(0))]
        text_bytes: ByteSize,
        /// Render the versioned tapes-locate/1 object as JSON.
        #[arg(long)]
        json: bool,
    },
    /// What each session of a selection ends on, one bounded record each, as
    /// tapes-endings/10, including a session's recognized launch mode when one
    /// was recorded. The selection uses the flags `list` and `export` take,
    /// applied before any transcript is opened; each selected session then
    /// costs one bounded read of its newest turns and one lineage read. Every
    /// fact rests on the normalized turn kinds and typed tool events of the
    /// turns that were read, never on their text: an unanswered request, a
    /// harness command or notice recorded last, a call the read never saw a
    /// result for, results no turn narrates, a closing assistant turn. What
    /// the read left unestablished is named beside them, and `source` is the
    /// coordinate to write down when filing a follow-up. The report infers no
    /// reason for an ending and labels no session complete. Failed Claude
    /// subagent enumeration or a required child-ending read leaves lineage
    /// uninspected and retains its diagnostic.
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
        /// Keep sessions the harness records as interactive or headless.
        /// Pi records one only with `tapes install pi`, OpenCode with `tapes install opencode`, and
        /// sessions without a recognized marker never match.
        #[arg(long, value_enum, value_name = "MODE")]
        launch: Option<LaunchArg>,
        /// Match case-insensitively against the recorded directory path.
        /// Sessions without a directory never match.
        #[arg(long, value_name = "SUBSTRING")]
        directory: Option<String>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is at or after this timestamp. RFC 3339 timestamps with an offset
        /// and bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = agent_tapes_core::parse_activity_timestamp)]
        since: Option<agent_tapes_core::ActivityTimestamp>,
        /// Keep sessions whose newest recorded activity, `last_activity_at`,
        /// is before this timestamp. RFC 3339 timestamps with an offset and
        /// bare YYYY-MM-DD dates are accepted.
        #[arg(long, value_name = "TIMESTAMP", value_parser = agent_tapes_core::parse_activity_timestamp)]
        until: Option<agent_tapes_core::ActivityTimestamp>,
        /// Order the selection by `last_activity_at` before --limit takes from
        /// it: newest first by default, or oldest first.
        #[arg(long, value_enum, value_name = "ORDER", default_value_t = SortArg::Newest)]
        sort: SortArg,
        /// Match case-insensitively against the last 32 normalized turns in
        /// each candidate session. The fixed tail keeps the selection bounded;
        /// a match outside it is not considered.
        #[arg(long, value_name = "SUBSTRING")]
        search: Option<String>,
        #[command(flatten)]
        input: InputArgs,
        #[command(flatten)]
        read: ReadArgs,
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
        /// Render the versioned tapes-endings/10 object as JSON. An ending's
        /// `source.coverage` is `read-window` when a source bound withheld
        /// turns, `window` when only the --tail window omitted any, and
        /// `session` otherwise.
        #[arg(long)]
        json: bool,
    },
    /// Export sessions as bundles. One session by id or `--latest`, or every
    /// session a selection holds: the listing flags, including `--launch`,
    /// choose the same set
    /// `list` would return, in the same order, and a `manifest.json` beside
    /// the bundles records that selection and every bundle it produced. Each
    /// session keeps its own bundle, bounded unless --full reads it whole; a
    /// session whose store cannot be read is recorded in the manifest's
    /// `failed` and does not stop the run, which fails only when every
    /// selected session did.
    ///
    /// A bundle's files are fixed views of one read: `.context.md` holds the
    /// exchange, as `show --exchange` returns it; `.json` and `.trace.md` hold
    /// every turn, and `.json` also holds the paired tool `events`. --only,
    /// --omit, and --origin narrow all three; the JSON and trace count omitted
    /// turns by kind under `projection`. Claude subagent count notes report
    /// failed directory enumeration instead of a count.
    #[command(group(clap::ArgGroup::new("export_selection")
        .args(["session", "latest", "title", "occurrence", "input", "native_store", "here", "project", "global", "harness", "model", "launch", "directory", "since", "until", "search"])
        .required(true).multiple(true)))]
    Export {
        #[command(flatten)]
        query: SessionQueryArgs,
        #[command(flatten)]
        read: ReadArgs,
        /// Directory for the exported bundles and their manifest.
        #[arg(long)]
        bundle: Option<PathBuf>,
        #[command(flatten)]
        kinds: TurnKindArgs,
        /// Read each whole recording instead of its bounded tail. The
        /// recording is streamed twice, first writing the JSON turns and then,
        /// replaying the first read, pairing tool events and writing the
        /// Markdown files, so memory follows one record rather than the file.
        /// Installed Claude, Codex, and Pi recordings; OpenCode refuses it by
        /// name, as a bulk export records under `failed`.
        #[arg(long, conflicts_with = "read_bytes")]
        full: bool,
        /// Export from a named replica over ssh: the replica runs its own
        /// tapes with the same arguments and writes the bundles on its own
        /// filesystem, and this command prints their paths as
        /// `destination:path`, which `scp` and `rsync` accept. Every path in
        /// the arguments, `--bundle` included, names the replica's own
        /// filesystem. A replica that cannot answer refuses with the cause and
        /// the destination.
        #[arg(long, value_name = "SSH-DESTINATION", conflicts_with = "input")]
        remote: Option<String>,
        /// Beside each supplied conversation's bundle, copy the source bytes
        /// behind it into `<bundle>.evidence/`: the conversation record's own
        /// span (a standalone object or a collection record) and each associated
        /// report's whole member, byte for byte and
        /// named by their SHA-256, with a tapes-evidence/2 manifest naming
        /// the input's length and digest, each file's member, size, CRC, and
        /// span, whether the input could hold associated reports at all, and
        /// what could not be copied. Reports come out of the supplied
        /// directory or archive holding the conversation. Supplied inputs
        /// only.
        #[arg(long, conflicts_with = "full")]
        evidence: bool,
    },
    /// Install an integration that records what a harness leaves out of its
    /// own recordings. `pi` writes tapes' pi extension, which records the
    /// launch mode of every session pi opens afterwards, into
    /// `$PI_CODING_AGENT_DIR/extensions` (default `~/.pi/agent/extensions`).
    /// `opencode` writes a plugin into `$OPENCODE_CONFIG_DIR/plugins`, or
    /// `$XDG_CONFIG_HOME/opencode/plugins` (default `~/.config/opencode/plugins`).
    /// A file there that tapes does not manage is refused.
    Install {
        #[arg(value_enum)]
        harness: IntegrationArg,
    },
    /// Remove an integration `install` wrote, refusing a file tapes does not
    /// manage. Sessions already recorded keep their launch entries.
    Uninstall {
        #[arg(value_enum)]
        harness: IntegrationArg,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum IntegrationArg {
    Pi,
    Opencode,
}

fn main() -> Result<()> {
    reset_sigpipe();
    // `--version` names the reader build on a second line; its first line
    // stays the version alone.
    let long_version: &'static str = Box::leak(
        format!(
            "{}\nreader: {}",
            env!("CARGO_PKG_VERSION"),
            agent_tapes_core::reader::describe()
        )
        .into_boxed_str(),
    );
    let matches = Cli::command().long_version(long_version).get_matches();
    let result = dispatch(Cli::from_arg_matches(&matches)?);
    if let Err(error) = &result {
        if agent_tapes_core::transient::is_transient(error) {
            eprintln!("Error: {error:?}");
            std::process::exit(TRANSIENT_EXIT);
        }
    }
    result
}

/// The exit status of a read that failed for a reason that can pass,
/// sysexits' EX_TEMPFAIL.
const TRANSIENT_EXIT: i32 = 75;

fn render_locate(report: &agent_tapes_core::locate::Locate) -> String {
    use agent_tapes_core::projection::single_line;
    use std::fmt::Write as _;
    let mut out = String::new();
    for source in &report.sources {
        let _ = write!(out, "{} {}", source.key, source.state.label());
        if let Some(length) = source.source_length {
            let _ = write!(out, " length={length}");
        }
        if let Some(diagnostic) = &source.diagnostic {
            let _ = write!(out, ": {}", single_line(diagnostic));
        }
        out.push('\n');
    }
    if report.stale {
        out.push_str("stale: a source changed since the workset was frozen\n");
    }
    for (outcome, count) in &report.counts {
        let _ = writeln!(out, "{} {count}", outcome.label());
    }
    out
}

fn render_workset(workset: &agent_tapes_core::workset::Workset, path: &std::path::Path) -> String {
    use agent_tapes_core::projection::single_line;
    use std::fmt::Write as _;
    let mut out = format!("{}\n", path.display());
    for source in &workset.sources {
        let _ = write!(
            out,
            "{} {} {} {}",
            source.key,
            source.role_label(),
            source.harness,
            single_line(&source.session_id)
        );
        if let Some(reference) = &source.reference {
            let _ = write!(out, " ({})", single_line(reference));
        }
        match (&source.lanes, &source.coverage, &source.diagnostic) {
            (Some(lanes), Some(coverage), _) => {
                let _ = writeln!(out, ": {} turns in {} lanes", coverage.turns, lanes.len());
            }
            (_, _, Some(diagnostic)) => {
                let _ = writeln!(out, ": unreadable: {}", single_line(diagnostic));
            }
            _ => out.push('\n'),
        }
    }
    out
}

fn print_json<T: Serialize>(value: &T, input: &InputArgs) -> Result<()> {
    let body = serde_json::to_string(value)?;
    if input.supplied() && body.len() as u64 > input.output_bytes.get() {
        return Err(anyhow!(
            "serialized output exceeds --output-bytes limit of {} bytes",
            input.output_bytes.get()
        ));
    }
    println!("{body}");
    Ok(())
}

fn print_json_with_selection_warnings<T: Serialize>(
    value: &T,
    input: &InputArgs,
    warnings: &[String],
) -> Result<()> {
    if warnings.is_empty() {
        return print_json(value, input);
    }
    let mut value = serde_json::to_value(value)?;
    if let serde_json::Value::Object(object) = &mut value {
        object.insert(
            "selection_warnings".to_owned(),
            serde_json::to_value(warnings)?,
        );
    }
    print_json(&value, input)
}

fn print_history_pin(reads: &[agent_tapes_core::model::ReadEvidence]) {
    if let Some(digest) = reads
        .first()
        .and_then(|read| read.source_prefix_digest.as_ref())
    {
        let verified: u64 = reads
            .iter()
            .flat_map(|read| &read.ranges)
            .filter(|range| range.kind == agent_tapes_core::model::ReadRangeKind::Verification)
            .map(|range| range.span.end - range.span.start)
            .sum();
        println!(
            "Pinned {} bytes; verified {} bytes",
            digest.span.end, verified
        );
    }
}

fn print_selection_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("Warning: {warning}.");
    }
}

/// What a change view was asked for on the command line.
#[derive(Args, Debug)]
struct ChangeAsk {
    /// Price the commits these revisions name, as
    /// tapes-change-usage/1. Repeatable. A change view reads the
    /// repository holding the current directory, or the one --project
    /// names, read-only through git, and every installed Claude, Codex
    /// and Pi recording active from an hour before the earliest selected
    /// commit to half an hour after the latest; an OpenCode session takes
    /// part through its directory and time alone. Each session is divided
    /// over every commit its evidence admits, so what a session gives a
    /// commit does not depend on what else is selected, though a wider
    /// selection reads the sessions of a wider interval. The
    /// answer has rows per commit, agent (`harness:model#effort`) and
    /// role (`author`, `reviewer`, `proximate`), each with requests,
    /// native tokens by harness and model, disjoint categories, the part
    /// of a recorded total no request explains, and the same amounts by
    /// evidence tier and basis; the uncommitted buckets per worktree and
    /// branch; unattributed amounts by reason; `unselected`, the shares
    /// the same sessions' evidence gives commits not selected, reported
    /// apart and out of the rows' total; and totals, in which rows,
    /// uncommitted, unattributed and unselected add up to the sessions'
    /// wholes. A change view refuses a session selection other than
    /// `<SESSION> --by commit`, --family, --global, --window, span flags,
    /// --full, --series, --reconcile, --event-keys, allowance rates,
    /// forecasts, listing filters, --read-bytes and supplied inputs; an
    /// unknown revision refuses with git's message, and an empty
    /// selection refuses.
    #[arg(long = "commit", value_name = "REV", conflicts_with_all = ["range", "branch", "commits_from"])]
    commit: Vec<String>,
    /// Price the commits of a range, `A..B` or `A...B` as `git log` reads
    /// them, as --commit does.
    #[arg(long, value_name = "A..B", conflicts_with_all = ["branch", "commits_from"])]
    range: Option<String>,
    /// Price a branch's commits, as --commit does: a live branch's
    /// commits the default branch lacks, or, for a branch with none, the
    /// commits each merge whose subject names it (`Merge branch 'name'`,
    /// `Merge remote-tracking branch 'origin/name'`, `Merge pull request
    /// #n from owner/name`, or `merge(name): ...` for a branch named
    /// `name` or ending in `/name`) brought from its second parent. A
    /// branch that is neither refuses, naming --range and --commits-from.
    #[arg(long, value_name = "NAME", conflicts_with = "commits_from")]
    branch: Option<String>,
    /// Price the commits a file lists, or standard input for `-`, as
    /// --commit does: one revision per line, blank lines and lines
    /// starting with `#` ignored.
    #[arg(long, value_name = "FILE|-")]
    commits_from: Option<String>,
    /// List the evidence behind each change-view row and bucket: its
    /// session, kind (created, edited, worktree, proximity, inspected),
    /// tier, time or span, the commits it names, and how an observed sha
    /// resolved (exact, patch-id, digests) with the sha the recording
    /// named. Evidence carries shas, paths and times, never transcript
    /// text.
    #[arg(long)]
    explain: bool,
    /// The evidence tiers that take part in a change view,
    /// comma-separated from `bound`, `matched` and `estimated` [default:
    /// all three]. The named tiers take part together, never as a
    /// cascade, so a tier alone places what it alone would; what only
    /// the left-out tiers would place is unattributed as `tiers not
    /// asked`, naming them under `excluded_tiers`. `tiers` names the
    /// tiers that ran.
    #[arg(long, value_enum, value_name = "TIER", value_delimiter = ',')]
    tiers: Vec<TierArg>,
}

impl ChangeAsk {
    fn selects(&self) -> bool {
        !self.commit.is_empty()
            || self.range.is_some()
            || self.branch.is_some()
            || self.commits_from.is_some()
    }

    /// Whether the arguments ask for a change view: a change selector, a
    /// change view's own flag, or a dimension only a change view groups by.
    fn asked(&self, by: &[ByArg]) -> bool {
        self.selects()
            || self.explain
            || !self.tiers.is_empty()
            || by.iter().any(|by| by.change_only())
    }

    fn selection(
        &self,
    ) -> Result<Option<agent_tapes_core::change_usage::ChangeSelection<'static>>> {
        use agent_tapes_core::change_usage::ChangeSelection;
        if !self.commit.is_empty() {
            return Ok(Some(ChangeSelection::Commits(self.commit.clone())));
        }
        if let Some(range) = &self.range {
            return Ok(Some(ChangeSelection::Range(range.clone())));
        }
        if let Some(branch) = &self.branch {
            return Ok(Some(ChangeSelection::Branch(branch.clone())));
        }
        let Some(source) = &self.commits_from else {
            return Ok(None);
        };
        let text = if source == "-" {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).map_err(|error| {
                anyhow!("--commits-from - could not read standard input: {error}")
            })?;
            text
        } else {
            std::fs::read_to_string(source)
                .map_err(|error| anyhow!("--commits-from {source} could not be read: {error}"))?
        };
        let revisions = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
        let source = if source == "-" {
            "standard input".to_owned()
        } else {
            source.clone()
        };
        Ok(Some(ChangeSelection::List { source, revisions }))
    }
}

/// Answer a change view, refusing what does not combine with one.
#[allow(clippy::too_many_arguments)]
fn usage_change(
    query: &SessionQueryArgs,
    read: &ReadArgs,
    by: &[ByArg],
    change: ChangeAsk,
    refused: &[(&str, bool)],
    window: Option<WindowBounds>,
    prices: Option<&agent_tapes_core::pricing::PriceTable>,
    json: bool,
) -> Result<()> {
    use agent_tapes_core::change_usage::{change_usage, ChangeSelection, Request};
    let mut dimensions = Vec::new();
    for by in by {
        let Some(dimension) = by.change() else {
            return Err(anyhow!(
                "--by {} groups a summary of sessions; a change view groups its rows by commit, \
                 merge, agent, role, tier and session",
                serde_kebab(
                    &by.to_possible_value()
                        .map(|value| value.get_name().to_owned())
                )
            ));
        };
        if !dimensions.contains(&dimension) {
            dimensions.push(dimension);
        }
    }
    for (flag, present) in refused {
        if *present {
            return Err(anyhow!(
                "{flag} does not combine with a change view, which divides consumption among \
                 commits; drop {flag}, or drop the change view's flags"
            ));
        }
    }
    if query.scope.global {
        return Err(anyhow!(
            "a change view reads one repository: the one holding the current directory, or the \
             one --project <PATH> names; --global refuses it"
        ));
    }
    if query.input.supplied() {
        return Err(anyhow!(
            "a change view reads installed recordings whole; a supplied input refuses it"
        ));
    }
    if read.read_bytes.is_some() {
        return Err(anyhow!(
            "a change view reads recordings whole; --read-bytes refuses it"
        ));
    }
    for (flag, present) in [
        ("--harness", query.harness.is_some()),
        ("--limit", query.limit.is_some()),
        ("--model", query.model.is_some()),
        ("--launch", query.launch.is_some()),
        ("--directory", query.directory.is_some()),
        ("--since", query.since.is_some()),
        ("--until", query.until.is_some()),
        ("--sort", query.sort.is_some()),
        ("--search", query.search.is_some()),
    ] {
        if present {
            return Err(anyhow!(
                "a change view reads every session whose activity meets its commits' evidence \
                 windows; {flag} filters a listing and refuses it"
            ));
        }
    }
    let selection = match (change.selection()?, query.single(), window) {
        (Some(selection), None, None) => selection,
        (Some(_), Some(_), _) => {
            return Err(anyhow!(
                "--commit, --range, --branch and --commits-from select commits, and a session \
                 selection does not combine with them; `<SESSION> --by commit` divides one \
                 session across the repository's commits"
            ))
        }
        (Some(_), None, Some(_)) => {
            return Err(anyhow!(
                "--window does not combine with --commit, --range, --branch or --commits-from; \
                 `--window FROM..TO --by commit` divides a window's consumption across commits"
            ))
        }
        (None, Some(one), None) => ChangeSelection::Session(one),
        (None, None, Some(window)) => {
            let from = window.from.ok_or_else(|| {
                anyhow!("a change view of a window needs its start: --window FROM..[TO]")
            })?;
            ChangeSelection::Window {
                from,
                to: window.to,
            }
        }
        (None, Some(_), Some(_)) => {
            return Err(anyhow!(
                "--window answers a selection of sessions; it does not combine with a session id"
            ))
        }
        (None, None, None) => {
            return Err(anyhow!(
                "a change view needs commits (--commit, --range, --branch or --commits-from), a \
                 session (`<SESSION> --by commit`) or a window (`--window FROM..TO --by commit`)"
            ))
        }
    };
    let directory = match &query.scope.project {
        Some(project) => project.clone(),
        None => std::env::current_dir()?,
    };
    let reader = agent_tapes_core::git_reader::GitReader::at(&directory)?.ok_or_else(|| {
        anyhow!("git is not installed; a change view reads its repository through git")
    })?;
    if reader.toplevel()?.is_none() {
        return Err(anyhow!(
            "{} is not inside a Git repository: run the change view inside one, or name one with \
             --project <PATH>",
            directory.display()
        ));
    }
    let repository =
        agent_tapes_core::change_attribution::Repository::with_reader(reader, &directory)?;
    let tiers = if change.tiers.is_empty() {
        agent_tapes_core::change_attribution::Tiers::all()
    } else {
        agent_tapes_core::change_attribution::Tiers::of(
            change.tiers.iter().copied().map(Into::into),
        )
    };
    let usage = change_usage(
        &query.input.backends()?,
        &repository,
        Request {
            selection,
            by: dimensions,
            tiers,
            explain: change.explain,
            prices,
        },
    )?;
    if json {
        print_json(&usage, &query.input)
    } else {
        print!("{}", render_change_usage(&usage));
        Ok(())
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    let Some(mut command) = cli.command else {
        guide::print();
        return Ok(());
    };
    let remote = command.remote_destination().map(str::to_owned);
    if let Some(selector) = command.selector_mut() {
        apply_self(selector, remote.as_deref())?;
    }
    match command {
        Command::Capture {
            session,
            harness,
            json,
            input,
        } => {
            input.validate_collection()?;
            reject_installed_selection(&input, harness.as_deref(), &ScopeArgs::default())?;
            let backends = input.backends()?;
            let session = if session == self_token::TOKEN {
                let resolved = self_token::resolve(&input.backends()?)?;
                eprintln!("Note: {}.", resolved.report());
                resolved.id
            } else {
                session
            };
            let report = agent_tapes_core::capture_with_backends(
                &backends,
                id_selection(&session, harness.as_deref()),
            )?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "{} {} {} {}",
                    report.harness, report.session, report.store_path_class, report.state
                );
            }
        }
        Command::List {
            harness,
            scope,
            limit,
            model,
            launch,
            directory,
            since,
            until,
            sort,
            search,
            search_turns,
            full,
            search_matches,
            input,
            json,
            prices,
            remote,
        } => {
            input.validate_bulk()?;
            reject_installed_selection(&input, harness.as_deref(), &scope)?;
            if since
                .zip(until)
                .is_some_and(|(since, until)| since >= until)
            {
                return Err(anyhow!("--since must be earlier than --until"));
            }
            if remote.is_some() && input.native() {
                return Err(anyhow!("--native-store conflicts with --remote"));
            }
            if remote.is_some() && prices.prices.is_some() {
                return Err(anyhow!(
                    "--prices prices a local read; a replica's answer under --remote is not priced"
                ));
            }
            if let Some(destination) = remote.as_deref() {
                let args = remote_list_args(&RemoteListQuery {
                    harness: &harness,
                    scope: &scope,
                    limit,
                    model: &model,
                    launch,
                    directory: &directory,
                    since,
                    until,
                    sort,
                    search: &search,
                    search_turns,
                    full,
                    search_matches,
                });
                let result = remote_list(destination, &args);
                let (result, failure) = match result {
                    Ok(result) => (result, None),
                    Err(unavailable) => (
                        unreachable_list(sort, since, until, &unavailable),
                        Some(unavailable),
                    ),
                };
                if json {
                    print_json(&result, &input)?;
                } else {
                    println!("Replica: {destination}");
                    print_session_list(&result.sessions);
                    print_availability_note(&result);
                    print_full_search(&result);
                }
                if let Some(unavailable) = failure {
                    return Err(unavailable.into());
                }
                return Ok(());
            }
            if full {
                eprintln!("Searching whole recordings; --limit bounds sessions read after metadata filters.");
            } else if search.is_some() {
                eprintln!(
                    "Searching a bounded window of normalized turns in each candidate session before applying --limit."
                );
            }
            let filters = agent_tapes_core::ListFilters {
                model: model.as_deref(),
                launch: launch.map(Into::into),
                directory: directory.as_deref(),
                since,
                until,
                search: search.as_deref(),
                search_turns: Some(search_turns),
            };
            let result = if full {
                let selected_scope = if input.supplied() {
                    None
                } else {
                    scope.within().resolve()?
                };
                agent_tapes_core::search::list(
                    &input.backends()?,
                    harness.as_deref(),
                    selected_scope.as_ref(),
                    &filters,
                    agent_tapes_core::search::Options {
                        limit: limit.unwrap_or(20),
                        sort: sort.into(),
                        match_limit: search_matches,
                    },
                )?
            } else if input.supplied() {
                let backends = input.backends()?;
                agent_tapes_core::list_with_backends_options(
                    &backends,
                    None,
                    None,
                    limit.unwrap_or(20),
                    &filters,
                    sort.into(),
                )?
            } else {
                let mut result = agent_tapes_core::list_with_backends_options(
                    &input.backends()?,
                    harness.as_deref(),
                    scope.within().resolve()?.as_ref(),
                    limit.unwrap_or(20),
                    &filters,
                    sort.into(),
                )?;
                liveness::annotate(&mut result.sessions);
                result
            };
            let mut result = result;
            if let Some(table) = prices.table()? {
                for session in &mut result.sessions {
                    session.price(&table);
                }
            }
            if json {
                print_json(&result, &input)?;
            } else {
                print_session_list(&result.sessions);
                print_availability_note(&result);
                print_full_search(&result);
            }
        }
        Command::Artifact {
            session,
            reference,
            bytes,
            json,
            input,
        } => {
            if input.supplied() {
                return Err(anyhow!("artifact requires a native recording"));
            }
            let transcript = agent_tapes_core::show_with_backends(
                &input.backends()?,
                Selection::IdInHarness {
                    id: &session,
                    harness: "claude",
                },
                1,
            )?;
            let artifact = agent_tapes_core::artifact::read(
                &transcript.session,
                &reference,
                usize::try_from(bytes.get())?,
            )?;
            if json {
                println!("{}", serde_json::to_string(&artifact)?);
            } else {
                println!(
                    "bytes: {}; truncated: {}; sha256: {}; encoding: {}",
                    artifact.bytes, artifact.truncated, artifact.sha256, artifact.encoding
                );
                print!("{}", artifact.body);
            }
        }
        Command::Show {
            turn_chars,
            selection,
            read,
            tail,
            exchange,
            kinds,
            full,
            json,
            prices,
            remote,
        } => {
            selection.validate_input()?;
            read.refuse_supplied(&selection.input)?;
            let by_latest = selection.latest;
            let turns = if exchange {
                Some((
                    kinds.origin.map_or(TurnSelection::EXCHANGE, |origin| {
                        TurnSelection::EXCHANGE.with_origin(origin)
                    }),
                    "--exchange",
                ))
            } else {
                kinds.selection()
            };
            if remote.is_some() && selection.input.native() {
                return Err(anyhow!("--native-store conflicts with --remote"));
            }
            if remote.is_some() && prices.prices.is_some() {
                return Err(anyhow!(
                    "--prices prices a local read; a replica's answer under --remote is not priced"
                ));
            }
            if let Some(destination) = remote.as_deref() {
                let args =
                    remote_show_args(&selection, &read, tail, exchange, &kinds, full, turn_chars);
                let mut transcript = remote_show(destination, &args)?;
                transcript.session.source.replica = Some(destination.to_owned());
                if json {
                    print_json(&transcript, &selection.input)?;
                } else {
                    print_transcript(&transcript, by_latest, turns.map(|(_, flag)| flag));
                }
                return Ok(());
            }
            let installed_backends = (!selection.input.supplied())
                .then(|| read.backends(&selection.input))
                .transpose()?;
            let latest_warnings = installed_backends.as_deref().map_or_else(
                || Ok(Vec::new()),
                |backends| selection.latest_warnings(backends),
            )?;
            print_selection_warnings(&latest_warnings);
            if full {
                if selection.input.supplied() {
                    return Err(anyhow!(
                        "--full reads installed recordings; a supplied input is bounded by \
                         --scan-bytes, --decoded-bytes, and --record-bytes"
                    ));
                }
                return show_full(
                    &selection,
                    tail,
                    turns,
                    json,
                    by_latest,
                    turn_chars,
                    prices.table()?,
                );
            }
            // A selection is cut from every turn the read reached, so its
            // window counts kept turns rather than turns of every kind.
            let read_tail = if turns.is_some() {
                Some(usize::MAX)
            } else {
                tail
            };
            let transcript = if selection.input.supplied() {
                let backends = selection.input.backends()?;
                agent_tapes_core::show_with_backends(
                    &backends,
                    selection.selection(),
                    read_tail.unwrap_or(100),
                )?
            } else {
                let mut transcript = agent_tapes_core::show_with_backends(
                    installed_backends
                        .as_deref()
                        .expect("installed backends are present for an installed selection"),
                    selection.selection(),
                    read_tail.unwrap_or(100),
                )?;
                liveness::annotate(std::slice::from_mut(&mut transcript.session));
                transcript
            };
            let mut transcript = match turns {
                Some((kept, _)) => transcript.project(kept, tail.unwrap_or(100)),
                None => transcript,
            };
            if let Some(limit) = turn_chars {
                for turn in &mut transcript.turns {
                    turn.cap_text(limit);
                }
            }
            if let Some(table) = prices.table()? {
                transcript.session.price(&table);
            }
            if json {
                print_json(&transcript, &selection.input)?;
            } else {
                print_transcript(&transcript, by_latest, turns.map(|(_, flag)| flag));
            }
        }
        Command::Page {
            selection,
            cursor,
            pin,
            bytes,
            json,
        } => {
            selection.validate_input()?;
            if selection.input.supplied() {
                return Err(anyhow!(
                    "--input is not supported by page; supplied exports have no installed history cursor"
                ));
            }
            let cursor = agent_tapes_core::history::walk_cursor(pin, cursor.as_deref())?;
            let latest_warnings = selection.latest_warnings(&selection.input.backends()?)?;
            print_selection_warnings(&latest_warnings);
            let page = agent_tapes_core::history::page_with_backends(
                &selection.input.backends()?,
                selection.selection(),
                cursor,
                bytes.get_usize(),
            )?;
            if json {
                println!("{}", serde_json::to_string(&page)?);
            } else {
                println!(
                    "{} ({}): bytes {}..{} of {}; {} malformed records, {} skipped fragment bytes",
                    page.session.id,
                    page.session.harness(),
                    page.start,
                    page.end,
                    page.source_bytes,
                    page.skipped_records,
                    page.skipped_fragment_bytes
                );
                if let Some(pin) = &page.pin {
                    println!(
                        "Pinned {} bytes in {} chunks of {} bytes; verified {} bytes",
                        pin.pinned_bytes, pin.chunks, pin.chunk_bytes, pin.verification_bytes
                    );
                }
                for turn in &page.turns {
                    println!("[{} #{}] {}", human_speaker(turn), turn.ordinal, turn.text);
                }
                if let Some(cursor) = page.next_cursor {
                    println!("Next cursor: {cursor}");
                }
            }
        }
        Command::HistorySearch {
            selection,
            cursor,
            pin,
            bytes,
            pages,
            search,
            json,
        } => {
            selection.validate_input()?;
            if selection.input.supplied() {
                return Err(anyhow!(
                    "--input is not supported by history-search; supplied exports have no installed history cursor"
                ));
            }
            let cursor = agent_tapes_core::history::walk_cursor(pin, cursor.as_deref())?;
            let latest_warnings = selection.latest_warnings(&selection.input.backends()?)?;
            print_selection_warnings(&latest_warnings);
            let report = agent_tapes_core::history::search_with_backends(
                &selection.input.backends()?,
                selection.selection(),
                cursor,
                bytes.get_usize(),
                pages,
                &search,
            )?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!(
                    "{} pages, {} bytes read; {} malformed records, {} skipped fragment bytes",
                    report.pages_read,
                    report.bytes_read,
                    report.skipped_records,
                    report.skipped_fragment_bytes
                );
                print_history_pin(&report.reads);
                for found in report.matches {
                    println!(
                        "[page {}..{} page-local #{} {}{}] {}",
                        found.page_start,
                        found.page_end,
                        found.ordinal,
                        found.kind.label(),
                        found
                            .ts
                            .map(|ts| format!(" {}", human_timestamp(ts)))
                            .unwrap_or_default(),
                        found.text
                    );
                }
                if report.matches_truncated {
                    println!("Match output stopped at 100 records.");
                }
                if let Some(cursor) = report.next_cursor {
                    println!("Unsearched history remains. Next cursor: {cursor}");
                }
            }
        }
        Command::Metadata {
            selection,
            cursor,
            pin,
            bytes,
            pages,
            json,
        } => {
            selection.validate_input()?;
            if selection.input.supplied() {
                return Err(anyhow!(
                    "--input is not supported by metadata; supplied exports have no installed history cursor"
                ));
            }
            let cursor = agent_tapes_core::history::walk_cursor(pin, cursor.as_deref())?;
            let latest_warnings = selection.latest_warnings(&selection.input.backends()?)?;
            print_selection_warnings(&latest_warnings);
            let report = agent_tapes_core::history::metadata_with_backends(
                &selection.input.backends()?,
                selection.selection(),
                cursor,
                bytes.get_usize(),
                pages,
            )?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("Recorded model observations: {} pages, {} bytes; {} malformed records, {} skipped fragment bytes", report.pages_read, report.bytes_read, report.skipped_records, report.skipped_fragment_bytes);
                print_history_pin(&report.reads);
                for observation in report.observations {
                    match observation.timestamp {
                        Some(timestamp) => println!(
                            "{} {}",
                            observation.model.identity(),
                            human_timestamp(timestamp)
                        ),
                        None => println!("{}", observation.model.identity()),
                    }
                }
                if report.observations_truncated {
                    println!("Observation output stopped at 100 records.");
                }
                if let Some(cursor) = report.next_cursor {
                    println!("Unread metadata history remains. Next cursor: {cursor}");
                }
            }
        }
        Command::Child {
            selection,
            read,
            reference,
            tail,
            full,
            json,
        } => {
            selection.validate_input()?;
            let latest_warnings = if full {
                selection.latest_warnings(&selection.input.backends()?)?
            } else {
                selection.latest_warnings(&read.backends(&selection.input)?)?
            };
            print_selection_warnings(&latest_warnings);
            if full {
                refuse_full_supplied(&selection.input)?;
            }
            if selection.input.supplied() {
                return Err(anyhow!(
                    "--input is not supported by child; supplied exports have no child store"
                ));
            }
            let child = if full {
                agent_tapes_core::child::read_full_with_backends(
                    &selection.input.backends()?,
                    selection.selection(),
                    &reference,
                    tail,
                )?
            } else {
                agent_tapes_core::child::read_with_backends(
                    &read.backends(&selection.input)?,
                    selection.selection(),
                    &reference,
                    tail,
                )?
            };
            if json {
                println!("{}", serde_json::to_string(&child)?);
            } else {
                println!(
                    "Child {} of {} ({})",
                    child.reference,
                    child.parent.id,
                    child.parent.harness()
                );
                print_transcript(&child.transcript, false, None);
                print!("{}", render_usage(&child.usage));
                if !child.ending.facts.is_empty() {
                    println!(
                        "Ending facts: {}",
                        child
                            .ending
                            .facts
                            .iter()
                            .map(|fact| fact.label())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if !child.ending.incomplete.is_empty() {
                    println!(
                        "Incomplete: {}",
                        child
                            .ending
                            .incomplete
                            .iter()
                            .map(|cause| cause.label())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
        Command::Events {
            selection,
            read,
            tail,
            name,
            command,
            count,
            call_id,
            program,
            full,
            json,
            full_arguments,
        } => {
            selection.validate_input()?;
            if selection.input.after_occurrence.is_some() {
                return Err(anyhow!("--after-occurrence is not supported by events"));
            }
            read.refuse_supplied(&selection.input)?;
            let by_latest = selection.latest;
            let options = agent_tapes_core::event::EventOptions { full_arguments };
            let event_backends = if full {
                selection.input.backends()?
            } else {
                read.backends(&selection.input)?
            };
            let latest_warnings = selection.latest_warnings(&event_backends)?;
            print_selection_warnings(&latest_warnings);
            let filter = agent_tapes_core::event::EventFilter {
                names: &name,
                call_ids: &call_id,
                programs: &program,
                command: command.as_deref(),
            };
            if full {
                refuse_full_supplied(&selection.input)?;
            }
            selection.validate_window()?;
            if selection.single().is_none() || count {
                if selection.single().is_none() && !selection.has_set_selection() {
                    return Err(anyhow!("events needs a session ID, --latest, or an explicit selection such as --here"));
                }
                return events_selection(
                    &selection,
                    &event_backends,
                    tail,
                    &filter,
                    options,
                    full,
                    count,
                    json,
                );
            }
            if full {
                return events_full(&selection, tail, &filter, json, by_latest, options);
            }
            let mut events = if selection.input.supplied() {
                let backends = selection.input.backends()?;
                agent_tapes_core::events_with_options_with_backends(
                    &backends,
                    selection.single().expect("single selection"),
                    tail.unwrap_or(usize::MAX),
                    options,
                )?
            } else {
                let mut events = agent_tapes_core::events_with_options_with_backends(
                    &event_backends,
                    selection.single().expect("single selection"),
                    tail.unwrap_or(usize::MAX),
                    options,
                )?;
                liveness::annotate(std::slice::from_mut(&mut events.session));
                events
            };
            events.retain(&filter);
            if json {
                print_json(&events, &selection.input)?;
            } else {
                print_events(&events, by_latest);
            }
        }
        Command::Lineage {
            selection,
            read,
            full,
            json,
        } => {
            selection.validate_input()?;
            read.refuse_supplied(&selection.input)?;
            let lineage_backends = if full {
                selection.input.backends()?
            } else {
                read.backends(&selection.input)?
            };
            let latest_warnings = selection.latest_warnings(&lineage_backends)?;
            print_selection_warnings(&latest_warnings);
            let lineage = if full {
                refuse_full_supplied(&selection.input)?;
                agent_tapes_core::lineage_full_with_backends(
                    &selection.input.backends()?,
                    selection.selection(),
                )?
            } else if selection.input.supplied() {
                let backends = selection.input.backends()?;
                agent_tapes_core::lineage_with_backends(&backends, selection.selection())?
            } else {
                agent_tapes_core::lineage_with_backends(&lineage_backends, selection.selection())?
            };
            if json {
                print_json(&lineage, &selection.input)?;
            } else {
                print!("{}", render_lineage(&lineage));
            }
        }
        Command::Timeline {
            query,
            read,
            full,
            json,
        } => {
            query.validate_input()?;
            read.refuse_supplied(&query.input)?;
            if full {
                refuse_full_supplied(&query.input)?;
            }
            let backends = if query.input.supplied() || full {
                query.input.backends()?
            } else {
                read.backends(&query.input)?
            };
            let warnings = query.latest_warnings(&backends)?;
            print_selection_warnings(&warnings);
            let mut sink = TimelineOutput {
                out: std::io::stdout().lock(),
                json,
                first_session: true,
                first_record: true,
            };
            if json {
                use std::io::Write;
                write!(sink.out, "{{\"schema\":\"tapes-timeline/1\",\"sessions\":[")?;
            }
            if let Some(one) = query.single() {
                agent_tapes_core::timeline::single(&backends, one, full, &mut sink)?;
            } else {
                if !query.has_set_selection() {
                    return Err(anyhow!("timeline needs a session ID, --latest, or an explicit selection such as --here"));
                }
                agent_tapes_core::timeline::selection(&backends, &query.set(), full, &mut sink)?;
            }
            if json {
                use std::io::Write;
                writeln!(sink.out, "]}}")?;
            }
        }
        Command::Stats {
            query,
            read,
            prices,
            full,
            idle,
            json,
        } => {
            query.validate_input()?;
            read.refuse_supplied(&query.input)?;
            if full {
                refuse_full_supplied(&query.input)?;
            }
            let single_backends = if full {
                query.input.backends()?
            } else {
                read.backends(&query.input)?
            };
            let latest_warnings = query.latest_warnings(&single_backends)?;
            print_selection_warnings(&latest_warnings);
            // A selection's summary carries no cost to price, but a table the
            // caller named that cannot be read still refuses.
            let prices = prices.table()?;
            if let Some(one) = query.single() {
                let prices = prices.as_ref();
                let stats = if full {
                    agent_tapes_core::stats_full_priced_with_backends(
                        &query.input.backends()?,
                        one,
                        idle,
                        prices,
                    )?
                } else if query.input.supplied() {
                    let backends = query.input.backends()?;
                    agent_tapes_core::stats_priced_with_backends(&backends, one, idle, prices)?
                } else {
                    agent_tapes_core::stats_priced_with_backends(
                        &single_backends,
                        one,
                        idle,
                        prices,
                    )?
                };
                if json {
                    print_json_with_selection_warnings(&stats, &query.input, &latest_warnings)?;
                } else {
                    print!("{}", render_stats(&stats));
                }
            } else {
                if !query.has_set_selection() {
                    return Err(anyhow!("stats needs a session ID, --latest, or an explicit selection such as --here"));
                }
                let summary = if full {
                    agent_tapes_core::stats_summary::with_backends_and_idle(
                        &query.input.backends()?,
                        &query.set(),
                        SessionRead::Whole,
                        idle,
                    )?
                } else if query.input.supplied() {
                    let backends = query.input.backends()?;
                    agent_tapes_core::stats_summary::with_backends_and_idle(
                        &backends,
                        &query.set(),
                        SessionRead::Bounded,
                        idle,
                    )?
                } else {
                    agent_tapes_core::stats_summary::with_backends_and_idle(
                        &read.backends(&query.input)?,
                        &query.set(),
                        SessionRead::Bounded,
                        idle,
                    )?
                };
                if json {
                    print_json(&summary, &query.input)?;
                } else {
                    print_stats_summary(&summary);
                    print_unplaced_note(summary.unplaced.as_ref(), summary.scanned);
                }
                if summary.read == 0 && !summary.failed.is_empty() {
                    return Err(anyhow!("no selected session could be read"));
                }
            }
        }
        Command::Usage {
            query,
            read,
            by,
            full,
            series,
            reconcile,
            family,
            continuations,
            evidence,
            event_keys,
            from,
            to,
            requests,
            prices,
            window,
            quota_window,
            quota,
            allowance_rates,
            forecast,
            concurrency,
            change,
            json,
        } => {
            query.validate_input()?;
            let prices = price_table(prices.as_deref())?;
            let prices = prices.as_ref();
            read.refuse_supplied(&query.input)?;
            if change.asked(&by) {
                let refused = [
                    ("--family", family),
                    ("--quota-window", quota_window.is_some()),
                    ("--allowance-rates", allowance_rates),
                    ("--forecast", forecast.is_some()),
                    ("--from", from.is_some()),
                    ("--to", to.is_some()),
                    ("--requests", requests.is_some()),
                    ("--full", full),
                    ("--series", series.is_some()),
                    ("--reconcile", reconcile),
                    ("--event-keys", event_keys.is_some()),
                ];
                return usage_change(&query, &read, &by, *change, &refused, window, prices, json);
            }
            if !by.is_empty() && query.single().is_some() {
                return Err(anyhow!(
                    "--by groups a selection of sessions, and one session is not grouped; \
                     `<SESSION> --by commit` divides one session across a repository's commits"
                ));
            }
            let span = requests
                .or((from.is_some() || to.is_some()).then_some(SpanBounds::Time { from, to }));
            if span.is_some() {
                if query.single().is_none() {
                    return Err(anyhow!(
                        "--from, --to and --requests answer a span of one session; name a \
                         session id, --latest, or --title"
                    ));
                }
                refuse_full_supplied(&query.input)?;
            }
            let series = parse_series(series)?;
            let event_keys = parse_event_keys(event_keys)?;
            if event_keys.is_some() && query.input.supplied() {
                return Err(anyhow!(
                    "--event-keys is available only for installed recording sessions"
                ));
            }
            if event_keys.is_some() && query.single().is_none() {
                return Err(anyhow!("--event-keys answers one installed session; name a session id, --latest, or --title"));
            }
            if series.is_some() && query.input.supplied() {
                return Err(anyhow!(
                    "--series is available only for installed recording sessions"
                ));
            }
            if series.is_some() && query.single().is_none() {
                return Err(anyhow!(
                    "--series answers one installed session; name a session id, --latest, or --title"
                ));
            }
            if reconcile && (query.input.supplied() || query.single().is_none()) {
                return Err(anyhow!(
                    "--reconcile answers one installed session; name a session id, --latest, or \
                     --title"
                ));
            }
            if family {
                let Some(one) = query.single() else {
                    return Err(anyhow!(
                        "--family answers one installed session; name a session id, --latest, or \
                         --title"
                    ));
                };
                if query.input.supplied() {
                    return Err(anyhow!(
                        "--family reads installed recordings whole; a supplied input refuses it"
                    ));
                }
                let evidence = if evidence.is_empty() {
                    None
                } else {
                    Some(agent_tapes_core::attribution_evidence::read_paths(
                        &evidence,
                    )?)
                };
                let backends = query.input.backends()?;
                let latest_warnings = query.latest_warnings(&backends)?;
                print_selection_warnings(&latest_warnings);
                let family = agent_tapes_core::family::family_usage_with_evidence(
                    &backends,
                    one,
                    agent_tapes_core::family::FamilyOptions {
                        continuations,
                        prices,
                    },
                    evidence.as_deref(),
                )?;
                if json {
                    print_json_with_selection_warnings(&family, &query.input, &latest_warnings)?;
                } else {
                    print!("{}", render_family(&family));
                }
                return Ok(());
            }
            if full {
                refuse_full_supplied(&query.input)?;
            }
            let single_backends = if full {
                query.input.backends()?
            } else {
                read.backends(&query.input)?
            };
            let latest_warnings = query.latest_warnings(&single_backends)?;
            print_selection_warnings(&latest_warnings);
            if let Some(one) = query.single() {
                let usage = if let Some(span) = span {
                    agent_tapes_core::usage_spans_with_backends(
                        &query.input.backends()?,
                        one,
                        &[span],
                    )?
                    .spans
                    .remove(0)
                } else if full {
                    if series.is_some() || reconcile || event_keys.is_some() {
                        agent_tapes_core::usage_with_options_with_backends(
                            &query.input.backends()?,
                            one,
                            UsageOptions {
                                full: true,
                                series,
                                reconcile,
                                event_keys,
                            },
                        )?
                    } else {
                        agent_tapes_core::usage_full_with_backends(&query.input.backends()?, one)?
                    }
                } else if query.input.supplied() {
                    let backends = query.input.backends()?;
                    agent_tapes_core::usage_with_backends(&backends, one)?
                } else if series.is_some() || reconcile || event_keys.is_some() {
                    agent_tapes_core::usage_with_options_with_backends(
                        &single_backends,
                        one,
                        UsageOptions {
                            full: false,
                            series,
                            reconcile,
                            event_keys,
                        },
                    )?
                } else {
                    agent_tapes_core::usage_with_backends(&single_backends, one)?
                };
                let mut usage = usage;
                if let Some(table) = prices {
                    agent_tapes_core::pricing::price_view(&mut usage, table);
                }
                if json {
                    print_json_with_selection_warnings(&usage, &query.input, &latest_warnings)?;
                } else {
                    print!("{}", render_usage(&usage));
                }
            } else {
                if full {
                    return Err(anyhow!(
                        "--full reads one session's whole recording: name a session id or pass \
                         --latest"
                    ));
                }
                let selected = query.has_set_selection() || !by.is_empty();
                if !selected {
                    return Err(anyhow!(
                        "usage answers one session or a selection of them: name a session id, \
                         pass --latest, or select a set with --here, --project <path>, --global, \
                         or a listing filter"
                    ));
                }
                query.validate_window()?;
                query.describe_search();
                let by = grouping(&by);
                let ask =
                    window
                        .map(WindowAsk::Bounds)
                        .or(quota_window.map(|at| WindowAsk::Quota {
                            quota: quota.into(),
                            at,
                        }));
                if let Some(ForecastPlan(plan)) = &forecast {
                    if query.input.supplied() {
                        return Err(anyhow!(
                            "--forecast reads installed recordings whole; a supplied input refuses it"
                        ));
                    }
                    let mut selection = query.set();
                    let lookback = window.unwrap_or(WindowBounds {
                        from: selection.filters.since,
                        to: selection.filters.until,
                    });
                    selection.filters.until = None;
                    let forecast = agent_tapes_core::allowance_forecast_with_backends(
                        &query.input.backends()?,
                        &selection,
                        lookback,
                        quota.into(),
                        plan,
                        concurrency.map(|lanes| lanes as usize),
                    )?;
                    if json {
                        print_json(&forecast, &query.input)?;
                    } else {
                        print!("{}", render_forecast(&forecast));
                    }
                    return Ok(());
                }
                if allowance_rates {
                    if query.input.supplied() {
                        return Err(anyhow!(
                            "--allowance-rates reads installed recordings whole; a supplied input refuses it"
                        ));
                    }
                    let mut selection = query.set();
                    let lookback = window.unwrap_or(WindowBounds {
                        from: selection.filters.since,
                        to: selection.filters.until,
                    });
                    if lookback.from.is_none() {
                        return Err(anyhow!(
                            "--allowance-rates needs a lookback: --window FROM..TO or --since"
                        ));
                    }
                    // The lookback cuts consumption at its edges; a session
                    // that ran past its end still has requests inside it.
                    selection.filters.until = None;
                    let rates = agent_tapes_core::allowance_rates_with_backends(
                        &query.input.backends()?,
                        &selection,
                        &by,
                        lookback,
                        quota.into(),
                    )?;
                    if json {
                        print_json(&rates, &query.input)?;
                    } else {
                        print!("{}", render_allowance_rates(&rates, &by));
                    }
                    return Ok(());
                }
                let summary = if let Some(ask) = ask {
                    if query.input.supplied() {
                        return Err(anyhow!(
                            "--window and --quota-window read installed recordings whole; a supplied input refuses them"
                        ));
                    }
                    agent_tapes_core::usage_window_with_backends(
                        &query.input.backends()?,
                        &query.set(),
                        &by,
                        ask,
                        prices,
                    )?
                } else if query.input.supplied() {
                    let backends = query.input.backends()?;
                    agent_tapes_core::usage_summary_priced(&backends, &query.set(), &by, prices)?
                } else {
                    agent_tapes_core::usage_summary_priced(
                        &read.backends(&query.input)?,
                        &query.set(),
                        &by,
                        prices,
                    )?
                };
                if json {
                    print_json(&summary, &query.input)?;
                } else {
                    if let Some(window) = &summary.window {
                        print!("{}", render_window(window));
                    }
                    print!("{}", render_usage_summary(&summary, &by));
                    print_diagnostics(
                        summary.scan_truncated,
                        summary.scanned,
                        &summary.unreadable,
                        &summary.unsearched,
                        &summary.partial_search,
                        &summary.unavailable,
                    );
                    print_unplaced_note(summary.unplaced.as_ref(), summary.scanned);
                }
            }
        }
        Command::Brief {
            selection,
            read,
            prices,
            tail,
            json,
        } => {
            selection.validate_input()?;
            read.refuse_supplied(&selection.input)?;
            let brief_backends = read.backends(&selection.input)?;
            let latest_warnings = selection.latest_warnings(&brief_backends)?;
            print_selection_warnings(&latest_warnings);
            let prices = prices.table()?;
            let brief = if selection.input.supplied() {
                let backends = selection.input.backends()?;
                agent_tapes_core::brief_priced_with_backends(
                    &backends,
                    selection.selection(),
                    tail,
                    prices.as_ref(),
                )?
            } else {
                let mut brief = agent_tapes_core::brief_priced_with_backends(
                    &brief_backends,
                    selection.selection(),
                    tail,
                    prices.as_ref(),
                )?;
                liveness::annotate_brief(&mut brief);
                brief
            };
            if json {
                print_json(&brief, &selection.input)?;
            } else {
                print!("{}", render_brief(&brief));
            }
        }
        Command::Workset {
            selection,
            bundle,
            lane_bytes,
        } => {
            selection.validate_input()?;
            if selection.input.supplied() {
                return Err(anyhow!(
                    "workset reads installed recordings in full; a supplied input is not one"
                ));
            }
            agent_tapes_core::workset::check_lane_bytes(lane_bytes.get())?;
            let backends = selection.input.backends()?;
            print_selection_warnings(&selection.latest_warnings(&backends)?);
            let mut workset = agent_tapes_core::workset::build_with_backends(
                &backends,
                selection.selection(),
                lane_bytes.get(),
                Some(&bundle),
            )?;
            liveness::annotate_workset(&mut workset);
            let path = agent_tapes_core::workset::write_manifest(&bundle, &workset)?;
            print!("{}", render_workset(&workset, &path));
        }
        Command::Locate {
            workset,
            refs,
            text_bytes,
            json,
        } => {
            use agent_tapes_core::locate;
            if text_bytes.get_usize() > locate::MAX_TEXT_BYTES {
                return Err(anyhow!(
                    "--text-bytes must be at most {} bytes; got {}",
                    locate::MAX_TEXT_BYTES,
                    text_bytes.get()
                ));
            }
            let frozen = locate::read_manifest(&workset)?;
            let refs = refs
                .as_deref()
                .map(locate::read_refs)
                .transpose()?
                .unwrap_or_default();
            let report = locate::locate_with_backends(
                &agent_tapes_core::backend::backends(),
                &frozen,
                &refs,
                text_bytes.get_usize(),
            )?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print!("{}", render_locate(&report));
            }
        }
        Command::Endings {
            harness,
            scope,
            limit,
            model,
            launch,
            directory,
            since,
            until,
            sort,
            search,
            input,
            read,
            tail,
            text,
            json,
        } => {
            input.validate_bulk()?;
            read.refuse_supplied(&input)?;
            reject_installed_selection(&input, harness.as_deref(), &scope)?;
            if since
                .zip(until)
                .is_some_and(|(since, until)| since >= until)
            {
                return Err(anyhow!("--since must be earlier than --until"));
            }
            if search.is_some() {
                eprintln!(
                    "Searching a bounded window of normalized turns in each candidate session before applying --limit."
                );
            }
            let selection = agent_tapes_core::SessionSelection {
                within: if input.supplied() {
                    Where::Global
                } else {
                    scope.within()
                },
                harness: if input.supplied() {
                    None
                } else {
                    harness.as_deref()
                },
                limit,
                filters: agent_tapes_core::ListFilters {
                    model: model.as_deref(),
                    launch: launch.map(Into::into),
                    directory: directory.as_deref(),
                    since,
                    until,
                    search: search.as_deref(),
                    search_turns: None,
                },
                sort: sort.into(),
            };
            let report = if input.supplied() {
                let backends = input.backends()?;
                agent_tapes_core::endings::endings_with_backends(&backends, &selection, tail, text)?
            } else {
                let mut report = agent_tapes_core::endings::endings_with_backends(
                    &read.backends(&input)?,
                    &selection,
                    tail,
                    text,
                )?;
                liveness::annotate_endings(&mut report.endings);
                report
            };
            if json {
                print_json(&report, &input)?;
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
                    &report.partial_search,
                    &report.unavailable,
                );
                print_unplaced_note(report.unplaced.as_ref(), report.scanned);
            }
        }
        Command::Install {
            harness: IntegrationArg::Pi,
        } => integration::install_pi()?,
        Command::Install {
            harness: IntegrationArg::Opencode,
        } => integration::install_opencode()?,
        Command::Uninstall {
            harness: IntegrationArg::Pi,
        } => integration::uninstall_pi()?,
        Command::Uninstall {
            harness: IntegrationArg::Opencode,
        } => integration::uninstall_opencode()?,
        Command::Export {
            query,
            read,
            bundle,
            kinds,
            full,
            evidence,
            remote,
        } => {
            query.validate_input()?;
            read.refuse_supplied(&query.input)?;
            if evidence && !query.input.supplied() {
                return Err(anyhow!(
                    "--evidence copies the bytes of a supplied input; an installed recording's \
                     file is already the evidence"
                ));
            }
            if full && query.input.supplied() {
                return Err(anyhow!(
                    "--full reads installed recordings; a supplied input is bounded by \
                     --scan-bytes, --decoded-bytes, and --record-bytes"
                ));
            }
            if remote.is_some() && query.input.native() {
                return Err(anyhow!("--native-store conflicts with --remote"));
            }
            if let Some(destination) = remote.as_deref() {
                let args = remote_export_args(&query, &read, &kinds, full, bundle.as_deref());
                let manifest = remote_export(destination, &args)?;
                print!("{manifest}");
                return Ok(());
            }
            let view = kinds.selection().map(|(kept, _)| kept);
            let whole = if full {
                agent_tapes_core::ExportRead::Whole
            } else if evidence {
                agent_tapes_core::ExportRead::Evidence
            } else {
                agent_tapes_core::ExportRead::Bounded
            };
            let single_backends = if full {
                query.input.backends()?
            } else {
                read.backends(&query.input)?
            };
            let latest_warnings = query.latest_warnings(&single_backends)?;
            print_selection_warnings(&latest_warnings);
            if let Some(one) = query.single() {
                let bundle = if query.input.supplied() {
                    let backends = query.input.backends()?;
                    agent_tapes_core::export_with_backends(
                        &backends,
                        one,
                        bundle.as_deref(),
                        view,
                        whole,
                    )?
                } else {
                    agent_tapes_core::export_with_backends(
                        &single_backends,
                        one,
                        bundle.as_deref(),
                        view,
                        whole,
                    )?
                };
                print_manifest(&bundle);
            } else {
                query.validate_window()?;
                query.describe_search();
                let export = if query.input.supplied() {
                    let backends = query.input.backends()?;
                    agent_tapes_core::export_selection_with_backends(
                        &backends,
                        &query.set(),
                        bundle.as_deref(),
                        view,
                        whole,
                    )?
                } else {
                    agent_tapes_core::export_selection_with_backends(
                        &read.backends(&query.input)?,
                        &query.set(),
                        bundle.as_deref(),
                        view,
                        whole,
                    )?
                };
                print_selection_manifest(&export);
                print_unplaced_note(export.manifest.unplaced.as_ref(), export.manifest.scanned);
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

/// The arguments a replica's own `tapes` is asked with. Flags are appended in
/// one place per direction so a transport command can be read without opening
/// the command that built it.
#[derive(Default)]
struct RemoteArgs(Vec<OsString>);

impl RemoteArgs {
    fn raw(&mut self, argument: impl AsRef<OsStr>) -> &mut Self {
        self.0.push(argument.as_ref().to_os_string());
        self
    }

    fn flag(&mut self, name: &str) -> &mut Self {
        self.raw(name)
    }

    fn flag_if(&mut self, name: &str, present: bool) -> &mut Self {
        if present {
            self.flag(name);
        }
        self
    }

    fn value(&mut self, name: &str, value: impl AsRef<OsStr>) -> &mut Self {
        self.raw(name).raw(value)
    }

    fn value_if(&mut self, name: &str, value: Option<impl AsRef<OsStr>>) -> &mut Self {
        if let Some(value) = value {
            self.value(name, value);
        }
        self
    }

    fn scope(&mut self, scope: &ScopeArgs) -> &mut Self {
        self.flag_if("--here", scope.here);
        self.value_if("--project", scope.project.as_ref());
        self.flag_if("--global", scope.global)
    }

    fn into_vec(self) -> Vec<OsString> {
        self.0
    }
}

fn sort_label(sort: SortArg) -> &'static str {
    match sort {
        SortArg::Newest => "newest",
        SortArg::Oldest => "oldest",
    }
}

fn timestamp_arg(timestamp: agent_tapes_core::ActivityTimestamp) -> String {
    timestamp.to_rfc3339()
}

fn kind_labels(kinds: &[TurnKind]) -> String {
    kinds
        .iter()
        .map(|kind| kind.label())
        .collect::<Vec<_>>()
        .join(",")
}

/// A listing asked of a replica: the same selection this machine would apply
/// locally, resolved there, because the stores and the paths are the
/// replica's.
struct RemoteListQuery<'a> {
    harness: &'a Option<String>,
    scope: &'a ScopeArgs,
    limit: Option<usize>,
    model: &'a Option<String>,
    launch: Option<LaunchArg>,
    directory: &'a Option<String>,
    since: Option<agent_tapes_core::ActivityTimestamp>,
    until: Option<agent_tapes_core::ActivityTimestamp>,
    sort: SortArg,
    search: &'a Option<String>,
    search_turns: usize,
    full: bool,
    search_matches: usize,
}

fn remote_list_args(query: &RemoteListQuery<'_>) -> Vec<OsString> {
    let mut args = RemoteArgs::default();
    args.raw("list");
    args.scope(query.scope);
    args.value_if("--harness", query.harness.as_deref());
    args.value_if("--limit", query.limit.map(|limit| limit.to_string()));
    args.value_if("--model", query.model.as_deref());
    args.value_if("--launch", query.launch.map(launch_label));
    args.value_if("--directory", query.directory.as_deref());
    args.value_if("--since", query.since.map(timestamp_arg));
    args.value_if("--until", query.until.map(timestamp_arg));
    args.value("--sort", sort_label(query.sort));
    args.value_if("--search", query.search.as_deref());
    if query.full {
        args.raw("--full");
        args.value("--search-matches", query.search_matches.to_string());
    } else if query.search.is_some() {
        args.value(
            "--search-turns",
            if query.search_turns == usize::MAX {
                "all".to_owned()
            } else {
                query.search_turns.to_string()
            },
        );
    }
    args.into_vec()
}

/// A transcript read asked of a replica. The remote command is `show`, which
/// is the read `export` takes as well; the two differ only in the window and
/// in what the local side does with the answer.
fn remote_show_args(
    selection: &SelectionArgs,
    read: &ReadArgs,
    tail: Option<usize>,
    exchange: bool,
    kinds: &TurnKindArgs,
    full: bool,
    turn_chars: Option<usize>,
) -> Vec<OsString> {
    let mut args = RemoteArgs::default();
    args.raw("show");
    args.value_if("--turn-chars", turn_chars.map(|limit| limit.to_string()));
    if let Some(session) = selection.session.as_deref() {
        args.raw(session);
    }
    args.value_if("--title", selection.title.as_deref());
    args.flag_if("--latest", selection.latest);
    for exclude in &selection.exclude {
        args.value("--exclude", exclude);
    }
    args.value_if("--harness", selection.harness.as_deref());
    args.scope(&selection.scope);
    remote_read_args(&mut args, read, tail, kinds, full);
    args.flag_if("--exchange", exchange);
    args.into_vec()
}

/// The export asked of a replica. It is the replica's own `export` with the
/// caller's arguments, so the replica writes the bundles, on its own
/// filesystem, through the reader that holds the recording. A bundle
/// assembled from a transcript answer would not be the same artifact: tool
/// events are not part of the transcript wire contract, and a bundle that
/// silently held none would be worse than the refusal. `--bundle` therefore
/// names a directory on the replica.
fn remote_export_args(
    query: &SessionQueryArgs,
    read: &ReadArgs,
    kinds: &TurnKindArgs,
    full: bool,
    bundle: Option<&std::path::Path>,
) -> Vec<OsString> {
    let mut args = RemoteArgs::default();
    args.raw("export");
    if let Some(session) = query.session.as_deref() {
        args.raw(session);
    }
    args.value_if("--title", query.title.as_deref());
    args.flag_if("--latest", query.latest);
    for exclude in &query.exclude {
        args.value("--exclude", exclude);
    }
    args.value_if("--harness", query.harness.as_deref());
    args.value_if("--limit", query.limit.map(|limit| limit.to_string()));
    args.value_if("--model", query.model.as_deref());
    args.value_if("--launch", query.launch.map(launch_label));
    args.value_if("--directory", query.directory.as_deref());
    args.value_if("--since", query.since.map(timestamp_arg));
    args.value_if("--until", query.until.map(timestamp_arg));
    args.value_if("--sort", query.sort.map(sort_label));
    args.value_if("--search", query.search.as_deref());

    args.scope(&query.scope);
    args.value_if("--bundle", bundle);
    remote_read_args(&mut args, read, None, kinds, full);
    args.into_vec()
}

fn remote_read_args(
    args: &mut RemoteArgs,
    read: &ReadArgs,
    tail: Option<usize>,
    kinds: &TurnKindArgs,
    full: bool,
) {
    args.value_if(
        "--read-bytes",
        read.read_bytes.map(|bytes| bytes.get().to_string()),
    );
    args.value_if("--tail", tail.map(|tail| tail.to_string()));
    args.value_if(
        "--origin",
        kinds.origin.map(|origin| origin.label().to_owned()),
    );
    if !kinds.only.is_empty() {
        args.value("--only", kind_labels(&kinds.only));
    }
    if !kinds.omit.is_empty() {
        args.value("--omit", kind_labels(&kinds.omit));
    }
    args.flag_if("--full", full);
}

fn remote_transport(
    destination: &str,
) -> std::result::Result<remote::Replica, remote::Unavailable> {
    let settings = remote::Settings::resolve().map_err(|error| {
        remote::Unavailable::new(
            destination,
            format!("the local transport cannot be configured: {error:#}"),
        )
    })?;
    Ok(remote::Replica::new(destination, settings))
}

fn remote_list(
    destination: &str,
    args: &[OsString],
) -> std::result::Result<agent_tapes_core::SessionList, remote::Unavailable> {
    let mut list: agent_tapes_core::SessionList =
        remote_transport(destination)?.json(args, agent_tapes_core::LIST_SCHEMA)?;
    for session in &mut list.sessions {
        session.source.replica = Some(destination.to_owned());
    }
    Ok(list)
}

fn remote_show(destination: &str, args: &[OsString]) -> Result<Transcript> {
    Ok(remote_transport(destination)?.json(args, agent_tapes_core::model::SESSION_SCHEMA)?)
}

/// The replica's export manifest, with every path named as the path it is:
/// one on the replica. `replica:/path` is the coordinate the caller can hand
/// to `scp` or `rsync`, and it is never read as a path on this machine.
fn remote_export(destination: &str, args: &[OsString]) -> Result<String> {
    let output = remote_transport(destination)?.output(args)?;
    let text = std::str::from_utf8(&output).map_err(|_| {
        anyhow!("remote replica {destination} unavailable: the export manifest is not UTF-8")
    })?;
    let mut rendered = format!("Replica: {destination}\n");
    for line in text.lines().filter(|line| !line.is_empty()) {
        let Some((path, size)) = line.split_once('\t') else {
            return Err(anyhow!(
                "remote replica {destination} unavailable: the export answered a line that is \
                 not a file manifest: {line}"
            ));
        };
        rendered.push_str(&format!("{destination}:{path}\t{size}\n"));
    }
    Ok(rendered)
}

/// A listing a replica could not answer. The empty session set is never read
/// as "the replica holds nothing": the cause and the destination are the
/// answer's substance, and the caller exits unsuccessfully with them.
fn unreachable_list(
    sort: SortArg,
    since: Option<agent_tapes_core::ActivityTimestamp>,
    until: Option<agent_tapes_core::ActivityTimestamp>,
    unavailable: &remote::Unavailable,
) -> agent_tapes_core::SessionList {
    agent_tapes_core::SessionList {
        schema: agent_tapes_core::LIST_SCHEMA,
        full_search: None,
        sort: sort.into(),
        activity: (since.is_some() || until.is_some())
            .then_some(agent_tapes_core::ActivityWindow { since, until }),
        sessions: Vec::new(),
        artifacts: Vec::new(),
        unavailable: vec![unavailable.diagnostic()],
        unreadable: Vec::new(),
        unsearched: Vec::new(),
        partial_search: Vec::new(),
        scanned: 0,
        scan_truncated: false,
        unplaced: None,
    }
}

fn coverage_name(coverage: TurnCoverage) -> &'static str {
    match coverage {
        TurnCoverage::Session => "session",
        TurnCoverage::ReadWindow => "read-window",
    }
}

fn print_stats_summary(summary: &agent_tapes_core::stats_summary::StatsSummary) {
    println!(
        "active: {}ms, {} stretches, idle threshold {}ms",
        summary.active, summary.stretches, summary.idle_threshold
    );
    println!(
        "Recorded tool usage: {} selected, {} read, {} failed",
        summary.selected,
        summary.read,
        summary.failed.len()
    );
    for (harness, tools) in &summary.by_harness {
        println!(
            "{harness}: {} calls, {} results, {} complete pairs, {} incomplete, {} recorded errors",
            tools.calls,
            tools.results,
            tools.paired,
            tools.incomplete.total(),
            tools.errors
        );
        for row in &tools.by_name {
            println!(
                "  {}: {} calls, {} paired, {} errors",
                row.name, row.calls, row.paired, row.errors
            );
        }
    }
    for (harness, kinds) in &summary.kinds_by_harness {
        if !kinds.unobserved.is_empty() {
            println!(
                "{harness}: no session read holds a turn of {}, which the harness can record",
                kinds
                    .unobserved
                    .iter()
                    .map(|kind| kind.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    for session in &summary.sessions {
        println!(
            "  {} ({}): {} coverage",
            session.session.id,
            source_harness(&session.session.source),
            coverage_name(session.coverage.turns)
        );
    }
    for failure in &summary.failed {
        println!(
            "Unread {} ({}): {}",
            failure.id, failure.harness, failure.error
        );
    }
    print_diagnostics(
        summary.scan_truncated,
        summary.scanned,
        &summary.unreadable,
        &summary.unsearched,
        &summary.partial_search,
        &summary.unavailable,
    );
}

/// The continuation's own reading order: which session this is, where it
/// worked, where it stopped, what it left open, and what it last said. A fact
/// the recording does not carry has no line, so what is printed is what the
/// read established.
fn render_brief(brief: &Brief) -> String {
    let mut out = format!(
        "session: {} {}",
        source_harness(&brief.session.source),
        brief.session.id
    );
    match (&brief.session.title, &brief.session.derived_title) {
        (Some(title), _) => out.push_str(&format!(" {title}")),
        (None, Some(hint)) => out.push_str(&format!(" ~{hint}")),
        (None, None) => {}
    }
    out.push_str(&match brief.session.live {
        Some(LiveState::Working) => " [working]".to_owned(),
        Some(LiveState::Idle) => " [idle]".to_owned(),
        None => String::new(),
    });
    out.push('\n');

    out.push_str("working set: ");
    match &brief.working_set.directory {
        Some(directory) => {
            out.push_str(&directory.display().to_string());
            if !brief.working_set.directory_exists {
                out.push_str(" (gone)");
            }
        }
        None => out.push('-'),
    }
    if let Some(git) = &brief.working_set.git {
        out.push_str(&render_git(git));
    }
    out.push('\n');

    out.push_str(&format!(
        "ending: {}",
        brief
            .ending
            .last_turn
            .as_ref()
            .map_or_else(|| "-".to_owned(), |turn| speaker(&turn.role, turn.kind))
    ));
    for fact in &brief.ending.facts {
        out.push_str(&format!(" {}", fact.label()));
    }
    if !brief.ending.incomplete.is_empty() {
        out.push_str(&format!(
            " [incomplete: {}]",
            brief
                .ending
                .incomplete
                .iter()
                .map(|limit| limit.label())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out.push('\n');

    for call in &brief.in_flight.calls_without_result {
        out.push_str(&render_open_call(call));
    }
    for child in &brief.in_flight.children {
        out.push_str(&render_open_child(child));
    }

    out.push_str("tail:\n");
    for entry in &brief.tail {
        let heading = [
            Some(speaker(&entry.role, entry.kind)),
            Some(format!("#{}", entry.ordinal)),
            entry.ts.map(human_timestamp),
            entry
                .truncated
                .then(|| format!("cut at {TAIL_TEXT_CHARS} characters")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        out.push_str(&format!("[{heading}]\n{}\n", entry.text));
    }

    render_truncation_notes(&mut out, &brief.truncation);
    render_notes(&mut out, &brief.notes);
    out
}

fn render_git(git: &GitContext) -> String {
    let mut rendered = String::new();
    if let Some(head) = &git.head {
        rendered.push_str(&format!(" head {head}"));
    }
    if let Some(branch) = &git.branch {
        rendered.push_str(&format!(" branch {branch}"));
    }
    rendered
}

fn render_open_call(call: &OpenCall) -> String {
    let mut rendered = format!(
        "in flight: call {} {} #{}",
        call.name.as_deref().unwrap_or("-"),
        call.call_id.as_deref().unwrap_or("-"),
        call.ordinal
    );
    if let Some(ts) = call.ts {
        rendered.push_str(&format!(" {}", human_timestamp(ts)));
    }
    rendered.push('\n');
    rendered
}

fn render_open_child(child: &OpenChild) -> String {
    let mut rendered = format!(
        "in flight: child {} {}",
        child.reference,
        child.role.as_deref().unwrap_or("-")
    );
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
        source_harness(&ending.session.source),
        ending
            .session
            .last_activity_at
            .map_or_else(|| "unavailable".to_owned(), human_timestamp),
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
        source_harness(&lineage.session.source),
        lineage.session.id
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
    if let Some(owner) = &child.carried_from {
        rendered.push_str(&format!(" carried from {owner}"));
    }
    rendered.push('\n');
    rendered
}

/// One line per group, in the order the JSON carries them, and a group the
/// read holds nothing for has no line at all. The tool table is the only
/// multi-line group.
fn render_stats(stats: &StatsView) -> String {
    let mut out = format!(
        "session: {} {}",
        source_harness(&stats.session.source),
        stats.session.id
    );
    if let Some(model) = &stats.session.model {
        out.push_str(&format!(" {}", model.identity()));
    }
    out.push('\n');
    out.push_str(&format!("coverage: {}\n", render_coverage(&stats.coverage)));
    out.push_str(&format!("turns: {}\n", render_turn_kinds(&stats.turns)));
    if let Some(kinds) = &stats.kinds {
        out.push_str(&render_declaration(kinds));
    }
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
    if let Some(turns) = &stats.assistant_turns_ms {
        out.push_str(&format!(
            "assistant turns: {}\n",
            render_assistant_turns(turns)
        ));
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

fn parse_idle(value: &str) -> Result<i64, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    i64::try_from(duration.as_millis())
        .map_err(|_| "idle duration exceeds the millisecond range".to_owned())
}

fn render_time(durations: &TimeStats) -> String {
    [
        ("recorded span", durations.recorded_span),
        ("active", durations.active),
        ("idle threshold", durations.idle_threshold),
        ("in tool", durations.in_tool),
        ("longest gap between turns", durations.between_turns_max),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name} {value}ms")))
    .chain(
        durations
            .stretches
            .map(|count| format!("{count} stretches")),
    )
    .chain(std::iter::once(format!(
        "{} turns with timestamps",
        durations.count_with_timestamps
    )))
    .collect::<Vec<_>>()
    .join(", ")
}

/// One assistant record's wall clock against the record before it, counted
/// into the distribution beside the read's tool clock.
fn render_assistant_turns(durations: &AssistantTurnDurations) -> String {
    format!(
        "{} measured, median {}ms, p90 {}ms, max {}ms",
        durations.count, durations.median, durations.p90, durations.max
    )
}

fn render_usage_stats(out: &mut String, usage: &UsageStats) {
    render_measurement_coverage(
        out,
        coverage_label(usage.accounting.as_ref()),
        &usage.measurement_coverage,
    );
    for reason in &usage.measurement_incomplete {
        out.push_str(&format!("measurement incomplete: {reason}\n"));
    }
    if let Some(tokens) = &usage.tokens {
        out.push_str(&format!("tokens: {}\n", render_tokens(tokens)));
    }
    if let Some(cost) = &usage.cost {
        out.push_str(&format!("cost: {}\n", render_cost(cost)));
    }
    if let Some(estimate) = &usage.list_price_estimate {
        out.push_str(&render_estimate_lines("", estimate));
    }
    render_completeness(out, "", &usage.measurement_completeness);
    if let Some(accounting) = &usage.accounting {
        out.push_str(&format!("accounting: {}\n", render_accounting(accounting)));
    }
    for model in usage.by_model.iter().flatten() {
        out.push_str(&render_model_usage(model));
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

/// The kinds a harness cannot record, so their zeros are not read as
/// observations, and the default its reader gives a user turn.
fn render_declaration(kinds: &agent_tapes_core::model::KindDeclaration) -> String {
    let mut out = String::new();
    let unrecordable = TurnKind::ALL
        .into_iter()
        .filter(|kind| !kinds.recordable.keeps(*kind))
        .map(TurnKind::label)
        .collect::<Vec<_>>();
    if !unrecordable.is_empty() {
        out.push_str(&format!(
            "not recordable by this harness: {}\n",
            unrecordable.join(", ")
        ));
    }
    if let Some(default) = &kinds.user_default {
        out.push_str(&format!(
            "user turns without evidence: {}, because {}\n",
            default.kind.label(),
            default.basis
        ));
    }
    out
}

fn warning_label(warning: Warning) -> &'static str {
    match warning {
        Warning::ReadWindow => "read-window",
        Warning::TailWindow => "tail-window",
        Warning::KindUnknown => "kind-unknown",
        Warning::KindUndeclared => "kind-undeclared",
        Warning::IncompletePairs => "incomplete-pairs",
        Warning::NoTimestamps => "no-timestamps",
    }
}

/// One line per fact, and a fact the harness did not record has no line.
/// The closing notes are `show`'s, because the read behind them is the same.
fn render_span(span: &SpanPlacement) -> String {
    let bounds = match span.bounds {
        Some(SpanBounds::Time { from, to }) => format!(
            "{} .. {}",
            from.map_or_else(|| "start".to_owned(), |from| from.to_rfc3339()),
            to.map_or_else(|| "end".to_owned(), |to| to.to_rfc3339())
        ),
        Some(SpanBounds::Requests { first, last }) => format!(
            "requests {}..{}",
            first.map_or_else(String::new, |first| first.to_string()),
            last.map_or_else(String::new, |last| last.to_string())
        ),
        None => "remainder".to_owned(),
    };
    let mut out = format!(
        "span: {bounds}: {} of {} requests\n",
        span.requests, span.session_requests
    );
    for (label, request) in [("first", &span.first), ("last", &span.last)] {
        if let Some(request) = request {
            out.push_str(&format!("span {label}: request {}", request.ordinal));
            if let Some(at) = request.timestamp {
                out.push_str(&format!(" at {}", at.to_rfc3339()));
            }
            if let Some(id) = &request.id {
                out.push_str(&format!(" ({id})"));
            }
            out.push('\n');
        }
    }
    if span.unplaced > 0 {
        out.push_str(&format!(
            "span unplaced: {} requests or records could not be placed for or against it\n",
            span.unplaced
        ));
    }
    if span.overlapped > 0 {
        out.push_str(&format!(
            "span overlapped: {} requests are in another span too\n",
            span.overlapped
        ));
    }
    out
}

fn render_usage(usage: &UsageView) -> String {
    let mut out = format!(
        "session: {} {}",
        source_harness(&usage.session.source),
        usage.session.id
    );
    if let Some(model) = &usage.session.model {
        out.push_str(&format!(" {}", model.identity()));
    }
    out.push('\n');
    if let Some(span) = &usage.span {
        out.push_str(&render_span(span));
    }
    if let Some(tokens) = &usage.tokens {
        out.push_str(&format!("tokens: {}\n", render_tokens(tokens)));
    }
    if let Some(disjoint) = &usage.disjoint {
        out.push_str(&render_disjoint(disjoint));
    }
    if let Some(cost) = &usage.cost {
        out.push_str(&format!("cost: {}\n", render_cost(cost)));
    }
    if let Some(estimate) = &usage.list_price_estimate {
        out.push_str(&render_estimate_lines("", estimate));
    }
    if let Some(coverage) = &usage.cost_coverage {
        out.push_str(&format!(
            "cost coverage: {}\n",
            render_cost_coverage(coverage)
        ));
    }
    render_measurement_coverage(
        &mut out,
        coverage_label(usage.accounting.as_ref()),
        &usage.measurement_coverage,
    );
    if !usage.measurement_incomplete.is_empty() {
        out.push_str(&format!(
            "measurement incomplete: {}\n",
            usage.measurement_incomplete.join(", ")
        ));
    }
    render_completeness(&mut out, "", &usage.measurement_completeness);
    if usage.request_failures > 0 {
        out.push_str(&format!(
            "failed request observations: {}; consumption unknown\n",
            usage.request_failures
        ));
    }
    if usage.unrecognized_usage > 0 {
        out.push_str(&format!(
            "unrecognized records carrying usage: {}; their consumption is in no counter\n",
            usage.unrecognized_usage
        ));
    }
    if let Some(recorded) = &usage.recorded_total {
        out.push_str(&render_recorded_usage(recorded));
    }
    if let Some(accounting) = &usage.accounting {
        let within = if usage.span.is_some() {
            ", within the span"
        } else {
            ""
        };
        out.push_str(&format!(
            "accounting: {}{within}\n",
            render_accounting(accounting)
        ));
    }
    if let Some(status) = &usage.session.model_observation {
        out.push_str(&format!(
            "model observations: mixed={}, attribution-uncertain={}{}\n",
            status.mixed,
            status.attribution_uncertain,
            status
                .distinct_observed
                .map_or_else(String::new, |count| format!(", {count} distinct observed"))
        ));
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
    if let Some(unexplained) = &usage.unexplained {
        out.push_str(&render_unexplained("", unexplained));
    }
    if let Some(attribution) = &usage.attribution {
        out.push_str(&render_attribution(attribution));
    }
    if let Some(requests) = &usage.requests {
        out.push_str(&render_request_dimensions(requests));
    }
    if let Some(consumption) = &usage.consumption {
        out.push_str(&render_consumption(consumption));
    }
    if let Some(reconciliation) = &usage.reconciliation {
        out.push_str(&render_reconciliation(reconciliation));
    }
    if let Some(keys) = &usage.consumption_event_keys {
        out.push_str(&render_consumption_keys(keys));
    }
    if let Some(series) = &usage.series {
        out.push_str(&format!(
            "series: {} observations returned of {}; basis and counted status are read-local\n",
            series.returned, series.observed
        ));
        if series.omissions.row_cap > 0
            || series.omissions.byte_budget > 0
            || series.omissions.oversized_row > 0
        {
            out.push_str(&format!(
                "series omissions: row-cap {}, byte-budget {}, oversized {}\n",
                series.omissions.row_cap,
                series.omissions.byte_budget,
                series.omissions.oversized_row
            ));
        }
        for row in &series.rows {
            out.push_str(&render_observation(row));
        }
    }
    render_truncation_notes(&mut out, &usage.truncation);
    render_notes(&mut out, &usage.notes);
    if let Some(read) = usage.read.as_ref().filter(|read| {
        read.projection_options
            .iter()
            .any(|option| option == "full")
    }) {
        let length = match read.coordinate_domain.as_str() {
            "opencode-message" => format!("{} messages", read.source_length),
            _ => format!("source length {} bytes", read.source_length),
        };
        out.push_str(&format!(
            "Read evidence: the whole recording was streamed; {length}.\n"
        ));
    }
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

/// The disjoint categories and the native convention they were mapped from.
fn render_disjoint(disjoint: &DisjointTokens) -> String {
    let mut out = format!(
        "disjoint: {} [{}]\n",
        render_categories(&DisjointCells {
            uncached_input: disjoint.uncached_input,
            cache_read: disjoint.cache_read,
            cache_write: disjoint.cache_write.as_ref(),
            output: disjoint.output,
            reasoning: disjoint.reasoning,
            total: disjoint.total,
        }),
        render_convention(&disjoint.convention)
    );
    if !disjoint.missing.is_empty() {
        out.push_str(&format!(
            "disjoint missing: {}\n",
            disjoint.missing.join(", ")
        ));
    }
    render_disjoint_subtotals(
        &mut out,
        "disjoint",
        &DisjointCells {
            uncached_input: disjoint.uncached_input,
            cache_read: disjoint.cache_read,
            cache_write: disjoint.cache_write.as_ref(),
            output: disjoint.output,
            reasoning: disjoint.reasoning,
            total: disjoint.total,
        },
        &disjoint.measurement_completeness,
    );
    out
}

/// A line per incompletely measured category that holds a known amount,
/// then the categories and reasons that withheld the total.
fn render_disjoint_subtotals(
    out: &mut String,
    prefix: &str,
    cells: &DisjointCells<'_>,
    completeness: &MeasurementCompleteness,
) {
    for (category, present) in [
        ("uncached_input", cells.uncached_input.is_some()),
        ("cache_read", cells.cache_read.is_some()),
        ("cache_write", cells.cache_write.is_some()),
        ("output", cells.output.is_some()),
        ("reasoning", cells.reasoning.is_some()),
    ] {
        if let Some(reasons) = present
            .then(|| render_reasons(completeness, category))
            .flatten()
        {
            out.push_str(&format!("{prefix} {category}: known subtotal; {reasons}\n"));
        }
    }
    if cells.total.is_none() {
        if let Some(withheld) = render_withholders(completeness) {
            out.push_str(&format!("{prefix} total withheld: {withheld}\n"));
        }
    }
}

/// The disjoint categories one figure or one sum holds.
struct DisjointCells<'a> {
    uncached_input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<&'a CacheWrites>,
    output: Option<u64>,
    reasoning: Option<u64>,
    total: Option<u64>,
}

fn render_categories(cells: &DisjointCells<'_>) -> String {
    let mut parts = Vec::new();
    if let Some(count) = cells.uncached_input {
        parts.push(format!("uncached input {count}"));
    }
    if let Some(count) = cells.cache_read {
        parts.push(format!("cache read {count}"));
    }
    if let Some(writes) = cells.cache_write {
        parts.push(format!("cache write {}", render_cache_writes(writes)));
    }
    if let Some(count) = cells.output {
        parts.push(match cells.reasoning {
            Some(reasoning) => format!("output {count} (reasoning {reasoning})"),
            None => format!("output {count}"),
        });
    }
    if let Some(total) = cells.total {
        parts.push(format!("total {total}"));
    }
    parts.join(", ")
}

fn render_cache_writes(writes: &CacheWrites) -> String {
    [
        ("5m", writes.ephemeral_5m),
        ("1h", writes.ephemeral_1h),
        ("TTL unrecorded", writes.ttl_unrecorded),
    ]
    .into_iter()
    .filter_map(|(name, count)| count.map(|count| format!("{name} {count}")))
    .collect::<Vec<_>>()
    .join(" + ")
}

fn render_convention(convention: &TokenConvention) -> String {
    let input = match convention.input {
        InputConvention::ExcludesCache => "input excludes cache",
        InputConvention::IncludesCacheRead => "input includes cache read",
    };
    let output = match convention.output {
        OutputConvention::IncludesReasoning => "output includes reasoning",
        OutputConvention::ExcludesReasoning => "output excludes reasoning",
    };
    format!("native {input}, {output}")
}

fn render_counts(counts: impl IntoIterator<Item = (impl AsRef<str>, usize)>) -> String {
    counts
        .into_iter()
        .map(|(name, count)| format!("{} {count}", name.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What the read's requests recorded beside their tokens, one dimension per
/// clause; a dimension no request recorded has no clause.
fn render_request_dimensions(requests: &RequestDimensions) -> String {
    let mut parts = vec![format!("{} identified", requests.requests)];
    if requests.cache_write.total() > 0 {
        parts.push(format!(
            "cache write {}",
            render_cache_writes(&requests.cache_write)
        ));
    }
    for (name, values) in [
        ("service tier", &requests.service_tier),
        ("speed", &requests.speed),
        ("inference geo", &requests.inference_geo),
    ] {
        if !values.is_empty() {
            parts.push(format!(
                "{name} {}",
                render_counts(values.iter().map(|(value, count)| (value, *count)))
            ));
        }
    }
    for (name, count) in [
        ("web search", requests.web_search_requests),
        ("web fetch", requests.web_fetch_requests),
    ] {
        if let Some(count) = count {
            parts.push(format!("{name} {count}"));
        }
    }
    let mut out = format!("requests: {}\n", parts.join("; "));
    if !requests.missing.is_empty() {
        out.push_str(&format!(
            "requests missing: {}\n",
            render_counts(requests.missing.iter().map(|(name, count)| (name, *count)))
        ));
    }
    if !requests.conflicting.is_empty() {
        out.push_str(&format!(
            "requests conflicting: {}\n",
            render_counts(
                requests
                    .conflicting
                    .iter()
                    .map(|(name, count)| (name, *count))
            )
        ));
    }
    if requests.omitted_values > 0 {
        out.push_str(&format!(
            "requests omitted values: {}\n",
            requests.omitted_values
        ));
    }
    out
}

/// The recorded total, the identified union, and each counter's comparison,
/// then the per-model split and the sources read.
fn render_recorded_usage(recorded: &RecordedUsage) -> String {
    let mut out = format!(
        "set-aside recorded cost-state: {}\n",
        render_accounting(&recorded.accounting)
    );
    if let Some(tokens) = &recorded.tokens {
        out.push_str(&format!("  recorded tokens: {}\n", render_tokens(tokens)));
    }
    if let Some(cost) = &recorded.cost {
        out.push_str(&format!("  recorded cost: {}\n", render_cost(cost)));
    }
    if let Some(coverage) = &recorded.cost_coverage {
        out.push_str(&format!(
            "  recorded cost coverage: {}\n",
            render_cost_coverage(coverage)
        ));
    }
    for model in recorded.by_model.iter().flatten() {
        out.push_str(&format!("  recorded {}", render_model_usage(model)));
    }
    out
}

fn render_reconciliation(reconciliation: &Reconciliation) -> String {
    let mut out = format!("reconciliation: rules {}", reconciliation.rules);
    match &reconciliation.recorded {
        Some(recorded) => out.push_str(&format!(
            "; recorded total from the newest of {} cost-states, {} resets{}",
            recorded.observations,
            recorded.resets,
            if recorded.after_last_request {
                ""
            } else {
                ", comparison frontier unestablished"
            }
        )),
        None => out.push_str("; no recorded total"),
    }
    if let Some(recorded) = &reconciliation.recorded {
        if let Some(cost) = &recorded.cost {
            out.push_str(&format!(
                "; recorded cost {} [{}]",
                render_cost(cost),
                render_accounting(&recorded.accounting)
            ));
        }
        if let Some(coverage) = &recorded.cost_coverage {
            out.push_str(&format!(
                "; recorded cost coverage {}",
                render_cost_coverage(coverage)
            ));
        }
    }
    let identified = &reconciliation.identified;
    out.push_str(&format!(
        "; family request count {} across {} recordings; {} revised, {} conflicting identities{}\n",
        identified
            .requests
            .map_or_else(|| "unestablished".to_owned(), |count| count.to_string()),
        reconciliation.sources.len(),
        identified.revised,
        identified.conflicts,
        if identified.complete {
            ""
        } else {
            ", incomplete"
        }
    ));
    out.push_str(&format!(
        "request identity coverage: {}\n",
        identified.identity_coverage
    ));
    if !reconciliation.scope_established {
        out.push_str(&format!(
            "reconciliation scope: not established; missing {}\n",
            reconciliation.missing.join(", ")
        ));
    }
    for row in &reconciliation.counters {
        let figure =
            |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |value| value.to_string());
        let status = match row.status {
            CounterStatus::Reconciled => "reconciled",
            CounterStatus::Discrepancy => "discrepancy",
            CounterStatus::Conflict => "conflict",
            CounterStatus::Withheld => "withheld",
        };
        out.push_str(&format!(
            "  {}: recorded {}, identified {}, difference {}, {status}{}\n",
            row.counter,
            figure(row.recorded),
            figure(row.identified),
            row.difference
                .map_or_else(|| "-".to_owned(), |difference| difference.to_string()),
            if row.reasons.is_empty() {
                String::new()
            } else {
                format!(" ({})", row.reasons.join(", "))
            }
        ));
    }
    if let Some(unkeyed) = &identified.unkeyed {
        out.push_str(&format!(
            "  without request identity: {}\n",
            render_tokens(unkeyed)
        ));
    }
    for model in &reconciliation.by_model {
        out.push_str(&format!(
            "  model {}: recorded {}; identified {} in {} requests\n",
            model.model,
            model
                .recorded
                .as_ref()
                .map_or_else(|| "-".to_owned(), render_tokens),
            model
                .identified
                .as_ref()
                .map_or_else(|| "-".to_owned(), render_tokens),
            model
                .requests
                .map_or_else(|| "unestablished".to_owned(), |count| count.to_string())
        ));
        if let Some(cost) = &model.cost {
            out.push_str(&format!(
                "    recorded cost: {} [{}]\n",
                render_cost(cost),
                model
                    .accounting
                    .as_ref()
                    .map_or_else(String::new, render_accounting)
            ));
        }
        if let Some(coverage) = &model.cost_coverage {
            out.push_str(&format!(
                "    recorded cost coverage: {}\n",
                render_cost_coverage(coverage)
            ));
        }
    }
    for source in &reconciliation.sources {
        let role = match source.role {
            SourceRole::Parent => "parent".to_owned(),
            SourceRole::Child => {
                format!("child {}", source.agent_id.as_deref().unwrap_or_default())
            }
        };
        out.push_str(&format!(
            "  source {role}: {}, {} records, {} local requests, {} without identity, {} answered without \
             request, {} failed{}{}\n",
            if source.complete { "complete" } else { "incomplete" },
            source.records,
            source.requests,
            source.unkeyed,
            source.unrequested,
            source.failed,
            if source.unrecognized_usage > 0 {
                format!(", {} unrecognized with usage", source.unrecognized_usage)
            } else {
                String::new()
            },
            source
                .error
                .as_ref()
                .map_or_else(String::new, |error| format!("; {error}"))
        ));
        if let Some(tokens) = &source.identified {
            out.push_str(&format!(
                "    local request tokens: {}\n",
                render_tokens(tokens)
            ));
        }
        if let Some(tokens) = &source.unkeyed_tokens {
            out.push_str(&format!(
                "    without request identity: {}\n",
                render_tokens(tokens)
            ));
        }
    }
    render_notes(&mut out, &reconciliation.notes);
    out
}

/// Four decimal places, so a fraction of a cent is neither rounded away nor
/// dressed up as more precision than the harness recorded. Every recorded
/// cost is the harness's own estimate on a basis the recording does not
/// state.
fn render_cost_coverage(coverage: &CostCoverage) -> String {
    format!("{}; {} zero-written requests, {} zero-written observations; any cost is a known priced subtotal",
        if coverage.complete { "complete" } else { "incomplete" },
        coverage.zero_written_requests, coverage.zero_written_observations)
}

/// A list-price estimate as one phrase: the amount or its bounds, marked
/// as an estimate from the caller's table, with the rows it used.
fn render_estimate(estimate: &ListPriceEstimate) -> String {
    let amount = match (estimate.amount, estimate.at_least, estimate.at_most) {
        (Some(amount), _, _) => format!("{amount:.4}"),
        (None, Some(low), Some(high)) => format!("{low:.4} to {high:.4}"),
        _ => "unpriced".to_owned(),
    };
    let models = estimate
        .models
        .iter()
        .map(|model| format!("{} effective {}", model.id, model.effective))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "list-price estimate {amount} {} (from the price table: {models})",
        estimate.currency
    )
}

fn estimate_incomplete(estimate: &ListPriceEstimate) -> Option<String> {
    render_incomplete_names(
        &estimate.measurement_completeness,
        &agent_tapes_core::pricing::PRICED_CATEGORIES,
        "priced categories",
    )
}

fn render_estimate_lines(prefix: &str, estimate: &ListPriceEstimate) -> String {
    let mut out = format!("{prefix}{}\n", render_estimate(estimate));
    for billing in &estimate.billing {
        let terms = match billing.terms {
            agent_tapes_core::pricing::BillingTerms::Subscription => {
                "subscription terms, no long-context surcharge"
            }
            agent_tapes_core::pricing::BillingTerms::Api => "per-token API terms",
            agent_tapes_core::pricing::BillingTerms::Unestablished => {
                "billing unestablished, between base and long-context rates"
            }
        };
        out.push_str(&format!(
            "{prefix}list-price billing: {terms} ({})\n",
            billing.evidence
        ));
    }
    if let Some(incomplete) = estimate_incomplete(estimate) {
        out.push_str(&format!(
            "{prefix}list-price estimate completeness: {incomplete}\n"
        ));
    }
    out
}

fn render_cost(cost: &Cost) -> String {
    format!(
        "{:.4} USD (harness-calculated estimate, pricing basis not recorded)",
        cost.usd
    )
}

fn render_accounting(accounting: &Accounting) -> String {
    let basis = match accounting.basis {
        AccountingBasis::RecordedTotal => "a recorded total",
        AccountingBasis::SummedRequests => "a sum of per-request records",
    };
    let (selection, coverage) = match (accounting.selection, accounting.coverage) {
        (_, AccountingCoverage::SinceReset) => ("", "since the observed counter reset"),
        (None, AccountingCoverage::Session) => ("", "the whole session"),
        (None, AccountingCoverage::ReadWindow) => ("", "the bounded read window"),
        (Some(AccountingSelection::ActiveBranch), AccountingCoverage::Session) => {
            (" along the active branch", "the whole branch")
        }
        (Some(AccountingSelection::ActiveBranch), AccountingCoverage::ReadWindow) => (
            " along the active branch",
            "only the part of it the read could link",
        ),
        (Some(AccountingSelection::RetainedBranches), AccountingCoverage::Session) => {
            (" over every retained branch", "the whole recording")
        }
        (Some(AccountingSelection::RetainedBranches), AccountingCoverage::ReadWindow) => {
            (" over every retained branch", "the bounded read window")
        }
    };
    format!("{basis}{selection}, covering {coverage}")
}

/// What every retained branch consumed, beside the session's own counters
/// when those select one branch.
fn render_consumption(consumption: &Consumption) -> String {
    let mut figures = Vec::new();
    if let Some(tokens) = &consumption.tokens {
        figures.push(format!("tokens {}", render_tokens(tokens)));
    }
    if let Some(cost) = &consumption.cost {
        figures.push(format!("cost {}", render_cost(cost)));
    }
    if let Some(estimate) = &consumption.list_price_estimate {
        figures.push(render_estimate(estimate));
    }
    let events = &consumption.events;
    let identity = &consumption.identity;
    let mut out = format!(
        "consumption: {}; {}\n",
        figures.join(", "),
        render_accounting(&consumption.accounting)
    );
    if let Some(coverage) = &consumption.cost_coverage {
        out.push_str(&format!(
            "consumption cost coverage: {}\n",
            render_cost_coverage(coverage)
        ));
    }
    out.push_str(&format!(
        "consumption events: {} counted, {} on the active branch, {} on other branches, {} repeats\n",
        events.counted, events.active_branch, events.other_branches, events.repeats
    ));
    render_measurement_coverage(&mut out, "consumption", &consumption.measurement_coverage);
    render_completeness(
        &mut out,
        "consumption ",
        &consumption.measurement_completeness,
    );
    out.push_str(&format!(
        "consumption identity: {} by observed provider response (account namespace unestablished), {} by recording entry, {} unidentified",
        identity.provider_response, identity.recording_entry, identity.unidentified
    ));
    if let Some(parent) = &identity.forked_from {
        out.push_str(&format!(
            "; forked from {parent}, whose events it may repeat"
        ));
    }
    if identity.fork_parent_unestablished {
        out.push_str("; fork parent identity unestablished");
    }
    out.push('\n');
    if !consumption.incomplete.is_empty() {
        out.push_str(&format!(
            "consumption incomplete: {}\n",
            consumption.incomplete.join(", ")
        ));
    }
    out
}

/// What a harness's counters select, as a coverage line names the figure:
/// the active branch where a recording retains others, else the session.
fn coverage_label(accounting: Option<&Accounting>) -> &'static str {
    match accounting.and_then(|accounting| accounting.selection) {
        Some(AccountingSelection::ActiveBranch) => "active",
        _ => "session",
    }
}

/// The reasons one counter or category is incompletely measured, in the
/// fixed labels `measurement_completeness` carries.
fn render_reasons(completeness: &MeasurementCompleteness, name: &str) -> Option<String> {
    completeness
        .iter()
        .find(|(counter, _)| *counter == name)
        .map(|(_, reasons)| {
            reasons
                .iter()
                .map(|reason| reason.label())
                .collect::<Vec<_>>()
                .join(", ")
        })
}

/// The incompletely measured `names` of one figure, each distinct reason set
/// once with the names that share it: `input: missing-counter; output,
/// cache_read: stopped-attempt`. A set covering all of `names` reads `all
/// <noun>`.
fn render_incomplete_names(
    completeness: &MeasurementCompleteness,
    names: &[&str],
    noun: &str,
) -> Option<String> {
    let mut groups: std::collections::BTreeMap<String, Vec<&str>> = Default::default();
    for name in names {
        if let Some(reasons) = render_reasons(completeness, name) {
            groups.entry(reasons).or_default().push(name);
        }
    }
    let clauses = groups
        .into_iter()
        .map(|(reasons, members)| {
            let members = if members.len() == names.len() {
                format!("all {noun}")
            } else {
                members.join(", ")
            };
            format!("{members}: {reasons}")
        })
        .collect::<Vec<_>>();
    (!clauses.is_empty()).then(|| clauses.join("; "))
}

/// The line naming a figure's incomplete counters, where it has any.
fn render_completeness(out: &mut String, prefix: &str, completeness: &MeasurementCompleteness) {
    if let Some(incomplete) = render_incomplete_names(completeness, &COUNTERS, "counters") {
        out.push_str(&format!("{prefix}completeness: {incomplete}\n"));
    }
}

/// The summed categories that withheld a disjoint total, with their reasons.
fn render_withholders(completeness: &MeasurementCompleteness) -> Option<String> {
    render_incomplete_names(completeness, &SUMMED_CATEGORIES, "summed categories")
}

fn render_measurement_coverage(
    out: &mut String,
    prefix: &str,
    counters: &std::collections::BTreeMap<String, agent_tapes_core::usage::MeasurementCoverage>,
) {
    for (counter, coverage) in counters {
        if coverage.missing > 0 || coverage.unpriced > 0 {
            out.push_str(&format!(
                "{prefix} {counter}: known subtotal; {}/{} events measured, {} missing, {} unpriced\n",
                coverage.measured, coverage.measured + coverage.missing + coverage.unpriced, coverage.missing, coverage.unpriced
            ));
        }
    }
}

fn render_consumption_keys(series: &ConsumptionEventKeys) -> String {
    let observed = series
        .observed
        .map_or_else(|| "unmeasured".to_owned(), |count| count.to_string());
    let mut out = format!("consumption event keys: {} returned of {observed}; provider account namespace unestablished, recording entries local\n", series.returned);
    let omitted = &series.omissions;
    if omitted.row_cap
        + omitted.byte_budget
        + omitted.oversized_key
        + omitted.unidentified
        + omitted.private_or_invalid_identity
        + omitted.unavailable_source
        > 0
    {
        out.push_str(&format!("consumption key omissions: row-cap {}, byte-budget {}, oversized {}, unidentified {}, private-or-invalid {}, unavailable-source {}\n", omitted.row_cap, omitted.byte_budget, omitted.oversized_key, omitted.unidentified, omitted.private_or_invalid_identity, omitted.unavailable_source));
    }
    if let Some(reason) = series.unavailable {
        out.push_str(&format!("consumption keys unavailable: {reason}\n"));
    }
    if let (Some(coverage), Some(unreadable)) = (series.coverage, series.unreadable_records) {
        out.push_str(&format!(
            "consumption key coverage: {coverage:?}; {unreadable} unreadable records\n"
        ));
    }
    if let Some(coverage) = series.copy_coverage {
        out.push_str(&format!(
            "fork copy coverage: {coverage:?}; {} conflicts\n",
            series.copy_conflicts
        ));
    }
    for key in &series.keys {
        out.push_str(&format!("  {:?}", key.kind));
        if let (Some(provider), Some(response)) = (&key.provider, &key.response_id) {
            out.push_str(&format!(" {provider} {response}"));
        }
        if let Some(entry) = &key.recording_entry {
            out.push_str(&format!(
                "; recording {} entry {}",
                entry.session_id, entry.entry_id
            ));
        }
        if let Some(copy) = &key.copy_of {
            out.push_str(&format!(
                "; verified copy of {} entry {}",
                copy.session_id, copy.entry_id
            ));
        }
        if let Some(measurement) = &key.measurement {
            out.push_str(&render_event_measurement(measurement));
        }
        out.push('\n');
    }
    out
}

/// One event's amounts on its key's line: the disjoint total or the
/// categories it holds, the cost, and the counters it did not record.
fn render_event_measurement(measurement: &ConsumptionEventMeasurement) -> String {
    let mut out = String::new();
    match &measurement.disjoint {
        Some(disjoint) => match disjoint.total {
            Some(total) => out.push_str(&format!("; tokens {total}")),
            None => out.push_str("; tokens known subtotal"),
        },
        None => out.push_str("; tokens unmeasured"),
    }
    match &measurement.cost {
        Some(cost) => out.push_str(&format!("; cost ${:.4}", cost.usd)),
        None => out.push_str("; cost unmeasured"),
    }
    if !measurement.measurement_completeness.is_empty() {
        let incomplete = measurement
            .measurement_completeness
            .iter()
            .map(|(counter, _)| counter)
            .collect::<Vec<_>>();
        out.push_str(&format!("; incomplete: {}", incomplete.join(", ")));
    }
    out
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
    if let Some(credits) = &limits.credits {
        if let Some(balance) = &credits.balance {
            out.push_str(&format!(
                "credits balance: {}\n",
                render_recorded_value(balance)
            ));
        }
        if let Some(has_credits) = credits.has_credits {
            out.push_str(&format!("credits available: {has_credits}\n"));
        }
        if let Some(unlimited) = credits.unlimited {
            out.push_str(&format!("credits unlimited: {unlimited}\n"));
        }
    }
    if let Some(reached) = limits.spend_control_reached {
        out.push_str(&format!("spend control reached: {reached}\n"));
    }
    if let Some(reached) = limits.rate_limit_reached {
        out.push_str(&format!("rate limit reached: {reached}\n"));
    }
    if let Some(kind) = &limits.rate_limit_reached_type {
        out.push_str(&format!("rate limit reached type: {kind}\n"));
    }
    if let Some(observed_at) = limits.observed_at {
        out.push_str(&format!(
            "quota observed: {}\n",
            human_timestamp(observed_at)
        ));
    }
}

fn render_rate_window(window: &RateWindow) -> String {
    let mut rendered = format!("{}% used", render_recorded_value(&window.used_percent));
    if let Some(minutes) = window.window_minutes {
        rendered.push_str(&format!(" of a {minutes}-minute window"));
    }
    if let Some(resets_at) = window.resets_at {
        rendered.push_str(&format!(", resets {}", human_timestamp(resets_at)));
    }
    rendered
}

fn render_recorded_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
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

/// The part of a recorded total that no request explains, model by model,
/// how a span or window placed it, and what it could not place.
fn render_unexplained(prefix: &str, unexplained: &Unexplained) -> String {
    let whose = match unexplained.explained_by {
        Some(ExplainedBy::SessionRequests) => "the session's requests",
        Some(ExplainedBy::FamilyRequests) => "the requests of the session and its subagents",
        None => "the sessions' requests",
    };
    let entry = |model: &UnexplainedModel| {
        let estimate = model
            .list_price_estimate
            .as_ref()
            .map_or_else(String::new, |estimate| {
                format!("; {}", render_estimate(estimate))
            });
        format!(
            "model {}: {}{estimate}",
            model.model,
            render_tokens(&model.tokens)
        )
    };
    let mut out = String::new();
    for model in &unexplained.by_model {
        out.push_str(&format!(
            "{prefix}recorded total not explained by {whose}: {}\n",
            entry(model)
        ));
    }
    match unexplained.placement {
        Some(Placement::Bounded) if !unexplained.by_model.is_empty() => out.push_str(&format!(
            "{prefix}unexplained placement: bounded, each recorded total's interval held whole\n"
        )),
        Some(Placement::Estimated { basis, .. }) => out.push_str(&format!(
            "{prefix}unexplained placement: estimated, an interval between recorded totals \
             shared where cut ({basis})\n"
        )),
        _ => {}
    }
    let reasons = unexplained
        .unplaced_reasons
        .iter()
        .map(serde_kebab)
        .collect::<Vec<_>>()
        .join(", ");
    for model in &unexplained.unplaced {
        out.push_str(&format!(
            "{prefix}unexplained and not placed ({reasons}), held by the remainder: {}\n",
            entry(model)
        ));
    }
    out
}

fn render_model_usage(model: &ModelUsage) -> String {
    let mut rendered = format!(
        "model {}{}:",
        model.model,
        model
            .variant
            .as_ref()
            .map_or_else(String::new, |variant| format!(" ({variant})"))
    );
    if let Some(tokens) = &model.tokens {
        rendered.push_str(&format!(" {}", render_tokens(tokens)));
    }
    if let Some(cost) = &model.cost {
        rendered.push_str(&format!("; {}", render_cost(cost)));
    }
    if let Some(coverage) = &model.cost_coverage {
        rendered.push_str(&format!(
            "; cost coverage {}",
            render_cost_coverage(coverage)
        ));
    }
    if let Some(requests) = model.request_count {
        rendered.push_str(&format!("; {requests} observed requests"));
    }
    if let Some(estimate) = &model.list_price_estimate {
        rendered.push_str(&format!("; {}", render_estimate(estimate)));
    }
    rendered.push('\n');
    if let Some(estimate) = &model.list_price_estimate {
        if let Some(incomplete) = estimate_incomplete(estimate) {
            rendered.push_str(&format!(
                "model {} list-price estimate completeness: {incomplete}\n",
                model.model
            ));
        }
    }
    render_measurement_coverage(
        &mut rendered,
        &format!("model {}", model.model),
        &model.measurement_coverage,
    );
    if !model.measurement_incomplete.is_empty() {
        rendered.push_str(&format!(
            "model {} measurement incomplete: {}\n",
            model.model,
            model.measurement_incomplete.join(", ")
        ));
    }
    render_completeness(
        &mut rendered,
        &format!("model {} ", model.model),
        &model.measurement_completeness,
    );
    rendered
}

fn render_attribution(attribution: &UsageAttribution) -> String {
    let basis = match attribution.basis {
        agent_tapes_core::usage::ObservationBasis::UsageRecord => "usage-record",
        agent_tapes_core::usage::ObservationBasis::TokenEventAdvance => "token-event-advance",
    };
    let coverage = match attribution.coverage {
        AccountingCoverage::Session => "the whole session",
        AccountingCoverage::ReadWindow => "the bounded read window",
        AccountingCoverage::SinceReset => "the portion since an observed reset",
    };
    let mut rendered = format!(
        "attribution: basis {basis}, covering {coverage}; {} observed, {} counted, {} attributed, {} unattributed, {} leading uncounted, {} resets\n",
        attribution.observed,
        attribution.counted,
        attribution.attributed,
        attribution.unattributed,
        attribution.leading_uncounted,
        attribution.resets
    );
    if !attribution.incomplete.is_empty() {
        rendered.push_str(&format!(
            "attribution incomplete: {}\n",
            attribution.incomplete.join(", ")
        ));
    }
    if !attribution.bounds.is_empty() {
        rendered.push_str(&format!(
            "attribution bounds: {}\n",
            attribution.bounds.join(", ")
        ));
    }
    if attribution.outside_total > 0 {
        rendered.push_str(&format!(
            "outside the recorded total: {} requests{}\n",
            attribution.outside_total,
            attribution
                .outside_total_tokens
                .as_ref()
                .map(|tokens| format!(", {}", render_tokens(tokens)))
                .unwrap_or_default()
        ));
    }
    rendered
}

fn render_observation(row: &UsageObservation) -> String {
    let classification = match row.classification {
        ObservationClassification::Request => "request",
        ObservationClassification::Repeat => "repeat",
        ObservationClassification::Advance => "advance",
        ObservationClassification::LeadingUncounted => "leading-uncounted",
        ObservationClassification::UnchangedTotal => "unchanged-total",
        ObservationClassification::ResetAdvance => "reset-advance",
        ObservationClassification::QuotaOnly => "quota-only",
        ObservationClassification::Unattributed => "unattributed",
        ObservationClassification::Incomplete => "incomplete",
    };
    let ordinal = row.native_ordinal.map_or_else(
        || "ordinal -".to_owned(),
        |ordinal| format!("ordinal {ordinal}"),
    );
    let model = row.model.as_ref().map_or_else(String::new, |model| {
        row.variant.as_ref().map_or_else(
            || format!(" model {model}"),
            |variant| format!(" model {model} ({variant})"),
        )
    });
    format!(
        "  observation {} {} {}{}{}\n",
        row.record_type,
        ordinal,
        classification,
        if row.counted {
            " counted"
        } else {
            " uncounted"
        },
        model
    )
}

/// One row per group and a closing total, each counter cell naming how many
/// of the row's sessions recorded it whenever that is fewer than all of
/// them. A counter no session recorded has no figure to print.
fn render_window(window: &WindowRecord) -> String {
    let edge =
        |at: Option<Instant>, open: &str| at.map_or_else(|| open.to_owned(), |at| at.to_rfc3339());
    let mut out = format!(
        "window: {} .. {} ({})\n",
        edge(window.bounds.from, "start"),
        edge(window.bounds.to, "now"),
        serde_kebab(&window.boundary)
    );
    if let Some(quota) = &window.quota {
        out.push_str(&format!(
            "quota: {} {} window of {} minutes resetting {}, observed in {}{}\n",
            quota.harness,
            serde_kebab(&quota.observation.quota),
            quota.observation.window_minutes,
            quota.observation.resets_at.to_rfc3339(),
            quota.session,
            quota
                .inferred
                .map_or_else(String::new, |reason| format!("; inferred: {reason}"))
        ));
    }
    let allowance = window
        .sessions
        .iter()
        .any(|share| !share.allowance.is_empty());
    out.push_str("SESSION\tHARNESS\tLAUNCH\tROOT\tREQUESTS\tDISJOINT TOKENS");
    out.push_str(if allowance { "\tALLOWANCE\n" } else { "\n" });
    let shared = shared_ids(window.sessions.iter().flat_map(|share| {
        [
            (share.harness.as_str(), share.id.as_str()),
            (share.root_harness.as_str(), share.root.as_str()),
        ]
    }));
    for share in &window.sessions {
        let root = session_cell(&share.root_harness, &share.root, &shared);
        let root = if (&share.root_harness, &share.root) == (&share.harness, &share.id) {
            "-".to_owned()
        } else if share.root_resolved {
            root
        } else {
            format!("{root} (unresolved)")
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{} of {}{}\t{}\n",
            share.id,
            share.harness,
            share.launch.map_or("-", |launch| launch.as_str()),
            root,
            share.requests,
            share.session_requests,
            if share.unplaced > 0 {
                format!(" ({} unplaced)", share.unplaced)
            } else {
                String::new()
            },
            share.consumed()
        ));
        if allowance {
            out.pop();
            let cell = if share.allowance.is_empty() {
                "-".to_owned()
            } else {
                share
                    .allowance
                    .iter()
                    .map(render_allowance)
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            out.push_str(&format!("\t{cell}\n"));
        }
    }
    for pool in &window.allowance {
        out.push_str(&format!(
            "allowance pool {} {} {}{} window: observed {}, attributed {}, outside the selection {}, without recorded use {} ({} observations, {} intervals)\n",
            pool.pool.harness,
            pool.pool.plan.as_deref().unwrap_or("-"),
            serde_kebab(&pool.pool.window),
            pool.pool
                .window_minutes
                .map_or_else(String::new, |minutes| format!(" {minutes}-minute")),
            render_percent(&pool.observed),
            render_percent(&pool.attributed),
            render_percent(&pool.outside_selection),
            render_percent(&pool.without_recorded_use),
            pool.coverage.observations,
            pool.coverage.intervals,
        ));
    }
    if let Some(assumption) = window.allowance_assumption {
        out.push_str(&format!("allowance pool assumption: {assumption}\n"));
    }
    if window.empty > 0 {
        out.push_str(&format!(
            "{} selected sessions placed no request inside the window\n",
            window.empty
        ));
    }
    out.push('\n');
    out
}

fn render_percent(percent: &agent_tapes_core::allowance::PercentBounds) -> String {
    format!(
        "{:.1}% ({:.1}-{:.1})",
        percent.estimate, percent.at_least, percent.at_most
    )
}

fn render_allowance(allowance: &agent_tapes_core::allowance::Allowance) -> String {
    format!(
        "{} {}",
        serde_kebab(&allowance.window),
        render_percent(&allowance.percent)
    )
}

fn render_allowance_rates(
    rates: &agent_tapes_core::allowance::AllowanceRates,
    by: &[GroupBy],
) -> String {
    let edge = |at: Option<Instant>| at.map_or_else(|| "open".to_owned(), |at| at.to_rfc3339());
    let mut out = format!(
        "allowance rates: {} window, lookback {} .. {}, weighted by {}\n",
        serde_kebab(&rates.window),
        edge(rates.lookback.from),
        edge(rates.lookback.to),
        serde_kebab(&rates.weighting)
    );
    let mut columns = by.iter().map(key_column).collect::<Vec<_>>();
    columns.extend([
        "PLAN",
        "LIMIT",
        "WINDOW MIN",
        "LANES",
        "%/LANE MEDIAN",
        "P90",
        "MAX",
        "SMOOTHED %/LANE MEDIAN",
        "TOKENS/% TOTAL",
        "UNCACHED",
        "CACHE READ",
        "OUTPUT",
        "REASONING",
        "MEDIAN MIN",
        "MEASURED",
    ]);
    out.push_str(&columns.join("\t"));
    out.push('\n');
    let shared = SharedKeyIds::of(rates.groups.iter().map(|group| &group.key));
    for group in &rates.groups {
        let mut cells = by
            .iter()
            .map(|dimension| key_cell(&group.key, *dimension, &shared).unwrap_or_default())
            .collect::<Vec<_>>();
        let stat_of =
            |lane: &agent_tapes_core::allowance::LanePercent,
             pick: fn(&agent_tapes_core::allowance::Quantiles) -> f64| {
                format!(
                    "{:.2} ({:.2}-{:.2})",
                    pick(&lane.estimate),
                    pick(&lane.at_least),
                    pick(&lane.at_most)
                )
            };
        let stat = |pick: fn(&agent_tapes_core::allowance::Quantiles) -> f64| {
            stat_of(&group.percent_per_lane, pick)
        };
        let ratio = |ratio: &agent_tapes_core::allowance::TokensPerPercent| {
            format!("{:.0}", ratio.estimate)
        };
        cells.push(group.plan.clone().unwrap_or_else(|| "-".to_owned()));
        cells.push(group.limit.clone().unwrap_or_else(|| "-".to_owned()));
        cells.push(
            group
                .window_minutes
                .map_or_else(|| "-".to_owned(), |minutes| minutes.to_string()),
        );
        cells.push(group.lanes.to_string());
        cells.push(stat(|quantiles| quantiles.median));
        cells.push(stat(|quantiles| quantiles.p90));
        cells.push(stat(|quantiles| quantiles.max));
        cells.push(stat_of(&group.smoothed_percent_per_lane, |quantiles| {
            quantiles.median
        }));
        match &group.tokens_per_percent {
            Some(split) => cells.extend([
                ratio(&split.total),
                ratio(&split.uncached_input),
                ratio(&split.cache_read),
                ratio(&split.output),
                ratio(&split.reasoning),
            ]),
            None => cells.extend(std::iter::repeat_n("-".to_owned(), 5)),
        }
        cells.push(format!("{:.1}", group.median_duration_minutes));
        cells.push(format!(
            "{} .. {}",
            group.measured_over.from.to_rfc3339(),
            group.measured_over.to.to_rfc3339()
        ));
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out.push_str(&format!(
        "left out: {} running, {} without allowance, {} from harnesses that record no used percent\n",
        rates.excluded_running,
        rates.excluded_without_allowance,
        rates.excluded_absent.len()
    ));
    out.push_str(&render_lanes_read(&rates.lanes_read));
    out
}

fn render_lanes_read(lanes: &agent_tapes_core::allowance::LanesRead) -> String {
    let mut out = format!(
        "lanes read: up to {} sessions per harness ({})",
        lanes.limit,
        if lanes.explicit_limit {
            "--limit"
        } else {
            "the lane bound"
        }
    );
    if lanes.left_out_by_limit > 0 {
        out.push_str(&format!(
            "; {} sessions beyond it left out",
            lanes.left_out_by_limit
        ));
    }
    if lanes.lane_bound_reached {
        out.push_str("; the listing reached its bound, so sessions beyond it were not read");
    }
    out.push('\n');
    out
}

fn render_forecast(forecast: &agent_tapes_core::allowance::AllowanceForecast) -> String {
    let bounds = |bounds: &agent_tapes_core::allowance::PercentBounds, digits: usize| {
        let open = |value: f64| {
            if value.is_finite() {
                format!("{value:.digits$}")
            } else {
                "open".to_owned()
            }
        };
        format!(
            "{} ({}-{})",
            open(bounds.estimate),
            open(bounds.at_least),
            open(bounds.at_most)
        )
    };
    let state = &forecast.window_state;
    let mut out = format!(
        "forecast: {} {} window of {}, plan {}\n",
        state.pool.harness,
        serde_kebab(&forecast.window),
        state.window_minutes.map_or_else(
            || "unrecorded length".to_owned(),
            |minutes| format!("{minutes} minutes")
        ),
        state.pool.plan.as_deref().unwrap_or("unrecorded"),
    );
    out.push_str(&format!(
        "newest observation {}: {}% used, {}% left{}{}\n",
        state.observed_at.to_rfc3339(),
        state.used_percent,
        bounds(&state.remaining_percent, 1),
        state
            .resets_at
            .map(|reset| format!(", resets {}", reset.to_rfc3339()))
            .unwrap_or_default(),
        if state.reset_since_observed {
            " (passed: the window is taken as whole)"
        } else {
            ""
        }
    ));
    out.push_str("MODEL\tVARIANT\tLANES\tMEASURED\tMEDIAN TOKENS\tTOKENS/%\t%/LANE\t%\n");
    for lanes in &forecast.plan {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{:.0}\t{:.0}\t{}\t{}\n",
            lanes.model,
            lanes.variant.as_deref().unwrap_or("-"),
            lanes.lanes,
            lanes.measured_lanes,
            lanes.median_lane_tokens,
            lanes.tokens_per_percent.estimate,
            bounds(&lanes.percent_per_lane, 2),
            bounds(&lanes.percent, 2),
        ));
    }
    out.push_str(&render_lanes_read(&forecast.lanes_read));
    out.push_str(&format!(
        "mix: {}% of the window, fits: {}\n",
        bounds(&forecast.percent, 2),
        serde_kebab(&forecast.fits)
    ));
    out.push_str(&format!(
        "with {} lanes at once: {}%/min, the window spent in {} min{}\n",
        forecast.concurrency,
        bounds(&forecast.percent_per_minute, 4),
        bounds(&forecast.minutes_to_exhaustion, 0),
        match forecast.exhausted_before_reset {
            Some(true) => ", before the reset",
            Some(false) => ", not before the reset",
            None => "",
        }
    ));
    out
}

fn render_usage_summary(summary: &UsageSummary, by: &[GroupBy]) -> String {
    let key_columns: Vec<&str> = if by.is_empty() {
        vec!["GROUP"]
    } else {
        by.iter().map(key_column).collect()
    };
    let priced = std::iter::once(&summary.totals)
        .chain(summary.groups.iter().map(|group| &group.tally))
        .any(|tally| tally.list_price_estimate.is_some() || !tally.list_price_withheld.is_empty());
    let tally_cells = |tally: &UsageTally| {
        let mut cells = tally_cells(tally);
        if priced {
            cells.push(estimate_cell(tally));
        }
        cells
    };
    let mut out = key_columns.join("\t");
    out.push_str(
        "\tSESSIONS\tINPUT\tOUTPUT\tREASONING\tCACHE READ\tCACHE WRITE\tDISJOINT TOTAL\tCOST\t\
         COVERAGE (RECORDED/SUMMED/WINDOW/SINCE-RESET/NONE)",
    );
    out.push_str(if priced {
        "\tLIST-PRICE ESTIMATE\n"
    } else {
        "\n"
    });
    let shared = SharedKeyIds::of(summary.groups.iter().map(|group| &group.key));
    for group in &summary.groups {
        let mut cells: Vec<String> = by
            .iter()
            .map(|dimension| key_cell(&group.key, *dimension, &shared).unwrap_or_default())
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
    if let Some(disjoint) = &summary.totals.disjoint {
        out.push_str(&render_disjoint_sum(disjoint));
    }
    if summary
        .groups
        .iter()
        .any(|group| group.tally.cost.is_some())
        || summary
            .partitions
            .iter()
            .any(|partition| partition.tally.cost.is_some())
    {
        out.push_str(
            "Cost is each harness's own estimate; no recording states its pricing basis.\n",
        );
    }
    if let Some(coverage) = &summary.totals.cost_coverage {
        out.push_str(&format!(
            "Cost coverage: {}\n",
            render_cost_coverage(coverage)
        ));
    }
    if let Some(estimate) = &summary.totals.list_price_estimate {
        out.push_str(&format!("Total {}", render_estimate_lines("", estimate)));
    }
    if let Some(unexplained) = &summary.totals.unexplained {
        out.push_str(&render_unexplained("Total ", unexplained));
    }
    if !summary.totals.list_price_withheld.is_empty() {
        out.push_str(&format!(
            "List-price estimate withheld: {}\n",
            summary
                .totals
                .list_price_withheld
                .iter()
                .map(serde_kebab)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    render_measurement_coverage(&mut out, "summed", &summary.totals.measurement_coverage);
    if summary.totals.mixed_accounting {
        out.push_str(
            "Mixed accounting: total counters are omitted; compatible partitions follow.\n",
        );
    }
    render_identity_bounds(&mut out, &summary.totals);
    if summary.totals.mixed_models > 0 || summary.totals.attribution_uncertain > 0 {
        out.push_str(&format!(
            "Model observations: {} mixed, {} attribution-uncertain sessions; grouping uses the latest observed model only.\n",
            summary.totals.mixed_models,
            summary.totals.attribution_uncertain
        ));
    }
    for partition in &summary.partitions {
        out.push_str(&format!(
            "Accounting partition {} ({}): {} sessions; {}\n",
            partition.harness,
            partition
                .accounting
                .as_ref()
                .map(render_accounting)
                .unwrap_or_else(|| "no accounting".to_owned()),
            partition.tally.sessions,
            tally_cells(&partition.tally).join(" | ")
        ));
        render_identity_bounds(&mut out, &partition.tally);
    }
    out
}

/// A family: its agent rows, one row per session, its totals per accounting
/// basis, and what the search for launched sessions covered.
fn render_change_usage(usage: &agent_tapes_core::change_usage::ChangeUsage) -> String {
    use agent_tapes_core::change_usage::{Asked, Dimension, MergeGroup, Tally};
    let short = |sha: &str| sha.chars().take(10).collect::<String>();
    let priced = usage.totals.sessions.list_price_estimate.is_some();
    let price = |estimate: Option<&ListPriceEstimate>| {
        let Some(estimate) = estimate else {
            return "-".to_owned();
        };
        let amount = match (estimate.amount, estimate.at_least, estimate.at_most) {
            (Some(amount), _, _) => format!("{amount:.4} {}", estimate.currency),
            (None, Some(low), Some(high)) => format!("{low:.4}-{high:.4} {}", estimate.currency),
            _ => "-".to_owned(),
        };
        match estimate_incomplete(estimate) {
            Some(incomplete) => format!("{amount} [{incomplete}]"),
            None => amount,
        }
    };
    let tokens = |tally: &Tally| {
        tally
            .disjoint
            .as_ref()
            .and_then(|disjoint| disjoint.total)
            .map_or_else(|| "-".to_owned(), |total| total.to_string())
    };
    let cells = |tally: &Tally| {
        let mut cells = vec![tally.requests.to_string(), tokens(tally)];
        if priced {
            cells.push(price(tally.list_price_estimate.as_ref()));
        }
        cells
    };
    let summary = |tally: &Tally| {
        let mut text = format!(
            "requests {}, disjoint tokens {}",
            tally.requests,
            tokens(tally)
        );
        if priced {
            text.push_str(&format!(
                ", list-price estimate {}",
                price(tally.list_price_estimate.as_ref())
            ));
        }
        text
    };
    let asked = match &usage.selection {
        Asked::Commits { revisions } => format!("commits {}", revisions.join(", ")),
        Asked::Range { range } => format!("range {range}"),
        Asked::Branch {
            branch, resolved, ..
        } => format!("branch {branch} ({})", serde_kebab(resolved)),
        Asked::CommitsFrom { source, revisions } => {
            format!("{revisions} revisions from {source}")
        }
        Asked::Session { session } => format!("session {}:{}", session.harness, session.id),
        Asked::Window { from, to } => format!(
            "window {}..{}",
            human_timestamp(*from),
            human_timestamp(*to)
        ),
    };
    let tiers: Vec<String> = usage.tiers.iter().map(serde_kebab).collect();
    let mut out = format!(
        "Change usage of {asked} in {}: {} commits, {} sessions, tiers {}\n",
        usage.repository.root.display(),
        usage.commits.len(),
        usage.sessions.len(),
        tiers.join(", ")
    );
    let mut header: Vec<&str> = Vec::new();
    for dimension in &usage.by {
        match dimension {
            Dimension::Commit => header.extend(["COMMIT", "SUBJECT"]),
            Dimension::Merge => header.push("MERGE"),
            Dimension::Agent => header.push("AGENT"),
            Dimension::Role => header.push("ROLE"),
            Dimension::Tier => header.push("TIER"),
            Dimension::Session => header.push("SESSION"),
        }
    }
    header.extend(["REQUESTS", "DISJOINT TOKENS"]);
    if priced {
        header.push("LIST-PRICE ESTIMATE");
    }
    header.push("EVIDENCE (TIER/BASIS REQUESTS)");
    out.push_str(&header.join("\t"));
    out.push('\n');
    for row in &usage.rows {
        let mut line = Vec::new();
        for dimension in &usage.by {
            match dimension {
                Dimension::Commit => {
                    line.push(row.key.commit.as_deref().map(short).unwrap_or_default());
                    line.push(row.subject.clone().unwrap_or_default());
                }
                Dimension::Merge => line.push(match &row.key.merge {
                    Some(MergeGroup::Merge { sha, subject }) => {
                        format!("{} {subject}", short(sha))
                    }
                    Some(MergeGroup::Mainline { sha, .. }) => {
                        format!("{} (on the mainline)", short(sha))
                    }
                    Some(MergeGroup::Unmerged) | None => "unmerged".to_owned(),
                }),
                Dimension::Agent => line.push(row.key.agent.clone().unwrap_or_default()),
                Dimension::Role => line.push(
                    row.key
                        .role
                        .map(|role| role.as_str().to_owned())
                        .unwrap_or_default(),
                ),
                Dimension::Tier => line.push(
                    row.key
                        .tier
                        .map(|tier| tier.as_str().to_owned())
                        .unwrap_or_default(),
                ),
                Dimension::Session => line.push(
                    row.key
                        .session
                        .as_ref()
                        .map(|session| format!("{}:{}", session.harness, session.id))
                        .unwrap_or_default(),
                ),
            }
        }
        line.extend(cells(&row.tally));
        line.push(
            row.evidence
                .iter()
                .map(|evidence| {
                    format!(
                        "{}/{} {}",
                        evidence.tier.as_str(),
                        evidence.basis.as_str(),
                        evidence.tally.requests
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push_str(&line.join("\t"));
        out.push('\n');
        for explained in &row.explain {
            out.push_str(&render_explained(explained));
        }
    }
    out.push_str(&format!("Total: {}\n", summary(&usage.totals.rows)));
    if !usage.totals.rows.unexplained.is_empty() {
        let parts: Vec<String> = usage
            .totals
            .rows
            .unexplained
            .iter()
            .map(|model| {
                let tokens = &model.tokens;
                format!(
                    "{}:{} input {}, output {}, cache read {}, cache write {}",
                    model.harness,
                    model.model,
                    tokens.input.unwrap_or(0),
                    tokens.output.unwrap_or(0),
                    tokens.cache_read.unwrap_or(0),
                    tokens.cache_write.unwrap_or(0)
                )
            })
            .collect();
        out.push_str(&format!(
            "Unexplained parts of recorded totals, apart from the tokens above: {}\n",
            parts.join("; ")
        ));
    }
    if !usage.uncommitted.is_empty() {
        out.push_str("Uncommitted, work after a session's last commit at a place:\n");
        for bucket in &usage.uncommitted {
            let place = match &bucket.branch {
                Some(branch) => format!("{} ({branch})", bucket.worktree.display()),
                None => bucket.worktree.display().to_string(),
            };
            out.push_str(&format!("  {place}: {}\n", summary(&bucket.tally)));
            for explained in &bucket.explain {
                out.push_str(&render_explained(explained));
            }
        }
    }
    if !usage.unattributed.is_empty() {
        out.push_str("Unattributed:\n");
        for part in &usage.unattributed {
            let reason = if part.excluded_tiers.is_empty() {
                part.reason.as_str().to_owned()
            } else {
                let excluded: Vec<&str> = part
                    .excluded_tiers
                    .iter()
                    .map(|tier| tier.as_str())
                    .collect();
                format!("{} ({})", part.reason.as_str(), excluded.join(", "))
            };
            out.push_str(&format!("  {reason}: {}\n", summary(&part.tally)));
        }
    }
    if !usage.unselected.is_empty() {
        out.push_str(
            "Unselected commits, the same sessions' shares of commits not selected, out of the \
             total:\n",
        );
        for commit in &usage.unselected {
            out.push_str(&format!(
                "  {} {}: {}\n",
                short(&commit.commit),
                commit.subject.as_deref().unwrap_or_default(),
                summary(&commit.tally)
            ));
        }
    }
    out.push_str(&format!(
        "Sessions' wholes, which the total, uncommitted, unattributed and unselected add up to: \
         {}\n",
        summary(&usage.totals.sessions)
    ));
    out.push_str("SESSION\tAGENT\tWHOLE REQUESTS\tIN THE TOTAL\n");
    for session in &usage.sessions {
        out.push_str(&format!(
            "{}:{}\t{}\t{}\t{}\n",
            session.harness,
            session.id,
            session.agent,
            session.whole.requests,
            session.rows.requests
        ));
        for note in &session.notes {
            out.push_str(&format!("  note: {note}\n"));
        }
    }
    for note in &usage.notes {
        out.push_str(&format!("Note: {note}\n"));
    }
    out
}

fn render_explained(explained: &agent_tapes_core::change_usage::Explained) -> String {
    let evidence = &explained.evidence;
    let at = human_timestamp(evidence.at);
    let mut text = format!(
        "  evidence {}:{} {} {} at {at}",
        explained.session.harness,
        explained.session.id,
        serde_kebab(&evidence.kind),
        evidence.tier.as_str()
    );
    if let Some(to) = evidence.to {
        text.push_str(&format!(" to {}", human_timestamp(to)));
    }
    let named: Vec<String> = evidence
        .work
        .iter()
        .map(|work| match work {
            agent_tapes_core::change_attribution::Work::Commit { sha } => {
                sha.chars().take(10).collect()
            }
            agent_tapes_core::change_attribution::Work::Uncommitted { worktree, branch } => {
                match branch {
                    Some(branch) => format!("uncommitted {} ({branch})", worktree.display()),
                    None => format!("uncommitted {}", worktree.display()),
                }
            }
        })
        .collect();
    text.push_str(&format!(" naming {}", named.join(", ")));
    if let Some(via) = evidence.via {
        text.push_str(&format!(" via {}", serde_kebab(&via)));
    }
    if let Some(observed) = &evidence.observed {
        text.push_str(&format!(" (recorded as {observed})"));
    }
    text.push('\n');
    text
}

fn render_family(family: &agent_tapes_core::family::FamilyUsage) -> String {
    let priced = family
        .agents
        .iter()
        .map(|agent| &agent.usage)
        .chain(family.sessions.iter().map(|session| &session.usage))
        .chain(std::iter::once(&family.totals))
        .any(|tally| tally.list_price_estimate.is_some() || !tally.list_price_withheld.is_empty());
    let cells = |tally: &UsageTally| {
        let mut cells = tally_cells(tally);
        if priced {
            cells.push(estimate_cell(tally));
        }
        cells
    };
    let header = |first: &str| {
        let mut out = format!(
            "{first}\tSESSIONS\tINPUT\tOUTPUT\tREASONING\tCACHE READ\tCACHE WRITE\tDISJOINT TOTAL\t\
             COST\tCOVERAGE (RECORDED/SUMMED/WINDOW/SINCE-RESET/NONE)"
        );
        out.push_str(if priced {
            "\tLIST-PRICE ESTIMATE\n"
        } else {
            "\n"
        });
        out
    };
    let mut out = format!(
        "Family of {}:{} ({} members)\n",
        family.root.harness,
        family.root.id,
        family.sessions.len()
    );
    out.push_str(&header("AGENT\tROLE"));
    for agent in &family.agents {
        let mut row = vec![agent.agent.clone(), serde_kebab(&agent.role)];
        row.extend(cells(&agent.usage));
        out.push_str(&row.join("\t"));
        out.push('\n');
    }
    let mut total = vec!["TOTAL".to_owned(), String::new()];
    total.extend(cells(&family.totals));
    out.push_str(&total.join("\t"));
    out.push('\n');
    out.push_str(&header("SESSION\tROLE\tAGENT\tLINK"));
    for session in &family.sessions {
        let mut row = vec![
            format!("{}:{}", session.harness, session.id),
            serde_kebab(&session.role),
            session.agent.clone(),
            serde_kebab(&session.link),
        ];
        row.extend(cells(&session.usage));
        out.push_str(&row.join("\t"));
        out.push('\n');
        if let Some(link) = &session.evidence {
            out.push_str(&format!(
                "  linked by {} ({}, {}){}{}\n",
                link.file,
                link.producer,
                serde_kebab(&link.tier),
                link.label
                    .as_ref()
                    .map(|label| format!(", label {label}"))
                    .unwrap_or_default(),
                link.subject
                    .as_ref()
                    .map(|subject| format!(", subject {subject}"))
                    .unwrap_or_default()
            ));
        }
        if session.shared_requests > 0 {
            out.push_str(&format!(
                "  {} requests also recorded in an earlier member's recording are counted there only\n",
                session.shared_requests
            ));
        }
    }
    if let Some(disjoint) = &family.totals.disjoint {
        out.push_str(&render_disjoint_sum(disjoint));
    }
    if let Some(coverage) = &family.totals.cost_coverage {
        out.push_str(&format!(
            "Cost coverage: {}\n",
            render_cost_coverage(coverage)
        ));
    }
    if let Some(estimate) = &family.totals.list_price_estimate {
        out.push_str(&format!("Total {}", render_estimate_lines("", estimate)));
    }
    if let Some(unexplained) = &family.totals.unexplained {
        out.push_str(&render_unexplained("Total ", unexplained));
    }
    if !family.totals.list_price_withheld.is_empty() {
        out.push_str(&format!(
            "List-price estimate withheld: {}\n",
            family
                .totals
                .list_price_withheld
                .iter()
                .map(serde_kebab)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if family.totals.mixed_accounting {
        out.push_str(
            "Mixed accounting: total counters are omitted; compatible partitions follow.\n",
        );
    }
    if let Some(priced) = &family.priced_across_bases {
        let range = if priced.at_least == priced.at_most {
            format!("{:.4}", priced.at_least)
        } else {
            format!("{:.4}-{:.4}", priced.at_least, priced.at_most)
        };
        out.push_str(&format!(
            "List-price estimate across {} accounting partitions: {range} {}{}\n",
            priced.partitions,
            priced.currency,
            if priced.unpriced_partitions > 0 {
                format!(
                    " (at least; {} partitions carry no estimate)",
                    priced.unpriced_partitions
                )
            } else {
                String::new()
            }
        ));
    }
    render_identity_bounds(&mut out, &family.totals);
    for partition in &family.partitions {
        out.push_str(&format!(
            "Accounting partition {} ({}): {} sessions; {}\n",
            partition.harness,
            partition
                .accounting
                .as_ref()
                .map(render_accounting)
                .unwrap_or_else(|| "no accounting".to_owned()),
            partition.tally.sessions,
            cells(&partition.tally).join(" | ")
        ));
        render_identity_bounds(&mut out, &partition.tally);
    }
    for member in &family.withheld {
        out.push_str(&format!(
            "Withheld: {}:{} ({}): {}\n",
            member.harness,
            member.id,
            serde_kebab(&member.role),
            serde_kebab(&member.reason)
        ));
    }
    for continuation in &family.continuations {
        out.push_str(&format!(
            "Continuation of {}:{}: {}{}; {} recordings read, {} bytes\n",
            continuation.session.harness,
            continuation.session.id,
            serde_kebab(&continuation.status),
            continuation
                .from
                .as_ref()
                .map(|from| format!(" from {}:{}", from.harness, from.id))
                .unwrap_or_default(),
            continuation.recordings_examined,
            continuation.bytes_examined
        ));
    }
    let search = &family.search;
    out.push_str(&format!(
        "Search for launched sessions: {} listed since {}, {} opened for a marker, {} began outside \
         every member's interval{}; {}\n",
        search.candidates_listed,
        search
            .since
            .map_or_else(|| "-".to_owned(), |since| since.to_rfc3339()),
        search.candidates_examined,
        search.outside_interval,
        if search.untimed > 0 {
            format!(", {} with no recorded start", search.untimed)
        } else {
            String::new()
        },
        if search.complete {
            "complete".to_owned()
        } else {
            format!(
                "incomplete ({})",
                search
                    .incomplete
                    .iter()
                    .map(serde_kebab_tag)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    ));
    for unreadable in &search.unreadable {
        out.push_str(&format!(
            "Unreadable opening: {}:{}\n",
            unreadable.harness, unreadable.id
        ));
    }
    for unavailable in &search.unavailable {
        out.push_str(&format!("Unavailable: {unavailable}\n"));
    }
    if let Some(evidence) = &family.evidence {
        render_family_evidence(&mut out, evidence);
    }
    out
}

/// The evidence files a family read, what their records did, the children
/// that could not be found and the sessions with conflicting claims.
fn render_family_evidence(out: &mut String, evidence: &agent_tapes_core::family::EvidenceUse) {
    out.push_str(&format!(
        "Evidence: {} files, {} children looked up by id, {} unresolved, {} conflicts\n",
        evidence.files.len(),
        evidence.looked_up,
        evidence.unresolved.len(),
        evidence.conflicts.len()
    ));
    for file in &evidence.files {
        let report = &file.report;
        out.push_str(&format!(
            "  {} ({}): lineage {} read, {} used; binding {} read, {} used, not yet used; {} malformed; \
             {} duplicates; adopted {}, agreed {}, conflicting {}, unresolved {}, not applied {}\n",
            report.path,
            report.producer,
            report.lineage.read,
            report.lineage.used,
            report.binding.read,
            report.binding.used,
            report.malformed.len(),
            report.duplicates,
            file.adopted,
            file.agreed,
            file.conflicting,
            file.unresolved,
            file.not_applied
        ));
        for line in &report.malformed {
            out.push_str(&format!(
                "    line {}: malformed ({})\n",
                line.line,
                serde_kebab(&line.reason)
            ));
        }
        for (kind, count) in &report.unknown_kinds {
            out.push_str(&format!("    {count} records of unknown kind {kind:?}\n"));
        }
        if let Some(truncated) = &report.truncated {
            out.push_str(&format!(
                "    read stopped at line {}: {}\n",
                truncated.line,
                serde_kebab(&truncated.reason)
            ));
        }
    }
    for child in &evidence.unresolved {
        out.push_str(&format!(
            "Unresolved: {}:{} launched by {}:{} ({}, {}): {}\n",
            child.child.harness,
            child.child.id,
            child.parent.harness,
            child.parent.id,
            child.file,
            child.producer,
            serde_kebab(&child.reason)
        ));
    }
    for conflict in &evidence.conflicts {
        out.push_str(&format!(
            "Conflict: {}:{} counted {}\n",
            conflict.session.harness,
            conflict.session.id,
            conflict.placed_under.as_ref().map_or_else(
                || "nowhere (not a member)".to_owned(),
                |parent| format!("under {}:{}", parent.harness, parent.id)
            )
        ));
        for claim in &conflict.claims {
            let source = match &claim.source {
                agent_tapes_core::family::ClaimSource::Recording { link } => {
                    format!("its recording ({})", serde_kebab(link))
                }
                agent_tapes_core::family::ClaimSource::Evidence {
                    file,
                    producer,
                    tier,
                } => format!("{file} ({producer}, {})", serde_kebab(tier)),
            };
            out.push_str(&format!(
                "  names {}:{} by {source}\n",
                claim.parent.harness, claim.parent.id
            ));
        }
    }
}

/// A tagged value's tag, as it serializes.
fn serde_kebab_tag<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value["reason"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn render_overlap(out: &mut String, tally: &UsageTally) {
    let Some(overlap) = &tally.overlap else {
        return;
    };
    match overlap.disjointness {
        Some(Disjointness::InferredFromAbsentCopyEvidence) => out.push_str(
            "Sessions no recording relates are summed as disjoint, inferred from absent copy evidence.\n",
        ),
        Some(Disjointness::InferredFromSampledCopyEvidence) => out.push_str(&format!(
            "Sessions no recording relates are summed as disjoint, inferred from sampled copy evidence: \
             the id check kept only a sample of the ids of {}.\n",
            overlap.sampled.join(", ")
        )),
        None => {}
    }
    for related in &overlap.related {
        let evidence = related
            .evidence
            .iter()
            .map(serde_kebab)
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "Related sessions {} ({evidence}): union {}",
            related.sessions.join(", "),
            serde_kebab(&related.union),
        ));
        if !related.withheld.is_empty() {
            let reasons = related.withheld.iter().map(serde_kebab).collect::<Vec<_>>();
            out.push_str(&format!(" [{}]", reasons.join(", ")));
        }
        out.push('\n');
    }
}

fn serde_kebab(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn render_identity_bounds(out: &mut String, tally: &UsageTally) {
    render_overlap(out, tally);
    if tally.identity_unestablished {
        out.push_str(
            "Event equality across these sessions is unestablished; exact totals are withheld.\n",
        );
    }
    if let Some(bounds) = &tally.identity_bounds {
        for (name, bound) in [
            ("input", &bounds.input),
            ("output", &bounds.output),
            ("reasoning", &bounds.reasoning),
            ("cache read", &bounds.cache_read),
            ("cache write", &bounds.cache_write),
        ] {
            if let Some(bound) = bound {
                out.push_str(&format!(
                    "Identity bound {name}: at least {}",
                    bound.at_least
                ));
                match bound.at_most {
                    Some(upper) => out.push_str(&format!(", at most {upper}\n")),
                    None => out.push_str("; upper bound unmeasured\n"),
                }
            }
        }
        if let Some(bound) = &bounds.cost {
            out.push_str(&format!(
                "Identity bound cost: at least {} USD",
                bound.at_least
            ));
            match bound.at_most {
                Some(upper) => out.push_str(&format!(", at most {upper} USD\n")),
                None => out.push_str("; upper bound unmeasured\n"),
            }
        }
    }
    for figure in &tally.session_figures {
        out.push_str(&format!("Session figure {} {}", figure.harness, figure.id));
        if let Some(tokens) = &figure.tokens {
            out.push_str(&format!(": {}", render_tokens(tokens)));
        }
        if let Some(cost) = &figure.cost {
            out.push_str(&format!("; {}", render_cost(cost)));
        }
        if figure.tokens.is_none() && figure.cost.is_none() {
            out.push_str(": unmeasured");
        }
        if let Some(estimate) = &figure.list_price_estimate {
            out.push_str(&format!("; {}", render_estimate(estimate)));
        }
        out.push('\n');
        render_measurement_coverage(
            out,
            &format!("session {}", figure.id),
            &figure.measurement_coverage,
        );
        for reason in &figure.measurement_incomplete {
            out.push_str(&format!("session {}: {reason}\n", figure.id));
        }
        render_completeness(
            out,
            &format!("session {} ", figure.id),
            &figure.measurement_completeness,
        );
    }
}

/// The selection's disjoint categories, the native conventions they were
/// mapped from, and how many sessions could not supply each category.
fn render_disjoint_sum(disjoint: &DisjointSum) -> String {
    let mut out = format!(
        "Disjoint total over {} sessions: {}\n",
        disjoint.sessions,
        render_categories(&DisjointCells {
            uncached_input: disjoint.uncached_input,
            cache_read: disjoint.cache_read,
            cache_write: disjoint.cache_write.as_ref(),
            output: disjoint.output,
            reasoning: disjoint.reasoning,
            total: disjoint.total,
        })
    );
    for (harness, convention) in &disjoint.conventions {
        out.push_str(&format!(
            "Convention {harness}: {}\n",
            render_convention(convention)
        ));
    }
    if !disjoint.missing.is_empty() {
        out.push_str(&format!(
            "Disjoint missing, sessions per category: {}\n",
            render_counts(disjoint.missing.iter().map(|(name, count)| (name, *count)))
        ));
    }
    render_disjoint_subtotals(
        &mut out,
        "Disjoint",
        &DisjointCells {
            uncached_input: disjoint.uncached_input,
            cache_read: disjoint.cache_read,
            cache_write: disjoint.cache_write.as_ref(),
            output: disjoint.output,
            reasoning: disjoint.reasoning,
            total: disjoint.total,
        },
        &disjoint.measurement_completeness,
    );
    out
}

fn key_column(dimension: &GroupBy) -> &'static str {
    match dimension {
        GroupBy::Harness => "HARNESS",
        GroupBy::Model => "MODEL",
        GroupBy::Variant => "VARIANT",
        GroupBy::Directory => "DIRECTORY",
        GroupBy::Session => "SESSION",
        GroupBy::Launch => "LAUNCH",
        GroupBy::Parent => "PARENT",
    }
}

/// A group's cell for one dimension. A session or parent whose id `shared`
/// holds for that dimension is qualified with its harness.
fn key_cell(key: &GroupKey, dimension: GroupBy, shared: &SharedKeyIds<'_>) -> Option<String> {
    match dimension {
        GroupBy::Harness => key.harness.clone(),
        GroupBy::Model => key.model.clone(),
        GroupBy::Variant => key.variant.clone(),
        GroupBy::Directory => key.directory.clone(),
        GroupBy::Session => {
            let harness = key.session_harness.as_deref().unwrap_or_default();
            Some(session_cell(
                harness,
                key.session.as_deref()?,
                &shared.sessions,
            ))
        }
        GroupBy::Launch => key.launch.clone(),
        GroupBy::Parent => {
            let harness = key.parent_harness.as_deref().unwrap_or_default();
            Some(session_cell(
                harness,
                key.parent.as_deref()?,
                &shared.parents,
            ))
        }
    }
}

/// The session and parent ids that name sessions of more than one harness
/// among the groups.
struct SharedKeyIds<'a> {
    sessions: std::collections::HashSet<&'a str>,
    parents: std::collections::HashSet<&'a str>,
}

impl<'a> SharedKeyIds<'a> {
    fn of(keys: impl Iterator<Item = &'a GroupKey> + Clone) -> Self {
        Self {
            sessions: shared_ids(keys.clone().filter_map(|key| {
                Some((key.session_harness.as_deref()?, key.session.as_deref()?))
            })),
            parents: shared_ids(
                keys.filter_map(|key| {
                    Some((key.parent_harness.as_deref()?, key.parent.as_deref()?))
                }),
            ),
        }
    }
}

/// The ids among these sessions that name sessions of more than one harness.
fn shared_ids<'a>(
    sessions: impl Iterator<Item = (&'a str, &'a str)>,
) -> std::collections::HashSet<&'a str> {
    let mut harnesses = std::collections::HashMap::<&str, &str>::new();
    let mut shared = std::collections::HashSet::new();
    for (harness, id) in sessions {
        if *harnesses.entry(id).or_insert(harness) != harness {
            shared.insert(id);
        }
    }
    shared
}

/// A session's id, as `<harness>:<id>` where `shared` holds the id.
fn session_cell(harness: &str, id: &str, shared: &std::collections::HashSet<&str>) -> String {
    if shared.contains(id) {
        format!("{harness}:{id}")
    } else {
        id.to_owned()
    }
}

/// A summary row's list-price estimate cell, its incomplete categories in
/// brackets, or `withheld` with the reasons.
fn estimate_cell(tally: &UsageTally) -> String {
    if !tally.list_price_withheld.is_empty() {
        let reasons = tally
            .list_price_withheld
            .iter()
            .map(serde_kebab)
            .collect::<Vec<_>>();
        return format!("withheld [{}]", reasons.join(", "));
    }
    let Some(estimate) = &tally.list_price_estimate else {
        return "-".to_owned();
    };
    let amount = match (estimate.amount, estimate.at_least, estimate.at_most) {
        (Some(amount), _, _) => format!("{amount:.4} {}", estimate.currency),
        (None, Some(low), Some(high)) => format!("{low:.4}-{high:.4} {}", estimate.currency),
        _ => "-".to_owned(),
    };
    match estimate_incomplete(estimate) {
        Some(incomplete) => format!("{amount} [{incomplete}]"),
        None => amount,
    }
}

fn tally_cells(tally: &UsageTally) -> Vec<String> {
    let tokens = tally.tokens.as_ref();
    let sessions = tally.sessions;
    let counted = &tally.counted;
    let coverage = &tally.coverage;
    let completeness = &tally.measurement_completeness;
    let counter = |name: &str, sum: Option<u64>, counted: usize| {
        qualified_cell(
            counter_cell(sum, counted, sessions),
            render_reasons(completeness, name),
        )
    };
    vec![
        sessions.to_string(),
        counter(
            "input",
            tokens.and_then(|tokens| tokens.input),
            counted.input,
        ),
        counter(
            "output",
            tokens.and_then(|tokens| tokens.output),
            counted.output,
        ),
        counter(
            "reasoning",
            tokens.and_then(|tokens| tokens.reasoning),
            counted.reasoning,
        ),
        counter(
            "cache_read",
            tokens.and_then(|tokens| tokens.cache_read),
            counted.cache_read,
        ),
        counter(
            "cache_write",
            tokens.and_then(|tokens| tokens.cache_write),
            counted.cache_write,
        ),
        tally
            .disjoint
            .as_ref()
            .map_or_else(String::new, |disjoint| match disjoint.total {
                Some(total) => counter_cell(Some(total), disjoint.sessions, sessions),
                None => render_withholders(&disjoint.measurement_completeness)
                    .map_or_else(String::new, |withheld| format!("withheld [{withheld}]")),
            }),
        qualified_cell(
            sum_cell(
                tally
                    .cost
                    .as_ref()
                    .map(|cost| format!("{:.4} USD", cost.usd)),
                counted.cost,
                sessions,
            ),
            render_reasons(completeness, "cost"),
        ),
        format!(
            "{}/{}/{}/{}/{}",
            coverage.recorded_total,
            coverage.summed_session,
            coverage.summed_read_window,
            coverage.since_reset,
            coverage.no_accounting
        ),
    ]
}

/// A figure's cell followed by the reasons it is incomplete. A cell with no
/// figure stays empty.
fn qualified_cell(cell: String, reasons: Option<String>) -> String {
    match reasons {
        Some(reasons) if !cell.is_empty() => format!("{cell} [{reasons}]"),
        _ => cell,
    }
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
            events.session.harness(),
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
    let mut rendered = format!(
        "{heading} {} {} {} {outcome}",
        record.event.name.as_deref().unwrap_or("-"),
        record.event.call_id.as_deref().unwrap_or("-"),
        record.event.status.as_deref().unwrap_or("-")
    );
    for invocation in &record.event.invocations {
        rendered.push_str(&format!(
            " declared:{}",
            invocation.program.as_deref().unwrap_or("unknown")
        ));
    }
    for consumption in &record.event.artifact_consumptions {
        rendered.push_str(&format!(" artifact:{:?}", consumption.status));
    }
    rendered
}

fn incomplete_label(incomplete: &Incomplete) -> &'static str {
    match incomplete {
        Incomplete::NoResultInRead => "no-result-in-read",
        Incomplete::CallBeforeReadBound => "call-before-read-bound",
        Incomplete::CallNotRecorded => "call-not-recorded",
    }
}

fn print_full_search(result: &agent_tapes_core::SessionList) {
    if let Some(search) = &result.full_search {
        println!(
            "Sessions read: {}; selection truncated: {}",
            search.sessions_read, search.selection_truncated
        );
        for coverage in &search.coverage {
            println!(
                "{} {}: whole={}, matches_truncated={}",
                coverage.harness,
                coverage.session,
                coverage.whole_recording_read,
                coverage.matches_truncated
            );
            for found in &coverage.matches {
                println!(
                    "  {} {:?} {:?} {:?}",
                    found.ordinal, found.kind, found.ts, found.record_ref
                );
            }
        }
    }
}

fn print_session_list(sessions: &[Session]) {
    if sessions.is_empty() {
        return;
    }
    println!("ID\tLIVE\tHARNESS\tMODEL\tSTATUS\tTITLE\tDIRECTORY\tLAST ACTIVITY");
    for session in sessions {
        let model = session
            .model
            .as_ref()
            .map_or_else(String::new, |model| model.identity());
        let status = session
            .model_observation
            .as_ref()
            .map_or_else(String::new, |status| {
                match (status.mixed, status.attribution_uncertain) {
                    (true, true) => "mixed,uncertain".to_owned(),
                    (true, false) => "mixed".to_owned(),
                    (false, true) => "uncertain".to_owned(),
                    (false, false) => "".to_owned(),
                }
            });
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            session.id,
            live_label(session),
            session.harness(),
            model,
            status,
            human_title(session),
            session
                .directory
                .as_deref()
                .map_or_else(String::new, |path| path.display().to_string()),
            session
                .last_activity_at
                .map_or_else(|| "unavailable".to_owned(), human_timestamp)
        );
    }
}

fn source_harness(source: &SourceDescriptor) -> &str {
    source.recorded_harness.as_deref().unwrap_or(&source.origin)
}

fn live_label(session: &Session) -> &'static str {
    match session.live {
        Some(LiveState::Working) => "working",
        Some(LiveState::Idle) => "idle",
        None => "",
    }
}

fn print_availability_note(result: &agent_tapes_core::SessionList) {
    print_diagnostics(
        result.scan_truncated,
        result.scanned,
        &result.unreadable,
        &result.unsearched,
        &result.partial_search,
        &result.unavailable,
    );
    print_unplaced_note(result.unplaced.as_ref(), result.scanned);
}

fn print_unplaced_note(unplaced: Option<&agent_tapes_core::Unplaced>, scanned: usize) {
    let Some(unplaced) = unplaced else {
        return;
    };
    let examples = unplaced
        .examples
        .iter()
        .map(|directory| directory.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let more = if unplaced.directories > unplaced.examples.len() {
        ", …"
    } else {
        ""
    };
    let noun = if unplaced.directories == 1 {
        "directory"
    } else {
        "directories"
    };
    println!(
        "Not placed: the bounded scan excluded {} distinct recorded {noun} among {scanned} inspected candidates; their sessions' membership in this project cannot be proven (examples: {examples}{more}). `tapes list --global --directory <substring>` reaches them.",
        unplaced.directories
    );
}

/// What a listing could not reach, stated the same way wherever a listing
/// backs the answer: a set that stopped short is a view, not the store.
fn print_diagnostics(
    scan_truncated: bool,
    scanned: usize,
    unreadable: &[String],
    unsearched: &[String],
    partial_search: &[String],
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
    print_search_diagnostics("Unsearched", unsearched);
    print_search_diagnostics("Partial search", partial_search);
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

fn print_transcript(transcript: &Transcript, by_latest: bool, selected_by: Option<&str>) {
    print!("{}", render_transcript(transcript, by_latest, selected_by));
}

/// `show --full` writes each turn as the reader produces it, so the whole
/// recording is never held. `--tail` keeps only a ring of the newest turns.
#[allow(clippy::too_many_arguments)]
fn show_full(
    selection: &SelectionArgs,
    tail: Option<usize>,
    turns: Option<(TurnSelection, &'static str)>,
    json: bool,
    by_latest: bool,
    turn_chars: Option<usize>,
    prices: Option<agent_tapes_core::pricing::PriceTable>,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut sink = FullShow {
        turn_chars,
        prices,
        out: std::io::BufWriter::new(stdout.lock()),
        by_latest,
        tail,
        projection: turns.map(|(kept, flag)| (Projection::new(kept), flag)),
        json,
        writer: None,
        session: None,
        window: std::collections::VecDeque::new(),
        last: None,
        total: 0,
    };
    let read = agent_tapes_core::show_full_with_backends(
        &selection.input.backends()?,
        selection.selection(),
        &mut sink,
    )?;
    sink.finish(read)
}

#[allow(clippy::too_many_arguments)]
fn events_selection(
    query: &SessionQueryArgs,
    backends: &[Box<dyn Backend>],
    tail: Option<usize>,
    filter: &agent_tapes_core::event::EventFilter<'_>,
    options: agent_tapes_core::event::EventOptions,
    full: bool,
    count_only: bool,
    json: bool,
) -> Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    if json {
        write!(
            out,
            "{{\"schema\":\"tapes-events-selection/4\",\"sessions\":["
        )?;
    }
    let mut first = true;
    let mut emit = |backend: &dyn Backend, session: &Session| -> Result<()> {
        if json {
            if !first {
                write!(out, ",")?;
            }
            write!(out, "{{")?;
        }
        first = false;
        let mut sink = SelectionEvents {
            out: &mut out,
            count: 0,
            json,
            count_only,
            writer: None,
        };
        let events = if full {
            agent_tapes_core::events_full_for_session(
                backend,
                session,
                tail.unwrap_or(usize::MAX),
                filter,
                options,
                &mut sink,
            )?
            .events
        } else {
            let mut events =
                backend.events_with_options(session, tail.unwrap_or(usize::MAX), options)?;
            events.retain(filter);
            sink.session(&events.session)?;
            for record in &events.events {
                sink.record(record.clone())?;
            }
            events
        };
        let calls = sink.count;
        if let Some(writer) = sink.writer.take() {
            writer.close(sink.out, &events)?;
        }
        if json {
            if !count_only {
                write!(sink.out, ",")?;
            }
            write!(
                sink.out,
                "\"session\":{},\"count\":{},\"partial\":{},\"truncation\":{}}}",
                serde_json::to_string(&events.session)?,
                calls,
                events.truncated,
                serde_json::to_string(&events.truncation)?
            )?;
        } else {
            writeln!(
                sink.out,
                "{} ({}): {} calls; partial={}",
                events.session.id,
                events.session.harness(),
                calls,
                events.truncated
            )?;
            let mut notes = String::new();
            render_truncation_notes(&mut notes, &events.truncation);
            sink.out.write_all(notes.as_bytes())?;
        }
        Ok(())
    };
    if let Some(one) = query.single() {
        agent_tapes_core::visit_event_session(backends, one, &mut emit)?;
    } else {
        let discovery = agent_tapes_core::visit_event_sessions(backends, &query.set(), &mut emit)?;
        if json {
            write!(
                out,
                "],\"discovery\":{}}}",
                serde_json::to_string(&discovery)?
            )?;
            out.flush()?;
            return Ok(());
        }
        print_availability_note(&discovery);
    }
    if json {
        writeln!(out, "]}}")?;
    }
    out.flush()?;
    Ok(())
}

struct SelectionEvents<'a, W: std::io::Write> {
    out: &'a mut W,
    count: usize,
    json: bool,
    count_only: bool,
    writer: Option<agent_tapes_core::event::StreamedEventsJson>,
}
impl<W: std::io::Write> agent_tapes_core::EventSink for SelectionEvents<'_, W> {
    fn session(&mut self, session: &Session) -> Result<()> {
        if !self.count_only {
            if self.json {
                self.out.write_all(b"\"events\":")?;
                self.writer = Some(agent_tapes_core::event::StreamedEventsJson::open(
                    self.out, session,
                )?);
            } else {
                self.out
                    .write_all(render_header(session, false).as_bytes())?;
            }
        }
        Ok(())
    }
    fn record(&mut self, record: EventRecord) -> Result<()> {
        if record.event.kind == agent_tapes_core::event::EventKind::ToolCall {
            self.count += 1;
        }
        if !self.count_only {
            if let Some(writer) = &mut self.writer {
                writer.record(self.out, &record)?;
            } else {
                self.out.write_all(render_event(&record).as_bytes())?;
                self.out.write_all(b"\n")?;
            }
        }
        Ok(())
    }
}

/// `events --full` writes each kept event as the second read pairs it, so the
/// whole recording is never held.
fn events_full(
    selection: &SessionQueryArgs,
    tail: Option<usize>,
    filter: &agent_tapes_core::event::EventFilter<'_>,
    json: bool,
    by_latest: bool,
    options: agent_tapes_core::event::EventOptions,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut sink = FullEvents {
        out: std::io::BufWriter::new(stdout.lock()),
        by_latest,
        json,
        writer: None,
    };
    let streamed = agent_tapes_core::events_full_with_options_with_backends(
        &selection.input.backends()?,
        selection.single().expect("single selection"),
        tail.unwrap_or(usize::MAX),
        filter,
        options,
        &mut sink,
    )?;
    sink.finish(streamed)
}

struct FullEvents<W: std::io::Write> {
    out: W,
    by_latest: bool,
    json: bool,
    /// The events object being written, under `--json`.
    writer: Option<agent_tapes_core::event::StreamedEventsJson>,
}

impl<W: std::io::Write> agent_tapes_core::EventSink for FullEvents<W> {
    fn session(&mut self, session: &Session) -> Result<()> {
        let mut session = session.clone();
        liveness::annotate(std::slice::from_mut(&mut session));
        if self.json {
            self.writer = Some(agent_tapes_core::event::StreamedEventsJson::open(
                &mut self.out,
                &session,
            )?);
        } else {
            self.out
                .write_all(render_header(&session, self.by_latest).as_bytes())?;
        }
        Ok(())
    }

    fn record(&mut self, record: EventRecord) -> Result<()> {
        match &mut self.writer {
            Some(writer) => writer.record(&mut self.out, &record)?,
            None => {
                self.out.write_all(render_event(&record).as_bytes())?;
                self.out.write_all(b"\n")?;
            }
        }
        Ok(())
    }
}

impl<W: std::io::Write> FullEvents<W> {
    fn finish(mut self, streamed: agent_tapes_core::StreamedEvents) -> Result<()> {
        let events = &streamed.events;
        if let Some(writer) = self.writer.take() {
            writer.close(&mut self.out, events)?;
            self.out.write_all(b"\n")?;
            self.out.flush()?;
            return Ok(());
        }
        let mut footer = String::new();
        if self.by_latest {
            render_latest_note(&mut footer, &events.session);
        }
        render_truncation_notes(&mut footer, &events.truncation);
        render_notes(&mut footer, &events.notes);
        footer.push_str(&format!(
            "Read evidence: the whole recording was streamed twice; {}.\n",
            streamed
                .read
                .coordinates
                .describe(streamed.read.source_length)
        ));
        self.out.write_all(footer.as_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

struct FullShow<W: std::io::Write> {
    turn_chars: Option<usize>,
    /// The table the session is priced with, where one is in effect.
    prices: Option<agent_tapes_core::pricing::PriceTable>,
    out: W,
    by_latest: bool,
    tail: Option<usize>,
    /// The kinds kept, counting what they omit, and the flag that chose them.
    projection: Option<(Projection, &'static str)>,
    json: bool,
    /// The session object being written, under `--json`.
    writer: Option<agent_tapes_core::model::StreamedSessionJson>,
    session: Option<Session>,
    /// The newest turns, under `--tail`.
    window: std::collections::VecDeque<Turn>,
    /// The newest turn written, which the activity note compares against.
    last: Option<Turn>,
    total: usize,
}

impl<W: std::io::Write> agent_tapes_core::TurnSink for FullShow<W> {
    fn session(&mut self, session: &Session) -> Result<()> {
        let mut session = session.clone();
        liveness::annotate(std::slice::from_mut(&mut session));
        if let Some(table) = &self.prices {
            session.price(table);
        }
        if self.json {
            self.writer = Some(agent_tapes_core::model::StreamedSessionJson::open(
                &mut self.out,
                &session,
            )?);
        } else {
            self.out
                .write_all(render_header(&session, self.by_latest).as_bytes())?;
        }
        self.session = Some(session);
        Ok(())
    }

    fn turn(&mut self, mut turn: Turn) -> Result<()> {
        if let Some((projection, _)) = &mut self.projection {
            if !projection.admit_turn(&turn) {
                return Ok(());
            }
        }
        if let Some(limit) = self.turn_chars {
            turn.cap_text(limit);
        }
        self.total += 1;
        match self.tail {
            Some(0) => {}
            Some(bound) => {
                if self.window.len() == bound {
                    self.window.pop_front();
                }
                self.window.push_back(turn);
            }
            None => {
                self.emit(&turn)?;
                self.last = Some(turn);
            }
        }
        Ok(())
    }
}

impl<W: std::io::Write> FullShow<W> {
    fn emit(&mut self, turn: &Turn) -> Result<()> {
        match &mut self.writer {
            Some(writer) => writer.turn(&mut self.out, turn)?,
            None => self.out.write_all(render_turn(turn).as_bytes())?,
        }
        Ok(())
    }

    fn finish(mut self, read: agent_tapes_core::backend::StreamedTranscript) -> Result<()> {
        let session = self
            .session
            .take()
            .ok_or_else(|| anyhow!("the whole-recording read resolved no session"))?;
        let (turns, window) = match self.tail {
            Some(bound) => {
                let turns = self.window.drain(..).collect::<Vec<_>>();
                for turn in &turns {
                    self.emit(turn)?;
                }
                let window = Truncation::window_of(&turns, self.total, bound);
                (turns, window)
            }
            None => (self.last.take().into_iter().collect(), None),
        };
        let mut transcript = read.transcript(session, turns, window);
        let flag = self.projection.take().map(|(projection, flag)| {
            transcript.projection = Some(projection);
            flag
        });
        if let Some(writer) = self.writer.take() {
            writer.close(&mut self.out, &transcript)?;
            self.out.write_all(b"\n")?;
            self.out.flush()?;
            return Ok(());
        }
        let mut footer = String::new();
        render_footer(&mut footer, &transcript, self.by_latest, flag);
        footer.push_str(&format!(
            "Read evidence: the whole recording was streamed; {}.\n",
            read.coordinates.describe(read.source_length)
        ));
        self.out.write_all(footer.as_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

/// The human render carries the same two partiality signals the JSON does.
/// `--tail` is the recommended first probe, so a window that does not say it
/// is one would be read as the whole session. A session reached by `--latest`
/// names itself, because the caller did not name it and may have been handed
/// its own session. `selected_by` names the flag that chose the kept turn
/// kinds, so the note says what to drop to see the rest.
fn render_transcript(
    transcript: &Transcript,
    by_latest: bool,
    selected_by: Option<&str>,
) -> String {
    let mut out = render_header(&transcript.session, by_latest);
    for turn in &transcript.turns {
        out.push_str(&render_turn(turn));
    }
    render_footer(&mut out, transcript, by_latest, selected_by);
    out
}

fn render_header(session: &Session, by_latest: bool) -> String {
    if session.live.is_some()
        || by_latest
        || session.source.replica.is_some()
        || session.launch.is_some()
    {
        let launch = session.launch.map_or_else(String::new, |launch| {
            format!(" [launch: {}]", launch.as_str())
        });
        format!(
            "# {} {}{}{}{}\n",
            session.harness(),
            session.id,
            launch,
            live_marker(session),
            replica_marker(session)
        )
    } else {
        String::new()
    }
}

fn render_turn(turn: &Turn) -> String {
    let role = human_speaker(turn);
    let mut text = turn.text.clone();
    for part in &turn.parts {
        if let agent_tapes_core::content::ContentPart::FileReference { reference, .. } = part {
            let availability = reference.body_availability.map_or_else(
                || "unknown".to_owned(),
                |availability| serde_json::to_string(&availability).unwrap_or_default(),
            );
            text.push_str(&format!(
                "\nfile-reference: {}; body: {}",
                reference.path.as_deref().unwrap_or("(unnamed)"),
                availability.trim_matches('"'),
            ));
            if let Some(size) = &reference.recorded_size {
                text.push_str(&format!("; recorded size: {size}"));
            }
            if let Some(lines) = &reference.body_lines {
                text.push_str(&format!("; lines: {}-{}", lines.start, lines.end));
            }
            if let Some(total) = reference.total_lines {
                text.push_str(&format!("; total lines: {total}"));
            }
        }
    }
    for part in &turn.parts {
        if let agent_tapes_core::content::ContentPart::StructuredArtifact {
            descriptor,
            native_kind,
            ..
        } = part
        {
            if native_kind == "attachment/prompt_snapshot" {
                text.push_str(&format!(
                    "\nprompt-snapshot: {}; body: reference-only",
                    descriptor.text
                ));
            }
        }
    }
    if let Some(coverage) = &turn.coverage {
        if let Some(reason) = &coverage.omitted_reason {
            if reason.starts_with("attachment ") {
                text.push_str(&format!(
                    "\ncontent-coverage: {}",
                    serde_json::to_string(coverage).unwrap_or_default()
                ));
            }
        }
    }
    let mut rendered = match turn.ts {
        Some(ts) => format!(
            "[{role} #{} {}]\n{}\n",
            turn.ordinal,
            human_timestamp(ts),
            text
        ),
        None => format!("[{role} #{}]\n{}\n", turn.ordinal, text),
    };
    if turn.text_truncated == Some(true) {
        if let Some(original) = turn.text_chars {
            rendered.push_str(&format!(
                "[{} characters omitted]\n",
                original.saturating_sub(turn.text.chars().count())
            ));
        }
    }
    rendered
}

/// Everything a transcript's human render says after its turns.
fn render_footer(
    out: &mut String,
    transcript: &Transcript,
    by_latest: bool,
    selected_by: Option<&str>,
) {
    if transcript.session.start_uncertain {
        out.push_str(&format!(
            "Note: The recorded start could not be read; {} is the earliest record reached, and the \
             session began at or before it.\n",
            transcript
                .session
                .started_at
                .map_or_else(|| "unavailable".to_owned(), human_timestamp)
        ));
    }
    render_activity_note(out, transcript);
    if by_latest {
        render_latest_note(out, &transcript.session);
    }
    render_truncation_notes(out, &transcript.truncation);
    if let Some(projection) = &transcript.projection {
        let drop =
            selected_by.map_or_else(String::new, |flag| format!(" Drop {flag} to see them."));
        out.push_str(&format!(
            "Note: Only {} turns are shown; turns omitted by kind: {}.{drop}\n",
            projection.kept_summary(),
            projection.omitted_summary()
        ));
    }
    render_read_notes(out, transcript);
    render_notes(out, &transcript.notes);
}

fn render_read_notes(out: &mut String, transcript: &Transcript) {
    if let Some(terminal) = &transcript.terminal {
        let label = terminal.payload_type.as_deref().map_or_else(
            || terminal.record_type.clone(),
            |kind| format!("{}/{}", terminal.record_type, kind),
        );
        out.push_str(&format!(
            "Note: The newest terminal observation is `{label}`; recorded stop evidence does not say whether the session is stopped now.\n"
        ));
        if let Some(message) = &terminal.message {
            out.push_str(&format!("Terminal message: {}\n", message.text));
        }
    }
    if let Some(text_tail) = &transcript.text_tail {
        if let Some(reason) = text_tail.empty_reason {
            let reason = match reason {
                agent_tapes_core::model::EmptyTextTailReason::NoOperatorAssistantTextInRead => {
                    "the bounded read held no operator or assistant text"
                }
                agent_tapes_core::model::EmptyTextTailReason::ZeroRequestedTail => {
                    "the requested text tail was zero"
                }
                agent_tapes_core::model::EmptyTextTailReason::EmptyCompleteProjection => {
                    "the complete projection held no operator or assistant text"
                }
            };
            out.push_str(&format!("Note: The text tail is empty: {reason}.\n"));
        }
    }
    if let Some(read) = &transcript.read {
        let ranges = read
            .ranges
            .iter()
            .map(|range| {
                format!(
                    "{:?} [{}..{})",
                    range.kind, range.span.start, range.span.end
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "Read evidence: source length {} bytes; configured bound {}; ranges: {ranges}.\n",
            read.source_length,
            human_bytes(read.configured_bound)
        ));
    }
}

/// Name what to pass to reach the session before the one `--latest` picked:
/// `self` when that session is the caller's own, its id otherwise.
fn render_latest_note(out: &mut String, session: &Session) {
    let own = self_token::detect(|variable| std::env::var(variable).ok())
        .is_ok_and(|detected| detected.id == session.id);
    if own {
        out.push_str(
            "Note: --latest picked the newest session in scope, which is this one. Pass --exclude self to reach the one before it.\n",
        );
    } else {
        out.push_str(&format!(
            "Note: --latest picked the newest session in scope. Pass --exclude {} to reach the one before it.\n",
            session.id
        ));
    }
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
                 what --tail or `tapes export` can reach, and a larger --read-bytes reaches them.\n",
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
            SourceBound::InputCoverage { gaps } => out.push_str(&format!(
                "Note: The supplied input has {gaps} explicit coverage gap(s); this transcript is a known projection, not a complete collection.\n"
            )),
        }
    }
}

fn render_activity_note(out: &mut String, transcript: &Transcript) {
    let newest_turn = transcript.turns.iter().rev().find_map(|turn| turn.ts);
    let activity_after_newest_turn = transcript
        .session
        .last_activity_at
        .as_ref()
        .zip(newest_turn.as_ref())
        .is_some_and(|(activity, newest_turn)| activity.timestamp() > newest_turn.timestamp());
    let Some(trailing_record) = transcript.trailing_record.as_ref() else {
        if activity_after_newest_turn {
            if let Some(newest_turn) = newest_turn {
                out.push_str(&format!(
                    "Note: The store records activity at {}, after the newest turn rendered here ({}). \
                     What came later is a record `show` does not render as a turn.\n",
                    human_timestamp(transcript.session.last_activity_at.expect("activity was checked")),
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
                human_timestamp(transcript.session.last_activity_at.expect("activity was checked")),
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

/// A transcript read from another machine names it in the header, so a human
/// reader cannot mistake a replica's recording for one of this machine's.
fn replica_marker(session: &Session) -> String {
    session
        .source
        .replica
        .as_deref()
        .map_or_else(String::new, |destination| {
            format!(" [replica {destination}]")
        })
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

struct TimelineOutput<W: std::io::Write> {
    out: W,
    json: bool,
    first_session: bool,
    first_record: bool,
}

impl<W: std::io::Write> agent_tapes_core::timeline::Sink for TimelineOutput<W> {
    fn session(&mut self, session: &agent_tapes_core::model::Session) -> Result<()> {
        if self.json {
            if !self.first_session {
                write!(self.out, ",")?;
            }
            self.first_session = false;
            write!(self.out, "{{\"session\":")?;
            serde_json::to_writer(&mut self.out, &session.id)?;
            write!(self.out, ",\"records\":[")?;
        }
        self.first_record = true;
        Ok(())
    }
    fn record(&mut self, record: agent_tapes_core::timeline::Record) -> Result<()> {
        if self.json {
            if !self.first_record {
                write!(self.out, ",")?;
            }
            serde_json::to_writer(&mut self.out, &record)?;
        } else {
            writeln!(
                self.out,
                "{} {} {}",
                record.ordinal,
                serde_json::to_value(record.kind)?
                    .as_str()
                    .unwrap_or("unknown"),
                record
                    .ts
                    .map(|ts| ts.to_rfc3339())
                    .unwrap_or_else(|| "-".into())
            )?;
        }
        self.first_record = false;
        Ok(())
    }
    fn end(&mut self, truncated: bool, notes: &[String]) -> Result<()> {
        if self.json {
            write!(self.out, "],\"truncated\":{truncated},\"notes\":")?;
            serde_json::to_writer(&mut self.out, notes)?;
            write!(self.out, "}}")?;
        }
        Ok(())
    }
}

fn parse_search_turns(value: &str) -> std::result::Result<usize, String> {
    if value == "all" {
        return Ok(usize::MAX);
    }
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| "expected a positive number of turns or all".to_owned())
}

fn print_search_diagnostics(label: &str, diagnostics: &[String]) {
    let mut groups = std::collections::BTreeMap::<(String, String), usize>::new();
    for diagnostic in diagnostics {
        let (harness, reason) = diagnostic
            .split_once(" session ")
            .and_then(|(harness, rest)| rest.split_once(": ").map(|(_, reason)| (harness, reason)))
            .unwrap_or_else(|| diagnostic.split_once(' ').unwrap_or((diagnostic, "")));
        *groups
            .entry((harness.to_owned(), reason.to_owned()))
            .or_default() += 1;
    }
    for ((harness, reason), count) in groups {
        println!("{label}: {harness}: {reason} ({count})");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_session_command_accepts_a_harness_with_an_id() {
        use clap::Parser;
        for command in [
            "show",
            "page",
            "history-search",
            "metadata",
            "child",
            "events",
            "lineage",
            "brief",
            "usage",
            "stats",
            "export",
            "capture",
        ] {
            let mut args = vec!["tapes", command, "synthetic-prefix", "--harness", "claude"];
            if command == "child" {
                args.extend(["--reference", "synthetic-child"]);
            }
            if command == "history-search" {
                args.extend(["--search", "synthetic"]);
            }
            assert!(super::Cli::try_parse_from(args).is_ok(), "{command}");
        }
    }
    use super::*;
    use agent_tapes_core::model::{
        End, OrdinalRange, Role, SourceDescriptor, TrailingRecord, Turn, TurnKind, TurnWindow,
    };
    use chrono::{DateTime, Utc};

    fn transcript(truncated: bool) -> Transcript {
        Transcript {
            session: Session {
                id: "s1".to_owned(),
                source: SourceDescriptor::installed("claude", "fixture-recording"),
                metadata: None,
                model: None,
                model_observation: None,
                model_selections: Vec::new(),
                launch: None,
                title: None,
                derived_title: None,
                derived_title_truncated: None,
                directory: None,
                started_at: Some(Utc::now()),
                last_activity_at: Some(Utc::now()),
                live: None,
                cost: None,
                tokens: None,
                accounting: None,
                start_uncertain: false,
                occurrence: None,
                usage_detail: None,
                list_price_estimate: None,
            },
            turns: vec![Turn {
                origin: None,
                text_truncated: None,
                text_chars: None,
                context_event: None,
                model: None,
                role: Role::User,
                kind: TurnKind::Operator,
                text: "fix the parser".to_owned(),
                ts: None,
                ordinal: 0,
                native_id: None,
                request_turn_id: None,
                metadata: None,
                record_ref: None,
                parts: Vec::new(),
                coverage: None,
                channel: None,
                recipient: None,
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
            read: None,
            terminal: None,
            text_tail: None,
            artifacts: Vec::new(),
            graph: None,
            trailing_record: None,
            notes: vec!["Skipped 1 unparseable line.".to_owned()],
            projection: None,
            kinds: None,
            locations: None,
        }
    }

    #[test]
    fn the_human_render_says_when_it_is_a_window() {
        let windowed = render_transcript(&transcript(true), false, None);
        assert!(
            windowed.contains(
                "Showing the last 1 of 3 turns; 2 earlier turns fall outside the 1-turn window"
            ),
            "{windowed}"
        );
        assert!(windowed.contains("--tail 3"), "{windowed}");
        assert!(windowed.contains("tapes export"), "{windowed}");
        assert!(windowed.contains("Skipped 1 unparseable line."));

        let whole = render_transcript(&transcript(false), false, None);
        assert!(!whole.contains("Showing the last"), "{whole}");
        assert!(whole.contains("Skipped 1 unparseable line."));
        assert!(whole.starts_with("[user #0]"), "{whole}");
    }

    #[test]
    fn an_uncertain_start_is_named_as_a_floor() {
        let mut floor = transcript(false);
        floor.session.start_uncertain = true;
        let rendered = render_transcript(&floor, false, None);
        assert!(
            rendered.contains("The recorded start could not be read"),
            "{rendered}"
        );
        assert!(!render_transcript(&transcript(false), false, None).contains("could not be read"));
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
        let rendered = render_transcript(&bounded, false, None);
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
        session.session.last_activity_at = Some("2026-08-23T23:27:56Z".parse().unwrap());
        let rendered = render_transcript(&session, false, None);
        assert!(rendered.contains("does not render as a turn"), "{rendered}");
        assert!(rendered.contains("23:27:56"), "{rendered}");
        assert!(rendered.contains("23:24:52"), "{rendered}");

        session.session.last_activity_at = Some(newest_turn);
        let agreeing = render_transcript(&session, false, None);
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
            Some("2026-08-23T23:24:52.400Z".parse::<DateTime<Utc>>().unwrap());

        let same_second = render_transcript(&transcript, false, None);
        assert!(
            !same_second.contains("The store records activity"),
            "{same_second}"
        );

        transcript.session.last_activity_at =
            Some("2026-08-23T23:24:53Z".parse::<DateTime<Utc>>().unwrap());
        let next_second = render_transcript(&transcript, false, None);
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
        transcript.session.last_activity_at = Some(trailing_timestamp);
        transcript.trailing_record = Some(TrailingRecord {
            kind: "event_msg".to_owned(),
            timestamp: Some(trailing_timestamp),
        });

        let rendered = render_transcript(&transcript, false, None);
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
        let picked = render_transcript(&transcript(false), true, None);
        assert!(picked.starts_with("# claude s1"), "{picked}");
        assert!(picked.contains("--exclude s1"), "{picked}");

        let named = render_transcript(&transcript(false), false, None);
        assert!(!named.contains("--exclude"), "{named}");
    }

    #[test]
    fn coverage_is_labelled_by_what_the_counters_select() {
        use agent_tapes_core::model::AccountingBasis;
        let accounting = |selection| Accounting {
            basis: AccountingBasis::SummedRequests,
            coverage: AccountingCoverage::Session,
            selection,
        };
        assert_eq!(
            coverage_label(Some(&accounting(Some(AccountingSelection::ActiveBranch)))),
            "active"
        );
        assert_eq!(coverage_label(Some(&accounting(None))), "session");
        assert_eq!(coverage_label(None), "session");
    }

    #[test]
    fn incomplete_counters_share_a_clause_by_reason_set() {
        use agent_tapes_core::measurement::MeasurementReason::{MissingCounter, StoppedAttempt};
        let mut completeness = MeasurementCompleteness::default();
        completeness.qualify("input", MissingCounter);
        completeness.qualify("input", StoppedAttempt);
        completeness.qualify("output", StoppedAttempt);
        completeness.qualify("cost", StoppedAttempt);
        assert_eq!(
            render_incomplete_names(&completeness, &COUNTERS, "counters").as_deref(),
            Some("input: missing-counter, stopped-attempt; output, cost: stopped-attempt")
        );
        completeness.qualify_all(StoppedAttempt);
        assert_eq!(
            render_incomplete_names(&completeness, &COUNTERS, "counters").as_deref(),
            Some("input: missing-counter, stopped-attempt; output, reasoning, cache_read, cache_write, cost: stopped-attempt")
        );
        let mut stopped = MeasurementCompleteness::default();
        stopped.qualify_all(StoppedAttempt);
        assert_eq!(
            render_incomplete_names(&stopped, &COUNTERS, "counters").as_deref(),
            Some("all counters: stopped-attempt")
        );
    }
}
