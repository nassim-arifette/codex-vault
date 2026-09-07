use super::measure::vault_storage_breakdown;
use crate::error::{Result, VaultError};
use crate::manifest::load_manifest;
use crate::paths::{codex_root, normalized_path, vault_paths};
use crate::rollout::{is_codex_zstd_jsonl, is_plain_jsonl};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct StorageCategory {
    files: u64,
    bytes: u64,
}

impl StorageCategory {
    fn add(&mut self, bytes: u64) {
        self.files = self.files.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

fn path_key(path: &Path) -> String {
    let key = normalized_path(path).to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key
    }
}

fn backup_kind(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name.contains(".original.") {
        "immutable_originals"
    } else if name.contains(".prechain-") {
        "prechain_snapshots"
    } else if name.contains(".prerestore-chain-") {
        "prerestore_chain_snapshots"
    } else if name.contains(".precompact-") {
        "precompact_snapshots"
    } else if name.contains(".prerestore-") {
        "prerestore_snapshots"
    } else if name.contains(".snapshot-") {
        "manual_snapshots"
    } else {
        "other_recovery_anchors"
    }
}

fn native_rollout_storage() -> Result<StorageCategory> {
    let mut native = StorageCategory::default();
    for source in ["sessions", "archived_sessions"] {
        let root = codex_root().join(source);
        if !root.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry.map_err(|e| {
                VaultError::io(
                    "measuring native rollout storage",
                    &root,
                    io::Error::other(e),
                )
            })?;
            let path = entry.path();
            if entry.file_type().is_file() && (is_plain_jsonl(path) || is_codex_zstd_jsonl(path)) {
                native.add(fs::metadata(path)?.len());
            }
        }
    }
    Ok(native)
}

fn backup_owner_key(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.strip_suffix(".jsonl.zst")?;
    for marker in [
        ".prerestore-chain-",
        ".precompact-",
        ".prerestore-",
        ".prechain-",
        ".snapshot-",
        ".original",
    ] {
        if let Some(index) = name.rfind(marker) {
            let owner = &name[..index];
            if !owner.is_empty() {
                return Some(owner.to_string());
            }
        }
    }
    None
}

#[derive(Default)]
struct BackupFiles {
    archives: BTreeMap<String, (PathBuf, u64)>,
    other_files: StorageCategory,
}

fn backup_files(backups: &Path) -> Result<BackupFiles> {
    let mut files = BackupFiles::default();
    if !backups.exists() {
        return Ok(files);
    }
    for entry in walkdir::WalkDir::new(backups).follow_links(false) {
        let entry = entry.map_err(|e| {
            VaultError::io("measuring backup storage", backups, io::Error::other(e))
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let bytes = fs::metadata(&path)?.len();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".jsonl.zst"))
        {
            files.archives.insert(path_key(&path), (path, bytes));
        } else {
            files.other_files.add(bytes);
        }
    }
    Ok(files)
}

/// Read-only inventory of native rollouts, recovery anchors, unreferenced/ambiguous backups,
/// recovery metadata and the rebuildable search index.
///
/// Backup garbage classification is intentionally fail-closed: if any recovery journal cannot be
/// read, files not referenced by the journals we *could* read are reported as ambiguous rather
/// than unreferenced. This keeps the inventory useful without ever presenting a potentially
/// required recovery snapshot as safe garbage.
pub fn inventory() -> Result<Value> {
    let native = native_rollout_storage()?;
    let vault = vault_paths();
    let vault_breakdown = vault_storage_breakdown(&vault)?;
    let backup_files = backup_files(&vault.backups)?;

    let mut referenced: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut readable_manifest_keys = BTreeSet::new();
    let mut readable_manifests = 0u64;
    let mut unreadable_manifests = Vec::new();
    let mut recorded_anchor_references = 0u64;
    if vault.manifests.is_dir() {
        for entry in fs::read_dir(&vault.manifests)? {
            let path = entry?.path();
            if !path.is_file() || path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            match load_manifest(&path) {
                Ok(Some(manifest)) => {
                    readable_manifests = readable_manifests.saturating_add(1);
                    if let Some(key) = path.file_stem().and_then(|stem| stem.to_str()) {
                        readable_manifest_keys.insert(key.to_string());
                    }
                    for anchor in manifest.anchors() {
                        recorded_anchor_references = recorded_anchor_references.saturating_add(1);
                        referenced
                            .entry(path_key(&anchor.backup_path))
                            .or_insert(anchor.backup_path);
                    }
                }
                Ok(None) => {}
                Err(error) => unreadable_manifests.push(json!({
                    "path": path,
                    "code": error.code(),
                    "error": error.to_string(),
                })),
            }
        }
    }

    let mut required_total = StorageCategory::default();
    let mut required_by_kind: BTreeMap<&'static str, StorageCategory> = [
        "immutable_originals",
        "prechain_snapshots",
        "prerestore_chain_snapshots",
        "precompact_snapshots",
        "prerestore_snapshots",
        "manual_snapshots",
        "other_recovery_anchors",
    ]
    .into_iter()
    .map(|kind| (kind, StorageCategory::default()))
    .collect();
    let mut missing_referenced = 0u64;
    let mut external_referenced = StorageCategory::default();

    for (key, path) in &referenced {
        if let Some((stored_path, bytes)) = backup_files.archives.get(key) {
            required_total.add(*bytes);
            required_by_kind
                .get_mut(backup_kind(stored_path))
                .expect("all backup kinds initialized")
                .add(*bytes);
        } else if path.is_file() {
            let bytes = fs::metadata(path)?.len();
            required_total.add(bytes);
            let path_root = path_key(path);
            let vault_root = path_key(&vault.root);
            let inside_vault = path_root == vault_root
                || path_root
                    .strip_prefix(&vault_root)
                    .is_some_and(|tail| tail.starts_with('/'));
            if !inside_vault {
                external_referenced.add(bytes);
            }
            required_by_kind
                .get_mut(backup_kind(path))
                .expect("all backup kinds initialized")
                .add(bytes);
        } else {
            missing_referenced = missing_referenced.saturating_add(1);
        }
    }

    let journals_readable = unreadable_manifests.is_empty();
    let mut unreferenced = StorageCategory::default();
    let mut ambiguous = StorageCategory::default();
    let mut ambiguous_missing_owner_journal = 0u64;
    let mut ambiguous_unknown_owner = 0u64;
    for (key, (path, bytes)) in &backup_files.archives {
        if !referenced.contains_key(key) {
            if !journals_readable {
                ambiguous.add(*bytes);
                continue;
            }
            match backup_owner_key(path) {
                Some(owner) if readable_manifest_keys.contains(&owner) => unreferenced.add(*bytes),
                Some(_) => {
                    ambiguous.add(*bytes);
                    ambiguous_missing_owner_journal =
                        ambiguous_missing_owner_journal.saturating_add(1);
                }
                None => {
                    ambiguous.add(*bytes);
                    ambiguous_unknown_owner = ambiguous_unknown_owner.saturating_add(1);
                }
            }
        }
    }
    let reference_audit_complete = journals_readable && ambiguous.files == 0;

    let external_required_bytes = external_referenced.bytes;
    let total_bytes = native
        .bytes
        .saturating_add(vault_breakdown.total_bytes)
        .saturating_add(external_required_bytes);

    Ok(json!({
        "status": "ok",
        "kind": "storage_inventory",
        "measurement": "logical_file_bytes",
        "read_only": true,
        "native_rollouts": native,
        "required_recovery_anchors": {
            "files": required_total.files,
            "bytes": required_total.bytes,
            "recorded_anchor_references": recorded_anchor_references,
            "immutable_originals": required_by_kind["immutable_originals"],
            "prechain_snapshots": required_by_kind["prechain_snapshots"],
            "prerestore_chain_snapshots": required_by_kind["prerestore_chain_snapshots"],
            "precompact_snapshots": required_by_kind["precompact_snapshots"],
            "prerestore_snapshots": required_by_kind["prerestore_snapshots"],
            "manual_snapshots": required_by_kind["manual_snapshots"],
            "other_recovery_anchors": required_by_kind["other_recovery_anchors"],
            "external_to_vault_backups": external_referenced,
            "missing_referenced_files": missing_referenced,
        },
        "unreferenced_backups": if journals_readable {
            json!({
                "classification": "not_referenced_by_readable_owner_journal",
                "files": unreferenced.files,
                "bytes": unreferenced.bytes,
                "retention_eligibility": "not_assessed",
            })
        } else {
            json!({
                "classification": "indeterminate",
                "files": Value::Null,
                "bytes": Value::Null,
                "retention_eligibility": "not_assessed",
                "reason": "one or more recovery journals are unreadable; unmatched backups cannot be proven unreferenced",
            })
        },
        "ambiguous_backups": {
            "files": ambiguous.files,
            "bytes": ambiguous.bytes,
            "missing_owner_journal_files": ambiguous_missing_owner_journal,
            "unknown_owner_files": ambiguous_unknown_owner,
            "reason": if !journals_readable {
                "one or more recovery journals are unreadable; unmatched backups cannot be proven unreferenced"
            } else if ambiguous.files > 0 {
                "one or more backup archives have no readable owner journal or an unknown naming scheme"
            } else {
                "none"
            },
            "retention_eligibility": "not_assessed",
        },
        "other_backup_directory_files": {
            "files": backup_files.other_files.files,
            "bytes": backup_files.other_files.bytes,
            "classification": "not_backup_archives",
        },
        "recovery_metadata": {
            "bytes": vault_breakdown.metadata_bytes,
            "readable_manifests": readable_manifests,
            "unreadable_manifests": unreadable_manifests.len(),
            "unreadable_manifest_details": unreadable_manifests,
            "backup_reference_audit_complete": reference_audit_complete,
        },
        "search_index": {
            "bytes": vault_breakdown.index_bytes,
            "derived": true,
            "rebuildable": true,
            "rebuild_command": "codex-vault index --rebuild",
        },
        "vault": {
            "root": vault.root,
            "bytes": vault_breakdown.total_bytes,
            "backup_bytes": vault_breakdown.backup_bytes,
        },
        "total_bytes": total_bytes,
        "retention_eligibility": "not_assessed",
        "note": "Inventory only. No files were created, changed or deleted; this command does not decide whether any backup should be removed.",
    }))
}
