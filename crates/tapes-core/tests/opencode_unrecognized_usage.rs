//! OpenCode's session counters are the session row's recorded total, so only
//! a message outside the roles the reader projects can hold consumption the
//! figure does not show. Such a message qualifies the figure when it carries
//! OpenCode's usage shape, a `tokens` object holding a counter; the
//! projection reports that as `usage_counters`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{opencode::OpenCodeBackend, Backend};
use agent_tapes_core::{usage_full_with_backends, usage_with_backends, Selection};
use serde_json::{json, Value};

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const SESSION: &str = "ses_database_only_fixture";
const NATIVE: [&str; 6] = [
    "input",
    "output",
    "reasoning",
    "cache_read",
    "cache_write",
    "cost",
];

/// A copy of the OpenCode fixtures whose database message rows are extended
/// by `rows`, served by a program the copy resolves its responses from.
struct Fixtures(PathBuf);

impl Fixtures {
    fn new(rows: &[Value]) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-opencode-unrecognized-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode");
        fs::create_dir_all(root.join("bin")).unwrap();
        for entry in fs::read_dir(&source).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                fs::copy(entry.path(), root.join(entry.file_name())).unwrap();
            }
        }
        let messages = root.join("database-messages-database-only.json");
        let mut body = fs::read_to_string(&messages).unwrap();
        for row in rows {
            body.push_str(&row.to_string());
            body.push('\n');
        }
        fs::write(&messages, body).unwrap();
        std::os::unix::fs::symlink(root.join("opencode2"), root.join("bin/opencode")).unwrap();
        let data = root.join("data/opencode");
        fs::create_dir_all(&data).unwrap();
        fs::File::create(data.join("opencode.db")).unwrap();
        Self(root)
    }

    fn backend(&self) -> Box<dyn Backend> {
        std::env::set_var("XDG_DATA_HOME", self.0.join("data"));
        Box::new(OpenCodeBackend::new(self.0.join("bin/opencode")))
    }
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A message row as the projection returns it.
fn message(id: &str, role: &str, usage_counters: bool) -> Value {
    json!({
        "id": id,
        "time_created": 1784663090000_u64,
        "data": json!({
            "role": role,
            "time": {"created": 1784663090000_u64},
            "usage_counters": usage_counters,
        }).to_string(),
    })
}

fn view(fixtures: &Fixtures, full: bool) -> Value {
    let backends = vec![fixtures.backend()];
    let view = if full {
        usage_full_with_backends(&backends, Selection::Id(SESSION)).unwrap()
    } else {
        usage_with_backends(&backends, Selection::Id(SESSION)).unwrap()
    };
    serde_json::to_value(view).unwrap()
}

fn reasons<'a>(figure: &'a Value, counter: &str) -> Vec<&'a str> {
    figure["measurement_completeness"][counter]
        .as_array()
        .map(|reasons| reasons.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[test]
fn only_a_usage_bearing_message_outside_the_projected_roles_qualifies_the_figure() {
    for full in [false, true] {
        let label = format!("opencode full={full}");
        // The projected roles carry usage on their own rows, and a message
        // outside them without a token counter holds none.
        let whole = Fixtures::new(&[
            message("msg_extra_assistant", "assistant", true),
            message("msg_summary", "summary", false),
        ]);
        let figure = view(&whole, full);
        assert!(figure["tokens"].is_object(), "{label}: {figure}");
        assert!(
            figure.get("unrecognized_usage").is_none(),
            "{label}: {figure}"
        );
        for counter in NATIVE {
            assert!(
                !reasons(&figure, counter).contains(&"unrecognized-usage"),
                "{label} × {counter}: {figure}"
            );
        }

        let unread = Fixtures::new(&[message("msg_summary", "summary", true)]);
        let figure = view(&unread, full);
        assert_eq!(figure["unrecognized_usage"], 1, "{label}: {figure}");
        for counter in NATIVE {
            assert!(
                reasons(&figure, counter).contains(&"unrecognized-usage"),
                "{label} × {counter}: {figure}"
            );
        }
    }
}
