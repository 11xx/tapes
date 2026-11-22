use crate::{CandidatePage, DiscoveryError, Harness, IdentityBasis, NativeSession};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, ReadDir};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_ENTRIES: usize = 10_000;
const MAX_CANDIDATES: usize = 1_000;
const DEADLINE: Duration = Duration::from_secs(2);

fn profiles(home: &Path) -> PathBuf {
    home.join("userdata/providers/antigravity")
}

pub(crate) fn available(home: &Path) -> bool {
    !matches!(fs::symlink_metadata(profiles(home)), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

fn session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes[14] == b'4'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn directory(path: &Path) -> Result<Option<ReadDir>, DiscoveryError> {
    let coordinate = || path.display().to_string();
    match fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => fs::read_dir(path)
            .map(Some)
            .map_err(|error| DiscoveryError::io(coordinate(), "enumerate profiles", &error)),
        Ok(_) => Err(DiscoveryError::InvalidMetadata {
            coordinate: coordinate(),
            reason: "expected a native directory",
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DiscoveryError::io(
            coordinate(),
            "inspect directory",
            &error,
        )),
    }
}

fn each_entry(
    directory_path: &Path,
    entries: ReadDir,
    visited: &mut usize,
    mut visit: impl FnMut(&Path, &mut usize) -> Result<(), DiscoveryError>,
) -> Result<(), DiscoveryError> {
    for entry in entries {
        if *visited == MAX_ENTRIES {
            return Err(DiscoveryError::BoundExhausted {
                coordinate: directory_path.display().to_string(),
                bound: "10000 Antigravity profile and conversation directory entries",
            });
        }
        *visited += 1;
        let entry = entry.map_err(|error| {
            DiscoveryError::io(
                directory_path.display().to_string(),
                "read directory entry",
                &error,
            )
        })?;
        visit(&entry.path(), visited)?;
    }
    Ok(())
}

fn conversation_dirs(
    home: &Path,
    visited: &mut usize,
    mut visit: impl FnMut(&Path, ReadDir, &mut usize) -> Result<(), DiscoveryError>,
) -> Result<(), DiscoveryError> {
    let profiles = profiles(home);
    let Some(entries) = directory(&profiles)? else {
        return Ok(());
    };
    each_entry(&profiles, entries, visited, |path, visited| {
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            return Ok(());
        };
        if name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            let acp = path.join("antigravity-acp");
            if directory(path)?.is_some() && directory(&acp)?.is_some() {
                let conversations = acp.join("conversations");
                if let Some(entries) = directory(&conversations)? {
                    visit(&conversations, entries, visited)?;
                }
            }
        }
        Ok(())
    })
}

fn inspect(home: &Path, database: &Path, id: &str) -> Result<NativeSession, DiscoveryError> {
    let coordinate = database.display().to_string();
    let invalid = |reason| DiscoveryError::InvalidMetadata {
        coordinate: coordinate.clone(),
        reason,
    };
    // Only the native identity column participates in discovery.
    if !fs::symlink_metadata(database)
        .map_err(|error| DiscoveryError::io(&coordinate, "inspect database", &error))?
        .is_file()
    {
        return Err(invalid(
            "expected a regular Antigravity conversation database",
        ));
    }
    let command_path = std::path::absolute(database)
        .map_err(|error| DiscoveryError::io(&coordinate, "resolve database location", &error))?;
    let bytes = crate::opencode::run_command_with_xdg(
        OsStr::new("sqlite3"),
        &[
            "-readonly".into(),
            "-batch".into(),
            "-init".into(),
            "/dev/null".into(),
            "-json".into(),
            command_path.into_os_string(),
            "SELECT DISTINCT cascade_id FROM trajectory_meta LIMIT 2".into(),
        ],
        &coordinate,
        None,
        DEADLINE,
    )?;
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("missing or unreadable Antigravity cascade identity"))?;
    let [row] = rows.as_slice() else {
        return Err(invalid(
            "missing or conflicting Antigravity cascade identities",
        ));
    };
    let found = row["cascade_id"]
        .as_str()
        .filter(|found| session_id(found))
        .ok_or_else(|| invalid("malformed Antigravity cascade identity"))?;
    if found != id {
        return Err(DiscoveryError::IdentityMismatch {
            coordinate,
            path: database.to_owned(),
            queried: id.to_owned(),
            found: found.to_owned(),
        });
    }
    Ok(NativeSession::file(
        id.to_owned(),
        Harness::Antigravity,
        format!("antigravity:{}", home.display()),
        database.to_owned(),
        IdentityBasis::Database,
    ))
}

fn duplicate(home: &Path) -> DiscoveryError {
    DiscoveryError::InvalidMetadata {
        coordinate: profiles(home).display().to_string(),
        reason: "ambiguous Antigravity identity in multiple provider profiles",
    }
}

pub(crate) fn locate(home: &Path, id: &str) -> Result<Option<NativeSession>, DiscoveryError> {
    if !session_id(id) {
        return Ok(None);
    }
    let mut found = None;
    let filename = OsString::from(format!("{id}.db"));
    conversation_dirs(home, &mut 0, |dir, _, _| {
        let path = dir.join(&filename);
        match fs::symlink_metadata(&path) {
            Ok(_) if found.is_some() => Err(duplicate(home)),
            Ok(_) => {
                found = Some(path);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DiscoveryError::io(
                path.display().to_string(),
                "locate database",
                &error,
            )),
        }
    })?;
    found.map(|path| inspect(home, &path, id)).transpose()
}

pub(crate) fn candidates(home: &Path, limit: usize, prefix: Option<&str>) -> CandidatePage {
    let mut page = CandidatePage::empty();
    let mut matches = BTreeMap::<String, Option<PathBuf>>::new();
    let limit = limit.min(MAX_CANDIDATES);
    let mut capped = false;
    let walked = conversation_dirs(home, &mut page.visited_entries, |dir, entries, visited| {
        each_entry(dir, entries, visited, |path, _| {
            let Some(id) = path
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(|name| name.strip_suffix(".db"))
                .filter(|id| session_id(id) && prefix.is_none_or(|prefix| id.starts_with(prefix)))
            else {
                return Ok(());
            };
            if let Some(existing) = matches.get_mut(id) {
                *existing = None;
            } else if matches.len() == limit {
                capped = true;
            } else {
                matches.insert(id.to_owned(), Some(path.to_owned()));
            }
            Ok(())
        })
    });
    if let Err(error) = walked {
        page.complete = false;
        page.failures.push(error);
        // An unvisited profile can hold a second copy of any candidate.
        return page;
    }
    if capped {
        page.complete = false;
    }
    let started = Instant::now();
    for (id, path) in matches {
        if started.elapsed() >= Duration::from_secs(10) {
            page.complete = false;
            page.failures.push(DiscoveryError::BoundExhausted {
                coordinate: profiles(home).display().to_string(),
                bound: "Antigravity metadata page time",
            });
            break;
        }
        page.scanned += 1;
        match path.map_or_else(|| Err(duplicate(home)), |path| inspect(home, &path, &id)) {
            Ok(record) => page.records.push(record),
            Err(error) => {
                page.complete = false;
                page.unreadable_ids.push(id);
                page.failures.push(error);
            }
        }
    }
    page
}
