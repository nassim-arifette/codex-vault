//! Shared, synthetic lifecycle exercised by both the CLI suite and the Codex oracle.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn seed(path: &Path, id: &str, cwd: &Path) {
    fs::write(
        path,
        format!(
            "{}\n",
            json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{
                "id":id,"timestamp":"2026-01-01T00:00:00Z","cwd":cwd,"originator":"codex_cli_rs",
                "cli_version":"0.152.1","source":"cli","model_provider":"mock",
                "base_instructions":{"text":"Synthetic lifecycle test. Answer briefly."}
            }})
        ),
    )
    .unwrap();
}

fn append(path: &Path, records: &[Value]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{}", json!({"timestamp":"2026-01-01T00:00:00Z","type":record["type"],"payload":record["payload"]})).unwrap();
    }
}

fn turn(path: &Path, cwd: &Path, id: &str) {
    append(
        path,
        &[
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":id,"model_context_window":200000}}),
            json!({"type":"turn_context","payload":{"turn_id":id,"cwd":cwd,"approval_policy":"never","sandbox_policy":{"type":"read-only"},"model":"gpt-5.4","effort":"medium","summary":"auto"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":format!("Lifecycle {id}: café 🦀 authentication decision."),"images":[],"local_images":[],"text_elements":[]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("Lifecycle {id}: café 🦀 authentication decision.")}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":format!("Lifecycle {id}: keep rotating refresh tokens.")}]}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":id,"last_agent_message":"Keep rotating refresh tokens."}}),
        ],
    );
}

fn checkpoint(path: &Path, number: u64) {
    append(
        path,
        &[
            json!({"type":"compacted","payload":{"message":"Synthetic checkpoint","window_number":number,"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Lifecycle summary: keep rotating refresh tokens."}]}]}}),
        ],
    );
}

/// Returns the final pre-compaction state for an independent reconstruction comparison.
pub fn lifecycle(path: &Path, codex: &Path, vault: &Path, cwd: &Path) -> Vec<u8> {
    let run = |args: &[&str]| -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_codex-vault"))
            .args(["--json", "--no-progress"])
            .args(args)
            .env("CODEX_HOME", codex)
            .env("CODEX_VAULT_HOME", vault)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let target = path.to_str().unwrap();
    let mut original: Option<Value> = None;
    let mut history = Vec::<Value>::new();
    let mut backups = BTreeMap::<String, String>::new();
    let mut assert_journal = || {
        let states = run(&["restore", target, "--list"]);
        let manifest_path = walkdir::WalkDir::new(vault.join("manifests"))
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.path().extension().is_some_and(|s| s == "json"))
            .unwrap();
        let manifest: Value =
            serde_json::from_slice(&fs::read(manifest_path.path()).unwrap()).unwrap();
        assert_eq!(manifest["status"], "ok");
        if let Some(first) = &original {
            assert_eq!(first, &manifest["original"]);
        } else {
            original = Some(manifest["original"].clone());
        }
        let entries = manifest["history"].as_array().unwrap();
        assert!(entries.starts_with(&history), "journal must be append-only");
        history = entries.clone();
        let anchors = states["anchors"].as_array().unwrap();
        let reachable: BTreeSet<_> = anchors
            .iter()
            .map(|a| a["backup_path"].as_str().unwrap().to_owned())
            .collect();
        for entry in walkdir::WalkDir::new(vault.join("backups"))
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let name = entry.path().to_str().unwrap().to_owned();
            assert!(reachable.contains(&name), "backup is no longer reachable");
            let actual = hash(&fs::read(entry.path()).unwrap());
            if let Some(prior) = backups.insert(name, actual.clone()) {
                assert_eq!(prior, actual);
            }
        }
        let typed: codex_vault::manifest::Manifest = serde_json::from_value(manifest).unwrap();
        for anchor in typed.anchors() {
            assert_eq!(
                hash(&fs::read(&anchor.backup_path).unwrap()),
                anchor.backup_sha256
            );
        }
        run(&["doctor", target, "--deep"]);
    };
    turn(path, cwd, "initial");
    turn(path, cwd, "history");
    checkpoint(path, 3);
    turn(path, cwd, "checkpoint-tail");
    run(&["archive", target]);
    assert_journal();
    let archived = fs::read(path).unwrap();
    turn(path, cwd, "after-archive");
    let grown = fs::read(path).unwrap();
    assert!(grown.starts_with(&archived));
    assert_journal();
    assert_eq!(run(&["compact", target])["status"], "ok");
    assert_journal();
    let compacted = fs::read(path).unwrap();
    turn(path, cwd, "after-compact");
    assert!(fs::read(path).unwrap().starts_with(&compacted));
    assert_journal();
    run(&["index"]);
    let found = run(&["search", "initial authentication"]);
    let passage = found["matches"][0]["id"].as_str().unwrap();
    assert_eq!(
        run(&["read", passage])["verified_reference"]["kind"],
        "backup"
    );
    assert_journal();
    let newer = fs::read(path).unwrap();
    run(&["restore", target, "--original"]);
    assert_eq!(fs::read(path).unwrap(), archived);
    assert_journal();
    let listed = run(&["restore", target, "--list"]);
    let newer_hash = hash(&newer);
    let anchor = listed["anchors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["source_sha256"] == newer_hash)
        .unwrap();
    run(&[
        "restore",
        target,
        "--to",
        anchor["backup_path"].as_str().unwrap(),
    ]);
    assert_eq!(fs::read(path).unwrap(), newer);
    assert_journal();
    checkpoint(path, 4);
    turn(path, cwd, "final");
    let baseline = fs::read(path).unwrap();
    assert_journal();
    assert_eq!(run(&["compact", target])["status"], "ok");
    assert_journal();
    let final_bytes = fs::read(path).unwrap();
    assert!(final_bytes.len() < baseline.len());
    // Capture the final compacted state as well, then restore every distinct saved state.
    run(&["archive", target, "--force"]);
    assert_journal();
    let all = run(&["restore", target, "--list"]);
    let mut checked = BTreeSet::new();
    for anchor in all["anchors"].as_array().unwrap() {
        if !checked.insert(anchor["source_sha256"].as_str().unwrap().to_owned()) {
            continue;
        }
        run(&[
            "restore",
            target,
            "--to",
            anchor["backup_path"].as_str().unwrap(),
        ]);
        assert_eq!(hash(&fs::read(path).unwrap()), anchor["source_sha256"]);
        assert_journal();
    }
    assert!(checked.len() >= 5, "exercise several distinct generations");
    let final_hash = hash(&final_bytes);
    let anchor = all["anchors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["source_sha256"] == final_hash)
        .unwrap();
    run(&[
        "restore",
        target,
        "--to",
        anchor["backup_path"].as_str().unwrap(),
    ]);
    assert_eq!(fs::read(path).unwrap(), final_bytes);
    assert_journal();
    run(&["index", "--rebuild"]);
    assert_eq!(
        run(&["search", "initial authentication"])["matches"][0]["id"],
        passage
    );
    run(&["read", passage]);
    assert_journal();
    baseline
}
