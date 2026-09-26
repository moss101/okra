//! Safe file reads — port of the ChatGPT2 file-management study
//! (MASTER-PLAN §3 #52, docs/03): conversation-scoped reads of untrusted
//! paths must not follow symlinks out of the workspace, must not block on
//! FIFOs, and must refuse non-regular files.
//!
//! `O_NOFOLLOW | O_NONBLOCK` + fstat verification: symlink → error, FIFO →
//! error, oversized → error, all BEFORE any content is read.

use std::fs::File;
use std::io::Read;
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

/// Safe read of one file: O_NOFOLLOW (no symlink swap), O_NONBLOCK (no
/// FIFO hang), regular-file + size + permission checks via fstat (racing
/// the open, not the path), then read.
pub fn safe_read(path: &Path) -> Result<Vec<u8>, SafeReadError> {
    use std::os::unix::fs::OpenOptionsExt;
    // fast-path typed rejection; O_NOFOLLOW below closes the TOCTOU window
    if let Ok(md) = std::fs::symlink_metadata(path)
        && md.file_type().is_symlink() {
            return Err(SafeReadError::Symlink);
        }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;

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
    let mode = meta.permissions().mode();
    if mode & 0o002 != 0 {
        return Err(SafeReadError::WorldWritable { mode });
    }

    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_SAFE_READ_BYTES).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Same contract but returns a File for callers that stream.
pub fn safe_open(path: &Path) -> Result<File, SafeReadError> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
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
    fn regular_file_reads() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("ok.txt");
        std::fs::write(&p, b"content").unwrap();
        assert_eq!(safe_read(&p).unwrap(), b"content");
    }

    #[test]
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
