//! Integration coverage for the half of the tool that can destroy data.
//!
//! Everything here drives the real operations against a throwaway `CODEX_HOME` /
//! `CODEX_VAULT_HOME`, because the invariants that matter — "the bytes come back exactly", "no
//! appended turn is ever lost", "a failed run leaves nothing behind" — only exist end to end.

use codex_vault::commands::{analyze_command, compact_safe_command, doctor_command, BatchOptions};
use codex_vault::error::VaultError;
use codex_vault::manifest::{load_manifest, CodexVersionSource, Status};
use codex_vault::ops::{
    archive_impl, compact_safe_impl, compact_safe_impl_with, doctor_one, prune_one, restore_impl,
    CompactOptions, DoctorDepth, RestoreTarget,
};
use codex_vault::parallel::ProgressMode;
use codex_vault::paths::{ensure_vault_paths, manifest_path, VaultKey};
use codex_vault::rollout::DEFAULT_SCAN_WINDOW;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

// ---------------------------------------------------------------------------- harness
/// Batch options for tests: fixed worker count, never any progress output.
fn quiet_batch(jobs: usize) -> BatchOptions {
    BatchOptions {
        jobs,
        progress: ProgressMode::Never,
    }
}

/// `CODEX_HOME` is process-global, so tests that install one run one at a time.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct Sandbox {
    dir: TempDir,
    _guard: MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new() -> Self {
        let guard = env_lock();
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("codex/sessions")).unwrap();
        fs::create_dir_all(dir.path().join("vault")).unwrap();
        std::env::set_var("CODEX_HOME", dir.path().join("codex"));
        std::env::set_var("CODEX_VAULT_HOME", dir.path().join("vault"));
        Sandbox { dir, _guard: guard }
    }

    fn sessions(&self) -> PathBuf {
        self.dir.path().join("codex/sessions")
    }

    fn vault(&self) -> PathBuf {
        self.dir.path().join("vault")
    }

    /// Write a rollout whose suffix is a provable bounded reconstruction.
    fn compactable_session(&self, name: &str, id: &str, cwd: &str) -> PathBuf {
        let mut lines = vec![
            json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[]}}),
            json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t0"}}),
            json!({"type":"turn_context","payload":{"turn_id":"t0","model":"gpt"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"old"}}),
            json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t0"}}),
            json!({"type":"compacted",
                   "payload":{"replacement_history":[{"role":"user"}],"window_number":3}}),
        ];
        lines.extend(completed_turn("t1"));
        let path = self.sessions().join(name);
        write_jsonl(&path, &lines);
        path
    }

    fn backups(&self) -> Vec<String> {
        let dir = self.vault().join("backups");
        let mut out: Vec<String> = fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }
}

fn completed_turn(turn: &str) -> Vec<Value> {
    vec![
        json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":turn}}),
        json!({"type":"turn_context","payload":{"turn_id":turn,"model":"gpt"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
        json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":turn}}),
    ]
}

fn write_jsonl(path: &Path, lines: &[Value]) {
    let mut body = String::new();
    for l in lines {
        body.push_str(&serde_json::to_string(l).unwrap());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

fn append_jsonl(path: &Path, lines: &[Value]) {
    let mut body = fs::read_to_string(path).unwrap();
    for l in lines {
        body.push_str(&serde_json::to_string(l).unwrap());
        body.push('\n');
    }
    fs::write(path, body).unwrap();
}

/// Every scratch file the vault could have left, anywhere it could have left one.
fn leftover_temp_files(sandbox: &Sandbox) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.filter_map(Result::ok) {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("tmp") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(sandbox.dir.path(), &mut out);
    out
}

// ---------------------------------------------------------------- round-trip invariants

#[test]
fn compact_then_restore_reproduces_the_original_byte_for_byte() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-a.jsonl", "sess-a", "C:/work/a");
    let before = fs::read(&session).unwrap();

    let result = compact_safe_impl(&session).unwrap();
    assert_eq!(result.status, "ok", "{:?}", result.reason);
    assert!(
        fs::read(&session).unwrap().len() < before.len(),
        "compaction should have removed bytes"
    );

    // An undetected Codex version is reported but does not by itself make the session suspect;
    // otherwise every machine without `codex` on PATH would sit at permanent "warning".
    let check = doctor_one(&session, DoctorDepth::Deep).unwrap();
    assert_eq!(check.status, "ok", "{:?}", check.notes);
    assert!(check.session_ok && check.backup_ok && check.manifest_ok);
    assert!(check.unreferenced_backups.is_empty() && check.stale_temp_files.is_empty());

    let restored = restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_eq!(restored.status, "ok", "{:?}", restored.reason);
    assert_eq!(
        fs::read(&session).unwrap(),
        before,
        "restore must reproduce the original bytes exactly"
    );
}

#[test]
fn a_failed_compaction_leaves_no_scratch_files_and_no_change() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-b.jsonl", "sess-b", "C:/work/b");
    let before = fs::read(&session).unwrap();

    // Make writing the journal impossible: a directory sits where the manifest file must go.
    // This is the real failure that used to strand a compacted scratch file next to the
    // transcript and a `prepared` manifest temp in the vault.
    let vault = ensure_vault_paths().unwrap();
    fs::create_dir_all(manifest_path(&vault, &VaultKey::for_rollout(&session))).unwrap();

    let err = compact_safe_impl(&session).unwrap_err();
    assert!(
        matches!(
            err,
            VaultError::Io { .. } | VaultError::ManifestInvalid { .. }
        ),
        "unexpected error: {err}"
    );
    assert_eq!(
        fs::read(&session).unwrap(),
        before,
        "a failed compaction must not touch the transcript"
    );
    assert!(
        leftover_temp_files(&sb).is_empty(),
        "scratch files survived a failed run: {:?}",
        leftover_temp_files(&sb)
    );
}

// ------------------------------------------------------------ nothing appended is ever lost

#[test]
fn archive_only_fallback_records_its_snapshot_and_restore_does_not_rewind() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-c.jsonl", "sess-c", "C:/work/c");
    compact_safe_impl(&session).unwrap();

    // Codex keeps working and writes a record this build does not understand, which makes the
    // next compaction fall back to archive-only.
    append_jsonl(
        &session,
        &[
            json!({"type":"event_msg","payload":{"type":"turn_started","turn_id":"t2"}}),
            json!({"type":"future_semantic_record","payload":{"x":1}}),
            json!({"type":"event_msg","payload":{"type":"turn_complete","turn_id":"t2"}}),
        ],
    );
    let grown = fs::read(&session).unwrap();

    let fallback = compact_safe_impl(&session).unwrap();
    assert_eq!(fallback.status, "archived_only");
    assert_eq!(
        fs::read(&session).unwrap(),
        grown,
        "the fallback must not modify the transcript"
    );

    // The freshly captured state must be recorded, not orphaned.
    let vault = ensure_vault_paths().unwrap();
    let manifest = load_manifest(&manifest_path(&vault, &VaultKey::for_rollout(&session)))
        .unwrap()
        .unwrap();
    let snapshot = fallback.backup.clone().unwrap();
    assert!(
        manifest.anchors().iter().any(|a| a.backup_path == snapshot),
        "the fallback snapshot is not reachable from the journal"
    );
    assert_eq!(
        manifest.restore.source_size as usize,
        grown.len(),
        "restore should target the newest captured state"
    );

    // A default restore must therefore be a no-op rather than a rewind.
    restore_impl(&session, RestoreTarget::Latest).unwrap();
    assert_eq!(fs::read(&session).unwrap(), grown);

    let check = doctor_one(&session, DoctorDepth::Deep).unwrap();
    assert!(
        check.unreferenced_backups.is_empty(),
        "orphaned backups: {:?}",
        check.unreferenced_backups
    );
}

#[test]
fn restore_captures_the_current_state_before_replacing_it() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-d.jsonl", "sess-d", "C:/work/d");
    compact_safe_impl(&session).unwrap();
    append_jsonl(&session, &completed_turn("t9"));
    let grown = fs::read(&session).unwrap();

    // Deliberately rewind to the pre-compaction original: the appended turn is not in it.
    let result = restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_eq!(result.status, "ok");
    assert_ne!(fs::read(&session).unwrap(), grown);
    assert!(
        result.reason.iter().any(|r| r.contains("captured")),
        "restore should say where the replaced state went: {:?}",
        result.reason
    );

    // ...and that state must still be reachable, so nothing was actually lost.
    let vault = ensure_vault_paths().unwrap();
    let manifest = load_manifest(&manifest_path(&vault, &VaultKey::for_rollout(&session)))
        .unwrap()
        .unwrap();
    let captured = manifest
        .anchors()
        .into_iter()
        .find(|a| a.source_size as usize == grown.len())
        .expect("the pre-restore capture is not among the recorded anchors");
    restore_impl(&session, RestoreTarget::Backup(captured.backup_path)).unwrap();
    assert_eq!(
        fs::read(&session).unwrap(),
        grown,
        "the state discarded by restore must be recoverable"
    );
}

// ------------------------------------------------------------------ crash and tamper recovery

#[test]
fn an_interrupted_compaction_is_recoverable_from_the_prepared_journal() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-e.jsonl", "sess-e", "C:/work/e");
    let before = fs::read(&session).unwrap();
    compact_safe_impl(&session).unwrap();

    // Rewind the journal to the state a crash between `prepared` and the commit would leave.
    let vault = ensure_vault_paths().unwrap();
    let mpath = manifest_path(&vault, &VaultKey::for_rollout(&session));
    let mut raw: Value = serde_json::from_str(&fs::read_to_string(&mpath).unwrap()).unwrap();
    raw["status"] = json!("prepared");
    raw.as_object_mut().unwrap().remove("committed_at");
    fs::write(&mpath, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

    let check = doctor_one(&session, DoctorDepth::Deep).unwrap();
    assert_eq!(check.status, "warning");
    assert!(
        check.notes.iter().any(|n| n.contains("prepared")),
        "doctor should flag an interrupted compaction: {:?}",
        check.notes
    );
    assert_eq!(
        load_manifest(&mpath).unwrap().unwrap().status,
        Status::Prepared
    );

    restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_eq!(fs::read(&session).unwrap(), before);
}

#[test]
fn a_tampered_backup_is_refused_rather_than_restored() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-f.jsonl", "sess-f", "C:/work/f");
    compact_safe_impl(&session).unwrap();
    let compacted = fs::read(&session).unwrap();

    let backup = sb.vault().join("backups/rollout-f.original.jsonl.zst");
    let mut bytes = fs::read(&backup).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&backup, bytes).unwrap();

    let result = restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_eq!(result.status, "failed");
    assert!(result.reason.iter().any(|r| r.contains("hash mismatch")));
    assert_eq!(
        fs::read(&session).unwrap(),
        compacted,
        "a refused restore must leave the transcript alone"
    );
    assert!(leftover_temp_files(&sb).is_empty());

    let check = doctor_one(&session, DoctorDepth::Deep).unwrap();
    assert!(!check.backup_ok, "doctor should notice the tampered backup");
}

// ------------------------------------------------------------------------ journal invariants

#[test]
fn a_manifest_missing_a_required_field_is_refused_not_ignored() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-g.jsonl", "sess-g", "C:/work/g");
    compact_safe_impl(&session).unwrap();

    let vault = ensure_vault_paths().unwrap();
    let mpath = manifest_path(&vault, &VaultKey::for_rollout(&session));
    let mut raw: Value = serde_json::from_str(&fs::read_to_string(&mpath).unwrap()).unwrap();
    // Drop the hash `restore` verifies. Under the old stringly-typed journal this silently
    // disabled the check; now it must be an error.
    raw["restore"]
        .as_object_mut()
        .unwrap()
        .remove("source_sha256");
    fs::write(&mpath, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

    let err = load_manifest(&mpath).unwrap_err();
    assert!(matches!(err, VaultError::ManifestInvalid { .. }), "{err}");
    let err = restore_impl(&session, RestoreTarget::Latest).unwrap_err();
    assert!(matches!(err, VaultError::ManifestInvalid { .. }), "{err}");
}

#[test]
fn a_legacy_v1_manifest_is_upgraded_rather_than_discarded() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-h.jsonl", "sess-h", "C:/work/h");
    let before = fs::read(&session).unwrap();
    archive_impl(&session, false).unwrap();

    let vault = ensure_vault_paths().unwrap();
    let mpath = manifest_path(&vault, &VaultKey::for_rollout(&session));
    let current = load_manifest(&mpath).unwrap().unwrap();

    // Rewrite it in the original flat v1 shape.
    let legacy = json!({
        "manifest_version": 1,
        "created_at": current.created_at,
        "session_id": "sess-h",
        "session_path": current.session_path,
        "mode": "archive",
        "status": "ok",
        "schema_adapter": "codex-rollout-envelope-v0.1",
        "codex_version": Value::Null,
        "original_size": current.original.source_size,
        "original_sha256": current.original.source_sha256,
        "original_backup_path": current.original.backup_path,
        "original_backup_sha256": current.original.backup_sha256,
        "result_size": current.result_size,
        "result_sha256": current.result_sha256,
        "backup_path": current.original.backup_path,
        "backup_sha256": current.original.backup_sha256,
    });
    fs::write(&mpath, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

    let upgraded = load_manifest(&mpath).unwrap().unwrap();
    assert_eq!(upgraded.manifest_version, 2);
    assert_eq!(
        upgraded.original.source_sha256,
        current.original.source_sha256
    );
    assert!(!upgraded.codex_version_detected);

    // And the upgraded journal still drives a real restore.
    restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_eq!(fs::read(&session).unwrap(), before);
}

#[test]
fn a_manifest_from_a_newer_build_is_refused() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-i.jsonl", "sess-i", "C:/work/i");
    archive_impl(&session, false).unwrap();

    let vault = ensure_vault_paths().unwrap();
    let mpath = manifest_path(&vault, &VaultKey::for_rollout(&session));
    let mut raw: Value = serde_json::from_str(&fs::read_to_string(&mpath).unwrap()).unwrap();
    raw["manifest_version"] = json!(99);
    fs::write(&mpath, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

    let err = load_manifest(&mpath).unwrap_err();
    assert!(matches!(err, VaultError::ManifestInvalid { .. }), "{err}");
}

// ----------------------------------------------------------------------------- housekeeping

#[test]
fn archive_force_snapshots_stay_reachable() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-j.jsonl", "sess-j", "C:/work/j");
    archive_impl(&session, false).unwrap();
    append_jsonl(&session, &completed_turn("t5"));
    let grown = fs::read(&session).unwrap();

    let forced = archive_impl(&session, true).unwrap();
    assert_eq!(forced.status, "snapshot_created");
    assert_eq!(sb.backups().len(), 2);

    let check = doctor_one(&session, DoctorDepth::Deep).unwrap();
    assert!(
        check.unreferenced_backups.is_empty(),
        "a --force snapshot must be recorded: {:?}",
        check.unreferenced_backups
    );

    // Rewind, then come back to the snapshot.
    restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_ne!(fs::read(&session).unwrap(), grown);
    restore_impl(&session, RestoreTarget::Backup(forced.backup.unwrap())).unwrap();
    assert_eq!(fs::read(&session).unwrap(), grown);
}

#[test]
fn prune_is_a_dry_run_until_asked_to_apply() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-k.jsonl", "sess-k", "C:/work/k");
    compact_safe_impl(&session).unwrap();

    let debris = sb.sessions().join("rollout-k.jsonl.compact.1.tmp");
    fs::write(&debris, b"x").unwrap();

    let dry = prune_one(&session, false, false).unwrap();
    assert_eq!(dry["stale_temp_files"], json!(1));
    assert_eq!(dry["removed"].as_array().unwrap().len(), 0);
    assert!(debris.exists(), "a dry run must not delete anything");

    let applied = prune_one(&session, false, true).unwrap();
    assert_eq!(applied["removed"].as_array().unwrap().len(), 1);
    assert!(!debris.exists());
}

#[test]
fn prune_refuses_to_judge_backups_without_a_readable_journal() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-l.jsonl", "sess-l", "C:/work/l");
    archive_impl(&session, false).unwrap();

    let vault = ensure_vault_paths().unwrap();
    fs::write(
        manifest_path(&vault, &VaultKey::for_rollout(&session)),
        "{ not json",
    )
    .unwrap();

    let result = prune_one(&session, true, true).unwrap();
    assert_eq!(result["unreferenced_backups"], json!(0));
    assert!(result["note"].as_str().unwrap().contains("refusing"));
    assert!(
        sb.vault()
            .join("backups/rollout-l.original.jsonl.zst")
            .exists(),
        "prune must never delete a backup it could not prove unreferenced"
    );
}

// ------------------------------------------------------------------------------- provenance

#[test]
fn the_pinned_codex_version_comes_from_the_transcript_not_the_installed_cli() {
    let sb = Sandbox::new();
    let path = sb.sessions().join("rollout-m.jsonl");
    let mut lines = vec![json!({"type":"session_meta","payload":{
        "id":"sess-m",
        "cwd":"C:/work/m",
        "cli_version":"0.150.0-alpha.12.2",
        "originator":"Codex Desktop",
        "source":"vscode",
        "history_mode":"paginated",
        "context_window":{"window_id":"win-7"}
    }})];
    lines.push(json!({"type":"compacted",
                      "payload":{"replacement_history":[{"r":1}],"window_number":3}}));
    lines.extend(completed_turn("t1"));
    write_jsonl(&path, &lines);

    // Even with an explicit environment override present, the transcript's own record wins:
    // it names the build that actually wrote this file.
    std::env::set_var("CODEX_VAULT_CODEX_VERSION", "9.9.9-from-env");
    archive_impl(&path, false).unwrap();
    std::env::remove_var("CODEX_VAULT_CODEX_VERSION");

    let vault = ensure_vault_paths().unwrap();
    let m = load_manifest(&manifest_path(&vault, &VaultKey::for_rollout(&path)))
        .unwrap()
        .unwrap();
    assert_eq!(m.codex_version.as_deref(), Some("0.150.0-alpha.12.2"));
    assert_eq!(m.codex_version_source, CodexVersionSource::SessionMeta);
    assert!(m.codex_version_detected);
    assert_eq!(m.originator.as_deref(), Some("Codex Desktop"));
    assert_eq!(m.client_source.as_deref(), Some("vscode"));
    assert_eq!(m.history_mode.as_deref(), Some("paginated"));
    assert_eq!(m.context_window_id.as_deref(), Some("win-7"));
    assert!(
        m.notes.iter().all(|n| !n.contains("Codex version")),
        "a version read from the transcript needs no caveat: {:?}",
        m.notes
    );
}

#[test]
fn a_transcript_without_cli_version_falls_back_to_the_environment() {
    let sb = Sandbox::new();
    let path = sb.compactable_session("rollout-n.jsonl", "sess-n", "C:/work/n");

    std::env::set_var("CODEX_VAULT_CODEX_VERSION", "0.140.0-pinned");
    archive_impl(&path, false).unwrap();
    std::env::remove_var("CODEX_VAULT_CODEX_VERSION");

    let vault = ensure_vault_paths().unwrap();
    let m = load_manifest(&manifest_path(&vault, &VaultKey::for_rollout(&path)))
        .unwrap()
        .unwrap();
    assert_eq!(m.codex_version.as_deref(), Some("0.140.0-pinned"));
    assert_eq!(m.codex_version_source, CodexVersionSource::Environment);
    assert_eq!(m.originator, None);
}

// ------------------------------------------------------------------------------ doctor depth

#[test]
fn standard_doctor_still_catches_a_tampered_archive() {
    // The standard pass skips decompression because `create_verified_backup` already proved the
    // archive decodes to the recorded content. That reasoning only holds if a change to the
    // archive bytes is still detected — which is what this asserts.
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-o.jsonl", "sess-o", "C:/work/o");
    compact_safe_impl(&session).unwrap();

    assert!(
        doctor_one(&session, DoctorDepth::Standard)
            .unwrap()
            .backup_ok
    );

    let backup = sb.vault().join("backups/rollout-o.original.jsonl.zst");
    let mut bytes = fs::read(&backup).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&backup, bytes).unwrap();

    let standard = doctor_one(&session, DoctorDepth::Standard).unwrap();
    assert!(
        !standard.backup_ok,
        "standard doctor missed a tampered archive"
    );
    assert_eq!(standard.status, "warning");
    assert!(!standard.deep);

    let deep = doctor_one(&session, DoctorDepth::Deep).unwrap();
    assert!(!deep.backup_ok);
    assert!(deep.deep);
}

#[test]
fn standard_doctor_skips_reparsing_a_byte_identical_transcript() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-p.jsonl", "sess-p", "C:/work/p");
    compact_safe_impl(&session).unwrap();

    let standard = doctor_one(&session, DoctorDepth::Standard).unwrap();
    assert!(standard.session_ok);
    assert!(
        standard.notes.iter().any(|n| n.contains("inherited")),
        "expected the inherited-validity note: {:?}",
        standard.notes
    );

    // Once Codex appends, the transcript is no longer byte-identical, so it is parsed again.
    append_jsonl(&session, &completed_turn("t7"));
    let grown = doctor_one(&session, DoctorDepth::Standard).unwrap();
    assert!(grown.session_ok);
    assert!(
        grown.notes.iter().all(|n| !n.contains("inherited")),
        "a grown transcript must not inherit validity: {:?}",
        grown.notes
    );

    // And corruption in the appended region is still caught.
    let mut body = fs::read_to_string(&session).unwrap();
    body.push_str("{ this is not json\n");
    fs::write(&session, body).unwrap();
    let broken = doctor_one(&session, DoctorDepth::Standard).unwrap();
    assert!(!broken.session_ok, "corrupt appended JSONL went unnoticed");
}

// ------------------------------------------------------------------------------ batch running

/// A rollout whose head parses but whose body is not valid UTF-8, so discovery succeeds and the
/// per-session work fails. Before batches reported errors per session, this aborted the run.
fn unreadable_session(sb: &Sandbox, name: &str, id: &str, cwd: &str) -> PathBuf {
    let path = sb.sessions().join(name);
    let mut bytes =
        serde_json::to_vec(&json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}})).unwrap();
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"type\":\"event_msg\",\"payload\":{\"x\":\"");
    bytes.extend_from_slice(&[0xff, 0xfe]);
    bytes.extend_from_slice(b"\"}}\n");
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn a_batch_reports_a_broken_session_instead_of_aborting() {
    let sb = Sandbox::new();
    sb.compactable_session("rollout-q1.jsonl", "sess-q1", "C:/work/q");
    unreadable_session(&sb, "rollout-q2.jsonl", "sess-q2", "C:/work/q");
    sb.compactable_session("rollout-q3.jsonl", "sess-q3", "C:/work/q");

    let out = analyze_command(None, None, DEFAULT_SCAN_WINDOW, quiet_batch(4)).unwrap();
    let rows = out["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "every session must be accounted for");

    let errors: Vec<&Value> = rows.iter().filter(|r| r["status"] == "error").collect();
    assert_eq!(errors.len(), 1, "{rows:#?}");
    assert_eq!(errors[0]["session_id"], "sess-q2");
    assert_eq!(errors[0]["code"], "io_error");

    let analysed = rows.iter().filter(|r| r.get("analysis").is_some()).count();
    assert_eq!(analysed, 2, "the healthy sessions must still be analysed");
}

#[test]
fn parallel_and_serial_batches_produce_identical_output() {
    let sb = Sandbox::new();
    for i in 0..6 {
        sb.compactable_session(
            &format!("rollout-r{i}.jsonl"),
            &format!("sess-r{i}"),
            "C:/work/r",
        );
    }
    // Ordering must come from the session list, never from which worker finished first.
    let serial = doctor_command(None, None, false, quiet_batch(1)).unwrap();
    let parallel = doctor_command(None, None, false, quiet_batch(8)).unwrap();
    assert_eq!(serial, parallel);
    assert_eq!(serial.as_array().unwrap().len(), 6);

    let serial = analyze_command(None, None, DEFAULT_SCAN_WINDOW, quiet_batch(1)).unwrap();
    let parallel = analyze_command(None, None, DEFAULT_SCAN_WINDOW, quiet_batch(8)).unwrap();
    assert_eq!(serial, parallel);
}

#[test]
fn a_destructive_batch_never_reaches_a_parent_projects_sessions() {
    let sb = Sandbox::new();
    sb.compactable_session("rollout-s1.jsonl", "sess-here", "C:/work/repo/frontend");
    sb.compactable_session(
        "rollout-s2.jsonl",
        "sess-below",
        "C:/work/repo/frontend/src",
    );
    sb.compactable_session("rollout-s3.jsonl", "sess-parent", "C:/work");
    sb.compactable_session("rollout-s4.jsonl", "sess-sibling", "C:/work/repo/backend");

    let touched = compact_safe_command(
        None,
        Some("C:/work/repo/frontend".to_string()),
        CompactOptions::default(),
        quiet_batch(1),
    )
    .unwrap();
    let ids: Vec<String> = touched["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&"sess-here".to_string()));
    assert!(ids.contains(&"sess-below".to_string()));
    assert!(!ids.contains(&"sess-parent".to_string()), "{ids:?}");
    assert!(!ids.contains(&"sess-sibling".to_string()), "{ids:?}");
}

// ================================================================ one vault entry per rollout

/// Write a rollout that declares `id`, under an arbitrary file name.
///
/// A Codex thread spans several rollout files (`rollout-<ts>-<thread>_<fork>.jsonl`), so the same
/// `session_meta.id` legitimately appears in more than one file.
fn session_named(sb: &Sandbox, file: &str, id: &str, cwd: &str, marker: &str) -> PathBuf {
    let mut lines = vec![
        json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":format!("HISTORY-{marker}")}]}}),
        json!({"type":"compacted",
               "payload":{"replacement_history":[{"role":"user"}],"window_number":1}}),
    ];
    lines.extend(completed_turn(marker));
    let path = sb.sessions().join(file);
    write_jsonl(&path, &lines);
    path
}

#[test]
fn two_rollouts_of_one_thread_never_share_a_vault_entry() {
    // Regression: the vault was keyed on `session_meta.id`. Two rollout files of the same thread
    // then shared one manifest and one "immutable original", and `restore --original` on the
    // second wrote the first one's transcript into it — silently, with doctor reporting `ok`.
    let sb = Sandbox::new();
    let a = session_named(
        &sb,
        "rollout-a-thread.jsonl",
        "one-thread",
        "C:/work/dup",
        "A",
    );
    let b = session_named(
        &sb,
        "rollout-b-thread_fork.jsonl",
        "one-thread",
        "C:/work/dup",
        "B",
    );

    compact_safe_impl(&a).unwrap();
    compact_safe_impl(&b).unwrap();

    let vault = ensure_vault_paths().unwrap();
    let ka = VaultKey::for_rollout(&a);
    let kb = VaultKey::for_rollout(&b);
    assert_ne!(ka, kb, "the two files must not resolve to one key");
    assert!(manifest_path(&vault, &ka).exists());
    assert!(manifest_path(&vault, &kb).exists());

    let ma = load_manifest(&manifest_path(&vault, &ka)).unwrap().unwrap();
    let mb = load_manifest(&manifest_path(&vault, &kb)).unwrap().unwrap();
    assert_ne!(
        ma.original.backup_path, mb.original.backup_path,
        "each rollout needs its own immutable original"
    );
    assert_ne!(ma.original.source_sha256, mb.original.source_sha256);

    // The decisive assertion: restoring B must put *B* back, not A.
    restore_impl(&b, RestoreTarget::Original).unwrap();
    let restored = fs::read_to_string(&b).unwrap();
    assert!(
        restored.contains("HISTORY-B") && !restored.contains("HISTORY-A"),
        "restore wrote the other rollout's content: {}",
        &restored[..restored.len().min(200)]
    );

    for path in [&a, &b] {
        let check = doctor_one(path, DoctorDepth::Deep).unwrap();
        assert!(check.backup_ok && check.manifest_ok, "{:?}", check.notes);
        assert!(check.unreferenced_backups.is_empty());
    }
}

#[test]
fn a_manifest_stored_under_the_legacy_thread_key_is_adopted() {
    let sb = Sandbox::new();
    let session = sb.compactable_session("rollout-t.jsonl", "sess-t", "C:/work/t");
    let before = fs::read(&session).unwrap();
    archive_impl(&session, false).unwrap();

    let vault = ensure_vault_paths().unwrap();
    let key = VaultKey::for_rollout(&session);
    let legacy = VaultKey::legacy_thread_id("sess-t");
    assert_ne!(key, legacy);

    // Move the journal back to where the previous layout would have put it.
    fs::rename(manifest_path(&vault, &key), manifest_path(&vault, &legacy)).unwrap();

    // It still drives a real restore, because it names this very file.
    restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_eq!(fs::read(&session).unwrap(), before);
    let check = doctor_one(&session, DoctorDepth::Deep).unwrap();
    assert!(
        check.manifest_exists && check.backup_ok,
        "{:?}",
        check.notes
    );
}

#[test]
fn a_legacy_manifest_naming_another_rollout_is_not_adopted() {
    let sb = Sandbox::new();
    let a = session_named(&sb, "rollout-x-thread.jsonl", "shared-id", "C:/work/x", "A");
    let b = session_named(
        &sb,
        "rollout-y-thread_fork.jsonl",
        "shared-id",
        "C:/work/x",
        "B",
    );
    archive_impl(&a, false).unwrap();

    let vault = ensure_vault_paths().unwrap();
    let legacy = VaultKey::legacy_thread_id("shared-id");
    fs::rename(
        manifest_path(&vault, &VaultKey::for_rollout(&a)),
        manifest_path(&vault, &legacy),
    )
    .unwrap();

    // B shares the thread id, so the legacy key matches — but the manifest names A.
    archive_impl(&b, false).unwrap();
    let mb = load_manifest(&manifest_path(&vault, &VaultKey::for_rollout(&b)))
        .unwrap()
        .unwrap();
    assert!(
        paths_equal_str(&mb.session_path, &b),
        "B adopted a journal belonging to A: {}",
        mb.session_path
    );
    assert_eq!(
        mb.original.source_size as usize,
        fs::read(&b).unwrap().len()
    );
}

fn paths_equal_str(recorded: &str, actual: &Path) -> bool {
    let norm = |p: &Path| {
        p.canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase()
    };
    norm(Path::new(recorded)) == norm(actual)
}

// ============================================================== spawned threads are refused

fn spawned_session(sb: &Sandbox, file: &str, id: &str, cwd: &str, thread_source: &str) -> PathBuf {
    let mut lines = vec![json!({"type":"session_meta","payload":{
        "id": id,
        "session_id": "the-parent-thread",
        "cwd": cwd,
        "thread_source": thread_source,
        "parent_thread_id": "the-parent-thread",
        "multi_agent_version": "v2",
        "subagent_history_start_ordinal": 11
    }})];
    // Records ahead of the checkpoint, so a permitted compaction has something to remove.
    lines.extend(completed_turn("t0"));
    lines.push(json!({"type":"compacted",
                      "payload":{"replacement_history":[{"role":"user"}],"window_number":1}}));
    lines.extend(completed_turn("t1"));
    let path = sb.sessions().join(file);
    write_jsonl(&path, &lines);
    path
}

#[test]
fn compacting_a_spawned_thread_is_refused_by_default() {
    let sb = Sandbox::new();
    let path = spawned_session(&sb, "rollout-sub.jsonl", "sub-1", "C:/work/s", "subagent");
    let before = fs::read(&path).unwrap();

    let err = compact_safe_impl(&path).unwrap_err();
    match &err {
        VaultError::SpawnedThreadRefused { thread_source, .. } => {
            assert_eq!(thread_source.as_deref(), Some("subagent"))
        }
        other => panic!("expected a spawned-thread refusal, got {other}"),
    }
    assert_eq!(fs::read(&path).unwrap(), before);

    // Archiving is non-destructive, so it stays allowed.
    assert_eq!(archive_impl(&path, false).unwrap().status, "ok");
    assert_eq!(fs::read(&path).unwrap(), before);

    // And the refusal is an opt-out, not a wall.
    let allowed = compact_safe_impl_with(
        &path,
        CompactOptions {
            allow_spawned_threads: true,
            ..CompactOptions::default()
        },
    )
    .unwrap();
    assert_eq!(allowed.status, "ok");
    assert!(fs::read(&path).unwrap().len() < before.len());
}

#[test]
fn a_guardian_review_thread_is_refused_too() {
    let sb = Sandbox::new();
    let path = spawned_session(
        &sb,
        "rollout-gr.jsonl",
        "gr-1",
        "C:/work/s",
        "guardian_review",
    );
    assert!(matches!(
        compact_safe_impl(&path).unwrap_err(),
        VaultError::SpawnedThreadRefused { .. }
    ));
}

#[test]
fn a_batch_skips_spawned_threads_without_failing() {
    let sb = Sandbox::new();
    sb.compactable_session("rollout-u1.jsonl", "sess-u1", "C:/work/u");
    spawned_session(&sb, "rollout-u2.jsonl", "sub-u2", "C:/work/u", "subagent");

    let out = compact_safe_command(
        None,
        Some("C:/work/u".to_string()),
        CompactOptions::default(),
        quiet_batch(1),
    )
    .unwrap();
    let rows = out["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let statuses: Vec<&str> = rows
        .iter()
        .map(|r| {
            r.get("status")
                .or_else(|| r.pointer("/result/status"))
                .and_then(Value::as_str)
                .unwrap_or("?")
        })
        .collect();
    assert!(statuses.contains(&"ok"), "{rows:#?}");
    assert!(statuses.contains(&"skipped_spawned_thread"), "{rows:#?}");
}

// ========================================================= paginated thread lineages

/// Build one page of a paginated thread.
///
/// Codex splits a long thread across rollout files named `rollout-<ts>-<thread>_<page>.jsonl`.
/// Every page after the first records `history_base`, naming the page it continues from together
/// with a **byte offset** into it.
fn lineage_page(
    sb: &Sandbox,
    file: &str,
    thread_id: &str,
    continues_from: Option<(&str, u64)>,
    marker: &str,
) -> PathBuf {
    let mut meta = json!({
        "id": thread_id,
        "session_id": thread_id,
        "cwd": "C:/work/lineage",
        "history_mode": "paginated",
        "thread_source": "user",
    });
    if let Some((source_page, offset)) = continues_from {
        meta["history_base"] = json!({
            "thread_id": source_page,
            "end_ordinal_exclusive": 42,
            "end_byte_offset": offset,
        });
    }
    let mut lines = vec![
        json!({"type": "session_meta", "payload": meta}),
        json!({"type":"response_item","payload":{"type":"message","role":"user",
               "content":[{"type":"input_text","text":format!("PAGE-{marker}")}]}}),
    ];
    lines.extend(completed_turn("t0"));
    lines.push(json!({"type":"compacted",
                      "payload":{"replacement_history":[{"role":"user"}],"window_number":1}}));
    lines.extend(completed_turn("t1"));
    let path = sb.sessions().join(file);
    write_jsonl(&path, &lines);
    path
}

#[test]
fn a_page_another_one_continues_from_is_never_shortened() {
    // Proven against real transcripts: compacting a non-final page makes Codex refuse the whole
    // thread with "invalid paginated history lineage: cutoff byte offset is past the source
    // rollout". There is deliberately no override for this, unlike the spawned-thread refusal:
    // that one is merely unvalidated, this one is known to break the session.
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-1-thr1.jsonl", "thr1", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    let tail = lineage_page(
        &sb,
        "rollout-2-thr1_pg2.jsonl",
        "thr1",
        Some(("thr1", root_size)),
        "TAIL",
    );

    let before = fs::read(&root).unwrap();
    let err = compact_safe_impl(&root).unwrap_err();
    match &err {
        VaultError::LineageSourceRefused { successors, .. } => {
            assert_eq!(successors.len(), 1);
            assert!(successors[0].ends_with("rollout-2-thr1_pg2.jsonl"));
        }
        other => panic!("expected a lineage refusal, got {other}"),
    }
    assert_eq!(
        fs::read(&root).unwrap(),
        before,
        "the source page was modified"
    );

    // The newest page has no successor, so it is safe to compact.
    let tail_before = fs::metadata(&tail).unwrap().len();
    assert_eq!(compact_safe_impl(&tail).unwrap().status, "ok");
    assert!(fs::metadata(&tail).unwrap().len() < tail_before);
}

#[test]
fn a_batch_skips_a_lineage_source_rather_than_failing() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-3-thr2.jsonl", "thr2", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    lineage_page(
        &sb,
        "rollout-4-thr2_pg2.jsonl",
        "thr2",
        Some(("thr2", root_size)),
        "TAIL",
    );
    sb.compactable_session("rollout-5-plain.jsonl", "sess-plain", "C:/work/lineage");

    let out = compact_safe_command(
        None,
        Some("C:/work/lineage".to_string()),
        CompactOptions::default(),
        quiet_batch(1),
    )
    .unwrap();
    let rows = out["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let statuses: Vec<&str> = rows
        .iter()
        .map(|r| {
            r.get("status")
                .or_else(|| r.pointer("/result/status"))
                .and_then(Value::as_str)
                .unwrap_or("?")
        })
        .collect();
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == "skipped_lineage_source")
            .count(),
        1,
        "the source page should be reported, not abort the batch: {rows:#?}"
    );
    assert!(statuses.contains(&"ok"), "{rows:#?}");
    assert_eq!(fs::read(&root).unwrap().len() as u64, root_size);
}

#[test]
fn doctor_reports_a_lineage_already_broken() {
    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-6-thr3.jsonl", "thr3", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    lineage_page(
        &sb,
        "rollout-7-thr3_pg2.jsonl",
        "thr3",
        Some(("thr3", root_size)),
        "TAIL",
    );

    assert!(
        !doctor_one(&root, DoctorDepth::Standard)
            .unwrap()
            .lineage_broken
    );

    // Shorten the source page behind the vault's back, as an earlier build would have.
    let mut body = fs::read_to_string(&root).unwrap();
    body.truncate(body.len() / 2);
    fs::write(&root, body).unwrap();

    let check = doctor_one(&root, DoctorDepth::Standard).unwrap();
    assert!(check.lineage_broken, "{:?}", check.notes);
    assert_eq!(check.status, "warning");
    assert!(
        check.notes.iter().any(|n| n.contains("resume")),
        "the note should say the thread can no longer be resumed: {:?}",
        check.notes
    );
}

#[test]
fn prune_preserves_a_siblings_referenced_legacy_backup() {
    use codex_vault::manifest::write_manifest;
    use codex_vault::paths::backup_path;
    let sb = Sandbox::new();
    let a = session_named(&sb, "rollout-legacy-a.jsonl", "shared", "C:/work", "A");
    let archived = archive_impl(&a, false).unwrap();
    let vault = ensure_vault_paths().unwrap();
    let legacy = VaultKey::legacy_thread_id("shared");
    let legacy_backup = backup_path(&vault, &legacy);
    let mut m = load_manifest(archived.manifest.as_ref().unwrap())
        .unwrap()
        .unwrap();
    fs::rename(&m.original.backup_path, &legacy_backup).unwrap();
    m.original.backup_path = legacy_backup.clone();
    m.restore.backup_path = legacy_backup.clone();
    for h in &mut m.history {
        if let Some(a) = &mut h.anchor {
            a.backup_path = legacy_backup.clone();
        }
    }
    write_manifest(&legacy, &vault, &m).unwrap();
    fs::remove_file(archived.manifest.unwrap()).unwrap();
    let b = session_named(&sb, "rollout-legacy-b.jsonl", "shared", "C:/work", "B");
    archive_impl(&b, false).unwrap();
    let result = prune_one(&b, true, true).unwrap();
    assert_eq!(result["unreferenced_backups"], 0);
    assert!(legacy_backup.exists());
    assert_eq!(
        restore_impl(&a, RestoreTarget::Original).unwrap().status,
        "ok"
    );
}

#[test]
fn prune_preserves_backups_if_any_journal_is_unreadable() {
    let sb = Sandbox::new();
    let p = sb.compactable_session("rollout-prune-a.jsonl", "prune-a", "C:/work");
    archive_impl(&p, false).unwrap();
    let orphan = sb
        .vault()
        .join("backups/rollout-prune-a.snapshot-orphan.jsonl.zst");
    fs::write(&orphan, b"orphan").unwrap();
    fs::write(sb.vault().join("manifests/another.json"), b"broken journal").unwrap();
    let result = prune_one(&p, true, true).unwrap();
    assert!(orphan.exists());
    assert!(result["note"].as_str().unwrap().contains("refusing"));
}

#[test]
fn mutation_lock_blocks_other_operations_and_prune() {
    use codex_vault::fsatomic::MutationGuard;
    let sb = Sandbox::new();
    let p = sb.compactable_session("rollout-busy.jsonl", "busy", "C:/work");
    let vault = ensure_vault_paths().unwrap();
    let guard = MutationGuard::acquire(&vault.root, &p).unwrap();
    assert!(matches!(
        archive_impl(&p, false),
        Err(VaultError::SessionLocked { .. })
    ));
    assert!(matches!(
        prune_one(&p, true, true),
        Err(VaultError::SessionLocked { .. })
    ));
    let other_vault = sb.dir.path().join("other-vault");
    fs::create_dir(&other_vault).unwrap();
    assert!(MutationGuard::acquire(&other_vault, &p).is_err());
    drop(guard);
    assert!(archive_impl(&p, false).is_ok());
}

#[cfg(windows)]
#[test]
fn windows_replacement_denies_writers_until_verification_finishes() {
    use codex_vault::fsatomic::{lock_session, TempFile};
    let sb = Sandbox::new();
    let p = sb.compactable_session("rollout-lock.jsonl", "lock", "C:/work");
    let _source = lock_session(&p).unwrap();
    let temp = TempFile::beside(&p, "test");
    fs::write(temp.path(), fs::read(&p).unwrap()).unwrap();
    let replacement = temp.replace_locked(&p).unwrap();
    assert!(fs::OpenOptions::new().append(true).open(&p).is_err());
    assert!(fs::read(&p).is_ok());
    drop(replacement);
    assert!(fs::OpenOptions::new().append(true).open(&p).is_ok());
}

#[cfg(windows)]
#[test]
fn restore_refuses_before_replacement_if_undo_journal_cannot_be_written() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    let sb = Sandbox::new();
    let p = sb.compactable_session("rollout-restore-lock.jsonl", "restore-lock", "C:/work");
    let archived = archive_impl(&p, false).unwrap();
    append_jsonl(&p, &completed_turn("new-conversation"));
    let before = fs::read(&p).unwrap();
    let mf = archived.manifest.unwrap();
    let old_journal = fs::read(&mf).unwrap();
    let _block = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&mf)
        .unwrap();
    assert!(restore_impl(&p, RestoreTarget::Original).is_err());
    assert_eq!(fs::read(&p).unwrap(), before);
    assert_eq!(fs::read(&mf).unwrap(), old_journal);
}

#[test]
fn repeated_compaction_is_a_noop_and_keeps_the_restore_target() {
    let sb = Sandbox::new();
    let p = sb.compactable_session("rollout-repeat.jsonl", "repeat", "C:/work");
    let first = compact_safe_impl(&p).unwrap();
    let bytes = fs::read(&p).unwrap();
    let mf = first.manifest.unwrap();
    let journal = fs::read(&mf).unwrap();
    let backups = sb.backups();
    assert_eq!(compact_safe_impl(&p).unwrap().status, "already_compact");
    assert_eq!(fs::read(&p).unwrap(), bytes);
    assert_eq!(fs::read(&mf).unwrap(), journal);
    assert_eq!(sb.backups(), backups);
}

#[test]
fn codex_compressed_rollout_is_readable_but_cannot_be_rewritten() {
    use codex_vault::hashing::compress_file_with_input_sha;
    let sb = Sandbox::new();
    let plain = sb.compactable_session("rollout-managed.jsonl", "managed", "C:/work");
    let packed = sb.sessions().join("rollout-managed.jsonl.zst");
    compress_file_with_input_sha(&plain, &packed, 3).unwrap();
    let before = fs::read(&packed).unwrap();
    assert!(
        codex_vault::analysis::analyze_session(&packed)
            .unwrap()
            .can_compact
    );
    assert!(matches!(
        compact_safe_impl(&packed),
        Err(VaultError::CodexManagedZstd { .. })
    ));
    assert!(matches!(
        archive_impl(&packed, false),
        Err(VaultError::CodexManagedZstd { .. })
    ));
    assert!(matches!(
        restore_impl(&packed, RestoreTarget::Original),
        Err(VaultError::CodexManagedZstd { .. })
    ));
    assert_eq!(fs::read(&packed).unwrap(), before);
}

#[test]
fn compaction_retains_an_original_still_stored_under_the_legacy_key() {
    use codex_vault::manifest::write_manifest;
    use codex_vault::paths::backup_path;
    let sb = Sandbox::new();
    let p = sb.compactable_session("rollout-migrated.jsonl", "migrated", "C:/work");
    let original = fs::read(&p).unwrap();
    let archived = archive_impl(&p, false).unwrap();
    let vault = ensure_vault_paths().unwrap();
    let legacy = VaultKey::legacy_thread_id("migrated");
    let legacy_backup = backup_path(&vault, &legacy);
    let mut m = load_manifest(archived.manifest.as_ref().unwrap())
        .unwrap()
        .unwrap();
    fs::rename(&m.original.backup_path, &legacy_backup).unwrap();
    m.original.backup_path = legacy_backup.clone();
    m.restore.backup_path = legacy_backup.clone();
    for h in &mut m.history {
        if let Some(a) = &mut h.anchor {
            a.backup_path = legacy_backup.clone();
        }
    }
    write_manifest(&legacy, &vault, &m).unwrap();
    fs::remove_file(archived.manifest.unwrap()).unwrap();
    append_jsonl(&p, &completed_turn("later"));
    assert_eq!(compact_safe_impl(&p).unwrap().status, "ok");
    assert_eq!(
        restore_impl(&p, RestoreTarget::Original).unwrap().status,
        "ok"
    );
    assert_eq!(fs::read(&p).unwrap(), original);
}
