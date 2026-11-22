//! Detached Antigravity children are named by a parent descriptor, a native
//! spawn step and the launch tool metadata in the child's own inbox.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use agent_tapes_discovery::NativeStore;
use anyhow::{ensure, Context, Result};
use serde_json::Value;

use crate::t3::query_sqlite;

const MAX_CHILDREN: usize = 64;
const MAX_INBOX_ENTRIES: usize = 256;
const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

pub(crate) struct DetachedChild {
    pub id: String,
    pub task: String,
    pub descriptor: PathBuf,
}

pub(crate) fn detached_children(store: &NativeStore, parent: &str) -> Result<Vec<DetachedChild>> {
    let identity = store
        .locate_exact(parent)?
        .context("missing Antigravity parent conversation database")?;
    let database = identity.locator().context("missing parent database path")?;
    let profile = database
        .parent()
        .and_then(Path::parent)
        .context("missing Antigravity profile")?;
    let brain = profile.join("brain");
    let system = brain.join(parent).join(".system_generated");
    let directory = system.join("subagents");
    for path in [&brain, &brain.join(parent), &system, &directory] {
        if !native_directory(path)? {
            return Ok(Vec::new());
        }
    }

    let mut children = Vec::new();
    let mut tasks = HashSet::new();
    let mut json_bytes_left = MAX_JSON_BYTES;
    for (index, entry) in fs::read_dir(&directory)?.enumerate() {
        ensure!(
            index < MAX_CHILDREN,
            "Antigravity detached child directory exceeds {MAX_CHILDREN} entries"
        );
        let descriptor = entry?.path();
        let Some(child) = json_stem(&descriptor) else {
            continue;
        };
        let record = read_json(&descriptor, &mut json_bytes_left)?;
        ensure!(
            record["conversationId"].as_str() == Some(child) && child != parent,
            "Antigravity child descriptor has conflicting conversation identity"
        );
        let step = record["spawnStepIndex"]
            .as_u64()
            .context("Antigravity child descriptor has no spawn step")?;
        let native = store
            .locate_exact(child)?
            .context("missing Antigravity child conversation database")?;
        ensure!(
            native.locator().and_then(Path::parent) == database.parent(),
            "Antigravity child conversation database is in a different profile"
        );
        let task = inbox_task(&brain, parent, child, &mut json_bytes_left)?;
        ensure!(
            spawn_matches(database, step, &task)?,
            "Antigravity child launch does not match the parent database spawn step"
        );
        ensure!(
            tasks.insert(task.clone()),
            "multiple Antigravity children claim the same launch tool call"
        );
        children.push(DetachedChild {
            id: child.to_owned(),
            task,
            descriptor,
        });
    }
    Ok(children)
}

fn native_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        _ => anyhow::bail!("unreadable Antigravity native directory {}", path.display()),
    }
}

fn json_stem(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()?.strip_suffix(".json")
}

fn read_json(path: &Path, bytes_left: &mut u64) -> Result<Value> {
    let bound = (*bytes_left).min(MAX_RECORD_BYTES);
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
        "Antigravity native record is not a regular file"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(bound + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= bound,
        "Antigravity native record exceeds byte bound"
    );
    *bytes_left -= bytes.len() as u64;
    Ok(serde_json::from_slice(&bytes)?)
}

fn inbox_task(brain: &Path, parent: &str, child: &str, bytes_left: &mut u64) -> Result<String> {
    let child_root = brain.join(child);
    let system = child_root.join(".system_generated");
    let inbox = system.join("messages");
    for path in [&child_root, &system, &inbox] {
        ensure!(
            native_directory(path)?,
            "missing Antigravity child launch inbox"
        );
    }
    let mut task = None;
    for (index, entry) in fs::read_dir(&inbox)?.enumerate() {
        ensure!(
            index < MAX_INBOX_ENTRIES,
            "Antigravity child inbox exceeds {MAX_INBOX_ENTRIES} entries"
        );
        let path = entry?.path();
        if matches!(json_stem(&path), None | Some("read")) {
            continue;
        }
        let message = read_json(&path, bytes_left)?;
        let tool = &message["sourceMetadata"]["tool"];
        let call = &tool["toolCall"];
        if message["sender"].as_str() != Some(parent)
            || call["name"].as_str() != Some("invoke_subagent")
        {
            continue;
        }
        ensure!(
            tool["conversationId"].as_str() == Some(parent),
            "Antigravity child launch has a conflicting parent conversation"
        );
        let id = call["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("Antigravity child launch has no tool call ID")?;
        ensure!(
            task.replace(id.to_owned()).is_none(),
            "Antigravity child has multiple launch messages"
        );
    }
    task.context("Antigravity child's inbox does not record the parent's launch tool call")
}

fn spawn_matches(database: &Path, step: u64, task: &str) -> Result<bool> {
    // Detached invoke step, using TrajectoryStep.metadata.tool_call.
    let bound = MAX_RECORD_BYTES * 2 + 256;
    let rows = query_sqlite(
        database,
        &format!(
            "SELECT hex(step_payload) AS payload FROM steps
         WHERE idx = {step} AND step_type = 132 AND step_format = 0
           AND typeof(step_payload) = 'blob' AND length(step_payload) <= {MAX_RECORD_BYTES}
         LIMIT 2"
        ),
        bound,
    )?;
    let [row] = rows.as_slice() else {
        return Ok(false);
    };
    let Some(hex) = row["payload"].as_str() else {
        return Ok(false);
    };
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<_>>>();
    let call = bytes
        .as_deref()
        .and_then(|step| length_field(step, 5))
        .and_then(|metadata| length_field(metadata, 4));
    Ok(call.is_some_and(|call| {
        length_field(call, 1) == Some(task.as_bytes())
            && length_field(call, 2) == Some(b"invoke_subagent")
    }))
}

/// Read one singular protobuf bytes field, rejecting malformed or repeated fields.
fn length_field(mut input: &[u8], wanted: u64) -> Option<&[u8]> {
    let mut result = None;
    while !input.is_empty() {
        let tag = varint(&mut input)?;
        let field = tag >> 3;
        if field == 0 {
            return None;
        }
        let length = match tag & 7 {
            0 if field != wanted => {
                varint(&mut input)?;
                continue;
            }
            1 if field != wanted => 8,
            2 => usize::try_from(varint(&mut input)?).ok()?,
            5 if field != wanted => 4,
            _ => return None,
        };
        let (value, rest) = input.split_at_checked(length)?;
        if field == wanted && result.replace(value).is_some() {
            return None;
        }
        input = rest;
    }
    result
}

fn varint(input: &mut &[u8]) -> Option<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (byte, rest) = input.split_first()?;
        *input = rest;
        if shift == 63 && *byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}
