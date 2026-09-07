use super::scope::project_key;
use crate::discovery::discover_sessions;
use crate::error::{Result, VaultError};
use crate::manifest::load_manifest;
use crate::paths::vault_paths;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
pub(super) struct Source {
    pub(super) path: PathBuf,
    pub(super) kind: &'static str,
    pub(super) expected_hash: Option<String>,
    pub(super) title: String,
}

pub(super) fn sources() -> Result<Vec<Source>> {
    let mut items = BTreeMap::new();
    for s in discover_sessions(None)? {
        items.insert(
            project_key(&s.path),
            Source {
                path: s.path,
                kind: "native",
                expected_hash: None,
                title: s.title.unwrap_or_default(),
            },
        );
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
                    let source = Source {
                        path: anchor.backup_path,
                        kind: "backup",
                        expected_hash: Some(anchor.backup_sha256),
                        title: String::new(),
                    };
                    let key = project_key(&source.path);
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
