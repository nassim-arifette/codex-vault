use super::shared::open_journal;
use crate::backup::unreferenced_backups;
use crate::error::Result;
use crate::fsatomic::{stale_temp_files, MutationGuard};
use crate::paths::{ensure_vault_paths, VaultKey};
use crate::rollout::{read_session_head, rollout_stem};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub fn prune_one(path: &Path, include_backups: bool, apply: bool) -> Result<Value> {
    let vault = ensure_vault_paths()?;
    let _operation = MutationGuard::acquire(&vault.root, path)?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();

    let journal = open_journal(&vault, path, &session_id);
    let keys = match &journal {
        Ok(j) => j.keys(),
        Err(_) => vec![
            VaultKey::for_rollout(path),
            VaultKey::legacy_thread_id(&session_id),
        ],
    };

    let mut targets = Vec::new();
    for key in &keys {
        targets.extend(stale_temp_files(&vault.backups, key.as_str()));
        targets.extend(stale_temp_files(&vault.manifests, key.as_str()));
    }
    if let Some(dir) = path.parent() {
        targets.extend(stale_temp_files(dir, &rollout_stem(path)));
    }
    targets.sort();
    targets.dedup();
    let temp_count = targets.len();

    let mut backups = Vec::new();
    let mut manifest_note = None;
    if include_backups {
        match journal.map(|j| j.manifest) {
            Ok(Some(m)) => match unreferenced_backups(&vault, &keys, Some(&m)) {
                Ok(paths) => backups = paths,
                Err(err) => {
                    manifest_note = Some(format!(
                        "cannot read every recovery journal ({err}); refusing to delete backups"
                    ))
                }
            },
            Ok(None) => {
                manifest_note = Some(
                    "no manifest for this session; refusing to judge any backup unreferenced"
                        .to_string(),
                )
            }
            Err(err) => {
                manifest_note = Some(format!(
                    "manifest unreadable ({err}); refusing to judge any backup unreferenced"
                ))
            }
        }
    }
    targets.extend(backups.iter().cloned());

    let mut removed = Vec::new();
    let mut failed = Vec::new();
    if apply {
        for t in &targets {
            match fs::remove_file(t) {
                Ok(()) => removed.push(t.clone()),
                Err(err) => failed.push(json!({
                    "path": t.to_string_lossy(),
                    "error": err.to_string(),
                })),
            }
        }
    }

    Ok(json!({
        "session_id": session_id,
        "vault_key": keys.first(),
        "session": path.to_string_lossy(),
        "stale_temp_files": temp_count,
        "unreferenced_backups": backups.len(),
        "candidates": targets.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        "removed": removed.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        "failed": failed,
        "note": manifest_note,
    }))
}
