use std::path::Path;
use std::process::Command;

use agent_tapes_core::backend::{backends_from_stores, Query, MAX_READ_BYTES, MIN_READ_BYTES};
use agent_tapes_core::{history, NativeStore, Selection};

#[test]
fn explicit_stores_ignore_ambient_environment() {
    const CHILD: &str = "TAPES_EXPLICIT_STORE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let elsewhere = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/absent");
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "explicit_stores_ignore_ambient_environment"])
            .env(CHILD, "1")
            .env("HOME", &elsewhere)
            .env("CLAUDE_CONFIG_DIR", &elsewhere)
            .env("CODEX_HOME", &elsewhere)
            .env("PI_CODING_AGENT_SESSION_DIR", &elsewhere)
            .env(
                "PI_CODING_AGENT_SESSION_FILE",
                elsewhere.join("absent.jsonl"),
            )
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let before: Vec<_> = std::env::vars_os().collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let stores = [
        (NativeStore::claude(root.join("claude")), "session-claude"),
        (
            NativeStore::codex(root.join("codex")),
            "00000000-0000-0000-0000-000000000001",
        ),
        (NativeStore::pi(root.join("pi")), "session-pi"),
    ];
    for (store, id) in stores {
        let backends = backends_from_stores([store], MIN_READ_BYTES).unwrap();
        assert_eq!(backends.len(), 1);
        let resolved = agent_tapes_core::resolve_session(&backends, id).unwrap();
        let transcript = backends[0].transcript(&resolved.session, 100).unwrap();
        assert!(!transcript.turns.is_empty());
        assert_eq!(transcript.read.unwrap().configured_bound, MIN_READ_BYTES);
        if id == "session-pi" {
            assert!(history::page_with_backends(&backends, Selection::Id(id), None, 1024).is_err());
            continue;
        }
        let page =
            history::page_with_backends(&backends, Selection::Id(id), None, 64 * 1024).unwrap();
        assert_eq!(page.session.id, id);
        assert!(!page.turns.is_empty());
        let needle = &page
            .turns
            .iter()
            .find(|turn| !turn.text.is_empty())
            .unwrap()
            .text;
        let search =
            history::search_with_backends(&backends, Selection::Id(id), None, 64 * 1024, 1, needle)
                .unwrap();
        assert!(!search.matches.is_empty());
        let metadata =
            history::metadata_with_backends(&backends, Selection::Id(id), None, 64 * 1024, 1)
                .unwrap();
        assert_eq!(metadata.session.id, id);
        assert!(!metadata.observations.is_empty());
    }
    let missing =
        backends_from_stores([NativeStore::claude(root.join("absent"))], MIN_READ_BYTES).unwrap();
    assert!(missing[0]
        .list(&Query::unscoped(10))
        .unwrap()
        .sessions
        .is_empty());
    assert!(
        history::page_with_backends(&missing, Selection::Id("session-claude"), None, 1024).is_err()
    );
    assert!(backends_from_stores([], MIN_READ_BYTES).unwrap().is_empty());
    assert!(backends_from_stores([], MIN_READ_BYTES - 1).is_err());
    assert!(backends_from_stores([], MAX_READ_BYTES + 1).is_err());
    assert_eq!(before, std::env::vars_os().collect::<Vec<_>>());
}
