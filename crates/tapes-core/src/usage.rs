//! The single-session usage view: where a session's quota went.
//!
//! `Session` carries the normalized counters and the `accounting` record
//! stating their basis and coverage. What a harness records beside them — a
//! context window, a provider quota window, wall-clock durations, a per-model
//! split — has no normalized field, so it rides on the session as
//! `usage_detail` and reaches a consumer through this projection.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{skipped_records_note, StreamedTranscript};
use crate::content::ContentInventory;
use crate::measurement::{Measured, MeasurementCompleteness, MeasurementReason};
use crate::model::{
    Accounting, AccountingBasis, AccountingCoverage, CacheWrites, Cost, CostCoverage,
    DisjointTokens, Launch, Model, ModelObservationStatus, ReadEvidence, ReadGap, RecordRef, Role,
    Session, SessionMetadata, SourceBound, SourceDescriptor, TerminalObservation, TextTailEvidence,
    TokenConvention, Tokens, Transcript, Truncation, Turn,
};

pub const USAGE_SCHEMA: &str = "tapes-usage/11";

/// Recorded usage facts and measurement notes kept beside the session's
/// normalized counters. A source fact that was not recorded remains absent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageDetail {
    /// Tokens the session's model can hold at once, as the harness reports it.
    pub context_window: Option<u64>,
    /// The provider quota the harness last observed. Quota is a separate fact
    /// from the session's own accounting: it covers the account, not this
    /// recording.
    pub rate_limits: Option<RateLimits>,
    /// Wall-clock durations the harness accumulated for the session.
    pub durations_ms: Option<Durations>,
    /// The session's counters split by the model that spent them.
    pub by_model: Option<Vec<ModelUsage>>,
    /// Request-observation attribution kept separate from the session's
    /// cumulative recorded total.
    pub attribution: Option<UsageAttribution>,
    /// The session counters' cache writes split by TTL, where the counters
    /// are a sum over requests that each recorded the split.
    pub cache_write_by_ttl: Option<CacheWrites>,
    /// Price-relevant facts the read's requests recorded beside their tokens.
    pub requests: Option<RequestDimensions>,
    pub cost_coverage: Option<CostCoverage>,
    /// Per-counter measurement coverage of the session's selected events.
    pub measurement_coverage: BTreeMap<String, MeasurementCoverage>,
    pub measurement_incomplete: Vec<String>,
    /// Reasons the reader recorded that the session's counters do not show,
    /// such as a stopped request. `Session::measurement_completeness` adds
    /// what the counters, their coverage and their accounting establish.
    pub measurement_completeness: MeasurementCompleteness,
    /// Failed request observations, whose consumption is unknown.
    pub request_failures: usize,
    /// Records the reader has no reading for that carry usage-shaped fields
    /// for their harness. Their consumption is in no counter, so every
    /// counter of the figure is qualified; an unrecognized record without
    /// usage-shaped fields is not counted.
    pub unrecognized_usage: usize,
    /// Consumption over every branch a recording retains, where the
    /// session's counters select only one of them.
    pub consumption: Option<Consumption>,
    pub recorded_total: Option<RecordedUsage>,
    pub accounting_notes: Vec<String>,
}

impl UsageDetail {
    pub fn is_empty(&self) -> bool {
        self.context_window.is_none()
            && self.rate_limits.is_none()
            && self.durations_ms.is_none()
            && self.by_model.is_none()
            && self.attribution.is_none()
            && self.cache_write_by_ttl.is_none()
            && self.requests.is_none()
            && self.cost_coverage.is_none()
            && self.measurement_coverage.is_empty()
            && self.measurement_incomplete.is_empty()
            && self.measurement_completeness.is_empty()
            && self.request_failures == 0
            && self.unrecognized_usage == 0
            && self.consumption.is_none()
            && self.recorded_total.is_none()
            && self.accounting_notes.is_empty()
    }

    /// The detail, or nothing when the harness recorded none of it.
    pub fn into_option(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RateLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<RateWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<RateWindow>,
    /// The account's plan, as the harness names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Credits are an account observation. The native balance may be a
    /// number, string, or boolean, so its JSON representation is retained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_control_reached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_reached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_reached_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

impl RateLimits {
    pub fn is_empty(&self) -> bool {
        self.primary.is_none()
            && self.secondary.is_none()
            && self.plan.is_none()
            && self.credits.is_none()
            && self.spend_control_reached.is_none()
            && self.rate_limit_reached.is_none()
            && self.rate_limit_reached_type.is_none()
            && self.observed_at.is_none()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Credits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_credits: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unlimited: Option<bool>,
}

impl Credits {
    pub fn is_empty(&self) -> bool {
        self.balance.is_none() && self.has_credits.is_none() && self.unlimited.is_none()
    }
}

/// One quota window: how much of it is spent, how long it is, and when it
/// starts over.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RateWindow {
    /// Preserve the native number/string representation instead of coercing
    /// a recorded value into a different type.
    pub used_percent: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Durations {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_without_retries: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

impl Durations {
    pub fn is_empty(&self) -> bool {
        self.api.is_none()
            && self.api_without_retries.is_none()
            && self.tool.is_none()
            && self.total.is_none()
    }
}

/// What one model spent, as the harness attributed it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelUsage {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_count: Option<usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub measurement_coverage: BTreeMap<String, MeasurementCoverage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measurement_incomplete: Vec<String>,
    /// The model's incompletely measured counters. A reader sets the reasons
    /// its counters do not show; the usage view adds the rest.
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
}

impl ModelUsage {
    /// Add what the model's own facts establish, over `scope`, to the
    /// reasons its reader recorded, and `shared`, the reasons no single
    /// model carries.
    fn qualify(&mut self, scope: Option<AccountingCoverage>, shared: &MeasurementCompleteness) {
        let assessed = MeasurementCompleteness::assess(Measured {
            tokens: self.tokens.as_ref(),
            cost: self.cost.as_ref(),
            cost_coverage: self.cost_coverage.as_ref(),
            coverage: Some(&self.measurement_coverage),
            scope,
            events: self.request_count.unwrap_or(0),
        });
        self.measurement_completeness.merge(&assessed);
        self.measurement_completeness.merge(shared);
    }
}

/// A cumulative cost-state preserved as a separate source fact. Its dollars
/// and counters follow its own accounting and never join a request sum.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordedUsage {
    pub accounting: Accounting,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_model: Option<Vec<ModelUsage>>,
}

/// The most distinct values one request dimension tallies, and the longest
/// value it keeps. A value past either bound is counted in `omitted_values`.
const MAX_DIMENSION_VALUES: usize = 16;
const MAX_DIMENSION_VALUE_BYTES: usize = 64;

/// Price-relevant facts the requests a read identified recorded beside their
/// token counters, tallied once per request. A value tally counts requests by
/// the value each recorded. A request that recorded no value for a dimension
/// is counted under `missing` for that dimension and is never assigned one;
/// `cache_write_ttl` counts requests whose cache writes carry no usable TTL
/// split. `conflicting` counts requests with incompatible observations of a
/// dimension; their ambiguous values are withheld rather than summed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RequestDimensions {
    pub requests: usize,
    pub cache_write: CacheWrites,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub service_tier: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub speed: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub inference_geo: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search_requests: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_fetch_requests: Option<u64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: BTreeMap<&'static str, usize>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub conflicting: BTreeMap<&'static str, usize>,
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_values: usize,
}

impl RequestDimensions {
    pub(crate) fn add(&mut self, other: Self) {
        self.requests += other.requests;
        self.cache_write.add(&other.cache_write);
        for (values, incoming) in [
            (&mut self.service_tier, other.service_tier),
            (&mut self.speed, other.speed),
            (&mut self.inference_geo, other.inference_geo),
        ] {
            for (value, count) in incoming {
                if !values.contains_key(&value) && values.len() >= MAX_DIMENSION_VALUES {
                    self.omitted_values += count;
                } else {
                    *values.entry(value).or_default() += count;
                }
            }
        }
        for (total, value) in [
            (&mut self.web_search_requests, other.web_search_requests),
            (&mut self.web_fetch_requests, other.web_fetch_requests),
        ] {
            if let Some(value) = value {
                *total = Some(total.unwrap_or(0).saturating_add(value));
            }
        }
        for (name, count) in other.missing {
            *self.missing.entry(name).or_default() += count;
        }
        for (name, count) in other.conflicting {
            *self.conflicting.entry(name).or_default() += count;
        }
        self.omitted_values += other.omitted_values;
    }

    /// Count one request's recorded value for `dimension`, or its absence.
    pub fn tally(&mut self, dimension: &'static str, value: Option<&str>) {
        let Some(value) = value else {
            *self.missing.entry(dimension).or_default() += 1;
            return;
        };
        let values = match dimension {
            "service_tier" => &mut self.service_tier,
            "speed" => &mut self.speed,
            "inference_geo" => &mut self.inference_geo,
            _ => return,
        };
        if value.len() > MAX_DIMENSION_VALUE_BYTES
            || (!values.contains_key(value) && values.len() >= MAX_DIMENSION_VALUES)
        {
            self.omitted_values += 1;
            return;
        }
        *values.entry(value.to_owned()).or_default() += 1;
    }

    /// Add one request's count of a server-side tool use, or note its absence.
    pub fn count(&mut self, dimension: &'static str, value: Option<u64>) {
        let slot = match dimension {
            "web_search_requests" => &mut self.web_search_requests,
            "web_fetch_requests" => &mut self.web_fetch_requests,
            _ => return,
        };
        match value {
            Some(value) => *slot = Some(slot.unwrap_or(0).saturating_add(value)),
            None => *self.missing.entry(dimension).or_default() += 1,
        }
    }
}

/// The basis used for observed request accounting in one read. This is not a
/// claim about the harness's whole history: a bounded read can only choose
/// among the records it actually reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationBasis {
    UsageRecord,
    TokenEventAdvance,
}

/// What an observation row represents. Rows are source observations; only
/// rows with `counted: true` contribute to the selected attribution basis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationClassification {
    Request,
    Repeat,
    Advance,
    LeadingUncounted,
    UnchangedTotal,
    ResetAdvance,
    QuotaOnly,
    Unattributed,
    Incomplete,
}

/// The model/effort request observations that a read could attribute, plus
/// the bounded evidence that prevented false precision.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageAttribution {
    pub basis: ObservationBasis,
    pub coverage: AccountingCoverage,
    pub observed: usize,
    pub counted: usize,
    pub attributed: usize,
    pub unattributed: usize,
    pub leading_uncounted: usize,
    pub resets: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unattributed_tokens: Option<Tokens>,
    /// Per-counter coverage among the unattributed requests, so their sum is
    /// a known subtotal wherever a request did not record a counter.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub unattributed_coverage: BTreeMap<String, MeasurementCoverage>,
    /// The unattributed counters that are incompletely measured and why; the
    /// usage view fills it from their coverage and the attribution's scope.
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub unattributed_completeness: MeasurementCompleteness,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bounds: Vec<String>,
}

/// What a recording consumed across every branch it retains. A harness that
/// rewinds by appending a new branch keeps the abandoned one, and its
/// requests were consumed whichever branch the conversation resumed on. The
/// session's own counters stay the active branch's; this figure counts each
/// consumption event the read reached once, keyed by its identity.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Consumption {
    /// `summed-requests` over `retained-branches`. Coverage is `session` when
    /// the read began at the recording's first byte and `read-window` when it
    /// kept a tail.
    pub accounting: Accounting,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    /// Known cost subtotal, present only when at least one event is priced.
    /// Its measurement coverage qualifies missing and unpriced events;
    /// a numeric zero beside nonzero usage is unpriced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Pricing completeness among counted events. Assistant-message events
    /// establish request counts; every zero-written event is an observation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    /// Per-counter coverage among the counted events. Missing dimensions
    /// leave known subtotals, never complete measurements.
    pub measurement_coverage: BTreeMap<String, MeasurementCoverage>,
    /// The incompletely measured counters and why: their coverage, the read's
    /// reach, unreadable records, and stopped requests.
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
    pub events: ConsumptionEvents,
    pub identity: ConsumptionIdentity,
    /// Why the figure is not a complete measurement of what it covers: records
    /// the read could not decode, and requests whose recorded usage stopped
    /// with them, and counters with missing or unpriced events. Empty with
    /// `session` coverage, the quantities are complete; cross-recording
    /// identity still depends on an established account namespace.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
}

/// Coverage of one quantity among counted consumption events. The three
/// counts sum to `events.counted`; zero is measured only when it is priced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementCoverage {
    pub measured: usize,
    pub missing: usize,
    pub unpriced: usize,
}

/// The consumption events behind a figure, by the branch that holds them.
/// `active_branch` plus `other_branches` is `counted`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ConsumptionEvents {
    pub counted: usize,
    pub active_branch: usize,
    pub other_branches: usize,
    /// Observations of an event already counted: the same entry recorded
    /// again, or another entry naming the same provider response. A repeat
    /// is a witness and adds nothing.
    pub repeats: usize,
}

/// How the counted events are identified, which decides how far a union
/// with another recording's events can be exact.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ConsumptionIdentity {
    /// Keyed by an observed (provider, response) identifier within this read.
    /// Equality across recordings requires an established account namespace.
    pub provider_response: usize,
    /// Pi does not establish the provider account namespace. Observed response
    /// identifiers alone cannot establish equality across recording sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_account_namespace: Option<String>,
    /// Keyed by the entry's identity within this recording alone; whether
    /// another recording holds the same event is unestablished.
    pub recording_entry: usize,
    /// Measured with no identity at all.
    pub unidentified: usize,
    /// The normalized parent session id derived from a fork's header. A
    /// fork copies its parent's entries, so events the two share are
    /// witnessed in both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// The header records a fork relationship but its parent id cannot be derived.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fork_parent_unestablished: bool,
}

/// A native entry coordinate is local to the selected recording. Equality
/// across recordings requires a verified copy relation, not matching ids alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConsumptionRecordingEntry {
    pub session_id: String,
    pub entry_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsumptionKeyKind {
    ProviderResponse,
    RecordingEntry,
}

/// What one consumption event measured, on the terms a figure is measured:
/// the disjoint token categories, and the cost with its kind and coverage.
/// A category or the cost the event did not record is absent, never zero,
/// and `measurement_completeness` names it. The measurement carries no
/// recording locator.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsumptionEventMeasurement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disjoint: Option<DisjointTokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Present when the event's price is absent or written as a zero beside
    /// usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    /// The event's incompletely measured counters and categories, and why.
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
}

/// An observed identity, with its recording coordinate where available.
/// Provider account equality is unestablished. `copy_of` witnesses a matching
/// consumption entry in the named fork source; `entry_digest` identifies the
/// normalized consumption facts used to verify that relation. `measurement`
/// is the event's own amounts, bounded and omitted with the key.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsumptionEventKey {
    pub kind: ConsumptionKeyKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_account_namespace: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_entry: Option<ConsumptionRecordingEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<ConsumptionRecordingEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurement: Option<ConsumptionEventMeasurement>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ConsumptionKeyOmissions {
    #[serde(skip_serializing_if = "is_zero")]
    pub row_cap: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub byte_budget: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub oversized_key: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub unidentified: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub private_or_invalid_identity: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub unavailable_source: usize,
}

/// Fixed coverage categories contain no native diagnostic or recording locator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsumptionCopyCoverage {
    Session,
    ReadWindow,
    Unavailable,
    Unreadable,
    Unestablished,
}

/// A recent suffix of counted consumption identities with each event's own
/// measurement, separate from the figures that cover the whole read. Omitted
/// or unidentified keys, incomplete source reads, and unestablished account
/// equality prevent an exact cross-recording union unless separate copy
/// evidence establishes it; [`ConsumptionEventKeys::union`] applies that rule
/// to the keys and measurements of two selections.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsumptionEventKeys {
    /// The selected session's own id, absent when it is not a portable
    /// identifier, as a key's own coordinate is then omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The parent session id derived from a fork's header, where it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// The basis the keys' measurements are on: amounts of different bases
    /// never join one union.
    pub basis: AccountingBasis,
    pub keys: Vec<ConsumptionEventKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<usize>,
    pub returned: usize,
    pub omissions: ConsumptionKeyOmissions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<AccountingCoverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable_records: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_coverage: Option<ConsumptionCopyCoverage>,
    #[serde(skip_serializing_if = "is_zero")]
    pub copy_conflicts: usize,
}

/// Recent event identities with their measurements, limited to 1..=10000 keys
/// and an 8 MiB serialized key budget. The figures that cover the whole read
/// remain independent of either output bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsumptionKeyOptions {
    pub limit: usize,
}

/// One raw Codex accounting observation. A row is not necessarily a request:
/// repeated modern records, unchanged legacy totals, and quota-only events are
/// retained as observations but do not become counted requests.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageObservation {
    pub record_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_ordinal: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    pub classification: ObservationClassification,
    pub counted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_token_usage: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_token_usage: Option<Tokens>,
    /// The row's own request counters, `usage` or else `last_token_usage`,
    /// in disjoint categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disjoint: Option<DisjointTokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<RateLimits>,
}

/// Reasons a bounded recent observation suffix omitted rows. The three
/// counters are independent: a row can be evicted by the requested cap while
/// another is rejected by the serialized byte budget.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct UsageObservationOmissions {
    #[serde(skip_serializing_if = "is_zero")]
    pub row_cap: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub byte_budget: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub oversized_row: usize,
}

/// The opt-in observation suffix and the read-side coverage around it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageObservationSeries {
    pub rows: Vec<UsageObservation>,
    pub observed: usize,
    pub returned: usize,
    pub omissions: UsageObservationOmissions,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<ReadGap>,
    #[serde(skip_serializing_if = "is_zero")]
    pub gaps_omitted: usize,
}

/// Options for the core usage-observation entry point. The CLI maps
/// `--series` to this value; the core API does not depend on CLI arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageObservationOptions {
    pub limit: usize,
}

/// Read options for the core usage entry point. Observation rows and event
/// keys are opt-in suffixes; ordinary usage retains neither ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageOptions {
    pub full: bool,
    pub series: Option<UsageObservationOptions>,
    /// Reconcile the recorded total with the family's identified requests.
    pub reconcile: bool,
    pub event_keys: Option<ConsumptionKeyOptions>,
}

/// A backend's bounded, pinned observation result. The returned session owns
/// the same recorded totals and attribution facts as the rows, while the
/// optional ring contains only the requested suffix.
#[derive(Debug)]
pub struct UsageObservationResult {
    pub session: Session,
    pub series: UsageObservationSeries,
}

/// Normalized turns the bounded read reached, by role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TurnCounts {
    pub user: usize,
    pub assistant: usize,
    pub tool: usize,
    pub reasoning: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub system: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub developer: usize,
    pub total: usize,
    pub coverage: TurnCoverage,
}

/// How much of the session the turn counts cover. A read that reached a bound
/// withholding turns counted only what it reached.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnCoverage {
    #[default]
    Session,
    ReadWindow,
}

/// The session identity a usage answer is about, without its transcript.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageSession {
    pub id: String,
    pub source: SourceDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_observation: Option<ModelObservationStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_selections: Vec<crate::model::ModelSelectionSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<Launch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<std::path::PathBuf>,
}

/// One session's usage. `tokens`, `cost`, and `accounting` are the session's
/// own fields, with costs qualified against their tokens: a `recorded-total` is cumulative and a
/// `summed-requests` figure is a sum of per-request records, so either may be
/// added across sessions with compatible accounting, while `coverage` says how much of each session the
/// figure covers. `tokens` keeps the harness's own convention; `disjoint`
/// restates the same counters in categories that add across harnesses.
/// `requests` covers the requests the read identified, which need not be the
/// scope of a recorded total. Cost is only what a harness recorded, and
/// `rate_limits` is the provider's account-wide quota rather than this
/// session's spend.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageView {
    pub schema: &'static str,
    pub session: UsageSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disjoint: Option<DisjointTokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub measurement_coverage: BTreeMap<String, MeasurementCoverage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measurement_incomplete: Vec<String>,
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
    pub turns: TurnCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<RateLimits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub durations_ms: Option<Durations>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_model: Option<Vec<ModelUsage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<UsageAttribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<RequestDimensions>,
    #[serde(skip_serializing_if = "is_zero")]
    pub request_failures: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub unrecognized_usage: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumption: Option<Consumption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_total: Option<RecordedUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<UsageObservationSeries>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation: Option<crate::reconcile::Reconciliation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumption_event_keys: Option<ConsumptionEventKeys>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_tail: Option<TextTailEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentInventory>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Truncation::is_empty")]
    pub truncation: Truncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Project a read session into its usage answer. Nothing is added to the
/// session's counters and nothing is derived from another counter; the
/// projection reports what the read holds.
pub fn usage(transcript: &Transcript) -> UsageView {
    let mut tally = TurnTally::default();
    for turn in &transcript.turns {
        tally.add(turn);
    }
    tally.counts.coverage = turn_coverage(&transcript.truncation);
    project(
        &transcript.session,
        tally,
        ReadFacts {
            read: transcript.read.clone(),
            terminal: transcript.terminal.clone(),
            text_tail: transcript.text_tail.clone(),
            truncated: transcript.truncated,
            truncation: transcript.truncation.clone(),
            notes: transcript.notes.clone(),
        },
    )
}

/// Project a whole-recording read into its usage answer. `session` carries
/// the counters folded from every record, and `turns` every turn the read
/// streamed, so the counts cover the session.
pub fn streamed(session: &Session, mut turns: TurnTally, read: &StreamedTranscript) -> UsageView {
    let mut notes = skipped_records_note(read.skipped)
        .into_iter()
        .collect::<Vec<_>>();
    notes.extend(read.notes.iter().cloned());
    let truncation = Truncation {
        window: None,
        source: read.source_bounds.clone(),
    };
    turns.counts.coverage = turn_coverage(&truncation);
    project(
        session,
        turns,
        ReadFacts {
            read: Some(read.read_evidence(session.source.producer.clone())),
            terminal: read.terminal.clone(),
            text_tail: None,
            truncated: !truncation.is_empty(),
            truncation,
            notes,
        },
    )
}

/// What a read established beside its turns, as a usage answer repeats it.
struct ReadFacts {
    read: Option<ReadEvidence>,
    terminal: Option<TerminalObservation>,
    text_tail: Option<TextTailEvidence>,
    truncated: bool,
    truncation: Truncation,
    notes: Vec<String>,
}

fn project(session: &Session, turns: TurnTally, facts: ReadFacts) -> UsageView {
    let mut detail = session.usage_detail.clone().unwrap_or_default();
    let shared = session.shared_model_reasons();
    let scope = match &detail.attribution {
        Some(attribution) => Some(attribution.coverage),
        None => session
            .accounting
            .as_ref()
            .map(|accounting| accounting.coverage),
    };
    for model in detail.by_model.iter_mut().flatten() {
        model.qualify(scope, &shared);
    }
    if let Some(attribution) = &mut detail.attribution {
        attribution.unattributed_completeness = MeasurementCompleteness::assess(Measured {
            tokens: attribution.unattributed_tokens.as_ref(),
            coverage: Some(&attribution.unattributed_coverage),
            scope: Some(attribution.coverage),
            events: attribution.unattributed,
            ..Measured::default()
        });
    }
    if let Some(recorded) = &mut detail.recorded_total {
        for model in recorded.by_model.iter_mut().flatten() {
            model.qualify(Some(recorded.accounting.coverage), &shared);
        }
    }
    let mut notes = facts.notes;
    notes.extend(detail.accounting_notes);
    UsageView {
        schema: USAGE_SCHEMA,
        session: UsageSession {
            id: session.id.clone(),
            source: session.source.clone(),
            metadata: session.metadata.clone(),
            model: session.model.clone(),
            model_observation: session.model_observation.clone(),
            model_selections: session.model_selections.clone(),
            launch: session.launch,
            started_at: session.started_at,
            last_activity_at: session.last_activity_at,
            directory: session.directory.clone(),
        },
        accounting: session.accounting.clone(),
        tokens: session.tokens.clone(),
        disjoint: session.disjoint_tokens(),
        cost: session.estimated_cost(),
        cost_coverage: session.cost_coverage(),
        measurement_coverage: detail.measurement_coverage,
        measurement_incomplete: detail.measurement_incomplete,
        measurement_completeness: session.measurement_completeness(),
        turns: turns.counts,
        context_window: detail.context_window,
        rate_limits: detail.rate_limits,
        durations_ms: detail.durations_ms,
        by_model: detail.by_model,
        attribution: detail.attribution,
        requests: detail.requests,
        request_failures: detail.request_failures,
        unrecognized_usage: detail.unrecognized_usage,
        consumption: detail.consumption,
        recorded_total: detail.recorded_total,
        series: None,
        reconciliation: None,
        consumption_event_keys: None,
        read: facts.read,
        terminal: facts.terminal,
        text_tail: facts.text_tail,
        content: turns.content.into_option(),
        truncated: facts.truncated,
        truncation: facts.truncation,
        notes,
    }
}

impl Session {
    /// Which of the session's counters are incompletely measured, and why:
    /// the single answer every figure derived from them asks.
    pub fn measurement_completeness(&self) -> MeasurementCompleteness {
        let detail = self.usage_detail.as_ref();
        let cost = self.estimated_cost();
        let cost_coverage = self.cost_coverage();
        let mut completeness = MeasurementCompleteness::assess(Measured {
            tokens: self.tokens.as_ref(),
            cost: cost.as_ref(),
            cost_coverage: cost_coverage.as_ref(),
            coverage: detail.map(|detail| &detail.measurement_coverage),
            scope: self
                .accounting
                .as_ref()
                .map(|accounting| accounting.coverage),
            events: detail
                .and_then(|detail| detail.requests.as_ref())
                .map_or(0, |requests| requests.requests),
        });
        if let Some(detail) = detail {
            completeness.merge(&detail.measurement_completeness);
        }
        if self.request_failures() > 0 {
            completeness.qualify_all(MeasurementReason::StoppedAttempt);
        }
        if self.unrecognized_usage() > 0 {
            completeness.qualify_all(MeasurementReason::UnrecognizedUsage);
        }
        completeness
    }

    /// The session's completeness as a member of a selection, where a
    /// counter it holds no amount for leaves the selection's sum a known
    /// subtotal.
    fn selected_measurement_completeness(&self) -> MeasurementCompleteness {
        let mut completeness = self.measurement_completeness();
        completeness.include_absent(self.tokens.as_ref(), self.estimated_cost().as_ref());
        completeness
    }

    /// Reasons that qualify every model's share, because the read could not
    /// place them on one model.
    fn shared_model_reasons(&self) -> MeasurementCompleteness {
        let mut shared = MeasurementCompleteness::default();
        if self.request_failures() > 0 {
            shared.qualify_all(MeasurementReason::StoppedAttempt);
        }
        if self.unrecognized_usage() > 0 {
            shared.qualify_all(MeasurementReason::UnrecognizedUsage);
        }
        let attribution = self
            .usage_detail
            .as_ref()
            .and_then(|detail| detail.attribution.as_ref());
        if attribution.is_some_and(|attribution| {
            attribution.unattributed > 0 || !attribution.incomplete.is_empty()
        }) {
            shared.qualify_all(MeasurementReason::IncompleteAttribution);
        }
        shared
    }

    fn unrecognized_usage(&self) -> usize {
        self.usage_detail
            .as_ref()
            .map_or(0, |detail| detail.unrecognized_usage)
    }

    fn request_failures(&self) -> usize {
        self.usage_detail
            .as_ref()
            .map_or(0, |detail| detail.request_failures)
    }

    /// The session counters in disjoint categories, under its harness's
    /// convention. Absent when the session recorded no tokens or its harness
    /// has no established convention.
    pub fn disjoint_tokens(&self) -> Option<DisjointTokens> {
        let tokens = self.tokens.as_ref()?;
        let convention = TokenConvention::of(self.harness())?;
        let by_ttl = self
            .usage_detail
            .as_ref()
            .and_then(|detail| detail.cache_write_by_ttl.as_ref());
        let disjoint = DisjointTokens::from_native(tokens, convention, by_ttl);
        Some(disjoint.with_completeness(&self.measurement_completeness()))
    }
}

/// Turn counts and the content inventory, folded one turn at a time so a
/// streamed read never holds its turns.
#[derive(Clone, Debug, Default)]
pub struct TurnTally {
    counts: TurnCounts,
    content: ContentInventory,
}

impl TurnTally {
    pub fn add(&mut self, turn: &Turn) {
        match turn.role {
            Role::User => self.counts.user += 1,
            Role::Assistant => self.counts.assistant += 1,
            Role::Tool => self.counts.tool += 1,
            Role::Reasoning => self.counts.reasoning += 1,
            Role::System => self.counts.system += 1,
            Role::Developer => self.counts.developer += 1,
        }
        self.counts.total += 1;
        self.content.add(turn);
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Only a bound that withheld whole turns can shorten the count. Text cut
/// inside a turn the read did reach leaves the turn counted.
fn turn_coverage(truncation: &Truncation) -> TurnCoverage {
    let withheld_turns = truncation.source.iter().any(|bound| {
        matches!(
            bound,
            SourceBound::FileTail { .. }
                | SourceBound::RecordPage { .. }
                | SourceBound::InputCoverage { .. }
        )
    });
    if withheld_turns {
        TurnCoverage::ReadWindow
    } else {
        TurnCoverage::Session
    }
}

/// A dimension a usage summary groups sessions by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupBy {
    Harness,
    /// The model's id, without the variant that may qualify it.
    Model,
    /// The model's variant, which qualifies its id where a harness records
    /// one — a reasoning effort, a service tier.
    Variant,
    Directory,
}

impl GroupBy {
    /// The session's value for this dimension, absent when the session
    /// carries no such fact.
    fn of(self, session: &Session) -> Option<String> {
        match self {
            Self::Harness => Some(session.harness().to_owned()),
            Self::Model => session.model.as_ref().map(|model| model.id.clone()),
            Self::Variant => session
                .model
                .as_ref()
                .and_then(|model| model.variant.clone()),
            Self::Directory => session
                .directory
                .as_deref()
                .map(|path| path.display().to_string()),
        }
    }
}

/// What a group's sessions share. A dimension the grouping did not ask for,
/// and one the sessions did not record, are both absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GroupKey {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
}

/// How many sessions carried each counter. A sum says how much; this says
/// over how many sessions, so a total is read for what it covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CountedSessions {
    pub input: usize,
    pub output: usize,
    pub reasoning: usize,
    pub cache_read: usize,
    pub cache_write: usize,
    pub cost: usize,
}

/// The sessions behind a sum, counted by the accounting of their counters.
/// A recorded total and a sum of per-request records are both addable; the
/// coverage of a summed figure says how much of its session it covers, and a
/// session with no counters at all contributes nothing but itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CoverageCounts {
    pub recorded_total: usize,
    pub summed_session: usize,
    pub summed_read_window: usize,
    pub since_reset: usize,
    pub no_accounting: usize,
}

/// Counters summed over a set of sessions. Every sum is over the sessions
/// that recorded it, and `counted` says how many those were; a counter no
/// session recorded is absent rather than zero. `tokens` sums native counters
/// and so is offered only within one harness's convention; `disjoint` is the
/// sum that adds across harnesses.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageTally {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mixed_accounting: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub mixed_models: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub attribution_uncertain: usize,
    pub sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disjoint: Option<DisjointSum>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    pub coverage: CoverageCounts,
    pub counted: CountedSessions,
    /// The summed native counters some selected session measured
    /// incompletely or not at all, with every reason among them.
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
    /// Per counter, the events the selected sessions' figures sum, by whether
    /// each recorded it. A session that carries no event coverage adds
    /// nothing here, and `counted` says how many sessions have an amount.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub measurement_coverage: BTreeMap<String, MeasurementCoverage>,
    /// Exact unions require established equality or disjointness of events.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub identity_unestablished: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_bounds: Option<UsageIdentityBounds>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub session_figures: Vec<SessionUsageFigure>,
}

/// One session's measurement, with no recording locator or raw diagnostic.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionUsageFigure {
    pub id: String,
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub measurement_coverage: BTreeMap<String, MeasurementCoverage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub measurement_incomplete: Vec<String>,
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
}

impl From<&Session> for SessionUsageFigure {
    fn from(session: &Session) -> Self {
        let detail = session.usage_detail.as_ref();
        Self {
            id: session.id.clone(),
            harness: session.harness().to_owned(),
            accounting: session.accounting.clone(),
            tokens: session.tokens.clone(),
            cost: session.estimated_cost(),
            cost_coverage: session.cost_coverage(),
            measurement_coverage: detail
                .map(|detail| detail.measurement_coverage.clone())
                .unwrap_or_default(),
            measurement_incomplete: detail
                .map(|detail| detail.measurement_incomplete.clone())
                .unwrap_or_default(),
            measurement_completeness: session.measurement_completeness(),
        }
    }
}

/// Bounds on comparable known measurements when sessions may share events.
/// Missing measurements leave the upper bound unestablished.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct QuantityBounds<T> {
    pub at_least: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_most: Option<T>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageIdentityBounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<QuantityBounds<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<QuantityBounds<f64>>,
}

/// Disjoint categories summed over a set of sessions, each converted from its
/// own harness's convention, which `conventions` names by harness. `sessions`
/// counts the sessions that recorded tokens. A category sums the sessions
/// with known category amounts. `measurement_completeness` gives each
/// category the reasons any selected session's category is incomplete,
/// including sessions with no token counters. `missing` counts those
/// sessions per category; `convention` names an unestablished mapping.
/// `total` requires complete measurement of all four summed categories in
/// every selected session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DisjointSum {
    pub conventions: BTreeMap<String, TokenConvention>,
    pub sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncached_input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<CacheWrites>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "MeasurementCompleteness::is_empty")]
    pub measurement_completeness: MeasurementCompleteness,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: BTreeMap<&'static str, usize>,
}

impl DisjointSum {
    fn add(&mut self, session: &Session) {
        if session.tokens.is_none() {
            self.qualify_every_category(MeasurementReason::MissingCounter);
            return;
        }
        self.sessions += 1;
        let Some(disjoint) = session.disjoint_tokens() else {
            *self.missing.entry("convention").or_default() += 1;
            self.qualify_every_category(MeasurementReason::Undetermined);
            return;
        };
        self.conventions
            .insert(session.harness().to_owned(), disjoint.convention);
        for (total, value) in [
            (&mut self.uncached_input, disjoint.uncached_input),
            (&mut self.cache_read, disjoint.cache_read),
            (&mut self.output, disjoint.output),
            (&mut self.reasoning, disjoint.reasoning),
        ] {
            if let Some(value) = value {
                *total = Some(total.unwrap_or(0).saturating_add(value));
            }
        }
        if let Some(writes) = disjoint.cache_write {
            self.cache_write.get_or_insert_default().add(&writes);
        }
        self.measurement_completeness
            .merge(&disjoint.measurement_completeness);
        for name in disjoint.missing {
            *self.missing.entry(name).or_default() += 1;
        }
    }

    /// A selected session that establishes no category leaves every
    /// category's sum a known subtotal.
    fn qualify_every_category(&mut self, reason: MeasurementReason) {
        for category in crate::measurement::CATEGORIES {
            self.measurement_completeness.qualify(category, reason);
            *self.missing.entry(category).or_default() += 1;
        }
    }

    fn finish(mut self) -> Option<Self> {
        if self.sessions == 0 {
            return None;
        }
        self.total = self
            .measurement_completeness
            .is_complete(&crate::model::SUMMED_CATEGORIES)
            .then(|| {
                [
                    self.uncached_input,
                    self.cache_read,
                    self.cache_write.map(|writes| writes.total()),
                    self.output,
                ]
                .into_iter()
                .try_fold(0u64, |sum, count| {
                    count.map(|count| sum.saturating_add(count))
                })
            })
            .flatten();
        Some(self)
    }
}

/// One group of sessions and what they spent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageGroup {
    pub key: GroupKey,
    #[serde(flatten)]
    pub tally: UsageTally,
}

/// Sessions grouped by the requested dimensions, plus the tally over all of
/// them. Groups are ordered by their key values, ascending, in the order the
/// dimensions were requested; a group whose key value is absent sorts first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageAggregate {
    pub groups: Vec<UsageGroup>,
    pub totals: UsageTally,
}

/// Group sessions by the requested dimensions and sum their recorded
/// counters. Nothing is inferred: a cost is only a recorded cost, a token
/// count is only a recorded token count, and neither is derived from the
/// other. Incompatible accounting withholds native sums and cross-basis
/// disjoint sums; `partitioned_aggregate` also returns compatible subtotals.
/// With no dimensions the whole set is one group under an empty key.
pub fn aggregate(sessions: &[Session], by: &[GroupBy]) -> UsageAggregate {
    partitioned_aggregate(sessions, by).0
}

fn unchecked_aggregate(sessions: &[Session], by: &[GroupBy]) -> UsageAggregate {
    let mut groups: BTreeMap<Vec<Option<String>>, Tally> = BTreeMap::new();
    let mut totals = Tally::default();
    for session in sessions {
        let key = by.iter().map(|dimension| dimension.of(session)).collect();
        groups.entry(key).or_default().add(session);
        totals.add(session);
    }
    UsageAggregate {
        groups: groups
            .into_iter()
            .map(|(values, tally)| UsageGroup {
                key: group_key(by, &values),
                tally: tally.finish(),
            })
            .collect(),
        totals: totals.finish(),
    }
}

fn group_key(by: &[GroupBy], values: &[Option<String>]) -> GroupKey {
    let mut key = GroupKey::default();
    for (dimension, value) in by.iter().zip(values) {
        let slot = match dimension {
            GroupBy::Harness => &mut key.harness,
            GroupBy::Model => &mut key.model,
            GroupBy::Variant => &mut key.variant,
            GroupBy::Directory => &mut key.directory,
        };
        slot.clone_from(value);
    }
    key
}

/// One counter being summed: the running total and how many sessions have
/// contributed to it.
#[derive(Clone, Copy, Default)]
struct Counter {
    total: u64,
    counted: usize,
}

impl Counter {
    fn add(&mut self, value: Option<u64>) {
        if let Some(value) = value {
            self.total = self.total.saturating_add(value);
            self.counted += 1;
        }
    }

    fn sum(self) -> Option<u64> {
        (self.counted > 0).then_some(self.total)
    }
}

#[derive(Clone, Default)]
struct Tally {
    measurement_completeness: MeasurementCompleteness,
    measurement_coverage: BTreeMap<String, MeasurementCoverage>,
    sessions: usize,
    disjoint: DisjointSum,
    input: Counter,
    output: Counter,
    reasoning: Counter,
    cache_read: Counter,
    cache_write: Counter,
    cost_usd: f64,
    cost_counted: usize,
    cost_coverage: Option<CostCoverage>,
    coverage: CoverageCounts,
    mixed_models: usize,
    attribution_uncertain: usize,
}

impl Tally {
    fn add(&mut self, session: &Session) {
        self.measurement_completeness
            .merge(&session.selected_measurement_completeness());
        for (counter, coverage) in session
            .usage_detail
            .iter()
            .flat_map(|detail| &detail.measurement_coverage)
        {
            let sum = self
                .measurement_coverage
                .entry(counter.clone())
                .or_default();
            sum.measured += coverage.measured;
            sum.missing += coverage.missing;
            sum.unpriced += coverage.unpriced;
        }
        self.sessions += 1;
        if let Some(coverage) = session.cost_coverage() {
            let total = self.cost_coverage.get_or_insert(CostCoverage {
                complete: true,
                ..CostCoverage::default()
            });
            total.complete &= coverage.complete;
            total.zero_written_requests += coverage.zero_written_requests;
            total.zero_written_observations += coverage.zero_written_observations;
        }
        self.disjoint.add(session);
        if let Some(tokens) = &session.tokens {
            self.input.add(tokens.input);
            self.output.add(tokens.output);
            self.reasoning.add(tokens.reasoning);
            self.cache_read.add(tokens.cache_read);
            self.cache_write.add(tokens.cache_write);
        }
        if let Some(cost) = session.estimated_cost() {
            self.cost_usd += cost.usd;
            self.cost_counted += 1;
        }
        // Accounting is present exactly when a counter is, so its absence is
        // the session that recorded nothing to sum.
        match session
            .accounting
            .as_ref()
            .map(|accounting| accounting.basis)
        {
            None => self.coverage.no_accounting += 1,
            Some(AccountingBasis::RecordedTotal) => self.coverage.recorded_total += 1,
            Some(AccountingBasis::SummedRequests) => {
                match session
                    .accounting
                    .as_ref()
                    .map(|accounting| accounting.coverage)
                {
                    Some(AccountingCoverage::ReadWindow) => self.coverage.summed_read_window += 1,
                    _ => self.coverage.summed_session += 1,
                }
            }
        }
        if session
            .accounting
            .as_ref()
            .is_some_and(|accounting| accounting.coverage == AccountingCoverage::SinceReset)
        {
            self.coverage.since_reset += 1;
        }
        if let Some(status) = &session.model_observation {
            if status.mixed {
                self.mixed_models += 1;
            }
            if status.attribution_uncertain {
                self.attribution_uncertain += 1;
            }
        }
    }

    fn finish(self) -> UsageTally {
        let counted = CountedSessions {
            input: self.input.counted,
            output: self.output.counted,
            reasoning: self.reasoning.counted,
            cache_read: self.cache_read.counted,
            cache_write: self.cache_write.counted,
            cost: self.cost_counted,
        };
        let any_token_counted = counted.input
            + counted.output
            + counted.reasoning
            + counted.cache_read
            + counted.cache_write
            > 0;
        UsageTally {
            measurement_completeness: self.measurement_completeness,
            measurement_coverage: self.measurement_coverage,
            mixed_accounting: false,
            mixed_models: self.mixed_models,
            attribution_uncertain: self.attribution_uncertain,
            sessions: self.sessions,
            tokens: any_token_counted.then(|| Tokens {
                input: self.input.sum(),
                output: self.output.sum(),
                reasoning: self.reasoning.sum(),
                cache_read: self.cache_read.sum(),
                cache_write: self.cache_write.sum(),
            }),
            disjoint: self.disjoint.finish(),
            cost: (self.cost_counted > 0).then_some(Cost { usd: self.cost_usd }),
            cost_coverage: self.cost_coverage,
            coverage: self.coverage,
            counted,
            identity_unestablished: false,
            identity_bounds: None,
            session_figures: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::{json, Value};

    use super::*;
    use crate::model::{
        AccountingBasis, AccountingCoverage, Session, SourceDescriptor, Turn, TurnKind,
    };

    pub(super) fn session() -> Session {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Session {
            id: "fixture-session".to_owned(),
            source: SourceDescriptor::installed("fixture", "fixture-recording"),
            metadata: None,
            model: None,
            model_observation: None,
            model_selections: Vec::new(),
            launch: None,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory: None,
            started_at: Some(ts),
            last_activity_at: Some(ts),
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            start_uncertain: false,
            occurrence: None,
            usage_detail: None,
        }
    }

    fn turn(role: Role) -> Turn {
        Turn {
            origin: None,
            text_truncated: None,
            text_chars: None,
            context_event: None,
            model: None,
            kind: role.kind().unwrap_or(TurnKind::Operator),
            role,
            text: "fixture".to_owned(),
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
        }
    }

    #[test]
    fn a_session_without_harness_detail_omits_every_optional_object() {
        let transcript = Transcript::new(
            session(),
            vec![turn(Role::User), turn(Role::Assistant)],
            Truncation::default(),
            None,
            Vec::new(),
        );
        let value = serde_json::to_value(usage(&transcript)).unwrap();

        assert_eq!(value["schema"], USAGE_SCHEMA);
        for absent in [
            "tokens",
            "cost",
            "cost_coverage",
            "disjoint",
            "accounting",
            "context_window",
            "rate_limits",
            "durations_ms",
            "by_model",
            "attribution",
            "requests",
            "request_failures",
            "unrecognized_usage",
            "consumption",
            "recorded_total",
            "reconciliation",
            "truncation",
            "notes",
        ] {
            assert!(value.get(absent).is_none(), "{absent} in {value}");
        }
        assert_eq!(
            value["turns"],
            json!({
                "user": 1,
                "assistant": 1,
                "tool": 0,
                "reasoning": 0,
                "total": 2,
                "coverage": "session"
            })
        );
    }

    #[test]
    fn the_view_repeats_the_sessions_counters_and_its_recorded_detail() {
        let mut session = session();
        session.tokens = Some(Tokens {
            input: Some(10),
            output: Some(20),
            reasoning: None,
            cache_read: None,
            cache_write: None,
        });
        session.cost = Some(Cost { usd: 1.5 });
        session.accounting = Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
            selection: None,
        });
        session.usage_detail = Some(UsageDetail {
            context_window: Some(828_400),
            rate_limits: Some(RateLimits {
                primary: Some(RateWindow {
                    used_percent: json!(1.0),
                    window_minutes: Some(300),
                    resets_at: Some(Utc.timestamp_opt(1_788_512_962, 0).unwrap()),
                }),
                secondary: None,
                plan: Some("plus".to_owned()),
                credits: None,
                spend_control_reached: None,
                rate_limit_reached: None,
                rate_limit_reached_type: None,
                observed_at: None,
            }),
            durations_ms: Some(Durations {
                api: Some(1_000),
                api_without_retries: None,
                tool: None,
                total: Some(2_000),
            }),
            by_model: Some(vec![ModelUsage {
                measurement_completeness: Default::default(),
                model: "fixture-model".to_owned(),
                variant: None,
                tokens: Some(Tokens {
                    input: Some(10),
                    output: Some(20),
                    reasoning: None,
                    cache_read: None,
                    cache_write: None,
                }),
                cost: Some(Cost { usd: 1.5 }),
                cost_coverage: None,
                request_count: None,
                measurement_coverage: std::collections::BTreeMap::new(),
                measurement_incomplete: Vec::new(),
            }]),
            attribution: None,
            cache_write_by_ttl: None,
            requests: None,
            cost_coverage: None,
            request_failures: 0,
            unrecognized_usage: 0,
            consumption: None,
            recorded_total: None,
            accounting_notes: Vec::new(),
            ..UsageDetail::default()
        });
        let transcript = Transcript::new(
            session,
            vec![turn(Role::Tool), turn(Role::Reasoning)],
            Truncation::default(),
            None,
            Vec::new(),
        );

        let value: Value = serde_json::to_value(usage(&transcript)).unwrap();
        assert_eq!(value["tokens"], json!({ "input": 10, "output": 20 }));
        assert_eq!(value["cost"]["usd"], 1.5);
        assert_eq!(value["cost"]["kind"], "harness-calculated-estimate");
        assert_eq!(value["cost"]["missing"], json!(["pricing_basis"]));
        assert!(value.get("disjoint").is_none(), "{value}");
        assert_eq!(
            value["accounting"],
            json!({ "basis": "recorded-total", "coverage": "session" })
        );
        assert_eq!(value["context_window"], 828_400);
        assert_eq!(
            value["rate_limits"],
            json!({
                "primary": {
                    "used_percent": 1.0,
                    "window_minutes": 300,
                    "resets_at": "2026-09-04T09:09:22Z"
                },
                "plan": "plus"
            })
        );
        assert_eq!(value["durations_ms"], json!({ "api": 1000, "total": 2000 }));
        assert_eq!(value["by_model"].as_array().unwrap().len(), 1);
        let model = &value["by_model"][0];
        assert_eq!(model["model"], "fixture-model");
        assert_eq!(model["tokens"], json!({ "input": 10, "output": 20 }));
        assert_eq!(model["cost"]["usd"], 1.5);
        assert_eq!(model["cost"]["kind"], "harness-calculated-estimate");
        assert_eq!(model["cost"]["missing"], json!(["pricing_basis"]));
        assert_eq!(value["turns"]["tool"], 1);
        assert_eq!(value["turns"]["reasoning"], 1);
    }

    #[test]
    fn a_bound_that_withheld_turns_makes_the_counts_a_read_window() {
        for (source, expected) in [
            (Vec::new(), "session"),
            (
                vec![SourceBound::FileTail { bytes: 4_194_304 }],
                "read-window",
            ),
            (
                vec![SourceBound::RecordPage {
                    records: 1_000,
                    of: "messages".to_owned(),
                }],
                "read-window",
            ),
            (
                vec![SourceBound::TurnText {
                    turns: 1,
                    chars: 4_000,
                }],
                "session",
            ),
        ] {
            let transcript = Transcript::new(
                session(),
                vec![turn(Role::User)],
                Truncation {
                    window: None,
                    source,
                },
                None,
                Vec::new(),
            );
            let value = serde_json::to_value(usage(&transcript)).unwrap();
            assert_eq!(value["turns"]["coverage"], expected, "{value}");
        }
    }

    fn spender(
        harness: &str,
        model: Option<(&str, Option<&str>)>,
        tokens: Option<Tokens>,
        cost: Option<f64>,
        accounting: Option<(AccountingBasis, AccountingCoverage)>,
    ) -> Session {
        Session {
            source: SourceDescriptor::installed(harness, "fixture-recording"),
            model: model.map(|(id, variant)| Model {
                id: id.to_owned(),
                variant: variant.map(str::to_owned),
            }),
            model_observation: None,
            model_selections: Vec::new(),
            tokens,
            cost: cost.map(|usd| Cost { usd }),
            accounting: accounting.map(|(basis, coverage)| Accounting {
                basis,
                coverage,
                selection: None,
            }),
            ..session()
        }
    }

    fn tokens(input: Option<u64>, output: Option<u64>, reasoning: Option<u64>) -> Option<Tokens> {
        Some(Tokens {
            input,
            output,
            reasoning,
            cache_read: None,
            cache_write: None,
        })
    }

    pub(super) fn spenders() -> Vec<Session> {
        vec![
            spender(
                "codex",
                Some(("gpt-fixture", Some("high"))),
                tokens(Some(100), Some(10), Some(5)),
                Some(1.0),
                Some((AccountingBasis::RecordedTotal, AccountingCoverage::Session)),
            ),
            spender(
                "codex",
                Some(("gpt-fixture", Some("high"))),
                tokens(Some(200), Some(20), None),
                None,
                Some((
                    AccountingBasis::SummedRequests,
                    AccountingCoverage::ReadWindow,
                )),
            ),
            spender("codex", Some(("gpt-fixture", None)), None, None, None),
            spender(
                "claude",
                Some(("claude-fixture", None)),
                Some(Tokens {
                    input: None,
                    output: None,
                    reasoning: None,
                    cache_read: Some(7),
                    cache_write: None,
                }),
                Some(2.5),
                Some((AccountingBasis::SummedRequests, AccountingCoverage::Session)),
            ),
        ]
    }

    /// A sum is over the sessions that recorded the counter, and `counted`
    /// says how many those were, so a group of three sessions whose figure
    /// came from two is not read as three.
    #[test]
    fn a_group_sums_only_the_counters_its_sessions_recorded() {
        let aggregate = unchecked_aggregate(&spenders(), &[GroupBy::Harness, GroupBy::Model]);
        let value = serde_json::to_value(&aggregate).unwrap();

        let codex_native = json!({
            "input": ["missing-counter", "read-window"],
            "output": ["missing-counter", "read-window"],
            "reasoning": ["missing-counter", "read-window"],
            "cache_read": ["missing-counter", "read-window"],
            "cache_write": ["missing-counter", "read-window"],
            "cost": ["missing-price", "read-window"]
        });
        let codex_disjoint = json!({
            "uncached_input": ["missing-counter", "read-window"],
            "output": ["missing-counter", "read-window"],
            "reasoning": ["missing-counter", "read-window"],
            "cache_read": ["missing-counter", "read-window"],
            "cache_write": ["missing-counter", "read-window"]
        });
        assert_eq!(
            value["groups"],
            json!([
                {
                    "key": { "harness": "claude", "model": "claude-fixture" },
                    "sessions": 1,
                    "tokens": { "cache_read": 7 },
                    "measurement_completeness": {
                        "input": ["missing-counter"], "output": ["missing-counter"],
                        "reasoning": ["missing-counter"], "cache_write": ["missing-counter"]
                    },
                    "disjoint": {
                        "conventions": {
                            "claude": { "input": "excludes-cache", "output": "includes-reasoning" }
                        },
                        "sessions": 1,
                        "cache_read": 7,
                        "measurement_completeness": {
                            "uncached_input": ["missing-counter"], "output": ["missing-counter"],
                            "reasoning": ["missing-counter"], "cache_write": ["missing-counter"]
                        },
                        "missing": { "cache_write": 1, "output": 1, "reasoning": 1, "uncached_input": 1 }
                    },
                    "cost": { "usd": 2.5, "kind": "harness-calculated-estimate", "missing": ["pricing_basis"] },
                    "coverage": {
                        "recorded_total": 0,
                        "summed_session": 1,
                        "summed_read_window": 0,
                        "since_reset": 0,
                        "no_accounting": 0
                    },
                    "counted": {
                        "input": 0,
                        "output": 0,
                        "reasoning": 0,
                        "cache_read": 1,
                        "cache_write": 0,
                        "cost": 1
                    }
                },
                {
                    "key": { "harness": "codex", "model": "gpt-fixture" },
                    "sessions": 3,
                    "tokens": { "input": 300, "output": 30, "reasoning": 5 },
                    "measurement_completeness": codex_native,
                    "disjoint": {
                        "conventions": {
                            "codex": { "input": "includes-cache-read", "output": "includes-reasoning" }
                        },
                        "sessions": 2,
                        "output": 30,
                        "reasoning": 5,
                        "measurement_completeness": codex_disjoint,
                        "missing": { "cache_read": 3, "cache_write": 3, "output": 2, "reasoning": 2, "uncached_input": 3 }
                    },
                    "cost": { "usd": 1.0, "kind": "harness-calculated-estimate", "missing": ["pricing_basis"] },
                    "cost_coverage": { "complete": false, "zero_written_requests": 0, "zero_written_observations": 0 },
                    "coverage": {
                        "recorded_total": 1,
                        "summed_session": 0,
                        "summed_read_window": 1,
                        "since_reset": 0,
                        "no_accounting": 1
                    },
                    "counted": {
                        "input": 2,
                        "output": 2,
                        "reasoning": 1,
                        "cache_read": 0,
                        "cache_write": 0,
                        "cost": 1
                    }
                }
            ])
        );
        assert_eq!(
            value["totals"],
            json!({
                "sessions": 4,
                "measurement_completeness": codex_native,
                "tokens": { "input": 300, "output": 30, "reasoning": 5, "cache_read": 7 },
                "disjoint": {
                    "conventions": {
                        "claude": { "input": "excludes-cache", "output": "includes-reasoning" },
                        "codex": { "input": "includes-cache-read", "output": "includes-reasoning" }
                    },
                    "sessions": 3,
                    "cache_read": 7,
                    "output": 30,
                    "reasoning": 5,
                    "measurement_completeness": codex_disjoint,
                    "missing": {
                        "cache_read": 3,
                        "cache_write": 4,
                        "output": 3,
                        "reasoning": 3,
                        "uncached_input": 4
                    }
                },
                "cost": { "usd": 3.5, "kind": "harness-calculated-estimate", "missing": ["pricing_basis"] },
                "cost_coverage": { "complete": false, "zero_written_requests": 0, "zero_written_observations": 0 },
                "coverage": {
                    "recorded_total": 1,
                    "summed_session": 1,
                    "summed_read_window": 1,
                    "since_reset": 0,
                    "no_accounting": 1
                },
                "counted": {
                    "input": 2,
                    "output": 2,
                    "reasoning": 1,
                    "cache_read": 1,
                    "cache_write": 0,
                    "cost": 2
                }
            })
        );
    }

    /// A model without a variant is its own group, keyed by the absence
    /// rather than by an empty string, and a group whose sessions recorded
    /// nothing carries no counters at all.
    #[test]
    fn an_unrecorded_variant_keys_a_group_by_its_absence() {
        let aggregate = aggregate(&spenders()[..3], &[GroupBy::Variant]);
        let value = serde_json::to_value(&aggregate).unwrap();

        assert_eq!(value["groups"][0]["key"], json!({}));
        assert_eq!(value["groups"][0]["sessions"], 1);
        for absent in ["tokens", "cost"] {
            assert!(
                value["groups"][0].get(absent).is_none(),
                "{absent} in {value}"
            );
        }
        assert_eq!(value["groups"][0]["coverage"]["no_accounting"], 1);
        assert_eq!(value["groups"][1]["key"], json!({ "variant": "high" }));
        assert_eq!(value["groups"][1]["sessions"], 2);
        assert_eq!(value["groups"].as_array().unwrap().len(), 2);
    }

    /// Without a dimension the selection is one group, so a summary can be
    /// asked for a single total.
    #[test]
    fn no_dimension_leaves_one_group_over_the_whole_selection() {
        let aggregate = aggregate(&spenders(), &[]);
        assert_eq!(aggregate.groups.len(), 1);
        assert_eq!(aggregate.groups[0].key, GroupKey::default());
        assert_eq!(aggregate.groups[0].tally, aggregate.totals);
    }
}

/// A compatible accounting domain; missing counters remain absent within it.
#[derive(Debug, Serialize)]
pub struct AccountingPartition {
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
    #[serde(flatten)]
    pub tally: UsageTally,
}

pub fn partitioned_aggregate(
    sessions: &[Session],
    by: &[GroupBy],
) -> (UsageAggregate, Vec<AccountingPartition>) {
    let mut aggregate = unchecked_aggregate(sessions, by);
    let mut partitions: BTreeMap<(String, String), (Option<Accounting>, Tally, Domains)> =
        BTreeMap::new();
    let mut domains: BTreeMap<Vec<Option<String>>, Domains> = BTreeMap::new();
    let mut all = Domains::default();
    for session in sessions {
        let key = (
            session.harness().to_owned(),
            format!("{:?}", session.accounting),
        );
        let partition = partitions.entry(key.clone()).or_insert_with(|| {
            (
                session.accounting.clone(),
                Tally::default(),
                Domains::default(),
            )
        });
        partition.1.add(session);
        partition.2.add(session, &key);
        let group = by.iter().map(|dimension| dimension.of(session)).collect();
        domains.entry(group).or_default().add(session, &key);
        all.add(session, &key);
    }
    all.apply(&mut aggregate.totals);
    for (group, domains) in aggregate.groups.iter_mut().zip(domains.into_values()) {
        domains.apply(&mut group.tally);
    }
    let partitions = partitions
        .into_iter()
        .map(|((harness, _), (accounting, tally, domains))| {
            let mut tally = tally.finish();
            domains.apply(&mut tally);
            AccountingPartition {
                harness,
                accounting,
                tally,
            }
        })
        .collect();
    (aggregate, partitions)
}

/// The accounting domains one group's counters span. Native counters add
/// only within one harness and accounting; disjoint categories add across
/// harnesses but still only within one accounting basis and coverage.
#[derive(Default)]
struct Domains {
    native: std::collections::BTreeSet<(String, String)>,
    /// The domains of sessions that counted events and hold an accounting,
    /// with the basis each counts events on: events add only within one
    /// domain.
    covered: std::collections::BTreeMap<(String, String), AccountingBasis>,
    /// The harnesses of sessions that counted events and wrote no counter, so
    /// hold no accounting. Only a request sum counts events without writing
    /// an amount: a recorded total is one event that holds its counters.
    covered_unestablished: std::collections::BTreeSet<String>,
    accounting: std::collections::BTreeSet<String>,
    pi: bool,
    figures: Vec<SessionUsageFigure>,
}

impl Domains {
    fn add(&mut self, session: &Session, key: &(String, String)) {
        self.pi |= session.harness() == "pi";
        self.figures.push(SessionUsageFigure::from(session));
        if session.tokens.is_some() || session.cost.is_some() {
            self.native.insert(key.clone());
        }
        if session
            .usage_detail
            .as_ref()
            .is_some_and(|detail| !detail.measurement_coverage.is_empty())
        {
            match &session.accounting {
                Some(accounting) => {
                    self.covered.insert(key.clone(), accounting.basis);
                }
                None => {
                    self.covered_unestablished
                        .insert(session.harness().to_owned());
                }
            }
        }
        if session.tokens.is_some() {
            self.accounting.insert(key.1.clone());
        }
    }

    fn apply(&self, tally: &mut UsageTally) {
        if self.pi {
            tally.session_figures.clone_from(&self.figures);
        }
        if !self.events_share_a_domain() {
            tally.measurement_coverage.clear();
        }
        if self.native.len() > 1 {
            tally.tokens = None;
            tally.cost = None;
            tally.measurement_coverage.clear();
            tally.mixed_accounting = true;
        }
        if self.accounting.len() > 1 {
            tally.disjoint = None;
        }
        if self.pi && self.figures.len() > 1 {
            tally.identity_unestablished = true;
            tally.identity_bounds = (self.native.len() == 1).then(|| self.bounds());
            tally.tokens = None;
            tally.cost = None;
            tally.disjoint = None;
            tally.measurement_coverage.clear();
        }
    }

    /// Whether every session that counted events counted them in one
    /// harness and accounting basis, so their events may be added.
    fn events_share_a_domain(&self) -> bool {
        let harnesses = self
            .covered
            .keys()
            .map(|(harness, _)| harness)
            .chain(&self.covered_unestablished)
            .collect::<std::collections::BTreeSet<_>>();
        self.covered.len() <= 1
            && harnesses.len() <= 1
            && self.covered.values().all(|basis| {
                self.covered_unestablished.is_empty() || *basis == AccountingBasis::SummedRequests
            })
    }

    fn bounds(&self) -> UsageIdentityBounds {
        let tokens = |name: &str, counter: fn(&Tokens) -> Option<u64>| {
            let values = self
                .figures
                .iter()
                .map(|figure| figure.tokens.as_ref().and_then(counter))
                .collect::<Vec<_>>();
            values.iter().flatten().copied().max().map(|at_least| {
                let complete = self
                    .figures
                    .iter()
                    .zip(&values)
                    .all(|(figure, value)| value.is_some() && figure.counter_complete(name));
                QuantityBounds {
                    at_least,
                    at_most: complete
                        .then(|| values.into_iter().flatten().fold(0u64, u64::saturating_add)),
                }
            })
        };
        let costs = self
            .figures
            .iter()
            .filter_map(|figure| figure.cost.as_ref().map(|cost| cost.usd))
            .collect::<Vec<_>>();
        UsageIdentityBounds {
            input: tokens("input", |tokens| tokens.input),
            output: tokens("output", |tokens| tokens.output),
            reasoning: tokens("reasoning", |tokens| tokens.reasoning),
            cache_read: tokens("cache_read", |tokens| tokens.cache_read),
            cache_write: tokens("cache_write", |tokens| tokens.cache_write),
            cost: costs
                .iter()
                .copied()
                .reduce(f64::max)
                .map(|at_least| QuantityBounds {
                    at_least,
                    at_most: self
                        .figures
                        .iter()
                        .all(|figure| figure.cost.is_some() && figure.counter_complete("cost"))
                        .then(|| costs.iter().sum()),
                }),
        }
    }
}

impl SessionUsageFigure {
    fn counter_complete(&self, counter: &str) -> bool {
        self.measurement_completeness.is_complete(&[counter])
    }
}

#[cfg(test)]
mod partition_tests {
    use super::*;
    use crate::model::{InputConvention, OutputConvention};

    fn native(
        input: u64,
        output: u64,
        reasoning: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> Tokens {
        Tokens {
            input: Some(input),
            output: Some(output),
            reasoning: Some(reasoning),
            cache_read: Some(cache_read),
            cache_write: Some(cache_write),
        }
    }

    fn recorded(harness: &str, tokens: Tokens) -> Session {
        Session {
            source: SourceDescriptor::installed(harness, "fixture-recording"),
            tokens: Some(tokens),
            accounting: Some(Accounting {
                basis: AccountingBasis::RecordedTotal,
                coverage: AccountingCoverage::Session,
                selection: None,
            }),
            ..super::tests::session()
        }
    }

    /// Each harness's native counters land in the same four categories: Codex
    /// cache reads leave its input, OpenCode reasoning joins its output.
    #[test]
    fn each_convention_maps_onto_the_same_disjoint_categories() {
        let codex = DisjointTokens::from_native(
            &native(1_200, 300, 50, 1_000, 0),
            TokenConvention::of("codex").unwrap(),
            None,
        );
        assert_eq!(
            (codex.uncached_input, codex.output, codex.total),
            (Some(200), Some(300), Some(1_500))
        );
        let opencode = DisjointTokens::from_native(
            &native(3, 237, 53, 182_960, 1_122),
            TokenConvention::of("opencode").unwrap(),
            None,
        );
        assert_eq!(
            opencode.convention,
            TokenConvention {
                input: InputConvention::ExcludesCache,
                output: OutputConvention::ExcludesReasoning,
            }
        );
        assert_eq!(
            (opencode.output, opencode.reasoning, opencode.total),
            (Some(290), Some(53), Some(184_375))
        );
        assert_eq!(opencode.missing, vec!["cache_write_ttl"]);
    }

    /// A category the native counters do not determine is named, not filled
    /// in: Codex input with cache writes may or may not include them, and
    /// reasoning beyond its output contradicts the convention.
    #[test]
    fn an_undetermined_category_is_named_missing_and_withholds_the_total() {
        let codex = DisjointTokens::from_native(
            &native(1_200, 300, 400, 1_000, 10),
            TokenConvention::of("codex").unwrap(),
            None,
        );
        assert_eq!(codex.uncached_input, None);
        assert_eq!(codex.reasoning, None);
        assert_eq!(codex.total, None);
        assert_eq!(
            codex.missing,
            vec!["uncached_input", "cache_write_ttl", "reasoning"]
        );
        let unsplit = DisjointTokens::from_native(
            &native(1, 2, 0, 3, 40),
            TokenConvention::of("claude").unwrap(),
            Some(&CacheWrites {
                ephemeral_5m: Some(10),
                ephemeral_1h: Some(10),
                ttl_unrecorded: None,
            }),
        );
        assert_eq!(
            unsplit.cache_write,
            Some(CacheWrites {
                ttl_unrecorded: Some(40),
                ..CacheWrites::default()
            })
        );
    }

    /// A Claude and a Codex total have no raw input sum, but their disjoint
    /// categories add; sessions of different accounting add in neither form.
    #[test]
    fn disjoint_categories_add_across_harnesses_but_not_across_accounting() {
        let sessions = vec![
            recorded("claude", native(5, 65, 12, 2_300, 360)),
            recorded("codex", native(1_200, 300, 50, 1_000, 0)),
        ];
        let (aggregate, partitions) = partitioned_aggregate(&sessions, &[]);
        let totals = &aggregate.totals;
        assert!(totals.mixed_accounting);
        assert!(totals.tokens.is_none());
        let disjoint = totals.disjoint.as_ref().unwrap();
        assert_eq!(disjoint.sessions, 2);
        assert_eq!(disjoint.uncached_input, Some(205));
        assert_eq!(disjoint.cache_read, Some(3_300));
        assert_eq!(disjoint.output, Some(365));
        assert_eq!(disjoint.reasoning, Some(62));
        assert_eq!(disjoint.total, Some(5 + 2_300 + 360 + 65 + 1_500));
        assert_eq!(
            disjoint.conventions.keys().collect::<Vec<_>>(),
            ["claude", "codex"]
        );
        assert_eq!(partitions.len(), 2);

        let mut summed = sessions;
        summed[0].accounting = Some(Accounting {
            basis: AccountingBasis::SummedRequests,
            coverage: AccountingCoverage::Session,
            selection: None,
        });
        let (aggregate, partitions) = partitioned_aggregate(&summed, &[]);
        assert!(aggregate.totals.disjoint.is_none());
        assert!(partitions
            .iter()
            .all(|partition| partition.tally.disjoint.is_some()));
    }

    #[test]
    fn public_aggregate_refuses_cross_basis_disjoint_sums() {
        let mut sessions = vec![
            recorded("claude", native(5, 65, 12, 2_300, 360)),
            recorded("claude", native(5, 65, 12, 2_300, 360)),
        ];
        sessions[1].accounting.as_mut().unwrap().basis = AccountingBasis::SummedRequests;
        let result = aggregate(&sessions, &[GroupBy::Harness]);
        assert!(result.totals.mixed_accounting);
        assert!(result.totals.disjoint.is_none());
        assert!(result.groups[0].tally.disjoint.is_none());
        let (_, partitions) = partitioned_aggregate(&sessions, &[]);
        assert_eq!(partitions.len(), 2);
        assert!(partitions.iter().all(|part| part.tally.disjoint.is_some()));
    }

    #[test]
    fn aggregate_reports_incomplete_cost_and_the_zero_written_count() {
        let mut priced = recorded("claude", native(1, 2, 0, 0, 0));
        priced.cost = Some(Cost { usd: 0.5 });
        let mut unpriced = priced.clone();
        unpriced.cost = None;
        let value = serde_json::to_value(aggregate(&[priced, unpriced], &[])).unwrap();
        assert_eq!(value["totals"]["cost_coverage"]["complete"], false);
    }

    #[test]
    fn public_aggregates_classify_incoming_zero_cost_and_keep_priced_subtotals() {
        let mut zero = recorded("pi", native(40, 60, 10, 12, 14));
        zero.cost = Some(Cost { usd: 0.0 });
        let alone = aggregate(&[zero.clone()], &[]);
        assert!(alone.totals.cost.is_none());
        let value = serde_json::to_value(&alone).unwrap();
        assert_eq!(value["totals"]["cost_coverage"]["complete"], false);
        assert_eq!(
            value["totals"]["cost_coverage"]["zero_written_observations"],
            1
        );
        assert_eq!(value["totals"]["counted"]["cost"], 0);
        let mut priced = zero.clone();
        priced.cost = Some(Cost { usd: 0.5 });
        let (mixed, partitions) = partitioned_aggregate(&[zero, priced], &[]);
        assert!(mixed.totals.cost.is_none());
        assert_eq!(
            mixed
                .totals
                .identity_bounds
                .as_ref()
                .unwrap()
                .cost
                .as_ref()
                .unwrap()
                .at_least,
            0.5
        );
        assert!(mixed
            .totals
            .identity_bounds
            .as_ref()
            .unwrap()
            .cost
            .as_ref()
            .unwrap()
            .at_most
            .is_none());
        assert_eq!(
            mixed.totals.session_figures[1].cost.as_ref().unwrap().usd,
            0.5
        );
        assert_eq!(mixed.totals.counted.cost, 1);
        let value = serde_json::to_value(&partitions).unwrap();
        assert_eq!(value[0]["cost_coverage"]["complete"], false);
        assert_eq!(value[0]["cost_coverage"]["zero_written_observations"], 1);
    }

    #[test]
    fn an_empty_group_does_not_shift_a_later_mixed_group() {
        let sessions = super::tests::spenders()[..3].to_vec();
        let (aggregate, _) = partitioned_aggregate(&sessions, &[GroupBy::Variant]);
        let empty_variant = &aggregate.groups[0];
        let high = &aggregate.groups[1];
        assert!(empty_variant.key.variant.is_none());
        assert!(!empty_variant.tally.mixed_accounting);
        assert_eq!(high.key.variant.as_deref(), Some("high"));
        assert!(high.tally.mixed_accounting);
        assert!(high.tally.tokens.is_none());
    }
}
