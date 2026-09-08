use super::scope::project_key;
use crate::error::{Result, VaultError};
use crate::manifest::load_manifest;
use crate::paths::{codex_root, vault_paths};
use crate::rollout::{is_codex_zstd_jsonl, is_plain_jsonl};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Clone)]
pub(super) struct Source {
    pub(super) path: PathBuf,
    /// Canonical project key for `path`, computed exactly once during discovery. Building an index
    /// used to normalize/canonicalize the same path again for source identity and again while
    /// processing it; that becomes very expensive in catalogs containing tens of thousands of
    /// small files on Windows.
    pub(super) key: String,
    pub(super) kind: &'static str,
    pub(super) expected_hash: Option<String>,
}

pub(super) fn sources() -> Result<Vec<Source>> {
    let mut items = BTreeMap::new();
    // Index discovery only needs paths. `discover_sessions` parses every rollout head in order to
    // populate CLI metadata, which doubles catalog I/O before indexing even starts. Defer parsing
    // until build actually needs a source's head, and let unchanged refreshes avoid it entirely.
    for source_dir in ["sessions", "archived_sessions"] {
        let base = codex_root().join(source_dir);
        if !base.exists() {
            continue;
        }
        for entry in WalkDir::new(base)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if !entry.file_type().is_file() || (!is_plain_jsonl(path) && !is_codex_zstd_jsonl(path))
            {
                continue;
            }
            let path = path.to_path_buf();
            let key = project_key(&path);
            items.insert(
                key.clone(),
                Source {
                    path,
                    key,
                    kind: "native",
                    expected_hash: None,
                },
            );
        }
    }
    let manifests = vault_paths().manifests;
    if manifests.is_dir() {
        for entry in fs::read_dir(manifests)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            if let Some(m) = load_manifest(&path)? {
                for anchor in m.anchors() {
                    if !anchor.backup_path.is_file() {
                        return Err(VaultError::BackupMissing {
                            path: anchor.backup_path,
                        });
                    }
                    let key = project_key(&anchor.backup_path);
                    let source = Source {
                        path: anchor.backup_path,
                        key: key.clone(),
                        kind: "backup",
                        expected_hash: Some(anchor.backup_sha256),
                    };
                    if let Some(previous) = items.get(&key) {
                        if previous.expected_hash.is_some()
                            && previous.expected_hash != source.expected_hash
                        {
                            return Err(VaultError::Index {
                                reason: "journals disagree about an archive hash".into(),
                            });
                        }
                    }
                    items.insert(key, source);
                }
            }
        }
    }
    Ok(items.into_values().collect())
}
