//! List-price estimates from a price table the caller supplies. tapes holds
//! no prices of its own: a price table file names, per model, what a million
//! tokens of each disjoint category cost, and an estimate prices a figure's
//! disjoint categories by the row of the model that served them. An
//! estimate sits beside any cost a harness wrote and never replaces it.
//! What it cannot price — a model the table lacks, usage no model is known
//! to have served, a counter the figure measured incompletely — leaves it a
//! known subtotal with a reason; a cache write with no recorded TTL bounds it
//! between the 5-minute and 1-hour prices.
//!
//! `tapes-price-table/1` prices every token of a category at one rate.
//! `tapes-price-table/2` adds terms priced by the request: a long-context
//! rate above a prompt size, for the billing it applies to; time-of-day
//! rates; and a validity window per row. A figure prices those terms request
//! by request where its reader kept its requests, and between the terms'
//! rates where it did not.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc, Weekday};
use serde::{Deserialize, Serialize};

use crate::measurement::{MeasurementCompleteness, MeasurementReason};
use crate::model::{
    CacheWrites, DisjointTokens, Model, ModelObservationStatus, ModelSelectionSpan, TokenConvention,
};
use crate::usage::ModelUsage;

pub const PRICE_TABLE_SCHEMA: &str = "tapes-price-table/1";
/// The table format whose rows may carry terms priced by the request.
pub const PRICE_TABLE_SCHEMA_V2: &str = "tapes-price-table/2";
pub const ESTIMATE_KIND: &str = "list-price-estimate";
/// The environment variable naming a price table, after `--prices`.
pub const PRICES_ENV: &str = "TAPES_PRICES";
/// The most of a price table file read into memory.
const MAX_TABLE_BYTES: u64 = 4 * 1024 * 1024;

/// The disjoint categories an estimate prices. Reasoning is a share of
/// output and is priced as output.
pub const PRICED_CATEGORIES: [&str; 4] = ["uncached_input", "cache_read", "cache_write", "output"];

/// What a million tokens of each disjoint category cost under one model. A
/// category the row leaves out has no price.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryPrices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uncached_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
}

/// One model's row: the id a harness records, other ids it answers to, the
/// date its prices were read and where, and, in a version 2 table, the terms
/// priced by the request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelPrice {
    pub id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub effective: NaiveDate,
    pub source: String,
    pub per_million: CategoryPrices,
    /// The first instant the row's prices are in effect; absent, always
    /// before `effective_until`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<DateTime<Utc>>,
    /// The instant the row's prices stop being in effect, exclusive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_until: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_context: Option<LongContext>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub time_of_day: Vec<TimeOfDayRates>,
}

impl ModelPrice {
    /// Whether the row prices every token of a category at one rate.
    fn is_plain(&self) -> bool {
        self.effective_from.is_none()
            && self.effective_until.is_none()
            && self.long_context.is_none()
            && self.time_of_day.is_empty()
    }

    /// Whether the row's prices are in effect at `at`.
    fn covers(&self, at: DateTime<Utc>) -> bool {
        self.effective_from.is_none_or(|from| from <= at)
            && self.effective_until.is_none_or(|until| at < until)
    }

    /// Whether the row's prices are in effect at some instant of `from..=to`,
    /// either end open.
    fn overlaps(&self, from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> bool {
        self.effective_from
            .zip(to)
            .is_none_or(|(start, to)| start <= to)
            && self
                .effective_until
                .zip(from)
                .is_none_or(|(until, from)| from < until)
    }

    /// The rates a request made at `at` pays below any long-context
    /// threshold: a time-of-day window's that holds it, else the row's.
    fn rates_at(&self, at: DateTime<Utc>) -> &CategoryPrices {
        self.time_of_day
            .iter()
            .find(|window| window.holds(at))
            .map_or(&self.per_million, |window| &window.per_million)
    }
}

/// Which billing a long-context term applies to: tokens billed one by one
/// through an API key, or a subscription plan's quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BillingKind {
    Api,
    Subscription,
}

/// The input categories a long-context threshold counts toward a request's
/// prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCategory {
    UncachedInput,
    CacheRead,
    CacheWrite,
}

/// Rates a whole request pays once its prompt passes a threshold, under
/// the billing the term names.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LongContext {
    /// The prompt size the term starts past, or at when `inclusive`.
    pub prompt_tokens: u64,
    pub inclusive: bool,
    /// The categories whose tokens make up the prompt.
    pub counts: Vec<PromptCategory>,
    pub billing: Vec<BillingKind>,
    /// Rates that replace the request's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_million: Option<CategoryPrices>,
    /// Factors on the request's own rates; a category left out keeps its
    /// rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multipliers: Option<CategoryPrices>,
}

impl LongContext {
    /// Whether a prompt of `tokens` passes the threshold.
    fn passed_by(&self, tokens: u64) -> bool {
        if self.inclusive {
            tokens >= self.prompt_tokens
        } else {
            tokens > self.prompt_tokens
        }
    }

    /// The rates a request above the threshold pays instead of `base`.
    fn rates(&self, base: &CategoryPrices) -> CategoryPrices {
        if let Some(prices) = &self.per_million {
            return prices.clone();
        }
        let factors = self.multipliers.clone().unwrap_or_default();
        let scale = |price: Option<f64>, factor: Option<f64>| {
            price.map(|price| price * factor.unwrap_or(1.0))
        };
        CategoryPrices {
            uncached_input: scale(base.uncached_input, factors.uncached_input),
            cache_read: scale(base.cache_read, factors.cache_read),
            cache_write_5m: scale(base.cache_write_5m, factors.cache_write_5m),
            cache_write_1h: scale(base.cache_write_1h, factors.cache_write_1h),
            output: scale(base.output, factors.output),
        }
    }

    fn prompt(&self, request: &RequestTokens) -> u64 {
        self.counts
            .iter()
            .map(|category| match category {
                PromptCategory::UncachedInput => request.uncached_input,
                PromptCategory::CacheRead => request.cache_read,
                PromptCategory::CacheWrite => request.cache_write_total(),
            })
            .sum()
    }
}

/// Rates a request pays when its time of day, in UTC, falls in
/// `from..until` on a day the window opens on; a window whose `until` is
/// not after `from` runs past midnight into the next day.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TimeOfDayRates {
    pub from: NaiveTime,
    pub until: NaiveTime,
    pub per_million: CategoryPrices,
    /// The UTC days of the week the window opens on; every day when absent.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_days"
    )]
    pub days: Option<Vec<Weekday>>,
}

impl TimeOfDayRates {
    fn holds(&self, at: DateTime<Utc>) -> bool {
        let time = at.time();
        let opened = if self.from < self.until {
            (self.from <= time && time < self.until).then_some(at.weekday())
        } else if self.from <= time {
            Some(at.weekday())
        } else {
            (time < self.until).then_some(at.weekday().pred())
        };
        opened.is_some_and(|day| self.days.as_ref().is_none_or(|days| days.contains(&day)))
    }
}

/// The days of the week a price table names, as it names them.
const DAY_NAMES: [(&str, Weekday); 7] = [
    ("mon", Weekday::Mon),
    ("tue", Weekday::Tue),
    ("wed", Weekday::Wed),
    ("thu", Weekday::Thu),
    ("fri", Weekday::Fri),
    ("sat", Weekday::Sat),
    ("sun", Weekday::Sun),
];

fn serialize_days<S: serde::Serializer>(
    days: &Option<Vec<Weekday>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let names = days.iter().flatten().filter_map(|day| {
        DAY_NAMES
            .iter()
            .find(|(_, named)| named == day)
            .map(|(name, _)| *name)
    });
    serializer.collect_seq(names)
}

/// A window's days of the week, each named once.
fn days_of_week(entry: &str, names: &[String]) -> Result<Vec<Weekday>> {
    if names.is_empty() {
        return Err(anyhow!(
            "{entry}: time_of_day days must name at least one day"
        ));
    }
    let mut days = Vec::new();
    for name in names {
        let day = DAY_NAMES
            .iter()
            .find(|(named, _)| named == name)
            .map(|(_, day)| *day)
            .ok_or_else(|| {
                anyhow!(
                    "{entry}: time_of_day days are written mon, tue, wed, thu, fri, sat or sun, \
                     not `{name}`"
                )
            })?;
        if days.contains(&day) {
            return Err(anyhow!("{entry}: time_of_day days name `{name}` twice"));
        }
        days.push(day);
    }
    Ok(days)
}

/// A validated price table.
#[derive(Clone, Debug, PartialEq)]
pub struct PriceTable {
    pub currency: String,
    pub models: Vec<ModelPrice>,
    /// The format the table was written in.
    pub schema: &'static str,
    index: HashMap<String, Vec<usize>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    schema: String,
    currency: String,
    #[serde(default)]
    models: Vec<RawModel>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    effective: toml::value::Datetime,
    source: String,
    per_million: CategoryPrices,
    effective_from: Option<toml::value::Datetime>,
    effective_until: Option<toml::value::Datetime>,
    long_context: Option<RawLongContext>,
    #[serde(default)]
    time_of_day: Vec<RawTimeOfDay>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLongContext {
    above_prompt_tokens: Option<u64>,
    from_prompt_tokens: Option<u64>,
    prompt_tokens: Vec<PromptCategory>,
    #[serde(default)]
    billing: Option<Vec<BillingKind>>,
    per_million: Option<CategoryPrices>,
    multipliers: Option<CategoryPrices>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTimeOfDay {
    from: String,
    until: String,
    per_million: CategoryPrices,
    days: Option<Vec<String>>,
}

/// Refuse a negative or non-finite price, naming the entry and category.
fn check_prices(entry: &str, prices: &CategoryPrices, what: &str) -> Result<()> {
    for (category, price) in [
        ("uncached_input", prices.uncached_input),
        ("cache_read", prices.cache_read),
        ("cache_write_5m", prices.cache_write_5m),
        ("cache_write_1h", prices.cache_write_1h),
        ("output", prices.output),
    ] {
        if price.is_some_and(|price| !price.is_finite() || price < 0.0) {
            return Err(anyhow!(
                "{entry}: the {category} {what} must be a finite number of at least zero"
            ));
        }
    }
    Ok(())
}

/// A validity bound: a date, at midnight UTC, or an instant with an offset.
fn instant(entry: &str, key: &str, value: &toml::value::Datetime) -> Result<DateTime<Utc>> {
    let refuse = || {
        anyhow!("{entry}: {key} must be a date or an instant with an offset, such as 2027-01-01T00:00:00Z")
    };
    let date = value.date.ok_or_else(refuse)?;
    let date = NaiveDate::from_ymd_opt(date.year.into(), date.month.into(), date.day.into())
        .ok_or_else(refuse)?;
    match (value.time, value.offset) {
        (None, None) => Ok(date.and_hms_opt(0, 0, 0).ok_or_else(refuse)?.and_utc()),
        (Some(_), Some(_)) => DateTime::parse_from_rfc3339(&value.to_string())
            .map(|instant| instant.with_timezone(&Utc))
            .map_err(|_| refuse()),
        _ => Err(refuse()),
    }
}

/// A time of day written `HH:MM`, in UTC.
fn time_of_day(entry: &str, key: &str, text: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(text, "%H:%M").map_err(|_| {
        anyhow!("{entry}: time_of_day {key} must be a UTC time written HH:MM, not `{text}`")
    })
}

/// Whether two rows are in effect at some instant together.
fn validity_overlaps(first: &ModelPrice, second: &ModelPrice) -> bool {
    first
        .effective_from
        .zip(second.effective_until)
        .is_none_or(|(from, until)| from < until)
        && second
            .effective_from
            .zip(first.effective_until)
            .is_none_or(|(from, until)| from < until)
}

impl PriceTable {
    /// Parse and validate a `tapes-price-table/1` or `/2` document. Unknown
    /// keys, negative or non-finite prices, and an id or alias named twice
    /// refuse, naming the entry; a version 2 table names one id in several
    /// rows only when their validity windows do not meet. A version 1 table
    /// refuses the version 2 terms.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawTable = toml::from_str(text).context("the price table is not valid")?;
        let schema = match raw.schema.as_str() {
            PRICE_TABLE_SCHEMA => PRICE_TABLE_SCHEMA,
            PRICE_TABLE_SCHEMA_V2 => PRICE_TABLE_SCHEMA_V2,
            other => {
                return Err(anyhow!(
                    "the price table's schema is `{other}`, not `{PRICE_TABLE_SCHEMA}` or `{PRICE_TABLE_SCHEMA_V2}`"
                ))
            }
        };
        if raw.currency.trim().is_empty() {
            return Err(anyhow!("the price table names no currency"));
        }
        let mut models = Vec::with_capacity(raw.models.len());
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (position, model) in raw.models.into_iter().enumerate() {
            let entry = format!("models[{position}] (id `{}`)", model.id);
            if model.id.is_empty() {
                return Err(anyhow!("{entry} has an empty id"));
            }
            let effective = match (model.effective.date, model.effective.time) {
                (Some(date), None) => {
                    NaiveDate::from_ymd_opt(date.year.into(), date.month.into(), date.day.into())
                        .ok_or_else(|| anyhow!("{entry} has an impossible effective date"))?
                }
                _ => {
                    return Err(anyhow!(
                        "{entry}: effective must be a date, such as 2026-10-01"
                    ))
                }
            };
            check_prices(&entry, &model.per_million, "price")?;
            let terms = model.effective_from.is_some()
                || model.effective_until.is_some()
                || model.long_context.is_some()
                || !model.time_of_day.is_empty();
            if terms && schema == PRICE_TABLE_SCHEMA {
                return Err(anyhow!(
                    "{entry}: effective_from, effective_until, long_context and time_of_day need schema `{PRICE_TABLE_SCHEMA_V2}`"
                ));
            }
            let effective_from = model
                .effective_from
                .as_ref()
                .map(|value| instant(&entry, "effective_from", value))
                .transpose()?;
            let effective_until = model
                .effective_until
                .as_ref()
                .map(|value| instant(&entry, "effective_until", value))
                .transpose()?;
            if effective_from
                .zip(effective_until)
                .is_some_and(|(from, until)| from >= until)
            {
                return Err(anyhow!(
                    "{entry}: effective_from must come before effective_until"
                ));
            }
            let long_context = model
                .long_context
                .map(|term| long_context(&entry, term))
                .transpose()?;
            let mut windows: Vec<TimeOfDayRates> = Vec::new();
            for window in model.time_of_day {
                let from = time_of_day(&entry, "from", &window.from)?;
                let until = time_of_day(&entry, "until", &window.until)?;
                if from == until {
                    return Err(anyhow!(
                        "{entry}: a time_of_day window must not start where it ends"
                    ));
                }
                check_prices(&entry, &window.per_million, "time_of_day price")?;
                let days = window
                    .days
                    .as_deref()
                    .map(|names| days_of_week(&entry, names))
                    .transpose()?;
                let window = TimeOfDayRates {
                    from,
                    until,
                    per_million: window.per_million,
                    days,
                };
                // Every minute of a week, from a Monday.
                let monday = DateTime::<Utc>::from_naive_utc_and_offset(
                    NaiveDate::from_isoywd_opt(2024, 1, Weekday::Mon)
                        .and_then(|day| day.and_hms_opt(0, 0, 0))
                        .expect("a Monday"),
                    Utc,
                );
                let overlapping = (0..7 * 24 * 60).any(|minute| {
                    let at = monday + chrono::Duration::minutes(minute);
                    window.holds(at) && windows.iter().any(|held| held.holds(at))
                });
                if overlapping {
                    return Err(anyhow!("{entry}: time_of_day windows overlap"));
                }
                windows.push(window);
            }
            let row = ModelPrice {
                id: model.id,
                aliases: model.aliases,
                effective,
                source: model.source,
                per_million: model.per_million,
                effective_from,
                effective_until,
                long_context,
                time_of_day: windows,
            };
            for name in std::iter::once(&row.id).chain(&row.aliases) {
                if name.is_empty() {
                    return Err(anyhow!("{entry} has an empty alias"));
                }
                let rows = index.entry(name.clone()).or_default();
                if let Some(earlier) = rows.iter().find(|earlier| {
                    schema == PRICE_TABLE_SCHEMA
                        || **earlier == position
                        || validity_overlaps(&models[**earlier], &row)
                }) {
                    return Err(anyhow!(
                        "{entry} names `{name}`, which models[{earlier}] already names{}",
                        if schema == PRICE_TABLE_SCHEMA {
                            ""
                        } else {
                            " for a validity window that meets this one"
                        }
                    ));
                }
                rows.push(position);
            }
            models.push(row);
        }
        Ok(Self {
            currency: raw.currency,
            models,
            schema,
            index,
        })
    }

    /// Read and validate the table at `path`, a regular file of bounded size.
    /// The open never waits on a FIFO's writer; anything but a regular file
    /// is refused before it is read.
    pub fn read(path: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("cannot open the price table {}", path.display()))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(anyhow!("the price table {} is not a file", path.display()));
        }
        if metadata.len() > MAX_TABLE_BYTES {
            return Err(anyhow!(
                "the price table {} is larger than {} bytes",
                path.display(),
                MAX_TABLE_BYTES
            ));
        }
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::Read::take(file, MAX_TABLE_BYTES), &mut text)
            .with_context(|| format!("cannot read the price table {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in the price table {}", path.display()))
    }

    /// The row for a model id as a harness records it, by id or alias: the
    /// first in the table where several rows name it for different times.
    pub fn price(&self, id: &str) -> Option<&ModelPrice> {
        self.rows(id).next()
    }

    /// Every row that names a model id, by id or alias.
    pub fn rows<'a>(&'a self, id: &str) -> impl Iterator<Item = &'a ModelPrice> + 'a {
        self.index
            .get(id)
            .into_iter()
            .flatten()
            .map(|position| &self.models[*position])
    }
}

/// Validate a long-context term.
fn long_context(entry: &str, term: RawLongContext) -> Result<LongContext> {
    let (prompt_tokens, inclusive) = match (term.above_prompt_tokens, term.from_prompt_tokens) {
        (Some(tokens), None) => (tokens, false),
        (None, Some(tokens)) => (tokens, true),
        _ => {
            return Err(anyhow!(
            "{entry}: long_context names exactly one of above_prompt_tokens and from_prompt_tokens"
        ))
        }
    };
    if term.prompt_tokens.is_empty() {
        return Err(anyhow!(
            "{entry}: long_context prompt_tokens names the categories a prompt counts"
        ));
    }
    let mut counts = term.prompt_tokens;
    let named = counts.len();
    counts.sort();
    counts.dedup();
    if counts.len() != named {
        return Err(anyhow!(
            "{entry}: long_context prompt_tokens names a category twice"
        ));
    }
    let billing = term.billing.unwrap_or_else(|| vec![BillingKind::Api]);
    if billing.is_empty() {
        return Err(anyhow!("{entry}: long_context billing names no billing"));
    }
    match (&term.per_million, &term.multipliers) {
        (Some(prices), None) => check_prices(entry, prices, "long_context price")?,
        (None, Some(factors)) => check_prices(entry, factors, "long_context multiplier")?,
        _ => {
            return Err(anyhow!(
                "{entry}: long_context names exactly one of per_million and multipliers"
            ))
        }
    }
    let mut billing = billing;
    billing.sort();
    billing.dedup();
    Ok(LongContext {
        prompt_tokens,
        inclusive,
        counts,
        billing,
        per_million: term.per_million,
        multipliers: term.multipliers,
    })
}

/// One request's disjoint tokens. Reasoning is a share of output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestTokens {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_write_unrecorded: u64,
    pub output: u64,
}

impl RequestTokens {
    /// The request's tokens from its disjoint categories, or none when one
    /// it prices is missing.
    pub fn of(disjoint: &DisjointTokens) -> Option<Self> {
        let writes = disjoint.cache_write?;
        Some(Self {
            uncached_input: disjoint.uncached_input?,
            cache_read: disjoint.cache_read?,
            cache_write_5m: writes.ephemeral_5m.unwrap_or(0),
            cache_write_1h: writes.ephemeral_1h.unwrap_or(0),
            cache_write_unrecorded: writes.ttl_unrecorded.unwrap_or(0),
            output: disjoint.output?,
        })
    }

    /// The figure's tokens, a category it lacks counted as none.
    fn present(disjoint: &DisjointTokens) -> Self {
        let writes = disjoint.cache_write.unwrap_or_default();
        Self {
            uncached_input: disjoint.uncached_input.unwrap_or(0),
            cache_read: disjoint.cache_read.unwrap_or(0),
            cache_write_5m: writes.ephemeral_5m.unwrap_or(0),
            cache_write_1h: writes.ephemeral_1h.unwrap_or(0),
            cache_write_unrecorded: writes.ttl_unrecorded.unwrap_or(0),
            output: disjoint.output.unwrap_or(0),
        }
    }

    fn cache_write_total(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h + self.cache_write_unrecorded
    }

    fn add(&mut self, other: &Self) {
        self.uncached_input += other.uncached_input;
        self.cache_read += other.cache_read;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.cache_write_unrecorded += other.cache_write_unrecorded;
        self.output += other.output;
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The figure's tokens less `requests`, or none when the requests hold
    /// more of a category than the figure, or the figure lacks one.
    fn remainder(disjoint: &DisjointTokens, requests: &Self) -> Option<Self> {
        let writes = disjoint.cache_write?;
        Some(Self {
            uncached_input: disjoint
                .uncached_input?
                .checked_sub(requests.uncached_input)?,
            cache_read: disjoint.cache_read?.checked_sub(requests.cache_read)?,
            cache_write_5m: writes
                .ephemeral_5m
                .unwrap_or(0)
                .checked_sub(requests.cache_write_5m)?,
            cache_write_1h: writes
                .ephemeral_1h
                .unwrap_or(0)
                .checked_sub(requests.cache_write_1h)?,
            cache_write_unrecorded: writes
                .ttl_unrecorded
                .unwrap_or(0)
                .checked_sub(requests.cache_write_unrecorded)?,
            output: disjoint.output?.checked_sub(requests.output)?,
        })
    }
}

/// One counted request as a reader kept it for pricing: when it was made,
/// the model in effect, and its tokens.
#[derive(Clone, Debug, PartialEq)]
struct RequestUse {
    at: Option<DateTime<Utc>>,
    /// The position of the model in effect among the read's models.
    model: Option<u16>,
    tokens: RequestTokens,
    counted: bool,
}

/// The most requests one read keeps for pricing. A read that counts more
/// keeps none, and its figure is priced as one whose requests are unknown.
pub const MAX_REQUEST_USES: usize = 100_000;

/// The requests a read counted, kept for terms priced by the request. A
/// request missing a priced category, or a read past the bound, leaves the
/// set unusable, and its figure is priced as one whose requests are
/// unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestUses {
    models: Vec<crate::model::Model>,
    requests: Vec<RequestUse>,
    unusable: bool,
}

impl RequestUses {
    /// Keep one counted request, returning its position.
    pub fn push(
        &mut self,
        at: Option<DateTime<Utc>>,
        model: Option<&crate::model::Model>,
        disjoint: &DisjointTokens,
    ) -> Option<usize> {
        if self.unusable {
            return None;
        }
        let Some(tokens) = RequestTokens::of(disjoint) else {
            self.discard();
            return None;
        };
        self.push_tokens(at, model, tokens)
    }

    fn push_tokens(
        &mut self,
        at: Option<DateTime<Utc>>,
        model: Option<&crate::model::Model>,
        tokens: RequestTokens,
    ) -> Option<usize> {
        if self.unusable {
            return None;
        }
        if self.requests.len() >= MAX_REQUEST_USES {
            self.discard();
            return None;
        }
        let model = match model {
            None => None,
            Some(model) => match self.models.iter().position(|held| held == model) {
                Some(position) => Some(position),
                None if self.models.len() < usize::from(u16::MAX) => {
                    self.models.push(model.clone());
                    Some(self.models.len() - 1)
                }
                None => {
                    self.discard();
                    return None;
                }
            },
        };
        self.requests.push(RequestUse {
            at,
            model: model.map(|position| position as u16),
            tokens,
            counted: true,
        });
        Some(self.requests.len() - 1)
    }

    /// Leave a kept request out of the figure, as one its total never
    /// counted.
    pub fn uncount(&mut self, position: usize) {
        if let Some(request) = self.requests.get_mut(position) {
            request.counted = false;
        }
    }

    /// Give up the requests: the figure is priced as one whose requests are
    /// unknown.
    pub fn discard(&mut self) {
        self.unusable = true;
        self.requests = Vec::new();
        self.models = Vec::new();
    }

    /// Add another read's requests.
    pub fn extend(&mut self, other: &Self) {
        if other.unusable {
            self.discard();
        }
        for request in other.requests.iter().filter(|request| request.counted) {
            let model = request
                .model
                .map(|position| &other.models[usize::from(position)]);
            self.push_tokens(request.at, model, request.tokens);
        }
    }

    pub fn len(&self) -> usize {
        self.requests
            .iter()
            .filter(|request| request.counted)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The counted requests of one model share, by id and, where the share
    /// names one, variant; `sole` takes too the requests no model was in
    /// effect for. None when the set is unusable.
    fn of_share<'a>(
        &'a self,
        id: &'a str,
        variant: Option<&'a str>,
        sole: bool,
    ) -> Option<impl Iterator<Item = &'a RequestUse> + 'a> {
        (!self.unusable).then(move || {
            self.requests.iter().filter(move |request| {
                request.counted
                    && match request.model {
                        None => sole,
                        Some(position) => {
                            let model = &self.models[usize::from(position)];
                            model.id == id
                                && (sole
                                    || variant.is_none_or(|variant| {
                                        model.variant.as_deref() == Some(variant)
                                    }))
                        }
                    }
            })
        })
    }
}

/// How the recording says its tokens were billed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BillingTerms {
    /// A subscription plan's quota, which no long-context surcharge reaches.
    Subscription,
    /// Tokens billed one by one, as an API key bills them.
    Api,
    /// The recording establishes neither.
    Unestablished,
}

/// A session's billing and the recorded fact that establishes it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Billing {
    pub terms: BillingTerms,
    pub evidence: String,
}

impl Billing {
    pub fn subscription(evidence: impl Into<String>) -> Self {
        Self {
            terms: BillingTerms::Subscription,
            evidence: evidence.into(),
        }
    }

    pub fn api(evidence: impl Into<String>) -> Self {
        Self {
            terms: BillingTerms::Api,
            evidence: evidence.into(),
        }
    }

    /// The billing of a recording that names none.
    pub fn unestablished() -> Self {
        Self {
            terms: BillingTerms::Unestablished,
            evidence: "the recording names no billing".to_owned(),
        }
    }

    /// The billing of a read that left part of its recording unread: what
    /// the read's own records establish, if anything, is named, but none is
    /// established, since a record the read skipped can contradict it.
    pub fn of_read_window(shown: Option<Self>) -> Self {
        let Some(shown) = shown else {
            return Self {
                terms: BillingTerms::Unestablished,
                evidence: "the read window names no billing, and records outside it went unread"
                    .to_owned(),
            };
        };
        let terms = match shown.terms {
            BillingTerms::Subscription => "a subscription",
            BillingTerms::Api => "per-token billing",
            BillingTerms::Unestablished => return shown,
        };
        Self {
            terms: BillingTerms::Unestablished,
            evidence: format!(
                "the read window shows {terms} ({}), but records outside it went unread",
                shown.evidence
            ),
        }
    }

    /// Whether a term for `billing` applies: known, or unknown when the
    /// recording establishes no billing and the term does not cover both.
    fn applies(&self, billing: &[BillingKind]) -> Option<bool> {
        match self.terms {
            BillingTerms::Subscription => Some(billing.contains(&BillingKind::Subscription)),
            BillingTerms::Api => Some(billing.contains(&BillingKind::Api)),
            BillingTerms::Unestablished => {
                let both = billing.contains(&BillingKind::Api)
                    && billing.contains(&BillingKind::Subscription);
                both.then_some(true)
            }
        }
    }
}

/// What a figure's terms priced by the request read: the requests its read
/// kept, the billing its recording establishes, and the times its activity
/// spans, for usage whose requests are unknown.
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestTerms<'a> {
    pub requests: Option<&'a RequestUses>,
    pub billing: Option<&'a Billing>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// Where a price table came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PriceOrigin {
    /// Named by the caller, as `--prices` names it.
    Named,
    /// Named by `TAPES_PRICES`.
    Environment,
    /// The default location under the user's configuration directory.
    Default,
}

/// A table the lookup found.
#[derive(Clone, Debug)]
pub struct LoadedPrices {
    pub table: PriceTable,
    pub path: PathBuf,
    pub origin: PriceOrigin,
}

/// What the lookup found: a table, none at all, or a default table that
/// could not be used, which leaves estimates absent.
#[derive(Debug)]
pub enum PriceLookup {
    Loaded(LoadedPrices),
    Absent,
    DefaultInvalid { path: PathBuf, error: String },
}

impl PriceLookup {
    pub fn table(&self) -> Option<&PriceTable> {
        match self {
            Self::Loaded(loaded) => Some(&loaded.table),
            _ => None,
        }
    }
}

/// The default table location: `$XDG_CONFIG_HOME/tapes/prices.toml`, or
/// `~/.config/tapes/prices.toml` when that variable is unset or empty.
pub fn default_table_path() -> Option<PathBuf> {
    let configured = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    configured
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .map(|directory| directory.join("tapes").join("prices.toml"))
}

/// Find the price table: `named` (the caller's `--prices`), then the file
/// `TAPES_PRICES` names, then the default location. A named or environment
/// table that is missing or invalid refuses; a default table that is invalid
/// is reported and leaves estimates absent; no table at all is absence.
pub fn lookup(named: Option<&Path>) -> Result<PriceLookup> {
    let environment = std::env::var_os(PRICES_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let explicit = named
        .map(|path| (path.to_path_buf(), PriceOrigin::Named))
        .or_else(|| environment.map(|path| (path, PriceOrigin::Environment)));
    if let Some((path, origin)) = explicit {
        let table = PriceTable::read(&path)?;
        return Ok(PriceLookup::Loaded(LoadedPrices {
            table,
            path,
            origin,
        }));
    }
    let Some(path) = default_table_path().filter(|path| path.exists()) else {
        return Ok(PriceLookup::Absent);
    };
    Ok(match PriceTable::read(&path) {
        Ok(table) => PriceLookup::Loaded(LoadedPrices {
            table,
            path,
            origin: PriceOrigin::Default,
        }),
        Err(error) => PriceLookup::DefaultInvalid {
            path,
            error: format!("{error:#}"),
        },
    })
}

/// A model an estimate priced, as its table row names it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct PricedModel {
    pub id: String,
    pub effective: NaiveDate,
    pub source: String,
}

/// A figure's list price under a caller's table. `amount` is the estimate
/// where every priced token has one price; a cache write with no recorded
/// TTL gives `at_least` and `at_most` instead. `models` names each table row
/// used. A category `measurement_completeness` names is a known subtotal, or
/// unpriced, for the reasons it gives.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ListPriceEstimate {
    pub kind: &'static str,
    pub currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_least: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_most: Option<f64>,
    pub schema: &'static str,
    pub models: Vec<PricedModel>,
    /// The billing each priced recording establishes, where a row it used
    /// carries a long-context term, which charges by billing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub billing: Vec<Billing>,
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
}

impl ListPriceEstimate {
    /// Whether every priced category is completely measured and priced.
    pub fn is_complete(&self) -> bool {
        self.measurement_completeness.is_empty()
    }
}

/// Estimates added together: shares of one figure, or the figures of a
/// summary.
#[derive(Clone, Debug, Default)]
pub struct EstimateSum {
    currency: Option<String>,
    schema: Option<&'static str>,
    low: f64,
    high: f64,
    priced: bool,
    models: BTreeSet<PricedModel>,
    billing: BTreeSet<Billing>,
    completeness: MeasurementCompleteness,
}

impl EstimateSum {
    /// Add another sum, priced or not: its reasons qualify this one either way.
    pub fn merge(&mut self, other: Self) {
        if self.currency.is_none() {
            self.currency = other.currency;
        }
        if self.schema.is_none() {
            self.schema = other.schema;
        }
        self.billing.extend(other.billing);
        self.low += other.low;
        self.high += other.high;
        self.priced |= other.priced;
        self.models.extend(other.models);
        self.completeness.merge(&other.completeness);
    }

    /// Qualify every priced category, for a reason no single share carries.
    pub fn qualify_all(&mut self, reason: MeasurementReason) {
        for category in PRICED_CATEGORIES {
            self.completeness.qualify(category, reason);
        }
    }

    pub fn qualify(&mut self, category: &str, reason: MeasurementReason) {
        self.completeness.qualify(category, reason);
    }

    /// The summed estimate, absent when nothing was priced.
    pub fn finish(self) -> Option<ListPriceEstimate> {
        if !self.priced {
            return None;
        }
        let bounded = (self.high - self.low).abs() > f64::EPSILON * self.high.abs().max(1.0);
        Some(ListPriceEstimate {
            kind: ESTIMATE_KIND,
            currency: self.currency.unwrap_or_default(),
            amount: (!bounded).then_some(self.low),
            at_least: bounded.then_some(self.low),
            at_most: bounded.then_some(self.high),
            schema: self.schema.unwrap_or(PRICE_TABLE_SCHEMA),
            models: self.models.into_iter().collect(),
            billing: self.billing.into_iter().collect(),
            measurement_completeness: self.completeness,
        })
    }
}

/// An empty sum under `table` carrying the reasons the figure's priced
/// categories carry.
fn carried(disjoint: &DisjointTokens, table: &PriceTable) -> EstimateSum {
    let mut sum = EstimateSum {
        currency: Some(table.currency.clone()),
        schema: Some(table.schema),
        ..EstimateSum::default()
    };
    for category in PRICED_CATEGORIES {
        for (counter, reasons) in disjoint.measurement_completeness.iter() {
            if counter == category {
                for reason in reasons {
                    sum.qualify(category, *reason);
                }
            }
        }
    }
    sum
}

/// Price one model share: by its row as [`price_share`] does when the row
/// prices every token of a category at one rate, and request by request
/// when its rows carry terms priced by the request.
fn price_model(
    disjoint: &DisjointTokens,
    id: &str,
    variant: Option<&str>,
    table: &PriceTable,
    terms: &RequestTerms<'_>,
    sole: bool,
) -> EstimateSum {
    let rows = table.rows(id).collect::<Vec<_>>();
    match rows.as_slice() {
        [] => price_share(disjoint, None, table),
        [row] if row.is_plain() => price_share(disjoint, Some(row), table),
        rows => price_with_terms(disjoint, id, variant, rows, table, terms, sole),
    }
}

/// Price a share whose rows carry terms priced by the request. The share's
/// requests, where its read kept them and they sum to no more than the
/// share, are priced one by one at the rates in force at each; what they
/// leave of the share is priced between every rate it could pay, its
/// categories qualified with the reason it could pay more than one.
fn price_with_terms(
    disjoint: &DisjointTokens,
    id: &str,
    variant: Option<&str>,
    rows: &[&ModelPrice],
    table: &PriceTable,
    terms: &RequestTerms<'_>,
    sole: bool,
) -> EstimateSum {
    let mut sum = carried(disjoint, table);
    for (category, present) in [
        ("uncached_input", disjoint.uncached_input.is_some()),
        ("cache_read", disjoint.cache_read.is_some()),
        ("cache_write", disjoint.cache_write.is_some()),
        ("output", disjoint.output.is_some()),
    ] {
        if !present && disjoint.measurement_completeness.is_complete(&[category]) {
            sum.qualify(category, MeasurementReason::MissingCounter);
        }
    }
    let billing = terms
        .billing
        .cloned()
        .unwrap_or_else(Billing::unestablished);
    let requests = terms
        .requests
        .and_then(|uses| uses.of_share(id, variant, sole))
        .map(Iterator::collect::<Vec<_>>);
    let mut held = RequestTokens::default();
    for request in requests.iter().flatten() {
        held.add(&request.tokens);
    }
    let (requests, remainder) = match requests
        .as_ref()
        .and_then(|_| RequestTokens::remainder(disjoint, &held))
    {
        Some(remainder) => (requests.unwrap_or_default(), remainder),
        None => (Vec::new(), RequestTokens::present(disjoint)),
    };
    for request in requests {
        price_tokens(
            &mut sum,
            &request.tokens,
            Some(request.at),
            rows,
            terms,
            &billing,
        );
    }
    if !remainder.is_empty() {
        price_tokens(&mut sum, &remainder, None, rows, terms, &billing);
    }
    sum
}

/// Price `tokens` at every rate they could pay among `rows`, adding the
/// lowest and the highest. `request` holds the time of a single request,
/// whose prompt size is known, or is absent for usage whose requests are
/// unknown; a known request with no time could have been made at any time
/// the figure spans.
fn price_tokens(
    sum: &mut EstimateSum,
    tokens: &RequestTokens,
    request: Option<Option<DateTime<Utc>>>,
    rows: &[&ModelPrice],
    terms: &RequestTerms<'_>,
    billing: &Billing,
) {
    let at = request.flatten();
    let in_force = rows
        .iter()
        .copied()
        .filter(|row| match at {
            Some(at) => row.covers(at),
            None => row.overlaps(terms.from, terms.to),
        })
        .collect::<Vec<_>>();
    let present = |sum: &mut EstimateSum, reason: MeasurementReason| {
        for (category, amount) in [
            ("uncached_input", tokens.uncached_input),
            ("cache_read", tokens.cache_read),
            ("cache_write", tokens.cache_write_total()),
            ("output", tokens.output),
        ] {
            if amount > 0 {
                sum.qualify(category, reason);
            }
        }
    };
    if in_force.is_empty() {
        present(sum, MeasurementReason::OutsideValidity);
        return;
    }
    if at.is_none() && !in_force_throughout(&in_force, terms.from, terms.to) {
        present(sum, MeasurementReason::OutsideValidity);
    }
    let mut reasons = BTreeSet::new();
    if at.is_none() && in_force.len() > 1 {
        reasons.insert(MeasurementReason::RequestTermsUnknown);
    }
    let mut candidates = Vec::new();
    for row in in_force {
        let bases = match at {
            Some(at) => vec![row.rates_at(at)],
            None => {
                if !row.time_of_day.is_empty() {
                    reasons.insert(MeasurementReason::RequestTermsUnknown);
                }
                std::iter::once(&row.per_million)
                    .chain(row.time_of_day.iter().map(|window| &window.per_million))
                    .collect()
            }
        };
        for base in bases {
            let Some(term) = &row.long_context else {
                candidates.push(base.clone());
                continue;
            };
            sum.billing.insert(billing.clone());
            let passed = request.map(|_| term.passed_by(term.prompt(tokens)));
            match (passed, billing.applies(&term.billing)) {
                (Some(false), _) | (_, Some(false)) => candidates.push(base.clone()),
                (Some(true), Some(true)) => candidates.push(term.rates(base)),
                (passed, applies) => {
                    candidates.push(base.clone());
                    candidates.push(term.rates(base));
                    if passed.is_none() {
                        reasons.insert(MeasurementReason::RequestTermsUnknown);
                    }
                    if applies.is_none() {
                        reasons.insert(MeasurementReason::BillingUnestablished);
                    }
                }
            }
        }
        sum.models.insert(PricedModel {
            id: row.id.clone(),
            effective: row.effective,
            source: row.source.clone(),
        });
    }
    let costs = candidates
        .iter()
        .map(|prices| cost(tokens, prices))
        .collect::<Vec<_>>();
    let low = costs
        .iter()
        .map(|cost| cost.low)
        .fold(f64::INFINITY, f64::min);
    let high = costs.iter().map(|cost| cost.high).fold(0.0, f64::max);
    sum.low += low;
    sum.high += high;
    sum.priced |= costs.iter().any(|cost| cost.priced);
    for cost in &costs {
        for category in &cost.missing {
            sum.qualify(category, MeasurementReason::MissingPrice);
        }
        if cost.ttl_bounded {
            sum.qualify("cache_write", MeasurementReason::TtlUnrecorded);
        }
    }
    let same = |left: f64, right: f64| {
        (left - right).abs() <= f64::EPSILON * left.abs().max(right.abs()).max(1.0)
    };
    let differ = costs.iter().any(|cost| {
        !same(cost.low, costs[0].low)
            || !same(cost.high, costs[0].high)
            || cost.missing != costs[0].missing
    });
    if differ {
        for reason in reasons {
            present(sum, reason);
        }
    }
}

/// Whether `rows` hold, between them, a row in effect at every instant of
/// `from..=to`, either end open.
fn in_force_throughout(
    rows: &[&ModelPrice],
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> bool {
    let mut rows = rows.to_vec();
    rows.sort_by_key(|row| row.effective_from);
    // Every instant before `reached` is held; `None` is the open past.
    let mut reached = from;
    for row in rows {
        let gap = row
            .effective_from
            .is_some_and(|start| reached.is_none_or(|reached| start > reached));
        if gap {
            return false;
        }
        let Some(until) = row.effective_until else {
            return true;
        };
        if to.is_some_and(|to| until > to) {
            return true;
        }
        reached = Some(reached.map_or(until, |reached| reached.max(until)));
    }
    false
}

/// What tokens cost under one set of rates: an amount, or bounds where cache
/// writes recorded no TTL, the categories the rates leave unpriced, and
/// whether they price any of the tokens, as rates do for tokens of none.
struct Cost {
    low: f64,
    high: f64,
    missing: Vec<&'static str>,
    ttl_bounded: bool,
    priced: bool,
}

fn cost(tokens: &RequestTokens, prices: &CategoryPrices) -> Cost {
    let mut cost = Cost {
        low: 0.0,
        high: 0.0,
        missing: Vec::new(),
        ttl_bounded: false,
        priced: tokens.is_empty(),
    };
    for (category, amount, price) in [
        (
            "uncached_input",
            tokens.uncached_input,
            prices.uncached_input,
        ),
        ("cache_read", tokens.cache_read, prices.cache_read),
        ("cache_write", tokens.cache_write_5m, prices.cache_write_5m),
        ("cache_write", tokens.cache_write_1h, prices.cache_write_1h),
        ("output", tokens.output, prices.output),
    ] {
        if amount == 0 {
            continue;
        }
        match price {
            Some(price) => {
                let amount = amount as f64 * price / 1_000_000.0;
                cost.low += amount;
                cost.high += amount;
                cost.priced = true;
            }
            None if !cost.missing.contains(&category) => cost.missing.push(category),
            None => {}
        }
    }
    if tokens.cache_write_unrecorded > 0 {
        match (prices.cache_write_5m, prices.cache_write_1h) {
            (Some(short), Some(long)) => {
                let amount = tokens.cache_write_unrecorded as f64 / 1_000_000.0;
                cost.low += amount * short.min(long);
                cost.high += amount * short.max(long);
                cost.ttl_bounded = true;
                cost.priced = true;
            }
            _ if !cost.missing.contains(&"cache_write") => cost.missing.push("cache_write"),
            _ => {}
        }
    }
    cost
}

/// Price one model's disjoint categories by `row`, or qualify them all
/// `unlisted-model` where the table has no row. A category the share
/// measured incompletely keeps its reasons.
pub fn price_share(
    disjoint: &DisjointTokens,
    row: Option<&ModelPrice>,
    table: &PriceTable,
) -> EstimateSum {
    let mut sum = carried(disjoint, table);
    let Some(row) = row else {
        sum.qualify_all(MeasurementReason::UnlistedModel);
        return sum;
    };
    let prices = &row.per_million;
    let add =
        |sum: &mut EstimateSum, category: &str, tokens: Option<u64>, price: Option<f64>| match (
            tokens, price,
        ) {
            (None, _) => {
                if disjoint.measurement_completeness.is_complete(&[category]) {
                    sum.qualify(category, MeasurementReason::MissingCounter);
                }
            }
            (Some(0), _) => sum.priced = true,
            (Some(tokens), Some(price)) => {
                let amount = tokens as f64 * price / 1_000_000.0;
                sum.low += amount;
                sum.high += amount;
                sum.priced = true;
            }
            (Some(_), None) => sum.qualify(category, MeasurementReason::MissingPrice),
        };
    add(
        &mut sum,
        "uncached_input",
        disjoint.uncached_input,
        prices.uncached_input,
    );
    add(
        &mut sum,
        "cache_read",
        disjoint.cache_read,
        prices.cache_read,
    );
    add(&mut sum, "output", disjoint.output, prices.output);
    match disjoint.cache_write {
        None => add(&mut sum, "cache_write", None, None),
        Some(CacheWrites {
            ephemeral_5m,
            ephemeral_1h,
            ttl_unrecorded,
        }) => {
            for (tokens, price) in [
                (ephemeral_5m, prices.cache_write_5m),
                (ephemeral_1h, prices.cache_write_1h),
            ] {
                if tokens.is_some() {
                    add(&mut sum, "cache_write", tokens, price);
                }
            }
            match ttl_unrecorded {
                None | Some(0) => {}
                Some(tokens) => match (prices.cache_write_5m, prices.cache_write_1h) {
                    (Some(short), Some(long)) => {
                        let tokens = tokens as f64 / 1_000_000.0;
                        sum.low += tokens * short.min(long);
                        sum.high += tokens * short.max(long);
                        sum.priced = true;
                        sum.qualify("cache_write", MeasurementReason::TtlUnrecorded);
                    }
                    _ => sum.qualify("cache_write", MeasurementReason::MissingPrice),
                },
            }
        }
    }
    sum.models.insert(PricedModel {
        id: row.id.clone(),
        effective: row.effective,
        source: row.source.clone(),
    });
    sum
}

/// What an estimate of one figure reads: its disjoint categories, the
/// models its harness observed serving it, and its per-model split where
/// the harness recorded one.
pub struct PricedFigure<'a> {
    pub disjoint: Option<&'a DisjointTokens>,
    pub convention: Option<TokenConvention>,
    pub model: Option<&'a Model>,
    pub selections: &'a [ModelSelectionSpan],
    pub observation: Option<&'a ModelObservationStatus>,
    pub by_model: Option<&'a [ModelUsage]>,
    /// What terms priced by the request read.
    pub terms: RequestTerms<'a>,
}

/// The estimate of one figure and of each model share it splits into.
/// `whole` is absent when no model is known to have served the figure.
pub struct FigureEstimate {
    pub whole: Option<EstimateSum>,
    pub by_model: Vec<Option<ListPriceEstimate>>,
}

impl PricedFigure<'_> {
    /// The one model id the figure's model evidence names, if it names
    /// exactly one.
    fn sole_model(&self) -> Option<&str> {
        let ids = if self.selections.is_empty() {
            if self.observation.is_some_and(|status| status.mixed) {
                return None;
            }
            self.model
                .map(|model| model.id.as_str())
                .into_iter()
                .collect::<BTreeSet<_>>()
        } else {
            self.selections
                .iter()
                .map(|span| span.model.id.as_str())
                .collect::<BTreeSet<_>>()
        };
        (ids.len() == 1).then(|| *ids.iter().next().unwrap())
    }

    /// Price the figure under `table`. A figure whose evidence names one
    /// model is that model's share whole, and a split the harness recorded
    /// for the same model prices each part alike. A figure that names more
    /// than one model, or whose uncertain attribution leaves part of it out
    /// of its model shares, is priced share by share from its split, and
    /// what the shares leave of the whole is `incomplete-attribution`. A
    /// figure with neither has no estimate.
    pub fn estimate(&self, table: &PriceTable) -> FigureEstimate {
        let convention = self
            .convention
            .or_else(|| self.disjoint.map(|disjoint| disjoint.convention));
        let shares = self.by_model.unwrap_or_default();
        let share_disjoint = |model: &ModelUsage| {
            let convention = convention?;
            let tokens = model.tokens.as_ref()?;
            Some(
                DisjointTokens::from_native(tokens, convention, model.cache_write_by_ttl.as_ref())
                    .with_completeness(&model.measurement_completeness),
            )
        };
        let share_sums = shares
            .iter()
            .map(|model| {
                share_disjoint(model).map(|disjoint| {
                    price_model(
                        &disjoint,
                        &model.model,
                        model.variant.as_deref(),
                        table,
                        &self.terms,
                        false,
                    )
                })
            })
            .collect::<Vec<_>>();
        let by_model = share_sums
            .iter()
            .map(|sum| sum.clone().and_then(EstimateSum::finish))
            .collect::<Vec<_>>();
        let Some(disjoint) = self.disjoint else {
            return FigureEstimate {
                whole: None,
                by_model,
            };
        };
        let parts = shares.iter().map(share_disjoint).collect::<Vec<_>>();
        // Under uncertain attribution, shares that leave part of the figure
        // unattributed say the one model in evidence did not serve all of it.
        let unattributed = self
            .observation
            .is_some_and(|status| status.attribution_uncertain)
            && !shares.is_empty()
            && PRICED_CATEGORIES.iter().any(|category| {
                category_amount(disjoint, category).is_some()
                    && !shares_cover(disjoint, &parts, category)
            });
        let sole = self
            .sole_model()
            .filter(|sole| shares.iter().all(|model| model.model == *sole))
            .filter(|_| !unattributed);
        let whole = if let Some(sole) = sole {
            let mut sum = price_model(disjoint, sole, None, table, &self.terms, true);
            if self
                .observation
                .is_some_and(|status| status.attribution_uncertain)
            {
                sum.qualify_all(MeasurementReason::IncompleteAttribution);
            }
            Some(sum)
        } else if !shares.is_empty() {
            let mut sum = EstimateSum {
                currency: Some(table.currency.clone()),
                schema: Some(table.schema),
                ..EstimateSum::default()
            };
            for share in share_sums.into_iter().flatten() {
                sum.merge(share);
            }
            for category in PRICED_CATEGORIES {
                if !shares_cover(disjoint, &parts, category) {
                    sum.qualify(category, MeasurementReason::IncompleteAttribution);
                }
            }
            Some(sum)
        } else {
            None
        };
        FigureEstimate { whole, by_model }
    }
}

/// A figure's amount of one priced category.
fn category_amount(disjoint: &DisjointTokens, category: &str) -> Option<u64> {
    match category {
        "uncached_input" => disjoint.uncached_input,
        "cache_read" => disjoint.cache_read,
        "cache_write" => disjoint.cache_write.map(|writes| writes.total()),
        _ => disjoint.output,
    }
}

/// Whether the model shares account for all of the figure's `category`.
fn shares_cover(whole: &DisjointTokens, parts: &[Option<DisjointTokens>], category: &str) -> bool {
    let amount = |disjoint: &DisjointTokens| category_amount(disjoint, category);
    let Some(total) = amount(whole) else {
        return false;
    };
    parts
        .iter()
        .try_fold(0u64, |sum, part| {
            Some(sum.saturating_add(amount(part.as_ref()?)?))
        })
        .is_some_and(|covered| covered >= total)
}

/// Price a usage answer under `table`: the figure, each model share it
/// splits into, and a retained-branch consumption beside it.
pub fn price_view(view: &mut crate::usage::UsageView, table: &PriceTable) {
    let terms = RequestTerms {
        requests: view.request_uses.as_ref(),
        billing: view.billing.as_ref(),
        from: view.session.started_at,
        to: view.session.last_activity_at,
    };
    let estimate = PricedFigure {
        disjoint: view.disjoint.as_ref(),
        convention: None,
        model: view.session.model.as_ref(),
        selections: &view.session.model_selections,
        observation: view.session.model_observation.as_ref(),
        by_model: view.by_model.as_deref(),
        terms,
    }
    .estimate(table);
    view.list_price_estimate = estimate.whole.and_then(EstimateSum::finish);
    for (model, priced) in view.by_model.iter_mut().flatten().zip(estimate.by_model) {
        model.list_price_estimate = priced;
    }
    let convention = view.disjoint.as_ref().map(|disjoint| disjoint.convention);
    if let (Some(consumption), Some(convention)) = (view.consumption.as_mut(), convention) {
        let disjoint = consumption.tokens.as_ref().map(|tokens| {
            DisjointTokens::from_native(tokens, convention, None)
                .with_completeness(&consumption.measurement_completeness)
        });
        // Consumption on the active branch alone is priced as the session
        // figure is: by the session's model evidence, split by the model each
        // event names where every event names one. The session's evidence is
        // the active branch's, so events on other branches are priced by the
        // model each names, share by share, and what no model names is left
        // out as `incomplete-attribution`.
        let other_branches = consumption.events.other_branches > 0;
        // Consumption holds its own events, those on other branches too, so
        // it reads its own requests rather than the active branch's.
        let consumption_terms = RequestTerms {
            requests: consumption.request_uses.as_ref(),
            ..terms
        };
        let mut served = consumption.served.clone();
        for share in &mut served {
            share
                .measurement_completeness
                .merge(&consumption.measurement_completeness);
        }
        let attributed = served
            .iter()
            .filter_map(|share| share.request_count)
            .sum::<usize>()
            == consumption.events.counted;
        let session = (!other_branches).then_some(&view.session);
        let estimate = PricedFigure {
            disjoint: disjoint.as_ref(),
            convention: Some(convention),
            model: session.and_then(|session| session.model.as_ref()),
            selections: session.map_or(&[], |session| &session.model_selections),
            observation: session.and_then(|session| session.model_observation.as_ref()),
            by_model: (other_branches || attributed).then_some(&served),
            terms: consumption_terms,
        }
        .estimate(table);
        consumption.list_price_estimate = estimate.whole.and_then(EstimateSum::finish);
    }
}

/// Price every figure of a span answer under `table`.
pub fn price_spans(usage: &mut crate::usage::SpanUsage, table: &PriceTable) {
    for view in usage
        .spans
        .iter_mut()
        .chain([&mut usage.remainder, &mut usage.whole])
    {
        price_view(view, table);
    }
}
