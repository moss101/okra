//! Remote sync write access (MASTER-PLAN §3 #48, from ZCode
//! `remote-sync/remoteSyncWriteAccess.ts`): the pre-flight check every
//! sync domain (skills / settings / plugins / MCP) runs before it
//! attempts to write a remote or local target directory.
//!
//! Donor contract kept: the probe creates the directory tree if missing,
//! writes a UNIQUE marker file with exclusive-create (`wx` — never
//! clobber an existing file), deletes it, and reports ok with the path —
//! or not-ok with the underlying error message. A marker left behind by
//! a crashed probe is cleaned best-effort; uniqueness comes from pid +
//! monotonic serial, so concurrent probes never fight over one name.
//!
//! The donor returns a plain `{ok, path, error?}` object rather than a
//! throwing error: a non-writable target is a *sync outcome* (the sync
//! refuses and reports), not a panic.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteAccessResult {
    pub path: PathBuf,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WriteAccessResult {
    pub fn ok(path: &Path) -> Self {
        WriteAccessResult {
            path: path.to_path_buf(),
            ok: true,
            error: None,
        }
    }

    pub fn failed(path: &Path, error: impl Into<String>) -> Self {
        WriteAccessResult {
            path: path.to_path_buf(),
            ok: false,
            error: Some(error.into()),
        }
    }
}

fn next_marker(path: &Path) -> PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    path.join(format!(
        ".okra-sync-preflight-{}-{}",
        std::process::id(),
        serial
    ))
}

/// `checkRemoteSyncDirectoryWriteAccess`: exclusive-create probe.
pub fn check_directory_write_access(directory: &Path) -> WriteAccessResult {
    let marker = next_marker(directory);
    match probe(directory, &marker) {
        Ok(()) => WriteAccessResult::ok(directory),
        Err(e) => {
            // best-effort cleanup of a half-written marker
            let _ = std::fs::remove_file(&marker);
            WriteAccessResult::failed(directory, e.to_string())
        }
    }
}

fn probe(directory: &Path, marker: &Path) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(directory)?;
    // exclusive create ("wx" in the donor): if a file already exists at
    // the marker name, that is a real conflict, not success
    let mut options = std::fs::File::options();
    options.write(true).create_new(true);
    let mut file = options.open(marker)?;
    use std::io::Write as _;
    file.write_all(b"ok")?;
    file.flush()?;
    std::fs::remove_file(marker)?;
    Ok(())
}

/// `checkRemoteSyncDirectoriesWriteAccess`: all directories must pass;
/// the first failure wins and carries its path.
pub fn check_directories_write_access(directories: &[PathBuf]) -> WriteAccessResult {
    for directory in directories {
        let result = check_directory_write_access(directory);
        if !result.ok {
            return result;
        }
    }
    WriteAccessResult::ok(
        &directories
            .iter()
            .fold(PathBuf::new(), |acc, p| {
                if acc.as_os_str().is_empty() {
                    p.clone()
                } else {
                    let mut joined = acc.into_os_string();
                    joined.push(", ");
                    joined.push(p.as_os_str());
                    PathBuf::from(joined)
                }
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_directory_passes_and_leaves_no_marker() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("skills");
        let result = check_directory_write_access(&dir);
        assert!(result.ok, "{result:?}");
        assert!(dir.is_dir(), "missing tree was created");
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(entries.is_empty(), "probe marker cleaned up: {entries:?}");
    }

    #[test]
    fn read_only_directory_fails_with_error_message() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("readonly");
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            let result = check_directory_write_access(&dir);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(!result.ok, "{result:?}");
            assert!(result.error.is_some());
        }
    }

    #[test]
    fn file_in_place_of_directory_fails() {
        let td = tempfile::tempdir().unwrap();
        let blocker = td.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let result = check_directory_write_access(&blocker);
        assert!(!result.ok, "a file cannot become a sync target dir");
        // and the blocker file was not destroyed by the probe
        assert!(blocker.is_file());
    }

    #[test]
    #[cfg(unix)] // readonly-dir refusal is a unix mode-bit contract
    fn multi_directory_check_reports_the_first_failure() {
        let td = tempfile::tempdir().unwrap();
        let good = td.path().join("good");
        let readonly = td.path().join("readonly");
        std::fs::create_dir_all(&readonly).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let all = vec![good.clone(), readonly.clone()];
        let result = check_directories_write_access(&all);
        #[cfg(unix)]
        assert!(!result.ok);
        assert_eq!(result.path, readonly, "the failing directory is named");
        let _ = &good;
    }
}
