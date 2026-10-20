//! Read failures that can pass. A store read retries a busy or locked
//! database and a momentarily unavailable server within a small bound; a
//! read that still fails carries [`TransientFailure`], or the discovery and
//! resolution errors that say the same, so a caller can read again rather
//! than take the session for one that holds nothing.

use agent_tapes_discovery::{transient::ATTEMPTS, DiscoveryError, ResolveError};

/// A store stayed busy or unavailable through every bounded attempt.
#[derive(Debug)]
pub struct TransientFailure {
    /// The store the read reached, such as `opencode database`.
    pub source: String,
    /// What the store said, such as SQLite's `database is locked`.
    pub cause: String,
}

impl std::fmt::Display for TransientFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} busy after {ATTEMPTS} attempts, a transient failure a later read may not meet ({})",
            self.source, self.cause
        )
    }
}

impl std::error::Error for TransientFailure {}

/// Whether `error` failed for a reason that can pass, so that reading again
/// may succeed. Every cause in its chain is consulted.
pub fn is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<TransientFailure>()
            || cause
                .downcast_ref::<DiscoveryError>()
                .is_some_and(DiscoveryError::is_transient)
            || cause
                .downcast_ref::<ResolveError>()
                .is_some_and(ResolveError::is_transient)
    })
}
