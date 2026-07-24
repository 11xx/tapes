use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const SESSION_SCHEMA: &str = "tapes-session/1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    pub started_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
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
            directory: Some("/work/tapes".into()),
            started_at: timestamp(1_700_000_000),
            last_activity_at: timestamp(1_700_000_100),
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
        assert!(!object.contains_key("model"));
        assert!(!object.contains_key("cost"));
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
