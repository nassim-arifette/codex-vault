use crate::error::{Result, VaultError};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

/// Stable lock files are deliberately not removed: unlinking one while another process has
/// opened it would allow two different inodes to act as the same lock. The OS releases locks
/// on exit, including a crash. The vault lock also serializes prune with journal/backup writes;
/// the path lock protects a transcript even when two processes use different vault homes.
pub struct MutationGuard {
    _vault: File,
    _session: File,
}

/// One vault-wide mutation lock plus a stable path lock for every rollout participating in a
/// coordinated conversation operation. Paths are sorted first so two processes can never take
/// the same set in opposite orders.
pub struct MultiMutationGuard {
    _vault: File,
    _sessions: Vec<File>,
}

fn acquire_file(path: &Path, session: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|e| VaultError::io("opening operation lock", path, e))?;
    FileExt::try_lock_exclusive(&file).map_err(|source| VaultError::SessionLocked {
        path: session.to_path_buf(),
        source,
    })?;
    Ok(file)
}

impl MultiMutationGuard {
    pub fn acquire(vault: &Path, sessions: &[PathBuf]) -> Result<Self> {
        let first = sessions.first().ok_or(VaultError::InvalidInput {
            reason: "cannot lock an empty conversation".to_string(),
        })?;
        let vault_guard = acquire_file(&vault.join("mutation.lock"), first)?;
        let locks = std::env::temp_dir().join("codex-vault-operation-locks");
        fs::create_dir_all(&locks)?;
        let mut identities = Vec::with_capacity(sessions.len());
        for session in sessions {
            let canonical = session
                .canonicalize()
                .map_err(|e| VaultError::io("resolving operation lock", session, e))?;
            let mut identity = crate::paths::normalized_path(&canonical)
                .to_string_lossy()
                .into_owned();
            if cfg!(windows) {
                identity = identity.to_lowercase();
            }
            identities.push((identity, session.clone()));
        }
        identities.sort_by(|a, b| a.0.cmp(&b.0));
        identities.dedup_by(|a, b| a.0 == b.0);
        let mut guards = Vec::with_capacity(identities.len());
        for (identity, session) in identities {
            let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
            guards.push(acquire_file(
                &locks.join(format!("{digest}.lock")),
                &session,
            )?);
        }
        Ok(Self {
            _vault: vault_guard,
            _sessions: guards,
        })
    }
}

impl MutationGuard {
    pub fn acquire(vault: &Path, session: &Path) -> Result<Self> {
        let vault_guard = acquire_file(&vault.join("mutation.lock"), session)?;
        let locks = std::env::temp_dir().join("codex-vault-operation-locks");
        fs::create_dir_all(&locks)?;
        let canonical = session
            .canonicalize()
            .map_err(|e| VaultError::io("resolving operation lock", session, e))?;
        let identity = crate::paths::normalized_path(&canonical)
            .to_string_lossy()
            .into_owned();
        let identity = if cfg!(windows) {
            identity.to_lowercase()
        } else {
            identity
        };
        let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
        let session_guard = acquire_file(&locks.join(format!("{digest}.lock")), session)?;
        Ok(Self {
            _vault: vault_guard,
            _session: session_guard,
        })
    }
}
