//! A text answer names a root by its id, qualified with its harness where the
//! same id names roots of different harnesses.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

/// A Claude session id a marker also names in pi.
const SHARED: &str = "30000000-0000-4000-8000-0000000000f1";
/// A Claude session launched by the pi session of id `SHARED`.
const LAUNCHED: &str = "30000000-0000-4000-8000-0000000000f2";

struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-cli-final-wave-lineage-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("projects/-fixtures-project");
        fs::create_dir_all(&project).unwrap();
        for (id, minute, prompt) in [
            (SHARED, "10:00", "Lead the fixture work.".to_owned()),
            (
                LAUNCHED,
                "10:10",
                format!("tapes-parent: pi:{SHARED}\nInspect the fixture."),
            ),
        ] {
            let records = [
                json!({"type": "user", "sessionId": id, "uuid": format!("{id}-user"),
                    "entrypoint": "sdk-cli", "timestamp": format!("2026-01-10T{minute}:00Z"),
                    "cwd": "/fixtures/project",
                    "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}}),
                json!({"type": "assistant", "sessionId": id, "uuid": format!("{id}-reply"),
                    "entrypoint": "sdk-cli", "requestId": format!("{id}-request"),
                    "timestamp": format!("2026-01-10T{minute}:30Z"), "cwd": "/fixtures/project",
                    "message": {"role": "assistant", "model": "claude-fixture",
                        "content": [{"type": "text", "text": "ok"}],
                        "usage": {"input_tokens": 1, "output_tokens": 100,
                            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}}),
            ];
            let body: String = records.iter().map(|record| format!("{record}\n")).collect();
            fs::write(project.join(format!("{id}.jsonl")), body).unwrap();
        }
        Self(root)
    }

    fn usage(&self, arguments: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env_remove("PI_SESSION_FILE")
            .env_remove("TAPES_PRICES")
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .arg("usage")
            .args(arguments)
            .args(["--global", "--native-store", "claude", "--native-root"])
            .arg(&self.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
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

/// Claim: grouped by parent, the text table writes a root id that names roots
/// of two harnesses as `<harness>:<id>`, one row per root, and a window's
/// ROOT column does the same for a share's root. Rejects two roots summed in
/// one row, and two rows or a root that read as the same session.
#[test]
fn a_shared_root_id_is_written_with_its_harness() {
    let store = Store::new();
    let summary = store.usage(&["--by", "parent"]);
    let rows = |text: &str, prefix: &str| {
        text.lines()
            .filter(|line| line.starts_with(prefix))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rows(&summary, &format!("claude:{SHARED}\t")).len(),
        1,
        "{summary}"
    );
    assert_eq!(
        rows(&summary, &format!("pi:{SHARED}\t")).len(),
        1,
        "{summary}"
    );

    let window = store.usage(&["--by", "parent", "--window", "2026-01-10T09:00:00Z.."]);
    let launched = rows(&window, LAUNCHED);
    assert_eq!(launched.len(), 1, "{window}");
    assert!(
        launched[0].contains(&format!("\tpi:{SHARED} (unresolved)\t")),
        "{window}"
    );
}
