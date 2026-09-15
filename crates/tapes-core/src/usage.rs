//! The single-session usage view: where a session's quota went.
//!
//! `Session` carries the normalized counters and the `accounting` record
//! stating their basis and coverage. What a harness records beside them — a
//! context window, a provider quota window, wall-clock durations, a per-model
//! split — has no normalized field, so it rides on the session as
//! `usage_detail` and reaches a consumer through this projection.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::backend::{skipped_records_note, StreamedTranscript};
use crate::content::ContentInventory;
use crate::model::{
    Accounting, AccountingBasis, AccountingCoverage, Cost, Model, ReadEvidence, Role, Session,
    SessionMetadata, SourceBound, SourceDescriptor, TerminalObservation, TextTailEvidence, Tokens,
    Transcript, Truncation, Turn,
};

pub const USAGE_SCHEMA: &str = "tapes-usage/5";

/// Usage facts a harness records that the normalized session model has no
/// field for. Each member is present exactly when the harness recorded it.
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
}

impl UsageDetail {
    pub fn is_empty(&self) -> bool {
        self.context_window.is_none()
            && self.rate_limits.is_none()
            && self.durations_ms.is_none()
            && self.by_model.is_none()
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
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
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
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<std::path::PathBuf>,
}

/// One session's usage. `tokens`, `cost`, and `accounting` are the session's
/// own fields, unchanged: a `recorded-total` is cumulative and a
/// `summed-requests` figure is a sum of per-request records, so either may be
/// added across sessions, while `coverage` says how much of each session the
/// figure covers. Cost is only what a harness recorded, and `rate_limits` is
/// the provider's account-wide quota rather than this session's spend.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageView {
    pub schema: &'static str,
    pub session: UsageSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
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
    let detail = session.usage_detail.clone().unwrap_or_default();
    UsageView {
        schema: USAGE_SCHEMA,
        session: UsageSession {
            id: session.id.clone(),
            source: session.source.clone(),
            metadata: session.metadata.clone(),
            model: session.model.clone(),
            started_at: session.started_at,
            last_activity_at: session.last_activity_at,
            directory: session.directory.clone(),
        },
        accounting: session.accounting.clone(),
        tokens: session.tokens.clone(),
        cost: session.cost.clone(),
        turns: turns.counts,
        context_window: detail.context_window,
        rate_limits: detail.rate_limits,
        durations_ms: detail.durations_ms,
        by_model: detail.by_model,
        read: facts.read,
        terminal: facts.terminal,
        text_tail: facts.text_tail,
        content: turns.content.into_option(),
        truncated: facts.truncated,
        truncation: facts.truncation,
        notes: facts.notes,
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
    pub no_accounting: usize,
}

/// Counters summed over a set of sessions. Every sum is over the sessions
/// that recorded it, and `counted` says how many those were; a counter no
/// session recorded is absent rather than zero.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageTally {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mixed_accounting: bool,
    pub sessions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    pub coverage: CoverageCounts,
    pub counted: CountedSessions,
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
/// other. With no dimensions the whole set is one group under an empty key.
pub fn aggregate(sessions: &[Session], by: &[GroupBy]) -> UsageAggregate {
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

#[derive(Clone, Copy, Default)]
struct Tally {
    sessions: usize,
    input: Counter,
    output: Counter,
    reasoning: Counter,
    cache_read: Counter,
    cache_write: Counter,
    cost_usd: f64,
    cost_counted: usize,
    coverage: CoverageCounts,
}

impl Tally {
    fn add(&mut self, session: &Session) {
        self.sessions += 1;
        if let Some(tokens) = &session.tokens {
            self.input.add(tokens.input);
            self.output.add(tokens.output);
            self.reasoning.add(tokens.reasoning);
            self.cache_read.add(tokens.cache_read);
            self.cache_write.add(tokens.cache_write);
        }
        if let Some(cost) = &session.cost {
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
            mixed_accounting: false,
            sessions: self.sessions,
            tokens: any_token_counted.then(|| Tokens {
                input: self.input.sum(),
                output: self.output.sum(),
                reasoning: self.reasoning.sum(),
                cache_read: self.cache_read.sum(),
                cache_write: self.cache_write.sum(),
            }),
            cost: (self.cost_counted > 0).then_some(Cost { usd: self.cost_usd }),
            coverage: self.coverage,
            counted,
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

    fn session() -> Session {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Session {
            id: "fixture-session".to_owned(),
            source: SourceDescriptor::installed("fixture", "fixture-recording"),
            metadata: None,
            model: None,
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
            "accounting",
            "context_window",
            "rate_limits",
            "durations_ms",
            "by_model",
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
                model: "fixture-model".to_owned(),
                tokens: Some(Tokens {
                    input: Some(10),
                    output: Some(20),
                    reasoning: None,
                    cache_read: None,
                    cache_write: None,
                }),
                cost: Some(Cost { usd: 1.5 }),
            }]),
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
        assert_eq!(value["cost"], json!({ "usd": 1.5 }));
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
        assert_eq!(
            value["by_model"],
            json!([{
                "model": "fixture-model",
                "tokens": { "input": 10, "output": 20 },
                "cost": { "usd": 1.5 }
            }])
        );
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
            tokens,
            cost: cost.map(|usd| Cost { usd }),
            accounting: accounting.map(|(basis, coverage)| Accounting { basis, coverage }),
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
        let aggregate = aggregate(&spenders(), &[GroupBy::Harness, GroupBy::Model]);
        let value = serde_json::to_value(&aggregate).unwrap();

        assert_eq!(
            value["groups"],
            json!([
                {
                    "key": { "harness": "claude", "model": "claude-fixture" },
                    "sessions": 1,
                    "tokens": { "cache_read": 7 },
                    "cost": { "usd": 2.5 },
                    "coverage": {
                        "recorded_total": 0,
                        "summed_session": 1,
                        "summed_read_window": 0,
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
                    "cost": { "usd": 1.0 },
                    "coverage": {
                        "recorded_total": 1,
                        "summed_session": 0,
                        "summed_read_window": 1,
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
                "tokens": { "input": 300, "output": 30, "reasoning": 5, "cache_read": 7 },
                "cost": { "usd": 3.5 },
                "coverage": {
                    "recorded_total": 1,
                    "summed_session": 1,
                    "summed_read_window": 1,
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
    let mut aggregate = aggregate(sessions, by);
    let mut partitions: BTreeMap<(String, String), (Option<Accounting>, Tally)> = BTreeMap::new();
    let mut domains: BTreeMap<Vec<Option<String>>, std::collections::BTreeSet<(String, String)>> =
        BTreeMap::new();
    let mut all = std::collections::BTreeSet::new();
    for session in sessions {
        let key = (
            session.harness().to_owned(),
            format!("{:?}", session.accounting),
        );
        partitions
            .entry(key.clone())
            .or_insert_with(|| (session.accounting.clone(), Tally::default()))
            .1
            .add(session);
        let group = by.iter().map(|dimension| dimension.of(session)).collect();
        let domains = domains.entry(group).or_default();
        if session.tokens.is_some() || session.cost.is_some() {
            domains.insert(key.clone());
            all.insert(key);
        }
    }
    let suppress = |tally: &mut UsageTally| {
        tally.tokens = None;
        tally.cost = None;
        tally.mixed_accounting = true;
    };
    if all.len() > 1 {
        suppress(&mut aggregate.totals);
    }
    for (group, domains) in aggregate.groups.iter_mut().zip(domains.into_values()) {
        if domains.len() > 1 {
            suppress(&mut group.tally);
        }
    }
    let partitions = partitions
        .into_iter()
        .map(|((harness, _), (accounting, tally))| AccountingPartition {
            harness,
            accounting,
            tally: tally.finish(),
        })
        .collect();
    (aggregate, partitions)
}

#[cfg(test)]
mod partition_tests {
    use super::*;
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
