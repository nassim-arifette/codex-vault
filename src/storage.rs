//! Logical file sizes, including every retained backup. Filesystem allocation is not measured.
use crate::error::{Result, VaultError};
use crate::hashing::sha256_zstd_decompressed;
use crate::manifest::Manifest;
use crate::paths::VaultPaths;
use serde::Serialize;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

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

#[derive(Debug, Serialize)]
pub struct StorageSnapshot {
    pub native_bytes: u64,
    pub vault_bytes: u64,
    pub backup_bytes: u64,
}

impl StorageSnapshot {
    pub fn read(path: &Path, vault: &VaultPaths) -> Result<Self> {
        Ok(Self {
            native_bytes: fs::metadata(path)?.len(),
            vault_bytes: directory_bytes(&vault.root)?,
            backup_bytes: directory_bytes(&vault.backups)?,
        })
    }
    pub fn delta(&self, after: &Self) -> Value {
        let before_total = self.native_bytes as i128 + self.vault_bytes as i128;
        let after_total = after.native_bytes as i128 + after.vault_bytes as i128;
        json!({
            "scope": "selected_transcript_and_entire_vault", "measurement": "logical_bytes",
            "before": self, "after": after,
            "net_saved_bytes": before_total - after_total,
            "new_backup_bytes": after.backup_bytes as i128 - self.backup_bytes as i128,
            "space_increased": after_total > before_total
        })
    }
}

struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Same encoder and level as a real backup, writing only to a byte counter.
pub fn compressed_size(path: &Path) -> Result<u64> {
    let mut encoder = zstd::stream::Encoder::new(Counter(0), 3)?;
    io::copy(&mut File::open(path)?, &mut encoder)?;
    Ok(encoder.finish()?.0)
}

pub fn preview(
    path: &Path,
    manifest: Option<&Manifest>,
    current_sha: &str,
    result_size: u64,
    needs_backup: bool,
) -> Result<Value> {
    let before = fs::metadata(path)?.len();
    // Compaction only reuses the immutable original when it matches the current transcript.
    let reuse = if let Some(m) = manifest {
        m.original.source_sha256 == current_sha
            && m.original.backup_path.is_file()
            && sha256_zstd_decompressed(&m.original.backup_path)? == current_sha
    } else {
        false
    };
    let new_backup = if needs_backup && !reuse {
        compressed_size(path)?
    } else {
        0
    };
    let saved = before as i128 - result_size as i128 - new_backup as i128;
    Ok(
        json!({"input_size":before, "result_size":result_size, "native_transcript_changed":false,
        "storage_preview":{"new_backup_bytes":new_backup, "estimated_net_saved_bytes_excluding_metadata":saved,
            "metadata_growth_bytes":null, "may_increase_usage":saved <= 0,
            "note":"Preview excludes journal/summary growth; actual operation reports all retained backups and vault files."}}),
    )
}
