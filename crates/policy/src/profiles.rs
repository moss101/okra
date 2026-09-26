//! Sandbox profiles — port of grok `xai-grok-sandbox/src/profiles.rs`.
//!
//! Profile names (`profiles.rs:69-78`): Workspace (default), Devbox,
//! ReadOnly, Strict, Off, Custom(String). `restricts_network` is true only
//! for ReadOnly | Strict (`:81-84`).
//!
//! The anti-hollow-out merge rule (`profiles.rs:114-164`): a project may ADD
//! new profile names but must NEVER redefine a global one — project config
//! cannot weaken the host's policy.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileName {
    Workspace,
    Devbox,
    ReadOnly,
    Strict,
    Off,
    Custom(String),
}

/// `FromStr` accepts the donor's spellings (`profiles.rs:73-77`):
/// "workspace" | "devbox" | "read-only"/"readonly" | "strict" |
/// "off"/"none" | other → Custom.
pub fn parse_profile_name(s: &str) -> ProfileName {
    match s {
        "workspace" => ProfileName::Workspace,
        "devbox" => ProfileName::Devbox,
        "read-only" | "readonly" => ProfileName::ReadOnly,
        "strict" => ProfileName::Strict,
        "off" | "none" => ProfileName::Off,
        other => ProfileName::Custom(other.to_string()),
    }
}

impl ProfileName {
    pub fn restricts_network(&self) -> bool {
        matches!(self, ProfileName::ReadOnly | ProfileName::Strict)
    }
}

/// `SandboxProfile` (`profiles.rs:28-43`): deny overrides read/write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SandboxProfile {
    pub name: String,
    pub read_only: Vec<PathBuf>,
    pub read_write: Vec<PathBuf>,
    pub deny: Vec<PathBuf>,
    /// Paths that are write-denied but readable (hook sources,
    /// `hook_write_deny`).
    #[serde(default)]
    pub write_deny: Vec<PathBuf>,
    pub default_read: bool,
    pub restrict_network: bool,
}

impl SandboxProfile {
    /// Path permission resolution: deny > write_deny > read_write > read >
    /// (default_read ? read : none).
    pub fn classify(&self, path: &std::path::Path) -> PathAccess {
        if self.deny.iter().any(|d| path.starts_with(d)) {
            return PathAccess::Denied;
        }
        if self.write_deny.iter().any(|d| path.starts_with(d)) {
            return PathAccess::ReadAllowed;
        }
        if self.read_write.iter().any(|d| path.starts_with(d)) {
            return PathAccess::WriteAllowed;
        }
        if self.read_only.iter().any(|d| path.starts_with(d)) || self.default_read {
            return PathAccess::ReadAllowed;
        }
        PathAccess::NoAccess
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    NoAccess,
    ReadAllowed,
    WriteAllowed,
    Denied,
}

/// `ProfileConfig` (`profiles.rs:50-61`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    #[serde(default)]
    pub restrict_network: bool,
    #[serde(default)]
    pub read_only: Vec<PathBuf>,
    #[serde(default)]
    pub read_write: Vec<PathBuf>,
    #[serde(default)]
    pub deny: Vec<PathBuf>,
}

/// `SandboxConfig` (`profiles.rs:63-67`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    pub profiles: HashMap<String, ProfileConfig>,
}

/// Additive-only merge (`profiles.rs:114-164`): a project may add new
/// profile names; redefining a global name is REJECTED, not merged — the
/// project config cannot hollow out the host's policy.
pub fn merge_configs(
    global: &SandboxConfig,
    project: &SandboxConfig,
) -> Result<SandboxConfig, String> {
    let mut merged = SandboxConfig { profiles: global.profiles.clone() };
    for (name, cfg) in &project.profiles {
        if global.profiles.contains_key(name) {
            return Err(format!(
                "project config may not redefine global sandbox profile `{name}` (anti-hollow-out rule)"
            ));
        }
        merged.profiles.insert(name.clone(), cfg.clone());
    }
    Ok(merged)
}

/// Resolve a profile through its `extends` chain against a config.
pub fn resolve_profile(config: &SandboxConfig, name: &str) -> Option<SandboxProfile> {
    let mut chain: Vec<(String, ProfileConfig)> = Vec::new();
    let mut current = name.to_string();
    loop {
        if chain.iter().any(|(n, _)| *n == current) {
            return None; // cycle
        }
        let cfg = config.profiles.get(&current)?;
        chain.push((current.clone(), cfg.clone()));
        match &cfg.extends {
            Some(parent) => current = parent.clone(),
            None => break,
        }
    }
    // fold from the root of the chain downward
    let mut read_only = Vec::new();
    let mut read_write = Vec::new();
    let mut deny = Vec::new();
    let mut restrict_network = false;
    for (_, cfg) in chain.iter().rev() {
        read_only.extend(cfg.read_only.iter().cloned());
        read_write.extend(cfg.read_write.iter().cloned());
        deny.extend(cfg.deny.iter().cloned());
        restrict_network |= cfg.restrict_network;
    }
    // network-restricted profiles default to read-only outside declared
    // paths (strict posture); unrestricted profiles follow the workspace
    // default (read outside, write inside read_write).
    let default_read = restrict_network;
    Some(SandboxProfile {
        name: name.to_string(),
        read_only,
        read_write,
        deny,
        write_deny: Vec::new(),
        default_read,
        restrict_network,
    })
}

/// The built-in Strict profile: workspace read-only, no network.
pub fn strict_profile(workspace: &std::path::Path) -> SandboxProfile {
    SandboxProfile {
        name: "strict".into(),
        read_only: vec![workspace.to_path_buf()],
        read_write: vec![],
        deny: vec![],
        write_deny: vec![],
        default_read: false,
        restrict_network: true,
    }
}
