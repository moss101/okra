//! Credential domain (MASTER-PLAN §3 #48, from ZCode
//! `packages/services/src/credential/`): the secure store for provider
//! tokens and login secrets, keyed by credential key (e.g. an OAuth
//! provider id — pairs with [`crate::oauth`]).
//!
//! Donor contracts kept:
//! - the store is a JSON map at `~/.okra/credentials.json`; **values are
//!   encrypted at rest** with AES-256-GCM under an `enc:v1:` prefix
//!   (12-byte nonce, 16-byte auth tag, base64url) — key = sha256 of the
//!   configured secret, falling back to a platform+home+user derived
//!   secret when `OKRA_CREDENTIAL_SECRET` is unset;
//! - **a corrupt store is never auto-overwritten**: a malformed file is
//!   backed up as `.corrupt-<ts>` and the error propagates — treating
//!   corrupt JSON as an empty store would silently wipe OAuth tokens on
//!   the next save;
//! - **reads-modify-write is atomic** (temp + rename) with private file
//!   mode (0600) — the temp file must not leak secrets via umask;
//! - keys are validated non-empty; error payloads carry reasons, never
//!   credential contents.

use std::path::PathBuf;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential key must be a non-empty string")]
    EmptyKey,
    #[error("unable to read credentials file {path}: {source}")]
    ReadIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("credentials are corrupt: {path} (backup: {backup:?})")]
    Corrupt {
        path: PathBuf,
        backup: Option<PathBuf>,
    },
    #[error("credential decryption failed (wrong secret or tampered value)")]
    Decrypt,
    #[error("credential io: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential codec: {0}")]
    Codec(#[from] serde_json::Error),
}

const ENCRYPTED_PREFIX: &str = "enc:v1:";
const NONCE_BYTES: usize = 12;

/// The default encryption secret: `OKRA_CREDENTIAL_SECRET` when set, else
/// a platform+home+user fallback (donor `defaultCredentialSecret`).
fn default_credential_secret() -> String {
    if let Ok(configured) = std::env::var("OKRA_CREDENTIAL_SECRET")
        && !configured.trim().is_empty()
    {
        return configured;
    }
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "unknown".into());
    format!("okra-credential-fallback:{}:{}:{}", std::env::consts::OS, home, username)
}

fn cipher(secret: &str) -> Aes256Gcm {
    let key = Sha256::digest(secret.as_bytes());
    Aes256Gcm::new_from_slice(&key).expect("sha256 output is a valid AES-256 key")
}

/// Encrypt to `enc:v1:<nonce b64url>.<ciphertext+tag b64url>`.
fn encrypt_value(secret: &str, value: &str) -> String {
    let aead = cipher(secret);
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    let _ = getrandom::getrandom(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let sealed = aead
        .encrypt(nonce, value.as_bytes())
        .expect("AES-GCM encryption of an in-memory value cannot fail");
    format!(
        "{}{}.{}",
        ENCRYPTED_PREFIX,
        b64url(&nonce_bytes),
        b64url(&sealed)
    )
}

fn decrypt_value(secret: &str, stored: &str) -> Result<String, CredentialError> {
    let Some(raw) = stored.strip_prefix(ENCRYPTED_PREFIX) else {
        return Err(CredentialError::Decrypt);
    };
    let Some((nonce_part, sealed_part)) = raw.split_once('.') else {
        return Err(CredentialError::Decrypt);
    };
    let nonce = decode_b64url(nonce_part).ok_or(CredentialError::Decrypt)?;
    let sealed = decode_b64url(sealed_part).ok_or(CredentialError::Decrypt)?;
    if nonce.len() != NONCE_BYTES {
        return Err(CredentialError::Decrypt);
    }
    let aead = cipher(secret);
    aead.decrypt(Nonce::from_slice(&nonce), sealed.as_ref())
        .map(|plain| String::from_utf8_lossy(&plain).into_owned())
        .map_err(|_| CredentialError::Decrypt)
}

fn b64url(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[n as usize & 63] as char);
        }
    }
    out
}

fn decode_b64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The credential store over one credentials.json.
pub struct CredentialStore {
    path: PathBuf,
    /// Explicit secret override; None = env/fallback resolution.
    secret: Option<String>,
}

impl CredentialStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        CredentialStore {
            path: home.into().join(".okra").join("credentials.json"),
            secret: None,
        }
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        CredentialStore {
            path: path.into(),
            secret: None,
        }
    }

    /// Pin the encryption secret explicitly (tests, multi-store hosts).
    pub fn with_secret(mut self, secret: impl Into<String>) -> Self {
        self.secret = Some(secret.into());
        self
    }

    fn secret(&self) -> String {
        self.secret
            .clone()
            .unwrap_or_else(default_credential_secret)
    }

    /// Strict read: missing file → empty; corrupt → backed up + hard
    /// error (a later save must never silently wipe other credentials).
    fn read_all(&self) -> Result<Map<String, Value>, CredentialError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Map::new())
            }
            Err(e) => {
                return Err(CredentialError::ReadIo {
                    path: self.path.clone(),
                    source: e,
                })
            }
        };
        let parsed: Result<Value, _> = serde_json::from_str(&raw);
        match parsed {
            Ok(Value::Object(map)) if map.values().all(Value::is_string) => Ok(map),
            _ => {
                // preserve the evidence, refuse to overwrite
                let backup = self.path.with_extension(format!(
                    "json.corrupt-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                ));
                let backup = std::fs::rename(&self.path, &backup).ok().map(|_| backup);
                Err(CredentialError::Corrupt {
                    path: self.path.clone(),
                    backup,
                })
            }
        }
    }

    /// Atomic private write (0600): the temp file must not leak secrets
    /// through umask before the rename lands.
    fn write_all(&self, map: &Map<String, Value>) -> Result<(), CredentialError> {
        use std::io::Write as _;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(&serde_json::to_vec_pretty(&Value::Object(map.clone()))?)?;
            file.write_all(b"\n")?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// `load`: decrypt one credential; None when absent.
    pub fn load(&self, key: &str) -> Result<Option<String>, CredentialError> {
        if key.is_empty() {
            return Err(CredentialError::EmptyKey);
        }
        let map = self.read_all()?;
        match map.get(key).and_then(Value::as_str) {
            Some(stored) => decrypt_value(&self.secret(), stored).map(Some),
            None => Ok(None),
        }
    }

    /// `save`: encrypt and upsert one credential.
    pub fn save(&self, key: &str, value: &str) -> Result<(), CredentialError> {
        if key.is_empty() {
            return Err(CredentialError::EmptyKey);
        }
        let mut map = self.read_all()?;
        map.insert(key.to_string(), Value::String(encrypt_value(&self.secret(), value)));
        self.write_all(&map)
    }

    /// `delete`: remove one credential; absent keys are a no-op.
    pub fn delete(&self, key: &str) -> Result<bool, CredentialError> {
        if key.is_empty() {
            return Err(CredentialError::EmptyKey);
        }
        let mut map = self.read_all()?;
        let removed = map.remove(key).is_some();
        if removed {
            self.write_all(&map)?;
        }
        Ok(removed)
    }

    /// Credential keys present (never values).
    pub fn keys(&self) -> Result<Vec<String>, CredentialError> {
        let map = self.read_all()?;
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_delete_round_trip() {
        let td = tempfile::tempdir().unwrap();
        let store = CredentialStore::with_path(td.path().join("credentials.json"));
        store.save("provider:zai", "secret-token-123").unwrap();
        assert_eq!(
            store.load("provider:zai").unwrap().as_deref(),
            Some("secret-token-123")
        );
        assert!(store.delete("provider:zai").unwrap());
        assert!(!store.delete("provider:zai").unwrap(), "idempotent delete");
        assert_eq!(store.load("provider:zai").unwrap(), None);
        // empty keys rejected
        assert!(matches!(
            store.save("", "v"),
            Err(CredentialError::EmptyKey)
        ));
    }

    #[test]
    fn values_are_encrypted_at_rest() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("credentials.json");
        let store = CredentialStore::with_path(&path);
        store.save("provider:zai", "plaintext-secret").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("plaintext-secret"), "plaintext leaked: {raw}");
        assert!(raw.contains(ENCRYPTED_PREFIX), "stored under enc:v1: prefix");
        // file is private
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secrets file is user-only");
        }
        // rotation: two saves produce different ciphertexts (fresh nonce)
        store.save("provider:zai", "plaintext-secret-2").unwrap();
        let raw2 = std::fs::read_to_string(&path).unwrap();
        assert_ne!(raw, raw2);
    }

    #[test]
    fn secret_changes_the_ciphertext_and_decryption() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("credentials.json");
        let one = CredentialStore::with_path(&path).with_secret("secret-one");
        let two = CredentialStore::with_path(&path).with_secret("secret-two");

        one.save("k", "value").unwrap();
        let raw_one = std::fs::read_to_string(&path).unwrap();

        // same plaintext under a different secret → different ciphertext
        two.save("k2", "value").unwrap();
        let raw_two = std::fs::read_to_string(&path).unwrap();
        assert_ne!(raw_one, raw_two);

        // decrypting the first save under the new secret must fail
        assert!(matches!(two.load("k"), Err(CredentialError::Decrypt)));
        // the original secret still decrypts its own save
        assert_eq!(one.load("k").unwrap().as_deref(), Some("value"));
    }

    #[test]
    fn corrupt_store_backed_up_and_never_overwritten() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("credentials.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let store = CredentialStore::with_path(&path);
        let err = store.load("k").unwrap_err();
        assert!(err.to_string().contains("corrupt"), "{err}");
        // the corrupt file was preserved (renamed to a .corrupt backup)
        assert!(!path.exists(), "original moved aside");
        let backup = match &err {
            CredentialError::Corrupt { backup, .. } => backup.clone().expect("backup recorded"),
            other => panic!("{other}"),
        };
        assert!(
            std::fs::read_to_string(&backup)
                .unwrap()
                .contains("this is not json"),
            "corrupt evidence preserved"
        );
        // a save after the failure starts a FRESH store (old corrupt file
        // was moved aside, not overwritten) — other credentials were never
        // silently destroyed by an empty-read overwrite
        store.save("new", "v").unwrap();
        assert_eq!(store.load("new").unwrap().as_deref(), Some("v"));
    }

    #[test]
    fn keys_listing_never_exposes_values() {
        let td = tempfile::tempdir().unwrap();
        let store = CredentialStore::with_path(td.path().join("credentials.json"));
        store.save("b", "v-b").unwrap();
        store.save("a", "v-a").unwrap();
        assert_eq!(store.keys().unwrap(), vec!["a".to_string(), "b".to_string()]);
    }
}
