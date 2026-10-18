//! List-price estimates from a price table the caller supplies. tapes holds
//! no prices of its own: a `tapes-price-table/1` file names, per model, what
//! a million tokens of each disjoint category cost, and an estimate prices a
//! figure's disjoint categories by the row of the model that served them.
//! An estimate sits beside any cost a harness wrote and never replaces it.
//! What it cannot price — a model the table lacks, usage no model is known
//! to have served, a counter the figure measured incompletely — leaves it a
//! known subtotal with a reason; a cache write with no recorded TTL bounds it
//! between the 5-minute and 1-hour prices.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::measurement::{MeasurementCompleteness, MeasurementReason};
use crate::model::{
    CacheWrites, DisjointTokens, Model, ModelObservationStatus, ModelSelectionSpan, TokenConvention,
};
use crate::usage::ModelUsage;

pub const PRICE_TABLE_SCHEMA: &str = "tapes-price-table/1";
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
/// date its prices were read and where.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelPrice {
    pub id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub effective: NaiveDate,
    pub source: String,
    pub per_million: CategoryPrices,
}

/// A validated price table.
#[derive(Clone, Debug, PartialEq)]
pub struct PriceTable {
    pub currency: String,
    pub models: Vec<ModelPrice>,
    index: HashMap<String, usize>,
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
}

impl PriceTable {
    /// Parse and validate a `tapes-price-table/1` document. Unknown keys,
    /// negative or non-finite prices, and an id or alias named twice refuse,
    /// naming the entry.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawTable = toml::from_str(text).context("the price table is not valid")?;
        if raw.schema != PRICE_TABLE_SCHEMA {
            return Err(anyhow!(
                "the price table's schema is `{}`, not `{PRICE_TABLE_SCHEMA}`",
                raw.schema
            ));
        }
        if raw.currency.trim().is_empty() {
            return Err(anyhow!("the price table names no currency"));
        }
        let mut models = Vec::with_capacity(raw.models.len());
        let mut index = HashMap::new();
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
            for (category, price) in [
                ("uncached_input", model.per_million.uncached_input),
                ("cache_read", model.per_million.cache_read),
                ("cache_write_5m", model.per_million.cache_write_5m),
                ("cache_write_1h", model.per_million.cache_write_1h),
                ("output", model.per_million.output),
            ] {
                if price.is_some_and(|price| !price.is_finite() || price < 0.0) {
                    return Err(anyhow!(
                        "{entry}: the {category} price must be a finite number of at least zero"
                    ));
                }
            }
            for name in std::iter::once(&model.id).chain(&model.aliases) {
                if name.is_empty() {
                    return Err(anyhow!("{entry} has an empty alias"));
                }
                if let Some(earlier) = index.insert(name.clone(), position) {
                    return Err(anyhow!(
                        "{entry} names `{name}`, which models[{earlier}] already names"
                    ));
                }
            }
            models.push(ModelPrice {
                id: model.id,
                aliases: model.aliases,
                effective,
                source: model.source,
                per_million: model.per_million,
            });
        }
        Ok(Self {
            currency: raw.currency,
            models,
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

    /// The row for a model id as a harness records it, by id or alias.
    pub fn price(&self, id: &str) -> Option<&ModelPrice> {
        self.index.get(id).map(|position| &self.models[*position])
    }
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
    low: f64,
    high: f64,
    priced: bool,
    models: BTreeSet<PricedModel>,
    completeness: MeasurementCompleteness,
}

impl EstimateSum {
    /// Add another sum, priced or not: its reasons qualify this one either way.
    pub fn merge(&mut self, other: Self) {
        if self.currency.is_none() {
            self.currency = other.currency;
        }
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
            schema: PRICE_TABLE_SCHEMA,
            models: self.models.into_iter().collect(),
            measurement_completeness: self.completeness,
        })
    }
}

/// Price one model's disjoint categories by `row`, or qualify them all
/// `unlisted-model` where the table has no row. A category the share
/// measured incompletely keeps its reasons.
pub fn price_share(
    disjoint: &DisjointTokens,
    row: Option<&ModelPrice>,
    table: &PriceTable,
) -> EstimateSum {
    let mut sum = EstimateSum {
        currency: Some(table.currency.clone()),
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
    /// than one model is priced share by share from its split, and what the
    /// shares leave of the whole is `incomplete-attribution`. A figure with
    /// neither has no estimate.
    pub fn estimate(&self, table: &PriceTable) -> FigureEstimate {
        let convention = self
            .convention
            .or_else(|| self.disjoint.map(|disjoint| disjoint.convention));
        let shares = self.by_model.unwrap_or_default();
        let share_disjoint = |model: &ModelUsage| {
            let convention = convention?;
            let tokens = model.tokens.as_ref()?;
            Some(
                DisjointTokens::from_native(tokens, convention, None)
                    .with_completeness(&model.measurement_completeness),
            )
        };
        let share_sums = shares
            .iter()
            .map(|model| {
                share_disjoint(model)
                    .map(|disjoint| price_share(&disjoint, table.price(&model.model), table))
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
        let sole = self
            .sole_model()
            .filter(|sole| shares.iter().all(|model| model.model == *sole));
        let whole = if let Some(sole) = sole {
            let mut sum = price_share(disjoint, table.price(sole), table);
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
                ..EstimateSum::default()
            };
            for share in share_sums.into_iter().flatten() {
                sum.merge(share);
            }
            let parts = shares.iter().map(share_disjoint).collect::<Vec<_>>();
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

/// Whether the model shares account for all of the figure's `category`.
fn shares_cover(whole: &DisjointTokens, parts: &[Option<DisjointTokens>], category: &str) -> bool {
    let amount = |disjoint: &DisjointTokens| match category {
        "uncached_input" => disjoint.uncached_input,
        "cache_read" => disjoint.cache_read,
        "cache_write" => disjoint.cache_write.map(|writes| writes.total()),
        _ => disjoint.output,
    };
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
    let estimate = PricedFigure {
        disjoint: view.disjoint.as_ref(),
        convention: None,
        model: view.session.model.as_ref(),
        selections: &view.session.model_selections,
        observation: view.session.model_observation.as_ref(),
        by_model: view.by_model.as_deref(),
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
        let estimate = PricedFigure {
            disjoint: disjoint.as_ref(),
            convention: Some(convention),
            model: view.session.model.as_ref(),
            selections: &view.session.model_selections,
            observation: view.session.model_observation.as_ref(),
            by_model: None,
        }
        .estimate(table);
        // The model evidence is the active branch's, so events on other
        // branches are attributed to no model.
        consumption.list_price_estimate = estimate.whole.and_then(|mut sum| {
            if consumption.events.other_branches > 0 {
                sum.qualify_all(MeasurementReason::IncompleteAttribution);
            }
            sum.finish()
        });
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
