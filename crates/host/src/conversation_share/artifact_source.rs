//! Artifact source (MASTER-PLAN §3 #48, from ZCode
//! `conversationShareArtifactSource.ts`): the seam between the share
//! pipeline and wherever the artifact bytes live — a workspace path
//! locally, or a remote host's file service over SSH/WSL/Docker.
//!
//! Donor contracts kept:
//! - **refs are plain paths**: any `scheme://` reference is rejected
//!   (`unsafe_structure`) — artifact references must travel as formal
//!   artifact rows, never as inline URLs;
//! - **containment**: the ref (absolute or workspace-relative) is
//!   realpath'd and must stay inside the workspace realpath — a symlink
//!   escaping the workspace is refused;
//! - **regular files only**, bounded by `max_bytes` (`limit_exceeded`);
//! - **remote staging is stability-checked**: stat → chunked range reads
//!   → stat again; a size/mtime change mid-read is
//!   `changed_during_staging`, a short chunk is `ended_during_staging`,
//!   an oversized chunk (the file grew) likewise;
//! - **skippable degradation**: missing file / not-a-file errors are
//!   marked skippable so the discovery pass can drop that artifact with a
//!   warning instead of killing the whole publish; transport failures are
//!   NOT skippable — a publish must never succeed while silently losing
//!   an artifact;
//! - error payloads carry the reason, never the path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::fsutil;

/// The read request (donor `ConversationShareArtifactReadInput`).
#[derive(Debug, Clone)]
pub struct ArtifactRead<'a> {
    pub workspace: &'a Path,
    pub reference: &'a str,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedArtifact {
    pub bytes: Vec<u8>,
    pub canonical_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactStat {
    pub canonical_path: PathBuf,
    pub size: u64,
    pub mtime_ms: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactSourceError {
    #[error("conversation artifact source must be a workspace path")]
    SchemeRef,
    #[error("conversation artifact is outside the workspace")]
    OutsideWorkspace,
    #[error("conversation artifact exceeds the byte limit")]
    LimitExceeded,
    #[error("conversation artifact source is not a readable file")]
    NotAFile,
    #[error("conversation artifact cannot be read")]
    NotFound,
    #[error("conversation artifact ended during staging")]
    EndedDuringStaging,
    #[error("conversation artifact changed during staging")]
    ChangedDuringStaging,
    #[error("conversation artifact byte limit is invalid")]
    InvalidLimit,
    #[error("conversation artifact transport failed")]
    Transport,
}

impl ArtifactSourceError {
    /// `isSkippableArtifactReadError`: only "the file is gone / not a
    /// file" degrades to a non-blocking warning. Everything else —
    /// transport failures, permission errors, contract violations —
    /// propagates.
    pub fn is_skippable(&self) -> bool {
        matches!(
            self,
            ArtifactSourceError::NotFound | ArtifactSourceError::NotAFile
        )
    }
}

/// donor regex `^[a-zA-Z][a-zA-Z\d+.-]*:\/\//`
fn has_scheme(reference: &str) -> bool {
    let Some(pos) = reference.find("://") else {
        return false;
    };
    let scheme = &reference[..pos];
    let mut chars = scheme.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

fn io_to_source_error(e: &std::io::Error) -> ArtifactSourceError {
    match e.kind() {
        std::io::ErrorKind::NotFound => ArtifactSourceError::NotFound,
        _ => ArtifactSourceError::Transport,
    }
}

/// Where artifact bytes come from.
pub trait ArtifactSource: Send + Sync {
    fn stat(&self, input: &ArtifactRead<'_>) -> Result<ArtifactStat, ArtifactSourceError>;
    fn read(&self, input: &ArtifactRead<'_>) -> Result<MaterializedArtifact, ArtifactSourceError>;
}

/// Resolve a ref against the workspace and verify containment in the
/// realpath'd workspace (symlink escapes are refused).
fn resolve_contained(
    workspace: &Path,
    reference: &str,
    resolve: impl Fn(&Path) -> Result<PathBuf, ArtifactSourceError>,
) -> Result<(PathBuf, PathBuf), ArtifactSourceError> {
    let workspace_real = resolve(workspace)?;
    let requested = if reference.starts_with('/') {
        PathBuf::from(reference)
    } else {
        workspace.join(reference)
    };
    let canonical = resolve(&requested)?;
    if !canonical.starts_with(&workspace_real) {
        return Err(ArtifactSourceError::OutsideWorkspace);
    }
    Ok((workspace_real, canonical))
}

/// The local workspace source (`createLocalConversationShareArtifactSource`).
#[derive(Debug, Clone, Default)]
pub struct LocalArtifactSource;

impl ArtifactSource for LocalArtifactSource {
    fn stat(&self, input: &ArtifactRead<'_>) -> Result<ArtifactStat, ArtifactSourceError> {
        if has_scheme(input.reference) {
            return Err(ArtifactSourceError::SchemeRef);
        }
        let (_ws, canonical) = resolve_contained(input.workspace, input.reference, |p| {
            fsutil::canonicalize(p).map_err(|e| io_to_source_error(&e))
        })?;
        let meta = std::fs::metadata(&canonical).map_err(|e| io_to_source_error(&e))?;
        if !meta.is_file() {
            return Err(ArtifactSourceError::NotAFile);
        }
        Ok(ArtifactStat {
            canonical_path: canonical,
            size: meta.len(),
            mtime_ms: meta.modified().ok().and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_millis() as u64)
            }),
        })
    }

    fn read(&self, input: &ArtifactRead<'_>) -> Result<MaterializedArtifact, ArtifactSourceError> {
        let stat = self.stat(input)?;
        if stat.size > input.max_bytes {
            return Err(ArtifactSourceError::LimitExceeded);
        }
        let bytes =
            std::fs::read(&stat.canonical_path).map_err(|e| io_to_source_error(&e))?;
        Ok(MaterializedArtifact {
            bytes,
            canonical_path: stat.canonical_path,
        })
    }
}

/// The remote file-service seam (the `IFileService` surface the share
/// pipeline uses: realpath/stat/chunked range reads). Errors carry a
/// code-ish message; "ENOENT"/"ENOTDIR" degrade to skippable.
pub trait RemoteFileService: Send + Sync {
    fn resolve_path(&self, path: &Path) -> Result<PathBuf, String>;
    /// (size, mtime_ms)
    fn stat(&self, path: &Path) -> Result<(u64, Option<u64>), String>;
    fn read_range(&self, path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, String>;
}

const REMOTE_READ_CHUNK_BYTES: u64 = 512 * 1024;

/// The remote source (`createRemoteConversationShareArtifactSource`):
/// chunked range reads bracketed by stats so a concurrently changing
/// artifact is detected instead of half-uploaded.
#[derive(Clone)]
pub struct RemoteArtifactSource {
    service: Arc<dyn RemoteFileService>,
}

impl RemoteArtifactSource {
    pub fn new(service: Arc<dyn RemoteFileService>) -> Self {
        RemoteArtifactSource { service }
    }

    fn map_transport(&self, message: String) -> ArtifactSourceError {
        if message.contains("ENOENT") || message.contains("ENOTDIR") || message.contains("EISDIR")
        {
            ArtifactSourceError::NotFound
        } else {
            ArtifactSourceError::Transport
        }
    }

    fn stat_via_service(
        &self,
        input: &ArtifactRead<'_>,
    ) -> Result<ArtifactStat, ArtifactSourceError> {
        if has_scheme(input.reference) {
            return Err(ArtifactSourceError::SchemeRef);
        }
        let svc = &*self.service;
        let (_ws, canonical) = resolve_contained(input.workspace, input.reference, |p| {
            svc.resolve_path(p).map_err(|m| self.map_transport(m))
        })?;
        let (size, mtime_ms) = svc.stat(&canonical).map_err(|m| self.map_transport(m))?;
        if size == 0 && mtime_ms.is_none() {
            // the fake/local contract for "not a file": (0, None)
            return Err(ArtifactSourceError::NotAFile);
        }
        Ok(ArtifactStat {
            canonical_path: canonical,
            size,
            mtime_ms,
        })
    }
}

impl ArtifactSource for RemoteArtifactSource {
    fn stat(&self, input: &ArtifactRead<'_>) -> Result<ArtifactStat, ArtifactSourceError> {
        self.stat_via_service(input)
    }

    fn read(&self, input: &ArtifactRead<'_>) -> Result<MaterializedArtifact, ArtifactSourceError> {
        if input.max_bytes == 0 {
            return Err(ArtifactSourceError::InvalidLimit);
        }
        let stat = self.stat_via_service(input)?;
        if stat.size > input.max_bytes {
            return Err(ArtifactSourceError::LimitExceeded);
        }
        let svc = &*self.service;
        let mut bytes = Vec::with_capacity(stat.size as usize);
        let mut offset = 0u64;
        while offset < stat.size {
            let length = REMOTE_READ_CHUNK_BYTES.min(stat.size - offset);
            let chunk = svc
                .read_range(&stat.canonical_path, offset, length)
                .map_err(|m| self.map_transport(m))?;
            if chunk.is_empty() {
                return Err(ArtifactSourceError::EndedDuringStaging);
            }
            if offset + chunk.len() as u64 > stat.size {
                return Err(ArtifactSourceError::ChangedDuringStaging);
            }
            bytes.extend_from_slice(&chunk);
            offset += chunk.len() as u64;
        }
        let after = self.stat_via_service(input)?;
        if after.size != stat.size || after.mtime_ms != stat.mtime_ms {
            return Err(ArtifactSourceError::ChangedDuringStaging);
        }
        Ok(MaterializedArtifact {
            bytes,
            canonical_path: stat.canonical_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_refs_are_rejected_everywhere() {
        let td = tempfile::tempdir().unwrap();
        let local = LocalArtifactSource;
        let input = ArtifactRead {
            workspace: td.path(),
            reference: "file:///etc/passwd",
            max_bytes: 1024,
        };
        let e = local.read(&input).unwrap_err();
        assert!(matches!(e, ArtifactSourceError::SchemeRef));
        assert!(!e.is_skippable(), "an inline URL is a contract violation");

        // the local detector distinguishes real schemes from plain paths
        assert!(has_scheme("okra-artifact://x"));
        assert!(has_scheme("https://x"));
        assert!(!has_scheme("plain.md"));
        assert!(!has_scheme("a/b:note")); // colon without //
    }

    #[test]
    #[cfg_attr(not(unix), allow(unused_variables))]
    fn local_containment_rejects_symlink_escape() {
        let td = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"no").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret"), td.path().join("link"))
            .unwrap();

        let source = LocalArtifactSource;
        #[cfg(unix)]
        {
            let e = source
                .read(&ArtifactRead {
                    workspace: td.path(),
                    reference: "link",
                    max_bytes: 1024,
                })
                .unwrap_err();
            assert!(matches!(e, ArtifactSourceError::OutsideWorkspace));
            assert!(!e.is_skippable());
        }
        let _ = outside;
    }

    #[test]
    fn local_reads_files_and_enforces_caps() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("a.md"), b"# artifact\n").unwrap();
        let source = LocalArtifactSource;
        let read = source
            .read(&ArtifactRead {
                workspace: td.path(),
                reference: "a.md",
                max_bytes: 1024,
            })
            .unwrap();
        assert_eq!(read.bytes, b"# artifact\n");
        assert!(read.canonical_path.ends_with("a.md"));
        let stat = source
            .stat(&ArtifactRead {
                workspace: td.path(),
                reference: "a.md",
                max_bytes: 1024,
            })
            .unwrap();
        assert_eq!(stat.size, 11);
        assert!(stat.mtime_ms.is_some());

        let e = source
            .read(&ArtifactRead {
                workspace: td.path(),
                reference: "a.md",
                max_bytes: 4,
            })
            .unwrap_err();
        assert!(matches!(e, ArtifactSourceError::LimitExceeded));
        assert!(!e.is_skippable());
    }

    #[test]
    fn missing_and_non_file_are_skippable() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir(td.path().join("a-directory")).unwrap();
        let source = LocalArtifactSource;
        let missing = source
            .read(&ArtifactRead {
                workspace: td.path(),
                reference: "nope.md",
                max_bytes: 100,
            })
            .unwrap_err();
        assert!(matches!(missing, ArtifactSourceError::NotFound));
        assert!(missing.is_skippable());
        let not_a_file = source
            .read(&ArtifactRead {
                workspace: td.path(),
                reference: "a-directory",
                max_bytes: 100,
            })
            .unwrap_err();
        assert!(matches!(not_a_file, ArtifactSourceError::NotAFile));
        assert!(not_a_file.is_skippable());
    }

    /// Fake remote FS serving one file with configurable chunking.
    struct FakeRemote {
        files: std::sync::Mutex<std::collections::BTreeMap<PathBuf, Vec<u8>>>,
    }

    impl FakeRemote {
        fn with_file(path: PathBuf, bytes: Vec<u8>) -> Self {
            let mut files = std::collections::BTreeMap::new();
            files.insert(path, bytes);
            FakeRemote {
                files: std::sync::Mutex::new(files),
            }
        }
    }

    impl RemoteFileService for FakeRemote {
        fn resolve_path(&self, path: &Path) -> Result<PathBuf, String> {
            Ok(path.to_path_buf())
        }
        fn stat(&self, path: &Path) -> Result<(u64, Option<u64>), String> {
            let map = self.files.lock().unwrap();
            match map.get(path) {
                Some(bytes) => Ok((bytes.len() as u64, Some(1_000))),
                None => Err("ENOENT".into()),
            }
        }
        fn read_range(&self, path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, String> {
            let map = self.files.lock().unwrap();
            let bytes = map.get(path).ok_or_else(|| "ENOENT".to_string())?;
            if offset >= bytes.len() as u64 {
                return Ok(Vec::new());
            }
            let end = (offset + length).min(bytes.len() as u64);
            Ok(bytes[offset as usize..end as usize].to_vec())
        }
    }

    #[test]
    fn remote_chunked_reads_assemble_the_file() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("artifact.bin");
        let content: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();
        let source = RemoteArtifactSource::new(Arc::new(FakeRemote::with_file(
            path.clone(),
            content.clone(),
        )));
        let read = source
            .read(&ArtifactRead {
                workspace: td.path(),
                reference: "artifact.bin",
                max_bytes: 1 << 20,
            })
            .unwrap();
        assert_eq!(read.bytes, content);
        assert_eq!(read.canonical_path, path);
    }

    /// stat says 100 bytes but the second read returns nothing: the
    /// artifact was truncated mid-staging.
    struct TruncatingService;
    impl RemoteFileService for TruncatingService {
        fn resolve_path(&self, path: &Path) -> Result<PathBuf, String> {
            Ok(path.to_path_buf())
        }
        fn stat(&self, _path: &Path) -> Result<(u64, Option<u64>), String> {
            Ok((100, Some(1)))
        }
        fn read_range(&self, _path: &Path, offset: u64, _length: u64) -> Result<Vec<u8>, String> {
            if offset == 0 {
                Ok(vec![0u8; 50])
            } else {
                Ok(Vec::new())
            }
        }
    }

    /// stat says 100 bytes but reads return more: the artifact grew.
    struct GrowingService;
    impl RemoteFileService for GrowingService {
        fn resolve_path(&self, path: &Path) -> Result<PathBuf, String> {
            Ok(path.to_path_buf())
        }
        fn stat(&self, _path: &Path) -> Result<(u64, Option<u64>), String> {
            Ok((100, Some(1)))
        }
        fn read_range(&self, _path: &Path, _offset: u64, _length: u64) -> Result<Vec<u8>, String> {
            Ok(vec![0u8; 512 * 1024 + 1])
        }
    }

    /// The artifact shrinks between the bracketing stats: mtime/size
    /// mismatch → changed during staging.
    struct ShrinkingStatService;
    impl RemoteFileService for ShrinkingStatService {
        fn resolve_path(&self, path: &Path) -> Result<PathBuf, String> {
            Ok(path.to_path_buf())
        }
        fn stat(&self, _path: &Path) -> Result<(u64, Option<u64>), String> {
            // first call (before) reports 100; a second call reports 40
            use std::sync::atomic::{AtomicU32, Ordering};
            static CALLS: AtomicU32 = AtomicU32::new(0);
            if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok((100, Some(1)))
            } else {
                Ok((40, Some(2)))
            }
        }
        fn read_range(&self, _path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, String> {
            if offset == 0 {
                Ok(vec![0u8; length as usize])
            } else {
                Ok(Vec::new())
            }
        }
    }

    #[test]
    fn remote_staging_detects_concurrent_changes() {
        let td = tempfile::tempdir().unwrap();
        let input = ArtifactRead {
            workspace: td.path(),
            reference: "a.bin",
            max_bytes: 1 << 20,
        };

        let e = RemoteArtifactSource::new(Arc::new(TruncatingService))
            .read(&input)
            .unwrap_err();
        assert!(matches!(e, ArtifactSourceError::EndedDuringStaging));
        assert!(!e.is_skippable());

        let e = RemoteArtifactSource::new(Arc::new(GrowingService))
            .read(&input)
            .unwrap_err();
        assert!(matches!(e, ArtifactSourceError::ChangedDuringStaging));

        let e = RemoteArtifactSource::new(Arc::new(ShrinkingStatService))
            .read(&input)
            .unwrap_err();
        assert!(matches!(e, ArtifactSourceError::ChangedDuringStaging));
    }

    #[test]
    fn invalid_limit_is_a_contract_error() {
        let td = tempfile::tempdir().unwrap();
        let source = RemoteArtifactSource::new(Arc::new(FakeRemote::with_file(
            td.path().join("a"),
            b"x".to_vec(),
        )));
        let e = source
            .read(&ArtifactRead {
                workspace: td.path(),
                reference: "a",
                max_bytes: 0,
            })
            .unwrap_err();
        assert!(matches!(e, ArtifactSourceError::InvalidLimit));
    }
}
