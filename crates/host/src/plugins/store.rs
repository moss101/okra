//! Plugin store — sha256 content-addressed distribution (MASTER-PLAN §3
//! #46), ZCode's contract (`zip-source.ts` + `marketplace.ts`):
//! - the digest is computed over the archive BYTES and must be a
//!   64-character lowercase hex string;
//! - verification happens BEFORE anything is written — a mismatch is fatal
//!   and leaves no partial state;
//! - installs are content-addressed: the same bytes always land at the same
//!   address, so a re-install is a no-op and a tampered store is detectable
//!   by re-hashing.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::manifest;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// ZCode: "Plugin zip source sha256 must be a 64 character hex string".
    #[error("plugin sha256 must be a 64 character lowercase hex string (got {0:?})")]
    BadDigestFormat(String),
    /// ZCode: "Plugin zip sha256 mismatch: expected=…, actual=…" — checked
    /// before any write, exactly like the donor.
    #[error("plugin sha256 mismatch: expected={expected}, actual={actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("bundle is not a valid plugin: {0}")]
    InvalidBundle(String),
    #[error("plugin receipt codec: {0}")]
    Receipt(#[from] serde_json::Error),
    #[error("plugin io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReceipt {
    pub sha256: String,
    pub plugin_name: Option<String>,
    pub size_bytes: usize,
    pub installed_at_epoch_ms: u64,
    /// Hex Ed25519 public key of the signer, when installed from a signed
    /// envelope. Unsigned installs leave this absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<String>,
}

pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        hex.push(char::from_digit((byte & 0xf) as u32, 16).unwrap_or('0'));
    }
    hex
}

/// The content-addressed plugin store rooted at a directory.
#[derive(Debug, Clone)]
pub struct PluginStore {
    root: PathBuf,
}

impl PluginStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        PluginStore { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Install a plugin bundle. `expected_sha256` is the distributor's
    /// pinned digest; it is validated for format, then against the actual
    /// bytes, both before any filesystem mutation. The manifest is parsed
    /// for the receipt's display name; a bundle whose manifest fails to
    /// parse is rejected (an unnameable plugin is uninstallable policy-wise).
    pub fn install(
        &self,
        bundle: &[u8],
        expected_sha256: &str,
        signer: Option<&str>,
    ) -> Result<InstallReceipt, InstallError> {
        if !is_sha256_hex(expected_sha256) {
            return Err(InstallError::BadDigestFormat(expected_sha256.to_string()));
        }
        let actual = sha256_hex(bundle);
        if actual != expected_sha256 {
            return Err(InstallError::DigestMismatch {
                expected: expected_sha256.to_string(),
                actual,
            });
        }
        let manifest::ParsedManifest { manifest, .. } =
            manifest::parse_manifest(&String::from_utf8_lossy(bundle));
        // A bare manifest names itself here; archive bundles (zip `PK` /
        // ustar) get named when their manifest is extracted at load time.
        let plugin_name = manifest.map(|m| m.name);
        if plugin_name.is_none() && !bundle.starts_with(b"PK") && !looks_like_tar(bundle) {
            return Err(InstallError::InvalidBundle(
                "bundle is neither a bare manifest, a zip, nor a tar archive".into(),
            ));
        }

        let dir = self.root.join(&actual);
        if dir.join("install.json").exists() {
            // Content-addressed: identical bytes already installed.
            let receipt = std::fs::read(dir.join("install.json"))?;
            return Ok(serde_json::from_slice(&receipt)?);
        }
        std::fs::create_dir_all(&dir)?;
        // Atomic install: stage in a sibling temp dir, then rename into the
        // content address. A crash leaves a staging dir, never a partial
        // content address (the receipt is written last inside the stage).
        let stage = self.root.join(format!(".staging-{actual}"));
        std::fs::create_dir_all(&stage)?;
        let receipt = InstallReceipt {
            sha256: actual,
            plugin_name,
            size_bytes: bundle.len(),
            installed_at_epoch_ms: now_ms(),
            signer: signer.map(str::to_string),
        };
        std::fs::write(stage.join("bundle.bin"), bundle)?;
        std::fs::write(stage.join("install.json"), serde_json::to_vec_pretty(&receipt)?)?;
        std::fs::rename(&stage, &dir)?;
        Ok(receipt)
    }

    /// The installed content address directory, if present.
    pub fn installed_dir(&self, sha256: &str) -> Option<PathBuf> {
        if !is_sha256_hex(sha256) {
            return None;
        }
        let dir = self.root.join(sha256);
        dir.join("install.json").exists().then_some(dir)
    }

    /// Tamper check: re-hash the stored bundle against its address.
    pub fn verify_installed(&self, sha256: &str) -> Result<(), InstallError> {
        let Some(dir) = self.installed_dir(sha256) else {
            return Err(InstallError::InvalidBundle("not installed".into()));
        };
        let bundle = std::fs::read(dir.join("bundle.bin"))?;
        let actual = sha256_hex(&bundle);
        if actual != sha256 {
            return Err(InstallError::DigestMismatch {
                expected: sha256.to_string(),
                actual,
            });
        }
        Ok(())
    }
}

fn looks_like_tar(bundle: &[u8]) -> bool {
    bundle.len() > 512 && bundle[257..262] == *b"ustar"
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_bundle(name: &str) -> String {
        format!(r#"{{ "name": "{name}", "version": "1.0.0" }}"#)
    }

    #[test]
    fn digest_helpers_match_kat() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(is_sha256_hex(&sha256_hex(b"x")));
        assert!(!is_sha256_hex(&sha256_hex(b"x").to_uppercase()));
        assert!(!is_sha256_hex("abc"));
    }

    #[test]
    fn install_rejects_bad_format_and_mismatch_before_write() {
        let td = tempfile::tempdir().unwrap();
        let store = PluginStore::open(td.path().join("plugins"));
        let bundle = manifest_bundle("p");
        // wrong hex format
        let err = store
            .install(bundle.as_bytes(), &"Z".repeat(64), None)
            .unwrap_err();
        assert!(matches!(err, InstallError::BadDigestFormat(_)), "{err}");
        // right format, wrong digest: nothing may be written — no store
        // root, no staging dirs, no partial state
        let err = store
            .install(bundle.as_bytes(), &"a".repeat(64), None)
            .unwrap_err();
        let InstallError::DigestMismatch { expected, actual } = err else {
            panic!("{err}")
        };
        assert_eq!(expected, "a".repeat(64));
        assert_eq!(actual, sha256_hex(bundle.as_bytes()));
        assert!(!td.path().join("plugins").exists());
    }

    #[test]
    fn install_is_content_addressed_and_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let store = PluginStore::open(td.path().join("plugins"));
        let bundle = manifest_bundle("notes");
        let sha = sha256_hex(bundle.as_bytes());
        let r1 = store.install(bundle.as_bytes(), &sha, None).unwrap();
        assert_eq!(r1.sha256, sha);
        assert_eq!(r1.plugin_name.as_deref(), Some("notes"));
        let r2 = store.install(bundle.as_bytes(), &sha, None).unwrap();
        assert_eq!(r1, r2);
        assert!(store.installed_dir(&sha).is_some());
        assert!(store.installed_dir(&"b".repeat(64)).is_none());
        store.verify_installed(&sha).unwrap();
    }

    #[test]
    fn tampered_store_is_detected() {
        let td = tempfile::tempdir().unwrap();
        let store = PluginStore::open(td.path().join("plugins"));
        let bundle = manifest_bundle("p");
        let sha = sha256_hex(bundle.as_bytes());
        store.install(bundle.as_bytes(), &sha, None).unwrap();
        std::fs::write(store.installed_dir(&sha).unwrap().join("bundle.bin"), b"evil").unwrap();
        let err = store.verify_installed(&sha).unwrap_err();
        assert!(matches!(err, InstallError::DigestMismatch { .. }), "{err}");
    }

    #[test]
    fn non_plugin_bundle_rejected() {
        let td = tempfile::tempdir().unwrap();
        let store = PluginStore::open(td.path().join("plugins"));
        let bytes = b"not a plugin at all........";
        let err = store
            .install(bytes, &sha256_hex(bytes), None)
            .unwrap_err();
        assert!(matches!(err, InstallError::InvalidBundle(_)), "{err}");
    }
}
