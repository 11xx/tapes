//! Harness integrations that record what a harness leaves out of its own
//! recordings. Each is one regular file tapes writes into the harness's
//! configuration and marks as its own, so it can be replaced or removed
//! without touching a file someone else wrote.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

const PI_EXTENSION_FILE: &str = "tapes-launch.ts";
const PI_EXTENSION_MARKER: &str = "// Managed by tapes. Reinstall instead of editing this file.\n";
const PI_EXTENSION_SOURCE: &str = include_str!("../pi/tapes-launch.ts");

/// pi reads extensions from `<agent dir>/extensions`. The agent dir is
/// `$PI_CODING_AGENT_DIR`, which pi expands as it does every path it is
/// given (`~`, `~/…`, and `file://` URLs), or `~/.pi/agent`.
fn pi_extension_path() -> Result<PathBuf> {
    let home = || {
        env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set"))
    };
    let agent_dir = match env::var_os("PI_CODING_AGENT_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => expand_pi_path(dir, home)?,
        None => home()?.join(".pi/agent"),
    };
    Ok(agent_dir.join("extensions").join(PI_EXTENSION_FILE))
}

fn expand_pi_path(raw: OsString, home: impl Fn() -> Result<PathBuf>) -> Result<PathBuf> {
    let Some(text) = raw.to_str() else {
        return Ok(PathBuf::from(raw));
    };
    if text == "~" {
        return home();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return Ok(home()?.join(rest));
    }
    if let Some(url) = text.strip_prefix("file://") {
        let path = url
            .strip_prefix("localhost")
            .unwrap_or(url)
            .strip_prefix('/')
            .ok_or_else(|| anyhow!("PI_CODING_AGENT_DIR {text} is not a local file URL"))?;
        return Ok(PathBuf::from(OsString::from_vec(percent_decode(
            &format!("/{path}"),
        )?)));
    }
    Ok(PathBuf::from(text))
}

fn percent_decode(text: &str) -> Result<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .and_then(|pair| std::str::from_utf8(pair).ok())
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| anyhow!("{text} holds an invalid percent escape"))?;
            decoded.push(hex);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(decoded)
}

fn pi_extension() -> String {
    format!("{PI_EXTENSION_MARKER}{PI_EXTENSION_SOURCE}")
}

/// What occupies the extension's path: nothing, a file tapes manages (with
/// its bytes), or anything else, which tapes never replaces or removes.
enum Occupant {
    Absent,
    Managed(Vec<u8>),
    Foreign(&'static str),
}

fn occupant(path: &Path) -> Result<Occupant> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Occupant::Absent),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()))
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(Occupant::Foreign("is a symbolic link"));
    }
    if !metadata.is_file() {
        return Ok(Occupant::Foreign("is not a regular file"));
    }
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    if bytes.starts_with(PI_EXTENSION_MARKER.as_bytes()) {
        Ok(Occupant::Managed(bytes))
    } else {
        Ok(Occupant::Foreign("is not managed by tapes"))
    }
}

/// Confirm the path still holds exactly the managed bytes inspected earlier,
/// so a file someone put there in between is never replaced or removed.
fn still_managed(path: &Path, inspected: &[u8]) -> Result<()> {
    match occupant(path)? {
        Occupant::Managed(bytes) if bytes == inspected => Ok(()),
        _ => bail!("{} changed while tapes was working; rerun", path.display()),
    }
}

pub fn install_pi() -> Result<()> {
    let path = pi_extension_path()?;
    let target = pi_extension();
    let inspected = match occupant(&path)? {
        Occupant::Managed(bytes) if bytes == target.as_bytes() => {
            println!(
                "The pi extension is already installed in {}.",
                path.display()
            );
            return Ok(());
        }
        Occupant::Foreign(why) => bail!("{} {why}; move it first", path.display()),
        Occupant::Managed(bytes) => Some(bytes),
        Occupant::Absent => None,
    };
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let staged = directory.join(format!(".{PI_EXTENSION_FILE}.{}", std::process::id()));
    fs::write(&staged, &target).with_context(|| format!("cannot write {}", staged.display()))?;
    let published = match &inspected {
        // A hard link never replaces an existing path, so a file that
        // appeared since the inspection is left alone.
        None => fs::hard_link(&staged, &path).map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                anyhow!("{} appeared while tapes was working; rerun", path.display())
            } else {
                anyhow!(error).context(format!("cannot install {}", path.display()))
            }
        }),
        Some(bytes) => still_managed(&path, bytes).and_then(|()| {
            fs::rename(&staged, &path).with_context(|| format!("cannot install {}", path.display()))
        }),
    };
    let _ = fs::remove_file(&staged);
    published?;
    println!("Installed the pi extension in {}.", path.display());
    println!("pi sessions opened from now on record their launch mode; run /reload in open ones.");
    Ok(())
}

pub fn uninstall_pi() -> Result<()> {
    let path = pi_extension_path()?;
    match occupant(&path)? {
        Occupant::Absent => println!("No pi extension is installed in {}.", path.display()),
        Occupant::Managed(bytes) => {
            still_managed(&path, &bytes)?;
            fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            println!("Removed the pi extension from {}.", path.display());
            println!("Run /reload in pi sessions that are already open.");
        }
        Occupant::Foreign(why) => bail!("{} {why}; refusing to remove it", path.display()),
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
        assert!(installed.contains("session: ctx.sessionManager.getSessionId()"));
    }

    #[test]
    fn agent_dir_values_expand_the_way_pi_expands_them() {
        let home = || Ok(PathBuf::from("/home/fixture"));
        let expand = |raw: &str| expand_pi_path(OsString::from(raw), home).unwrap();
        assert_eq!(expand("~"), PathBuf::from("/home/fixture"));
        assert_eq!(expand("~/agent"), PathBuf::from("/home/fixture/agent"));
        assert_eq!(expand("/srv/pi agent"), PathBuf::from("/srv/pi agent"));
        assert_eq!(expand("relative/agent"), PathBuf::from("relative/agent"));
        assert_eq!(expand("~other/agent"), PathBuf::from("~other/agent"));
        assert_eq!(
            expand("file:///srv/pi%20agent"),
            PathBuf::from("/srv/pi agent")
        );
        assert_eq!(expand("file://localhost/srv/pi"), PathBuf::from("/srv/pi"));
        assert!(expand_pi_path(OsString::from("file://host/srv"), home).is_err());
        assert!(expand_pi_path(OsString::from("file:///srv/%zz"), home).is_err());
    }
}
