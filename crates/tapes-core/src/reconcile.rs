//! Reconciling a recorded usage total with the requests a recording family
//! identifies.
//!
//! A harness's cumulative total and the request records beside it are two
//! observations whose scopes need not match: the total may count children,
//! background calls, or a counter epoch the records do not. The report keeps
//! both as source facts, deduplicates requests by their provider identity
//! across every recording read, and subtracts per counter only where the
//! subtraction is defined. A difference is signed and never clamped; a
//! positive one is a discrepancy until the total's membership is established.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::model::Tokens;

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

/// One recording the reconciliation read whole. `revision`, `length`, and
/// `sha256` identify the exact bytes read. `records` counts request records
/// with usage; `requests` the distinct identities first witnessed here, whose
/// counters `identified` sums, so the sources' `identified` partition the
/// union. `unkeyed` counts records with usage and no request identity,
/// `unrequested` records the harness marks as answered without a request,
/// and `failed` records the harness wrote in place of a response that failed,
/// whose consumption is unknown. A source is `complete` only when every
/// record decoded, none stands for a failed request, and the identity budget
/// held.
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
    pub unrequested: usize,
    #[serde(skip_serializing_if = "is_zero")]
    pub failed: usize,
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
/// newest observation follows every parent request record.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordedTotal {
    pub tokens: Tokens,
    pub observations: usize,
    pub resets: usize,
    pub after_last_request: bool,
}

/// The deduplicated union of identified requests across every source read.
/// A request is identified by its provider request id; one witnessed by more
/// than one record is one request, and `cross_recording` counts those
/// witnessed in more than one recording. Of a request's witnesses, the one
/// that dominates every other on each counter is its usage: `revised` counts
/// requests whose witnesses differed that way. `conflicts` counts requests
/// with two witnesses neither of which dominates; `tokens` keeps the first of
/// those. `unkeyed` sums the records with no identity, which are measured but
/// cannot be deduplicated.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IdentifiedRequests {
    pub requests: usize,
    pub cross_recording: usize,
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
    pub recorded: Option<Tokens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identified: Option<Tokens>,
    pub requests: usize,
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

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// What one request identity's witnesses established.
struct Witness {
    tokens: Tokens,
    model: Option<String>,
    first_source: usize,
    cross_recording: bool,
    revised: bool,
    conflicting: bool,
}

/// Request witnesses and recorded totals gathered across a family's sources.
#[derive(Default)]
pub(crate) struct FamilyLedger {
    sources: Vec<ReconciledSource>,
    requests: HashMap<String, Witness>,
    unkeyed: Option<Tokens>,
    conflicting_counters: [bool; COUNTERS.len()],
    conflicting: Vec<String>,
    conflicts: usize,
    budget_exhausted: bool,
    recorded: Vec<Tokens>,
    recorded_by_model: BTreeMap<String, Tokens>,
    recorded_position: Option<usize>,
    last_parent_request: Option<usize>,
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
            unrequested: 0,
            failed: 0,
            skipped: 0,
            identified: None,
            error: None,
        });
        self.sources.len() - 1
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
        entry.complete = skipped == 0 && entry.failed == 0 && !self.budget_exhausted;
    }

    pub(crate) fn failed(&mut self, source: usize, error: String) {
        let entry = &mut self.sources[source];
        entry.complete = false;
        entry.error = Some(error);
    }

    /// The family's children could not be listed, so the union cannot be
    /// complete.
    pub(crate) fn unenumerated(&mut self, error: String) {
        self.unenumerated = true;
        self.notes.push(error);
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
    ) {
        self.sources[source].records += 1;
        if self.sources[source].role == SourceRole::Parent {
            self.last_parent_request = Some(position);
        }
        let Some(request_id) = request_id else {
            self.sources[source].unkeyed += 1;
            add_tokens(&mut self.unkeyed, &tokens);
            return;
        };
        if let Some(witness) = self.requests.get_mut(request_id) {
            witness.cross_recording |= witness.first_source != source;
            if witness.tokens == tokens || witness.tokens.dominates(&tokens) {
                witness.revised |= witness.tokens != tokens;
            } else if tokens.dominates(&witness.tokens) {
                witness.tokens = tokens;
                witness.revised = true;
            } else {
                for (index, name) in COUNTERS.iter().enumerate() {
                    if counter(&witness.tokens, name) != counter(&tokens, name) {
                        self.conflicting_counters[index] = true;
                    }
                }
                if !witness.conflicting {
                    witness.conflicting = true;
                    self.conflicts += 1;
                    if self.conflicting.len() < MAX_CONFLICT_EXAMPLES {
                        self.conflicting.push(request_id.to_owned());
                    }
                }
            }
            return;
        }
        if self.requests.len() >= MAX_IDENTIFIED_REQUESTS {
            self.budget_exhausted = true;
            return;
        }
        self.sources[source].requests += 1;
        self.requests.insert(
            request_id.to_owned(),
            Witness {
                tokens,
                model: model.map(str::to_owned),
                first_source: source,
                cross_recording: false,
                revised: false,
                conflicting: false,
            },
        );
    }

    /// One cumulative total the parent recorded, with its per-model split.
    pub(crate) fn recorded(
        &mut self,
        position: usize,
        tokens: Tokens,
        by_model: BTreeMap<String, Tokens>,
    ) {
        self.recorded.push(tokens);
        self.recorded_by_model = by_model;
        self.recorded_position = Some(position);
    }

    pub(crate) fn finish(mut self) -> Reconciliation {
        let mut union = None;
        let mut cross_recording = 0;
        let mut revised = 0;
        let mut by_source: Vec<Option<Tokens>> = vec![None; self.sources.len()];
        let mut identified_by_model: BTreeMap<String, (Option<Tokens>, usize)> = BTreeMap::new();
        let mut partially_recorded = [false; COUNTERS.len()];
        for witness in self.requests.values() {
            for (index, name) in COUNTERS.iter().enumerate() {
                partially_recorded[index] |= counter(&witness.tokens, name).is_none();
            }
            add_tokens(&mut union, &witness.tokens);
            add_tokens(&mut by_source[witness.first_source], &witness.tokens);
            cross_recording += usize::from(witness.cross_recording);
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
        let complete = !self.unenumerated && self.sources.iter().all(|source| source.complete);
        let resets = self
            .recorded
            .windows(2)
            .filter(|pair| {
                COUNTERS.iter().any(|name| {
                    matches!(
                        (counter(&pair[0], name), counter(&pair[1], name)),
                        (Some(before), Some(after)) if after < before
                    )
                })
            })
            .count();
        let recorded = self.recorded.last().cloned().map(|tokens| RecordedTotal {
            tokens,
            observations: self.recorded.len(),
            resets,
            after_last_request: match (self.recorded_position, self.last_parent_request) {
                (Some(total), Some(request)) => total > request,
                _ => true,
            },
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
                    self.conflicting_counters[index],
                )
            })
            .collect();
        let mut by_model: BTreeMap<String, ModelReconciliation> = BTreeMap::new();
        for (model, tokens) in self.recorded_by_model {
            by_model.insert(
                model.clone(),
                ModelReconciliation {
                    model,
                    recorded: Some(tokens),
                    identified: None,
                    requests: 0,
                },
            );
        }
        for (model, (tokens, requests)) in identified_by_model {
            let row = by_model
                .entry(model.clone())
                .or_insert(ModelReconciliation {
                    model,
                    recorded: None,
                    identified: None,
                    requests: 0,
                });
            row.identified = tokens;
            row.requests = requests;
        }
        let mut conflicting = self.conflicting;
        conflicting.sort();
        Reconciliation {
            rules: RECONCILIATION_RULES,
            sources: self.sources,
            recorded,
            identified: IdentifiedRequests {
                requests: self.requests.len(),
                cross_recording,
                revised,
                conflicts: self.conflicts,
                conflicting,
                tokens: union,
                unkeyed: self.unkeyed,
                complete,
            },
            counters,
            by_model: by_model.into_values().collect(),
            scope_established: false,
            missing: vec!["recorded-total-membership"],
            notes: self.notes,
        }
    }
}

/// Compare one counter. The difference is computed only within one counter
/// epoch, at a frontier past every parent request, and where every witness of
/// a request agrees on the counter. The union is complete for a counter when
/// every source read whole and every identified request recorded it. A
/// negative difference is a conflict even over an incomplete union, since
/// completing it only grows it.
fn compare(
    name: &'static str,
    recorded: Option<&RecordedTotal>,
    union: Option<&Tokens>,
    complete: bool,
    conflicting: bool,
) -> CounterReconciliation {
    let recorded_value = recorded.and_then(|total| counter(&total.tokens, name));
    let identified = union.and_then(|tokens| counter(tokens, name));
    let mut reasons = Vec::new();
    if recorded.is_none() {
        reasons.push("no-recorded-total");
    } else if recorded_value.is_none() || identified.is_none() {
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
