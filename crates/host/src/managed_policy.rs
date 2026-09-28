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

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

impl ManagedPin {
    fn with_file_hash(mut self, file_hash: String) -> Self {
        if let Some(p) = self.provenance.as_mut() {
            p.sha256 = file_hash;
        }
        self
    }
}

/// Load a provisioned trust file: a JSON array of hex Ed25519 public keys
/// whose signed pins are approved. `None` = no trust file provisioned
/// (any valid-signature signer enforces, tamper-evident via provenance).
pub fn load_trusted_signers(path: &Path) -> Option<Vec<String>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let list = parsed.as_array()?;
    Some(
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// The admin-side counterpart of the envelope parsed by
/// `load_managed_pin_verified`: sign a policy payload so a distributed pin
/// is tamper-evident and its signer provable. The signature covers
/// `sha256(payload_text)` — the exact bytes verification hashes — so an
/// envelope can never certify a payload other than the one embedded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedPinEnvelope {
    /// The policy JSON, embedded verbatim (string, not re-serialized —
    /// re-serialization could silently differ from the signed bytes).
    pub payload: String,
    /// Ed25519 public key of the signer, 64 lowercase hex chars.
    pub signer: String,
    /// Ed25519 signature over sha256(payload), 128 hex chars.
    pub signature: String,
}

/// Validate the policy FIRST, then sign: an admin can never mint an
/// envelope for a payload the daemon would fail-closed on for schema
/// reasons — the error surfaces at signing time, not at every deployed
/// machine. The seed is the 32-byte Ed25519 signing seed; its public key
/// is what a trust file must list.
pub fn sign_policy_envelope(seed: [u8; 32], policy_json: &str) -> Result<SignedPinEnvelope, String> {
    // parse errors name the schema problem before any key material is used
    ManagedPolicy::from_json_str(policy_json)?;
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let digest: [u8; 32] = {
        let mut h = Sha256::new();
        h.update(policy_json.as_bytes());
        h.finalize().into()
    };
    use ed25519_dalek::Signer as _;
    let signature = key.sign(&digest);
    Ok(SignedPinEnvelope {
        payload: policy_json.to_string(),
        signer: hex_encode(&key.verifying_key().to_bytes()),
        signature: hex_encode(&signature.to_bytes()),
    })
}

/// Parse a signing-seed file for `okra pin-sign --key-file`: either exactly/// 32 raw bytes, or hex text (64 hex chars, surrounding whitespace allowed).
/// Anything else is an error naming what was wrong — never a truncated key.
pub fn parse_signing_seed(raw: &[u8]) -> Result<[u8; 32], String> {
    if raw.len() == 32 {
        let mut seed = [0u8; 32];
        seed.copy_from_slice(raw);
        return Ok(seed);
    }
    let text = std::str::from_utf8(raw)
        .map_err(|_| format!("key file is neither 32 raw bytes nor UTF-8 hex text ({} bytes)", raw.len()))?
        .trim();
    decode_32_hex(text).ok_or_else(|| {
        format!("key file text is not 64 hex chars (got {} chars)", text.len())
    })
}

/// The hex Ed25519 public key a provisioned trust file must list for this
/// signing seed — what `--generate-key` prints so an admin can copy it
/// without touching key material math themselves.
pub fn public_key_hex(seed: [u8; 32]) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    hex_encode(&key.verifying_key().to_bytes())
}

/// Load the pin a RUNNING process must enforce. Resolution order:
/// `OKRA_MANAGED_PIN` env (tests, deployment override) → `<home>/.okra/managed-
/// policy.json`. Absent file = NotConfigured (user in control); present
/// but broken = FailClosed (most restrictive) — both honest states the
/// caller must act on, never skip.
pub fn runtime_pin() -> ManagedPin {
    let path = match std::env::var_os("OKRA_MANAGED_PIN") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => crate::fsutil::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".okra")
            .join("managed-policy.json"),
    };
    let trust = std::env::var_os("OKRA_TRUST_FILE")
        .map(PathBuf::from)
        .and_then(|p| load_trusted_signers(&p));
    load_managed_pin_verified(&path, trust.as_deref())
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

    const POLICY_JSON: &str =
        r#"{"source":"corp-it","sandboxCeiling":"read-only","approvalMustAsk":true}"#;

    #[test]
    fn sign_round_trips_through_verified_load() {
        let env = sign_policy_envelope([7u8; 32], POLICY_JSON).unwrap();
        assert_eq!(env.signer.len(), 64);
        assert_eq!(env.signature.len(), 128);
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("pin.json");
        std::fs::write(&path, serde_json::to_vec(&env).unwrap()).unwrap();
        // unsigned trust file (None): any valid-signature signer enforces
        let pin = load_managed_pin(&path);
        assert_eq!(pin.state, PinState::Enforced);
        let prov = pin.provenance.unwrap();
        assert_eq!(prov.signature_verified, Some(true));
        assert_eq!(prov.signer.as_deref(), Some(env.signer.as_str()));
        assert_eq!(prov.source, "corp-it");
        // and the policy dims actually carry
        let pin = load_managed_pin(&path);
        assert_eq!(pin.sandbox_ceiling(), Some(SandboxCeiling::ReadOnly));
        assert!(pin.approval_must_ask());
    }

    #[test]
    fn signed_piner_is_enforced_only_when_trusted() {
        let env = sign_policy_envelope([9u8; 32], POLICY_JSON).unwrap();
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("pin.json");
        std::fs::write(&path, serde_json::to_vec(&env).unwrap()).unwrap();
        let trust = td.path().join("trust.json");
        std::fs::write(&trust, serde_json::to_vec(&vec![env.signer.clone()]).unwrap()).unwrap();
        let trusted = load_trusted_signers(&trust).unwrap();
        let pin = load_managed_pin_verified(&path, Some(&trusted));
        assert_eq!(pin.state, PinState::Enforced);
        // a different key's trust list fails the same envelope closed
        let other = sign_policy_envelope([3u8; 32], POLICY_JSON).unwrap();
        let strangers = vec![other.signer.clone()];
        let pin = load_managed_pin_verified(&path, Some(&strangers));
        match &pin.state {
            PinState::FailClosed { reason } => assert!(reason.contains("not trusted"), "{reason}"),
            other => panic!("expected fail-closed, got {other:?}"),
        }
    }

    #[test]
    fn sign_refuses_a_payload_the_daemon_would_reject() {
        let err = sign_policy_envelope([1u8; 32], "{\"sandboxCeiling\": \"nonsense\"}").unwrap_err();
        assert!(err.contains("schema mismatch"), "{err}");
        // and nothing was signed: the envelope never exists for bad payloads
        let env2 = sign_policy_envelope([1u8; 32], "not json");
        assert!(env2.is_err());
    }

    #[test]
    fn tampered_payload_after_signing_fails_verification() {
        let env = sign_policy_envelope([5u8; 32], POLICY_JSON).unwrap();
        let mut tampered = env.clone();
        tampered.payload = POLICY_JSON.replace("corp-it", "attacker");
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("pin.json");
        std::fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        match load_managed_pin(&path).state {
            PinState::FailClosed { reason } => {
                assert!(reason.contains("signature invalid"), "{reason}")
            }
            other => panic!("expected fail-closed, got {other:?}"),
        }
    }

    #[test]
    fn signing_seed_parses_raw_and_hex_and_nothing_else() {
        // raw 32 bytes pass through
        let raw: Vec<u8> = (0u8..=31).collect();
        let expected: [u8; 32] = raw.as_slice().try_into().unwrap();
        assert_eq!(parse_signing_seed(&raw).unwrap(), expected);
        // hex text with trailing newline (the realistic file form)
        let hex_file: String = "ab".repeat(32) + "\n";
        assert_eq!(
            parse_signing_seed(hex_file.as_bytes()).unwrap(),
            [0xabu8; 32]
        );
        // uppercase hex is accepted too
        assert_eq!(
            parse_signing_seed(b"AB".repeat(32).as_slice()).unwrap(),
            [0xabu8; 32]
        );
        // non-UTF-8 bytes of the wrong length refuse, naming the flaw
        assert!(parse_signing_seed(&[0xff, 0xfe, 0x00])
            .unwrap_err()
            .contains("neither 32 raw bytes"));
        assert!(parse_signing_seed(b"ab".repeat(31).as_slice())
            .unwrap_err()
            .contains("not 64 hex chars"));
        assert!(parse_signing_seed(b"hello world, this is not a key at all")
            .unwrap_err()
            .contains("not 64 hex chars"));
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

#[test]
fn dump_signed_fixture_for_manual_cli_debug() {
    use ed25519_dalek::{Signer, SigningKey};
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let policy_json = r#"{ "source": "org-it", "sandboxCeiling": "read-only" }"#;
    use sha2::Digest as _;
    let mut h = Sha256::new();
    h.update(policy_json.as_bytes());
    let sig = key.sign(&h.finalize());
    let signer_hex: String = key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let sig_hex: String = sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let doc = serde_json::json!({ "payload": policy_json, "signer": signer_hex, "signature": sig_hex });
    let dir = std::path::Path::new("/tmp/pin-debug");
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("managed-policy.json"), serde_json::to_string(&doc).unwrap()).unwrap();
    std::fs::write(dir.join("signer.hex"), &signer_hex).unwrap();
    println!("fixture at /tmp/pin-debug signer={signer_hex}");
}
