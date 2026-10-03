//! Harness integrations that record what a harness leaves out of its own
//! recordings. Each is one file tapes writes into the harness's
//! configuration and marks as its own, so it can be replaced or removed
//! without touching a file someone else wrote.

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

const PI_EXTENSION_FILE: &str = "tapes-launch.ts";
const PI_EXTENSION_MARKER: &str = "// Managed by tapes. Reinstall instead of editing this file.\n";
const PI_EXTENSION_SOURCE: &str = include_str!("../pi/tapes-launch.ts");

/// pi reads extensions from `$PI_CODING_AGENT_DIR/extensions`, by default
/// `~/.pi/agent/extensions`.
fn pi_extension_path() -> Result<PathBuf> {
    let agent_dir = match env::var_os("PI_CODING_AGENT_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(home).join(".pi/agent"))
            .ok_or_else(|| anyhow!("neither PI_CODING_AGENT_DIR nor HOME is set"))?,
    };
    Ok(agent_dir.join("extensions").join(PI_EXTENSION_FILE))
}

fn pi_extension() -> String {
    format!("{PI_EXTENSION_MARKER}{PI_EXTENSION_SOURCE}")
}

fn read_existing(path: &PathBuf) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn install_pi() -> Result<()> {
    let path = pi_extension_path()?;
    let target = pi_extension();
    match read_existing(&path)? {
        Some(current) if current == target.as_bytes() => {
            println!(
                "The pi extension is already installed in {}.",
                path.display()
            );
            return Ok(());
        }
        Some(current) if !current.starts_with(PI_EXTENSION_MARKER.as_bytes()) => {
            bail!(
                "{} exists and tapes does not manage it; move it first",
                path.display()
            );
        }
        _ => {}
    }
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let staged = directory.join(format!(".{PI_EXTENSION_FILE}.{}", std::process::id()));
    fs::write(&staged, &target).with_context(|| format!("cannot write {}", staged.display()))?;
    if let Err(error) = fs::rename(&staged, &path) {
        let _ = fs::remove_file(&staged);
        return Err(error).with_context(|| format!("cannot install {}", path.display()));
    }
    println!("Installed the pi extension in {}.", path.display());
    println!("pi sessions opened from now on record their launch mode; run /reload in open ones.");
    Ok(())
}

pub fn uninstall_pi() -> Result<()> {
    let path = pi_extension_path()?;
    match read_existing(&path)? {
        None => println!("No pi extension is installed in {}.", path.display()),
        Some(current) if current.starts_with(PI_EXTENSION_MARKER.as_bytes()) => {
            fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            println!("Removed the pi extension from {}.", path.display());
            println!("Run /reload in pi sessions that are already open.");
        }
        Some(_) => bail!(
            "{} exists and tapes does not manage it; refusing to remove it",
            path.display()
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_installed_extension_opens_with_the_marker_and_writes_the_launch_entry() {
        let installed = pi_extension();
        assert!(installed.starts_with(PI_EXTENSION_MARKER));
        assert!(installed.contains(&format!(
            "appendEntry(\"{}\"",
            agent_tapes_core::backend::pi::LAUNCH_ENTRY_TYPE
        )));
    }
}
