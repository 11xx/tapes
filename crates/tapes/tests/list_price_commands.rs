//! Every command that reports a session's harness cost carries the
//! list-price estimate beside it, priced as `usage --prices` prices it, and
//! carries none without a table.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const SESSION: &str = "30000000-0000-4000-8000-000000000001";
const COPY: &str = "30000000-0000-4000-8000-000000000002";

/// The fixture session's bounded figure priced by the fixture table, by hand:
/// 28 uncached input at 3, 10,400 cache reads at 0.3, 2,900 1-hour cache
/// writes at 6 and 750 output at 15, per million tokens.
const ESTIMATE: f64 = (28.0 * 3.0 + 10_400.0 * 0.3 + 2_900.0 * 6.0 + 750.0 * 15.0) / 1e6;

fn core_fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tapes-core/tests/fixtures")
        .join(name)
}

/// A Claude store holding the resumed-session fixture and its copy, with an
/// empty configuration home, so no default table applies.
struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-cli-price-commands-{}-{nonce}",
            std::process::id()
        ));
        let project = root.join("projects/-fixtures-resumed-project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        for id in [SESSION, COPY] {
            fs::copy(
                core_fixtures("claude-resumed/project").join(format!("{id}.jsonl")),
                project.join(format!("{id}.jsonl")),
            )
            .unwrap();
        }
        Self(root)
    }

    fn run(&self, arguments: &[&str], priced: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tapes"));
        command
            .env_remove("PI_SESSION_FILE")
            .env_remove("TAPES_PRICES")
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .args(arguments)
            .args(["--native-store", "claude", "--native-root"])
            .arg(&self.0);
        if priced {
            command
                .arg("--prices")
                .arg(core_fixtures("prices/prices.toml"));
        }
        command.output().unwrap()
    }

    fn json(&self, arguments: &[&str], priced: bool) -> Value {
        let output = self.run(&[arguments, &["--json"]].concat(), priced);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn text(&self, arguments: &[&str], priced: bool) -> String {
        let output = self.run(arguments, priced);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_estimate(estimate: &Value, what: &str) {
    assert_eq!(
        estimate["kind"], "list-price-estimate",
        "{what}: {estimate}"
    );
    let amount = estimate["amount"]
        .as_f64()
        .unwrap_or_else(|| panic!("{what}: {estimate}"));
    assert!((amount - ESTIMATE).abs() < 1e-12, "{what}: {amount}");
    assert_eq!(estimate["models"][0]["id"], "claude-fixture", "{what}");
}

/// The JSON answers of each command, and where each puts the session's
/// estimate.
fn answers(store: &Store, priced: bool) -> Vec<(&'static str, Value)> {
    let list = store.json(&["list"], priced);
    let show = store.json(&["show", SESSION], priced);
    let full = store.json(&["show", SESSION, "--full"], priced);
    let stats = store.json(&["stats", SESSION], priced);
    let brief = store.json(&["brief", SESSION], priced);
    let summary = store.json(&["usage", "--global"], priced);
    let listed = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == SESSION)
        .cloned()
        .unwrap();
    let figure = summary["totals"]["session_figures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|figure| figure["id"] == SESSION)
        .cloned()
        .unwrap();
    vec![
        ("list", listed["list_price_estimate"].clone()),
        ("show", show["session"]["list_price_estimate"].clone()),
        (
            "show --full",
            full["session"]["list_price_estimate"].clone(),
        ),
        ("stats", stats["usage"]["list_price_estimate"].clone()),
        ("brief", brief["usage"]["list_price_estimate"].clone()),
        ("summary figure", figure["list_price_estimate"].clone()),
    ]
}

/// Claim: with `--prices`, `list --json`, `show --json` (bounded and
/// `--full`), `stats --json`, `brief --json` and a summary's per-session
/// overlap figure each carry the session's list-price estimate, equal to
/// the hand-priced figure, under the schemas that name it. Rejects a
/// command that reports a cost with no estimate beside it.
#[test]
fn each_command_carries_the_estimate_with_a_table() {
    let store = Store::new();
    for (what, estimate) in answers(&store, true) {
        assert_estimate(&estimate, what);
    }
    assert_eq!(store.json(&["list"], true)["schema"], "tapes-list/11");
    assert_eq!(
        store.json(&["show", SESSION], true)["schema"],
        "tapes-session/21"
    );
    assert_eq!(
        store.json(&["stats", SESSION], true)["schema"],
        "tapes-stats/16"
    );
    assert_eq!(
        store.json(&["brief", SESSION], true)["schema"],
        "tapes-brief/14"
    );
    assert_eq!(
        store.json(&["usage", "--global"], true)["schema"],
        "tapes-usage-summary/13"
    );
}

/// Claim: without a table, by flag, environment or default, no command
/// carries an estimate. Rejects an estimate invented from no table.
#[test]
fn no_command_carries_an_estimate_without_a_table() {
    let store = Store::new();
    for (what, estimate) in answers(&store, false) {
        assert!(estimate.is_null(), "{what}: {estimate}");
    }
}

/// Claim: text output adds the estimate, labelled as one from the price
/// table, only where it already prints a cost: the stats usage block and a
/// summary's per-session figure lines. Rejects an unlabelled amount, or an
/// estimate printed where no cost is.
#[test]
fn text_labels_the_estimate_where_a_cost_is_printed() {
    let store = Store::new();
    let line = "list-price estimate 0.0319 USD (from the price table: claude-fixture effective 2026-01-01)";
    let stats = store.text(&["stats", SESSION], true);
    assert!(stats.contains(line), "{stats}");
    let summary = store.text(&["usage", "--global"], true);
    assert!(
        summary
            .lines()
            .any(|text| text.starts_with("Session figure") && text.contains(line)),
        "{summary}"
    );
    for arguments in [&["stats", SESSION][..], &["usage", "--global"][..]] {
        let unpriced = store.text(arguments, false);
        assert!(!unpriced.contains("list-price estimate"), "{unpriced}");
    }
    let shown = store.text(&["show", SESSION], true);
    assert!(!shown.contains("list-price estimate"), "{shown}");
}

/// Claim: a named table that cannot be read refuses each command, as it
/// refuses `usage`. Rejects answering without the estimate the caller asked
/// for.
#[test]
fn a_named_table_that_cannot_be_read_refuses_each_command() {
    let store = Store::new();
    let missing = store.0.join("absent.toml");
    for arguments in [
        &["list"][..],
        &["show", SESSION][..],
        &["stats", SESSION][..],
        &["brief", SESSION][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env_remove("TAPES_PRICES")
            .env("HOME", store.0.join("home"))
            .env("XDG_CONFIG_HOME", store.0.join("config"))
            .args(arguments)
            .args(["--native-store", "claude", "--native-root"])
            .arg(&store.0)
            .arg("--prices")
            .arg(&missing)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{arguments:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&missing.display().to_string()),
            "{arguments:?}: {stderr}"
        );
    }
}
