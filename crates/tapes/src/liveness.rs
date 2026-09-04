use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tapes_core::endings::Ending;
use tapes_core::model::{LiveState, Session};

const MAX_STATUS_BYTES: usize = 64 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Deserialize)]
struct Snapshot {
    threads: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Thread {
    id: String,
    state: AuthorityState,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AuthorityState {
    Working,
    Completed,
    Idle,
    Attention,
}

/// Read the current registry once. Every failure means that present-tense
/// state is unknown, so callers retain the recording-only result. The
/// authority is polled without blocking on either its pipe or its exit.
pub fn snapshot() -> HashMap<String, LiveState> {
    let Ok(mut child) = spawn_status() else {
        return HashMap::new();
    };

    let Some(mut stdout) = child.stdout.take() else {
        terminate_child(&mut child);
        return HashMap::new();
    };

    let Some((bytes, status)) = read_status(&mut child, &mut stdout) else {
        return HashMap::new();
    };
    if !status.success() {
        return HashMap::new();
    }

    let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&bytes) else {
        return HashMap::new();
    };
    snapshot
        .threads
        .into_iter()
        .filter_map(|value| {
            // Deserialize each entry independently. A future state on one
            // thread must not make known states for other threads vanish.
            let thread = serde_json::from_value::<Thread>(value).ok()?;
            let state = match thread.state {
                AuthorityState::Working => LiveState::Working,
                AuthorityState::Completed | AuthorityState::Idle | AuthorityState::Attention => {
                    LiveState::Idle
                }
            };
            Some((thread.id, state))
        })
        .collect()
}

fn spawn_status() -> io::Result<Child> {
    let mut command = Command::new("harness-status");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // A status helper may leave descendants holding stdout open. Isolating
    // its process group lets timeout cleanup close those inherited writers.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

fn read_status(child: &mut Child, stdout: &mut ChildStdout) -> Option<(Vec<u8>, ExitStatus)> {
    if set_nonblocking(stdout).is_err() {
        terminate_child(child);
        return None;
    }

    let deadline = Instant::now() + STATUS_TIMEOUT;
    let mut bytes = Vec::new();
    let mut stdout_open = true;
    let mut status = None;

    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => status = Some(exit),
                Ok(None) => {}
                Err(_) => {
                    terminate_child(child);
                    return None;
                }
            }
        }
        if let Some(exit) = status {
            if !stdout_open {
                return Some((bytes, exit));
            }
        }

        let Some(timeout) = remaining_millis(deadline) else {
            terminate_child(child);
            return None;
        };
        if stdout_open {
            let mut descriptor = libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let polled = unsafe { libc::poll(&mut descriptor, 1, timeout) };
            if polled == -1 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                terminate_child(child);
                return None;
            }
            if polled == 0 {
                terminate_child(child);
                return None;
            }
            if descriptor.revents & libc::POLLNVAL != 0 {
                terminate_child(child);
                return None;
            }

            let mut chunk = [0_u8; 8192];
            match stdout.read(&mut chunk) {
                Ok(0) => stdout_open = false,
                Ok(read) => {
                    bytes.extend_from_slice(&chunk[..read]);
                    if bytes.len() > MAX_STATUS_BYTES {
                        terminate_child(child);
                        return None;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    terminate_child(child);
                    return None;
                }
            }
        } else {
            // stdout was closed before the helper exited. Keep its wall time
            // bounded instead of falling into an unbounded wait.
            let polled = unsafe { libc::poll(std::ptr::null_mut(), 0, timeout) };
            if polled == -1 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                terminate_child(child);
                return None;
            }
        }
    }
}

fn set_nonblocking(stdout: &ChildStdout) -> io::Result<()> {
    let descriptor = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn remaining_millis(deadline: Instant) -> Option<i32> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    Some(remaining.as_millis().clamp(1, i32::MAX as u128) as i32)
}

fn terminate_child(child: &mut Child) {
    let process_group = -(child.id() as libc::pid_t);
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn annotate(sessions: &mut [Session]) {
    let live = snapshot();
    for session in sessions {
        session.live = live.get(&session.id).cloned();
    }
}

/// The same join onto the sessions an endings report names, from one snapshot
/// however many endings it holds.
pub fn annotate_endings(endings: &mut [Ending]) {
    let live = snapshot();
    for ending in endings {
        ending.session.live = live.get(&ending.session.id).cloned();
    }
}
