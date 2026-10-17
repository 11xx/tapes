use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{pi_consumed, pi_parent_id, pi_unread_usage, PiBackend, PiConsumed, PiConsumption};
use crate::backend::{
    read_recording, read_recording_at, stream_jsonl, validate_native_identity, ReadPin,
};
use crate::measurement::{Measured, MeasurementCompleteness, MeasurementReason};
use crate::model::{
    AccountingBasis, AccountingCoverage, Cost, CostCoverage, DisjointTokens, ReadEvidence, Session,
    TokenConvention, Tokens,
};
use crate::usage::{
    ConsumptionCopyCoverage, ConsumptionEventKey, ConsumptionEventKeys,
    ConsumptionEventMeasurement, ConsumptionKeyKind, ConsumptionKeyOmissions,
    ConsumptionKeyOptions, ConsumptionRecordingEntry,
};

const MAX_KEY_BYTES: usize = 8 * 1024 * 1024;

#[derive(PartialEq, Eq)]
enum ForkSource {
    Absent,
    Named(String),
    Unestablished,
}

impl ForkSource {
    fn observed(header: &Value) -> Self {
        match header.get("parentSession") {
            None => Self::Absent,
            Some(value) => value
                .as_str()
                .filter(|id| !id.is_empty())
                .map_or(Self::Unestablished, |reference| {
                    Self::Named(reference.to_owned())
                }),
        }
    }
}

pub(super) fn read_keys(
    backend: &PiBackend,
    session: &Session,
    evidence: Option<&ReadEvidence>,
    options: ConsumptionKeyOptions,
) -> Result<ConsumptionEventKeys> {
    if !(1..=10_000).contains(&options.limit) {
        anyhow::bail!("consumption event keys must be between 1 and 10000");
    }
    Ok(
        read_keys_at(backend, session, evidence, options).unwrap_or_else(|_| {
            let observed = session
                .usage_detail
                .as_ref()
                .and_then(|detail| detail.consumption.as_ref())
                .map(|consumption| consumption.events.counted);
            ConsumptionEventKeys {
                session_id: public_id(&session.id).then(|| session.id.clone()),
                forked_from: None,
                basis: AccountingBasis::SummedRequests,
                keys: Vec::new(),
                observed,
                returned: 0,
                omissions: ConsumptionKeyOmissions {
                    unavailable_source: observed.unwrap_or(0),
                    ..Default::default()
                },
                coverage: None,
                unreadable_records: None,
                unrecognized_usage: None,
                unavailable: Some("source-unreadable-or-changed"),
                copy_coverage: None,
                copy_conflicts: 0,
            }
        }),
    )
}

fn read_keys_at(
    backend: &PiBackend,
    session: &Session,
    evidence: Option<&ReadEvidence>,
    options: ConsumptionKeyOptions,
) -> Result<ConsumptionEventKeys> {
    let path = backend.recording(session)?;
    let mut buffer = KeyBuffer::new(options.limit);
    let mut consumed = PiConsumption::default();
    let mut parent = None;
    let mut unrecognized = 0;
    let full = evidence.is_some_and(|read| {
        read.projection_options
            .iter()
            .any(|option| option == "full")
    });
    let mut observe = |entry: &Value| -> Result<()> {
        validate_native_identity(entry, "pi", &session.id)?;
        if entry["type"] == "session" {
            let observed = ForkSource::observed(entry);
            parent = Some(match parent.take() {
                Some(previous) if previous != observed => ForkSource::Unestablished,
                _ => observed,
            });
        }
        unrecognized += usize::from(pi_unread_usage(entry));
        if let Some(event) = pi_consumed(entry) {
            let id = entry["id"].as_str();
            if consumed.observe(id, &event, false) {
                buffer.observe(&session.id, entry, &event);
            }
        }
        Ok(())
    };
    let (coverage, unreadable) = if full {
        let evidence = evidence.unwrap();
        let pin =
            ReadPin {
                length: evidence.source_length,
                revision: evidence
                    .source_revision
                    .as_deref()
                    .ok_or_else(|| anyhow!("the full usage read has no source revision"))?,
                prefix_sha256: Some(evidence.source_prefix_sha256.as_ref().ok_or_else(|| {
                    anyhow!("the full usage read has no prefix integrity evidence")
                })?),
            };
        let streamed = stream_jsonl(&path, Some(pin), |entry, _, _| {
            observe(entry)?;
            Ok(false)
        })?;
        (AccountingCoverage::Session, streamed.skipped)
    } else {
        let recording = if let Some(evidence) = evidence {
            read_recording_at(&path, evidence)?
        } else {
            read_recording(&path, backend.read_bytes)?
        };
        crate::backend::validate_recording_identity(&recording, "pi", &session.id)?;
        for entry in recording
            .opening()
            .iter()
            .filter(|entry| entry["type"] == "session")
            .chain(&recording.tail.values)
        {
            observe(entry)?;
        }
        (
            if recording.tail.truncated {
                AccountingCoverage::ReadWindow
            } else {
                AccountingCoverage::Session
            },
            recording.tail.skipped,
        )
    };
    let forked_from = match &parent {
        Some(ForkSource::Named(reference)) => pi_parent_id(reference),
        _ => None,
    };
    let (copy_coverage, copy_conflicts) = match parent {
        Some(ForkSource::Absent) => (None, 0),
        Some(ForkSource::Named(reference)) => {
            verify_copies(backend, session, Some(&reference), full, &mut buffer.keys)
        }
        _ => (Some(ConsumptionCopyCoverage::Unestablished), 0),
    };
    let (keys, omissions) = buffer.finish();
    Ok(ConsumptionEventKeys {
        session_id: public_id(&session.id).then(|| session.id.clone()),
        forked_from,
        basis: AccountingBasis::SummedRequests,
        observed: Some(consumed.events.counted),
        returned: keys.len(),
        keys,
        omissions,
        coverage: Some(coverage),
        unreadable_records: Some(unreadable),
        unrecognized_usage: Some(unrecognized),
        unavailable: None,
        copy_coverage,
        copy_conflicts,
    })
}

/// Portable identity fields exclude path syntax, whitespace and diagnostics.
fn public_id(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn entry_digest(entry: &Value, consumed: &PiConsumed) -> String {
    let facts = serde_json::json!([entry["type"], entry["timestamp"], consumed]);
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&facts).expect("normalized consumption facts serialize"))
    )
}

/// What one event measured: its disjoint categories and its price, each
/// absent where the entry did not record it, with the reasons it is not
/// complete. An entry that wrote a zero price beside usage is unpriced, and
/// a stopped request's amounts are partial.
fn event_measurement(consumed: &PiConsumed) -> ConsumptionEventMeasurement {
    let usage = &consumed.usage;
    let tokens = Tokens {
        input: usage.input,
        output: usage.output,
        reasoning: usage.reasoning,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
    };
    let unpriced = usage.is_unpriced();
    let cost = usage.cost.filter(|_| !unpriced).map(|usd| Cost { usd });
    let cost_coverage = cost.is_none().then_some(CostCoverage {
        complete: false,
        zero_written_requests: usize::from(unpriced && consumed.assistant_request),
        zero_written_observations: usize::from(unpriced),
    });
    let mut completeness = MeasurementCompleteness::assess(Measured {
        tokens: Some(&tokens),
        cost: cost.as_ref(),
        cost_coverage: cost_coverage.as_ref(),
        scope: Some(AccountingCoverage::Session),
        events: 1,
        ..Measured::default()
    });
    if consumed.stopped {
        completeness.qualify_all(MeasurementReason::StoppedAttempt);
    }
    let disjoint = TokenConvention::of("pi").map(|convention| {
        DisjointTokens::from_native(&tokens, convention, None).with_completeness(&completeness)
    });
    ConsumptionEventMeasurement {
        disjoint,
        cost,
        cost_coverage,
        measurement_completeness: completeness,
    }
}

struct KeyBuffer {
    limit: usize,
    bytes: usize,
    keys: VecDeque<ConsumptionEventKey>,
    omissions: ConsumptionKeyOmissions,
}

impl KeyBuffer {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            bytes: 0,
            keys: VecDeque::new(),
            omissions: ConsumptionKeyOmissions::default(),
        }
    }

    fn observe(&mut self, session_id: &str, entry: &Value, consumed: &PiConsumed) {
        let id = entry["id"].as_str().filter(|id| !id.is_empty());
        if id.is_none() && consumed.response.is_none() {
            self.omissions.unidentified += 1;
            return;
        }
        if !public_id(session_id)
            || id.is_some_and(|id| !public_id(id))
            || consumed
                .response
                .as_ref()
                .is_some_and(|(provider, response)| !public_id(provider) || !public_id(response))
        {
            self.omissions.private_or_invalid_identity += 1;
            return;
        }
        let key = ConsumptionEventKey {
            kind: if consumed.response.is_some() {
                ConsumptionKeyKind::ProviderResponse
            } else {
                ConsumptionKeyKind::RecordingEntry
            },
            provider: consumed
                .response
                .as_ref()
                .map(|response| response.0.clone()),
            response_id: consumed
                .response
                .as_ref()
                .map(|response| response.1.clone()),
            provider_account_namespace: consumed.response.as_ref().map(|_| "unestablished"),
            recording_entry: id.map(|id| ConsumptionRecordingEntry {
                session_id: session_id.to_owned(),
                entry_id: id.to_owned(),
            }),
            entry_digest: id.map(|_| entry_digest(entry, consumed)),
            copy_of: None,
            measurement: Some(event_measurement(consumed)),
        };
        self.push(key);
    }

    fn push(&mut self, key: ConsumptionEventKey) {
        let size = key_size(&key);
        if size > MAX_KEY_BYTES {
            self.omissions.oversized_key += 1;
            return;
        }
        while self.keys.len() >= self.limit {
            let old = self.keys.pop_front().unwrap();
            self.bytes -= key_size(&old);
            self.omissions.row_cap += 1;
        }
        while self.bytes + size > MAX_KEY_BYTES {
            let old = self.keys.pop_front().unwrap();
            self.bytes -= key_size(&old);
            self.omissions.byte_budget += 1;
        }
        self.bytes += size;
        self.keys.push_back(key);
    }

    /// Copy annotations also belong to the serialized byte budget.
    fn finish(mut self) -> (Vec<ConsumptionEventKey>, ConsumptionKeyOmissions) {
        let mut keys = Vec::new();
        let mut bytes = 0;
        while let Some(key) = self.keys.pop_back() {
            let size = key_size(&key);
            if size > MAX_KEY_BYTES {
                self.omissions.oversized_key += 1;
            } else if bytes + size > MAX_KEY_BYTES {
                self.omissions.byte_budget += 1;
            } else {
                bytes += size;
                keys.push(key);
            }
        }
        keys.reverse();
        (keys, self.omissions)
    }
}

fn key_size(key: &ConsumptionEventKey) -> usize {
    serde_json::to_vec(key)
        .expect("consumption identities serialize")
        .len()
}

fn verify_copies(
    backend: &PiBackend,
    session: &Session,
    reference: Option<&str>,
    full: bool,
    keys: &mut VecDeque<ConsumptionEventKey>,
) -> (Option<ConsumptionCopyCoverage>, usize) {
    use ConsumptionCopyCoverage::{
        ReadWindow, Session as Whole, Unavailable, Unestablished, Unreadable,
    };
    let Some(reference) = reference else {
        return (None, 0);
    };
    let Some(parent) = pi_parent_id(reference).filter(|parent| parent != &session.id) else {
        return (Some(Unestablished), 0);
    };
    let Some(store) = &backend.store else {
        return (Some(Unavailable), 0);
    };
    let native = match store.locate_exact(&parent) {
        Ok(Some(native)) => native,
        Ok(None) => return (Some(Unavailable), 0),
        Err(_) => return (Some(Unreadable), 0),
    };
    let Some(path) = native.locator() else {
        return (Some(Unavailable), 0);
    };
    if reference != parent {
        let same = path
            .canonicalize()
            .ok()
            .zip(Path::new(reference).canonicalize().ok())
            .is_some_and(|(located, referenced)| located == referenced);
        if !same {
            return (Some(Unestablished), 0);
        }
    }
    let wanted = keys
        .iter()
        .filter_map(|key| {
            Some((
                key.recording_entry.as_ref()?.entry_id.clone(),
                key.entry_digest.as_ref()?.clone(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut matches = BTreeMap::<String, bool>::new();
    let mut observe = |entry: &Value| -> Result<()> {
        validate_native_identity(entry, "pi", &parent)?;
        if let Some(id) = entry["id"].as_str() {
            if let Some(expected) = wanted.get(id) {
                if let Some(consumed) = pi_consumed(entry) {
                    let same = entry_digest(entry, &consumed) == *expected;
                    matches
                        .entry(id.to_owned())
                        .and_modify(|matched| *matched &= same)
                        .or_insert(same);
                }
            }
        }
        Ok(())
    };
    let read = if full {
        stream_jsonl(path, None, |entry, _, _| {
            observe(entry)?;
            Ok(false)
        })
        .map(|read| (Whole, read.skipped))
    } else {
        read_recording(path, backend.read_bytes).and_then(|read| {
            crate::backend::validate_recording_identity(&read, "pi", &parent)?;
            for entry in &read.tail.values {
                observe(entry)?;
            }
            Ok((
                if read.tail.truncated {
                    ReadWindow
                } else {
                    Whole
                },
                read.tail.skipped,
            ))
        })
    };
    let Ok((coverage, unreadable)) = read else {
        return (Some(Unreadable), 0);
    };
    let conflicts = matches.values().filter(|same| !**same).count();
    for key in keys {
        if let Some(entry) = &key.recording_entry {
            if matches.get(&entry.entry_id) == Some(&true) {
                key.copy_of = Some(ConsumptionRecordingEntry {
                    session_id: parent.clone(),
                    entry_id: entry.entry_id.clone(),
                });
            }
        }
    }
    (
        Some(if unreadable > 0 { Unreadable } else { coverage }),
        conflicts,
    )
}
