//! Reconciling a recorded usage total with the requests a recording family
//! identifies.
//!
//! A harness's cumulative total and the request records beside it are two
//! observations whose scopes need not match: the total may count children,
//! background calls, or a counter epoch the records do not. The report keeps
//! both as source facts, selects request witnesses within each recording
//! and validated family, and subtracts only where identity and frontier
//! evidence define the comparison. A difference is signed and never clamped;
//! a positive one is a discrepancy until the total's membership is established.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::model::{Accounting, Cost, CostCoverage, Tokens};
use crate::usage::{ModelUsage, RecordedUsage};

/// The reader rules a reconciliation applies. A report names them so it can
/// be reproduced from the same source revisions.
pub const RECONCILIATION_RULES: &str = "tapes-claude-reconciliation/1";

/// The most distinct request identities one reconciliation holds. Past it the
/// union is incomplete and the reads that reached it say so.
const MAX_IDENTIFIED_REQUESTS: usize = 200_000;
/// The most conflicting request identities a report names.
const MAX_CONFLICT_EXAMPLES: usize = 16;

/// The comparable native counters, in report order.
const COUNTERS: [&str; 5] = ["input", "output", "reasoning", "cache_read", "cache_write"];

fn counter(tokens: &Tokens, name: &str) -> Option<u64> {
    match name {
        "input" => tokens.input,
        "output" => tokens.output,
        "reasoning" => tokens.reasoning,
        "cache_read" => tokens.cache_read,
        "cache_write" => tokens.cache_write,
        _ => None,
    }
}

fn add_counter(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

fn add_tokens(total: &mut Option<Tokens>, value: &Tokens) {
    let total = total.get_or_insert(Tokens {
        input: None,
        output: None,
        reasoning: None,
        cache_read: None,
        cache_write: None,
    });
    add_counter(&mut total.input, value.input);
    add_counter(&mut total.output, value.output);
    add_counter(&mut total.reasoning, value.reasoning);
    add_counter(&mut total.cache_read, value.cache_read);
    add_counter(&mut total.cache_write, value.cache_write);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceRole {
    Parent,
    Child,
}

/// Why a reconciliation could not use a source or enumerate a family. A
/// reason is a fixed category: the error that caused it can name a recording
/// path, so none of its text reaches the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureReason {
    /// The recording names a native session other than the family's, or a child
    /// recording names another agent.
    ForeignIdentity,
    /// A child recording carries no native parent identity evidence.
    NoIdentity,
    /// The recording could not be opened, read, or replayed unchanged.
    SourceUnreadableOrChanged,
    /// The family's subagent recordings could not be listed.
    SubagentsUnenumerated,
}

impl FailureReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ForeignIdentity => "foreign-identity",
            Self::NoIdentity => "no-identity",
            Self::SourceUnreadableOrChanged => "source-unreadable-or-changed",
            Self::SubagentsUnenumerated => "subagents-unenumerated",
        }
    }
}

/// One recording the reconciliation read whole. `revision`, `length`, and
/// `sha256` identify the exact bytes read. `records` counts request records
/// with usage; `requests` the distinct recording-local identities, whose
/// counters `identified` sums. These local figures are separate observations.
/// `unkeyed` counts records with usage and no request identity,
/// `unrequested` records the harness marks as answered without a request,
/// and `failed` records the harness wrote in place of a response that failed,
/// whose consumption is unknown. `unrecognized_usage` counts records the
/// reader has no reading for that carry usage-shaped fields. A source is
/// `complete` only when every record decoded, none stands for a failed
/// request or carries usage no request reading counts, and the identity
/// budget held.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReconciledSource {
    pub role: SourceRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub locator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub complete: bool,
    pub records: usize,
    pub requests: usize,
    pub unkeyed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unkeyed_tokens: Option<Tokens>,
    pub unrequested: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub failed: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub unrecognized_usage: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub skipped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identified: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The harness's cumulative total: the newest observation in the parent, kept
/// as recorded. `resets` counts observations below their predecessor on some
/// counter, each a new counter epoch. `after_last_request` says whether the
/// newest observation reaches every source's request frontier. Parent file
/// positions order parent requests. A child request is ordered against the
/// total by time: the total's own timestamp where it writes one, else the
/// newest timestamp of the parent records written before it, since records
/// are appended and the total was written no earlier than any of them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordedTotal {
    pub tokens: Tokens,
    pub accounting: Accounting,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    pub observations: usize,
    pub resets: usize,
    pub after_last_request: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

/// Request identities selected within each recording. A family union and
/// its request count require a compatible provider/account namespace;
/// without that evidence `identity_coverage` is unestablished and union
/// figures are absent. Native children validated below the parent's subagent
/// directory share its request namespace. `revised` and `conflicts` describe
/// the selected witnesses in that namespace. A witness dominating every
/// other counter observation discharges a provisional conflict. Unkeyed
/// counters remain separate measurements.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IdentifiedRequests {
    pub identity_coverage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cross_recording: Option<usize>,
    pub revised: usize,
    pub conflicts: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicting: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unkeyed: Option<Tokens>,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CounterStatus {
    /// The total equals the identified union.
    Reconciled,
    /// The total exceeds the identified union, or the union is incomplete,
    /// and the total's scope is not established; the difference is not
    /// proved consumption beyond the identified requests.
    Discrepancy,
    /// The identified union exceeds the total it should be within.
    Conflict,
    /// The two figures are not comparable, so no difference is computed.
    Withheld,
}

/// One counter's recorded total, identified union, and their signed
/// difference `recorded - identified`. `reasons` names what kept the counter
/// from reconciling.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CounterReconciliation {
    pub counter: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identified: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difference: Option<i128>,
    pub status: CounterStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<&'static str>,
}

/// One model's recorded total beside the requests identified under it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelReconciliation {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_coverage: Option<CostCoverage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identified: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests: Option<usize>,
}

/// A recorded total reconciled with a recording family's identified requests.
/// `missing` names the evidence that would establish the total's scope.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reconciliation {
    pub rules: &'static str,
    pub sources: Vec<ReconciledSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded: Option<RecordedTotal>,
    pub identified: IdentifiedRequests,
    pub counters: Vec<CounterReconciliation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by_model: Vec<ModelReconciliation>,
    pub scope_established: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

fn failure_text(session_id: &str, reason: FailureReason) -> String {
    format!("Claude session {session_id}: {}", reason.as_str())
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// What one request identity's witnesses established.
#[derive(Clone)]
struct Witness {
    tokens: Tokens,
    model: Option<String>,
    first_source: usize,
    envelope: Tokens,
    conflicting_counters: [bool; COUNTERS.len()],
    revised: bool,
    conflicting: bool,
}

impl Witness {
    fn observe(&mut self, tokens: &Tokens) {
        self.revised |= self.tokens != *tokens;
        if tokens.dominates(&self.envelope) {
            self.tokens = tokens.clone();
            self.conflicting = false;
            self.conflicting_counters = [false; COUNTERS.len()];
        } else if !self.tokens.dominates(tokens) {
            for (index, name) in COUNTERS.iter().enumerate() {
                self.conflicting_counters[index] |=
                    counter(&self.tokens, name) != counter(tokens, name);
            }
            self.conflicting = true;
            if tokens.dominates(&self.tokens) {
                self.tokens = tokens.clone();
            }
        }
        extend_envelope(&mut self.envelope, tokens);
    }

    fn merge(&mut self, other: &Self) {
        self.observe(&other.tokens);
        self.revised |= other.revised;
        extend_envelope(&mut self.envelope, &other.envelope);
        if self.tokens.dominates(&self.envelope) {
            self.conflicting = false;
            self.conflicting_counters = [false; COUNTERS.len()];
        } else {
            self.conflicting = true;
            for (held, incoming) in self
                .conflicting_counters
                .iter_mut()
                .zip(other.conflicting_counters)
            {
                *held |= incoming;
            }
        }
    }
}

fn extend_envelope(envelope: &mut Tokens, tokens: &Tokens) {
    for (held, value) in [
        (&mut envelope.input, tokens.input),
        (&mut envelope.output, tokens.output),
        (&mut envelope.reasoning, tokens.reasoning),
        (&mut envelope.cache_read, tokens.cache_read),
        (&mut envelope.cache_write, tokens.cache_write),
    ] {
        if let Some(value) = value {
            *held = Some(held.unwrap_or(0).max(value));
        }
    }
}

/// Request witnesses and recorded totals gathered across a family's sources.
#[derive(Default)]
pub(crate) struct FamilyLedger {
    sources: Vec<ReconciledSource>,
    family_members: Vec<bool>,
    requests: BTreeMap<(usize, String), Witness>,
    budget_exhausted: bool,
    recorded: Option<Tokens>,
    recorded_observations: usize,
    recorded_resets: usize,
    recorded_by_model: BTreeMap<String, Tokens>,
    recorded_usage: Option<RecordedUsage>,
    recorded_position: Option<usize>,
    last_parent_request: Option<usize>,
    recorded_at: Option<DateTime<Utc>>,
    /// The newest timestamp of the parent records read so far, in file
    /// order, and its value when the newest total was recorded.
    newest_parent_stamp: Option<DateTime<Utc>>,
    recorded_after: Option<DateTime<Utc>>,
    last_child_request: Option<DateTime<Utc>>,
    child_frontier_missing: bool,
    unenumerated: bool,
    notes: Vec<String>,
}

impl FamilyLedger {
    /// Open a source and return its index for the observations that follow.
    pub(crate) fn source(
        &mut self,
        role: SourceRole,
        agent_id: Option<String>,
        locator: String,
    ) -> usize {
        self.family_members
            .push(self.sources.is_empty() && role == SourceRole::Parent);
        self.sources.push(ReconciledSource {
            role,
            agent_id,
            locator,
            revision: None,
            length: None,
            sha256: None,
            complete: false,
            records: 0,
            requests: 0,
            unkeyed: 0,
            unkeyed_tokens: None,
            unrequested: 0,
            failed: 0,
            unrecognized_usage: 0,
            skipped: 0,
            identified: None,
            error: None,
        });
        self.sources.len() - 1
    }

    /// Admit a child whose native parent identity and directory membership
    /// were positively validated by the family reader.
    pub(crate) fn validated_child(&mut self, source: usize) {
        if self.sources[source].role == SourceRole::Child {
            self.family_members[source] = true;
        }
    }

    /// Record the bytes a source's read covered, and whether it decoded all.
    pub(crate) fn read(
        &mut self,
        source: usize,
        revision: String,
        length: u64,
        sha256: String,
        skipped: usize,
    ) {
        let entry = &mut self.sources[source];
        entry.revision = Some(revision);
        entry.length = Some(length);
        entry.sha256 = Some(sha256);
        entry.skipped = skipped;
        entry.complete = skipped == 0
            && entry.failed == 0
            && entry.unrecognized_usage == 0
            && !self.budget_exhausted;
    }

    pub(crate) fn failed(&mut self, source: usize, session_id: &str, reason: FailureReason) {
        let entry = &mut self.sources[source];
        entry.complete = false;
        entry.error = Some(failure_text(session_id, reason));
        self.family_members[source] = false;
        entry.requests = 0;
        entry.identified = None;
        entry.unkeyed_tokens = None;
        self.requests.retain(|(owner, _), _| *owner != source);
    }

    /// A child kept under the parent but spawned under `owner`, which the
    /// parent continued: its requests are the owner's family's, so this one
    /// holds no source for it.
    pub(crate) fn carried_child(&mut self, agent_id: &str, owner: &str) {
        self.notes.push(format!(
            "Subagent {agent_id} is carried over from session {owner}, which spawned it; its \
             requests are counted in that session's family."
        ));
    }

    /// The family's children could not be listed, so the union cannot be
    /// complete.
    pub(crate) fn unenumerated(&mut self, session_id: &str, reason: FailureReason) {
        self.unenumerated = true;
        self.notes.push(failure_text(session_id, reason));
    }

    /// A record the reader has no reading for that carries usage-shaped
    /// fields: consumption the union does not hold.
    pub(crate) fn unrecognized_usage(&mut self, source: usize) {
        self.sources[source].unrecognized_usage += 1;
    }

    pub(crate) fn unrequested(&mut self, source: usize) {
        self.sources[source].unrequested += 1;
    }

    /// A record standing in for a failed response. Its counters are the
    /// harness's placeholder, not a measurement, so it joins no union.
    pub(crate) fn failed_request(&mut self, source: usize) {
        self.sources[source].failed += 1;
    }

    /// One request record's usage. `position` is the record's place in its
    /// source, which orders a parent's requests against its recorded totals.
    pub(crate) fn request(
        &mut self,
        source: usize,
        position: usize,
        request_id: Option<&str>,
        model: Option<&str>,
        tokens: Tokens,
        observed_at: Option<DateTime<Utc>>,
    ) {
        self.sources[source].records += 1;
        if self.sources[source].role == SourceRole::Parent {
            self.last_parent_request = Some(position);
        } else if let Some(observed_at) = observed_at {
            self.last_child_request = Some(
                self.last_child_request
                    .map_or(observed_at, |held| held.max(observed_at)),
            );
        } else {
            self.child_frontier_missing = true;
        }
        let Some(request_id) = request_id else {
            self.sources[source].unkeyed += 1;
            add_tokens(&mut self.sources[source].unkeyed_tokens, &tokens);
            return;
        };
        let key = (source, request_id.to_owned());
        if let Some(witness) = self.requests.get_mut(&key) {
            witness.observe(&tokens);
            return;
        }
        if self.requests.len() >= MAX_IDENTIFIED_REQUESTS {
            self.budget_exhausted = true;
            return;
        }
        self.sources[source].requests += 1;
        self.requests.insert(
            key,
            Witness {
                envelope: tokens.clone(),
                conflicting_counters: [false; COUNTERS.len()],
                tokens,
                model: model.map(str::to_owned),
                first_source: source,
                revised: false,
                conflicting: false,
            },
        );
    }

    /// A parent record's timestamp, in file order: a total recorded later
    /// was written no earlier than it.
    pub(crate) fn parent_stamp(&mut self, at: DateTime<Utc>) {
        self.newest_parent_stamp = Some(self.newest_parent_stamp.map_or(at, |held| held.max(at)));
    }

    /// One cumulative total the parent recorded, with its per-model split.
    pub(crate) fn recorded(
        &mut self,
        position: usize,
        tokens: Tokens,
        by_model: BTreeMap<String, Tokens>,
        usage: RecordedUsage,
        observed_at: Option<DateTime<Utc>>,
    ) {
        if self.recorded.as_ref().is_some_and(|before| {
            COUNTERS.iter().any(|name| {
                matches!((counter(before, name), counter(&tokens, name)),
                (Some(before), Some(after)) if after < before)
            })
        }) {
            self.recorded_resets += 1;
        }
        self.recorded = Some(tokens);
        self.recorded_observations += 1;
        self.recorded_usage = Some(usage);
        self.recorded_by_model = by_model;
        self.recorded_position = Some(position);
        self.recorded_at = observed_at;
        self.recorded_after = self.newest_parent_stamp;
    }

    pub(crate) fn finish(mut self) -> Reconciliation {
        let mut union = None;
        let namespace_established = !self.sources.is_empty()
            && !self.unenumerated
            && self.family_members.iter().all(|member| *member);
        let mut conflicting_counters = [false; COUNTERS.len()];
        let mut conflicting = Vec::new();
        let mut conflicts = 0;
        let mut revised = 0;
        let mut by_source: Vec<Option<Tokens>> = vec![None; self.sources.len()];
        let mut identified_by_model: BTreeMap<String, (Option<Tokens>, usize)> = BTreeMap::new();
        let mut partially_recorded = [false; COUNTERS.len()];
        let mut family: BTreeMap<(Option<usize>, &str), (Witness, bool)> = BTreeMap::new();
        for ((_, request_id), witness) in &self.requests {
            add_tokens(&mut by_source[witness.first_source], &witness.tokens);
            let namespace = (!namespace_established).then_some(witness.first_source);
            match family.entry((namespace, request_id.as_str())) {
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let (held, cross_recording) = entry.get_mut();
                    *cross_recording |= held.first_source != witness.first_source;
                    held.merge(witness);
                }
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert((witness.clone(), false));
                }
            }
        }
        let mut cross_recording = 0;
        for ((_, request_id), (witness, cross)) in &family {
            cross_recording += usize::from(*cross);
            if witness.conflicting {
                conflicts += 1;
                conflicting.push(format!("recording{}:{request_id}", witness.first_source));
                conflicting.sort();
                conflicting.truncate(MAX_CONFLICT_EXAMPLES);
            }
            for (total, flag) in conflicting_counters
                .iter_mut()
                .zip(witness.conflicting_counters)
            {
                *total |= flag;
            }
            for (index, name) in COUNTERS.iter().enumerate() {
                partially_recorded[index] |= counter(&witness.tokens, name).is_none();
            }
            add_tokens(&mut union, &witness.tokens);
            revised += usize::from(witness.revised);
            if let Some(model) = &witness.model {
                let entry = identified_by_model.entry(model.clone()).or_default();
                add_tokens(&mut entry.0, &witness.tokens);
                entry.1 += 1;
            }
        }
        for (source, identified) in self.sources.iter_mut().zip(by_source) {
            source.identified = identified;
            source.complete &= !self.budget_exhausted;
        }
        if self.budget_exhausted {
            self.notes.push(format!(
                "The union holds at most {MAX_IDENTIFIED_REQUESTS} request identities; later ones were not counted."
            ));
        }
        let complete = namespace_established
            && !self.unenumerated
            && self.sources.iter().all(|source| source.complete);
        let mut unkeyed = None;
        for source in &self.sources {
            if let Some(tokens) = &source.unkeyed_tokens {
                add_tokens(&mut unkeyed, tokens);
            }
        }
        if !namespace_established {
            union = None;
            unkeyed = None;
            identified_by_model.clear();
        }
        let mut costs: BTreeMap<String, ModelUsage> = self
            .recorded_usage
            .as_ref()
            .and_then(|usage| usage.by_model.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|model| (model.model.clone(), model))
            .collect();
        let usage = self.recorded_usage.as_ref();
        // When the total was written, as far as a positive fact says.
        let written = self.recorded_at.or(self.recorded_after);
        if self.recorded.is_some() && self.last_child_request.is_some() && written.is_none() {
            self.notes.push(
                "The recorded total writes no timestamp and no timestamped parent record precedes \
                 it, so child requests cannot be ordered against it."
                    .to_owned(),
            );
        }
        let recorded = self.recorded.clone().map(|tokens| RecordedTotal {
            accounting: usage.unwrap().accounting.clone(),
            cost: usage.and_then(|usage| usage.cost.clone()),
            written_cost_usd: usage.and_then(|usage| usage.written_cost_usd),
            cost_coverage: usage.and_then(|usage| usage.cost_coverage.clone()),
            tokens,
            observations: self.recorded_observations,
            resets: self.recorded_resets,
            observed_at: self.recorded_at,
            after_last_request: match (self.recorded_position, self.last_parent_request) {
                (Some(total), Some(request)) => total > request,
                _ => true,
            } && !self.child_frontier_missing
                && !self.unenumerated
                && !self.sources.iter().any(|source| {
                    source.error.is_some()
                        || (source.role == SourceRole::Child && source.failed > 0)
                })
                && self
                    .last_child_request
                    .is_none_or(|request| written.is_some_and(|total| total >= request)),
        });
        let counters = COUNTERS
            .iter()
            .enumerate()
            .map(|(index, name)| {
                compare(
                    name,
                    recorded.as_ref(),
                    union.as_ref(),
                    complete && !partially_recorded[index],
                    conflicting_counters[index],
                    namespace_established,
                )
            })
            .collect();
        let mut by_model: BTreeMap<String, ModelReconciliation> = BTreeMap::new();
        for (model, tokens) in self.recorded_by_model {
            let cost = costs.remove(&model);
            by_model.insert(
                model.clone(),
                ModelReconciliation {
                    model,
                    cost: cost.as_ref().and_then(|model| model.cost.clone()),
                    cost_coverage: cost.and_then(|model| model.cost_coverage),
                    accounting: usage.map(|usage| usage.accounting.clone()),
                    recorded: Some(tokens),
                    identified: None,
                    requests: namespace_established.then_some(0),
                },
            );
        }
        for (model, cost) in costs {
            by_model.insert(
                model.clone(),
                ModelReconciliation {
                    model,
                    cost: cost.cost,
                    cost_coverage: cost.cost_coverage,
                    accounting: usage.map(|usage| usage.accounting.clone()),
                    recorded: cost.tokens,
                    identified: None,
                    requests: namespace_established.then_some(0),
                },
            );
        }
        for (model, (tokens, requests)) in identified_by_model {
            let row = by_model
                .entry(model.clone())
                .or_insert(ModelReconciliation {
                    model,
                    cost: None,
                    cost_coverage: None,
                    accounting: None,
                    recorded: None,
                    identified: None,
                    requests: namespace_established.then_some(0),
                });
            row.identified = tokens;
            row.requests = Some(requests);
        }
        conflicting.sort();
        conflicting.truncate(MAX_CONFLICT_EXAMPLES);
        let mut missing = vec!["recorded-total-membership"];
        if !namespace_established {
            missing.push("request-namespace");
        }
        if recorded
            .as_ref()
            .is_some_and(|total| !total.after_last_request)
        {
            missing.push("comparison-frontier");
        }
        let family_scope = self.sources.len() > 1;
        Reconciliation {
            rules: RECONCILIATION_RULES,
            sources: self.sources,
            recorded,
            identified: IdentifiedRequests {
                identity_coverage: if namespace_established && family_scope {
                    "validated-family"
                } else if namespace_established {
                    "recording-local"
                } else {
                    "unestablished"
                },
                requests: namespace_established.then_some(family.len()),
                cross_recording: namespace_established.then_some(cross_recording),
                revised,
                conflicts,
                conflicting,
                tokens: union,
                unkeyed,
                complete,
            },
            counters,
            by_model: by_model.into_values().collect(),
            scope_established: false,
            missing,
            notes: self.notes,
        }
    }
}

/// Compare one counter. The difference is computed only within one counter
/// epoch, at a frontier covering every source, with an established identity
/// namespace, and where every witness of a request agrees on the counter.
/// The union is complete for a counter when every source read whole and
/// every identified request recorded it. A
/// negative difference is a conflict even over an incomplete union, since
/// completing it only grows it.
fn compare(
    name: &'static str,
    recorded: Option<&RecordedTotal>,
    union: Option<&Tokens>,
    complete: bool,
    conflicting: bool,
    namespace_established: bool,
) -> CounterReconciliation {
    let recorded_value = recorded.and_then(|total| counter(&total.tokens, name));
    let identified = union.and_then(|tokens| counter(tokens, name));
    let mut reasons = Vec::new();
    if !namespace_established {
        reasons.push("request-namespace");
    }
    if recorded.is_none() {
        reasons.push("no-recorded-total");
    } else if recorded_value.is_none() || (namespace_established && identified.is_none()) {
        reasons.push("counter-unrecorded");
    }
    if recorded.is_some_and(|total| total.resets > 0) {
        reasons.push("counter-reset");
    }
    if recorded.is_some_and(|total| !total.after_last_request) {
        reasons.push("frontier");
    }
    if conflicting {
        reasons.push("request-conflict");
    }
    let withheld = |reasons| CounterReconciliation {
        counter: name,
        recorded: recorded_value,
        identified,
        difference: None,
        status: CounterStatus::Withheld,
        reasons,
    };
    if !reasons.is_empty() {
        return withheld(reasons);
    }
    let (Some(total), Some(union)) = (recorded_value, identified) else {
        return withheld(reasons);
    };
    let difference = i128::from(total) - i128::from(union);
    let status = if difference < 0 {
        CounterStatus::Conflict
    } else if !complete {
        reasons.push("incomplete-union");
        CounterStatus::Discrepancy
    } else if difference == 0 {
        CounterStatus::Reconciled
    } else {
        reasons.push("recorded-total-membership");
        CounterStatus::Discrepancy
    };
    CounterReconciliation {
        counter: name,
        recorded: recorded_value,
        identified,
        difference: Some(difference),
        status,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(output: u64, cache_read: u64) -> Tokens {
        Tokens {
            input: Some(1),
            output: Some(output),
            reasoning: Some(0),
            cache_read: Some(cache_read),
            cache_write: Some(0),
        }
    }

    #[test]
    fn a_later_dominant_witness_discharges_provisional_conflicts() {
        let mut ledger = FamilyLedger::default();
        let source = ledger.source(SourceRole::Parent, None, "fixture".to_owned());
        ledger.request(
            source,
            1,
            Some("request"),
            Some("model"),
            tokens(5, 10),
            None,
        );
        ledger.request(
            source,
            2,
            Some("request"),
            Some("model"),
            tokens(10, 5),
            None,
        );
        ledger.request(
            source,
            3,
            Some("request"),
            Some("model"),
            tokens(10, 10),
            None,
        );
        ledger.read(
            source,
            "fixture-revision".to_owned(),
            1,
            "fixture-digest".to_owned(),
            0,
        );
        let report = ledger.finish();
        assert_eq!(report.identified.conflicts, 0);
        assert!(report.identified.conflicting.is_empty());
        assert_eq!(report.identified.tokens, Some(tokens(10, 10)));
    }

    #[test]
    fn family_deduplication_keeps_each_recordings_conflict_evidence() {
        for dominant in [false, true] {
            for conflicting_child in [false, true] {
                let mut ledger = FamilyLedger::default();
                let parent = ledger.source(SourceRole::Parent, None, "parent".to_owned());
                let child = ledger.source(SourceRole::Child, None, "child".to_owned());
                ledger.validated_child(child);
                let (conflicting, other) = if conflicting_child {
                    (child, parent)
                } else {
                    (parent, child)
                };
                for (position, counters) in [tokens(5, 10), tokens(10, 5)].into_iter().enumerate() {
                    ledger.request(conflicting, position, Some("request"), None, counters, None);
                }
                ledger.request(
                    other,
                    0,
                    Some("request"),
                    None,
                    tokens(10, if dominant { 10 } else { 5 }),
                    None,
                );
                let report = ledger.finish();
                assert_eq!(report.identified.identity_coverage, "validated-family");
                assert_eq!(report.identified.requests, Some(1));
                assert_eq!(report.identified.cross_recording, Some(1));
                assert_eq!(report.identified.conflicts, usize::from(!dominant));
                if dominant {
                    assert_eq!(report.identified.tokens, Some(tokens(10, 10)));
                }
                assert_eq!(report.sources[parent].requests, 1);
                assert_eq!(report.sources[child].requests, 1);
            }
        }
    }

    #[test]
    fn an_additional_parent_cannot_join_the_family_namespace() {
        let mut ledger = FamilyLedger::default();
        for _ in 0..2 {
            let source = ledger.source(SourceRole::Parent, None, "recording".to_owned());
            ledger.validated_child(source);
            ledger.request(source, 0, Some("request"), None, tokens(10, 10), None);
        }
        let report = ledger.finish();
        assert_eq!(report.identified.identity_coverage, "unestablished");
        assert_eq!(report.identified.requests, None);
        assert_eq!(report.identified.tokens, None);
        assert!(report.missing.contains(&"request-namespace"));
    }

    #[test]
    fn cumulative_observations_keep_constant_resident_state() {
        let mut ledger = FamilyLedger::default();
        for position in 0..2048 {
            ledger.recorded(
                position,
                tokens(position as u64, 0),
                BTreeMap::new(),
                RecordedUsage {
                    accounting: Accounting {
                        basis: crate::model::AccountingBasis::RecordedTotal,
                        coverage: crate::model::AccountingCoverage::Session,
                        selection: None,
                    },
                    tokens: Some(tokens(position as u64, 0)),
                    cost: None,
                    written_cost_usd: None,
                    cost_coverage: None,
                    by_model: None,
                },
                None,
            );
        }
        assert_eq!(ledger.recorded.iter().count(), 1);
        let report = ledger.finish();
        assert_eq!(report.recorded.unwrap().observations, 2048);
    }
}
