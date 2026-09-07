use super::transaction::ChainTxPage;
use crate::error::{Result, VaultError};
use crate::fsatomic::TempFile;
use crate::hashing::{decompress_file, sha256_file};
use crate::paths::{VaultKey, VaultPaths};
use std::path::{Path, PathBuf};

pub(super) fn backup_target(vault: &VaultPaths, path: &Path, txid: &str, suffix: &str) -> PathBuf {
    let key = VaultKey::for_rollout(path);
    vault
        .backups
        .join(format!("{key}.{suffix}-{txid}.jsonl.zst"))
}

pub(super) fn restore_pages_from_anchors(pages: &[ChainTxPage]) -> Result<()> {
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
