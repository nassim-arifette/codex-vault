//! The recovery journal.
//!
//! This is the single source of truth for undoing a destructive operation, so it is a typed,
//! versioned document rather than a bag of JSON keys. Every field a safety check depends on is
//! non-`Option`: a manifest that cannot be deserialized is a refusal, not a silently skipped
//! verification. The previous flat v1 shape is still readable and is upgraded on load.

use crate::error::{Result, VaultError};
use crate::fsatomic::TempFile;
use crate::paths::{manifest_path, summary_path, VaultKey, VaultPaths};
use crate::util::format_size;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

/// Bumped from 1 when the flat key layout became the nested, typed layout below.
pub const MANIFEST_VERSION: u32 = 2;

pub const SCHEMA_ADAPTER: &str = "codex-bounded-context-2026-09-v0.1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Archive,
    CompactSafe,
    ArchiveOnlyFallback,
}

/// Where the recorded Codex version came from. Provenance about the provenance: a version read
/// out of the transcript describes the build that wrote it, while one probed from `PATH` only
/// describes this machine today.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexVersionSource {
    /// Read from the transcript's own `session_meta`. Authoritative.
    SessionMeta,
    /// Supplied by `CODEX_VAULT_CODEX_VERSION`.
    Environment,
    /// Probed by running the installed `codex --version`.
    InstalledCli,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Written before the destructive rename. If a crash leaves this on disk, `restore` still
    /// knows the exact pre-operation state.
    Prepared,
    Ok,
    RestoredAfterFailedVerification,
}

/// An exact transcript state the vault can put back, plus the backup that holds it.
///
/// `source_*` describe the decompressed transcript; `backup_*` describe the `.zst` on disk.
/// Both are required: verifying only the archive proves the file is intact, not that it holds
/// the content the journal claims.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryAnchor {
    pub backup_path: PathBuf,
    pub backup_sha256: String,
    pub source_sha256: String,
    pub source_size: u64,
}

/// What one operation did, appended in order. Every backup the vault has ever written appears
/// here, so no verified snapshot can become unreachable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: String,
    pub operation: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<RecoveryAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionRecord {
    pub session_meta_index: usize,
    pub cutoff_index: usize,
    pub checkpoint_index: Option<usize>,
    pub window_number: Option<u64>,
    pub replacement_history_items: Option<usize>,
    pub input_size: u64,
    pub input_sha256: String,
    pub kept_lines: usize,
    pub removed_lines: usize,
    pub kept_bytes: u64,
    pub removed_bytes: u64,
    pub reduction_this_operation_percent: f64,
    pub reduction_from_original_percent: f64,
    pub compatibility_basis: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<String>,
    pub session_id: String,
    pub session_path: String,
    pub mode: Mode,
    pub status: Status,
    pub schema_adapter: String,
    /// `None` means detection failed, which is a compatibility risk worth surfacing rather than
    /// a neutral absence — see `codex_version_detected`.
    pub codex_version: Option<String>,
    pub codex_version_detected: bool,
    /// New in v2.1. Optional because these are *facts the transcript may or may not carry*, not
    /// inputs to a safety check — no verification consults them, so absence is not a refusal.
    #[serde(default)]
    pub codex_version_source: CodexVersionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_source: Option<String>,
    /// `paginated` and friends: how Codex stores history, which bounds what reconstruction needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_mode: Option<String>,
    /// The window a compaction's `window_number` counts within.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_id: Option<String>,
    /// The first immutable full backup ever taken. Never overwritten.
    pub original: RecoveryAnchor,
    /// The newest verified capture: what `restore` puts back by default.
    pub restore: RecoveryAnchor,
    /// The transcript state this operation left on disk.
    pub result_size: u64,
    pub result_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionRecord>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_restored_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_restore_sha256: Option<String>,
}

impl Manifest {
    /// Record an operation and, when it captured a new exact state, make that state the anchor
    /// `restore` will use. Any backup passed here becomes reachable through `restore --list`.
    pub fn record(
        &mut self,
        at: String,
        operation: &str,
        outcome: &str,
        anchor: Option<RecoveryAnchor>,
        note: Option<String>,
    ) {
        if let Some(anchor) = anchor.clone() {
            self.restore = anchor;
        }
        self.history.push(HistoryEntry {
            at,
            operation: operation.to_string(),
            outcome: outcome.to_string(),
            anchor,
            note,
        });
    }

    /// Every recovery anchor this session has: the immutable original plus every snapshot
    /// recorded in history, oldest first.
    pub fn anchors(&self) -> Vec<RecoveryAnchor> {
        let mut out = vec![self.original.clone()];
        for entry in &self.history {
            if let Some(a) = &entry.anchor {
                if !out.iter().any(|x| x.backup_path == a.backup_path) {
                    out.push(a.clone());
                }
            }
        }
        if !out
            .iter()
            .any(|x| x.backup_path == self.restore.backup_path)
        {
            out.push(self.restore.clone());
        }
        out
    }
}

/// Read a manifest, upgrading the legacy flat v1 layout.
///
/// A file that exists but cannot be understood is an error. Treating it as "absent" would skip
/// every integrity check that consults it, which is the wrong default for a recovery journal.
pub fn load_manifest(path: &Path) -> Result<Option<Manifest>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|e| VaultError::io("reading manifest", path, e))?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| VaultError::json("parsing manifest", path, e))?;

    let version = value
        .get("manifest_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| VaultError::ManifestInvalid {
            path: path.to_path_buf(),
            reason: "missing manifest_version".to_string(),
        })?;

    let value = match version {
        1 => upgrade_v1(path, &value)?,
        v if v as u32 == MANIFEST_VERSION => value,
        other => {
            return Err(VaultError::ManifestInvalid {
                path: path.to_path_buf(),
                reason: format!(
                    "manifest_version {other} was written by a newer Codex Vault; refusing to \
                     act on a journal this build cannot fully understand"
                ),
            })
        }
    };

    let manifest: Manifest =
        serde_json::from_value(value).map_err(|e| VaultError::ManifestInvalid {
            path: path.to_path_buf(),
            reason: format!("{e}; a manifest missing required fields cannot be verified"),
        })?;
    Ok(Some(manifest))
}

/// Convert the original flat layout into the typed one.
///
/// v1 stored `restore_*` keys that were optional in practice; when they are absent the immutable
/// original is the only state that was ever proven, so it becomes both anchors.
fn upgrade_v1(path: &Path, v: &Value) -> Result<Value> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let n = |k: &str| v.get(k).and_then(Value::as_u64);
    let invalid = |reason: String| VaultError::ManifestInvalid {
        path: path.to_path_buf(),
        reason,
    };

    let original_sha = s("original_sha256")
        .ok_or_else(|| invalid("v1 manifest without original_sha256".to_string()))?;
    let original_size = n("original_size")
        .ok_or_else(|| invalid("v1 manifest without original_size".to_string()))?;
    let original_backup = s("original_backup_path")
        .or_else(|| s("backup_path"))
        .ok_or_else(|| invalid("v1 manifest without a backup path".to_string()))?;
    let original_backup_sha = s("original_backup_sha256")
        .or_else(|| s("backup_sha256"))
        .ok_or_else(|| invalid("v1 manifest without a backup SHA-256".to_string()))?;

    let original = RecoveryAnchor {
        backup_path: PathBuf::from(&original_backup),
        backup_sha256: original_backup_sha.clone(),
        source_sha256: original_sha.clone(),
        source_size: original_size,
    };
    let restore = RecoveryAnchor {
        backup_path: PathBuf::from(s("restore_backup_path").unwrap_or(original_backup)),
        backup_sha256: s("restore_backup_sha256").unwrap_or(original_backup_sha),
        source_sha256: s("restore_source_sha256").unwrap_or_else(|| original_sha.clone()),
        source_size: n("restore_source_size").unwrap_or(original_size),
    };

    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("archive") => "archive",
        Some("archive-only-fallback") => "archive-only-fallback",
        _ => "compact-safe",
    };
    let status = match v.get("status").and_then(Value::as_str) {
        Some("prepared") => "prepared",
        Some("restored_after_failed_verification") => "restored_after_failed_verification",
        _ => "ok",
    };

    let compaction = match (n("cutoff_index"), n("session_meta_index")) {
        (Some(cutoff), Some(meta)) => serde_json::json!({
            "session_meta_index": meta,
            "cutoff_index": cutoff,
            "checkpoint_index": n("checkpoint_index"),
            "window_number": n("window_number"),
            "replacement_history_items": n("replacement_history_items"),
            "input_size": n("input_size").unwrap_or(original_size),
            "input_sha256": s("input_sha256").unwrap_or_else(|| original_sha.clone()),
            "kept_lines": n("kept_lines").unwrap_or(0),
            "removed_lines": n("removed_lines").unwrap_or(0),
            "kept_bytes": n("kept_bytes").unwrap_or(0),
            "removed_bytes": n("removed_bytes").unwrap_or(0),
            "reduction_this_operation_percent":
                v.get("reduction_this_operation_percent").and_then(Value::as_f64).unwrap_or(0.0),
            "reduction_from_original_percent":
                v.get("reduction_from_original_percent").and_then(Value::as_f64).unwrap_or(0.0),
            "compatibility_basis": s("compatibility_basis").unwrap_or_default(),
        }),
        _ => Value::Null,
    };

    Ok(serde_json::json!({
        "manifest_version": MANIFEST_VERSION,
        "created_at": s("created_at").unwrap_or_default(),
        "committed_at": s("committed_at"),
        "session_id": s("session_id")
            .ok_or_else(|| invalid("v1 manifest without session_id".to_string()))?,
        "session_path": s("session_path").unwrap_or_default(),
        "mode": mode,
        "status": status,
        "schema_adapter": s("schema_adapter").unwrap_or_else(|| SCHEMA_ADAPTER.to_string()),
        "codex_version": s("codex_version"),
        "codex_version_detected": s("codex_version").is_some(),
        "codex_version_source": if s("codex_version").is_some() { "installed_cli" } else { "unknown" },
        "original": original,
        "restore": restore,
        "result_size": n("result_size").unwrap_or(original_size),
        "result_sha256": s("result_sha256").unwrap_or(original_sha),
        "compaction": compaction,
        "history": [{
            "at": s("created_at").unwrap_or_default(),
            "operation": mode,
            "outcome": "upgraded-from-manifest-v1",
        }],
        "notes": v.get("notes").cloned().unwrap_or_else(|| Value::Array(vec![])),
        "last_restored_at": s("last_restored_at"),
        "last_restore_sha256": s("last_restore_sha256"),
    }))
}

pub fn write_manifest(key: &VaultKey, vault: &VaultPaths, manifest: &Manifest) -> Result<PathBuf> {
    let path = manifest_path(vault, key);
    let temp = TempFile::beside(&path, "manifest");
    let mut file = File::create(temp.path())
        .map_err(|e| VaultError::io("creating manifest temp file", temp.path(), e))?;
    serde_json::to_writer_pretty(&mut file, manifest)
        .map_err(|e| VaultError::json("writing manifest", temp.path(), e))?;
    file.write_all(b"\n")
        .map_err(|e| VaultError::io("writing manifest", temp.path(), e))?;
    file.sync_all()
        .map_err(|e| VaultError::io("flushing manifest", temp.path(), e))?;
    drop(file);
    temp.commit_onto(&path)?;
    Ok(path)
}

pub fn write_summary(key: &VaultKey, vault: &VaultPaths, manifest: &Manifest) -> Result<PathBuf> {
    let path = summary_path(vault, key);
    let mut body = String::new();
    writeln!(&mut body, "# Codex Vault — {}", manifest.session_id).ok();
    writeln!(&mut body, "- Vault key: `{key}`").ok();
    writeln!(&mut body).ok();
    writeln!(&mut body, "- Mode: `{:?}`", manifest.mode).ok();
    writeln!(&mut body, "- Status: `{:?}`", manifest.status).ok();
    writeln!(&mut body, "- Created: `{}`", manifest.created_at).ok();
    writeln!(&mut body, "- Session: `{}`", manifest.session_path).ok();
    writeln!(
        &mut body,
        "- Original size: `{}`",
        format_size(manifest.original.source_size)
    )
    .ok();
    writeln!(
        &mut body,
        "- Result size: `{}`",
        format_size(manifest.result_size)
    )
    .ok();
    writeln!(
        &mut body,
        "- Codex version: `{}` (source: {:?})",
        manifest
            .codex_version
            .clone()
            .unwrap_or_else(|| "not detected".to_string()),
        manifest.codex_version_source
    )
    .ok();
    if let Some(o) = &manifest.originator {
        writeln!(&mut body, "- Originator: `{o}`").ok();
    }
    if let Some(h) = &manifest.history_mode {
        writeln!(&mut body, "- History mode: `{h}`").ok();
    }
    if let Some(c) = &manifest.compaction {
        writeln!(&mut body, "- Safe cutoff line: `{}`", c.cutoff_index + 1).ok();
        if let Some(i) = c.checkpoint_index {
            writeln!(&mut body, "- Checkpoint line: `{}`", i + 1).ok();
        }
    }
    writeln!(&mut body).ok();
    writeln!(&mut body, "## Recovery anchors").ok();
    for a in manifest.anchors() {
        writeln!(
            &mut body,
            "- `{}` — {} of transcript",
            a.backup_path.display(),
            format_size(a.source_size)
        )
        .ok();
    }
    fs::write(&path, body).map_err(|e| VaultError::io("writing summary", &path, e))?;
    Ok(path)
}
