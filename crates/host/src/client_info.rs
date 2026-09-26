//! Client config + device identity (MASTER-PLAN §3 #48 — ZCode
//! `client-config/` + `device/` domains): the stable facts a daemon tells
//! every surface on attach.
//!
//! Contracts:
//! - **device identity is stable**: a random 16-byte id is generated once
//!   per okra home, persisted at `~/.okra/device_id` (0600), and reused
//!   forever — two daemons on the same home share it; different homes
//!   differ;
//! - **client config** is the hello payload a surface receives: daemon
//!   name, protocol version, the device id, the daemon's session dir, and
//!   the capability list (which wire dialects and commands this daemon
//!   serves).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    /// 32-char lowercase hex, random at first generation.
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    pub daemon: String,
    pub protocol_version: u32,
    pub device_id: String,
    pub okra_version: String,
    pub sessions_dir: PathBuf,
    pub capabilities: Vec<String>,
}

pub const DAEMON_NAME: &str = "okra";
pub const PROTOCOL_VERSION: u32 = 3;
const DEVICE_ID_FILE: &str = "device_id";

/// Load-or-create the device identity under the okra home. Generation is
/// idempotent: the first call writes a random 32-hex id (0600), every
/// later call reads the same value.
pub fn load_or_create_device_identity(home: &Path) -> Result<DeviceIdentity, std::io::Error> {
    let path = home.join(".okra").join(DEVICE_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let id = existing.trim().to_string();
        if is_valid_device_id(&id) {
            return Ok(DeviceIdentity { device_id: id });
        }
        // a malformed id is regenerated (it carries no user data)
    }
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
    let id = random_device_id();
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(id.as_bytes())?;
    }
    Ok(DeviceIdentity { device_id: id })
}

fn random_device_id() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_valid_device_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Build the client-config payload a daemon hands to every surface.
pub fn client_config(
    home: &Path,
    sessions_dir: impl Into<PathBuf>,
    okra_version: impl Into<String>,
    capabilities: Vec<String>,
) -> Result<ClientConfig, std::io::Error> {
    Ok(ClientConfig {
        daemon: DAEMON_NAME.to_string(),
        protocol_version: PROTOCOL_VERSION,
        device_id: load_or_create_device_identity(home)?.device_id,
        okra_version: okra_version.into(),
        sessions_dir: sessions_dir.into(),
        capabilities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_identity_is_stable_per_home() {
        let td = tempfile::tempdir().unwrap();
        let a = load_or_create_device_identity(td.path()).unwrap();
        let b = load_or_create_device_identity(td.path()).unwrap();
        assert_eq!(a.device_id, b.device_id, "same home → same identity");
        assert_eq!(a.device_id.len(), 32);
        assert!(
            a.device_id.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{a:?}"
        );
        // the persisted file is 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(td.path().join(".okra/device_id"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn different_homes_have_different_identities() {
        let a = load_or_create_device_identity(tempfile::tempdir().unwrap().path()).unwrap();
        let b = load_or_create_device_identity(tempfile::tempdir().unwrap().path()).unwrap();
        assert_ne!(a.device_id, b.device_id);
    }

    #[test]
    fn malformed_persisted_id_is_regenerated() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join(".okra")).unwrap();
        std::fs::write(td.path().join(".okra/device_id"), "NOT-AN-ID").unwrap();
        let identity = load_or_create_device_identity(td.path()).unwrap();
        assert!(is_valid_device_id(&identity.device_id));
        // and the regenerated value is stable
        let again = load_or_create_device_identity(td.path()).unwrap();
        assert_eq!(identity.device_id, again.device_id);
    }

    #[test]
    fn client_config_carries_the_identity_and_capabilities() {
        let td = tempfile::tempdir().unwrap();
        let config = client_config(
            td.path(),
            td.path().join(".okra/sessions"),
            "0.1.0",
            vec!["ndjson".into(), "http+sse".into()],
        )
        .unwrap();
        assert_eq!(config.daemon, "okra");
        assert_eq!(config.protocol_version, 3);
        assert_eq!(config.okra_version, "0.1.0");
        assert_eq!(config.device_id.len(), 32);
        assert_eq!(
            config.capabilities,
            vec!["ndjson".to_string(), "http+sse".to_string()]
        );
    }
}
