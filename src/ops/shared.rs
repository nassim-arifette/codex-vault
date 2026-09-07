use crate::backup::paths_equal;
use crate::error::{Result, VaultError};
use crate::manifest::{
    load_manifest, write_manifest, write_summary, CodexVersionSource, Manifest, Mode,
    RecoveryAnchor, Status, MANIFEST_VERSION, SCHEMA_ADAPTER,
};
use crate::paths::{detect_codex_version, ensure_vault_paths, manifest_path, VaultKey, VaultPaths};
use crate::rollout::{read_session_head, SessionHead};
use crate::util::{format_size, now_iso_utc};
use std::path::{Path, PathBuf};

/// Resolve the Codex build a manifest should pin, best source first.
///
/// The transcript wins: `session_meta.cli_version` names the build that actually wrote this
/// file, while the installed CLI only describes this machine today and may be many versions
/// newer than an old rollout.
pub(super) fn resolve_codex_version(head: &SessionHead) -> (Option<String>, CodexVersionSource) {
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
pub(super) fn codex_version_note(source: CodexVersionSource) -> Option<String> {
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
pub(super) fn apply_provenance(manifest: &mut Manifest, head: &SessionHead) {
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
pub(super) struct ManifestDraft<'a> {
    pub(super) session_id: &'a str,
    pub(super) head: &'a SessionHead,
    pub(super) path: &'a Path,
    pub(super) mode: Mode,
    /// The first immutable full backup, used only when no journal exists yet.
    pub(super) original: &'a RecoveryAnchor,
    /// The anchor a fresh journal should point `restore` at.
    pub(super) restore: &'a RecoveryAnchor,
    pub(super) result_size: u64,
    pub(super) result_sha256: &'a str,
    pub(super) notes: Vec<String>,
}

pub(super) fn new_manifest(draft: ManifestDraft<'_>, status: Status) -> Manifest {
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
pub(super) fn open_journal(vault: &VaultPaths, path: &Path, session_id: &str) -> Result<Journal> {
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

/// Record one page participating in a whole-conversation transaction. The recovery anchor is
/// written before any native page is replaced so ordinary `doctor`, `restore --list`, indexing
/// and MCP all keep seeing the archived history even though the transaction itself spans files.
pub(crate) fn prepare_chain_page_manifest(path: &Path, anchor: &RecoveryAnchor) -> Result<PathBuf> {
    let vault = ensure_vault_paths()?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let original = journal
        .manifest
        .as_ref()
        .map(|m| m.original.clone())
        .unwrap_or_else(|| anchor.clone());
    let mut manifest = manifest_for(
        ManifestDraft {
            session_id: &session_id,
            head: &head,
            path,
            mode: Mode::CompactConversation,
            original: &original,
            restore: anchor,
            result_size: anchor.source_size,
            result_sha256: &anchor.source_sha256,
            notes: vec!["whole-conversation compaction transaction prepared".to_string()],
        },
        journal.manifest.clone(),
    );
    manifest.status = Status::Prepared;
    manifest.committed_at = None;
    manifest.record(
        now_iso_utc(),
        "compact-conversation",
        "prepared",
        Some(anchor.clone()),
        Some("exact pre-operation page captured for coordinated transaction".to_string()),
    );
    write_manifest(&journal.key, &vault, &manifest)
}

pub(crate) fn commit_chain_page_manifest(
    path: &Path,
    result_size: u64,
    result_sha256: &str,
    removed_bytes: u64,
) -> Result<PathBuf> {
    let vault = ensure_vault_paths()?;
    let head = read_session_head(path)?;
    let session_id = head.session_id.clone();
    let journal = open_journal(&vault, path, &session_id)?;
    let mut manifest = journal.manifest.ok_or(VaultError::Internal {
        detail: "chain page commit without prepared manifest",
    })?;
    manifest.mode = Mode::CompactConversation;
    manifest.status = Status::Ok;
    manifest.committed_at = Some(now_iso_utc());
    manifest.result_size = result_size;
    manifest.result_sha256 = result_sha256.to_string();
    manifest.record(
        now_iso_utc(),
        "compact-conversation",
        "committed",
        None,
        Some(format!(
            "{} removed from this page",
            format_size(removed_bytes)
        )),
    );
    let file = write_manifest(&journal.key, &vault, &manifest)?;
    let _ = write_summary(&journal.key, &vault, &manifest);
    Ok(file)
}

/// Carry forward an existing journal, or build a fresh one describing the state we just captured.
///
/// Reusing the existing manifest is what keeps a session's history in one document; an earlier
/// version returned the stale file untouched, which silently orphaned every backup it did not
/// know about.
pub(super) fn manifest_for(draft: ManifestDraft<'_>, existing: Option<Manifest>) -> Manifest {
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
