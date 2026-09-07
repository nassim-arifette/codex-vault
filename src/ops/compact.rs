use super::shared::{manifest_for, open_journal, ManifestDraft};
use super::CommandResult;
use crate::analysis::analyze_session_within;
use crate::backup::{archive_current_locked, ensure_backup_for_compaction};
use crate::discovery::lineage_successors;
use crate::error::{Result, VaultError};
use crate::fsatomic::{
    copy_compacted_transcript, file_identity, lock_session, path_file_identity, FileIdentity,
    MutationGuard, TempFile,
};
use crate::hashing::{decompress_file, sha256_file};
use crate::manifest::{write_manifest, write_summary, CompactionRecord, Mode, Status};
use crate::paths::{ensure_vault_paths, manifest_path, VaultPaths};
use crate::rollout::{
    ensure_plain_native_session, read_session_head, verify_jsonl, DEFAULT_SCAN_WINDOW,
};
use crate::util::{format_size, now_iso_utc};
use serde_json::json;
use std::fs;
use std::path::Path;

const COMPATIBILITY_BASIS: &str = "Codex bounded model-context scan: valid compaction + completed \
     turn context; invalid compactions and rollback force full replay";

/// Knobs for one compaction.
#[derive(Clone, Copy, Debug)]
pub struct CompactOptions {
    /// Estimate compressed backup size without creating files or replacing the transcript.
    pub dry_run: bool,
    /// How far back the reverse walk may look; see [`DEFAULT_SCAN_WINDOW`].
    pub scan_window: usize,
    /// Compact rollouts belonging to threads Codex spawned.
    ///
    /// Off by default. Codex will not resume such a rollout standalone — "cannot resume an
    /// unloaded multi-agent v2 sub-agent through its parent" — so the differential harness
    /// cannot check that compacting one preserves what the model sees. Their `session_meta`
    /// also carries `subagent_history_start_ordinal`, which suggests a parent replays a child's
    /// history by position. Until that is proven safe, the vault leaves them alone.
    pub allow_spawned_threads: bool,
}

impl Default for CompactOptions {
    fn default() -> Self {
        CompactOptions {
            dry_run: false,
            scan_window: DEFAULT_SCAN_WINDOW,
            allow_spawned_threads: false,
        }
    }
}

pub fn compact_safe_impl(path: &Path) -> Result<CommandResult> {
    compact_safe_impl_with(path, CompactOptions::default())
}

pub fn compact_safe_impl_within(path: &Path, window: usize) -> Result<CommandResult> {
    compact_safe_impl_with(
        path,
        CompactOptions {
            scan_window: window,
            ..CompactOptions::default()
        },
    )
}

pub fn compact_safe_impl_with(path: &Path, options: CompactOptions) -> Result<CommandResult> {
    ensure_plain_native_session(path)?;
    if !options.dry_run {
        crate::fsatomic::ensure_supported_mutation_filesystem(path)?;
    }
    let vault = if options.dry_run {
        crate::paths::vault_paths()
    } else {
        ensure_vault_paths()?
    };
    let _operation = if options.dry_run {
        None
    } else {
        Some(MutationGuard::acquire(&vault.root, path)?)
    };
    let source_lock = lock_session(path)?;
    let source_identity = file_identity(&source_lock)?;
    let before = crate::storage::StorageSnapshot::read(path, &vault)?;
    let mut result = compact_locked(path, options, &vault, source_identity)?;
    if !options.dry_run {
        match crate::storage::StorageSnapshot::read(path, &vault) {
            Ok(after) => {
                let delta = before.delta(&after);
                if delta["space_increased"] == true {
                    result.reason.push(
                        "Total storage increased after including retained backups and journals."
                            .into(),
                    );
                }
                result.stats["storage"] = delta;
            }
            Err(err) => result.reason.push(format!(
                "Operation finished, but storage accounting failed: {err}"
            )),
        }
    }
    Ok(result)
}

fn compact_locked(
    path: &Path,
    options: CompactOptions,
    vault: &VaultPaths,
    source_identity: FileIdentity,
) -> Result<CommandResult> {
    let head = read_session_head(path)?;
    if head.provenance.is_spawned_thread() && !options.allow_spawned_threads {
        return Err(VaultError::SpawnedThreadRefused {
            path: path.to_path_buf(),
            thread_source: head.provenance.thread_source.clone(),
        });
    }
    // No override for this one. A spawned thread is merely *unvalidated*; shortening a page that
    // another continues from is *proven* to make Codex refuse the whole thread.
    let successors = lineage_successors(&head.session_id, &head.page_id);
    if !successors.is_empty() {
        return Err(VaultError::LineageSourceRefused {
            path: path.to_path_buf(),
            successors: successors.into_iter().map(|s| s.path).collect(),
        });
    }
    let session_id = head.session_id.clone();
    let journal = open_journal(vault, path, &session_id)?;
    let analysis = analyze_session_within(path, options.scan_window)?;

    if options.dry_run {
        let result_size = analysis
            .estimated_result_size_bytes
            .filter(|_| analysis.can_compact)
            .unwrap_or(analysis.original_size_bytes);
        return Ok(CommandResult {
            status: "preview".into(),
            session: path.to_string_lossy().into(),
            manifest: None,
            backup: None,
            reason: analysis.reasons.clone(),
            stats: crate::storage::preview(
                path,
                journal.manifest.as_ref(),
                &analysis.content_sha256,
                result_size,
                !analysis.can_compact || analysis.estimated_removed_bytes != Some(0),
            )?,
        });
    }

    if analysis.can_compact && analysis.estimated_removed_bytes == Some(0) {
        return Ok(CommandResult {
            status: "already_compact".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: journal
                .manifest
                .as_ref()
                .map(|_| manifest_path(vault, &journal.key)),
            backup: None,
            reason: vec![
                "nothing to compact: this transcript already contains only the required suffix"
                    .to_string(),
            ],
            stats: json!({"native_transcript_changed": false, "removed_bytes": 0}),
        });
    }

    if !analysis.can_compact {
        // Safety fallback from the MVP spec: archive the exact current transcript, but do not
        // remove a single native JSONL record when a bounded cutoff cannot be proven.
        let (archived, is_new) =
            archive_current_locked(path, &journal.key, vault, &analysis.content_sha256)?;
        let original = journal
            .manifest
            .as_ref()
            .map(|m| m.original.clone())
            .unwrap_or_else(|| archived.clone());
        let mut manifest = manifest_for(
            ManifestDraft {
                session_id: &session_id,
                head: &head,
                path,
                mode: Mode::ArchiveOnlyFallback,
                original: &original,
                restore: &archived,
                result_size: archived.source_size,
                result_sha256: &archived.source_sha256,
                notes: analysis.reasons.clone(),
            },
            journal.manifest.clone(),
        );
        manifest.status = Status::Ok;
        manifest.committed_at = Some(now_iso_utc());
        // The previous code returned an existing manifest untouched here, which left this
        // freshly verified capture unreachable and let `restore` rewind past it.
        manifest.record(
            now_iso_utc(),
            "archive-only-fallback",
            if is_new {
                "captured-current-state"
            } else {
                "current-state-already-captured"
            },
            Some(archived.clone()),
            Some("no bounded cutoff could be proven; transcript left unchanged".to_string()),
        );
        let manifest_file = write_manifest(&journal.key, vault, &manifest)?;
        let _ = write_summary(&journal.key, vault, &manifest);
        return Ok(CommandResult {
            status: "archived_only".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(archived.backup_path),
            reason: analysis.reasons.clone(),
            stats: json!({
                "analysis": analysis,
                "native_transcript_changed": false,
                "recovery_source_created": is_new,
            }),
        });
    }

    // The analysis pass establishes the expected source hash. Backup creation compresses that
    // state and then re-hashes the live source once more to close the append-after-EOF race.
    let backup = ensure_backup_for_compaction(
        path,
        &journal.key,
        vault,
        &analysis.content_sha256,
        journal.manifest.as_ref(),
    )?;
    let current_input_sha = backup.restore.source_sha256.clone();
    let current_input_size = backup.restore.source_size;
    if current_input_sha != analysis.content_sha256 {
        return Err(VaultError::SessionChanged {
            stage: "pre-compaction verification",
        });
    }

    let cutoff = analysis.cutoff_index.ok_or(VaultError::Internal {
        detail: "analysis reported can_compact without a cutoff index",
    })?;
    let session_meta = analysis.session_meta_index.ok_or(VaultError::Internal {
        detail: "analysis reported can_compact without a session_meta index",
    })?;

    let compact_tmp = TempFile::beside(path, "compact");
    let copy = copy_compacted_transcript(path, compact_tmp.path(), session_meta, cutoff)?;
    crate::util::test_abort("compact_temp_written");
    crate::util::test_io_fail("compact_temp_write")
        .map_err(|e| VaultError::io("writing compact temp", compact_tmp.path(), e))?;
    let (kept_lines, removed_lines, kept_bytes, removed_bytes) = (
        copy.kept_lines,
        copy.removed_lines,
        copy.kept_bytes,
        copy.removed_bytes,
    );
    // The copy re-read the source from end to end, so its hash *is* the concurrent-write check.
    if copy.source_sha256 != current_input_sha {
        return Err(VaultError::SessionChanged {
            stage: "compaction",
        });
    }
    if sha256_file(path)? != current_input_sha {
        return Err(VaultError::SessionChanged {
            stage: "compaction output verification",
        });
    }

    let (compact_ok, compact_issues) = verify_jsonl(compact_tmp.path())?;
    if !compact_ok {
        return Ok(CommandResult {
            status: "verification_failed".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: None,
            backup: Some(backup.restore.backup_path),
            reason: compact_issues,
            stats: json!({}),
        });
    }

    let result_sha_before_replace = copy.result_sha256.clone();
    let expected_result_size = fs::metadata(compact_tmp.path())
        .map_err(|e| VaultError::io("sizing the compacted transcript", compact_tmp.path(), e))?
        .len();
    if expected_result_size != copy.result_size {
        return Err(VaultError::mismatch(
            "compacted file size does not match what was written",
            copy.result_size,
            expected_result_size,
        ));
    }

    let reduction_this_operation = if current_input_size > 0 {
        (1.0 - expected_result_size as f64 / current_input_size as f64) * 100.0
    } else {
        0.0
    };
    let reduction_from_original = if backup.original.source_size > 0 {
        (1.0 - expected_result_size as f64 / backup.original.source_size as f64) * 100.0
    } else {
        0.0
    };

    let mut manifest = manifest_for(
        ManifestDraft {
            session_id: &session_id,
            head: &head,
            path,
            mode: Mode::CompactSafe,
            original: &backup.original,
            restore: &backup.restore,
            result_size: expected_result_size,
            result_sha256: &result_sha_before_replace,
            notes: analysis.reasons.clone(),
        },
        journal.manifest.clone(),
    );
    manifest.original = backup.original.clone();
    manifest.restore = backup.restore.clone();
    manifest.status = Status::Prepared;
    manifest.committed_at = None;
    manifest.compaction = Some(CompactionRecord {
        session_meta_index: session_meta,
        cutoff_index: cutoff,
        checkpoint_index: analysis.checkpoint_index,
        window_number: analysis.window_number,
        replacement_history_items: analysis.replacement_history_items_at_checkpoint,
        input_size: current_input_size,
        input_sha256: current_input_sha.clone(),
        kept_lines,
        removed_lines,
        kept_bytes,
        removed_bytes,
        reduction_this_operation_percent: reduction_this_operation,
        reduction_from_original_percent: reduction_from_original,
        compatibility_basis: COMPATIBILITY_BASIS.to_string(),
    });
    if backup.captured_new_snapshot {
        manifest.record(
            now_iso_utc(),
            "compact-safe",
            "captured-pre-compaction-snapshot",
            Some(backup.restore.clone()),
            Some("session had grown since the immutable original".to_string()),
        );
    }

    crate::util::test_pause("pre_replace");
    if path_file_identity(path)? != source_identity {
        return Err(VaultError::SessionChanged {
            stage: "pre-replacement source identity verification",
        });
    }
    if sha256_file(path)? != current_input_sha {
        return Err(VaultError::SessionChanged {
            stage: "pre-replacement verification",
        });
    }

    // Persist the recovery journal *before* the destructive rename. If the process dies after
    // this point, `restore` still knows the exact pre-compaction backup to materialize.
    let manifest_file = write_manifest(&journal.key, vault, &manifest)?;
    crate::util::test_abort("prepared_journal_written");

    let _replacement_lock = compact_tmp.replace_locked(path)?;
    crate::util::test_abort("atomic_replacement");
    let recovery_manifest = manifest_file.clone();
    (|| {
        let (active_ok, active_issues) = verify_jsonl(path)?;
        let active_sha = sha256_file(path)?;
        if !active_ok || active_sha != result_sha_before_replace {
            let mut failure_reasons = active_issues;
            if active_sha != result_sha_before_replace {
                failure_reasons.push(format!(
                "post-replace hash mismatch: expected {result_sha_before_replace}, got {active_sha}"
            ));
            }

            let restore_tmp = TempFile::beside(path, "restore-after-compact");
            decompress_file(&backup.restore.backup_path, restore_tmp.path())?;
            let restored_sha = sha256_file(restore_tmp.path())?;
            if restored_sha == backup.restore.source_sha256 {
                let _restore_lock = restore_tmp.replace_locked(path)?;
                let restored_active_sha = sha256_file(path)?;
                if restored_active_sha != backup.restore.source_sha256 {
                    return Err(VaultError::mismatch(
                        "automatic restore replaced the transcript but its hash is wrong",
                        &backup.restore.source_sha256,
                        &restored_active_sha,
                    ));
                }
                manifest.status = Status::RestoredAfterFailedVerification;
                manifest.last_restored_at = Some(now_iso_utc());
                manifest.last_restore_sha256 = Some(restored_active_sha.clone());
                manifest.result_size = backup.restore.source_size;
                manifest.result_sha256 = restored_active_sha.clone();
                manifest.record(
                    now_iso_utc(),
                    "compact-safe",
                    "restored-after-failed-verification",
                    None,
                    Some(failure_reasons.join("; ")),
                );
                write_manifest(&journal.key, vault, &manifest)?;
                return Ok(CommandResult {
                    status: "restored_after_failed_verification".to_string(),
                    session: path.to_string_lossy().to_string(),
                    manifest: Some(manifest_file),
                    backup: Some(backup.restore.backup_path),
                    reason: failure_reasons,
                    stats: json!({
                        "failed_active_sha256": active_sha,
                        "restored_sha256": restored_active_sha,
                    }),
                });
            }
            return Err(VaultError::mismatch(
                "post-replace verification failed and the automatic restore could not be verified",
                &backup.restore.source_sha256,
                &restored_sha,
            ));
        }

        let result_size = fs::metadata(path)
            .map_err(|e| VaultError::io("reading compacted size", path, e))?
            .len();
        if result_size != expected_result_size {
            return Err(VaultError::mismatch(
                "post-replace size",
                expected_result_size,
                result_size,
            ));
        }
        crate::util::test_abort("post_replacement_verified");

        manifest.status = Status::Ok;
        manifest.committed_at = Some(now_iso_utc());
        manifest.record(
            now_iso_utc(),
            "compact-safe",
            "committed",
            None,
            Some(format!(
                "{removed_lines} line(s), {} removed",
                format_size(removed_bytes)
            )),
        );
        let manifest_file = write_manifest(&journal.key, vault, &manifest)?;
        crate::util::test_abort("final_journal_commit");
        let _ = write_summary(&journal.key, vault, &manifest);

        Ok(CommandResult {
            status: "ok".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(backup.restore.backup_path),
            reason: manifest.notes.clone(),
            stats: json!({
                "kept_lines": kept_lines,
                "removed_lines": removed_lines,
                "removed_bytes": removed_bytes,
                "original_size": backup.original.source_size,
                "input_size": current_input_size,
                "result_size": result_size,
                "reduction_this_operation_percent": reduction_this_operation,
                "reduction_from_original_percent": reduction_from_original,
            }),
        })
    })()
    .map_err(|e: VaultError| e.after_replacement(&recovery_manifest))
}
