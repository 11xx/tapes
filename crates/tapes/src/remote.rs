//! Bounded queries against a named replica over ssh.
//!
//! A replica answers with its own `tapes` build's JSON, so nothing here
//! understands a harness store: the transport carries a command, reads a
//! bounded answer, and refuses when there is none. Every way a replica can
//! fail to answer is classified and named beside the destination the caller
//! asked for.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The program that carries a query. A wrapper, a fixed configuration file,
/// or a jump host is a deployment choice, so the transport resolves through
/// this variable rather than a hard-coded `ssh`.
pub const SSH_VARIABLE: &str = "TAPES_SSH";
/// What this machine accepts from one replica answer unless the operator
/// raises it. A remote read is asked for bounded output and the bound is
/// enforced here as well, because a replica runs another machine's program and
/// its answer can be anything.
pub const MAX_BYTES_VARIABLE: &str = "TAPES_REMOTE_MAX_BYTES";
pub const DEADLINE_VARIABLE: &str = "TAPES_REMOTE_DEADLINE_MS";

const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_DEADLINE: Duration = Duration::from_secs(60);
/// How much of a failing command's stderr is kept for the cause. The rest is
/// drained so the child never blocks on a full pipe.
const MAX_STDERR_BYTES: usize = 8 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const CLOSED_PIPE_POLL: Duration = Duration::from_millis(5);

/// Why a replica could not answer. The destination is part of the fact: a
/// caller asking two replicas must be able to tell whose transport failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    destination: String,
    cause: String,
}

impl Unavailable {
    pub(crate) fn new(destination: &str, cause: impl Into<String>) -> Self {
        Self {
            destination: destination.to_owned(),
            cause: cause.into(),
        }
    }

    /// The same fact as it appears in a listing's `unavailable` diagnostics.
    pub fn diagnostic(&self) -> String {
        format!("replica {}: {}", self.destination, self.cause)
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "remote replica {} unavailable: {}",
            self.destination, self.cause
        )
    }
}

impl std::error::Error for Unavailable {}

/// The local half of the transport: which program carries a query and how much
/// of an answer this machine accepts.
#[derive(Debug, Clone)]
pub struct Settings {
    program: OsString,
    /// Whether the program is this tool's own default `ssh`. Only the default
    /// is invoked with the option that keeps a prompt from stopping the query;
    /// a program an operator named owns its own behavior.
    batch_mode: bool,
    max_bytes: usize,
    deadline: Duration,
}

impl Settings {
    pub fn resolve() -> anyhow::Result<Self> {
        let (program, batch_mode) = match std::env::var_os(SSH_VARIABLE) {
            Some(program) => (program, false),
            None => (OsString::from("ssh"), true),
        };
        if program.is_empty() {
            return Err(anyhow!("{SSH_VARIABLE} names no program"));
        }
        let max_bytes = match std::env::var(MAX_BYTES_VARIABLE) {
            Ok(value) => value
                .parse::<usize>()
                .ok()
                .filter(|bytes| *bytes > 0)
                .ok_or_else(|| anyhow!("{MAX_BYTES_VARIABLE} is not a positive byte count"))?,
            Err(_) => DEFAULT_MAX_BYTES,
        };
        let deadline = match std::env::var(DEADLINE_VARIABLE) {
            Ok(value) => value
                .parse::<u64>()
                .ok()
                .filter(|millis| *millis > 0)
                .map(Duration::from_millis)
                .ok_or_else(|| {
                    anyhow!("{DEADLINE_VARIABLE} is not a positive millisecond count")
                })?,
            Err(_) => DEFAULT_DEADLINE,
        };
        Ok(Self {
            program,
            batch_mode,
            max_bytes,
            deadline,
        })
    }
}

/// A named replica: where a query goes, not what it holds.
pub struct Replica {
    destination: String,
    settings: Settings,
}

impl Replica {
    pub fn new(destination: &str, settings: Settings) -> Self {
        Self {
            destination: destination.to_owned(),
            settings,
        }
    }

    /// Ask the replica's own `tapes` and read its JSON answer. `args` names
    /// the remote command's arguments without `--json`, which this adds.
    pub fn json<T: DeserializeOwned>(
        &self,
        args: &[OsString],
        schema: &str,
    ) -> std::result::Result<T, Unavailable> {
        let bytes = self.answer(args, true)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            Unavailable::new(
                &self.destination,
                format!("the answer is not JSON: {error}"),
            )
        })?;
        let answer_schema = value.get("schema").and_then(Value::as_str).ok_or_else(|| {
            Unavailable::new(
                &self.destination,
                "the answer names no schema; an unrelated reader answered",
            )
        })?;
        if answer_schema != schema {
            return Err(Unavailable::new(
                &self.destination,
                format!("unsupported remote schema {answer_schema}; this reader reads {schema}"),
            ));
        }
        serde_json::from_value(value).map_err(|error| {
            Unavailable::new(
                &self.destination,
                format!("the {schema} answer does not fit this reader: {error}"),
            )
        })
    }

    /// Ask the replica's own `tapes` and read its output as the command
    /// writes it. Used where the answer is not a schema object, so the caller
    /// reads the command's own stdout contract.
    pub fn output(&self, args: &[OsString]) -> std::result::Result<Vec<u8>, Unavailable> {
        self.answer(args, false)
    }

    fn answer(&self, args: &[OsString], json: bool) -> std::result::Result<Vec<u8>, Unavailable> {
        let command = remote_command(args, json)
            .map_err(|cause| Unavailable::new(&self.destination, cause))?;
        let mut child = self.spawn(&command)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Unavailable::new(&self.destination, "the transport has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Unavailable::new(&self.destination, "the transport has no stderr"))?;
        let mut pipes = match Pipes::new(stdout, stderr) {
            Ok(pipes) => pipes,
            Err(error) => {
                self.terminate(&mut child);
                return Err(Unavailable::new(
                    &self.destination,
                    format!("cannot read the transport: {error}"),
                ));
            }
        };

        let deadline = Instant::now() + self.settings.deadline;
        let mut status = None;
        loop {
            if status.is_none() {
                status = match child.try_wait() {
                    Ok(exit) => exit,
                    Err(error) => {
                        self.terminate(&mut child);
                        return Err(Unavailable::new(
                            &self.destination,
                            format!("cannot watch the transport: {error}"),
                        ));
                    }
                };
            }
            if pipes.closed() && status.is_some() {
                break;
            }
            let Some(timeout) = remaining_millis(deadline) else {
                self.terminate(&mut child);
                return Err(self.timeout());
            };
            if pipes.closed() {
                std::thread::sleep(CLOSED_PIPE_POLL);
                continue;
            }
            match pipes.await_and_drain(timeout) {
                Ok(true) => {}
                Ok(false) => {
                    self.terminate(&mut child);
                    return Err(self.timeout());
                }
                Err(error) => {
                    self.terminate(&mut child);
                    return Err(Unavailable::new(
                        &self.destination,
                        format!("cannot read the transport: {error}"),
                    ));
                }
            }
            if pipes.stdout.len() > self.settings.max_bytes {
                self.terminate(&mut child);
                return Err(Unavailable::new(
                    &self.destination,
                    format!(
                        "the answer exceeds the {}-byte local bound; raise \
                         {MAX_BYTES_VARIABLE} or ask the replica for less",
                        self.settings.max_bytes
                    ),
                ));
            }
        }

        let exit = status.expect("the loop ends only after the transport exited");
        if !exit.success() {
            let cause = self.exit_cause(&exit, &pipes.stderr);
            return Err(Unavailable::new(&self.destination, cause));
        }
        if pipes.stdout.is_empty() {
            return Err(Unavailable::new(
                &self.destination,
                "the transport succeeded but the replica returned no output",
            ));
        }
        // The replica's own notes and warnings are part of its answer, so
        // they are passed on rather than dropped with the transport's exit.
        if !pipes.stderr.is_empty() {
            let _ = std::io::stderr().write_all(&pipes.stderr);
        }
        Ok(pipes.stdout)
    }

    fn timeout(&self) -> Unavailable {
        Unavailable::new(
            &self.destination,
            format!(
                "no answer within {}ms; raise {DEADLINE_VARIABLE} for a slower replica",
                self.settings.deadline.as_millis()
            ),
        )
    }

    fn spawn(&self, remote_command: &str) -> std::result::Result<Child, Unavailable> {
        let mut command = Command::new(&self.settings.program);
        if self.settings.batch_mode {
            // Default ssh reads a host-key question or a password from
            // /dev/tty. A query runs in the background with no terminal of
            // its own, so that read stops the transport until the deadline
            // and the prompt would be reported as the replica being slow.
            // BatchMode turns the prompt into an immediate failure instead.
            command.arg("-o").arg("BatchMode=yes");
        }
        command
            .arg(&self.destination)
            .arg(remote_command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A transport that leaves descendants holding stdout open would
        // otherwise outlive its own exit. Isolating its process group lets
        // timeout and bound cleanup close those inherited writers.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().map_err(|error| {
            Unavailable::new(
                &self.destination,
                format!(
                    "cannot run {} ({SSH_VARIABLE}): {error}",
                    self.settings.program.to_string_lossy()
                ),
            )
        })
    }

    fn exit_cause(&self, exit: &ExitStatus, stderr: &[u8]) -> String {
        let reason = match exit.code() {
            Some(255) => "the ssh transport failed".to_owned(),
            Some(127) => "no tapes command on the replica".to_owned(),
            Some(126) => "the replica's tapes command is not executable".to_owned(),
            Some(code) => format!("the remote command failed with exit status {code}"),
            None => "the remote command was terminated by a signal".to_owned(),
        };
        match stderr_detail(stderr) {
            Some(detail) => format!("{reason}: {detail}"),
            None => reason,
        }
    }

    fn terminate(&self, child: &mut Child) {
        // The transport is its own process group, so a shell or multiplexer
        // holding the pipes goes down with it.
        let process_group = -(child.id() as libc::pid_t);
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The command string ssh hands to the replica's shell. Every argument is
/// quoted, because a destination and a command are one shell word list: an
/// argument carrying a space, a quote, or a metacharacter must reach the
/// remote `tapes` as the caller wrote it, and nothing in a title or a search
/// string may be interpreted there. `json` appends the flag that asks for a
/// schema object, for a command whose answer is one.
fn remote_command(args: &[OsString], json: bool) -> std::result::Result<String, String> {
    let mut command = String::from("tapes");
    for arg in args {
        command.push(' ');
        command.push_str(&quote(arg)?);
    }
    if json {
        command.push_str(" --json");
    }
    Ok(command)
}

fn quote(arg: &OsStr) -> std::result::Result<String, String> {
    let text = arg
        .to_str()
        .ok_or_else(|| format!("a remote query cannot carry {arg:?}"))?;
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for character in text.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    Ok(quoted)
}

fn stderr_detail(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.lines().collect::<Vec<_>>().join(" "))
}

fn remaining_millis(deadline: Instant) -> Option<i32> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    Some(remaining.as_millis().clamp(1, i32::MAX as u128) as i32)
}

/// Both answer pipes of one transport, read without blocking so the deadline
/// covers a command that never exits as well as one that never speaks.
struct Pipes {
    stdout_pipe: ChildStdout,
    stderr_pipe: ChildStderr,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_open: bool,
    stderr_open: bool,
}

impl Pipes {
    fn new(stdout_pipe: ChildStdout, stderr_pipe: ChildStderr) -> std::io::Result<Self> {
        set_nonblocking(stdout_pipe.as_raw_fd())?;
        set_nonblocking(stderr_pipe.as_raw_fd())?;
        Ok(Self {
            stdout_pipe,
            stderr_pipe,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_open: true,
            stderr_open: true,
        })
    }

    fn closed(&self) -> bool {
        !self.stdout_open && !self.stderr_open
    }

    /// Poll the open pipes and take what is ready. `false` means the poll
    /// timed out with nothing left to read from an open pipe.
    fn await_and_drain(&mut self, timeout: i32) -> std::io::Result<bool> {
        let mut descriptors = Vec::new();
        let has_stdout = self.stdout_open;
        let has_stderr = self.stderr_open;
        if has_stdout {
            descriptors.push(libc::pollfd {
                fd: self.stdout_pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if has_stderr {
            descriptors.push(libc::pollfd {
                fd: self.stderr_pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout,
            )
        };
        if ready == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                return Ok(true);
            }
            return Err(error);
        }
        if ready == 0 {
            return Ok(false);
        }
        let stderr_index = descriptors.len() - 1;
        // A hangup or error is also a reason to read: the read reports the
        // end of the pipe or its failure, and a WouldBlock leaves it open.
        if has_stdout && descriptors[0].revents != 0 {
            self.drain_stdout()?;
        }
        if has_stderr && descriptors[stderr_index].revents != 0 {
            self.drain_stderr()?;
        }
        Ok(true)
    }

    fn drain_stdout(&mut self) -> std::io::Result<()> {
        let mut chunk = [0_u8; READ_CHUNK];
        match self.stdout_pipe.read(&mut chunk) {
            Ok(0) => self.stdout_open = false,
            Ok(bytes) => self.stdout.extend_from_slice(&chunk[..bytes]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn drain_stderr(&mut self) -> std::io::Result<()> {
        let mut chunk = [0_u8; 4096];
        match self.stderr_pipe.read(&mut chunk) {
            Ok(0) => self.stderr_open = false,
            Ok(bytes) => {
                let room = MAX_STDERR_BYTES.saturating_sub(self.stderr.len());
                self.stderr.extend_from_slice(&chunk[..bytes.min(room)]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }
}

fn set_nonblocking(descriptor: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_remote_command_quotes_every_word_and_asks_for_json() {
        let command = remote_command(&args(&["show", "a b", "--title", "it's"]), true).unwrap();
        assert_eq!(command, "tapes 'show' 'a b' '--title' 'it'\\''s' --json");
        let plain = remote_command(&args(&["export", "a b"]), false).unwrap();
        assert_eq!(plain, "tapes 'export' 'a b'");
    }

    #[test]
    fn a_non_unicode_argument_cannot_cross_a_shell_word_list() {
        use std::os::unix::ffi::OsStringExt as _;
        let argument = OsString::from_vec(vec![b'a', 0x80]);
        let error = remote_command(&[argument], true).unwrap_err();
        assert!(error.contains("cannot carry"), "{error}");
    }

    #[test]
    fn an_unavailable_names_its_destination_and_cause() {
        let unavailable = Unavailable::new("agent@host", "no tapes command on the replica");
        assert_eq!(
            unavailable.diagnostic(),
            "replica agent@host: no tapes command on the replica"
        );
        assert_eq!(
            unavailable.to_string(),
            "remote replica agent@host unavailable: no tapes command on the replica"
        );
    }
}
