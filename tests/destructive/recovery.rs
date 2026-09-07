use super::*;

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

#[test]
fn simulated_disk_full_and_permission_failures_are_stable_and_never_replace_the_rollout() {
    for (stage, error_kind) in [
        ("backup_write", "storage_full"),
        ("manifest_write", "storage_full"),
        ("compact_temp_write", "storage_full"),
        ("manifest_write", "permission_denied"),
    ] {
        let sb = Sandbox::new();
        let path = sb.compactable_session(
            &format!("rollout-io-{stage}-{error_kind}.jsonl"),
            &format!("io-{stage}-{error_kind}"),
            "C:/work",
        );
        let before = fs::read(&path).unwrap();
        let output = run_io_failure(&sb, &path, stage, error_kind);
        assert_eq!(output.status.code(), Some(6), "stage {stage}");
        let error = cli_error(&output);
        assert_eq!(error["code"], "io_error", "stage {stage}: {error}");
        assert_eq!(
            error["native_transcript_changed"], false,
            "stage {stage}: {error}"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "stage {stage}");
        assert!(
            leftover_temp_files(&sb).is_empty(),
            "normal I/O failure cleanup left scratch at {stage}: {:?}",
            leftover_temp_files(&sb)
        );
    }
}

#[test]
fn an_unusable_vault_root_is_an_io_error_before_native_replacement() {
    let sb = Sandbox::new();
    let path = sb.compactable_session("rollout-unusable-vault.jsonl", "unusable-vault", "C:/work");
    let before = fs::read(&path).unwrap();
    let unusable = sb.dir.path().join("vault-is-a-file");
    fs::write(&unusable, b"not a directory").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_codex-vault"))
        .args(["--json", "--no-progress", "compact", path.to_str().unwrap()])
        .env("CODEX_HOME", sb.dir.path().join("codex"))
        .env("CODEX_VAULT_HOME", &unusable)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(6));
    assert_eq!(cli_error(&output)["code"], "io_error");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn a_source_that_disappears_mid_operation_fails_without_committing_recovery_state() {
    let sb = Sandbox::new();
    let path = sb.compactable_session("rollout-disappears.jsonl", "disappears", "C:/work");
    let before = fs::read(&path).unwrap();
    let output = run_compact_paused_at(&sb, &path, "analysis", true, || {
        fs::remove_file(&path).unwrap();
    });
    assert!(!output.status.success());
    let text = combined_output(&output);
    assert!(
        text.contains("io_error") || text.contains("session_changed"),
        "{text}"
    );
    let manifest = manifest_path(
        &ensure_vault_paths().unwrap(),
        &VaultKey::for_rollout(&path),
    );
    assert!(!manifest.exists());
    // The deletion was performed by the adversarial actor, not by Vault; restore the fixture only
    // so the assertion can prove what bytes that actor removed.
    fs::write(&path, &before).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn missing_truncated_and_corrupt_backups_are_refused_before_restore_replacement() {
    enum Damage {
        Missing,
        Truncated,
        Corrupt,
    }
    for damage in [Damage::Missing, Damage::Truncated, Damage::Corrupt] {
        let sb = Sandbox::new();
        let path = sb.compactable_session("rollout-bad-backup.jsonl", "bad-backup", "C:/work");
        let compact = compact_safe_impl(&path).unwrap();
        let compacted = fs::read(&path).unwrap();
        let backup = compact.backup.unwrap();
        match damage {
            Damage::Missing => fs::remove_file(&backup).unwrap(),
            Damage::Truncated => {
                let mut bytes = fs::read(&backup).unwrap();
                bytes.truncate(bytes.len() / 2);
                fs::write(&backup, bytes).unwrap();
            }
            Damage::Corrupt => {
                let mut bytes = fs::read(&backup).unwrap();
                for byte in bytes.iter_mut().take(16) {
                    *byte ^= 0x5a;
                }
                fs::write(&backup, bytes).unwrap();
            }
        }
        match restore_impl(&path, RestoreTarget::Original) {
            Err(VaultError::BackupMissing { .. }) => {}
            Ok(result) => assert_eq!(result.status, "failed"),
            Err(other) => panic!("unexpected restore error: {other}"),
        }
        assert_eq!(fs::read(&path).unwrap(), compacted);
    }
}

#[test]
fn invalid_and_truncated_manifests_are_never_partially_trusted() {
    for body in ["{ definitely not json", "{\"manifest_version\":2,"] {
        let sb = Sandbox::new();
        let path = sb.compactable_session(
            "rollout-invalid-manifest.jsonl",
            "invalid-manifest",
            "C:/work",
        );
        let archived = archive_impl(&path, false).unwrap();
        let before = fs::read(&path).unwrap();
        let manifest = archived.manifest.unwrap();
        fs::write(&manifest, body).unwrap();
        let error = restore_impl(&path, RestoreTarget::Latest).unwrap_err();
        assert!(matches!(
            error,
            VaultError::Json { .. } | VaultError::ManifestInvalid { .. }
        ));
        assert_eq!(fs::read(&path).unwrap(), before);

        let backup = archived.backup.unwrap();
        let prune = prune_one(&path, true, true).unwrap();
        assert!(backup.exists());
        assert!(prune["note"].as_str().unwrap().contains("refusing"));
    }
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
    codex_vault::index::build(None, true).unwrap();

    let fallback = compact_safe_impl(&session).unwrap();
    assert_eq!(fallback.status, "archived_only");
    assert_eq!(fallback.stats["recovery_source_created"], true);
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
    let fallback_value = compact_result_value(fallback);
    assert_eq!(fallback_value["search_index"]["may_be_stale"], true);
    assert!(
        manifest.anchors().iter().any(|a| a.backup_path == snapshot),
        "the fallback snapshot is not reachable from the journal"
    );
    assert_eq!(
        manifest.restore.source_size as usize,
        grown.len(),
        "restore should target the newest captured state"
    );

    let repeated = compact_result_value(compact_safe_impl(&session).unwrap());
    assert_eq!(repeated["status"], "archived_only");
    assert_eq!(repeated["stats"]["recovery_source_created"], true);
    assert_eq!(repeated["search_index"]["may_be_stale"], true);

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
fn pre_replacement_verification_failure_does_not_claim_the_index_became_stale() {
    let _sb = Sandbox::new();
    fs::write(
        codex_vault::index::database_path(),
        b"existing derived index sentinel",
    )
    .unwrap();
    let result = CommandResult {
        status: "verification_failed".to_string(),
        session: "synthetic".to_string(),
        manifest: None,
        backup: None,
        reason: vec!["generated compact output was rejected before replacement".to_string()],
        stats: json!({}),
    };
    let value = compact_result_value(result);
    assert!(value.get("search_index").is_none());
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

#[test]
fn restore_reversibility_keeps_the_newer_state_reachable_and_records_both_transitions() {
    let sb = Sandbox::new();
    let session = sb.compactable_session(
        "rollout-restore-reversible.jsonl",
        "restore-reversible",
        "C:/work",
    );
    let state_a = fs::read(&session).unwrap();
    compact_safe_impl(&session).unwrap();
    append_jsonl(&session, &completed_turn("state-c"));
    let state_c = fs::read(&session).unwrap();
    let state_c_sha = codex_vault::hashing::sha256_file(&session).unwrap();

    restore_impl(&session, RestoreTarget::Original).unwrap();
    assert_eq!(fs::read(&session).unwrap(), state_a);

    let listed = codex_vault::ops::list_anchors(&session).unwrap();
    let state_c_anchor = listed["anchors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|anchor| anchor["source_sha256"] == state_c_sha)
        .expect("state C must appear in restore --list after restoring A");
    let state_c_backup = PathBuf::from(state_c_anchor["backup_path"].as_str().unwrap());
    assert_eq!(state_c_anchor["exists"], true);

    restore_impl(&session, RestoreTarget::Backup(state_c_backup)).unwrap();
    assert_eq!(fs::read(&session).unwrap(), state_c);
    assert_eq!(
        codex_vault::hashing::sha256_file(&session).unwrap(),
        state_c_sha
    );

    let manifest = load_manifest(&manifest_path(
        &ensure_vault_paths().unwrap(),
        &VaultKey::for_rollout(&session),
    ))
    .unwrap()
    .unwrap();
    let restore_events: Vec<_> = manifest
        .history
        .iter()
        .filter(|entry| entry.operation == "restore")
        .map(|entry| entry.outcome.as_str())
        .collect();
    assert!(
        restore_events.ends_with(&["prepared", "restored", "prepared", "restored"]),
        "unexpected restore history: {restore_events:?}"
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
