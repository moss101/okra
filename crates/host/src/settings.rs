//! Settings domain (MASTER-PLAN §3 #48 — the "setting" strangler domain,
//! from ZCode `packages/services/src/setting/`): a two-scope typed
//! settings store for the daemon and every surface.
//!
//! Semantics kept from the donor shape:
//! - **two scopes**: user settings live next to the okra config
//!   (`~/.okra/settings.json`), workspace settings under the project
//!   (`<ws>/.okra/settings.json`);
//! - **resolution is workspace-wins**: `get` reads the workspace value
//!   when present, else the user value, else the DECLARED DEFAULT from
//!   the catalog — an undeclared key is still stored, but flags a
//!   diagnostic so unknown keys are visible to surfaces;
//! - **update is a patch**: `update(patch, scope)` merges the given keys
//!   into one scope atomically (write-temp-then-rename), never touching
//!   the other scope;
//! - **declared catalog**: every known key carries a default and a
//!   description, so surfaces can render a settings UI without hardcoding.
//!
//! This store holds plain JSON values — it is NOT the sync target of
//! `settings_sync` (which moves skills/commands/plugins/MCP between
//! agents); it is the daemon's own configuration plane.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    User,
    Workspace,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettingDescriptor {
    pub key: &'static str,
    pub default: Value,
    pub description: &'static str,
}

/// The declared settings catalog (okra's own settings; additive growth).
pub fn catalog() -> Vec<SettingDescriptor> {
    vec![
        SettingDescriptor {
            key: "notifications.enabled",
            default: Value::Bool(true),
            description: "surface native notifications for turn-complete/permission/question",
        },
        SettingDescriptor {
            key: "downloads.prompt",
            default: Value::Bool(false),
            description: "prompt for a save path on downloads instead of the download dir",
        },
        SettingDescriptor {
            key: "usage.retention_days",
            default: Value::from(90u64),
            description: "days to keep local usage telemetry before purge",
        },
        SettingDescriptor {
            key: "subagent.max_depth",
            default: Value::from(2u64),
            description: "default fork budget for subagent launches",
        },
        SettingDescriptor {
            key: "permissions.rules",
            default: Value::Array(Vec::new()),
            description: "project permission rules (#24): confirmed suggestedPermissionUpdates land here as {tool, pathPrefix?, effect} objects",
        },
    ]
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("settings key not declared: {0}")]
    UnknownKey(String),
    #[error("settings io: {0}")]
    Io(#[from] std::io::Error),
    #[error("settings codec: {0}")]
    Codec(#[from] serde_json::Error),
}

pub struct SettingsStore {
    user_path: PathBuf,
    workspace_path: Option<PathBuf>,
}

impl SettingsStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        SettingsStore {
            user_path: home.into().join(".okra").join("settings.json"),
            workspace_path: None,
        }
    }

    /// Attach a workspace scope (project `.okra/settings.json`).
    pub fn with_workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace_path = Some(workspace.into().join(".okra").join("settings.json"));
        self
    }

    fn scope_path(&self, scope: SettingsScope) -> Option<&Path> {
        match scope {
            SettingsScope::User => Some(&self.user_path),
            SettingsScope::Workspace => self.workspace_path.as_deref(),
        }
    }

    fn read_scope(&self, scope: SettingsScope) -> Map<String, Value> {
        let Some(path) = self.scope_path(scope) else {
            return Map::new();
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(_) => return Map::new(),
        };
        serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    fn write_scope(&self, scope: SettingsScope, map: &Map<String, Value>) -> Result<(), SettingsError> {
        let Some(path) = self.scope_path(scope) else {
            return Err(SettingsError::Io(std::io::Error::other(
                "workspace scope not attached",
            )));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // atomic: temp + rename so a reader never sees a torn settings file
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&Value::Object(map.clone()))?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The declared default for `key`, or None when undeclared.
    pub fn default_for(key: &str) -> Option<Value> {
        catalog()
            .into_iter()
            .find(|d| d.key == key)
            .map(|d| d.default)
    }

    /// Resolve `key`: workspace value → user value → declared default.
    /// Returns `(value, from_workspace)`.
    pub fn get(&self, key: &str) -> Result<(Value, bool), SettingsError> {
        if self.workspace_path.is_some() {
            let map = self.read_scope(SettingsScope::Workspace);
            if let Some(v) = map.get(key) {
                return Ok((v.clone(), true));
            }
        }
        let user = self.read_scope(SettingsScope::User);
        if let Some(v) = user.get(key) {
            return Ok((v.clone(), false));
        }
        Self::default_for(key)
            .map(|v| (v, false))
            .ok_or_else(|| SettingsError::UnknownKey(key.to_string()))
    }

    /// Set one key in one scope. Undeclared keys are stored (forward
    /// compatibility) but flagged in the returned diagnostics.
    pub fn set(
        &self,
        key: &str,
        value: Value,
        scope: SettingsScope,
    ) -> Result<Vec<String>, SettingsError> {
        let mut diagnostics = Vec::new();
        if Self::default_for(key).is_none() {
            diagnostics.push(format!("undeclared settings key stored: {key}"));
        }
        let mut map = self.read_scope(scope);
        map.insert(key.to_string(), value);
        self.write_scope(scope, &map)?;
        Ok(diagnostics)
    }

    /// `update`: merge a patch into one scope atomically.
    pub fn update(
        &self,
        patch: &Map<String, Value>,
        scope: SettingsScope,
    ) -> Result<Vec<String>, SettingsError> {
        let mut diagnostics = Vec::new();
        let mut map = self.read_scope(scope);
        for (key, value) in patch {
            if Self::default_for(key).is_none() {
                diagnostics.push(format!("undeclared settings key stored: {key}"));
            }
            map.insert(key.clone(), value.clone());
        }
        self.write_scope(scope, &map)?;
        Ok(diagnostics)
    }

    /// Remove a key from a scope (the lower scope / default then applies).
    pub fn delete(&self, key: &str, scope: SettingsScope) -> Result<bool, SettingsError> {
        let mut map = self.read_scope(scope);
        let removed = map.remove(key).is_some();
        if removed {
            self.write_scope(scope, &map)?;
        }
        Ok(removed)
    }

    /// The full effective view: every declared key resolved, plus any
    /// extra keys found in either scope.
    pub fn effective(&self) -> Result<Map<String, Value>, SettingsError> {
        let mut effective = Map::new();
        for d in catalog() {
            let (value, _) = self.get(d.key)?;
            effective.insert(d.key.to_string(), value);
        }
        for scope in [SettingsScope::User, SettingsScope::Workspace] {
            for (key, value) in self.read_scope(scope) {
                effective.entry(key).or_insert(value);
            }
        }
        Ok(effective)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store(home: &Path, ws: Option<&Path>) -> SettingsStore {
        let s = SettingsStore::new(home);
        match ws {
            Some(ws) => s.with_workspace(ws),
            None => s,
        }
    }

    #[test]
    fn declared_defaults_apply_without_files() {
        let td = tempfile::tempdir().unwrap();
        let s = store(td.path(), None);
        let (value, from_ws) = s.get("notifications.enabled").unwrap();
        assert_eq!(value, json!(true));
        assert!(!from_ws);
        assert!(matches!(
            s.get("no.such.key"),
            Err(SettingsError::UnknownKey(_))
        ));
    }

    #[test]
    fn workspace_wins_over_user_over_default() {
        let td = tempfile::tempdir().unwrap();
        let ws = td.path().join("project");
        let s = store(td.path(), Some(&ws));

        // user scope set
        s.set("usage.retention_days", json!(30), SettingsScope::User)
            .unwrap();
        let (value, from_ws) = s.get("usage.retention_days").unwrap();
        assert_eq!(value, json!(30));
        assert!(!from_ws);

        // workspace override wins and is reported as such
        s.set("usage.retention_days", json!(7), SettingsScope::Workspace)
            .unwrap();
        let (value, from_ws) = s.get("usage.retention_days").unwrap();
        assert_eq!(value, json!(7));
        assert!(from_ws);

        // delete the workspace key → user value applies again
        assert!(s.delete("usage.retention_days", SettingsScope::Workspace).unwrap());
        let (value, from_ws) = s.get("usage.retention_days").unwrap();
        assert_eq!(value, json!(30));
        assert!(!from_ws);
    }

    #[test]
    fn update_is_a_scoped_atomic_patch() {
        let td = tempfile::tempdir().unwrap();
        let ws = td.path().join("project");
        let s = store(td.path(), Some(&ws));
        let patch: Map<String, Value> = serde_json::from_value(json!({
            "notifications.enabled": false,
            "downloads.prompt": true
        }))
        .unwrap();
        let diags = s.update(&patch, SettingsScope::Workspace).unwrap();
        assert!(diags.is_empty());

        // both keys effective; user scope untouched
        assert_eq!(s.get("notifications.enabled").unwrap().0, json!(false));
        assert_eq!(s.get("downloads.prompt").unwrap().0, json!(true));
        // no workspace file would exist if this had gone to the user scope
        let ws_map = s.read_scope(SettingsScope::Workspace);
        assert_eq!(ws_map.len(), 2);
    }

    #[test]
    fn undeclared_keys_stored_but_flagged() {
        let td = tempfile::tempdir().unwrap();
        let s = store(td.path(), None);
        let diags = s
            .set("future.key", json!("v"), SettingsScope::User)
            .unwrap();
        assert_eq!(diags.len(), 1);
        assert!(diags[0].contains("undeclared"));
        // forward compatible: still readable
        assert_eq!(s.get("future.key").unwrap().0, json!("v"));
    }

    #[test]
    fn effective_view_covers_catalog_and_extras() {
        let td = tempfile::tempdir().unwrap();
        let ws = td.path().join("project");
        std::fs::create_dir_all(ws.join(".okra")).unwrap();
        let s = store(td.path(), Some(&ws));
        s.set("downloads.prompt", json!(true), SettingsScope::Workspace)
            .unwrap();
        s.set("custom", json!(1), SettingsScope::User).unwrap();

        let effective = s.effective().unwrap();
        // every declared key resolves
        for d in catalog() {
            assert!(effective.contains_key(d.key), "missing {}", d.key);
        }
        assert_eq!(effective["downloads.prompt"], json!(true), "workspace wins");
        assert_eq!(effective["notifications.enabled"], json!(true), "default");
        assert_eq!(effective["custom"], json!(1), "extra keys included");
    }
}
