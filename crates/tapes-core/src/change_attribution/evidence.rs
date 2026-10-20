//! Evidence files in a repository's attribution: which sessions their
//! records place, the work each record names as the repository holds it,
//! and the sessions they bring in.
//!
//! A session a `binding` or `event` record names is placed by those records
//! alone. A record's work is one item, named by its label, or by its commits
//! where it has none; the commits of a label are every commit its records
//! name, and records that name different commits for one label are
//! reported. A session such a record names is read when its work names a
//! selected commit, or meets the selected window, wherever its recording
//! began: it is looked up by harness and id, never through the listing. A
//! `lineage` record makes its child a child of its parent where both are
//! read and the child's own recording names no parent.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::Serialize;

use super::gather::{read_session, Gathered, Member, MemberRule};
use super::{
    Declared, DeclaredBinding, DeclaredEvent, DeclaredWork, RecordSource, Repository, SessionKey,
};
use crate::attribution::Tier;
use crate::attribution_evidence::{self, EvidenceFile, FileReport, SessionRef};
use crate::backend::Backend;
use crate::family::UnresolvedReason;
use crate::model::Session;
use crate::span::Instant;
use crate::window::{self, ParentEvidence};

/// The records evidence files hold, read for one repository.
pub struct Records {
    files: Vec<(String, String)>,
    reports: Vec<FileReport>,
    sessions: BTreeMap<SessionKey, SessionRecords>,
    /// Each child's lineage claims: its claimed parent, and the file.
    lineage: BTreeMap<SessionKey, Vec<(SessionKey, usize)>>,
    disagreements: Vec<Disagreement>,
}

#[derive(Default)]
struct SessionRecords {
    bindings: Vec<DeclaredBinding>,
    events: Vec<DeclaredEvent>,
    /// The file of each binding and of each event.
    binding_files: Vec<usize>,
    event_files: Vec<usize>,
    /// The files whose records name the session.
    files: BTreeSet<usize>,
}

/// Records that name different commits for one label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Disagreement {
    pub label: String,
    /// Each commit set the records name, with the files that name it.
    pub sets: Vec<CommitSet>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommitSet {
    pub commits: Vec<String>,
    pub files: Vec<String>,
}

/// A session a record names that no installed store holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Unresolved {
    pub session: SessionKey,
    pub reason: UnresolvedReason,
    pub files: Vec<String>,
}

/// A session whose recording shows it creating a commit that no record
/// naming it binds it to. The records still place it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Conflict {
    pub session: SessionKey,
    pub commit: String,
    pub files: Vec<String>,
}

/// A session whose recording and records, or several records, name
/// different parents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParentConflict {
    pub session: SessionKey,
    /// The parent its own recording names, which places it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded: Option<SessionKey>,
    /// Each parent records claim, with the files that claim it.
    pub claimed: Vec<(SessionKey, Vec<String>)>,
    /// The parent the session inherits from, where one is read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placed_under: Option<SessionKey>,
}

/// What the records did to a gathering.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EvidenceMembership {
    /// Sessions looked up by id in a store.
    pub looked_up: usize,
    pub unresolved: Vec<Unresolved>,
    /// Members a lineage record made a child of another member.
    pub lineage_applied: usize,
    pub parent_conflicts: Vec<ParentConflict>,
}

fn key(reference: &SessionRef) -> SessionKey {
    SessionKey {
        harness: reference.harness.clone(),
        id: reference.id.clone(),
    }
}

fn tier(tier: attribution_evidence::Tier) -> Tier {
    match tier {
        attribution_evidence::Tier::Bound => Tier::Bound,
        attribution_evidence::Tier::Declared => Tier::Matched,
    }
}

/// The identity of a record's work: its label, or its commits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identity {
    Label(String),
    Commits(BTreeSet<String>),
}

fn identity(work: &attribution_evidence::Work) -> Identity {
    match &work.label {
        Some(label) => Identity::Label(label.clone()),
        None => Identity::Commits(work.commits.iter().cloned().collect()),
    }
}

impl Records {
    /// Read the records of `files` for `repository`: which commits it holds
    /// decides how each record's work is shared.
    pub fn new(files: &[EvidenceFile], repository: &Repository) -> Result<Self> {
        let all_work = files.iter().flat_map(|file| {
            file.bindings
                .iter()
                .map(|binding| &binding.work)
                .chain(file.events.iter().map(|event| &event.work))
        });
        // Each identity's commits, and for a label, the sets its records
        // name, with their files.
        let mut commits: BTreeMap<Identity, BTreeSet<String>> = BTreeMap::new();
        let mut written: BTreeMap<String, BTreeMap<BTreeSet<String>, BTreeSet<usize>>> =
            BTreeMap::new();
        for (index, file) in files.iter().enumerate() {
            let works = file
                .bindings
                .iter()
                .map(|binding| &binding.work)
                .chain(file.events.iter().map(|event| &event.work));
            for work in works {
                let named: BTreeSet<String> = work.commits.iter().cloned().collect();
                commits
                    .entry(identity(work))
                    .or_default()
                    .extend(named.iter().cloned());
                if let (Some(label), false) = (&work.label, named.is_empty()) {
                    written
                        .entry(label.clone())
                        .or_default()
                        .entry(named)
                        .or_default()
                        .insert(index);
                }
            }
        }
        let named: BTreeSet<String> = all_work
            .flat_map(|work| work.commits.iter().cloned())
            .collect();
        let held = if named.is_empty() {
            BTreeSet::new()
        } else {
            repository.names_commits(&named)?
        };
        let paths: Vec<String> = files.iter().map(|file| file.report.path.clone()).collect();
        let disagreements = written
            .into_iter()
            .filter(|(_, sets)| sets.len() > 1)
            .map(|(label, sets)| Disagreement {
                label,
                sets: sets
                    .into_iter()
                    .map(|(commits, files)| CommitSet {
                        commits: commits.into_iter().collect(),
                        files: files
                            .into_iter()
                            .map(|index| paths[index].clone())
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        let declared = |work: &attribution_evidence::Work| -> DeclaredWork {
            let all = commits.get(&identity(work)).cloned().unwrap_or_default();
            let (held, unheld): (Vec<String>, Vec<String>) =
                all.into_iter().partition(|sha| held.contains(sha));
            DeclaredWork {
                label: work.label.clone(),
                held,
                unheld,
            }
        };

        let mut sessions: BTreeMap<SessionKey, SessionRecords> = BTreeMap::new();
        let mut lineage: BTreeMap<SessionKey, Vec<(SessionKey, usize)>> = BTreeMap::new();
        for (index, file) in files.iter().enumerate() {
            let source = |work: &DeclaredWork, role: &Option<String>, whole: bool| RecordSource {
                file: file.report.path.clone(),
                producer: file.report.producer.clone(),
                label: work.label.clone(),
                unheld: work.unheld.clone(),
                role: role.clone(),
                whole_session: whole,
            };
            for binding in &file.bindings {
                let work = declared(&binding.work);
                let held = sessions.entry(key(&binding.session)).or_default();
                held.files.insert(index);
                held.binding_files.push(index);
                held.bindings.push(DeclaredBinding {
                    source: source(&work, &binding.role, binding.span.is_none()),
                    tier: tier(binding.tier),
                    span: binding.span.map(|span| (span.from, span.to)),
                    work,
                });
            }
            for event in &file.events {
                let work = declared(&event.work);
                let held = sessions.entry(key(&event.session)).or_default();
                held.files.insert(index);
                held.event_files.push(index);
                held.events.push(DeclaredEvent {
                    source: source(&work, &event.role, false),
                    tier: tier(event.tier),
                    at: event.at,
                    work,
                });
            }
            for record in &file.lineage {
                lineage
                    .entry(key(&record.child))
                    .or_default()
                    .push((key(&record.parent), index));
            }
        }
        Ok(Self {
            files: files
                .iter()
                .map(|file| (file.report.path.clone(), file.report.producer.clone()))
                .collect(),
            reports: files.iter().map(|file| file.report.clone()).collect(),
            sessions,
            lineage,
            disagreements,
        })
    }

    /// Whether a binding or an event names the session, which then places
    /// it alone.
    pub fn names(&self, session: &SessionKey) -> bool {
        self.sessions.contains_key(session)
    }

    /// What the records state about the session.
    pub fn declared(&self, session: &SessionKey) -> Option<Declared> {
        self.sessions.get(session).map(|held| Declared {
            bindings: held.bindings.clone(),
            events: held.events.clone(),
        })
    }

    /// Every commit the session's records name, held or not.
    pub fn commits_of(&self, session: &SessionKey) -> BTreeSet<String> {
        let Some(held) = self.sessions.get(session) else {
            return BTreeSet::new();
        };
        held.bindings
            .iter()
            .map(|binding| &binding.work)
            .chain(held.events.iter().map(|event| &event.work))
            .flat_map(|work| work.held.iter().chain(&work.unheld).cloned())
            .collect()
    }

    /// The files whose records name the session, as the caller named them.
    pub fn files_of(&self, session: &SessionKey) -> Vec<String> {
        self.sessions
            .get(session)
            .map(|held| {
                held.files
                    .iter()
                    .map(|index| self.files[*index].0.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The sessions whose records' work names one of `commits`.
    pub fn naming(&self, commits: &BTreeSet<String>) -> BTreeSet<SessionKey> {
        self.sessions
            .iter()
            .filter(|(_, held)| {
                held.bindings
                    .iter()
                    .map(|binding| &binding.work)
                    .chain(held.events.iter().map(|event| &event.work))
                    .any(|work| work.held.iter().any(|sha| commits.contains(sha)))
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// The sessions whose records meet `[from, to)`: a span or an event
    /// inside it, and, as `whole`, those bound for their whole session,
    /// which meet it where the session's own activity does.
    pub fn meeting(
        &self,
        from: Instant,
        to: Instant,
    ) -> (BTreeSet<SessionKey>, BTreeSet<SessionKey>) {
        let mut inside = BTreeSet::new();
        let mut whole = BTreeSet::new();
        for (key, held) in &self.sessions {
            let spans = held.bindings.iter().any(|binding| {
                binding
                    .span
                    .is_some_and(|(start, end)| start < to && from < end)
            });
            let events = held
                .events
                .iter()
                .any(|event| from <= event.at && event.at < to);
            if spans || events {
                inside.insert(key.clone());
            } else if held.bindings.iter().any(|binding| binding.span.is_none()) {
                whole.insert(key.clone());
            }
        }
        (inside, whole)
    }

    /// Per file, how many of its bindings and events name one of
    /// `sessions`.
    pub fn applied(&self, sessions: &BTreeSet<SessionKey>) -> Vec<(usize, usize)> {
        let mut counts = vec![(0, 0); self.files.len()];
        for (key, held) in &self.sessions {
            if !sessions.contains(key) {
                continue;
            }
            for file in &held.binding_files {
                counts[*file].0 += 1;
            }
            for file in &held.event_files {
                counts[*file].1 += 1;
            }
        }
        counts
    }

    /// The files read, each with its producer.
    pub fn files(&self) -> &[(String, String)] {
        &self.files
    }

    pub fn reports(&self) -> &[FileReport] {
        &self.reports
    }

    pub fn disagreements(&self) -> &[Disagreement] {
        &self.disagreements
    }

    /// The parents lineage records claim for `child`, each with its files.
    fn claims(&self, child: &SessionKey) -> Vec<(SessionKey, Vec<String>)> {
        let mut claimed: BTreeMap<SessionKey, BTreeSet<usize>> = BTreeMap::new();
        for (parent, file) in self.lineage.get(child).into_iter().flatten() {
            claimed.entry(parent.clone()).or_default().insert(*file);
        }
        claimed
            .into_iter()
            .map(|(parent, files)| {
                (
                    parent,
                    files
                        .into_iter()
                        .map(|index| self.files[index].0.clone())
                        .collect(),
                )
            })
            .collect()
    }
}

/// A session by harness and id in that harness's stores.
fn locate(
    backends: &[Box<dyn Backend>],
    session: &SessionKey,
) -> std::result::Result<(Session, usize), UnresolvedReason> {
    let mut installed = false;
    let mut failed = false;
    for (index, backend) in backends.iter().enumerate() {
        if backend.harness() != session.harness {
            continue;
        }
        installed = true;
        match backend.locate(&session.id) {
            Ok(Some(found)) if found.id == session.id => return Ok((found, index)),
            Ok(_) => {}
            Err(_) => failed = true,
        }
    }
    Err(match (installed, failed) {
        (false, _) => UnresolvedReason::HarnessNotInstalled,
        (true, true) => UnresolvedReason::LookupFailed,
        (true, false) => UnresolvedReason::NotFound,
    })
}

/// Whether a session's activity meets `[from, to)`.
fn active_in(session: &Session, from: Instant, to: Instant) -> bool {
    let start = session.started_at.or(session.last_activity_at);
    let end = session.last_activity_at.or(session.started_at);
    match (start, end) {
        (Some(start), Some(end)) => start < to && from <= end,
        _ => false,
    }
}

/// What a view asks records to bring in.
pub enum Wanted<'a> {
    /// Sessions whose records name these commits.
    Commits(&'a BTreeSet<String>),
    /// Sessions whose records, or whose activity for a whole-session
    /// binding, meet `[from, to)`.
    Window { from: Instant, to: Instant },
    /// One session.
    Session(SessionKey),
}

/// Bring into `gathered` the sessions records name that `wanted` asks for,
/// and those a member names as its parent, each looked up by id; give
/// members the parents lineage records claim; and, unless `only`, add the
/// subagent recordings of every member they brought in. With `only`, the
/// gathering holds the sessions records name and nothing else.
pub fn extend(
    backends: &[Box<dyn Backend>],
    gathered: &mut Gathered,
    records: &Records,
    wanted: &Wanted<'_>,
    only: bool,
) -> EvidenceMembership {
    let mut membership = EvidenceMembership::default();
    if only {
        gathered
            .members
            .retain(|member| records.names(&member.key()));
    }
    for member in &mut gathered.members {
        if records.names(&member.key()) {
            member.rules.insert(MemberRule::Evidence);
        }
    }
    let present: BTreeSet<SessionKey> = gathered.members.iter().map(Member::key).collect();
    let (mut asked, whole) = match wanted {
        Wanted::Commits(commits) => (records.naming(commits), BTreeSet::new()),
        Wanted::Window { from, to } => records.meeting(*from, *to),
        Wanted::Session(session) => (
            [session.clone()]
                .into_iter()
                .filter(|session| records.names(session))
                .collect(),
            BTreeSet::new(),
        ),
    };
    // A member's recorded or claimed parent placed by records decides how
    // the member is placed, so it is read whatever its work.
    if !only {
        for member in &gathered.members {
            let recorded = window::parent_in_harness(&member.session)
                .filter(|(_, _, evidence)| *evidence != ParentEvidence::ForkOrigin)
                .map(|(harness, id, _)| SessionKey { harness, id });
            let claimed = records
                .claims(&member.key())
                .into_iter()
                .map(|(parent, _)| parent);
            asked.extend(
                recorded
                    .into_iter()
                    .chain(claimed)
                    .filter(|parent| records.names(parent)),
            );
        }
    }
    let mut added: Vec<usize> = Vec::new();
    for session in asked.iter().chain(&whole) {
        if present.contains(session) {
            continue;
        }
        membership.looked_up += 1;
        let (found, backend) = match locate(backends, session) {
            Ok(found) => found,
            Err(reason) => {
                membership.unresolved.push(Unresolved {
                    session: session.clone(),
                    reason,
                    files: records.files_of(session),
                });
                continue;
            }
        };
        if whole.contains(session) && !asked.contains(session) {
            let Wanted::Window { from, to } = wanted else {
                continue;
            };
            if !active_in(&found, *from, *to) {
                continue;
            }
        }
        gathered.membership.read += 1;
        match read_session(backends[backend].as_ref(), &found) {
            Ok(read) => {
                added.push(gathered.members.len());
                gathered.members.push(Member::new(
                    read,
                    backend,
                    BTreeSet::from([MemberRule::Evidence]),
                ));
            }
            Err(error) => gathered
                .membership
                .unreadable
                .push((session.clone(), format!("{error:#}"))),
        }
    }
    if only {
        return membership;
    }
    {
        let mut sessions: Vec<(usize, &mut Session)> = gathered
            .members
            .iter_mut()
            .enumerate()
            .filter(|(index, _)| added.contains(index))
            .map(|(_, member)| (member.backend, &mut member.session))
            .collect();
        crate::family::declare_parents(backends, &mut sessions);
    }
    super::gather::assign_parents(&mut gathered.members);
    // Lineage records fill only what recordings leave silent.
    let keys: BTreeSet<SessionKey> = gathered.members.iter().map(Member::key).collect();
    let first: BTreeMap<SessionKey, usize> = gathered
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| (member.key(), index))
        .collect();
    for member in &mut gathered.members {
        if member.rules.contains(&MemberRule::Child) {
            continue;
        }
        let claimed = records.claims(&member.key());
        if claimed.is_empty() {
            continue;
        }
        let recorded = window::parent_in_harness(&member.session)
            .filter(|(_, _, evidence)| *evidence != ParentEvidence::ForkOrigin)
            .map(|(harness, id, _)| SessionKey { harness, id });
        let mut placed_under = member.parent.clone();
        if recorded.is_none() {
            placed_under = claimed
                .iter()
                .map(|(parent, _)| parent)
                .filter(|parent| keys.contains(*parent) && **parent != member.key())
                .min_by_key(|parent| first.get(*parent).copied())
                .cloned();
            if placed_under.is_some() {
                member.parent.clone_from(&placed_under);
                membership.lineage_applied += 1;
            }
        }
        let disagrees = match &recorded {
            Some(recorded) => claimed.iter().any(|(parent, _)| parent != recorded),
            None => claimed.len() > 1,
        };
        let concerns = recorded.iter().any(|parent| keys.contains(parent))
            || claimed.iter().any(|(parent, _)| keys.contains(parent));
        if disagrees && concerns {
            membership.parent_conflicts.push(ParentConflict {
                session: member.key(),
                recorded,
                claimed,
                placed_under,
            });
        }
    }
    super::gather::add_children(backends, gathered, &added);
    membership
}
