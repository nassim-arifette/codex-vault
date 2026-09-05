//! The four session operations: archive, compact-safe, restore and doctor.

use crate::analysis::analyze_session_within;
use crate::backup::{
    archive_current_locked, create_verified_backup, create_verified_backup_of,
    ensure_backup_for_compaction, paths_equal, unreferenced_backups,
};
use crate::discovery::lineage_successors;
use crate::error::{Result, VaultError};
use crate::fsatomic::{
    copy_compacted_transcript, lock_session, stale_temp_files, MutationGuard, TempFile,
};
use crate::hashing::{decompress_file, sha256_file, sha256_rollout_prefix};
use crate::manifest::{
    load_manifest, write_manifest, write_summary, CodexVersionSource, CompactionRecord, Manifest,
    Mode, RecoveryAnchor, Status, MANIFEST_VERSION, SCHEMA_ADAPTER,
};
use crate::paths::{
    backup_path, detect_codex_version, ensure_vault_paths, manifest_path, prerestore_backup_path,
    snapshot_backup_path, VaultKey, VaultPaths,
};
use crate::rollout::{
    ensure_plain_native_session, read_session_head, rollout_stem, verify_jsonl, SessionHead,
    DEFAULT_SCAN_WINDOW,
};
use crate::util::{format_size, now_iso_utc};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const COMPATIBILITY_BASIS: &str = "Codex bounded model-context scan: valid compaction + completed \
     turn context; invalid compactions and rollback force full replay";

#[derive(Debug, Serialize)]
pub struct CommandResult {
    pub status: String,
    pub session: String,
    pub manifest: Option<PathBuf>,
    pub backup: Option<PathBuf>,
    pub reason: Vec<String>,
    pub stats: Value,
}

#[derive(Debug, Serialize)]
pub struct DoctorCheck {
    pub session: String,
    pub session_path: PathBuf,
    pub status: String,
    pub notes: Vec<String>,
    pub backup_exists: bool,
    pub backup_ok: bool,
    pub session_ok: bool,
    pub manifest_exists: bool,
    pub manifest_ok: bool,
    /// Backups on disk that no manifest anchor points at. Should always be empty.
    pub unreferenced_backups: Vec<PathBuf>,
    /// Scratch files left by a process that died mid-operation.
    pub stale_temp_files: Vec<PathBuf>,
    /// Whether archives were decompressed and the transcript re-parsed.
    pub deep: bool,
    /// A later page of this thread points past the end of this rollout, so `codex resume` fails.
    pub lineage_broken: bool,
}

/// Which recorded state a `restore` should put back.
#[derive(Clone, Debug, Default)]
pub enum RestoreTarget {
    /// The newest verified capture — what the journal's `restore` anchor points at.
    #[default]
    Latest,
    /// The first immutable full backup.
    Original,
    /// A specific backup file, which must be one of the manifest's anchors.
    Backup(PathBuf),
}

/// Resolve the Codex build a manifest should pin, best source first.
///
/// The transcript wins: `session_meta.cli_version` names the build that actually wrote this
/// file, while the installed CLI only describes this machine today and may be many versions
/// newer than an old rollout.
fn resolve_codex_version(head: &SessionHead) -> (Option<String>, CodexVersionSource) {
    if let Some(v) = head.provenance.cli_version.clone() {
        return (Some(v), CodexVersionSource::SessionMeta);
    }
    if let Ok(v) = std::env::var("CODEX_VAULT_CODEX_VERSION") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return (Some(v), CodexVersionSource::Environment);
        }
    }
    match detect_codex_version() {
        Some(v) => (Some(v), CodexVersionSource::InstalledCli),
        None => (None, CodexVersionSource::Unknown),
    }
}

/// An unpinnable or second-hand version is a compatibility risk worth stating, because the
/// transcript format is not a stable public API.
fn codex_version_note(source: CodexVersionSource) -> Option<String> {
    match source {
        CodexVersionSource::Unknown => Some(
            "no Codex version available: this transcript's session_meta carries no \
             cli_version and no installed `codex` could be resolved, so this manifest cannot \
             pin the transcript layout to a build"
                .to_string(),
        ),
        CodexVersionSource::InstalledCli => Some(
            "Codex version was taken from the installed CLI, not from the transcript; it \
             describes this machine today rather than the build that wrote this rollout"
                .to_string(),
        ),
        CodexVersionSource::SessionMeta | CodexVersionSource::Environment => None,
    }
}

/// Copy the transcript's own provenance onto a manifest.
fn apply_provenance(manifest: &mut Manifest, head: &SessionHead) {
    let (version, source) = resolve_codex_version(head);
    if let Some(note) = codex_version_note(source) {
        manifest.notes.push(note);
    }
    manifest.codex_version_detected = version.is_some();
    manifest.codex_version = version;
    manifest.codex_version_source = source;
    manifest.originator = head.provenance.originator.clone();
    manifest.client_source = head.provenance.source.clone();
    manifest.history_mode = head.provenance.history_mode.clone();
    manifest.context_window_id = head.provenance.context_window_id.clone();
}

/// The state an operation just produced, described once instead of threaded through a long
/// parameter list.
struct ManifestDraft<'a> {
    session_id: &'a str,
    head: &'a SessionHead,
    path: &'a Path,
    mode: Mode,
    /// The first immutable full backup, used only when no journal exists yet.
    original: &'a RecoveryAnchor,
    /// The anchor a fresh journal should point `restore` at.
    restore: &'a RecoveryAnchor,
    result_size: u64,
    result_sha256: &'a str,
    notes: Vec<String>,
}

fn new_manifest(draft: ManifestDraft<'_>, status: Status) -> Manifest {
    let ManifestDraft {
        session_id,
        head,
        path,
        mode,
        original,
        restore,
        result_size,
        result_sha256,
        notes,
    } = draft;
    let mut manifest = Manifest {
        manifest_version: MANIFEST_VERSION,
        created_at: now_iso_utc(),
        committed_at: None,
        session_id: session_id.to_string(),
        session_path: path.to_string_lossy().to_string(),
        mode,
        status,
        schema_adapter: SCHEMA_ADAPTER.to_string(),
        codex_version: None,
        codex_version_detected: false,
        codex_version_source: CodexVersionSource::Unknown,
        originator: None,
        client_source: None,
        history_mode: None,
        context_window_id: None,
        original: original.clone(),
        restore: restore.clone(),
        result_size,
        result_sha256: result_sha256.to_string(),
        compaction: None,
        history: Vec::new(),
        notes,
        last_restored_at: None,
        last_restore_sha256: None,
    };
    apply_provenance(&mut manifest, head);
    manifest
}

/// The vault entry for one rollout file: its key, and the journal already stored under it.
pub struct Journal {
    pub key: VaultKey,
    /// The pre-P0 key, kept so an existing vault stays readable and auditable.
    pub legacy_key: VaultKey,
    pub manifest: Option<Manifest>,
}

impl Journal {
    /// Every key this rollout's files could be stored under.
    pub fn keys(&self) -> Vec<VaultKey> {
        if self.key == self.legacy_key {
            vec![self.key.clone()]
        } else {
            vec![self.key.clone(), self.legacy_key.clone()]
        }
    }
}

/// Open the vault entry for a rollout, migrating from the old thread-id key when it applies.
///
/// A manifest stored under the legacy key is adopted only if it names *this* file. A Codex thread
/// spans several rollout files, so a legacy manifest may well belong to a sibling; adopting it
/// blindly is exactly how one session's "immutable original" came to describe another's content.
fn open_journal(vault: &VaultPaths, path: &Path, session_id: &str) -> Result<Journal> {
    let key = VaultKey::for_rollout(path);
    let legacy_key = VaultKey::legacy_thread_id(session_id);

    if let Some(m) = load_manifest(&manifest_path(vault, &key))? {
        return Ok(Journal {
            key,
            legacy_key,
            manifest: Some(m),
        });
    }
    if key != legacy_key {
        if let Some(m) = load_manifest(&manifest_path(vault, &legacy_key))? {
            if paths_equal(Path::new(&m.session_path), path) {
                return Ok(Journal {
                    key,
                    legacy_key,
                    manifest: Some(m),
                });
            }
        }
    }
    Ok(Journal {
        key,
        legacy_key,
        manifest: None,
    })
}

/// Carry forward an existing journal, or build a fresh one describing the state we just captured.
///
/// Reusing the existing manifest is what keeps a session's history in one document; an earlier
/// version returned the stale file untouched, which silently orphaned every backup it did not
/// know about.
fn manifest_for(draft: ManifestDraft<'_>, existing: Option<Manifest>) -> Manifest {
    match existing {
        Some(mut m) => {
            m.mode = draft.mode;
            m.session_path = draft.path.to_string_lossy().to_string();
            m.result_size = draft.result_size;
            m.result_sha256 = draft.result_sha256.to_string();
            m.notes = draft.notes;
            apply_provenance(&mut m, draft.head);
            m
        }
        None => new_manifest(draft, Status::Ok),
    }
}

pub fn archive_impl(path: &Path, force: bool) -> Result<CommandResult> {
    ensure_plain_native_session(path)?;
    let vault = ensure_vault_paths()?;
    let _operation = MutationGuard::acquire(&vault.root, path)?;
    let _lock = lock_session(path)?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let immutable_backup = backup_path(&vault, &journal.key);

    if immutable_backup.exists() && !force {
        return Ok(CommandResult {
            status: "exists".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_path(&vault, &journal.key)),
            backup: Some(immutable_backup),
            reason: vec!["immutable original backup already exists".to_string()],
            stats: json!({}),
        });
    }

    let is_first = !immutable_backup.exists();
    let target = if is_first {
        immutable_backup.clone()
    } else {
        snapshot_backup_path(&vault, &journal.key)
    };
    let anchor = create_verified_backup(path, &target)?;
    let size = anchor.source_size;

    // Even a `--force` snapshot is recorded, so it stays reachable from `restore --list`.
    let original = if is_first {
        anchor.clone()
    } else {
        journal
            .manifest
            .as_ref()
            .map(|m| m.original.clone())
            .unwrap_or_else(|| anchor.clone())
    };
    let mut manifest = manifest_for(
        ManifestDraft {
            session_id: &session_id,
            head: &head,
            path,
            mode: Mode::Archive,
            original: &original,
            restore: &anchor,
            result_size: size,
            result_sha256: &anchor.source_sha256,
            notes: vec!["archive-only mode; native transcript unchanged".to_string()],
        },
        journal.manifest.clone(),
    );
    manifest.status = Status::Ok;
    manifest.committed_at = Some(now_iso_utc());
    manifest.record(
        now_iso_utc(),
        "archive",
        if is_first {
            "created-immutable-original"
        } else {
            "created-snapshot"
        },
        Some(anchor.clone()),
        (!is_first).then(|| {
            "--force preserved the immutable original and captured a separate snapshot".to_string()
        }),
    );
    let manifest_file = write_manifest(&journal.key, &vault, &manifest)?;
    let _ = write_summary(&journal.key, &vault, &manifest);

    Ok(CommandResult {
        status: if is_first {
            "ok".to_string()
        } else {
            "snapshot_created".to_string()
        },
        session: path.to_string_lossy().to_string(),
        manifest: Some(manifest_file),
        backup: Some(target),
        reason: manifest.notes.clone(),
        stats: json!({
            "size": size,
            "size_human": format_size(size),
            "sha256": anchor.source_sha256,
        }),
    })
}

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
    let _source_lock = lock_session(path)?;
    let before = crate::storage::StorageSnapshot::read(path, &vault)?;
    let mut result = compact_locked(path, options, &vault)?;
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
            stats: json!({"analysis": analysis, "native_transcript_changed": false}),
        });
    }

    // The analysis pass already hashed the transcript; the backup proves that same content is
    // now durably captured, so neither step needs another traversal of the file.
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

    // Persist the recovery journal *before* the destructive rename. If the process dies after
    // this point, `restore` still knows the exact pre-compaction backup to materialize.
    let manifest_file = write_manifest(&journal.key, vault, &manifest)?;

    let _replacement_lock = compact_tmp.replace_locked(path)?;
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

/// Choose which recorded anchor to put back, refusing anything the journal has not verified.
fn resolve_restore_anchor(
    manifest: Option<&Manifest>,
    vault: &VaultPaths,
    key: &VaultKey,
    target: &RestoreTarget,
) -> Result<RecoveryAnchor> {
    let Some(m) = manifest else {
        // No journal: the immutable original is the only thing we could possibly assert.
        let fallback = backup_path(vault, key);
        return Err(VaultError::ManifestInvalid {
            path: manifest_path(vault, key),
            reason: format!(
                "no manifest for this session; {} cannot be verified against a recorded state",
                fallback.display()
            ),
        });
    };
    match target {
        RestoreTarget::Latest => Ok(m.restore.clone()),
        RestoreTarget::Original => Ok(m.original.clone()),
        RestoreTarget::Backup(wanted) => m
            .anchors()
            .into_iter()
            .find(|a| paths_equal(&a.backup_path, wanted))
            .ok_or_else(|| VaultError::ManifestInvalid {
                path: manifest_path(vault, key),
                reason: format!(
                    "{} is not a recovery anchor recorded for this session; run `restore --list`",
                    wanted.display()
                ),
            }),
    }
}

pub fn restore_impl(path: &Path, target: RestoreTarget) -> Result<CommandResult> {
    ensure_plain_native_session(path)?;
    let vault = ensure_vault_paths()?;
    let _operation = MutationGuard::acquire(&vault.root, path)?;
    let _lock = lock_session(path)?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let manifest_file = manifest_path(&vault, &journal.key);
    let manifest = journal.manifest.clone();
    let anchor = resolve_restore_anchor(manifest.as_ref(), &vault, &journal.key, &target)?;

    if !anchor.backup_path.exists() {
        return Err(VaultError::BackupMissing {
            path: anchor.backup_path,
        });
    }

    // Both checks are now unconditional. Previously a manifest missing either key skipped them.
    let compressed_sha = sha256_file(&anchor.backup_path)?;
    if compressed_sha != anchor.backup_sha256 {
        return Ok(CommandResult {
            status: "failed".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(anchor.backup_path),
            reason: vec![format!(
                "restore backup hash mismatch: expected {}, got {compressed_sha}",
                anchor.backup_sha256
            )],
            stats: json!({}),
        });
    }

    // Capture what is on disk *now* before overwriting it. Restoring an older anchor after Codex
    // has appended new turns would otherwise discard them with no way back.
    let current_size = fs::metadata(path)
        .map_err(|e| VaultError::io("reading session size", path, e))?
        .len();
    let current_sha = sha256_file(path)?;
    let mut reason = Vec::new();
    let pre_restore = if current_sha == anchor.source_sha256 {
        reason.push("transcript already matches the requested state".to_string());
        None
    } else {
        let captured = create_verified_backup_of(
            path,
            &prerestore_backup_path(&vault, &journal.key),
            Some(&current_sha),
        )?;
        if current_size > anchor.source_size
            && sha256_rollout_prefix(path, anchor.source_size)? == anchor.source_sha256
        {
            reason.push(format!(
                "the transcript grew by {} after this state was recorded; that content is \
                 preserved in {} and reachable with `restore --to`",
                format_size(current_size - anchor.source_size),
                captured.backup_path.display()
            ));
        } else {
            reason.push(format!(
                "the transcript as it stood ({}) was captured to {} before being replaced",
                format_size(current_size),
                captured.backup_path.display()
            ));
        }
        Some(captured)
    };

    let temp = TempFile::beside(path, "restore");
    decompress_file(&anchor.backup_path, temp.path())?;
    let (ok, issues) = verify_jsonl(temp.path())?;
    if !ok {
        return Ok(CommandResult {
            status: "failed".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(anchor.backup_path),
            reason: issues,
            stats: json!({}),
        });
    }
    let restored_sha = sha256_file(temp.path())?;
    if restored_sha != anchor.source_sha256 {
        return Ok(CommandResult {
            status: "failed".to_string(),
            session: path.to_string_lossy().to_string(),
            manifest: Some(manifest_file),
            backup: Some(anchor.backup_path),
            reason: vec![format!(
                "restore content hash mismatch: expected {}, got {restored_sha}",
                anchor.source_sha256
            )],
            stats: json!({}),
        });
    }
    let mut m = manifest.ok_or(VaultError::Internal {
        detail: "restore anchor without manifest",
    })?;
    // Commit the undo anchor BEFORE changing any native bytes, exactly as compaction does.
    // A disk error or interrupted restore must never leave the only newer state prunable.
    m.status = Status::Prepared;
    m.committed_at = None;
    m.record(
        now_iso_utc(),
        "restore",
        "prepared",
        pre_restore.clone(),
        Some(format!("requested {}", anchor.backup_path.display())),
    );
    if pre_restore.is_none() {
        m.restore = anchor.clone();
    }
    write_manifest(&journal.key, &vault, &m)?;
    let _replacement_lock = temp.replace_locked(path)?;
    let recovery_manifest = manifest_file.clone();
    (|| {
    let active_sha = sha256_file(path)?;
    let active_size = fs::metadata(path)?.len();
    if active_sha != anchor.source_sha256 || active_size != anchor.source_size {
        return Err(VaultError::mismatch("restored transcript after replacement", &anchor.source_sha256, &active_sha));
    }
    {
        m.last_restored_at = Some(now_iso_utc());
        m.last_restore_sha256 = Some(restored_sha.clone());
        m.result_size = anchor.source_size;
        m.result_sha256 = restored_sha.clone();
        // `record` promotes the pre-restore capture to the newest anchor, so an unwanted restore
        // is itself undoable.
        m.record(
            now_iso_utc(),
            "restore",
            "restored",
            pre_restore.clone(),
            Some(format!("restored {}", anchor.backup_path.display())),
        );
        if pre_restore.is_none() {
            m.restore = anchor.clone();
        }
        m.status = Status::Ok;
        m.committed_at = Some(now_iso_utc());
        write_manifest(&journal.key, &vault, &m)?;
        let _ = write_summary(&journal.key, &vault, &m);
    }

    reason.push("session restored exactly to the requested recorded state".to_string());
    Ok(CommandResult {
        status: "ok".to_string(),
        session: path.to_string_lossy().to_string(),
        manifest: Some(manifest_file),
        backup: Some(anchor.backup_path),
        reason,
        stats: json!({
            "size": anchor.source_size,
            "sha256": restored_sha,
            "replaced_size": current_size,
            "pre_restore_backup": pre_restore.map(|a| a.backup_path.to_string_lossy().to_string()),
        }),
    })
    })().map_err(|e: VaultError| e.after_replacement(&recovery_manifest))
}

/// List every recovery anchor recorded for a session, newest last.
pub fn list_anchors(path: &Path) -> Result<Value> {
    let vault = ensure_vault_paths()?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let Some(m) = journal.manifest.clone() else {
        return Ok(json!({
            "session_id": session_id,
            "vault_key": journal.key,
            "anchors": [],
            "history": [],
        }));
    };
    let anchors: Vec<Value> = m
        .anchors()
        .into_iter()
        .map(|a| {
            json!({
                "backup_path": a.backup_path.to_string_lossy(),
                "exists": a.backup_path.exists(),
                "source_size": a.source_size,
                "source_size_human": format_size(a.source_size),
                "source_sha256": a.source_sha256,
                "is_original": paths_equal(&a.backup_path, &m.original.backup_path),
                "is_current_restore_target": paths_equal(&a.backup_path, &m.restore.backup_path),
            })
        })
        .collect();
    Ok(json!({
        "session_id": session_id,
        "vault_key": journal.key,
        "session": path.to_string_lossy(),
        "anchors": anchors,
        "history": m.history,
    }))
}

/// How thoroughly `doctor` re-proves what the journal already recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DoctorDepth {
    /// Verify the archive *bytes* against the manifest, and trust the decompression check that
    /// was performed when the backup was created.
    ///
    /// This is not a shortcut: `create_verified_backup` proved `decompress(archive) ==
    /// source_sha256` before the archive was ever committed. If the archive bytes still hash to
    /// what the journal recorded, decompressing them necessarily yields the same content. The
    /// deep pass only adds protection against a zstd decoder behaving differently than it did
    /// then. Neither mode helps if an attacker rewrites the archive *and* the manifest together;
    /// the vault does not sign its journal.
    #[default]
    Standard,
    /// Additionally decompress every archive and re-parse the whole transcript.
    Deep,
}

pub fn doctor_one(path: &Path, depth: DoctorDepth) -> Result<DoctorCheck> {
    let vault = ensure_vault_paths()?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let mut notes = Vec::new();
    let mut status = "ok".to_string();

    let journal = match open_journal(&vault, path, &session_id) {
        Ok(j) => j,
        Err(err) => {
            notes.push(format!("manifest unusable: {err}"));
            Journal {
                key: VaultKey::for_rollout(path),
                legacy_key: VaultKey::legacy_thread_id(&session_id),
                manifest: None,
            }
        }
    };
    let manifest = journal.manifest.clone();
    let manifest_exists = manifest.is_some();
    let mut manifest_ok = manifest_exists && manifest.is_some();
    if !manifest_exists {
        notes.push("missing manifest".to_string());
    }

    // Lineage first: the live transcript must be an exact or append-only descendant of something
    // the journal recorded. Establishing that also tells us whether the JSONL still needs parsing.
    let current_size = fs::metadata(path)
        .map_err(|e| VaultError::io("reading session size", path, e))?
        .len();
    let mut lineage_exact = false;
    if let Some(m) = manifest.as_ref() {
        let mut candidates: Vec<(String, u64)> = vec![(m.result_sha256.clone(), m.result_size)];
        for a in m.anchors() {
            candidates.push((a.source_sha256, a.source_size));
        }
        candidates.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
        candidates.dedup();

        let mut lineage_ok = false;
        for (expected_sha, expected_size) in candidates {
            // A recorded state longer than the file cannot be a prefix of it; skip without reading.
            if expected_size > current_size {
                continue;
            }
            if sha256_rollout_prefix(path, expected_size)? == expected_sha {
                lineage_ok = true;
                lineage_exact = expected_size == current_size;
                break;
            }
        }
        if !lineage_ok {
            manifest_ok = false;
            notes.push(
                "current transcript is not an exact or append-only descendant of any state \
                 recorded in the manifest"
                    .to_string(),
            );
        }
    }

    // Re-parsing a transcript that is byte-identical to a state we already validated proves
    // nothing new, and on a multi-GB rollout it is the single most expensive thing doctor does.
    let (session_ok, session_errors) = if lineage_exact && depth == DoctorDepth::Standard {
        notes.push(
            "JSONL validity inherited from a byte-identical recorded state; run `doctor --deep` \
             to re-parse the transcript"
                .to_string(),
        );
        (true, Vec::new())
    } else {
        match verify_jsonl(path) {
            Ok((ok, errs)) => (ok, errs),
            Err(err) => (false, vec![format!("session read error: {err}")]),
        }
    };
    if !session_ok {
        notes.extend(session_errors.into_iter().take(4));
    }

    let mut backup_exists = false;
    let mut backup_ok = manifest.is_some();
    // An operation that never reached its commit is a real finding, unlike the informational
    // note about an undetected Codex version.
    let mut interrupted = false;

    // Verify every anchor, not just the newest: the immutable original is a second line of
    // defence and must stay provable on its own.
    if let Some(m) = manifest.as_ref() {
        for anchor in m.anchors() {
            if paths_equal(&anchor.backup_path, &m.restore.backup_path) {
                backup_exists = anchor.backup_path.exists();
            }
            if !anchor.backup_path.exists() {
                backup_ok = false;
                notes.push(format!("missing backup: {}", anchor.backup_path.display()));
                continue;
            }
            match sha256_file(&anchor.backup_path) {
                Ok(compressed) if compressed != anchor.backup_sha256 => {
                    backup_ok = false;
                    manifest_ok = false;
                    notes.push(format!(
                        "backup {} bytes do not match the manifest SHA-256",
                        anchor.backup_path.display()
                    ));
                    continue;
                }
                Ok(_) => {}
                Err(err) => {
                    backup_ok = false;
                    notes.push(format!("backup read error: {err}"));
                    continue;
                }
            }
            if depth == DoctorDepth::Deep {
                match crate::hashing::sha256_zstd_decompressed_with_size(&anchor.backup_path) {
                    Ok((decoded_sha, decoded_size)) => {
                        if decoded_sha != anchor.source_sha256 || decoded_size != anchor.source_size
                        {
                            backup_ok = false;
                            manifest_ok = false;
                            notes.push(format!(
                                "backup {} does not decode to the recorded state",
                                anchor.backup_path.display()
                            ));
                        }
                    }
                    Err(err) => {
                        backup_ok = false;
                        notes.push(format!(
                            "backup {} decode error: {err}",
                            anchor.backup_path.display()
                        ));
                    }
                }
            }
        }

        // Report what the *journal* pins, not what the transcript could offer: a manifest
        // upgraded from v1 has no version even though the rollout in front of us names one.
        match (
            m.codex_version_source,
            head.provenance.cli_version.as_deref(),
        ) {
            (CodexVersionSource::Unknown, Some(from_transcript)) => notes.push(format!(
                "this manifest pins no Codex version, but the transcript records \
                 `{from_transcript}`; the next archive or compact-safe will record it"
            )),
            (CodexVersionSource::Unknown, None) => notes.push(
                "neither this manifest nor the transcript records a Codex version, so the \
                 transcript layout cannot be pinned to a build"
                    .to_string(),
            ),
            (CodexVersionSource::InstalledCli, _) => notes.push(
                "this manifest's Codex version came from the installed CLI, not from the \
                 transcript; it describes this machine rather than the build that wrote the \
                 rollout"
                    .to_string(),
            ),
            _ => {}
        }
        if m.status == Status::Prepared {
            interrupted = true;
            notes.push(
                "manifest is still in `prepared` state: an operation was interrupted before it \
                 committed; `restore` will put the pre-operation state back"
                    .to_string(),
            );
        }
    }

    // A page whose successor points past its end means the thread can no longer be resumed.
    let current_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut lineage_broken = false;
    for successor in lineage_successors(&head.session_id, &head.page_id) {
        if successor.is_broken_by(current_size) {
            lineage_broken = true;
            notes.push(format!(
                "this rollout is shorter than the byte offset {} continues from; Codex can no                  longer resume the thread. `restore` puts the page back.",
                successor.path.display()
            ));
        }
    }

    // Anything on disk the journal does not know about is a leak, not a spare copy.
    let unreferenced = match unreferenced_backups(&vault, &journal.keys(), manifest.as_ref()) {
        Ok(paths) => paths,
        Err(err) => {
            status = "warning".to_string();
            notes.push(format!("cannot audit unreferenced backups: {err}"));
            Vec::new()
        }
    };
    for p in &unreferenced {
        notes.push(format!(
            "backup not referenced by the manifest: {}",
            p.display()
        ));
    }

    let mut stale = Vec::new();
    for key in journal.keys() {
        stale.extend(stale_temp_files(&vault.backups, key.as_str()));
        stale.extend(stale_temp_files(&vault.manifests, key.as_str()));
    }
    if let Some(dir) = path.parent() {
        stale.extend(stale_temp_files(dir, &rollout_stem(path)));
    }
    stale.sort();
    stale.dedup();
    for p in &stale {
        notes.push(format!(
            "leftover temporary file from an interrupted run: {}",
            p.display()
        ));
    }

    if !backup_ok
        || !manifest_ok
        || !session_ok
        || interrupted
        || lineage_broken
        || !unreferenced.is_empty()
        || !stale.is_empty()
    {
        status = "warning".to_string();
    }

    Ok(DoctorCheck {
        session: session_id,
        session_path: path.to_path_buf(),
        status,
        notes,
        backup_exists,
        backup_ok,
        session_ok,
        manifest_exists,
        manifest_ok,
        unreferenced_backups: unreferenced,
        stale_temp_files: stale,
        deep: depth == DoctorDepth::Deep,
        lineage_broken,
    })
}

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
