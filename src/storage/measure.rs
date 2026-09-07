use crate::error::{Result, VaultError};
use crate::paths::VaultPaths;
use serde::Serialize;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct VaultStorageBreakdown {
    pub total_bytes: u64,
    pub backup_bytes: u64,
    pub metadata_bytes: u64,
    pub index_bytes: u64,
}

/// Split Vault storage into retained recovery archives, the optional SQLite index (including WAL
/// sidecars) and the remaining recovery metadata/journals. These are logical file bytes.
pub fn vault_storage_breakdown(vault: &VaultPaths) -> Result<VaultStorageBreakdown> {
    let total_bytes = directory_bytes(&vault.root)?;
    let backup_bytes = directory_bytes(&vault.backups)?;
    let mut index_bytes = 0u64;
    if vault.root.is_dir() {
        for entry in fs::read_dir(&vault.root)? {
            let path = entry?.path();
            if path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "index.sqlite" || n.starts_with("index.sqlite-"))
            {
                index_bytes = index_bytes.saturating_add(fs::metadata(path)?.len());
            }
        }
    }
    Ok(VaultStorageBreakdown {
        total_bytes,
        backup_bytes,
        index_bytes,
        metadata_bytes: total_bytes
            .saturating_sub(backup_bytes)
            .saturating_sub(index_bytes),
    })
}

pub fn directory_bytes(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry =
            entry.map_err(|e| VaultError::io("measuring storage", path, io::Error::other(e)))?;
        if entry.file_type().is_file() {
            total += fs::metadata(entry.path())?.len();
        }
    }
    Ok(total)
}
