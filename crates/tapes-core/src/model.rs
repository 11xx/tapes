use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::event::ToolEvent;

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
    /// Whether `derived_title` was shortened to the display bound. Present
    /// alongside a derived title so JSON consumers can distinguish a complete
    /// hint from one whose trailing text was omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_title_truncated: Option<bool>,
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
    /// The basis and coverage of the session-level cost and token counters.
    /// Present exactly when at least one of those counters is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting: Option<Accounting>,
    /// Where `tapes` read this session from, as an opaque coordinate: a
    /// recording file's path, or a store and the endpoint within it. A
    /// consumer writes it down beside the id and does not parse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// True when the recorded start could not be read: the recording is past
    /// the reader's file bound and its opening carried no timestamp. Then
    /// `started_at` is the earliest record the reader reached, and the true
    /// start is at or before it. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_uncertain: bool,
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

impl Model {
    /// The identity shown in list output and used by model filters.
    pub fn identity(&self) -> String {
        self.variant.as_ref().map_or_else(
            || self.id.clone(),
            |variant| format!("{} ({variant})", self.id),
        )
    }
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

/// What the session-level counters are, and how much of the session they cover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounting {
    pub basis: AccountingBasis,
    pub coverage: AccountingCoverage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountingBasis {
    RecordedTotal,
    SummedRequests,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountingCoverage {
    Session,
    ReadWindow,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<DateTime<Utc>>,
    /// Position in the session's normalized turn sequence: zero-based and
    /// dense from the first turn the reader reaches. Assigned when a
    /// transcript is assembled, so a turn outside one carries zero.
    #[serde(default)]
    pub ordinal: usize,
    /// The harness's own id for the record this turn came from, when it
    /// records one. Several turns can share it: one record can carry a
    /// message, its reasoning, and its tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    /// Harness-neutral tool data used by in-process projections. Session JSON
    /// keeps the harness envelope in `text` as its stable wire contract.
    #[serde(skip)]
    pub tool: Option<ToolEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailingRecord {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

/// Why a transcript is not the whole session, one entry per cause. The two
/// halves call for different recoveries: a window is reopened wider with
/// `--tail` or `export`; a source bound was reached by the reader itself, and
/// no request through `tapes` reaches past it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Truncation {
    /// The requested turn window dropped turns the read had produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<TurnWindow>,
    /// Bounds the source read reached. How much lies beyond each is unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source: Vec<SourceBound>,
}

impl Truncation {
    pub fn is_empty(&self) -> bool {
        self.window.is_none() && self.source.is_empty()
    }

    /// A window that dropped turns, or nothing when every turn fit.
    pub fn window(returned: usize, total: usize, bound: usize) -> Option<TurnWindow> {
        (total > returned).then(|| TurnWindow {
            returned,
            omitted: total - returned,
            omitted_from: End::Head,
            bound,
            ordinals: (returned > 0).then(|| OrdinalRange {
                first: total - returned,
                last: total - 1,
            }),
            omitted_exact: true,
        })
    }
}

/// A turn window keeps the newest turns, so what it omits is always the head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnWindow {
    pub returned: usize,
    pub omitted: usize,
    pub omitted_from: End,
    /// The window size that was in force.
    pub bound: usize,
    /// The ordinals the window holds, so a reference outside it is known to
    /// be outside rather than absent. An empty window holds none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinals: Option<OrdinalRange>,
    /// Whether `omitted` is the whole count. A reader that stops fetching once
    /// the window is full knows only what it fetched: the count is a floor, a
    /// wider request fetches older turns, and ordinals count from the oldest
    /// turn this read reached rather than from the session's start.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub omitted_exact: bool,
}

impl TurnWindow {
    /// A window that dropped nothing the reader saw, for a read that stopped
    /// at the window's edge without looking further.
    pub fn whole(returned: usize, bound: usize) -> Self {
        Self {
            returned,
            omitted: 0,
            omitted_from: End::Head,
            bound,
            ordinals: (returned > 0).then(|| OrdinalRange {
                first: 0,
                last: returned - 1,
            }),
            omitted_exact: true,
        }
    }
}

fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrdinalRange {
    pub first: usize,
    pub last: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum End {
    Head,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SourceBound {
    /// Only the final `bytes` of the recording file were read.
    FileTail { bytes: u64 },
    /// The read stopped after the newest `records` of the named record kind;
    /// anything older in the store was not fetched.
    RecordPage { records: usize, of: String },
    /// `turns` turns carry text the store read cut at `chars` characters.
    TurnText { turns: usize, chars: usize },
}

/// Format a byte count for human output without false precision.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else if value.fract() == 0.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
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
            if let Some((title, truncated)) = turns
                .iter()
                .filter(|turn| turn.role == Role::User)
                .find_map(|turn| derive_title_info(&turn.text))
            {
                self.derived_title = Some(title);
                self.derived_title_truncated = Some(truncated);
            }
        }
        self
    }
}

/// Turn the first user message into a compact, human-readable listing hint.
/// Harness envelopes that are known to surround the user message are removed
/// before whitespace is collapsed; unknown content remains untouched.
pub fn derive_title(text: &str) -> Option<String> {
    derive_title_info(text).map(|(title, _)| title)
}

fn derive_title_info(text: &str) -> Option<(String, bool)> {
    let mut cleaned = text.to_owned();
    strip_known_envelopes(&mut cleaned);

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
        Some((truncated, true))
    } else {
        Some((bounded, false))
    }
}

fn strip_known_envelopes(text: &mut String) {
    loop {
        let mut changed = strip_leading_agents_heading(text);
        for tag in [
            "environment_context",
            "collaboration_mode",
            "permissions_instructions",
            "apps_instructions",
            "plugins_instructions",
            "skills_instructions",
            "INSTRUCTIONS",
            "recommended_plugins",
        ] {
            changed |= strip_envelope(text, tag);
        }
        if !changed {
            break;
        }
    }
}

fn strip_leading_agents_heading(text: &mut String) -> bool {
    let leading = text.len() - text.trim_start().len();
    let remainder = &text[leading..];
    let Some(after_prefix) = remainder.strip_prefix("# AGENTS.md instructions") else {
        return false;
    };
    let prefix_len = remainder.len() - after_prefix.len();
    let Some(newline) = after_prefix.find('\n') else {
        text.truncate(leading);
        return true;
    };
    let end = leading + prefix_len + newline + 1;
    text.replace_range(leading..end, " ");
    true
}

fn strip_envelope(text: &mut String, tag: &str) -> bool {
    let opening = format!("<{tag}>");
    let closing = format!("</{tag}>");
    let mut search_from = 0;
    let mut changed = false;
    while let Some(relative_start) = text[search_from..].find(&opening) {
        let start = search_from + relative_start;
        let content_start = start + opening.len();
        let Some(relative_end) = text[content_start..].find(&closing) else {
            text.replace_range(start.., "");
            return true;
        };
        let end = content_start + relative_end + closing.len();
        text.replace_range(start..end, " ");
        changed = true;
        search_from = start + 1;
    }
    changed
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

/// `truncated` is the derived signal that anything was omitted; `truncation`
/// says what and why. A transcript built through `new` keeps the two in
/// agreement.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    pub session: Session,
    pub turns: Vec<Turn>,
    pub truncated: bool,
    pub truncation: Truncation,
    pub trailing_record: Option<TrailingRecord>,
    pub notes: Vec<String>,
}

impl Transcript {
    pub fn new(
        session: Session,
        turns: Vec<Turn>,
        truncation: Truncation,
        trailing_record: Option<TrailingRecord>,
        notes: Vec<String>,
    ) -> Self {
        Self {
            session,
            turns,
            truncated: !truncation.is_empty(),
            truncation,
            trailing_record,
            notes,
        }
    }
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
            truncation: &self.truncation,
            trailing_record: self.trailing_record.as_ref(),
            notes: &self.notes,
        }
        .serialize(serializer)
    }
}

fn truncation_is_empty(truncation: &&Truncation) -> bool {
    truncation.is_empty()
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
            truncation: serialized.truncation,
            trailing_record: serialized.trailing_record,
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
    #[serde(skip_serializing_if = "truncation_is_empty")]
    truncation: &'a Truncation,
    #[serde(skip_serializing_if = "Option::is_none")]
    trailing_record: Option<&'a TrailingRecord>,
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
    truncation: Truncation,
    #[serde(default)]
    trailing_record: Option<TrailingRecord>,
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
            derived_title_truncated: None,
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
            accounting: Some(Accounting {
                basis: AccountingBasis::RecordedTotal,
                coverage: AccountingCoverage::Session,
            }),
            store: None,
            start_uncertain: false,
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
        let accounting = Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        };
        let turn = Turn {
            role: Role::Assistant,
            text: "Done".into(),
            ts: Some(timestamp(1_700_000_050)),
            ordinal: 0,
            native_id: None,
            tool: None,
        };
        let transcript = Transcript {
            session: session(),
            turns: vec![turn.clone()],
            truncated: true,
            truncation: Truncation::default(),
            trailing_record: Some(TrailingRecord {
                kind: "event_msg".into(),
                timestamp: Some(timestamp(1_700_000_060)),
            }),
            notes: vec!["One record was unavailable.".into()],
        };

        assert_round_trip(&model);
        assert_round_trip(&cost);
        assert_round_trip(&tokens);
        assert_round_trip(&accounting);
        assert_eq!(
            serde_json::to_value(&accounting).unwrap(),
            json!({
                "basis": "recorded-total",
                "coverage": "session"
            })
        );
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
        session.tokens = None;
        session.accounting = None;

        let value = serde_json::to_value(session).unwrap();
        let object = value.as_object().unwrap();

        assert!(!object.contains_key("title"));
        assert!(!object.contains_key("derived_title"));
        assert!(!object.contains_key("derived_title_truncated"));
        assert!(!object.contains_key("model"));
        assert!(!object.contains_key("cost"));
        assert!(!object.contains_key("tokens"));
        assert!(!object.contains_key("accounting"));
    }

    #[test]
    fn derived_title_removes_known_envelopes_and_collapses_whitespace() {
        let title = derive_title(
            "# AGENTS.md instructions for /work\n\n<INSTRUCTIONS>\nfollow the repository rules\n</INSTRUCTIONS>\n<recommended_plugins>\n- one-plugin\n</recommended_plugins>\n<environment_context>\n  <cwd>/work</cwd>\n</environment_context>\n\n  inspect\n   the   fixture  ",
        );

        assert_eq!(title.as_deref(), Some("inspect the fixture"));
    }

    #[test]
    fn derived_title_handles_plugins_before_heading_and_leading_whitespace() {
        let title = derive_title(
            "\n  <recommended_plugins>\n- one-plugin\n</recommended_plugins>\n\n  # AGENTS.md instructions\n\n<INSTRUCTIONS>\nfollow the repository rules\n</INSTRUCTIONS>\n\n  inspect the fixture  ",
        );

        assert_eq!(title.as_deref(), Some("inspect the fixture"));
    }

    #[test]
    fn derived_title_skips_instruction_only_user_turns() {
        let mut session = session();
        session.title = None;
        let session = session.with_derived_title(&[
            Turn {
                role: Role::User,
                text: "# AGENTS.md instructions for /work\n<INSTRUCTIONS>rules</INSTRUCTIONS>\n<recommended_plugins>plugins</recommended_plugins>".into(),
                ts: None,
                ordinal: 0,
                native_id: None,
                tool: None,
            },
            Turn {
                role: Role::User,
                text: "Implement the readable title.".into(),
                ts: None,
                ordinal: 0,
                native_id: None,
                tool: None,
            },
        ]);

        assert_eq!(
            session.derived_title.as_deref(),
            Some("Implement the readable title.")
        );
    }

    #[test]
    fn derived_title_is_bounded_and_marks_truncation() {
        let title = derive_title(&"word ".repeat(DERIVED_TITLE_MAX_CHARS));
        let title = title.unwrap();

        assert_eq!(title.chars().count(), DERIVED_TITLE_MAX_CHARS);
        assert!(title.ends_with('\u{2026}'));
    }

    #[test]
    fn derived_title_json_exposes_whether_the_hint_was_truncated() {
        let mut complete = session();
        complete.title = None;
        let complete = complete.with_derived_title(&[Turn {
            role: Role::User,
            text: "A short request".into(),
            ts: None,
            ordinal: 0,
            native_id: None,
            tool: None,
        }]);
        let complete_json = serde_json::to_value(complete).unwrap();
        assert_eq!(complete_json["derived_title"], "A short request");
        assert_eq!(complete_json["derived_title_truncated"], false);

        let mut shortened = session();
        shortened.title = None;
        let shortened = shortened.with_derived_title(&[Turn {
            role: Role::User,
            text: "word ".repeat(DERIVED_TITLE_MAX_CHARS),
            ts: None,
            ordinal: 0,
            native_id: None,
            tool: None,
        }]);
        let shortened_json = serde_json::to_value(shortened).unwrap();
        assert_eq!(shortened_json["derived_title_truncated"], true);
        assert!(shortened_json["derived_title"]
            .as_str()
            .unwrap()
            .ends_with('…'));
    }

    #[test]
    fn a_literal_ellipsis_does_not_claim_truncation() {
        let mut session = session();
        session.title = None;
        let session = session.with_derived_title(&[Turn {
            role: Role::User,
            text: "A complete request…".into(),
            ts: None,
            ordinal: 0,
            native_id: None,
            tool: None,
        }]);

        assert_eq!(
            session.derived_title.as_deref(),
            Some("A complete request…")
        );
        assert_eq!(session.derived_title_truncated, Some(false));
    }

    #[test]
    fn derived_title_does_not_replace_recorded_title() {
        let session = session().with_derived_title(&[Turn {
            role: Role::User,
            text: "A different request".into(),
            ts: None,
            ordinal: 0,
            native_id: None,
            tool: None,
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
            truncation: Truncation::default(),
            trailing_record: None,
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

    #[test]
    fn truncation_names_each_cause_and_derives_the_flag() {
        let truncation = Truncation {
            window: Truncation::window(100, 147, 100),
            source: vec![
                SourceBound::FileTail {
                    bytes: 4 * 1024 * 1024,
                },
                SourceBound::RecordPage {
                    records: 1000,
                    of: "messages".into(),
                },
                SourceBound::TurnText {
                    turns: 3,
                    chars: 4000,
                },
            ],
        };
        let transcript =
            Transcript::new(session(), Vec::new(), truncation.clone(), None, Vec::new());
        assert!(transcript.truncated);

        let value = serde_json::to_value(&transcript).unwrap();
        assert_eq!(
            value["truncation"],
            json!({
                "window": { "returned": 100, "omitted": 47, "omitted_from": "head", "bound": 100, "ordinals": { "first": 47, "last": 146 } },
                "source": [
                    { "kind": "file-tail", "bytes": 4_194_304 },
                    { "kind": "record-page", "records": 1000, "of": "messages" },
                    { "kind": "turn-text", "turns": 3, "chars": 4000 }
                ]
            })
        );
        let decoded: Transcript = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.truncation, truncation);
        assert_round_trip(&truncation);

        assert!(Truncation::window(147, 147, 200).is_none());
        let whole = Transcript::new(
            session(),
            Vec::new(),
            Truncation::default(),
            None,
            Vec::new(),
        );
        assert!(!whole.truncated);
        let value = serde_json::to_value(&whole).unwrap();
        assert!(value.get("truncation").is_none(), "{value}");
        assert_eq!(value["truncated"], false);
    }

    #[test]
    fn a_turn_carries_its_ordinal_and_omits_an_absent_native_id() {
        let turn = Turn {
            role: Role::User,
            text: "hello".into(),
            ts: None,
            ordinal: 12,
            native_id: None,
            tool: None,
        };
        let value = serde_json::to_value(&turn).unwrap();
        assert_eq!(value["ordinal"], 12);
        assert!(value.get("native_id").is_none(), "{value}");
        assert!(value.get("ts").is_none(), "{value}");

        let named = Turn {
            native_id: Some("msg_1".into()),
            ..turn
        };
        let value = serde_json::to_value(&named).unwrap();
        assert_eq!(value["native_id"], "msg_1");
        assert_round_trip(&named);

        let mut session = session();
        session.store = None;
        let value = serde_json::to_value(&session).unwrap();
        assert!(value.get("store").is_none(), "{value}");
    }

    #[test]
    fn a_transcript_without_a_truncation_record_still_decodes() {
        let value = json!({
            "schema": SESSION_SCHEMA,
            "session": serde_json::to_value(session()).unwrap(),
            "turns": [],
            "truncated": true
        });
        let decoded: Transcript = serde_json::from_value(value).unwrap();
        assert!(decoded.truncated);
        assert!(decoded.truncation.is_empty());
    }

    #[test]
    fn human_bytes_names_the_unit_without_false_precision() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(4 * 1024 * 1024), "4 MiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn transcript_omits_an_absent_trailing_record_and_timestamp() {
        let mut transcript = Transcript {
            session: session(),
            turns: Vec::new(),
            truncated: false,
            truncation: Truncation::default(),
            trailing_record: Some(TrailingRecord {
                kind: "last-prompt".into(),
                timestamp: None,
            }),
            notes: Vec::new(),
        };

        let value = serde_json::to_value(&transcript).unwrap();
        assert_eq!(value["trailing_record"]["kind"], "last-prompt");
        assert!(value["trailing_record"].get("timestamp").is_none());

        transcript.trailing_record = None;
        let value = serde_json::to_value(&transcript).unwrap();
        assert!(value.get("trailing_record").is_none());
    }
}
