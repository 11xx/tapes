//! Record which source the reader was built from, so a projection can name
//! the build that produced it. A build outside a git checkout, such as one
//! from a published package, records no revision.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn main() {
    // Rebuild when the checked-out commit moves or the reader's own sources
    // change; the git paths are absolute even from a linked worktree.
    for path in [
        git(&["rev-parse", "--git-path", "HEAD"]),
        git(&["symbolic-ref", "-q", "HEAD"])
            .and_then(|reference| git(&["rev-parse", "--git-path", &reference])),
    ]
    .into_iter()
    .flatten()
    {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=build.rs");

    // A copy of the crate inside some other repository must not report that
    // repository's commit, so the revision counts only when git tracks this
    // crate's own manifest.
    let tracked = git(&["ls-files", "--full-name", "--", "Cargo.toml"]).is_some();
    let revision = tracked.then(|| git(&["rev-parse", "HEAD"])).flatten();
    let modified = revision.is_some()
        && git(&["status", "--porcelain", "--untracked-files=no", "--", "."]).is_some();
    println!(
        "cargo:rustc-env=TAPES_SOURCE_REVISION={}",
        revision.unwrap_or_default()
    );
    println!("cargo:rustc-env=TAPES_SOURCE_MODIFIED={modified}");
}
