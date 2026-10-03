//! Harness integrations that record what a harness leaves out of its own
//! recordings. Each is one regular file tapes writes into the harness's
//! configuration and marks as its own, so it can be replaced or removed
//! without touching a file someone else wrote.

use std::env;
use std::ffi::{CStr, OsString};
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
/// given (`~`, `~/…`, and `file:` URLs), or `~/.pi/agent`. The home
/// directory is `$HOME`, or the account's home when `HOME` is unset, as
/// Node's `os.homedir()` resolves it.
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
    if text.len() >= 7 && text[..7].eq_ignore_ascii_case("file://") {
        return file_url_path(text);
    }
    Ok(PathBuf::from(text))
}

/// The local path a `file:` URL names, as Node's `fileURLToPath` derives it on
/// a Unix host: the host is empty or `localhost`, the query and fragment are
/// not part of the path, percent escapes are decoded, and an encoded `/` is
/// refused.
fn file_url_path(url: &str) -> Result<PathBuf> {
    let rest = &url[7..];
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let rest = &rest[..end];
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
        bail!("{url} names host {host}; a file URL must be local");
    }
    if path.is_empty() {
        bail!("{url} names no path");
    }
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2f") {
        bail!("{url} holds an encoded '/'");
    }
    Ok(PathBuf::from(OsString::from_vec(percent_decode(path)?)))
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

/// Why a captured file is not one tapes wrote, or `None` when it is: a
/// regular file opening with the marker. A symbolic link, dangling or not,
/// is never tapes'.
fn foreign(path: &Path) -> Result<Option<&'static str>> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(Some("is a symbolic link"));
    }
    if !metadata.is_file() {
        return Ok(Some("is not a regular file"));
    }
    let bytes = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok((!bytes.starts_with(PI_EXTENSION_MARKER.as_bytes())).then_some("is not managed by tapes"))
}

/// Move whatever occupies `path` aside, atomically, so its ownership is judged
/// on what was actually taken rather than on an earlier look at the path.
/// Returns the captured path, or `None` when nothing was there.
fn capture(path: &Path, directory: &Path) -> Result<Option<PathBuf>> {
    let captured = directory.join(format!(".{PI_EXTENSION_FILE}.{}.old", std::process::id()));
    match fs::rename(path, &captured) {
        Ok(()) => Ok(Some(captured)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot move {} aside", path.display())),
    }
}

/// Put `source` at `path` without replacing anything there: a hard link
/// refuses an existing path, so a file that appeared meanwhile survives.
fn place(source: &Path, path: &Path) -> std::io::Result<()> {
    fs::hard_link(source, path)?;
    fs::remove_file(source)
}

/// Return a captured file that tapes does not own to its place, and say why
/// tapes left it alone. If something else took the place meanwhile, both are
/// kept and the captured file's new name is reported.
fn restore(captured: &Path, path: &Path, refusal: &str) -> anyhow::Error {
    match place(captured, path) {
        Ok(()) => anyhow!("{} {refusal}", path.display()),
        Err(_) => anyhow!(
            "{} changed while tapes was working; the file that was there is kept as {}",
            path.display(),
            captured.display()
        ),
    }
}

pub fn install_pi() -> Result<()> {
    let path = pi_extension_path()?;
    let target = pi_extension();
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
        && fs::read(&path).is_ok_and(|bytes| bytes == target.as_bytes())
    {
        println!(
            "The pi extension is already installed in {}.",
            path.display()
        );
        return Ok(());
    }
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(directory)
        .with_context(|| format!("cannot create {}", directory.display()))?;
    let staged = directory.join(format!(".{PI_EXTENSION_FILE}.{}.new", std::process::id()));
    fs::write(&staged, &target).with_context(|| format!("cannot write {}", staged.display()))?;
    let installed = install_staged(&staged, &path, directory);
    let _ = fs::remove_file(&staged);
    installed?;
    println!("Installed the pi extension in {}.", path.display());
    println!("pi sessions opened from now on record their launch mode; run /reload in open ones.");
    Ok(())
}

fn install_staged(staged: &Path, path: &Path, directory: &Path) -> Result<()> {
    let captured = capture(path, directory)?;
    if let Some(captured) = &captured {
        if let Some(why) = foreign(captured)? {
            return Err(restore(captured, path, &format!("{why}; move it first")));
        }
    }
    match place(staged, path) {
        Ok(()) => {
            if let Some(captured) = captured {
                let _ = fs::remove_file(captured);
            }
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if let Some(captured) = captured {
                let _ = fs::remove_file(captured);
            }
            bail!("{} appeared while tapes was working; rerun", path.display())
        }
        Err(error) => {
            if let Some(captured) = &captured {
                let _ = place(captured, path);
            }
            Err(error).with_context(|| format!("cannot install {}", path.display()))
        }
    }
}

pub fn uninstall_pi() -> Result<()> {
    let path = pi_extension_path()?;
    let directory = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let Some(captured) = capture(&path, directory)? else {
        println!("No pi extension is installed in {}.", path.display());
        return Ok(());
    };
    if let Some(why) = foreign(&captured)? {
        return Err(restore(
            &captured,
            &path,
            &format!("{why}; refusing to remove it"),
        ));
    }
    fs::remove_file(&captured).with_context(|| format!("cannot remove {}", captured.display()))?;
    println!("Removed the pi extension from {}.", path.display());
    println!("Run /reload in pi sessions that are already open.");
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
        assert_eq!(expand("file://LOCALHOST/srv/pi"), PathBuf::from("/srv/pi"));
        assert_eq!(expand("FILE:///srv/pi"), PathBuf::from("/srv/pi"));
        assert_eq!(expand("file:///srv/pi?x"), PathBuf::from("/srv/pi"));
        assert_eq!(expand("file:///srv/pi#x"), PathBuf::from("/srv/pi"));
        for refused in [
            "file://host/srv",
            "file:///srv/%zz",
            "file:///srv%2Fpi",
            "file://",
        ] {
            assert!(
                expand_pi_path(OsString::from(refused), home).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn the_account_home_answers_when_home_is_unset() {
        assert!(account_home().is_some_and(|home| home.is_absolute()));
    }

    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("tapes-integration-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_file_that_appears_after_capture_is_never_replaced() {
        let root = scratch("appeared");
        let path = root.join(PI_EXTENSION_FILE);
        let staged = root.join("staged");
        fs::write(&path, pi_extension()).unwrap();
        fs::write(&staged, pi_extension()).unwrap();
        let captured = capture(&path, &root).unwrap().unwrap();
        fs::write(&path, "foreign\n").unwrap();
        assert_eq!(
            place(&staged, &path).unwrap_err().kind(),
            ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "foreign\n");
        assert!(captured.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_foreign_capture_is_restored_or_kept_beside_a_newcomer() {
        let root = scratch("restore");
        let path = root.join(PI_EXTENSION_FILE);
        fs::write(&path, "foreign\n").unwrap();
        let captured = capture(&path, &root).unwrap().unwrap();
        assert_eq!(foreign(&captured).unwrap(), Some("is not managed by tapes"));
        restore(&captured, &path, "test");
        assert_eq!(fs::read_to_string(&path).unwrap(), "foreign\n");
        assert!(!captured.exists());

        let captured = capture(&path, &root).unwrap().unwrap();
        fs::write(&path, "newcomer\n").unwrap();
        let message = restore(&captured, &path, "test").to_string();
        assert!(message.contains("is kept as"), "{message}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "newcomer\n");
        assert_eq!(fs::read_to_string(&captured).unwrap(), "foreign\n");
        let _ = fs::remove_dir_all(&root);
    }
}
