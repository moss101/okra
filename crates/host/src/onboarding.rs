//! Onboarding record (MASTER-PLAN §3 #48, from ZCode
//! `onboardingRecord.ts` + services): the durable first-run wizard state.
//!
//! Donor design constraints kept verbatim:
//! - an **independent local JSON** (`~/.okra/onboarding-record.json`) —
//!   never mixed into AppSettings;
//! - anchored to the **deviceMid** (okra's device identity), with
//!   entries supporting multiple userIds plus null (API-key / not
//!   logged in);
//! - **skip is an explicit answer**: a skipped field records `null`,
//!   distinct from "a value was chosen";
//! - **uploadState is reserved** (`pending`) for a future server upload
//!   — nothing uploads today;
//! - **schema versioning with read-side migration**: a v1 file (no
//!   decisions) upgrades to v2 (empty decisions) on read; the next write
//!   persists v2. Occupation stays a free string so old records can't
//!   fail validation as the occupation list evolves.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client_info;

pub const RECORD_VERSION_CURRENT: u32 = 2;

/// One wizard completion entry (occupation / mode / preferences).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingEntry {
    /// Owner user id; None = API-key / not logged in.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Free string — the occupation list evolves, old records must not
    /// fail validation. None = skipped page.
    pub occupation: Option<String>,
    /// `coding` | `office`; None = skipped.
    pub interface_mode: Option<String>,
    pub memory_enabled: Option<bool>,
    pub proactive_suggestions_enabled: Option<bool>,
    pub completed_at: String,
    /// Reserved for a future server upload; every local record is pending.
    pub upload_state: UploadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadState {
    Pending,
}

/// A wizard dismissal decision (the user closed the wizard instead of
/// completing it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingDecision {
    pub user_id: Option<String>,
    pub status: DecisionStatus,
    pub reason: DecisionReason,
    pub decided_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Dismissed,
    ExistingLocalUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionReason {
    UserClosed,
    ExistingLocalTask,
}

/// The durable record file. v2 on disk; a v1 file upgrades in memory
/// (empty decisions) and persists as v2 on the next write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingRecordFile {
    pub version: u32,
    pub device_mid: String,
    pub entries: Vec<OnboardingEntry>,
    #[serde(default)]
    pub decisions: Vec<OnboardingDecision>,
}

#[derive(Debug, thiserror::Error)]
pub enum OnboardingError {
    #[error("onboarding io: {0}")]
    Io(#[from] std::io::Error),
    #[error("onboarding codec: {0}")]
    Codec(#[from] serde_json::Error),
}

/// The service anchored to one okra home (device identity comes from
/// `client_info`).
pub struct OnboardingService {
    path: PathBuf,
    device_mid: String,
}

impl OnboardingService {
    pub fn open(home: &Path) -> Result<Self, OnboardingError> {
        Ok(OnboardingService {
            path: home.join(".okra").join("onboarding-record.json"),
            device_mid: client_info::load_or_create_device_identity(home)?.device_id,
        })
    }

    fn file_path(&self) -> PathBuf {
        self.path.clone()
    }

    fn read_file(&self) -> Result<OnboardingRecordFile, OnboardingError> {
        let raw = match std::fs::read_to_string(self.file_path()) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(OnboardingRecordFile {
                    version: RECORD_VERSION_CURRENT,
                    device_mid: self.device_mid.clone(),
                    entries: Vec::new(),
                    decisions: Vec::new(),
                })
            }
            Err(e) => return Err(e.into()),
        };
        let value: Value = serde_json::from_str(&raw)?;
        let version = value.get("version").and_then(Value::as_u64).unwrap_or(1) as u32;
        let mut file: OnboardingRecordFile = serde_json::from_value(value)?;
        file.device_mid = self.device_mid.clone();
        if version < 2 {
            // read-side migration: v1 has no decisions; upgrade in memory
            file.version = RECORD_VERSION_CURRENT;
            file.decisions = Vec::new();
        }
        Ok(file)
    }

    fn write_file(&mut self, file: &mut OnboardingRecordFile) -> Result<(), OnboardingError> {
        file.version = RECORD_VERSION_CURRENT;
        file.device_mid = self.device_mid.clone();
        if let Some(parent) = self.file_path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            self.file_path(),
            serde_json::to_vec_pretty(&Value::Object(
                serde_json::from_value(serde_json::to_value(file)?)?,
            ))?,
        )?;
        Ok(())
    }

    /// Append a wizard completion entry (`appendRecord`): the device
    /// anchor comes from the service, uploadState is always `pending`.
    pub fn append_entry(
        &mut self,
        user_id: Option<&str>,
        occupation: Option<&str>,
        interface_mode: Option<&str>,
        memory_enabled: Option<bool>,
        proactive_suggestions_enabled: Option<bool>,
        completed_at: impl Into<String>,
    ) -> Result<OnboardingEntry, OnboardingError> {
        let entry = OnboardingEntry {
            user_id: user_id.map(str::to_string),
            occupation: occupation.map(str::to_string),
            interface_mode: interface_mode.map(str::to_string),
            memory_enabled,
            proactive_suggestions_enabled,
            completed_at: completed_at.into(),
            upload_state: UploadState::Pending,
        };
        let mut file = self.read_file()?;
        file.entries.push(entry.clone());
        self.write_file(&mut file)?;
        Ok(entry)
    }

    /// Record a wizard dismissal (explicitly different from completing).
    pub fn record_decision(
        &mut self,
        user_id: Option<&str>,
        status: DecisionStatus,
        reason: DecisionReason,
        decided_at: impl Into<String>,
    ) -> Result<OnboardingDecision, OnboardingError> {
        let decision = OnboardingDecision {
            user_id: user_id.map(str::to_string),
            status,
            reason,
            decided_at: decided_at.into(),
        };
        let mut file = self.read_file()?;
        file.decisions.push(decision.clone());
        self.write_file(&mut file)?;
        Ok(decision)
    }

    /// The full record (entries + decisions) for surfaces.
    pub fn record(&self) -> Result<OnboardingRecordFile, OnboardingError> {
        self.read_file()
    }

    /// The wizard has at least one completion entry.
    pub fn has_completed_wizard(&self) -> Result<bool, OnboardingError> {
        Ok(!self.read_file()?.entries.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(home: &Path) -> OnboardingService {
        OnboardingService::open(home).unwrap()
    }

    fn full_entry(completed_at: &str) -> impl Fn() -> OnboardingEntry {
        move || OnboardingEntry {
            user_id: None,
            occupation: Some("engineer".into()),
            interface_mode: Some("coding".into()),
            memory_enabled: Some(true),
            proactive_suggestions_enabled: Some(false),
            completed_at: completed_at.into(),
            upload_state: UploadState::Pending,
        }
    }

    #[test]
    fn record_is_device_anchored_and_independent_of_settings() {
        let td = tempfile::tempdir().unwrap();
        let mut s = svc(td.path());
        s.append_entry(
            None,
            Some("engineer"),
            Some("coding"),
            Some(true),
            Some(false),
            "2026-09-26T10:00:00Z",
        )
        .unwrap();
        let record = s.record().unwrap();
        assert_eq!(record.device_mid.len(), 32);
        assert_eq!(record.entries.len(), 1);
        assert_eq!(record.entries[0].upload_state, UploadState::Pending);
        assert!(!td.path().join(".okra/settings.json").exists(),
            "onboarding is an independent local file, never mixed into settings");
    }

    #[test]
    fn skips_are_null_not_missing() {
        let td = tempfile::tempdir().unwrap();
        let mut s = svc(td.path());
        // every page skipped: fields recorded as null, still a completed entry
        s.append_entry(None, None, None, None, None, "t1").unwrap();
        let e = &s.record().unwrap().entries[0];
        assert_eq!(e.occupation, None);
        assert_eq!(e.interface_mode, None);
        assert_eq!(e.memory_enabled, None);
        assert_eq!(e.proactive_suggestions_enabled, None);
        assert_eq!(e.completed_at, "t1");
    }

    #[test]
    fn multiple_user_ids_supported() {
        let td = tempfile::tempdir().unwrap();
        let mut s = svc(td.path());
        s.append_entry(Some("user-1"), Some("eng"), Some("coding"), Some(true), None, "t1")
            .unwrap();
        s.append_entry(Some("user-2"), Some("design"), Some("office"), None, None, "t2")
            .unwrap();
        let record = s.record().unwrap();
        assert_eq!(record.entries.len(), 2);
        assert_eq!(record.entries[0].user_id.as_deref(), Some("user-1"));
        assert_eq!(record.entries[1].user_id.as_deref(), Some("user-2"));
    }

    #[test]
    fn v1_file_upgrades_to_v2_with_empty_decisions() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join(".okra/onboarding-record.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // a v1 file: entries only, no decisions array
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1,
                "deviceMid": "dev",
                "entries": [{
                    "userId": null,
                    "occupation": "eng",
                    "interfaceMode": "coding",
                    "memoryEnabled": true,
                    "proactiveSuggestionsEnabled": false,
                    "completedAt": "t",
                    "uploadState": "pending"
                }]
            })
            .to_string(),
        )
        .unwrap();
        let s = svc(td.path());
        let record = s.record().unwrap();
        assert_eq!(record.version, 2, "read-side upgrade to v2");
        assert!(record.decisions.is_empty());
        assert_eq!(record.entries.len(), 1);
        // the next write persists v2
        let mut s = s;
        s.record_decision(None, DecisionStatus::Dismissed, DecisionReason::UserClosed, "t")
            .unwrap();
        let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk["version"], 2);
        assert_eq!(on_disk["decisions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn decisions_record_dismissal_distinct_from_completion() {
        let td = tempfile::tempdir().unwrap();
        let mut s = svc(td.path());
        s.record_decision(
            None,
            DecisionStatus::Dismissed,
            DecisionReason::UserClosed,
            "t",
        )
        .unwrap();
        assert!(!s.has_completed_wizard().unwrap(), "dismissal is not completion");
        let mut s = svc(td.path());
        s.append_entry(None, Some("eng"), Some("coding"), None, None, "t")
            .unwrap();
        assert!(s.has_completed_wizard().unwrap());
        let _ = full_entry("x")();
    }
}
