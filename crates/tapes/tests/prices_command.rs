//! `tapes prices update --from DIR` writes the dated table and its fetch
//! record under the data directory, and `tapes prices show` reads them back.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn sources() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tapes-core/tests/fixtures/prices/sources")
}

fn tapes(home: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
        .env_remove("TAPES_PRICES")
        .env("HOME", home.join("home"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn update_from_a_directory_writes_the_table_and_show_reads_it_back() {
    // Claim: `prices update --from DIR` writes prices.toml and
    // prices-fetch.json under $XDG_DATA_HOME/tapes without the network, and
    // `prices show MODEL --json` answers that model's row, a dated id by its
    // undated row. Rejects a table written elsewhere, one show cannot read,
    // and a dated id going unanswered.
    let root = tempfile::tempdir().unwrap();
    let from = sources();
    let update = tapes(
        root.path(),
        &[
            "prices",
            "update",
            "--from",
            from.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let report: Value = serde_json::from_slice(&update.stdout).unwrap();
    assert!(report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|source| source["ok"] == true));
    let data = root.path().join("data/tapes");
    assert!(data.join("prices.toml").is_file());
    let record: Value =
        serde_json::from_str(&fs::read_to_string(data.join("prices-fetch.json")).unwrap()).unwrap();
    assert_eq!(record["schema"], "tapes-price-fetch/1");
    assert_eq!(record["sources"]["openai"]["models"], 8);

    let show = tapes(
        root.path(),
        &["prices", "show", "claude-haiku-5-5-20261001", "--json"],
    );
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stderr)
    );
    let shown: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["schema"], "tapes-prices-show/1");
    assert_eq!(shown["usage_table"]["chosen_by"], "absent");
    let models = shown["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["id"], "claude-haiku-5-5");
    assert_eq!(models[0]["per_million"]["uncached_input"], 0.1);
    assert_eq!(models[0]["long_context"]["prompt_tokens"], 100_000);
    assert_eq!(shown["fetch"]["sources"]["anthropic"]["ok"], true);
}

#[test]
fn a_failed_source_is_reported_and_exits_non_zero() {
    // Claim: a source missing from --from fails alone: the command exits
    // non-zero naming it, and the other sources' rows are still written.
    // Rejects a failure reported as success and one source stopping all.
    let root = tempfile::tempdir().unwrap();
    let from = root.path().join("from");
    fs::create_dir_all(&from).unwrap();
    for name in ["anthropic.md", "litellm.json"] {
        fs::copy(sources().join(name), from.join(name)).unwrap();
    }
    let update = tapes(
        root.path(),
        &["prices", "update", "--from", from.to_str().unwrap()],
    );
    assert!(!update.status.success());
    let stdout = String::from_utf8_lossy(&update.stdout);
    assert!(stdout.contains("openai: failed"), "{stdout}");
    assert!(String::from_utf8_lossy(&update.stderr).contains("openai"));
    let show = tapes(
        root.path(),
        &["prices", "show", "claude-opus-5-5", "--json"],
    );
    assert!(show.status.success());
}
