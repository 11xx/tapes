//! Which recordings a repository's evidence comes from, and how each is read
//! and divided.
//!
//! A directory listing finds only the sessions whose recorded directory
//! still lies in the repository; a removed worktree leaves its sessions
//! where nothing can place them. So a session active in the asked interval
//! belongs to the repository's evidence when any of these holds:
//!
//! - a location it recorded lies in the repository or one of its live
//!   worktrees;
//! - a commit it created or inspected names a commit the repository holds;
//! - its recorded remote URL is one of the repository's remotes;
//! - a location, or a path a tool call named, lies under a directory the
//!   rules above placed: a gone directory where such a session made a
//!   commit, or where a session recorded the repository's remote.
//!
//! A member's subagent recordings join it, and a session whose recording
//! names a member as its launcher or spawner inherits from that member.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use super::repository::Repository;
use super::{
    partition, Amount, Amounts, Counts, EditInput, Facts, Location, Mention, Params, Partition,
    Piece, SeenCommit, SessionAttribution, SessionEvidence, SessionKey,
};
use crate::backend::{Backend, StreamOptions, StreamedTranscript};
use crate::commit_observations::CommitObservation;
use crate::event::{EventFilter, EventKind, EventOptions, EventRecord};
use crate::model::Session;
use crate::shell_commands;
use crate::span::{Instant, SpanPlacement};
use crate::window::{self, ParentEvidence};
use crate::{EventSink, ListFilters, ListSort};

/// The most sessions of each harness a gathering lists.
const LISTING_LIMIT: usize = 5_000;

/// Why a session's recording is part of a repository's evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemberRule {
    /// A recorded location lies in the repository or a live worktree.
    Location,
    /// A commit it created or inspected is the repository's.
    Commit,
    /// Its recorded remote is one of the repository's.
    Remote,
    /// A location or a named path lies under a placed directory.
    Placed,
    /// It is a member's subagent recording.
    Child,
}

/// A recording read for its evidence.
pub(crate) struct SessionRead {
    session: Session,
    read: StreamedTranscript,
    observations: Vec<CommitObservation>,
    mentions: Vec<Mention>,
    edits: Vec<EditInput>,
}

/// One session whose recording is part of the repository's evidence.
pub struct Member {
    pub session: Session,
    pub backend: usize,
    pub rules: BTreeSet<MemberRule>,
    /// The member this one inherits from where its own evidence places
    /// nothing.
    pub parent: Option<SessionKey>,
    read: SessionRead,
}

impl Member {
    pub fn key(&self) -> SessionKey {
        key_of(&self.session)
    }
}

fn key_of(session: &Session) -> SessionKey {
    SessionKey {
        harness: session.harness().to_owned(),
        id: session.id.clone(),
    }
}

/// How a gathering went: what it listed and read, and what it could not.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Membership {
    pub listed: usize,
    pub read: usize,
    /// Sessions that could not be read, with why.
    pub unreadable: Vec<(SessionKey, String)>,
    /// Gone directories the members' commits and remotes placed.
    pub placed: Vec<PathBuf>,
    /// The listing reached its bound for some harness.
    pub listing_bounded: bool,
}

/// The recordings a repository's evidence comes from for an interval.
pub struct Gathered {
    pub members: Vec<Member>,
    pub membership: Membership,
}

/// The sessions active in `[from, to]` whose recordings are part of the
/// repository's evidence, each read once, with their subagent recordings.
pub fn gather(
    backends: &[Box<dyn Backend>],
    repository: &Repository,
    from: Instant,
    to: Instant,
) -> Result<Gathered> {
    let listed = crate::list_scoped(
        backends,
        None,
        None,
        LISTING_LIMIT,
        &ListFilters {
            since: Some(from),
            ..Default::default()
        },
        ListSort::Oldest,
    )?;
    let mut membership = Membership {
        listed: listed.sessions.len(),
        listing_bounded: listed.scan_truncated,
        ..Default::default()
    };
    let mut candidates: Vec<(usize, SessionRead)> = Vec::new();
    for (session, backend) in listed.sessions.into_iter().zip(listed.origins) {
        if session.started_at.is_some_and(|started| started > to) {
            continue;
        }
        membership.read += 1;
        match read_session(backends[backend].as_ref(), &session) {
            Ok(read) => candidates.push((backend, read)),
            Err(error) => membership
                .unreadable
                .push((key_of(&session), format!("{error:#}"))),
        }
    }
    let named: BTreeSet<String> = candidates
        .iter()
        .flat_map(|(_, read)| read.observations.iter().map(|seen| seen.sha.clone()))
        .collect();
    let existing = repository.names_commits(&named)?;

    let mut rules: Vec<BTreeSet<MemberRule>> = vec![BTreeSet::new(); candidates.len()];
    let mut placed: BTreeSet<PathBuf> = BTreeSet::new();
    for ((_, read), rules) in candidates.iter().zip(rules.iter_mut()) {
        let locations = locations(read);
        if locations
            .iter()
            .any(|location| repository.holds(&location.cwd) == Some(true))
        {
            rules.insert(MemberRule::Location);
        }
        let remote = read
            .read
            .locations
            .iter()
            .flat_map(|held| held.observations.iter())
            .filter_map(|observation| observation.repository_url.as_deref())
            .any(|url| repository.has_remote(url));
        if remote {
            rules.insert(MemberRule::Remote);
            for location in &locations {
                if repository.holds(&location.cwd).is_none() {
                    placed.insert(location.cwd.clone());
                }
            }
        }
        let mut committed = false;
        for seen in read
            .observations
            .iter()
            .filter(|seen| existing.contains(&seen.sha))
        {
            committed = true;
            if !seen.created {
                continue;
            }
            let at_creation = seen.at.and_then(|at| {
                locations
                    .iter()
                    .rfind(|location| location.at.is_none_or(|stamp| stamp <= at))
            });
            if let Some(location) = at_creation {
                if repository.holds(&location.cwd).is_none() {
                    placed.insert(location.cwd.clone());
                }
            }
        }
        if committed {
            rules.insert(MemberRule::Commit);
        }
    }
    let places: Vec<PathBuf> = repository
        .worktrees
        .iter()
        .map(|worktree| worktree.path.clone())
        .chain(placed.iter().cloned())
        .collect();
    let under_placed = |path: &Path| placed.iter().any(|place| path.starts_with(place));
    let under_places = |path: &Path| places.iter().any(|place| path.starts_with(place));
    for ((_, read), rules) in candidates.iter().zip(rules.iter_mut()) {
        let located = locations(read)
            .iter()
            .any(|location| under_placed(&location.cwd));
        let named = read
            .mentions
            .iter()
            .map(|mention| mention.path.as_path())
            .chain(read.edits.iter().map(|edit| edit.path.as_path()))
            .any(under_places);
        if located || named {
            rules.insert(MemberRule::Placed);
        }
    }
    membership.placed = placed.into_iter().collect();

    let mut members: Vec<Member> = candidates
        .into_iter()
        .zip(rules)
        .filter(|(_, rules)| !rules.is_empty())
        .map(|((backend, read), rules)| Member {
            session: read.session.clone(),
            backend,
            rules,
            parent: None,
            read,
        })
        .collect();

    // A launcher or spawner named by a member's recording.
    {
        let mut sessions: Vec<(usize, &mut Session)> = members
            .iter_mut()
            .map(|member| (member.backend, &mut member.session))
            .collect();
        crate::family::declare_parents(backends, &mut sessions);
    }
    let keys: BTreeSet<SessionKey> = members.iter().map(Member::key).collect();
    for member in &mut members {
        if let Some((harness, id, evidence)) = window::parent_in_harness(&member.session) {
            let parent = SessionKey { harness, id };
            if evidence != ParentEvidence::ForkOrigin && keys.contains(&parent) {
                member.parent = Some(parent);
            }
        }
    }

    // Subagent recordings.
    let mut children: Vec<Member> = Vec::new();
    for member in &members {
        let (found, failures) = backends[member.backend].window_children(&member.session);
        for failure in failures {
            membership.unreadable.push((member.key(), failure));
        }
        for child in found {
            membership.read += 1;
            match read_session(backends[member.backend].as_ref(), &child) {
                Ok(read) => children.push(Member {
                    session: child,
                    backend: member.backend,
                    rules: BTreeSet::from([MemberRule::Child]),
                    parent: Some(member.key()),
                    read,
                }),
                Err(error) => membership
                    .unreadable
                    .push((key_of(&child), format!("{error:#}"))),
            }
        }
    }
    members.extend(children);
    Ok(Gathered {
        members,
        membership,
    })
}

/// The repository's attribution of what its members consumed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RepositoryAttribution {
    pub sessions: Vec<SessionAttribution>,
    /// The commits evidence could name, oldest first.
    pub commits: Vec<String>,
    /// More commits met the members' interval than a read keeps.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub commits_truncated: bool,
}

/// Divide every member's consumption among the repository's commits.
pub fn attribute(
    backends: &[Box<dyn Backend>],
    repository: &Repository,
    gathered: &Gathered,
    params: &Params,
) -> Result<RepositoryAttribution> {
    attribute_within(backends, repository, gathered, params, None)
}

/// As [`attribute`], each member's consumption cut to `within` where it is
/// given: only what its requests inside it consumed is divided, by the
/// partition its whole evidence gives, and its whole is that part.
pub fn attribute_within(
    backends: &[Box<dyn Backend>],
    repository: &Repository,
    gathered: &Gathered,
    params: &Params,
    within: Option<Piece>,
) -> Result<RepositoryAttribution> {
    let stamps: Vec<Instant> = gathered
        .members
        .iter()
        .flat_map(|member| requests(&member.read.read).into_iter().flatten())
        .collect();
    let (Some(first), Some(last)) = (stamps.iter().min(), stamps.iter().max()) else {
        return Ok(RepositoryAttribution {
            sessions: Vec::new(),
            commits: Vec::new(),
            commits_truncated: false,
        });
    };
    let universe = repository.universe(*first, *last, params)?;
    let mut branches: BTreeSet<String> = repository
        .worktrees
        .iter()
        .filter_map(|worktree| worktree.branch.clone())
        .collect();
    for member in &gathered.members {
        branches.extend(
            locations(&member.read)
                .into_iter()
                .filter_map(|location| location.branch),
        );
    }
    let since = *first - params.lookahead - params.lookback;
    let branch_commits = repository.branch_commits(&branches, since)?;
    let arrivals = repository.arrivals(&branches, since)?;
    let facts = Facts {
        commits: universe.facts.clone(),
        places: repository
            .worktrees
            .iter()
            .map(|worktree| worktree.path.clone())
            .chain(gathered.membership.placed.iter().cloned())
            .collect(),
        worktree_branches: repository
            .worktrees
            .iter()
            .filter_map(|worktree| Some((worktree.path.clone(), worktree.branch.clone()?)))
            .collect(),
        branch_commits,
        arrivals,
    };

    let mut evidences: Vec<(SessionKey, SessionEvidence)> = Vec::new();
    for member in &gathered.members {
        let mut evidence = evidence(&member.read, repository, &universe, params)?;
        evidence.inherits = member.parent.is_some();
        evidences.push((member.key(), evidence));
    }
    let mut partitions = divide(gathered, &evidences, &facts, params);
    let mut placed_by_every_tier = None;
    // What only the tiers left out would place is named for them.
    if !params.tiers.is_all() {
        let every = Params {
            tiers: super::Tiers::all(),
            ..*params
        };
        let full = divide(gathered, &evidences, &facts, &every);
        for (key, partition) in &mut partitions {
            if let Some(full) = full.get(key) {
                partition.name_tiers_not_asked(full);
            }
        }
        placed_by_every_tier = Some(
            full.iter()
                .map(|(key, partition)| {
                    let commits = partition
                        .entries
                        .iter()
                        .filter_map(|entry| match &entry.owner {
                            crate::attribution::Owner::Item { work, .. } => {
                                work.sha().map(str::to_owned)
                            }
                            crate::attribution::Owner::Unattributed(_) => None,
                        })
                        .collect();
                    (key.clone(), commits)
                })
                .collect::<BTreeMap<SessionKey, BTreeSet<String>>>(),
        );
    }

    let mut reads: Vec<Result<Read, String>> = Vec::new();
    let mut cut: Vec<Partition> = Vec::new();
    for member in &gathered.members {
        let mut partition = partitions.remove(&member.key()).unwrap_or_default();
        if let Some(within) = within {
            partition.cut_to(within);
        }
        reads.push(
            read_amounts(
                backends[member.backend].as_ref(),
                member,
                &partition,
                within,
            )
            .map_err(|error| format!("{error:#}")),
        );
        cut.push(partition);
    }
    let withheld = explain_by_families(gathered, &mut reads);

    let mut sessions = Vec::new();
    for (index, (member, partition)) in gathered.members.iter().zip(cut).enumerate() {
        let key = member.key();
        let mut notes = Vec::new();
        if !partition.stretches {
            notes.push(
                "the read kept no request timeline, so no working stretch was estimated".to_owned(),
            );
        }
        if withheld.contains(&index) {
            notes.push(
                "the part of the recorded total no request explains is withheld: its subagents' \
                 requests, which the total counts, are not all known"
                    .to_owned(),
            );
        }
        let (amounts, billing, measured, agent) = match reads[index].as_ref() {
            Ok(read) => (
                Some(read.amounts.clone()),
                read.billing.clone(),
                read.counters.clone(),
                read.agent.clone(),
            ),
            Err(error) => {
                notes.push(format!(
                    "the recording could not be read in pieces: {error}"
                ));
                (None, None, None, agent_label(&member.session))
            }
        };
        let placed_by_every_tier =
            placed_by_every_tier
                .as_ref()
                .map(|placed: &BTreeMap<SessionKey, BTreeSet<String>>| {
                    placed.get(&key).cloned().unwrap_or_default()
                });
        sessions.push(SessionAttribution {
            session: key,
            parent: member.parent.clone(),
            agent,
            rules: member.rules.clone(),
            started_at: member.session.started_at,
            last_activity_at: member.session.last_activity_at,
            billing,
            measured,
            placed_by_every_tier,
            partition,
            amounts,
            notes,
        });
    }
    Ok(RepositoryAttribution {
        sessions,
        commits: universe
            .facts
            .iter()
            .map(|commit| commit.sha.clone())
            .collect(),
        commits_truncated: universe.set.commits_truncated,
    })
}

/// Every member divided by its own evidence, and then each child given its
/// parent's allocation where its own evidence places nothing, parents
/// before children, so inheritance carries down a chain.
fn divide(
    gathered: &Gathered,
    evidences: &[(SessionKey, SessionEvidence)],
    facts: &Facts,
    params: &Params,
) -> BTreeMap<SessionKey, Partition> {
    let mut partitions: BTreeMap<SessionKey, Partition> = evidences
        .iter()
        .map(|(key, evidence)| (key.clone(), partition(evidence, facts, params)))
        .collect();
    let parents: BTreeMap<SessionKey, SessionKey> = gathered
        .members
        .iter()
        .filter_map(|member| Some((member.key(), member.parent.clone()?)))
        .collect();
    let depth = |key: &SessionKey| {
        let mut depth = 0;
        let mut at = key;
        while let Some(parent) = parents.get(at) {
            depth += 1;
            at = parent;
            if depth > parents.len() {
                break;
            }
        }
        depth
    };
    let mut order: Vec<&SessionKey> = parents.keys().collect();
    order.sort_by_key(|key| depth(key));
    for child in order {
        let parent = &parents[child];
        if parent == child {
            continue;
        }
        if let Some(parent) = partitions.get(parent).cloned() {
            if let Some(partition) = partitions.get_mut(child) {
                partition.inherit(&parent);
            }
        }
    }
    partitions
}

/// What one member's read took: its amounts, the bounds they were read
/// over, and what its family's measure of a recorded total needs.
struct Read {
    amounts: Amounts,
    bounds: Vec<crate::span::SpanBounds>,
    /// The recorded totals of a recording whose part no request explains.
    chain: Option<crate::unexplained::RecordedChain>,
    /// A subagent recording's requests in time, for its parent's total.
    timed: Option<Vec<crate::unexplained::TimedRequest>>,
    measured: bool,
    counters: Option<crate::model::Tokens>,
    billing: Option<crate::pricing::Billing>,
    agent: String,
}

/// A Claude parent's recorded total counts its subagents' requests, and
/// those subagents are members measured by their own requests, so the part
/// of the total no request explains is measured against the whole family's
/// requests, as a window read measures it: each request is counted once. A
/// family whose subagents' requests are not all known withholds the
/// parent's part, which its subagents' shares may hold again. The members
/// whose part was withheld are returned.
fn explain_by_families(gathered: &Gathered, reads: &mut [Result<Read, String>]) -> BTreeSet<usize> {
    let mut families: BTreeMap<SessionKey, Vec<usize>> = BTreeMap::new();
    for (index, member) in gathered.members.iter().enumerate() {
        if let (true, Some(parent)) = (member.rules.contains(&MemberRule::Child), &member.parent) {
            families.entry(parent.clone()).or_default().push(index);
        }
    }
    let mut withheld = BTreeSet::new();
    for (index, member) in gathered.members.iter().enumerate() {
        let Some(children) = families.get(&member.key()) else {
            continue;
        };
        let Some(chain) = reads[index]
            .as_ref()
            .ok()
            .and_then(|read| read.chain.clone())
        else {
            continue;
        };
        let mut requests = Vec::new();
        let mut complete = true;
        for child in children {
            match &reads[*child] {
                Ok(Read {
                    timed: Some(timed), ..
                }) if requests.len() + timed.len() <= crate::unexplained::MAX_FAMILY_REQUESTS => {
                    requests.extend(timed.iter().cloned());
                }
                Ok(Read {
                    timed: None,
                    measured: false,
                    ..
                }) => {}
                _ => complete = false,
            }
        }
        let Ok(read) = reads[index].as_mut() else {
            continue;
        };
        let placed = complete
            .then(|| chain.accrual(Some(&requests)).place(&read.bounds))
            .flatten();
        if !complete {
            withheld.insert(index);
        }
        read.amounts.explain(placed.as_ref());
    }
    withheld
}

impl Amounts {
    /// Replace every figure's unexplained part with what `placed` gives it,
    /// or with none.
    fn explain(&mut self, placed: Option<&crate::unexplained::PlacedUnexplained>) {
        let parts = |unexplained: Option<&crate::unexplained::Unexplained>| {
            let mut models: BTreeMap<String, Counts> = BTreeMap::new();
            for model in unexplained
                .map(|part| part.by_model.as_slice())
                .unwrap_or_default()
            {
                models
                    .entry(model.model.clone())
                    .or_default()
                    .add(&Counts::of(&model.tokens, None));
            }
            models.retain(|_, counts| !counts.is_zero());
            models
        };
        for (position, piece) in self.pieces.iter_mut().enumerate() {
            piece.unexplained = parts(placed.and_then(|placed| placed.spans.get(position)));
        }
        let count = self.pieces.len();
        match self.within {
            true => {
                self.whole.unexplained = parts(placed.and_then(|placed| placed.spans.get(count)));
            }
            false => {
                self.remainder.unexplained = parts(placed.map(|placed| &placed.remainder));
                self.whole.unexplained = parts(placed.map(|placed| &placed.whole));
            }
        }
    }
}

/// The label of the agent a figure was served by: `harness:model#effort`.
fn agent_label(session: &Session) -> String {
    let mut label = session.harness().to_owned();
    if let Some(model) = &session.model {
        label.push(':');
        label.push_str(&model.id);
        if let Some(variant) = &model.variant {
            label.push('#');
            label.push_str(variant);
        }
    }
    label
}

/// The partition's pieces, read from the recording in one pass, and the
/// window the pieces were cut to as the whole, where one was.
fn read_amounts(
    backend: &dyn Backend,
    member: &Member,
    partition: &Partition,
    within: Option<Piece>,
) -> Result<Read> {
    let mut bounds: Vec<_> = partition
        .pieces
        .iter()
        .map(|piece| piece.bounds())
        .collect();
    if let Some(within) = within {
        bounds.push(within.bounds());
    }
    let mut figures = backend
        .stream_spans(&member.read.session, &member.read.read, &bounds)?
        .billed_as_whole();
    let detail = figures.whole.usage_detail.as_ref();
    let chain = detail
        .filter(|detail| detail.unexplained.is_some())
        .and_then(|detail| detail.recorded_chain.clone());
    let timed = detail.and_then(|detail| detail.timed_requests.clone());
    let billing = detail.and_then(|detail| detail.billing.clone());
    let measured = figures.whole.tokens.is_some();
    let counters = figures.whole.tokens.clone();
    let agent = agent_label(&figures.whole);
    let whole = match within {
        Some(_) => {
            let (figure, placement) = figures.spans.pop().expect("the window was read");
            amount(&figure, Some(&placement))
        }
        None => amount(&figures.whole, None),
    };
    Ok(Read {
        amounts: Amounts {
            pieces: figures
                .spans
                .iter()
                .map(|(figure, placement)| amount(figure, Some(placement)))
                .collect(),
            remainder: amount(&figures.remainder.0, Some(&figures.remainder.1)),
            whole,
            within: within.is_some(),
        },
        bounds,
        chain,
        timed,
        measured,
        counters,
        billing,
        agent,
    })
}

/// What one figure of a span read holds.
fn amount(figure: &Session, placement: Option<&SpanPlacement>) -> Amount {
    let mut amount = Amount {
        requests: placement.map_or_else(
            || {
                figure
                    .usage_detail
                    .as_ref()
                    .and_then(|detail| detail.by_model.as_ref())
                    .map_or(0, |models| {
                        models
                            .iter()
                            .filter_map(|model| model.request_count)
                            .sum::<usize>()
                    }) as u64
            },
            |placement| placement.requests as u64,
        ),
        ..Default::default()
    };
    let detail = figure.usage_detail.as_ref();
    match detail.and_then(|detail| detail.by_model.as_ref()) {
        Some(models) => {
            for model in models {
                if let Some(tokens) = &model.tokens {
                    amount
                        .models
                        .entry(model.model.clone())
                        .or_default()
                        .add(&Counts::of(tokens, model.cache_write_by_ttl.as_ref()));
                }
            }
        }
        None => {
            if let Some(tokens) = &figure.tokens {
                let model = figure
                    .model
                    .as_ref()
                    .map_or_else(String::new, |model| model.id.clone());
                let ttl = detail.and_then(|detail| detail.cache_write_by_ttl.as_ref());
                amount
                    .models
                    .entry(model)
                    .or_default()
                    .add(&Counts::of(tokens, ttl));
            }
        }
    }
    amount.models.retain(|_, counts| !counts.is_zero());
    if let Some(unexplained) = detail.and_then(|detail| detail.unexplained.as_ref()) {
        for model in &unexplained.by_model {
            amount
                .unexplained
                .entry(model.model.clone())
                .or_default()
                .add(&Counts::of(&model.tokens, None));
        }
    }
    amount
}

/// Read a recording once for its evidence: the request timeline and
/// working stretches, its locations, the commits its results name, and the
/// directories its tool calls name.
fn read_session(backend: &dyn Backend, session: &Session) -> Result<SessionRead> {
    #[derive(Default)]
    struct Collect {
        observations: Vec<CommitObservation>,
        mentions: Vec<Mention>,
        edits: Vec<EditInput>,
    }
    impl EventSink for Collect {
        fn session(&mut self, _: &Session) -> Result<()> {
            Ok(())
        }
        fn record(&mut self, record: EventRecord) -> Result<()> {
            if let Some(commits) = &record.commits {
                self.observations
                    .extend(commits.observations.iter().cloned());
            }
            if let Some(edits) = &record.edits {
                for edit in &edits.observations {
                    if let Some(at) = edit.at {
                        self.edits.push(EditInput {
                            at,
                            path: PathBuf::from(&edit.path),
                            added: edit.added.clone(),
                        });
                    }
                }
            }
            if record.event.kind == EventKind::ToolCall {
                if let (Some(at), Some(arguments)) = (record.ts, &record.event.arguments) {
                    let text = arguments
                        .complete_text()
                        .unwrap_or(arguments.preview.as_str());
                    let shell = shell_commands::is_shell_tool(record.event.name.as_deref());
                    for path in named_directories(text, shell) {
                        self.mentions.push(Mention { at, path });
                    }
                }
            }
            Ok(())
        }
    }
    let mut sink = Collect::default();
    let streamed = crate::events_full_for_session_with(
        backend,
        session,
        usize::MAX,
        &EventFilter::default(),
        EventOptions {
            full_arguments: true,
        },
        StreamOptions { timeline: true },
        &mut sink,
    )?;
    Ok(SessionRead {
        session: session.clone(),
        read: streamed.read,
        observations: sink.observations,
        mentions: sink.mentions,
        edits: sink.edits,
    })
}

/// The stamp of each request the read counted, in recording order.
fn requests(read: &StreamedTranscript) -> Vec<Option<Instant>> {
    crate::usage::timeline(read)
        .map(|timeline| {
            timeline
                .requests
                .iter()
                .map(|request| request.timestamp)
                .collect()
        })
        .unwrap_or_default()
}

fn locations(read: &SessionRead) -> Vec<Location> {
    let mut held: Vec<Location> = read
        .read
        .locations
        .iter()
        .flat_map(|locations| locations.observations.iter())
        .map(|observation| Location {
            at: observation.at,
            cwd: PathBuf::from(&observation.cwd),
            branch: observation.branch.clone(),
            outside: false,
        })
        .collect();
    if held.is_empty() {
        if let Some(directory) = &read.session.directory {
            held.push(Location {
                at: read.session.started_at,
                cwd: directory.clone(),
                branch: None,
                outside: false,
            });
        }
    }
    held
}

/// One member's evidence, its observed shas resolved in the repository.
fn evidence(
    read: &SessionRead,
    repository: &Repository,
    universe: &super::repository::Universe,
    params: &Params,
) -> Result<SessionEvidence> {
    let starts = crate::usage::stretches(&read.read, params.idle.num_milliseconds())
        .map(|stretches| stretches.iter().map(|stretch| stretch.first).collect());
    let mut commits = Vec::new();
    for seen in &read.observations {
        commits.push(SeenCommit {
            at: seen.at,
            call: seen.call.ordinal,
            created: seen.created,
            branch: seen.branch.clone(),
            observed: seen.sha.clone(),
            resolved: repository.resolve(universe, &seen.sha, params)?,
        });
    }
    Ok(SessionEvidence {
        requests: requests(&read.read),
        starts,
        locations: locations(read)
            .into_iter()
            .map(|location| Location {
                outside: repository.holds(&location.cwd) == Some(false),
                ..location
            })
            .collect(),
        commits,
        mentions: read.mentions.clone(),
        edits: read.edits.clone(),
        inherits: false,
    })
}

/// The absolute paths a tool call's argument text names as a directory to
/// work in or a file to write: the values of `workdir`, `cwd`, `file_path`,
/// `notebook_path` and `path` members, wherever they appear in JSON or in a
/// script that embeds JSON, and for a shell tool the operands of `cd` and
/// `git -C` in its commands.
fn named_directories(text: &str, shell: bool) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for key in ["workdir", "cwd", "file_path", "notebook_path", "path"] {
        let quoted = format!("\"{key}\"");
        let mut rest = text;
        while let Some(at) = rest.find(&quoted) {
            rest = &rest[at + quoted.len()..];
            let Some(value) = rest.trim_start().strip_prefix(':') else {
                continue;
            };
            let Some(value) = value.trim_start().strip_prefix('"') else {
                continue;
            };
            let path = &value[..value.find(['"', '\\']).unwrap_or(value.len())];
            if path.starts_with('/') {
                found.push(PathBuf::from(path));
            }
        }
    }
    if shell {
        for command in shell_commands::commands(text) {
            let words: Vec<&str> = command.words.iter().map(String::as_str).collect();
            match words.as_slice() {
                ["cd", directory, ..] if directory.starts_with('/') => {
                    found.push(PathBuf::from(directory));
                }
                [program, rest @ ..] if shell_commands::basename(program) == "git" => {
                    for pair in rest.windows(2) {
                        if pair[0] == "-C" && pair[1].starts_with('/') {
                            found.push(PathBuf::from(pair[1]));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::named_directories;
    use std::path::PathBuf;

    #[test]
    fn named_directories_reads_members_and_shell_operands() {
        let text = r#"const r = await tools.exec_command({"cmd":"bash -lc 'cd /w/one && git -C /w/two status'","workdir":"/w/three"});"#;
        assert_eq!(
            named_directories(text, true),
            vec![
                PathBuf::from("/w/one"),
                PathBuf::from("/w/three"),
                PathBuf::from("/w/two")
            ]
        );
        let edit = r#"{"file_path":"/w/four/src/lib.rs","old_string":"a","new_string":"b"}"#;
        assert_eq!(
            named_directories(edit, false),
            vec![PathBuf::from("/w/four/src/lib.rs")]
        );
        assert!(named_directories(r#"{"path":"relative/file"}"#, false).is_empty());
    }
}
