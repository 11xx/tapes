use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::{
    accounting_for, default_native_store, enrich_native_session, head_directory, head_jsonl,
    list_discovered_files, list_discovered_files_with_search, read_bounds, read_recording,
    recorded_total_coverage, replay_pin, skipped_records_note, stream_jsonl,
    stream_jsonl_with_gaps, streamed_trailing_record, terminal_from_values, timestamp,
    trailing_record, transcript_from_recording, ActivityRange, Backend, FoldedSession, Jsonl,
    Listing, ParsedFile, ParsedTurns, Query, ReadPin, StreamedTranscript, TokenTotals,
    UnmappedTally,
};
use crate::content::{parts_from_array, project_text, tool_coverage, tool_part};
use crate::event::{Bounded, EventKind, ToolEvent};
use crate::history::PageProjection;
use crate::lineage::{ChildRef, Lineage, ParentRef, SourceRef};
use crate::measurement::{known_subtotal_notes, MeasurementReason};
use crate::model::{
    is_known_envelope, is_known_notice, AccountingBasis, AccountingCoverage, ByteSpan,
    DisjointTokens, KindDeclaration, Launch, Model, ModelObservationStatus, ReadGap, RecordRef,
    Role, Session, SourceDescriptor, TerminalObservation, TokenConvention, Tokens, TrailingRecord,
    Transcript, Turn, TurnKind, TurnSelection, UserDefault,
};
use crate::overlap::CopyEvidence;
use crate::span::{Placer, SpanBounds, SpanSessions};
use crate::usage::{
    Credits, ModelUsage, ObservationBasis, ObservationClassification, RateLimits, RateWindow,
    UsageAttribution, UsageDetail, UsageObservation, UsageObservationOmissions,
    UsageObservationOptions, UsageObservationResult, UsageObservationSeries,
};
use agent_tapes_discovery::{
    Harness as NativeHarness, IdentityBasis, NativeSession, NativeStore, StoreCatalog,
};

#[derive(Clone, Debug)]
pub struct CodexBackend {
    native_root: Option<PathBuf>,
    store: Option<NativeStore>,
    /// The store's walk, shared by every lookup, listing and join of this
    /// backend. Listing membership and join answers are the walk's; a
    /// named lookup that misses in an older walk walks again and replaces
    /// it.
    catalog: StoreCatalog,
    /// How much of a recording's end a transcript read takes.
    read_bytes: u64,
}

type CodexTranscriptRead = (
    Vec<Turn>,
    super::Recording,
    Option<TrailingRecord>,
    Option<TerminalObservation>,
    crate::model::UnmappedRecords,
);

struct CandidateRecording {
    path: PathBuf,
    id: String,
}

impl CodexBackend {
    /// A backend over the Codex store at `root`. Its listings answer from
    /// one walk of the store taken at first use; a consumer that holds the
    /// backend and needs a listing that sees later recordings builds a new
    /// backend. A named lookup sees later recordings either way.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            store: Some(NativeStore::codex(root)),
            catalog: StoreCatalog::new(),
            read_bytes: super::DEFAULT_READ_BYTES,
            native_root: None,
        }
    }

    pub(crate) fn from_store(store: Option<NativeStore>) -> Self {
        Self {
            store,
            catalog: StoreCatalog::new(),
            read_bytes: super::DEFAULT_READ_BYTES,
            native_root: None,
        }
    }

    pub(crate) fn with_native_root(mut self, root: PathBuf) -> Self {
        self.native_root = Some(root);
        self
    }

    /// How many directory walks this backend's catalog has made.
    pub fn catalog_walks(&self) -> usize {
        self.catalog.walks()
    }

    fn located(&self, native: Option<NativeSession>) -> Result<Option<Session>> {
        let Some(native) = native else {
            return Ok(None);
        };
        let path = native
            .locator()
            .ok_or_else(|| anyhow!("Codex native session {} has no file locator", native.id()))?;
        let (session, _, _) = self.parse(&native, path, ParsedTurns::TitleOnly)?;
        Ok(Some(enrich_native_session(&native, session)?))
    }

    fn source(&self, path: &Path) -> SourceDescriptor {
        let mut source = SourceDescriptor::installed("codex", path.display().to_string());
        if let Some(root) = &self.native_root {
            source.kind = crate::model::SourceKind::ExplicitNativeStore;
            if let Some(location) = &mut source.location {
                location.container = Some(root.display().to_string());
            }
        }
        source
    }

    /// Read at most `read_bytes` from the end of each recording.
    pub fn with_read_bytes(self, read_bytes: u64) -> Self {
        Self { read_bytes, ..self }
    }

    fn parse(
        &self,
        native: &NativeSession,
        path: &Path,
        wanted: ParsedTurns,
    ) -> Result<(Session, Vec<Turn>, Jsonl)> {
        let recording = read_recording(path, self.read_bytes)?;
        let (started_at, last_activity_at) = recording
            .time_range()
            .map_or((None, None), |(started, activity)| {
                (Some(started), Some(activity))
            });
        // `session_meta` is the file's first line, so the opening is where the
        // id and the recorded working directory live whatever the file's size.
        let opening = recording.opening();
        let read = &recording.tail;
        let mut identity_seen = false;
        for (in_opening, value) in opening
            .iter()
            .map(|value| (true, value))
            .chain(read.values.iter().map(|value| (false, value)))
            .filter(|(_, value)| value["type"] == "session_meta")
        {
            identity_seen |= in_opening;
            let id = value["payload"]["id"].as_str().ok_or_else(|| {
                anyhow!("{} has an invalid session_meta identity", path.display())
            })?;
            if id != native.id() {
                anyhow::bail!(
                    "Codex session_meta identity {id} disagrees with selected native identity {}",
                    native.id()
                );
            }
        }
        if native.identity_basis() == Some(IdentityBasis::Header) && !identity_seen {
            anyhow::bail!(
                "{} no longer contains the native session_meta identity selected during discovery",
                path.display()
            );
        }
        let directory = opening
            .iter()
            .find_map(codex_cwd)
            .map(PathBuf::from)
            .or_else(|| {
                read.values
                    .iter()
                    .rev()
                    .find_map(codex_cwd)
                    .map(PathBuf::from)
            });
        let launch = opening.first().and_then(codex_launch);
        let all = wanted == ParsedTurns::All;
        let mut reader = CodexTurns::default();
        let turns = read
            .values
            .iter()
            .filter(|_| all)
            .flat_map(|value| reader.counted(value))
            .collect::<Vec<_>>();
        let mut records = CodexRecords::new(path, read.truncated);
        records.observe_read(&read.values, &read.spans, &read.gaps, &read.source_revision);
        let facts = records.finish();

        let mut session = Session {
            id: native.id().to_owned(),
            source: self.source(path),
            metadata: None,
            model: facts.model,
            model_observation: facts.model_observation,
            model_selections: facts.model_selections,
            launch,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory,
            started_at,
            last_activity_at,
            live: None,
            cost: None,
            tokens: facts.tokens,
            accounting: facts.accounting,
            start_uncertain: recording.start_uncertain(),
            occurrence: None,
            usage_detail: facts.usage_detail,
        };
        let forked_from: Vec<String> = opening
            .iter()
            .find(|value| value["type"] == "session_meta")
            .and_then(|value| value["payload"]["forked_from_id"].as_str())
            .map(str::to_owned)
            .into_iter()
            .collect();
        session.record_copy_evidence(CopyEvidence {
            origins: forked_from.clone(),
            copied_origins: forked_from,
            ..CopyEvidence::default()
        });
        if let Some(parent) = opening
            .iter()
            .find(|value| value["type"] == "session_meta")
            .and_then(|value| value["payload"]["parent_thread_id"].as_str())
        {
            session
                .usage_detail
                .get_or_insert_with(Default::default)
                .declared_parent = Some(parent.to_owned());
        }
        // The opening is the start of the file, so its first user turn is the
        // session's first user turn even when the tail cannot see it.
        let session = if read.truncated {
            let mut reader = CodexTurns::default();
            session.with_derived_title_from(opening.iter().flat_map(|value| reader.counted(value)))
        } else if all {
            session.with_derived_title(&turns)
        } else {
            let mut reader = CodexTurns::default();
            session
                .with_derived_title_from(read.values.iter().flat_map(|value| reader.counted(value)))
        };

        Ok((session, turns, recording.tail))
    }
    fn read_history(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
        projection: PageProjection,
    ) -> Result<crate::history::Page> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("session has no source file"))?;
        crate::history::read_file(
            session,
            "codex",
            path,
            cursor,
            bytes,
            projection,
            |values, spans, revision| {
                let turns = if projection == PageProjection::Transcript {
                    let mut reader = CodexTurns::default();
                    values
                        .iter()
                        .zip(spans)
                        .flat_map(|(value, span)| {
                            let mut turns = reader.parse(value);
                            super::attach_record_refs(
                                &mut turns,
                                &format!("file:{}", path.display()),
                                Some(revision),
                                Some(*span),
                            );
                            turns
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let models = values
                    .iter()
                    .filter(|value| value["type"] == "turn_context")
                    .filter_map(|value| {
                        let payload = &value["payload"];
                        Some(crate::history::ModelObservation {
                            model: Model {
                                id: payload["model"].as_str()?.to_owned(),
                                variant: turn_context_effort(payload).map(str::to_owned),
                            },
                            timestamp: timestamp(&value["timestamp"]),
                        })
                    })
                    .collect();
                (turns, models)
            },
        )
    }
}

impl Default for CodexBackend {
    fn default() -> Self {
        Self::from_store(default_native_store(NativeHarness::Codex))
    }
}

impl Backend for CodexBackend {
    fn kinds(&self) -> KindDeclaration {
        KindDeclaration {
            recordable: TurnSelection::only([
                TurnKind::Operator,
                TurnKind::Assistant,
                TurnKind::Reasoning,
                TurnKind::Tool,
                TurnKind::Ambient,
                TurnKind::Notice,
            ]),
            user_default: Some(UserDefault {
                kind: TurnKind::Operator,
                basis: "Codex wraps the text it puts in the user role in elements of its own, so a user message holding anything else is the operator's".to_owned(),
            }),
        }
    }

    fn history_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> Result<crate::history::Page> {
        self.read_history(session, cursor, bytes, PageProjection::Transcript)
    }

    fn metadata_page(
        &self,
        session: &Session,
        cursor: Option<&str>,
        bytes: usize,
    ) -> Result<crate::history::Page> {
        self.read_history(session, cursor, bytes, PageProjection::Models)
    }

    fn harness(&self) -> &'static str {
        "codex"
    }

    fn accounting_bases(&self) -> &'static [AccountingBasis] {
        &[AccountingBasis::RecordedTotal]
    }

    fn available(&self) -> bool {
        self.store.as_ref().is_some_and(NativeStore::available)
    }

    fn list_titles(&self, _query: &Query) -> Result<Listing> {
        // This backend supplies display hints, not recorded titles.
        Ok(Listing::default())
    }

    fn list(&self, query: &Query) -> Result<Listing> {
        let Some(store) = self.store.as_ref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_discovered_files(
            (store, &self.catalog),
            query,
            |native, path| head_directory(path, native, codex_cwd),
            |native, path| {
                self.parse(native, path, ParsedTurns::TitleOnly)
                    .map(|(session, _, _)| session)
            },
        );
        listing
            .sessions
            .sort_by_key(|session| session.last_activity_at);
        listing.sessions.reverse();
        Ok(listing)
    }

    fn list_with_search(&self, query: &Query, needle: &str, tail: usize) -> Result<Listing> {
        let Some(store) = self.store.as_ref() else {
            return Ok(Listing::default());
        };
        let mut listing = list_discovered_files_with_search(
            (store, &self.catalog),
            query,
            needle,
            tail,
            self.read_bytes,
            |native, path| head_directory(path, native, codex_cwd),
            |native, path| {
                self.parse(native, path, ParsedTurns::All)
                    .map(|(session, turns, read)| ParsedFile {
                        session,
                        turns,
                        truncated: read.truncated,
                    })
            },
        );
        listing
            .sessions
            .sort_by_key(|session| session.last_activity_at);
        listing.sessions.reverse();
        Ok(listing)
    }

    /// Locate by filename: no other session file is parsed, though the store
    /// is still walked to find it.
    fn locate(&self, id: &str) -> Result<Option<Session>> {
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        self.located(store.locate_named_in(&self.catalog, id)?)
    }

    fn locate_relative(&self, id: &str) -> Result<Option<Session>> {
        let Some(store) = self.store.as_ref() else {
            return Ok(None);
        };
        self.located(store.locate_exact_in(&self.catalog, id)?)
    }

    fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("codex session {} has no file locator", session.id))?;
        let (turns, recording, trailing_record, terminal, unmapped) =
            read_transcript(path, self.read_bytes, &session.id)?;
        let mut session = session.clone();
        session.launch = recording.opening().first().and_then(codex_launch);
        let mut transcript = transcript_from_recording(
            session,
            turns,
            tail,
            &recording,
            terminal,
            trailing_record,
            Vec::new(),
        );
        if let Some(read) = &mut transcript.read {
            read.unmapped = Some(unmapped);
        }
        Ok(transcript)
    }

    fn supports_full_search(&self) -> bool {
        true
    }

    fn stream_transcript(
        &self,
        session: &Session,
        replay: Option<&StreamedTranscript>,
        turn: &mut dyn FnMut(Turn) -> Result<()>,
    ) -> Result<StreamedTranscript> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("codex session {} has no file locator", session.id))?;
        let domain = format!("file:{}", path.display());
        let mut terminal = None;
        let mut launch = None;
        let mut reader = CodexTurns::default();
        // The session's accounting folds from the same records and gaps as
        // the turns, as `stream_session` would replay them.
        let records = std::cell::RefCell::new(CodexRecords::new(path, false));
        let read = stream_jsonl_with_gaps(
            path,
            replay_pin(replay)?,
            |value, span, revision| {
                super::validate_native_identity(value, "codex", &session.id)?;
                records.borrow_mut().observe(value, Some(span), revision);
                if value["type"] == "session_meta" {
                    launch = codex_launch(value);
                }
                if let Some(observed) = codex_terminal(value) {
                    terminal = Some(observed);
                }
                let mut parsed = reader.parse(value);
                super::attach_record_refs(&mut parsed, &domain, Some(revision), Some(span));
                let produced = !parsed.is_empty();
                for mut parsed_turn in parsed {
                    parsed_turn.establish_origin(launch);
                    turn(parsed_turn)?;
                }
                Ok(produced)
            },
            |gap| records.borrow_mut().observe_gap(gap),
        )?;
        let mut records = records.into_inner();
        for gap in &read.gaps {
            records.observe_read_gap(gap);
        }
        let mut whole = session.clone();
        records.apply(&mut whole);
        Ok(StreamedTranscript {
            coordinates: super::StreamCoordinates::FileBytes,
            source_length: read.source_length,
            source_bounds: Vec::new(),
            source_revision: Some(read.revision),
            source_prefix_sha256: Some(read.prefix_sha256),
            skipped: read.skipped,
            trailing_record: streamed_trailing_record(read.last.as_ref(), codex_trailing_kind),
            terminal,
            gaps: read.gaps,
            notes: Vec::new(),
            unmapped: Some(reader.unmapped()),
            unrecognized_usage: 0,
            kinds: None,
            folded: Some(Box::new(FoldedSession {
                asked: session.clone(),
                whole,
            })),
        })
    }

    /// A Codex child is an ordinary rollout whose header names its parent, so
    /// lineage joins two independent records: the spawn and wait calls in the
    /// parent, keyed by the agent path they name, and the headers of the
    /// store's own recordings. Only headers are read; no child's turns are.
    fn lineage(&self, session: &Session) -> Result<Lineage> {
        let (files, path, discovery_notes) = self.lineage_inputs(session)?;
        self.lineage_from_inputs(session, files, path, discovery_notes)
    }

    fn stream_lineage(&self, session: &Session) -> Result<Lineage> {
        let (files, path, discovery_notes) = self.lineage_inputs(session)?;
        self.stream_lineage_from_inputs(session, files, path, discovery_notes)
    }

    fn stream_session(&self, session: &Session, read: &StreamedTranscript) -> Result<Session> {
        if let Some(whole) = read.folded_for(session) {
            return Ok(whole);
        }
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("codex session {} has no file locator", session.id))?;
        let records = std::cell::RefCell::new(CodexRecords::new(path, false));
        let streamed = stream_jsonl_with_gaps(
            path,
            Some(read.pin()?),
            |value, span, revision| {
                super::validate_native_identity(value, "codex", &session.id)?;
                records.borrow_mut().observe(value, Some(span), revision);
                Ok(false)
            },
            |gap| {
                records.borrow_mut().observe_gap(gap);
            },
        )?;
        let mut records = records.into_inner();
        for gap in &streamed.gaps {
            records.observe_read_gap(gap);
        }
        let mut whole = session.clone();
        records.apply(&mut whole);
        Ok(whole)
    }

    /// Each token count that moves the cumulative total is one request whose
    /// tokens are the move, as a span reads it, and each `rate_limits` window
    /// that reports a used percent is an observation of its pool: the account
    /// `session_meta` names, the limit, the plan, the window and its length.
    fn allowance_timeline(&self, session: &Session) -> Result<Option<crate::allowance::Timeline>> {
        use crate::allowance::{PoolKey, RateObservation, TimedUse};
        use crate::window::QuotaKind;
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("codex session {} has no file locator", session.id))?;
        let mut timeline = crate::allowance::Timeline::default();
        let mut account = None::<String>;
        let mut previous = None::<Tokens>;
        stream_jsonl(path, None, |value, _, _| {
            super::validate_native_identity(value, "codex", &session.id)?;
            if value["type"] == "session_meta" && account.is_none() {
                account = value["payload"]["creator_account_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
            }
            if !is_token_count(value) {
                return Ok(false);
            }
            let Some(at) = timestamp(&value["timestamp"]) else {
                return Ok(false);
            };
            if let Some(total) = codex_tokens(value) {
                let change = previous
                    .as_ref()
                    .map(|previous| compare_totals(previous, &total));
                let base = previous.replace(total.clone());
                if !matches!(change, Some(TotalChange::Unchanged)) {
                    let base = base.filter(|_| !matches!(change, Some(TotalChange::Reset)));
                    let moved = |pick: fn(&Tokens) -> Option<u64>| {
                        let now = pick(&total).unwrap_or(0);
                        now.saturating_sub(base.as_ref().and_then(pick).unwrap_or(0))
                    };
                    let input = moved(|tokens| tokens.input);
                    let cache_read = moved(|tokens| tokens.cache_read);
                    let cache_write = moved(|tokens| tokens.cache_write);
                    timeline.requests.push(TimedUse {
                        at,
                        uncached_input: input.saturating_sub(cache_read + cache_write),
                        cache_read,
                        cache_write,
                        output: moved(|tokens| tokens.output),
                        reasoning: moved(|tokens| tokens.reasoning),
                    });
                }
            }
            let limits = &value["payload"]["rate_limits"];
            for (quota, name) in [
                (QuotaKind::Primary, "primary"),
                (QuotaKind::Secondary, "secondary"),
            ] {
                let window = &limits[name];
                let Some(used_percent) = window["used_percent"].as_f64() else {
                    continue;
                };
                timeline.observations.push(RateObservation {
                    at,
                    pool: PoolKey {
                        harness: "codex".to_owned(),
                        account: account.clone(),
                        limit: limits["limit_id"].as_str().map(str::to_owned),
                        plan: limits["plan_type"].as_str().map(str::to_owned),
                        window: quota,
                        window_minutes: window["window_minutes"].as_u64(),
                    },
                    used_percent,
                    resets_at: window["resets_at"].as_i64().and_then(epoch_seconds),
                });
            }
            Ok(false)
        })?;
        Ok(Some(timeline))
    }

    /// A Codex request is a token count that moves the cumulative total, and
    /// its share is the move: a span's figure is the difference of the
    /// totals at its boundaries. A total that fell restarts the count, so the
    /// request after it contributes its own total and qualifies its span
    /// `since-reset`, and the whole is the sum of every request's move.
    fn stream_spans(
        &self,
        session: &Session,
        read: &StreamedTranscript,
        spans: &[SpanBounds],
    ) -> Result<SpanSessions> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("codex session {} has no file locator", session.id))?;
        let state = std::cell::RefCell::new((
            CodexRecords::new(path, false),
            Placer::new(spans),
            CodexSpans::default(),
        ));
        let streamed = stream_jsonl_with_gaps(
            path,
            Some(read.pin()?),
            |value, span, revision| {
                super::validate_native_identity(value, "codex", &session.id)?;
                let (records, placer, figures) = &mut *state.borrow_mut();
                records.observe(value, Some(span), revision);
                figures.observe(value, placer);
                Ok(false)
            },
            |gap| {
                let (records, placer, _) = &mut *state.borrow_mut();
                records.observe_gap(gap);
                placer.unreadable();
            },
        )?;
        let (mut records, placer, figures) = state.into_inner();
        for gap in &streamed.gaps {
            records.observe_read_gap(gap);
        }
        let mut whole = session.clone();
        records.apply(&mut whole);
        let mut notes = vec![
            "A Codex span is the difference of the cumulative totals at its boundaries; \
             its model split is not divided."
                .to_owned(),
        ];
        notes.extend(figures.whole(&mut whole));
        let placed = placer.finish();
        let figures = figures.finish(session, &whole, placed.figures());
        Ok(SpanSessions::assemble(placed, figures, whole, notes))
    }

    fn usage_observations(
        &self,
        session: &Session,
        read: Option<&crate::model::ReadEvidence>,
        options: UsageObservationOptions,
    ) -> Result<UsageObservationResult> {
        let path = super::session_file_path(session)
            .ok_or_else(|| anyhow!("codex session {} has no file locator", session.id))?;
        let read_window = read.is_none_or(|read| read.source_length > read.configured_bound);
        let records = CodexRecords::with_series(path, read_window, options);
        let records = if let Some(evidence) = read {
            if evidence
                .projection_options
                .iter()
                .any(|option| option == "full")
            {
                let prefix_sha256 = evidence.source_prefix_sha256.as_ref().ok_or_else(|| {
                    anyhow!("the full usage read has no prefix integrity evidence")
                })?;
                let records = std::cell::RefCell::new(records);
                stream_jsonl_with_gaps(
                    path,
                    Some(ReadPin {
                        length: evidence.source_length,
                        revision: evidence
                            .source_revision
                            .as_deref()
                            .ok_or_else(|| anyhow!("the full usage read has no source revision"))?,
                        prefix_sha256: Some(prefix_sha256),
                    }),
                    |value, span, revision| {
                        super::validate_native_identity(value, "codex", &session.id)?;
                        records.borrow_mut().observe(value, Some(span), revision);
                        Ok(false)
                    },
                    |gap| records.borrow_mut().observe_gap(gap),
                )?;
                records.into_inner()
            } else {
                let recording = super::read_recording_at(path, evidence)?;
                super::validate_recording_identity(&recording, "codex", &session.id)?;
                let mut records = records;
                records.observe_read(
                    &recording.tail.values,
                    &recording.tail.spans,
                    &recording.tail.gaps,
                    &recording.tail.source_revision,
                );
                records
            }
        } else {
            let recording = super::read_recording(path, self.read_bytes)?;
            super::validate_recording_identity(&recording, "codex", &session.id)?;
            let mut records = records;
            records.set_read_window(recording.tail.truncated);
            records.observe_read(
                &recording.tail.values,
                &recording.tail.spans,
                &recording.tail.gaps,
                &recording.tail.source_revision,
            );
            records
        };
        let mut observed = session.clone();
        let series = records.apply(&mut observed);
        Ok(UsageObservationResult {
            session: observed,
            series: series.ok_or_else(|| anyhow!("Codex did not produce an observation series"))?,
        })
    }
}

impl CodexBackend {
    fn lineage_from_inputs(
        &self,
        session: &Session,
        files: Vec<CandidateRecording>,
        path: PathBuf,
        discovery_notes: Vec<String>,
    ) -> Result<Lineage> {
        let recording = read_recording(&path, self.read_bytes)?;
        super::validate_recording_identity(&recording, "codex", &session.id)?;
        let header = recording
            .opening()
            .iter()
            .find(|value| value["type"] == "session_meta")
            .map(|value| value["payload"].clone())
            .unwrap_or(Value::Null);
        let mut agents = SpawnedAgents::default();
        for value in &recording.tail.values {
            agents.observe(value);
        }
        let lineage = Lineage {
            truncation: read_bounds(&recording.tail),
            ..self.relatives(&files, &path, session, &header, agents, discovery_notes)
        };
        Ok(lineage)
    }

    fn stream_lineage_from_inputs(
        &self,
        session: &Session,
        files: Vec<CandidateRecording>,
        path: PathBuf,
        discovery_notes: Vec<String>,
    ) -> Result<Lineage> {
        let mut header = None;
        let mut agents = SpawnedAgents::default();
        let read = stream_jsonl(&path, None, |value, _, _| {
            super::validate_native_identity(value, "codex", &session.id)?;
            if header.is_none() && value["type"] == "session_meta" {
                header = Some(value["payload"].clone());
            }
            agents.observe(value);
            Ok(false)
        })?;
        let header = header.unwrap_or(Value::Null);
        let mut lineage = self.relatives(&files, &path, session, &header, agents, discovery_notes);
        lineage.notes.extend(skipped_records_note(read.skipped));
        Ok(lineage)
    }

    /// Use discovery-owned native locators for sibling and parent joins.
    fn lineage_inputs(
        &self,
        session: &Session,
    ) -> Result<(Vec<CandidateRecording>, PathBuf, Vec<String>)> {
        let path = super::session_file_path(session)
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("codex session {} has no file locator", session.id))?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow!("codex store is unavailable"))?;
        let page = store.candidates_in(&self.catalog, 100_000);
        let files = page
            .records
            .iter()
            .filter_map(|native| {
                native.locator().map(|path| CandidateRecording {
                    path: path.to_path_buf(),
                    id: native.id().to_owned(),
                })
            })
            .collect();
        let mut notes = page
            .failures
            .into_iter()
            .map(|failure| format!("Codex lineage candidate: {failure}"))
            .collect::<Vec<_>>();
        notes.extend(
            page.unreadable_ids
                .into_iter()
                .map(|id| format!("Codex lineage candidate {id} could not be identified")),
        );
        if !page.complete && notes.is_empty() {
            notes.push("Codex lineage candidate coverage is incomplete".to_owned());
        }
        Ok((files, path, notes))
    }

    /// The relatives a rollout's header and spawned agents name, joined to the
    /// store's child rollout headers.
    fn relatives(
        &self,
        files: &[CandidateRecording],
        path: &Path,
        session: &Session,
        header: &Value,
        agents: SpawnedAgents,
        mut notes: Vec<String>,
    ) -> Lineage {
        let parent = header["parent_thread_id"].as_str().map(|native_id| {
            let resolved = match self
                .store
                .as_ref()
                .ok_or_else(|| anyhow!("codex store is unavailable"))
                .and_then(|store| {
                    store
                        .locate_exact_in(&self.catalog, native_id)
                        .map_err(Into::into)
                }) {
                Ok(session) => session.is_some(),
                Err(error) => {
                    notes.push(format!(
                        "Codex parent session {native_id} could not be resolved: {error:#}"
                    ));
                    false
                }
            };
            ParentRef {
                resolved,
                native_id: native_id.to_owned(),
                source: "session_meta.parent_thread_id".to_owned(),
            }
        });

        let mut agents = agents.agents;
        let (probed, probe_bound) = child_headers(files, path, &session.id, &mut notes);
        for child in probed {
            let entry = agents.entry(child.agent_path.clone()).or_default();
            entry.session_id = Some(child.thread_id);
            entry.nickname = child.nickname.or_else(|| entry.nickname.take());
            entry.source.push(SourceRef::File {
                path: child.path.display().to_string(),
            });
        }

        let children = agents
            .into_iter()
            .map(|(reference, agent)| ChildRef {
                role: agent.nickname.or(agent.task_name),
                model: agent.model,
                group: agent_group(&reference),
                spawned_at: agent.spawned_at,
                completed_at: agent.completed_at,
                disposition: agent.disposition,
                resolved: agent.session_id.is_some(),
                session_id: agent.session_id,
                source: agent.source,
                ..ChildRef::new(reference, self.harness())
            })
            .collect();

        if let Some(inspected) = probe_bound {
            notes.push(format!(
                "The lineage read inspected the newest {inspected} recordings in the store; \
                 an older child recording was not looked for."
            ));
        }
        let mut lineage = Lineage {
            parent,
            children,
            forked_from: header["forked_from_id"].as_str().map(str::to_owned),
            notes,
            ..Lineage::default()
        };
        // The agents are gathered by path rather than in record order.
        lineage.sort_children();
        lineage
    }
}

fn read_transcript(path: &Path, read_bytes: u64, expected: &str) -> Result<CodexTranscriptRead> {
    let recording = read_recording(path, read_bytes)?;
    super::validate_recording_identity(&recording, "codex", expected)?;
    let read = &recording.tail;
    let mut turns = Vec::new();
    let mut last_turn = None;
    let mut reader = CodexTurns::default();
    for (index, value) in read.values.iter().enumerate() {
        let mut parsed = reader.parse(value);
        super::attach_record_refs(
            &mut parsed,
            &format!("file:{}", path.display()),
            Some(&recording.tail.source_revision),
            read.spans.get(index).copied(),
        );
        if !parsed.is_empty() {
            last_turn = Some(index);
        }
        turns.extend(parsed);
    }
    let trailing_record = trailing_record(read.values.iter(), last_turn, codex_trailing_kind);
    let terminal = terminal_from_values(&read.values, codex_terminal);
    Ok((
        turns,
        recording,
        trailing_record,
        terminal,
        reader.unmapped(),
    ))
}

/// Codex records the working directory in its `session_meta` header and
/// repeats it on every `turn_context`, so either line answers the question.
fn codex_cwd(value: &Value) -> Option<&str> {
    match value["type"].as_str() {
        Some("session_meta") | Some("turn_context") => value["payload"]["cwd"].as_str(),
        _ => None,
    }
}

fn codex_launch(value: &Value) -> Option<Launch> {
    if value["type"] != "session_meta" {
        return None;
    }
    match &value["payload"]["source"] {
        Value::String(source) => match source.as_str() {
            "cli" | "vscode" => Some(Launch::Interactive),
            "exec" => Some(Launch::Headless),
            _ => None,
        },
        Value::Object(source) if source.contains_key("subagent") => Some(Launch::Headless),
        _ => None,
    }
}

const MAX_MODEL_KEYS: usize = 32;
const MAX_MODEL_KEY_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_IDS: usize = 16_384;
const MAX_RESPONSE_ID_BYTES: usize = 1024 * 1024;
const MAX_IDENTITY_BYTES: usize = 16 * 1024;
const MAX_ATTRIBUTION_REASONS: usize = 32;
const MAX_SERIES_BYTES: usize = 8 * 1024 * 1024;
const MAX_SERIES_GAPS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ModelKey {
    model: String,
    variant: Option<String>,
}

impl ModelKey {
    fn from_model(model: &Model) -> Option<Self> {
        (model.id.len() <= MAX_IDENTITY_BYTES
            && model
                .variant
                .as_ref()
                .is_none_or(|variant| variant.len() <= MAX_IDENTITY_BYTES))
        .then(|| Self {
            model: model.id.clone(),
            variant: model.variant.clone(),
        })
    }

    fn bytes(&self) -> usize {
        self.model.len() + self.variant.as_ref().map_or(0, String::len)
    }
}

#[derive(Default)]
struct ModelBudget {
    keys: BTreeSet<ModelKey>,
    bytes: usize,
    exhausted: bool,
}

impl ModelBudget {
    /// How many distinct keys the budget retained, which is the read's
    /// distinct-model count exactly while the budget has room for every key
    /// observed.
    fn retained(&self) -> usize {
        self.keys.len()
    }

    fn reserve(&mut self, key: &ModelKey) -> bool {
        if self.keys.contains(key) {
            return true;
        }
        if self.keys.len() >= MAX_MODEL_KEYS
            || self.bytes.saturating_add(key.bytes()) > MAX_MODEL_KEY_BYTES
        {
            self.exhausted = true;
            return false;
        }
        self.bytes += key.bytes();
        self.keys.insert(key.clone());
        true
    }
}

#[derive(Default)]
struct BoundedReasons(Vec<String>);

impl BoundedReasons {
    fn add(&mut self, reason: &str) {
        if !self.0.iter().any(|existing| existing == reason)
            && self.0.len() < MAX_ATTRIBUTION_REASONS
        {
            self.0.push(reason.to_owned());
        }
    }

    fn extend(&mut self, reasons: &Self) {
        for reason in &reasons.0 {
            self.add(reason);
        }
    }

    fn into_inner(self) -> Vec<String> {
        self.0
    }
}

#[derive(Default)]
struct CandidateModel {
    tokens: TokenTotals,
    request_count: usize,
}

#[derive(Default)]
struct UsageCandidate {
    observed: usize,
    counted: usize,
    attributed: usize,
    unattributed: usize,
    leading_uncounted: usize,
    resets: usize,
    outside_total: usize,
    outside_total_tokens: TokenTotals,
    by_model: BTreeMap<ModelKey, CandidateModel>,
    unattributed_tokens: TokenTotals,
    incomplete: BoundedReasons,
    bounds: BoundedReasons,
}

impl UsageCandidate {
    fn observe(&mut self) {
        self.observed += 1;
    }

    fn leave_total(&mut self, tokens: &Tokens) {
        self.outside_total += 1;
        self.outside_total_tokens.add(
            tokens.input,
            tokens.output,
            tokens.reasoning,
            tokens.cache_read,
            tokens.cache_write,
        );
    }

    fn add_unattributed(&mut self, tokens: Option<&Tokens>) {
        self.unattributed += 1;
        // An unattributed request that recorded no usage still counts, with
        // every counter missing.
        self.unattributed_tokens.add(
            tokens.and_then(|tokens| tokens.input),
            tokens.and_then(|tokens| tokens.output),
            tokens.and_then(|tokens| tokens.reasoning),
            tokens.and_then(|tokens| tokens.cache_read),
            tokens.and_then(|tokens| tokens.cache_write),
        );
    }

    fn count_request(
        &mut self,
        tokens: Option<&Tokens>,
        model: Option<&Model>,
        identity_valid: bool,
        model_budget: &mut ModelBudget,
    ) -> bool {
        if !identity_valid {
            self.add_unattributed(tokens);
            self.incomplete.add("request-identity-missing");
            return false;
        }
        self.counted += 1;
        let Some(model) = model else {
            self.add_unattributed(tokens);
            self.incomplete.add("model-attribution-unknown");
            return true;
        };
        let Some(key) = ModelKey::from_model(model) else {
            self.add_unattributed(tokens);
            self.bounds.add("model-identity-too-large");
            return true;
        };
        if !model_budget.reserve(&key) {
            self.add_unattributed(tokens);
            self.bounds.add("model-key-budget");
            return true;
        }
        let entry = self.by_model.entry(key).or_default();
        // A counted request that recorded no usage still counts, with every
        // counter missing.
        entry.tokens.add(
            tokens.and_then(|tokens| tokens.input),
            tokens.and_then(|tokens| tokens.output),
            tokens.and_then(|tokens| tokens.reasoning),
            tokens.and_then(|tokens| tokens.cache_read),
            tokens.and_then(|tokens| tokens.cache_write),
        );
        entry.request_count += 1;
        self.attributed += 1;
        true
    }

    fn attribution(
        &self,
        basis: ObservationBasis,
        coverage: AccountingCoverage,
        global_incomplete: &BoundedReasons,
        global_bounds: &BoundedReasons,
    ) -> UsageAttribution {
        let mut incomplete = BoundedReasons::default();
        incomplete.extend(&self.incomplete);
        incomplete.extend(global_incomplete);
        let mut bounds = BoundedReasons::default();
        bounds.extend(&self.bounds);
        bounds.extend(global_bounds);
        let (unattributed_tokens, unattributed_coverage) =
            self.unattributed_tokens.clone().finish_covered();
        let (outside_total_tokens, outside_total_coverage) =
            self.outside_total_tokens.clone().finish_covered();
        UsageAttribution {
            basis,
            coverage,
            observed: self.observed,
            counted: self.counted,
            attributed: self.attributed,
            unattributed: self.unattributed,
            leading_uncounted: self.leading_uncounted,
            resets: self.resets,
            outside_total: self.outside_total,
            outside_total_tokens,
            outside_total_coverage,
            unattributed_tokens,
            unattributed_coverage,
            unattributed_completeness: Default::default(),
            incomplete: incomplete.into_inner(),
            bounds: bounds.into_inner(),
        }
    }

    fn into_models(self) -> Option<Vec<ModelUsage>> {
        let models = self
            .by_model
            .into_iter()
            .map(|(key, value)| {
                let (tokens, measurement_coverage) = value.tokens.finish_covered();
                ModelUsage {
                    list_price_estimate: None,
                    measurement_completeness: Default::default(),
                    model: key.model,
                    variant: key.variant,
                    tokens,
                    cost: None,
                    cost_coverage: None,
                    request_count: Some(value.request_count),
                    measurement_incomplete: known_subtotal_notes(&measurement_coverage),
                    measurement_coverage,
                }
            })
            .collect::<Vec<_>>();
        (!models.is_empty()).then_some(models)
    }
}

struct PendingSeriesRow {
    row: UsageObservation,
    basis: ObservationBasis,
    counted: bool,
}

struct SeriesBuffer {
    limit: usize,
    bytes: usize,
    observed: usize,
    rows: VecDeque<PendingSeriesRow>,
    omissions: UsageObservationOmissions,
    gaps: Vec<ReadGap>,
    gaps_omitted: usize,
}

impl SeriesBuffer {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            bytes: 0,
            observed: 0,
            rows: VecDeque::new(),
            omissions: UsageObservationOmissions::default(),
            gaps: Vec::new(),
            gaps_omitted: 0,
        }
    }

    fn add_gap(&mut self, gap: &ReadGap) {
        if self.gaps.iter().any(|existing| existing == gap) {
            return;
        }
        if self.gaps.len() < MAX_SERIES_GAPS {
            self.gaps.push(gap.clone());
        } else {
            self.gaps_omitted += 1;
        }
    }

    fn push(&mut self, mut row: UsageObservation, basis: ObservationBasis, counted: bool) {
        self.observed += 1;
        row.counted = counted;
        let Ok(size) = serde_json::to_vec(&row).map(|bytes| bytes.len()) else {
            self.omissions.oversized_row += 1;
            return;
        };
        if size > MAX_SERIES_BYTES {
            self.omissions.oversized_row += 1;
            return;
        }
        while self.rows.len() >= self.limit {
            if let Some(old) = self.rows.pop_front() {
                self.bytes = self.bytes.saturating_sub(serialized_size(&old.row));
                self.omissions.row_cap += 1;
            }
        }
        while self.bytes.saturating_add(size) > MAX_SERIES_BYTES {
            let Some(old) = self.rows.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(serialized_size(&old.row));
            self.omissions.byte_budget += 1;
        }
        self.bytes += size;
        self.rows.push_back(PendingSeriesRow {
            row,
            basis,
            counted,
        });
    }

    fn finish(mut self, basis: ObservationBasis) -> UsageObservationSeries {
        let mut rows = VecDeque::new();
        let mut bytes: usize = 0;
        while let Some(mut pending) = self.rows.pop_back() {
            pending.row.counted = pending.basis == basis && pending.counted;
            let size = serialized_size(&pending.row);
            if size > MAX_SERIES_BYTES {
                self.omissions.oversized_row += 1;
                continue;
            }
            if bytes.saturating_add(size) > MAX_SERIES_BYTES {
                self.omissions.byte_budget += 1;
                continue;
            }
            bytes += size;
            rows.push_front(pending.row);
        }
        let rows = rows.into_iter().collect::<Vec<_>>();
        UsageObservationSeries {
            observed: self.observed,
            returned: rows.len(),
            rows,
            omissions: self.omissions,
            gaps: self.gaps,
            gaps_omitted: self.gaps_omitted,
        }
    }
}

fn serialized_size<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

struct CodexUsageObserver {
    domain: String,
    read_window: bool,
    tokens: Option<Tokens>,
    context_window: Option<u64>,
    rate_limits: Option<RateLimits>,
    latest_model: Option<Model>,
    current_model: Option<Model>,
    selections: crate::model_observation::Selections,
    model_budget: ModelBudget,
    model_observed: bool,
    mixed: bool,
    previous_model_key: Option<ModelKey>,
    seen_response_ids: HashSet<String>,
    seen_response_id_bytes: usize,
    response_id_budget_exhausted: bool,
    previous_total: Option<Tokens>,
    /// The usage of each request counted since the last token count that
    /// carried a total, which settles whether the total moved for them.
    unsettled: Vec<Tokens>,
    modern_seen: bool,
    legacy_seen: bool,
    legacy_preceded_modern: bool,
    reset_seen: bool,
    observed_resets: usize,
    gap_seen: bool,
    modern: UsageCandidate,
    legacy: UsageCandidate,
    incomplete: BoundedReasons,
    bounds: BoundedReasons,
    /// Records carrying codex's usage shape that neither usage reading counts.
    unrecognized_usage: usize,
    activity: ActivityRange,
    series: Option<SeriesBuffer>,
}

impl CodexUsageObserver {
    fn new(path: &Path, read_window: bool, series_limit: Option<usize>) -> Self {
        Self {
            domain: format!("file:{}", path.display()),
            read_window,
            tokens: None,
            context_window: None,
            rate_limits: None,
            latest_model: None,
            current_model: None,
            selections: crate::model_observation::Selections::default(),
            model_budget: ModelBudget::default(),
            model_observed: false,
            mixed: false,
            previous_model_key: None,
            seen_response_ids: HashSet::new(),
            seen_response_id_bytes: 0,
            response_id_budget_exhausted: false,
            previous_total: None,
            unsettled: Vec::new(),
            modern_seen: false,
            legacy_seen: false,
            legacy_preceded_modern: false,
            reset_seen: false,
            observed_resets: 0,
            gap_seen: false,
            modern: UsageCandidate::default(),
            legacy: UsageCandidate::default(),
            incomplete: BoundedReasons::default(),
            bounds: BoundedReasons::default(),
            unrecognized_usage: 0,
            activity: ActivityRange::default(),
            series: series_limit.map(SeriesBuffer::new),
        }
    }

    fn observe_gap(&mut self, gap: &ReadGap) {
        self.gap_seen = true;
        self.selections.interrupt();
        self.current_model = None;
        self.previous_total = None;
        self.incomplete.add("source-gap");
        if let Some(series) = self.series.as_mut() {
            series.add_gap(gap);
        }
    }

    fn observe_read_gap(&mut self, gap: &ReadGap) {
        self.gap_seen = true;
        self.incomplete.add("source-gap");
        if let Some(series) = self.series.as_mut() {
            series.add_gap(gap);
        }
    }

    fn observe(&mut self, value: &Value, span: Option<ByteSpan>, revision: &str) {
        self.activity.observe(value);
        if value["type"] == "turn_context" {
            self.observe_context(value);
        }
        if value["type"] == "token_usage_record" {
            self.observe_modern(value, span, revision);
        } else if is_token_count(value) {
            self.observe_legacy(value, span, revision);
        } else if codex_unread_usage(value) {
            self.unrecognized_usage += 1;
        }
    }

    fn observe_context(&mut self, value: &Value) {
        self.model_observed = true;
        let payload = &value["payload"];
        let Some(id) = payload["model"].as_str() else {
            self.current_model = None;
            self.incomplete.add("model-attribution-unknown");
            return;
        };
        if id.len() > MAX_IDENTITY_BYTES {
            self.current_model = None;
            self.latest_model = None;
            self.incomplete.add("model-identity-too-large");
            self.bounds.add("model-identity-budget");
            return;
        }
        let variant = turn_context_effort(payload).map(|effort| {
            if effort.len() <= MAX_IDENTITY_BYTES {
                effort.to_owned()
            } else {
                self.incomplete.add("model-variant-too-large");
                String::new()
            }
        });
        let variant = variant.filter(|variant| !variant.is_empty());
        let model = Model {
            id: id.to_owned(),
            variant,
        };
        let Some(key) = ModelKey::from_model(&model) else {
            self.current_model = None;
            self.latest_model = None;
            self.incomplete.add("model-identity-too-large");
            return;
        };
        if self.previous_model_key.as_ref() != Some(&key) {
            if self.previous_model_key.is_some() {
                self.mixed = true;
            }
            self.previous_model_key = Some(key.clone());
        }
        if !self.model_budget.reserve(&key) {
            self.bounds.add("model-key-budget");
        }
        self.selections.observe(
            model.clone(),
            timestamp(&value["timestamp"]),
            payload["id"].as_str(),
        );
        self.current_model = Some(model.clone());
        self.latest_model = Some(model);
    }

    fn record_ref(
        &self,
        span: Option<ByteSpan>,
        revision: &str,
        native_id: Option<String>,
    ) -> Option<RecordRef> {
        Some(RecordRef {
            domain: self.domain.clone(),
            revision: Some(revision.to_owned()),
            span,
            native_id,
            pointer: None,
            part_index: 0,
            content_part_index: None,
        })
    }

    fn observe_modern(&mut self, value: &Value, span: Option<ByteSpan>, revision: &str) {
        let payload = &value["payload"];
        let usage = tokens_from_object(&payload["usage"]);
        let response_id_raw = payload["response_id"].as_str();
        let response_id = response_id_raw
            .filter(|id| id.len() <= MAX_IDENTITY_BYTES)
            .map(str::to_owned);
        let mut row = UsageObservation {
            record_type: "token_usage_record".to_owned(),
            payload_type: None,
            native_ordinal: value["ordinal"].as_u64(),
            record_ref: self.record_ref(span, revision, response_id.clone()),
            timestamp: timestamp(&value["timestamp"]),
            response_id,
            model: self.current_model.as_ref().map(|model| model.id.clone()),
            variant: self
                .current_model
                .as_ref()
                .and_then(|model| model.variant.clone()),
            classification: ObservationClassification::Incomplete,
            counted: false,
            usage: usage.clone(),
            total_token_usage: None,
            last_token_usage: None,
            disjoint: codex_disjoint(usage.as_ref()),
            context_window: None,
            rate_limits: None,
        };
        let Some(usage) = usage else {
            self.incomplete.add("modern-usage-missing");
            self.push_series(row, ObservationBasis::UsageRecord, false);
            return;
        };
        self.legacy_preceded_modern |= self.legacy_seen && !self.modern_seen;
        self.modern_seen = true;
        self.modern.observe();
        let Some(response_id) = response_id_raw.filter(|id| !id.is_empty()) else {
            self.modern.add_unattributed(Some(&usage));
            self.modern.incomplete.add("request-identity-missing");
            row.classification = ObservationClassification::Unattributed;
            self.push_series(row, ObservationBasis::UsageRecord, false);
            return;
        };
        if response_id.len() > MAX_IDENTITY_BYTES {
            self.modern.add_unattributed(Some(&usage));
            self.modern.incomplete.add("request-identity-too-large");
            self.bounds.add("response-id-size");
            row.classification = ObservationClassification::Incomplete;
            self.push_series(row, ObservationBasis::UsageRecord, false);
            return;
        }
        if self.seen_response_ids.contains(response_id) {
            row.classification = ObservationClassification::Repeat;
            self.push_series(row, ObservationBasis::UsageRecord, false);
            return;
        }
        if self.response_id_budget_exhausted
            || self.seen_response_ids.len() >= MAX_RESPONSE_IDS
            || self
                .seen_response_id_bytes
                .saturating_add(response_id.len())
                > MAX_RESPONSE_ID_BYTES
        {
            self.response_id_budget_exhausted = true;
            self.modern.add_unattributed(Some(&usage));
            self.modern.incomplete.add("response-id-dedup-incomplete");
            self.bounds.add("response-id-budget");
            row.classification = ObservationClassification::Incomplete;
            self.push_series(row, ObservationBasis::UsageRecord, false);
            return;
        }
        self.seen_response_id_bytes += response_id.len();
        self.seen_response_ids.insert(response_id.to_owned());
        let attributed_before = self.modern.attributed;
        let counted = self.modern.count_request(
            Some(&usage),
            self.current_model.as_ref(),
            true,
            &mut self.model_budget,
        );
        row.classification = if self.modern.attributed > attributed_before {
            ObservationClassification::Request
        } else if counted {
            ObservationClassification::Unattributed
        } else {
            ObservationClassification::Incomplete
        };
        if counted {
            self.unsettled.push(usage);
        }
        self.push_series(row, ObservationBasis::UsageRecord, counted);
    }

    /// A token count carrying a total settles the requests counted since
    /// the previous one. A total that stood still moved for none of them; a
    /// total whose last usage is the newest request's own moved for it
    /// alone. Any other move is not attributed to particular requests.
    fn settle(&mut self, change: Option<&TotalChange>, last: Option<&Tokens>) {
        let mut unsettled = std::mem::take(&mut self.unsettled);
        match change {
            Some(TotalChange::Unchanged) => {}
            Some(TotalChange::Incomparable) => return,
            _ if unsettled.last() == last && last.is_some() => {
                unsettled.pop();
            }
            _ => return,
        }
        for usage in &unsettled {
            self.modern.leave_total(usage);
        }
    }

    fn observe_legacy(&mut self, value: &Value, span: Option<ByteSpan>, revision: &str) {
        let total = codex_tokens(value);
        let last = codex_last_tokens(value);
        if let Some(total) = &total {
            self.tokens = Some(total.clone());
        }
        if let Some(window) = codex_context_window(value) {
            self.context_window = Some(window);
        }
        let context_window = codex_context_window(value);
        let rate_limits = codex_rate_limits(value);
        if let Some(limits) = &rate_limits {
            self.rate_limits = Some(limits.clone());
        }
        let row_total = total.clone();
        let row_last = last.clone();
        let row_record_ref = self.record_ref(span, revision, None);
        let row_model = self.current_model.as_ref().map(|model| model.id.clone());
        let row_variant = self
            .current_model
            .as_ref()
            .and_then(|model| model.variant.clone());
        let row_ordinal = value["ordinal"].as_u64();
        let row_timestamp = timestamp(&value["timestamp"]);
        let row = move |classification, counted| UsageObservation {
            record_type: "event_msg".to_owned(),
            payload_type: Some("token_count".to_owned()),
            native_ordinal: row_ordinal,
            record_ref: row_record_ref.clone(),
            timestamp: row_timestamp,
            response_id: None,
            model: row_model.clone(),
            variant: row_variant.clone(),
            classification,
            counted,
            usage: None,
            total_token_usage: row_total.clone(),
            disjoint: codex_disjoint(row_last.as_ref()),
            last_token_usage: row_last.clone(),
            context_window,
            rate_limits: rate_limits.clone(),
        };
        let Some(total) = total else {
            self.push_series(
                row(ObservationClassification::QuotaOnly, false),
                ObservationBasis::TokenEventAdvance,
                false,
            );
            return;
        };
        self.legacy_seen = true;
        self.legacy.observe();
        let previous = self.previous_total.replace(total.clone());
        self.settle(
            previous
                .as_ref()
                .map(|previous| compare_totals(previous, &total))
                .as_ref(),
            last.as_ref(),
        );
        let (classification, counted) = match previous {
            None if !self.read_window && supports_single_legacy_request(&total, last.as_ref()) => {
                let counted = self.legacy.count_request(
                    last.as_ref(),
                    self.current_model.as_ref(),
                    true,
                    &mut self.model_budget,
                );
                (ObservationClassification::Advance, counted)
            }
            None => {
                self.legacy.leading_uncounted += 1;
                self.legacy.add_unattributed(last.as_ref());
                self.legacy.incomplete.add("leading-uncounted");
                (ObservationClassification::LeadingUncounted, false)
            }
            Some(previous) => match compare_totals(&previous, &total) {
                TotalChange::Advance => {
                    let counted = if last.is_some() {
                        self.legacy.count_request(
                            last.as_ref(),
                            self.current_model.as_ref(),
                            true,
                            &mut self.model_budget,
                        )
                    } else {
                        self.legacy.add_unattributed(None);
                        self.legacy.incomplete.add("last-usage-missing");
                        false
                    };
                    (ObservationClassification::Advance, counted)
                }
                TotalChange::Reset => {
                    self.legacy.resets += 1;
                    self.observed_resets += 1;
                    self.reset_seen = true;
                    let counted = if last.is_some() {
                        self.legacy.count_request(
                            last.as_ref(),
                            self.current_model.as_ref(),
                            true,
                            &mut self.model_budget,
                        )
                    } else {
                        self.legacy.add_unattributed(None);
                        self.legacy.incomplete.add("reset-last-usage-missing");
                        false
                    };
                    (ObservationClassification::ResetAdvance, counted)
                }
                TotalChange::Unchanged => (ObservationClassification::UnchangedTotal, false),
                TotalChange::Incomparable => {
                    self.legacy.add_unattributed(last.as_ref());
                    self.legacy.incomplete.add("cumulative-gap");
                    (ObservationClassification::Incomplete, false)
                }
            },
        };
        self.push_series(
            row(classification, counted),
            ObservationBasis::TokenEventAdvance,
            counted,
        );
    }

    fn push_series(&mut self, row: UsageObservation, basis: ObservationBasis, counted: bool) {
        if let Some(series) = self.series.as_mut() {
            series.push(row, basis, counted);
        }
    }

    fn finish(mut self) -> CodexUsageFacts {
        // No token count followed these requests, so the total never moved
        // for them.
        for usage in std::mem::take(&mut self.unsettled) {
            self.modern.leave_total(&usage);
        }
        let basis = if self.modern_seen {
            ObservationBasis::UsageRecord
        } else {
            ObservationBasis::TokenEventAdvance
        };
        let mut selected = if self.modern_seen {
            self.modern
        } else {
            self.legacy
        };
        if self.read_window && !self.modern_seen {
            selected
                .incomplete
                .add("modern-records-not-observed-in-read");
        }
        // Choosing the modern basis discards the legacy candidate whole. Where
        // legacy observations came first, the requests behind them are not in
        // the selected aggregate and summing the two would be worse, so the
        // read names what its basis cannot account for.
        if self.legacy_preceded_modern {
            selected
                .incomplete
                .add("legacy-observations-before-modern-basis");
        }
        selected.resets = self.observed_resets;
        if self.model_budget.exhausted {
            selected.bounds.add("model-key-budget");
        }
        if self.response_id_budget_exhausted {
            selected.bounds.add("response-id-budget");
        }
        let accounting_coverage = if self.reset_seen {
            AccountingCoverage::SinceReset
        } else if self.read_window || self.gap_seen {
            AccountingCoverage::ReadWindow
        } else {
            AccountingCoverage::Session
        };
        let observation_coverage = if self.read_window || self.gap_seen {
            AccountingCoverage::ReadWindow
        } else {
            AccountingCoverage::Session
        };
        let attribution =
            (selected.observed > 0 || self.modern_seen || self.legacy_seen).then(|| {
                selected.attribution(basis, observation_coverage, &self.incomplete, &self.bounds)
            });
        let by_model = selected.into_models();
        let (selection_status, model_selections) = self.selections.finish(!self.read_window);
        let model_observation = (self.model_observed
            || attribution
                .as_ref()
                .is_some_and(|attribution| attribution.unattributed > 0)
            || !self.incomplete.0.is_empty()
            || !self.bounds.0.is_empty())
        .then_some(ModelObservationStatus {
            mixed: self.mixed,
            head_read: !self.read_window,
            attribution_uncertain: selection_status
                .as_ref()
                .is_some_and(|s| s.attribution_uncertain)
                || attribution.as_ref().is_some_and(|attribution| {
                    attribution.unattributed > 0
                        || !attribution.incomplete.is_empty()
                        || !attribution.bounds.is_empty()
                })
                || !self.incomplete.0.is_empty()
                || !self.bounds.0.is_empty(),
            // Past the key budget the retained set is a floor rather than a
            // count, so no number is published for it.
            distinct_observed: (self.model_observed
                && !self.model_budget.exhausted
                && selection_status
                    .as_ref()
                    .is_some_and(|s| s.distinct_observed.is_some()))
            .then(|| self.model_budget.retained()),
        });
        let measurement_coverage = recorded_total_coverage(self.tokens.as_ref());
        let accounting = accounting_for(
            self.tokens.as_ref(),
            None,
            &measurement_coverage,
            AccountingBasis::RecordedTotal,
            accounting_coverage,
        );
        let usage_detail = UsageDetail {
            context_window: self.context_window,
            rate_limits: self.rate_limits,
            durations_ms: None,
            by_model,
            attribution,
            measurement_incomplete: known_subtotal_notes(&measurement_coverage),
            measurement_coverage,
            unrecognized_usage: self.unrecognized_usage,
            ..UsageDetail::default()
        }
        .into_option();
        let series = self.series.map(|series| series.finish(basis));
        CodexUsageFacts {
            tokens: self.tokens,
            accounting,
            model: self.latest_model,
            model_observation,
            model_selections,
            usage_detail,
            activity: self.activity,
            series,
        }
    }
}

/// Codex span figures, each the moves of the cumulative total its requests
/// made, with what the span's own token counts reported beside them.
/// `moves` sums every request's move, the whole the figures add up to.
#[derive(Default)]
struct CodexSpans {
    previous: Option<Tokens>,
    response: Option<(String, Option<Tokens>)>,
    model: Option<Model>,
    figures: BTreeMap<usize, CodexSpanFigure>,
    moves: TokenTotals,
    restarts: usize,
}

#[derive(Default)]
struct CodexSpanFigure {
    tokens: TokenTotals,
    since_reset: bool,
    model: Option<Model>,
    /// The model ids in effect at the span's requests.
    models: BTreeSet<String>,
    context_window: Option<u64>,
    rate_limits: Option<RateLimits>,
    unrecognized_usage: usize,
}

impl CodexSpans {
    fn observe(&mut self, value: &Value, placer: &mut Placer) {
        let at = timestamp(&value["timestamp"]);
        if value["type"] == "turn_context" {
            let payload = &value["payload"];
            self.model = payload["model"].as_str().map(|id| Model {
                id: id.to_owned(),
                variant: turn_context_effort(payload)
                    .filter(|effort| !effort.is_empty())
                    .map(str::to_owned),
            });
        } else if value["type"] == "token_usage_record" {
            let payload = &value["payload"];
            self.response = payload["response_id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= MAX_IDENTITY_BYTES)
                .map(|id| (id.to_owned(), tokens_from_object(&payload["usage"])));
        } else if let Some(total) = codex_tokens(value) {
            let change = self
                .previous
                .as_ref()
                .map(|previous| compare_totals(previous, &total));
            let previous = self.previous.replace(total.clone());
            if matches!(change, Some(TotalChange::Unchanged)) {
                return;
            }
            let reset = matches!(change, Some(TotalChange::Reset));
            let base = previous.filter(|_| !reset).unwrap_or(Tokens {
                input: Some(0),
                output: Some(0),
                reasoning: Some(0),
                cache_read: Some(0),
                cache_write: Some(0),
            });
            // A cumulative counter never falls within an epoch, so one that
            // stands at zero moved by zero even where the base did not write it.
            let moved = |base: Option<u64>, total: Option<u64>| match (base, total?) {
                (_, 0) => Some(0),
                (base, total) => Some(total.saturating_sub(base?)),
            };
            let last = codex_last_tokens(value);
            let response = self
                .response
                .take()
                .filter(|(_, usage)| usage.is_some() && *usage == last)
                .map(|(id, _)| id);
            let request = (
                moved(base.input, total.input),
                moved(base.output, total.output),
                moved(base.reasoning, total.reasoning),
                moved(base.cache_read, total.cache_read),
                moved(base.cache_write, total.cache_write),
            );
            self.moves
                .add(request.0, request.1, request.2, request.3, request.4);
            self.restarts += usize::from(reset);
            for index in placer.request(at, response.as_deref()) {
                let figure = self.figures.entry(index).or_default();
                figure
                    .tokens
                    .add(request.0, request.1, request.2, request.3, request.4);
                figure.since_reset |= reset;
                figure.model.clone_from(&self.model);
                if let Some(model) = &self.model {
                    figure.models.insert(model.id.clone());
                }
                if let Some(window) = codex_context_window(value) {
                    figure.context_window = Some(window);
                }
                if let Some(limits) = codex_rate_limits(value) {
                    figure.rate_limits = Some(limits);
                }
            }
        } else if codex_unread_usage(value) {
            for index in placer.record(at, MeasurementReason::UnrecognizedUsage) {
                self.figures.entry(index).or_default().unrecognized_usage += 1;
            }
        }
    }

    /// A restarted total counts only the requests after the restart, so the
    /// whole the figures add up to is every request's move, `since-reset`,
    /// beside the total recorded since the restart that `usage --full`
    /// keeps. Without a restart the two are the same total.
    fn whole(&self, whole: &mut Session) -> Option<String> {
        if self.restarts == 0 {
            return None;
        }
        let (tokens, measurement_coverage) = self.moves.clone().finish_covered();
        whole.accounting = accounting_for(
            tokens.as_ref(),
            None,
            &measurement_coverage,
            AccountingBasis::RecordedTotal,
            AccountingCoverage::SinceReset,
        );
        whole.tokens = tokens;
        let detail = whole.usage_detail.get_or_insert_with(Default::default);
        detail.measurement_incomplete = known_subtotal_notes(&measurement_coverage);
        detail.measurement_coverage = measurement_coverage;
        Some(format!(
            "Codex's cumulative total restarted {} time(s); the whole here sums every \
             request's move across the restarts, while `usage --full` keeps the total \
             recorded since the last restart.",
            self.restarts
        ))
    }

    fn finish(mut self, session: &Session, whole: &Session, count: usize) -> Vec<Session> {
        (0..count)
            .map(|index| {
                let figure = self.figures.remove(&index).unwrap_or_default();
                let (tokens, measurement_coverage) = figure.tokens.finish_covered();
                let mut span = session.clone();
                span.accounting = accounting_for(
                    tokens.as_ref(),
                    None,
                    &measurement_coverage,
                    AccountingBasis::RecordedTotal,
                    if figure.since_reset {
                        AccountingCoverage::SinceReset
                    } else {
                        AccountingCoverage::Session
                    },
                );
                span.tokens = tokens;
                span.cost = None;
                span.usage_detail = UsageDetail {
                    context_window: figure.context_window,
                    rate_limits: figure.rate_limits,
                    measurement_incomplete: known_subtotal_notes(&measurement_coverage),
                    measurement_coverage,
                    unrecognized_usage: figure.unrecognized_usage,
                    ..UsageDetail::default()
                }
                .into_option();
                span.model = figure.model;
                // A span served by more than one model says so, and names no
                // split between them.
                span.model_observation =
                    (figure.models.len() > 1).then_some(ModelObservationStatus {
                        mixed: true,
                        head_read: true,
                        attribution_uncertain: true,
                        distinct_observed: Some(figure.models.len()),
                    });
                span.model_selections = Vec::new();
                crate::span::keep_identity(&mut span, whole);
                span
            })
            .collect()
    }
}

struct CodexUsageFacts {
    tokens: Option<Tokens>,
    accounting: Option<crate::model::Accounting>,
    model: Option<Model>,
    model_observation: Option<ModelObservationStatus>,
    model_selections: Vec<crate::model::ModelSelectionSpan>,
    usage_detail: Option<UsageDetail>,
    activity: ActivityRange,
    series: Option<UsageObservationSeries>,
}

fn tokens_from_object(value: &Value) -> Option<Tokens> {
    let object = value.as_object()?;
    let mut totals = TokenTotals::default();
    totals.add(
        object.get("input_tokens").and_then(Value::as_u64),
        object.get("output_tokens").and_then(Value::as_u64),
        object
            .get("reasoning_output_tokens")
            .and_then(Value::as_u64),
        object.get("cached_input_tokens").and_then(Value::as_u64),
        object
            .get("cache_write_input_tokens")
            .and_then(Value::as_u64),
    );
    totals.finish()
}

/// One observation's request counters in disjoint categories. Codex
/// records no TTL for its cache writes.
fn codex_disjoint(tokens: Option<&Tokens>) -> Option<DisjointTokens> {
    let convention = TokenConvention::of("codex")?;
    tokens.map(|tokens| DisjointTokens::from_native(tokens, convention, None))
}

fn codex_last_tokens(value: &Value) -> Option<Tokens> {
    if !is_token_count(value) {
        return None;
    }
    tokens_from_object(&value["payload"]["info"]["last_token_usage"])
}

fn supports_single_legacy_request(total: &Tokens, last: Option<&Tokens>) -> bool {
    last.is_some_and(|last| total == last)
}

enum TotalChange {
    Advance,
    Reset,
    Unchanged,
    Incomparable,
}

fn compare_totals(previous: &Tokens, current: &Tokens) -> TotalChange {
    let pairs = [
        (previous.input, current.input),
        (previous.output, current.output),
        (previous.reasoning, current.reasoning),
        (previous.cache_read, current.cache_read),
        (previous.cache_write, current.cache_write),
    ];
    let mut comparable = false;
    let mut increased = false;
    let mut decreased = false;
    for (previous, current) in pairs {
        if let (Some(previous), Some(current)) = (previous, current) {
            comparable = true;
            increased |= current > previous;
            decreased |= current < previous;
        }
    }
    if !comparable {
        TotalChange::Incomparable
    } else if decreased {
        TotalChange::Reset
    } else if increased {
        TotalChange::Advance
    } else {
        TotalChange::Unchanged
    }
}

struct CodexRecords {
    observer: CodexUsageObserver,
}

impl CodexRecords {
    fn new(path: &Path, read_window: bool) -> Self {
        Self {
            observer: CodexUsageObserver::new(path, read_window, None),
        }
    }

    fn with_series(path: &Path, read_window: bool, options: UsageObservationOptions) -> Self {
        Self {
            observer: CodexUsageObserver::new(path, read_window, Some(options.limit)),
        }
    }

    fn observe(&mut self, value: &Value, span: Option<ByteSpan>, revision: &str) {
        self.observer.observe(value, span, revision);
    }

    fn set_read_window(&mut self, read_window: bool) {
        self.observer.read_window = read_window;
    }

    fn observe_gap(&mut self, gap: &ReadGap) {
        self.observer.observe_gap(gap);
    }

    fn observe_read_gap(&mut self, gap: &ReadGap) {
        self.observer.observe_read_gap(gap);
    }

    fn observe_read(
        &mut self,
        values: &[Value],
        spans: &[ByteSpan],
        gaps: &[ReadGap],
        revision: &str,
    ) {
        let mut gaps = gaps.iter().peekable();
        for (value, span) in values.iter().zip(spans.iter().copied()) {
            while gaps.peek().is_some_and(|gap| gap.span.end <= span.start) {
                self.observe_gap(gaps.next().expect("peeked gap"));
            }
            self.observe(value, Some(span), revision);
        }
        for gap in gaps {
            self.observe_read_gap(gap);
        }
    }

    fn finish(self) -> CodexUsageFacts {
        self.observer.finish()
    }

    fn apply(self, session: &mut Session) -> Option<UsageObservationSeries> {
        let facts = self.finish();
        session.accounting = facts.accounting;
        session.tokens = facts.tokens;
        let copies = session
            .usage_detail
            .as_ref()
            .and_then(|detail| detail.copy_evidence.clone());
        session.usage_detail = facts.usage_detail;
        if let Some(copies) = copies {
            session.record_copy_evidence(copies);
        }
        session.model = facts.model;
        session.model_observation = facts.model_observation;
        session.model_selections = facts.model_selections;
        facts.activity.apply(session);
        facts.series
    }
}

/// The cumulative session totals a `token_count` event carries. Codex writes
/// one after every model response with `info.total_token_usage` as the running
/// total and `info.last_token_usage` as that response alone; the total is what
/// the normalized counters mean, so the newest event in the read window is the
/// session's accounting so far. An event without usage carries no answer and
/// is passed over for an older one. Counters Codex did not write stay absent;
/// a counter it wrote as zero is zero.
fn codex_tokens(value: &Value) -> Option<Tokens> {
    if !is_token_count(value) {
        return None;
    }
    let usage = value["payload"]["info"]["total_token_usage"].as_object()?;
    let counter = |name: &str| usage.get(name).and_then(Value::as_u64);
    let mut totals = TokenTotals::default();
    totals.add(
        counter("input_tokens"),
        counter("output_tokens"),
        counter("reasoning_output_tokens"),
        counter("cached_input_tokens"),
        counter("cache_write_input_tokens"),
    );
    totals.finish()
}

/// The context window the newest `token_count` event naming one reports.
/// Events carrying no `info` say nothing about it and are passed over.
fn codex_context_window(value: &Value) -> Option<u64> {
    is_token_count(value)
        .then(|| value["payload"]["info"]["model_context_window"].as_u64())
        .flatten()
}

/// The provider quota the newest `token_count` event carrying one observed.
/// It rides alongside `info` and describes the account rather than this
/// session, so it is reported as its own fact and never mixed into the
/// session's counters.
fn codex_rate_limits(value: &Value) -> Option<RateLimits> {
    if !is_token_count(value) {
        return None;
    }
    let limits = &value["payload"]["rate_limits"];
    let window = |name: &str| {
        let window = &limits[name];
        window["used_percent"]
            .is_number()
            .then(|| RateWindow {
                used_percent: window["used_percent"].clone(),
                window_minutes: window["window_minutes"].as_u64(),
                resets_at: window["resets_at"].as_i64().and_then(epoch_seconds),
            })
            .or_else(|| {
                window["used_percent"].as_str().map(|_| RateWindow {
                    used_percent: window["used_percent"].clone(),
                    window_minutes: window["window_minutes"].as_u64(),
                    resets_at: window["resets_at"].as_i64().and_then(epoch_seconds),
                })
            })
    };
    let credits = limits["credits"].as_object().map(|credits| Credits {
        balance: credits.get("balance").cloned(),
        has_credits: credits.get("has_credits").and_then(Value::as_bool),
        unlimited: credits.get("unlimited").and_then(Value::as_bool),
    });
    let mut limits = RateLimits {
        primary: window("primary"),
        secondary: window("secondary"),
        plan: limits["plan_type"].as_str().map(str::to_owned),
        credits: credits.filter(|credits| !credits.is_empty()),
        spend_control_reached: limits["spend_control_reached"].as_bool(),
        rate_limit_reached: limits["rate_limit_reached"].as_bool(),
        rate_limit_reached_type: limits["rate_limit_reached_type"]
            .as_str()
            .map(str::to_owned),
        observed_at: None,
    };
    if !limits.is_empty() {
        limits.observed_at = timestamp(&value["timestamp"]);
    }
    (!limits.is_empty()).then_some(limits)
}

fn epoch_seconds(seconds: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(seconds, 0)
}

/// Whether a record carries codex's usage shape, a `payload.usage` object
/// holding a token counter or a `payload.info` holding a running or
/// per-response token usage, on a record that is neither a
/// `token_usage_record` nor a `token_count` event, the two the usage
/// readings count.
fn codex_unread_usage(value: &Value) -> bool {
    let payload = &value["payload"];
    let counted = value["type"] == "token_usage_record" || is_token_count(value);
    !counted
        && (payload["usage"].as_object().is_some_and(|usage| {
            [
                "input_tokens",
                "output_tokens",
                "reasoning_output_tokens",
                "cached_input_tokens",
                "cache_write_input_tokens",
                "total_tokens",
            ]
            .iter()
            .any(|counter| usage.get(*counter).is_some_and(Value::is_number))
        }) || ["total_token_usage", "last_token_usage"]
            .iter()
            .any(|usage| payload["info"][*usage].is_object()))
}

fn is_token_count(value: &Value) -> bool {
    value["type"] == "event_msg" && value["payload"]["type"] == "token_count"
}

fn codex_terminal(value: &Value) -> Option<TerminalObservation> {
    if value["type"] != "event_msg" {
        return None;
    }
    let payload = &value["payload"];
    let payload_type = payload["type"].as_str()?;
    let explicitly_terminal = matches!(
        payload_type,
        "task_complete" | "turn_aborted" | "turn_complete" | "error"
    ) || payload["terminal"].as_bool() == Some(true)
        || payload.get("outcome").is_some()
        || payload.get("code").is_some();
    if !explicitly_terminal {
        return None;
    }
    // Codex puts the actionable failure on `error` for usage-limit task
    // completions. That nested record is the native error authority when it
    // supplies either field; the outer fields remain fallbacks for terminal
    // records written in the flatter form.
    let nested_error = payload.get("error");
    let code = nested_error
        .and_then(|error| error.get("codex_error_info"))
        .filter(|value| !value.is_null())
        .or_else(|| payload.get("code").filter(|value| !value.is_null()))
        .cloned();
    let message = nested_error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| payload.get("message").and_then(Value::as_str))
        .map(bounded_terminal_text);
    Some(TerminalObservation {
        record_type: value["type"].as_str()?.to_owned(),
        payload_type: Some(payload_type.to_owned()),
        timestamp: timestamp(&value["timestamp"]),
        native_id: payload["id"]
            .as_str()
            .or_else(|| payload["event_id"].as_str())
            .map(str::to_owned),
        turn_id: payload["turn_id"]
            .as_str()
            .or_else(|| payload["context"]["turn_id"].as_str())
            .map(str::to_owned),
        outcome: payload["outcome"]
            .as_str()
            .or_else(|| payload["status"].as_str())
            .map(str::to_owned),
        code,
        message,
        duration_ms: payload["duration_ms"].as_i64(),
    })
}

fn bounded_terminal_text(text: &str) -> crate::model::BoundedText {
    const MAX_TERMINAL_MESSAGE_CHARS: usize = 2_048;
    let chars = text.chars().count();
    crate::model::BoundedText {
        text: text.chars().take(MAX_TERMINAL_MESSAGE_CHARS).collect(),
        chars,
        truncated: chars > MAX_TERMINAL_MESSAGE_CHARS,
    }
}

fn codex_trailing_kind(value: &Value) -> Option<&'static str> {
    match value["type"].as_str()? {
        "session_meta" => Some("session_meta"),
        "turn_context" => Some("turn_context"),
        "event_msg" => Some("event_msg"),
        "world_state" => Some("world_state"),
        _ => None,
    }
}

/// Codex writes no field naming who a user message came from; the elements it
/// wraps its own text in are what separate that text from somebody's request.
/// A message whose every block is context the harness attached is ambient,
/// one that adds only a message the harness raised on its own is a notice, and
/// any other text was addressed to the agent. Each message answers from its
/// own record, so every read of it gives the same kind.
fn codex_user_kind(payload: &Value) -> TurnKind {
    let blocks = payload["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "input_text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        TurnKind::Operator
    } else if blocks.iter().all(|block| is_known_envelope(block)) {
        TurnKind::Ambient
    } else if blocks
        .iter()
        .all(|block| is_known_envelope(block) || is_known_notice(block))
    {
        TurnKind::Notice
    } else {
        TurnKind::Operator
    }
}

/// Codex records much of its tool work twice: a `response_item` the model
/// saw, and an `event_msg` item the runtime reports. Code mode records the
/// operations its `exec` script ran only as items, so an item is a tool turn
/// of its own unless a record read before it names the same operation.
/// Reading a recording's records in order through one `CodexTurns` keeps
/// each operation once.
#[derive(Default)]
struct CodexTurns {
    /// Every `id` and `call_id` a `response_item` has carried so far.
    response_ids: HashSet<String>,
    /// Runtime item ids whose `item_started` record was reached in this read.
    /// A completed item outside this set carries both halves of its own
    /// operation, because its one record contains the invocation and result.
    started_runtime_ids: HashSet<String>,
    /// The ids and actions of web searches turned into a turn whose other
    /// record has not been read yet, and whether that turn came from the item.
    /// Codex records a search both as an item and as a `web_search_call`, in
    /// either order; a record of the other kind sharing a key consumes the
    /// pending one, so a search repeated later is a turn of its own.
    searches: Vec<(bool, Vec<String>)>,
    /// The records that produced no turn.
    unmapped: UnmappedTally,
}

impl CodexTurns {
    /// The record's turns, counted as built.
    fn counted(&mut self, value: &Value) -> Vec<Turn> {
        let turns = self.parse(value);
        crate::counters::built_turns(turns.len());
        turns
    }

    fn parse(&mut self, value: &Value) -> Vec<Turn> {
        let turns = self.parse_unless_mirrored(value);
        if let Some(turns) = &turns {
            if turns.is_empty() {
                let (native_type, declined) = codex_unmapped(value);
                self.unmapped.add(native_type, declined);
            }
        } else {
            self.unmapped.add(codex_unmapped(value).0, true);
        }
        turns.unwrap_or_default()
    }

    /// The records a read represented as no turn, mirrors among them.
    fn unmapped(self) -> crate::model::UnmappedRecords {
        self.unmapped.finish()
    }

    /// The record's turns, or nothing when it mirrors a record already read.
    fn parse_unless_mirrored(&mut self, value: &Value) -> Option<Vec<Turn>> {
        let payload = &value["payload"];
        let mut completed_only = false;
        match value["type"].as_str() {
            Some("response_item") => {
                for key in ["id", "call_id"] {
                    if let Some(id) = payload[key].as_str() {
                        self.response_ids.insert(id.to_owned());
                    }
                }
                if payload["type"] == "web_search_call" && !self.first_search(payload, false) {
                    return None;
                }
            }
            Some("event_msg") => {
                let item = &payload["item"];
                if payload["type"] == "item_started" && is_codex_runtime_tool(item) {
                    if let Some(id) = item["id"].as_str() {
                        self.started_runtime_ids.insert(id.to_owned());
                    }
                } else if payload["type"] == "item_completed" && is_codex_runtime_tool(item) {
                    completed_only = item["id"]
                        .as_str()
                        .is_some_and(|id| !self.started_runtime_ids.contains(id));
                }
                if item["type"] == "WebSearch" {
                    if !self.first_search(item, true) {
                        return None;
                    }
                } else if item["id"]
                    .as_str()
                    .is_some_and(|id| self.response_ids.contains(id))
                {
                    return None;
                }
            }
            _ => {}
        }
        Some(parse_turns(value, completed_only))
    }

    /// Whether this is the first record of its web search: remembered when
    /// it is, and consuming the first record's keys when it is the second.
    fn first_search(&mut self, search: &Value, item: bool) -> bool {
        let keys = [
            search["id"].as_str().map(str::to_owned),
            (!search["action"].is_null()).then(|| search["action"].to_string()),
        ];
        let keys = keys.into_iter().flatten().collect::<Vec<_>>();
        match self.searches.iter().position(|(pending_item, pending)| {
            *pending_item != item && pending.iter().any(|key| keys.contains(key))
        }) {
            Some(first) => {
                self.searches.remove(first);
                false
            }
            None => {
                self.searches.push((item, keys));
                true
            }
        }
    }
}

/// The native type a Codex record that produced no turn is counted under —
/// `type`, then `payload.type`, then an item's `type` — and whether the reader
/// declines it: the session header and settings it reads, accounting and
/// lifecycle events, and the items that restate a message, reasoning, or
/// compaction the reader already represents.
fn codex_unmapped(value: &Value) -> (String, bool) {
    let payload = &value["payload"];
    let native_type = [
        value["type"].as_str(),
        payload["type"].as_str(),
        payload["item"]["type"].as_str(),
    ]
    .into_iter()
    .map_while(|part| part)
    .collect::<Vec<_>>()
    .join("/");
    let declined = matches!(
        native_type.as_str(),
        "session_meta"
            | "turn_context"
            | "token_usage_record"
            | "event_msg/token_count"
            | "event_msg/task_started"
            | "event_msg/task_complete"
            | "event_msg/turn_aborted"
            | "event_msg/thread_settings_applied"
            | "event_msg/item_completed/Reasoning"
            | "event_msg/item_completed/AgentMessage"
            | "event_msg/item_completed/UserMessage"
            | "event_msg/item_completed/ContextCompaction"
            | "event_msg/item_completed/Plan"
    );
    (native_type, declined)
}

fn parse_turns(value: &Value, completed_only: bool) -> Vec<Turn> {
    if value["type"] == "event_msg"
        && matches!(
            value["payload"]["type"].as_str(),
            Some("item_started" | "item_completed")
        )
    {
        let payload = &value["payload"];
        let item = &payload["item"];
        if !is_codex_runtime_tool(item) {
            return Vec::new();
        }
        let completed = payload["type"] == "item_completed";
        let event = codex_runtime_tool_event(item, completed, completed_only);
        return vec![Turn {
            origin: None,
            text_truncated: None,
            text_chars: None,
            context_event: None,
            model: None,
            role: Role::Tool,
            kind: TurnKind::Tool,
            text: item.to_string(),
            ts: timestamp(&value["timestamp"]),
            ordinal: 0,
            native_id: item["id"]
                .as_str()
                .or_else(|| payload["id"].as_str())
                .map(str::to_owned),
            request_turn_id: payload["turn_id"].as_str().map(str::to_owned),
            metadata: None,
            record_ref: None,
            channel: None,
            recipient: None,
            parts: vec![tool_part(item, "payload.item", "codex-item")],
            coverage: Some(tool_coverage()),
            tool: Some(event),
        }];
    }
    if value["type"] != "response_item" {
        return Vec::new();
    }
    let payload = &value["payload"];
    let ts = timestamp(&value["timestamp"]);
    let native_id = payload["id"].as_str().map(str::to_owned);
    let (role, kind, text, tool, parts, coverage) = match payload["type"].as_str() {
        Some("message") => {
            let role = match payload["role"].as_str() {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                Some("developer") => Role::Developer,
                _ => return Vec::new(),
            };
            let (parts, coverage) = parts_from_array(&payload["content"], "payload.content");
            let text = project_text(&parts);
            let kind = role.kind().unwrap_or_else(|| codex_user_kind(payload));
            (role, kind, text, None, parts, coverage)
        }
        Some("reasoning") => {
            let text = reasoning_text(payload);
            let parts = if text == "[encrypted reasoning]" {
                vec![crate::content::ContentPart::Unknown {
                    native_kind: "encrypted".to_owned(),
                    descriptor: crate::content::bounded_shape(payload),
                    source_field: "payload.encrypted_content".to_owned(),
                    record_ref: None,
                }]
            } else {
                vec![crate::content::text_part(
                    text.as_str(),
                    "payload.summary",
                    "reasoning",
                )]
            };
            let coverage = if text == "[encrypted reasoning]" {
                crate::content::ContentCoverage {
                    carrier: crate::content::ContentCarrier::DirectPart,
                    availability: crate::content::ContentAvailability::Unknown,
                    retained_parts: 1,
                    omitted_parts: 0,
                    omitted_reason: Some("encrypted-reasoning-placeholder".to_owned()),
                }
            } else {
                crate::content::ContentCoverage {
                    carrier: crate::content::ContentCarrier::DirectPart,
                    availability: crate::content::ContentAvailability::RetainedBody,
                    retained_parts: 1,
                    omitted_parts: 0,
                    omitted_reason: None,
                }
            };
            (
                Role::Reasoning,
                TurnKind::Reasoning,
                text,
                None,
                parts,
                coverage,
            )
        }
        Some("web_search_call") => (
            Role::Tool,
            TurnKind::Tool,
            payload.to_string(),
            Some(codex_runtime_tool_event(payload, true, false)),
            vec![tool_part(payload, "payload", "web_search_call")],
            tool_coverage(),
        ),
        Some(
            subtype @ ("function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"),
        ) => (
            Role::Tool,
            TurnKind::Tool,
            payload.to_string(),
            Some(codex_tool_event(payload, subtype)),
            vec![tool_part(payload, "payload", subtype)],
            tool_coverage(),
        ),
        _ => return Vec::new(),
    };
    ((!text.is_empty()) || !parts.is_empty())
        .then_some(Turn {
            origin: None,
            text_truncated: None,
            text_chars: None,
            context_event: None,
            model: None,
            role,
            kind,
            text,
            ts,
            ordinal: 0,
            native_id,
            request_turn_id: payload["turn_id"]
                .as_str()
                .or_else(|| payload["context"]["turn_id"].as_str())
                .map(str::to_owned),
            metadata: None,
            record_ref: None,
            channel: payload["channel"].as_str().map(str::to_owned),
            recipient: payload["recipient"].as_str().map(str::to_owned),
            parts,
            coverage: Some(coverage),
            tool,
        })
        .into_iter()
        .collect()
}

fn is_codex_runtime_tool(item: &Value) -> bool {
    // The native item variants that describe a tool operation. AgentMessage,
    // Reasoning, UserMessage, ContextCompaction, and Plan (the text of the
    // `<proposed_plan>` assistant message that follows it) mirror
    // conversation state; a tool item that mirrors a `response_item` is left
    // out by `CodexTurns`.
    matches!(
        item["type"].as_str(),
        Some(
            "CommandExecution"
                | "FileChange"
                | "McpToolCall"
                | "ImageView"
                | "Extension"
                | "WebSearch"
                | "CollabAgentToolCall"
                | "SubAgentActivity"
        )
    )
}

fn codex_tool_event(payload: &Value, subtype: &str) -> ToolEvent {
    let call = matches!(subtype, "function_call" | "custom_tool_call");
    let arguments = match subtype {
        "function_call" => Bounded::retaining_value(&payload["arguments"]),
        "custom_tool_call" => Bounded::retaining_value(&payload["input"]),
        _ => None,
    };
    let argument_value = match subtype {
        "function_call" => payload_json(&payload["arguments"]),
        "custom_tool_call" => payload_json(&payload["input"]),
        _ => Value::Null,
    };
    ToolEvent {
        kind: if call {
            EventKind::ToolCall
        } else {
            EventKind::ToolResult
        },
        subtype: subtype.to_owned(),
        name: call
            .then(|| payload["name"].as_str())
            .flatten()
            .map(str::to_owned),
        call_id: payload["call_id"].as_str().map(str::to_owned),
        status: (subtype == "custom_tool_call")
            .then(|| payload["status"].as_str())
            .flatten()
            .map(str::to_owned),
        arguments,
        output: (!call)
            .then(|| Bounded::from_value(&payload["output"]))
            .flatten(),
        read: call
            .then(|| {
                matches!(
                    payload["name"].as_str(),
                    Some("exec_command" | "shell_command")
                )
                .then(|| {
                    argument_value["cmd"]
                        .as_str()
                        .and_then(crate::event::shell_read)
                })
                .flatten()
            })
            .flatten(),
        returned_read: (!call).then(|| codex_returned_read(&payload["output"])),
        completed_ts: None,
        invocations: if call {
            crate::event::invocations_from_tool(
                payload["name"].as_str(),
                match subtype {
                    "function_call" => &payload["arguments"],
                    "custom_tool_call" => &payload["input"],
                    _ => &argument_value,
                },
                "payload.arguments",
            )
        } else {
            Vec::new()
        },
        artifact_references: if call {
            crate::event::artifact_references(&argument_value)
        } else {
            crate::event::artifact_references(&payload_json(&payload["output"]))
        },
        artifact_consumptions: Vec::new(),
        exit: None,
        self_contained: false,
    }
}

fn codex_returned_read(output: &Value) -> crate::event::ReturnedRead {
    let Some(text) = output.as_str() else {
        return crate::event::returned_read(output, None);
    };
    let status = text
        .lines()
        .find_map(|line| line.strip_prefix("Process exited with code "))
        .and_then(|code| code.parse::<i64>().ok())
        .map(|code| code == 0);
    let bytes = text
        .split_once("\nOutput:\n")
        .or_else(|| text.split_once("\nFinal output:\n"))
        .map(|(_, body)| body)
        .or_else(|| (!text.starts_with("Chunk ID:")).then_some(text));
    bytes.map_or(
        crate::event::ReturnedRead {
            succeeded: status,
            sha256: None,
            content_sha256: None,
            content_lines: None,
        },
        |bytes| crate::event::returned_read(&Value::String(bytes.to_owned()), status),
    )
}

fn codex_runtime_tool_event(item: &Value, completed: bool, completed_only: bool) -> ToolEvent {
    let arguments = runtime_arguments(item);
    ToolEvent {
        kind: if completed && !completed_only {
            EventKind::ToolResult
        } else {
            EventKind::ToolCall
        },
        subtype: item["type"].as_str().unwrap_or("codex-item").to_owned(),
        name: item["type"].as_str().map(str::to_owned),
        call_id: item["id"].as_str().map(str::to_owned),
        status: item["status"].as_str().map(str::to_owned),
        arguments: (completed_only || !completed)
            .then(|| arguments.and_then(|value| Bounded::retaining_value(&value)))
            .flatten(),
        output: completed
            .then(|| {
                [
                    "stdout",
                    "stderr",
                    "formatted_output",
                    "aggregated_output",
                    "result",
                    "results",
                    "action",
                ]
                .into_iter()
                .find_map(|key| Bounded::from_value(&item[key]))
            })
            .flatten(),
        read: (completed_only || !completed)
            .then(|| {
                item["command"]
                    .as_str()
                    .or_else(|| item["cmd"].as_str())
                    .and_then(crate::event::shell_read)
            })
            .flatten(),
        returned_read: completed.then(|| {
            crate::event::returned_read(
                &item["stdout"],
                item["exit_code"].as_i64().map(|code| code == 0),
            )
        }),
        completed_ts: completed
            .then(|| timestamp(&item["completed_at"]))
            .flatten(),
        invocations: crate::event::structured_runtime_invocations(item, "payload.item.command"),
        artifact_references: if completed {
            crate::event::artifact_references(item)
        } else {
            Vec::new()
        },
        artifact_consumptions: Vec::new(),
        exit: (completed && item["type"] == "CommandExecution")
            .then(|| item["exit_code"].as_i64())
            .flatten(),
        self_contained: completed_only,
    }
}

fn runtime_arguments(item: &Value) -> Option<Value> {
    ["command", "arguments", "input", "path", "query", "changes"]
        .into_iter()
        .find_map(|field| item.get(field).filter(|value| !value.is_null()).cloned())
}

fn reasoning_text(payload: &Value) -> String {
    let text = ["summary", "content"]
        .into_iter()
        .flat_map(|field| payload[field].as_array().into_iter().flatten())
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        text
    } else if payload["encrypted_content"].as_str().is_some() {
        "[encrypted reasoning]".to_owned()
    } else {
        String::new()
    }
}

/// Recordings whose headers the lineage read will open while looking for
/// children. Well above any store seen in practice, so it bounds a
/// pathological one without hiding a real child.
const MAX_LINEAGE_PROBES: usize = 5_000;
/// The function call Codex records when a session spawns another agent.
const SPAWN_AGENT: &str = "spawn_agent";
/// The status an agent report carries once that agent has finished.
const AGENT_COMPLETED: &str = "completed";

/// One agent a parent drove, as its own records name it. The key is the agent
/// path, which is what a child's header and the parent's outputs share.
#[derive(Default)]
struct SpawnedAgent {
    task_name: Option<String>,
    nickname: Option<String>,
    model: Option<String>,
    spawned_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    disposition: Option<String>,
    session_id: Option<String>,
    source: Vec<SourceRef>,
}

/// A child recording found by its header.
struct ChildHeader {
    thread_id: String,
    agent_path: String,
    nickname: Option<String>,
    path: PathBuf,
}

/// The agents a rollout's own records say it drove. A `spawn_agent` call
/// carries the request and its output names the path the agent runs under;
/// every call that reports on agents answers with their statuses under those
/// same paths, so an agent whose spawn is behind the read bound is still
/// named by the report that mentions it.
#[derive(Default)]
struct SpawnedAgents {
    agents: HashMap<String, SpawnedAgent>,
    /// Spawn calls whose output has not been read yet.
    spawns: HashMap<String, SpawnedAgent>,
}

impl SpawnedAgents {
    fn observe(&mut self, value: &Value) {
        if value["type"] != "response_item" {
            return;
        }
        let payload = &value["payload"];
        let ts = timestamp(&value["timestamp"]);
        let call_id = payload["call_id"].as_str().unwrap_or_default().to_owned();
        match payload["type"].as_str() {
            Some("function_call") if payload["name"] == SPAWN_AGENT => {
                let arguments = payload_json(&payload["arguments"]);
                self.spawns.insert(
                    call_id.clone(),
                    SpawnedAgent {
                        task_name: arguments["task_name"].as_str().map(str::to_owned),
                        model: arguments["model"].as_str().map(str::to_owned),
                        spawned_at: ts,
                        source: vec![SourceRef::Record { native_id: call_id }],
                        ..SpawnedAgent::default()
                    },
                );
            }
            Some("function_call_output") => {
                let output = payload_json(&payload["output"]);
                if let Some(spawn) = self.spawns.remove(&call_id) {
                    let Some(path) = output["task_name"].as_str() else {
                        return;
                    };
                    let agent = self.agents.entry(path.to_owned()).or_default();
                    agent.task_name = spawn.task_name;
                    agent.model = spawn.model;
                    agent.spawned_at = spawn.spawned_at;
                    agent.source.extend(spawn.source);
                    agent.source.push(SourceRef::Record {
                        native_id: call_id.clone(),
                    });
                }
                for reported in output["agents"].as_array().into_iter().flatten() {
                    let Some(name) = reported["agent_name"].as_str() else {
                        continue;
                    };
                    let Some(status) = reported["agent_status"].as_str() else {
                        continue;
                    };
                    let agent = self.agents.entry(name.to_owned()).or_default();
                    agent.disposition = Some(status.to_owned());
                    if status == AGENT_COMPLETED && agent.completed_at.is_none() {
                        agent.completed_at = ts;
                    }
                    agent.source.push(SourceRef::Record {
                        native_id: call_id.clone(),
                    });
                }
            }
            _ => {}
        }
    }
}

/// A Codex payload field that carries JSON as text, or as the value itself.
fn payload_json(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// The store's recordings whose headers name this session as their parent,
/// and the probe bound when the store held more than the read inspected.
/// Failed probes retain their source and I/O cause in the coverage notes.
fn child_headers(
    files: &[CandidateRecording],
    own: &Path,
    id: &str,
    notes: &mut Vec<String>,
) -> (Vec<ChildHeader>, Option<usize>) {
    let inspected = files.len().min(MAX_LINEAGE_PROBES);
    let bound = (files.len() > inspected).then_some(inspected);
    let children = files
        .iter()
        .take(inspected)
        .filter(|candidate| candidate.path.as_path() != own)
        .filter_map(|candidate| {
            let path = &candidate.path;
            let expected = &candidate.id;
            let values = match head_jsonl(path) {
                Ok(values) => values,
                Err(error) => {
                    notes.push(format!(
                        "Codex lineage candidate header {} could not be read: {error:#}",
                        path.display()
                    ));
                    return None;
                }
            };
            for value in &values {
                if let Err(error) = super::validate_native_identity(value, "codex", expected) {
                    notes.push(format!(
                        "Codex lineage candidate header {} could not be read: {error:#}",
                        path.display()
                    ));
                    return None;
                }
            }
            let payload = values
                .into_iter()
                .find(|value| value["type"] == "session_meta")
                .map(|value| value["payload"].clone())?;
            (payload["parent_thread_id"].as_str() == Some(id)).then_some(())?;
            Some(ChildHeader {
                thread_id: payload["id"].as_str()?.to_owned(),
                agent_path: payload["agent_path"].as_str()?.to_owned(),
                nickname: payload["agent_nickname"].as_str().map(str::to_owned),
                path: path.clone(),
            })
        })
        .collect();
    (children, bound)
}

/// The namespace an agent path sits in, where the harness organizes children
/// under one.
fn agent_group(path: &str) -> Option<String> {
    let (group, name) = path.rsplit_once('/')?;
    (!name.is_empty()).then(|| {
        if group.is_empty() {
            "/".to_owned()
        } else {
            group.to_owned()
        }
    })
}

/// The effort a `turn_context` names. Current rollouts write `effort`; older
/// ones wrote `reasoning_effort`, or kept it in the collaboration mode's
/// settings.
fn turn_context_effort(payload: &Value) -> Option<&str> {
    payload["effort"]
        .as_str()
        .or_else(|| payload["reasoning_effort"].as_str())
        .or_else(|| payload["collaboration_mode"]["settings"]["reasoning_effort"].as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_STORE: AtomicUsize = AtomicUsize::new(0);

    struct Store(PathBuf);

    impl Drop for Store {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn failed_child_probe(full: bool, read_failure: bool, identity_failure: bool) {
        let root = Store(std::env::temp_dir().join(format!(
            "tapes-child-probe-{}-{}",
            std::process::id(),
            NEXT_STORE.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir_all(&root.0).unwrap();
        let parent_id = "00000000-0000-0000-0000-000000000001";
        let child_id = "00000000-0000-0000-0000-000000000002";
        let parent_path = root
            .0
            .join(format!("rollout-2026-01-01T10-00-00-{parent_id}.jsonl"));
        let child_path = root
            .0
            .join(format!("rollout-2026-01-01T11-00-00-{child_id}.jsonl"));
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/codex")
                .join(parent_path.file_name().unwrap()),
            &parent_path,
        )
        .unwrap();
        fs::write(
            &child_path,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": child_id,
                        "parent_thread_id": parent_id,
                        "agent_path": "/root/worker"
                    }
                })
            ),
        )
        .unwrap();
        let backend = CodexBackend::new(&root.0);
        let parent = backend.locate(parent_id).unwrap().unwrap();
        for lineage in [backend.lineage(&parent), backend.stream_lineage(&parent)] {
            let lineage = lineage.unwrap();
            assert_eq!(lineage.children.len(), 1);
            assert!(lineage.children[0].resolved);
        }
        let (files, path, notes) = backend.lineage_inputs(&parent).unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert!(files.iter().any(|candidate| candidate.path == child_path));
        fs::remove_file(&child_path).unwrap();
        if identity_failure {
            fs::write(
                &child_path,
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "session_meta", "payload": {"id": "foreign-recording",
                            "parent_thread_id": parent_id, "agent_path": "/root/worker"}
                    })
                ),
            )
            .unwrap();
        } else if read_failure {
            fs::create_dir(&child_path).unwrap();
        }
        let lineage = if full {
            backend.stream_lineage_from_inputs(&parent, files, path, notes)
        } else {
            backend.lineage_from_inputs(&parent, files, path, notes)
        }
        .unwrap();
        assert!(lineage.children.is_empty());
        assert_eq!(lineage.notes.len(), 1, "{lineage:?}");
        assert!(lineage.notes[0].contains(&child_path.display().to_string()));
        assert!(lineage.notes[0].contains("could not be read"));
        if identity_failure {
            assert!(lineage.notes[0].contains("native session ID"));
            return;
        }
        let cause = std::io::Error::from_raw_os_error(if read_failure {
            libc::EISDIR
        } else {
            libc::ENOENT
        });
        assert!(lineage.notes[0].contains(&cause.to_string()));
    }

    #[test]
    fn bounded_lineage_retains_child_header_open_failure_after_discovery() {
        failed_child_probe(false, false, false);
    }

    #[test]
    fn streamed_lineage_retains_child_header_open_failure_after_discovery() {
        failed_child_probe(true, false, false);
    }

    #[test]
    fn bounded_lineage_retains_child_header_read_failure_after_discovery() {
        failed_child_probe(false, true, false);
    }

    #[test]
    fn streamed_lineage_retains_child_header_read_failure_after_discovery() {
        failed_child_probe(true, true, false);
    }
    #[test]
    fn child_header_identity_changes_after_discovery_are_rejected() {
        failed_child_probe(false, false, true);
        failed_child_probe(true, false, true);
    }
}
