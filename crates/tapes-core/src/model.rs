use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::content::ContentInventory;
use crate::content::{ContentCoverage, ContentPart};
use crate::event::ToolEvent;
use crate::usage::UsageDetail;

pub const SESSION_SCHEMA: &str = "tapes-session/4";
/// Maximum length of a title derived from the first user turn.
pub const DERIVED_TITLE_MAX_CHARS: usize = 96;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// Opaque occurrence within a supplied source. It disambiguates repeated
    /// native ids without changing the native conversation identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<String>,
    pub source: SourceDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SessionMetadata>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<DateTime<Utc>>,
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
    /// True when the recorded start could not be read: the recording is past
    /// the reader's file bound and its opening carried no timestamp. Then
    /// `started_at` is the earliest record the reader reached, and the true
    /// start is at or before it. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub start_uncertain: bool,
    /// Usage facts a harness records beside the normalized counters, filled
    /// by the backend that holds them. They are harness-shaped rather than
    /// part of the session contract, so the session wire object does not carry
    /// them; the usage projection is where they reach a consumer.
    #[serde(skip)]
    pub usage_detail: Option<UsageDetail>,
}

/// Provider metadata attached to a supplied conversation without treating a
/// mode, collection, or status label as a model or outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Whether a normalized session came from a harness-owned recording or from a
/// caller-supplied export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    InstalledRecording,
    SuppliedExport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScopeAuthority {
    Declared,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceScope {
    pub value: String,
    pub authority: ScopeAuthority,
}

/// The opaque source/container coordinate is separate from `Session.id`, which
/// remains the native conversation or session identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    pub locator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    pub kind: SourceKind,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<SourceScope>,
    pub representation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<SourceLocation>,
}

impl SourceDescriptor {
    pub fn installed(harness: &str, locator: impl Into<String>) -> Self {
        Self {
            kind: SourceKind::InstalledRecording,
            origin: harness.to_owned(),
            recorded_harness: Some(harness.to_owned()),
            scope: None,
            representation: format!("{harness}-recording"),
            producer: Some(harness.to_owned()),
            location: Some(SourceLocation {
                locator: locator.into(),
                member: None,
            }),
        }
    }

    pub fn supplied(
        origin: &str,
        representation: &str,
        producer: Option<&str>,
        locator: impl Into<String>,
        scope: Option<SourceScope>,
    ) -> Self {
        Self {
            kind: SourceKind::SuppliedExport,
            origin: origin.to_owned(),
            recorded_harness: None,
            scope,
            representation: representation.to_owned(),
            producer: producer.map(str::to_owned),
            location: Some(SourceLocation {
                locator: locator.into(),
                member: None,
            }),
        }
    }
}

impl Session {
    /// The harness authority used by existing routing and human labels. A
    /// supplied export has no recorded harness, so its origin is the label.
    pub fn harness(&self) -> &str {
        self.source
            .recorded_harness
            .as_deref()
            .unwrap_or(&self.source.origin)
    }

    pub fn locator(&self) -> Option<&str> {
        self.source
            .location
            .as_ref()
            .map(|location| location.locator.as_str())
    }
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
    /// What the record the turn came from is, beyond the role that carries
    /// it. Filled from the harness's own fields, never from the text.
    pub kind: TurnKind,
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
    /// The request/turn identity recorded by a harness, kept separate from
    /// the message or item identity that carries the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_turn_id: Option<String>,
    /// A qualified source coordinate for the record and normalized part this
    /// turn represents. Presentation ordinals are intentionally separate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_ref: Option<RecordRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// Ordered native content evidence. `text` is the normalized readable
    /// projection of text-bearing parts, never a second source of truth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<ContentPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<ContentCoverage>,
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

/// A byte interval in the source descriptor used by one bounded read.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteSpan {
    pub start: u64,
    pub end: u64,
}

impl ByteSpan {
    pub fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// The purpose of one physical source read. Alignment and context are kept
/// apart from the bytes whose records were normalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadRangeKind {
    Head,
    Tail,
    Context,
    Alignment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRange {
    pub kind: ReadRangeKind,
    pub span: ByteSpan,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadGap {
    pub span: ByteSpan,
    pub reason: String,
}

/// What one bounded source observation actually inspected. A source length is
/// not a claim that the source is append-only, and a file validator remains a
/// local observation rather than a portable content identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadEvidence {
    pub source_length: u64,
    pub configured_bound: u64,
    pub coordinate_domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    pub projection: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projection_options: Vec<String>,
    pub observed_at: DateTime<Utc>,
    pub ranges: Vec<ReadRange>,
    /// Decoded source records in read order, with the same absolute spans used
    /// by normalized values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<ByteSpan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<ReadGap>,
}

/// A source identity that is meaningful only within its qualified domain.
/// File spans are portable across page sizes on an unchanged path, while the
/// path and opaque revision prevent relocation or rewrite from becoming an
/// equality claim.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordRef {
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<ByteSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    pub part_index: usize,
}

/// A bounded text observation whose shortening is explicit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedText {
    pub text: String,
    pub chars: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// A terminal record observed in the reached source window. The reader only
/// fills outcome, code, message, and duration when the native record supplied
/// them; an unknown subtype never becomes an invented success or failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalObservation {
    pub record_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmptyTextTailReason {
    NoOperatorAssistantTextInRead,
    ZeroRequestedTail,
    EmptyCompleteProjection,
}

/// The result of the text-only tail projection, kept distinct from source and
/// turn truncation so an empty result has an honest reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextTailEvidence {
    pub requested: usize,
    pub returned: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty_reason: Option<EmptyTextTailReason>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptEvidence {
    pub read: Option<ReadEvidence>,
    pub terminal: Option<TerminalObservation>,
    pub text_tail: Option<TextTailEvidence>,
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
    System,
    Developer,
}

impl Role {
    /// The kind a role settles by itself. A harness records its own commands,
    /// its attached context, and the messages it injects in the same user
    /// envelope an operator's prompt arrives in, so a user turn's kind comes
    /// from the fields the harness wrote beside the text.
    pub fn kind(&self) -> Option<TurnKind> {
        match self {
            Role::User => None,
            Role::Assistant => Some(TurnKind::Assistant),
            Role::Tool => Some(TurnKind::Tool),
            Role::Reasoning => Some(TurnKind::Reasoning),
            Role::System | Role::Developer => Some(TurnKind::Ambient),
        }
    }
}

/// Who a turn's content came from, which the role alone cannot say: a user
/// turn holds an operator's request, a harness command, context the harness
/// attached, or a message the harness injected. Every value rests on a field
/// the harness itself wrote; `Unknown` is the answer where it wrote none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnKind {
    /// Content addressed to the agent by the person or caller driving the
    /// harness. A record carrying ambient context beside a request is one.
    Operator,
    /// The agent's own visible text.
    Assistant,
    /// The agent's reasoning.
    Reasoning,
    /// A tool call or its result.
    Tool,
    /// A harness command or control message recorded in a user envelope.
    Control,
    /// Context the harness attached on its own, carrying no request.
    Ambient,
    /// A message the harness injected into the user envelope on the system's
    /// behalf.
    Notice,
    /// A user-envelope turn the harness gives no evidence for.
    Unknown,
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
    let cleaned = without_known_envelopes(text);

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

/// Blocks a harness wraps in a tag of its own around the operator's message.
const ENVELOPE_TAGS: [&str; 9] = [
    "environment_context",
    "collaboration_mode",
    "permissions_instructions",
    "apps_instructions",
    "plugins_instructions",
    "skills_instructions",
    "INSTRUCTIONS",
    "recommended_plugins",
    "in-app-browser-context",
];

/// Blocks a harness opens with a heading rather than a tag.
const ENVELOPE_HEADINGS: [&str; 2] = ["# AGENTS.md instructions", "# Files mentioned by the user:"];

/// The text with every block the harness attached around it removed. What
/// remains is what somebody wrote, or nothing when the text was all envelope.
pub fn without_known_envelopes(text: &str) -> String {
    let mut cleaned = text.to_owned();
    strip_known_envelopes(&mut cleaned);
    cleaned
}

/// Whether the text holds attached blocks and nothing else.
pub fn is_known_envelope(text: &str) -> bool {
    without_known_envelopes(text).trim().is_empty()
}

fn strip_known_envelopes(text: &mut String) {
    loop {
        let mut changed = false;
        for heading in ENVELOPE_HEADINGS {
            changed |= strip_leading_heading(text, heading);
        }
        for tag in ENVELOPE_TAGS {
            changed |= strip_envelope(text, tag);
        }
        if !changed {
            break;
        }
    }
}

fn strip_leading_heading(text: &mut String, heading: &str) -> bool {
    let leading = text.len() - text.trim_start().len();
    let remainder = &text[leading..];
    let Some(after_prefix) = remainder.strip_prefix(heading) else {
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
    let opening = format!("<{tag}");
    let closing = format!("</{tag}>");
    let mut search_from = 0;
    let mut changed = false;
    while let Some(relative_start) = text[search_from..].find(&opening) {
        let start = search_from + relative_start;
        let after_tag = &text[start + opening.len()..];
        // An opening tag ends at its own `>`, whatever attributes ride on it;
        // a longer tag that merely begins the same way is another element.
        let Some(attributes) = after_tag
            .find('>')
            .filter(|end| after_tag[..*end].chars().all(|c| c != '<'))
            .filter(|end| *end == 0 || after_tag.starts_with(char::is_whitespace))
        else {
            search_from = start + 1;
            continue;
        };
        let content_start = start + opening.len() + attributes + 1;
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

impl TurnKind {
    /// The name a human render uses for the kind.
    pub fn label(self) -> &'static str {
        match self {
            TurnKind::Operator => "operator",
            TurnKind::Assistant => "assistant",
            TurnKind::Reasoning => "reasoning",
            TurnKind::Tool => "tool",
            TurnKind::Control => "control",
            TurnKind::Ambient => "ambient",
            TurnKind::Notice => "notice",
            TurnKind::Unknown => "unknown",
        }
    }
}

/// How a turn is named in human output. A user turn holding something other
/// than an operator's message names what the harness recorded it as, so a
/// reader judging an ending is not told a command was an unanswered prompt.
pub fn human_speaker(turn: &Turn) -> String {
    speaker(&turn.role, turn.kind)
}

/// The same naming for a turn a projection carries by its role and kind
/// rather than whole.
pub fn speaker(role: &Role, kind: TurnKind) -> String {
    let name = match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::Reasoning => "reasoning",
        Role::System => "system",
        Role::Developer => "developer",
    };
    if *role == Role::User && kind != TurnKind::Operator {
        return format!("{name}/{}", kind.label());
    }
    name.to_owned()
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
    pub read: Option<ReadEvidence>,
    pub terminal: Option<TerminalObservation>,
    pub text_tail: Option<TextTailEvidence>,
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
        Self::with_evidence(
            session,
            turns,
            truncation,
            TranscriptEvidence {
                read: None,
                terminal: None,
                text_tail: None,
            },
            trailing_record,
            notes,
        )
    }

    pub fn with_evidence(
        session: Session,
        turns: Vec<Turn>,
        truncation: Truncation,
        evidence: TranscriptEvidence,
        trailing_record: Option<TrailingRecord>,
        notes: Vec<String>,
    ) -> Self {
        Self {
            session,
            turns,
            truncated: !truncation.is_empty(),
            truncation,
            read: evidence.read,
            terminal: evidence.terminal,
            text_tail: evidence.text_tail,
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
            read: self.read.as_ref(),
            terminal: self.terminal.as_ref(),
            text_tail: self.text_tail.as_ref(),
            content: crate::content::inventory(&self.turns),
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
            read: serialized.read,
            terminal: serialized.terminal,
            text_tail: serialized.text_tail,
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
    read: Option<&'a ReadEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<&'a TerminalObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_tail: Option<&'a TextTailEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ContentInventory>,
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
    read: Option<ReadEvidence>,
    #[serde(default)]
    terminal: Option<TerminalObservation>,
    #[serde(default)]
    text_tail: Option<TextTailEvidence>,
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
            source: SourceDescriptor::installed("codex", "/work/recording.jsonl"),
            metadata: None,
            model: Some(Model {
                id: "gpt-5.6-sol".into(),
                variant: Some("high".into()),
            }),
            title: Some("Build the model".into()),
            derived_title: None,
            derived_title_truncated: None,
            directory: Some("/work/tapes".into()),
            started_at: Some(timestamp(1_700_000_000)),
            last_activity_at: Some(timestamp(1_700_000_100)),
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
            start_uncertain: false,
            occurrence: None,
            usage_detail: None,
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
            kind: TurnKind::Assistant,
            text: "Done".into(),
            ts: Some(timestamp(1_700_000_050)),
            ordinal: 0,
            native_id: None,
            request_turn_id: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
            tool: None,
        };
        let transcript = Transcript {
            session: session(),
            turns: vec![turn.clone()],
            truncated: true,
            truncation: Truncation::default(),
            read: None,
            terminal: None,
            text_tail: None,
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
    fn an_envelope_is_recognized_by_its_tag_whatever_attributes_it_carries() {
        assert!(is_known_envelope(
            "<in-app-browser-context url=\"https://example.invalid/page\">\n  page\n</in-app-browser-context>\n"
        ));
        assert!(is_known_envelope(
            "# Files mentioned by the user:\n<environment_context>/work</environment_context>"
        ));
        // A block that carries a request beside the envelope is not envelope.
        assert!(!is_known_envelope(
            "<environment_context>\n  <cwd>/work</cwd>\n</environment_context>\nfix the parser"
        ));
    }

    #[test]
    fn an_element_whose_name_merely_begins_with_a_known_tag_is_left_alone() {
        assert_eq!(
            derive_title("<INSTRUCTIONS_FOR_HUMANS>read me</INSTRUCTIONS_FOR_HUMANS>").as_deref(),
            Some("<INSTRUCTIONS_FOR_HUMANS>read me</INSTRUCTIONS_FOR_HUMANS>")
        );
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
                kind: TurnKind::Operator,
                text: "# AGENTS.md instructions for /work\n<INSTRUCTIONS>rules</INSTRUCTIONS>\n<recommended_plugins>plugins</recommended_plugins>".into(),
                ts: None,
                ordinal: 0,
                native_id: None,
                request_turn_id: None,
                record_ref: None,
                parts: Vec::new(),
                coverage: None,
                channel: None,
                recipient: None,
                tool: None,
            },
            Turn {
                role: Role::User,
                kind: TurnKind::Operator,
                text: "Implement the readable title.".into(),
                ts: None,
                ordinal: 0,
                native_id: None,
                request_turn_id: None,
                record_ref: None,
                parts: Vec::new(),
                coverage: None,
                channel: None,
                recipient: None,
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
            kind: TurnKind::Operator,
            text: "A short request".into(),
            ts: None,
            ordinal: 0,
            native_id: None,
            request_turn_id: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
            tool: None,
        }]);
        let complete_json = serde_json::to_value(complete).unwrap();
        assert_eq!(complete_json["derived_title"], "A short request");
        assert_eq!(complete_json["derived_title_truncated"], false);

        let mut shortened = session();
        shortened.title = None;
        let shortened = shortened.with_derived_title(&[Turn {
            role: Role::User,
            kind: TurnKind::Operator,
            text: "word ".repeat(DERIVED_TITLE_MAX_CHARS),
            ts: None,
            ordinal: 0,
            native_id: None,
            request_turn_id: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
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
            kind: TurnKind::Operator,
            text: "A complete request…".into(),
            ts: None,
            ordinal: 0,
            native_id: None,
            request_turn_id: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
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
            kind: TurnKind::Operator,
            text: "A different request".into(),
            ts: None,
            ordinal: 0,
            native_id: None,
            request_turn_id: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
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
            read: None,
            terminal: None,
            text_tail: None,
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
            kind: TurnKind::Operator,
            text: "hello".into(),
            ts: None,
            ordinal: 12,
            native_id: None,
            request_turn_id: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
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

        let session = session();
        let value = serde_json::to_value(&session).unwrap();
        assert!(value.get("source").is_some(), "{value}");
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
            read: None,
            terminal: None,
            text_tail: None,
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
