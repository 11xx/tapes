//! A test process leaves nothing behind in the temp directory, including the
//! roots that outlive every test in it.

mod support;

use std::path::PathBuf;
use std::process::Command;

const CHILD: &str = "TAPES_TEMP_ROOT_CHILD";

/// Runs `test` again in a child process told to `outcome` ("pass" or
/// "fail") after creating the shared data home, and returns the data home
/// the child reported and whether the child succeeded.
fn data_home_of_child(test: &str, outcome: &str) -> (PathBuf, bool) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, outcome)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let root = stdout
        .lines()
        .find_map(|line| line.split_once("data-home:").map(|(_, root)| root))
        .unwrap_or_else(|| panic!("child named no data home: {stdout}"));
    (PathBuf::from(root), output.status.success())
}

/// In a child, creates the shared data home and passes or fails as told.
/// Returns false in the parent.
fn act_as_child() -> bool {
    let Ok(outcome) = std::env::var(CHILD) else {
        return false;
    };
    let root = support::empty_opencode_data_home();
    assert!(root.join("opencode/opencode.db").is_file());
    println!("data-home:{}", root.display());
    assert_eq!(outcome, "pass", "the child fails as told");
    true
}

/// Rejects a shared XDG data home that outlives a test process whose tests
/// all passed: every run would add another root to the temp directory.
#[test]
fn the_shared_data_home_is_removed_when_a_passing_test_process_exits() {
    if act_as_child() {
        return;
    }
    let (root, passed) = data_home_of_child(
        "the_shared_data_home_is_removed_when_a_passing_test_process_exits",
        "pass",
    );
    assert!(passed, "the child was told to pass");
    assert!(
        !root.exists(),
        "the shared data home outlived its passing process: {}",
        root.display()
    );
}

/// Rejects a shared XDG data home that outlives a test process with a failed
/// test: a failing run must not leave its roots behind either.
#[test]
fn the_shared_data_home_is_removed_when_a_failing_test_process_exits() {
    if act_as_child() {
        return;
    }
    let (root, passed) = data_home_of_child(
        "the_shared_data_home_is_removed_when_a_failing_test_process_exits",
        "fail",
    );
    assert!(!passed, "the child was told to fail");
    assert!(
        !root.exists(),
        "the shared data home outlived its failing process: {}",
        root.display()
    );
}
