//! Which selected sessions may share consumption events.
//!
//! A summary adds sessions' figures, so a session that holds a copy of
//! another's history would be counted twice. Two sessions are related when
//! the recordings say so: one declares the other as its fork origin or
//! parent, or both recorded the same request id or message uuid. Relatedness
//! is transitive, so related sessions form overlap groups. Sessions no
//! evidence relates are added as disjoint, and the summary says that
//! disjointness is inferred from the absence of copy evidence.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use serde::Serialize;

use crate::consumption_union::{ConsumptionAmounts, UnionWithheld};
use crate::model::Session;
use crate::usage::ConsumptionEventKeys;

/// Identifiers one read visited, kept as digests so memory stays bounded
/// however long the recording is. A recording with more identifiers than the
/// cap keeps the smallest digests, which two recordings sharing an identifier
/// agree on, so shared history is still found where the copy is a large part
/// of both; `sampled` says some identifiers were not kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Witnesses {
    digests: BTreeSet<u64>,
    sampled: bool,
}

impl Witnesses {
    /// Identifiers kept per kind of witness.
    pub const CAP: usize = 2048;

    pub fn observe(&mut self, id: &str) {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        let digest = hasher.finish();
        if self.digests.len() < Self::CAP {
            self.digests.insert(digest);
            return;
        }
        match self.digests.last().copied() {
            Some(largest) if digest < largest => {
                if self.digests.insert(digest) {
                    self.digests.pop_last();
                    self.sampled = true;
                }
            }
            Some(largest) if digest == largest => {}
            _ => self.sampled = true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    /// Whether some identifier was left out of the kept set.
    pub fn sampled(&self) -> bool {
        self.sampled
    }

    fn digests(&self) -> impl Iterator<Item = u64> + '_ {
        self.digests.iter().copied()
    }
}

/// What a session's recording says about history it shares with others.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyEvidence {
    /// The sessions the recording declares as its fork origin or parent.
    pub origins: Vec<String>,
    /// The recording says it is a fork, but names no origin that can be
    /// identified, so any session of its harness may hold the history it
    /// copied.
    pub origin_unplaced: bool,
    /// Request ids the read visited.
    pub request_ids: Witnesses,
    /// Message uuids the read visited.
    pub message_uuids: Witnesses,
}

impl CopyEvidence {
    /// Note the ids one record of the read carries.
    pub fn observe_record(&mut self, request_id: Option<&str>, message_uuid: Option<&str>) {
        if let Some(id) = request_id {
            self.request_ids.observe(id);
        }
        if let Some(id) = message_uuid {
            self.message_uuids.observe(id);
        }
    }

    /// Whether either kind of id check left some identifier out.
    pub fn sampled(&self) -> bool {
        self.request_ids.sampled() || self.message_uuids.sampled()
    }

    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
            && !self.origin_unplaced
            && self.request_ids.is_empty()
            && self.message_uuids.is_empty()
    }
}

impl Session {
    /// Keep what the read found about shared history beside the session's
    /// usage, leaving the detail absent when the read found nothing.
    pub(crate) fn record_copy_evidence(&mut self, evidence: CopyEvidence) {
        if !evidence.is_empty() {
            self.usage_detail
                .get_or_insert_with(Default::default)
                .copy_evidence = Some(evidence);
        }
    }
}

impl Session {
    /// Note sessions the store declares this one a fork or child of.
    pub(crate) fn declare_origins(&mut self, origins: Vec<String>) {
        if origins.is_empty() {
            return;
        }
        let detail = self.usage_detail.get_or_insert_with(Default::default);
        let evidence = detail.copy_evidence.get_or_insert_with(Default::default);
        for origin in origins {
            if !evidence.origins.contains(&origin) {
                evidence.origins.push(origin);
            }
        }
    }
}

/// What relates two sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlapEvidence {
    /// One recording declares the other session as its fork origin or
    /// parent.
    DeclaredFork,
    /// A recording declares a fork whose origin it does not identify, so it
    /// cannot be shown apart from any session of its harness.
    ForkOriginUnplaced,
    /// Both recordings hold the same request id.
    SharedRequestId,
    /// Both recordings hold the same message uuid.
    SharedMessageUuid,
}

/// How a summary knows the sessions it adds do not share events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disjointness {
    /// No recording declares a relation to another selected session and none
    /// holds an id another holds. Absence of copy evidence is not proof, so
    /// the figure is inferred, not established.
    InferredFromAbsentCopyEvidence,
    /// As above, but some session's id check kept only a bounded sample of
    /// its ids, so a copy sharing a few ids with it can go unseen. The
    /// figure's `sampled` names those sessions.
    InferredFromSampledCopyEvidence,
}

/// Whether the amounts of an overlap group were added as an exact union.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelatedUnion {
    /// The members' event keys establish their union, and the summary's
    /// disjoint figures and cost include exactly that union.
    Exact,
    /// The summary's exact totals are withheld; its bounds run from the
    /// largest member to the sum of the members.
    Withheld,
}

/// An overlap group within one summary group: the sessions it holds and what
/// relates them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RelatedSessions {
    pub sessions: Vec<String>,
    pub evidence: Vec<OverlapEvidence>,
    pub union: RelatedUnion,
    /// Why the union is not exact; empty for an exact one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<UnionWithheld>,
}

/// How a figure's sessions are known to share no events, and which of them
/// the recordings relate.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UsageOverlap {
    /// How the figure's unrelated sessions and groups are known to share no
    /// events. Absent when every session is in one overlap group, so nothing
    /// was added as disjoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disjointness: Option<Disjointness>,
    /// The sessions whose id check kept only a sample of their ids; present
    /// exactly when the disjointness is inferred from sampled evidence.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sampled: Vec<String>,
    /// The overlap groups among the figure's sessions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedSessions>,
}

/// The selection's overlap groups, computed once over every selected session
/// so each summary group, partition and total reads the same relations.
#[derive(Debug, Default)]
pub struct Overlap {
    /// The overlap group each session belongs to.
    component: Vec<usize>,
    groups: HashMap<usize, Group>,
}

#[derive(Debug)]
struct Group {
    members: Vec<usize>,
    evidence: BTreeSet<OverlapEvidence>,
    union: GroupUnion,
}

#[derive(Debug)]
enum GroupUnion {
    Exact(Box<ConsumptionAmounts>),
    Withheld(Vec<UnionWithheld>),
}

/// One overlap group as it falls within a set of sessions.
pub(crate) struct Bucket<'a> {
    pub members: Vec<usize>,
    group: &'a Group,
    whole: bool,
}

impl Bucket<'_> {
    /// The amounts of an exactly unioned group whose every member is in the
    /// set; a group cut by the set has no exact union within it.
    pub fn exact(&self) -> Option<&ConsumptionAmounts> {
        match (&self.group.union, self.whole) {
            (GroupUnion::Exact(amounts), true) => Some(amounts),
            _ => None,
        }
    }

    pub fn related(&self, sessions: &[Session]) -> RelatedSessions {
        let (union, withheld) = match (&self.group.union, self.whole) {
            (GroupUnion::Exact(_), true) => (RelatedUnion::Exact, Vec::new()),
            (GroupUnion::Withheld(reasons), _) => (RelatedUnion::Withheld, reasons.clone()),
            (GroupUnion::Exact(_), false) => (
                RelatedUnion::Withheld,
                vec![UnionWithheld::PartialSelection],
            ),
        };
        RelatedSessions {
            sessions: self
                .members
                .iter()
                .map(|index| sessions[*index].id.clone())
                .collect(),
            evidence: self.group.evidence.iter().copied().collect(),
            union,
            withheld,
        }
    }
}

struct Components(Vec<usize>);

impl Components {
    fn find(&mut self, mut index: usize) -> usize {
        while self.0[index] != index {
            self.0[index] = self.0[self.0[index]];
            index = self.0[index];
        }
        index
    }

    fn join(&mut self, first: usize, second: usize) {
        let (first, second) = (self.find(first), self.find(second));
        if first != second {
            self.0[second.max(first)] = second.min(first);
        }
    }
}

fn evidence_of(session: &Session) -> Option<&CopyEvidence> {
    session
        .usage_detail
        .as_ref()
        .and_then(|detail| detail.copy_evidence.as_ref())
}

impl Overlap {
    /// Relate `sessions` by what their recordings declare and hold. `keys`
    /// reads one session's consumption event keys, and is asked only for the
    /// members of a group of exactly two.
    pub fn of(
        sessions: &[Session],
        keys: &mut dyn FnMut(usize) -> Option<ConsumptionEventKeys>,
    ) -> Self {
        let mut components = Components((0..sessions.len()).collect());
        let mut edges: Vec<(usize, OverlapEvidence)> = Vec::new();
        let mut by_id: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (index, session) in sessions.iter().enumerate() {
            by_id
                .entry((session.harness(), session.id.as_str()))
                .or_default()
                .push(index);
        }
        for (index, session) in sessions.iter().enumerate() {
            let Some(evidence) = evidence_of(session) else {
                continue;
            };
            for origin in &evidence.origins {
                for other in by_id
                    .get(&(session.harness(), origin.as_str()))
                    .into_iter()
                    .flatten()
                    .filter(|other| **other != index)
                {
                    components.join(index, *other);
                    edges.push((index, OverlapEvidence::DeclaredFork));
                }
            }
            if evidence.origin_unplaced {
                for (other, peer) in sessions.iter().enumerate() {
                    if other != index && peer.harness() == session.harness() {
                        components.join(index, other);
                        edges.push((index, OverlapEvidence::ForkOriginUnplaced));
                    }
                }
            }
        }
        for (kind, witnesses) in [
            (
                OverlapEvidence::SharedRequestId,
                (|evidence: &CopyEvidence| &evidence.request_ids)
                    as fn(&CopyEvidence) -> &Witnesses,
            ),
            (
                OverlapEvidence::SharedMessageUuid,
                |evidence: &CopyEvidence| &evidence.message_uuids,
            ),
        ] {
            let mut holders: HashMap<u64, usize> = HashMap::new();
            for (index, session) in sessions.iter().enumerate() {
                let Some(evidence) = evidence_of(session) else {
                    continue;
                };
                for digest in witnesses(evidence).digests() {
                    match holders.get(&digest) {
                        Some(&first) => {
                            components.join(first, index);
                            edges.push((index, kind));
                        }
                        None => {
                            holders.insert(digest, index);
                        }
                    }
                }
            }
        }
        let component: Vec<usize> = (0..sessions.len()).map(|i| components.find(i)).collect();
        let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (index, root) in component.iter().enumerate() {
            members.entry(*root).or_default().push(index);
        }
        let mut evidence: HashMap<usize, BTreeSet<OverlapEvidence>> = HashMap::new();
        for (index, kind) in edges {
            evidence.entry(component[index]).or_default().insert(kind);
        }
        let groups = members
            .into_iter()
            .filter(|(_, members)| members.len() > 1)
            .map(|(root, members)| {
                let union = union_of(&members, keys);
                let evidence = evidence.remove(&root).unwrap_or_default();
                (
                    root,
                    Group {
                        members,
                        evidence,
                        union,
                    },
                )
            })
            .collect();
        Self { component, groups }
    }

    /// The groups of two or more sessions of `members`, each with the
    /// members of the set it holds.
    pub(crate) fn buckets(&self, members: &[usize]) -> Vec<Bucket<'_>> {
        let mut held: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for index in members {
            let root = self.component.get(*index).copied().unwrap_or(*index);
            if self.groups.contains_key(&root) {
                held.entry(root).or_default().push(*index);
            }
        }
        held.into_iter()
            .filter(|(_, held)| held.len() > 1)
            .map(|(root, held)| {
                let group = &self.groups[&root];
                Bucket {
                    whole: held.len() == group.members.len(),
                    members: held,
                    group,
                }
            })
            .collect()
    }
}

/// The union of a group's members: exact for a parent and fork whose event
/// keys establish it, withheld otherwise.
fn union_of(
    members: &[usize],
    keys: &mut dyn FnMut(usize) -> Option<ConsumptionEventKeys>,
) -> GroupUnion {
    let [first, second] = members else {
        return GroupUnion::Withheld(vec![UnionWithheld::MoreThanTwoSelections]);
    };
    let (Some(first), Some(second)) = (keys(*first), keys(*second)) else {
        return GroupUnion::Withheld(vec![UnionWithheld::OmittedKeys]);
    };
    let union = first.union(&second);
    match union.amounts {
        Some(amounts) => GroupUnion::Exact(Box::new(amounts)),
        None => GroupUnion::Withheld(union.withheld),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_witness_set_keeps_the_smallest_digests() {
        let mut first = Witnesses::default();
        let mut second = Witnesses::default();
        for index in 0..Witnesses::CAP * 2 {
            first.observe(&format!("id-{index}"));
        }
        for index in (0..Witnesses::CAP * 2).rev() {
            second.observe(&format!("id-{index}"));
        }
        assert_eq!(first, second);
        assert!(first.sampled());
        assert_eq!(first.digests().count(), Witnesses::CAP);
    }

    #[test]
    fn a_small_witness_set_is_exact() {
        let mut witnesses = Witnesses::default();
        witnesses.observe("a");
        witnesses.observe("a");
        witnesses.observe("b");
        assert_eq!(witnesses.digests().count(), 2);
        assert!(!witnesses.sampled());
    }
}
