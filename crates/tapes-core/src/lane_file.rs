//! A lane file: the lane's turns as the lane projection renders them, after a
//! header and a read-only margin of earlier context from the same source.
//! The lane section's body is exactly the projection of the lane's turns, so
//! its length is the lane's `projected_bytes`. Markers are whole lines; the
//! projection escapes every text line that could equal one or pass for a
//! turn heading, so the first whole line equal to a marker is the marker.

use std::fmt::Write as _;

use crate::projection::{reference, single_line};

/// Opens the margin section. No projected turn starts with `=`.
pub const MARGIN_MARKER: &str =
    "=== MARGIN: read-only context from earlier in this source; do not cite ===";
/// The whole margin section's body when no operator turn precedes the lane.
pub const EMPTY_MARGIN: &str = "(no operator turn precedes this lane)";
/// Opens the lane section; everything after this line is the lane's body.
pub const LANE_MARKER: &str = "=== LANE: the turns this lane covers ===";

/// The directory a source's lane files are written into, and how their
/// headers name the source.
#[derive(Clone, Debug)]
pub struct LaneSource {
    pub key: String,
    pub harness: String,
    pub session: String,
}

/// An operator turn's projection and the first assistant turn's after it.
#[derive(Clone, Debug)]
pub struct MarginPair {
    pub operator: String,
    pub reply: Option<String>,
}

/// Render a lane file: header, margin section, lane section.
pub fn render(
    source: &LaneSource,
    lane_id: &str,
    first_turn: usize,
    last_turn: usize,
    margin: &[MarginPair],
    body: &str,
) -> String {
    let mut out = String::with_capacity(body.len() + 1024);
    let _ = writeln!(out, "# Lane {lane_id}");
    let _ = writeln!(
        out,
        "source: {} ({}, session {})",
        source.key,
        single_line(&source.harness),
        single_line(&source.session)
    );
    let _ = writeln!(
        out,
        "turns: {}..{}",
        reference(&source.key, first_turn),
        reference(&source.key, last_turn)
    );
    out.push_str(
        "Cite only turns in the lane section, by their `<key>#<ordinal>` \
         reference copied exactly; the margin is context and is not cited.\n\n",
    );
    out.push_str(MARGIN_MARKER);
    out.push('\n');
    if margin.is_empty() {
        out.push_str(EMPTY_MARGIN);
        out.push_str("\n\n");
    }
    for pair in margin {
        out.push_str(&pair.operator);
        if let Some(reply) = &pair.reply {
            out.push_str(reply);
        }
    }
    out.push_str(LANE_MARKER);
    out.push('\n');
    out.push_str(body);
    out
}
