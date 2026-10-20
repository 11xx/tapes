//! An OpenCode read under concurrent readers either succeeds or names its
//! failure as transient. A database another connection holds busy or locked,
//! or an API that answers that it is momentarily unavailable, is read again
//! within a small bound; a read that still fails says it failed transiently,
//! never that the session holds nothing, and a failure that cannot pass is
//! neither retried nor called transient.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use agent_tapes_core::backend::{backends_from_stores, Backend, Query, MIN_READ_BYTES};
use agent_tapes_core::transient::is_transient;
use agent_tapes_core::{
    resolve_session, usage_with_backends, NativeStore, ResolveError, Selection,
};
use agent_tapes_discovery::transient::ATTEMPTS;
use serde_json::Value;

/// The session the fixture database holds.
const DATABASE_SESSION: &str = "ses_database_only_fixture";
/// The session the fixture API holds.
const API_SESSION: &str = "ses_000000fixtureSharedSession";
/// Text every fixture API transcript holds.
const API_NEEDLE: &str = "Inspect the fixture";

/// SQLite's message for a busy database, as the sqlite3 shell prints it.
const SQLITE_BUSY: &str = "Parse error in 6th command line argument: database is locked (5)";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode")
}

/// A usage report without the store's location, which names each store's own
/// temporary directory.
fn located_nowhere(usage: agent_tapes_core::usage::UsageView) -> Value {
    let mut usage = serde_json::to_value(usage).unwrap();
    usage["session"]["source"]
        .as_object_mut()
        .unwrap()
        .remove("location");
    usage
}

fn write_program(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// A directory on `PATH` holding `opencode`, the fixture program answering
/// database queries, and `sqlite3`, which answers busy or fails for as many
/// queries of a kind as the database's directory says and passes every
/// other query to `opencode db`. Each query it receives is logged by kind.
fn database_programs() {
    static PROGRAMS: OnceLock<()> = OnceLock::new();
    PROGRAMS.get_or_init(|| {
        let bin = support::process_root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(fixtures().join("opencode2"), bin.join("opencode")).unwrap();
        write_program(
            &bin.join("sqlite3"),
            &format!(
                r#"#!/bin/sh
for argument; do database=$query; query=$argument; done
directory=${{database%/*}}
kind=other
case $query in
  *"FROM session"*) kind=session ;;
  *"FROM message"*) kind=message ;;
esac
printf '%s\n' "$kind" >> "$directory/calls"
if [ -f "$directory/busy-$kind" ]; then
  left=$(cat "$directory/busy-$kind")
  if [ "$left" -gt 0 ]; then
    echo $((left - 1)) > "$directory/busy-$kind"
    echo '{SQLITE_BUSY}' >&2
    exit 1
  fi
fi
if [ -f "$directory/fail-$kind" ]; then
  echo "Parse error in 6th command line argument: no such table: $kind" >&2
  exit 1
fi
exec "${{0%/*}}/opencode" db --format tsv "$query"
"#
            ),
        );
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut entries = vec![bin];
        entries.extend(std::env::split_paths(&path));
        std::env::set_var("PATH", std::env::join_paths(entries).unwrap());
    });
}

/// A stable OpenCode store read through `sqlite3` on `PATH`.
struct DatabaseStore {
    root: tempfile::TempDir,
}

impl DatabaseStore {
    fn new() -> Self {
        database_programs();
        let root = tempfile::TempDir::with_prefix("tapes-opencode-transient-").unwrap();
        fs::create_dir_all(root.path().join("opencode")).unwrap();
        fs::write(
            root.path().join("opencode/opencode.db"),
            b"SQLite format 3\0",
        )
        .unwrap();
        Self { root }
    }

    fn directory(&self) -> PathBuf {
        self.root.path().join("opencode")
    }

    /// The next `count` queries of `kind` answer that the database is locked.
    fn busy(self, kind: &str, count: usize) -> Self {
        fs::write(
            self.directory().join(format!("busy-{kind}")),
            count.to_string(),
        )
        .unwrap();
        self
    }

    /// Every query of `kind` fails for a reason that cannot pass.
    fn failing(self, kind: &str) -> Self {
        fs::write(self.directory().join(format!("fail-{kind}")), "").unwrap();
        self
    }

    fn calls(&self, kind: &str) -> usize {
        fs::read_to_string(self.directory().join("calls"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == kind)
            .count()
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        backends_from_stores(
            [NativeStore::opencode_stable_at(
                "opencode",
                self.directory(),
            )],
            MIN_READ_BYTES,
        )
        .unwrap()
    }

    fn usage(&self) -> anyhow::Result<Value> {
        usage_with_backends(&self.backends(), Selection::Id(DATABASE_SESSION)).map(located_nowhere)
    }
}

/// A v2 OpenCode store whose program is the fixture behind a wrapper. Its
/// `api` command answers that the database is locked, and its `serve` API
/// answers HTTP 503, for as many message reads as its directory says.
struct ApiStore {
    root: tempfile::TempDir,
}

impl ApiStore {
    fn new() -> Self {
        let root = tempfile::TempDir::with_prefix("tapes-opencode-transient-api-").unwrap();
        fs::create_dir_all(root.path().join("opencode")).unwrap();
        fs::write(root.path().join("opencode/opencode-next.db"), b"").unwrap();
        let fixture = fixtures().join("opencode2");
        let state = root.path();
        write_program(
            &root.path().join("opencode2"),
            &format!(
                r#"#!/bin/sh
state='{state}'
fixture='{fixture}'
if [ "${{1:-}}" = serve ]; then
  exec /usr/bin/python3 -I - "$state" "$fixture" "$@" <<'PYTHON'
import http.server
import subprocess
import sys

state, fixture = sys.argv[1], sys.argv[2]

def take(name):
    path = f"{{state}}/{{name}}"
    try:
        left = int(open(path).read())
    except FileNotFoundError:
        return False
    if left <= 0:
        return False
    open(path, "w").write(str(left - 1))
    return True

class Api(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if "/message" in self.path and take("unavailable"):
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        answer = subprocess.run(
            [fixture, "api", "--standalone", "get", self.path], capture_output=True
        )
        self.send_response(200 if answer.returncode == 0 else 500)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(answer.stdout)))
        self.end_headers()
        self.wfile.write(answer.stdout)

    def log_message(self, *args):
        pass

port = int(sys.argv[sys.argv.index("--port") + 1])
http.server.HTTPServer(("127.0.0.1", port), Api).serve_forever()
PYTHON
fi
case ${{4:-}} in
  */message*)
    left=$(cat "$state/busy" 2>/dev/null || echo 0)
    if [ "$left" -gt 0 ]; then
      echo $((left - 1)) > "$state/busy"
      echo 'SQLiteError: database is locked' >&2
      exit 1
    fi
    ;;
esac
exec "$fixture" "$@"
"#,
                state = state.display(),
                fixture = fixture.display(),
            ),
        );
        Self { root }
    }

    /// The next `count` message reads through the `api` command answer that
    /// the database is locked.
    fn busy(self, count: usize) -> Self {
        fs::write(self.root.path().join("busy"), count.to_string()).unwrap();
        self
    }

    /// The next `count` message requests to the served API answer HTTP 503.
    fn unavailable(self, count: usize) -> Self {
        fs::write(self.root.path().join("unavailable"), count.to_string()).unwrap();
        self
    }

    fn backends(&self) -> Vec<Box<dyn Backend>> {
        backends_from_stores(
            [NativeStore::opencode_v2_at(
                self.root.path().join("opencode2"),
                self.root.path().join("opencode"),
            )],
            MIN_READ_BYTES,
        )
        .unwrap()
    }

    fn usage(&self) -> anyhow::Result<Value> {
        usage_with_backends(&self.backends(), Selection::Id(API_SESSION)).map(located_nowhere)
    }
}

/// A transcript read whose database answers busy, then answers, measures the
/// session as a read that never met the busy database does. Rejects a read
/// that gives up at the first busy answer and fails the session's usage.
#[test]
fn a_transcript_read_the_database_answers_busy_then_answers_measures_the_session() {
    let clean = DatabaseStore::new().usage().unwrap();
    let store = DatabaseStore::new().busy("message", ATTEMPTS - 1);
    let usage = store.usage().unwrap();
    assert_eq!(usage, clean);
}

/// A session lookup whose database answers busy, then answers, resolves the
/// session. Rejects a lookup that gives up at the first busy answer and
/// reports the session unresolvable.
#[test]
fn a_session_lookup_the_database_answers_busy_then_answers_resolves_the_session() {
    let clean = DatabaseStore::new().usage().unwrap();
    let store = DatabaseStore::new().busy("session", ATTEMPTS - 1);
    let usage = store.usage().unwrap();
    assert_eq!(usage, clean);
}

/// A transcript read the database keeps busy through every attempt fails as
/// transient, saying so, after exactly the bounded number of attempts.
/// Rejects a failure that reads like any other, and a retry without bound.
#[test]
fn a_transcript_read_still_busy_after_every_attempt_fails_as_transient() {
    let store = DatabaseStore::new().busy("message", 1_000);
    let error = store.usage().unwrap_err();
    assert!(is_transient(&error), "not transient: {error:#}");
    let message = format!("{error:#}");
    assert!(message.contains("transient"), "{message}");
    assert!(message.contains("database is locked"), "{message}");
    assert_eq!(store.calls("message"), ATTEMPTS);
}

/// A session lookup the database keeps busy through every attempt fails to
/// resolve as transient. Rejects a lookup failure that reads as an
/// incomplete, failed, or missing session.
#[test]
fn a_session_lookup_still_busy_after_every_attempt_resolves_as_transient() {
    let store = DatabaseStore::new().busy("session", 1_000);
    let error = match resolve_session(&store.backends(), DATABASE_SESSION) {
        Ok(resolved) => panic!("resolved {}", resolved.session.id),
        Err(error) => error,
    };
    assert!(
        matches!(error, ResolveError::Transient { .. }),
        "not transient: {error}"
    );
    assert!(error.to_string().contains("transiently"), "{error}");
    let error = store.usage().unwrap_err();
    assert!(is_transient(&error), "not transient: {error:#}");
}

/// A read whose database fails for a reason that cannot pass is attempted
/// once and is not transient. Rejects a reader that retries, or calls
/// transient, every failure.
#[test]
fn a_read_failure_that_cannot_pass_is_neither_retried_nor_transient() {
    let store = DatabaseStore::new().failing("message");
    let error = store.usage().unwrap_err();
    assert!(!is_transient(&error), "transient: {error:#}");
    assert!(!format!("{error:#}").contains("transient"), "{error:#}");
    assert_eq!(store.calls("message"), 1);
}

/// A v2 transcript read whose `api` command answers that the database is
/// locked, then answers, measures the session as a read that never met the
/// busy database does; one still busy after every attempt is transient.
/// Rejects an API read that gives up at the first busy answer.
#[test]
fn a_v2_transcript_read_the_api_command_answers_busy_then_answers_measures_the_session() {
    let clean = ApiStore::new().usage().unwrap();
    let usage = ApiStore::new().busy(ATTEMPTS - 1).usage().unwrap();
    assert_eq!(usage, clean);

    let error = ApiStore::new().busy(1_000).usage().unwrap_err();
    assert!(is_transient(&error), "not transient: {error:#}");
}

/// A v2 content search whose served API answers HTTP 503, then answers,
/// searches every session; one still unavailable after every attempt names
/// the session unsearched for a transient failure. Rejects a search that
/// gives up at the first 503, and an unsearched session that does not say
/// its failure may pass.
#[test]
fn a_v2_search_the_api_answers_unavailable_then_answers_searches_every_session() {
    let query = Query::unscoped(50);
    let clean = ApiStore::new().backends()[0]
        .list_with_search(&query, API_NEEDLE, 50)
        .unwrap();
    assert!(clean.unsearched.is_empty(), "{:?}", clean.unsearched);
    assert!(!clean.sessions.is_empty());

    let store = ApiStore::new().unavailable(ATTEMPTS - 1);
    let listing = store.backends()[0]
        .list_with_search(&query, API_NEEDLE, 50)
        .unwrap();
    assert!(listing.unsearched.is_empty(), "{:?}", listing.unsearched);
    let ids = |listing: &agent_tapes_core::backend::Listing| {
        listing
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&listing), ids(&clean));

    let store = ApiStore::new().unavailable(1_000);
    let listing = store.backends()[0]
        .list_with_search(&query, API_NEEDLE, 50)
        .unwrap();
    assert!(!listing.unsearched.is_empty());
    for unsearched in &listing.unsearched {
        assert!(unsearched.contains("transient"), "{unsearched}");
        assert!(unsearched.contains("HTTP 503"), "{unsearched}");
    }
}
