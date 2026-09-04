//! The single-session usage view: where a session's quota went.
//!
//! `Session` carries the normalized counters and the `accounting` record
//! stating their basis and coverage. What a harness records beside them — a
//! context window, a provider quota window, wall-clock durations, a per-model
//! split — has no normalized field, so it rides on the session as
//! `usage_detail` and reaches a consumer through this projection.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::model::{Accounting, Cost, Model, Role, SourceBound, Tokens, Transcript, Truncation};

pub const USAGE_SCHEMA: &str = "tapes-usage/1";

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
}

impl RateLimits {
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.secondary.is_none() && self.plan.is_none()
    }
}

/// One quota window: how much of it is spent, how long it is, and when it
/// starts over.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RateWindow {
    pub used_percent: f64,
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
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub started_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<std::path::PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
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
    let session = &transcript.session;
    let detail = session.usage_detail.clone().unwrap_or_default();
    UsageView {
        schema: USAGE_SCHEMA,
        session: UsageSession {
            id: session.id.clone(),
            harness: session.harness.clone(),
            model: session.model.clone(),
            started_at: session.started_at,
            last_activity_at: session.last_activity_at,
            directory: session.directory.clone(),
            store: session.store.clone(),
        },
        accounting: session.accounting.clone(),
        tokens: session.tokens.clone(),
        cost: session.cost.clone(),
        turns: turn_counts(transcript),
        context_window: detail.context_window,
        rate_limits: detail.rate_limits,
        durations_ms: detail.durations_ms,
        by_model: detail.by_model,
        truncated: transcript.truncated,
        truncation: transcript.truncation.clone(),
        notes: transcript.notes.clone(),
    }
}

fn turn_counts(transcript: &Transcript) -> TurnCounts {
    let mut counts = TurnCounts {
        coverage: turn_coverage(&transcript.truncation),
        ..TurnCounts::default()
    };
    for turn in &transcript.turns {
        match turn.role {
            Role::User => counts.user += 1,
            Role::Assistant => counts.assistant += 1,
            Role::Tool => counts.tool += 1,
            Role::Reasoning => counts.reasoning += 1,
        }
        counts.total += 1;
    }
    counts
}

/// Only a bound that withheld whole turns can shorten the count. Text cut
/// inside a turn the read did reach leaves the turn counted.
fn turn_coverage(truncation: &Truncation) -> TurnCoverage {
    let withheld_turns = truncation.source.iter().any(|bound| {
        matches!(
            bound,
            SourceBound::FileTail { .. } | SourceBound::RecordPage { .. }
        )
    });
    if withheld_turns {
        TurnCoverage::ReadWindow
    } else {
        TurnCoverage::Session
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::{json, Value};

    use super::*;
    use crate::model::{AccountingBasis, AccountingCoverage, Session, Turn, TurnKind};

    fn session() -> Session {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Session {
            id: "fixture-session".to_owned(),
            harness: "fixture".to_owned(),
            model: None,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory: None,
            started_at: ts,
            last_activity_at: ts,
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            store: None,
            start_uncertain: false,
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
                    used_percent: 1.0,
                    window_minutes: Some(300),
                    resets_at: Some(Utc.timestamp_opt(1_788_512_962, 0).unwrap()),
                }),
                secondary: None,
                plan: Some("plus".to_owned()),
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
}
