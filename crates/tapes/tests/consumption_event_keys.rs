use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

static TEMP_ROOTS: AtomicU64 = AtomicU64::new(0);

const PARENT: &str = "2026-01-15T10-00-00-000Z_keys-parent.jsonl";
const FORK: &str = "2026-01-15T11-00-00-000Z_keys-fork.jsonl";

struct Store(PathBuf);

impl Store {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tapes-event-keys-{}-{}-{nonce}",
            std::process::id(),
            TEMP_ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tapes-core/tests/fixtures/pi-consumption-keys");
        fs::copy(fixtures.join(PARENT), sessions.join(PARENT)).unwrap();
        let fork = fs::read_to_string(fixtures.join(FORK)).unwrap();
        let (header, entries) = fork.split_once('\n').unwrap();
        let mut header: Value = serde_json::from_str(header).unwrap();
        header["parentSession"] = serde_json::json!(sessions.join(PARENT));
        fs::write(sessions.join(FORK), format!("{header}\n{entries}")).unwrap();
        Self(root)
    }

    fn run(&self, id: &str, options: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_tapes"))
            .args(["usage", id, "--native-store", "pi", "--native-root"])
            .arg(&self.0)
            .args(options)
            .output()
            .unwrap()
    }

    fn usage(&self, id: &str, options: &[&str]) -> Value {
        let mut options = options.to_vec();
        options.push("--json");
        let output = self.run(id, &options);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn edit(&self, file: &str, edit: impl Fn(&mut Value)) {
        let path = self.0.join("sessions").join(file);
        let lines = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                edit(&mut entry);
                entry.to_string()
            })
            .collect::<Vec<_>>();
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn bounded_event_keys_establish_only_verified_parent_fork_equality() {
    let store = Store::new();
    for full in [false, true] {
        let options = if full {
            vec!["--event-keys", "--full"]
        } else {
            vec!["--event-keys"]
        };
        let parent = store.usage("keys-parent", &options);
        let fork = store.usage("keys-fork", &options);
        let mut union = BTreeSet::new();
        for view in [&parent, &fork] {
            let series = &view["consumption_event_keys"];
            assert_eq!(series["observed"], 3);
            assert_eq!(series["returned"], 3);
            assert_eq!(series["keys"][0]["kind"], "provider-response");
            assert_eq!(
                series["keys"][0]["provider_account_namespace"],
                "unestablished"
            );
            assert_eq!(series["keys"][1]["kind"], "recording-entry");
            for key in series["keys"].as_array().unwrap() {
                let coordinate = key.get("copy_of").unwrap_or(&key["recording_entry"]);
                union.insert((
                    coordinate["session_id"].as_str().unwrap(),
                    coordinate["entry_id"].as_str().unwrap(),
                ));
            }
            let output = series.to_string();
            assert!(!output.contains(store.0.to_str().unwrap()), "{output}");
            assert!(!output.contains(".jsonl"), "{output}");
        }
        assert_eq!(union.len(), 4);
        assert_eq!(
            fork["consumption_event_keys"]["keys"][0]["copy_of"]["session_id"],
            "keys-parent"
        );
        assert!(fork["consumption_event_keys"]["keys"][2]
            .get("copy_of")
            .is_none());
    }
}

#[test]
fn key_caps_report_omissions_without_changing_measurements() {
    let store = Store::new();
    let plain = store.usage("keys-parent", &[]);
    assert!(plain.get("consumption_event_keys").is_none());
    for full in [false, true] {
        let options = if full {
            vec!["--event-keys=1", "--full"]
        } else {
            vec!["--event-keys=1"]
        };
        let view = store.usage("keys-parent", &options);
        let series = &view["consumption_event_keys"];
        assert_eq!(series["observed"], 3);
        assert_eq!(series["returned"], 1);
        assert_eq!(series["omissions"]["row_cap"], 2);
        assert_eq!(series["keys"][0]["response_id"], "response-parent");
        assert_eq!(view["consumption"], plain["consumption"]);
        assert_eq!(view["tokens"], plain["tokens"]);
    }
}

#[test]
fn repeated_observations_return_one_event_key() {
    let store = Store::new();
    let path = store.0.join("sessions").join(PARENT);
    let contents = fs::read_to_string(&path).unwrap();
    let mut repeated: Value = serde_json::from_str(contents.lines().nth(1).unwrap()).unwrap();
    repeated["id"] = serde_json::json!("repeat-request");
    repeated["parentId"] = serde_json::json!("parent-request");
    fs::write(path, format!("{contents}{repeated}\n")).unwrap();
    for full in [false, true] {
        let options = if full {
            vec!["--event-keys", "--full"]
        } else {
            vec!["--event-keys"]
        };
        let view = store.usage("keys-parent", &options);
        assert_eq!(view["consumption_event_keys"]["observed"], 3);
        assert_eq!(view["consumption_event_keys"]["returned"], 3);
        assert_eq!(view["consumption"]["events"]["repeats"], 1);
        assert_eq!(view["cost"]["usd"], 0.75);
    }
}

#[test]
fn bounded_parent_reads_qualify_copy_coverage() {
    let store = Store::new();
    let path = store.0.join("sessions").join(PARENT);
    let contents = fs::read_to_string(&path).unwrap();
    let padding = serde_json::json!({"type":"custom","id":"padding","parentId":"parent-request","data":"x".repeat(6*1024*1024)});
    fs::write(path, format!("{contents}{padding}\n")).unwrap();
    let bounded = store.usage("keys-fork", &["--event-keys"]);
    assert_eq!(
        bounded["consumption_event_keys"]["copy_coverage"],
        "read-window"
    );
    assert_eq!(bounded["consumption_event_keys"]["returned"], 3);
    assert!(bounded["consumption_event_keys"]["keys"][0]
        .get("copy_of")
        .is_none());
    assert_eq!(bounded["consumption"]["cost"]["usd"], 0.875);
    let full = store.usage("keys-fork", &["--event-keys", "--full"]);
    assert_eq!(full["consumption_event_keys"]["copy_coverage"], "session");
    assert_eq!(
        full["consumption_event_keys"]["keys"][0]["copy_of"]["session_id"],
        "keys-parent"
    );
}

#[test]
fn human_keys_and_help_report_limits_and_namespace_uncertainty() {
    let store = Store::new();
    let output = store.run("keys-fork", &["--event-keys=1"]);
    assert!(output.status.success());
    let human = String::from_utf8(output.stdout).unwrap();
    assert!(human.contains("1 returned of 3"));
    assert!(human.contains("row-cap 2"));
    assert!(human.contains("account namespace unestablished"));
    assert!(!human.contains(store.0.to_str().unwrap()));
    assert!(!human.contains(".jsonl"));
    let help = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .args(["usage", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--event-keys"));
    assert!(help.contains("8 MiB"));
    assert!(help.contains("Copy coverage and conflicts"));
}

#[test]
fn a_missing_or_corrupt_parent_qualifies_only_copy_evidence() {
    for corrupt in [false, true] {
        let store = Store::new();
        let parent = store.0.join("sessions").join(PARENT);
        if corrupt {
            fs::write(parent, "{broken\n").unwrap();
        } else {
            fs::remove_file(parent).unwrap();
        }
        for full in [false, true] {
            let options = if full {
                vec!["--event-keys", "--full"]
            } else {
                vec!["--event-keys"]
            };
            let view = store.usage("keys-fork", &options);
            assert_eq!(view["cost"]["usd"], 0.875);
            assert_eq!(view["consumption_event_keys"]["returned"], 3);
            assert_eq!(
                view["consumption_event_keys"]["copy_coverage"],
                if corrupt { "unreadable" } else { "unavailable" }
            );
            assert!(view["consumption_event_keys"]["keys"]
                .as_array()
                .unwrap()
                .iter()
                .all(|key| key.get("copy_of").is_none()));
            let keys = view["consumption_event_keys"].to_string();
            assert!(!keys.contains(store.0.to_str().unwrap()), "{keys}");
            assert!(!keys.contains("No such file"), "{keys}");
        }
    }
}

#[test]
fn one_bad_parent_record_keeps_healthy_copy_witnesses() {
    let store = Store::new();
    let path = store.0.join("sessions").join(PARENT);
    let lines = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            if line.contains("\"id\":\"shared-request\"") {
                "{broken".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>();
    fs::write(path, lines.join("\n") + "\n").unwrap();
    fs::write(
        store
            .0
            .join("sessions/2026-01-15T12-00-00-000Z_unrelated.jsonl"),
        "{broken\n",
    )
    .unwrap();
    for full in [false, true] {
        let options = if full {
            vec!["--event-keys", "--full"]
        } else {
            vec!["--event-keys"]
        };
        let view = store.usage("keys-fork", &options);
        let keys = &view["consumption_event_keys"];
        assert_eq!(keys["returned"], 3);
        assert_eq!(keys["copy_coverage"], "unreadable");
        assert!(keys["keys"][0].get("copy_of").is_none());
        assert_eq!(keys["keys"][1]["copy_of"]["entry_id"], "shared-compaction");
        assert_eq!(view["consumption"]["cost"]["usd"], 0.875);
    }
}

#[test]
fn contradictory_parent_observations_do_not_establish_a_copy() {
    let store = Store::new();
    store.edit(PARENT, |entry| {
        if entry["id"] == "shared-request" {
            entry["message"]["usage"]["input"] = serde_json::json!(11);
        }
    });
    let view = store.usage("keys-fork", &["--event-keys", "--full"]);
    let keys = &view["consumption_event_keys"];
    assert_eq!(keys["copy_conflicts"], 1);
    assert!(keys["keys"][0].get("copy_of").is_none());
    assert_eq!(keys["keys"][1]["copy_of"]["entry_id"], "shared-compaction");
    assert_eq!(view["cost"]["usd"], 0.875);
}

#[test]
fn private_or_unidentified_keys_are_counted_without_hiding_healthy_keys() {
    for unidentified in [false, true] {
        let store = Store::new();
        store.edit(PARENT, |entry| {
            if entry["id"] == "shared-request" {
                if unidentified {
                    entry.as_object_mut().unwrap().remove("id");
                    entry["message"]
                        .as_object_mut()
                        .unwrap()
                        .remove("responseId");
                } else {
                    entry["message"]["responseId"] =
                        serde_json::json!("/home/private-owner/recording.jsonl");
                }
                entry["message"]["errorMessage"] =
                    serde_json::json!("failed to open /home/private-owner/recording.jsonl");
            }
        });
        for full in [false, true] {
            let options = if full {
                vec!["--event-keys", "--full"]
            } else {
                vec!["--event-keys"]
            };
            let view = store.usage("keys-parent", &options);
            let keys = &view["consumption_event_keys"];
            assert_eq!(keys["observed"], 3);
            assert_eq!(keys["returned"], 2);
            assert_eq!(
                keys["omissions"][if unidentified {
                    "unidentified"
                } else {
                    "private_or_invalid_identity"
                }],
                1
            );
            assert!(!keys.to_string().contains("private-owner"));
            assert_eq!(view["consumption"]["cost"]["usd"], 0.75);
        }
    }
}

#[test]
fn provider_response_keys_do_not_assert_an_account_namespace() {
    let store = Store::new();
    store.edit(FORK, |entry| {
        if entry["type"] == "session" {
            entry.as_object_mut().unwrap().remove("parentSession");
        }
    });
    let view = store.usage("keys-fork", &["--event-keys"]);
    let keys = &view["consumption_event_keys"];
    assert!(keys.get("copy_coverage").is_none());
    assert!(keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .all(|key| key.get("copy_of").is_none()));
    assert_eq!(
        keys["keys"][0]["provider_account_namespace"],
        "unestablished"
    );
}

#[test]
fn unreadable_or_conflicting_fork_metadata_leaves_copy_equality_unestablished() {
    for parent in [
        serde_json::json!(null),
        serde_json::json!({"path":"/home/private-owner/recording.jsonl"}),
        serde_json::json!("conflicting-header"),
    ] {
        let store = Store::new();
        if parent == "conflicting-header" {
            let path = store.0.join("sessions").join(FORK);
            let contents = fs::read_to_string(&path).unwrap();
            let header = serde_json::json!({"type":"session","id":"keys-fork","parentSession":"other-parent"});
            fs::write(path, format!("{contents}{header}\n")).unwrap();
        } else {
            store.edit(FORK, |entry| {
                if entry["type"] == "session" {
                    entry["parentSession"] = parent.clone();
                }
            });
        }
        for full in [false, true] {
            let options = if full {
                vec!["--event-keys", "--full"]
            } else {
                vec!["--event-keys"]
            };
            let view = store.usage("keys-fork", &options);
            let keys = &view["consumption_event_keys"];
            assert_eq!(keys["copy_coverage"], "unestablished", "{keys}");
            assert_eq!(keys["returned"], 3);
            assert_eq!(view["cost"]["usd"], 0.875);
            assert!(!keys.to_string().contains("private-owner"));
            assert!(keys["keys"]
                .as_array()
                .unwrap()
                .iter()
                .all(|key| key.get("copy_of").is_none()));
        }
    }
}

#[test]
fn empty_response_ids_do_not_collapse_distinct_recording_entries() {
    for empty in ["responseId", "provider", "entry"] {
        let store = Store::new();
        store.edit(PARENT, |entry| {
            if entry["message"]["role"] == "assistant" {
                if empty == "entry" {
                    entry["id"] = serde_json::json!("");
                } else {
                    entry["message"][empty] = serde_json::json!("");
                    if empty == "provider" {
                        entry["message"]["responseId"] = serde_json::json!("response-repeated");
                    }
                }
            }
        });
        for full in [false, true] {
            let options = if full {
                vec!["--event-keys", "--full"]
            } else {
                vec!["--event-keys"]
            };
            let view = store.usage("keys-parent", &options);
            assert_eq!(view["consumption"]["events"]["counted"], 3);
            assert_eq!(view["consumption"]["cost"]["usd"], 0.75);
            assert_eq!(view["consumption_event_keys"]["returned"], 3);
            if empty != "entry" {
                assert!(view["consumption_event_keys"]["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|key| key["kind"] == "recording-entry"));
            } else {
                assert!(view["consumption_event_keys"]["keys"][0]
                    .get("recording_entry")
                    .is_none());
            }
        }
    }
}

#[test]
fn the_default_cap_and_cli_range_are_enforced() {
    let store = Store::new();
    let path = store.0.join("sessions").join(PARENT);
    let mut lines = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let template: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
    for number in 0..198 {
        let mut entry = template.clone();
        entry["id"] = serde_json::json!(format!("extra-{number}"));
        entry["parentId"] = serde_json::json!(if number == 0 {
            "parent-request".to_owned()
        } else {
            format!("extra-{}", number - 1)
        });
        entry["message"]["responseId"] = serde_json::json!(format!("response-extra-{number}"));
        lines.push(entry.to_string());
    }
    fs::write(path, lines.join("\n") + "\n").unwrap();
    let default = store.usage("keys-parent", &["--event-keys"]);
    assert_eq!(default["consumption_event_keys"]["observed"], 201);
    assert_eq!(default["consumption_event_keys"]["returned"], 200);
    assert_eq!(default["consumption_event_keys"]["omissions"]["row_cap"], 1);
    let maximum = store.usage("keys-parent", &["--event-keys=10000"]);
    assert_eq!(maximum["consumption_event_keys"]["returned"], 201);
    for option in ["--event-keys=0", "--event-keys=10001"] {
        let output = store.run("keys-parent", &[option]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("between 1 and 10000"));
    }
}

#[test]
fn serialized_byte_and_oversized_key_omissions_are_visible() {
    for oversized in [false, true] {
        let store = Store::new();
        let megabyte = 1024 * 1024;
        store.edit(PARENT, |entry| {
            if entry["type"] != "session" {
                let length = if oversized {
                    9 * megabyte
                } else {
                    3 * megabyte
                };
                if entry["message"]["role"] == "assistant" {
                    entry["message"]["responseId"] = serde_json::json!(
                        entry["id"].as_str().unwrap().to_owned() + &"a".repeat(length)
                    );
                } else if !oversized {
                    entry["id"] = serde_json::json!("c".repeat(length));
                }
            }
        });
        let view = store.usage("keys-parent", &["--event-keys=10000", "--full"]);
        let keys = &view["consumption_event_keys"];
        assert_eq!(keys["observed"], 3);
        assert_eq!(keys["returned"], if oversized { 1 } else { 2 });
        assert_eq!(
            keys["omissions"][if oversized {
                "oversized_key"
            } else {
                "byte_budget"
            }],
            if oversized { 2 } else { 1 }
        );
        let bytes: usize = keys["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| serde_json::to_vec(key).unwrap().len())
            .sum();
        assert!(bytes <= 8 * megabyte);
        assert_eq!(view["consumption"]["cost"]["usd"], 0.75);
    }
}

#[test]
fn a_changed_key_source_does_not_discard_a_held_measurement() {
    use agent_tapes_core::backend::{pi::PiBackend, Backend};
    use agent_tapes_core::usage::ConsumptionKeyOptions;
    let store = Store::new();
    let backend = PiBackend::new(store.0.join("sessions"));
    let session = backend.locate("keys-parent").unwrap().unwrap();
    let transcript = backend.transcript(&session, usize::MAX).unwrap();
    fs::remove_file(store.0.join("sessions").join(PARENT)).unwrap();
    let keys = backend.consumption_event_keys(
        &session,
        transcript.read.as_ref(),
        ConsumptionKeyOptions { limit: 200 },
    );
    assert!(
        keys.is_ok(),
        "optional keys must not discard the held session measurement"
    );
    let keys = serde_json::to_value(keys.unwrap()).unwrap();
    assert_eq!(keys["unavailable"], "source-unreadable-or-changed");
    assert_eq!(keys["observed"], 3);
    assert_eq!(session.cost.unwrap().usd, 0.75);
    assert!(!keys.to_string().contains(store.0.to_str().unwrap()));
}

#[test]
fn full_key_replays_keep_the_pinned_prefix_and_qualify_edits() {
    use agent_tapes_core::backend::{pi::PiBackend, Backend};
    use agent_tapes_core::usage::ConsumptionKeyOptions;
    for append in [false, true] {
        let store = Store::new();
        let backend = PiBackend::new(store.0.join("sessions"));
        let session = backend.locate("keys-parent").unwrap().unwrap();
        let read = backend
            .stream_transcript(&session, None, &mut |_| Ok(()))
            .unwrap();
        let whole = backend.stream_session(&session, &read).unwrap();
        let evidence = read.read_evidence(Some("pi".to_owned()));
        if append {
            use std::io::Write;
            let path = store.0.join("sessions").join(PARENT);
            let contents = fs::read_to_string(&path).unwrap();
            let mut extra: Value = serde_json::from_str(contents.lines().last().unwrap()).unwrap();
            extra["id"] = serde_json::json!("later-request");
            extra["message"]["responseId"] = serde_json::json!("response-later");
            writeln!(
                fs::OpenOptions::new().append(true).open(path).unwrap(),
                "{extra}"
            )
            .unwrap();
        } else {
            let path = store.0.join("sessions").join(PARENT);
            let contents = fs::read_to_string(&path)
                .unwrap()
                .replace("\"input\":10", "\"input\":11");
            fs::write(path, contents).unwrap();
        }
        let keys = backend
            .consumption_event_keys(
                &whole,
                Some(&evidence),
                ConsumptionKeyOptions { limit: 200 },
            )
            .unwrap();
        let value = serde_json::to_value(keys).unwrap();
        assert_eq!(value["observed"], 3);
        if append {
            assert_eq!(value["returned"], 3);
            assert!(value.get("unavailable").is_none());
        } else {
            assert_eq!(value["returned"], 0);
            assert_eq!(value["unavailable"], "source-unreadable-or-changed");
            assert!(value.get("coverage").is_none());
            assert_eq!(value["omissions"]["unavailable_source"], 3);
        }
        assert_eq!(whole.cost.unwrap().usd, 0.75);
    }
}

#[test]
fn copy_annotations_share_the_serialized_byte_budget() {
    let store = Store::new();
    let huge_id = "c".repeat(5 * 1024 * 1024);
    for file in [PARENT, FORK] {
        store.edit(file, |entry| {
            if entry["id"] == "shared-compaction" {
                entry["id"] = serde_json::json!(huge_id);
            }
            if entry["parentId"] == "shared-compaction" {
                entry["parentId"] = serde_json::json!(huge_id);
            }
        });
    }
    let view = store.usage("keys-fork", &["--event-keys=10000", "--full"]);
    let keys = &view["consumption_event_keys"];
    assert_eq!(keys["observed"], 3);
    assert_eq!(keys["returned"], 2);
    assert_eq!(keys["omissions"]["oversized_key"], 1);
    assert_eq!(view["consumption"]["cost"]["usd"], 0.875);
}

#[test]
fn event_keys_refuse_selections_supplied_inputs_and_other_harnesses() {
    let store = Store::new();
    for arguments in [
        vec!["usage", "--global", "--event-keys"],
        vec!["usage", "keys-parent", "--event-keys", "--series"],
        vec![
            "usage",
            "keys-parent",
            "--event-keys",
            "--read-bytes",
            "4MiB",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    let input = store.0.join("input.json");
    fs::write(
        &input,
        r#"{"messages":[{"role":"user","content":"A message."}]}"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tapes"))
        .args(["usage", "--event-keys", "--input"])
        .arg(input)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("installed recording"));
    use agent_tapes_core::backend::{codex::CodexBackend, Backend};
    use agent_tapes_core::usage::ConsumptionKeyOptions;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tapes-core/tests/fixtures/codex");
    let backend = CodexBackend::new(fixtures);
    let session = backend
        .locate("00000000-0000-0000-0000-000000000001")
        .unwrap()
        .unwrap();
    let error = backend
        .consumption_event_keys(&session, None, ConsumptionKeyOptions { limit: 200 })
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("do not support consumption event keys"));
}

/// A consumer's union over the JSON of two selections: each distinct
/// coordinate once, summed from the per-event measurements, and withheld
/// unless every key of both is present.
fn json_union(selections: [&Value; 2]) -> Option<(f64, u64)> {
    let mut events = std::collections::BTreeMap::new();
    for series in selections {
        let series = &series["consumption_event_keys"];
        if series["observed"] != series["returned"] {
            return None;
        }
        for key in series["keys"].as_array().unwrap() {
            let coordinate = key.get("copy_of").unwrap_or(&key["recording_entry"]);
            let measurement = &key["measurement"];
            events.insert(
                (
                    coordinate["session_id"].as_str().unwrap().to_owned(),
                    coordinate["entry_id"].as_str().unwrap().to_owned(),
                ),
                (
                    measurement["cost"]["usd"].as_f64()?,
                    measurement["disjoint"]["total"].as_u64()?,
                ),
            );
        }
    }
    Some(
        events
            .values()
            .fold((0.0, 0), |(cost, tokens), (usd, total)| {
                (cost + usd, tokens + total)
            }),
    )
}

#[test]
fn per_event_measurements_give_the_parent_fork_union_and_an_omission_withholds_it() {
    let store = Store::new();
    for full in [false, true] {
        let mut options = vec!["--event-keys"];
        options.extend(full.then_some("--full"));
        let parent = store.usage("keys-parent", &options);
        let fork = store.usage("keys-fork", &options);
        assert_eq!(json_union([&parent, &fork]), Some((1.25, 110)));
        for view in [&parent, &fork] {
            for key in view["consumption_event_keys"]["keys"].as_array().unwrap() {
                let measurement = &key["measurement"];
                assert_eq!(measurement["cost"]["kind"], "harness-calculated-estimate");
                assert!(measurement.get("measurement_completeness").is_none());
            }
        }

        let mut capped = vec!["--event-keys=2"];
        capped.extend(full.then_some("--full"));
        let capped_parent = store.usage("keys-parent", &capped);
        assert_eq!(
            capped_parent["consumption_event_keys"]["omissions"]["row_cap"],
            1
        );
        assert_eq!(json_union([&capped_parent, &fork]), None);
        // The healthy selection keeps its own figure beside the withheld union.
        assert_eq!(fork["consumption"]["cost"]["usd"], 0.875);
    }
}
