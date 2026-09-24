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

#[derive(Default)]
struct DatabaseRows {
    values: Vec<Value>,
    scanned: usize,
    complete: bool,
    failures: Vec<DiscoveryError>,
    unreadable_ids: Vec<String>,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreAuthority {
    Program,
    ExplicitStore,
    DefaultStore,
}

/// A read-only OpenCode metadata source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenCodeStore {
    program: OsString,
    flavor: OpenCodeFlavor,
    data_directory: Option<PathBuf>,
    authority: StoreAuthority,
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
        Self {
            program,
            flavor,
            data_directory: default_data_directory(),
            authority: StoreAuthority::Program,
        }
    }

    pub fn stable(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            flavor: OpenCodeFlavor::Stable,
            data_directory: default_data_directory(),
            authority: StoreAuthority::Program,
        }
    }

    pub fn v2(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            flavor: OpenCodeFlavor::V2,
            data_directory: default_data_directory(),
            authority: StoreAuthority::Program,
        }
    }

    pub fn stable_at(program: impl Into<OsString>, data_directory: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            flavor: OpenCodeFlavor::Stable,
            data_directory: Some(data_directory.into()),
            authority: StoreAuthority::ExplicitStore,
        }
    }

    pub fn v2_at(program: impl Into<OsString>, data_directory: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            flavor: OpenCodeFlavor::V2,
            data_directory: Some(data_directory.into()),
            authority: StoreAuthority::ExplicitStore,
        }
    }

    fn default_stable() -> Self {
        Self {
            program: OsString::from("opencode"),
            flavor: OpenCodeFlavor::Stable,
            data_directory: default_data_directory(),
            authority: StoreAuthority::DefaultStore,
        }
    }

    fn default_v2() -> Self {
        Self {
            program: OsString::from("opencode2"),
            flavor: OpenCodeFlavor::V2,
            data_directory: default_data_directory(),
            authority: StoreAuthority::DefaultStore,
        }
    }

    pub fn flavor(&self) -> OpenCodeFlavor {
        self.flavor
    }

    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// The data directory selected by XDG_DATA_HOME or the HOME fallback.
    pub fn data_directory(&self) -> Option<PathBuf> {
        self.data_directory.clone()
    }

    /// Resolve a database file within the configured OpenCode data directory.
    pub fn database_file(&self, name: &str) -> Option<PathBuf> {
        self.data_directory
            .as_ref()
            .map(|directory| directory.join(name))
    }

    /// Return a database file only when its read-only SQLite header is valid.
    pub fn sqlite_database_file(&self, name: &str) -> Result<Option<PathBuf>, DiscoveryError> {
        let Some(path) = self.database_file(name) else {
            return Ok(None);
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(DiscoveryError::io(
                    path.display().to_string(),
                    "inspect OpenCode database",
                    &error,
                ));
            }
        };
        if !metadata.is_file() {
            return Err(DiscoveryError::InvalidMetadata {
                coordinate: path.display().to_string(),
                reason: "OpenCode database is not a regular file",
            });
        }
        let mut file = fs::File::open(&path).map_err(|error| {
            DiscoveryError::io(path.display().to_string(), "open OpenCode database", &error)
        })?;
        let mut header = [0_u8; 16];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(DiscoveryError::InvalidMetadata {
                    coordinate: path.display().to_string(),
                    reason: "OpenCode database has an incomplete SQLite header",
                });
            }
            Err(error) => {
                return Err(DiscoveryError::io(
                    path.display().to_string(),
                    "read OpenCode database header",
                    &error,
                ));
            }
        }
        if header != *b"SQLite format 3\0" {
            return Err(DiscoveryError::InvalidMetadata {
                coordinate: path.display().to_string(),
                reason: "OpenCode database has an invalid SQLite header",
            });
        }
        Ok(Some(path))
    }

    pub fn coordinate(&self) -> String {
        match self.flavor {
            OpenCodeFlavor::Stable => self.database_file("opencode.db").map_or_else(
                || format!("{} database", self.program.to_string_lossy()),
                |path| path.display().to_string(),
            ),
            OpenCodeFlavor::V2 => format!("{} api", self.program.to_string_lossy()),
        }
    }

    fn session_coordinate(&self, id: &str) -> String {
        match self.flavor {
            OpenCodeFlavor::Stable => self.coordinate(),
            OpenCodeFlavor::V2 => format!(
                "{}:/api/session/{}",
                self.program.to_string_lossy(),
                encode_path_component(id)
            ),
        }
    }

    /// Report absence without running a harness command that could create a store.
    pub fn available(&self) -> bool {
        self.store_exists().unwrap_or(true)
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
        if self.authority == StoreAuthority::Program {
            return Ok(program_available(&self.program));
        }
        let Some(directory) = self.data_directory.as_ref() else {
            return Ok(false);
        };
        let metadata = match fs::metadata(directory) {
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
        let database_name = match self.flavor {
            OpenCodeFlavor::Stable => "opencode.db",
            OpenCodeFlavor::V2 => "opencode-next.db",
        };
        let Some(database) = self.database_file(database_name) else {
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
        if self.authority == StoreAuthority::DefaultStore {
            self.sqlite_database_file(database_name)?;
        }
        Ok(program_available(&self.program)
            || (self.flavor == OpenCodeFlavor::Stable && program_available(OsStr::new("sqlite3"))))
    }

    fn is_default_program(&self, name: &str) -> bool {
        Path::new(&self.program).components().count() == 1
            && Path::new(&self.program).file_name() == Some(OsStr::new(name))
    }

    fn captured_xdg_data_home(&self) -> Option<&OsStr> {
        if self.authority == StoreAuthority::Program
            && !self.is_default_program("opencode")
            && !self.is_default_program("opencode2")
        {
            return None;
        }
        self.data_directory
            .as_deref()
            .and_then(Path::parent)
            .map(Path::as_os_str)
    }

    fn run_program(&self, program: &OsStr, args: &[OsString]) -> Result<Vec<u8>, DiscoveryError> {
        run_command_with_xdg(
            program,
            args,
            &self.coordinate(),
            self.captured_xdg_data_home(),
            COMMAND_DEADLINE,
        )
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
        if let Some(error) = rows.failures.into_iter().next() {
            return Err(error);
        }
        match rows.values.as_slice() {
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
        let mut rows = self.database_rows(&query)?;
        if !rows.failures.is_empty() {
            let mut known = rows
                .values
                .iter()
                .filter_map(|row| row.get("id").and_then(Value::as_str))
                .chain(rows.unreadable_ids.iter().map(String::as_str))
                .map(str::to_owned)
                .collect::<std::collections::HashSet<_>>();
            for id in self.database_ids(limit)? {
                if known.insert(id.clone()) {
                    rows.unreadable_ids.push(id);
                }
            }
        }
        Ok(self.page_from_rows(rows, limit))
    }

    fn database_ids(&self, limit: usize) -> Result<Vec<String>, DiscoveryError> {
        let query = format!(
            "SELECT id AS row FROM session ORDER BY time_updated DESC LIMIT {}",
            limit.min(MAX_ROWS)
        );
        let Some(bytes) = self.database_text(&query)? else {
            return Ok(Vec::new());
        };
        let text = String::from_utf8(bytes).map_err(|_| DiscoveryError::InvalidMetadata {
            coordinate: self.coordinate(),
            reason: "OpenCode database ID response is not UTF-8",
        })?;
        parse_database_ids(&text, &self.coordinate(), limit.min(MAX_ROWS))
    }

    fn database_rows(&self, query: &str) -> Result<DatabaseRows, DiscoveryError> {
        let Some(bytes) = self.database_text(query)? else {
            return Ok(DatabaseRows {
                complete: true,
                ..DatabaseRows::default()
            });
        };
        let text = String::from_utf8(bytes).map_err(|_| DiscoveryError::InvalidMetadata {
            coordinate: self.coordinate(),
            reason: "OpenCode database response is not UTF-8",
        })?;
        parse_database_rows(&text, &self.coordinate())
    }

    fn database_text(&self, query: &str) -> Result<Option<Vec<u8>>, DiscoveryError> {
        if self.flavor == OpenCodeFlavor::Stable && self.is_default_program("opencode") {
            let Some(database) = self.sqlite_database_file("opencode.db")? else {
                if self.authority == StoreAuthority::DefaultStore {
                    return Ok(None);
                }
                return self
                    .run_program(
                        &self.program,
                        &[
                            OsString::from("db"),
                            OsString::from("--format"),
                            OsString::from("tsv"),
                            OsString::from(query),
                        ],
                    )
                    .map(Some);
            };
            if program_available(OsStr::new("sqlite3")) {
                let database =
                    database
                        .to_str()
                        .ok_or_else(|| DiscoveryError::InvalidMetadata {
                            coordinate: self.coordinate(),
                            reason: "OpenCode database path is not UTF-8",
                        })?;
                return run_command_with_xdg(
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
                    self.captured_xdg_data_home(),
                    COMMAND_DEADLINE,
                )
                .map(Some);
            }
        }
        self.run_program(
            &self.program,
            &[
                OsString::from("db"),
                OsString::from("--format"),
                OsString::from("tsv"),
                OsString::from(query),
            ],
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
        let bytes = self.run_program(
            &self.program,
            &[
                OsString::from("api"),
                OsString::from("--standalone"),
                OsString::from("get"),
                OsString::from(path),
            ],
        )?;
        serde_json::from_slice(&bytes).map_err(|_| DiscoveryError::InvalidMetadata {
            coordinate: self.coordinate(),
            reason: "OpenCode API response is malformed JSON",
        })
    }

    fn page_from_rows(&self, rows: DatabaseRows, limit: usize) -> CandidatePage {
        let values = rows.values;
        let mut page = CandidatePage {
            records: Vec::new(),
            scanned: rows.scanned,
            visited_entries: 0,
            complete: rows.complete && rows.scanned < limit,
            failures: rows.failures,
            unreadable_ids: rows.unreadable_ids,
        };
        self.retain_rows(values.iter().take(limit), &mut page);
        if rows.scanned > limit {
            page.complete = false;
            page.failures.push(DiscoveryError::BoundExhausted {
                coordinate: self.coordinate(),
                bound: "requested OpenCode candidate limit",
            });
        } else if rows.scanned == limit && limit == MAX_ROWS {
            page.complete = false;
        }
        page
    }

    fn retain_rows<'a>(&self, rows: impl Iterator<Item = &'a Value>, page: &mut CandidatePage) {
        let mut retained = page
            .failures
            .iter()
            .map(|failure| failure.to_string().len() + size_of::<DiscoveryError>())
            .sum::<usize>()
            + page
                .unreadable_ids
                .iter()
                .map(|id| id.len() + size_of::<String>())
                .sum::<usize>();
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
            .filter(|id| !id.is_empty() && !id.bytes().any(|byte| byte.is_ascii_control()))
            .ok_or_else(|| DiscoveryError::InvalidMetadata {
                coordinate: self.coordinate(),
                reason: "OpenCode session metadata has no valid ID",
            })?
            .to_owned();
        let store = self.session_coordinate(&id);
        Ok(NativeSession::opencode(id, store, self.flavor, metadata))
    }
}

pub(crate) fn default_stores() -> Vec<OpenCodeStore> {
    let mut stores = vec![OpenCodeStore::default_stable()];
    if program_available(OsStr::new("opencode2")) {
        stores.push(OpenCodeStore::default_v2());
    }
    stores
}

fn default_data_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|path| path.join("opencode"))
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

fn parse_database_rows(text: &str, coordinate: &str) -> Result<DatabaseRows, DiscoveryError> {
    if text.trim().is_empty() {
        return Ok(DatabaseRows {
            complete: true,
            ..DatabaseRows::default()
        });
    }
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
    let mut rows = DatabaseRows {
        complete: true,
        ..DatabaseRows::default()
    };
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let json = line
            .strip_prefix("row\t")
            .or_else(|| line.strip_prefix("row "))
            .unwrap_or(line)
            .trim_end_matches('\r');
        if rows.scanned >= MAX_ROWS {
            rows.complete = false;
            rows.failures.push(DiscoveryError::BoundExhausted {
                coordinate: coordinate.to_owned(),
                bound: "1,000 OpenCode metadata rows",
            });
            break;
        }
        rows.scanned += 1;
        match serde_json::from_str::<Value>(json) {
            Ok(row) => rows.values.push(row),
            Err(_) => {
                if let Some(id) = malformed_database_id(json) {
                    rows.unreadable_ids.push(id);
                }
                rows.complete = false;
                rows.failures.push(DiscoveryError::InvalidMetadata {
                    coordinate: coordinate.to_owned(),
                    reason: "OpenCode database returned malformed row metadata",
                });
            }
        }
    }
    Ok(rows)
}

fn parse_database_ids(
    text: &str,
    coordinate: &str,
    limit: usize,
) -> Result<Vec<String>, DiscoveryError> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut lines = text.lines();
    if lines.next().map(|header| header.trim_end_matches('\r')) != Some("row") {
        return Err(DiscoveryError::InvalidMetadata {
            coordinate: coordinate.to_owned(),
            reason: "OpenCode database ID response has an unexpected header",
        });
    }
    let mut ids = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let cell = line
            .strip_prefix("row\t")
            .or_else(|| line.strip_prefix("row "))
            .unwrap_or(line)
            .trim_end_matches('\r');
        let id = decode_tsv_cell(cell).ok_or_else(|| DiscoveryError::InvalidMetadata {
            coordinate: coordinate.to_owned(),
            reason: "OpenCode database returned an invalid session ID cell",
        })?;
        if id.is_empty() || id.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(DiscoveryError::InvalidMetadata {
                coordinate: coordinate.to_owned(),
                reason: "OpenCode database returned an empty or invalid session ID",
            });
        }
        ids.push(id);
        if ids.len() > limit {
            return Err(DiscoveryError::BoundExhausted {
                coordinate: coordinate.to_owned(),
                bound: "1,000 OpenCode metadata rows",
            });
        }
    }
    Ok(ids)
}

fn decode_tsv_cell(cell: &str) -> Option<String> {
    if !cell.starts_with('"') {
        return Some(cell.to_owned());
    }
    if !cell.ends_with('"') || cell.len() < 2 {
        return None;
    }
    let mut decoded = String::with_capacity(cell.len() - 2);
    let mut bytes = cell[1..cell.len() - 1].bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte == b'"' {
            bytes.next_if_eq(&b'"')?;
        }
        decoded.push(byte as char);
    }
    Some(decoded)
}

fn malformed_database_id(line: &str) -> Option<String> {
    let key_end = line.find("\"id\"")? + "\"id\"".len();
    let remainder = line[key_end..].trim_start().strip_prefix(':')?.trim_start();
    let value = remainder.strip_prefix('"')?;
    let end = value.find('"')?;
    (!value[..end].is_empty() && !value[..end].bytes().any(|byte| byte.is_ascii_control()))
        .then(|| value[..end].to_owned())
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

#[cfg(test)]
fn run_command(
    program: &OsStr,
    args: &[OsString],
    coordinate: &str,
    deadline_after: Duration,
) -> Result<Vec<u8>, DiscoveryError> {
    run_command_with_xdg(program, args, coordinate, None, deadline_after)
}

fn run_command_with_xdg(
    program: &OsStr,
    args: &[OsString],
    coordinate: &str,
    xdg_data_home: Option<&OsStr>,
    deadline_after: Duration,
) -> Result<Vec<u8>, DiscoveryError> {
    let deadline = Instant::now() + deadline_after;
    let mut command = Command::new(program);
    if let Some(xdg_data_home) = xdg_data_home {
        command.env("XDG_DATA_HOME", xdg_data_home);
    }
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
        .map(|code| format!("exited with exit status: {code}"))
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

    fn path_with_first(first: &Path) -> Option<std::ffi::OsString> {
        let previous = std::env::var_os("PATH");
        let mut paths = vec![first.to_path_buf()];
        if let Some(previous) = previous.as_ref() {
            paths.extend(std::env::split_paths(previous));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        previous
    }

    fn restore_path(previous: Option<std::ffi::OsString>) {
        if let Some(value) = previous {
            std::env::set_var("PATH", value);
        } else {
            std::env::remove_var("PATH");
        }
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
    fn stable_listing_keeps_valid_rows_and_recovers_ids_for_malformed_rows() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let temp = Temp::new();
        let old_xdg = configure_xdg(&temp);
        let log = temp.path().join("requests.log");
        let fake = fake_program(
            &temp,
            "opencode-custom-rows",
            &format!(
                "printf '%s\\n' \"$4\" >> '{}'; case \"$4\" in *'SELECT id AS row FROM session'*) printf 'row\\nses_readable\\nses_hidden\\n' ;; *'ORDER BY time_updated DESC LIMIT'*) printf 'row\\nrow\\t{{\"id\":\"ses_readable\",\"title\":\"safe\"}}\\nrow\\t{{\"id\":\"ses_hidden\",\"title\":\"broken\\n' ;; *) printf 'row\\nrow\\t{{\"id\":\"ses_hidden\",\"title\":\"broken\\n' ;; esac",
                log.display()
            ),
        );
        let page = OpenCodeStore::stable(fake).candidates(10);
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].id(), "ses_readable");
        assert_eq!(page.unreadable_ids, vec!["ses_hidden"]);
        assert!(!page.complete);
        assert!(page
            .failures
            .iter()
            .any(|failure| failure.to_string().contains("malformed row metadata")));
        let requests = fs::read_to_string(log).unwrap();
        assert!(requests.contains("SELECT id AS row FROM session ORDER BY"));
        assert!(!requests.contains("message"));
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
            OpenCodeStore::stable("opencode")
                .data_directory()
                .map(|directory| directory.join("opencode.db")),
            Some(home.join(".local/share/opencode/opencode.db"))
        );
        std::env::set_var("XDG_DATA_HOME", "");
        assert_eq!(
            OpenCodeStore::stable("opencode")
                .data_directory()
                .map(|directory| directory.join("opencode.db")),
            Some(PathBuf::from("opencode/opencode.db"))
        );
        std::env::set_var("XDG_DATA_HOME", "relative-data");
        assert_eq!(
            OpenCodeStore::stable("opencode")
                .data_directory()
                .map(|directory| directory.join("opencode.db")),
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
    fn opencode_store_captures_its_data_root_at_construction() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("XDG_DATA_HOME");
        let first = Temp::new();
        let second = Temp::new();
        std::env::set_var("XDG_DATA_HOME", first.path());
        let store = OpenCodeStore::default_stable();
        std::env::set_var("XDG_DATA_HOME", second.path());
        assert_eq!(store.data_directory(), Some(first.path().join("opencode")));
        restore_xdg(previous);
    }

    #[test]
    fn explicit_custom_v2_program_does_not_require_the_default_data_root() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("XDG_DATA_HOME");
        let temp = Temp::new();
        std::env::set_var("XDG_DATA_HOME", temp.path().join("absent-data"));
        let program = fake_program(
            &temp,
            "opencode2-custom",
            "printf '{\"data\":{\"id\":\"ses_native\"}}\\n'",
        );
        let store = OpenCodeStore::v2(program);
        let session = store.locate_exact("ses_native").unwrap().unwrap();
        assert_eq!(session.id(), "ses_native");
        restore_xdg(previous);
    }

    #[test]
    fn default_v2_command_uses_the_data_root_captured_at_construction() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let temp = Temp::new();
        let first = temp.path().join("first-data");
        let second = temp.path().join("second-data");
        fs::create_dir_all(first.join("opencode")).unwrap();
        fs::write(
            first.join("opencode/opencode-next.db"),
            b"SQLite format 3\0",
        )
        .unwrap();
        let log = temp.path().join("xdg-seen");
        fake_program(
            &temp,
            "opencode2",
            &format!(
                "printf '%s' \"$XDG_DATA_HOME\" > '{}'; printf '{{\"data\":{{\"id\":\"ses_native\"}}}}\\n'",
                log.display()
            ),
        );
        std::env::set_var("XDG_DATA_HOME", &first);
        let old_path = path_with_first(temp.path());
        let v2 = default_stores()
            .into_iter()
            .find(|store| store.flavor() == OpenCodeFlavor::V2)
            .unwrap();
        std::env::set_var("XDG_DATA_HOME", &second);

        assert_eq!(
            v2.locate_exact("ses_native").unwrap().unwrap().id(),
            "ses_native"
        );
        assert_eq!(fs::read_to_string(log).unwrap(), first.to_string_lossy());

        restore_path(old_path);
        restore_xdg(old_xdg);
    }

    #[test]
    fn default_v2_program_is_not_invoked_without_its_store_file() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let temp = Temp::new();
        let data = temp.path().join("data");
        fs::create_dir_all(data.join("opencode")).unwrap();
        std::env::set_var("XDG_DATA_HOME", &data);
        let marker = temp.path().join("v2-invoked");
        fake_program(
            &temp,
            "opencode2",
            &format!(
                "printf invoked > '{}'; printf '{{\"data\":{{\"id\":\"ses_native\"}}}}\\n'",
                marker.display()
            ),
        );
        let test_path = path_with_first(temp.path());
        let v2 = default_stores()
            .into_iter()
            .find(|store| store.flavor() == OpenCodeFlavor::V2)
            .unwrap();
        assert!(v2.locate_exact("ses_native").unwrap().is_none());
        assert!(!marker.exists());

        fs::write(data.join("opencode/opencode-next.db"), b"SQLite format 3\0").unwrap();
        assert_eq!(
            v2.locate_exact("ses_native").unwrap().unwrap().id(),
            "ses_native"
        );
        assert!(marker.exists());
        restore_path(test_path);
        restore_xdg(old_xdg);
    }

    #[test]
    fn empty_stable_query_is_absence_and_does_not_block_a_v2_exact_hit() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let temp = Temp::new();
        std::env::set_var("XDG_DATA_HOME", temp.path().join("absent-data"));
        let stable = fake_program(&temp, "stable-empty", "exit 0");
        let v2 = fake_program(
            &temp,
            "v2-hit",
            "printf '{\"data\":{\"id\":\"ses_v2-only\"}}\\n'",
        );
        let discovery = crate::Discovery::new([
            crate::NativeStore::opencode_stable(stable),
            crate::NativeStore::opencode_v2(v2),
        ]);
        assert_eq!(
            discovery.resolve("ses_v2-only").unwrap().id(),
            "ses_v2-only"
        );
        restore_xdg(old_xdg);
    }

    #[test]
    fn malformed_present_stable_database_does_not_fall_back_to_program() {
        let _lock = XDG_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        let temp = Temp::new();
        let data = temp.path().join("data");
        fs::create_dir_all(data.join("opencode")).unwrap();
        fs::write(data.join("opencode/opencode.db"), b"not sqlite").unwrap();
        std::env::set_var("XDG_DATA_HOME", &data);
        let marker = temp.path().join("opencode-invoked");
        fake_program(
            &temp,
            "opencode",
            &format!("printf invoked > '{}'", marker.display()),
        );
        let path = path_with_first(temp.path());
        let store = OpenCodeStore::stable("opencode");
        for (header, reason) in [
            (
                b"not sqlite".as_slice(),
                "OpenCode database has an incomplete SQLite header",
            ),
            (
                b"not a sqlite db!".as_slice(),
                "OpenCode database has an invalid SQLite header",
            ),
        ] {
            fs::write(data.join("opencode/opencode.db"), header).unwrap();
            assert!(matches!(
                store.locate_exact("ses_any"),
                Err(DiscoveryError::InvalidMetadata {
                    reason: actual,
                    ..
                }) if actual == reason
            ));
        }
        assert!(!marker.exists());
        restore_path(path);
        restore_xdg(old_xdg);
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
            DiscoveryError::CommandFailed { ref status, .. }
                if status == "exited with exit status: 17"
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
