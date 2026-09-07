use crate::error::{Result, VaultError};
#[cfg(not(windows))]
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
#[cfg(windows)]
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    file: u64,
}

#[cfg(unix)]
pub fn file_identity(file: &File) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        device: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
pub fn file_identity(file: &File) -> Result<FileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: `info` points to writable storage for exactly the structure requested and the raw
    // handle comes from a live `File` retained for the duration of the call.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) };
    if ok == 0 {
        return Err(VaultError::io(
            "reading transcript file identity",
            Path::new("<open transcript>"),
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: a successful GetFileInformationByHandle initialized the structure.
    let info = unsafe { info.assume_init() };
    Ok(FileIdentity {
        device: info.dwVolumeSerialNumber as u64,
        file: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    })
}

pub fn path_file_identity(path: &Path) -> Result<FileIdentity> {
    let file =
        File::open(path).map_err(|e| VaultError::io("opening transcript identity", path, e))?;
    file_identity(&file)
}

/// Create sensitive output privately from the first byte, independently of the user's umask.
pub fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// WSL's Windows-drive mounts use 9p. A locked replacement there can succeed but fail when
/// reopened for verification, so mutation is refused before any replacement is prepared.
#[cfg(target_os = "linux")]
pub fn ensure_supported_mutation_filesystem(path: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let native =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| VaultError::InvalidInput {
            reason: "The session path contains a null byte.".into(),
        })?;
    let mut info = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the path is null-terminated and `info` is writable storage of the exact statfs type.
    if unsafe { libc::statfs(native.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(VaultError::io(
            "checking the session filesystem",
            path,
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: a successful statfs call initialized `info`.
    let info = unsafe { info.assume_init() };
    // V9FS_MAGIC from Linux's <linux/magic.h>; includes WSL Windows-drive mounts.
    if info.f_type == 0x0102_1997 {
        return Err(VaultError::InvalidInput {
            reason: "Linux compaction and restoration are not supported on 9p/DrvFS mounts (including Windows drives in WSL). Use Codex Vault for Windows for Windows files, or work on a copy in the Linux filesystem.".into(),
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn ensure_supported_mutation_filesystem(_path: &Path) -> Result<()> {
    Ok(())
}

/// Replace a transcript and retain a file guard across post-replacement verification.
pub(super) fn replace_locked(temp_path: &Path, dest: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let original = fs::metadata(dest)?;
        let replacement = fs::metadata(temp_path)?;
        if original.uid() != replacement.uid() || original.gid() != replacement.gid() {
            std::os::unix::fs::chown(temp_path, Some(original.uid()), Some(original.gid()))?;
        }
        fs::set_permissions(temp_path, original.permissions())?;
    }

    #[cfg(windows)]
    {
        preserve_windows_dacl(dest, temp_path)?;
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::GENERIC_READ;
        use windows_sys::Win32::Storage::FileSystem::{
            FileRenameInfoEx, SetFileInformationByHandle, DELETE, FILE_RENAME_INFO,
            FILE_SHARE_DELETE, FILE_SHARE_READ,
        };

        let guard = OpenOptions::new()
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(temp_path)
            .map_err(|e| VaultError::io("locking replacement for rename", temp_path, e))?;
        let absolute = std::path::absolute(dest)?;
        let target: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        let offset = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let bytes = std::mem::size_of::<FILE_RENAME_INFO>() + target.len() * 2;
        // u64 storage provides the alignment required by the HANDLE field on x64.
        let mut buffer = vec![0u64; bytes.div_ceil(8)];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: `info` is aligned writable storage large enough for FILE_RENAME_INFO and its
        // variable UTF-16 filename. `guard` and `target` remain alive for the complete call.
        let ok = unsafe {
            (*info).Anonymous.Flags = 0x1 | 0x2; // REPLACE_IF_EXISTS | POSIX_SEMANTICS
            (*info).FileNameLength = (target.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                target.as_ptr(),
                buffer.as_mut_ptr().cast::<u8>().add(offset).cast::<u16>(),
                target.len(),
            );
            SetFileInformationByHandle(
                guard.as_raw_handle(),
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
        Ok(guard)
    }

    #[cfg(not(windows))]
    {
        let guard = lock_session(temp_path)?;
        atomic_replace(temp_path, dest)?;
        Ok(guard)
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
    // SAFETY: `src` is a valid null-terminated UTF-16 path and the null buffer query is the API's
    // documented way to obtain the required security-descriptor size.
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
    // SAFETY: `storage` is aligned and sized according to the preceding query; src/dst remain
    // live null-terminated buffers and all out-pointers reference initialized writable values.
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

#[cfg(windows)]
pub fn lock_session(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    // Deny other writers at the OS sharing layer while still permitting readers and replacement.
    let share_mode = if cfg!(debug_assertions)
        && std::env::var_os("CODEX_VAULT_TEST_ALLOW_WRITER_RACES").is_some()
    {
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    } else {
        FILE_SHARE_READ | FILE_SHARE_DELETE
    };
    OpenOptions::new()
        .read(true)
        .share_mode(share_mode)
        .open(path)
        .map_err(|source| VaultError::SessionLocked {
            path: path.to_path_buf(),
            source,
        })
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
    // SAFETY: both path buffers are valid null-terminated UTF-16 strings and remain alive for the
    // call; optional backup/extension arguments are intentionally null.
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
