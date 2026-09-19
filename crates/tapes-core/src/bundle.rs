use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use crate::event::{self, PairIndex, Pairing};
use crate::model::{
    human_bytes, human_speaker, human_timestamp, human_title, AccountingBasis, AccountingCoverage,
    Role, Session, SourceBound, StreamedSessionJson, Transcript, Turn,
};

/// One exported file: where it landed and how big it is.
pub struct BundleFile {
    pub path: PathBuf,
    pub bytes: u64,
}

/// The three files an export writes, in the order a rescuer should read them,
/// and the manifest of the evidence set a supplied export copies beside them.
pub struct Bundle {
    pub context: BundleFile,
    pub json: BundleFile,
    pub trace: BundleFile,
    pub evidence: Option<BundleFile>,
}

impl Bundle {
    pub fn files(&self) -> impl Iterator<Item = &BundleFile> {
        [&self.context, &self.json, &self.trace]
            .into_iter()
            .chain(self.evidence.as_ref())
    }

    /// Remove the files this invocation published after a later stage failed.
    /// The namespace was reserved before any of them were written, so this
    /// never targets another export's files.
    pub(crate) fn discard(self) -> Result<()> {
        let mut errors = Vec::new();
        for file in [&self.context, &self.json, &self.trace] {
            if let Err(error) = remove_owned_file(&file.path) {
                errors.push(error);
            }
        }
        if let Some(evidence) = &self.evidence {
            if let Err(error) = remove_owned_file(&evidence.path) {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "failed to remove this export's published files: {}",
                errors.join("; ")
            ))
        }
    }
}

/// The commit a session's working directory sat on, when that is knowable.
#[derive(Clone, Debug, Serialize)]
pub struct GitContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Export a transcript held in memory, through the same two passes a
/// streamed recording takes.
pub fn export(transcript: &Transcript, directory: &Path) -> Result<Bundle> {
    let mut first = JsonPass::open(directory, &transcript.session)?;
    for turn in &transcript.turns {
        first.turn(turn)?;
    }
    let mut second = first.close(transcript)?;
    for turn in &transcript.turns {
        second.turn(turn)?;
    }
    second.finish()
}

/// The first of a bundle's two passes over one sequence of turns: each turn
/// is written into the `.json` file, a `tapes-session` object, and its tool
/// events are observed for pairing. Nothing but the pairing index outlives a
/// turn, so a recording streamed through it is never held.
pub struct JsonPass {
    prefix: PathBuf,
    namespace: ExportNamespace,
    session: Session,
    json: PartialFile,
    writer: StreamedSessionJson,
    index: PairIndex,
}

impl JsonPass {
    /// Open the bundle's files under a new timestamped prefix in `directory`.
    /// Volatile `live` state is not part of a bundle.
    pub fn open(directory: &Path, session: &Session) -> Result<Self> {
        let namespace = ExportNamespace::reserve(directory, session)?;
        let prefix = namespace.prefix.clone();
        let mut session = session.clone();
        session.live = None;
        let mut json = PartialFile::create(&prefix, "json")?;
        let writer = StreamedSessionJson::open(&mut json, &session)?;
        Ok(Self {
            prefix,
            namespace,
            session,
            json,
            writer,
            index: PairIndex::default(),
        })
    }

    pub fn turn(&mut self, turn: &Turn) -> Result<()> {
        self.writer.turn(&mut self.json, turn)?;
        for record in event::turn_records(turn) {
            self.index.observe(&record);
        }
        Ok(())
    }

    /// Close the pass with `facts`, a transcript carrying every fact of the
    /// read (its turns are not consulted): the `.json` members that follow
    /// `turns` and the session directory's `git`, then the Markdown headers,
    /// which state those facts before the second pass writes any turn.
    pub fn close(mut self, facts: &Transcript) -> Result<EventPass> {
        self.writer.close_members(&mut self.json, facts)?;
        if let Some(git) = git_context(self.session.directory.as_deref()) {
            self.json.write_all(b",\"git\":")?;
            serde_json::to_writer(&mut self.json, &git)?;
        }
        let mut context = PartialFile::create(&self.prefix, "context.md")?;
        let mut header = String::new();
        write_header(&mut header, &self.session, facts, "context");
        context.write_all(header.as_bytes())?;
        let mut trace = PartialFile::create(&self.prefix, "trace.md")?;
        header.clear();
        write_header(&mut header, &self.session, facts, "trace");
        if facts.projection.is_some() {
            header.push_str(
                "Only the kept turn kinds are traced; export without --only or --omit to trace every kind.\n\n",
            );
        }
        trace.write_all(header.as_bytes())?;
        Ok(EventPass {
            json: self.json,
            context,
            trace,
            namespace: self.namespace,
            pairing: self
                .index
                .pairing(event::read_was_bounded(&facts.truncation.source)),
            events: 0,
            turn: String::new(),
        })
    }
}

/// The second pass over the same turns, in the same order: each turn's tool
/// events are paired and appended to the `.json` file's `events`, and the turn
/// is written into `.context.md` when it belongs to the exchange and into
/// `.trace.md` always. A sequence that differs from the first pass's refuses.
pub struct EventPass {
    json: PartialFile,
    context: PartialFile,
    trace: PartialFile,
    namespace: ExportNamespace,
    pairing: Pairing,
    events: usize,
    /// One turn's Markdown, reused so a long turn's buffer is allocated once.
    turn: String,
}

impl EventPass {
    pub fn turn(&mut self, turn: &Turn) -> Result<()> {
        for record in event::turn_records(turn) {
            let record = self.pairing.emit(record)?;
            let separator: &[u8] = if self.events == 0 {
                b",\"events\":["
            } else {
                b","
            };
            self.json.write_all(separator)?;
            serde_json::to_writer(&mut self.json, &record)?;
            self.events += 1;
        }
        if turn.kind.in_exchange() {
            self.turn.clear();
            write_context_turn(&mut self.turn, turn);
            self.context.write_all(self.turn.as_bytes())?;
        }
        self.turn.clear();
        write_trace_turn(&mut self.turn, turn);
        self.trace.write_all(self.turn.as_bytes())?;
        Ok(())
    }

    /// Close the `.json` object and place all three files. A second pass
    /// shorter than the first refuses here, and nothing is placed.
    pub fn finish(mut self) -> Result<Bundle> {
        self.pairing.finish()?;
        if self.events > 0 {
            self.json.write_all(b"]")?;
        }
        self.json.write_all(b"}")?;
        let [context, json, trace] = place_all([self.context, self.json, self.trace])?;
        let namespace = self.namespace;
        namespace.release();
        Ok(Bundle {
            context,
            json,
            trace,
            evidence: None,
        })
    }
}

/// The file a bulk export writes beside its bundles, naming the selection
/// that produced them.
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

/// Write an export manifest into the directory holding its bundles, under the
/// same atomic rename the bundle files use.
pub fn write_manifest(directory: &Path, body: &str) -> Result<BundleFile> {
    write_atomically(&directory.join("manifest"), "json", body)
}

/// A prefix that sorts by export time and still names its session.
fn bundle_stem(session: &Session) -> String {
    let id = session
        .id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!(
        "{}-{}-{id}",
        Utc::now().format("%Y%m%dT%H%M%SZ"),
        session.harness()
    )
}

/// An export owns a stem before it creates any sibling file. The reservation
/// marker is hidden and short-lived; the three published files remain direct
/// children of the caller's bundle directory.
struct ExportNamespace {
    prefix: PathBuf,
    reservation: PathBuf,
}

impl ExportNamespace {
    fn reserve(directory: &Path, session: &Session) -> Result<Self> {
        fs::create_dir_all(directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
        let base = directory.join(bundle_stem(session));
        for suffix in 0..10_000_u32 {
            let prefix = if suffix == 0 {
                base.clone()
            } else {
                PathBuf::from(format!("{}-{suffix}", base.display()))
            };
            let stem = prefix
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("tapes-export");
            let reservation = prefix.with_file_name(format!(".{stem}.reserve"));
            let marker = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&reservation)
            {
                Ok(marker) => marker,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to reserve export namespace {}", prefix.display())
                    })
                }
            };
            drop(marker);
            let occupied = ["context.md", "json", "trace.md"]
                .into_iter()
                .any(|extension| {
                    let mut path = prefix.as_os_str().to_owned();
                    path.push(format!(".{extension}"));
                    let path = PathBuf::from(path);
                    path.exists() || PathBuf::from(format!("{}.partial", path.display())).exists()
                })
                || {
                    let evidence = PathBuf::from(format!("{}.evidence", prefix.display()));
                    evidence.exists()
                        || PathBuf::from(format!("{}.partial", evidence.display())).exists()
                };
            if occupied {
                let _ = fs::remove_file(&reservation);
                continue;
            }
            return Ok(Self {
                prefix,
                reservation,
            });
        }
        Err(anyhow::anyhow!(
            "could not reserve a unique export namespace under {}",
            directory.display()
        ))
    }

    fn release(&self) {
        let _ = fs::remove_file(&self.reservation);
    }
}

impl Drop for ExportNamespace {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.reservation);
    }
}

/// Place every file or none: when one cannot be placed, the files already
/// placed are removed, so a failed export leaves no partial bundle.
fn place_all<const N: usize>(files: [PartialFile; N]) -> Result<[BundleFile; N]> {
    let mut placed = Vec::with_capacity(N);
    for file in files {
        match file.place() {
            Ok(file) => placed.push(file),
            Err(error) => {
                for file in &placed {
                    let _ = remove_owned_file(&file.path);
                }
                return Err(error);
            }
        }
    }
    Ok(placed
        .try_into()
        .unwrap_or_else(|_| unreachable!("one placed file per partial file")))
}

/// Write a whole body under a temporary name, then rename it into place.
fn write_atomically(prefix: &Path, extension: &str, body: &str) -> Result<BundleFile> {
    let mut file = PartialFile::create(prefix, extension)?;
    file.write_all(body.as_bytes())?;
    file.place()
}

/// A bundle file written under a `.partial` name beside its own and renamed
/// into place once complete, so a half-written file never looks complete. A
/// file dropped before it is placed is removed.
struct PartialFile {
    path: PathBuf,
    temporary: PathBuf,
    out: BufWriter<File>,
    bytes: u64,
    placed: bool,
}

impl PartialFile {
    fn create(prefix: &Path, extension: &str) -> Result<Self> {
        let mut path = prefix.as_os_str().to_owned();
        path.push(".");
        path.push(extension);
        let path = PathBuf::from(path);

        let mut temporary = path.clone().into_os_string();
        temporary.push(".partial");
        let temporary = PathBuf::from(temporary);

        if let Some(directory) = path.parent() {
            fs::create_dir_all(directory)
                .with_context(|| format!("failed to create {}", directory.display()))?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        Ok(Self {
            path,
            temporary,
            out: BufWriter::with_capacity(256 * 1024, file),
            bytes: 0,
            placed: false,
        })
    }

    fn place(mut self) -> Result<BundleFile> {
        self.out
            .flush()
            .with_context(|| format!("failed to write {}", self.temporary.display()))?;
        fs::hard_link(&self.temporary, &self.path)
            .with_context(|| format!("failed to place {}", self.path.display()))?;
        if let Err(error) = fs::remove_file(&self.temporary) {
            let _ = fs::remove_file(&self.path);
            return Err(error)
                .with_context(|| format!("failed to retire {}", self.temporary.display()));
        }
        self.placed = true;
        Ok(BundleFile {
            path: self.path.clone(),
            bytes: self.bytes,
        })
    }
}

fn remove_owned_file(path: &Path) -> std::result::Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{} is not a regular file; it was left untouched",
            path.display()
        ));
    }
    fs::remove_file(path).map_err(|error| format!("{}: {error}", path.display()))
}

impl std::io::Write for PartialFile {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.out.write(buffer)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if !self.placed {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

/// One turn of `.context.md`, the exchange `show --exchange` returns as
/// `TurnKind::in_exchange` defines it — small enough to read whole. Every
/// other kind is in the trace.
fn write_context_turn(out: &mut String, turn: &Turn) {
    write_turn_heading(out, &human_speaker(turn), turn);
    write_turn_body(out, turn);
}

/// One turn of `.trace.md`: every turn, reasoning and tool chronology
/// included, for free-text search.
fn write_trace_turn(out: &mut String, turn: &Turn) {
    match turn.role {
        Role::Tool => {
            let label = tool_label(turn);
            write_turn_heading(out, &format!("tool: {label}"), turn);
        }
        _ => write_turn_heading(out, &human_speaker(turn), turn),
    }
    write_turn_body(out, turn);
    if let Some(tool) = &turn.tool {
        for invocation in &tool.invocations {
            let _ = writeln!(
                out,
                "invocation: {}",
                serde_json::to_string(invocation).unwrap_or_default()
            );
        }
        for consumption in &tool.artifact_consumptions {
            let _ = writeln!(
                out,
                "artifact-consumption: {}",
                serde_json::to_string(consumption).unwrap_or_default()
            );
        }
    }
}

fn write_turn_body(out: &mut String, turn: &Turn) {
    out.push_str(turn.text.trim_end());
    out.push_str("\n\n");
    for part in &turn.parts {
        if part.text().is_none() {
            let _ = writeln!(
                out,
                "content-part: {}",
                serde_json::to_string(part).unwrap_or_default()
            );
        }
    }
}

/// The facts a bundle file states before its turns: the exported `session`,
/// and what `transcript` records about the read.
fn write_header(out: &mut String, session: &Session, transcript: &Transcript, kind: &str) {
    let _ = writeln!(out, "# {} {} ({kind})", session.harness(), session.id);
    let _ = writeln!(out);
    if let Some(projection) = &transcript.projection {
        let _ = writeln!(
            out,
            "- projection: kept {}; omitted {}",
            projection.kept_summary(),
            projection.omitted_summary()
        );
    }
    let title = human_title(session);
    if !title.is_empty() {
        let _ = writeln!(out, "- title: {title}");
    }
    if let Some(model) = &session.model {
        match &model.variant {
            Some(variant) => {
                let _ = writeln!(out, "- model: {} ({variant})", model.id);
            }
            None => {
                let _ = writeln!(out, "- model: {}", model.id);
            }
        }
    }
    if let Some(directory) = &session.directory {
        let _ = writeln!(out, "- directory: {}", directory.display());
    }
    if let Some(last_activity) = session.last_activity_at {
        let _ = writeln!(out, "- last activity: {}", human_timestamp(last_activity));
    } else {
        let _ = writeln!(out, "- last activity: unavailable");
    }
    if let Some(tokens) = &session.tokens {
        let mut counters = Vec::new();
        if let Some(input) = tokens.input {
            counters.push(format!("input {input}"));
        }
        if let Some(output) = tokens.output {
            counters.push(format!("output {output}"));
        }
        if let Some(reasoning) = tokens.reasoning {
            counters.push(format!("reasoning {reasoning}"));
        }
        if let Some(cache_read) = tokens.cache_read {
            counters.push(format!("cache read {cache_read}"));
        }
        if let Some(cache_write) = tokens.cache_write {
            counters.push(format!("cache write {cache_write}"));
        }
        if !counters.is_empty() {
            let accounting = session.accounting.as_ref().map(|accounting| {
                let basis = match accounting.basis {
                    AccountingBasis::RecordedTotal => "recorded total",
                    AccountingBasis::SummedRequests => "summed requests",
                };
                let coverage = match accounting.coverage {
                    AccountingCoverage::Session => "whole session",
                    AccountingCoverage::ReadWindow => "read window",
                };
                format!(" ({basis}, {coverage})")
            });
            let _ = writeln!(
                out,
                "- tokens: {}{}",
                counters.join(", "),
                accounting.unwrap_or_default()
            );
        }
    }
    if let Some(window) = &transcript.truncation.window {
        let _ = if window.omitted_exact {
            writeln!(
                out,
                "- truncated: the last {} of {} turns; {} earlier turns fall outside the {}-turn window",
                window.returned,
                window.returned + window.omitted,
                window.omitted,
                window.bound
            )
        } else {
            writeln!(
                out,
                "- truncated: the last {} of at least {} turns; the read stopped once the {}-turn window was full",
                window.returned,
                window.returned + window.omitted,
                window.bound
            )
        };
    }
    for bound in &transcript.truncation.source {
        let _ = match bound {
            SourceBound::FileTail { bytes } => writeln!(
                out,
                "- truncated: only the final {} of the recording was read",
                human_bytes(*bytes)
            ),
            SourceBound::RecordPage { records, of } => writeln!(
                out,
                "- truncated: only the newest {records} {of} were fetched from the store"
            ),
            SourceBound::TurnText { turns, chars } => writeln!(
                out,
                "- truncated: {turns} turn(s) carry text cut at {chars} characters by the store read"
            ),
            SourceBound::InputCoverage { gaps } => writeln!(
                out,
                "- truncated: supplied input has {gaps} explicit coverage gap(s)"
            ),
        };
    }
    if let Some(read) = &transcript.read {
        let _ = if read
            .projection_options
            .iter()
            .any(|option| option == "full")
        {
            writeln!(
                out,
                "- source read: the whole recording was streamed; {} {} observed",
                read.source_length,
                if read.coordinate_domain == "file-byte-range" {
                    "bytes"
                } else {
                    "records"
                }
            )
        } else {
            writeln!(
                out,
                "- source read: {} bytes observed; configured bound {}",
                read.source_length,
                human_bytes(read.configured_bound)
            )
        };
        for range in &read.ranges {
            let _ = writeln!(
                out,
                "- read range: {:?} [{}..{})",
                range.kind, range.span.start, range.span.end
            );
        }
        if !read.gaps.is_empty() {
            let _ = writeln!(out, "- read gaps: {}", read.gaps.len());
        }
    }
    if let Some(terminal) = &transcript.terminal {
        let _ = writeln!(
            out,
            "- terminal: {}{}",
            terminal.record_type,
            terminal
                .payload_type
                .as_deref()
                .map_or(String::new(), |kind| format!("/{kind}"))
        );
    }
    for note in &transcript.notes {
        let _ = writeln!(out, "- note: {note}");
    }
    let _ = writeln!(out);
}

/// Each heading names the turn's ordinal, so a line quoted from the bundle
/// can be traced back to the same turn `show` renders.
fn write_turn_heading(out: &mut String, speaker: &str, turn: &Turn) {
    match turn.ts {
        Some(ts) => {
            let _ = writeln!(
                out,
                "## {speaker} #{} — {}",
                turn.ordinal,
                human_timestamp(ts)
            );
        }
        None => {
            let _ = writeln!(out, "## {speaker} #{}", turn.ordinal);
        }
    }
    let _ = writeln!(out);
}

/// Tool turns carry the harness's own JSON envelope. Lead with whatever names
/// the tool so the trace is scannable; the envelope stays beneath it.
fn tool_label(turn: &Turn) -> String {
    if let Some(name) = turn.tool.as_ref().and_then(|event| event.name.as_ref()) {
        return name.clone();
    }
    let Ok(value) = serde_json::from_str::<Value>(&turn.text) else {
        return "unnamed".to_owned();
    };
    for key in ["name", "toolName", "tool_name"] {
        if let Some(name) = value[key].as_str() {
            return name.to_owned();
        }
    }
    for key in ["tool_use_id", "call_id", "callID", "toolCallId"] {
        if value[key].is_string() {
            return "result".to_owned();
        }
    }
    "unnamed".to_owned()
}

/// The commit and branch a recorded working directory sits on, read from the
/// directory itself. Absent when the recording names no directory, when the
/// directory is gone, and when it is not a repository.
pub fn git_context(directory: Option<&Path>) -> Option<GitContext> {
    let directory = directory?;
    if !directory.is_dir() {
        return None;
    }
    let head = git(directory, &["rev-parse", "HEAD"]);
    let branch =
        git(directory, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|branch| branch != "HEAD");
    (head.is_some() || branch.is_some()).then_some(GitContext { head, branch })
}

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_bundle_file_that_cannot_be_placed_takes_the_placed_ones_with_it() {
        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-place-all-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let prefix = directory.join("bundle");
        let files = ["context.md", "json", "trace.md"]
            .map(|extension| super::PartialFile::create(&prefix, extension).unwrap());
        // A non-empty directory where the last file belongs refuses its rename.
        fs::create_dir_all(directory.join("bundle.trace.md/occupied")).unwrap();

        assert!(super::place_all(files).is_err());
        let mut left = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        left.sort();
        assert_eq!(left, ["bundle.trace.md"]);
        fs::remove_dir_all(directory).unwrap();
    }

    use chrono::TimeZone;

    use super::*;
    use crate::model::{
        Accounting, AccountingBasis, AccountingCoverage, LiveState, Model, SourceDescriptor,
        Tokens, TrailingRecord, Truncation, Turn, TurnKind, SESSION_SCHEMA,
    };

    fn transcript() -> Transcript {
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        Transcript {
            session: Session {
                id: "ses_abc".into(),
                source: SourceDescriptor::installed("opencode", "opencode-database"),
                metadata: None,
                model: Some(Model {
                    id: "kimi-k3".into(),
                    variant: Some("max".into()),
                }),
                title: Some("A rescue".into()),
                derived_title: None,
                derived_title_truncated: None,
                directory: None,
                started_at: Some(ts),
                last_activity_at: Some(ts),
                live: None,
                cost: None,
                tokens: None,
                accounting: None,
                start_uncertain: false,
                occurrence: None,
                usage_detail: None,
            },
            turns: vec![
                Turn {
                    role: Role::User,
                    kind: TurnKind::Operator,
                    text: "fix the parser".into(),
                    ts: Some(ts),
                    ordinal: 0,
                    native_id: None,
                    request_turn_id: None,
                    metadata: None,
                    record_ref: None,
                    parts: Vec::new(),
                    coverage: None,
                    channel: None,
                    recipient: None,
                    tool: None,
                },
                Turn {
                    role: Role::Reasoning,
                    kind: TurnKind::Reasoning,
                    text: "the parser drops empty lines".into(),
                    ts: Some(ts),
                    ordinal: 0,
                    native_id: None,
                    request_turn_id: None,
                    metadata: None,
                    record_ref: None,
                    parts: Vec::new(),
                    coverage: None,
                    channel: None,
                    recipient: None,
                    tool: None,
                },
                Turn {
                    role: Role::Tool,
                    kind: TurnKind::Tool,
                    text: r#"{"name":"shell","input":{"command":"cargo test"}}"#.into(),
                    ts: Some(ts),
                    ordinal: 0,
                    native_id: None,
                    request_turn_id: None,
                    metadata: None,
                    record_ref: None,
                    parts: Vec::new(),
                    coverage: None,
                    channel: None,
                    recipient: None,
                    tool: None,
                },
                Turn {
                    role: Role::Assistant,
                    kind: TurnKind::Assistant,
                    text: "fixed it".into(),
                    ts: Some(ts),
                    ordinal: 0,
                    native_id: None,
                    request_turn_id: None,
                    metadata: None,
                    record_ref: None,
                    parts: Vec::new(),
                    coverage: None,
                    channel: None,
                    recipient: None,
                    tool: None,
                },
            ],
            truncated: false,
            truncation: Truncation::default(),
            read: None,
            terminal: None,
            text_tail: None,
            artifacts: Vec::new(),
            graph: None,
            trailing_record: None,
            notes: vec!["1 entry belongs to an abandoned branch.".into()],
            projection: None,
            kinds: None,
        }
    }

    /// Each test gets its own directory: they run in parallel and every one of
    /// them counts and then removes the directory's contents.
    fn export_into_temporary_directory(test: &str) -> (PathBuf, Bundle) {
        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let bundle = export(&transcript(), &directory).unwrap();
        (directory, bundle)
    }

    #[test]
    fn export_writes_three_files_sharing_one_prefix() {
        let (directory, bundle) = export_into_temporary_directory("three-files");

        let names = bundle
            .files()
            .map(|file| file.path.file_name().unwrap().to_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 3);
        let stem = names[0].strip_suffix(".context.md").unwrap().to_owned();
        assert_eq!(names[1], format!("{stem}.json"));
        assert_eq!(names[2], format!("{stem}.trace.md"));
        assert!(stem.ends_with("-opencode-ses_abc"));
        for file in bundle.files() {
            assert!(file.path.is_file(), "{} is missing", file.path.display());
        }
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 3);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn context_holds_operator_turns_and_excludes_reasoning_and_tools() {
        let (directory, bundle) = export_into_temporary_directory("context");
        let context = fs::read_to_string(&bundle.context.path).unwrap();

        assert!(context.contains("fix the parser"));
        assert!(context.contains("fixed it"));
        assert!(!context.contains("the parser drops empty lines"));
        assert!(!context.contains("cargo test"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn trace_holds_the_whole_chronology_and_names_its_tools() {
        let (directory, bundle) = export_into_temporary_directory("trace");
        let trace = fs::read_to_string(&bundle.trace.path).unwrap();

        assert!(trace.contains("## reasoning"));
        assert!(trace.contains("the parser drops empty lines"));
        assert!(trace.contains("## tool: shell"));
        assert!(trace.contains("cargo test"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_streamed_session_object_matches_the_whole_transcript() {
        let expected = serde_json::to_string(&transcript()).unwrap();
        let mut rest = transcript();
        let turns = std::mem::take(&mut rest.turns);
        let mut out = Vec::new();
        let mut writer = crate::model::StreamedSessionJson::open(&mut out, &rest.session).unwrap();
        for turn in &turns {
            writer.turn(&mut out, turn).unwrap();
        }
        writer.close(&mut out, &rest).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }

    #[test]
    fn json_carries_the_schema_and_the_transcript_notes() {
        let (directory, bundle) = export_into_temporary_directory("json");
        let value: Value =
            serde_json::from_str(&fs::read_to_string(&bundle.json.path).unwrap()).unwrap();

        assert_eq!(value["schema"], SESSION_SCHEMA);
        assert_eq!(value["session"]["id"], "ses_abc");
        assert_eq!(value["turns"].as_array().unwrap().len(), 4);
        assert_eq!(value["notes"][0], "1 entry belongs to an abandoned branch.");
        assert!(value.get("git").is_none());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn json_carries_a_trailing_record() {
        let directory = std::env::temp_dir().join(format!(
            "tapes-bundle-{}-trailing-record",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        let mut transcript = transcript();
        transcript.trailing_record = Some(TrailingRecord {
            kind: "event_msg".into(),
            timestamp: Some(Utc.timestamp_opt(1_700_000_001, 0).unwrap()),
        });
        let bundle = export(&transcript, &directory).unwrap();
        let value: Value =
            serde_json::from_str(&fs::read_to_string(&bundle.json.path).unwrap()).unwrap();

        assert_eq!(
            value["trailing_record"],
            serde_json::json!({
                "kind": "event_msg",
                "timestamp": "2023-11-14T22:13:21Z"
            })
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn json_omits_volatile_live_state() {
        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-{}-live-state", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let mut transcript = transcript();
        transcript.session.live = Some(LiveState::Working);
        let bundle = export(&transcript, &directory).unwrap();
        let value: Value =
            serde_json::from_str(&fs::read_to_string(&bundle.json.path).unwrap()).unwrap();

        assert!(value["session"].get("live").is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn markdown_marks_derived_titles_and_formats_timestamps_for_humans() {
        let mut transcript = transcript();
        let ts = Utc.timestamp_opt(1_700_000_000, 123_456_789).unwrap();
        transcript.session.title = None;
        transcript.session.derived_title = Some("Inspect the fixture".into());
        transcript.session.last_activity_at = Some(ts);
        for turn in &mut transcript.turns {
            turn.ts = Some(ts);
        }

        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-{}-derived-title", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let bundle = export(&transcript, &directory).unwrap();
        let context = fs::read_to_string(&bundle.context.path).unwrap();
        let trace = fs::read_to_string(&bundle.trace.path).unwrap();

        assert!(context.contains("- title: ~Inspect the fixture"));
        assert!(context.contains("- last activity: 2023-11-14T22:13:20Z"));
        assert!(trace.contains("## user #0 — 2023-11-14T22:13:20Z"));
        assert!(!context.contains(".123456789Z"));
        assert!(!trace.contains(".123456789Z"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn markdown_headers_report_tokens_and_accounting() {
        let mut transcript = transcript();
        transcript.session.tokens = Some(Tokens {
            input: Some(100),
            output: Some(50),
            reasoning: Some(25),
            cache_read: Some(10),
            cache_write: Some(5),
        });
        transcript.session.accounting = Some(Accounting {
            basis: AccountingBasis::RecordedTotal,
            coverage: AccountingCoverage::Session,
        });

        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-{}-token-header", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let bundle = export(&transcript, &directory).unwrap();
        let context = fs::read_to_string(&bundle.context.path).unwrap();
        let trace = fs::read_to_string(&bundle.trace.path).unwrap();
        let line = "- tokens: input 100, output 50, reasoning 25, cache read 10, cache write 5 (recorded total, whole session)";
        assert!(context.contains(line), "{context}");
        assert!(trace.contains(line), "{trace}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn no_partial_files_survive_a_completed_export() {
        let (directory, _) = export_into_temporary_directory("atomic");

        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            assert!(
                !path.to_string_lossy().ends_with(".partial"),
                "{} survived",
                path.display()
            );
        }

        fs::remove_dir_all(directory).unwrap();
    }

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
    }

    fn scratch(test: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("tapes-bundle-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    fn json_of(file: &BundleFile) -> Value {
        serde_json::from_str(&fs::read_to_string(&file.path).unwrap()).unwrap()
    }

    /// A Markdown file's turns: everything after its title and header lists.
    fn turns_of(file: &BundleFile) -> String {
        let text = fs::read_to_string(&file.path).unwrap();
        text.splitn(3, "\n\n").nth(2).unwrap().to_owned()
    }

    fn events_value(events: &[crate::event::EventRecord]) -> Value {
        if events.is_empty() {
            Value::Null
        } else {
            serde_json::to_value(events).unwrap()
        }
    }

    /// Where the bounded read covers a whole recording, a whole-recording
    /// export writes the bundle a bounded one does: the same turns, the same
    /// paired events, which are the ones `events` projects, and the same
    /// Markdown turns.
    #[test]
    fn a_whole_export_matches_a_bounded_export_where_the_bound_covers_the_file() {
        use crate::backend::{
            claude::ClaudeBackend, codex::CodexBackend, pi::PiBackend, Backend, Query,
        };
        use crate::ExportRead;

        let root = fixture_root();
        let backends: Vec<Box<dyn Backend>> = vec![
            Box::new(ClaudeBackend::new(root.join("claude"))),
            Box::new(CodexBackend::new(root.join("codex"))),
            Box::new(PiBackend::new(root.join("pi"))),
        ];
        let directory = scratch("whole-matches-bounded");
        let (mut sessions, mut events) = (0, 0);
        for backend in &backends {
            for session in backend.list(&Query::unscoped(usize::MAX)).unwrap().sessions {
                let label = format!("{} {}", backend.harness(), session.id);
                let export = |read, into: &str| {
                    crate::export_session(
                        backend.as_ref(),
                        &session,
                        &directory.join(into),
                        None,
                        read,
                        &[],
                    )
                    .unwrap()
                };
                let bounded = export(ExportRead::Bounded, "bounded");
                let whole = export(ExportRead::Whole, "whole");
                let (bounded_json, whole_json) = (json_of(&bounded.json), json_of(&whole.json));
                assert_eq!(whole_json["turns"], bounded_json["turns"], "{label}");
                let projected = crate::event::project(
                    backend.transcript(&session, usize::MAX).unwrap(),
                    usize::MAX,
                )
                .events;
                assert_eq!(whole_json["events"], events_value(&projected), "{label}");
                assert_eq!(bounded_json["events"], events_value(&projected), "{label}");
                assert_eq!(whole_json["read"]["projection_options"][0], "full");
                assert_eq!(
                    turns_of(&whole.context),
                    turns_of(&bounded.context),
                    "{label}"
                );
                assert_eq!(turns_of(&whole.trace), turns_of(&bounded.trace), "{label}");
                sessions += 1;
                events += projected.len();
            }
        }
        let leftovers = fs::read_dir(directory.join("whole"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .path()
                    .to_string_lossy()
                    .ends_with(".partial")
            })
            .count();
        fs::remove_dir_all(&directory).unwrap();
        assert_eq!(leftovers, 0);
        assert!(
            sessions >= 5 && events > 0,
            "{sessions} sessions, {events} events"
        );
    }

    /// A recording appended to between an export's two reads is exported as
    /// the first read saw it: the replay stops where that read stopped, so a
    /// call whose result arrived in between stays unanswered instead of
    /// pairing with a result the JSON turns never held. A second read that is
    /// not replayed diverges from the first, and no file is placed.
    #[test]
    fn a_recording_appended_between_the_two_reads_exports_what_the_first_read_saw() {
        use std::cell::Cell;
        use std::io::Write as _;

        use crate::backend::{claude::ClaudeBackend, Backend, Listing, Query, StreamedTranscript};
        use crate::ExportRead;

        struct Appending {
            inner: ClaudeBackend,
            path: PathBuf,
            appended: Cell<bool>,
            replays: bool,
        }

        impl Backend for Appending {
            fn kinds(&self) -> crate::model::KindDeclaration {
                self.inner.kinds()
            }
            fn harness(&self) -> &'static str {
                self.inner.harness()
            }
            fn available(&self) -> bool {
                true
            }
            fn list(&self, query: &Query) -> Result<Listing> {
                self.inner.list(query)
            }
            fn locate(&self, id: &str) -> Result<Option<Session>> {
                self.inner.locate(id)
            }
            fn transcript(&self, session: &Session, tail: usize) -> Result<Transcript> {
                self.inner.transcript(session, tail)
            }
            fn stream_transcript(
                &self,
                session: &Session,
                replay: Option<&StreamedTranscript>,
                turn: &mut dyn FnMut(Turn) -> Result<()>,
            ) -> Result<StreamedTranscript> {
                let replay = replay.filter(|_| self.replays);
                let read = self.inner.stream_transcript(session, replay, turn)?;
                if !self.appended.replace(true) {
                    // A later modification time, so the file's revision moves.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    let record = |uuid: &str, role: &str, content: Value| {
                        serde_json::json!({"type": role, "sessionId": "appended", "uuid": uuid, "timestamp": "2026-01-01T10:00:05Z", "cwd": "/fixtures/project", "message": {"role": role, "content": content}}).to_string() + "\n"
                    };
                    let late = record(
                        "r2",
                        "user",
                        serde_json::json!([{"type": "tool_result", "tool_use_id": "tool-late", "content": "late result"}]),
                    ) + &record("a3", "assistant", Value::from("late answer"));
                    fs::OpenOptions::new()
                        .append(true)
                        .open(&self.path)?
                        .write_all(late.as_bytes())?;
                }
                Ok(read)
            }
        }

        let directory = scratch("appended-between-reads");
        let store = directory.join("store");
        let project = store.join("-fixtures-project");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("appended.jsonl");
        let record = |uuid: &str, role: &str, content: Value| {
            serde_json::json!({"type": role, "sessionId": "appended", "uuid": uuid, "timestamp": "2026-01-01T10:00:00Z", "cwd": "/fixtures/project", "message": {"role": role, "content": content}}).to_string() + "\n"
        };
        let opening = record("u1", "user", Value::from("start"))
            + &record(
                "a1",
                "assistant",
                serde_json::json!([{"type": "tool_use", "id": "tool-1", "name": "fixture_tool", "input": {}}]),
            )
            + &record(
                "r1",
                "user",
                serde_json::json!([{"type": "tool_result", "tool_use_id": "tool-1", "content": "done"}]),
            )
            + &record(
                "a2",
                "assistant",
                serde_json::json!([{"type": "tool_use", "id": "tool-late", "name": "fixture_tool", "input": {}}]),
            );

        let export = |replays: bool, into: &str| {
            fs::write(&path, &opening).unwrap();
            let backend = Appending {
                inner: ClaudeBackend::new(&store),
                path: path.clone(),
                appended: Cell::new(false),
                replays,
            };
            let session = backend.locate("appended").unwrap().unwrap();
            let before = backend.transcript(&session, usize::MAX).unwrap();
            let bundle = crate::export_session(
                &backend,
                &session,
                &directory.join(into),
                None,
                ExportRead::Whole,
                &[],
            );
            (before, bundle)
        };

        let (before, bundle) = export(true, "replayed");
        let bundle = bundle.unwrap();
        let json = json_of(&bundle.json);
        let expected = crate::event::project(before.clone(), usize::MAX).events;
        assert_eq!(json["turns"], serde_json::to_value(&before.turns).unwrap());
        assert_eq!(json["events"], events_value(&expected));
        assert_eq!(json["read"]["source_length"], opening.len() as u64);
        assert!(
            expected
                .iter()
                .any(|event| event.event.call_id.as_deref() == Some("tool-late")
                    && event.incomplete == Some(crate::event::Incomplete::NoResultInRead)),
            "{expected:?}"
        );
        assert!(!fs::read_to_string(&bundle.trace.path)
            .unwrap()
            .contains("late answer"));

        let (_, diverged) = export(false, "unreplayed");
        let error = format!("{:#}", diverged.err().expect("an unreplayed second read"));
        let placed = fs::read_dir(directory.join("unreplayed")).unwrap().count();
        fs::remove_dir_all(&directory).unwrap();
        assert!(
            error.contains("differed between the two pairing passes"),
            "{error}"
        );
        assert_eq!(placed, 0);
    }

    #[test]
    fn tool_labels_come_from_whichever_key_the_harness_uses() {
        let turn = |text: &str| Turn {
            role: Role::Tool,
            kind: TurnKind::Tool,
            text: text.to_owned(),
            ts: None,
            ordinal: 0,
            native_id: None,
            request_turn_id: None,
            metadata: None,
            record_ref: None,
            parts: Vec::new(),
            coverage: None,
            channel: None,
            recipient: None,
            tool: None,
        };
        assert_eq!(tool_label(&turn(r#"{"name":"shell"}"#)), "shell");
        assert_eq!(tool_label(&turn(r#"{"tool_use_id":"toolu_1"}"#)), "result");
        assert_eq!(tool_label(&turn(r#"{"call_id":"call_1"}"#)), "result");
        assert_eq!(tool_label(&turn("not json")), "unnamed");
    }
}
