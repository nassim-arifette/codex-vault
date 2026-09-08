use crate::backup::comparable_path;
use crate::discovery::{LineageIndex, LineageSuccessor};
use crate::fsatomic::TEMP_SUFFIX;
use crate::manifest::{load_manifest, Manifest};
use crate::paths::{VaultKey, VaultPaths};
use crate::rollout::rollout_stem;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Ephemeral catalog metadata shared by every row of one doctor/prune command.
///
/// The safety-sensitive backup classification remains a complete union of recovery journals; the
/// difference is that the union and directory listings are built once per command instead of once
/// per target. This is deliberately not persisted across commands, so it cannot become a stale
/// source of truth for deletion decisions.
pub(crate) struct CatalogContext {
    recovery: RecoveryAudit,
    backup_temps: BTreeMap<String, PathBuf>,
    manifest_temps: BTreeMap<String, PathBuf>,
    native_temps: HashMap<PathBuf, BTreeMap<String, PathBuf>>,
    lineage: Option<LineageIndex>,
}

#[derive(Default)]
struct RecoveryAudit {
    referenced: HashSet<String>,
    backups: BTreeMap<String, PathBuf>,
    error: Option<String>,
}

impl CatalogContext {
    pub(crate) fn build(
        vault: &VaultPaths,
        targets: &[PathBuf],
        audit_backups: bool,
        include_lineage: bool,
    ) -> Self {
        let mut recovery = RecoveryAudit::default();
        let mut backup_temps = BTreeMap::new();
        let mut manifest_temps = BTreeMap::new();
        let mut manifest_paths = Vec::new();

        if let Ok(entries) = fs::read_dir(&vault.manifests) {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        if audit_backups && recovery.error.is_none() {
                            recovery.error =
                                Some(format!("reading recovery journal directory: {err}"));
                        }
                        continue;
                    }
                };
                let path = entry.path();
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    insert_temp(&mut manifest_temps, &path);
                }
                if audit_backups && path.extension().and_then(|s| s.to_str()) == Some("json") {
                    manifest_paths.push(path);
                }
            }
        } else if audit_backups {
            recovery.error = Some("cannot read recovery journal directory".to_string());
        }

        if audit_backups {
            for path in manifest_paths {
                match load_manifest(&path) {
                    Ok(Some(manifest)) => {
                        for anchor in manifest.anchors() {
                            recovery
                                .referenced
                                .insert(comparable_path(&anchor.backup_path));
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        if recovery.error.is_none() {
                            recovery.error = Some(err.to_string());
                        }
                    }
                }
            }
        }

        if let Ok(entries) = fs::read_dir(&vault.backups) {
            for entry in entries.filter_map(std::result::Result::ok) {
                let path = entry.path();
                if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    continue;
                }
                insert_temp(&mut backup_temps, &path);
                if audit_backups
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".jsonl.zst"))
                {
                    if let Some(name) = file_name(&path) {
                        recovery.backups.insert(name, path);
                    }
                }
            }
        }

        let mut native_temps = HashMap::new();
        for parent in targets.iter().filter_map(|path| path.parent()) {
            if native_temps.contains_key(parent) {
                continue;
            }
            let mut files = BTreeMap::new();
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.filter_map(std::result::Result::ok) {
                    let path = entry.path();
                    if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                        insert_temp(&mut files, &path);
                    }
                }
            }
            native_temps.insert(parent.to_path_buf(), files);
        }

        CatalogContext {
            recovery,
            backup_temps,
            manifest_temps,
            native_temps,
            lineage: include_lineage.then(LineageIndex::discover),
        }
    }

    pub(super) fn unreferenced_backups(
        &self,
        keys: &[VaultKey],
        manifest: Option<&Manifest>,
    ) -> std::result::Result<Vec<PathBuf>, &str> {
        if let Some(error) = self.recovery.error.as_deref() {
            return Err(error);
        }
        let local_references: HashSet<String> = manifest
            .map(|manifest| {
                manifest
                    .anchors()
                    .into_iter()
                    .map(|anchor| comparable_path(&anchor.backup_path))
                    .collect()
            })
            .unwrap_or_default();
        let mut found = Vec::new();
        for key in keys {
            let prefix = format!("{key}.");
            for path in prefixed_values(&self.recovery.backups, &prefix) {
                let identity = comparable_path(path);
                if !self.recovery.referenced.contains(&identity)
                    && !local_references.contains(&identity)
                {
                    found.push(path.clone());
                }
            }
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    pub(super) fn stale_temp_files(&self, path: &Path, keys: &[VaultKey]) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for key in keys {
            found.extend(
                prefixed_values(&self.backup_temps, key.as_str())
                    .into_iter()
                    .cloned(),
            );
            found.extend(
                prefixed_values(&self.manifest_temps, key.as_str())
                    .into_iter()
                    .cloned(),
            );
        }
        if let Some(parent) = path.parent() {
            if let Some(files) = self.native_temps.get(parent) {
                found.extend(
                    prefixed_values(files, &rollout_stem(path))
                        .into_iter()
                        .cloned(),
                );
            }
        }
        found.sort();
        found.dedup();
        found
    }

    pub(super) fn lineage_successors(
        &self,
        thread_id: &str,
        page_id: &str,
    ) -> Option<&[LineageSuccessor]> {
        self.lineage
            .as_ref()
            .map(|lineage| lineage.successors(thread_id, page_id))
    }
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

fn insert_temp(files: &mut BTreeMap<String, PathBuf>, path: &Path) {
    let Some(name) = file_name(path) else {
        return;
    };
    if name.ends_with(TEMP_SUFFIX) {
        files.insert(name, path.to_path_buf());
    }
}

fn prefixed_values<'a>(files: &'a BTreeMap<String, PathBuf>, prefix: &str) -> Vec<&'a PathBuf> {
    files
        .range(prefix.to_string()..)
        .take_while(|(name, _)| name.starts_with(prefix))
        .map(|(_, path)| path)
        .collect()
}
