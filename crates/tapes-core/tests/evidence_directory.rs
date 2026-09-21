#![cfg(unix)]

use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tapes_core::backend::{Backend, Query};
use tapes_core::evidence;
use tapes_core::input::{InputBackend, InputFormat, InputOptions};
use tapes_core::model::Transcript;

const MAX_MEMBER_BYTES: u64 = 512 * 1024 * 1024;

#[test]
fn a_leaf_symlink_replaced_after_the_read_is_not_copied() {
    let mut fixture = Fixture::new("leaf-symlink");
    let transcript = fixture.transcript("file-report.dat");
    let outside = fixture.outside_file("leaf-marker.dat");
    let report = fixture.root.join("file-report.dat");
    fs::remove_file(&report).unwrap();
    symlink(&outside, &report).unwrap();

    let manifest = fixture.write_evidence(&transcript);

    assert_report_gap(&manifest, "file-report.dat");
    assert_eq!(manifest["associations"][0]["copied"], false);
    assert!(!manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| { file["role"] == "report" }));
}

#[test]
fn an_ancestor_symlink_replaced_after_the_read_is_not_copied() {
    let mut fixture = Fixture::new("ancestor-symlink");
    let transcript = fixture.transcript("nested/file-report.dat");
    let outside = fixture.outside_directory("ancestor-target");
    fs::write(outside.join("file-report.dat"), b"outside marker").unwrap();
    let report = fixture.root.join("nested/file-report.dat");
    let nested = fixture.root.join("nested");
    fs::remove_file(report).unwrap();
    fs::remove_dir(nested).unwrap();
    symlink(&outside, fixture.root.join("nested")).unwrap();

    let manifest = fixture.write_evidence(&transcript);

    assert_report_gap(&manifest, "nested/file-report.dat");
    assert_eq!(manifest["associations"][0]["copied"], false);
    assert!(!manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|file| { file["role"] == "report" }));
}

#[test]
fn a_nonregular_report_replacement_is_not_copied() {
    let fixture = Fixture::new("nonregular");
    let transcript = fixture.transcript("file-report.dat");
    let report = fixture.root.join("file-report.dat");
    fs::remove_file(&report).unwrap();
    fs::create_dir(&report).unwrap();

    let manifest = fixture.write_evidence(&transcript);

    assert_report_gap(&manifest, "file-report.dat");
    assert_eq!(manifest["associations"][0]["copied"], false);
}

#[test]
fn a_valid_nested_report_is_copied_verbatim() {
    let fixture = Fixture::new("nested-valid");
    let transcript = fixture.transcript("nested/file-report.dat");

    let manifest = fixture.write_evidence(&transcript);

    assert_eq!(manifest["gaps"], json!([]));
    assert_eq!(manifest["associations"][0]["copied"], true);
    let report = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["role"] == "report")
        .unwrap();
    assert_eq!(
        fs::read(
            fixture
                .evidence_dir()
                .join(report["file"].as_str().unwrap())
        )
        .unwrap(),
        report_bytes()
    );
}

#[test]
fn a_traversal_member_name_is_not_copied() {
    let fixture = Fixture::new("traversal");
    let mut transcript = fixture.transcript("file-report.dat");
    transcript.artifacts[0].path = Some("../outside.dat".to_owned());

    let manifest = fixture.write_evidence(&transcript);

    assert_report_gap(&manifest, "../outside.dat");
    assert_eq!(manifest["associations"][0]["copied"], false);
}

#[test]
fn a_report_that_exceeds_the_member_bound_is_not_read() {
    let fixture = Fixture::new("member-bound");
    let transcript = fixture.transcript("file-report.dat");
    let report = fixture.root.join("file-report.dat");
    let file = File::create(report).unwrap();
    file.set_len(MAX_MEMBER_BYTES + 1).unwrap();

    let manifest = fixture.write_evidence(&transcript);

    assert_report_gap(&manifest, "file-report.dat");
    assert!(manifest["gaps"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("over the"));
}

struct Fixture {
    root: PathBuf,
    extras: Vec<PathBuf>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-evidence-directory-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("conversations.json"), conversation_bytes()).unwrap();
        Self {
            root,
            extras: Vec::new(),
        }
    }

    fn transcript(&self, report_member: &str) -> Transcript {
        let report = self.root.join(report_member);
        if let Some(parent) = report.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&report, report_bytes()).unwrap();

        let backend = InputBackend::new(InputOptions::new(
            vec![self.root.clone()],
            InputFormat::Openai,
        ))
        .unwrap();
        let session = backend
            .list(&Query::unscoped(10))
            .unwrap()
            .sessions
            .into_iter()
            .find(|session| session.id == "audit-1")
            .unwrap();
        let transcript = backend.transcript(&session, usize::MAX).unwrap();
        assert_eq!(transcript.artifacts.len(), 1);
        assert_eq!(transcript.artifacts[0].path.as_deref(), Some(report_member));
        transcript
    }

    fn outside_file(&mut self, name: &str) -> PathBuf {
        let path = self.root.with_file_name(format!(
            "{}-{name}",
            self.root.file_name().unwrap().to_string_lossy()
        ));
        fs::write(&path, b"outside marker").unwrap();
        self.extras.push(path.clone());
        path
    }

    fn outside_directory(&mut self, name: &str) -> PathBuf {
        let path = self.root.with_file_name(format!(
            "{}-{name}",
            self.root.file_name().unwrap().to_string_lossy()
        ));
        fs::create_dir(&path).unwrap();
        self.extras.push(path.clone());
        path
    }

    fn write_evidence(&self, transcript: &Transcript) -> Value {
        let bundle = self.root.join("bundle");
        fs::create_dir(&bundle).unwrap();
        let manifest = evidence::write(transcript, &bundle.join("projection.json")).unwrap();
        serde_json::from_slice(&fs::read(manifest.path).unwrap()).unwrap()
    }

    fn evidence_dir(&self) -> PathBuf {
        self.root.join("bundle/projection.evidence")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        for extra in &self.extras {
            let _ = fs::remove_dir_all(extra);
            let _ = fs::remove_file(extra);
        }
    }
}

fn assert_report_gap(manifest: &Value, member: &str) {
    assert!(manifest["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .any(|gap| { gap["member"] == member }));
}

fn conversation_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!([{
        "id": "audit-1",
        "current_node": "n",
        "mapping": {
            "n": {
                "id": "n",
                "parent": null,
                "message": {
                    "id": "m",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["audit"]}
                }
            }
        }
    }]))
    .unwrap()
}

fn report_bytes() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "widget_session_id": "report-1",
        "backing_conversation_id": "audit-1",
        "widget_state": {
            "status": "completed",
            "report_message": {
                "author": {"role": "assistant"},
                "content": {"parts": ["report"]}
            }
        }
    }))
    .unwrap()
}
