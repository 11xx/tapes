//! A command that prices refreshes the fetched price table when it is due,
//! prices with it by default, and never fails because the refresh did.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Utc};
use serde_json::Value;

const SESSION: &str = "30000000-0000-4000-8000-000000000001";

fn core_fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tapes-core/tests/fixtures")
        .join(name)
}

/// A Claude store holding one session served by a model the fetched source
/// fixtures price, with empty config and data homes.
struct Store(tempfile::TempDir);

impl Store {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("projects/-fixtures-resumed-project");
        fs::create_dir_all(&project).unwrap();
        let recording = fs::read_to_string(
            core_fixtures("claude-resumed/project").join(format!("{SESSION}.jsonl")),
        )
        .unwrap()
        .replace("\"claude-fixture\"", "\"claude-haiku-5-5\"");
        fs::write(project.join(format!("{SESSION}.jsonl")), recording).unwrap();
        Self(root)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.0.path().join(relative)
    }

    fn table(&self) -> PathBuf {
        self.path("data/tapes/prices.toml")
    }

    fn record(&self) -> PathBuf {
        self.path("data/tapes/prices-fetch.json")
    }

    /// `tapes` with `arguments` over this store, refreshing from `from`,
    /// whatever its exit status.
    fn tapes(&self, arguments: &[&str], from: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env_remove("TAPES_PRICES")
            .env_remove("TAPES_OFFLINE")
            .env_remove("PI_SESSION_FILE")
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("TAPES_PRICES_FROM", from)
            .args(arguments)
            .args(["--json", "--native-store", "claude", "--native-root"])
            .arg(self.0.path())
            .output()
            .unwrap()
    }

    fn usage(&self, arguments: &[&str], from: &Path, environment: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tapes"));
        command
            .env_remove("TAPES_PRICES")
            .env_remove("TAPES_OFFLINE")
            .env_remove("PI_SESSION_FILE")
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("TAPES_PRICES_FROM", from)
            .arg("usage")
            .arg(SESSION)
            .args(arguments)
            .args(["--json", "--native-store", "claude", "--native-root"])
            .arg(self.0.path());
        for (name, value) in environment {
            command.env(name, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn sources() -> PathBuf {
    core_fixtures("prices/sources")
}

fn answer(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn usage_fetches_the_table_once_and_prices_with_it() {
    // Claim: with no table anywhere, usage refreshes the data table from
    // TAPES_PRICES_FROM, prices the session with it, and a second run reads
    // it without fetching again. Rejects no default table, and a refresh on
    // every run.
    let store = Store::new();
    let first = store.usage(&[], &sources(), &[]);
    assert!(store.table().is_file());
    assert!(String::from_utf8_lossy(&first.stderr).is_empty());
    let estimate = &answer(&first)["list_price_estimate"];
    assert!(estimate["amount"].is_number(), "{estimate}");
    // Two hours back: fresh, and past the hour a stale table waits, so only
    // freshness keeps the second run from fetching.
    let mut record: Value = serde_json::from_slice(&fs::read(store.record()).unwrap()).unwrap();
    let earlier = |at: &Value| {
        let at = at.as_str().unwrap().parse::<DateTime<Utc>>().unwrap();
        Value::from((at - chrono::Duration::hours(2)).to_rfc3339())
    };
    record["attempted_at"] = earlier(&record["attempted_at"]);
    for source in record["sources"].as_object_mut().unwrap().values_mut() {
        source["fetched_at"] = earlier(&source["fetched_at"]);
    }
    let record = serde_json::to_vec(&record).unwrap();
    fs::write(store.record(), &record).unwrap();
    let second = store.usage(&[], &sources(), &[]);
    assert!(answer(&second)["list_price_estimate"]["amount"].is_number());
    assert_eq!(fs::read(store.record()).unwrap(), record);
}

#[test]
fn offline_leaves_no_table_and_no_estimate() {
    // Claim: TAPES_OFFLINE=1 and --offline skip the refresh, so with no
    // table there is none written and no estimate. Rejects a refresh despite
    // either.
    let store = Store::new();
    let flagged = store.usage(&["--offline"], &sources(), &[]);
    let environment = store.usage(&[], &sources(), &[("TAPES_OFFLINE", "1")]);
    for output in [flagged, environment] {
        assert!(answer(&output).get("list_price_estimate").is_none());
    }
    assert!(!store.path("data").exists());
}

#[test]
fn a_named_table_neither_refreshes_nor_merges() {
    // Claim: --prices FILE is used alone: no data table is written, and a
    // config table's rows do not price beside it. Rejects a refresh despite
    // --prices, and a merge into a named table.
    let store = Store::new();
    let named = store.path("named.toml");
    fs::write(
        &named,
        "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n",
    )
    .unwrap();
    fs::create_dir_all(store.path("config/tapes")).unwrap();
    fs::copy(
        core_fixtures("prices/prices.toml"),
        store.path("config/tapes/prices.toml"),
    )
    .unwrap();
    fs::write(
        store.path("config/tapes/prices.toml"),
        fs::read_to_string(store.path("config/tapes/prices.toml"))
            .unwrap()
            .replace("\"claude-fixture\"", "\"claude-haiku-5-5\""),
    )
    .unwrap();
    let output = store.usage(&["--prices", named.to_str().unwrap()], &sources(), &[]);
    assert!(!store.path("data").exists());
    let estimate = &answer(&output)["list_price_estimate"];
    assert!(estimate.get("amount").is_none(), "{estimate}");
}

#[test]
fn a_failing_refresh_warns_once_and_the_command_succeeds() {
    // Claim: when every source fails, usage still answers and exits 0, with
    // one warning naming the failures and that no table was fetched.
    // Rejects a failing refresh failing the command, or a warning per source.
    let store = Store::new();
    let empty = tempfile::tempdir().unwrap();
    let output = store.usage(&[], empty.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.starts_with("warning: prices not refreshed (anthropic: "));
    assert!(
        stderr.trim_end().ends_with("; no fetched table"),
        "{stderr}"
    );
}

#[test]
fn a_partial_refresh_names_only_the_source_that_failed() {
    // Claim: when some sources refresh and one fails, the one warning says
    // the prices were refreshed except that source, and the session is
    // priced. Rejects saying nothing was refreshed when most of it was.
    let store = Store::new();
    let partial = tempfile::tempdir().unwrap();
    for name in ["anthropic.md", "litellm.json"] {
        fs::copy(sources().join(name), partial.path().join(name)).unwrap();
    }
    let output = store.usage(&[], partial.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("warning: prices refreshed, except openai: "),
        "{stderr}"
    );
    assert!(answer(&output)["list_price_estimate"]["amount"].is_number());
}

#[test]
fn commands_that_never_price_do_not_refresh() {
    // Claim: `stats` over a selection, and `usage --allowance-rates` and
    // `usage --forecast` over one, price nothing, so a due refresh writes no
    // table there, whether or not the command then finds an answer; `usage`
    // of one session still refreshes. Rejects a network
    // fetch and a data-directory write for a command that never prices.
    let store = Store::new();
    let mut refreshed = Vec::new();
    for arguments in [
        &["stats", "--global"][..],
        &[
            "usage",
            "--global",
            "--harness",
            "claude",
            "--allowance-rates",
            "--since",
            "2020-01-01",
        ],
        &[
            "usage",
            "--global",
            "--harness",
            "claude",
            "--forecast",
            "claude-haiku-5-5=1",
            "--since",
            "2020-01-01",
        ],
    ] {
        let output = store.tapes(arguments, &sources());
        assert!(
            output.status.success() || arguments.contains(&"--forecast"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if store.path("data").exists() {
            refreshed.push(arguments.join(" "));
            fs::remove_dir_all(store.path("data")).unwrap();
        }
    }
    assert!(refreshed.is_empty(), "{refreshed:?} refreshed");
    assert!(store
        .tapes(&["usage", SESSION], &sources())
        .status
        .success());
    assert!(store.table().is_file());
}

#[test]
fn a_refresh_another_process_holds_the_lock_for_is_skipped_silently() {
    // Claim: while another process holds `prices.lock` and a data table
    // exists, a due refresh is skipped without a write or a warning, and
    // once the lock is released the next command refreshes. Rejects two
    // processes fetching at once and the later one overwriting the earlier
    // one's table and record.
    let store = Store::new();
    fs::create_dir_all(store.path("data/tapes")).unwrap();
    let table = "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n";
    fs::write(store.table(), table).unwrap();
    let lock = fs::File::create(store.path("data/tapes/prices.lock")).unwrap();
    lock.lock().unwrap();
    let held = store.usage(&[], &sources(), &[]);
    assert_eq!(fs::read_to_string(store.table()).unwrap(), table);
    assert!(!store.record().exists());
    assert!(String::from_utf8_lossy(&held.stderr).is_empty());
    lock.unlock().unwrap();
    store.usage(&[], &sources(), &[]);
    assert!(store.record().is_file());
    assert_ne!(fs::read_to_string(store.table()).unwrap(), table);
}

#[test]
fn without_a_table_a_command_waits_for_the_refresh_another_process_holds() {
    // Claim: with no data table, a command that finds `prices.lock` held
    // waits for its holder, decides again, and prices with the table the
    // holder wrote without fetching itself. Rejects skipping the refresh and
    // answering with no estimate on a fresh install.
    let written = Store::new();
    written.usage(&[], &sources(), &[]);
    let store = Store::new();
    fs::create_dir_all(store.path("data/tapes")).unwrap();
    let lock = fs::File::create(store.path("data/tapes/prices.lock")).unwrap();
    lock.lock().unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .env_remove("TAPES_PRICES")
        .env_remove("TAPES_OFFLINE")
        .env_remove("PI_SESSION_FILE")
        .env("HOME", store.path("home"))
        .env("XDG_CONFIG_HOME", store.path("config"))
        .env("XDG_DATA_HOME", store.path("data"))
        .env("TAPES_PRICES_FROM", sources())
        .args([
            "usage",
            SESSION,
            "--json",
            "--native-store",
            "claude",
            "--native-root",
        ])
        .arg(store.0.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    fs::copy(written.table(), store.table()).unwrap();
    let record = fs::read(written.record()).unwrap();
    fs::write(store.record(), &record).unwrap();
    lock.unlock().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(answer(&output)["list_price_estimate"]["amount"].is_number());
    assert_eq!(fs::read(store.record()).unwrap(), record);
}
