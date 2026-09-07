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

/// Best-effort process lifetime peak resident memory. Chain reports use this instead of a current
/// RSS sample so a short-lived spike during compression/rewrite cannot be hidden by measuring late.
#[cfg(windows)]
pub fn process_peak_rss_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    (ok != 0).then_some(counters.PeakWorkingSetSize as u64)
}

#[cfg(target_os = "linux")]
pub fn process_peak_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let ok = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if ok != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    Some((usage.ru_maxrss as u64).saturating_mul(1024))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_peak_rss_bytes() -> Option<u64> {
    None
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
