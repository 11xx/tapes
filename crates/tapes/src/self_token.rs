//! `self`: the session the caller is running in, named by the variable its
//! harness exports into the caller's shell.
//!
//! The token is the caller's claim about itself, not an inference from
//! recency, so it is accepted wherever a session id is and is never applied
//! unasked. What it became is always reported, it fails when no variable
//! answers, and the id it names must be one an installed store holds.

use anyhow::{anyhow, Result};
use tapes_core::backend::Backend;

/// The token a caller writes in place of its own session id.
pub const TOKEN: &str = "self";

/// The variables a harness exports its session id in, in precedence order,
/// with the harness each names. Claude has two spellings: `CLAUDE_SESSION_ID`
/// is set by hand and `CLAUDE_CODE_SESSION_ID` is what Claude Code exports,
/// so a deliberately exported id beats the ambient one. OpenCode v2 exports
/// no session id, so it has no rung.
const LADDER: [(&str, &str); 5] = [
    ("CLAUDE_SESSION_ID", "claude"),
    ("CLAUDE_CODE_SESSION_ID", "claude"),
    ("CODEX_THREAD_ID", "codex"),
    ("OPENCODE_SESSION", "opencode"),
    ("PI_SESSION_ID", "pi"),
];

/// What `self` resolved to, and the variable that said so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfSession {
    pub id: String,
    pub harness: &'static str,
    pub variable: &'static str,
}

impl SelfSession {
    /// The one line that reports the resolution.
    pub fn report(&self) -> String {
        format!(
            "self is {} session {}, from {}",
            self.harness, self.id, self.variable
        )
    }
}

/// The session the first set variable of the ladder names, read through
/// `var` so a caller can supply the environment.
pub fn detect(var: impl Fn(&str) -> Option<String>) -> Result<SelfSession> {
    LADDER
        .into_iter()
        .find_map(|(variable, harness)| {
            var(variable)
                .map(|id| id.trim().to_owned())
                .filter(|id| !id.is_empty())
                .map(|id| SelfSession {
                    id,
                    harness,
                    variable,
                })
        })
        .ok_or_else(|| {
            anyhow!(
                "self names no session here: none of {} is set",
                LADDER
                    .iter()
                    .map(|(variable, _)| *variable)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The session `self` names in this process's environment, checked against
/// the stores: an exported id can be stale or leaked from a parent, so one no
/// store holds as a session of the harness that exported it is refused.
pub fn resolve(backends: &[Box<dyn Backend>]) -> Result<SelfSession> {
    let detected = detect(|variable| std::env::var(variable).ok())?;
    let held = tapes_core::resolve_session(backends, &detected.id)
        .ok()
        .filter(|resolved| {
            resolved.session.id == detected.id && resolved.session.harness() == detected.harness
        });
    if held.is_none() {
        return Err(anyhow!(
            "{}, but no installed store holds that {} session",
            detected.report(),
            detected.harness
        ));
    }
    Ok(detected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |variable| {
            pairs
                .iter()
                .find(|(name, _)| *name == variable)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_hand_set_claude_spelling_outranks_the_one_claude_code_exports() {
        let detected = detect(env(&[
            ("CLAUDE_CODE_SESSION_ID", "ambient"),
            ("CLAUDE_SESSION_ID", "chosen"),
            ("CODEX_THREAD_ID", "codex-thread"),
        ]))
        .unwrap();
        assert_eq!(
            detected,
            SelfSession {
                id: "chosen".to_owned(),
                harness: "claude",
                variable: "CLAUDE_SESSION_ID",
            }
        );
        assert_eq!(
            detected.report(),
            "self is claude session chosen, from CLAUDE_SESSION_ID"
        );
    }

    #[test]
    fn an_empty_variable_is_not_an_answer_and_no_answer_names_every_variable() {
        let detected = detect(env(&[("CLAUDE_SESSION_ID", " "), ("PI_SESSION_ID", "p1")])).unwrap();
        assert_eq!(detected.variable, "PI_SESSION_ID");
        assert_eq!(detected.harness, "pi");

        let error = detect(env(&[])).unwrap_err().to_string();
        for (variable, _) in LADDER {
            assert!(error.contains(variable), "{error}");
        }
    }
}
