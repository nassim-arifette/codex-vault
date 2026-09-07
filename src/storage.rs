//! Logical file sizes, including every retained backup. Filesystem allocation is not measured.
use crate::error::{Result, VaultError};
use crate::hashing::sha256_zstd_decompressed;
use crate::manifest::{load_manifest, Manifest};
use crate::paths::{codex_root, normalized_path, vault_paths, VaultPaths};
use crate::rollout::{is_codex_zstd_jsonl, is_plain_jsonl};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
