use super::shared::{manifest_for, open_journal, ManifestDraft};
use super::CommandResult;
use crate::backup::create_verified_backup;
use crate::error::Result;
use crate::fsatomic::{lock_session, MutationGuard};
use crate::manifest::{write_manifest, write_summary, Mode, Status};
use crate::paths::{backup_path, ensure_vault_paths, manifest_path, snapshot_backup_path};
use crate::rollout::{ensure_plain_native_session, read_session_head};
use crate::util::{format_size, now_iso_utc};
use serde_json::json;
use std::path::Path;

pub fn archive_impl(path: &Path, force: bool) -> Result<CommandResult> {
    ensure_plain_native_session(path)?;
    let vault = ensure_vault_paths()?;
    let _operation = MutationGuard::acquire(&vault.root, path)?;
    let _lock = lock_session(path)?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let immutable_backup = backup_path(&vault, &journal.key);

    if immutable_backup.exists() && !force {
        return Ok(CommandResult {
            status: "exists".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_path(&vault, &journal.key)),
            backup: Some(immutable_backup),
            reason: vec!["immutable original backup already exists".to_string()],
            stats: json!({}),
        });
    }

    let is_first = !immutable_backup.exists();
    let target = if is_first {
        immutable_backup.clone()
    } else {
        snapshot_backup_path(&vault, &journal.key)
    };
    let anchor = create_verified_backup(path, &target)?;
    let size = anchor.source_size;

    // Even a `--force` snapshot is recorded, so it stays reachable from `restore --list`.
    let original = if is_first {
        anchor.clone()
    } else {
        journal
            .manifest
            .as_ref()
            .map(|m| m.original.clone())
            .unwrap_or_else(|| anchor.clone())
    };
    let mut manifest = manifest_for(
        ManifestDraft {
            session_id: &session_id,
            head: &head,
            path,
            mode: Mode::Archive,
            original: &original,
            restore: &anchor,
            result_size: size,
            result_sha256: &anchor.source_sha256,
            notes: vec!["archive-only mode; native transcript unchanged".to_string()],
        },
        journal.manifest.clone(),
    );
    manifest.status = Status::Ok;
    manifest.committed_at = Some(now_iso_utc());
    manifest.record(
        now_iso_utc(),
        "archive",
        if is_first {
            "created-immutable-original"
        } else {
            "created-snapshot"
        },
        Some(anchor.clone()),
        (!is_first).then(|| {
            "--force preserved the immutable original and captured a separate snapshot".to_string()
        }),
    );
    let manifest_file = write_manifest(&journal.key, &vault, &manifest)?;
    let _ = write_summary(&journal.key, &vault, &manifest);

    Ok(CommandResult {
        status: if is_first {
            "ok".to_string()
        } else {
            "snapshot_created".to_string()
        },
        session: path.to_string_lossy().to_string(),
        manifest: Some(manifest_file),
        backup: Some(target),
        reason: manifest.notes.clone(),
        stats: json!({
            "size": size,
            "size_human": format_size(size),
            "sha256": anchor.source_sha256,
        }),
    })
}
