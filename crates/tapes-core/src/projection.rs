//! The lane projection: one turn rendered as the text a lane carries. Lane
//! sizes are measured in the bytes this rendering produces, so a lane cut and
//! the lane file written from it cannot disagree.

use std::fmt::Write as _;

use crate::content::{ArtifactReference, ContentPart};
use crate::model::Turn;

/// The workset reference a turn is cited by: `<key>#<ordinal>`.
pub fn reference(key: &str, ordinal: usize) -> String {
    format!("{key}#{ordinal}")
}

/// Render one turn deterministically: a header naming its workset reference,
/// kind and origin; a compact line for a tool call or result with the
/// artifact references it carries; then the turn's text. Every rendering ends
/// with a blank line, so concatenated turns stay separated.
pub fn project_turn(key: &str, turn: &Turn) -> String {
    let mut out = format!("## {} {}", reference(key, turn.ordinal), turn.kind.label());
    if let Some(origin) = turn.origin {
        let _ = write!(out, " ({})", origin.label());
    }
    out.push('\n');
    if let Some(tool) = &turn.tool {
        out.push_str("tool:");
        let kind = match tool.kind {
            crate::EventKind::ToolCall => "call",
            crate::EventKind::ToolResult => "result",
        };
        let _ = write!(out, " {kind}");
        for field in [&tool.name, &tool.call_id, &tool.status]
            .into_iter()
            .flatten()
        {
            let _ = write!(out, " {field}");
        }
        if let Some(exit) = tool.exit {
            let _ = write!(out, " exit={exit}");
        }
        for artifact in tool.artifact_references.iter().filter_map(artifact_token) {
            let _ = write!(out, " artifact={artifact}");
        }
        out.push('\n');
    }
    for part in &turn.parts {
        if let ContentPart::FileReference { reference, .. } = part {
            if let Some(token) = artifact_token(reference) {
                let _ = writeln!(out, "artifact: {token}");
            }
        }
    }
    if !turn.text.is_empty() {
        out.push_str(&turn.text);
        if !turn.text.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push('\n');
    out
}

fn artifact_token(artifact: &ArtifactReference) -> Option<&str> {
    artifact.reference.as_deref()
}
