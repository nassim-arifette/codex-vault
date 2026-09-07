use super::planning::chain_paths;
use super::recovery::{backup_target, restore_pages_from_anchors};
use super::transaction::{
    load_latest_transaction, write_transaction, ChainTransaction, ChainTxPage, CHAIN_TX_VERSION,
};
use crate::backup::create_verified_backup_of;
use crate::discovery::resolve_conversation_chain;
use crate::error::{Result, VaultError};
use crate::fsatomic::{
    ensure_supported_mutation_filesystem, lock_session, MultiMutationGuard, TempFile,
};
use crate::hashing::{decompress_file, sha256_file};
use crate::ops::{commit_chain_page_manifest, prepare_chain_page_manifest};
use crate::paths::ensure_vault_paths;
use crate::util::{now_epoch_millis, now_iso_utc};
use serde_json::{json, Value};
use std::path::Path;

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
