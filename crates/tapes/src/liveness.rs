use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};

use serde::Deserialize;
use tapes_core::model::{LiveState, Session};

const MAX_STATUS_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct Snapshot {
    threads: Vec<Thread>,
}

#[derive(Debug, Deserialize)]
struct Thread {
    id: String,
    #[allow(dead_code)]
    harness: String,
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
/// state is unknown, so callers retain the recording-only result.
pub fn snapshot() -> HashMap<String, LiveState> {
    let Ok(mut child) = Command::new("harness-status")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return HashMap::new();
    };

    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.wait();
        return HashMap::new();
    };
    let mut bytes = Vec::new();
    let read = stdout
        .by_ref()
        .take((MAX_STATUS_BYTES + 1) as u64)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > MAX_STATUS_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return HashMap::new();
    }
    let Ok(status) = child.wait() else {
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
        .map(|thread| {
            let state = match thread.state {
                AuthorityState::Working => LiveState::Working,
                AuthorityState::Completed | AuthorityState::Idle | AuthorityState::Attention => {
                    LiveState::Idle
                }
            };
            (thread.id, state)
        })
        .collect()
}

pub fn annotate(sessions: &mut [Session]) {
    let live = snapshot();
    for session in sessions {
        session.live = live.get(&session.id).cloned();
    }
}
