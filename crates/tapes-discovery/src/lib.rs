//! Bounded discovery of native coding-agent session stores.
//!
//! This crate returns native identity and store location only. Transcript
//! interpretation and any delivery decision belong to its caller.

mod error;
mod file;

pub use error::DiscoveryError;
pub use file::{CandidatePage, FileIdentityBasis};

use std::env;
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

/// Store configuration captured at construction time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeStore {
    Claude {
        root: PathBuf,
    },
    Codex {
        root: PathBuf,
    },
    Pi {
        root: PathBuf,
    },
    #[cfg(feature = "opencode")]
    OpenCode(crate::opencode::OpenCodeStore),
}

impl NativeStore {
    pub fn claude(root: impl Into<PathBuf>) -> Self {
        Self::Claude { root: root.into() }
    }

    pub fn codex(root: impl Into<PathBuf>) -> Self {
        Self::Codex { root: root.into() }
    }

    pub fn pi(root: impl Into<PathBuf>) -> Self {
        Self::Pi { root: root.into() }
    }

    pub fn harness(&self) -> Harness {
        match self {
            Self::Claude { .. } => Harness::Claude,
            Self::Codex { .. } => Harness::Codex,
            Self::Pi { .. } => Harness::Pi,
            #[cfg(feature = "opencode")]
            Self::OpenCode(_) => Harness::OpenCode,
        }
    }

    pub fn root(&self) -> Option<&Path> {
        match self {
            Self::Claude { root } | Self::Codex { root } | Self::Pi { root } => Some(root),
            #[cfg(feature = "opencode")]
            Self::OpenCode(_) => None,
        }
    }

    pub fn coordinate(&self) -> String {
        match self {
            Self::Claude { root } | Self::Codex { root } | Self::Pi { root } => {
                format!("{}:{}", self.harness(), root.display())
            }
            #[cfg(feature = "opencode")]
            Self::OpenCode(store) => store.coordinate(),
        }
    }

    pub fn locate_exact(&self, id: &str) -> Result<Option<NativeSession>, DiscoveryError> {
        file::locate_exact(self, id)
    }

    pub fn candidates(&self, limit: usize) -> CandidatePage {
        file::candidates(self, limit)
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
    #[cfg(feature = "opencode")]
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

    #[cfg(feature = "opencode")]
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
            #[cfg(feature = "opencode")]
            metadata: None,
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
        if let Some(root) = env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("PI_CODING_AGENT_DIR")
                    .map(PathBuf::from)
                    .map(|path| path.join("sessions"))
            })
            .or_else(|| home.as_ref().map(|path| path.join(".pi/agent/sessions")))
        {
            stores.push(NativeStore::pi(root));
        }
        #[cfg(feature = "opencode")]
        stores.extend(crate::opencode::default_stores(home.as_deref()));
        Self::new(stores)
    }

    pub fn stores(&self) -> &[NativeStore] {
        &self.stores
    }

    pub fn resolve(&self, query: &str) -> Result<NativeSession, ResolveError> {
        resolve_native(&self.stores, query)
    }
}

/// Aggregate exact/prefix outcomes for the closed native store set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    InvalidQuery,
    NotFound {
        query: String,
        truncated: bool,
    },
    Incomplete {
        query: String,
        diagnostics: Vec<String>,
    },
    Ambiguous {
        query: String,
        candidates: Vec<NativeIdentity>,
    },
    BackendFailed {
        query: String,
        failures: Vec<String>,
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
            Self::NotFound { query, truncated } => {
                write!(f, "no native session matches {query:?}")?;
                if *truncated {
                    f.write_str("; candidate coverage is incomplete")?;
                }
                Ok(())
            }
            Self::Incomplete { query, diagnostics } => write!(
                f,
                "cannot prove a unique native session for {query:?}; use the full ID ({})",
                diagnostics.join("; ")
            ),
            Self::Ambiguous { query, candidates } => {
                write!(f, "native session query {query:?} is ambiguous among {} candidates", candidates.len())
            }
            Self::BackendFailed { query, failures } => write!(
                f,
                "native session query {query:?} could not be completed: {}",
                failures.join("; ")
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

/// Public identity projection for resolver ambiguity diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeIdentity {
    pub id: String,
    pub harness: Harness,
    pub store: String,
}

fn resolve_native(stores: &[NativeStore], query: &str) -> Result<NativeSession, ResolveError> {
    if query.trim().is_empty() || query.contains('\0') {
        return Err(ResolveError::InvalidQuery);
    }
    let mut located = Vec::new();
    let mut failures = Vec::new();
    for (index, store) in stores.iter().enumerate() {
        match store.locate_exact(query) {
            Ok(Some(session)) if session.id() == query => located.push((index, session)),
            Ok(_) => {}
            Err(error) => failures.push((index, store.harness(), error.to_string())),
        }
    }
    if let Some((index, session)) = located.iter().find(|(index, s)| {
        s.harness() == Harness::OpenCode
            && stores[..*index]
                .iter()
                .any(|earlier| earlier.harness() == s.harness())
    }) {
        let prior = failures
            .iter()
            .filter(|(failed_index, harness, _)| {
                *failed_index < *index && harness == &session.harness()
            })
            .map(|(_, _, message)| message.clone())
            .collect::<Vec<_>>();
        if !prior.is_empty() {
            return Err(ResolveError::StoreFailed {
                query: query.to_owned(),
                answered_by: session.store_coordinate().to_owned(),
                failures: prior,
            });
        }
    }
    if located.len() == 1 {
        return Ok(located.remove(0).1);
    }
    if located.len() > 1 {
        let mut candidates = located
            .into_iter()
            .map(|(_, session)| NativeIdentity {
                id: session.id().to_owned(),
                harness: session.harness(),
                store: session.store_coordinate().to_owned(),
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|a, b| a.id.cmp(&b.id).then(a.harness.cmp(&b.harness)));
        let shared = candidates
            .iter()
            .all(|candidate| candidate.harness == Harness::OpenCode);
        if shared {
            let selected_store = stores
                .iter()
                .position(|store| store.harness() == Harness::OpenCode)
                .unwrap_or_default();
            return stores[selected_store]
                .locate_exact(query)
                .map_err(|error| ResolveError::BackendFailed {
                    query: query.to_owned(),
                    failures: vec![error.to_string()],
                })?
                .ok_or_else(|| ResolveError::NotFound {
                    query: query.to_owned(),
                    truncated: false,
                });
        }
        return Err(ResolveError::Ambiguous {
            query: query.to_owned(),
            candidates,
        });
    }

    let mut matches = Vec::new();
    let mut diagnostics = Vec::new();
    let mut truncated = false;
    for store in stores {
        let page = store.candidates(1_000);
        truncated |= !page.complete;
        diagnostics.extend(page.failures.iter().map(ToString::to_string));
        matches.extend(
            page.records
                .into_iter()
                .filter(|session| session.id().starts_with(query)),
        );
    }
    matches.sort_by(|a, b| a.id().cmp(b.id()).then(a.harness().cmp(&b.harness())));
    matches.dedup_by(|a, b| a.id() == b.id() && a.harness() == b.harness());
    match matches.len() {
        0 if !diagnostics.is_empty() => Err(ResolveError::BackendFailed {
            query: query.to_owned(),
            failures: diagnostics,
        }),
        0 => Err(ResolveError::NotFound {
            query: query.to_owned(),
            truncated,
        }),
        1 if truncated || !diagnostics.is_empty() => Err(ResolveError::Incomplete {
            query: query.to_owned(),
            diagnostics,
        }),
        1 => Ok(matches.remove(0)),
        _ => Err(ResolveError::Ambiguous {
            query: query.to_owned(),
            candidates: matches
                .into_iter()
                .map(|session| NativeIdentity {
                    id: session.id().to_owned(),
                    harness: session.harness(),
                    store: session.store_coordinate().to_owned(),
                })
                .collect(),
        }),
    }
}
