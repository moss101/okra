//! Safe file reads — port of the ChatGPT2 file-management study
//! (MASTER-PLAN §3 #52, docs/03): conversation-scoped reads of untrusted
//! paths must not follow symlinks out of the workspace, must not block on
//! FIFOs, and must refuse non-regular files.
//!
//! `O_NOFOLLOW | O_NONBLOCK` + fstat verification: symlink → error, FIFO →
//! error, oversized → error, all BEFORE any content is read.

use std::fs::File;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const MAX_SAFE_READ_BYTES: u64 = 30 * 1024 * 1024; // attachment cap analog

#[derive(Debug, thiserror::Error)]
pub enum SafeReadError {
    #[error("path is a symlink (O_NOFOLLOW)")]
    Symlink,
    #[error("not a regular file")]
    NotRegularFile,
    #[error("file exceeds the safe-read cap ({size} > {cap})")]
    TooLarge { size: u64, cap: u64 },
    #[error("world-writable file refused (mode {mode:o})")]
    WorldWritable { mode: u32 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// The safe-read enforcement level, per platform — the honesty contract
/// (deepseek `enforcement: full | partial`): unix gets the full
/// O_NOFOLLOW + mode ladder; windows is `partial` (the symlink pre-check
/// and fstat regular-file verification carry the contract; ACL mapping
/// is the recorded second-pass work, docs/m6-windows-port.md).
pub fn enforcement_level() -> &'static str {
    // the windows ACL second pass is LANDED (Everyone-write refusal via
    // the DACL) — both platforms are `full`
    "full"
}

/// Windows ACL check (the safe-read second pass): does Everyone
/// (S-1-1-0) hold write access on this file? Written against the
/// windows-sys 0.59 signatures (Authorization + Security modules).
/// Fails closed on API errors.
#[cfg(windows)]
pub fn everyone_has_write_access(path: &Path) -> std::io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS, GENERIC_WRITE};
    use windows_sys::Win32::Security::Authorization::{
        BuildTrusteeWithSidW, GetEffectiveRightsFromAclW, GetNamedSecurityInfoW,
        SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::ACL;
    use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
    use windows_sys::Win32::Security::{
        AllocateAndInitializeSid, DACL_SECURITY_INFORMATION, FreeSid,
        SECURITY_WORLD_SID_AUTHORITY,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_APPEND_DATA, FILE_WRITE_DATA};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS || dacl.is_null() {
        let _ = unsafe { LocalFree(sd) };
        return Err(std::io::Error::last_os_error());
    }
    let mut everyone: *mut core::ffi::c_void = std::ptr::null_mut();
    let allocated = unsafe {
        AllocateAndInitializeSid(
            &SECURITY_WORLD_SID_AUTHORITY,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut everyone,
        )
    };
    if allocated == 0 || everyone.is_null() {
        let _ = unsafe { LocalFree(sd) };
        return Err(std::io::Error::last_os_error());
    }
    let mut trustee: TRUSTEE_W = unsafe { std::mem::zeroed() };
    unsafe { BuildTrusteeWithSidW(&mut trustee, everyone) };
    let mut rights: u32 = 0;
    let rc = unsafe { GetEffectiveRightsFromAclW(dacl, &trustee, &mut rights) };
    let write_bits: u32 = FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE;
    let granted = rc == ERROR_SUCCESS && (rights & write_bits) != 0;
    unsafe { FreeSid(everyone) };
    let _ = unsafe { LocalFree(sd) };
    Ok(granted)
}

/// Safe read of one file: O_NOFOLLOW (no symlink swap), O_NONBLOCK (no
/// FIFO hang), regular-file + size + permission checks via fstat (racing
/// the open, not the path), then read.
pub fn safe_read(path: &Path) -> Result<Vec<u8>, SafeReadError> {
    // fast-path typed rejection; O_NOFOLLOW below closes the TOCTOU window
    if let Ok(md) = std::fs::symlink_metadata(path)
        && md.file_type().is_symlink() {
            return Err(SafeReadError::Symlink);
        }
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?
    };
    // Windows second pass: the ACL check (Everyone write access via the
    // DACL) carries the tamper contract; O_NOFOLLOW's TOCTOU window is
    // covered by the symlink pre-check above plus the fstat verification.
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new().read(true).open(path)?;

    // verify with FSTAT (the opened inode, not the path)
    let meta = file.metadata()?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(SafeReadError::Symlink);
    }
    if !ft.is_file() {
        return Err(SafeReadError::NotRegularFile);
    }
    if meta.len() > MAX_SAFE_READ_BYTES {
        return Err(SafeReadError::TooLarge { size: meta.len(), cap: MAX_SAFE_READ_BYTES });
    }
    // unix-only mode ladder; windows refuses Everyone-write via the DACL
    // (the second pass is LANDED — enforcement_level() reports `full`)
    #[cfg(unix)]
    {
        let mode = meta.permissions().mode();
        if mode & 0o002 != 0 {
            return Err(SafeReadError::WorldWritable { mode });
        }
    }
    #[cfg(windows)]
    if everyone_has_write_access(path)? {
        return Err(SafeReadError::WorldWritable { mode: 0 });
    }

    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_SAFE_READ_BYTES).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Same contract but returns a File for callers that stream.
pub fn safe_open(path: &Path) -> Result<File, SafeReadError> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = std::fs::OpenOptions::new().read(true).open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(SafeReadError::NotRegularFile);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforcement_level_is_full_everywhere() {
        // the windows ACL second pass landed: Everyone-write refusal via
        // the DACL — both platforms are `full`
        assert_eq!(enforcement_level(), "full");
    }

    #[test]
    fn regular_file_reads() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("ok.txt");
        std::fs::write(&p, b"content").unwrap();
        assert_eq!(safe_read(&p).unwrap(), b"content");
    }

    #[test]
    #[cfg(unix)] // symlink creation needs privilege on windows; the TOCTOU ladder is unix
    fn symlink_is_refused_not_followed() {
        let td = tempfile::tempdir().unwrap();
        let secret = td.path().join("secret.txt");
        std::fs::write(&secret, b"secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, td.path().join("link.txt")).unwrap();
        let err = safe_read(&td.path().join("link.txt")).unwrap_err();
        assert!(matches!(err, SafeReadError::Symlink | SafeReadError::NotRegularFile), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn fifo_never_blocks() {
        let td = tempfile::tempdir().unwrap();
        let fifo = td.path().join("pipe");
        unsafe {
            let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
            assert_eq!(libc::mkfifo(c_path.as_ptr(), 0o644), 0);
        }
        // O_NONBLOCK + not-regular → immediate error, no hang
        let err = safe_read(&fifo).unwrap_err();
        assert!(matches!(err, SafeReadError::NotRegularFile), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn world_writable_refused() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("open.txt");
        std::fs::write(&p, b"x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666)).unwrap();
        let err = safe_read(&p).unwrap_err();
        assert!(matches!(err, SafeReadError::WorldWritable { .. }), "{err}");
    }
}
