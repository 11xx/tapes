//! Explicit bounded reads of recording-relative persisted tool output.
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::content::{ArtifactReference, ContentPart};
use crate::model::{RecordRef, Session, Turn};

pub const SCHEMA: &str = "tapes-artifact/1";
pub const DEFAULT_BYTES: usize = 64 * 1024;
pub const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Coordinate {
    session: String,
    source: String,
    record: RecordRef,
    path: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn token(
    artifact: &ArtifactReference,
    session: &str,
    source: &crate::model::SourceDescriptor,
) -> Option<String> {
    if artifact.kind != "claude-persisted-tool-output" || artifact.descriptor_truncated {
        return None;
    }
    let mut record = artifact.source.clone()?;
    record.content_part_index = None;
    let coordinate = Coordinate {
        session: session.to_owned(),
        source: hex(&Sha256::digest(serde_json::to_vec(source).ok()?)),
        record,
        path: artifact.path.clone()?,
    };
    Some(format!(
        "ta1:{}",
        hex(&serde_json::to_vec(&coordinate).ok()?)
    ))
}

pub(crate) fn attach_tokens(
    turn: &mut Turn,
    session: &str,
    source: &crate::model::SourceDescriptor,
) {
    for part in &mut turn.parts {
        if let ContentPart::FileReference { reference, .. } = part {
            reference.reference = token(reference, session, source);
        }
    }
    if let Some(tool) = &mut turn.tool {
        for reference in &mut tool.artifact_references {
            reference.reference = token(reference, session, source);
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Artifact {
    pub schema: &'static str,
    pub bytes: usize,
    pub truncated: bool,
    pub sha256: String,
    pub encoding: &'static str,
    pub body: String,
}

pub fn read(session: &Session, reference: &str, budget: usize) -> Result<Artifact> {
    if budget == 0 || budget > MAX_BYTES {
        bail!("artifact byte budget must be between 1 and 4 MiB");
    }
    let encoded = reference
        .strip_prefix("ta1:")
        .context("unsupported artifact reference version")?;
    if encoded.len() > 32768 || encoded.len() % 2 != 0 {
        bail!("invalid artifact reference");
    }
    let decoded = (0..encoded.len())
        .step_by(2)
        .map(|index| {
            encoded
                .get(index..index + 2)
                .context("invalid artifact reference")
                .and_then(|pair| Ok(u8::from_str_radix(pair, 16)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let coordinate: Coordinate =
        serde_json::from_slice(&decoded).context("invalid artifact reference")?;
    let recording =
        crate::backend::session_file_path(session).context("session has no native recording")?;
    if coordinate.session != session.id
        || coordinate.source != hex(&Sha256::digest(serde_json::to_vec(&session.source)?))
        || session.source.origin != "claude"
        || coordinate.record.domain != format!("file:{}", recording.display())
    {
        bail!("artifact reference belongs to another session or source");
    }
    if recording.file_stem().and_then(|name| name.to_str()) != Some(session.id.as_str()) {
        bail!("recording has no confined session directory mapping");
    }
    let name = coordinate
        .path
        .strip_prefix("tool-results/")
        .context("artifact name is outside tool-results")?;
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        bail!("artifact name is outside tool-results");
    }
    let span = coordinate
        .record
        .span
        .context("artifact reference has no record span")?;
    if span.is_empty() || span.len() > 16 * 1024 * 1024 {
        bail!("artifact record exceeds the bounded verification read");
    }
    let mut file = crate::backend::open_regular_file(recording)?;
    file.seek(SeekFrom::Start(span.start))?;
    let mut record = Vec::new();
    file.take(span.len()).read_to_end(&mut record)?;
    let value: serde_json::Value =
        serde_json::from_slice(&record).context("artifact originating record is stale")?;
    if value["sessionId"].as_str() != Some(session.id.as_str()) {
        bail!("artifact reference belongs to another session");
    }
    if !crate::backend::claude::persisted_mapping_matches(&value, &session.id) {
        bail!("persisted envelope has no verified session-relative mapping");
    }
    let mut turns = crate::backend::claude::parse_turns(&value);
    crate::backend::attach_record_refs(
        &mut turns,
        &coordinate.record.domain,
        coordinate.record.revision.as_deref(),
        Some(span),
    );
    for turn in &mut turns {
        attach_tokens(turn, &session.id, &session.source);
    }
    if !turns
        .iter()
        .filter_map(|turn| turn.tool.as_ref())
        .flat_map(|tool| &tool.artifact_references)
        .any(|artifact| artifact.reference.as_deref() == Some(reference))
    {
        bail!("artifact originating result no longer names this file");
    }
    let root = recording
        .parent()
        .context("recording has no parent")?
        .join(&session.id);
    let file = confined_open(&root, name)?;
    let mut bytes = Vec::with_capacity(budget + 1);
    file.take((budget + 1) as u64).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > budget;
    bytes.truncate(budget);
    let sha256 = hex(&Sha256::digest(&bytes));
    let (encoding, body) = match String::from_utf8(bytes.clone()) {
        Ok(text) => ("utf-8", text),
        Err(_) => ("hex", hex(&bytes)),
    };
    Ok(Artifact {
        schema: SCHEMA,
        bytes: bytes.len(),
        truncated,
        sha256,
        encoding,
        body,
    })
}

#[cfg(unix)]
fn confined_open(root: &Path, name: &str) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(root)
        .context("cannot open confined session directory without following symlinks")?;
    fn child(parent: &std::fs::File, name: &str, directory: bool) -> Result<std::fs::File> {
        let name = CString::new(name)?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // The parent descriptor pins the directory; ownership of the returned fd transfers to File.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("confined artifact open refused");
        }
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
    let results = child(&root, "tool-results", true)?;
    let file = child(&results, name, false)?;
    if !file.metadata()?.is_file() {
        bail!("artifact is not a regular file");
    }
    Ok(file)
}

#[cfg(not(unix))]
fn confined_open(_root: &Path, _name: &str) -> Result<std::fs::File> {
    bail!("confined no-follow artifact reads are unsupported on this platform")
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::backend::Backend;

    #[test]
    fn artifact_record_verification_refuses_fifos() {
        crate::test_support::with_fifo(
            "artifact::admission_tests::artifact_record_verification_refuses_fifos",
            |fifo| {
                let id = fifo.file_stem().unwrap().to_str().unwrap();
                let root = fifo.with_extension("store");
                let project = root.join("project");
                std::fs::create_dir_all(&project).unwrap();
                let recording = project.join(format!("{id}.jsonl"));
                std::fs::write(&recording, format!("{{\"type\":\"user\",\"sessionId\":\"{id}\",\"message\":{{\"role\":\"user\",\"content\":\"fixture\"}}}}\n")).unwrap();
                let backend = crate::backend::claude::ClaudeBackend::new(&root);
                let session = backend.locate(id).unwrap().unwrap();
                let coordinate = Coordinate {
                    session: id.to_owned(),
                    source: hex(&Sha256::digest(
                        serde_json::to_vec(&session.source).unwrap(),
                    )),
                    record: RecordRef {
                        domain: format!("file:{}", recording.display()),
                        revision: None,
                        span: Some(crate::model::ByteSpan { start: 0, end: 1 }),
                        native_id: None,
                        pointer: None,
                        part_index: 0,
                        content_part_index: None,
                    },
                    path: "tool-results/result.txt".to_owned(),
                };
                let reference = format!("ta1:{}", hex(&serde_json::to_vec(&coordinate).unwrap()));
                std::fs::remove_file(&recording).unwrap();
                std::os::unix::fs::symlink(fifo, &recording).unwrap();
                let Err(error) = read(&session, &reference, 1) else {
                    panic!("FIFO must be refused");
                };
                assert!(
                    error.to_string().contains("not a regular file"),
                    "{error:#}"
                );
                std::fs::remove_dir_all(root).unwrap();
            },
        );
    }
}
