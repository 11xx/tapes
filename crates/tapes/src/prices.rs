//! `tapes prices`: fetch the vendors' published prices into the data
//! directory's table, and show what the tables hold.

use std::path::{Path, PathBuf};

use agent_tapes_core::price_update::{self, FetchRecord, UpdateReport, FETCH_FILE, TABLE_FILE};
use agent_tapes_core::pricing::{self, CategoryPrices, ModelPrice, PriceLookup, PriceTable};
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use serde::Serialize;

pub const SHOW_SCHEMA: &str = "tapes-prices-show/1";

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
        /// instead of fetching; a missing file fails that source.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Print the outcome as JSON, tapes-prices-update/1.
        #[arg(long)]
        json: bool,
    },
    /// Show which table usage pricing reads and how it was chosen, the data
    /// table's path, each source's last fetch, and per model its rates in
    /// effect now, any long-context term, and its source. With MODEL, every
    /// row naming it — by id, alias, or as a dated id `-YYYYMMDD` whose
    /// undated id a row names — with its validity window. JSON is
    /// tapes-prices-show/1.
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

fn update(from: Option<&Path>, json: bool) -> Result<()> {
    let report = price_update::update(&data_directory()?, from, Utc::now())?;
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
    /// `named`, `environment`, `default`, `absent`, `default-invalid` or
    /// `invalid`.
    chosen_by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct DataTable {
    path: PathBuf,
    exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct Shown {
    schema: &'static str,
    usage_table: UsageTable,
    data_table: DataTable,
    fetch: Option<FetchRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// With a model, every row naming it; else each id's row in effect now.
    models: Vec<ModelPrice>,
}

fn usage_table() -> UsageTable {
    match pricing::lookup(None) {
        Ok(PriceLookup::Loaded(loaded)) => UsageTable {
            chosen_by: serde_json::to_value(loaded.origin)
                .ok()
                .and_then(|origin| origin.as_str().map(str::to_owned))
                .unwrap_or_default(),
            path: Some(loaded.path),
            error: None,
        },
        Ok(PriceLookup::Absent) => UsageTable {
            chosen_by: "absent".to_owned(),
            path: None,
            error: None,
        },
        Ok(PriceLookup::DefaultInvalid { path, error }) => UsageTable {
            chosen_by: "default-invalid".to_owned(),
            path: Some(path),
            error: Some(error),
        },
        Err(error) => UsageTable {
            chosen_by: "invalid".to_owned(),
            path: None,
            error: Some(format!("{error:#}")),
        },
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
    let shown = Shown {
        schema: SHOW_SCHEMA,
        usage_table: usage_table(),
        data_table: DataTable {
            path,
            exists,
            error,
        },
        fetch: FetchRecord::read(&directory.join(FETCH_FILE)),
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
    if let Some(path) = &usage.path {
        text.push_str(&format!(" {}", path.display()));
    }
    if let Some(error) = &usage.error {
        text.push_str(&format!(" ({error})"));
    }
    text.push('\n');
    text.push_str(&format!(
        "Data table: {}{}\n",
        shown.data_table.path.display(),
        match (&shown.data_table.error, shown.data_table.exists) {
            (Some(error), _) => format!(" (unreadable: {error})"),
            (None, false) => " (absent; run `tapes prices update`)".to_owned(),
            (None, true) => String::new(),
        }
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
