//! Harness integrations that record what a harness leaves out of its own
//! recordings. Each is one regular file tapes writes into the harness's
//! configuration and marks as its own, so it can be replaced or removed
//! without touching a file someone else wrote.
//!
//! Installing does not lock the harness's configuration. A file written at
//! the integration's path in the instant between tapes' check and its write
//! can be replaced or removed; any file present when tapes looks, and not
//! marked as tapes', is left alone.

use std::env;
use std::ffi::{CStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

const PI_EXTENSION_FILE: &str = "tapes-launch.ts";
const PI_EXTENSION_MARKER: &str = "// Managed by tapes. Reinstall instead of editing this file.\n";
const PI_EXTENSION_SOURCE: &str = include_str!("../pi/tapes-launch.ts");

/// pi reads extensions from `<agent dir>/extensions`. The agent dir is
/// `$PI_CODING_AGENT_DIR` with a leading `~` expanded, as pi expands it, or
/// `~/.pi/agent`. pi also accepts a `file://` URL there; tapes refuses one
/// rather than risk resolving it differently. The home directory is `$HOME`,
/// or the account's home when `HOME` is unset, as Node's `os.homedir()`
/// resolves it.
fn pi_extension_path() -> Result<PathBuf> {
    let agent_dir = match env::var_os("PI_CODING_AGENT_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => expand_pi_path(dir, home_dir)?,
        None => home_dir()?.join(".pi/agent"),
    };
    Ok(agent_dir.join("extensions").join(PI_EXTENSION_FILE))
}

fn home_dir() -> Result<PathBuf> {
    if let Some(home) = env::var_os("HOME").filter(|home| !home.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    account_home().ok_or_else(|| anyhow!("HOME is unset and the account has no home directory"))
}

/// The home directory the password database records for this account.
fn account_home() -> Option<PathBuf> {
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    // SAFETY: every pointer refers to storage that outlives the call, and the
    // buffer length passed is the buffer's own length.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }
    // SAFETY: `getpwuid_r` succeeded and filled `entry`, whose `pw_dir` points
    // into `buffer` as a NUL-terminated string.
    let dir = unsafe { CStr::from_ptr((*found).pw_dir) };
    (!dir.to_bytes().is_empty()).then(|| PathBuf::from(OsString::from_vec(dir.to_bytes().to_vec())))
}

/// pi's own rule (`normalizePath`): `~` alone is the home directory and a
/// leading `~/` is relative to it; anything else is taken as written, except
/// a `file://` URL, which tapes refuses.
fn expand_pi_path(raw: OsString, home: impl Fn() -> Result<PathBuf>) -> Result<PathBuf> {
    let bytes = raw.as_encoded_bytes();
    if bytes == b"~" {
        return home();
    }
    if let Some(rest) = bytes.strip_prefix(b"~/") {
        let rest = OsString::from_vec(rest.to_vec());
        return Ok(home()?.join(rest));
    }
    if bytes.starts_with(b"file://") {
        bail!(
            "PI_CODING_AGENT_DIR is a file URL; tapes does not resolve URLs, so set it to the directory's path"
        );
    }
    Ok(PathBuf::from(raw))
}

fn pi_extension() -> String {
    format!("{PI_EXTENSION_MARKER}{PI_EXTENSION_SOURCE}")
}

/// What occupies the extension's path when tapes looks.
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

/// Write the extension beside its destination under a name created
/// exclusively, so no existing file is ever reused or overwritten.
fn stage(directory: &Path, contents: &str) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let staged = directory.join(format!(
        ".{PI_EXTENSION_FILE}.{}.{nanos}",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .with_context(|| format!("cannot create {}", staged.display()))?;
    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = written {
        let _ = fs::remove_file(&staged);
        return Err(error).with_context(|| format!("cannot write {}", staged.display()));
    }
    Ok(staged)
}

pub fn install_pi() -> Result<()> {
    let path = pi_extension_path()?;
    let target = pi_extension();
    let replacing = match occupant(&path)? {
        Occupant::Managed(bytes) if bytes == target.as_bytes() => {
            println!(
                "The pi extension is already installed in {}.",
                path.display()
            );
            return Ok(());
        }
        Occupant::Managed(_) => true,
        Occupant::Absent => false,
        Occupant::Foreign(why) => bail!("{} {why}; move it first", path.display()),
    };
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let staged = stage(directory, &target)?;
    // A new file is published with a hard link, which never replaces a path
    // that appeared meanwhile; an older managed file is replaced by rename.
    let published = if replacing {
        fs::rename(&staged, &path)
    } else {
        fs::hard_link(&staged, &path)
    };
    let _ = fs::remove_file(&staged);
    match published {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            bail!("{} appeared while tapes was working; rerun", path.display())
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot install {}", path.display()))
        }
    }
    println!("Installed the pi extension in {}.", path.display());
    println!("pi sessions opened from now on record their launch mode; run /reload in open ones.");
    Ok(())
}

pub fn uninstall_pi() -> Result<()> {
    let path = pi_extension_path()?;
    match occupant(&path)? {
        Occupant::Absent => println!("No pi extension is installed in {}.", path.display()),
        Occupant::Managed(_) => {
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
    use std::os::unix::ffi::OsStrExt;

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
        assert_eq!(expand("éééé/agent"), PathBuf::from("éééé/agent"));
        // pi's URL guard is case-sensitive, so it takes this value literally.
        assert_eq!(expand("FILE:///srv/pi"), PathBuf::from("FILE:///srv/pi"));
        assert!(expand_pi_path(OsString::from("file:///srv/pi"), home).is_err());
        let non_utf8 = OsString::from(std::ffi::OsStr::from_bytes(b"~/\xffagent"));
        assert_eq!(
            expand_pi_path(non_utf8, home).unwrap(),
            Path::new("/home/fixture").join(std::ffi::OsStr::from_bytes(b"\xffagent"))
        );
    }

    #[test]
    fn the_account_home_answers_when_home_is_unset() {
        assert!(account_home().is_some_and(|home| home.is_absolute()));
    }

    #[test]
    fn staging_never_reuses_an_existing_name() {
        let root = std::env::temp_dir().join(format!("tapes-stage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let first = stage(&root, "one").unwrap();
        let second = stage(&root, "two").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(&first).unwrap(), "one");
        assert_eq!(fs::read_to_string(&second).unwrap(), "two");
        let _ = fs::remove_dir_all(&root);
    }
}
