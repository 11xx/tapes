//! Fixtures shared by the integration tests that outlive any one test.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static OPENCODE_DATA_HOME: OnceLock<PathBuf> = OnceLock::new();

/// An XDG data directory holding an empty OpenCode database, so the fixture
/// program's API answers instead of an installed store.
///
/// `XDG_DATA_HOME` is process-wide and tests run in parallel, so one
/// directory serves every test in the process and no test's end may remove
/// it. The process removes it at exit, whether its tests passed or failed.
pub fn empty_opencode_data_home() -> &'static Path {
    OPENCODE_DATA_HOME.get_or_init(|| {
        let root = tempfile::TempDir::with_prefix("tapes-xdg-").unwrap().keep();
        fs::create_dir_all(root.join("opencode")).unwrap();
        File::create(root.join("opencode/opencode.db")).unwrap();
        assert_eq!(unsafe { libc::atexit(remove_opencode_data_home) }, 0);
        root
    })
}

extern "C" fn remove_opencode_data_home() {
    if let Some(root) = OPENCODE_DATA_HOME.get() {
        let _ = fs::remove_dir_all(root);
    }
}
