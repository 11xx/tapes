use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use agent_tapes_discovery::{NativeSession, NativeStore};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{
    accounting_for, attach_record_refs, first_prompt_in_file, marker_of, read_recording,
    replay_pin, stream_jsonl, timestamp, transcript_from_recording, ActivityRange, Backend,
    FoldedSession, Listing, Query, StreamCoordinates, StreamOptions, StreamedTranscript,
    TokenTotals, UnmappedTally, DEFAULT_READ_BYTES,
};
use crate::antigravity_children::{detached_children, length_field};
use crate::content::{text_part, tool_coverage, tool_part};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::lineage::{ChildRef, Lineage, SourceRef};
use crate::measurement::{known_subtotal_notes, MeasurementCompleteness, MeasurementReason};
use crate::model::{
    AccountingBasis, AccountingCoverage, DisjointTokens, EntryMetadata, KindDeclaration, Model,
    Role, Session, SourceDescriptor, SourceKind, TokenConvention, Tokens, Transcript, Turn,
    TurnKind, TurnSelection,
};
use crate::model_observation::Selections;
use crate::pricing::RequestUses;
use crate::span::{Placer, SpanBounds, SpanSessions};
use crate::t3::query_sqlite;
use crate::usage::{ModelUsage, ObservationBasis, UsageAttribution, UsageDetail};

const HARNESS: &str = "antigravity";
const MAX_METADATA_BLOB_BYTES: u64 = 65_536;
const MAX_SEEN_STEPS: usize = 100_000;

const DIRECT_TOKEN_KEYS: [&str; 6] = [
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "thinking_tokens",
    "reasoning_tokens",
];

const UNRECOGNIZED_USAGE_KEYS: [&str; 11] = [
    "total_tokens",
    "usage",
    "usage_metadata",
    "usageMetadata",
    "token_usage",
    "tokenUsage",
    "cost",
    "cost_usd",
    "costUsd",
    "total_cost",
    "totalCost",
];

pub struct AntigravityBackend {
    store: NativeStore,
    read_bytes: u64,
    native_root: Option<PathBuf>,
}

impl AntigravityBackend {
    pub fn new(t3_home: impl Into<PathBuf>) -> Self {
        Self::from_store(NativeStore::antigravity(t3_home), DEFAULT_READ_BYTES)
    }

    pub(super) fn from_store(store: NativeStore, read_bytes: u64) -> Self {
        Self {
            store,
            read_bytes,
            native_root: None,
        }
    }

    pub fn with_read_bytes(mut self, read_bytes: u64) -> Self {
        self.read_bytes = read_bytes;
        self
    }

    pub(crate) fn with_native_root(mut self, root: PathBuf) -> Self {
        self.native_root = Some(root);
        self
    }

    fn source(&self, path: &Path, database: &Path) -> SourceDescriptor {
        let mut source = SourceDescriptor::installed(HARNESS, path.display().to_string());
        source.representation = "antigravity-acp-transcript".to_owned();
        if self.native_root.is_some() {
            source.kind = SourceKind::ExplicitNativeStore;
        }
        if let Some(location) = &mut source.location {
            location.native_path = Some(path.to_path_buf());
            location.container = Some(self.native_root.as_ref().map_or_else(
                || database.display().to_string(),
                |r| r.display().to_string(),
            ));
        }
        source
    }

    fn session(&self, identity: &NativeSession) -> Result<Session> {
        let database = identity
            .locator()
            .context("Antigravity identity has no database locator")?;
        let acp = database
            .parent()
            .and_then(Path::parent)
            .context("Antigravity database has no profile")?;
        let path = acp
            .join("brain")
            .join(identity.id())
            .join(".system_generated/logs/transcript_full.jsonl");
        check_recording_path(&path)?;
        let recording = read_recording(&path, self.read_bytes)?;
        let opening = recording.opening();
        anyhow::ensure!(
            opening.iter().chain(&recording.tail.values).any(|record| {
                record["step_index"].is_u64()
                    && record["source"].is_string()
                    && record["type"].is_string()
                    && record["status"].is_string()
            }),
            "Antigravity transcript has no readable native steps at {}",
            path.display()
        );
        let latest_model = recording.tail.values.iter().rev().find_map(record_model);
        let (model_observation, model_selections) = bounded_selections(&recording.tail);
        let mut activity = ActivityRange::default();
        for record in recording.head.iter().chain(&recording.tail.values) {
            activity.observe_timestamp(timestamp(&record["created_at"]));
        }
        let mut usage = AntigravityUsage::default();
        for record in &recording.tail.values {
            usage.observe_record(record, None);
        }
        let directory = workspace_directory(database);
        let mut session = Session {
            id: identity.id().to_owned(),
            source: self.source(&path, database),
            metadata: None,
            model: latest_model,
            model_observation,
            model_selections,
            launch: None,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory,
            started_at: None,
            last_activity_at: None,
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            start_uncertain: false,
            occurrence: None,
            usage_detail: None,
            list_price_estimate: None,
            t3: None,
        };
        activity.apply(&mut session);
        session.start_uncertain = recording.tail.truncated
            && !opening
                .iter()
                .any(|record| timestamp(&record["created_at"]).is_some());
        let coverage = if recording.tail.truncated {
            AccountingCoverage::ReadWindow
        } else {
            AccountingCoverage::Session
        };
        usage.apply(&mut session, coverage, recording.tail.skipped);
        Ok(
            session.with_derived_title_from(opening.iter().filter_map(|record| {
                (record["type"] == "USER_INPUT" && record["source"] == "USER_EXPLICIT")
                    .then(|| read_turns(record).into_iter().next())
                    .flatten()
            })),
        )
    }

    fn list_candidates(&self, query: &Query) -> Listing {
        let page = self
            .store
            .candidates_with_prefix(query.ceiling, query.id_prefix.as_deref());
        let mut listing = Listing {
            scanned: page.scanned,
            scan_truncated: !page.complete,
            unavailable_ids: page.unreadable_ids,
            unavailable: page
                .failures
                .into_iter()
                .map(|error| error.to_string())
                .collect(),
            ..Listing::default()
        };
        for native in page.records {
            if !query.admits_id(native.id()) {
                continue;
            }
            match self.session(&native) {
                Ok(session) => {
                    let placed = query.scope.is_none_or(|scope| {
                        session
                            .directory
                            .as_deref()
                            .is_some_and(|directory| scope.contains(directory))
                    });
                    if placed && query.matches(&session) {
                        listing.sessions.push(session);
                    }
                }
                Err(error) => {
                    listing.unavailable_ids.push(native.id().to_owned());
                    listing
                        .unavailable
                        .push(format!("Antigravity session {}: {error:#}", native.id()));
                }
            }
        }
        listing
            .sessions
            .sort_by(|a, b| crate::compare_sessions(a, b, query.sort));
        listing
    }

    fn t3_notes(&self, id: &str) -> Vec<String> {
        if self.native_root.is_some() {
            return Vec::new();
        }
        let Some(root) = self.store.root() else {
            return Vec::new();
        };
        crate::t3::T3Store::at(root)
            .antigravity_turn_notes(id)
            .unwrap_or_else(|error| {
                vec![format!(
                    "T3 Antigravity turn evidence unavailable: {error:#}"
                )]
            })
    }
}

impl Backend for AntigravityBackend {
    fn harness(&self) -> &'static str {
        HARNESS
    }

    fn accounting_bases(&self) -> &'static [AccountingBasis] {
        &[AccountingBasis::SummedRequests]
    }

    fn available(&self) -> bool {
        self.store.available()
    }

    fn terminal_exact_failure(&self) -> bool {
        true
    }

    fn kinds(&self) -> KindDeclaration {
        KindDeclaration {
            recordable: TurnSelection::only([
                TurnKind::Operator,
                TurnKind::Assistant,
                TurnKind::Reasoning,
                TurnKind::Tool,
                TurnKind::Notice,
            ]),
            user_default: None,
        }
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let mut listing = self.list_candidates(query);
        listing.sessions.truncate(query.limit);
        Ok(listing)
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        let mut listing =
            super::filter_listing_search(self, self.list_candidates(query), needle, tail);
        listing.sessions.truncate(query.limit);
        Ok(listing)
    }

    fn locate(&self, id: &str) -> Result<Option<Session>> {
        self.store
            .locate_exact(id)?
            .as_ref()
            .map(|native| self.session(native))
            .transpose()
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let path = recording_path(session)?;
        let recording = read_recording(path, self.read_bytes)?;
        let domain = format!("file:{}", path.display());
        let mut turns = Vec::new();
        let mut unmapped = UnmappedTally::default();
        let mut usage = AntigravityUsage::default();
        for (record, span) in recording.tail.values.iter().zip(&recording.tail.spans) {
            usage.observe_record(record, None);
            let mut parsed = read_and_tally(record, &mut unmapped);
            attach_record_refs(
                &mut parsed,
                &domain,
                Some(&recording.tail.source_revision),
                Some(*span),
            );
            turns.extend(parsed);
        }
        let mut updated = session.clone();
        updated.model = recording.tail.values.iter().rev().find_map(record_model);
        (updated.model_observation, updated.model_selections) = bounded_selections(&recording.tail);
        let mut activity = ActivityRange::default();
        for record in recording.head.iter().chain(&recording.tail.values) {
            activity.observe_timestamp(timestamp(&record["created_at"]));
        }
        activity.apply(&mut updated);
        let coverage = if recording.tail.truncated {
            AccountingCoverage::ReadWindow
        } else {
            AccountingCoverage::Session
        };
        usage.apply(&mut updated, coverage, recording.tail.skipped);
        let mut transcript = transcript_from_recording(
            updated,
            turns,
            tail,
            &recording,
            None,
            None,
            self.t3_notes(&session.id),
        );
        transcript.kinds = Some(self.kinds());
        if let Some(read) = &mut transcript.read {
            read.unmapped = Some(unmapped.finish());
        }
        Ok(transcript)
    }

    fn supports_full_search(&self) -> bool {
        true
    }

    fn first_prompt_markers(
        &self,
        sessions: &[&Session],
    ) -> Vec<Option<Result<crate::launch_parent::LaunchMarker>>> {
        sessions
            .iter()
            .map(|session| {
                let path = match recording_path(session) {
                    Ok(path) => path,
                    Err(error) => return Some(Err(error)),
                };
                Some(
                    first_prompt_in_file(path, HARNESS, &session.id, antigravity_prompt_text)
                        .map(|record| marker_of(record.as_ref(), antigravity_prompt_text)),
                )
            })
            .collect()
    }

    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let children = detached_children(&self.store, &session.id)?
            .into_iter()
            .map(|child| {
                let mut reference = ChildRef::new(child.id.clone(), HARNESS);
                reference.session_id = Some(child.id);
                reference.resolved = true;
                reference.source = vec![
                    SourceRef::File {
                        path: child.descriptor.display().to_string(),
                    },
                    SourceRef::Record {
                        native_id: child.task,
                    },
                ];
                reference
            })
            .collect();
        Ok(Lineage {
            children,
            ..Lineage::default()
        })
    }

    fn stream_lineage(&self, session: &Session) -> Result<crate::lineage::Lineage> {
        self.lineage(session)
    }

    fn stream_transcript_with(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        options: StreamOptions,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let path = recording_path(session)?;
        let domain = format!("file:{}", path.display());
        let mut whole = session.clone();
        whole.model = None;
        whole.model_observation = None;
        whole.model_selections = Vec::new();
        whole.tokens = None;
        whole.cost = None;
        whole.accounting = None;
        whole.usage_detail = None;
        whole.list_price_estimate = None;
        let mut activity = ActivityRange::default();
        let mut unmapped = UnmappedTally::default();
        let mut selections = Selections::default();
        let mut usage = AntigravityUsage::default();
        let mut placer = Placer::new(&[]).keeping(options.timeline);
        let mut prev_end: Option<u64> = None;
        let mut stamps = super::TurnStamps::new(options);
        let mut emit = super::timed(&mut stamps, turn);
        let read = stream_jsonl(path, replay_pin(replay)?, |record, span, revision| {
            if prev_end.is_some_and(|end| end != span.start) {
                selections.interrupt();
                placer.unreadable();
            }
            prev_end = Some(span.end);
            let ts = timestamp(&record["created_at"]);
            activity.observe_timestamp(ts);
            if let Some(model) = record_model(record) {
                let step_id = record["step_index"].as_u64().map(|idx| idx.to_string());
                selections.observe(model.clone(), ts, step_id.as_deref());
                whole.model = Some(model);
            }
            usage.observe_record(record, Some(&mut placer));
            let mut turns = read_and_tally(record, &mut unmapped);
            attach_record_refs(&mut turns, &domain, Some(revision), Some(span));
            let produced = !turns.is_empty();
            for turn in turns {
                emit(turn)?;
            }
            Ok(produced)
        })?;
        drop(emit);
        if read.skipped > 0 {
            selections.interrupt();
        }
        activity.apply(&mut whole);
        whole.start_uncertain = false;
        (whole.model_observation, whole.model_selections) = selections.finish(true);
        let unrecognized_usage = usage.unrecognized_usage;
        usage.apply(&mut whole, AccountingCoverage::Session, read.skipped);
        let placed = placer.finish();
        let (turn_times, requests, timeline_withheld) = stamps.finish(placed.timeline());
        let notes = replay.map_or_else(|| self.t3_notes(&session.id), |read| read.notes.clone());
        Ok(StreamedTranscript {
            coordinates: StreamCoordinates::FileBytes,
            source_length: read.source_length,
            source_bounds: Vec::new(),
            source_revision: Some(read.revision),
            source_prefix_sha256: Some(read.prefix_sha256),
            skipped: read.skipped,
            gaps: read.gaps,
            trailing_record: None,
            terminal: None,
            notes,
            unmapped: Some(unmapped.finish()),
            unrecognized_usage,
            kinds: Some(self.kinds()),
            folded: Some(Box::new(FoldedSession {
                asked: session.clone(),
                whole,
            })),
            turn_times,
            requests,
            timeline_withheld,
            locations: None,
        })
    }

    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        read.folded_for(session)
            .context("Antigravity stream belongs to a different session")
    }

    fn stream_spans(
        &self,
        session: &Session,
        read: &StreamedTranscript,
        spans: &[SpanBounds],
    ) -> Result<SpanSessions> {
        let path = recording_path(session)?;
        let mut records: Vec<(u64, SpanEvent)> = Vec::new();
        let mut activity = ActivityRange::default();
        let mut last_model = None;
        let mut selections = Selections::default();
        let mut prev_end: Option<u64> = None;
        let streamed = stream_jsonl(path, Some(read.pin()?), |record, span, _| {
            if prev_end.is_some_and(|end| end != span.start) {
                selections.interrupt();
            }
            prev_end = Some(span.end);
            let ts = timestamp(&record["created_at"]);
            activity.observe_timestamp(ts);
            if let Some(model) = record_model(record) {
                let step_id = record["step_index"].as_u64().map(|idx| idx.to_string());
                selections.observe(model.clone(), ts, step_id.as_deref());
                last_model = Some(model);
            }
            if let Some(event) = classify_usage(record) {
                records.push((span.start, event));
            }
            Ok(false)
        })?;
        if streamed.skipped > 0 {
            selections.interrupt();
        }
        let mut placer = Placer::new(spans);
        let mut whole_usage = AntigravityUsage::default();
        let mut per_figure = vec![AntigravityUsage::default(); placer.figures()];
        let mut gaps = streamed.gaps.iter().peekable();
        for (start, event) in records {
            while gaps.next_if(|gap| gap.span.start < start).is_some() {
                placer.unreadable();
            }
            let held = whole_usage.observe_event(event.clone(), Some(&mut placer));
            for index in held {
                per_figure[index].observe_event(event.clone(), None);
            }
        }
        for _ in gaps {
            placer.unreadable();
        }

        let mut base = session.clone();
        base.model = last_model;
        base.tokens = None;
        base.cost = None;
        base.accounting = None;
        base.usage_detail = None;
        base.list_price_estimate = None;
        base.start_uncertain = false;
        activity.apply(&mut base);
        (base.model_observation, base.model_selections) = selections.finish(true);

        let mut whole = base.clone();
        whole_usage.apply(&mut whole, AccountingCoverage::Session, streamed.skipped);

        let placed = placer.finish();
        let figures = per_figure
            .into_iter()
            .map(|usage| {
                let mut figure = base.clone();
                usage.apply(&mut figure, AccountingCoverage::Session, 0);
                crate::span::keep_identity(&mut figure, &whole);
                figure
            })
            .collect();
        Ok(SpanSessions::assemble(placed, figures, whole, Vec::new()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestRecord {
    step_index: Option<u64>,
    at: Option<DateTime<Utc>>,
    model: Option<Model>,
    tokens: Tokens,
    stopped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SpanEvent {
    Request(RequestRecord),
    FailedAttempt { at: Option<DateTime<Utc>> },
    UnrecognizedUsage { at: Option<DateTime<Utc>> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CounterParse {
    Absent,
    Valid(u64),
    Invalid,
}

fn parse_counter(record: &Value, key: &str) -> CounterParse {
    match record.get(key) {
        None | Some(Value::Null) => CounterParse::Absent,
        Some(value) => match value.as_u64() {
            Some(n) => CounterParse::Valid(n),
            None => CounterParse::Invalid,
        },
    }
}

fn has_any_usage_key(record: &Value) -> bool {
    let Some(map) = record.as_object() else {
        return false;
    };
    DIRECT_TOKEN_KEYS
        .iter()
        .chain(UNRECOGNIZED_USAGE_KEYS.iter())
        .any(|key| map.get(*key).is_some_and(|v| !v.is_null()))
}

fn has_unrecognized_usage_container(record: &Value) -> bool {
    let Some(map) = record.as_object() else {
        return false;
    };
    UNRECOGNIZED_USAGE_KEYS
        .iter()
        .any(|key| map.get(*key).is_some_and(|v| !v.is_null()))
}

fn parse_direct_tokens(record: &Value) -> Option<(Tokens, bool)> {
    let input = parse_counter(record, "input_tokens");
    let output = parse_counter(record, "output_tokens");
    let cache_read = parse_counter(record, "cache_read_tokens");
    let cache_write = parse_counter(record, "cache_write_tokens");
    let thinking = parse_counter(record, "thinking_tokens");
    let reasoning = parse_counter(record, "reasoning_tokens");
    if [input, output, cache_read, cache_write, thinking, reasoning]
        .contains(&CounterParse::Invalid)
    {
        return None;
    }
    let reasoning_val = match (thinking, reasoning) {
        (CounterParse::Valid(t), CounterParse::Valid(r)) if t != r => return None,
        (CounterParse::Valid(t), _) => Some(t),
        (_, CounterParse::Valid(r)) => Some(r),
        _ => None,
    };
    let val = |p: CounterParse| match p {
        CounterParse::Valid(v) => Some(v),
        _ => None,
    };
    let tokens = Tokens {
        input: val(input),
        output: val(output),
        reasoning: reasoning_val,
        cache_read: val(cache_read),
        cache_write: val(cache_write),
    };
    let any_present = tokens.input.is_some()
        || tokens.output.is_some()
        || tokens.reasoning.is_some()
        || tokens.cache_read.is_some()
        || tokens.cache_write.is_some();
    Some((tokens, any_present))
}

fn classify_usage(record: &Value) -> Option<SpanEvent> {
    let typ = record["type"].as_str();
    let source = record["source"].as_str();
    let status = record["status"].as_str();
    let at = timestamp(&record["created_at"]);
    let step_index = record["step_index"].as_u64();

    match (typ, source) {
        (Some("PLANNER_RESPONSE"), Some("MODEL")) => {
            if has_unrecognized_usage_container(record) {
                return Some(SpanEvent::UnrecognizedUsage { at });
            }
            let Some((tokens, any_present)) = parse_direct_tokens(record) else {
                return Some(SpanEvent::UnrecognizedUsage { at });
            };
            let model = record_model(record);
            if status == Some("DONE") {
                Some(SpanEvent::Request(RequestRecord {
                    step_index,
                    at,
                    model,
                    tokens,
                    stopped: false,
                }))
            } else if any_present {
                Some(SpanEvent::Request(RequestRecord {
                    step_index,
                    at,
                    model,
                    tokens,
                    stopped: true,
                }))
            } else {
                Some(SpanEvent::FailedAttempt { at })
            }
        }
        (Some("CHECKPOINT"), Some("SYSTEM")) => {
            if !has_any_usage_key(record) {
                return None;
            }
            if has_unrecognized_usage_container(record) || status != Some("DONE") {
                return Some(SpanEvent::UnrecognizedUsage { at });
            }
            let Some((tokens, any_present)) = parse_direct_tokens(record) else {
                return Some(SpanEvent::UnrecognizedUsage { at });
            };
            if !any_present {
                return Some(SpanEvent::UnrecognizedUsage { at });
            }
            Some(SpanEvent::Request(RequestRecord {
                step_index,
                at,
                model: record_model_field(record),
                tokens,
                stopped: false,
            }))
        }
        _ => has_any_usage_key(record).then_some(SpanEvent::UnrecognizedUsage { at }),
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ModelKey {
    model: String,
    variant: Option<String>,
}

#[derive(Clone, Default)]
struct ModelAcc {
    tokens: TokenTotals,
    requests: usize,
    stopped: bool,
}

#[derive(Clone, Default)]
struct AntigravityUsage {
    totals: TokenTotals,
    by_model: BTreeMap<ModelKey, ModelAcc>,
    unattributed_totals: TokenTotals,
    observed: usize,
    counted: usize,
    attributed: usize,
    unattributed: usize,
    stopped_with_usage: usize,
    request_failures: usize,
    unrecognized_usage: usize,
    conflicting_steps: usize,
    step_bound_reached: bool,
    seen_steps: HashMap<u64, (Tokens, Option<Model>)>,
    uses: RequestUses,
}

impl AntigravityUsage {
    fn observe_record(&mut self, record: &Value, placer: Option<&mut Placer>) -> Vec<usize> {
        let Some(event) = classify_usage(record) else {
            return Vec::new();
        };
        self.observe_event(event, placer)
    }

    fn observe_event(&mut self, event: SpanEvent, placer: Option<&mut Placer>) -> Vec<usize> {
        match event {
            SpanEvent::UnrecognizedUsage { at } => {
                self.unrecognized_usage += 1;
                placer.map_or_else(Vec::new, |p| {
                    p.record(at, MeasurementReason::UnrecognizedUsage)
                })
            }
            SpanEvent::FailedAttempt { at } => {
                self.request_failures += 1;
                placer.map_or_else(Vec::new, |p| {
                    p.record(at, MeasurementReason::StoppedAttempt)
                })
            }
            SpanEvent::Request(req) => {
                self.observed += 1;
                if let Some(step) = req.step_index {
                    if let Some(prior) = self.seen_steps.get(&step) {
                        if *prior == (req.tokens.clone(), req.model.clone()) {
                            return Vec::new();
                        }
                        self.conflicting_steps += 1;
                        return placer.map_or_else(Vec::new, |p| {
                            p.record(req.at, MeasurementReason::Undetermined)
                        });
                    }
                    if self.seen_steps.len() < MAX_SEEN_STEPS {
                        self.seen_steps
                            .insert(step, (req.tokens.clone(), req.model.clone()));
                    } else {
                        self.step_bound_reached = true;
                    }
                }
                self.counted += 1;
                if req.stopped {
                    self.stopped_with_usage += 1;
                }
                self.totals.add(
                    req.tokens.input,
                    req.tokens.output,
                    req.tokens.reasoning,
                    req.tokens.cache_read,
                    req.tokens.cache_write,
                );
                if let Some(convention) = TokenConvention::of(HARNESS) {
                    let disjoint = DisjointTokens::from_native(&req.tokens, convention, None);
                    self.uses.push(req.at, req.model.as_ref(), &disjoint);
                } else {
                    self.uses.discard();
                }
                match &req.model {
                    Some(model) => {
                        self.attributed += 1;
                        let entry = self
                            .by_model
                            .entry(ModelKey {
                                model: model.id.clone(),
                                variant: model.variant.clone(),
                            })
                            .or_default();
                        entry.tokens.add(
                            req.tokens.input,
                            req.tokens.output,
                            req.tokens.reasoning,
                            req.tokens.cache_read,
                            req.tokens.cache_write,
                        );
                        entry.requests += 1;
                        entry.stopped |= req.stopped;
                    }
                    None => {
                        self.unattributed += 1;
                        self.unattributed_totals.add(
                            req.tokens.input,
                            req.tokens.output,
                            req.tokens.reasoning,
                            req.tokens.cache_read,
                            req.tokens.cache_write,
                        );
                    }
                }
                let step_id = req.step_index.map(|idx| idx.to_string());
                placer.map_or_else(Vec::new, |p| p.request(req.at, step_id.as_deref()))
            }
        }
    }

    fn apply(self, session: &mut Session, coverage: AccountingCoverage, skipped: usize) {
        let (tokens, measurement_coverage) = self.totals.finish_covered();
        session.accounting = accounting_for(
            tokens.as_ref(),
            None,
            &measurement_coverage,
            AccountingBasis::SummedRequests,
            coverage,
        );
        session.tokens = tokens;
        session.cost = None;

        let by_model = (!self.by_model.is_empty()).then(|| {
            self.by_model
                .into_iter()
                .map(|(key, acc)| {
                    let (tokens, cov) = acc.tokens.finish_covered();
                    let incomplete = known_subtotal_notes(&cov);
                    let mut completeness = MeasurementCompleteness::default();
                    if acc.stopped {
                        completeness.qualify_all(MeasurementReason::StoppedAttempt);
                    }
                    ModelUsage {
                        list_price_estimate: None,
                        cache_write_by_ttl: None,
                        measurement_completeness: completeness,
                        model: key.model,
                        variant: key.variant,
                        tokens,
                        cost: None,
                        cost_coverage: None,
                        request_count: Some(acc.requests),
                        measurement_coverage: cov,
                        measurement_incomplete: incomplete,
                    }
                })
                .collect::<Vec<_>>()
        });

        let attribution = (self.unattributed > 0).then(|| {
            let (unattributed_tokens, unattributed_coverage) =
                self.unattributed_totals.finish_covered();
            UsageAttribution {
                basis: ObservationBasis::UsageRecord,
                coverage,
                observed: self.observed,
                counted: self.counted,
                attributed: self.attributed,
                unattributed: self.unattributed,
                leading_uncounted: 0,
                resets: 0,
                outside_total: 0,
                outside_total_tokens: None,
                outside_total_coverage: BTreeMap::new(),
                unattributed_tokens,
                unattributed_coverage,
                unattributed_completeness: MeasurementCompleteness::default(),
                incomplete: vec!["unattributed".to_owned()],
                bounds: Vec::new(),
            }
        });

        let mut completeness = MeasurementCompleteness::default();
        if self.stopped_with_usage > 0 {
            completeness.qualify_all(MeasurementReason::StoppedAttempt);
        }
        if self.conflicting_steps > 0 || self.step_bound_reached {
            completeness.qualify_all(MeasurementReason::Undetermined);
        }
        if skipped > 0 {
            completeness.qualify_all(MeasurementReason::UnreadableRecord);
        }

        let mut accounting_notes = Vec::new();
        if self.conflicting_steps > 0 {
            accounting_notes.push(format!(
                "{} Antigravity usage step(s) carried conflicting witnesses for the same step_index and were not summed.",
                self.conflicting_steps
            ));
        }
        if self.step_bound_reached {
            accounting_notes.push(format!(
                "Antigravity step deduplication reached the {MAX_SEEN_STEPS}-step bound."
            ));
        }

        let measurement_incomplete = known_subtotal_notes(&measurement_coverage);
        session.usage_detail = UsageDetail {
            by_model,
            attribution,
            measurement_coverage,
            measurement_incomplete,
            measurement_completeness: completeness,
            request_failures: self.request_failures,
            unrecognized_usage: self.unrecognized_usage,
            accounting_notes,
            request_uses: Some(self.uses).filter(|uses| !uses.is_empty()),
            ..UsageDetail::default()
        }
        .into_option();
    }
}

fn bounded_selections(
    read: &super::Jsonl,
) -> (
    Option<crate::model::ModelObservationStatus>,
    Vec<crate::model::ModelSelectionSpan>,
) {
    let mut selections = Selections::default();
    for (index, record) in read.values.iter().enumerate() {
        if index > 0 && read.spans[index - 1].end != read.spans[index].start {
            selections.interrupt();
        }
        if let Some(model) = record_model(record) {
            let step_id = record["step_index"].as_u64().map(|idx| idx.to_string());
            selections.observe(model, timestamp(&record["created_at"]), step_id.as_deref());
        }
    }
    if read.skipped > 0 {
        selections.interrupt();
    }
    selections.finish(!read.truncated)
}

fn workspace_directory(database: &Path) -> Option<PathBuf> {
    let bound = MAX_METADATA_BLOB_BYTES * 2 + 256;
    let rows = query_sqlite(
        database,
        &format!(
            "SELECT hex(data) AS data FROM trajectory_metadata_blob
             WHERE id = 'main' AND typeof(data) = 'blob' AND length(data) <= {MAX_METADATA_BLOB_BYTES}
             LIMIT 1"
        ),
        bound,
    )
    .ok()?;
    let [row] = rows.as_slice() else {
        return None;
    };
    let hex = row["data"].as_str()?;
    let bytes = decode_hex(hex)?;
    let raw_uri = length_field(&bytes, 7)
        .or_else(|| length_field(&bytes, 1).and_then(|ws| length_field(ws, 1)))?;
    let uri = std::str::from_utf8(raw_uri).ok()?;
    file_uri_to_path(uri)
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let encoded = rest.strip_prefix("localhost").unwrap_or(rest);
    if !encoded.starts_with('/') {
        return None;
    }
    let decoded = percent_decode(encoded)?;
    (!decoded.is_empty()).then(|| PathBuf::from(decoded))
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn recording_path(session: &Session) -> Result<&Path> {
    let path = super::session_file_path(session)
        .context("Antigravity session has no transcript locator")?;
    check_recording_path(path)?;
    Ok(path)
}

fn check_recording_path(path: &Path) -> Result<()> {
    // Check the brain path through its profile before opening any content.
    for directory in path.ancestors().skip(1).take(5) {
        let metadata = std::fs::symlink_metadata(directory)
            .with_context(|| format!("Antigravity transcript unavailable at {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_dir(),
            "Antigravity transcript directory is not a native directory: {}",
            directory.display()
        );
    }
    let info = std::fs::symlink_metadata(path)
        .with_context(|| format!("Antigravity transcript unavailable at {}", path.display()))?;
    anyhow::ensure!(
        info.is_file(),
        "Antigravity transcript is not a regular native file: {}",
        path.display()
    );
    Ok(())
}

fn antigravity_prompt_text(record: &Value) -> Option<String> {
    (record["type"] == "USER_INPUT" && record["source"] == "USER_EXPLICIT")
        .then(|| record["content"].as_str().map(str::to_owned))
        .flatten()
}

fn record_model(record: &Value) -> Option<Model> {
    (record["type"] == "PLANNER_RESPONSE")
        .then(|| record_model_field(record))
        .flatten()
}

fn record_model_field(record: &Value) -> Option<Model> {
    record["model"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(|id| Model {
            id: id.to_owned(),
            variant: None,
        })
}

fn read_and_tally(record: &Value, tally: &mut UnmappedTally) -> Vec<Turn> {
    let parsed = read_turns(record);
    if parsed.is_empty() {
        let kind = match record["type"].as_str() {
            Some("SYSTEM_MESSAGE" | "CHECKPOINT") => {
                tally.add(record["type"].as_str().unwrap_or_default().to_owned(), true);
                return parsed;
            }
            Some("PLANNER_RESPONSE") => "PLANNER_RESPONSE",
            Some("USER_INPUT") => "USER_INPUT",
            Some("GENERIC") => "GENERIC",
            _ => "(unknown Antigravity step)",
        };
        tally.add(kind.to_owned(), false);
    }
    parsed
}

fn read_turns(record: &Value) -> Vec<Turn> {
    let mut turns = Vec::new();
    match (record["type"].as_str(), record["source"].as_str()) {
        (Some("USER_INPUT"), Some("USER_EXPLICIT")) => {
            turns.extend(text_turn(record, Role::User, TurnKind::Operator, "content"));
        }
        (Some("PLANNER_RESPONSE"), Some("MODEL")) => {
            turns.extend(text_turn(
                record,
                Role::Assistant,
                TurnKind::Assistant,
                "content",
            ));
            turns.extend(text_turn(
                record,
                Role::Reasoning,
                TurnKind::Reasoning,
                "thinking",
            ));
            if let Some(calls) = record["tool_calls"].as_array() {
                for (index, call) in calls.iter().enumerate() {
                    if let Some(name) = call["name"].as_str().filter(|name| !name.is_empty()) {
                        let mut turn = turn(record, Role::Tool, TurnKind::Tool, call.to_string());
                        turn.parts.push(tool_part(
                            call,
                            &format!("tool_calls[{index}]"),
                            "tool-call",
                        ));
                        turn.coverage = Some(tool_coverage());
                        turn.tool = Some(tool_event(
                            EventKind::ToolCall,
                            "PLANNER_RESPONSE",
                            Some(name.to_owned()),
                            None,
                            Bounded::retaining_value(&call["args"]),
                            None,
                        ));
                        turns.push(turn);
                    }
                }
            }
        }
        (Some("GENERIC"), Some("MODEL")) => {
            if let Some(text) = record["content"].as_str().filter(|text| !text.is_empty()) {
                let mut result = turn(record, Role::Tool, TurnKind::Tool, text.to_owned());
                result
                    .parts
                    .push(tool_part(&record["content"], "content", "tool-result"));
                result.coverage = Some(tool_coverage());
                // A generic step's payload carries no call ID or tool name.
                result.tool = Some(tool_event(
                    EventKind::ToolResult,
                    "GENERIC",
                    None,
                    record["status"].as_str().map(str::to_owned),
                    None,
                    Some(Bounded::from_text(text)),
                ));
                turns.push(result);
            }
        }
        _ => {}
    }
    if record["status"] == "ERROR" && record["source"] == "MODEL" {
        turns.push(
            text_turn(record, Role::System, TurnKind::Notice, "error").unwrap_or_else(|| {
                turn(
                    record,
                    Role::System,
                    TurnKind::Notice,
                    "Antigravity recorded an error".to_owned(),
                )
            }),
        );
    }
    turns
}

fn text_turn(record: &Value, role: Role, kind: TurnKind, field: &str) -> Option<Turn> {
    let text = record[field].as_str().filter(|text| !text.is_empty())?;
    let mut turn = turn(record, role, kind, text.to_owned());
    turn.parts.push(text_part(
        text,
        field,
        record["type"].as_str().unwrap_or_default(),
    ));
    Some(turn)
}

fn turn(record: &Value, role: Role, kind: TurnKind, text: String) -> Turn {
    Turn {
        origin: None,
        text_truncated: None,
        text_chars: None,
        context_event: None,
        model: record_model(record),
        role,
        kind,
        text,
        ts: timestamp(&record["created_at"]),
        ordinal: 0,
        native_id: record["step_index"].as_u64().map(|index| index.to_string()),
        request_turn_id: None,
        metadata: record["status"].as_str().map(|status| EntryMetadata {
            engine: None,
            status: Some(status.to_owned()),
            label: None,
            source_fields: [("status".to_owned(), "/status".to_owned())].into(),
        }),
        record_ref: None,
        channel: None,
        recipient: None,
        parts: Vec::new(),
        coverage: None,
        tool: None,
    }
}

fn tool_event(
    kind: EventKind,
    subtype: &str,
    name: Option<String>,
    status: Option<String>,
    arguments: Option<Bounded>,
    output: Option<Bounded>,
) -> ToolEvent {
    ToolEvent {
        exit: None,
        kind,
        subtype: subtype.to_owned(),
        name,
        call_id: None,
        status,
        arguments,
        output,
        read: None,
        returned_read: None,
        commit_scan: None,
        edit_scan: None,
        completed_ts: None,
        invocations: Vec::new(),
        artifact_references: Vec::new(),
        artifact_consumptions: Vec::new(),
        self_contained: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_uri_decodes_local_paths_and_rejects_non_file_schemes() {
        assert_eq!(
            file_uri_to_path("file:///home/lobo/code/my%20repo"),
            Some(PathBuf::from("/home/lobo/code/my repo"))
        );
        assert_eq!(
            file_uri_to_path("file://localhost/tmp/work"),
            Some(PathBuf::from("/tmp/work"))
        );
        assert_eq!(file_uri_to_path("https://example.com/repo"), None);
        assert_eq!(file_uri_to_path("file://relative"), None);
    }

    #[test]
    fn usage_accumulator_deduplicates_identical_steps_and_flags_conflicts_and_checkpoints() {
        let mut usage = AntigravityUsage::default();
        let planner = json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-25T10:00:00Z",
            "model": "gemini-4-neon-xhigh",
            "input_tokens": 100,
            "cache_read_tokens": 400,
            "output_tokens": 50
        });
        usage.observe_record(&planner, None);
        // Exact duplicate witness counts once.
        usage.observe_record(&planner, None);
        // Conflicting witness for the same step_index is not summed.
        let conflicting = json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-25T10:00:01Z",
            "model": "gemini-4-neon-xhigh",
            "input_tokens": 999,
            "cache_read_tokens": 400,
            "output_tokens": 50
        });
        usage.observe_record(&conflicting, None);
        // Checkpoint compaction call counts in total and enters unattributed.
        let checkpoint = json!({
            "step_index": 2,
            "source": "SYSTEM",
            "type": "CHECKPOINT",
            "status": "DONE",
            "created_at": "2026-11-25T10:01:00Z",
            "input_tokens": 20,
            "cache_read_tokens": 500,
            "output_tokens": 10
        });
        usage.observe_record(&checkpoint, None);
        // Unexpected usage-bearing step increments unrecognized_usage.
        let unknown = json!({
            "step_index": 3,
            "source": "MODEL",
            "type": "GENERIC",
            "status": "DONE",
            "input_tokens": 7
        });
        usage.observe_record(&unknown, None);

        assert_eq!(usage.counted, 2);
        assert_eq!(usage.attributed, 1);
        assert_eq!(usage.unattributed, 1);
        assert_eq!(usage.conflicting_steps, 1);
        assert_eq!(usage.unrecognized_usage, 1);
    }
}
