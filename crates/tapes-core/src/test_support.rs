use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub(crate) fn with_fifo(test: &str, check: impl FnOnce(&Path)) {
    if std::env::var("TAPES_FIFO_TEST").as_deref() == Ok(test) {
        let directory = tempfile::TempDir::with_prefix("tapes-fifo-").unwrap();
        let path = directory.path().join("fifo");
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        check(&path);
        return;
    }
    // A child killed at the deadline removes nothing, so it writes inside a
    // temp directory this process removes.
    let child_temp = tempfile::TempDir::with_prefix("tapes-fifo-").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("TAPES_FIFO_TEST", test)
        .env("TMPDIR", child_temp.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "FIFO reader subprocess failed");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("reader waited for a FIFO writer");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
