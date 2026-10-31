//! T3 Code references in session arguments, and the T3 thread index behind a
//! listing. A T3 thread is a name for a native session, so a reference is
//! rewritten to the native id before any selector is read.

use agent_tapes_core::t3::{parse_link, NativeThread, Resolution, T3Index, T3Link, T3Store};
use agent_tapes_core::SessionList;
use anyhow::{anyhow, Result};

/// Where a selector reads from, which decides what a reference can mean.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Installed,
    /// A supplied input or `--native-store`: no T3 store is involved.
    Supplied,
    /// A replica, which resolves references against its own store.
    Remote,
}

/// Rewrite the session argument and each `--exclude`. Only the session
/// argument selects a harness: an excluded session says nothing about where
/// the latest one lies.
pub fn rewrite(
    session: &mut Option<String>,
    exclude: &mut [String],
    harness: &mut Option<String>,
    origin: Origin,
) -> Result<()> {
    if origin == Origin::Remote {
        return Ok(());
    }
    if origin == Origin::Supplied {
        for argument in session.iter().chain(exclude.iter()) {
            if parse_link(argument).is_ok_and(|link| link.is_none()) {
                continue;
            }
            return Err(anyhow!(
                "{argument} names a thread of the installed T3 store and selects nothing from a supplied input or --native-store"
            ));
        }
        return Ok(());
    }
    let store = T3Store::from_env();
    let mut rewriter = Rewriter {
        store: store.as_ref(),
        warned: false,
    };
    if let Some(argument) = session {
        if let Some(thread) = rewriter.resolve(argument)? {
            match harness.as_deref() {
                Some(named) if named != thread.harness => {
                    return Err(anyhow!(
                        "T3 thread {} is {} session {}, but --harness names {named}",
                        thread.thread_id,
                        thread.harness,
                        thread.native_id
                    ));
                }
                _ => *harness = Some(thread.harness.to_owned()),
            }
            eprintln!("{}", note(&thread));
            argument.clone_from(&thread.native_id);
        }
    }
    for argument in exclude {
        if let Some(thread) = rewriter.resolve(argument)? {
            eprintln!("{}", note(&thread));
            argument.clone_from(&thread.native_id);
        }
    }
    Ok(())
}

struct Rewriter<'a> {
    store: Option<&'a T3Store>,
    warned: bool,
}

impl Rewriter<'_> {
    fn resolve(&mut self, argument: &str) -> Result<Option<NativeThread>> {
        match parse_link(argument)? {
            Some(link) => resolve_link(self.store, &link).map(Some),
            None => self.resolve_bare(argument),
        }
    }

    /// A bare argument is a reference only when the store holds exactly that
    /// thread id. A store that cannot answer must not break an ordinary id.
    fn resolve_bare(&mut self, argument: &str) -> Result<Option<NativeThread>> {
        let Some(store) = self.store.filter(|store| store.exists()) else {
            return Ok(None);
        };
        if self.warned || argument.chars().any(char::is_control) {
            return Ok(None);
        }
        match store.resolve(argument) {
            Ok(Resolution::Absent) => Ok(None),
            Ok(Resolution::Native(thread)) => Ok(Some(thread)),
            Ok(Resolution::Refused(reason)) => Err(anyhow!(reason)),
            Err(error) => {
                self.warned = true;
                eprintln!("Warning: T3 thread ids were not checked: {error:#}");
                Ok(None)
            }
        }
    }
}

fn resolve_link(store: Option<&T3Store>, link: &T3Link) -> Result<NativeThread> {
    let thread = &link.thread_id;
    let store = store
        .filter(|store| store.exists())
        .ok_or_else(|| anyhow!("T3 thread {thread}: {}", no_store(store)))?;
    let database = store.database();
    if let (Some(named), Some(served)) = (&link.environment_id, store.environment_id()) {
        if *named != served {
            return Err(anyhow!(
                "T3 thread {thread}: the link names environment {named}, but the T3 store {} serves environment {served}; the thread is on the machine serving {named}",
                database.display()
            ));
        }
    }
    match store
        .resolve(thread)
        .map_err(|error| anyhow!("T3 thread {thread}: {error:#}"))?
    {
        Resolution::Native(native) => Ok(native),
        Resolution::Absent => Err(anyhow!(
            "T3 thread {thread} is not in the T3 store {}",
            database.display()
        )),
        Resolution::Refused(reason) => Err(anyhow!(reason)),
    }
}

/// What is missing when no T3 store can be read.
fn no_store(store: Option<&T3Store>) -> String {
    match store {
        Some(store) => format!(
            "there is no T3 store at {}; T3CODE_HOME names its base directory",
            store.database().display()
        ),
        None => "there is no T3 store: neither T3CODE_HOME nor HOME is set".to_owned(),
    }
}

/// The line that reports what a reference became.
fn note(thread: &NativeThread) -> String {
    let mut line = format!(
        "Note: T3 thread {} \"{}\" is {} session {}.",
        thread.thread_id, thread.title, thread.harness, thread.native_id
    );
    if !thread.others.is_empty() {
        line.push_str(&format!(" It also ran on {}.", thread.others.join(", ")));
    }
    if thread.deleted {
        line.push_str(" T3 marks the thread deleted.");
    }
    line
}

/// A listing's one read of the T3 store.
#[derive(Default)]
pub struct ListingIndex {
    index: T3Index,
    failure: Option<String>,
}

impl ListingIndex {
    /// Read the installed store's index. A `required` listing is defined by
    /// it and fails without it; any other listing stays intact, reporting a
    /// store it could not read.
    pub fn load(required: bool) -> Result<Self> {
        let store = T3Store::from_env();
        let Some(readable) = store.as_ref().filter(|store| store.exists()) else {
            if required {
                return Err(anyhow!("--t3: {}", no_store(store.as_ref())));
            }
            return Ok(Self::default());
        };
        match readable.index() {
            Ok(index) => Ok(Self {
                index,
                failure: None,
            }),
            Err(error) if required => Err(anyhow!("--t3: {error:#}")),
            Err(error) => Ok(Self {
                index: T3Index::default(),
                failure: Some(format!(
                    "T3 store {}: {error:#}",
                    readable.database().display()
                )),
            }),
        }
    }

    pub fn native_ids(&self) -> std::collections::HashSet<String> {
        self.index.native_ids()
    }

    /// Annotate a listing and add what the read could not cover. A `required`
    /// listing keeps only sessions a thread of their own harness maps to.
    pub fn annotate(&self, list: &mut SessionList, required: bool) {
        self.index.annotate(&mut list.sessions);
        if required {
            list.sessions.retain(|session| session.t3.is_some());
        }
        list.unavailable.extend(self.failure.clone());
        list.unavailable.extend(self.index.truncation());
    }
}
