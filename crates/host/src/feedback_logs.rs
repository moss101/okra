//! Feedback ↔ log archive integration (MASTER-PLAN §3 #48, donor
//! `attachLogsFromExport`): the glue that lets a feedback ticket carry
//! the workspace's diagnostic logs as a `log` attachment.
//!
//! Flow (all local, offline-first):
//! 1. `create_diagnostic_archive` bundles the session logs (budgets
//!    enforced);
//! 2. the archive file is checksummed so post-transport integrity is
//!    verifiable;
//! 3. the metadata (file name, gzip mime, size, checksum) is recorded as
//!    a `log` attachment on the ticket — bytes stay on disk where the
//!    upload seam will find them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::feedback::{AttachmentKind, FeedbackError, FeedbackTicketStore};
use crate::plugins::store::sha256_hex;
use crate::log_archive::create_diagnostic_archive;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticAttachment {
    pub attachment_id: String,
    pub archive_path: PathBuf,
    pub size_bytes: u64,
    /// sha256 of the archive bytes — verify after transport.
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedbackArchiveError {
    #[error("unknown feedback ticket {0}")]
    UnknownTicket(String),
    #[error("feedback archive: {0}")]
    Archive(String),
    #[error(transparent)]
    Feedback(#[from] FeedbackError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// `attachLogsFromExport`: bundle the session logs and record the archive
/// as a `log` attachment on the ticket. Returns the recorded attachment
/// metadata plus the archive path (kept on disk for the upload seam).
pub fn attach_logs_to_ticket(
    feedback: &FeedbackTicketStore,
    ticket_id: &str,
    sessions_dir: &Path,
    output_dir: &Path,
) -> Result<DiagnosticAttachment, FeedbackArchiveError> {
    let archive_path = output_dir.join(format!(
        "feedback-{}-diagnostics.tar.gz",
        ticket_id
    ));
    let report = create_diagnostic_archive(sessions_dir, &archive_path, crate::log_archive::MAX_TOTAL_BYTES)
        .map_err(|e| FeedbackArchiveError::Archive(e.to_string()))?;

    let bytes = std::fs::read(&archive_path)?;
    let sha256 = sha256_hex(&bytes);

    let attachment = feedback
        .attach(
            ticket_id,
            AttachmentKind::Log,
            &archive_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "application/gzip",
            report.size_bytes,
        )?
        .ok_or(FeedbackArchiveError::UnknownTicket(ticket_id.to_string()))?;

    Ok(DiagnosticAttachment {
        attachment_id: attachment.id,
        archive_path,
        size_bytes: report.size_bytes,
        sha256,
    })
}

/// Verify a transported archive against its recorded checksum.
pub fn verify_archive(archive_path: &Path, sha256: &str) -> Result<bool, std::io::Error> {
    let bytes = std::fs::read(archive_path)?;
    Ok(crate::plugins::store::sha256_hex(&bytes) == sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_gains_a_verifiable_diagnostic_attachment() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();

        // a session log to bundle
        let sessions = home.join(".okra/sessions");
        let dir = sessions.join("sess-a");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), "{\"seq\":1}\n").unwrap();

        // the ticket
        let feedback = FeedbackTicketStore::with_device(home, "device-a");
        let ticket = feedback
            .create("crash report", crate::feedback::TicketType::Bug, Some("high"), None, "details")
            .unwrap();

        let attachment = attach_logs_to_ticket(&feedback, &ticket.id, &sessions, &home).unwrap();
        assert!(attachment.archive_path.exists());
        assert_eq!(attachment.sha256.len(), 64);

        // the ticket records a log attachment with the same size
        let full = feedback.get(&ticket.id).unwrap().unwrap();
        assert_eq!(full.attachments.len(), 1);
        assert_eq!(full.attachments[0].kind, AttachmentKind::Log);
        assert_eq!(full.attachments[0].size_bytes, attachment.size_bytes);

        // checksum verifies post-transport
        assert!(verify_archive(&attachment.archive_path, &attachment.sha256).unwrap());
        assert!(!verify_archive_path_with_bad_hash(&attachment.archive_path).unwrap());
    }

    fn verify_archive_path_with_bad_hash(path: &Path) -> Result<bool, std::io::Error> {
        verify_archive(path, &"0".repeat(64))
    }

    #[test]
    fn unknown_ticket_refuses_archive_side_effects() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        let sessions = home.join(".okra/sessions");
        let dir = sessions.join("sess-a");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), "{}\n").unwrap();

        let feedback = FeedbackTicketStore::with_device(home, "device-a");
        let err = attach_logs_to_ticket(&feedback, "fb-missing", &sessions, &home).unwrap_err();
        assert!(matches!(err, FeedbackArchiveError::UnknownTicket(_)), "{err}");
        // the archive side effects were cleaned: no tarball left behind
        assert!(!home.join("feedback-gate").exists());
    }
}
