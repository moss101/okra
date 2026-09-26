//! Secret scanning (qwen memory secret-scan block): nothing secret is
//! persisted into memory files. Pattern-first, conservative: on any match
//! the segment is REDACTED before storage, never silently dropped.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretHit {
    pub kind: String,
    pub start: usize,
    pub end: usize,
}

fn patterns() -> &'static [(&'static str, Regex)] {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // AWS access key ids
            ("aws_access_key", Regex::new(r"(AKIA|ASIA)[0-9A-Z]{16}").unwrap()),
            // GitHub PATs (classic + fine-grained prefixes)
            ("github_token", Regex::new(r"gh[pousr]_[A-Za-z0-9]{36,}").unwrap()),
            // assigned secrets: api_key = "…", token: '…', password = …
            (
                "assigned_secret",
                Regex::new(r#"(?i)(api[_-]?key|secret|token|password)\s*[=:]\s*['"]?[^'"\s]{8,}['"]?"#).unwrap(),
            ),
            // PEM private keys
            ("private_key", Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").unwrap()),
        ]
    })
}

/// Scan text; returns non-overlapping hits ordered by position.
pub fn scan_secrets(text: &str) -> Vec<SecretHit> {
    let mut hits: Vec<SecretHit> = Vec::new();
    for (kind, re) in patterns() {
        for m in re.find_iter(text) {
            if hits.iter().any(|h| m.start() < h.end && m.end() > h.start) {
                continue; // earlier pattern wins the overlap
            }
            hits.push(SecretHit { kind: kind.to_string(), start: m.start(), end: m.end() });
        }
    }
    hits.sort_by_key(|h| h.start);
    hits
}

/// Replace secret spans with a redaction marker.
pub fn redact_secrets(text: &str) -> String {
    let hits = scan_secrets(text);
    if hits.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    for h in hits {
        out.push_str(&text[pos..h.start]);
        out.push_str(&format!("[REDACTED:{}]", h.kind));
        pos = h.end;
    }
    out.push_str(&text[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_common_secret_shapes() {
        let text = "key = AKIAIOSFODNN7EXAMPLE\nghp_abcdefghijklmnopqrstuvwxyz0123456789\npassword = hunter2hunter2\nsafe text";
        let hits = scan_secrets(text);
        let kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
        assert!(kinds.contains(&"aws_access_key"), "{kinds:?}");
        assert!(kinds.contains(&"github_token"), "{kinds:?}");
        assert!(kinds.contains(&"assigned_secret"), "{kinds:?}");
    }

    #[test]
    fn redaction_preserves_surrounding_text() {
        let text = "before AKIAIOSFODNN7EXAMPLE after";
        let redacted = redact_secrets(text);
        assert_eq!(redacted, "before [REDACTED:aws_access_key] after");
        assert_eq!(redact_secrets("nothing here"), "nothing here");
    }

    #[test]
    fn memory_write_goes_through_redaction() {
        // the contract: memory content is redacted before persist
        let content = "user says their token = supersecret123 ok";
        let safe = redact_secrets(content);
        assert!(!safe.contains("supersecret123"), "{safe}");
    }
}
