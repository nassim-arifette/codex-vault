use super::*;

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
