//! Process-wide tallies of reader work. A test reads them to bound what a
//! command does; nothing reads them to decide anything.

use std::sync::atomic::{AtomicUsize, Ordering};

static TURNS_BUILT: AtomicUsize = AtomicUsize::new(0);

/// How many turns session parsing has built from records, across listing,
/// lookup and searched reads, since the process started.
pub fn turns_built() -> usize {
    TURNS_BUILT.load(Ordering::Relaxed)
}

pub(crate) fn built_turns(count: usize) {
    TURNS_BUILT.fetch_add(count, Ordering::Relaxed);
}
