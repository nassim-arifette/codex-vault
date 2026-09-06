//! Codex/Vault root discovery and the on-disk layout of the vault.

use crate::error::Result;
use crate::util::now_epoch_millis;
use serde::Serialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::OnceLock;

#[derive(Debug, Serialize)]
pub struct VaultPaths {
    pub root: PathBuf,
    pub manifests: PathBuf,
    pub summaries: PathBuf,
    pub backups: PathBuf,
}

pub fn user_profile_root() -> PathBuf {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn codex_root() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_profile_root().join(".codex"))
}

pub fn vault_root() -> PathBuf {
    env::var_os("CODEX_VAULT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_profile_root().join(".codex-vault"))
}

/// Locate the `codex` executable without letting the OS pick it for us.
///
/// `Command::new("codex")` on Windows resolves through `CreateProcess`, which searches the
/// application directory and the *current directory* before `PATH`. Running the vault from a
/// directory containing a hostile `codex.exe` would then execute it. Resolving here ourselves —
/// absolute `PATH` entries only, current directory never — removes that entirely.
pub fn resolve_codex_binary() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("CODEX_VAULT_CODEX_BIN") {
        let path = PathBuf::from(explicit);
        return (path.is_absolute() && path.is_file()).then_some(path);
    }

    let extensions: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };

    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        // An empty or relative entry is exactly how the current directory sneaks in.
        if dir.as_os_str().is_empty() || !dir.is_absolute() {
            continue;
        }
        for ext in &extensions {
            let candidate = dir.join(format!("codex{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn probe_codex_version() -> Option<String> {
    let binary = resolve_codex_binary()?;
    let output = ProcessCommand::new(binary).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!raw.is_empty()).then_some(raw)
}

/// The version of the Codex that is *installed right now*, probed at most once per process.
///
/// This is only a fallback. A transcript records the build that wrote it (`session_meta`
/// `cli_version`), and that is what a manifest should pin — the installed Codex may be many
/// versions newer than the rollout in front of us. `CODEX_VAULT_CODEX_VERSION` overrides the
/// probe; `CODEX_VAULT_CODEX_BIN` points at an explicit absolute executable.
pub fn detect_codex_version() -> Option<String> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            env::var("CODEX_VAULT_CODEX_VERSION")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(probe_codex_version)
        })
        .clone()
}

pub fn vault_paths() -> VaultPaths {
    let root = vault_root();
    VaultPaths {
        root: root.clone(),
        manifests: root.join("manifests"),
        summaries: root.join("summaries"),
        backups: root.join("backups"),
    }
}

pub fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

pub fn ensure_vault_paths() -> Result<VaultPaths> {
    let paths = vault_paths();
    create_private_directory(&paths.root)?;
    create_private_directory(&paths.manifests)?;
    create_private_directory(&paths.summaries)?;
    create_private_directory(&paths.backups)?;
    Ok(paths)
}

/// Identifies one rollout **file** in the vault.
///
/// This used to be the Codex thread id from `session_meta`, which is not unique: a thread spans
/// several rollout files (`rollout-<ts>-<thread>_<fork>.jsonl`). Two such files then shared one
/// manifest and one "immutable original", and `restore --original` on the second would write the
/// first one's content into it. The rollout's own stem carries a timestamp plus one or two
/// UUIDs, so it identifies the file rather than the conversation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct VaultKey(String);

impl VaultKey {
    pub fn for_rollout(path: &Path) -> Self {
        let stem = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|name| {
                name.strip_suffix(".jsonl.zst")
                    .or_else(|| name.strip_suffix(".jsonl"))
                    .unwrap_or(name)
            })
            .unwrap_or("session");
        VaultKey::sanitized(stem)
    }

    /// The pre-P0 key, so an existing vault stays readable.
    pub fn legacy_thread_id(session_id: &str) -> Self {
        VaultKey::sanitized(session_id)
    }

    /// Filenames come from Codex, but the key ends up in a path: keep it to characters that
    /// cannot escape the vault directory.
    fn sanitized(raw: &str) -> Self {
        let cleaned: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let cleaned = cleaned.trim_matches('.').to_string();
        VaultKey(if cleaned.is_empty() {
            "session".to_string()
        } else {
            cleaned
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn manifest_path(vault: &VaultPaths, key: &VaultKey) -> PathBuf {
    vault.manifests.join(format!("{key}.json"))
}

pub fn summary_path(vault: &VaultPaths, key: &VaultKey) -> PathBuf {
    vault.summaries.join(format!("{key}.md"))
}

pub fn backup_path(vault: &VaultPaths, key: &VaultKey) -> PathBuf {
    vault.backups.join(format!("{key}.original.jsonl.zst"))
}

pub fn snapshot_backup_path(vault: &VaultPaths, key: &VaultKey) -> PathBuf {
    vault
        .backups
        .join(format!("{key}.snapshot-{}.jsonl.zst", now_epoch_millis()))
}

pub fn precompact_backup_path(vault: &VaultPaths, key: &VaultKey) -> PathBuf {
    vault
        .backups
        .join(format!("{key}.precompact-{}.jsonl.zst", now_epoch_millis()))
}

/// Captured immediately before a `restore` replaces the live transcript, so that undoing a
/// restore is always possible and no appended conversation can be lost.
pub fn prerestore_backup_path(vault: &VaultPaths, key: &VaultKey) -> PathBuf {
    vault
        .backups
        .join(format!("{key}.prerestore-{}.jsonl.zst", now_epoch_millis()))
}

/// Windows `canonicalize` returns extended-length paths (the `\\?\` verbatim prefix). Those leak
/// into manifests and error messages, and they do not compare equal to the same path written
/// conventionally — so every path the vault stores or displays is stripped back first.
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

pub fn normalized_path(path: &Path) -> PathBuf {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    strip_verbatim_prefix(&resolved.canonicalize().unwrap_or(resolved))
}

fn path_components(path: &Path) -> Vec<String> {
    normalized_path(path)
        .iter()
        .map(|c| c.to_string_lossy().to_ascii_lowercase())
        .collect()
}

/// True when either path contains the other. Convenient for *finding* sessions, but far too
/// broad to gate a destructive batch: a session whose cwd is `C:\work` is "related" to
/// `C:\work\repo\frontend`, and belongs to a different project.
pub fn is_path_related(base: &Path, candidate: &Path) -> bool {
    let a = path_components(base);
    let b = path_components(candidate);
    a == b || (a.len() <= b.len() && b.starts_with(&a)) || (b.len() <= a.len() && a.starts_with(&b))
}

/// True when `base` is `root` itself or lives inside it. This is the relation `compact --cwd`
/// uses, so the scope of a destructive batch can only ever narrow.
pub fn is_path_within(base: &Path, root: &Path) -> bool {
    let a = path_components(base);
    let b = path_components(root);
    a.len() >= b.len() && a.starts_with(&b)
}
