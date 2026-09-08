use crate::error::{Result, VaultError};
use crate::fsatomic::TempFile;
use crate::manifest::RecoveryAnchor;
use crate::paths::{create_private_directory, VaultPaths};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

pub(super) const CHAIN_TX_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ChainTxPage {
    pub(super) path: PathBuf,
    pub(super) page_id: String,
    pub(super) before: RecoveryAnchor,
    pub(super) after_size: u64,
    pub(super) after_sha256: String,
    pub(super) applied: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ChainTransaction {
    pub(super) version: u32,
    pub(super) transaction_id: String,
    pub(super) operation: String,
    pub(super) session_id: String,
    pub(super) created_at: String,
    pub(super) committed_at: Option<String>,
    pub(super) status: String,
    pub(super) pages: Vec<ChainTxPage>,
}

fn transactions_dir(vault: &VaultPaths) -> PathBuf {
    vault.root.join("transactions")
}

pub(super) fn transaction_path(vault: &VaultPaths, tx: &ChainTransaction) -> PathBuf {
    transactions_dir(vault).join(format!(
        "chain-{}-{}.json",
        tx.session_id.replace(
            |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
            "_"
        ),
        tx.transaction_id
    ))
}

pub(super) fn write_transaction(vault: &VaultPaths, tx: &ChainTransaction) -> Result<PathBuf> {
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

pub(super) fn pending_transaction(
    vault: &VaultPaths,
    session_id: &str,
) -> Result<Option<ChainTransaction>> {
    Ok(transactions_for(vault, session_id)?
        .into_iter()
        .find(|tx| tx.status == "prepared"))
}

pub(super) fn load_latest_transaction(
    vault: &VaultPaths,
    session_id: &str,
) -> Result<ChainTransaction> {
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
