//! Verbatim source evidence for a supplied conversation: the record's own
//! bytes and each associated report's whole member, copied out of the input
//! beside the bundle that projects them, with a manifest that says where
//! every byte came from.
//!
//! Bytes are copied, never re-serialized. The input is opened read-only. A
//! member that fails decompression or its checksum contributes no file, and
//! anything that could not be copied is a manifest gap rather than a partial
//! file.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd},
    os::unix::ffi::OsStrExt,
};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::bundle::BundleFile;
use crate::content::ArtifactReference;
use crate::model::{ByteSpan, SourceLocation, Transcript};
use crate::reader::ReaderIdentity;

pub const EVIDENCE_SCHEMA: &str = "tapes-evidence/2";

/// The most bytes one copied member may hold. A member larger than this is
/// a gap, so an evidence set stays bounded like every other read.
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
    /// Whether the input this record was read from can hold associated
    /// reports at all. A whole file holds one record and no siblings, so an
    /// empty `associations` there states the shape of the input rather than
    /// the absence of reports for the conversation.
    pub associations_resolvable: bool,
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

/// Where a supplied record's bytes live: a whole file, a member of a
/// supplied directory, or a member of a ZIP. A directory member and a ZIP
/// member both sit beside the siblings a report is copied from; a whole file
/// has no siblings and so can hold no association.
enum Source {
    File(PathBuf),
    Directory {
        root: PathBuf,
        path: PathBuf,
    },
    #[cfg(feature = "zip")]
    Zip {
        archive: PathBuf,
        member: String,
    },
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
    let source = locate(location)?;

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
        associations_resolvable: source.holds_siblings(),
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
        let copied = match copy_report(&source, &member, &partial.path) {
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
    /// The file whose bytes the record was read out of, observed whole for
    /// the manifest. For a directory member that is the member's own file,
    /// not the directory, because a directory has no length or digest.
    fn container(&self) -> &Path {
        match self {
            Source::File(path) => path,
            Source::Directory { path, .. } => path,
            #[cfg(feature = "zip")]
            Source::Zip { archive, .. } => archive,
        }
    }

    fn member(&self) -> Option<&str> {
        match self {
            Source::File(_) => None,
            Source::Directory { path, .. } => path.file_name().and_then(|name| name.to_str()),
            #[cfg(feature = "zip")]
            Source::Zip { member, .. } => Some(member),
        }
    }

    /// Whether the record sits beside other members a report could be copied
    /// from. A whole file does not, so its empty association list says
    /// nothing about the conversation.
    fn holds_siblings(&self) -> bool {
        match self {
            Source::File(_) => false,
            Source::Directory { .. } => true,
            #[cfg(feature = "zip")]
            Source::Zip { .. } => true,
        }
    }
}

/// Resolve where a projection's record bytes are to be read from. The read
/// records the container the record was one member of, so a supplied
/// directory and an archive are told apart by what the input was rather
/// than by what its paths look like.
fn locate(location: &SourceLocation) -> Result<Source> {
    let locator = PathBuf::from(&location.locator);
    let Some(container) = location.container.as_ref().map(PathBuf::from) else {
        return Ok(Source::File(locator));
    };
    if container.is_dir() {
        return Ok(Source::Directory {
            root: container,
            path: locator,
        });
    }
    zip_source(container, location.member.as_deref())
}

#[cfg(feature = "zip")]
fn zip_source(archive: PathBuf, member: Option<&str>) -> Result<Source> {
    let member = member
        .ok_or_else(|| anyhow!("the read names an archive container but no member within it"))?;
    Ok(Source::Zip {
        archive,
        member: member.to_owned(),
    })
}

#[cfg(not(feature = "zip"))]
fn zip_source(_archive: PathBuf, _member: Option<&str>) -> Result<Source> {
    Err(anyhow!(
        "ZIP evidence requires the `zip` feature; rebuild with `--features zip`"
    ))
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
        Source::Directory { path, .. } => (read_span(path, span)?, None),
        #[cfg(feature = "zip")]
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
    write_atomically(directory, &file, &bytes)?;
    Ok(EvidenceFile {
        role: EvidenceRole::Record,
        file,
        sha256,
        bytes: bytes.len() as u64,
        member,
        span: Some(span),
    })
}

/// Copy a whole report member out of whichever container holds it. A record
/// read from a file on its own has no container to look in.
fn copy_report(
    source: &Source,
    member: &str,
    directory: &Path,
) -> std::result::Result<EvidenceFile, String> {
    match source {
        Source::File(_) => Err(
            "a report is copied out of the directory or archive that holds its conversation; \
             this record was read from a file on its own"
                .to_owned(),
        ),
        Source::Directory { root, .. } => copy_sibling(root, member, directory),
        #[cfg(feature = "zip")]
        Source::Zip { archive, .. } => copy_member(archive, member, directory),
    }
}

/// Copy a whole report member sitting beside its conversation in a supplied
/// directory. The member name is the one the read normalized. On Unix the
/// root and every member directory are held by descriptor while the leaf is
/// opened without following symlinks, so a replacement cannot redirect the
/// read outside the supplied directory.
fn copy_sibling(
    root: &Path,
    member: &str,
    directory: &Path,
) -> std::result::Result<EvidenceFile, String> {
    let bytes = read_directory_member(root, member)?;
    write_report(member, bytes, None, directory)
}

/// Split a normalized member name into ordinary components, refusing any name
/// that could escape the supplied directory or designate the directory itself.
fn validate_member(member: &str) -> std::result::Result<Vec<&std::ffi::OsStr>, String> {
    let relative = Path::new(member);
    let components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(name) => Ok(name),
            _ => Err(()),
        })
        .collect::<std::result::Result<Vec<_>, _>>();
    let Ok(components) = components else {
        return Err(format!(
            "member {member} is not a name relative to the supplied directory"
        ));
    };
    if components.is_empty() {
        return Err(format!(
            "member {member} is not a name relative to the supplied directory"
        ));
    }
    Ok(components)
}

/// Read one directory member through stable descriptors. The size check before
/// reading avoids a known oversize file, while the extra byte in the reader
/// catches a file that grows after that observation without allocating or
/// copying an unbounded report.
fn read_directory_member(root: &Path, member: &str) -> std::result::Result<Vec<u8>, String> {
    let components = validate_member(member)?;
    #[cfg(unix)]
    {
        let mut directory = open_directory(root, member)?;
        for component in &components[..components.len() - 1] {
            directory = open_directory_at(&directory, component, member)?;
        }
        let leaf = components.last().expect("validated member has a leaf");
        let file = open_file_at(&directory, leaf, member)?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("member {member}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!(
                "member {member} is not a regular file; refusing to read it"
            ));
        }
        if metadata.len() > MAX_MEMBER_BYTES {
            return Err(format!(
                "member {member} holds {} bytes, over the {} bound",
                metadata.len(),
                crate::byte_size::ByteSize::new(MAX_MEMBER_BYTES)
            ));
        }
        let capacity = usize::try_from(metadata.len()).unwrap_or(0);
        let mut bytes = Vec::with_capacity(capacity);
        file.take(MAX_MEMBER_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("member {member}: {error}"))?;
        if bytes.len() as u64 > MAX_MEMBER_BYTES {
            return Err(format!(
                "member {member} grew beyond the {} bound while it was read",
                crate::byte_size::ByteSize::new(MAX_MEMBER_BYTES)
            ));
        }
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, components);
        Err(format!(
            "member {member}: directory evidence requires descriptor-relative no-follow file access"
        ))
    }
}

#[cfg(unix)]
fn open_directory(root: &Path, member: &str) -> std::result::Result<File, String> {
    let path = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| format!("member {member}: supplied directory path contains NUL"))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let descriptor = unsafe { libc::open(path.as_ptr(), flags, 0) };
    if descriptor < 0 {
        return Err(format!(
            "member {member}: failed to open supplied directory: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_directory_at(
    directory: &File,
    component: &std::ffi::OsStr,
    member: &str,
) -> std::result::Result<File, String> {
    let name = CString::new(component.as_bytes())
        .map_err(|_| format!("member {member}: path component contains NUL"))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };
    if descriptor < 0 {
        return Err(format!(
            "member {member}: failed to open directory component: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_file_at(
    directory: &File,
    component: &std::ffi::OsStr,
    member: &str,
) -> std::result::Result<File, String> {
    let name = CString::new(component.as_bytes())
        .map_err(|_| format!("member {member}: path component contains NUL"))?;
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };
    if descriptor < 0 {
        return Err(format!(
            "member {member}: failed to open report: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

/// Name a copied report by its digest, keeping its extension, and write it.
fn write_report(
    member: &str,
    bytes: Vec<u8>,
    facts: Option<MemberFacts>,
    directory: &Path,
) -> std::result::Result<EvidenceFile, String> {
    let sha256 = hex(&Sha256::digest(&bytes));
    let extension = Path::new(member)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let file = format!("sha256-{sha256}{extension}");
    write_atomically(directory, &file, &bytes)?;
    Ok(EvidenceFile {
        role: EvidenceRole::Report,
        file,
        sha256,
        bytes: bytes.len() as u64,
        member: facts,
        span: None,
    })
}

/// Copy a whole report member, named by its digest and keeping its extension.
#[cfg(feature = "zip")]
fn copy_member(
    archive: &Path,
    member: &str,
    directory: &Path,
) -> std::result::Result<EvidenceFile, String> {
    let (bytes, facts) = read_member(archive, member)?;
    write_report(member, bytes, Some(facts), directory)
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
#[cfg(feature = "zip")]
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
        fs::create_dir(&path).with_context(|| format!("failed to create {}", path.display()))?;
        Ok(Self {
            path,
            placed: false,
        })
    }

    fn place(mut self, target: &Path) -> Result<()> {
        if target.exists() {
            anyhow::bail!(
                "refusing to replace existing evidence directory {}",
                target.display()
            );
        }
        fs::rename(&self.path, target)
            .with_context(|| format!("failed to place {}", target.display()))?;
        self.placed = true;
        Ok(())
    }
}

/// Write one evidence member through a create-new temporary file, so a copy
/// that later becomes a manifest gap never leaves a readable partial member.
fn write_atomically(directory: &Path, name: &str, bytes: &[u8]) -> std::result::Result<(), String> {
    let target = directory.join(name);
    if let Some(reused) = existing_evidence_file(&target, bytes)? {
        if reused {
            return Ok(());
        }
    }
    let temporary = PathBuf::from(format!("{}.partial", target.display()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.flush()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    drop(file);
    if let Err(error) = fs::hard_link(&temporary, &target) {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            let reused = match existing_evidence_file(&target, bytes) {
                Ok(reused) => reused,
                Err(reason) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(reason);
                }
            };
            let _ = fs::remove_file(&temporary);
            if reused == Some(true) {
                return Ok(());
            }
        } else {
            let _ = fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        return Err(error.to_string());
    }
    if let Err(error) = fs::remove_file(&temporary) {
        let _ = fs::remove_file(&target);
        return Err(error.to_string());
    }
    Ok(())
}

/// Return `Some(true)` only for a regular file whose bytes are exactly the
/// requested content. Other file types and conflicting regular files stay
/// refusal paths, so an export never adopts an unrelated path.
fn existing_evidence_file(
    target: &Path,
    expected: &[u8],
) -> std::result::Result<Option<bool>, String> {
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{} is not a regular evidence file; refusing to replace it",
            target.display()
        ));
    }
    let actual = fs::read(target).map_err(|error| error.to_string())?;
    if actual == expected {
        Ok(Some(true))
    } else {
        Err(format!(
            "{} already contains different evidence bytes; refusing to replace it",
            target.display()
        ))
    }
}

impl Drop for PartialDirectory {
    fn drop(&mut self) {
        if !self.placed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(all(test, feature = "zip"))]
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
    fn a_report_write_failure_leaves_no_partial_member() {
        let root = temporary("report-write-failure");
        let archive = root.join("export.zip");
        let body = b"report body";
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .start_file(
                "report.dat",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(body).unwrap();
        writer.finish().unwrap();

        let name = format!("sha256-{}.dat", hex(&Sha256::digest(body)));
        let occupied = root.join(&name);
        fs::create_dir(&occupied).unwrap();
        fs::write(occupied.join("keep"), b"another export").unwrap();

        let error = copy_member(&archive, "report.dat", &root).unwrap_err();
        assert!(!error.is_empty());
        assert!(occupied.join("keep").is_file());
        assert!(!root.join(format!("{name}.partial")).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn conflicting_or_symlinked_evidence_is_never_replaced() {
        let root = temporary("report-conflict");
        let archive = root.join("export.zip");
        let body = b"report body";
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .start_file(
                "report.dat",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(body).unwrap();
        writer.finish().unwrap();
        let name = format!("sha256-{}.dat", hex(&Sha256::digest(body)));
        let target = root.join(&name);

        fs::write(&target, b"conflicting bytes").unwrap();
        assert!(copy_member(&archive, "report.dat", &root).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"conflicting bytes");
        assert!(!root.join(format!("{name}.partial")).exists());
        fs::remove_file(&target).unwrap();

        let destination = root.join("elsewhere");
        fs::write(&destination, b"symlink target").unwrap();
        std::os::unix::fs::symlink(&destination, &target).unwrap();
        assert!(copy_member(&archive, "report.dat", &root).is_err());
        assert_eq!(fs::read_link(&target).unwrap(), destination);
        assert!(!root.join(format!("{name}.partial")).exists());
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
