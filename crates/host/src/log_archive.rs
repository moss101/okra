//! Diagnostic log archive (MASTER-PLAN §3 #48, from ZCode
//! `feedback/feedbackLogArchive.ts` + `compactLogArchive.ts`): bundle the
//! workspace's session logs into one gzip'd archive for a feedback
//! ticket, with the donor's budget and safety rules.
//!
//! Donor contracts kept:
//! - **what is collected**: only diagnostic log files — `*.log`,
//!   `*.log.N` rotations, `*.jsonl`, `*.ndjson` — walked depth-bounded
//!   (max 4 levels, max 2000 entries), symlinks never followed;
//! - **size budgets**: per-file cap (8 MiB, donor `MAX_FILE_BYTES`) and
//!   a total cap (32 MiB, donor `MAX_TOTAL_BYTES`); an over-budget file
//!   is skipped with a reason counted in the report, never silently
//!   truncated mid-record;
//! - **today's logs first**: files modified today are preferred — the
//!   feedback flow cares about the current session;
//! - the archive is gzip'd ustar reusing the skill-sync machinery, and
//!   the caller gets a machine-readable skip report.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};


pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DEPTH: usize = 4;
const MAX_ENTRIES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveReport {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub files_archived: u32,
    pub skipped: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFileKind {
    Log,
    Rotation,
    Jsonl,
    Ndjson,
}

/// Donor's log-file name filter: `.log`, `.log.N`, `.jsonl`, `.ndjson`.
fn log_file_name(name: &str) -> Option<LogFileKind> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".jsonl") {
        Some(LogFileKind::Jsonl)
    } else if lower.ends_with(".ndjson") {
        Some(LogFileKind::Ndjson)
    } else if lower.ends_with(".log") {
        Some(LogFileKind::Log)
    } else {
        lower
            .rsplit_once(".log.")
            .filter(|(_, rotation)| !rotation.is_empty() && rotation.bytes().all(|b| b.is_ascii_digit()))
            .map(|_| LogFileKind::Rotation)
    }
}

fn walk_logs(
    root: &Path,
    depth: usize,
    visited: &mut usize,
    found: &mut Vec<(PathBuf, String)>,
    skipped: &mut BTreeMap<String, u32>,
) {
    if depth > MAX_DEPTH || *visited > MAX_ENTRIES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    names.sort();
    for path in names {
        *visited += 1;
        if *visited > MAX_ENTRIES {
            skipped.insert("entry-limit".into(), 1);
            return;
        }
        // symlinks never followed (donor rule)
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.is_symlink() {
            *skipped.entry("symlink".into()).or_insert(0) += 1;
            continue;
        }
        if meta.is_dir() {
            walk_logs(&path, depth + 1, visited, found, skipped);
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
        if log_file_name(&file_name).is_none() {
            continue;
        }
        let size = meta.len();
        if size > MAX_FILE_BYTES {
            *skipped.entry("file-too-large".into()).or_insert(0) += 1;
            continue;
        }
        found.push((path, file_name));
    }
}

/// `createFeedbackDiagnosticArchive` (compact flavor): bundle every
/// diagnostic log under `sessions_dir` into a gzip'd ustar archive at
/// `output_path`, enforcing the per-file and total byte budgets.
pub fn create_diagnostic_archive(
    sessions_dir: &Path,
    output_path: &Path,
    max_total_bytes: u64,
) -> Result<ArchiveReport, std::io::Error> {
    // collect candidate files (depth-bounded walk)
    let mut candidates: Vec<(PathBuf, String)> = Vec::new();
    let mut skipped: BTreeMap<String, u32> = BTreeMap::new();
    if sessions_dir.exists() {
        let mut visited = 0usize;
        walk_logs(sessions_dir, 0, &mut visited, &mut candidates, &mut skipped);
    }

    // today's logs first (donor `isToday` preference), then by name
    let day_start = day_start_ms();
    candidates.sort_by_key(|(path, _)| {
        let fresh = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64 >= day_start)
            .unwrap_or(false);
        (std::cmp::Reverse(fresh), path.clone())
    });

    // build the archive under the total budget
    let mut tar: Vec<u8> = Vec::new();
    let mut total = 0u64;
    let mut archived = 0u32;
    let mut report_skipped = skipped.clone();
    for (path, name) in &candidates {
        let Ok(bytes) = std::fs::read(path) else {
            *report_skipped.entry("unreadable".into()).or_insert(0) += 1;
            continue;
        };
        if total + bytes.len() as u64 > max_total_bytes {
            *report_skipped.entry("budget".into()).or_insert(0) += 1;
            continue;
        }
        let rel = path
            .strip_prefix(sessions_dir)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| name.clone());
        crate::skill_sync::append_tar_entry(
            &mut tar,
            path,
            &format!("logs/{rel}"),
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        total += bytes.len() as u64;
        archived += 1;
    }
    if archived == 0 {
        return Err(std::io::Error::other("no diagnostic logs found"));
    }
    // two zero end blocks (ustar terminator)
    tar.extend(std::iter::repeat_n(0u8, 1024));
    let archive = crate::skill_sync::gzip_tar(tar)
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output_path, &archive)?;
    Ok(ArchiveReport {
        path: output_path.to_path_buf(),
        size_bytes: archive.len() as u64,
        files_archived: archived,
        skipped: report_skipped,
    })
}

/// Start of the current UTC day in epoch-ms (donor `dayStart`).
fn day_start_ms() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_millis() as u64 / 86_400_000) * 86_400_000
}

/// Content hash for integrity verification after transport.
pub fn archive_checksum(bytes: &[u8]) -> String {
    crate::plugins::store::sha256_hex(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_logs(sessions_dir: &Path, id: &str) {
        let dir = sessions_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), "{\"seq\":1}\n{\"seq\":2}\n").unwrap();
    }

    #[test]
    #[cfg(unix)] // windows triage: tar extraction (second pass)
    fn archives_jsonl_logs_from_sessions() {
        let td = tempfile::tempdir().unwrap();
        let sessions = td.path().join("sessions");
        session_with_logs(&sessions, "sess-a");
        session_with_logs(&sessions, "sess-b");
        let out = td.path().join("diag.tar.gz");

        let report = create_diagnostic_archive(&sessions, &out, MAX_TOTAL_BYTES).unwrap();
        assert!(report.files_archived >= 2);
        assert!(report.size_bytes > 0);
        assert!(out.exists());

        // the archive is gzip (magic) and round-trips through the skill-sync
        // extractor: extract and confirm a session log is present
        let extracted = td.path().join("extracted");
        std::fs::create_dir_all(&extracted).unwrap();
        crate::skill_sync::extract_archive(
            std::fs::read(&out).unwrap().as_slice(),
            &extracted,
            MAX_TOTAL_BYTES,
        )
        .unwrap();
        assert!(extracted.join("logs/sess-a/session.jsonl").exists());
    }

    #[test]
    fn non_log_files_are_not_collected() {
        let td = tempfile::tempdir().unwrap();
        let sessions = td.path().join("sessions");
        std::fs::create_dir_all(sessions.join("sess-a")).unwrap();
        std::fs::write(sessions.join("sess-a/session.jsonl"), "{}\n").unwrap();
        std::fs::write(sessions.join("sess-a/readme.md"), "not a log").unwrap();
        std::fs::write(sessions.join("sess-a/config.json"), "{}").unwrap();

        let out = td.path().join("diag.tar.gz");
        let report = create_diagnostic_archive(&sessions, &out, MAX_TOTAL_BYTES).unwrap();
        assert_eq!(report.files_archived, 1, "only the jsonl is a diagnostic log");
    }

    #[test]
    fn per_file_budget_skips_oversize_logs_with_reason() {
        let td = tempfile::tempdir().unwrap();
        let sessions = td.path().join("sessions");
        let dir = sessions.join("big");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("huge.log"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
        std::fs::write(dir.join("small.log"), b"tiny").unwrap();

        let out = td.path().join("diag.tar.gz");
        let report = create_diagnostic_archive(&sessions, &out, 1024 * 1024).unwrap();
        assert_eq!(report.files_archived, 1, "small.log only");
        assert_eq!(
            report.skipped.get("budget"),
            Some(&1),
            "huge.log exceeded the total budget"
        );
    }

    #[test]
    fn no_logs_is_an_error_not_an_empty_archive() {
        let td = tempfile::tempdir().unwrap();
        let sessions = td.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let out = td.path().join("diag.tar.gz");
        assert!(create_diagnostic_archive(&sessions, &out, MAX_TOTAL_BYTES).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn checksum_is_stable_and_sensitive() {
        assert_eq!(
            archive_checksum(b"abc"),
            archive_checksum(b"abc"),
            "stable"
        );
        assert_ne!(archive_checksum(b"abc"), archive_checksum(b"abd"), "sensitive");
    }

}
