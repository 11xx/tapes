//! `tapes usage --prices` adds a list-price estimate beside the harness's
//! figures, from the named table, `TAPES_PRICES`, or the default table, and
//! refuses or warns exactly as the lookup's rules say.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const SESSION: &str = "30000000-0000-4000-8000-000000000001";

fn core_fixtures(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tapes-core/tests/fixtures")
        .join(name)
}

struct Store(PathBuf);

impl Store {
    /// A Claude store root holding the resumed-session fixture and its copy,
    /// with an empty configuration home.
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("tapes-cli-prices-{}-{nonce}", std::process::id()));
        let project = root.join("projects/-fixtures-resumed-project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        for id in [SESSION, "30000000-0000-4000-8000-000000000002"] {
            fs::copy(
                core_fixtures("claude-resumed/project").join(format!("{id}.jsonl")),
                project.join(format!("{id}.jsonl")),
            )
            .unwrap();
        }
        Self(root)
    }

    fn table(&self) -> PathBuf {
        core_fixtures("prices/prices.toml")
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn usage(&self, arguments: &[&str], environment: &[(&str, &Path)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tapes"));
        command
            .env_remove("PI_SESSION_FILE")
            .env_remove("TAPES_PRICES")
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .arg("usage")
            .args(arguments)
            .args(["--native-store", "claude", "--native-root"])
            .arg(&self.0);
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().unwrap()
    }

    fn json(&self, arguments: &[&str], environment: &[(&str, &Path)]) -> (Value, String) {
        let output = self.usage(&[arguments, &["--json"]].concat(), environment);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "{stderr}");
        (serde_json::from_slice(&output.stdout).unwrap(), stderr)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claim: `--prices FILE` adds a labelled estimate to the session answer,
/// its text line marked as an estimate from the table, and to a span answer;
/// the usage schema names the shape. Rejects an answer without the estimate,
/// or one that prints the estimate as the harness's cost.
#[test]
fn a_named_table_prices_the_answer() {
    let store = Store::new();
    let table = store.table();
    let table = table.to_str().unwrap();
    let (whole, _) = store.json(&[SESSION, "--full", "--prices", table], &[]);
    assert_eq!(whole["schema"], "tapes-usage/15");
    let estimate = &whole["list_price_estimate"];
    assert_eq!(estimate["kind"], "list-price-estimate");
    assert!((estimate["amount"].as_f64().unwrap() - 0.031854).abs() < 1e-12);
    assert!(whole["cost"].is_null());
    let (span, _) = store.json(
        &[SESSION, "--from", "2026-01-05T13:00:00Z", "--prices", table],
        &[],
    );
    assert!((span["list_price_estimate"]["amount"].as_f64().unwrap() - 0.012633).abs() < 1e-12);
    let output = store.usage(&[SESSION, "--full", "--prices", table], &[]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains(
            "list-price estimate 0.0319 USD (from the price table: claude-fixture effective 2026-01-01)"
        ),
        "{text}"
    );
}

/// Claim: a table named by `--prices` or `TAPES_PRICES` that is missing or
/// invalid refuses the command and names the entry. Rejects answering
/// without an estimate when the caller named a table.
#[test]
fn a_named_table_that_cannot_be_used_refuses() {
    let store = Store::new();
    let bad = store.write(
        "bad.toml",
        "schema = \"tapes-price-table/1\"\ncurrency = \"USD\"\n[[models]]\nid = \"a\"\neffective = 2026-01-01\nsource = \"s\"\nper_million = { output = -1.0 }\n",
    );
    for (arguments, environment) in [
        (vec![SESSION, "--prices", bad.to_str().unwrap()], vec![]),
        (
            vec![SESSION, "--prices", "/nonexistent/prices.toml"],
            vec![],
        ),
        (vec![SESSION], vec![("TAPES_PRICES", bad.as_path())]),
    ] {
        let output = store.usage(&arguments, &environment);
        assert!(!output.status.success(), "{arguments:?} succeeded");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("price table"), "{stderr}");
    }
    let output = store.usage(&[SESSION, "--prices", bad.to_str().unwrap()], &[]);
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("models[0] (id `a`): the output price must be a finite number of at least zero"));
}

/// Claim: an invalid default table warns once on stderr and leaves the
/// estimate absent, a valid one prices the answer, and no table leaves the
/// estimate absent without a warning. Rejects a refusal over a default table,
/// a silent failure, or a zero estimate.
#[test]
fn the_default_table_warns_or_prices_and_its_absence_is_silent() {
    let store = Store::new();
    let (absent, stderr) = store.json(&[SESSION], &[]);
    assert!(absent.get("list_price_estimate").is_none());
    assert!(!stderr.contains("price table"), "{stderr}");

    store.write(
        "config/tapes/prices.toml",
        "schema = \"tapes-price-table/1\"\n",
    );
    let (invalid, stderr) = store.json(&[SESSION], &[]);
    assert!(invalid.get("list_price_estimate").is_none());
    assert_eq!(
        stderr.matches("warning: the default price table").count(),
        1,
        "{stderr}"
    );

    store.write(
        "config/tapes/prices.toml",
        &fs::read_to_string(store.table()).unwrap(),
    );
    let (priced, stderr) = store.json(&[SESSION], &[]);
    assert!(
        priced["list_price_estimate"]["amount"].is_number(),
        "{priced}"
    );
    assert!(!stderr.contains("warning"), "{stderr}");
}

/// Claim: a summary with a table shows a LIST-PRICE ESTIMATE column, and
/// withholds it with its reason where the sessions' totals are withheld.
/// Rejects a summary that adds estimates of sessions sharing requests.
#[test]
fn a_summary_withholds_the_estimate_where_its_totals_are_withheld() {
    let store = Store::new();
    let table = store.table();
    let output = store.usage(&["--global", "--prices", table.to_str().unwrap()], &[]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.lines()
            .next()
            .unwrap()
            .ends_with("\tLIST-PRICE ESTIMATE"),
        "{text}"
    );
    assert!(text.contains("withheld [identity-unestablished]"), "{text}");
    assert!(
        text.contains("List-price estimate withheld: identity-unestablished"),
        "{text}"
    );
    let (summary, _) = store.json(&["--global", "--prices", table.to_str().unwrap()], &[]);
    assert_eq!(summary["schema"], "tapes-usage-summary/11");
    assert_eq!(
        summary["totals"]["list_price_withheld"],
        serde_json::json!(["identity-unestablished"])
    );
}
