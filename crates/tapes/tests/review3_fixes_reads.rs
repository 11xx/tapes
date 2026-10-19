//! A command reading a selection refuses a named price table it cannot
//! read, as it refuses for one session.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const SESSION: &str = "30000000-0000-4000-8000-000000000001";

/// A Claude store holding one fixture session, with an empty configuration
/// home, so no default table applies.
struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-cli-review3-reads-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("projects/-fixtures-resumed-project");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../tapes-core/tests/fixtures/claude-resumed/project")
                .join(format!("{SESSION}.jsonl")),
            project.join(format!("{SESSION}.jsonl")),
        )
        .unwrap();
        Self(root)
    }

    fn run(&self, arguments: &[&str], prices: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env_remove("PI_SESSION_FILE")
            .env_remove("TAPES_PRICES")
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .args(arguments)
            .args(["--native-store", "claude", "--native-root"])
            .arg(&self.0)
            .arg("--prices")
            .arg(prices)
            .output()
            .unwrap()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Claim: `stats` over a selection refuses a named price table that cannot
/// be read, naming it, as `usage` over the same selection does, and answers
/// with a table it can read. Rejects a selection's stats that never look at
/// the table and answer as though none were named.
#[test]
fn stats_over_a_selection_refuses_a_named_table_it_cannot_read() {
    let store = Store::new();
    let missing = store.0.join("absent.toml");
    for command in ["stats", "usage"] {
        let output = store.run(&[command, "--global", "--limit", "1", "--json"], &missing);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{command}: {stderr}");
        assert!(
            stderr.contains(&missing.display().to_string()),
            "{command}: {stderr}"
        );
    }
    let table = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tapes-core/tests/fixtures/prices/prices.toml");
    let output = store.run(&["stats", "--global", "--limit", "1", "--json"], &table);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["schema"], "tapes-stats-summary/8", "{summary}");
}
