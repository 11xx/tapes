//! Which sessions belong to the project a caller is standing in.
//!
//! A harness records the working directory a session ran in and nothing else,
//! so "the same project" has to be reconstructed from paths. Inside a
//! repository the answer is the repository's identity — its common Git
//! directory — which makes every linked worktree of one repository a single
//! project and keeps a submodule's sessions out of its superproject. Outside
//! one, it is the current directory's subtree.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// The set of session directories that count as one project.
#[derive(Debug)]
pub struct Scope {
    /// Canonical path of the repository's common Git directory. `None` when
    /// the origin is not in a working tree, which is what makes a scope fall
    /// back to plain subtree containment.
    identity: Option<PathBuf>,
    /// Directories whose subtrees belong to the project: every linked worktree
    /// of the repository, or the origin alone when there is no repository.
    roots: Vec<PathBuf>,
    /// A candidate directory's repository identity, once resolved. Candidates
    /// repeat heavily across a listing and each miss costs a process spawn.
    identities: RefCell<HashMap<PathBuf, Option<PathBuf>>>,
}

impl Scope {
    /// The project containing `origin`. Never fails on a missing or broken
    /// Git: absence of a repository is a fact about the path, and the scope
    /// degrades to the subtree rather than to an error.
    pub fn at(origin: &Path) -> Result<Self> {
        let origin =
            canonical(origin).with_context(|| format!("failed to resolve {}", origin.display()))?;
        let identity = repository_identity(&origin);
        let roots = identity
            .as_ref()
            .map(|_| worktree_roots(&origin))
            .filter(|roots| !roots.is_empty())
            .unwrap_or_else(|| vec![origin]);
        Ok(Self {
            identity,
            roots,
            identities: RefCell::new(HashMap::new()),
        })
    }

    /// The project containing the current directory.
    pub fn here() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to read the current directory")?;
        Self::at(&cwd)
    }

    /// Whether a session recorded in `directory` belongs to this project.
    ///
    /// Containment under a worktree root is the fast answer, but not the whole
    /// one: a submodule checkout lies below its superproject and is a
    /// different repository. Where a repository identity exists, containment
    /// only nominates a candidate and the identity decides.
    pub fn contains(&self, directory: &Path) -> bool {
        let Some(directory) = canonical(directory).ok() else {
            // The directory is gone. Nothing left on disk can prove which
            // repository it belonged to, so it is out of scope rather than
            // guessed at from its name.
            return false;
        };
        if !self.roots.iter().any(|root| directory.starts_with(root)) {
            return false;
        }
        let Some(identity) = self.identity.as_deref() else {
            return true;
        };
        let mut identities = self.identities.borrow_mut();
        identities
            .entry(directory.clone())
            .or_insert_with(|| repository_identity(&directory))
            .as_deref()
            == Some(identity)
    }

    /// The project's roots, for callers that can narrow a store by path before
    /// opening anything.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize()
}

/// A repository's identity: the canonical common Git directory, shared by
/// every linked worktree and distinct for a submodule. `None` when `path` is
/// not in a working tree, when it is a bare repository (whose subtree holds no
/// sessions worth unifying), or when Git is absent.
fn repository_identity(path: &Path) -> Option<PathBuf> {
    let inside = git(path, &["rev-parse", "--is-inside-work-tree"])?;
    if inside != "true" {
        return None;
    }
    let common = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    canonical(Path::new(&common)).ok()
}

fn worktree_roots(path: &Path) -> Vec<PathBuf> {
    let Some(listing) = git(path, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|root| canonical(Path::new(root)).ok())
        .collect()
}

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|value| value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(directory: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    fn repository(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tapes-scope-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp directory is creatable");
        run(&root, &["init", "-q"]);
        run(&root, &["config", "user.email", "scope@test"]);
        run(&root, &["config", "user.name", "scope"]);
        std::fs::write(root.join("file"), "contents").expect("file is writable");
        run(&root, &["add", "file"]);
        run(&root, &["commit", "-qm", "initial"]);
        root
    }

    #[test]
    fn a_subdirectory_is_the_same_project() {
        let root = repository("subdirectory");
        let nested = root.join("crates/inner");
        std::fs::create_dir_all(&nested).expect("nested directory is creatable");
        let scope = Scope::at(&root).expect("scope resolves");
        assert!(scope.contains(&nested));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_linked_worktree_is_the_same_project() {
        let root = repository("worktree");
        let linked = root.with_extension("linked");
        let _ = std::fs::remove_dir_all(&linked);
        run(&root, &["worktree", "add", "-q", linked.to_str().unwrap()]);

        // Asked from either side: a per-change worktree and its origin are one
        // project, which is the whole reason identity beats path containment.
        let from_root = Scope::at(&root).expect("scope resolves");
        assert!(from_root.contains(&linked));
        let from_linked = Scope::at(&linked).expect("scope resolves");
        assert!(from_linked.contains(&root));

        run(&root, &["worktree", "remove", linked.to_str().unwrap()]);
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_nested_repository_is_a_different_project() {
        let root = repository("superproject");
        let inner = root.join("vendor/inner");
        std::fs::create_dir_all(&inner).expect("nested directory is creatable");
        run(&inner, &["init", "-q"]);

        let scope = Scope::at(&root).expect("scope resolves");
        assert!(!scope.contains(&inner));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_directory_that_is_gone_is_out_of_scope() {
        let root = repository("missing");
        let scope = Scope::at(&root).expect("scope resolves");
        assert!(!scope.contains(&root.join("never-existed")));
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn without_a_repository_the_scope_is_the_subtree() {
        let root = std::env::temp_dir().join("tapes-scope-plain");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("temp directory is creatable");
        let scope = Scope::at(&root).expect("scope resolves");
        assert!(scope.contains(&nested));
        assert!(!scope.contains(&std::env::temp_dir()));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
