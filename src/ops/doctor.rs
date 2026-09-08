use super::catalog::CatalogContext;
use super::shared::{open_journal, Journal};
use crate::backup::paths_equal;
use crate::discovery::lineage_successors;
use crate::error::{Result, VaultError};
use crate::hashing::{sha256_file, sha256_rollout_prefix};
use crate::manifest::{CodexVersionSource, Status};
use crate::paths::{ensure_vault_paths, VaultKey};
use crate::rollout::{read_session_head, verify_jsonl};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

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
    // A single-target doctor should not parse every rollout merely to build the batch lineage
    // index. The direct lineage lookup first filters filenames by thread id before opening heads.
    // Batch doctor still builds one shared LineageIndex and amortizes that cost across all rows.
    let context = CatalogContext::build(&vault, &[path.to_path_buf()], true, false);
    doctor_one_with_context(path, depth, &context)
}

pub(crate) fn doctor_one_with_context(
    path: &Path,
    depth: DoctorDepth,
    context: &CatalogContext,
) -> Result<DoctorCheck> {
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
    let direct_successors;
    let successors = match context.lineage_successors(&head.session_id, &head.page_id) {
        Some(successors) => successors,
        None => {
            direct_successors = lineage_successors(&head.session_id, &head.page_id);
            &direct_successors
        }
    };
    for successor in successors {
        if successor.is_broken_by(current_size) {
            lineage_broken = true;
            notes.push(format!(
                "this rollout is shorter than the byte offset {} continues from; Codex can no                  longer resume the thread. `restore` puts the page back.",
                successor.path.display()
            ));
        }
    }

    // Anything on disk the journal does not know about is a leak, not a spare copy.
    let unreferenced = match context.unreferenced_backups(&journal.keys(), manifest.as_ref()) {
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

    let stale = context.stale_temp_files(path, &journal.keys());
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
