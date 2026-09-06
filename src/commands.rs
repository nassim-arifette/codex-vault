//! Thin CLI-facing wrappers that turn operations into JSON documents.

use crate::analysis::analyze_session_within;
use crate::discovery::{
    discover_sessions, discover_sessions_scoped, parse_filter, resolve_session_reference,
    FilterScope,
};
use crate::error::{Result, VaultError};
use crate::ops::{
    archive_impl, compact_safe_impl_with, doctor_one, list_anchors, prune_one, restore_impl,
    CompactOptions, DoctorDepth, RestoreTarget,
};
use crate::parallel::{map_ordered, Progress, ProgressMode};
use crate::paths::{codex_root, detect_codex_version, vault_root};
use crate::rollout::is_codex_zstd_jsonl;
use crate::util::format_size;
use serde_json::{json, Value};

pub fn scan_command(cwd_filter: Option<String>) -> Result<Value> {
    let filter = parse_filter(cwd_filter)?;
    let sessions = discover_sessions(filter.as_deref())?;
    let total_size: u64 = sessions.iter().map(|s| s.size_bytes).sum();
    Ok(json!({
        "codex_root": codex_root(),
        "vault_root": vault_root(),
        "codex_version": detect_codex_version(),
        "total_sessions": sessions.len(),
        "total_size_bytes": total_size,
        "total_size_human": format_size(total_size),
        "sessions": sessions,
    }))
}

pub fn analyze_command(
    session: Option<String>,
    cwd_filter: Option<String>,
    scan_window: usize,
    batch: BatchOptions,
) -> Result<Value> {
    let filter = parse_filter(cwd_filter)?;
    if let Some(reference) = session {
        let path = resolve_session_reference(&reference, filter.as_deref())?;
        return Ok(json!({
            "session": path.to_string_lossy(),
            "codex_native_zstd": is_codex_zstd_jsonl(&path),
            "analysis": analyze_session_within(&path, scan_window)?,
        }));
    }
    let sessions = discover_sessions(filter.as_deref())?;
    let progress = Progress::new("analyze", sessions.len(), batch.progress);
    let rows = map_ordered(&sessions, batch.jobs, |_, info| {
        let row = match analyze_session_within(&info.path, scan_window) {
            Ok(analysis) => json!({
                "session_id": info.session_id,
                "session": info.path.to_string_lossy(),
                "cli_version": info.cli_version,
                "analysis": analysis,
            }),
            // One unreadable rollout used to abort the whole batch. Report it and carry on.
            Err(err) => json!({
                "session_id": info.session_id,
                "session": info.path.to_string_lossy(),
                "status": "error",
                "code": err.code(),
                "exit_code": err.exit_code(),
                "error": err.to_string(),
            }),
        };
        progress.item_done(&info.session_id);
        row
    });
    Ok(json!({"sessions": rows}))
}

pub fn archive_command(session: String, cwd_filter: Option<String>, force: bool) -> Result<Value> {
    let filter = parse_filter(cwd_filter)?;
    let path = resolve_session_reference(&session, filter.as_deref())?;
    Ok(json!(archive_impl(&path, force)?))
}

/// Shared knobs for the commands that can act on many sessions at once.
#[derive(Clone, Copy, Debug)]
pub struct BatchOptions {
    pub jobs: usize,
    pub progress: ProgressMode,
}

impl Default for BatchOptions {
    fn default() -> Self {
        BatchOptions {
            jobs: crate::parallel::default_jobs(),
            progress: ProgressMode::default(),
        }
    }
}

pub fn compact_safe_command(
    session: Option<String>,
    cwd_filter: Option<String>,
    options: CompactOptions,
    batch: BatchOptions,
) -> Result<Value> {
    let batch_requested = session.is_none();
    if batch_requested && cwd_filter.is_none() {
        return Err(VaultError::RefusedImplicitBatch);
    }

    let filter = parse_filter(cwd_filter)?;
    if let Some(reference) = session {
        let path = resolve_session_reference(&reference, filter.as_deref())?;
        return Ok(json!(compact_safe_impl_with(&path, options)?));
    }

    // A destructive batch only ever narrows: the session's own cwd must live inside the filter.
    // `Related` would also match sessions from a *parent* directory, i.e. other projects.
    let sessions = discover_sessions_scoped(filter.as_deref(), FilterScope::Within)?;
    // Deliberately serial: this is the destructive path. Running several compactions at once
    // would multiply the number of transcripts in flight if something goes wrong, for a speed-up
    // that a single volume would not deliver anyway.
    let progress = Progress::new("compact-safe", sessions.len(), batch.progress);
    let mut rows = Vec::with_capacity(sessions.len());
    for info in sessions {
        if is_codex_zstd_jsonl(&info.path) {
            rows.push(json!({
                "session_id": info.session_id,
                "session": info.path.to_string_lossy(),
                "status": "read_only_native_zstd",
                "reason": ["Codex already manages this rollout as .jsonl.zst; Vault will not rewrite it"],
            }));
            progress.item_done(&info.session_id);
            continue;
        }
        // A spawned thread is skipped rather than failing the batch: in a real corpus they
        // are the majority, and a whole-project compaction should still do its job.
        if info.is_spawned_thread && !options.allow_spawned_threads {
            rows.push(json!({
                "session_id": info.session_id,
                "session": info.path.to_string_lossy(),
                "status": "skipped_spawned_thread",
                "thread_source": info.thread_source,
                "reason": ["Codex cannot resume a spawned thread standalone, so compacting it is \
                            unvalidated; pass --allow-spawned-threads to include it"],
            }));
            progress.item_done(&info.session_id);
            continue;
        }
        match compact_safe_impl_with(&info.path, options) {
            Ok(result) => rows.push(json!({
                "session_id": info.session_id,
                "result": result,
            })),
            Err(VaultError::LineageSourceRefused { successors, .. }) => rows.push(json!({
                "session_id": info.session_id,
                "session": info.path,
                "status": "skipped_lineage_source",
                "continued_by": successors,
                "reason": ["a later page depends on this one; only the latest page can be compacted"],
            })),
            Err(err) => rows.push(json!({
                "session_id": info.session_id,
                "session": info.path.to_string_lossy(),
                "status": "error",
                "code": err.code(),
                "exit_code": err.exit_code(),
                "error": err.to_string(),
            })),
        }
        progress.item_done(&info.session_id);
    }
    Ok(json!({"sessions": rows}))
}

pub fn restore_command(
    session: String,
    cwd_filter: Option<String>,
    original: bool,
    to: Option<String>,
    list: bool,
) -> Result<Value> {
    let filter = parse_filter(cwd_filter)?;
    let path = resolve_session_reference(&session, filter.as_deref())?;
    if list {
        return list_anchors(&path);
    }
    let target = match (original, to) {
        (_, Some(backup)) => RestoreTarget::Backup(std::path::PathBuf::from(backup)),
        (true, None) => RestoreTarget::Original,
        (false, None) => RestoreTarget::Latest,
    };
    Ok(json!(restore_impl(&path, target)?))
}

pub fn doctor_command(
    session: Option<String>,
    cwd_filter: Option<String>,
    deep: bool,
    batch: BatchOptions,
) -> Result<Value> {
    let depth = if deep {
        DoctorDepth::Deep
    } else {
        DoctorDepth::Standard
    };
    let filter = parse_filter(cwd_filter)?;
    if let Some(reference) = session {
        let path = resolve_session_reference(&reference, filter.as_deref())?;
        return Ok(json!([doctor_one(&path, depth)?]));
    }
    let sessions = discover_sessions(filter.as_deref())?;
    let progress = Progress::new("doctor", sessions.len(), batch.progress);
    let checks = map_ordered(&sessions, batch.jobs, |_, info| {
        let row = match doctor_one(&info.path, depth) {
            Ok(check) => json!(check),
            Err(err) => json!({
                "session": info.session_id,
                "session_path": info.path.to_string_lossy(),
                "status": "error",
                "code": err.code(),
                "exit_code": err.exit_code(),
                "error": err.to_string(),
            }),
        };
        progress.item_done(&info.session_id);
        row
    });
    Ok(json!(checks))
}

/// Exit status for completed reports, including batch failures that are intentionally rows
/// rather than early returns. Recovery refusals must not look like success to a CLI caller.
pub fn output_exit_code(value: &Value) -> u8 {
    use crate::error::exit;
    match value {
        Value::Array(rows) => rows.iter().map(output_exit_code).max().unwrap_or(0),
        Value::Object(row) => {
            let own = match row.get("status").and_then(Value::as_str) {
                Some("error") => row
                    .get("exit_code")
                    .and_then(Value::as_u64)
                    .unwrap_or(exit::IO as u64) as u8,
                Some(
                    "failed"
                    | "verification_failed"
                    | "restored_after_failed_verification"
                    | "warning",
                ) => exit::INTEGRITY,
                _ => 0,
            };
            let failed_deletions = row
                .get("failed")
                .and_then(Value::as_array)
                .is_some_and(|v| !v.is_empty());
            let children = ["sessions", "result"]
                .iter()
                .filter_map(|key| row.get(*key))
                .map(output_exit_code)
                .max()
                .unwrap_or(0);
            own.max(children)
                .max(if failed_deletions { exit::IO } else { 0 })
        }
        _ => 0,
    }
}

/// Report — and, with `apply`, remove — the debris an interrupted run can leave behind.
///
/// Dry-run by default: deleting a backup is not something a maintenance command should do
/// because the user typed it once.
pub fn prune_command(
    session: Option<String>,
    cwd_filter: Option<String>,
    unreferenced_backups: bool,
    apply: bool,
) -> Result<Value> {
    let filter = parse_filter(cwd_filter)?;
    let paths = match session {
        Some(reference) => vec![resolve_session_reference(&reference, filter.as_deref())?],
        None => discover_sessions(filter.as_deref())?
            .into_iter()
            .map(|s| s.path)
            .collect(),
    };
    let mut rows = Vec::with_capacity(paths.len());
    for path in &paths {
        rows.push(prune_one(path, unreferenced_backups, apply)?);
    }
    Ok(json!({
        "applied": apply,
        "included_unreferenced_backups": unreferenced_backups,
        "sessions": rows,
    }))
}
