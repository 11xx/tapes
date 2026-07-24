use std::process::{Command, Stdio};

fn tapes() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tapes"))
}

#[test]
fn list_help_exits_successfully() {
    assert!(tapes().args(["list", "--help"]).status().unwrap().success());
}

#[test]
fn show_help_exits_successfully() {
    assert!(tapes().args(["show", "--help"]).status().unwrap().success());
}

#[test]
fn export_help_exits_successfully() {
    assert!(tapes()
        .args(["export", "--help"])
        .status()
        .unwrap()
        .success());
}

#[test]
fn list_without_stores_is_empty_and_successful() {
    let temporary_home = std::env::temp_dir().join(format!("tapes-test-{}", std::process::id()));
    let output = tapes()
        .arg("list")
        .env("HOME", temporary_home)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn piped_help_does_not_panic() {
    let mut child = tapes()
        .arg("--help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();

    assert!(!String::from_utf8_lossy(&output.stderr).contains("panic"));
}
