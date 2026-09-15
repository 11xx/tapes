//! Bounded evidence for native content parts and referenced artifacts.
//!
//! Content is retained as an ordered projection of fields the source actually
//! recorded. References are descriptors only: this module never opens a path,
//! fetches a URL, decodes media bytes, or treats a path-shaped string as an
//! artifact without a verified content carrier.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{BoundedText, RecordRef};

pub const MAX_CONTENT_PARTS: usize = 128;
pub const MAX_ARTIFACT_REFERENCES: usize = 64;
pub const MAX_DESCRIPTOR_CHARS: usize = 4 * 1024;
pub const MAX_DESCRIPTOR_TOTAL_CHARS: usize = 16 * 1024;
pub const MAX_CITATION_FIELD_BYTES: usize = 4 * 1024;
pub const MAX_CITATION_DESCRIPTOR_BYTES: usize = 16 * 1024;
pub const MAX_STRUCTURED_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentCarrier {
    DirectPart,
    EncodedWidget,
    AssociatedArtifact,
    RecordedReference,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentAvailability {
    RetainedBody,
    ReferenceOnly,
    UnsupportedRepresentation,
    ReadBound,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentCoverage {
    pub carrier: ContentCarrier,
    pub availability: ContentAvailability,
    pub retained_parts: usize,
    pub omitted_parts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub omitted_reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentInventory {
    pub records: usize,
    pub parts: usize,
    pub references: usize,
    pub retained_body: usize,
    pub reference_only: usize,
    pub unsupported: usize,
    pub unknown: usize,
    pub omitted_parts: usize,
}

impl ContentInventory {
    pub fn is_empty(&self) -> bool {
        self.records == 0 && self.parts == 0 && self.omitted_parts == 0
    }

    pub fn merge(&mut self, other: &Self) {
        self.records += other.records;
        self.parts += other.parts;
        self.references += other.references;
        self.retained_body += other.retained_body;
        self.reference_only += other.reference_only;
        self.unsupported += other.unsupported;
        self.unknown += other.unknown;
        self.omitted_parts += other.omitted_parts;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReference {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<RecordRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// A bounded body retained when the source carried the artifact's text.
    /// A missing body means the source did not expose one, not that the
    /// reader opened a referenced resource and found it empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<BoundedText>,
    /// Whether the source exposed a usable body representation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_availability: Option<ContentAvailability>,
    /// Native citation metadata retained from the artifact body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<ArtifactCitation>,
    /// Citation groups or sources beyond the bounded collection.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_citations: usize,
    /// Whether citation traversal reached a structural bound before it could
    /// establish the omitted count.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub citation_traversal_incomplete: bool,
    /// Whether one or more citation descriptors was shortened by its field or
    /// cumulative descriptor budget.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub descriptor_truncated: bool,
}

/// A citation span recorded inside an artifact body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCitation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<usize>,
    /// Source pages associated with a grouped citation span.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<ArtifactCitationSource>,
    /// Sources omitted by the bounded group projection.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_sources: usize,
    /// Whether source traversal reached a structural bound before it could
    /// establish the omitted count.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_traversal_incomplete: bool,
}

/// A source retained under one grouped citation span.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCitationSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<BoundedText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<ArtifactCitationSource>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_sources: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_traversal_incomplete: bool,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ContentPart {
    Text {
        text: String,
        source_field: String,
        native_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        record_ref: Option<RecordRef>,
    },
    Transcription {
        text: String,
        source_field: String,
        native_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        record_ref: Option<RecordRef>,
    },
    MediaReference {
        reference: ArtifactReference,
        source_field: String,
        native_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        record_ref: Option<RecordRef>,
    },
    FileReference {
        reference: ArtifactReference,
        source_field: String,
        native_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        record_ref: Option<RecordRef>,
    },
    StructuredArtifact {
        descriptor: BoundedText,
        source_field: String,
        native_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reference: Option<ArtifactReference>,
        #[serde(skip_serializing_if = "Option::is_none")]
        record_ref: Option<RecordRef>,
    },
    ToolPayload {
        descriptor: BoundedText,
        source_field: String,
        native_kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        record_ref: Option<RecordRef>,
    },
    Unknown {
        native_kind: String,
        descriptor: BoundedText,
        source_field: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        record_ref: Option<RecordRef>,
    },
}

impl ContentPart {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } | Self::Transcription { text, .. } => Some(text),
            _ => None,
        }
    }

    pub fn record_ref(&self) -> Option<&RecordRef> {
        match self {
            Self::Text { record_ref, .. }
            | Self::Transcription { record_ref, .. }
            | Self::MediaReference { record_ref, .. }
            | Self::FileReference { record_ref, .. }
            | Self::StructuredArtifact { record_ref, .. }
            | Self::ToolPayload { record_ref, .. }
            | Self::Unknown { record_ref, .. } => record_ref.as_ref(),
        }
    }

    pub fn set_record_ref(&mut self, reference: RecordRef) {
        match self {
            Self::Text { record_ref, .. }
            | Self::Transcription { record_ref, .. }
            | Self::ToolPayload { record_ref, .. }
            | Self::Unknown { record_ref, .. } => *record_ref = Some(reference),
            Self::StructuredArtifact {
                reference: artifact,
                record_ref,
                ..
            } => {
                if let Some(artifact) = artifact {
                    artifact.source = Some(reference.clone());
                }
                *record_ref = Some(reference);
            }
            Self::MediaReference {
                reference: artifact,
                record_ref,
                ..
            }
            | Self::FileReference {
                reference: artifact,
                record_ref,
                ..
            } => {
                artifact.source = Some(reference.clone());
                *record_ref = Some(reference);
            }
        }
    }

    /// Attach a content part to its source record without changing the
    /// normalized turn coordinate carried by that record.
    pub fn set_record_ref_part(&mut self, mut reference: RecordRef, content_part_index: usize) {
        reference.content_part_index = Some(content_part_index);
        self.set_record_ref(reference);
    }
}

pub fn project_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(ContentPart::text)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn text_part(text: impl Into<String>, source_field: &str, native_kind: &str) -> ContentPart {
    ContentPart::Text {
        text: text.into(),
        source_field: source_field.to_owned(),
        native_kind: native_kind.to_owned(),
        record_ref: None,
    }
}

/// Keep a text body under a caller-selected character bound while retaining
/// its full length as evidence.
pub fn bounded_text(text: &str, max_chars: usize) -> BoundedText {
    let chars = text.chars().count();
    BoundedText {
        text: text.chars().take(max_chars).collect(),
        chars,
        truncated: chars > max_chars,
    }
}

/// Keep a text descriptor within a byte bound without splitting UTF-8, while
/// retaining its original Unicode scalar count and shortening fact.
pub fn bounded_text_bytes(text: &str, max_bytes: usize) -> BoundedText {
    let mut retained_bytes = 0;
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if retained_bytes + character.len_utf8() > max_bytes {
            break;
        }
        retained_bytes += character.len_utf8();
        end = next;
    }
    BoundedText {
        text: text[..end].to_owned(),
        chars: text.chars().count(),
        truncated: end < text.len(),
    }
}

pub fn transcription_part(
    text: impl Into<String>,
    source_field: &str,
    native_kind: &str,
) -> ContentPart {
    ContentPart::Transcription {
        text: text.into(),
        source_field: source_field.to_owned(),
        native_kind: native_kind.to_owned(),
        record_ref: None,
    }
}

pub fn tool_part(value: &Value, source_field: &str, native_kind: &str) -> ContentPart {
    ContentPart::ToolPayload {
        descriptor: bounded_shape(value),
        source_field: source_field.to_owned(),
        native_kind: native_kind.to_owned(),
        record_ref: None,
    }
}

pub fn tool_coverage() -> ContentCoverage {
    ContentCoverage {
        carrier: ContentCarrier::DirectPart,
        availability: ContentAvailability::RetainedBody,
        retained_parts: 1,
        omitted_parts: 0,
        omitted_reason: None,
    }
}

pub fn inventory(turns: &[crate::model::Turn]) -> Option<ContentInventory> {
    let mut inventory = ContentInventory {
        records: turns.iter().filter(|turn| !turn.parts.is_empty()).count(),
        ..ContentInventory::default()
    };
    for turn in turns {
        inventory.parts += turn.parts.len();
        inventory.references += turn
            .parts
            .iter()
            .filter(|part| {
                matches!(
                    part,
                    ContentPart::MediaReference { .. }
                        | ContentPart::FileReference { .. }
                        | ContentPart::StructuredArtifact {
                            reference: Some(_),
                            ..
                        }
                )
            })
            .count();
        inventory.omitted_parts += turn
            .coverage
            .as_ref()
            .map_or(0, |coverage| coverage.omitted_parts);
        match turn.coverage.as_ref().map(|coverage| coverage.availability) {
            Some(ContentAvailability::RetainedBody) => inventory.retained_body += 1,
            Some(ContentAvailability::ReferenceOnly) => inventory.reference_only += 1,
            Some(ContentAvailability::UnsupportedRepresentation) => inventory.unsupported += 1,
            Some(ContentAvailability::Unknown) | Some(ContentAvailability::ReadBound) => {
                inventory.unknown += 1
            }
            None => {}
        }
    }
    (!inventory.is_empty()).then_some(inventory)
}

pub fn parts_from_array(value: &Value, source_field: &str) -> (Vec<ContentPart>, ContentCoverage) {
    let Some(parts) = value.as_array() else {
        return (
            Vec::new(),
            ContentCoverage {
                carrier: ContentCarrier::DirectPart,
                availability: ContentAvailability::UnsupportedRepresentation,
                retained_parts: 0,
                omitted_parts: 0,
                omitted_reason: Some("content carrier was not an array".to_owned()),
            },
        );
    };
    let total = parts.len();
    let mut retained = Vec::new();
    for part in parts.iter().take(MAX_CONTENT_PARTS) {
        if let Some(text) = part.as_str() {
            let field = format!("{source_field}[{}]", retained.len());
            retained.push(text_part(text, &field, "text"));
            continue;
        }
        // OpenAI export mapping parts name their kind `content_type`.
        let native_kind = part["type"]
            .as_str()
            .or_else(|| part["content_type"].as_str())
            .unwrap_or("unknown");
        let field = format!("{source_field}[{}]", retained.len());
        let parsed = match native_kind {
            "input_text" | "output_text" | "text" => part["text"]
                .as_str()
                .map(|text| text_part(text, &field, native_kind))
                .or_else(|| Some(unknown_part(part, &field, native_kind))),
            "transcription"
            | "input_audio_transcription"
            | "audio_transcript"
            | "audio_transcription" => part
                .get("text")
                .or_else(|| part.get("transcript"))
                .and_then(Value::as_str)
                .map(|text| transcription_part(text, &field, native_kind))
                .or_else(|| Some(unknown_part(part, &field, native_kind))),
            "image"
            | "input_image"
            | "output_image"
            | "audio"
            | "image_asset_pointer"
            | "audio_asset_pointer" => Some(artifact_part(part, &field, native_kind, true)),
            // A real-time voice part wraps its audio pointer; the part itself
            // carries no reference of its own.
            "real_time_user_audio_video_asset_pointer" => Some(
                match part
                    .get("audio_asset_pointer")
                    .filter(|value| value.is_object())
                {
                    Some(audio) => artifact_part(audio, &field, native_kind, true),
                    None => unknown_part(part, &field, native_kind),
                },
            ),
            "file" | "input_file" | "output_file" | "file_reference" => {
                Some(artifact_part(part, &field, native_kind, false))
            }
            "structured_artifact" | "artifact" | "widget_state" => {
                Some(ContentPart::StructuredArtifact {
                    descriptor: bounded_shape(part),
                    source_field: field.clone(),
                    native_kind: native_kind.to_owned(),
                    reference: None,
                    record_ref: None,
                })
            }
            _ => Some(ContentPart::Unknown {
                native_kind: native_kind.to_owned(),
                descriptor: bounded_shape(part),
                source_field: field.clone(),
                record_ref: None,
            }),
        };
        if let Some(part) = parsed {
            retained.push(part);
        }
    }
    let availability = if retained.iter().any(|part| part.text().is_some()) {
        ContentAvailability::RetainedBody
    } else if retained.iter().any(|part| {
        matches!(
            part,
            ContentPart::MediaReference { .. } | ContentPart::FileReference { .. }
        )
    }) {
        ContentAvailability::ReferenceOnly
    } else if retained
        .iter()
        .any(|part| matches!(part, ContentPart::Unknown { .. }))
        || (retained.is_empty() && total > 0)
    {
        ContentAvailability::Unknown
    } else {
        ContentAvailability::UnsupportedRepresentation
    };
    let omitted_parts = total.saturating_sub(retained.len());
    (
        retained,
        ContentCoverage {
            carrier: ContentCarrier::DirectPart,
            availability,
            retained_parts: total.min(MAX_CONTENT_PARTS).min(total - omitted_parts),
            omitted_parts,
            omitted_reason: (omitted_parts > 0).then(|| {
                if total > MAX_CONTENT_PARTS {
                    "part-count-bound".to_owned()
                } else {
                    "descriptor-output-bound".to_owned()
                }
            }),
        },
    )
}

fn artifact_part(value: &Value, source_field: &str, native_kind: &str, media: bool) -> ContentPart {
    let Some(reference) = artifact_reference(value, native_kind) else {
        return unknown_part(value, source_field, native_kind);
    };
    if media {
        ContentPart::MediaReference {
            reference,
            source_field: source_field.to_owned(),
            native_kind: native_kind.to_owned(),
            record_ref: None,
        }
    } else {
        ContentPart::FileReference {
            reference,
            source_field: source_field.to_owned(),
            native_kind: native_kind.to_owned(),
            record_ref: None,
        }
    }
}

fn unknown_part(value: &Value, source_field: &str, native_kind: &str) -> ContentPart {
    ContentPart::Unknown {
        native_kind: native_kind.to_owned(),
        descriptor: bounded_shape(value),
        source_field: source_field.to_owned(),
        record_ref: None,
    }
}

pub fn artifact_reference(value: &Value, kind: &str) -> Option<ArtifactReference> {
    value
        .as_object()
        .and_then(|value| artifact_reference_object(value, kind))
}

pub fn artifact_reference_object(
    value: &serde_json::Map<String, Value>,
    kind: &str,
) -> Option<ArtifactReference> {
    let uri = ["uri", "url", "href", "asset_pointer"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str).map(str::to_owned));
    let path = value.get("path").and_then(Value::as_str).map(str::to_owned);
    let digest = ["digest", "sha256", "file_id", "fileId"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_str).map(str::to_owned));
    let bytes = ["bytes", "size", "byte_count", "size_bytes"]
        .into_iter()
        .find_map(|key| value.get(key).and_then(Value::as_u64));
    (uri.is_some() || path.is_some() || digest.is_some() || bytes.is_some()).then_some(
        ArtifactReference {
            kind: kind.to_owned(),
            identity: None,
            origin: None,
            backing: None,
            author: None,
            completion: None,
            citation_count: None,
            uri,
            path,
            digest,
            bytes,
            timestamp: None,
            source: None,
            action: None,
            body: None,
            body_availability: None,
            citations: Vec::new(),
            omitted_citations: 0,
            citation_traversal_incomplete: false,
            descriptor_truncated: false,
        },
    )
}

/// Replace values with bounded key/type descriptors. This keeps unknown or
/// potentially sensitive bodies out of diagnostic output while preserving the
/// shape a later adapter would need to identify.
pub fn bounded_shape(value: &Value) -> BoundedText {
    let shape = shape_value(value, 0);
    let rendered =
        serde_json::to_string(&shape).unwrap_or_else(|_| "{\"type\":\"unknown\"}".to_owned());
    let chars = rendered.chars().count();
    BoundedText {
        text: rendered.chars().take(MAX_DESCRIPTOR_CHARS).collect(),
        chars,
        truncated: chars > MAX_DESCRIPTOR_CHARS,
    }
}

fn shape_value(value: &Value, depth: usize) -> Value {
    if depth >= MAX_STRUCTURED_DEPTH {
        return Value::String("depth-limit".to_owned());
    }
    match value {
        Value::Null => Value::String("null".to_owned()),
        Value::Bool(_) => Value::String("boolean".to_owned()),
        Value::Number(_) => Value::String("number".to_owned()),
        Value::String(_) => Value::String("string".to_owned()),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .take(16)
                .map(|value| shape_value(value, depth + 1))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .keys()
                .take(64)
                .map(|key| (key.clone(), shape_value(&values[key], depth + 1)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_parts_keep_order_and_only_verified_references() {
        let (parts, coverage) = parts_from_array(
            &serde_json::json!([
                {"type":"input_text","text":"request"},
                {"type":"image","uri":"/canary","data":"secret-bytes"},
                {"type":"mystery","payload":{"value":"private"}}
            ]),
            "payload.content",
        );
        assert_eq!(parts.len(), 3);
        assert!(matches!(parts[0], ContentPart::Text { .. }));
        assert!(matches!(parts[1], ContentPart::MediaReference { .. }));
        assert!(matches!(parts[2], ContentPart::Unknown { .. }));
        assert_eq!(coverage.retained_parts, 3);
        assert_eq!(coverage.omitted_parts, 0);
        let rendered = serde_json::to_string(&parts).unwrap();
        assert!(rendered.contains("/canary"));
        assert!(!rendered.contains("secret-bytes"));
        assert!(!rendered.contains("private"));
    }

    #[test]
    fn openai_export_parts_are_classified_by_content_type() {
        let (parts, coverage) = parts_from_array(
            &serde_json::json!([
                {"content_type":"audio_transcription","text":"spoken","direction":"in","decoding_id":null},
                {"content_type":"image_asset_pointer","asset_pointer":"sediment://file_image","size_bytes":42,"width":1,"height":1},
                {"content_type":"real_time_user_audio_video_asset_pointer","audio_asset_pointer":{"content_type":"audio_asset_pointer","asset_pointer":"sediment://file_audio","size_bytes":7,"format":"wav"},"frames_asset_pointers":[]}
            ]),
            "message.content.parts",
        );
        assert!(
            matches!(&parts[0], ContentPart::Transcription { text, native_kind, .. } if text == "spoken" && native_kind == "audio_transcription")
        );
        let ContentPart::MediaReference { reference, .. } = &parts[1] else {
            panic!("image pointer is a media reference: {parts:?}");
        };
        assert_eq!(reference.uri.as_deref(), Some("sediment://file_image"));
        assert_eq!(reference.bytes, Some(42));
        let ContentPart::MediaReference { reference, .. } = &parts[2] else {
            panic!("real-time audio pointer is a media reference: {parts:?}");
        };
        assert_eq!(reference.uri.as_deref(), Some("sediment://file_audio"));
        assert_eq!(coverage.availability, ContentAvailability::RetainedBody);
    }

    #[test]
    fn part_and_descriptor_bounds_are_explicit() {
        let values = (0..(MAX_CONTENT_PARTS + 3))
            .map(|_| serde_json::json!({"type":"unknown","x":"value"}))
            .collect::<Vec<_>>();
        let (parts, coverage) = parts_from_array(&Value::Array(values), "payload.content");
        assert_eq!(parts.len(), MAX_CONTENT_PARTS);
        assert_eq!(coverage.omitted_parts, 3);
        assert_eq!(coverage.omitted_reason.as_deref(), Some("part-count-bound"));
    }

    #[test]
    fn empty_part_arrays_have_explicit_empty_coverage() {
        let (parts, coverage) = parts_from_array(&Value::Array(Vec::new()), "payload.content");
        assert!(parts.is_empty());
        assert_eq!(coverage.retained_parts, 0);
        assert_eq!(coverage.omitted_parts, 0);
        assert_eq!(
            coverage.availability,
            ContentAvailability::UnsupportedRepresentation
        );
    }

    #[test]
    fn shape_descriptors_keep_types_without_bodies() {
        let descriptor = bounded_shape(&serde_json::json!({
            "text": "not retained",
            "bytes": [1, 2, 3],
            "nested": {"kind": true}
        }));
        assert!(!descriptor.text.contains("not retained"));
        assert!(descriptor.text.contains("\"text\":\"string\""));
        assert!(descriptor.text.contains("\"bytes\":[\"number\""));
    }

    #[test]
    fn byte_bounded_text_keeps_utf8_boundaries_and_original_length() {
        let exact = bounded_text_bytes("é🧪z", "é🧪".len());
        assert_eq!(exact.text, "é🧪");
        assert_eq!(exact.chars, 3);
        assert!(exact.truncated);

        let between_scalars = bounded_text_bytes("é🧪z", 5);
        assert_eq!(between_scalars.text, "é");
        assert_eq!(between_scalars.chars, 3);
        assert!(between_scalars.truncated);
    }
}
