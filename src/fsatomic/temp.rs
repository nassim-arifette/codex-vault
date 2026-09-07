use super::platform::{atomic_replace, replace_locked};
use crate::error::{Result, VaultError};
use crate::util::now_epoch_millis;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Suffix shared by every temporary file the vault creates, so leftovers are identifiable.
pub const TEMP_SUFFIX: &str = ".tmp";

pub fn temp_path_for(path: &Path, suffix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut out = path.to_path_buf();
    let base = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("session");
    out.set_file_name(format!(
        "{base}.{suffix}.{}.{}.{}{TEMP_SUFFIX}",
        now_epoch_millis(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    out
}

/// An RAII scratch file that deletes itself unless it is explicitly consumed.
///
/// Every destructive operation writes through one of these. Before this existed, any `?` between
/// creating a temp and renaming it left the file on disk forever — including 3 MB compaction
/// scratch files sitting next to the live transcript, which is exactly the failure this guards.
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    /// Reserve a scratch path beside `target` (same directory, so the later rename stays on one
    /// volume and can therefore be atomic).
    pub fn beside(target: &Path, suffix: &str) -> Self {
        TempFile {
            path: temp_path_for(target, suffix),
            armed: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Move the scratch file onto `dest`, which must not already exist.
    pub fn rename_onto(mut self, dest: &Path) -> Result<()> {
        fs::rename(&self.path, dest)
            .map_err(|e| VaultError::io("moving a temporary file into place", dest, e))?;
        self.armed = false;
        Ok(())
    }

    /// Atomically replace `dest` with the scratch file.
    pub fn replace_onto(mut self, dest: &Path) -> Result<()> {
        atomic_replace(&self.path, dest)?;
        self.armed = false;
        Ok(())
    }

    /// Keep the replacement write-protected across the rename and all subsequent checks.
    pub fn replace_locked(mut self, dest: &Path) -> Result<std::fs::File> {
        let guard = replace_locked(&self.path, dest)?;
        self.armed = false;
        Ok(guard)
    }

    /// Move onto `dest` whether or not it exists, preferring the atomic path when it does.
    pub fn commit_onto(self, dest: &Path) -> Result<()> {
        if dest.exists() {
            self.replace_onto(dest)
        } else {
            self.rename_onto(dest)
        }
    }

    /// Delete the scratch file now instead of at end of scope.
    pub fn discard(mut self) {
        self.armed = false;
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Temporary files left behind by a process that died mid-operation. `doctor` surfaces these so
/// they never accumulate silently.
pub fn stale_temp_files(dir: &Path, stem: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(stem) && n.ends_with(TEMP_SUFFIX))
        })
        .collect();
    found.sort();
    found
}
