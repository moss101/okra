//! Integrity (MASTER-PLAN §3 #48, from ZCode
//! `conversationShareIntegrity.ts`): canonical-JSON sha256 over the
//! projection rows and the sorted artifact descriptor set, plus a
//! verifier that hashes the RAW received values — additive publisher
//! fields survive version skew; only server-read-time signed-URL keys
//! are strippable, so a hash mismatch really means the content changed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plugins::store::sha256_hex;

// ---------------------------------------------------------------------------
// Integrity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ShareIntegrity {
    pub projection_sha256: String,
    pub artifact_set_sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("conversation share JSON numbers must be finite")]
    NonFiniteNumber,
    #[error("conversation share JSON cannot contain {0}")]
    UnsupportedValue(&'static str),
    #[error("{kind}: {message}")]
    Projection {
        kind: ProjectionErrorKind,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionErrorKind {
    InvalidConversation,
    UnsafeStructure,
    ArtifactProtocolNotReady,
}

impl std::fmt::Display for ProjectionErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ProjectionErrorKind::InvalidConversation => "invalid_conversation",
            ProjectionErrorKind::UnsafeStructure => "unsafe_structure",
            ProjectionErrorKind::ArtifactProtocolNotReady => "artifact_protocol_not_ready",
        };
        f.write_str(name)
    }
}

pub(crate) fn projection_error(
    kind: ProjectionErrorKind,
    message: impl Into<String>,
) -> ShareError {
    ShareError::Projection {
        kind,
        message: message.into(),
    }
}

/// Donor `canonicalizeValue`: sorted object keys, no whitespace, arrays
/// inline. Lone surrogates cannot exist in a Rust `str` (UTF-8), and
/// serde_json `Number`s are finite by construction — both donor checks are
/// structurally guaranteed here, the finite one is re-asserted defensively.
pub fn canonical_json(value: &Value) -> Result<String, ShareError> {
    Ok(match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if !n.is_f64() && !n.is_i64() && !n.is_u64() {
                return Err(ShareError::NonFiniteNumber);
            }
            n.to_string()
        }
        Value::String(s) => Value::String(s.clone()).to_string(),
        Value::Array(items) => {
            let parts: Result<Vec<_>, _> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts?.join(","))
        }
        Value::Object(map) => {
            let mut entries = Vec::with_capacity(map.len());
            for (key, entry) in map {
                if key.contains('\u{0}') {
                    return Err(ShareError::UnsupportedValue("control-character keys"));
                }
                entries.push((key.as_str(), entry));
            }
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let parts: Result<Vec<_>, _> = entries
                .into_iter()
                .map(|(key, entry)| {
                    Ok(format!(
                        "{}:{}",
                        Value::String(key.to_string()),
                        canonical_json(entry)?
                    ))
                })
                .collect();
            format!("{{{}}}", parts?.join(","))
        }
    })
}

/// `sha256ConversationShareJson`.
pub fn sha256_canonical(value: &Value) -> Result<String, ShareError> {
    Ok(sha256_hex(canonical_json(value)?.as_bytes()))
}

/// Server-issued signed-URL fields, appended at read time and NOT part of
/// the artifact-set digest (donor `ARTIFACT_URL_KEYS` — a read-time
/// attachment list, not a known-fields allowlist, so publisher additions
/// survive version skew).
const ARTIFACT_URL_KEYS: [&str; 4] =
    ["download_url", "download_url_expires_at", "url", "url_expires_at"];

fn artifact_id_of(value: &Value) -> &str {
    value
        .get("artifact_id")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn strip_artifact_urls(value: &Value) -> Value {
    let Some(map) = value.as_object() else {
        return value.clone();
    };
    let mut stripped = map.clone();
    for key in ARTIFACT_URL_KEYS {
        stripped.remove(key);
    }
    Value::Object(stripped)
}

/// `buildConversationShareConfirmRequest`'s integrity block: rows hashed
/// as-is; artifacts sorted by `artifact_id` first.
pub fn build_integrity(
    raw_rows: &Value,
    raw_artifacts: &[Value],
) -> Result<ShareIntegrity, ShareError> {
    let mut artifacts: Vec<&Value> = raw_artifacts.iter().collect();
    artifacts.sort_by(|a, b| artifact_id_of(a).cmp(artifact_id_of(b)));
    let sorted: Vec<Value> = artifacts.into_iter().cloned().collect();
    Ok(ShareIntegrity {
        projection_sha256: sha256_canonical(raw_rows)?,
        artifact_set_sha256: sha256_canonical(&Value::Array(sorted))?,
    })
}

/// `verifyConversationShareIntegrity`: hash the RAW received values —
/// never a parsed/projected form. Server read-time URL fields are stripped
/// from artifacts before hashing; anything else hashes verbatim.
pub fn verify_integrity(
    raw_rows: &Value,
    raw_artifacts: &Value,
    integrity: &ShareIntegrity,
) -> Result<bool, ShareError> {
    let artifacts_value = match raw_artifacts {
        Value::Array(items) => {
            let mut sorted: Vec<Value> = items.clone();
            sorted.sort_by(|a, b| artifact_id_of(a).cmp(artifact_id_of(b)));
            Value::Array(sorted.into_iter().map(|v| strip_artifact_urls(&v)).collect())
        }
        other => other.clone(),
    };
    Ok(sha256_canonical(raw_rows)? == integrity.projection_sha256
        && sha256_canonical(&artifacts_value)? == integrity.artifact_set_sha256)
}

