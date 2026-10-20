//! The sessions evidence files launch from a family's members.
//!
//! A lineage record whose parent is a member names a child. The child is
//! looked up by harness and id in that harness's store, not through the
//! time-windowed listing, because a delegate's recording can begin outside
//! every member's interval. A recording that names a parent places its own
//! session: evidence fills only what recordings leave silent. Every claim on
//! a session that names a parent other than the one the family counted it
//! under is reported as a conflict, and a session counts once.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::attribution_evidence::{EvidenceFile, FileReport, Tier};
use crate::backend::Backend;
use crate::model::Session;
use crate::window::{self, ParentEvidence};

use super::{
    placed_by, read_markers, Candidate, Link, Member, Role, Roster, SessionKey, MAX_MEMBERS,
};

/// The lineage record that links a member.
#[derive(Clone, Debug, Serialize)]
pub struct EvidenceLink {
    /// The file as the caller named it.
    pub file: String,
    pub producer: String,
    pub tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The file and the position of the record in it.
    #[serde(skip)]
    record: (usize, usize),
}

/// One evidence file and what its records did to the family.
#[derive(Debug, Serialize)]
pub struct EvidenceFileUse {
    #[serde(flatten)]
    pub report: FileReport,
    /// Lineage records that placed a member.
    pub adopted: usize,
    /// Lineage records naming a member under the parent it already has.
    pub agreed: usize,
    /// Lineage records naming a parent other than the one a recording or an
    /// earlier record gave the session, or a session whose recording names a
    /// parent that is not a member.
    pub conflicting: usize,
    /// Lineage records whose child no installed store holds.
    pub unresolved: usize,
    /// Lineage records that changed nothing: the parent is not a member, the
    /// child is the family's root, or the family reached its member bound.
    pub not_applied: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnresolvedReason {
    /// No store of the child's harness holds the id.
    NotFound,
    /// No store of the child's harness is installed.
    HarnessNotInstalled,
    /// A store of the child's harness could not be read.
    LookupFailed,
    /// The child's opening could not be read, so whether its recording names
    /// a parent is unknown.
    OpeningUnreadable,
}

/// A lineage child that joined no family.
#[derive(Debug, Serialize)]
pub struct UnresolvedChild {
    pub child: SessionKey,
    pub parent: SessionKey,
    pub file: String,
    pub producer: String,
    pub reason: UnresolvedReason,
}

/// Where a claim on a session's parent came from.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ClaimSource {
    /// The session's own recording.
    Recording { link: Link },
    /// A lineage record.
    Evidence {
        file: String,
        producer: String,
        tier: Tier,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct ParentClaim {
    pub parent: SessionKey,
    pub source: ClaimSource,
}

/// A session with claims naming more than one parent.
#[derive(Debug, Serialize)]
pub struct Conflict {
    pub session: SessionKey,
    pub claims: Vec<ParentClaim>,
    /// The parent the family counted the session under; absent when it is
    /// not a member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placed_under: Option<SessionKey>,
}

/// What the evidence files the caller named contributed to a family.
#[derive(Debug, Serialize)]
pub struct EvidenceUse {
    pub files: Vec<EvidenceFileUse>,
    /// Children looked up by id in a store.
    pub looked_up: usize,
    pub unresolved: Vec<UnresolvedChild>,
    pub conflicts: Vec<Conflict>,
}

enum Lookup {
    Unresolved(UnresolvedReason),
    Resolved {
        session: Box<Session>,
        backend: usize,
    },
}

pub(super) struct EvidenceRun<'a> {
    files: &'a [EvidenceFile],
    /// Children in the order their first record appears.
    order: Vec<SessionKey>,
    /// Each child's records as file and position.
    by_child: HashMap<SessionKey, Vec<(usize, usize)>>,
    lookups: HashMap<SessionKey, Lookup>,
}

fn key(reference: &crate::attribution_evidence::SessionRef) -> SessionKey {
    SessionKey {
        harness: reference.harness.clone(),
        id: reference.id.clone(),
    }
}

/// The parent a session's own recording names, and how.
fn recorded_parent(session: &Session) -> Option<(SessionKey, ParentEvidence)> {
    window::parent_in_harness(session)
        .map(|(harness, id, evidence)| (SessionKey { harness, id }, evidence))
}

/// A session by harness and id in that harness's stores, with the opening
/// read for the parent its recording names.
fn look_up(backends: &[Box<dyn Backend>], child: &SessionKey) -> Lookup {
    let mut installed = false;
    let mut failed = false;
    for (index, backend) in backends.iter().enumerate() {
        if backend.harness() != child.harness {
            continue;
        }
        installed = true;
        match backend.locate(&child.id) {
            Ok(Some(session)) if session.id == child.id => {
                let mut pool = vec![Candidate {
                    session,
                    backend: index,
                    examined: true,
                }];
                if !read_markers(backends, &mut pool, &[0]).is_empty() {
                    return Lookup::Unresolved(UnresolvedReason::OpeningUnreadable);
                }
                let candidate = pool.remove(0);
                return Lookup::Resolved {
                    session: Box::new(candidate.session),
                    backend: index,
                };
            }
            Ok(_) => {}
            Err(_) => failed = true,
        }
    }
    Lookup::Unresolved(match (installed, failed) {
        (false, _) => UnresolvedReason::HarnessNotInstalled,
        (true, true) => UnresolvedReason::LookupFailed,
        (true, false) => UnresolvedReason::NotFound,
    })
}

/// The first session of each conversation a family holds more than one
/// recording of: a session a member continued is led by the session that
/// continued it.
fn conversations(roster: &Roster) -> HashMap<SessionKey, SessionKey> {
    let mut leaders = HashMap::new();
    for member in &roster.members {
        if let (Link::Continuation, Some(continuing)) = (member.link, &member.parent) {
            let leader = conversation(&leaders, continuing);
            leaders.insert(SessionKey::of(&member.session), leader);
        }
    }
    leaders
}

/// The session that leads the conversation `session` is a recording of.
fn conversation(leaders: &HashMap<SessionKey, SessionKey>, session: &SessionKey) -> SessionKey {
    leaders.get(session).unwrap_or(session).clone()
}

impl<'a> EvidenceRun<'a> {
    pub(super) fn new(files: &'a [EvidenceFile]) -> Self {
        let mut order = Vec::new();
        let mut by_child: HashMap<SessionKey, Vec<(usize, usize)>> = HashMap::new();
        for (file, evidence) in files.iter().enumerate() {
            for (position, record) in evidence.lineage.iter().enumerate() {
                let child = key(&record.child);
                let held = by_child.entry(child.clone()).or_default();
                if held.is_empty() {
                    order.push(child);
                }
                held.push((file, position));
            }
        }
        Self {
            files,
            order,
            by_child,
            lookups: HashMap::new(),
        }
    }

    /// The records naming `child` whose parent is a member.
    fn live(&self, child: &SessionKey, roster: &Roster) -> Vec<(usize, usize)> {
        self.by_child[child]
            .iter()
            .copied()
            .filter(|(file, position)| {
                roster.contains(&key(&self.files[*file].lineage[*position].parent))
            })
            .collect()
    }

    /// Add the children whose lineage records name a member, each placed as
    /// its recording or, where that is silent, the record whose parent
    /// joined first places it. Whether any joined.
    pub(super) fn adopt(&mut self, backends: &[Box<dyn Backend>], roster: &mut Roster) -> bool {
        let mut grew = false;
        for position in 0..self.order.len() {
            let child = self.order[position].clone();
            if roster.contains(&child) {
                continue;
            }
            let live = self.live(&child, roster);
            if live.is_empty() {
                continue;
            }
            if roster.members.len() >= MAX_MEMBERS {
                break;
            }
            let lookup = self
                .lookups
                .entry(child.clone())
                .or_insert_with(|| look_up(backends, &child));
            let Lookup::Resolved { session, backend } = lookup else {
                continue;
            };
            if let Some((parent, evidence)) = recorded_parent(session) {
                grew |= super::adopt(roster, session, *backend, &parent, evidence);
                continue;
            }
            let Some(&(file, record)) = live.iter().min_by_key(|(file, record)| {
                roster.index[&key(&self.files[*file].lineage[*record].parent)]
            }) else {
                continue;
            };
            let lineage = &self.files[file].lineage[record];
            grew |= roster.add(Member {
                session: Session::clone(session),
                backend: *backend,
                role: Role::Delegate,
                link: Link::Evidence,
                parent: Some(key(&lineage.parent)),
                evidence: Some(EvidenceLink {
                    file: self.files[file].report.path.clone(),
                    producer: self.files[file].report.producer.clone(),
                    tier: lineage.tier,
                    label: lineage.label.clone(),
                    subject: lineage.subject.clone(),
                    record: (file, record),
                }),
            });
        }
        grew
    }

    /// What every record did, and which sessions have conflicting claims.
    pub(super) fn finish(self, roster: &Roster) -> EvidenceUse {
        let mut files: Vec<EvidenceFileUse> = self
            .files
            .iter()
            .map(|file| EvidenceFileUse {
                report: file.report.clone(),
                adopted: 0,
                agreed: 0,
                conflicting: 0,
                unresolved: 0,
                not_applied: 0,
            })
            .collect();
        let mut unresolved = Vec::new();
        let mut conflicts = Vec::new();
        let leaders = conversations(roster);
        for child in &self.order {
            let member = roster.index.get(child).map(|at| &roster.members[*at]);
            // A root and the sessions it continues are placed by no parent.
            let placeable =
                member.is_none_or(|member| !matches!(member.link, Link::Root | Link::Continuation));
            let lookup = self.lookups.get(child);
            let recorded = match (member, lookup) {
                (Some(member), _) if member.link != Link::Evidence => {
                    member.parent.clone().map(|parent| (parent, member.link))
                }
                (None, Some(Lookup::Resolved { session, .. })) => recorded_parent(session)
                    .map(|(parent, evidence)| (parent, placed_by(evidence).1)),
                _ => None,
            };
            for &(file, position) in &self.by_child[child] {
                let record = &self.files[file].lineage[position];
                let parent = key(&record.parent);
                let count = &mut files[file];
                if !placeable || (member.is_none() && !roster.contains(&parent)) {
                    count.not_applied += 1;
                    continue;
                }
                match (member, lookup) {
                    (Some(member), _) => {
                        if member
                            .evidence
                            .as_ref()
                            .is_some_and(|link| link.record == (file, position))
                        {
                            count.adopted += 1;
                        } else if member.parent.as_ref().is_some_and(|placed| {
                            conversation(&leaders, placed) == conversation(&leaders, &parent)
                        }) {
                            count.agreed += 1;
                        } else {
                            count.conflicting += 1;
                        }
                    }
                    (None, Some(Lookup::Unresolved(reason))) => {
                        count.unresolved += 1;
                        unresolved.push(UnresolvedChild {
                            child: child.clone(),
                            parent,
                            file: self.files[file].report.path.clone(),
                            producer: self.files[file].report.producer.clone(),
                            reason: *reason,
                        });
                    }
                    (None, _)
                        if recorded.as_ref().is_some_and(|(named, _)| {
                            conversation(&leaders, named) != conversation(&leaders, &parent)
                        }) =>
                    {
                        count.conflicting += 1;
                    }
                    (None, _) => count.not_applied += 1,
                }
            }
            if !placeable {
                continue;
            }
            let mut claims: Vec<ParentClaim> = recorded
                .into_iter()
                .map(|(parent, link)| ParentClaim {
                    parent,
                    source: ClaimSource::Recording { link },
                })
                .collect();
            for &(file, position) in &self.by_child[child] {
                let record = &self.files[file].lineage[position];
                claims.push(ParentClaim {
                    parent: key(&record.parent),
                    source: ClaimSource::Evidence {
                        file: self.files[file].report.path.clone(),
                        producer: self.files[file].report.producer.clone(),
                        tier: record.tier,
                    },
                });
            }
            // A conflict concerns the family only when a claimed parent is
            // one of its members: a file may hold sessions the family does
            // not, and their disagreements are not its.
            let parents: HashSet<SessionKey> = claims
                .iter()
                .map(|claim| conversation(&leaders, &claim.parent))
                .collect();
            if parents.len() > 1 && claims.iter().any(|claim| roster.contains(&claim.parent)) {
                conflicts.push(Conflict {
                    session: child.clone(),
                    placed_under: member.and_then(|member| member.parent.clone()),
                    claims,
                });
            }
        }
        EvidenceUse {
            files,
            looked_up: self.lookups.len(),
            unresolved,
            conflicts,
        }
    }
}
