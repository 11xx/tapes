//! The session a launcher names as its child's parent, in the first line of
//! the child's first prompt.
//!
//! A tool that starts a session on another's behalf writes, as the first line
//! of the prompt it starts it with,
//!
//! ```text
//! tapes-parent: <harness>:<session-id>
//! ```
//!
//! and nothing before it. `<harness>` is `claude`, `codex`, `pi` or
//! `opencode`; `<session-id>` is the launching session's native id, 1 to 128
//! ASCII letters, digits, `-`, `_` or `.`. The line ends at a line feed or at
//! the end of the prompt. A first prompt that opens with `tapes-parent:` in
//! any other form is a malformed marker: it names no parent and the session
//! says so. The first prompt is the first user message whose text is not
//! wholly context or a notice the harness attached on its own.

use crate::model::{is_known_envelope, is_known_notice, Session};

/// What opens every marker line.
pub const MARKER_PREFIX: &str = "tapes-parent:";

/// The harnesses a marker may name.
const HARNESSES: [&str; 4] = ["claude", "codex", "pi", "opencode"];

/// The longest session id a marker carries.
const MAX_ID_BYTES: usize = 128;

/// What a first prompt says about the session that launched it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchMarker {
    /// The prompt does not open with a marker.
    Absent,
    /// The prompt names the session that launched it.
    Parent { harness: String, id: String },
    /// The prompt opens with the marker's prefix in another form.
    Malformed,
}

impl LaunchMarker {
    /// The marker a prompt's text opens with.
    pub fn read(text: &str) -> Self {
        let Some(rest) = text.strip_prefix(MARKER_PREFIX) else {
            return Self::Absent;
        };
        let line = rest.split_once('\n').map_or(rest, |(line, _)| line);
        let parsed = line.strip_prefix(' ').and_then(|named| {
            let (harness, id) = named.split_once(':')?;
            let valid_id = !id.is_empty()
                && id.len() <= MAX_ID_BYTES
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
            (HARNESSES.contains(&harness) && valid_id).then(|| Self::Parent {
                harness: harness.to_owned(),
                id: id.to_owned(),
            })
        });
        parsed.unwrap_or(Self::Malformed)
    }

    /// The marker of the first prompt among user message texts in recording
    /// order, passing over the texts the harness attached on its own. `None`
    /// when no text the sequence holds is a prompt.
    pub fn of_first_prompt<T: AsRef<str>>(texts: impl IntoIterator<Item = T>) -> Option<Self> {
        texts
            .into_iter()
            .find(|text| is_prompt(text.as_ref()))
            .map(|text| Self::read(text.as_ref()))
    }
}

/// Whether a user message's text is a prompt rather than context or a notice
/// the harness attached on its own.
pub fn is_prompt(text: &str) -> bool {
    !is_known_envelope(text) && !is_known_notice(text)
}

/// The prompt text without the marker line it opens with, when it opens with
/// one that names a parent.
pub fn without_marker(text: &str) -> &str {
    match LaunchMarker::read(text) {
        LaunchMarker::Parent { .. } => text.split_once('\n').map_or("", |(_, rest)| rest),
        _ => text,
    }
}

impl Session {
    /// Keep what a read of the first prompt found: the parent its marker
    /// names, a note on a marker that names none, or a note on a read that
    /// failed.
    pub(crate) fn declare_launch_parent_read(&mut self, read: anyhow::Result<LaunchMarker>) {
        match read {
            Ok(marker) => self.declare_launch_parent(marker),
            Err(error) => self
                .usage_detail
                .get_or_insert_with(Default::default)
                .accounting_notes
                .push(format!(
                    "The first prompt could not be read for a tapes-parent marker: {error:#}"
                )),
        }
    }

    /// Keep the parent a first prompt's marker names, or note a marker that
    /// names none. A marker naming the session itself names no parent.
    pub(crate) fn declare_launch_parent(&mut self, marker: LaunchMarker) {
        let note = match marker {
            LaunchMarker::Absent => return,
            LaunchMarker::Parent { id, .. } if id != self.id => {
                self.usage_detail
                    .get_or_insert_with(Default::default)
                    .launch_parent = Some(id);
                return;
            }
            LaunchMarker::Parent { .. } => {
                "The first prompt's tapes-parent marker names this session itself; it names no parent."
            }
            LaunchMarker::Malformed => {
                "The first prompt opens with a malformed tapes-parent marker; it names no parent."
            }
        };
        self.usage_detail
            .get_or_insert_with(Default::default)
            .accounting_notes
            .push(note.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_line_names_its_parent_and_any_other_form_is_malformed() {
        assert_eq!(
            LaunchMarker::read("tapes-parent: claude:30000000-0000-4000-8000-000000000001\nwork"),
            LaunchMarker::Parent {
                harness: "claude".to_owned(),
                id: "30000000-0000-4000-8000-000000000001".to_owned()
            }
        );
        assert_eq!(
            LaunchMarker::read("tapes-parent: pi:a.b_c-1"),
            LaunchMarker::Parent {
                harness: "pi".to_owned(),
                id: "a.b_c-1".to_owned()
            }
        );
        for malformed in [
            "tapes-parent: claude:",
            "tapes-parent:claude:abc",
            "tapes-parent:  claude:abc",
            "tapes-parent: gemini:abc",
            "tapes-parent: claude:abc def",
            "tapes-parent: claude:abc\r\nwork",
            "tapes-parent: claude:parent::agent",
            "tapes-parent: claude",
        ] {
            assert_eq!(
                LaunchMarker::read(malformed),
                LaunchMarker::Malformed,
                "{malformed}"
            );
        }
        let long = format!("tapes-parent: codex:{}", "a".repeat(MAX_ID_BYTES + 1));
        assert_eq!(LaunchMarker::read(&long), LaunchMarker::Malformed);
        for absent in [
            "Please read tapes-parent: claude:abc",
            " tapes-parent: claude:abc",
            "",
        ] {
            assert_eq!(LaunchMarker::read(absent), LaunchMarker::Absent, "{absent}");
        }
    }

    #[test]
    fn the_first_prompt_follows_attached_context() {
        let marker = LaunchMarker::of_first_prompt([
            "<environment_context>\n  <cwd>/work</cwd>\n</environment_context>",
            "tapes-parent: codex:019f0000-0000-7000-8000-000000000001\nInspect.",
            "tapes-parent: claude:later",
        ]);
        assert_eq!(
            marker,
            Some(LaunchMarker::Parent {
                harness: "codex".to_owned(),
                id: "019f0000-0000-7000-8000-000000000001".to_owned()
            })
        );
        assert_eq!(
            LaunchMarker::of_first_prompt(["Inspect.", "tapes-parent: claude:later"]),
            Some(LaunchMarker::Absent)
        );
    }
}
