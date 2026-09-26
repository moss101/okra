//! Artifact discovery kernels (MASTER-PLAN §3 #48, from ZCode
//! `conversationShareArtifactDiscovery.ts`) — the portable, transport-free
//! pieces of turning a local conversation into a shareable artifact
//! manifest:
//! - capability matching: which candidate files the server accepts, by
//!   extension + mime type, with media previews always excluded;
//! - change detection: re-verify each registered artifact's CURRENT bytes
//!   against its descriptor before upload — the row was written when the
//!   tool finished, the user may have edited the file since; projecting a
//!   stale file while uploading a fresh one would corrupt the share;
//! - the preview candidate fingerprint used for preflight comparisons.

use serde_json::Value;

use super::integrity::{canonical_json, ShareError};
use crate::plugins::store::sha256_hex;

/// Server-declared share capabilities (`conversationShareCapabilities`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShareCapabilities {
    pub allowed_artifacts: Vec<AllowedArtifact>,
    pub access_modes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AllowedArtifact {
    pub artifact_type: String,
    /// Extensions with or without the leading dot, matched case-insensitively.
    pub extensions: Vec<String>,
    pub mime_types: Vec<String>,
}

impl ShareCapabilities {
    /// Parse from the wire JSON; unknown shapes degrade to empty.
    pub fn from_wire(value: &Value) -> ShareCapabilities {
        let access_modes = value
            .get("access_modes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut caps = ShareCapabilities {
            access_modes,
            ..Default::default()
        };
        if let Some(list) = value.get("allowed_artifacts").and_then(Value::as_array) {
            for entry in list {
                let Some(artifact_type) = entry.get("type").and_then(Value::as_str) else {
                    continue;
                };
                let extensions = entry
                    .get("extensions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let mime_types = entry
                    .get("mime_types")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                caps.allowed_artifacts.push(AllowedArtifact {
                    artifact_type: artifact_type.to_string(),
                    extensions,
                    mime_types,
                });
            }
        }
        caps
    }
}

fn extension_of(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next()?;
    let dot = file_name.rfind('.')?;
    if dot == 0 || dot == file_name.len() - 1 {
        return None;
    }
    Some(file_name[dot + 1..].to_ascii_lowercase())
}

/// `matchAllowedArtifact`: media previews are candidate CARDS only — the
/// artifact type vocabulary has no media entries, so even a server that
/// wrongly advertises them never re-enters the uploadable manifest.
pub fn match_allowed_artifact(
    capabilities: &ShareCapabilities,
    source_ref: &str,
    mime_type: &str,
    preview_kind: Option<&str>,
) -> Option<String> {
    if matches!(preview_kind, Some("video") | Some("audio")) {
        return None;
    }
    let extension = extension_of(source_ref)?;
    for allowed in &capabilities.allowed_artifacts {
        let ext_match = allowed
            .extensions
            .iter()
            .any(|e| e.trim_start_matches('.').to_ascii_lowercase() == extension);
        let mime_match = allowed
            .mime_types
            .iter()
            .any(|m| m.eq_ignore_ascii_case(mime_type));
        if ext_match && mime_match {
            return Some(allowed.artifact_type.clone());
        }
    }
    None
}

/// One staged artifact: bytes re-read from the live source at publish time.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedArtifact {
    pub source_ref: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactDiscoveryError {
    #[error("conversation artifact changed after it was registered: {source_ref}")]
    ArtifactChanged { source_ref: String },
    #[error("artifact exceeds the share size limit: {source_ref} ({actual} > {max})")]
    TooLarge {
        source_ref: String,
        actual: u64,
        max: u64,
    },
    #[error("artifact read failed: {source_ref}: {message}")]
    Read {
        source_ref: String,
        message: String,
    },
}

/// `materializeRegisteredArtifacts`: re-read every registered artifact and
/// verify the CURRENT bytes still match the descriptor recorded in the
/// row (size AND sha256) — otherwise the projection/manifest would
/// describe the old file while multipart uploads the new one.
pub fn materialize_registered_artifacts(
    artifacts: &[Value],
    max_artifact_bytes: u64,
    mut read: impl FnMut(&str, u64) -> Result<Vec<u8>, String>,
) -> Result<Vec<MaterializedArtifact>, ArtifactDiscoveryError> {
    let mut out = Vec::new();
    for artifact in artifacts {
        let descriptor = artifact.get("descriptor").unwrap_or(artifact);
        let source_ref = descriptor
            .get("source_ref")
            .or_else(|| descriptor.get("ref"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let size_bytes = descriptor
            .get("size_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let expected_sha = descriptor
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or("");
        let bytes = read(&source_ref, max_artifact_bytes).map_err(|message| {
            ArtifactDiscoveryError::Read {
                source_ref: source_ref.clone(),
                message,
            }
        })?;
        if bytes.len() as u64 > max_artifact_bytes {
            return Err(ArtifactDiscoveryError::TooLarge {
                source_ref: source_ref.clone(),
                actual: bytes.len() as u64,
                max: max_artifact_bytes,
            });
        }
        let sha256 = sha256_hex(&bytes);
        if bytes.len() as u64 != size_bytes || sha256 != expected_sha {
            return Err(ArtifactDiscoveryError::ArtifactChanged { source_ref });
        }
        out.push(MaterializedArtifact {
            source_ref,
            bytes,
            sha256,
        });
    }
    Ok(out)
}

/// `getConversationSharePreviewCandidateFingerprint`: a stable identity
/// for the candidate SET (not the file contents) so a preflight pass can
/// detect that discovery inputs changed since the preview the user saw.
pub fn preview_candidate_fingerprint(candidates: &[Value]) -> Result<String, ShareError> {
    let projected: Vec<Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "sourceRef": c.get("sourceRef").cloned().unwrap_or(Value::Null),
                "displayName": c.get("displayName").cloned().unwrap_or(Value::Null),
                "previewKind": c.get("previewKind").cloned().unwrap_or(Value::Null),
                "artifactType": c.get("artifactType").cloned().unwrap_or(Value::Null),
                "mimeType": c.get("mimeType").cloned().unwrap_or(Value::Null),
                "productTurnId": c.get("productTurnId").cloned().unwrap_or(Value::Null),
                "requiresFileChanges": c.get("requiresFileChanges").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    canonical_json(&serde_json::Value::Array(projected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn capabilities() -> ShareCapabilities {
        ShareCapabilities::from_wire(&json!({
            "allowed_artifacts": [
                { "type": "document", "extensions": [".md", ".pdf"], "mime_types": ["text/markdown", "application/pdf"] },
                { "type": "image", "extensions": ["png"], "mime_types": ["image/png"] }
            ],
            "access_modes": ["view", "continue"]
        }))
    }

    #[test]
    fn capability_matching_by_extension_and_mime() {
        let caps = capabilities();
        assert_eq!(
            match_allowed_artifact(&caps, "file:///x/notes.md", "text/markdown", None),
            Some("document".to_string())
        );
        assert_eq!(
            match_allowed_artifact(&caps, "file:///x/notes.MD", "TEXT/Markdown", None),
            Some("document".to_string()),
            "case-insensitive matching"
        );
        assert_eq!(
            match_allowed_artifact(&caps, "file:///x/pic.PNG", "image/png", None),
            Some("image".to_string()),
            "dot optional in the table"
        );
        assert_eq!(
            match_allowed_artifact(&caps, "file:///x/noext", "text/markdown", None),
            None
        );
        assert_eq!(
            match_allowed_artifact(&caps, "file:///x/notes.md", "text/plain", None),
            None,
            "extension alone is not enough"
        );
        // media previews never enter the manifest
        assert_eq!(
            match_allowed_artifact(&caps, "file:///x/clip.mp4", "video/mp4", Some("video")),
            None
        );
        assert_eq!(
            match_allowed_artifact(&caps, "file:///x/clip.mp3", "audio/mpeg", Some("audio")),
            None
        );
    }

    #[test]
    fn materialization_verifies_current_bytes_against_descriptor() {
        let bytes = b"# stable content".to_vec();
        let sha = sha256_hex(&bytes);
        let artifacts = vec![serde_json::json!({
            "descriptor": {
                "ref": "file:///x/notes.md",
                "size_bytes": bytes.len(),
                "sha256": sha
            }
        })];
        let staged =
            materialize_registered_artifacts(&artifacts, 1024, |_, _| Ok(bytes.clone()))
                .unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].sha256, sha);

        // user edited the file after registration → refused
        let err = materialize_registered_artifacts(&artifacts, 1024, |_, _| {
            Ok(b"# edited!".to_vec())
        })
        .unwrap_err();
        assert!(matches!(
            err,
            ArtifactDiscoveryError::ArtifactChanged { .. }
        ));

        // over the size cap → refused
        let err = materialize_registered_artifacts(&artifacts, 4, |_, _| Ok(bytes.clone()))
            .unwrap_err();
        assert!(matches!(err, ArtifactDiscoveryError::TooLarge { .. }));
    }

    #[test]
    fn fingerprint_tracks_candidate_set_changes() {
        let c1 = vec![serde_json::json!({
            "sourceRef": "file:///x/a.md", "displayName": "a.md",
            "previewKind": "text", "artifactType": "document",
            "mimeType": "text/markdown", "productTurnId": "pt-1",
            "requiresFileChanges": false
        })];
        let f1 = preview_candidate_fingerprint(&c1).unwrap();
        assert_eq!(f1, preview_candidate_fingerprint(&c1).unwrap(), "stable");
        let mut c2 = c1.clone();
        c2[0]["displayName"] = serde_json::json!("b.md");
        assert_ne!(
            f1,
            preview_candidate_fingerprint(&c2).unwrap(),
            "set change visible"
        );
    }
}
