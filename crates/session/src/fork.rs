//! Session fork (MASTER-PLAN §3 #38: "fork mirrors tool schema for cache
//! reuse", from grok `handle.rs` `fork_session`): forking a session
//! derives the child's tool configuration from the parent's effective
//! configuration through a PURE function — same inputs, byte-identical
//! tool schema — so the sampler's prompt-cache prefix stays valid across
//! sibling forks.
//!
//! Donor contracts kept:
//! - fork requires an explicit parent (no implicit root parenting);
//! - **capability modes may narrow, never widen**: the child's set must
//!   be a subset of the parent's;
//! - **fork budget**: the child's depth is parent+1 and its budget is
//!   `parent.budget - 1` capped at the requested `max_depth`; an
//!   exhausted budget is `MaxDepthExceeded`;
//! - **tool-config inheritance**: the child uses its explicit override or
//!   the parent's effective config, then the active-agent messaging tool
//!   (the parent→child channel) is STRIPPED from the child — a subagent
//!   must not spawn subagent-message tools;
//! - **environment inheritance**: the parent's session env extended by
//!   the fork's extra env;
//! - the tool-schema fingerprint is sha256 over the canonical JSON of
//!   the effective tools array (serde_json sorts object keys; the tools
//!   ARRAY order is preserved because prompt-cache reuse depends on the
//!   exact wire order).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use okra_kernel as kernel;
use okra_kernel::SessionHandle;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use serde_json::Value;

/// The tool `kind` stripped from forked children (donor
/// `ToolKind::ActiveAgentMessage`): the parent→child messaging surface
/// exists only in the session that owns the children.
pub const ACTIVE_AGENT_MESSAGE_KIND: &str = "activeAgentMessage";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkConfig {
    /// The child's agent identity (donor: empty agent id is an error).
    pub agent_id: String,
    /// Capability modes; must be a SUBSET of the parent's.
    pub capability_mode: BTreeSet<String>,
    /// Ceiling on the child's own fork budget.
    pub max_depth: u32,
    /// Explicit tool-config override; absent = inherit the parent's
    /// effective config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_override: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkError {
    EmptyAgentId,
    ParentNotFound(String),
    CapabilityWidening {
        parent: BTreeSet<String>,
        child: BTreeSet<String>,
    },
    MaxDepthExceeded {
        parent_session: String,
    },
    Kernel(#[allow(dead_code)] String),
}

impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForkError::EmptyAgentId => write!(f, "fork requires a non-empty agent id"),
            ForkError::ParentNotFound(id) => write!(f, "parent session not found: {id}"),
            ForkError::CapabilityWidening { parent, child } => write!(
                f,
                "capability widening: parent {parent:?} does not include child {child:?}"
            ),
            ForkError::MaxDepthExceeded { parent_session } => {
                write!(f, "fork budget exhausted at parent {parent_session}")
            }
            ForkError::Kernel(msg) => write!(f, "kernel: {msg}"),
        }
    }
}

impl std::error::Error for ForkError {}

/// One session's fork-relevant state: identity, parent linkage, depth,
/// budget, capabilities, effective tool config, cwd, and env.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionForkState {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub depth: u32,
    pub fork_budget: u32,
    pub capability_mode: BTreeSet<String>,
    /// Effective tool config (the wire shape the sampler sees).
    pub effective_tool_config: Value,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

fn session_id(seed: &str) -> String {
    let hash = Sha256::digest(format!("{seed}:{}", now_nanos()).as_bytes());
    format!(
        "sess-{}",
        hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

impl SessionForkState {
    /// A root session: no parent, depth 0.
    pub fn root(
        session_id: impl Into<String>,
        cwd: impl Into<PathBuf>,
        capability_mode: BTreeSet<String>,
        fork_budget: u32,
        tool_config: Value,
        env: BTreeMap<String, String>,
    ) -> Self {
        SessionForkState {
            session_id: session_id.into(),
            parent_session_id: None,
            depth: 0,
            fork_budget,
            capability_mode,
            effective_tool_config: tool_config,
            cwd: cwd.into(),
            env,
        }
    }

    /// `fork_session`: derive the child state from this parent.
    pub fn fork(&self, config: &ForkConfig) -> Result<SessionForkState, ForkError> {
        if config.agent_id.is_empty() {
            return Err(ForkError::EmptyAgentId);
        }
        if !config
            .capability_mode
            .iter()
            .all(|mode| self.capability_mode.contains(mode))
        {
            return Err(ForkError::CapabilityWidening {
                parent: self.capability_mode.clone(),
                child: config.capability_mode.clone(),
            });
        }
        if self.fork_budget == 0 {
            return Err(ForkError::MaxDepthExceeded {
                parent_session: self.session_id.clone(),
            });
        }
        // tool-config inheritance + the active-agent strip
        let mut baseline = config
            .tool_config
            .clone()
            .unwrap_or_else(|| self.effective_tool_config.clone());
        if let Some(tools) = baseline.get_mut("tools").and_then(Value::as_array_mut) {
            tools.retain(|tool| tool.get("kind").and_then(Value::as_str) != Some(ACTIVE_AGENT_MESSAGE_KIND));
        }
        let mut env = self.env.clone();
        env.extend(config.extra_env.clone());
        Ok(SessionForkState {
            session_id: session_id(&config.agent_id),
            parent_session_id: Some(self.session_id.clone()),
            depth: self.depth.saturating_add(1),
            fork_budget: self
                .fork_budget
                .saturating_sub(1)
                .min(config.max_depth),
            capability_mode: config.capability_mode.clone(),
            effective_tool_config: baseline,
            cwd: config
                .cwd_override
                .clone()
                .unwrap_or_else(|| self.cwd.clone()),
            env,
        })
    }

    /// The tool-schema fingerprint: sha256 over the canonical JSON of the
    /// effective tools array. Two sessions with equal fingerprints present
    /// byte-identical tool blocks to the sampler — their prompt-cache
    /// prefixes over the tools segment are interchangeable.
    pub fn schema_fingerprint(&self) -> String {
        let empty = Vec::new();
        let tools = self
            .effective_tool_config
            .get("tools")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let hash = Sha256::digest(serde_json::to_string(tools).unwrap_or_default().as_bytes());
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Materialize the fork as a kernel session: the child's header links
    /// back to the parent (`parent_session`), inheriting the seeded flag.
    pub fn create_kernel_session(&self, sessions_dir: &Path) -> Result<SessionHandle, ForkError> {
        let header = kernel::SessionHeader {
            version: kernel::SESSION_FORMAT_VERSION,
            id: self.session_id.clone(),
            created_at: 0.0,
            cwd: self.cwd.to_string_lossy().into_owned(),
            parent_session: self.parent_session_id.clone(),
            is_seeded: false,
        };
        kernel::SessionHandle::create(sessions_dir, &header)
            .map_err(|e| ForkError::Kernel(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_config() -> Value {
        json!({
            "tools": [
                { "id": "read_file", "kind": "fs" },
                { "id": "send_subagent_message", "kind": "activeAgentMessage" },
                { "id": "write_file", "kind": "fs" }
            ]
        })
    }

    fn root() -> SessionForkState {
        SessionForkState::root(
            "sess-parent",
            "/work",
            BTreeSet::from(["full".to_string()]),
            2,
            tool_config(),
            BTreeMap::from([("HOME".to_string(), "/home/u".to_string())]),
        )
    }

    fn fork_config() -> ForkConfig {
        ForkConfig {
            agent_id: "sub-1".into(),
            capability_mode: BTreeSet::from(["full".to_string()]),
            max_depth: 4,
            tool_config: None,
            cwd_override: None,
            extra_env: BTreeMap::new(),
        }
    }

    #[test]
    fn fork_requires_agent_and_parent_budget() {
        let parent = root();
        // empty agent id
        let mut cfg = fork_config();
        cfg.agent_id = String::new();
        assert!(matches!(parent.fork(&cfg), Err(ForkError::EmptyAgentId)));

        // exhausted budget
        let mut exhausted = root();
        exhausted.fork_budget = 0;
        assert!(matches!(
            exhausted.fork(&fork_config()),
            Err(ForkError::MaxDepthExceeded { parent_session }) if parent_session == "sess-parent"
        ));
    }

    #[test]
    fn capability_modes_narrow_never_widen() {
        let parent = root();
        let mut cfg = fork_config();
        cfg.capability_mode = BTreeSet::from(["full".to_string(), "network".to_string()]);
        assert!(matches!(
            parent.fork(&cfg),
            Err(ForkError::CapabilityWidening { .. })
        ));
        // narrowing is fine
        cfg.capability_mode = BTreeSet::new();
        assert!(parent.fork(&cfg).is_ok());
    }

    #[test]
    fn child_inherits_config_and_strips_active_agent_tool() {
        let parent = root();
        let child = parent.fork(&fork_config()).unwrap();

        // parent linkage + depth/budget bookkeeping
        assert_eq!(child.parent_session_id.as_deref(), Some("sess-parent"));
        assert_eq!(child.depth, 1);
        assert_eq!(child.fork_budget, 1, "parent budget 2 - 1, capped at max_depth 4");
        assert_eq!(child.cwd, Path::new("/work"));
        assert_eq!(child.env.get("HOME").map(String::as_str), Some("/home/u"));

        // the child's tool config has NO active-agent tool
        let tools = child.effective_tool_config["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools
            .iter()
            .all(|t| t["kind"] != json!(ACTIVE_AGENT_MESSAGE_KIND)));
        // the parent keeps it
        assert_eq!(
            parent.effective_tool_config["tools"].as_array().unwrap().len(),
            3
        );
    }

    #[test]
    fn sibling_forks_mirror_the_tool_schema_for_cache_reuse() {
        let parent = root();
        let a = parent.fork(&fork_config()).unwrap();
        let b = parent.fork(&fork_config()).unwrap();
        // byte-identical tool schemas across siblings → cache prefix valid
        assert_eq!(a.schema_fingerprint(), b.schema_fingerprint());
        assert_eq!(
            a.effective_tool_config["tools"],
            b.effective_tool_config["tools"]
        );

        // a different tool config produces a different fingerprint
        let mut cfg = fork_config();
        cfg.tool_config = Some(json!({ "tools": [{ "id": "read_file", "kind": "fs" }] }));
        let c = parent.fork(&cfg).unwrap();
        assert_ne!(a.schema_fingerprint(), c.schema_fingerprint());

        // the fingerprint is deterministic across constructions
        let a2 = parent.fork(&fork_config()).unwrap();
        assert_eq!(a.schema_fingerprint(), a2.schema_fingerprint());
    }

    #[test]
    fn env_inheritance_and_cwd_override() {
        let parent = root();
        let mut cfg = fork_config();
        cfg.cwd_override = Some(PathBuf::from("/scratch/sub"));
        cfg.extra_env = BTreeMap::from([("OKRA_TASK".to_string(), "42".to_string())]);
        let child = parent.fork(&cfg).unwrap();
        assert_eq!(child.cwd, Path::new("/scratch/sub"));
        assert_eq!(child.env.get("HOME").map(String::as_str), Some("/home/u"));
        assert_eq!(child.env.get("OKRA_TASK").map(String::as_str), Some("42"));
    }

    #[test]
    fn budget_caps_at_requested_max_depth() {
        let parent = root();
        let mut cfg = fork_config();
        cfg.max_depth = 1;
        let child = parent.fork(&cfg).unwrap();
        assert_eq!(child.fork_budget, 1, "min(parent 2 - 1, max_depth 1)");
    }

    #[test]
    fn forked_kernel_session_links_parent() {
        let td = tempfile::tempdir().unwrap();
        let parent = root();
        let child = parent.fork(&fork_config()).unwrap();
        let handle = child.create_kernel_session(td.path()).unwrap();
        let header = handle.header().unwrap();
        assert_eq!(header.parent_session.as_deref(), Some("sess-parent"));
        assert_eq!(header.id, child.session_id);
    }
}
