//! Plugin domain (MASTER-PLAN §3 #46) — three composable pieces:
//! - [`manifest`]: kimi's data-only manifest shape with contained
//!   diagnostics (malformed optional fields degrade, never abort; runtime
//!   code fields are recorded and never interpreted);
//! - [`store`]: ZCode's sha256 content-addressed distribution — digest
//!   format + byte verification BEFORE any write, idempotent installs,
//!   tamper detection by re-hash;
//! - [`signing`]: clean-room Ed25519 over the content digest, so a trusted
//!   distributor vouches for exactly the bytes the store addresses.
//!
//! The end-to-end flow is `install_signed`: verify envelope → verify
//! digest → content-addressed install.

pub mod manifest;
pub mod signing;
pub mod store;

pub use manifest::{
    parse_manifest, plugin_name_valid, PluginAuthor, PluginDiagnostic, PluginManifest,
    ParsedManifest, SessionStart, DiagnosticSeverity, PLUGIN_SYSTEM_PROMPT_MAX_BYTES,
    UNSUPPORTED_RUNTIME_FIELDS,
};
pub use signing::{
    generate_seed, sign_bundle, verify_bundle, SignedPluginEnvelope, SigningError,
    SignatureVerdict, TrustStore,
};
pub use store::{is_sha256_hex, sha256_hex, InstallError, InstallReceipt, PluginStore};

/// Sign a bundle, install it content-addressed, and return the receipt —
/// the producer side of the flow.
pub fn publish_signed(
    store: &PluginStore,
    seed: [u8; 32],
    bundle: &[u8],
) -> Result<(InstallReceipt, SignedPluginEnvelope), PublishError> {
    let digest = store::sha256_hex(bundle);
    let envelope = signing::sign_bundle(seed, &digest)?;
    let receipt = store.install(bundle, &envelope.plugin_sha256, Some(&envelope.signer))?;
    Ok((receipt, envelope))
}

/// Verify + install a signed envelope from a distributor — the consumer
/// side. A `Trusted` verdict is required; anything else refuses to install.
pub fn install_signed(
    store: &PluginStore,
    bundle: &[u8],
    envelope: &SignedPluginEnvelope,
    trust: &TrustStore,
) -> Result<InstallReceipt, PublishError> {
    match signing::verify_bundle(bundle, envelope, trust)? {
        signing::SignatureVerdict::Trusted => {}
        verdict => return Err(PublishError::NotTrusted(format!("{verdict:?}"))),
    }
    Ok(store.install(bundle, &envelope.plugin_sha256, Some(&envelope.signer))?)
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error(transparent)]
    Signing(#[from] signing::SigningError),
    #[error(transparent)]
    Install(#[from] store::InstallError),
    #[error("signature did not yield a trusted verdict: {0}")]
    NotTrusted(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
        0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
        0x1c, 0xae, 0x7f, 0x60,
    ];

    fn bundle(name: &str) -> Vec<u8> {
        format!(r#"{{ "name": "{name}", "version": "1.0.0" }}"#).into_bytes()
    }

    #[test]
    fn publish_then_install_round_trip() {
        let td = tempfile::tempdir().unwrap();
        let store = PluginStore::open(td.path().join("plugins"));
        let bytes = bundle("signed-plugin");
        let (receipt, envelope) = publish_signed(&store, SEED, &bytes).unwrap();
        assert_eq!(receipt.plugin_name.as_deref(), Some("signed-plugin"));
        assert_eq!(receipt.signer.as_deref(), Some(envelope.signer.as_str()));

        // consumer: trusts the publisher's key
        let mut trust = TrustStore::new();
        trust.trust(&envelope.signer);
        let installed = install_signed(&store, &bytes, &envelope, &trust).unwrap();
        assert_eq!(installed, receipt);
        store.verify_installed(&receipt.sha256).unwrap();

        // consumer WITHOUT the key in its trust store refuses to install
        let err = install_signed(&store, &bytes, &envelope, &TrustStore::new()).unwrap_err();
        assert!(matches!(err, PublishError::NotTrusted(_)), "{err}");
    }

    #[test]
    fn tampered_distribution_never_installs() {
        let td = tempfile::tempdir().unwrap();
        let store = PluginStore::open(td.path().join("plugins"));
        let (_receipt, envelope) = publish_signed(&store, SEED, &bundle("p")).unwrap();
        let mut trust = TrustStore::new();
        trust.trust(&envelope.signer);
        let evil = bundle("p2");
        let err = install_signed(&store, &evil, &envelope, &trust).unwrap_err();
        assert!(matches!(
            err,
            PublishError::Signing(signing::SigningError::DigestMismatch { .. })
        ), "{err}");
    }
}
