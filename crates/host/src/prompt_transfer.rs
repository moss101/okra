//! Prompt attachment transfer (MASTER-PLAN §3 #48, from ZCode
//! `prompt-attachment-transfer/`): moving the files a user attached to a
//! prompt from where they are to where the turn will consume them.
//!
//! Donor contracts kept:
//! - **local workspaces are zero-copy**: staging returns the local path
//!   as the ref with `staged: false` and never fabricates upload
//!   progress (no fake progress events for a stat away);
//! - **remote workspaces stage a real copy** into a staging area, with
//!   progress phases `uploading → complete` carrying
//!   `uploadedBytes/totalBytes`;
//! - **adopt** commits a staged file into the conversation's attachments
//!   (okra: the conversation-scoped AttachmentStore); **cancel/cleanup**
//!   remove staged copies without adopting — a canceled operation never
//!   leaves residue.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::files::{AttachmentKind, AttachmentOrigin, AttachmentStore};
use crate::fsutil;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageParams {
    pub operation_id: String,
    pub session_id: String,
    pub local_path: PathBuf,
    pub file_name: String,
    pub mime: String,
    /// Client-reported size; when absent, stat decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageResult {
    pub operation_id: String,
    /// Zero-copy local path (local mode) or staged copy path (remote).
    pub reference: PathBuf,
    pub bytes: u64,
    pub staged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    Uploading,
    Complete,
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub operation_id: String,
    pub phase: TransferPhase,
    pub uploaded_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("attachment source is not a readable file: {0}")]
    NotAFile(PathBuf),
    #[error("attachment io: {0}")]
    Io(#[from] std::io::Error),
}

/// The local mode (donor `createLocalPromptAttachmentTransferService`):
/// zero-copy — the ref IS the local path, `staged: false`, and no
/// progress events are emitted for a stat.
#[derive(Debug, Clone, Default)]
pub struct LocalTransfer;

impl LocalTransfer {
    pub fn stage(&self, params: &StageParams) -> Result<StageResult, TransferError> {
        let bytes = match params.size_bytes {
            Some(n) if n > 0 => n,
            _ => std::fs::metadata(&params.local_path).map(|m| m.len()).unwrap_or(0),
        };
        Ok(StageResult {
            operation_id: params.operation_id.clone(),
            reference: params.local_path.clone(),
            bytes,
            staged: false,
        })
    }
}

/// The remote mode: really copies the file into a per-operation staging
/// directory, reports chunk progress, and supports adopt/cancel/cleanup.
pub struct StagingTransfer {
    staging_root: PathBuf,
    chunk: u64,
}

impl StagingTransfer {
    pub fn new(staging_root: impl Into<PathBuf>) -> Self {
        StagingTransfer {
            staging_root: staging_root.into(),
            chunk: 256 * 1024,
        }
    }

    fn op_dir(&self, operation_id: &str) -> PathBuf {
        self.staging_root.join(operation_id)
    }

    /// Stage a copy. `progress` fires per chunk with
    /// `uploading` totals, then a final `complete` event.
    pub fn stage(
        &self,
        params: &StageParams,
        progress: &mut dyn FnMut(TransferProgress),
    ) -> Result<StageResult, TransferError> {
        let meta = std::fs::metadata(&params.local_path)?;
        if !meta.is_file() {
            return Err(TransferError::NotAFile(params.local_path.clone()));
        }
        let total = meta.len();
        let op_dir = self.op_dir(&params.operation_id);
        std::fs::create_dir_all(&op_dir)?;
        let staged_path = op_dir.join(&params.file_name);

        let mut source = std::fs::File::open(&params.local_path)?;
        let mut dest = std::fs::File::create(&staged_path)?;
        use std::io::{Read as _, Write as _};
        let mut uploaded = 0u64;
        let mut buf = vec![0u8; self.chunk as usize];
        loop {
            let n = source.read(&mut buf)?;
            if n == 0 {
                break;
            }
            dest.write_all(&buf[..n])?;
            uploaded += n as u64;
            progress(TransferProgress {
                operation_id: params.operation_id.clone(),
                phase: TransferPhase::Uploading,
                uploaded_bytes: uploaded,
                total_bytes: total,
            });
        }
        dest.flush()?;
        progress(TransferProgress {
            operation_id: params.operation_id.clone(),
            phase: TransferPhase::Complete,
            uploaded_bytes: uploaded,
            total_bytes: total,
        });

        Ok(StageResult {
            operation_id: params.operation_id.clone(),
            reference: staged_path,
            bytes: uploaded,
            staged: true,
        })
    }

    pub fn cleanup(&self, operation_id: &str) {
        let _ = std::fs::remove_dir_all(self.op_dir(operation_id));
    }
}

/// `adopt`: commit a staged (or zero-copy local) file into the
/// conversation's attachment store, returning the attachment id.
/// Origin `DragDrop` matches the donor's drag-and-drop entry path.
pub fn adopt(
    store: &mut AttachmentStore,
    session_id: &str,
    staged: &StageResult,
) -> String {
    let attachment = store.add(
        session_id,
        AttachmentKind::ContextFile {
            path: staged.reference.clone(),
            origin: AttachmentOrigin::DragDrop,
        },
    );
    attachment.id
}

/// `cancel`: remove the staged copy — a canceled transfer leaves nothing.
pub fn cancel(staging: &StagingTransfer, operation_id: &str) {
    staging.cleanup(operation_id);
}

/// Zero-copy refs must still be validated before the turn reads them:
/// the file must exist and be a regular file (same bar as safe-read).
pub fn validate_ref(reference: &Path) -> Result<(), TransferError> {
    match fsutil::canonicalize(reference) {
        Ok(p) => {
            let meta = std::fs::metadata(&p)?;
            if meta.is_file() {
                Ok(())
            } else {
                Err(TransferError::NotAFile(reference.to_path_buf()))
            }
        }
        Err(e) => Err(TransferError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(path: &Path, size: Option<u64>) -> StageParams {
        StageParams {
            operation_id: "op-1".into(),
            session_id: "conv-1".into(),
            local_path: path.to_path_buf(),
            file_name: "photo.png".into(),
            mime: "image/png".into(),
            size_bytes: size,
        }
    }

    #[test]
    fn local_mode_is_zero_copy() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("photo.png");
        std::fs::write(&file, b"png bytes").unwrap();

        let result = LocalTransfer.stage(&params(&file, None)).unwrap();
        assert_eq!(result.reference, file, "ref is the local path itself");
        assert!(!result.staged);
        assert_eq!(result.bytes, "png bytes".len() as u64);
        // declared size wins when present (no stat needed)
        let result = LocalTransfer.stage(&params(&file, Some(999))).unwrap();
        assert_eq!(result.bytes, 999);
        // missing file: zero bytes, staged false — the donor's stat-catch
        let result = LocalTransfer.stage(&params(&td.path().join("nope"), None)).unwrap();
        assert_eq!(result.bytes, 0);
        assert!(!result.staged);
    }

    #[test]
    fn remote_staging_copies_with_progress_and_adopt() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("photo.png");
        let payload = vec![7u8; 700_000]; // spans multiple 256 KiB chunks
        std::fs::write(&file, &payload).unwrap();

        let staging = StagingTransfer::new(td.path().join("staging"));
        let mut events = Vec::new();
        let staged = staging
            .stage(&params(&file, None), &mut |p| events.push(p))
            .unwrap();
        assert!(staged.staged);
        assert_eq!(staged.bytes, payload.len() as u64);
        assert!(staged.reference.exists());
        assert_eq!(std::fs::read(&staged.reference).unwrap(), payload);

        // progress: uploading events ascending to total, then complete
        assert!(events.len() >= 3, "{events:?}");
        assert_eq!(events.first().unwrap().phase, TransferPhase::Uploading);
        assert_eq!(events.last().unwrap().phase, TransferPhase::Complete);
        assert_eq!(events.last().unwrap().uploaded_bytes, payload.len() as u64);
        let ascending: Vec<u64> = events
            .iter()
            .filter(|e| e.phase == TransferPhase::Uploading)
            .map(|e| e.uploaded_bytes)
            .collect();
        assert!(
            ascending.windows(2).all(|w| w[0] < w[1]),
            "uploaded_bytes strictly ascend: {ascending:?}"
        );

        // adopt commits the staged file into the conversation attachments
        let mut attachments = AttachmentStore::new();
        let id = adopt(&mut attachments, "conv-1", &staged);
        let listed = attachments.list("conv-1");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        match &listed[0].kind {
            AttachmentKind::ContextFile { path, .. } => {
                assert!(path.ends_with("photo.png"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cancel_and_cleanup_remove_staged_copies() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("doc.pdf");
        std::fs::write(&file, b"pdf").unwrap();
        let staging = StagingTransfer::new(td.path().join("staging"));
        let mut noop = |_: TransferProgress| {};
        let staged = staging.stage(&params(&file, None), &mut noop).unwrap();

        cancel(&staging, "op-1");
        assert!(!staged.reference.exists(), "canceled staging leaves nothing");

        // cleanup is also safe when the operation dir is already gone
        staging.cleanup("op-1");
        validate_ref(&file).unwrap();
    }

    #[test]
    fn validate_ref_rejects_directories_and_missing() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir(td.path().join("dir")).unwrap();
        assert!(matches!(
            validate_ref(&td.path().join("dir")),
            Err(TransferError::NotAFile(_))
        ));
        assert!(validate_ref(&td.path().join("nope")).is_err());
    }
}
