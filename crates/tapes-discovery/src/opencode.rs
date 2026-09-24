use crate::{CandidatePage, DiscoveryError, NativeSession};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ROWS: usize = 1_000;
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const COMMAND_DEADLINE: Duration = Duration::from_secs(30);
const SESSION_ID_PREFIX: &str = "ses_";

/// OpenCode's stable database and v2 API are stores of the same harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenCodeFlavor {
    Stable,
    V2,
}

impl OpenCodeFlavor {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::V2 => "opencode2",
        }
    }
}

/// A read-only OpenCode metadata source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeStore {
    program: OsString,
    flavor: OpenCodeFlavor,
}

impl OpenCodeStore {
    /// Select the stable store only for a program named `opencode`.
    pub fn new(program: impl Into<OsString>) -> Self {
        let program = program.into();
        let flavor = if Path::new(&program).file_name() == Some(OsStr::new("opencode")) {
            OpenCodeFlavor::Stable
        } else {
            OpenCodeFlavor::V2
        };
        Self { program, flavor }
    }

    pub fn stable(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            flavor: OpenCodeFlavor::Stable,
        }
    }

    pub fn v2(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            flavor: OpenCodeFlavor::V2,
        }
    }

    pub fn flavor(&self) -> OpenCodeFlavor {
        self.flavor
    }

    pub fn program(&self) -> &OsStr {
        &self.program
    }

    pub fn coordinate(&self) -> String {
        match self.flavor {
            OpenCodeFlavor::Stable => database_path().map_or_else(
                || format!("{} database", self.program.to_string_lossy()),
                |path| path.display().to_string(),
            ),
            OpenCodeFlavor::V2 => format!("{} API", self.program.to_string_lossy()),
        }
    }

    pub fn locate_exact(&self, id: &str) -> Result<Option<NativeSession>, DiscoveryError> {
        if !id.starts_with(SESSION_ID_PREFIX) {
            return Ok(None);
        }
        if !self.store_exists()? {
            return Ok(None);
        }
        match self.flavor {
            OpenCodeFlavor::Stable => self.database_locate(id),
            OpenCodeFlavor::V2 => self.api_locate(id),
        }
    }

    pub fn candidates(&self, limit: usize) -> CandidatePage {
        let mut page = CandidatePage {
            records: Vec::new(),
            scanned: 0,
            visited_entries: 0,
            complete: true,
            failures: Vec::new(),
            unreadable_ids: Vec::new(),
        };
        match self.store_exists() {
            Ok(false) => return page,
            Err(error) => {
                page.complete = false;
                page.failures.push(error);
                return page;
            }
            Ok(true) => {}
        }
        let result = match self.flavor {
            OpenCodeFlavor::Stable => self.database_candidates(limit.min(MAX_ROWS)),
            OpenCodeFlavor::V2 => self.api_candidates(limit.min(MAX_ROWS)),
        };
        match result {
            Ok(found) => found,
            Err(error) => {
                page.complete = false;
                page.failures.push(error);
                page
            }
        }
    }

    fn store_exists(&self) -> Result<bool, DiscoveryError> {
        let Some(directory) = data_directory() else {
            return Ok(false);
        };
        let metadata = match fs::metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(DiscoveryError::io(
                    directory.display().to_string(),
                    "inspect OpenCode data directory",
                    &error,
                ));
            }
        };
        if !metadata.is_dir() {
            return Err(DiscoveryError::InvalidMetadata {
                coordinate: directory.display().to_string(),
                reason: "OpenCode data root is not a directory",
            });
        }
        match self.flavor {
            OpenCodeFlavor::Stable => {
                let Some(database) = database_path() else {
                    return Ok(false);
                };
                let metadata = match fs::metadata(&database) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => {
                        return Err(DiscoveryError::io(
                            database.display().to_string(),
                            "inspect OpenCode database",
                            &error,
                        ));
                    }
                };
                if !metadata.is_file() {
                    return Err(DiscoveryError::InvalidMetadata {
                        coordinate: database.display().to_string(),
                        reason: "OpenCode database is not a regular file",
                    });
                }
                Ok((self.is_default_program("opencode")
                    && program_available(OsStr::new("sqlite3")))
                    || program_available(&self.program))
            }
            OpenCodeFlavor::V2 => Ok(program_available(&self.program)),
        }
    }

    fn is_default_program(&self, name: &str) -> bool {
        Path::new(&self.program).components().count() == 1
            && Path::new(&self.program).file_name() == Some(OsStr::new(name))
    }

    fn database_locate(&self, id: &str) -> Result<Option<NativeSession>, DiscoveryError> {
        let query = format!(
            "SELECT json_object('id', id, 'title', title, 'directory', directory, \
             'time_created', time_created, 'time_updated', time_updated, 'model', model, \
             'cost', cost, 'tokens_input', tokens_input, 'tokens_output', tokens_output, \
             'tokens_reasoning', tokens_reasoning, 'tokens_cache_read', tokens_cache_read, \
             'tokens_cache_write', tokens_cache_write) AS row FROM session WHERE id = {} LIMIT 1",
            sql_literal(id)
        );
        let rows = self.database_rows(&query)?;
        match rows.as_slice() {
            [] => Ok(None),
            [row] => {
                let record = self.record(row.clone())?;
                if record.id() != id {
                    return Err(DiscoveryError::InvalidMetadata {
                        coordinate: self.coordinate(),
                        reason: "OpenCode exact lookup returned a different ID",
                    });
                }
                Ok(Some(record))
            }
            _ => Err(DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode exact lookup returned multiple rows",
            }),
        }
    }

    fn database_candidates(&self, limit: usize) -> Result<CandidatePage, DiscoveryError> {
        let limit = limit.min(MAX_ROWS);
        let query = format!(
            "SELECT json_object('id', id, 'title', title, 'directory', directory, \
             'time_created', time_created, 'time_updated', time_updated, 'model', model, \
             'cost', cost, 'tokens_input', tokens_input, 'tokens_output', tokens_output, \
             'tokens_reasoning', tokens_reasoning, 'tokens_cache_read', tokens_cache_read, \
             'tokens_cache_write', tokens_cache_write) AS row FROM session \
             ORDER BY time_updated DESC LIMIT {limit}"
        );
        self.page_from_rows(self.database_rows(&query)?, limit)
    }

    fn database_rows(&self, query: &str) -> Result<Vec<Value>, DiscoveryError> {
        let Some(bytes) = self.database_text(query)? else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8(bytes).map_err(|_| DiscoveryError::InvalidMetadata {
            coordinate: self.coordinate(),
            reason: "OpenCode database response is not UTF-8",
        })?;
        parse_database_rows(&text, &self.coordinate())
    }

    fn database_text(&self, query: &str) -> Result<Option<Vec<u8>>, DiscoveryError> {
        let Some(database) = database_path() else {
            return Ok(None);
        };
        if self.is_default_program("opencode") && program_available(OsStr::new("sqlite3")) {
            let database = database
                .to_str()
                .ok_or_else(|| DiscoveryError::InvalidMetadata {
                    coordinate: self.coordinate(),
                    reason: "OpenCode database path is not UTF-8",
                })?;
            return run_command(
                OsStr::new("sqlite3"),
                &[
                    OsString::from("-readonly"),
                    OsString::from("-batch"),
                    OsString::from("-list"),
                    OsString::from("-header"),
                    OsString::from(database),
                    OsString::from(query),
                ],
                &self.coordinate(),
                COMMAND_DEADLINE,
            )
            .map(Some);
        }
        run_command(
            &self.program,
            &[
                OsString::from("db"),
                OsString::from("--format"),
                OsString::from("tsv"),
                OsString::from(query),
            ],
            &self.coordinate(),
            COMMAND_DEADLINE,
        )
        .map(Some)
    }

    fn api_locate(&self, id: &str) -> Result<Option<NativeSession>, DiscoveryError> {
        let path = format!("/api/session/{}", encode_path_component(id));
        let response = self.api_request(&path)?;
        let data = response
            .get("data")
            .ok_or_else(|| DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode exact response has no data field",
            })?;
        if data.is_null() {
            return Ok(None);
        }
        if !data.is_object() {
            return Err(DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode exact response data is not an object",
            });
        }
        let record = self.record(data.clone())?;
        if record.id() != id {
            return Err(DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode exact response ID does not match the query",
            });
        }
        Ok(Some(record))
    }

    fn api_candidates(&self, limit: usize) -> Result<CandidatePage, DiscoveryError> {
        let limit = limit.min(MAX_ROWS);
        let path = format!("/api/session?order=desc&limit={limit}");
        let response = self.api_request(&path)?;
        let rows = response
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode listing response data is not an array",
            })?;
        let scanned = rows.len().min(MAX_ROWS);
        let mut page = CandidatePage {
            records: Vec::new(),
            scanned,
            visited_entries: 0,
            complete: rows.len() < limit,
            failures: Vec::new(),
            unreadable_ids: Vec::new(),
        };
        if rows.len() > limit {
            page.complete = false;
            page.failures.push(DiscoveryError::BoundExhausted {
                coordinate: self.coordinate(),
                bound: "1,000 OpenCode metadata rows",
            });
        }
        self.retain_rows(rows.iter().take(limit), &mut page);
        if rows.len() >= limit && limit == MAX_ROWS {
            page.complete = false;
        }
        Ok(page)
    }

    fn api_request(&self, path: &str) -> Result<Value, DiscoveryError> {
        let bytes = run_command(
            &self.program,
            &[
                OsString::from("api"),
                OsString::from("--standalone"),
                OsString::from("get"),
                OsString::from(path),
            ],
            &self.coordinate(),
            COMMAND_DEADLINE,
        )?;
        serde_json::from_slice(&bytes).map_err(|_| DiscoveryError::InvalidMetadata {
            coordinate: self.coordinate(),
            reason: "OpenCode API response is malformed JSON",
        })
    }

    fn page_from_rows(
        &self,
        rows: Vec<Value>,
        limit: usize,
    ) -> Result<CandidatePage, DiscoveryError> {
        let mut page = CandidatePage {
            records: Vec::new(),
            scanned: rows.len(),
            visited_entries: 0,
            complete: rows.len() < limit,
            failures: Vec::new(),
            unreadable_ids: Vec::new(),
        };
        self.retain_rows(rows.iter().take(limit), &mut page);
        if rows.len() >= limit && limit == MAX_ROWS {
            page.complete = false;
        }
        Ok(page)
    }

    fn retain_rows<'a>(&self, rows: impl Iterator<Item = &'a Value>, page: &mut CandidatePage) {
        let mut retained = 0usize;
        for row in rows {
            match self.record(row.clone()) {
                Ok(record) => {
                    let serialized = serde_json::to_vec(row).map_or(0, |bytes| bytes.len());
                    let charge = serialized
                        .saturating_add(record.id().len())
                        .saturating_add(self.coordinate().len())
                        .saturating_add(size_of::<NativeSession>());
                    if retained.saturating_add(charge) > MAX_RETAINED_BYTES {
                        page.complete = false;
                        page.failures.push(DiscoveryError::BoundExhausted {
                            coordinate: self.coordinate(),
                            bound: "16 MiB retained data",
                        });
                        break;
                    }
                    retained += charge;
                    page.records.push(record);
                }
                Err(error) => {
                    if let Some(id) = row.get("id").and_then(Value::as_str) {
                        let charge = id.len() + size_of::<String>();
                        if retained.saturating_add(charge) <= MAX_RETAINED_BYTES {
                            retained += charge;
                            page.unreadable_ids.push(id.to_owned());
                        } else {
                            page.complete = false;
                        }
                    }
                    page.complete = false;
                    if retained
                        .saturating_add(error.to_string().len() + size_of::<DiscoveryError>())
                        <= MAX_RETAINED_BYTES
                    {
                        retained += error.to_string().len() + size_of::<DiscoveryError>();
                        page.failures.push(error);
                    } else if !page.failures.iter().any(|failure| {
                        matches!(
                            failure,
                            DiscoveryError::BoundExhausted {
                                bound: "16 MiB retained data",
                                ..
                            }
                        )
                    }) {
                        page.failures.push(DiscoveryError::BoundExhausted {
                            coordinate: self.coordinate(),
                            bound: "16 MiB retained data",
                        });
                    }
                }
            }
        }
    }

    fn record(&self, metadata: Value) -> Result<NativeSession, DiscoveryError> {
        if !metadata.is_object() {
            return Err(DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode session metadata is not an object",
            });
        }
        let id = metadata
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && !id.contains('\0'))
            .ok_or_else(|| DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode session metadata has no valid ID",
            })?
            .to_owned();
        Ok(NativeSession::opencode(
            id,
            self.coordinate(),
            self.flavor,
            metadata,
        ))
    }
}

pub(crate) fn default_stores() -> Vec<OpenCodeStore> {
    let mut stores = vec![OpenCodeStore::stable("opencode")];
    if program_available(OsStr::new("opencode2")) {
        stores.push(OpenCodeStore::v2("opencode2"));
    }
    stores
}

fn data_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|path| path.join("opencode"))
}

fn database_path() -> Option<PathBuf> {
    data_directory().map(|path| path.join("opencode.db"))
}

fn program_available(program: &OsStr) -> bool {
    Path::new(program).is_file()
        || std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
        })
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn encode_path_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn parse_database_rows(text: &str, coordinate: &str) -> Result<Vec<Value>, DiscoveryError> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .map(|header| header.trim_end_matches('\r'))
        .unwrap_or_default();
    if header != "row" {
        return Err(DiscoveryError::InvalidMetadata {
            coordinate: coordinate.to_owned(),
            reason: "OpenCode database response has an unexpected header",
        });
    }
    let mut rows = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let json = line
            .strip_prefix("row\t")
            .or_else(|| line.strip_prefix("row "))
            .unwrap_or(line)
            .trim_end_matches('\r');
        let row: Value =
            serde_json::from_str(json).map_err(|_| DiscoveryError::InvalidMetadata {
                coordinate: coordinate.to_owned(),
                reason: "OpenCode database returned malformed row metadata",
            })?;
        rows.push(row);
        if rows.len() > MAX_ROWS {
            return Err(DiscoveryError::BoundExhausted {
                coordinate: coordinate.to_owned(),
                bound: "1,000 OpenCode metadata rows",
            });
        }
    }
    Ok(rows)
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            self.terminate();
        }
    }
}

fn run_command(
    program: &OsStr,
    args: &[OsString],
    coordinate: &str,
    deadline_after: Duration,
) -> Result<Vec<u8>, DiscoveryError> {
    let deadline = Instant::now() + deadline_after;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = command.spawn().map_err(|error| {
        DiscoveryError::io(coordinate.to_owned(), "spawn metadata command", &error)
    })?;
    let mut child = ChildGuard::new(child);
    let stdout = child.child.stdout.take().expect("piped stdout is present");
    let stderr = child.child.stderr.take().expect("piped stderr is present");
    let mut stdout_reader = Some(thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    }));
    let mut stderr_reader = Some(thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = [0_u8; 8192];
        let mut drained = 0_u64;
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) => return Ok(drained),
                Ok(count) => drained = drained.saturating_add(count as u64),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }));
    let mut completed_stdout = None;
    let mut completed_stderr = None;
    let mut status = None;
    loop {
        if completed_stdout.is_none()
            && stdout_reader
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished)
        {
            let bytes = join_stdout(stdout_reader.take().expect("reader exists"), coordinate)?;
            if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                child.terminate();
                if let Some(reader) = stderr_reader.take() {
                    let _ = reader.join();
                }
                return Err(DiscoveryError::BoundExhausted {
                    coordinate: coordinate.to_owned(),
                    bound: "8 MiB OpenCode response",
                });
            }
            completed_stdout = Some(bytes);
        }
        if completed_stderr.is_none()
            && stderr_reader
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished)
        {
            completed_stderr = Some(stderr_reader.take().expect("stderr reader exists").join());
        }
        if status.is_none() {
            status = child.child.try_wait().map_err(|error| {
                DiscoveryError::io(coordinate, "wait for metadata command", &error)
            })?;
            child.reaped = status.is_some();
        }
        if status.is_some() && completed_stdout.is_some() && completed_stderr.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.terminate();
            if let Some(reader) = stdout_reader.take() {
                let _ = reader.join();
            }
            if let Some(reader) = stderr_reader.take() {
                let _ = reader.join();
            }
            return Err(DiscoveryError::TimedOut {
                coordinate: coordinate.to_owned(),
            });
        }
        thread::sleep(Duration::from_millis(5));
    }
    let bytes = completed_stdout.expect("stdout completed before command acceptance");
    let stderr_bytes = completed_stderr
        .expect("stderr completed before command acceptance")
        .map_err(|_| DiscoveryError::CommandFailed {
            coordinate: coordinate.to_owned(),
            status: "stderr reader stopped".to_owned(),
        })?
        .map_err(|error| DiscoveryError::io(coordinate, "drain metadata stderr", &error))?;
    let _stderr_was_drained = stderr_bytes;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(DiscoveryError::BoundExhausted {
            coordinate: coordinate.to_owned(),
            bound: "8 MiB OpenCode response",
        });
    }
    let status = status.expect("command status set before loop exit");
    if !status.success() {
        return Err(DiscoveryError::CommandFailed {
            coordinate: coordinate.to_owned(),
            status: exit_status_label(status),
        });
    }
    Ok(bytes)
}

fn join_stdout(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    coordinate: &str,
) -> Result<Vec<u8>, DiscoveryError> {
    reader
        .join()
        .map_err(|_| DiscoveryError::CommandFailed {
            coordinate: coordinate.to_owned(),
            status: "stdout reader stopped".to_owned(),
        })?
        .map_err(|error| DiscoveryError::io(coordinate, "read metadata response", &error))
}

fn exit_status_label(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("exit {code}"))
        .unwrap_or_else(|| "terminated by signal".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Harness;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Temp(PathBuf);

    static XDG_LOCK: Mutex<()> = Mutex::new(());

    impl Temp {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "tapes-opencode-discovery-{}-{nonce}",
                std::process::id()
            ));
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

    fn fake_program(temp: &Temp, name: &str, body: &str) -> PathBuf {
        let path = temp.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn configure_xdg(temp: &Temp) -> Option<std::ffi::OsString> {
        let old = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", temp.path());
        fs::create_dir_all(temp.path().join("opencode")).unwrap();
        fs::write(
            temp.path().join("opencode/opencode.db"),
            b"read-only fixture",
        )
        .unwrap();
        old
    }

    #[test]
    fn stable_metadata_query_uses_read_only_sql_and_canonical_row_identity() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let temp = Temp::new();
        let old_xdg = configure_xdg(&temp);
        let log = temp.path().join("requests.log");
        let fake = fake_program(
            &temp,
            "opencode-custom",
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'; printf 'row\\nrow\\t{{\"id\":\"ses_native\",\"title\":\"safe\"}}\\n'",
                log.display()
            ),
        );
        let store = OpenCodeStore::stable(fake);
        let session = store.locate_exact("ses_native").unwrap().unwrap();
        assert_eq!(session.id(), "ses_native");
        assert_eq!(session.harness(), Harness::OpenCode);
        assert_eq!(session.metadata().unwrap()["title"], "safe");
        let request = fs::read_to_string(log).unwrap();
        assert!(request.contains("WHERE id = 'ses_native'"));
        assert!(!request.contains("message"));
        restore_xdg(old_xdg);
    }

    #[test]
    fn stable_exact_query_escapes_sql_literal() {
        let id = "ses_'quoted";
        assert_eq!(sql_literal(id), "'ses_''quoted'");
    }

    #[test]
    fn xdg_data_home_falls_back_to_home_only_when_unset() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous_home = std::env::var_os("HOME");
        let previous_xdg = std::env::var_os("XDG_DATA_HOME");
        let temp = Temp::new();
        let home = temp.path().join("home");
        std::env::set_var("HOME", &home);
        std::env::remove_var("XDG_DATA_HOME");
        assert_eq!(
            database_path(),
            Some(home.join(".local/share/opencode/opencode.db"))
        );
        std::env::set_var("XDG_DATA_HOME", "");
        assert_eq!(database_path(), Some(PathBuf::from("opencode/opencode.db")));
        std::env::set_var("XDG_DATA_HOME", "relative-data");
        assert_eq!(
            database_path(),
            Some(PathBuf::from("relative-data/opencode/opencode.db"))
        );
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        restore_xdg(previous_xdg);
    }

    #[test]
    fn v2_identity_requires_matching_metadata_and_encodes_the_path_component() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let temp = Temp::new();
        let old_xdg = configure_xdg(&temp);
        let log = temp.path().join("requests.log");
        let fake = fake_program(
            &temp,
            "opencode2-fake",
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'; printf '{{\"data\":{{\"id\":\"ses_a/b?c\"}}}}\\n'",
                log.display()
            ),
        );
        let store = OpenCodeStore::v2(fake);
        let session = store.locate_exact("ses_a/b?c").unwrap().unwrap();
        assert_eq!(session.id(), "ses_a/b?c");
        let request = fs::read_to_string(log).unwrap();
        assert!(request.contains("/api/session/ses_a%2Fb%3Fc"));
        restore_xdg(old_xdg);
    }

    #[test]
    fn null_is_absence_but_malformed_non_null_identity_is_failure() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let temp = Temp::new();
        let old_xdg = configure_xdg(&temp);
        let fake_null = fake_program(&temp, "opencode2-null", "printf '{\"data\":null}\\n'");
        assert!(OpenCodeStore::v2(fake_null)
            .locate_exact("ses_missing")
            .unwrap()
            .is_none());
        let fake_bad = fake_program(&temp, "opencode2-bad", "printf '{\"data\":{\"id\":42}}\\n'");
        assert!(matches!(
            OpenCodeStore::v2(fake_bad).locate_exact("ses_bad"),
            Err(DiscoveryError::InvalidMetadata { .. })
        ));
        restore_xdg(old_xdg);
    }

    #[test]
    fn a_missing_program_or_store_is_absence_without_startup() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let temp = Temp::new();
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", temp.path().join("missing-data"));
        let missing_program = OpenCodeStore::v2("definitely-not-an-opencode-program");
        assert!(missing_program.locate_exact("ses_x").unwrap().is_none());
        assert!(missing_program.candidates(100).records.is_empty());
        restore_xdg(old_xdg);
    }

    #[test]
    fn command_deadline_kills_and_reaps_the_child() {
        let error = run_command(
            OsStr::new("/bin/sleep"),
            &[OsString::from("5")],
            "fake-opencode",
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(matches!(error, DiscoveryError::TimedOut { .. }));
    }

    #[test]
    fn deadline_covers_descendants_that_keep_the_response_pipes_open() {
        let started = Instant::now();
        let error = run_command(
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from("(sleep 5) & exit 0")],
            "fake-opencode",
            Duration::from_millis(75),
        )
        .unwrap_err();
        assert!(matches!(error, DiscoveryError::TimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn trickling_and_silent_commands_stop_at_the_deadline() {
        for body in ["printf '{'; sleep 5", "sleep 5"] {
            let error = run_command(
                OsStr::new("/bin/sh"),
                &[OsString::from("-c"), OsString::from(body)],
                "fake-opencode",
                Duration::from_millis(50),
            )
            .unwrap_err();
            assert!(matches!(error, DiscoveryError::TimedOut { .. }));
        }
    }

    #[test]
    fn oversized_stdout_is_capped_and_stderr_is_drained() {
        let oversized = run_command(
            OsStr::new("/usr/bin/head"),
            &[
                OsString::from("-c"),
                OsString::from((MAX_RESPONSE_BYTES + 1).to_string()),
                OsString::from("/dev/zero"),
            ],
            "fake-opencode",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(
            oversized,
            DiscoveryError::BoundExhausted {
                bound: "8 MiB OpenCode response",
                ..
            }
        ));

        let body = run_command(
            OsStr::new("/bin/sh"),
            &[
                OsString::from("-c"),
                OsString::from("/usr/bin/head -c 2097152 /dev/zero >&2; printf '{}'"),
            ],
            "fake-opencode",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(body, b"{}");
    }

    #[test]
    fn nonzero_command_status_is_typed_without_captured_output() {
        let error = run_command(
            OsStr::new("/bin/sh"),
            &[
                OsString::from("-c"),
                OsString::from("printf 'private output'; exit 17"),
            ],
            "fake-opencode",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            DiscoveryError::CommandFailed { ref status, .. } if status == "exit 17"
        ));
        assert!(!error.to_string().contains("private output"));
    }

    fn restore_xdg(value: Option<std::ffi::OsString>) {
        if let Some(value) = value {
            std::env::set_var("XDG_DATA_HOME", value);
        } else {
            std::env::remove_var("XDG_DATA_HOME");
        }
    }
}
