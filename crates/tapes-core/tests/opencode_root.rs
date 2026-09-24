use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tapes_core::backend::{backends, Query};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tapes-opencode-root-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_env(name: &str, value: impl AsRef<std::ffi::OsStr>) -> Option<OsString> {
    let old = std::env::var_os(name);
    std::env::set_var(name, value);
    old
}

fn restore_env(name: &str, value: Option<OsString>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}

fn fake_opencode2(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        r##"#!/usr/bin/python3
import json, os, sys

def session(root):
    return {
        "id": "ses_native",
        "title": root,
        "directory": "/fixtures/project",
        "time": {"created": 1784663070000, "updated": 1784663071000},
        "location": {"directory": "/fixtures/project"},
        "model": {"id": "fixture-model", "variant": "high"}
    }

def messages(root):
    return {
        "data": [{
            "id": "msg_fixture",
            "type": "user",
            "time": {"created": 1784663071000},
            "text": root
        }],
        "cursor": {"next": None}
    }

root = os.environ.get("XDG_DATA_HOME", "")
if sys.argv[1] == "serve":
    from http.server import BaseHTTPRequestHandler, HTTPServer
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            route = self.path.split("?", 1)[0]
            body = messages(root) if route.endswith("/message") else {"data": [session(root)]}
            encoded = json.dumps(body).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)
        def log_message(self, *_):
            pass
    HTTPServer(("127.0.0.1", int(sys.argv[5])), Handler).serve_forever()
elif sys.argv[1] == "api":
    path = sys.argv[4]
    route = path.split("?", 1)[0]
    if route == "/api/session":
        response = {"data": [session(root)]}
    elif route.endswith("/message"):
        response = messages(root)
    else:
        response = {"data": session(root)}
    print(json.dumps(response))
"##,
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn opencode_metadata_transcript_and_search_use_the_captured_store_root() {
    let temp = Temp::new();
    let bin = temp.path().join("bin");
    let home = temp.path().join("home");
    let root_a = temp.path().join("root-a");
    let root_b = temp.path().join("root-b");
    for root in [&root_a, &root_b] {
        fs::create_dir_all(root.join("opencode")).unwrap();
        fs::write(root.join("opencode/opencode-next.db"), b"SQLite format 3\0").unwrap();
    }
    fake_opencode2(&bin.join("opencode2"));

    let old_home = set_env("HOME", &home);
    let old_xdg = set_env("XDG_DATA_HOME", &root_a);
    let old_path = set_env(
        "PATH",
        std::env::join_paths([bin.as_os_str(), OsStr::new("/usr/bin"), OsStr::new("/bin")])
            .unwrap(),
    );
    let old_claude = set_env("CLAUDE_CONFIG_DIR", temp.path().join("absent-claude"));
    let old_codex = set_env("CODEX_HOME", temp.path().join("absent-codex"));
    let old_pi = set_env("PI_CODING_AGENT_SESSION_DIR", temp.path().join("absent-pi"));

    let backends = backends();
    let backend = backends
        .iter()
        .find(|backend| {
            backend
                .store()
                .is_some_and(|store| store.contains("opencode2"))
        })
        .expect("default v2 backend is present");
    std::env::set_var("XDG_DATA_HOME", &root_b);

    let session = backend.locate("ses_native").unwrap().unwrap();
    let root_a_text = root_a.to_string_lossy();
    let root_b_text = root_b.to_string_lossy();
    assert_eq!(session.title.as_deref(), Some(root_a_text.as_ref()));

    let transcript = backend.transcript(&session, 10).unwrap();
    assert!(transcript
        .turns
        .iter()
        .any(|turn| turn.text == root_a_text.as_ref()));
    assert!(!transcript
        .turns
        .iter()
        .any(|turn| turn.text == root_b_text.as_ref()));

    let query = Query::unscoped(10);
    let from_a = backend
        .list_with_search(&query, root_a.to_str().unwrap(), 10)
        .unwrap();
    assert_eq!(from_a.sessions.len(), 1);
    assert_eq!(from_a.sessions[0].id, "ses_native");
    let from_b = backend
        .list_with_search(&query, root_b.to_str().unwrap(), 10)
        .unwrap();
    assert!(from_b.sessions.is_empty());

    restore_env("PI_CODING_AGENT_SESSION_DIR", old_pi);
    restore_env("CODEX_HOME", old_codex);
    restore_env("CLAUDE_CONFIG_DIR", old_claude);
    restore_env("PATH", old_path);
    restore_env("XDG_DATA_HOME", old_xdg);
    restore_env("HOME", old_home);
}
