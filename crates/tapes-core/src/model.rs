use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const SESSION_SCHEMA: &str = "tapes-session/1";
/// Maximum length of a title derived from the first user turn.
pub const DERIVED_TITLE_MAX_CHARS: usize = 96;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A bounded human-facing hint derived from the first user turn when the
    /// harness did not record a title. It never replaces `title`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    pub started_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<LiveState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

/// The present-tense state supplied by the optional harness-status authority.
/// Recording backends do not populate it, because a transcript cannot answer
/// whether its session is running now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LiveState {
    Working,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub usd: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    Reasoning,
}

impl Session {
    /// Preserve recorded title absence while adding a bounded display hint.
    pub fn with_derived_title(mut self, turns: &[Turn]) -> Self {
        if self.title.is_none() {
            self.derived_title = turns
                .iter()
                .find(|turn| turn.role == Role::User)
                .and_then(|turn| derive_title(&turn.text));
        }
        self
    }
}

/// Turn the first user message into a compact, human-readable listing hint.
/// Harness envelopes that are known to surround the user message are removed
/// before whitespace is collapsed; unknown content remains untouched.
pub fn derive_title(text: &str) -> Option<String> {
    let mut cleaned = text.to_owned();
    for tag in [
        "environment_context",
        "collaboration_mode",
        "permissions_instructions",
        "apps_instructions",
        "plugins_instructions",
        "skills_instructions",
    ] {
        strip_envelope(&mut cleaned, tag);
    }

    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let mut chars = collapsed.chars();
    let bounded = chars
        .by_ref()
        .take(DERIVED_TITLE_MAX_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        let mut truncated = bounded
            .chars()
            .take(DERIVED_TITLE_MAX_CHARS.saturating_sub(1))
            .collect::<String>();
        truncated.push('\u{2026}');
        Some(truncated)
    } else {
        Some(bounded)
    }
}

fn strip_envelope(text: &mut String, tag: &str) {
    let opening = format!("<{tag}>");
    let closing = format!("</{tag}>");
    let mut search_from = 0;
    while let Some(relative_start) = text[search_from..].find(&opening) {
        let start = search_from + relative_start;
        let content_start = start + opening.len();
        let Some(relative_end) = text[content_start..].find(&closing) else {
            text.replace_range(start.., "");
            return;
        };
        let end = content_start + relative_end + closing.len();
        text.replace_range(start..end, " ");
        search_from = start + 1;
    }
}

/// Format a timestamp for human output without exposing fractional precision
/// or an offset spelling that varies by renderer.
pub fn human_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Render a title without making a derived hint look like harness metadata.
pub fn human_title(session: &Session) -> String {
    match (&session.title, &session.derived_title) {
        (Some(title), _) => title.clone(),
        (None, Some(title)) => format!("~{title}"),
        (None, None) => String::new(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub session: Session,
    pub turns: Vec<Turn>,
    pub truncated: bool,
    pub notes: Vec<String>,
}

impl Serialize for Transcript {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        TranscriptRef {
            schema: SESSION_SCHEMA,
            session: &self.session,
            turns: &self.turns,
            truncated: self.truncated,
            notes: &self.notes,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Transcript {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let serialized = SerializedTranscript::deserialize(deserializer)?;
        if serialized.schema != SESSION_SCHEMA {
            return Err(serde::de::Error::custom(format!(
                "unsupported schema: {}",
                serialized.schema
            )));
        }

        Ok(Self {
            session: serialized.session,
            turns: serialized.turns,
            truncated: serialized.truncated,
            notes: serialized.notes,
        })
    }
}

#[derive(Serialize)]
struct TranscriptRef<'a> {
    schema: &'static str,
    session: &'a Session,
    turns: &'a [Turn],
    truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: &'a Vec<String>,
}

#[derive(Deserialize)]
struct SerializedTranscript {
    schema: String,
    session: Session,
    turns: Vec<Turn>,
    truncated: bool,
    #[serde(default)]
    notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;

    use chrono::TimeZone;
    use serde::{de::DeserializeOwned, Serialize};
    use serde_json::{json, Value};

    use super::*;

    fn assert_round_trip<T>(value: &T)
    where
        T: Debug + PartialEq + Serialize + DeserializeOwned,
    {
        let json = serde_json::to_string(value).unwrap();
        let decoded = serde_json::from_str(&json).unwrap();
        assert_eq!(value, &decoded);
    }

    fn timestamp(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

    fn session() -> Session {
        Session {
            id: "session-1".into(),
            harness: "codex".into(),
            model: Some(Model {
                id: "gpt-5.6-sol".into(),
                variant: Some("high".into()),
            }),
            title: Some("Build the model".into()),
            derived_title: None,
            directory: Some("/work/tapes".into()),
            started_at: timestamp(1_700_000_000),
            last_activity_at: timestamp(1_700_000_100),
            live: None,
            cost: Some(Cost { usd: 0.25 }),
            tokens: Some(Tokens {
                input: Some(100),
                output: Some(50),
                reasoning: Some(25),
                cache_read: Some(10),
                cache_write: Some(5),
            }),
        }
    }

    #[test]
    fn every_type_round_trips_through_json() {
        let model = Model {
            id: "gpt-5.6-sol".into(),
            variant: Some("high".into()),
        };
        let cost = Cost { usd: 0.25 };
        let tokens = Tokens {
            input: Some(100),
            output: Some(50),
            reasoning: Some(25),
            cache_read: Some(10),
            cache_write: Some(5),
        };
        let turn = Turn {
            role: Role::Assistant,
            text: "Done".into(),
            ts: Some(timestamp(1_700_000_050)),
        };
        let transcript = Transcript {
            session: session(),
            turns: vec![turn.clone()],
            truncated: true,
            notes: vec!["One record was unavailable.".into()],
        };

        assert_round_trip(&model);
        assert_round_trip(&cost);
        assert_round_trip(&tokens);
        assert_round_trip(&session());
        assert_round_trip(&Role::Assistant);
        assert_round_trip(&turn);
        assert_round_trip(&transcript);
    }

    #[test]
    fn absent_session_fields_are_omitted() {
        let mut session = session();
        session.title = None;
        session.model = None;
        session.cost = None;

        let value = serde_json::to_value(session).unwrap();
        let object = value.as_object().unwrap();

        assert!(!object.contains_key("title"));
        assert!(!object.contains_key("derived_title"));
        assert!(!object.contains_key("model"));
        assert!(!object.contains_key("cost"));
    }

    #[test]
    fn derived_title_removes_known_envelopes_and_collapses_whitespace() {
        let title = derive_title(
            "<environment_context>\n  <cwd>/work</cwd>\n</environment_context>\n\n  inspect\n   the   fixture  ",
        );

        assert_eq!(title.as_deref(), Some("inspect the fixture"));
    }

    #[test]
    fn derived_title_is_bounded_and_marks_truncation() {
        let title = derive_title(&"word ".repeat(DERIVED_TITLE_MAX_CHARS));
        let title = title.unwrap();

        assert_eq!(title.chars().count(), DERIVED_TITLE_MAX_CHARS);
        assert!(title.ends_with('\u{2026}'));
    }

    #[test]
    fn derived_title_does_not_replace_recorded_title() {
        let session = session().with_derived_title(&[Turn {
            role: Role::User,
            text: "A different request".into(),
            ts: None,
        }]);

        assert_eq!(session.title.as_deref(), Some("Build the model"));
        assert!(session.derived_title.is_none());
    }

    #[test]
    fn human_timestamp_uses_whole_seconds_and_z() {
        let timestamp = Utc.timestamp_opt(1_700_000_000, 123_456_789).unwrap();

        assert_eq!(human_timestamp(timestamp), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn transcript_includes_schema_and_round_trips_truncated() {
        let transcript = Transcript {
            session: session(),
            turns: Vec::new(),
            truncated: true,
            notes: Vec::new(),
        };

        let value = serde_json::to_value(&transcript).unwrap();
        assert_eq!(value["schema"], json!(SESSION_SCHEMA));
        assert_eq!(value["truncated"], Value::Bool(true));
        assert!(value.get("notes").is_none());

        let decoded: Transcript = serde_json::from_value(value).unwrap();
        assert!(decoded.truncated);
        assert_eq!(decoded, transcript);
    }
}
