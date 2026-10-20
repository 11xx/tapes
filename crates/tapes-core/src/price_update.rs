//! A price table fetched from the vendors' published pages, kept with its
//! history under the user's data directory.
//!
//! Three sources feed it: Anthropic's and OpenAI's pricing pages, read as
//! markdown, and LiteLLM's community-maintained JSON, which prices the
//! models the two official pages do not. Each source is fetched with
//! `curl`, or read from a directory of copies, and parsed strictly: a page
//! whose tables no longer have the shape tapes reads fails that source,
//! which then contributes nothing and leaves its earlier rows as they were.
//!
//! The vendors' pages show only current prices, so the table keeps history
//! as validity windows. A fetched price equal to the row in effect keeps
//! that row; a different one closes the row at the update instant and opens
//! a new one there; a new id gets a row open backwards, so it also prices
//! earlier sessions. Rows are never dropped.
//!
//! Usage pricing reads this table beneath the user's config table (see
//! [`crate::pricing::lookup`]). A command that prices refreshes it first
//! when [`refresh_due`] holds, under [`FetchLimits::REFRESH`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant as Clock};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};

use crate::pricing::{
    self, BillingKind, CategoryPrices, LongContext, ModelPrice, PriceTable, PromptCategory,
};

/// The schema of `tapes prices update --json`.
pub const UPDATE_SCHEMA: &str = "tapes-prices-update/1";
/// The schema of the record of each fetch.
pub const FETCH_SCHEMA: &str = "tapes-price-fetch/1";
/// The table's file name in the data directory.
pub const TABLE_FILE: &str = "prices.toml";
/// The fetch record's file name in the data directory.
pub const FETCH_FILE: &str = "prices-fetch.json";
/// The `source` of every row LiteLLM priced.
pub const LITELLM_SOURCE: &str = "LiteLLM";
/// The most of a source read into memory.
const SOURCE_BYTES: u64 = 16 * 1024 * 1024;
/// Bytes of `curl`'s own error kept to name what failed.
const ERROR_BYTES: u64 = 2 * 1024;
/// The environment variable naming a directory of copies every update
/// reads in place of fetching, when it is given no other.
pub const FROM_ENV: &str = "TAPES_PRICES_FROM";
/// How long a fetched table stays fresh after a source last succeeded.
pub const FRESH_FOR: chrono::TimeDelta = chrono::TimeDelta::hours(24);
/// How long after an attempt a stale table waits before the next one.
pub const RETRY_AFTER: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// How long one source's fetch may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FetchLimits {
    /// `curl --connect-timeout`, when it is given one.
    pub connect: Option<Duration>,
    /// `curl --max-time`.
    pub transfer: Duration,
    /// How long `curl` may run before it is killed.
    pub kill: Duration,
}

impl FetchLimits {
    /// The limits of `tapes prices update`.
    pub const UPDATE: Self = Self {
        connect: None,
        transfer: Duration::from_secs(30),
        kill: Duration::from_secs(45),
    };
    /// The limits of the refresh a pricing command runs before it prices.
    pub const REFRESH: Self = Self {
        connect: Some(Duration::from_secs(5)),
        transfer: Duration::from_secs(10),
        kill: Duration::from_secs(15),
    };
}

/// The directory of copies `TAPES_PRICES_FROM` names, when it names one.
pub fn from_environment() -> Option<PathBuf> {
    std::env::var_os(FROM_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Whether the fetched table is due a refresh at `now`: it is absent, or no
/// source of `record` succeeded within [`FRESH_FOR`]; and the last attempt
/// is more than [`RETRY_AFTER`] old, or none is recorded.
pub fn refresh_due(record: Option<&FetchRecord>, table_exists: bool, now: DateTime<Utc>) -> bool {
    let fresh = table_exists
        && record.is_some_and(|record| {
            record
                .last_success()
                .is_some_and(|success| now - success < FRESH_FOR)
        });
    !fresh && record.is_none_or(|record| now - record.attempted_at > RETRY_AFTER)
}

/// One place prices are read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    /// The name the fetch record and the report use.
    pub name: &'static str,
    pub url: &'static str,
    /// The file `--from DIR` reads in place of the URL.
    pub file: &'static str,
}

pub const ANTHROPIC: Source = Source {
    name: "anthropic",
    url: "https://platform.claude.com/docs/en/about-claude/pricing.md",
    file: "anthropic.md",
};
pub const OPENAI: Source = Source {
    name: "openai",
    url: "https://platform.openai.com/docs/pricing.md",
    file: "openai.md",
};
pub const LITELLM: Source = Source {
    name: "litellm",
    url: "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
    file: "litellm.json",
};
pub const SOURCES: [Source; 3] = [ANTHROPIC, OPENAI, LITELLM];

/// One model's prices as a source states them now.
#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    pub id: String,
    pub per_million: CategoryPrices,
    pub long_context: Option<LongContext>,
    pub source: String,
}

/// The data directory: `$XDG_DATA_HOME/tapes`, or `~/.local/share/tapes`
/// when that variable is unset or empty.
pub fn data_directory() -> Option<PathBuf> {
    let configured = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    configured
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .map(|directory| directory.join("tapes"))
}

/// Cells of a markdown table row, trimmed; none for a line that is not one.
fn cells(line: &str) -> Option<Vec<String>> {
    let line = line.trim();
    let inner = line.strip_prefix('|')?.strip_suffix('|')?;
    Some(
        inner
            .split('|')
            .map(|cell| cell.trim().to_owned())
            .collect(),
    )
}

/// `text` with every `<sup>…</sup>` removed.
fn strip_sup(text: &str) -> String {
    let mut kept = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<sup>") {
        kept.push_str(&rest[..start]);
        match rest[start..].find("</sup>") {
            Some(end) => rest = &rest[start + end + "</sup>".len()..],
            None => {
                rest = "";
            }
        }
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

/// A per-million price written `$X / MTok`.
fn mtok_price(cell: &str) -> Result<f64> {
    let cell = strip_sup(cell);
    cell.strip_prefix('$')
        .and_then(|rest| rest.strip_suffix(" / MTok"))
        .and_then(|number| number.trim().parse::<f64>().ok())
        .filter(|price| price.is_finite() && *price >= 0.0)
        .ok_or_else(|| anyhow!("`{cell}` is not a price written $X / MTok"))
}

/// `claude-<family>-<n>[-<m>]` from `Claude <Family> <N>[.<M>]`.
fn claude_id(name: &str) -> Result<String> {
    let words: Vec<&str> = name.split_whitespace().collect();
    let refuse = || anyhow!("`{name}` is not a model named Claude <Family> <N> or <N.M>");
    let [claude, family, version] = words.as_slice() else {
        return Err(refuse());
    };
    if *claude != "Claude" || !family.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(refuse());
    }
    let parts: Vec<&str> = version.split('.').collect();
    if parts.is_empty()
        || parts.len() > 2
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(refuse());
    }
    Ok(format!(
        "claude-{}-{}",
        family.to_lowercase(),
        parts.join("-")
    ))
}

const ANTHROPIC_HEADER: [&str; 6] = [
    "Model",
    "Base input tokens",
    "5m cache writes",
    "1h cache writes",
    "Cache hits and refreshes",
    "Output tokens",
];

/// Which half of a long-context pair a row is.
enum Half {
    Whole,
    UpTo(u64),
    Over(u64),
}

/// The model cell's name and the pair half its parenthetical names. A
/// trailing parenthetical holding a markdown link is dropped.
fn anthropic_model(cell: &str) -> Result<(String, Half)> {
    let cell = strip_sup(cell);
    let Some(open) = cell.find('(') else {
        return Ok((cell, Half::Whole));
    };
    let name = cell[..open].trim().to_owned();
    let note = cell[open..].trim();
    let inner = note
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| anyhow!("`{cell}` has a parenthetical tapes does not read"))?;
    if inner.starts_with('[') && inner.contains("](") && inner.ends_with(')') {
        return Ok((name, Half::Whole));
    }
    let tokens = |text: &str| {
        text.strip_suffix(" tokens")
            .map(|number| number.replace(',', ""))
            .and_then(|number| number.parse::<u64>().ok())
    };
    if let Some(n) = inner.strip_prefix("for prompts up to ").and_then(tokens) {
        return Ok((name, Half::UpTo(n)));
    }
    if let Some(n) = inner.strip_prefix("for prompts over ").and_then(tokens) {
        return Ok((name, Half::Over(n)));
    }
    bail!("`{cell}` has a parenthetical tapes does not read")
}

/// Anthropic's pricing page: the table under `## Model pricing`.
pub fn parse_anthropic(text: &str, source: &str) -> Result<Vec<Fetched>> {
    let mut lines = text
        .lines()
        .skip_while(|line| line.trim() != "## Model pricing")
        .skip(1)
        .take_while(|line| !line.starts_with("## "));
    let header = lines
        .find(|line| line.trim_start().starts_with('|'))
        .and_then(cells)
        .ok_or_else(|| anyhow!("no table under `## Model pricing`"))?;
    if header != ANTHROPIC_HEADER {
        bail!(
            "the `## Model pricing` table's header is `{}`, not `{}`",
            header.join(" | "),
            ANTHROPIC_HEADER.join(" | ")
        );
    }
    lines.next();
    let mut rows: Vec<(String, Prompted)> = Vec::new();
    for line in lines {
        let Some(row) = cells(line) else { break };
        if row.len() != ANTHROPIC_HEADER.len() {
            bail!("`{}` does not have six cells", line.trim());
        }
        let (name, half) = anthropic_model(&row[0])?;
        let id = claude_id(&name)?;
        let prices = CategoryPrices {
            uncached_input: Some(mtok_price(&row[1])?),
            cache_write_5m: Some(mtok_price(&row[2])?),
            cache_write_1h: Some(mtok_price(&row[3])?),
            cache_read: Some(mtok_price(&row[4])?),
            output: Some(mtok_price(&row[5])?),
        };
        let position = match rows.iter().position(|(named, _)| *named == id) {
            Some(position) => position,
            None => {
                rows.push((id, Prompted::default()));
                rows.len() - 1
            }
        };
        let prompted = &mut rows[position].1;
        let slot = match half {
            Half::Whole => &mut prompted.whole,
            Half::UpTo(tokens) => &mut prompted.up_to.get_or_insert((tokens, None)).1,
            Half::Over(tokens) => &mut prompted.over.get_or_insert((tokens, None)).1,
        };
        if slot.replace(prices).is_some() {
            bail!("`{name}` is priced twice");
        }
    }
    if rows.is_empty() {
        bail!("the `## Model pricing` table has no rows");
    }
    rows.into_iter()
        .map(|(id, prompted)| {
            let (per_million, long_context) = match prompted {
                Prompted {
                    whole: Some(prices),
                    up_to: None,
                    over: None,
                } => (prices, None),
                Prompted {
                    whole: None,
                    up_to: Some((up_to, Some(base))),
                    over: Some((over, Some(long))),
                } if up_to == over => (
                    base,
                    Some(long_context_term(
                        over,
                        vec![
                            PromptCategory::UncachedInput,
                            PromptCategory::CacheRead,
                            PromptCategory::CacheWrite,
                        ],
                        long,
                    )),
                ),
                _ => bail!(
                    "`{id}` is not one row, nor a pair of rows for prompts up to and over one \
                     size"
                ),
            };
            Ok(Fetched {
                id,
                per_million,
                long_context,
                source: source.to_owned(),
            })
        })
        .collect()
}

/// One Claude model's rows: a whole row, or the halves of a prompt-size
/// pair, each with its size.
#[derive(Default)]
struct Prompted {
    whole: Option<CategoryPrices>,
    up_to: Option<(u64, Option<CategoryPrices>)>,
    over: Option<(u64, Option<CategoryPrices>)>,
}

/// A per-token API term charging `per_million` above `above` tokens.
fn long_context_term(
    above: u64,
    mut counts: Vec<PromptCategory>,
    per_million: CategoryPrices,
) -> LongContext {
    counts.sort();
    LongContext {
        prompt_tokens: above,
        inclusive: false,
        counts,
        billing: vec![BillingKind::Api],
        per_million: Some(per_million),
        multipliers: None,
    }
}

const OPENAI_PLAIN_HEADER: [&str; 4] = ["Model", "Input", "Output", "Cached input"];
const OPENAI_CONTEXT_COLUMNS: [&str; 6] = [
    "Short context input",
    "Short context output",
    "Short context cached input",
    "Long context input",
    "Long context output",
    "Long context cached input",
];

/// A dollar cell; `-` is no price.
fn dollar_price(cell: &str) -> Result<Option<f64>> {
    if cell == "-" {
        return Ok(None);
    }
    cell.strip_prefix('$')
        .and_then(|number| number.parse::<f64>().ok())
        .filter(|price| price.is_finite() && *price >= 0.0)
        .map(Some)
        .ok_or_else(|| anyhow!("`{cell}` is not a price written $X or -"))
}

/// The prompt size past which OpenAI's long-context prices apply, from
/// `Short context: ≤272K input tokens. Long context: >272K input tokens.`
fn openai_threshold(text: &str) -> Result<u64> {
    let kilo = |text: &str| {
        text.strip_suffix('K')
            .and_then(|number| number.parse::<u64>().ok())
    };
    text.lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("Short context: ≤")?;
            let (short, rest) = rest.split_once(" input tokens. Long context: >")?;
            let long = rest.strip_suffix(" input tokens.")?;
            let (short, long) = (kilo(short)?, kilo(long)?);
            (short == long).then_some(short * 1000)
        })
        .ok_or_else(|| {
            anyhow!(
                "the page does not say where long context starts, as `Short context: ≤NK input \
                 tokens. Long context: >NK input tokens.`"
            )
        })
}

/// OpenAI's pricing page: every table under each `### Standard pricing
/// data` heading.
pub fn parse_openai(text: &str, source: &str) -> Result<Vec<Fetched>> {
    let threshold = openai_threshold(text)?;
    let mut fetched: Vec<Fetched> = Vec::new();
    let mut standard = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            standard = trimmed == "### Standard pricing data";
            continue;
        }
        if !standard {
            continue;
        }
        let Some(header) = cells(trimmed) else {
            continue;
        };
        lines.next();
        let mut rows = Vec::new();
        while let Some(row) = lines.peek().and_then(|line| cells(line)) {
            lines.next();
            rows.push(row);
        }
        let column = |name: &str| header.iter().position(|cell| cell == name);
        let context: Option<Vec<usize>> = OPENAI_CONTEXT_COLUMNS
            .iter()
            .map(|name| column(name))
            .collect();
        let plain = header == OPENAI_PLAIN_HEADER;
        if header.first().map(String::as_str) != Some("Model") || (context.is_none() && !plain) {
            bail!(
                "a `### Standard pricing data` table's header is `{}`, which tapes does not read",
                header.join(" | ")
            );
        }
        for row in rows {
            if row.len() != header.len() {
                bail!("`{}` does not have {} cells", row.join(" | "), header.len());
            }
            let id = row[0].replace(" (<272K context length)", "");
            if id.is_empty() || id.contains([' ', '(', ')']) {
                bail!("`{}` is not a model id", row[0]);
            }
            let row = match &context {
                Some(columns) => {
                    let price = |index: usize| dollar_price(&row[columns[index]]);
                    let long = CategoryPrices {
                        uncached_input: price(3)?,
                        output: price(4)?,
                        cache_read: price(5)?,
                        ..CategoryPrices::default()
                    };
                    Fetched {
                        id,
                        per_million: CategoryPrices {
                            uncached_input: price(0)?,
                            output: price(1)?,
                            cache_read: price(2)?,
                            ..CategoryPrices::default()
                        },
                        long_context: (long != CategoryPrices::default()).then(|| {
                            long_context_term(
                                threshold,
                                vec![PromptCategory::UncachedInput, PromptCategory::CacheRead],
                                long,
                            )
                        }),
                        source: format!(
                            "{source} (30m and 12h cache writes have no tapes category and are \
                             unpriced)"
                        ),
                    }
                }
                None => Fetched {
                    id,
                    per_million: CategoryPrices {
                        uncached_input: dollar_price(&row[1])?,
                        output: dollar_price(&row[2])?,
                        cache_read: dollar_price(&row[3])?,
                        ..CategoryPrices::default()
                    },
                    long_context: None,
                    source: source.to_owned(),
                },
            };
            match fetched.iter().find(|earlier| earlier.id == row.id) {
                Some(earlier)
                    if earlier.per_million != row.per_million
                        || earlier.long_context != row.long_context =>
                {
                    bail!("`{}` is priced differently in two tables", row.id)
                }
                Some(_) => {}
                None => fetched.push(row),
            }
        }
    }
    if fetched.is_empty() {
        bail!("no `### Standard pricing data` table priced a model");
    }
    Ok(fetched)
}

/// A per-token cost as a per-million price, rounded clear of the float
/// error the multiplication leaves.
fn per_million(cost: f64) -> f64 {
    (cost * 1e6 * 1e9).round() / 1e9
}

/// LiteLLM's 1-hour cache write price, the one `_above_` key that names no
/// tier.
const CACHE_WRITE_1H_KEY: &str = "cache_creation_input_token_cost_above_1hr";

/// LiteLLM's JSON: its chat models without a provider prefix or a tiered
/// `_above_` price, whose tiers tapes cannot read and whose base rate alone
/// would underprice them, other than the ids in `official` and their dated
/// forms.
pub fn parse_litellm(text: &str, official: &BTreeSet<String>) -> Result<Vec<Fetched>> {
    let entries: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(text).context("LiteLLM's prices are not a JSON object")?;
    let mut fetched = Vec::new();
    for (id, entry) in entries {
        let Some(fields) = entry.as_object() else {
            continue;
        };
        if id.contains('/')
            || official.contains(&id)
            || pricing::undated(&id).is_some_and(|undated| official.contains(undated))
            || fields.get("mode").and_then(|mode| mode.as_str()) != Some("chat")
            || fields
                .keys()
                .any(|key| key.contains("_above_") && key != CACHE_WRITE_1H_KEY)
        {
            continue;
        }
        let cost = |key: &str| {
            fields
                .get(key)
                .and_then(|value| value.as_f64())
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .map(per_million)
        };
        let (Some(input), Some(output)) =
            (cost("input_cost_per_token"), cost("output_cost_per_token"))
        else {
            continue;
        };
        fetched.push(Fetched {
            id,
            per_million: CategoryPrices {
                uncached_input: Some(input),
                output: Some(output),
                cache_read: cost("cache_read_input_token_cost"),
                cache_write_5m: cost("cache_creation_input_token_cost"),
                cache_write_1h: cost(CACHE_WRITE_1H_KEY),
            },
            long_context: None,
            source: LITELLM_SOURCE.to_owned(),
        });
    }
    if fetched.is_empty() {
        bail!("LiteLLM's prices name no chat model tapes reads");
    }
    Ok(fetched)
}

/// What one update did to the table's rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    /// Ids priced for the first time.
    pub added: usize,
    /// Ids whose price changed, each now split at the update instant.
    pub changed: usize,
    pub unchanged: usize,
}

/// `existing` rows updated by `fetched` prices at `now`.
pub fn merge(
    existing: &[ModelPrice],
    fetched: &[Fetched],
    now: DateTime<Utc>,
) -> (Vec<ModelPrice>, Changes) {
    let mut rows = existing.to_vec();
    let mut changes = Changes::default();
    for price in fetched {
        let fresh = ModelPrice {
            id: price.id.clone(),
            aliases: Vec::new(),
            effective: now.date_naive(),
            source: price.source.clone(),
            per_million: price.per_million.clone(),
            effective_from: None,
            effective_until: None,
            long_context: price.long_context.clone(),
            time_of_day: Vec::new(),
        };
        let named = rows.iter().any(|row| row.id == price.id);
        let current = rows.iter().position(|row| {
            row.id == price.id
                && row.effective_from.is_none_or(|from| from <= now)
                && row.effective_until.is_none_or(|until| now < until)
        });
        match current {
            None if !named => {
                rows.push(fresh);
                changes.added += 1;
            }
            Some(position)
                if rows[position].per_million == fresh.per_million
                    && rows[position].long_context == fresh.long_context
                    && rows[position].time_of_day.is_empty() =>
            {
                changes.unchanged += 1;
            }
            Some(position) if rows[position].effective_from == Some(now) => {
                rows[position] = ModelPrice {
                    effective_from: Some(now),
                    effective_until: rows[position].effective_until,
                    ..fresh
                };
                changes.changed += 1;
            }
            Some(position) => {
                rows[position].effective_until = Some(now);
                rows.push(ModelPrice {
                    effective_from: Some(now),
                    ..fresh
                });
                changes.changed += 1;
            }
            None => {
                rows.push(ModelPrice {
                    effective_from: Some(now),
                    ..fresh
                });
                changes.changed += 1;
            }
        }
    }
    (rows, changes)
}

/// One source's part in the latest fetch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceRecord {
    pub url: String,
    pub ok: bool,
    /// When the source last succeeded.
    pub fetched_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// The models the source priced when it last succeeded.
    pub models: usize,
}

/// `prices-fetch.json`: when tapes last fetched, and how each source fared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FetchRecord {
    pub schema: String,
    pub attempted_at: DateTime<Utc>,
    pub sources: BTreeMap<String, SourceRecord>,
}

impl FetchRecord {
    /// The record at `path`; none when it is absent or not one.
    pub fn read(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<Self>(&text)
            .ok()
            .filter(|record| record.schema == FETCH_SCHEMA)
    }

    /// When any source last succeeded.
    pub fn last_success(&self) -> Option<DateTime<Utc>> {
        self.sources
            .values()
            .filter_map(|source| source.fetched_at)
            .max()
    }
}

/// How one source fared in an update.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SourceOutcome {
    pub name: String,
    pub url: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub models: usize,
    pub changes: Changes,
}

/// What `tapes prices update` did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpdateReport {
    pub schema: &'static str,
    pub table: PathBuf,
    pub record: PathBuf,
    pub attempted_at: DateTime<Utc>,
    pub sources: Vec<SourceOutcome>,
}

impl UpdateReport {
    pub fn failed(&self) -> bool {
        self.sources.iter().any(|source| !source.ok)
    }
}

/// Read one source, from `from` when given, else over the network.
fn read_source(source: &Source, from: Option<&Path>, limits: FetchLimits) -> Result<String> {
    match from {
        Some(directory) => {
            let path = directory.join(source.file);
            let file = std::fs::File::open(&path)
                .with_context(|| format!("cannot open {}", path.display()))?;
            let (bytes, truncated) = crate::pull_request::read_bounded(file, SOURCE_BYTES)?;
            if truncated {
                bail!("{} is larger than {SOURCE_BYTES} bytes", path.display());
            }
            String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
        }
        None => fetch(source.url, limits),
    }
}

/// Fetch `url` with `curl`, over HTTPS only, bounded by `limits` and in
/// size.
pub fn fetch(url: &str, limits: FetchLimits) -> Result<String> {
    let connect: Vec<String> = limits
        .connect
        .map(|connect| {
            vec![
                "--connect-timeout".to_owned(),
                connect.as_secs().to_string(),
            ]
        })
        .unwrap_or_default();
    let mut child = match Command::new("curl")
        .args(&connect)
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--max-time",
            &limits.transfer.as_secs().to_string(),
            "--max-filesize",
            &SOURCE_BYTES.to_string(),
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
            "prices are fetched with `curl`, which is not installed: install curl, or pass \
             copies of the pages with --from DIR"
        ),
        Err(error) => return Err(error).context("failed to run curl"),
    };
    let out = child.stdout.take().expect("stdout is piped");
    let err = child.stderr.take().expect("stderr is piped");
    let stdout = std::thread::spawn(move || crate::pull_request::read_bounded(out, SOURCE_BYTES));
    let stderr = std::thread::spawn(move || crate::pull_request::read_bounded(err, ERROR_BYTES));
    let started = Clock::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > limits.kill {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "curl gave no answer for {url} in {} seconds",
                limits.kill.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (stdout, truncated) = stdout.join().map_err(|_| anyhow!("curl output thread"))??;
    let (stderr, _) = stderr.join().map_err(|_| anyhow!("curl error thread"))??;
    if !status.success() {
        let said = String::from_utf8_lossy(&stderr);
        bail!("curl {url} failed ({status}): {}", said.trim());
    }
    if truncated {
        bail!("{url} is larger than {SOURCE_BYTES} bytes");
    }
    String::from_utf8(stdout).with_context(|| format!("{url} is not UTF-8"))
}

/// Write `contents` to `path` through a temporary file in its directory.
fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let directory = path.parent().expect("a data file has a directory");
    std::fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let name = path.file_name().expect("a data file has a name");
    let temporary = directory.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temporary, contents)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot replace {}", path.display())
    })
}

/// Update the table in `directory` from every source, read from `from`
/// when given, else fetched within `limits`. A source that fails
/// contributes nothing; the others still update. An existing table that
/// cannot be read refuses the whole update, so nothing it holds is lost.
pub fn update(
    directory: &Path,
    from: Option<&Path>,
    limits: FetchLimits,
    now: DateTime<Utc>,
) -> Result<UpdateReport> {
    let now = now.trunc_subsecs(0);
    let table_path = directory.join(TABLE_FILE);
    let record_path = directory.join(FETCH_FILE);
    let (currency, mut rows) = if table_path.exists() {
        let table = PriceTable::read(&table_path)?;
        (table.currency, table.models)
    } else {
        ("USD".to_owned(), Vec::new())
    };
    let previous = FetchRecord::read(&record_path);
    let mut parsed: Vec<(Source, Result<Vec<Fetched>>)> = Vec::new();
    for source in [ANTHROPIC, OPENAI] {
        let result = read_source(&source, from, limits).and_then(|text| match source.name {
            "anthropic" => parse_anthropic(&text, source.url),
            _ => parse_openai(&text, source.url),
        });
        parsed.push((source, result));
    }
    let official: BTreeSet<String> = parsed
        .iter()
        .filter_map(|(_, result)| result.as_ref().ok())
        .flatten()
        .map(|fetched| fetched.id.clone())
        .chain(
            rows.iter()
                .filter(|row| row.source != LITELLM_SOURCE)
                .map(|row| row.id.clone()),
        )
        .collect();
    let litellm =
        read_source(&LITELLM, from, limits).and_then(|text| parse_litellm(&text, &official));
    parsed.push((LITELLM, litellm));
    let mut outcomes = Vec::new();
    let mut record = FetchRecord {
        schema: FETCH_SCHEMA.to_owned(),
        attempted_at: now,
        sources: BTreeMap::new(),
    };
    for (source, result) in parsed {
        let earlier = previous
            .as_ref()
            .and_then(|record| record.sources.get(source.name));
        let outcome = match result {
            Ok(fetched) => {
                let (merged, changes) = merge(&rows, &fetched, now);
                rows = merged;
                record.sources.insert(
                    source.name.to_owned(),
                    SourceRecord {
                        url: source.url.to_owned(),
                        ok: true,
                        fetched_at: Some(now),
                        error: None,
                        models: fetched.len(),
                    },
                );
                SourceOutcome {
                    name: source.name.to_owned(),
                    url: source.url.to_owned(),
                    ok: true,
                    error: None,
                    models: fetched.len(),
                    changes,
                }
            }
            Err(error) => {
                let error = format!("{error:#}");
                record.sources.insert(
                    source.name.to_owned(),
                    SourceRecord {
                        url: source.url.to_owned(),
                        ok: false,
                        fetched_at: earlier.and_then(|earlier| earlier.fetched_at),
                        error: Some(error.clone()),
                        models: earlier.map_or(0, |earlier| earlier.models),
                    },
                );
                SourceOutcome {
                    name: source.name.to_owned(),
                    url: source.url.to_owned(),
                    ok: false,
                    error: Some(error),
                    models: 0,
                    changes: Changes::default(),
                }
            }
        };
        outcomes.push(outcome);
    }
    let text = PriceTable::to_toml(&currency, &rows)?;
    PriceTable::parse(&text).context("the updated price table does not read back")?;
    write_atomically(&table_path, text.as_bytes())?;
    let mut json = serde_json::to_vec_pretty(&record)?;
    json.push(b'\n');
    write_atomically(&record_path, &json)?;
    Ok(UpdateReport {
        schema: UPDATE_SCHEMA,
        table: table_path,
        record: record_path,
        attempted_at: now,
        sources: outcomes,
    })
}
