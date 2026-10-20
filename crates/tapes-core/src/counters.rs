//! Process-wide tallies of reader work. A test reads them to bound what a
//! command does; nothing reads them to decide anything.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static TURNS_BUILT: AtomicUsize = AtomicUsize::new(0);
static RECORDING_PASSES: AtomicUsize = AtomicUsize::new(0);
static DECODED_BYTES: AtomicU64 = AtomicU64::new(0);

/// How many turns session parsing has built from records, across listing,
/// lookup and searched reads, since the process started.
pub fn turns_built() -> usize {
    TURNS_BUILT.load(Ordering::Relaxed)
}

pub(crate) fn built_turns(count: usize) {
    TURNS_BUILT.fetch_add(count, Ordering::Relaxed);
}

/// How many times a whole recording, or the prefix a pin names, has been
/// streamed from its first byte, since the process started.
pub fn recording_passes() -> usize {
    RECORDING_PASSES.load(Ordering::Relaxed)
}

pub(crate) fn streamed_recording() {
    RECORDING_PASSES.fetch_add(1, Ordering::Relaxed);
}

/// How many bytes of recording records streaming passes have decoded into
/// JSON values, since the process started. A pass that reads a record's
/// line without decoding it adds nothing.
pub fn decoded_bytes() -> u64 {
    DECODED_BYTES.load(Ordering::Relaxed)
}

pub(crate) fn decoded(bytes: u64) {
    DECODED_BYTES.fetch_add(bytes, Ordering::Relaxed);
}
