use super::planning::{build_plans, chain_paths, preview};
use super::recovery::{backup_target, restore_pages_from_anchors};
use super::transaction::{
    pending_transaction, write_transaction, ChainTransaction, ChainTxPage, CHAIN_TX_VERSION,
};
use crate::backup::create_verified_backup_of;
use crate::discovery::resolve_conversation_chain;
use crate::error::{Result, VaultError};
use crate::fsatomic::{
    copy_compacted_paginated_page, ensure_supported_mutation_filesystem, lock_session,
    MultiMutationGuard, PaginatedCompactionCopy, TempFile,
};
use crate::hashing::sha256_file;
use crate::ops::{commit_chain_page_manifest, prepare_chain_page_manifest, CompactOptions};
use crate::paths::ensure_vault_paths;
use crate::rollout::verify_jsonl;
use crate::storage::{process_peak_rss_bytes, vault_storage_breakdown};
use crate::util::{format_size, now_epoch_millis, now_iso_utc};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::time::Instant;

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
