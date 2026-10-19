//! Bounded discovery of native coding-agent session stores.
//!
//! This crate returns native identity and store location only. Transcript
//! interpretation and any delivery decision belong to its caller.

mod error;
mod file;
mod opencode;

pub use error::DiscoveryError;
pub use file::{pi_recording_id, CandidatePage, StoreCatalog};
pub use opencode::{OpenCodeFlavor, OpenCodeStore};

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A native harness identity. OpenCode store generations share one harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    Claude,
    Codex,
    OpenCode,
    Pi,
}

impl Harness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
        }
    }
}

impl std::fmt::Display for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The evidence used to establish a file-backed canonical ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityBasis {
    Header,
    Filename,
}

/// The native storage projection selected for a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeStoreKind {
    ClaudeFiles,
    CodexFiles,
    OpenCode(OpenCodeFlavor),
    PiFiles,
}

/// Store configuration captured at construction time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeStore {
    Claude { root: PathBuf },
    Codex { root: PathBuf },
    ClaudeFile { path: PathBuf },
    CodexFile { path: PathBuf },
    Pi { root: PathBuf },
    PiFile { path: PathBuf },
    PiWithFile { root: PathBuf, path: PathBuf },
    OpenCode(OpenCodeStore),
}

impl NativeStore {
    pub fn claude(root: impl Into<PathBuf>) -> Self {
        Self::Claude { root: root.into() }
    }

    pub fn codex(root: impl Into<PathBuf>) -> Self {
        Self::Codex { root: root.into() }
    }

    /// Use one explicit Claude recording without scanning a directory.
    pub fn claude_file(path: impl Into<PathBuf>) -> Self {
        Self::ClaudeFile { path: path.into() }
    }

    /// Use one explicit Codex recording without scanning a directory.
    pub fn codex_file(path: impl Into<PathBuf>) -> Self {
        Self::CodexFile { path: path.into() }
    }

    pub fn pi(root: impl Into<PathBuf>) -> Self {
        Self::Pi { root: root.into() }
    }

    /// Use one explicit Pi recording without scanning a directory.
    pub fn pi_file(path: impl Into<PathBuf>) -> Self {
        Self::PiFile { path: path.into() }
    }

    /// Add one runtime-selected Pi recording ahead of a configured directory.
    pub fn pi_with_file(root: impl Into<PathBuf>, path: impl Into<PathBuf>) -> Self {
        Self::PiWithFile {
            root: root.into(),
            path: path.into(),
        }
    }

    pub fn opencode(program: impl Into<OsString>) -> Self {
        Self::OpenCode(OpenCodeStore::new(program))
    }

    pub fn opencode_stable(program: impl Into<OsString>) -> Self {
        Self::OpenCode(OpenCodeStore::stable(program))
    }

    pub fn opencode_v2(program: impl Into<OsString>) -> Self {
        Self::OpenCode(OpenCodeStore::v2(program))
    }

    pub fn opencode_stable_at(
        program: impl Into<OsString>,
        data_directory: impl Into<PathBuf>,
    ) -> Self {
        Self::OpenCode(OpenCodeStore::stable_at(program, data_directory))
    }

    pub fn opencode_v2_at(
        program: impl Into<OsString>,
        data_directory: impl Into<PathBuf>,
    ) -> Self {
        Self::OpenCode(OpenCodeStore::v2_at(program, data_directory))
    }

    pub fn harness(&self) -> Harness {
        match self {
            Self::Claude { .. } | Self::ClaudeFile { .. } => Harness::Claude,
            Self::Codex { .. } | Self::CodexFile { .. } => Harness::Codex,
            Self::Pi { .. } | Self::PiFile { .. } | Self::PiWithFile { .. } => Harness::Pi,
            Self::OpenCode(_) => Harness::OpenCode,
        }
    }

    /// Distinguish store generations that share one harness.
    pub fn store_label(&self) -> Option<&'static str> {
        match self {
            Self::OpenCode(store) => Some(match store.flavor() {
                OpenCodeFlavor::Stable => "stable",
                OpenCodeFlavor::V2 => "v2",
            }),
            _ => None,
        }
    }

    pub fn root(&self) -> Option<&Path> {
        match self {
            Self::Claude { root }
            | Self::Codex { root }
            | Self::Pi { root }
            | Self::PiWithFile { root, .. } => Some(root),
            Self::ClaudeFile { .. }
            | Self::CodexFile { .. }
            | Self::PiFile { .. }
            | Self::OpenCode(_) => None,
        }
    }

    pub(crate) fn file(&self) -> Option<&Path> {
        match self {
            Self::ClaudeFile { path }
            | Self::CodexFile { path }
            | Self::PiFile { path }
            | Self::PiWithFile { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn coordinate(&self) -> String {
        match self {
            Self::Claude { root } | Self::Codex { root } | Self::Pi { root } => {
                format!("{}:{}", self.harness(), root.display())
            }
            Self::ClaudeFile { path } | Self::CodexFile { path } | Self::PiFile { path } => {
                format!("{}:{}", self.harness(), path.display())
            }
            Self::PiWithFile { root, path } => {
                format!(
                    "{}:{} plus {}",
                    self.harness(),
                    root.display(),
                    path.display()
                )
            }
            Self::OpenCode(store) => store.coordinate(),
        }
    }

    pub fn kind(&self) -> NativeStoreKind {
        match self {
            Self::Claude { .. } | Self::ClaudeFile { .. } => NativeStoreKind::ClaudeFiles,
            Self::Codex { .. } | Self::CodexFile { .. } => NativeStoreKind::CodexFiles,
            Self::Pi { .. } | Self::PiFile { .. } | Self::PiWithFile { .. } => {
                NativeStoreKind::PiFiles
            }
            Self::OpenCode(store) => NativeStoreKind::OpenCode(store.flavor()),
        }
    }

    pub fn opencode_store(&self) -> Option<&OpenCodeStore> {
        match self {
            Self::OpenCode(store) => Some(store),
            _ => None,
        }
    }

    /// Whether the selected native source has a store to inspect.
    pub fn available(&self) -> bool {
        match self {
            Self::OpenCode(store) => store.available(),
            _ => file::available(self),
        }
    }

    pub fn locate_exact(&self, id: &str) -> Result<Option<NativeSession>, DiscoveryError> {
        match self {
            Self::OpenCode(store) => store.locate_exact(id),
            _ => file::locate_exact(self, id),
        }
    }

    pub fn candidates(&self, limit: usize) -> CandidatePage {
        match self {
            Self::OpenCode(store) => store.candidates(limit),
            _ => file::candidates(self, limit),
        }
    }

    /// As [`NativeStore::locate_exact`], answering from `catalog`'s walk of a
    /// file-backed store: a recording the walk did not see is absent. Joins
    /// that expect misses use this.
    pub fn locate_exact_in(
        &self,
        catalog: &StoreCatalog,
        id: &str,
    ) -> Result<Option<NativeSession>, DiscoveryError> {
        match self {
            Self::OpenCode(store) => store.locate_exact(id),
            _ => file::locate_exact_in(self, catalog, id),
        }
    }

    /// As [`NativeStore::locate_exact`] for an id a caller named: a hit
    /// answers from `catalog`'s walk, and a miss in a walk an earlier call
    /// took walks again once and answers from the new walk, which replaces
    /// the catalog's.
    pub fn locate_named_in(
        &self,
        catalog: &StoreCatalog,
        id: &str,
    ) -> Result<Option<NativeSession>, DiscoveryError> {
        match self {
            Self::OpenCode(store) => store.locate_exact(id),
            _ => file::locate_named_in(self, catalog, id),
        }
    }

    /// As [`NativeStore::candidates`], walking a file-backed store at most
    /// once per `catalog`.
    pub fn candidates_in(&self, catalog: &StoreCatalog, limit: usize) -> CandidatePage {
        match self {
            Self::OpenCode(store) => store.candidates(limit),
            _ => file::candidates_in(self, catalog, limit),
        }
    }

    /// As [`NativeStore::candidates_in`], with the generation of the
    /// catalog walk the candidates came from: a reader that keeps something
    /// derived from them keeps it while [`StoreCatalog::generation`] is the
    /// same. A store without a walk has none.
    pub fn candidates_in_walk(
        &self,
        catalog: &StoreCatalog,
        limit: usize,
    ) -> (CandidatePage, Option<usize>) {
        match self {
            Self::OpenCode(store) => (store.candidates(limit), None),
            _ => file::candidates_in_walk(self, catalog, limit),
        }
    }
}

/// A canonical native session with its originating store and native locator.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeSession {
    id: String,
    harness: Harness,
    store: String,
    locator: Option<PathBuf>,
    identity_basis: Option<IdentityBasis>,
    store_kind: NativeStoreKind,
    metadata: Option<serde_json::Value>,
}

impl NativeSession {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn harness(&self) -> Harness {
        self.harness
    }

    pub fn store_coordinate(&self) -> &str {
        &self.store
    }

    pub fn locator(&self) -> Option<&Path> {
        self.locator.as_deref()
    }

    pub fn identity_basis(&self) -> Option<IdentityBasis> {
        self.identity_basis
    }

    pub fn store_kind(&self) -> NativeStoreKind {
        self.store_kind
    }

    /// What the store recorded about the session beside its identity: an
    /// OpenCode session row, or the lineage fields of a Codex child
    /// rollout's header (`parent_thread_id`, `agent_path`,
    /// `agent_nickname`).
    pub fn metadata(&self) -> Option<&serde_json::Value> {
        self.metadata.as_ref()
    }

    fn file(
        id: String,
        harness: Harness,
        store: String,
        locator: PathBuf,
        identity_basis: IdentityBasis,
    ) -> Self {
        Self {
            id,
            harness,
            store,
            locator: Some(locator),
            identity_basis: Some(identity_basis),
            store_kind: match harness {
                Harness::Claude => NativeStoreKind::ClaudeFiles,
                Harness::Codex => NativeStoreKind::CodexFiles,
                Harness::OpenCode => NativeStoreKind::OpenCode(OpenCodeFlavor::Stable),
                Harness::Pi => NativeStoreKind::PiFiles,
            },
            metadata: None,
        }
    }

    /// The session with the lineage fields its recording's header names,
    /// which [`NativeSession::metadata`] then answers with.
    pub(crate) fn with_lineage(mut self, lineage: Option<serde_json::Value>) -> Self {
        if lineage.is_some() {
            self.metadata = lineage;
        }
        self
    }

    fn opencode(
        id: String,
        store: String,
        flavor: OpenCodeFlavor,
        metadata: serde_json::Value,
    ) -> Self {
        Self {
            id,
            harness: Harness::OpenCode,
            store,
            locator: None,
            identity_basis: None,
            store_kind: NativeStoreKind::OpenCode(flavor),
            metadata: Some(metadata),
        }
    }
}

/// Native store collection. It never accepts caller-provided recording rows.
#[derive(Clone, Debug, Default)]
pub struct Discovery {
    stores: Vec<NativeStore>,
}

impl Discovery {
    pub fn new(stores: impl IntoIterator<Item = NativeStore>) -> Self {
        Self {
            stores: stores.into_iter().collect(),
        }
    }

    pub fn from_env() -> Self {
        let home = env::var_os("HOME").map(PathBuf::from);
        let mut stores = Vec::new();
        if let Some(root) = env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .map(|path| path.join("projects"))
            .or_else(|| home.as_ref().map(|path| path.join(".claude/projects")))
        {
            stores.push(NativeStore::claude(root));
        }
        if let Some(root) = env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .map(|path| path.join("sessions"))
            .or_else(|| home.as_ref().map(|path| path.join(".codex/sessions")))
        {
            stores.push(NativeStore::codex(root));
        }
        let pi_root = env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("PI_CODING_AGENT_DIR")
                    .map(PathBuf::from)
                    .map(|path| path.join("sessions"))
            })
            .or_else(|| home.as_ref().map(|path| path.join(".pi/agent/sessions")));
        let pi_file = env::var_os("PI_SESSION_FILE").filter(|path| !path.is_empty());
        match (pi_root, pi_file) {
            (Some(root), Some(path)) => {
                stores.push(NativeStore::pi_with_file(root, PathBuf::from(path)));
            }
            (Some(root), None) => stores.push(NativeStore::pi(root)),
            (None, Some(path)) => stores.push(NativeStore::pi_file(PathBuf::from(path))),
            (None, None) => {}
        }
        stores.extend(
            opencode::default_stores()
                .into_iter()
                .map(NativeStore::OpenCode),
        );
        Self::new(stores)
    }

    pub fn stores(&self) -> &[NativeStore] {
        &self.stores
    }

    pub fn resolve(&self, query: &str) -> Result<NativeSession, ResolveError> {
        let sources = self
            .stores
            .iter()
            .map(|store| store as &dyn IdentitySource<NativeSession>)
            .collect::<Vec<_>>();
        resolve_with_sources(&sources, query).map(|resolved| resolved.record)
    }
}

/// Candidate enumeration coverage for one present or unreadable store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookupCoverage {
    pub harness: String,
    /// Store generation label, when the source supplies one.
    pub store: Option<String>,
    /// Whether candidate enumeration covered the entire store.
    pub complete: bool,
}

/// Aggregate exact/prefix outcomes for the closed native store set.
/// Failure and diagnostic vectors contain causes; coverage records searches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    InvalidQuery,
    NotFound {
        query: String,
        truncated: bool,
        diagnostics: Vec<String>,
        coverage: Vec<LookupCoverage>,
    },
    Incomplete {
        query: String,
        diagnostics: Vec<String>,
        coverage: Vec<LookupCoverage>,
    },
    Ambiguous {
        query: String,
        candidates: Vec<IdentitySummary>,
    },
    BackendFailed {
        query: String,
        failures: Vec<String>,
        coverage: Vec<LookupCoverage>,
    },
    StoreFailed {
        query: String,
        answered_by: String,
        failures: Vec<String>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidQuery => f.write_str("query must be non-empty and contain no NUL"),
            Self::NotFound { query, truncated, diagnostics, coverage } => {
                write!(f, "session {query} was not found")?;
                if *truncated {
                    f.write_str(
                        " (candidate coverage is incomplete; pass the full ID or --harness <h> for a direct lookup)",
                    )?;
                }
                let details = lookup_diagnostics(diagnostics, coverage);
                if !details.is_empty() {
                    write!(f, " ({details})")?;
                }
                Ok(())
            }
            Self::Incomplete { query, diagnostics, coverage } => write!(
                f,
                "cannot prove a unique native session for {query:?}; use the full ID or --harness <h> ({})",
                lookup_diagnostics(diagnostics, coverage)
            ),
            Self::Ambiguous { query, candidates } => {
                write!(f, "native session query {query:?} is ambiguous among {} candidates", candidates.len())
            }
            Self::BackendFailed { query, failures, coverage } => write!(
                f,
                "native session query {query:?} could not be completed: {}",
                lookup_diagnostics(failures, coverage)
            ),
            Self::StoreFailed { query, answered_by, failures } => write!(
                f,
                "native session query {query:?} could not use preceding store before {answered_by}: {}",
                failures.join("; ")
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

fn lookup_diagnostics(causes: &[String], coverage: &[LookupCoverage]) -> String {
    let mut details = causes.to_vec();
    if coverage.iter().any(|store| !store.complete) {
        for (complete, label) in [
            (false, "incomplete harness stores"),
            (true, "completely searched harness stores"),
        ] {
            let stores = coverage
                .iter()
                .filter(|store| store.complete == complete)
                .map(|store| {
                    let shared_harness = coverage
                        .iter()
                        .filter(|other| other.harness == store.harness)
                        .count()
                        > 1;
                    match store.store.as_deref().filter(|_| shared_harness) {
                        Some(label) => format!("{} ({label})", store.harness),
                        None => store.harness.clone(),
                    }
                })
                .collect::<Vec<_>>();
            details.push(format!(
                "{label}: {}",
                if stores.is_empty() {
                    "none".to_owned()
                } else {
                    stores.join(", ")
                }
            ));
        }
        details.push("use the full ID or --harness <h> to scope native lookup".to_owned());
    }
    details.join("; ")
}

/// Public identity projection for resolver ambiguity diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentitySummary {
    pub id: String,
    pub harness: String,
    pub store: Option<String>,
}

/// Identity fields shared by installed stores and core's supplied-source rows.
pub trait IdentityRecord {
    fn id(&self) -> &str;
    fn harness(&self) -> &str;
    fn store_coordinate(&self) -> Option<&str> {
        None
    }
}

impl IdentityRecord for NativeSession {
    fn id(&self) -> &str {
        self.id()
    }

    fn harness(&self) -> &str {
        self.harness.as_str()
    }

    fn store_coordinate(&self) -> Option<&str> {
        Some(self.store_coordinate())
    }
}

/// An exact lookup error with its prefix-fallback policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExactLookupFailure {
    pub message: String,
    pub refuses_prefix: bool,
}

/// A source adapted to the shared exact/prefix selector.
pub trait IdentitySource<R: IdentityRecord> {
    fn harness(&self) -> &str;
    /// A human-readable distinction between stores of the same harness.
    fn store_label(&self) -> Option<&str> {
        None
    }
    fn locate_exact(&self, query: &str) -> Result<Option<R>, String>;
    /// Preserve failures that prefix candidates cannot answer. Correct exact
    /// records still take precedence over these failures.
    fn locate_for_resolution(&self, query: &str) -> Result<Option<R>, ExactLookupFailure> {
        self.locate_exact(query)
            .map_err(|message| ExactLookupFailure {
                message,
                refuses_prefix: false,
            })
    }
    fn candidates(&self, limit: usize) -> CandidatePage<R, String>;
    fn shares_session_ids(&self) -> bool {
        false
    }
    fn terminal_exact_failure(&self) -> bool {
        false
    }
    fn available(&self) -> bool {
        true
    }
}

/// Enforce first-store identity precedence for projections of one harness.
///
/// Callers retain their own listing, filtering, and diagnostic rules; this
/// type only decides whether a later store may supply an identity.
#[derive(Clone, Debug, Default)]
pub struct SharedStorePrecedence {
    listed: HashMap<(String, String), usize>,
    unreadable: HashMap<(String, String), usize>,
    failed: HashMap<String, usize>,
    emitted: HashSet<(String, String)>,
}

impl SharedStorePrecedence {
    /// Reserve an identity observed in an earlier shared-store listing.
    pub fn record_listed(&mut self, harness: &str, shares_ids: bool, store: usize, id: &str) {
        if !shares_ids {
            return;
        }
        let index = self
            .listed
            .entry((harness.to_owned(), id.to_owned()))
            .or_insert(store);
        *index = (*index).min(store);
    }

    /// Reserve an identity an earlier shared store could not read.
    pub fn record_unreadable(&mut self, harness: &str, shares_ids: bool, store: usize, id: &str) {
        if !shares_ids {
            return;
        }
        let index = self
            .unreadable
            .entry((harness.to_owned(), id.to_owned()))
            .or_insert(store);
        *index = (*index).min(store);
    }

    /// Mark a shared store whose failed listing may hide any identity.
    pub fn record_store_failure(&mut self, harness: &str, shares_ids: bool, store: usize) {
        if !shares_ids {
            return;
        }
        let index = self.failed.entry(harness.to_owned()).or_insert(store);
        *index = (*index).min(store);
    }

    /// Whether an earlier shared store failed before this projection.
    pub fn has_prior_store_failure(&self, harness: &str, shares_ids: bool, store: usize) -> bool {
        shares_ids
            && self
                .failed
                .get(harness)
                .is_some_and(|failed| *failed < store)
    }

    /// Whether an earlier shared store failed or had an unreadable matching ID.
    pub fn blocks_later_store(
        &self,
        harness: &str,
        shares_ids: bool,
        store: usize,
        id: &str,
    ) -> bool {
        if !shares_ids {
            return false;
        }
        self.has_prior_store_failure(harness, true, store)
            || self
                .unreadable
                .get(&(harness.to_owned(), id.to_owned()))
                .is_some_and(|failed| *failed < store)
    }

    /// Admit one identity from its first shared store, once per harness.
    pub fn admits(&mut self, harness: &str, shares_ids: bool, store: usize, id: &str) -> bool {
        if !shares_ids {
            return true;
        }
        if self.blocks_later_store(harness, true, store, id) {
            return false;
        }
        let key = (harness.to_owned(), id.to_owned());
        let index = self.listed.entry(key.clone()).or_insert(store);
        if *index < store {
            return false;
        }
        *index = (*index).min(store);
        self.emitted.insert(key)
    }
}

impl IdentitySource<NativeSession> for NativeStore {
    fn store_label(&self) -> Option<&str> {
        NativeStore::store_label(self)
    }

    fn harness(&self) -> &str {
        self.harness().as_str()
    }

    fn locate_exact(&self, query: &str) -> Result<Option<NativeSession>, String> {
        NativeStore::locate_exact(self, query).map_err(|error| error.to_string())
    }

    fn locate_for_resolution(
        &self,
        query: &str,
    ) -> Result<Option<NativeSession>, ExactLookupFailure> {
        NativeStore::locate_exact(self, query).map_err(|error| ExactLookupFailure {
            refuses_prefix: matches!(error, DiscoveryError::IdentityMismatch { .. }),
            message: error.to_string(),
        })
    }

    fn candidates(&self, limit: usize) -> CandidatePage<NativeSession, String> {
        let page = NativeStore::candidates(self, limit);
        CandidatePage {
            records: page.records,
            scanned: page.scanned,
            visited_entries: page.visited_entries,
            complete: page.complete,
            failures: page
                .failures
                .into_iter()
                .map(|error| error.to_string())
                .collect(),
            unreadable_ids: page.unreadable_ids,
        }
    }

    fn shares_session_ids(&self) -> bool {
        self.harness() == Harness::OpenCode
    }
}

/// A successful resolution carries the source that owns the selected record.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved<R> {
    pub source_index: usize,
    pub record: R,
}

/// Resolve exact IDs before prefixes and preserve uncertainty across sources.
pub fn resolve_with_sources<R: IdentityRecord + Clone>(
    sources: &[&dyn IdentitySource<R>],
    query: &str,
) -> Result<Resolved<R>, ResolveError> {
    if query.trim().is_empty() || query.contains('\0') {
        return Err(ResolveError::InvalidQuery);
    }
    let mut exact = Vec::<Resolved<R>>::new();
    let mut refuses_prefix = false;
    let mut failures = Vec::<(usize, String, String)>::new();
    let mut precedence = SharedStorePrecedence::default();
    for (source_index, source) in sources.iter().enumerate() {
        match source.locate_for_resolution(query) {
            Ok(Some(record)) if record.id() == query => exact.push(Resolved {
                source_index,
                record,
            }),
            Ok(Some(_)) => {
                refuses_prefix = true;
                if source.shares_session_ids() {
                    precedence.record_store_failure(source.harness(), true, source_index);
                }
                failures.push((
                    source_index,
                    source.harness().to_owned(),
                    "exact lookup returned a nonmatching canonical ID".to_owned(),
                ));
            }
            Ok(None) => {}
            Err(error) => {
                if source.shares_session_ids() {
                    precedence.record_store_failure(source.harness(), true, source_index);
                }
                refuses_prefix |= error.refuses_prefix;
                failures.push((source_index, source.harness().to_owned(), error.message));
            }
        }
    }

    let mut admitted_exact = Vec::with_capacity(exact.len());
    for hit in exact {
        let source = sources[hit.source_index];
        if source.shares_session_ids() {
            if precedence.has_prior_store_failure(source.harness(), true, hit.source_index) {
                let prior = failures
                    .iter()
                    .filter(|(index, harness, _)| {
                        *index < hit.source_index && harness == source.harness()
                    })
                    .map(|(_, _, failure)| failure.clone())
                    .collect::<Vec<_>>();
                return Err(ResolveError::StoreFailed {
                    query: query.to_owned(),
                    answered_by: hit
                        .record
                        .store_coordinate()
                        .unwrap_or_else(|| source.harness())
                        .to_owned(),
                    failures: prior,
                });
            }
            if !precedence.admits(source.harness(), true, hit.source_index, hit.record.id()) {
                continue;
            }
        }
        admitted_exact.push(hit);
    }
    let mut exact = admitted_exact;

    if exact.len() == 1 {
        return Ok(exact.remove(0));
    }
    if exact.len() > 1 {
        let all_shared = exact.iter().all(|hit| {
            sources[hit.source_index].shares_session_ids()
                && hit.record.harness() == exact[0].record.harness()
        });
        if all_shared {
            exact.sort_by_key(|hit| hit.source_index);
            return Ok(exact.remove(0));
        }
        return Err(ResolveError::Ambiguous {
            query: query.to_owned(),
            candidates: identity_summaries(
                exact
                    .iter()
                    .map(|hit| (&hit.record as &dyn IdentityRecord, hit.source_index)),
            ),
        });
    }
    if failures
        .iter()
        .any(|(index, _, _)| sources[*index].terminal_exact_failure())
    {
        return Err(ResolveError::BackendFailed {
            query: query.to_owned(),
            failures: failure_messages(failures.iter()),
            coverage: Vec::new(),
        });
    }

    let mut pages = Vec::with_capacity(sources.len());
    let mut diagnostics = Vec::new();
    let mut truncated = false;
    for (source_index, source) in sources.iter().enumerate() {
        if !source.available() {
            let exact_failures = failures
                .iter()
                .filter(|(index, _, _)| *index == source_index)
                .map(|(_, harness, failure)| format!("{harness}: {failure}"))
                .collect::<Vec<_>>();
            let complete = exact_failures.is_empty();
            if !complete {
                truncated = true;
                diagnostics.extend(exact_failures.iter().cloned());
            }
            pages.push(CandidatePage::<R, String> {
                records: Vec::new(),
                scanned: 0,
                visited_entries: 0,
                complete,
                failures: exact_failures,
                unreadable_ids: Vec::new(),
            });
            continue;
        }
        let page = source.candidates(1_000);
        truncated |= !page.complete;
        diagnostics.extend(page.failures.iter().cloned());
        pages.push(page);
    }
    let coverage = pages
        .iter()
        .enumerate()
        .filter(|(index, page)| !page.complete || sources[*index].available())
        .map(|(index, page)| LookupCoverage {
            harness: sources[index].harness().to_owned(),
            store: sources[index].store_label().map(str::to_owned),
            complete: page.complete,
        })
        .collect::<Vec<_>>();
    for (source_index, page) in pages.iter().enumerate() {
        let source = sources[source_index];
        if !source.shares_session_ids() {
            continue;
        }
        if !page.failures.is_empty() {
            precedence.record_store_failure(source.harness(), true, source_index);
        }
        for id in &page.unreadable_ids {
            precedence.record_unreadable(source.harness(), true, source_index, id);
        }
    }
    let mut matches = Vec::<Resolved<R>>::new();
    for (source_index, page) in pages.iter().enumerate() {
        for record in &page.records {
            if !record.id().starts_with(query) {
                continue;
            }
            let source = sources[source_index];
            if source.shares_session_ids() {
                if precedence.blocks_later_store(source.harness(), true, source_index, record.id())
                {
                    let mut prior_failure = failures
                        .iter()
                        .filter(|(index, harness, _)| {
                            *index < source_index && harness == source.harness()
                        })
                        .map(|(_, _, failure)| failure.clone())
                        .collect::<Vec<_>>();
                    for earlier in 0..source_index {
                        if sources[earlier].harness() != source.harness() {
                            continue;
                        }
                        if pages[earlier]
                            .unreadable_ids
                            .iter()
                            .any(|id| id == record.id())
                        {
                            prior_failure.push(format!(
                                "{} store has an unreadable row for the same ID",
                                sources[earlier].harness()
                            ));
                        }
                        if !pages[earlier].failures.is_empty() {
                            prior_failure.extend(pages[earlier].failures.iter().cloned());
                        }
                    }
                    if !prior_failure.is_empty() {
                        return Err(ResolveError::StoreFailed {
                            query: query.to_owned(),
                            answered_by: record
                                .store_coordinate()
                                .unwrap_or_else(|| source.harness())
                                .to_owned(),
                            failures: prior_failure,
                        });
                    }
                }
                if !precedence.admits(source.harness(), true, source_index, record.id()) {
                    continue;
                }
            }
            matches.push(Resolved {
                source_index,
                record: record.clone(),
            });
        }
    }

    matches.sort_by(|left, right| {
        left.record
            .id()
            .cmp(right.record.id())
            .then(left.record.harness().cmp(right.record.harness()))
    });
    let exact_matches = matches
        .iter()
        .filter(|hit| hit.record.id() == query)
        .collect::<Vec<_>>();
    if exact_matches.len() == 1 {
        return Ok(exact_matches[0].clone());
    }
    if exact_matches.len() > 1 {
        let shared = exact_matches.iter().all(|hit| {
            sources[hit.source_index].shares_session_ids()
                && hit.record.harness() == exact_matches[0].record.harness()
        });
        if shared {
            return Ok(exact_matches
                .into_iter()
                .min_by_key(|hit| hit.source_index)
                .expect("exact hits exist")
                .clone());
        }
        return Err(ResolveError::Ambiguous {
            query: query.to_owned(),
            candidates: identity_summaries(
                exact_matches
                    .iter()
                    .map(|hit| (&hit.record as &dyn IdentityRecord, hit.source_index)),
            ),
        });
    }
    if refuses_prefix {
        return Err(ResolveError::BackendFailed {
            query: query.to_owned(),
            failures: failure_messages(failures.iter()),
            coverage,
        });
    }
    if matches.len() > 1 {
        return Err(ResolveError::Ambiguous {
            query: query.to_owned(),
            candidates: identity_summaries(
                matches
                    .iter()
                    .map(|hit| (&hit.record as &dyn IdentityRecord, hit.source_index)),
            ),
        });
    }
    match matches.len() {
        0 if !diagnostics.is_empty() || !failures.is_empty() => {
            let mut failures = failure_messages(failures.iter());
            failures.extend(diagnostics);
            failures.sort();
            failures.dedup();
            Err(ResolveError::BackendFailed {
                query: query.to_owned(),
                failures,
                coverage,
            })
        }
        0 => Err(ResolveError::NotFound {
            query: query.to_owned(),
            truncated,
            diagnostics: Vec::new(),
            coverage,
        }),
        1 if truncated || !diagnostics.is_empty() => Err(ResolveError::Incomplete {
            query: query.to_owned(),
            diagnostics,
            coverage,
        }),
        1 => Ok(matches.remove(0)),
        _ => unreachable!("multiple matches returned above"),
    }
}

fn identity_summaries<'a>(
    identities: impl Iterator<Item = (&'a dyn IdentityRecord, usize)>,
) -> Vec<IdentitySummary> {
    let mut candidates = identities
        .map(|(record, index)| IdentitySummary {
            id: record.id().to_owned(),
            harness: record.harness().to_owned(),
            store: record
                .store_coordinate()
                .map(str::to_owned)
                .or_else(|| Some(format!("source {index}"))),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then(left.harness.cmp(&right.harness))
            .then(left.store.cmp(&right.store))
    });
    candidates
}

fn failure_messages<'a>(
    failures: impl Iterator<Item = &'a (usize, String, String)>,
) -> Vec<String> {
    failures
        .map(|(_, harness, failure)| format!("{harness}: {failure}"))
        .collect()
}

/// Open without waiting for a FIFO writer, then inspect the actual source.
fn open_regular_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    Ok(file)
}
