use crate::error::{Result, VaultError};
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageFileKind {
    Backup,
    Manifest,
    Summary,
    Transaction,
}

impl StorageFileKind {
    fn as_str(self) -> &'static str {
        match self {
            StorageFileKind::Backup => "backup",
            StorageFileKind::Manifest => "manifest",
            StorageFileKind::Summary => "summary",
            StorageFileKind::Transaction => "transaction",
        }
    }
}

#[derive(Clone, Debug)]
pub struct NativeStorageFile {
    path: PathBuf,
    before_bytes: u64,
}

impl NativeStorageFile {
    pub fn from_before(path: impl Into<PathBuf>, before_bytes: u64) -> Self {
        Self {
            path: path.into(),
            before_bytes,
        }
    }

    pub fn capture(path: &Path) -> Result<Self> {
        Ok(Self::from_before(path, logical_file_bytes(path)?))
    }
}

#[derive(Clone, Debug)]
pub struct TrackedStorageFile {
    kind: StorageFileKind,
    path: PathBuf,
    before_bytes: u64,
}

impl TrackedStorageFile {
    pub fn capture(kind: StorageFileKind, path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let before_bytes = logical_file_bytes(&path)?;
        Ok(Self {
            kind,
            path,
            before_bytes,
        })
    }

    /// Record a path whose creator guarantees it did not exist before this operation.
    ///
    /// Backup and chain-transaction names contain a fresh operation id/timestamp and are committed
    /// with create-new semantics. Avoiding a redundant directory scan is the point of this helper.
    pub fn assumed_new(kind: StorageFileKind, path: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            path: path.into(),
            before_bytes: 0,
        }
    }
}

fn logical_file_bytes(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(VaultError::io("measuring operation storage", path, error)),
    }
}

/// Measure only persistent files this operation explicitly owns.
///
/// This intentionally does not walk the Vault. Callers capture the native transcript(s) plus the
/// exact manifest/summary paths before mutation, then add newly created backup/transaction paths.
/// Unrelated Vault activity can therefore neither inflate nor hide the reported delta.
/// Callers should include only tracked paths the operation is known to have written successfully.
pub fn operation_storage_report(
    native: &[NativeStorageFile],
    tracked: &[TrackedStorageFile],
) -> Result<Value> {
    let mut native_before = 0u64;
    let mut native_after = 0u64;
    let mut native_files = Vec::with_capacity(native.len());
    for file in native {
        let after_bytes = logical_file_bytes(&file.path)?;
        native_before = native_before.saturating_add(file.before_bytes);
        native_after = native_after.saturating_add(after_bytes);
        native_files.push(json!({
            "path": file.path,
            "before_bytes": file.before_bytes,
            "after_bytes": after_bytes,
            "delta_bytes": after_bytes as i128 - file.before_bytes as i128,
        }));
    }

    let mut files_created = Vec::new();
    let mut files_modified = Vec::new();
    let mut files_deleted = Vec::new();
    let mut persistent_vault_delta = 0i128;
    let mut backup_bytes_created = 0u64;
    let mut manifest_growth = 0i128;
    let mut summary_growth = 0i128;
    let mut transaction_growth = 0i128;

    for file in tracked {
        let after_bytes = logical_file_bytes(&file.path)?;
        let delta = after_bytes as i128 - file.before_bytes as i128;
        persistent_vault_delta += delta;
        match file.kind {
            StorageFileKind::Backup => {
                if file.before_bytes == 0 {
                    backup_bytes_created = backup_bytes_created.saturating_add(after_bytes);
                }
            }
            StorageFileKind::Manifest => manifest_growth += delta,
            StorageFileKind::Summary => summary_growth += delta,
            StorageFileKind::Transaction => transaction_growth += delta,
        }

        if file.before_bytes == 0 && after_bytes > 0 {
            files_created.push(json!({
                "kind": file.kind.as_str(),
                "path": file.path,
                "bytes": after_bytes,
            }));
        } else if file.before_bytes > 0 && after_bytes == 0 {
            files_deleted.push(json!({
                "kind": file.kind.as_str(),
                "path": file.path,
                "bytes_removed": file.before_bytes,
            }));
        } else if file.before_bytes > 0 && after_bytes > 0 {
            files_modified.push(json!({
                "kind": file.kind.as_str(),
                "path": file.path,
                "before_bytes": file.before_bytes,
                "after_bytes": after_bytes,
                "delta_bytes": delta,
            }));
        }
    }

    let native_saved = native_before as i128 - native_after as i128;
    let metadata_growth = manifest_growth + summary_growth + transaction_growth;
    let net_saved = native_saved - persistent_vault_delta;
    Ok(json!({
        "accounting_version": 2,
        "scope": "current_operation",
        "measurement": "logical_bytes",
        "native_before_bytes": native_before,
        "native_after_bytes": native_after,
        "native_saved_bytes": native_before.saturating_sub(native_after),
        "native_files": native_files,
        "backup_bytes_created": backup_bytes_created,
        // Kept as an alias for clients that consumed the v1 top-level delta field.
        "new_backup_bytes": backup_bytes_created,
        "manifest_growth_bytes": manifest_growth,
        "summary_growth_bytes": summary_growth,
        "transaction_growth_bytes": transaction_growth,
        "metadata_growth_bytes": metadata_growth,
        "persistent_vault_delta_bytes": persistent_vault_delta,
        "files_created": files_created,
        "files_modified": files_modified,
        "files_deleted": files_deleted,
        "net_saved_bytes": net_saved,
        "space_increased": net_saved < 0,
    }))
}
