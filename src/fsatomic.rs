//! Atomic replacement, advisory locking and temp-file management.

use crate::error::{Result, VaultError};
use crate::util::now_epoch_millis;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
#[cfg(windows)]
use std::io;
use std::io::{BufRead, BufReader, BufWriter, Write as IoWrite};
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
    /// ReplaceFileW opens its replacement with no sharing, so it cannot preserve this guard.
    /// Rename through the held handle instead (Windows 10+ FileRenameInfoEx). POSIX rename
    /// semantics keep the source guard alive while the name starts referring to the new file.
    pub fn replace_locked(mut self, dest: &Path) -> Result<File> {
        #[cfg(windows)]
        let guard = {
            preserve_windows_dacl(dest, &self.path)?;
            use std::os::windows::ffi::OsStrExt;
            use std::os::windows::fs::OpenOptionsExt;
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::GENERIC_READ;
            use windows_sys::Win32::Storage::FileSystem::{
                FileRenameInfoEx, SetFileInformationByHandle, DELETE, FILE_RENAME_INFO,
                FILE_SHARE_DELETE, FILE_SHARE_READ,
            };
            let f = OpenOptions::new()
                .access_mode(GENERIC_READ | DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
                .open(&self.path)
                .map_err(|e| VaultError::io("locking replacement for rename", &self.path, e))?;
            let absolute = std::path::absolute(dest)?;
            let target: Vec<u16> = absolute.as_os_str().encode_wide().collect();
            let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
            let bytes = std::mem::size_of::<FILE_RENAME_INFO>() + target.len() * 2;
            // u64 storage provides the alignment required by the HANDLE field on x64.
            let mut buffer = vec![0u64; bytes.div_ceil(8)];
            let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            let ok = unsafe {
                (*info).Anonymous.Flags = 0x1 | 0x2; // REPLACE_IF_EXISTS | POSIX_SEMANTICS
                (*info).FileNameLength = (target.len() * 2) as u32;
                std::ptr::copy_nonoverlapping(
                    target.as_ptr(),
                    buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                    target.len(),
                );
                SetFileInformationByHandle(
                    f.as_raw_handle(),
                    FileRenameInfoEx,
                    info.cast(),
                    bytes as u32,
                )
            };
            if ok == 0 {
                return Err(VaultError::io(
                    "replacing the locked transcript",
                    dest,
                    io::Error::last_os_error(),
                ));
            }
            f
        };
        #[cfg(not(windows))]
        let guard = {
            let f = lock_session(&self.path)?;
            atomic_replace(&self.path, dest)?;
            f
        };
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

/// Handle-based renames retain the scratch file's security descriptor. Preserve the native
/// transcript's DACL first so a custom private ACL is never replaced by directory defaults.
#[cfg(windows)]
fn preserve_windows_dacl(source: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        GetFileSecurityW, GetSecurityDescriptorControl, SetFileSecurityW,
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    let src: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut needed = 0u32;
    unsafe {
        GetFileSecurityW(
            src.as_ptr(),
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        return Err(VaultError::io(
            "reading transcript permissions",
            source,
            io::Error::last_os_error(),
        ));
    }
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    let descriptor = storage.as_mut_ptr().cast();
    let mut control = 0u16;
    let mut revision = 0u32;
    unsafe {
        if GetFileSecurityW(
            src.as_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor,
            needed,
            &mut needed,
        ) == 0
            || GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0
        {
            return Err(VaultError::io(
                "reading transcript permissions",
                source,
                io::Error::last_os_error(),
            ));
        }
        let inheritance = if control & SE_DACL_PROTECTED != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        if SetFileSecurityW(
            dst.as_ptr(),
            DACL_SECURITY_INFORMATION | inheritance,
            descriptor,
        ) == 0
        {
            return Err(VaultError::io(
                "preserving transcript permissions",
                target,
                io::Error::last_os_error(),
            ));
        }
    }
    Ok(())
}

/// Stable lock files are deliberately not removed: unlinking one while another process has
/// opened it would allow two different inodes to act as the same lock. The OS releases locks
/// on exit, including a crash. The vault lock also serializes prune with journal/backup writes;
/// the path lock protects a transcript even when two processes use different vault homes.
pub struct MutationGuard {
    _vault: File,
    _session: File,
}

impl MutationGuard {
    pub fn acquire(vault: &Path, session: &Path) -> Result<Self> {
        fn acquire_file(path: &Path, session: &Path) -> Result<File> {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .map_err(|e| VaultError::io("opening operation lock", path, e))?;
            FileExt::try_lock_exclusive(&f).map_err(|source| VaultError::SessionLocked {
                path: session.to_path_buf(),
                source,
            })?;
            Ok(f)
        }
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

#[cfg(windows)]
pub fn lock_session(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};

    // Deny other writers at the OS sharing layer while still permitting readers and
    // ReplaceFileW. If Codex already has a write handle open, this open should fail
    // with a sharing violation before we touch the transcript.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)
        .map_err(|source| VaultError::SessionLocked {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(file)
}

#[cfg(not(windows))]
pub fn lock_session(path: &Path) -> Result<File> {
    let file = OpenOptions::new().read(true).open(path)?;
    file.try_lock_exclusive()
        .map_err(|source| VaultError::SessionLocked {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(file)
}

#[cfg(windows)]
pub fn atomic_replace(temp_path: &Path, dest_path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    const ERROR_FILE_NOT_FOUND: i32 = 2;
    let replaced = dest_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let replacement = temp_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) {
        return fs::rename(temp_path, dest_path)
            .map_err(|e| VaultError::io("renaming replacement into place", dest_path, e));
    }
    Err(VaultError::io(
        "atomically replacing the transcript",
        dest_path,
        error,
    ))
}

#[cfg(not(windows))]
pub fn atomic_replace(temp_path: &Path, dest_path: &Path) -> Result<()> {
    fs::rename(temp_path, dest_path)?;
    Ok(())
}

/// What one compaction pass observed, so no caller has to re-read either file to learn it.
#[derive(Debug)]
pub struct CompactionCopy {
    pub kept_lines: usize,
    pub removed_lines: usize,
    pub kept_bytes: u64,
    pub removed_bytes: u64,
    /// SHA-256 of the source as it was read during this pass.
    pub source_sha256: String,
    /// SHA-256 and length of the file just written.
    pub result_sha256: String,
    pub result_size: u64,
}

pub fn copy_compacted_transcript(
    src: &Path,
    dst: &Path,
    session_meta_index: usize,
    cutoff_index: usize,
) -> Result<CompactionCopy> {
    let mut source_hasher = Sha256::new();
    let mut result_hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(src)?);
    let mut output = BufWriter::new(File::create(dst)?);
    let mut line = String::new();
    let mut physical_index = 0usize;
    let mut kept_lines = 0usize;
    let mut removed_lines = 0usize;
    let mut kept_bytes = 0u64;
    let mut removed_bytes = 0u64;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        source_hasher.update(line.as_bytes());
        let keep = physical_index == session_meta_index || physical_index >= cutoff_index;
        if keep {
            output.write_all(line.as_bytes())?;
            result_hasher.update(line.as_bytes());
            kept_lines += 1;
            kept_bytes = kept_bytes.saturating_add(n as u64);
        } else {
            removed_lines += 1;
            removed_bytes = removed_bytes.saturating_add(n as u64);
        }
        physical_index += 1;
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    Ok(CompactionCopy {
        kept_lines,
        removed_lines,
        kept_bytes,
        removed_bytes,
        source_sha256: format!("{:x}", source_hasher.finalize()),
        result_sha256: format!("{:x}", result_hasher.finalize()),
        result_size: kept_bytes,
    })
}
