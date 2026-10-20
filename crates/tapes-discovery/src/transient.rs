//! Store failures that can pass: a database another connection holds busy or
//! locked, or a server that answered that it is momentarily unavailable.
//!
//! A read retries such a failure a bounded number of times. One that still
//! fails is reported as transient, so a caller can tell it from a failure
//! that cannot pass and from a session that holds nothing.

use std::time::Duration;

/// Attempts a read makes at a failure that can pass, the first included.
pub const ATTEMPTS: usize = 4;

/// The pause before each attempt after the first; the bound on the time a
/// read spends waiting out a busy store.
const RETRY_DELAYS: [Duration; ATTEMPTS - 1] = [
    Duration::from_millis(25),
    Duration::from_millis(75),
    Duration::from_millis(225),
];

/// The pause before attempt `attempt`, counted from zero for the first.
pub fn retry_delay(attempt: usize) -> Duration {
    RETRY_DELAYS[attempt.saturating_sub(1).min(RETRY_DELAYS.len() - 1)]
}

/// SQLite's messages for its busy and locked results. The sqlite3 shell and
/// the SQLite bindings OpenCode runs print them on stderr.
const SQLITE_BUSY_MESSAGES: [&str; 5] = [
    "database is locked",
    "database table is locked",
    "database schema is locked",
    "SQLITE_BUSY",
    "SQLITE_LOCKED",
];

/// The stderr line of a failed store command that says SQLite was busy or
/// locked, trimmed, if one does.
pub fn busy_output(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .find(|line| {
            SQLITE_BUSY_MESSAGES
                .iter()
                .any(|message| line.contains(message))
        })
        .map(str::to_owned)
}

/// Whether an HTTP status says the server is momentarily unavailable.
pub fn transient_http_status(status: u16) -> bool {
    matches!(status, 429 | 503)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_output_names_the_sqlite_line_and_ignores_other_failures() {
        assert_eq!(
            busy_output(b"Parse error in 6th command line argument: database is locked (5)\n")
                .as_deref(),
            Some("Parse error in 6th command line argument: database is locked (5)")
        );
        assert_eq!(busy_output(b"Error: no such table: message\n"), None);
        assert_eq!(busy_output(b""), None);
    }

    #[test]
    fn retry_delays_grow_and_stay_bounded() {
        assert_eq!(retry_delay(1), Duration::from_millis(25));
        assert_eq!(retry_delay(3), Duration::from_millis(225));
        assert_eq!(retry_delay(9), Duration::from_millis(225));
    }
}
