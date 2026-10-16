//! The lane projection: one turn rendered as the text a lane carries. Lane
//! sizes are measured in the bytes this rendering produces, so a lane cut and
//! the lane file written from it cannot disagree.

use std::fmt::Write as _;

use crate::content::{ArtifactReference, ContentPart};
use crate::model::{Turn, TurnKind};

/// The most text bytes one operator or assistant turn renders.
pub const EXCHANGE_TEXT_BYTES: usize = 16 * 1024;
/// The most text bytes any other turn renders: tool calls and results,
/// reasoning, ambient context, notices.
pub const OTHER_TEXT_BYTES: usize = 2 * 1024;

/// Prepended to a text line that could be read as structure.
pub const ESCAPE_PREFIX: &str = "\\";

/// Line starts the projection itself writes: a turn heading, a lane file's
/// section marker, the omission marker, and the tool and artifact lines. A
/// turn's text line beginning with one of these, or with [`ESCAPE_PREFIX`],
/// is rendered with one more prefix in front, so transcript text forges none
/// of them and removing one prefix from such a line restores the text.
pub const STRUCTURAL_LINE_STARTS: [&str; 5] = ["## ", "=== ", "[… ", "tool: ", "artifact: "];

/// Escape one line of turn text, line ending included, by [`ESCAPE_PREFIX`].
pub fn escape_text_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.starts_with(ESCAPE_PREFIX)
        || STRUCTURAL_LINE_STARTS
            .iter()
            .any(|start| line.starts_with(start))
    {
        std::borrow::Cow::Owned(format!("{ESCAPE_PREFIX}{line}"))
    } else {
        std::borrow::Cow::Borrowed(line)
    }
}

/// A field rendered inside a single line, with line breaks and other control
/// characters replaced by spaces so it cannot start a line of its own.
pub fn single_line(field: &str) -> std::borrow::Cow<'_, str> {
    if field.chars().any(char::is_control) {
        std::borrow::Cow::Owned(
            field
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(field)
    }
}

/// The text-byte cap a turn of this kind renders under.
pub fn text_cap(kind: TurnKind) -> usize {
    match kind {
        TurnKind::Operator | TurnKind::Assistant => EXCHANGE_TEXT_BYTES,
        _ => OTHER_TEXT_BYTES,
    }
}

/// One rendered turn and the bytes of its text the cap left out.
pub struct Projection {
    pub text: String,
    pub omitted_bytes: u64,
}

/// The workset reference a turn is cited by: `<key>#<ordinal>`.
pub fn reference(key: &str, ordinal: usize) -> String {
    format!("{key}#{ordinal}")
}

/// Render one turn deterministically: a header naming its workset reference,
/// kind and origin; a compact line for a tool call or result with the
/// artifact references it carries; then the turn's text, cut at its kind's
/// cap on a character boundary with a marker naming the bytes left out, and
/// each text line escaped by [`escape_text_line`].
/// Every rendering ends with a blank line, so concatenated turns stay
/// separated.
pub fn project_turn(key: &str, turn: &Turn) -> String {
    project(key, turn).text
}

/// [`project_turn`], with the count of text bytes the cap left out.
pub fn project(key: &str, turn: &Turn) -> Projection {
    render(key, turn, text_cap(turn.kind))
}

/// The turn rendered as [`project_turn`] renders it, with its whole text.
pub fn project_whole(key: &str, turn: &Turn) -> String {
    render(key, turn, usize::MAX).text
}

fn render(key: &str, turn: &Turn, cap: usize) -> Projection {
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
            let _ = write!(out, " {}", single_line(field));
        }
        if let Some(exit) = tool.exit {
            let _ = write!(out, " exit={exit}");
        }
        for artifact in tool.artifact_references.iter().filter_map(artifact_token) {
            let _ = write!(out, " artifact={}", single_line(artifact));
        }
        out.push('\n');
    }
    for part in &turn.parts {
        if let ContentPart::FileReference { reference, .. } = part {
            if let Some(token) = artifact_token(reference) {
                let _ = writeln!(out, "artifact: {}", single_line(token));
            }
        }
    }
    let mut end = turn.text.len().min(cap);
    while !turn.text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &turn.text[..end];
    let omitted_bytes = (turn.text.len() - end) as u64;
    if !kept.is_empty() {
        for line in kept.split_inclusive('\n') {
            out.push_str(&escape_text_line(line));
        }
        if !kept.ends_with('\n') {
            out.push('\n');
        }
    }
    if omitted_bytes > 0 {
        let _ = writeln!(
            out,
            "[… {omitted_bytes} bytes omitted; cite {} to read the whole turn]",
            reference(key, turn.ordinal)
        );
    }
    out.push('\n');
    Projection {
        text: out,
        omitted_bytes,
    }
}

fn artifact_token(artifact: &ArtifactReference) -> Option<&str> {
    artifact.reference.as_deref()
}
