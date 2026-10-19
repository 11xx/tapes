//! An OpenCode session names its launcher through the `tapes-parent` marker
//! opening its first prompt, through the API and the database alike.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_tapes_core::backend::{opencode::OpenCodeBackend, Backend};
use agent_tapes_core::usage::GroupBy;
use agent_tapes_core::{usage_summary_with_backends, SessionSelection};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/opencode")
}

/// The fixture program under a name that selects its response set, with no
/// installed database, so the API or the fixture's own rows answer.
struct Alias(PathBuf, Option<PathBuf>);

impl Alias {
    fn new(name: &str, in_directory: bool) -> Self {
        static XDG: OnceLock<PathBuf> = OnceLock::new();
        let root = XDG.get_or_init(|| {
            let root =
                std::env::temp_dir().join(format!("tapes-series-xdg-{}", std::process::id()));
            fs::create_dir_all(root.join("opencode")).unwrap();
            File::create(root.join("opencode/opencode.db")).unwrap();
            root
        });
        std::env::set_var("XDG_DATA_HOME", root);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let (path, directory) = if in_directory {
            let directory = std::env::temp_dir().join(format!(
                "tapes-series-opencode-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).unwrap();
            (directory.join(name), Some(directory))
        } else {
            (
                std::env::temp_dir().join(format!("{name}-{}-{nonce}", std::process::id())),
                None,
            )
        };
        std::os::unix::fs::symlink(fixtures().join("opencode2"), &path).unwrap();
        Self(path, directory)
    }
}

impl Drop for Alias {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        if let Some(directory) = &self.1 {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

/// The parent each session of a `--by parent` summary groups under.
fn parent_groups(backend: Box<dyn Backend>) -> Vec<(String, u64)> {
    let summary = usage_summary_with_backends(
        &[backend],
        &SessionSelection {
            limit: Some(50),
            ..Default::default()
        },
        &[GroupBy::Parent],
    )
    .unwrap();
    let summary = serde_json::to_value(summary).unwrap();
    let mut groups = summary["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|group| {
            (
                group["key"]["parent"].as_str().unwrap().to_owned(),
                group["sessions"].as_u64().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    groups.sort();
    groups
}

/// Claim: an OpenCode session read through the API whose first message opens
/// with a `tapes-parent` marker groups under the session it names in a
/// `--by parent` summary. Rejects an OpenCode listing that never reads the
/// first prompt, which leaves the launched session its own group.
#[test]
fn an_opencode_api_session_groups_under_its_marker_parent() {
    let alias = Alias::new("opencode-launched", false);
    let groups = parent_groups(Box::new(OpenCodeBackend::new(&alias.0)));
    assert!(
        groups
            .iter()
            .any(|(parent, _)| parent == "30000000-0000-4000-8000-0000000000c1"),
        "{groups:?}"
    );
}

/// Claim: an OpenCode session read from the database whose first user
/// message's first text part opens with a marker groups under the session it
/// names. Rejects a database listing that never asks for the first prompt.
#[test]
fn an_opencode_database_session_groups_under_its_marker_parent() {
    let alias = Alias::new("opencode", true);
    let groups = parent_groups(Box::new(OpenCodeBackend::new(&alias.0)));
    assert!(
        groups
            .iter()
            .any(|(parent, _)| parent == "019f0000-0000-7000-8000-0000000000c2"),
        "{groups:?}"
    );
}
