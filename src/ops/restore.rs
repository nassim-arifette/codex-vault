use super::shared::open_journal;
use super::CommandResult;
use crate::backup::{create_verified_backup_of, paths_equal};
use crate::error::{Result, VaultError};
use crate::fsatomic::{lock_session, MutationGuard, TempFile};
use crate::hashing::{decompress_file, sha256_file, sha256_rollout_prefix};
use crate::manifest::{write_manifest, write_summary, Manifest, RecoveryAnchor, Status};
use crate::paths::{
    backup_path, ensure_vault_paths, manifest_path, prerestore_backup_path, VaultKey, VaultPaths,
};
use crate::rollout::{ensure_plain_native_session, read_session_head, verify_jsonl};
use crate::util::{format_size, now_iso_utc};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Which recorded state a `restore` should put back.
#[derive(Clone, Debug, Default)]
pub enum RestoreTarget {
    /// The newest verified capture — what the journal's `restore` anchor points at.
    #[default]
    Latest,
    /// The first immutable full backup.
    Original,
    /// A specific backup file, which must be one of the manifest's anchors.
    Backup(PathBuf),
}

/// Choose which recorded anchor to put back, refusing anything the journal has not verified.
fn resolve_restore_anchor(
    manifest: Option<&Manifest>,
    vault: &VaultPaths,
    key: &VaultKey,
    target: &RestoreTarget,
) -> Result<RecoveryAnchor> {
    let Some(m) = manifest else {
        // No journal: the immutable original is the only thing we could possibly assert.
        let fallback = backup_path(vault, key);
        return Err(VaultError::ManifestInvalid {
            path: manifest_path(vault, key),
            reason: format!(
                "no manifest for this session; {} cannot be verified against a recorded state",
                fallback.display()
            ),
        });
    };
    match target {
        RestoreTarget::Latest => Ok(m.restore.clone()),
        RestoreTarget::Original => Ok(m.original.clone()),
        RestoreTarget::Backup(wanted) => m
            .anchors()
            .into_iter()
            .find(|a| paths_equal(&a.backup_path, wanted))
            .ok_or_else(|| VaultError::ManifestInvalid {
                path: manifest_path(vault, key),
                reason: format!(
                    "{} is not a recovery anchor recorded for this session; run `restore --list`",
                    wanted.display()
                ),
            }),
    }
}

pub fn restore_impl(path: &Path, target: RestoreTarget) -> Result<CommandResult> {
    ensure_plain_native_session(path)?;
    crate::fsatomic::ensure_supported_mutation_filesystem(path)?;
    let vault = ensure_vault_paths()?;
    let _operation = MutationGuard::acquire(&vault.root, path)?;
    let _lock = lock_session(path)?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let manifest_file = manifest_path(&vault, &journal.key);
    let manifest = journal.manifest.clone();
    let anchor = resolve_restore_anchor(manifest.as_ref(), &vault, &journal.key, &target)?;

    if !anchor.backup_path.exists() {
        return Err(VaultError::BackupMissing {
            path: anchor.backup_path,
        });
    }

    // Both checks are now unconditional. Previously a manifest missing either key skipped them.
    let compressed_sha = sha256_file(&anchor.backup_path)?;
    if compressed_sha != anchor.backup_sha256 {
        return Ok(CommandResult {
            status: "failed".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(anchor.backup_path),
            reason: vec![format!(
                "restore backup hash mismatch: expected {}, got {compressed_sha}",
                anchor.backup_sha256
            )],
            stats: json!({}),
        });
    }

    // Capture what is on disk *now* before overwriting it. Restoring an older anchor after Codex
    // has appended new turns would otherwise discard them with no way back.
    let current_size = fs::metadata(path)
        .map_err(|e| VaultError::io("reading session size", path, e))?
        .len();
    let current_sha = sha256_file(path)?;
    let mut reason = Vec::new();
    let pre_restore = if current_sha == anchor.source_sha256 {
        reason.push("transcript already matches the requested state".to_string());
        None
    } else {
        let captured = create_verified_backup_of(
            path,
            &prerestore_backup_path(&vault, &journal.key),
            Some(&current_sha),
        )?;
        if current_size > anchor.source_size
            && sha256_rollout_prefix(path, anchor.source_size)? == anchor.source_sha256
        {
            reason.push(format!(
                "the transcript grew by {} after this state was recorded; that content is \
                 preserved in {} and reachable with `restore --to`",
                format_size(current_size - anchor.source_size),
                captured.backup_path.display()
            ));
        } else {
            reason.push(format!(
                "the transcript as it stood ({}) was captured to {} before being replaced",
                format_size(current_size),
                captured.backup_path.display()
            ));
        }
        Some(captured)
    };

    let temp = TempFile::beside(path, "restore");
    decompress_file(&anchor.backup_path, temp.path())?;
    let (ok, issues) = verify_jsonl(temp.path())?;
    if !ok {
        return Ok(CommandResult {
            status: "failed".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(anchor.backup_path),
            reason: issues,
            stats: json!({}),
        });
    }
    let restored_sha = sha256_file(temp.path())?;
    if restored_sha != anchor.source_sha256 {
        return Ok(CommandResult {
            status: "failed".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(anchor.backup_path),
            reason: vec![format!(
                "restore content hash mismatch: expected {}, got {restored_sha}",
                anchor.source_sha256
            )],
            stats: json!({}),
        });
    }
    crate::util::test_abort("restore_temp_written");
    let mut m = manifest.ok_or(VaultError::Internal {
        detail: "restore anchor without manifest",
    })?;
    // Commit the undo anchor BEFORE changing any native bytes, exactly as compaction does.
    // A disk error or interrupted restore must never leave the only newer state prunable.
    m.status = Status::Prepared;
    m.committed_at = None;
    m.record(
        now_iso_utc(),
        "restore",
        "prepared",
        pre_restore.clone(),
        Some(format!("requested {}", anchor.backup_path.display())),
    );
    if pre_restore.is_none() {
        m.restore = anchor.clone();
    }
    write_manifest(&journal.key, &vault, &m)?;
    crate::util::test_abort("prepared_journal_written");
    let _replacement_lock = temp.replace_locked(path)?;
    crate::util::test_abort("atomic_replacement");
    let recovery_manifest = manifest_file.clone();
    (|| {
    let active_sha = sha256_file(path)?;
    let active_size = fs::metadata(path)?.len();
    if active_sha != anchor.source_sha256 || active_size != anchor.source_size {
        return Err(VaultError::mismatch("restored transcript after replacement", &anchor.source_sha256, &active_sha));
    }
    crate::util::test_abort("post_replacement_verified");
    {
        m.last_restored_at = Some(now_iso_utc());
        m.last_restore_sha256 = Some(restored_sha.clone());
        m.result_size = anchor.source_size;
        m.result_sha256 = restored_sha.clone();
        // `record` promotes the pre-restore capture to the newest anchor, so an unwanted restore
        // is itself undoable.
        m.record(
            now_iso_utc(),
            "restore",
            "restored",
            pre_restore.clone(),
            Some(format!("restored {}", anchor.backup_path.display())),
        );
        if pre_restore.is_none() {
            m.restore = anchor.clone();
        }
        m.status = Status::Ok;
        m.committed_at = Some(now_iso_utc());
        write_manifest(&journal.key, &vault, &m)?;
        crate::util::test_abort("final_journal_commit");
        let _ = write_summary(&journal.key, &vault, &m);
    }

    reason.push("session restored exactly to the requested recorded state".to_string());
    Ok(CommandResult {
        status: "ok".to_string(),
        session: path.to_string_lossy().to_string(),
        manifest: Some(manifest_file),
        backup: Some(anchor.backup_path),
        reason,
        stats: json!({
            "size": anchor.source_size,
            "sha256": restored_sha,
            "replaced_size": current_size,
            "pre_restore_backup": pre_restore.map(|a| a.backup_path.to_string_lossy().to_string()),
        }),
    })
    })().map_err(|e: VaultError| e.after_replacement(&recovery_manifest))
}

/// List every recovery anchor recorded for a session, newest last.
pub fn list_anchors(path: &Path) -> Result<Value> {
    let vault = ensure_vault_paths()?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let Some(m) = journal.manifest.clone() else {
        return Ok(json!({
            "session_id": session_id,
            "vault_key": journal.key,
            "anchors": [],
            "history": [],
        }));
    };
    let anchors: Vec<Value> = m
        .anchors()
        .into_iter()
        .map(|a| {
            json!({
                "backup_path": a.backup_path.to_string_lossy(),
                "exists": a.backup_path.exists(),
                "source_size": a.source_size,
                "source_size_human": format_size(a.source_size),
                "source_sha256": a.source_sha256,
                "is_original": paths_equal(&a.backup_path, &m.original.backup_path),
                "is_current_restore_target": paths_equal(&a.backup_path, &m.restore.backup_path),
            })
        })
        .collect();
    Ok(json!({
        "session_id": session_id,
        "vault_key": journal.key,
        "session": path.to_string_lossy(),
        "anchors": anchors,
        "history": m.history,
    }))
}
