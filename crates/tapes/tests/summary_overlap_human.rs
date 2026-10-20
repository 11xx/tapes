//! The human summary states how unrelated sessions were added only when some
//! were, and names a sampled id check.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

struct Home(PathBuf);

impl Home {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-overlap-human-{label}-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("projects/fixture-project")).unwrap();
        Self(root)
    }

    /// A Claude session with one assistant record per request id.
    fn session(&self, id: &str, requests: &[&str]) {
        let mut lines = vec![json!({
            "type": "user", "sessionId": id, "uuid": format!("{id}-user"),
            "timestamp": "2026-01-01T10:00:00Z",
            "message": {"role": "user", "content": "fixture"}
        })];
        for request in requests {
            lines.push(json!({
                "type": "assistant", "sessionId": id, "requestId": request,
                "uuid": format!("{id}-{request}"),
                "message": {"role": "assistant", "model": "fixture-model", "content": [],
                            "usage": {"input_tokens": 5, "output_tokens": 7}}
            }));
        }
        let body = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(
            self.0.join(format!("projects/fixture-project/{id}.jsonl")),
            body,
        )
        .unwrap();
    }

    fn summary(&self) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .env("TAPES_OFFLINE", "1")
            .args([
                "usage",
                "--global",
                "--harness",
                "claude",
                "--by",
                "harness",
            ])
            .env("HOME", &self.0)
            .env("CLAUDE_CONFIG_DIR", &self.0)
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(Path::new(&self.0));
    }
}

const UNRELATED: &str = "Sessions no recording relates are summed as disjoint";

#[test]
fn the_disjointness_line_is_printed_only_when_unrelated_sessions_were_summed() {
    let related = Home::new("related");
    related.session("first", &["shared"]);
    related.session("second", &["shared"]);
    let human = related.summary();
    assert!(human.contains("Related sessions first, second"), "{human}");
    assert!(!human.contains(UNRELATED), "{human}");

    let unrelated = Home::new("unrelated");
    unrelated.session("first", &["one"]);
    unrelated.session("second", &["two"]);
    let human = unrelated.summary();
    assert!(
        human.contains(&format!(
            "{UNRELATED}, inferred from absent copy evidence.\n"
        )),
        "{human}"
    );

    let beside = Home::new("beside");
    beside.session("first", &["shared"]);
    beside.session("second", &["shared"]);
    beside.session("healthy", &["own"]);
    let human = beside.summary();
    assert!(human.contains("Related sessions"), "{human}");
    assert!(human.contains(UNRELATED), "{human}");
}

#[test]
fn a_sampled_id_check_is_named_in_the_human_summary() {
    let home = Home::new("sampled");
    let requests = (0..4096)
        .map(|index| format!("req-{index}"))
        .collect::<Vec<_>>();
    let requests = requests.iter().map(String::as_str).collect::<Vec<_>>();
    home.session("long", &requests);
    home.session("other", &["elsewhere"]);
    let human = home.summary();
    assert!(
        human.contains(&format!(
            "{UNRELATED}, inferred from sampled copy evidence: the id check kept only a sample of the ids of long.\n"
        )),
        "{human}"
    );
}
