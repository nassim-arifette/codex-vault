//! Creating and validating the verified zstd backups the vault recovers from.

use crate::error::{Result, VaultError};
use crate::fsatomic::TempFile;
use crate::hashing::{
    compress_file_with_input_sha, sha256_file, sha256_zstd_decompressed,
    sha256_zstd_decompressed_with_size,
};
use crate::manifest::{load_manifest, Manifest, RecoveryAnchor};
use crate::paths::{
    backup_path, precompact_backup_path, snapshot_backup_path, VaultKey, VaultPaths,
};
use std::fs;
use std::path::{Path, PathBuf};

/// The two anchors a compaction needs: the state it can undo to, and the immutable original.
#[derive(Debug)]
pub struct BackupState {
    /// Exact transcript state immediately before this destructive compaction.
    pub restore: RecoveryAnchor,
    /// First immutable full backup ever created for this session.
    pub original: RecoveryAnchor,
    /// True when this operation had to capture a new pre-compaction snapshot, i.e. the session
    /// had grown since the immutable original was taken.
    pub captured_new_snapshot: bool,
}

pub fn create_verified_backup(src: &Path, target: &Path) -> Result<RecoveryAnchor> {
    create_verified_backup_of(src, target, None)
}

/// Compress `src` into `target`, proving the archive decodes back to what was read.
///
/// `expected_source_sha` is the hash a caller already computed while reading the transcript for
/// another purpose. Supplying it turns the concurrent-write check into a comparison against that
/// earlier read instead of a second full pass over the file — the same guarantee, one fewer
/// traversal of what may be several gigabytes.
pub fn create_verified_backup_of(
    src: &Path,
    target: &Path,
    expected_source_sha: Option<&str>,
) -> Result<RecoveryAnchor> {
    // Every early return from here on drops `temp`, which deletes the partial archive.
    let temp = TempFile::beside(target, "archive");
    let source_size = fs::metadata(src)
        .map_err(|e| VaultError::io("reading session size", src, e))?
        .len();
    let input_sha = compress_file_with_input_sha(src, temp.path(), 3)?;
    let unchanged = match expected_source_sha {
        Some(expected) => expected == input_sha,
        None => sha256_file(src)? == input_sha,
    };
    if !unchanged {
        return Err(VaultError::SessionChanged {
            stage: "backup creation",
        });
    }
    let decoded_sha = sha256_zstd_decompressed(temp.path())?;
    if decoded_sha != input_sha {
        return Err(VaultError::mismatch(
            "zstd backup does not decode back to the source",
            input_sha,
            decoded_sha,
        ));
    }
    let compressed_sha = sha256_file(temp.path())?;
    temp.commit_onto(target)?;
    Ok(RecoveryAnchor {
        backup_path: target.to_path_buf(),
        backup_sha256: compressed_sha,
        source_sha256: input_sha,
        source_size,
    })
}

/// Re-verify the immutable original against the journal.
///
/// Both the decompressed content *and* the archive bytes are checked. Previously each check was
/// skipped when its manifest key happened to be absent; the typed manifest makes them required,
/// so a corrupted or swapped original can no longer pass unnoticed.
fn verify_original_against_manifest(
    manifest: &Manifest,
    decoded_sha: &str,
    decoded_size: u64,
    compressed_sha: &str,
) -> Result<()> {
    if manifest.original.source_sha256 != decoded_sha {
        return Err(VaultError::mismatch(
            "immutable original backup decoded hash",
            &manifest.original.source_sha256,
            decoded_sha,
        ));
    }
    if manifest.original.source_size != decoded_size {
        return Err(VaultError::mismatch(
            "immutable original backup decoded size",
            manifest.original.source_size,
            decoded_size,
        ));
    }
    if manifest.original.backup_sha256 != compressed_sha {
        return Err(VaultError::mismatch(
            "immutable original compressed SHA-256",
            &manifest.original.backup_sha256,
            compressed_sha,
        ));
    }
    Ok(())
}

/// `current_sha` must be the hash of `path` as the caller last read it; the analysis pass
/// produces it for free. `journal` is the manifest already opened for this rollout, if any.
pub fn ensure_backup_for_compaction(
    path: &Path,
    key: &VaultKey,
    vault: &VaultPaths,
    current_sha: &str,
    journal: Option<&Manifest>,
) -> Result<BackupState> {
    let original_backup = journal
        .map(|m| m.original.backup_path.clone())
        .unwrap_or_else(|| backup_path(vault, key));
    let current_size = fs::metadata(path)
        .map_err(|e| VaultError::io("reading session size", path, e))?
        .len();

    let original = if !original_backup.exists() {
        if journal.is_some() {
            return Err(VaultError::BackupMissing {
                path: original_backup,
            });
        }
        create_verified_backup_of(path, &original_backup, Some(current_sha))?
    } else {
        let (decoded_sha, decoded_size) = sha256_zstd_decompressed_with_size(&original_backup)?;
        let compressed_sha = sha256_file(&original_backup)?;
        if let Some(m) = journal {
            verify_original_against_manifest(m, &decoded_sha, decoded_size, &compressed_sha)?;
        }
        RecoveryAnchor {
            backup_path: original_backup.clone(),
            backup_sha256: compressed_sha,
            source_sha256: decoded_sha,
            source_size: decoded_size,
        }
    };

    if current_sha == original.source_sha256 {
        return Ok(BackupState {
            restore: original.clone(),
            original,
            captured_new_snapshot: false,
        });
    }

    // The session has grown since an earlier archive/compaction. Never overwrite the immutable
    // original; capture the exact current state so `restore` undoes this operation rather than
    // rewinding the conversation to the first archive.
    let restore =
        create_verified_backup_of(path, &precompact_backup_path(vault, key), Some(current_sha))?;
    if restore.source_sha256 != current_sha || restore.source_size != current_size {
        return Err(VaultError::SessionChanged {
            stage: "pre-compaction backup",
        });
    }

    Ok(BackupState {
        restore,
        original,
        captured_new_snapshot: true,
    })
}

/// Capture the current transcript without changing it, reusing the immutable original when the
/// session has not moved since it was taken.
pub fn archive_current_locked(
    path: &Path,
    key: &VaultKey,
    vault: &VaultPaths,
    current_sha: &str,
) -> Result<(RecoveryAnchor, bool)> {
    let immutable = backup_path(vault, key);
    if !immutable.exists() {
        return Ok((
            create_verified_backup_of(path, &immutable, Some(current_sha))?,
            true,
        ));
    }

    let (decoded_sha, decoded_size) = sha256_zstd_decompressed_with_size(&immutable)?;
    if current_sha == decoded_sha {
        return Ok((
            RecoveryAnchor {
                backup_path: immutable.clone(),
                backup_sha256: sha256_file(&immutable)?,
                source_sha256: decoded_sha,
                source_size: decoded_size,
            },
            false,
        ));
    }

    let snapshot = snapshot_backup_path(vault, key);
    Ok((
        create_verified_backup_of(path, &snapshot, Some(current_sha))?,
        true,
    ))
}

/// Backups on disk that no manifest anchor references. Reported by `doctor` so a snapshot can
/// never be silently orphaned the way an unrecorded fallback archive used to be.
pub fn unreferenced_backups(
    vault: &VaultPaths,
    keys: &[VaultKey],
    manifest: Option<&Manifest>,
) -> Result<Vec<PathBuf>> {
    let mut known: Vec<PathBuf> = manifest
        .map(|m| m.anchors().into_iter().map(|a| a.backup_path).collect())
        .unwrap_or_default();
    // A legacy thread key can belong to a sibling rollout. Only the union of ALL journals
    // can prove a backup unreferenced. Any unreadable journal makes deletion undecidable.
    for entry in fs::read_dir(&vault.manifests)? {
        let path = entry?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Some(m) = load_manifest(&path)? {
            known.extend(m.anchors().into_iter().map(|a| a.backup_path));
        }
    }
    // Both the current key and the legacy thread-id key are scanned, so backups written before
    // the vault was re-keyed are still audited rather than quietly forgotten.
    let prefixes: Vec<String> = keys.iter().map(|k| format!("{k}.")).collect();
    let Ok(entries) = fs::read_dir(&vault.backups) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.ends_with(".jsonl.zst") && prefixes.iter().any(|pre| n.starts_with(pre))
                })
        })
        .filter(|p| !known.iter().any(|k| paths_equal(k, p)))
        .collect();
    out.sort();
    Ok(out)
}

/// Compare two paths without being fooled by `\\?\` prefixes or case differences that
/// `canonicalize` introduces on Windows.
pub fn paths_equal(a: &Path, b: &Path) -> bool {
    fn key(p: &Path) -> String {
        let s = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let s = s.to_string_lossy().replace('\\', "/");
        let s = s.strip_prefix("//?/").unwrap_or(&s).to_string();
        s.to_ascii_lowercase()
    }
    key(a) == key(b)
}
