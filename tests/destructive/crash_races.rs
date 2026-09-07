use super::*;

#[test]
fn every_single_file_compaction_crash_boundary_preserves_or_recovers_the_preoperation_state() {
    let stages = [
        ("backup_created", false, false),
        ("backup_verified", false, false),
        ("compact_temp_written", false, false),
        ("prepared_journal_written", false, true),
        ("atomic_replacement", true, true),
        ("post_replacement_verified", true, true),
        ("final_journal_commit", true, false),
    ];

    for (stage, replacement_happened, prepared_expected) in stages {
        let sb = Sandbox::new();
        let path = sb.compactable_session(
            &format!("rollout-crash-compact-{stage}.jsonl"),
            &format!("crash-compact-{stage}"),
            "C:/work",
        );
        let original = fs::read(&path).unwrap();
        let output = run_crash_stage(
            &sb,
            &["--json", "--no-progress", "compact", path.to_str().unwrap()],
            stage,
        );
        assert!(!output.status.success(), "stage {stage} should abort");

        if replacement_happened {
            assert_ne!(fs::read(&path).unwrap(), original, "stage {stage}");
        } else {
            assert_eq!(fs::read(&path).unwrap(), original, "stage {stage}");
        }

        let doctor = doctor_one(&path, DoctorDepth::Standard).unwrap();
        if prepared_expected {
            assert_eq!(
                doctor.status, "warning",
                "stage {stage}: {:?}",
                doctor.notes
            );
            assert!(
                doctor.notes.iter().any(|note| note.contains("prepared")),
                "stage {stage}: {:?}",
                doctor.notes
            );
        }
        assert_manifest_has_no_temp_anchor(&path);

        if replacement_happened {
            let restored = restore_impl(&path, RestoreTarget::Latest).unwrap();
            assert_eq!(restored.status, "ok", "stage {stage}");
            assert_eq!(fs::read(&path).unwrap(), original, "stage {stage}");
        }
    }
}

#[test]
fn every_restore_crash_boundary_preserves_or_recovers_the_newer_pre_restore_state() {
    let stages = [
        ("backup_created", false, false),
        ("backup_verified", false, false),
        ("restore_temp_written", false, false),
        ("prepared_journal_written", false, true),
        ("atomic_replacement", true, true),
        ("post_replacement_verified", true, true),
        ("final_journal_commit", true, false),
    ];

    for (stage, replacement_happened, prepared_expected) in stages {
        let sb = Sandbox::new();
        let path = sb.compactable_session(
            &format!("rollout-crash-restore-{stage}.jsonl"),
            &format!("crash-restore-{stage}"),
            "C:/work",
        );
        compact_safe_impl(&path).unwrap();
        append_jsonl(&path, &completed_turn(&format!("newer-{stage}")));
        let newer = fs::read(&path).unwrap();
        let original_anchor = load_manifest(&manifest_path(
            &ensure_vault_paths().unwrap(),
            &VaultKey::for_rollout(&path),
        ))
        .unwrap()
        .unwrap()
        .original;

        let output = run_crash_stage(
            &sb,
            &[
                "--json",
                "--no-progress",
                "restore",
                path.to_str().unwrap(),
                "--original",
            ],
            stage,
        );
        assert!(!output.status.success(), "stage {stage} should abort");

        if replacement_happened {
            assert_eq!(
                codex_vault::hashing::sha256_file(&path).unwrap(),
                original_anchor.source_sha256,
                "stage {stage}"
            );
        } else {
            assert_eq!(fs::read(&path).unwrap(), newer, "stage {stage}");
        }

        let doctor = doctor_one(&path, DoctorDepth::Standard).unwrap();
        if prepared_expected {
            assert_eq!(
                doctor.status, "warning",
                "stage {stage}: {:?}",
                doctor.notes
            );
            assert!(
                doctor.notes.iter().any(|note| note.contains("prepared")),
                "stage {stage}: {:?}",
                doctor.notes
            );
        }
        assert_manifest_has_no_temp_anchor(&path);

        if replacement_happened {
            let restored = restore_impl(&path, RestoreTarget::Latest).unwrap();
            assert_eq!(restored.status, "ok", "stage {stage}");
            assert_eq!(fs::read(&path).unwrap(), newer, "stage {stage}");
        }
    }
}

#[cfg(windows)]
#[test]
fn a_concurrently_open_chain_page_refuses_the_whole_operation_without_partial_rewrite() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let sb = Sandbox::new();
    let root = lineage_page(&sb, "rollout-33-busychain.jsonl", "busychain", None, "ROOT");
    let root_size = fs::metadata(&root).unwrap().len();
    let leaf = lineage_page(
        &sb,
        "rollout-34-busychain_leaf.jsonl",
        "busychain",
        Some(("busychain", root_size)),
        "LEAF",
    );
    let before = [fs::read(&root).unwrap(), fs::read(&leaf).unwrap()];
    // Simulate Codex holding a writer-capable handle to one member of the dependency closure.
    let _writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .open(&leaf)
        .unwrap();

    let error = compact_conversation("busychain", None, CompactOptions::default()).unwrap_err();
    assert_eq!(error.code(), "session_locked");
    assert_eq!(fs::read(&root).unwrap(), before[0]);
    assert_eq!(fs::read(&leaf).unwrap(), before[1]);
}

#[cfg(windows)]
#[test]
fn a_writer_held_rollout_is_refused_with_a_clear_in_use_error() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let sb = Sandbox::new();
    let path = sb.compactable_session("rollout-active-writer.jsonl", "active-writer", "C:/work");
    let before = fs::read(&path).unwrap();
    let _writer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_codex-vault"))
        .args(["--json", "--no-progress", "compact", path.to_str().unwrap()])
        .env("CODEX_HOME", sb.dir.path().join("codex"))
        .env("CODEX_VAULT_HOME", sb.vault())
        .output()
        .unwrap();
    let text = combined_output(&output);
    assert!(!output.status.success());
    assert!(text.contains("session_locked"), "{text}");
    assert!(
        text.contains("session may still be open in Codex"),
        "{text}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn concurrent_appends_are_detected_at_every_single_file_compaction_stage() {
    for stage in ["analysis", "backup", "compact_output", "pre_replace"] {
        let sb = Sandbox::new();
        let path = sb.compactable_session(
            &format!("rollout-race-{stage}.jsonl"),
            &format!("race-{stage}"),
            "C:/work",
        );
        let before = fs::read(&path).unwrap();
        let marker = format!("ACTIVE-001-{stage}-APPEND-MUST-SURVIVE");
        let output = run_compact_paused_at(&sb, &path, stage, true, || {
            append_race_marker(&path, &marker)
                .expect("race append should be admitted by test hook");
        });
        let text = combined_output(&output);
        assert!(
            !output.status.success(),
            "{stage}: concurrent append must not yield apparent success: {text}"
        );
        assert!(text.contains("session_changed"), "{stage}: {text}");
        assert!(
            text.contains("Vault did not replace the transcript"),
            "{stage}: {text}"
        );
        let after = fs::read(&path).unwrap();
        assert!(
            after.starts_with(&before),
            "{stage}: Vault rewrote pre-existing bytes after detecting a race"
        );
        assert!(
            String::from_utf8_lossy(&after).contains(&marker),
            "{stage}: valid appended content was lost"
        );
    }
}

#[test]
fn an_external_source_replacement_is_detected_even_when_bytes_are_identical() {
    let sb = Sandbox::new();
    let path = sb.compactable_session("rollout-race-replaced.jsonl", "race-replaced", "C:/work");
    let before = fs::read(&path).unwrap();
    let replacement = sb.sessions().join("replacement-byte-identical.jsonl");
    fs::write(&replacement, &before).unwrap();

    let output = run_compact_paused_at(&sb, &path, "pre_replace", true, || {
        codex_vault::fsatomic::atomic_replace(&replacement, &path)
            .expect("external byte-identical replacement should succeed in race harness");
    });
    let text = combined_output(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("session_changed"), "{text}");
    assert!(
        text.contains("pre-replacement source identity verification"),
        "{text}"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
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
    let sha = codex_vault::hashing::sha256_file(&p).unwrap();
    let mf = first.manifest.unwrap();
    let journal = fs::read(&mf).unwrap();
    let backups = sb.backups();
    let restore_target = load_manifest(&mf).unwrap().unwrap().restore;
    for attempt in 2..=4 {
        assert_eq!(
            compact_safe_impl(&p).unwrap().status,
            "already_compact",
            "attempt {attempt}"
        );
        assert_eq!(fs::read(&p).unwrap(), bytes, "attempt {attempt}");
        assert_eq!(
            codex_vault::hashing::sha256_file(&p).unwrap(),
            sha,
            "attempt {attempt}"
        );
        assert_eq!(fs::read(&mf).unwrap(), journal, "attempt {attempt}");
        assert_eq!(sb.backups(), backups, "attempt {attempt}");
        assert_eq!(
            load_manifest(&mf).unwrap().unwrap().restore.source_sha256,
            restore_target.source_sha256,
            "attempt {attempt}"
        );
    }
    let deep = doctor_one(&p, DoctorDepth::Deep).unwrap();
    assert_eq!(deep.status, "ok", "{:?}", deep.notes);
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
