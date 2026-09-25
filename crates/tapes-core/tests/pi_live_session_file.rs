use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tapes_core::backend;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tapes-pi-live-session-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}

#[test]
fn native_pi_backend_reads_the_live_session_file() {
    let temp = Temp::new();
    let old_home = std::env::var_os("HOME");
    let old_file = std::env::var_os("PI_SESSION_FILE");
    let old_dir = std::env::var_os("PI_CODING_AGENT_DIR");
    let old_sessions = std::env::var_os("PI_CODING_AGENT_SESSION_DIR");
    let path = temp.0.join("runtime-session.jsonl");
    fs::write(
        &path,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-live\",\"timestamp\":\"2026-01-01T10:00:00Z\",\"cwd\":\"/fixtures/project\"}\n",
            "{\"type\":\"model_change\",\"id\":\"model-1\",\"parentId\":null,\"timestamp\":\"2026-01-01T10:00:01Z\",\"provider\":\"fixture-provider\",\"modelId\":\"pi-model-old\"}\n",
            "{\"type\":\"thinking_level_change\",\"id\":\"thinking-1\",\"parentId\":\"model-1\",\"timestamp\":\"2026-01-01T10:00:02Z\",\"thinkingLevel\":\"high\"}\n",
            "{\"type\":\"message\",\"id\":\"user-1\",\"parentId\":\"thinking-1\",\"timestamp\":\"2026-01-01T10:00:03Z\",\"message\":{\"role\":\"user\",\"timestamp\":1767261603000,\"content\":[{\"type\":\"text\",\"text\":\"Inspect the fixture.\"}]}}\n",
            "{\"type\":\"message\",\"id\":\"assistant-1\",\"parentId\":\"user-1\",\"timestamp\":\"2026-01-01T10:00:04Z\",\"message\":{\"role\":\"assistant\",\"timestamp\":1767261604000,\"provider\":\"fixture-provider\",\"model\":\"pi-model-live\",\"content\":[{\"type\":\"text\",\"text\":\"Live Pi answer.\"}]}}\n"
        ),
    )
    .unwrap();

    std::env::set_var("HOME", temp.0.join("home"));
    std::env::remove_var("PI_CODING_AGENT_DIR");
    std::env::remove_var("PI_CODING_AGENT_SESSION_DIR");
    std::env::set_var("PI_SESSION_FILE", &path);

    let backends = backend::backends();
    let pi = backends
        .iter()
        .find(|backend| backend.harness() == "pi")
        .unwrap();
    let session = pi.locate("session-live").unwrap().unwrap();
    let transcript = pi.transcript(&session, usize::MAX).unwrap();
    assert_eq!(session.model.as_ref().unwrap().id, "pi-model-live");
    assert_eq!(
        session.model.as_ref().unwrap().variant.as_deref(),
        Some("high")
    );
    assert!(transcript
        .turns
        .iter()
        .any(|turn| turn.text == "Live Pi answer."));

    restore_env("HOME", old_home);
    restore_env("PI_SESSION_FILE", old_file);
    restore_env("PI_CODING_AGENT_DIR", old_dir);
    restore_env("PI_CODING_AGENT_SESSION_DIR", old_sessions);
}
