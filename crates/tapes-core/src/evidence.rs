//! Verbatim source evidence for a supplied conversation: the record's own
//! bytes and each associated report's whole member, copied out of the input
//! beside the bundle that projects them, with a manifest that says where
//! every byte came from.
//!
//! Bytes are copied, never re-serialized. The input is opened read-only. A
//! member that fails decompression or its checksum contributes no file, and
//! anything that could not be copied is a manifest gap rather than a partial
//! file.

use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::bundle::BundleFile;
use crate::content::ArtifactReference;
use crate::model::{ByteSpan, Transcript};
use crate::reader::ReaderIdentity;

pub const EVIDENCE_SCHEMA: &str = "tapes-evidence/1";

/// The most bytes one copied member may decompress to. A member larger than
/// this is a gap, so an evidence set stays bounded like every other read.
const MAX_MEMBER_BYTES: u64 = 512 * 1024 * 1024;

/// The manifest an evidence set is described by.
#[derive(Debug, Serialize)]
pub struct EvidenceManifest {
    pub schema: &'static str,
    pub reader: ReaderIdentity,
    /// The supplied file the evidence was copied from, observed whole.
    pub input: InputObservation,
    /// The projection written beside the evidence, which cites it.
    pub projection: ProjectionCoordinates,
    pub files: Vec<EvidenceFile>,
    /// Reports the projection associates with the conversation, and which of
    /// its fields named the conversation.
    pub associations: Vec<Association>,
    /// What could not be copied, and why. A gap never leaves a file behind.
    pub gaps: Vec<EvidenceGap>,
}

#[derive(Debug, Serialize)]
pub struct InputObservation {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct ProjectionCoordinates {
    pub schema: String,
    pub projection_options: Vec<String>,
}

/// One copied file. `span` is present for a record copied out of a larger
/// source, `member` when that source is a ZIP member.
#[derive(Debug, Serialize)]
pub struct EvidenceFile {
    pub role: EvidenceRole,
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<MemberFacts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<ByteSpan>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceRole {
    /// The conversation record the projection's turns point into.
    Record,
    /// A report member associated with the conversation, copied whole.
    Report,
}

#[derive(Debug, Serialize)]
pub struct MemberFacts {
    pub name: String,
    pub size: u64,
    pub crc32: u32,
}

#[derive(Debug, Serialize)]
pub struct Association {
    pub member: String,
    /// The reference fields that name this conversation: `backing`,
    /// `originating_conversation`, or neither when only a message matched.
    pub named_by: Vec<&'static str>,
    pub copied: bool,
}

#[derive(Debug, Serialize)]
pub struct EvidenceGap {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    pub reason: String,
}

/// Where a supplied record's bytes live: a file, or a member of a ZIP.
enum Source {
    File(PathBuf),
    Zip { archive: PathBuf, member: String },
}

/// Copy the evidence behind `transcript`, a projection of a supplied input,
/// into `<json stem>.evidence/` beside the bundle's `.json`, and return the
/// manifest file. The directory is written under a `.partial` name and
/// renamed into place once complete.
pub fn write(transcript: &Transcript, json: &Path) -> Result<BundleFile> {
    let read = transcript
        .read
        .as_ref()
        .ok_or_else(|| anyhow!("the projection carries no read evidence to copy from"))?;
    let (Some(span), Some(digest)) = (read.records.first(), read.record_sha256.first()) else {
        return Err(anyhow!(
            "evidence export copies supplied-input records; this read names no digested record"
        ));
    };
    let location = transcript
        .session
        .source
        .location
        .as_ref()
        .ok_or_else(|| anyhow!("the session names no source to copy from"))?;
    let source = match &location.member {
        Some(member) => Source::Zip {
            archive: PathBuf::from(&location.locator),
            member: member.clone(),
        },
        None => Source::File(PathBuf::from(&location.locator)),
    };

    let stem = json
        .file_stem()
        .ok_or_else(|| anyhow!("{} has no file name", json.display()))?;
    let parent = json.parent().unwrap_or_else(|| Path::new("."));
    let mut name = stem.to_owned();
    name.push(".evidence");
    let directory = parent.join(&name);
    name.push(".partial");
    let partial = PartialDirectory::create(parent.join(name))?;

    let mut manifest = EvidenceManifest {
        schema: EVIDENCE_SCHEMA,
        reader: crate::reader::identity(),
        input: observe(source.container())?,
        projection: ProjectionCoordinates {
            schema: read.projection.clone(),
            projection_options: read.projection_options.clone(),
        },
        files: Vec::new(),
        associations: Vec::new(),
        gaps: Vec::new(),
    };

    match copy_record(&source, *span, digest, &partial.path) {
        Ok(file) => manifest.files.push(file),
        Err(reason) => manifest.gaps.push(EvidenceGap {
            member: source.member().map(str::to_owned),
            reason,
        }),
    }
    for report in associated_reports(transcript) {
        let Some(member) = report.path.clone() else {
            continue;
        };
        let named_by = [
            ("backing", report.backing.as_deref()),
            (
                "originating_conversation",
                report.originating_conversation.as_deref(),
            ),
        ]
        .into_iter()
        .filter(|(_, conversation)| *conversation == Some(transcript.session.id.as_str()))
        .map(|(field, _)| field)
        .collect();
        let copied = match &source {
            Source::Zip { archive, .. } => match copy_member(archive, &member, &partial.path) {
                Ok(file) => {
                    manifest.files.push(file);
                    true
                }
                Err(reason) => {
                    manifest.gaps.push(EvidenceGap {
                        member: Some(member.clone()),
                        reason,
                    });
                    false
                }
            },
            Source::File(_) => {
                manifest.gaps.push(EvidenceGap {
                    member: Some(member.clone()),
                    reason: "a report is copied only out of the ZIP that holds its conversation"
                        .to_owned(),
                });
                false
            }
        };
        manifest.associations.push(Association {
            member,
            named_by,
            copied,
        });
    }
    for gap in &read.gaps {
        manifest.gaps.push(EvidenceGap {
            member: None,
            reason: format!(
                "the read itself did not reach {}..{}: {}",
                gap.span.start, gap.span.end, gap.reason
            ),
        });
    }

    let body = serde_json::to_vec_pretty(&manifest).context("serialize the evidence manifest")?;
    fs::write(partial.path.join("manifest.json"), &body)
        .with_context(|| format!("failed to write {}", partial.path.display()))?;
    partial.place(&directory)?;
    Ok(BundleFile {
        path: directory.join("manifest.json"),
        bytes: body.len() as u64,
    })
}

impl Source {
    fn container(&self) -> &Path {
        match self {
            Source::File(path) => path,
            Source::Zip { archive, .. } => archive,
        }
    }

    fn member(&self) -> Option<&str> {
        match self {
            Source::File(_) => None,
            Source::Zip { member, .. } => Some(member),
        }
    }
}

/// The reports the projection associates with its conversation: artifacts
/// read from a member of the input beside the conversation's own.
fn associated_reports(transcript: &Transcript) -> Vec<&ArtifactReference> {
    transcript
        .artifacts
        .iter()
        .filter(|artifact| {
            artifact.source.as_ref().is_some_and(|source| {
                source
                    .pointer
                    .as_deref()
                    .is_some_and(|pointer| pointer.starts_with("/associated/"))
            })
        })
        .collect()
}

/// The container's length and SHA-256, read once from start to end.
fn observe(path: &Path) -> Result<InputObservation> {
    let mut file = BufReader::with_capacity(
        256 * 1024,
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut bytes = 0;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok(InputObservation {
        path: path.to_path_buf(),
        bytes,
        sha256: hex(&hasher.finalize()),
    })
}

/// Copy the record's span, refusing bytes whose digest is not the one the
/// read recorded: the input changed since the projection was taken.
fn copy_record(
    source: &Source,
    span: ByteSpan,
    digest: &str,
    directory: &Path,
) -> std::result::Result<EvidenceFile, String> {
    let (bytes, member) = match source {
        Source::File(path) => (read_span(path, span)?, None),
        Source::Zip { archive, member } => {
            let (whole, facts) = read_member(archive, member)?;
            let slice = usize::try_from(span.start)
                .ok()
                .zip(usize::try_from(span.end).ok())
                .and_then(|(start, end)| whole.get(start..end))
                .ok_or_else(|| {
                    format!("span {}..{} is outside the member", span.start, span.end)
                })?;
            (slice.to_vec(), Some(facts))
        }
    };
    let sha256 = hex(&Sha256::digest(&bytes));
    if sha256 != digest {
        return Err(format!(
            "the record's bytes no longer match the digest the read recorded ({digest}); the input changed"
        ));
    }
    let file = format!("sha256-{sha256}.json");
    fs::write(directory.join(&file), &bytes).map_err(|error| error.to_string())?;
    Ok(EvidenceFile {
        role: EvidenceRole::Record,
        file,
        sha256,
        bytes: bytes.len() as u64,
        member,
        span: Some(span),
    })
}

/// Copy a whole report member, named by its digest and keeping its extension.
fn copy_member(
    archive: &Path,
    member: &str,
    directory: &Path,
) -> std::result::Result<EvidenceFile, String> {
    let (bytes, facts) = read_member(archive, member)?;
    let sha256 = hex(&Sha256::digest(&bytes));
    let extension = Path::new(member)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let file = format!("sha256-{sha256}{extension}");
    fs::write(directory.join(&file), &bytes).map_err(|error| error.to_string())?;
    Ok(EvidenceFile {
        role: EvidenceRole::Report,
        file,
        sha256,
        bytes: bytes.len() as u64,
        member: Some(facts),
        span: None,
    })
}

fn read_span(path: &Path, span: ByteSpan) -> std::result::Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(span.start))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(span.end.saturating_sub(span.start))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if (bytes.len() as u64) < span.end - span.start {
        return Err(format!(
            "{} ends before the record's span {}..{}; the input changed",
            path.display(),
            span.start,
            span.end
        ));
    }
    Ok(bytes)
}

/// A whole member, read to its end so the archive's checksum is verified.
fn read_member(
    archive: &Path,
    member: &str,
) -> std::result::Result<(Vec<u8>, MemberFacts), String> {
    let file = File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| error.to_string())?;
    let mut entry = archive
        .by_name(member)
        .map_err(|error| format!("member {member}: {error}"))?;
    let facts = MemberFacts {
        name: member.to_owned(),
        size: entry.size(),
        crc32: entry.crc32(),
    };
    if facts.size > MAX_MEMBER_BYTES {
        return Err(format!(
            "member {member} decompresses to {} bytes, over the {} bound",
            facts.size,
            crate::byte_size::ByteSize::new(MAX_MEMBER_BYTES)
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(facts.size).unwrap_or(0));
    (&mut entry)
        .take(MAX_MEMBER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("member {member} failed verification: {error}"))?;
    Ok((bytes, facts))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A directory written under a temporary name and renamed into place once
/// complete; dropped unplaced, it is removed with everything in it.
struct PartialDirectory {
    path: PathBuf,
    placed: bool,
}

impl PartialDirectory {
    fn create(path: PathBuf) -> Result<Self> {
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self {
            path,
            placed: false,
        })
    }

    fn place(mut self, target: &Path) -> Result<()> {
        fs::rename(&self.path, target)
            .with_context(|| format!("failed to place {}", target.display()))?;
        self.placed = true;
        Ok(())
    }
}

impl Drop for PartialDirectory {
    fn drop(&mut self) {
        if !self.placed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn temporary(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("tapes-evidence-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_member_that_fails_its_checksum_is_not_copied() {
        let root = temporary("checksum");
        let archive = root.join("export.zip");
        let body = b"report body that will be altered in the archive";
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .start_file(
                "file_00aa.dat",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(body).unwrap();
        writer.finish().unwrap();
        let mut bytes = fs::read(&archive).unwrap();
        let at = bytes
            .windows(body.len())
            .position(|window| window == body)
            .unwrap();
        bytes[at] ^= 0xff;
        fs::write(&archive, bytes).unwrap();

        let error = copy_member(&archive, "file_00aa.dat", &root).unwrap_err();
        assert!(error.contains("failed verification"), "{error}");
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            1,
            "only the archive remains"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_record_whose_bytes_changed_since_the_read_is_refused() {
        let root = temporary("changed");
        let source = root.join("export.json");
        fs::write(&source, b"[{\"id\":\"a\"}]").unwrap();
        let span = ByteSpan { start: 1, end: 11 };
        let recorded = hex(&Sha256::digest(b"{\"id\":\"a\"}"));
        let copied = copy_record(&Source::File(source.clone()), span, &recorded, &root).unwrap();
        assert_eq!(copied.sha256, recorded);

        fs::write(&source, b"[{\"id\":\"b\"}]").unwrap();
        let error = copy_record(&Source::File(source), span, &recorded, &root).unwrap_err();
        assert!(error.contains("the input changed"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }
}
