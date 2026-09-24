use crate::{DiscoveryError, Harness, IdentityBasis, NativeSession, NativeStore};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, File, ReadDir};
use std::io::{self, Read};
use std::mem::size_of;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use crate::IdentityBasis as FileIdentityBasis;

const INITIAL_OPENING_BYTES: usize = 64 * 1024;
const MAX_OPENING_BYTES: usize = 1024 * 1024;
const MAX_VISITED_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 64;
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const MAX_PREFIX_CANDIDATES: usize = 1_000;

#[derive(Clone, Debug, PartialEq)]
pub struct CandidatePage<R = NativeSession, E = DiscoveryError> {
    pub records: Vec<R>,
    pub scanned: usize,
    pub visited_entries: usize,
    pub complete: bool,
    pub failures: Vec<E>,
    pub unreadable_ids: Vec<String>,
}

impl CandidatePage<NativeSession, DiscoveryError> {
    fn empty() -> Self {
        Self {
            records: Vec::new(),
            scanned: 0,
            visited_entries: 0,
            complete: true,
            failures: Vec::new(),
            unreadable_ids: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct FileEntry {
    path: PathBuf,
    modified: SystemTime,
}

#[derive(Debug)]
struct Walk {
    entries: Vec<FileEntry>,
    scanned: usize,
    complete: bool,
    failures: Vec<DiscoveryError>,
    retained: usize,
    entry_limit: usize,
}

impl Walk {
    fn new(entry_limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            scanned: 0,
            complete: true,
            failures: Vec::new(),
            retained: 0,
            entry_limit,
        }
    }

    fn fail(&mut self, error: DiscoveryError) {
        self.complete = false;
        let charge = error.to_string().len() + size_of::<DiscoveryError>();
        if self.retained.saturating_add(charge) <= MAX_RETAINED_BYTES {
            self.retained += charge;
            self.failures.push(error);
        } else if !self.failures.iter().any(|failure| {
            matches!(
                failure,
                DiscoveryError::BoundExhausted {
                    bound: "16 MiB retained data",
                    ..
                }
            )
        }) {
            self.failures.push(DiscoveryError::BoundExhausted {
                coordinate: "native store".to_owned(),
                bound: "16 MiB retained data",
            });
        }
    }
}

pub(crate) fn candidates(store: &NativeStore, limit: usize) -> CandidatePage {
    let Some(root) = store.root() else {
        return CandidatePage::empty();
    };
    let mut walk = walk(store, root);
    if walk.entries.is_empty() && walk.failures.is_empty() {
        return CandidatePage {
            records: Vec::new(),
            scanned: 0,
            visited_entries: walk.scanned,
            complete: walk.complete,
            failures: walk.failures,
            unreadable_ids: Vec::new(),
        };
    }
    walk.entries.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then(left.path.cmp(&right.path))
    });

    let mut retained = walk.retained;
    let mut page = CandidatePage {
        records: Vec::new(),
        scanned: 0,
        visited_entries: walk.scanned,
        complete: walk.complete,
        failures: walk.failures,
        unreadable_ids: Vec::new(),
    };
    let limit = limit.min(MAX_PREFIX_CANDIDATES);
    let mut seen = HashSet::<String>::new();
    let entry_count = walk.entries.len();
    let mut inspected = 0;
    for entry in walk.entries {
        if inspected >= limit {
            page.complete = false;
            page_failure(
                store,
                &mut page,
                &mut retained,
                DiscoveryError::BoundExhausted {
                    coordinate: store.coordinate(),
                    bound: "1,000 prefix candidates",
                },
            );
            break;
        }
        inspected += 1;
        page.scanned = inspected;
        match probe(store, &entry.path) {
            Ok(Some(session)) => {
                let id_charge = session.id().len() + size_of::<String>();
                if retained.saturating_add(id_charge) > MAX_RETAINED_BYTES {
                    page.complete = false;
                    page_failure(
                        store,
                        &mut page,
                        &mut retained,
                        DiscoveryError::BoundExhausted {
                            coordinate: store.coordinate(),
                            bound: "16 MiB retained data",
                        },
                    );
                    break;
                }
                let is_new = seen.insert(session.id().to_owned());
                retained += id_charge;
                if is_new {
                    let record_charge = session.id().len()
                        + session.store_coordinate().len()
                        + session
                            .locator()
                            .map(|path| path.as_os_str().len())
                            .unwrap_or_default()
                        + size_of::<NativeSession>();
                    if retained.saturating_add(record_charge) > MAX_RETAINED_BYTES {
                        page.complete = false;
                        page_failure(
                            store,
                            &mut page,
                            &mut retained,
                            DiscoveryError::BoundExhausted {
                                coordinate: store.coordinate(),
                                bound: "16 MiB retained data",
                            },
                        );
                        break;
                    }
                    retained += record_charge;
                    page.records.push(session);
                }
            }
            Ok(None) => {}
            Err(error) => {
                if let Some(id) = filename_id(store.harness(), &entry.path) {
                    let charge = id.len() + size_of::<String>();
                    if retained.saturating_add(charge) <= MAX_RETAINED_BYTES {
                        retained += charge;
                        page.unreadable_ids.push(id);
                    } else {
                        page.complete = false;
                    }
                }
                page_failure(store, &mut page, &mut retained, error);
                page.complete = false;
            }
        }
    }
    if entry_count > inspected && page.complete {
        page.complete = false;
        page_failure(
            store,
            &mut page,
            &mut retained,
            DiscoveryError::BoundExhausted {
                coordinate: store.coordinate(),
                bound: "1,000 prefix candidates",
            },
        );
    }
    page.unreadable_ids.sort();
    page.unreadable_ids.dedup();
    page
}

fn page_failure(
    store: &NativeStore,
    page: &mut CandidatePage,
    retained: &mut usize,
    error: DiscoveryError,
) {
    let charge = size_of::<DiscoveryError>() + error.to_string().len();
    if retained.saturating_add(charge) <= MAX_RETAINED_BYTES {
        *retained += charge;
        page.failures.push(error);
        return;
    }
    page.complete = false;
    if !page.failures.iter().any(|failure| {
        matches!(
            failure,
            DiscoveryError::BoundExhausted {
                bound: "16 MiB retained data",
                ..
            }
        )
    }) {
        page.failures.push(DiscoveryError::BoundExhausted {
            coordinate: store.coordinate(),
            bound: "16 MiB retained data",
        });
    }
}

pub(crate) fn locate_exact(
    store: &NativeStore,
    id: &str,
) -> Result<Option<NativeSession>, DiscoveryError> {
    locate_exact_using(store, id, probe)
}

fn locate_exact_using(
    store: &NativeStore,
    id: &str,
    mut inspect: impl FnMut(&NativeStore, &Path) -> Result<Option<NativeSession>, DiscoveryError>,
) -> Result<Option<NativeSession>, DiscoveryError> {
    let Some(root) = store.root() else {
        return Ok(None);
    };
    let walked = walk(store, root);
    let mut entries = walked.entries;
    entries.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then(left.path.cmp(&right.path))
    });
    let mut first_failure = walked.failures.into_iter().next();
    let mut filename_matches = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if filename_id(store.harness(), &entry.path).as_deref() == Some(id)
            || stem_matches(store.harness(), &entry.path, id)
        {
            filename_matches.push(index);
        }
    }
    for index in filename_matches {
        let path = &entries[index].path;
        match inspect(store, path) {
            Ok(Some(session)) if session.id() == id => return Ok(Some(session)),
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }

    // Native headers are authoritative and may differ from a rollout filename.
    // Probe opening metadata only; never decode the recording body here.
    for entry in &entries {
        if filename_id(store.harness(), &entry.path).as_deref() == Some(id)
            || stem_matches(store.harness(), &entry.path, id)
        {
            continue;
        }
        match inspect(store, &entry.path) {
            Ok(Some(session)) if session.id() == id => return Ok(Some(session)),
            Ok(_) => {}
            Err(error) => {
                if first_failure.is_none() {
                    first_failure = Some(error);
                }
            }
        }
    }
    if let Some(error) = first_failure {
        return Err(error);
    }
    Ok(None)
}

fn walk(store: &NativeStore, root: &Path) -> Walk {
    walk_with_entry_limit(store, root, MAX_VISITED_ENTRIES)
}

fn walk_with_entry_limit(store: &NativeStore, root: &Path, entry_limit: usize) -> Walk {
    let mut walk = Walk::new(entry_limit);
    let root_metadata = match fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return walk,
        Err(error) => {
            walk.fail(DiscoveryError::io(
                store.coordinate(),
                "inspect store root",
                &error,
            ));
            return walk;
        }
    };
    if !root_metadata.is_dir() {
        walk.fail(DiscoveryError::InvalidMetadata {
            coordinate: store.coordinate(),
            reason: "store root is not a directory",
        });
        return walk;
    }
    let mut visited = HashSet::new();
    visited.insert((root_metadata.dev(), root_metadata.ino()));
    match store.harness() {
        Harness::Claude => walk_claude(store, root, &mut walk, &mut visited),
        Harness::Codex | Harness::Pi => visit_tree(store, root, 0, &mut walk, &mut visited),
        Harness::OpenCode => {}
    }
    walk
}

fn walk_claude(
    store: &NativeStore,
    root: &Path,
    walk: &mut Walk,
    visited: &mut HashSet<(u64, u64)>,
) {
    let projects = match read_dir(store, root, walk) {
        Some(entries) => entries,
        None => return,
    };
    for entry in projects {
        if entry_limit_reached(root, walk) {
            return;
        }
        let Some((path, metadata)) = entry_metadata(store, entry, walk) else {
            continue;
        };
        if !metadata.is_dir() || !mark_directory(store, &metadata, &path, walk, visited) {
            continue;
        }
        let sessions = match read_dir(store, &path, walk) {
            Some(entries) => entries,
            None => continue,
        };
        for entry in sessions {
            if entry_limit_reached(&path, walk) {
                return;
            }
            let Some((path, metadata)) = entry_metadata(store, entry, walk) else {
                continue;
            };
            if metadata.is_file() && is_jsonl(&path) {
                retain_file(store, path, metadata, walk);
            }
        }
    }
}

fn visit_tree(
    store: &NativeStore,
    directory: &Path,
    depth: usize,
    walk: &mut Walk,
    visited: &mut HashSet<(u64, u64)>,
) {
    if depth >= MAX_DEPTH {
        walk.fail(DiscoveryError::BoundExhausted {
            coordinate: directory.display().to_string(),
            bound: "directory depth 64",
        });
        return;
    }
    let Some(entries) = read_dir(store, directory, walk) else {
        return;
    };
    for entry in entries {
        if entry_limit_reached(directory, walk) {
            return;
        }
        let Some((path, metadata)) = entry_metadata(store, entry, walk) else {
            continue;
        };
        if metadata.is_dir() {
            if mark_directory(store, &metadata, &path, walk, visited) {
                visit_tree(store, &path, depth + 1, walk, visited);
            }
        } else if metadata.is_file() && is_jsonl(&path) && native_filename(store.harness(), &path) {
            retain_file(store, path, metadata, walk);
        }
    }
}

fn entry_limit_reached(path: &Path, walk: &mut Walk) -> bool {
    if walk.scanned < walk.entry_limit {
        return false;
    }
    let bound = entry_limit_name(walk.entry_limit);
    if !walk.failures.iter().any(|error| {
        matches!(
            error,
            DiscoveryError::BoundExhausted {
                bound: observed,
                ..
            } if *observed == bound
        )
    }) {
        walk.fail(DiscoveryError::BoundExhausted {
            coordinate: path.display().to_string(),
            bound,
        });
    }
    true
}

fn entry_limit_name(limit: usize) -> &'static str {
    if limit == MAX_VISITED_ENTRIES {
        "100,000 directory entries"
    } else {
        "injected directory-entry limit"
    }
}

fn read_dir(_store: &NativeStore, path: &Path, walk: &mut Walk) -> Option<ReadDir> {
    match fs::read_dir(path) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            walk.fail(DiscoveryError::io(
                path.display().to_string(),
                "read directory",
                &error,
            ));
            None
        }
    }
}

fn entry_metadata(
    store: &NativeStore,
    entry: io::Result<fs::DirEntry>,
    walk: &mut Walk,
) -> Option<(PathBuf, fs::Metadata)> {
    if walk.scanned >= walk.entry_limit {
        walk.complete = false;
        let bound = entry_limit_name(walk.entry_limit);
        if !walk.failures.iter().any(|error| {
            matches!(
                error,
                DiscoveryError::BoundExhausted {
                    bound: observed,
                    ..
                } if *observed == bound
            )
        }) {
            walk.fail(DiscoveryError::BoundExhausted {
                coordinate: store.coordinate(),
                bound,
            });
        }
        return None;
    }
    walk.scanned += 1;
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) => {
            walk.fail(DiscoveryError::io(
                store.coordinate(),
                "read directory entry",
                &error,
            ));
            return None;
        }
    };
    let path = entry.path();
    match fs::metadata(&path) {
        Ok(metadata) => Some((path, metadata)),
        Err(error) => {
            walk.fail(DiscoveryError::io(
                path.display().to_string(),
                "inspect entry",
                &error,
            ));
            None
        }
    }
}

fn mark_directory(
    store: &NativeStore,
    metadata: &fs::Metadata,
    path: &Path,
    walk: &mut Walk,
    visited: &mut HashSet<(u64, u64)>,
) -> bool {
    if visited.insert((metadata.dev(), metadata.ino())) {
        true
    } else {
        let _ = store;
        let _ = path;
        let _ = walk;
        false
    }
}

fn retain_file(store: &NativeStore, path: PathBuf, metadata: fs::Metadata, walk: &mut Walk) {
    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            walk.fail(DiscoveryError::io(
                path.display().to_string(),
                "read modification time",
                &error,
            ));
            return;
        }
    };
    let charge = path.as_os_str().len() + size_of::<FileEntry>() + 80;
    if walk.retained.saturating_add(charge) > MAX_RETAINED_BYTES {
        walk.complete = false;
        walk.fail(DiscoveryError::BoundExhausted {
            coordinate: store.coordinate(),
            bound: "16 MiB retained data",
        });
        return;
    }
    walk.retained += charge;
    walk.entries.push(FileEntry { path, modified });
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

fn native_filename(harness: Harness, path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    match harness {
        Harness::Claude => true,
        Harness::Codex => stem.starts_with("rollout-") && stem.len() > 36,
        Harness::Pi => pi_filename_id(stem).is_some(),
        Harness::OpenCode => false,
    }
}

fn filename_id(harness: Harness, path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    match harness {
        Harness::Claude => Some(stem.to_owned()),
        Harness::Codex if stem.starts_with("rollout-") && stem.len() > 36 => {
            let id = stem.get(stem.len() - 36..)?;
            (!id.is_empty()).then(|| id.to_owned())
        }
        Harness::Pi => pi_filename_id(stem).map(str::to_owned),
        Harness::OpenCode | Harness::Codex => None,
    }
}

fn pi_filename_id(stem: &str) -> Option<&str> {
    let (timestamp, id) = stem.rsplit_once('_')?;
    let date = timestamp.get(..10)?;
    (date.as_bytes().get(4) == Some(&b'-')
        && date.as_bytes().get(7) == Some(&b'-')
        && timestamp.contains('T')
        && !id.is_empty())
    .then_some(id)
}

fn stem_matches(harness: Harness, path: &Path, id: &str) -> bool {
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    if harness == Harness::Claude {
        return stem == id;
    }
    stem.strip_suffix(id)
        .and_then(|prefix| prefix.chars().next_back())
        .is_some_and(|separator| !separator.is_alphanumeric())
}

fn probe(store: &NativeStore, path: &Path) -> Result<Option<NativeSession>, DiscoveryError> {
    probe_with_open(store, path, |path| File::open(path))
}

fn probe_with_open(
    store: &NativeStore,
    path: &Path,
    open: impl FnOnce(&Path) -> io::Result<File>,
) -> Result<Option<NativeSession>, DiscoveryError> {
    if !native_filename(store.harness(), path) || !is_jsonl(path) {
        return Ok(None);
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(DiscoveryError::io(
                path.display().to_string(),
                "inspect file",
                &error,
            ));
        }
    };
    if !metadata.is_file() {
        return Err(DiscoveryError::InvalidMetadata {
            coordinate: path.display().to_string(),
            reason: "native candidate is not a regular file",
        });
    }
    let mut file = match open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(DiscoveryError::io(
                path.display().to_string(),
                "open file",
                &error,
            ));
        }
    };
    let bytes = match opening(&mut file, metadata.len()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Err(DiscoveryError::io(
                path.display().to_string(),
                "read opening",
                &error,
            ));
        }
    };
    let header_id = native_identity(store.harness(), &bytes, path)?;
    let (id, basis) = match header_id {
        Some(id) => (id, IdentityBasis::Header),
        None => (
            filename_id(store.harness(), path).ok_or_else(|| DiscoveryError::InvalidMetadata {
                coordinate: path.display().to_string(),
                reason: "recognized filename has no canonical identifier",
            })?,
            IdentityBasis::Filename,
        ),
    };
    Ok(Some(NativeSession::file(
        id,
        store.harness(),
        store.coordinate(),
        path.to_path_buf(),
        basis,
    )))
}

fn opening(file: &mut File, size: u64) -> io::Result<Vec<u8>> {
    let first = size.min(INITIAL_OPENING_BYTES as u64) as usize;
    let mut bytes = vec![0; first];
    file.read_exact(&mut bytes)?;
    if size <= first as u64 || bytes.contains(&b'\n') {
        return Ok(bytes);
    }
    let target = size.min(MAX_OPENING_BYTES as u64) as usize;
    bytes.resize(target, 0);
    file.read_exact(&mut bytes[first..])?;
    Ok(bytes)
}

fn native_identity(
    harness: Harness,
    bytes: &[u8],
    path: &Path,
) -> Result<Option<String>, DiscoveryError> {
    let complete = if bytes.len() == MAX_OPENING_BYTES {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(&[][..], |index| &bytes[..=index])
    } else {
        bytes
    };
    let mut found: Option<String> = None;
    for line in complete.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let identity = match harness {
            Harness::Claude if value.get("sessionId").is_some() => Some(identity_string(
                value.get("sessionId"),
                "Claude sessionId",
                path,
            )?),
            Harness::Codex if value.get("type").and_then(Value::as_str) == Some("session_meta") => {
                Some(identity_string(
                    value.get("payload").and_then(|payload| payload.get("id")),
                    "Codex session_meta id",
                    path,
                )?)
            }
            Harness::Pi if value.get("type").and_then(Value::as_str) == Some("session") => {
                Some(identity_string(value.get("id"), "Pi session id", path)?)
            }
            _ => None,
        };
        if let Some(identity) = identity {
            if found.as_ref().is_some_and(|prior| prior != &identity) {
                return Err(DiscoveryError::InvalidMetadata {
                    coordinate: path.display().to_string(),
                    reason: "conflicting native identity fields",
                });
            }
            found = Some(identity);
        }
    }
    Ok(found)
}

fn identity_string(
    value: Option<&Value>,
    field: &'static str,
    path: &Path,
) -> Result<String, DiscoveryError> {
    match value.and_then(Value::as_str) {
        Some(value) if !value.is_empty() && !value.contains('\0') => Ok(value.to_owned()),
        _ => Err(DiscoveryError::InvalidMetadata {
            coordinate: path.display().to_string(),
            reason: field,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Discovery, ResolveError};
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("tapes-discovery-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(root: &Path, relative: &str, content: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn headers_win_over_native_filenames_and_report_their_basis() {
        let temp = Temp::new();
        let path = write(
            temp.path(),
            "claude/project/filename-id.jsonl",
            "{\"sessionId\":\"header-id\"}\n",
        );
        let session = NativeStore::claude(temp.path().join("claude"))
            .locate_exact("header-id")
            .unwrap()
            .unwrap();
        assert_eq!(session.id(), "header-id");
        assert_eq!(session.identity_basis(), Some(IdentityBasis::Header));
        assert_eq!(session.locator(), Some(path.as_path()));
    }

    #[test]
    fn filename_fallback_requires_a_native_name_and_header_absence() {
        let temp = Temp::new();
        let root = temp.path().join("codex");
        let path = write(
            &root,
            "2026/09/24/rollout-2026-09-24T12-00-00-000Z-00000000-0000-0000-0000-000000000001.jsonl",
            "{\"type\":\"event_msg\"}\n",
        );
        let id = filename_id(Harness::Codex, &path).unwrap();
        let session = NativeStore::codex(root).locate_exact(&id).unwrap().unwrap();
        assert_eq!(session.identity_basis(), Some(IdentityBasis::Filename));
        assert!(filename_id(Harness::Codex, Path::new("export.jsonl")).is_none());
    }

    #[test]
    fn malformed_and_conflicting_native_identity_is_a_failure() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        write(
            &root,
            "project/name.jsonl",
            "{\"sessionId\":null}\n{\"sessionId\":\"name\"}\n",
        );
        let err = NativeStore::claude(root).locate_exact("name").unwrap_err();
        assert!(matches!(err, DiscoveryError::InvalidMetadata { .. }));
    }

    #[test]
    fn conflicting_native_identity_is_not_replaced_with_a_filename_id() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        write(
            &root,
            "project/name.jsonl",
            "{\"sessionId\":\"header-one\"}\n{\"sessionId\":\"header-two\"}\n",
        );
        let error = NativeStore::claude(root).locate_exact("name").unwrap_err();
        assert!(matches!(
            error,
            DiscoveryError::InvalidMetadata {
                reason: "conflicting native identity fields",
                ..
            }
        ));
    }

    #[test]
    fn oversized_unterminated_opening_uses_the_native_filename_fallback() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        let path = write(
            &root,
            "project/filename-id.jsonl",
            &format!(
                "{{\"sessionId\":\"header-id\",\"body\":\"{}",
                "x".repeat(MAX_OPENING_BYTES)
            ),
        );
        let mut file = File::open(path).unwrap();
        let bytes = opening(&mut file, (MAX_OPENING_BYTES + 32) as u64).unwrap();
        assert_eq!(bytes.len(), MAX_OPENING_BYTES);
        assert_eq!(
            native_identity(Harness::Claude, &bytes, Path::new("filename-id.jsonl")).unwrap(),
            None
        );
    }

    #[test]
    fn exact_filename_permission_failure_remains_a_read_error() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        let path = write(&root, "project/id.jsonl", "{\"sessionId\":\"id\"}\n");
        let store = NativeStore::claude(root);
        let error = locate_exact_using(&store, "id", |store, path| {
            probe_with_open(store, path, |_| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            })
        })
        .unwrap_err();
        assert!(path.exists());
        assert!(matches!(
            error,
            DiscoveryError::Io {
                kind: io::ErrorKind::PermissionDenied,
                ..
            }
        ));
    }

    #[test]
    fn malformed_unrelated_lines_are_ignored() {
        let temp = Temp::new();
        let root = temp.path().join("pi");
        write(
            &root,
            "2026-09-24T12-00-00-000Z_session-id.jsonl",
            "not json\n{\"type\":\"session\",\"id\":\"session-id\"}\n",
        );
        assert!(NativeStore::pi(root)
            .locate_exact("session-id")
            .unwrap()
            .is_some());
    }

    #[test]
    fn claude_discovers_only_project_level_files() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        write(&root, "project/top.jsonl", "{\"sessionId\":\"top\"}\n");
        write(
            &root,
            "project/subagents/agent.jsonl",
            "{\"sessionId\":\"child\"}\n",
        );
        let page = NativeStore::claude(root).candidates(1_000);
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].id(), "top");
    }

    #[test]
    fn injected_directory_entry_limit_stops_the_walk_and_marks_coverage() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        for index in 0..20 {
            write(
                &root,
                &format!("project/session-{index:02}.jsonl"),
                &format!("{{\"sessionId\":\"session-{index:02}\"}}\n"),
            );
        }
        let walk = walk_with_entry_limit(&NativeStore::claude(&root), &root, 3);
        assert_eq!(walk.scanned, 3);
        assert!(!walk.complete);
        assert!(walk.failures.iter().any(|failure| matches!(
            failure,
            DiscoveryError::BoundExhausted {
                bound: "injected directory-entry limit",
                ..
            }
        )));
    }

    #[test]
    fn missing_root_is_absence_but_wrong_type_is_an_error() {
        let temp = Temp::new();
        let missing = NativeStore::pi(temp.path().join("missing"));
        assert!(missing.candidates(10).complete);
        let file = write(temp.path(), "root", "not a directory");
        let wrong = NativeStore::pi(file).candidates(10);
        assert!(!wrong.complete);
        assert!(matches!(
            wrong.failures.first(),
            Some(DiscoveryError::InvalidMetadata { .. })
        ));
    }

    #[test]
    fn one_prefix_hit_on_an_incomplete_page_is_not_unique() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        let first = write(
            &root,
            "project/a-needle-one.jsonl",
            "{\"sessionId\":\"needle-one\"}\n",
        );
        File::options()
            .write(true)
            .open(first)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(86400)),
            )
            .unwrap();
        for index in 0..999 {
            write(
                &root,
                &format!("project/middle-{index:04}.jsonl"),
                &format!("{{\"sessionId\":\"middle-{index:04}\"}}\n"),
            );
        }
        let last = write(
            &root,
            "project/z-needle-two.jsonl",
            "{\"sessionId\":\"needle-two\"}\n",
        );
        File::options()
            .write(true)
            .open(last)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        let store = NativeStore::claude(root);
        let resolution = Discovery::new([store]).resolve("needle");
        assert!(
            matches!(&resolution, Err(ResolveError::Incomplete { .. })),
            "{resolution:?}"
        );
    }

    #[test]
    fn exact_identity_is_not_limited_by_prefix_page_size() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        for index in 0..1_005 {
            write(
                &root,
                &format!("project/{index:04}-session.jsonl"),
                &format!("{{\"sessionId\":\"id-{index:04}\"}}\n"),
            );
        }
        let store = NativeStore::claude(root);
        assert!(store.locate_exact("id-1004").unwrap().is_some());
        assert!(Discovery::new([store]).resolve("id-1004").is_ok());
    }

    #[test]
    fn opening_probe_stops_before_a_large_tail() {
        let temp = Temp::new();
        let path = write(
            temp.path(),
            "claude/project/id.jsonl",
            &format!("{{\"sessionId\":\"id\"}}\n{}", "x".repeat(4 * 1024 * 1024)),
        );
        let mut file = File::open(path).unwrap();
        let bytes = opening(&mut file, 4 * 1024 * 1024).unwrap();
        assert_eq!(bytes.len(), INITIAL_OPENING_BYTES);
        assert!(bytes.len() < 4 * 1024 * 1024);
    }

    #[test]
    fn permission_failure_is_preserved_as_io_failure() {
        let temp = Temp::new();
        let root = temp.path().join("claude");
        let path = write(&root, "project/id.jsonl", "{\"sessionId\":\"id\"}\n");
        let error = probe_with_open(&NativeStore::claude(root), &path, |_| {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(matches!(
            error,
            DiscoveryError::Io {
                kind: io::ErrorKind::PermissionDenied,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycle_does_not_loop() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        let root = temp.path().join("pi");
        fs::create_dir_all(root.join("child")).unwrap();
        write(
            &root,
            "child/2026-09-24T12-00-00-000Z_session-id.jsonl",
            "{\"type\":\"session\",\"id\":\"id\"}\n",
        );
        symlink(&root, root.join("child/back")).unwrap();
        let page = NativeStore::pi(root).candidates(10);
        assert_eq!(page.records.len(), 1);
        assert!(page.complete);
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_native_names_are_ignored_without_opening_them() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp = Temp::new();
        let root = temp.path().join("claude");
        let fifo = root.join("project/fifo.jsonl");
        fs::create_dir_all(fifo.parent().unwrap()).unwrap();
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        let result = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        assert_eq!(result, 0);
        let page = NativeStore::claude(root).candidates(10);
        assert!(page.complete);
        assert!(page.records.is_empty());
    }
}
