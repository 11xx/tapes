//! `tapes prices`: fetch the vendors' published prices into the data
//! directory's table, and show what the tables hold.

use std::path::{Path, PathBuf};

use agent_tapes_core::price_update::{
    self, FetchLimits, FetchRecord, RefreshDecision, UpdateReport, FETCH_FILE, TABLE_FILE,
};
use agent_tapes_core::pricing::{self, CategoryPrices, ModelPrice, PriceLookup, PriceTable};
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use serde::Serialize;

pub const SHOW_SCHEMA: &str = "tapes-prices-show/2";
/// The environment variable that, set to `1`, keeps pricing commands from
/// refreshing the fetched table.
pub const OFFLINE_ENV: &str = "TAPES_OFFLINE";

#[derive(Subcommand)]
pub enum PricesCommand {
    /// Fetch Anthropic's and OpenAI's pricing pages and LiteLLM's JSON with
    /// `curl` and update the table at `$XDG_DATA_HOME/tapes/prices.toml`
    /// (`~/.local/share/tapes/prices.toml` when that is unset), a
    /// tapes-price-table/2 file. History is kept as validity windows: a price
    /// equal to the row in effect keeps it; a changed price closes that row
    /// now and opens a new one; a new id's row has no start, so it also
    /// prices earlier sessions. Rows are never dropped. A source that fails
    /// to fetch or parse contributes nothing and keeps its earlier rows; the
    /// others still update, each outcome is reported, and the command exits
    /// non-zero. `prices-fetch.json` beside the table (tapes-price-fetch/1)
    /// records the attempt and each source's last success. OpenAI's 30m and
    /// 12h cache writes have no tapes category and are left unpriced.
    Update {
        /// Read `anthropic.md`, `openai.md` and `litellm.json` from DIR
        /// instead of fetching; a missing file fails that source. Without
        /// it, the directory TAPES_PRICES_FROM names, if any.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Print the outcome as JSON, tapes-prices-update/1.
        #[arg(long)]
        json: bool,
    },
    /// Show which table usage pricing reads and how it was chosen —
    /// `named`, `environment`, `config`, `data`, `config+data`, `absent`,
    /// `config-invalid` or `invalid` — with any table it set aside, the
    /// config and data tables' paths, each source's last fetch, whether a
    /// pricing command would refresh the data table now and why, and per
    /// model its rates in effect now, any long-context term, and its source.
    /// With MODEL, every row naming it — by id, alias, or as a dated id
    /// `-YYYYMMDD` whose undated id a row names — with its validity window.
    /// Rows are the data table's. JSON is tapes-prices-show/2.
    Show {
        /// A model id as a harness records it.
        model: Option<String>,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: PricesCommand) -> Result<()> {
    match command {
        PricesCommand::Update { from, json } => update(from.as_deref(), json),
        PricesCommand::Show { model, json } => show(model.as_deref(), json),
    }
}

fn data_directory() -> Result<PathBuf> {
    price_update::data_directory().ok_or_else(|| {
        anyhow!("neither XDG_DATA_HOME nor HOME is set, so tapes has no data directory")
    })
}

/// Whether `--offline` or `TAPES_OFFLINE=1` keeps a command from refreshing.
pub fn offline(flag: bool) -> bool {
    flag || std::env::var_os(OFFLINE_ENV).is_some_and(|value| value == "1")
}

/// Refresh the data table before a command prices with it: when neither
/// `named` nor `TAPES_PRICES` names a table, the command is not offline, and
/// [`price_update::refresh_due`] holds. Never fails; a refresh that does is
/// one stderr warning.
pub fn refresh_before_pricing(named: Option<&Path>, offline_flag: bool) {
    if named.is_some() || pricing::environment_table_path().is_some() || offline(offline_flag) {
        return;
    }
    let Some(directory) = price_update::data_directory() else {
        return;
    };
    let now = Utc::now();
    let record = FetchRecord::read(&directory.join(FETCH_FILE));
    if !price_update::refresh_due(record.as_ref(), directory.join(TABLE_FILE).exists(), now) {
        return;
    }
    let from = price_update::from_environment();
    let failures: Vec<String> =
        match price_update::update(&directory, from.as_deref(), FetchLimits::REFRESH, now) {
            Ok(report) => report
                .sources
                .iter()
                .filter(|source| !source.ok)
                .map(|source| {
                    format!(
                        "{}: {}",
                        source.name,
                        source.error.as_deref().unwrap_or_default()
                    )
                })
                .collect(),
            Err(error) => vec![format!("{error:#}")],
        };
    if failures.is_empty() {
        return;
    }
    let fetched = FetchRecord::read(&directory.join(FETCH_FILE))
        .and_then(|record| record.last_success())
        .filter(|_| directory.join(TABLE_FILE).exists());
    eprintln!(
        "warning: prices not refreshed ({}); {}",
        failures.join("; ").replace('\n', " "),
        fetched.map_or("no fetched table".to_owned(), |at| format!(
            "using the table fetched {}",
            at.date_naive()
        ))
    );
}

fn update(from: Option<&Path>, json: bool) -> Result<()> {
    let from = from
        .map(Path::to_path_buf)
        .or_else(price_update::from_environment);
    let report = price_update::update(
        &data_directory()?,
        from.as_deref(),
        FetchLimits::UPDATE,
        Utc::now(),
    )?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print!("{}", render_update(&report));
    }
    let failed: Vec<&str> = report
        .sources
        .iter()
        .filter(|source| !source.ok)
        .map(|source| source.name.as_str())
        .collect();
    if !failed.is_empty() {
        bail!("{} failed and kept its earlier rows", failed.join(", "));
    }
    Ok(())
}

fn render_update(report: &UpdateReport) -> String {
    let mut text = format!("Price table: {}\n", report.table.display());
    for source in &report.sources {
        if source.ok {
            text.push_str(&format!(
                "{}: ok, {} models ({} new, {} changed, {} unchanged)\n",
                source.name,
                source.models,
                source.changes.added,
                source.changes.changed,
                source.changes.unchanged
            ));
        } else {
            text.push_str(&format!(
                "{}: failed, earlier rows kept: {}\n",
                source.name,
                source.error.as_deref().unwrap_or_default()
            ));
        }
    }
    text
}

/// The table usage pricing reads.
#[derive(Serialize)]
struct UsageTable {
    /// `named`, `environment`, `config`, `data`, `config+data`, `absent`,
    /// `config-invalid` or `invalid`.
    chosen_by: String,
    /// The files the table was read from.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Each table the lookup set aside, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ConfigTable {
    path: Option<PathBuf>,
    exists: bool,
}

#[derive(Serialize)]
struct DataTable {
    path: PathBuf,
    exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Whether a pricing command would refresh the data table now, and why.
#[derive(Serialize)]
struct Refresh {
    due: bool,
    /// `TAPES_OFFLINE=1` is set, so no command refreshes.
    offline: bool,
    /// `TAPES_PRICES` names a table, so no command refreshes.
    named_by_environment: bool,
    /// The data table's own decision, whatever the two above say.
    decision: RefreshDecision,
    last_success: Option<DateTime<Utc>>,
    last_attempt: Option<DateTime<Utc>>,
    reason: String,
}

#[derive(Serialize)]
struct Shown {
    schema: &'static str,
    usage_table: UsageTable,
    config_table: ConfigTable,
    data_table: DataTable,
    fetch: Option<FetchRecord>,
    refresh: Refresh,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// With a model, every row naming it; else each id's row in effect now.
    models: Vec<ModelPrice>,
}

fn usage_table() -> UsageTable {
    match pricing::lookup(None) {
        Ok(lookup) => {
            let warnings = lookup.warnings;
            match lookup.found {
                PriceLookup::Loaded(loaded) => UsageTable {
                    chosen_by: serde_json::to_value(loaded.origin)
                        .ok()
                        .and_then(|origin| origin.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    paths: loaded.paths,
                    error: None,
                    warnings,
                },
                PriceLookup::Absent => UsageTable {
                    chosen_by: "absent".to_owned(),
                    paths: Vec::new(),
                    error: None,
                    warnings,
                },
                PriceLookup::ConfigInvalid { path, error } => UsageTable {
                    chosen_by: "config-invalid".to_owned(),
                    paths: vec![path],
                    error: Some(error),
                    warnings,
                },
            }
        }
        Err(error) => UsageTable {
            chosen_by: "invalid".to_owned(),
            paths: Vec::new(),
            error: Some(format!("{error:#}")),
            warnings: Vec::new(),
        },
    }
}

fn refresh(record: Option<&FetchRecord>, exists: bool, now: DateTime<Utc>) -> Refresh {
    let offline = offline(false);
    let named_by_environment = pricing::environment_table_path().is_some();
    let decision = RefreshDecision::at(record, exists, now);
    let reason = if offline {
        format!("TAPES_OFFLINE=1 is set; {}", decision.reason())
    } else if named_by_environment {
        format!("TAPES_PRICES names the table; {}", decision.reason())
    } else {
        decision.reason()
    };
    Refresh {
        due: !offline && !named_by_environment && decision.due(),
        offline,
        named_by_environment,
        decision,
        last_success: record.and_then(FetchRecord::last_success),
        last_attempt: record.map(|record| record.attempted_at),
        reason,
    }
}

fn in_effect(row: &ModelPrice, now: DateTime<Utc>) -> bool {
    row.effective_from.is_none_or(|from| from <= now)
        && row.effective_until.is_none_or(|until| now < until)
}

fn show(model: Option<&str>, json: bool) -> Result<()> {
    let directory = data_directory()?;
    let path = directory.join(TABLE_FILE);
    let exists = path.exists();
    let (table, error) = if exists {
        match PriceTable::read(&path) {
            Ok(table) => (Some(table), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        }
    } else {
        (None, None)
    };
    let now = Utc::now();
    let models: Vec<ModelPrice> = match (&table, model) {
        (Some(table), Some(model)) => table.rows(model).cloned().collect(),
        (Some(table), None) => table
            .models
            .iter()
            .filter(|row| in_effect(row, now))
            .cloned()
            .collect(),
        (None, _) => Vec::new(),
    };
    let fetch = FetchRecord::read(&directory.join(FETCH_FILE));
    let config = pricing::default_table_path();
    let shown = Shown {
        schema: SHOW_SCHEMA,
        usage_table: usage_table(),
        config_table: ConfigTable {
            exists: config.as_ref().is_some_and(|path| path.exists()),
            path: config,
        },
        refresh: refresh(fetch.as_ref(), exists, now),
        data_table: DataTable {
            path,
            exists,
            error,
        },
        fetch,
        model: model.map(str::to_owned),
        models,
    };
    if json {
        println!("{}", serde_json::to_string(&shown)?);
    } else {
        print!("{}", render_show(&shown));
    }
    if model.is_some() && shown.models.is_empty() {
        bail!(
            "no row of {} names {}",
            shown.data_table.path.display(),
            model.unwrap_or_default()
        );
    }
    Ok(())
}

fn rates(prices: &CategoryPrices) -> String {
    let named = [
        ("in", prices.uncached_input),
        ("cache-read", prices.cache_read),
        ("write-5m", prices.cache_write_5m),
        ("write-1h", prices.cache_write_1h),
        ("out", prices.output),
    ];
    let shown: Vec<String> = named
        .iter()
        .filter_map(|(name, price)| price.map(|price| format!("{name} {price}")))
        .collect();
    if shown.is_empty() {
        "no prices".to_owned()
    } else {
        shown.join(", ")
    }
}

fn long_context(row: &ModelPrice) -> String {
    row.long_context
        .as_ref()
        .map(|term| {
            format!(
                " +long-context {} {}",
                if term.inclusive { "from" } else { "above" },
                term.prompt_tokens
            )
        })
        .unwrap_or_default()
}

fn render_show(shown: &Shown) -> String {
    let usage = &shown.usage_table;
    let mut text = format!("Usage pricing reads: {}", usage.chosen_by);
    for path in &usage.paths {
        text.push_str(&format!(" {}", path.display()));
    }
    if let Some(error) = &usage.error {
        text.push_str(&format!(" ({error})"));
    }
    text.push('\n');
    for warning in &usage.warnings {
        text.push_str(&format!("  set aside: {warning}\n"));
    }
    text.push_str(&format!(
        "Config table: {}\n",
        match (&shown.config_table.path, shown.config_table.exists) {
            (Some(path), true) => path.display().to_string(),
            (Some(path), false) => format!("{} (absent)", path.display()),
            (None, _) => "none (neither XDG_CONFIG_HOME nor HOME is set)".to_owned(),
        }
    ));
    text.push_str(&format!(
        "Data table: {}{}\n",
        shown.data_table.path.display(),
        match (&shown.data_table.error, shown.data_table.exists) {
            (Some(error), _) => format!(" (unreadable: {error})"),
            (None, false) => " (absent)".to_owned(),
            (None, true) => String::new(),
        }
    ));
    text.push_str(&format!(
        "Refresh: {} ({})\n",
        if shown.refresh.due { "due" } else { "not due" },
        shown.refresh.reason
    ));
    match &shown.fetch {
        Some(record) => {
            text.push_str(&format!(
                "Last fetch: {}\n",
                record.attempted_at.to_rfc3339()
            ));
            for (name, source) in &record.sources {
                text.push_str(&format!(
                    "  {name}: {}, last success {}{}\n",
                    if source.ok { "ok" } else { "failed" },
                    source
                        .fetched_at
                        .map_or("never".to_owned(), |at| at.to_rfc3339()),
                    source
                        .error
                        .as_deref()
                        .map(|error| format!(": {error}"))
                        .unwrap_or_default()
                ));
            }
        }
        None => text.push_str("Last fetch: none recorded\n"),
    }
    for row in &shown.models {
        if shown.model.is_some() {
            let window = |at: Option<DateTime<Utc>>, open: &str| {
                at.map_or(open.to_owned(), |at| at.to_rfc3339())
            };
            text.push_str(&format!(
                "{}{}: {}{} [{} .. {}) effective {} source {}\n",
                row.id,
                if row.aliases.is_empty() {
                    String::new()
                } else {
                    format!(" (aliases {})", row.aliases.join(", "))
                },
                rates(&row.per_million),
                long_context(row),
                window(row.effective_from, "always"),
                window(row.effective_until, "open"),
                row.effective,
                row.source
            ));
            if let Some(long) = row
                .long_context
                .as_ref()
                .and_then(|term| term.per_million.as_ref())
            {
                text.push_str(&format!("  long-context rates: {}\n", rates(long)));
            }
        } else {
            text.push_str(&format!(
                "{}: {}{} ({})\n",
                row.id,
                rates(&row.per_million),
                long_context(row),
                row.source
            ));
        }
    }
    text
}
