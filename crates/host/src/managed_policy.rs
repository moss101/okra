//! Enterprise managed policy pin — the M6 rollout core (MASTER-PLAN §4 M6
//! "enterprise managed-pin rollout"). An administrator deploys a signed-at-
//! rest JSON pin file; the daemon merges it OVER user settings with
//! fail-closed semantics:
//!
//! - **missing file** → `NotConfigured`: user settings fully in control;
//! - **corrupt / not-an-object** → `FailClosed`: every managed dimension
//!   clamps to its most restrictive value (sandbox → read-only, approvals
//!   → ask, providers → deny-all, turns → 1) and the diagnostics name the
//!   file — a broken pin must never silently widen;
//! - **valid** → `Enforced`: user values are clamped per dimension, every
//!   override is reported, and the pin's sha256 + source are recorded so
//!   rollout tooling can verify which bytes are in force.
//!
//! The distribution channel (MDM, group policy, provisioned image) is
//! enterprise-specific and stays outside this crate; any channel that puts
//! the file at the injected path gets enforced.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use sha2::{Digest, Sha256};

/// Schema version of the pin document.
pub const MANAGED_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxCeiling {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxCeiling {
    fn rank(self) -> u8 {
        match self {
            SandboxCeiling::ReadOnly => 0,
            SandboxCeiling::WorkspaceWrite => 1,
            SandboxCeiling::DangerFullAccess => 2,
        }
    }

    /// The most restrictive managed ceiling (used by the fail-closed path).
    pub fn most_restrictive() -> Self {
        SandboxCeiling::ReadOnly
    }
}

/// A user sandbox mode clamped under the managed ceiling.
pub fn clamp_sandbox(user: SandboxCeiling, ceiling: Option<SandboxCeiling>) -> (SandboxCeiling, bool) {
    match ceiling {
        Some(c) if user.rank() > c.rank() => (c, true),
        _ => (user, false),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedPolicy {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Free-form label of the deploying authority (recorded in provenance).
    #[serde(default)]
    pub source: String,
    /// The most permissive sandbox mode allowed. User requests above the
    /// ceiling are clamped down.
    #[serde(rename = "sandboxCeiling", default, skip_serializing_if = "Option::is_none")]
    pub sandbox_ceiling: Option<SandboxCeiling>,
    /// When true, `approval: never` is clamped to `ask` — the daemon must
    /// always ask a human for side-effecting tools.
    #[serde(rename = "approvalMustAsk", default, skip_serializing_if = "Option::is_none")]
    pub approval_must_ask: Option<bool>,
    /// When present, ONLY these model provider ids may be used.
    #[serde(rename = "providerAllowlist", default, skip_serializing_if = "Option::is_none")]
    pub provider_allowlist: Option<Vec<String>>,
    /// Upper bound on turns per run.
    #[serde(rename = "maxTurnsCeiling", default, skip_serializing_if = "Option::is_none")]
    pub max_turns_ceiling: Option<u32>,
}

fn default_version() -> u32 {
    MANAGED_POLICY_SCHEMA_VERSION
}

impl ManagedPolicy {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let parsed: Value =
            serde_json::from_str(raw).map_err(|e| format!("pin is not valid JSON: {e}"))?;
        if !parsed.is_object() {
            return Err("pin document is not an object".into());
        }
        serde_json::from_value(parsed).map_err(|e| format!("pin schema mismatch: {e}"))
    }
}

/// How the pin resolved — the enforcement-honesty type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PinState {
    /// No pin file at the managed path: user settings in control.
    NotConfigured,
    /// Pin present but unreadable/corrupt: every managed dimension is at
    /// its most restrictive value until an admin repairs the file.
    FailClosed { reason: String },
    /// Pin loaded and enforced as written.
    Enforced,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PinProvenance {
    pub path: PathBuf,
    pub sha256: String,
    pub source: String,
    /// None for unsigned (flat) pins; Some(true) once the embedded Ed25519
    /// signature verified against the embedded public key.
    pub signature_verified: Option<bool>,
    /// Hex public key of the signer (signed envelopes only).
    pub signer: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManagedPin {
    pub state: PinState,
    /// Present in `Enforced` and `FailClosed` (fail-closed values).
    pub policy: Option<ManagedPolicy>,
    pub provenance: Option<PinProvenance>,
    pub diagnostics: Vec<String>,
}

impl ManagedPin {
    /// The effective sandbox ceiling for this pin state.
    pub fn sandbox_ceiling(&self) -> Option<SandboxCeiling> {
        self.policy.as_ref().and_then(|p| p.sandbox_ceiling)
    }

    pub fn approval_must_ask(&self) -> bool {
        self.policy.as_ref().map(|p| p.approval_must_ask).unwrap_or(None) == Some(true)
    }

    pub fn provider_allowed(&self, provider_id: &str) -> bool {
        match self.policy.as_ref().and_then(|p| p.provider_allowlist.as_ref()) {
            Some(list) => list.iter().any(|a| a == provider_id),
            None => true,
        }
    }

    pub fn clamp_max_turns(&self, user: u32) -> (u32, bool) {
        match self.policy.as_ref().and_then(|p| p.max_turns_ceiling) {
            Some(c) if user > c => (c, true),
            _ => (user, false),
        }
    }
}

fn fail_closed(path: &Path, raw: Option<&[u8]>, reason: String) -> ManagedPin {
    let sha256 = raw
        .map(|b| {
            let mut h = Sha256::new();
            h.update(b);
            format!("{:x}", h.finalize())
        })
        .unwrap_or_default();
    ManagedPin {
        state: PinState::FailClosed { reason: reason.clone() },
        policy: Some(ManagedPolicy {
            version: MANAGED_POLICY_SCHEMA_VERSION,
            source: "fail-closed".into(),
            sandbox_ceiling: Some(SandboxCeiling::most_restrictive()),
            approval_must_ask: Some(true),
            provider_allowlist: Some(Vec::new()),
            max_turns_ceiling: Some(1),
        }),
        provenance: Some(PinProvenance {
            path: path.to_path_buf(),
            sha256,
            source: "fail-closed".into(),
            signature_verified: None,
            signer: None,
        }),
        diagnostics: vec![reason],
    }
}

/// Load the managed pin from `path`. Missing → NotConfigured; anything that
/// prevents a VALID read (unreadable, corrupt JSON, schema mismatch) →
/// FailClosed with the file's hash recorded (so admins can see exactly
/// which broken bytes caused the lockdown).
pub fn load_managed_pin(path: &Path) -> ManagedPin {
    load_managed_pin_verified(path, None)
}

/// Load with optional signer trust: when `trusted_signers` is provisioned,
/// a SIGNED envelope whose signer is not on the list fails closed (an
/// embedded-valid signature from an unapproved key is still a lockdown —
/// trust is an admin decision, not a crypto outcome). Unsigned flat pins
/// enforce as before; a signed envelope verifies its Ed25519 signature
/// (over sha256 of the payload text) against the embedded public key.
pub fn load_managed_pin_verified(
    path: &Path,
    trusted_signers: Option<&[String]>,
) -> ManagedPin {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ManagedPin {
                state: PinState::NotConfigured,
                policy: None,
                provenance: None,
                diagnostics: Vec::new(),
            };
        }
        Err(e) => return fail_closed(path, None, format!("pin unreadable: {e}")),
    };
    let text = match String::from_utf8(raw.clone()) {
        Ok(t) => t,
        Err(_) => return fail_closed(path, Some(&raw), "pin is not valid UTF-8".into()),
    };
    let doc: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return fail_closed(path, Some(&raw), format!("pin is not valid JSON: {e}")),
    };

    // Signed-envelope shape: { "payload": "<policy JSON text>",
    //                          "signer": "<64-hex pubkey>",
    //                          "signature": "<128-hex ed25519 over sha256(payload)>" }
    let envelope_payload = doc.get("payload").and_then(Value::as_str);
    let envelope_signer = doc.get("signer").and_then(Value::as_str);
    let envelope_sig = doc.get("signature").and_then(Value::as_str);
    if let (Some(payload_text), Some(signer), Some(sig_hex)) =
        (envelope_payload, envelope_signer, envelope_sig)
    {
        let mut h = Sha256::new();
        h.update(payload_text.as_bytes());
        let digest = h.finalize();

        let pk_bytes = decode_32_hex(signer);
        let sig_bytes = decode_64_hex(sig_hex);
        let verified = match (pk_bytes, sig_bytes) {
            (Some(pk), Some(sig)) => {
                use ed25519_dalek::{Signature, Verifier, VerifyingKey};
                VerifyingKey::from_bytes(&pk)
                    .and_then(|vk| vk.verify(&digest, &Signature::from_bytes(&sig)))
                    .is_ok()
            }
            _ => false,
        };
        let file_hash = {
            let mut h = Sha256::new();
            h.update(&raw);
            format!("{:x}", h.finalize())
        };
        if !verified {
            return fail_closed(
                path,
                Some(&raw),
                "pin signature invalid: payload does not verify against the embedded key".into(),
            )
            .with_file_hash(file_hash);
        }
        if let Some(trusted) = trusted_signers
            && !trusted.iter().any(|t| t.eq_ignore_ascii_case(signer))
        {
            return fail_closed(
                path,
                Some(&raw),
                format!("pin signer not trusted: {signer}"),
            )
            .with_file_hash(file_hash);
        }
        return match ManagedPolicy::from_json_str(payload_text) {
            Ok(policy) => {
                let source = policy.source.clone();
                ManagedPin {
                    state: PinState::Enforced,
                    policy: Some(policy),
                    provenance: Some(PinProvenance {
                        path: path.to_path_buf(),
                        sha256: file_hash,
                        source,
                        signature_verified: Some(true),
                        signer: Some(signer.to_string()),
                    }),
                    diagnostics: Vec::new(),
                }
            }
            Err(reason) => fail_closed(path, Some(&raw), reason),
        };
    }

    // Unsigned flat document (the original shape).
    match ManagedPolicy::from_json_str(&text) {
        Ok(policy) => {
            let mut h = Sha256::new();
            h.update(&raw);
            let sha256 = format!("{:x}", h.finalize());
            let source = policy.source.clone();
            ManagedPin {
                state: PinState::Enforced,
                policy: Some(policy),
                provenance: Some(PinProvenance {
                    path: path.to_path_buf(),
                    sha256,
                    source,
                    signature_verified: None,
                    signer: None,
                }),
                diagnostics: Vec::new(),
            }
        }
        Err(reason) => fail_closed(path, Some(&raw), reason),
    }
}

fn decode_32_hex(s: &str) -> Option<[u8; 32]> {
    let bytes = decode_hex(s)?;
    bytes.try_into().ok()
}

fn decode_64_hex(s: &str) -> Option<[u8; 64]> {
    let bytes = decode_hex(s)?;
    bytes.try_into().ok()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

impl ManagedPin {
    fn with_file_hash(mut self, file_hash: String) -> Self {
        if let Some(p) = self.provenance.as_mut() {
            p.sha256 = file_hash;
        }
        self
    }
}

/// Approval resolution under the pin: an `approvalMustAsk` pin clamps
/// `never` → `ask`, and reports the override.
pub fn resolve_approval(pin: &ManagedPin, user_allows_never: bool) -> (bool, bool) {
    // (never_still_allowed, overridden)
    if pin.approval_must_ask() && user_allows_never {
        (false, true)
    } else {
        (user_allows_never, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pin(dir: &Path, content: &str) -> PathBuf {
        let p = dir.join("managed-policy.json");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn missing_pin_is_not_configured() {
        let td = tempfile::tempdir().unwrap();
        let pin = load_managed_pin(&td.path().join("managed-policy.json"));
        assert_eq!(pin.state, PinState::NotConfigured);
        assert!(pin.policy.is_none());
        assert!(pin.provider_allowed("anything"));
        assert_eq!(pin.clamp_max_turns(500), (500, false));
    }

    #[test]
    fn valid_pin_enforces_and_records_provenance() {
        let td = tempfile::tempdir().unwrap();
        let path = write_pin(
            td.path(),
            r#"{ "version": 1, "source": "org-it", "sandboxCeiling": "workspace-write",
                 "approvalMustAsk": true, "providerAllowlist": ["glm", "kimi"],
                 "maxTurnsCeiling": 64 }"#,
        );
        let pin = load_managed_pin(&path);
        assert_eq!(pin.state, PinState::Enforced);

        // sandbox clamps up-request down, leaves equal-or-lower alone
        let (mode, over) = clamp_sandbox(SandboxCeiling::DangerFullAccess, pin.sandbox_ceiling());
        assert_eq!((mode, over), (SandboxCeiling::WorkspaceWrite, true));
        let (mode, over) = clamp_sandbox(SandboxCeiling::ReadOnly, pin.sandbox_ceiling());
        assert_eq!((mode, over), (SandboxCeiling::ReadOnly, false));

        // approval never → ask
        let (never_ok, over) = resolve_approval(&pin, true);
        assert!(!never_ok && over);

        // allowlist
        assert!(pin.provider_allowed("glm"));
        assert!(!pin.provider_allowed("mystery"));

        // turns clamp
        assert_eq!(pin.clamp_max_turns(32), (32, false));
        assert_eq!(pin.clamp_max_turns(500), (64, true));

        // provenance: sha256 matches the exact bytes on disk
        let prov = pin.provenance.unwrap();
        assert_eq!(prov.source, "org-it");
        let bytes = std::fs::read(&path).unwrap();
        let mut h = Sha256::new();
        h.update(&bytes);
        assert_eq!(prov.sha256, format!("{:x}", h.finalize()));
    }

    #[test]
    fn corrupt_pin_fails_closed_restrictive() {
        let td = tempfile::tempdir().unwrap();
        let path = write_pin(td.path(), "{ not json !!!");
        let pin = load_managed_pin(&path);
        assert!(matches!(pin.state, PinState::FailClosed { .. }));

        let policy = pin.policy.as_ref().unwrap();
        assert_eq!(policy.sandbox_ceiling, Some(SandboxCeiling::most_restrictive()));
        assert_eq!(policy.approval_must_ask, Some(true));
        assert_eq!(
            policy.provider_allowlist.as_ref().unwrap(),
            &Vec::<String>::new(),
            "deny-all"
        );
        assert_eq!(policy.max_turns_ceiling, Some(1));
        assert!(!pin.provider_allowed("glm"));
        assert_eq!(pin.clamp_max_turns(500), (1, true));
        assert!(!pin.diagnostics.is_empty());
    }

    #[test]
    fn non_object_and_bad_utf8_pins_also_fail_closed() {
        let td = tempfile::tempdir().unwrap();
        let arr = write_pin(td.path(), "[1, 2, 3]");
        assert!(matches!(
            load_managed_pin(&arr).state,
            PinState::FailClosed { .. }
        ));
        let bytes = td.path().join("bin.json");
        std::fs::write(&bytes, [0xff, 0xfe, 0x00]).unwrap();
        let pin = load_managed_pin(&bytes);
        assert!(matches!(pin.state, PinState::FailClosed { .. }));
    }

    use ed25519_dalek::{Signer, SigningKey};

    fn signed_envelope(seed: [u8; 32], policy_json: &str) -> (String, String) {
        let key = SigningKey::from_bytes(&seed);
        use sha2::Digest as _;
        let mut h = Sha256::new();
        h.update(policy_json.as_bytes());
        let sig = key.sign(&h.finalize());
        let doc = serde_json::json!({
            "payload": policy_json,
            "signer": hex_encode(&key.verifying_key().to_bytes()),
            "signature": hex_encode(&sig.to_bytes()),
        });
        (serde_json::to_string(&doc).unwrap(), hex_encode(&key.verifying_key().to_bytes()))
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn signed_envelope_verifies_and_trust_list_gates() {
        let td = tempfile::tempdir().unwrap();
        let policy_json = r#"{ "source": "org-it", "sandboxCeiling": "read-only" }"#;
        let (doc, signer) = signed_envelope([7u8; 32], policy_json);
        let path = write_pin(td.path(), &doc);

        // no trust list: valid signature enforces, marked verified
        let pin = load_managed_pin_verified(&path, None);
        assert_eq!(pin.state, PinState::Enforced);
        let prov = pin.provenance.unwrap();
        assert_eq!(prov.signature_verified, Some(true));
        assert_eq!(prov.signer.as_deref(), Some(signer.as_str()));

        // tampered payload → signature invalid → fail closed
        let tampered = doc.replace("read-only", "danger-full-access");
        let tampered_path = td.path().join("tampered.json");
        std::fs::write(&tampered_path, &tampered).unwrap();
        let pin = load_managed_pin_verified(&tampered_path, None);
        assert!(matches!(pin.state, PinState::FailClosed { .. }), "{:?}", pin.state);
        assert!(
            pin.diagnostics.iter().any(|d| d.contains("signature invalid")),
            "{:?}",
            pin.diagnostics
        );

        // trust list: signer on it → enforce; signer off it → fail closed
        let (doc2, signer2) = signed_envelope([9u8; 32], policy_json);
        let path3 = write_pin(td.path(), &doc2);
        let pin = load_managed_pin_verified(&path3, Some(std::slice::from_ref(&signer2)));
        assert_eq!(pin.state, PinState::Enforced);
        let pin = load_managed_pin_verified(&path3, Some(&["deadbeef".to_string()]));
        assert!(matches!(pin.state, PinState::FailClosed { .. }));
        assert!(pin.diagnostics.iter().any(|d| d.contains("not trusted")));
    }

    #[test]
    fn schema_version_field_is_validated_by_serde_defaults() {
        // version is optional-but-typed; a wrong-typed version is a schema error
        let td = tempfile::tempdir().unwrap();
        let path = write_pin(td.path(), r#"{ "version": "one" }"#);
        assert!(matches!(
            load_managed_pin(&path).state,
            PinState::FailClosed { .. }
        ));
    }
}
