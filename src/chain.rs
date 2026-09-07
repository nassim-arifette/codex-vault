//! Whole-conversation compaction for Codex paginated rollouts.
//!
//! The supported transformation is intentionally narrow: one complete linear pagination chain.
//! Every predecessor prefix consumed by its successor is compacted using the same bounded proof
//! as an ordinary rollout, the successor byte boundary is rewritten, and all pages are committed
//! under one durable transaction journal. Forks, missing dependencies and ambiguous layouts are
//! refused before any native byte changes.

use crate::analysis::{analyze_session_prefix_within, analyze_session_within, CompactionAnalysis};
use crate::backup::create_verified_backup_of;
use crate::discovery::{resolve_conversation_chain, ConversationChain};
use crate::error::{Result, VaultError};
use crate::fsatomic::{
    copy_compacted_paginated_page, ensure_supported_mutation_filesystem, lock_session,
    MultiMutationGuard, PaginatedCompactionCopy, TempFile,
};
use crate::hashing::{decompress_file, sha256_file};
use crate::manifest::RecoveryAnchor;
use crate::ops::{commit_chain_page_manifest, prepare_chain_page_manifest, CompactOptions};
use crate::paths::{create_private_directory, ensure_vault_paths, VaultKey, VaultPaths};
use crate::rollout::{ensure_plain_native_session, verify_jsonl};
use crate::storage::{
    compressed_size, process_peak_rss_bytes, vault_storage_breakdown, VaultStorageBreakdown,
};
use crate::util::{format_size, now_epoch_millis, now_iso_utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

const CHAIN_TX_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChainTxPage {
    path: PathBuf,
    page_id: String,
    before: RecoveryAnchor,
    after_size: u64,
    after_sha256: String,
    applied: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChainTransaction {
    version: u32,
    transaction_id: String,
    operation: String,
    session_id: String,
    created_at: String,
    committed_at: Option<String>,
    status: String,
    pages: Vec<ChainTxPage>,
}

struct PagePlan {
    path: PathBuf,
    page_id: String,
    consumed_prefix: u64,
    analysis: CompactionAnalysis,
    input_size: u64,
    input_sha256: String,
}

struct PreparedPage {
    temp: Option<TempFile>,
    copy: PaginatedCompactionCopy,
}

#[cfg(debug_assertions)]
fn maybe_abort_after_replace(replaced_pages: usize) {
    let requested = std::env::var("CODEX_VAULT_TEST_CHAIN_ABORT_AFTER_REPLACE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    if requested == Some(replaced_pages) {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
fn maybe_abort_after_replace(_replaced_pages: usize) {}

fn transactions_dir(vault: &VaultPaths) -> PathBuf {
    vault.root.join("transactions")
}

fn transaction_path(vault: &VaultPaths, tx: &ChainTransaction) -> PathBuf {
    transactions_dir(vault).join(format!(
        "chain-{}-{}.json",
        tx.session_id.replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
            "_"
        ),
        tx.transaction_id
    ))
}

fn write_transaction(vault: &VaultPaths, tx: &ChainTransaction) -> Result<PathBuf> {
    let dir = transactions_dir(vault);
    create_private_directory(&dir)?;
    let path = transaction_path(vault, tx);
    let temp = TempFile::beside(&path, "transaction");
    let mut file = crate::fsatomic::create_private_file(temp.path())
        .map_err(|e| VaultError::io("creating chain transaction", temp.path(), e))?;
    serde_json::to_writer_pretty(&mut file, tx)
        .map_err(|e| VaultError::json("writing chain transaction", temp.path(), e))?;
    file.write_all(b"\n")
        .map_err(|e| VaultError::io("writing chain transaction", temp.path(), e))?;
    file.sync_all()
        .map_err(|e| VaultError::io("flushing chain transaction", temp.path(), e))?;
    drop(file);
    temp.commit_onto(&path)?;
    Ok(path)
}

fn transactions_for(vault: &VaultPaths, session_id: &str) -> Result<Vec<ChainTransaction>> {
    let dir = transactions_dir(vault);
    let mut found = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = fs::read_to_string(&path)?;
            let tx: ChainTransaction = serde_json::from_str(&raw)
                .map_err(|e| VaultError::json("parsing chain transaction", &path, e))?;
            if tx.session_id == session_id {
                found.push(tx);
            }
        }
    }
    found.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(found)
}

fn pending_transaction(vault: &VaultPaths, session_id: &str) -> Result<Option<ChainTransaction>> {
    Ok(transactions_for(vault, session_id)?
        .into_iter()
        .find(|tx| tx.status == "prepared"))
}

fn chain_paths(chain: &ConversationChain) -> Vec<PathBuf> {
    chain.pages.iter().map(|p| p.path.clone()).collect()
}

fn build_plans(chain: &ConversationChain, options: CompactOptions) -> Result<Vec<PagePlan>> {
    if chain.pages.len() < 2 {
        return Err(VaultError::InvalidInput {
            reason: format!(
                "conversation `{}` has only one discovered rollout; use ordinary `compact`",
                chain.session_id
            ),
        });
    }
    let mut plans = Vec::with_capacity(chain.pages.len());
    for (index, page) in chain.pages.iter().enumerate() {
        ensure_plain_native_session(&page.path)?;
        let input_size = fs::metadata(&page.path)
            .map_err(|e| VaultError::io("reading chain page size", &page.path, e))?
            .len();
        let (consumed_prefix, analysis) = if let Some(successor) = chain.pages.get(index + 1) {
            let boundary =
                successor
                    .predecessor_end_byte_offset
                    .ok_or(VaultError::InvalidInput {
                        reason: format!("successor `{}` has no byte boundary", successor.page_id),
                    })?;
            if boundary > input_size {
                return Err(VaultError::InvalidInput {
                    reason: format!(
                        "page `{}` is {input_size} bytes but successor `{}` requires prefix {boundary}",
                        page.page_id, successor.page_id
                    ),
                });
            }
            (
                boundary,
                analyze_session_prefix_within(&page.path, boundary, options.scan_window)?,
            )
        } else {
            (
                input_size,
                analyze_session_within(&page.path, options.scan_window)?,
            )
        };
        if !analysis.can_compact {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "whole-conversation compaction refused: page `{}` has no proven compactable reconstruction prefix: {}",
                    page.page_id,
                    analysis.reasons.join("; ")
                ),
            });
        }
        let input_sha256 = sha256_file(&page.path)?;
        plans.push(PagePlan {
            path: page.path.clone(),
            page_id: page.page_id.clone(),
            consumed_prefix,
            analysis,
            input_size,
            input_sha256,
        });
    }
    Ok(plans)
}

fn preview(
    chain: &ConversationChain,
    plans: &[PagePlan],
    vault: &VaultPaths,
    started: Instant,
) -> Result<Value> {
    let native_before: u64 = plans.iter().map(|p| p.input_size).sum();
    let mut estimated_native_after = 0u64;
    let mut estimated_new_backups = 0u64;
    let mut rows = Vec::with_capacity(plans.len());
    for plan in plans {
        let prefix_after = plan
            .analysis
            .estimated_result_size_bytes
            .unwrap_or(plan.consumed_prefix);
        let tail = plan.input_size.saturating_sub(plan.consumed_prefix);
        let result = prefix_after.saturating_add(tail);
        estimated_native_after = estimated_native_after.saturating_add(result);
        let backup = compressed_size(&plan.path)?;
        estimated_new_backups = estimated_new_backups.saturating_add(backup);
        rows.push(json!({
            "page_id": plan.page_id,
            "path": plan.path,
            "input_bytes": plan.input_size,
            "consumed_prefix_bytes": plan.consumed_prefix,
            "estimated_result_bytes_before_history_base_rewrite": result,
            "estimated_removed_bytes": plan.input_size.saturating_sub(result),
            "estimated_new_backup_bytes": backup,
        }));
    }
    let vault = vault_storage_breakdown(vault).unwrap_or(VaultStorageBreakdown {
        total_bytes: 0,
        backup_bytes: 0,
        metadata_bytes: 0,
        index_bytes: 0,
    });
    let estimated_net =
        native_before as i128 - estimated_native_after as i128 - estimated_new_backups as i128;
    let runtime_ms = started.elapsed().as_millis();
    Ok(json!({
        "status": "preview",
        "session_id": chain.session_id,
        "page_count": chain.pages.len(),
        "layout": "linear_paginated_chain",
        "pages": rows,
        "storage": {
            "measurement": "logical_bytes",
            "native_before_bytes": native_before,
            "estimated_native_after_bytes": estimated_native_after,
            "estimated_new_backup_bytes": estimated_new_backups,
            "retained_backup_bytes_before": vault.backup_bytes,
            "recovery_metadata_bytes_before": vault.metadata_bytes,
            "optional_index_bytes_before": vault.index_bytes,
            "vault_total_bytes_before": vault.total_bytes,
            "estimated_net_saved_bytes_excluding_metadata_and_history_base_line_size_delta": estimated_net,
            "estimated_peak_temporary_disk_bytes": estimated_native_after,
            "note": "Dry-run reports persistent backup growth separately from temporary rewrite files and excludes the small serialized session_meta size delta plus transaction-journal scratch. Completed operation reports measured bytes."
        },
        "performance": {
            "runtime_ms": runtime_ms,
            "process_peak_ram_bytes": process_peak_rss_bytes(),
            "peak_ram_scope": "process_lifetime_high_water_mark"
        }
    }))
}

fn backup_target(vault: &VaultPaths, path: &Path, txid: &str, suffix: &str) -> PathBuf {
    let key = VaultKey::for_rollout(path);
    vault
        .backups
        .join(format!("{key}.{suffix}-{txid}.jsonl.zst"))
}

fn restore_pages_from_anchors(pages: &[ChainTxPage]) -> Result<()> {
    let mut temps = Vec::with_capacity(pages.len());
    for page in pages {
        let temp = TempFile::beside(&page.path, "chain-rollback");
        decompress_file(&page.before.backup_path, temp.path())?;
        let actual = sha256_file(temp.path())?;
        if actual != page.before.source_sha256 {
            return Err(VaultError::mismatch(
                "chain rollback backup content",
                &page.before.source_sha256,
                actual,
            ));
        }
        temps.push(Some(temp));
    }
    for (index, page) in pages.iter().enumerate() {
        temps[index]
            .take()
            .expect("prepared rollback temp")
            .replace_locked(&page.path)?;
        let actual = sha256_file(&page.path)?;
        if actual != page.before.source_sha256 {
            return Err(VaultError::mismatch(
                "chain rollback result",
                &page.before.source_sha256,
                actual,
            ));
        }
    }
    Ok(())
}

pub fn compact_conversation(
    reference: &str,
    cwd_filter: Option<&Path>,
    options: CompactOptions,
) -> Result<Value> {
    let started = Instant::now();
    let chain = resolve_conversation_chain(reference, cwd_filter)?;
    let vault = if options.dry_run {
        crate::paths::vault_paths()
    } else {
        ensure_vault_paths()?
    };
    let paths = chain_paths(&chain);
    if options.dry_run {
        let plans = build_plans(&chain, options)?;
        return preview(&chain, &plans, &vault, started);
    }

    if let Some(pending) = pending_transaction(&vault, &chain.session_id)? {
        return Err(VaultError::InvalidInput {
            reason: format!(
                "conversation `{}` has interrupted whole-conversation transaction `{}`; run `restore-conversation {}` before starting another compaction",
                chain.session_id, pending.transaction_id, chain.session_id
            ),
        });
    }

    for path in &paths {
        ensure_supported_mutation_filesystem(path)?;
    }
    let _operation = MultiMutationGuard::acquire(&vault.root, &paths)?;
    let mut source_locks = Vec::with_capacity(paths.len());
    for path in &paths {
        source_locks.push(lock_session(path)?);
    }
    // Resolve again after exclusion is established so a page created between discovery and lock
    // acquisition cannot be silently omitted.
    let locked_chain = resolve_conversation_chain(reference, cwd_filter)?;
    if chain_paths(&locked_chain) != paths {
        return Err(VaultError::SessionChanged {
            stage: "whole-conversation dependency discovery",
        });
    }
    let plans = build_plans(&locked_chain, options)?;
    let native_before: u64 = plans.iter().map(|p| p.input_size).sum();
    let vault_before = vault_storage_breakdown(&vault)?;

    let txid = format!("{}-{}", now_epoch_millis(), std::process::id());
    let mut tx = ChainTransaction {
        version: CHAIN_TX_VERSION,
        transaction_id: txid.clone(),
        operation: "compact-conversation".to_string(),
        session_id: locked_chain.session_id.clone(),
        created_at: now_iso_utc(),
        committed_at: None,
        status: "preparing".to_string(),
        pages: Vec::with_capacity(plans.len()),
    };

    // Capture every exact pre-operation page before producing any replacement.
    for plan in &plans {
        let target = backup_target(&vault, &plan.path, &txid, "prechain");
        let anchor = create_verified_backup_of(&plan.path, &target, Some(&plan.input_sha256))?;
        prepare_chain_page_manifest(&plan.path, &anchor)?;
        tx.pages.push(ChainTxPage {
            path: plan.path.clone(),
            page_id: plan.page_id.clone(),
            before: anchor,
            after_size: 0,
            after_sha256: String::new(),
            applied: false,
        });
    }

    let mut prepared = Vec::with_capacity(plans.len());
    let mut predecessor_new_boundary = None;
    for (index, plan) in plans.iter().enumerate() {
        let session_meta = plan
            .analysis
            .session_meta_index
            .ok_or(VaultError::Internal {
                detail: "chain analysis reported compactable without session_meta",
            })?;
        let cutoff = plan.analysis.cutoff_index.ok_or(VaultError::Internal {
            detail: "chain analysis reported compactable without cutoff",
        })?;
        let temp = TempFile::beside(&plan.path, "chain-compact");
        let copy = copy_compacted_paginated_page(
            &plan.path,
            temp.path(),
            plan.consumed_prefix,
            session_meta,
            cutoff,
            predecessor_new_boundary,
        )?;
        if copy.source_sha256 != plan.input_sha256 {
            return Err(VaultError::SessionChanged {
                stage: "whole-conversation replacement preparation",
            });
        }
        let (ok, issues) = verify_jsonl(temp.path())?;
        if !ok {
            return Err(VaultError::InvalidInput {
                reason: format!(
                    "generated chain page `{}` failed JSONL verification: {}",
                    plan.page_id,
                    issues.join("; ")
                ),
            });
        }
        tx.pages[index].after_size = copy.result_size;
        tx.pages[index].after_sha256 = copy.result_sha256.clone();
        predecessor_new_boundary = Some(copy.result_prefix_size);
        prepared.push(PreparedPage {
            temp: Some(temp),
            copy,
        });
    }

    // Rehash every source after all backups and replacements have been prepared. No source byte
    // may move between planning and the first native replacement.
    for plan in &plans {
        if sha256_file(&plan.path)? != plan.input_sha256 {
            return Err(VaultError::SessionChanged {
                stage: "whole-conversation pre-commit verification",
            });
        }
    }

    tx.status = "prepared".to_string();
    let tx_path = write_transaction(&vault, &tx)?;
    let commit_result: Result<()> = (|| {
        let mut replacement_locks = Vec::with_capacity(plans.len());
        for index in 0..plans.len() {
            let lock = prepared[index]
                .temp
                .take()
                .expect("prepared chain page")
                .replace_locked(&plans[index].path)?;
            replacement_locks.push(lock);
            tx.pages[index].applied = true;
            write_transaction(&vault, &tx)?;
            maybe_abort_after_replace(index + 1);
        }
        for (index, page) in tx.pages.iter().enumerate() {
            let actual = sha256_file(&page.path)?;
            let size = fs::metadata(&page.path)?.len();
            if actual != page.after_sha256 || size != page.after_size {
                return Err(VaultError::mismatch(
                    "whole-conversation committed page",
                    format!("{} bytes / {}", page.after_size, page.after_sha256),
                    format!("{size} bytes / {actual}"),
                ));
            }
            commit_chain_page_manifest(
                &page.path,
                page.after_size,
                &page.after_sha256,
                prepared[index].copy.removed_bytes,
            )?;
        }
        drop(replacement_locks);
        Ok(())
    })();

    if let Err(error) = commit_result {
        let rollback = restore_pages_from_anchors(&tx.pages);
        tx.status = if rollback.is_ok() {
            "rolled_back".to_string()
        } else {
            "rollback_failed".to_string()
        };
        let _ = write_transaction(&vault, &tx);
        if rollback.is_ok() {
            for page in &tx.pages {
                let _ = commit_chain_page_manifest(
                    &page.path,
                    page.before.source_size,
                    &page.before.source_sha256,
                    0,
                );
            }
            return Err(error);
        }
        return Err(error.after_replacement(&tx_path));
    }

    tx.status = "ok".to_string();
    tx.committed_at = Some(now_iso_utc());
    write_transaction(&vault, &tx)?;
    drop(source_locks);

    let native_after: u64 = tx.pages.iter().map(|p| p.after_size).sum();
    let vault_after = vault_storage_breakdown(&vault)?;
    let peak_temporary_disk_bytes: u64 = prepared
        .iter()
        .map(|prepared| prepared.copy.result_size)
        .sum();
    let runtime_ms = started.elapsed().as_millis();
    Ok(json!({
        "status": "ok",
        "session_id": tx.session_id,
        "transaction": tx_path,
        "page_count": tx.pages.len(),
        "pages": tx.pages.iter().zip(prepared.iter()).map(|(page, prepared)| json!({
            "page_id": page.page_id,
            "path": page.path,
            "input_bytes": page.before.source_size,
            "result_bytes": page.after_size,
            "removed_bytes": prepared.copy.removed_bytes,
            "rewritten_successor_boundary_bytes": prepared.copy.result_prefix_size,
        })).collect::<Vec<_>>(),
        "storage": {
            "measurement": "logical_bytes",
            "native_before_bytes": native_before,
            "native_after_bytes": native_after,
            "native_saved_bytes": native_before.saturating_sub(native_after),
            "backup_bytes_before": vault_before.backup_bytes,
            "backup_bytes_after": vault_after.backup_bytes,
            "new_backup_bytes": vault_after.backup_bytes.saturating_sub(vault_before.backup_bytes),
            "recovery_metadata_bytes_before": vault_before.metadata_bytes,
            "recovery_metadata_bytes_after": vault_after.metadata_bytes,
            "index_bytes_before": vault_before.index_bytes,
            "index_bytes_after": vault_after.index_bytes,
            "vault_total_bytes_before": vault_before.total_bytes,
            "vault_total_bytes_after": vault_after.total_bytes,
            "new_vault_bytes": vault_after.total_bytes.saturating_sub(vault_before.total_bytes),
            "peak_temporary_disk_bytes": peak_temporary_disk_bytes,
            "net_saved_bytes": (native_before as i128 + vault_before.total_bytes as i128) - (native_after as i128 + vault_after.total_bytes as i128),
            "native_saved_human": format_size(native_before.saturating_sub(native_after)),
        },
        "performance": {
            "runtime_ms": runtime_ms,
            "process_peak_ram_bytes": process_peak_rss_bytes(),
            "peak_ram_scope": "process_lifetime_high_water_mark"
        }
    }))
}

fn load_latest_transaction(vault: &VaultPaths, session_id: &str) -> Result<ChainTransaction> {
    let found = transactions_for(vault, session_id)?;
    // If recovery itself is interrupted, both the original operation and the recovery attempt can
    // be left `prepared`. Always drive toward the oldest still-pending transaction's complete
    // pre-operation state instead of accepting a newer partially restored intermediate.
    if let Some(pending) = found.iter().find(|tx| tx.status == "prepared") {
        return Ok(pending.clone());
    }
    found
        .into_iter()
        .rev()
        .find(|tx| tx.status == "ok")
        .ok_or(VaultError::InvalidInput {
            reason: format!(
                "no recoverable whole-conversation transaction recorded for `{session_id}`"
            ),
        })
}

/// Restore every page from the exact state captured by the latest successful chain operation.
/// The current complete chain is itself snapshotted first so the restore remains reversible.
pub fn restore_conversation(reference: &str, cwd_filter: Option<&Path>) -> Result<Value> {
    let chain = resolve_conversation_chain(reference, cwd_filter)?;
    let vault = ensure_vault_paths()?;
    let mut target = load_latest_transaction(&vault, &chain.session_id)?;
    let recovering_interrupted = target.status == "prepared";
    let paths = chain_paths(&chain);
    let _operation = MultiMutationGuard::acquire(&vault.root, &paths)?;
    let mut locks = Vec::new();
    for path in &paths {
        ensure_supported_mutation_filesystem(path)?;
        locks.push(lock_session(path)?);
    }
    if target.pages.len() != paths.len()
        || !target
            .pages
            .iter()
            .zip(paths.iter())
            .all(|(a, b)| a.path == *b)
    {
        return Err(VaultError::SessionChanged {
            stage: "whole-conversation restore dependency validation",
        });
    }

    let txid = format!("{}-{}", now_epoch_millis(), std::process::id());
    let mut undo = ChainTransaction {
        version: CHAIN_TX_VERSION,
        transaction_id: txid.clone(),
        operation: "restore-conversation".to_string(),
        session_id: chain.session_id.clone(),
        created_at: now_iso_utc(),
        committed_at: None,
        status: "preparing".to_string(),
        pages: Vec::new(),
    };
    let mut restore_temps = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let current_sha = sha256_file(path)?;
        let current = create_verified_backup_of(
            path,
            &backup_target(&vault, path, &txid, "prerestore-chain"),
            Some(&current_sha),
        )?;
        prepare_chain_page_manifest(path, &current)?;
        let wanted = &target.pages[index].before;
        let temp = TempFile::beside(path, "chain-restore");
        decompress_file(&wanted.backup_path, temp.path())?;
        if sha256_file(temp.path())? != wanted.source_sha256 {
            return Err(VaultError::mismatch(
                "whole-conversation restore backup",
                &wanted.source_sha256,
                sha256_file(temp.path())?,
            ));
        }
        undo.pages.push(ChainTxPage {
            path: path.clone(),
            page_id: chain.pages[index].page_id.clone(),
            before: current,
            after_size: wanted.source_size,
            after_sha256: wanted.source_sha256.clone(),
            applied: false,
        });
        restore_temps.push(Some(temp));
    }
    undo.status = "prepared".to_string();
    let tx_path = write_transaction(&vault, &undo)?;
    let result: Result<()> = (|| {
        let mut replacement_locks = Vec::with_capacity(paths.len());
        for index in 0..paths.len() {
            let lock = restore_temps[index]
                .take()
                .expect("prepared chain restore")
                .replace_locked(&paths[index])?;
            replacement_locks.push(lock);
            undo.pages[index].applied = true;
            write_transaction(&vault, &undo)?;
        }
        for page in &undo.pages {
            let actual = sha256_file(&page.path)?;
            if actual != page.after_sha256 {
                return Err(VaultError::mismatch(
                    "whole-conversation restored page",
                    &page.after_sha256,
                    actual,
                ));
            }
            commit_chain_page_manifest(&page.path, page.after_size, &page.after_sha256, 0)?;
        }
        drop(replacement_locks);
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = restore_pages_from_anchors(&undo.pages);
        undo.status = if rollback.is_ok() {
            "rolled_back"
        } else {
            "rollback_failed"
        }
        .to_string();
        let _ = write_transaction(&vault, &undo);
        return if rollback.is_ok() {
            Err(error)
        } else {
            Err(error.after_replacement(&tx_path))
        };
    }
    undo.status = "ok".to_string();
    undo.committed_at = Some(now_iso_utc());
    write_transaction(&vault, &undo)?;
    if recovering_interrupted {
        target.status = "recovered".to_string();
        target.committed_at = Some(now_iso_utc());
        write_transaction(&vault, &target)?;
    }
    drop(locks);
    Ok(json!({
        "status": "ok",
        "session_id": chain.session_id,
        "transaction": tx_path,
        "restored_from_transaction": target.transaction_id,
        "recovered_interrupted_transaction": recovering_interrupted,
        "page_count": paths.len(),
        "exact_complete_state_restored": true,
    }))
}
