//! Plugin signing — Ed25519, clean-room (MASTER-PLAN §3 #46: no donor owns
//! this piece; kimi's manifest is unsigned data, ZCode pins sha256s, so
//! okra combines both: the store pins content by sha256, and a signature
//! covers that digest so a distributor can vouch for an address).
//!
//! The signed message is the 64-byte sha256 **hex string** of the bundle —
//! signature and content-address verify against the same identity, so a
//! signature can never certify bytes other than those installed.
//!
//! Trust model: a `TrustStore` is a set of trusted Ed25519 public keys.
//! `Trusted` requires BOTH a valid signature AND a trusted signer — an
//! unknown but cryptographically valid signer is `UnknownSigner`, not
//! trusted, and a mismatched digest fails before signature math runs.

use std::collections::BTreeSet;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use super::store::{is_sha256_hex, sha256_hex};

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("signing key seed must be 32 bytes (got {0})")]
    BadSeed(usize),
    #[error("public key must be 32 bytes of hex (got {0:?})")]
    BadPublicKey(String),
    #[error("signature must be 64 bytes of hex (got {0:?})")]
    BadSignature(String),
    #[error("envelope digest must be a 64 character lowercase hex string (got {0:?})")]
    BadDigest(String),
    #[error("envelope digest does not match the bundle bytes: envelope={envelope}, actual={actual}")]
    DigestMismatch { envelope: String, actual: String },
}

/// Generate a 32-byte signing-key seed from the OS CSPRNG. Only use for
/// key provisioning; tests use fixed seeds (RFC 8032 vectors).
pub fn generate_seed() -> Result<[u8; 32], SigningError> {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|_| {
        SigningError::BadSeed(0) // CSPRNG failure collapsed to the seed error path
    })?;
    Ok(seed)
}

pub fn sign_bundle(seed: [u8; 32], bundle_sha256_hex: &str) -> Result<SignedPluginEnvelope, SigningError> {
    if !is_sha256_hex(bundle_sha256_hex) {
        return Err(SigningError::BadDigest(bundle_sha256_hex.to_string()));
    }
    let key = SigningKey::from_bytes(&seed);
    let signature = key.sign(bundle_sha256_hex.as_bytes());
    Ok(SignedPluginEnvelope {
        plugin_sha256: bundle_sha256_hex.to_string(),
        signer: hex(&key.verifying_key().to_bytes()),
        signature_hex: hex(&signature.to_bytes()),
    })
}

/// The signed-distribution envelope: which content address (sha256), who
/// vouches for it (Ed25519 public key hex), and the signature over the
/// digest string itself.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedPluginEnvelope {
    pub plugin_sha256: String,
    /// Ed25519 public key, 64 lowercase hex chars.
    pub signer: String,
    /// Ed25519 signature over the `plugin_sha256` string, 128 hex chars.
    pub signature_hex: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureVerdict {
    /// Valid signature from a trusted signer over the exact digest.
    Trusted,
    /// Cryptographically valid, but the signer is not in the trust store.
    UnknownSigner,
    /// Signature does not verify against the digest.
    Invalid,
}

/// The set of public keys this installation trusts as distributors.
#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    keys: BTreeSet<String>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trust(&mut self, public_key_hex: &str) {
        self.keys.insert(public_key_hex.to_ascii_lowercase());
    }

    pub fn is_trusted(&self, public_key_hex: &str) -> bool {
        self.keys.contains(&public_key_hex.to_ascii_lowercase())
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Verify an envelope against bundle bytes and the trust store.
pub fn verify_bundle(
    bundle: &[u8],
    envelope: &SignedPluginEnvelope,
    trust: &TrustStore,
) -> Result<SignatureVerdict, SigningError> {
    let actual = sha256_hex(bundle);
    if envelope.plugin_sha256 != actual {
        // Digest mismatch is fatal, not a signature verdict: these bytes
        // were never signed as claimed.
        return Err(SigningError::DigestMismatch {
            envelope: envelope.plugin_sha256.clone(),
            actual,
        });
    }
    let pk = decode_32_hex(&envelope.signer)
        .ok_or_else(|| SigningError::BadPublicKey(envelope.signer.clone()))?;
    let verifying_key = VerifyingKey::from_bytes(&pk)
        .map_err(|_| SigningError::BadPublicKey(envelope.signer.clone()))?;
    let sig_bytes = decode_64_hex(&envelope.signature_hex)
        .ok_or_else(|| SigningError::BadSignature(envelope.signature_hex.clone()))?;
    let signature = Signature::from_bytes(&sig_bytes);
    if verifying_key.verify(envelope.plugin_sha256.as_bytes(), &signature).is_err() {
        return Ok(SignatureVerdict::Invalid);
    }
    if !trust.is_trusted(&envelope.signer) {
        return Ok(SignatureVerdict::UnknownSigner);
    }
    Ok(SignatureVerdict::Trusted)
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
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8032 §7.1 TEST 1 (standard Ed25519 vector; clean-room use).
    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
        0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
        0x1c, 0xae, 0x7f, 0x60,
    ];
    const PUB: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    #[test]
    fn key_derivation_matches_rfc8032() {
        let key = SigningKey::from_bytes(&SEED);
        assert_eq!(hex(&key.verifying_key().to_bytes()), PUB);
    }

    #[test]
    fn sign_and_verify_round_trip_trusted() {
        let digest = sha256_hex(b"plugin bytes");
        let envelope = sign_bundle(SEED, &digest).unwrap();
        assert_eq!(envelope.signer, PUB);
        let mut trust = TrustStore::new();
        trust.trust(PUB);
        assert_eq!(
            verify_bundle(b"plugin bytes", &envelope, &trust).unwrap(),
            SignatureVerdict::Trusted
        );
        // unknown signer is valid-but-untrusted
        assert_eq!(
            verify_bundle(b"plugin bytes", &envelope, &TrustStore::new()).unwrap(),
            SignatureVerdict::UnknownSigner
        );
    }

    #[test]
    fn tampered_bundle_fails_digest_not_signature() {
        let envelope = sign_bundle(SEED, &sha256_hex(b"good")).unwrap();
        let mut trust = TrustStore::new();
        trust.trust(PUB);
        let err = verify_bundle(b"evil", &envelope, &trust).unwrap_err();
        assert!(matches!(err, SigningError::DigestMismatch { .. }), "{err}");
    }

    #[test]
    fn forged_signature_is_invalid() {
        let digest = sha256_hex(b"plugin bytes");
        let mut envelope = sign_bundle(SEED, &digest).unwrap();
        // flip one bit of the signature
        let mut sig = decode_64_hex(&envelope.signature_hex).unwrap();
        sig[0] ^= 1;
        envelope.signature_hex = hex(&sig);
        let mut trust = TrustStore::new();
        trust.trust(PUB);
        assert_eq!(
            verify_bundle(b"plugin bytes", &envelope, &trust).unwrap(),
            SignatureVerdict::Invalid
        );
    }

    #[test]
    fn key_substitution_is_invalid() {
        let digest = sha256_hex(b"plugin bytes");
        let mut envelope = sign_bundle(SEED, &digest).unwrap();
        // attacker's key claiming the honest signer's signature
        let attacker = SigningKey::from_bytes(&[7u8; 32]);
        envelope.signer = hex(&attacker.verifying_key().to_bytes());
        let mut trust = TrustStore::new();
        trust.trust(PUB);
        trust.trust(&envelope.signer);
        assert_eq!(
            verify_bundle(b"plugin bytes", &envelope, &trust).unwrap(),
            SignatureVerdict::Invalid
        );
    }

    #[test]
    fn envelope_field_format_is_validated() {
        assert!(matches!(
            sign_bundle(SEED, "not-a-digest"),
            Err(SigningError::BadDigest(_))
        ));
        let digest = sha256_hex(b"x");
        let mut envelope = sign_bundle(SEED, &digest).unwrap();
        envelope.signer = "zz".into();
        let err = verify_bundle(b"x", &envelope, &TrustStore::new()).unwrap_err();
        assert!(matches!(err, SigningError::BadPublicKey(_)), "{err}");
    }

    #[test]
    fn generated_seed_signs() {
        let seed = generate_seed().unwrap();
        let digest = sha256_hex(b"fresh key");
        let envelope = sign_bundle(seed, &digest).unwrap();
        let mut trust = TrustStore::new();
        trust.trust(&envelope.signer);
        assert_eq!(
            verify_bundle(b"fresh key", &envelope, &trust).unwrap(),
            SignatureVerdict::Trusted
        );
    }
}
