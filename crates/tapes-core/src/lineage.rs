//! Session lineage: which sessions a recording names as its relatives.
//!
//! A relationship exists only where a record states it — a child's header
//! naming its parent, a parent's spawn or completion event, a transcript file
//! under the parent's own directory, a session row's parent column. Directory
//! proximity, a shared title, and a timestamp coincidence create nothing. A
//! reference the store cannot resolve is kept and marked, because the missing
//! half is itself the fact a reader needs.
//!
//! Lineage refers to a child; it never absorbs one. Resolving a reference
//! reads the child's header or row and never its turns.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::model::{Model, Session, Truncation};

pub const LINEAGE_SCHEMA: &str = "tapes-lineage/1";

/// One session's relatives, as its harness recorded them.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Lineage {
    /// The session this one names as its parent, recorded by its own header
    /// or row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentRef>,
    /// The sessions this one's store records as spawned by it.
    pub children: Vec<ChildRef>,
    /// The session this one was forked from, where the harness records a fork
    /// as its own relationship.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// Bounds the lineage read itself reached, in the transcript's own terms.
    /// Lifted into the view rather than carried in the lineage object.
    #[serde(skip)]
    pub truncation: Truncation,
    /// What the read holds that has no field, in the transcript's own terms.
    #[serde(skip)]
    pub notes: Vec<String>,
}

impl Lineage {
    /// Order children so one store reads the same way twice: by the moment
    /// they were spawned, with a child whose spawn was never recorded after
    /// the ones whose was, then by reference.
    pub fn sort_children(&mut self) {
        self.children.sort_by(|left, right| {
            match (left.spawned_at, right.spawned_at) {
                (Some(left), Some(right)) => left.cmp(&right),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| left.reference.cmp(&right.reference))
        });
    }
}

/// The parent a session names, as it names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParentRef {
    /// The parent's identifier in the harness's own terms.
    pub native_id: String,
    /// Whether a session with that identifier is in the store.
    pub resolved: bool,
    /// The record that named it.
    pub source: String,
}

/// A session a parent's store records as spawned by it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChildRef {
    /// How the parent's store names the child: its session identifier where
    /// the records carry one, and otherwise the name the harness gave it.
    pub reference: String,
    /// The child's session id, where the store holds a session under it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub harness: String,
    /// The role the parent asked for, in the harness's own vocabulary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The model the records name for the child.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The namespace a harness organizes its children under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawned_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// The outcome the harness recorded, in its own vocabulary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// Whether the child's own recording is in the store.
    pub resolved: bool,
    /// Where this reference was read from.
    pub source: Vec<SourceRef>,
}

impl ChildRef {
    /// A child named by a harness with nothing else recorded about it yet.
    pub fn new(reference: String, harness: &str) -> Self {
        Self {
            reference,
            session_id: None,
            harness: harness.to_owned(),
            role: None,
            model: None,
            group: None,
            spawned_at: None,
            completed_at: None,
            disposition: None,
            resolved: false,
            source: Vec::new(),
        }
    }
}

/// Where a reference was read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SourceRef {
    /// A record in this session's own recording, by the identifier its
    /// harness wrote on it.
    Record { native_id: String },
    /// A file in the store.
    File { path: String },
    /// Another session in the store, by its id.
    Session { id: String },
}

/// The session identity a lineage answer is about, without its transcript.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LineageSession {
    pub id: String,
    pub harness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    pub started_at: DateTime<Utc>,
    pub last_activity_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<std::path::PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
}

/// One session's lineage. The relationships are the store's, and every one of
/// them is a reference: a child's transcript is read with `show` under its own
/// id, never through its parent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LineageView {
    pub schema: &'static str,
    pub session: LineageSession,
    pub lineage: Lineage,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Truncation::is_empty")]
    pub truncation: Truncation,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Project a read session and its recorded relationships into the lineage
/// answer. Nothing is added to what the records name.
pub fn view(session: &Session, mut lineage: Lineage) -> LineageView {
    lineage.sort_children();
    let truncation = std::mem::take(&mut lineage.truncation);
    let notes = std::mem::take(&mut lineage.notes);
    LineageView {
        schema: LINEAGE_SCHEMA,
        session: LineageSession {
            id: session.id.clone(),
            harness: session.harness.clone(),
            model: session.model.clone(),
            started_at: session.started_at,
            last_activity_at: session.last_activity_at,
            directory: session.directory.clone(),
            store: session.store.clone(),
        },
        lineage,
        truncated: !truncation.is_empty(),
        truncation,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::{json, Value};

    use super::*;
    use crate::model::SourceBound;

    fn session() -> Session {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Session {
            id: "fixture-session".to_owned(),
            harness: "fixture".to_owned(),
            model: None,
            title: None,
            derived_title: None,
            derived_title_truncated: None,
            directory: None,
            started_at: ts,
            last_activity_at: ts,
            live: None,
            cost: None,
            tokens: None,
            accounting: None,
            store: None,
            start_uncertain: false,
            usage_detail: None,
        }
    }

    fn child(reference: &str, spawned: Option<i64>) -> ChildRef {
        ChildRef {
            spawned_at: spawned.map(|seconds| Utc.timestamp_opt(seconds, 0).unwrap()),
            ..ChildRef::new(reference.to_owned(), "fixture")
        }
    }

    #[test]
    fn a_session_without_relatives_reports_an_empty_child_list_and_nothing_else() {
        let value = serde_json::to_value(view(&session(), Lineage::default())).unwrap();

        assert_eq!(value["schema"], LINEAGE_SCHEMA);
        assert_eq!(value["lineage"]["children"], json!([]));
        assert_eq!(value["truncated"], false);
        for absent in ["parent", "forked_from"] {
            assert!(
                value["lineage"].get(absent).is_none(),
                "{absent} in {value}"
            );
        }
        for absent in ["truncation", "notes"] {
            assert!(value.get(absent).is_none(), "{absent} in {value}");
        }
    }

    #[test]
    fn an_unresolved_reference_is_kept_with_what_named_it() {
        let lineage = Lineage {
            parent: Some(ParentRef {
                native_id: "absent-parent".to_owned(),
                resolved: false,
                source: "session.parentSession".to_owned(),
            }),
            children: vec![ChildRef {
                role: Some("explorer".to_owned()),
                source: vec![SourceRef::Record {
                    native_id: "call-1".to_owned(),
                }],
                ..child("/root/worker", Some(1_700_000_010))
            }],
            forked_from: Some("older-session".to_owned()),
            ..Lineage::default()
        };

        let value = serde_json::to_value(view(&session(), lineage)).unwrap();
        assert_eq!(
            value["lineage"]["parent"],
            json!({
                "native_id": "absent-parent",
                "resolved": false,
                "source": "session.parentSession"
            })
        );
        assert_eq!(value["lineage"]["forked_from"], "older-session");
        let child = &value["lineage"]["children"][0];
        assert_eq!(child["reference"], "/root/worker");
        assert_eq!(child["role"], "explorer");
        assert_eq!(child["resolved"], false);
        assert_eq!(
            child["source"],
            json!([{ "kind": "record", "native_id": "call-1" }])
        );
        assert!(child.get("session_id").is_none(), "{child}");
        assert!(child.get("disposition").is_none(), "{child}");
    }

    #[test]
    fn children_are_ordered_by_the_moment_they_were_spawned() {
        let lineage = Lineage {
            children: vec![
                child("unspawned", None),
                child("second", Some(1_700_000_020)),
                child("first", Some(1_700_000_010)),
            ],
            ..Lineage::default()
        };

        let view = view(&session(), lineage);
        let order = view
            .lineage
            .children
            .iter()
            .map(|child| child.reference.as_str())
            .collect::<Vec<_>>();
        assert_eq!(order, ["first", "second", "unspawned"]);
    }

    #[test]
    fn a_bounded_read_says_so_and_carries_its_notes() {
        let lineage = Lineage {
            truncation: Truncation {
                window: None,
                source: vec![SourceBound::FileTail { bytes: 4_194_304 }],
            },
            notes: vec!["The store was not exhausted.".to_owned()],
            ..Lineage::default()
        };

        let value = serde_json::to_value(view(&session(), lineage)).unwrap();
        assert_eq!(value["truncated"], true);
        assert_eq!(
            value["truncation"]["source"],
            json!([{ "kind": "file-tail", "bytes": 4_194_304 }])
        );
        assert_eq!(
            value["notes"],
            json!(["The store was not exhausted."]) as Value
        );
    }
}
