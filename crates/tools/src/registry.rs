//! Erased tool dispatch + registry — grok `ToolDyn`/`ToolDispatch`
//! (`tool.rs:322-350`, `dispatch.rs:32-68`) flattened for the sync runtime:
//! a tool is a boxed fn from approved args to a validated `ToolStream`.

use serde_json::Value;
use std::collections::HashMap;

use crate::pipeline::{normalize_before_hooks, ArgumentNormalizer, PipelineError, PreToolUseHook};
use crate::scheduler::ToolAccesses;
use crate::spec::ToolEntry;
use crate::stream::ToolStream;

/// The erased tool: executes APPROVED bytes only.
pub struct ErasedTool {
    pub entry: ToolEntry,
    /// Declared footprint for the scheduler (kimi semantics).
    pub accesses_for: Box<dyn Fn(&Value) -> ToolAccesses + Send + Sync>,
    pub execute: Box<dyn Fn(&Value) -> ToolStream + Send + Sync>,
}

impl std::fmt::Debug for ErasedTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErasedTool")
            .field("name", &self.entry.spec.name)
            .field("idempotent", &self.entry.spec.idempotent)
            .finish()
    }
}

impl ErasedTool {
    pub fn simple(
        entry: ToolEntry,
        accesses: ToolAccesses,
        execute: impl Fn(&Value) -> ToolStream + Send + Sync + 'static,
    ) -> Self {
        ErasedTool {
            entry,
            accesses_for: Box::new(move |_| accesses.clone()),
            execute: Box::new(execute),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("hook asks for approval on `{tool}`: {reason}")]
    HookAsk { tool: String, reason: String },
    #[error("duplicate tool `{0}`")]
    Duplicate(String),
    #[error("pipeline: {0}")]
    Pipeline(#[from] PipelineError),
    #[error("stream protocol violation: {0}")]
    StreamProtocol(String),
}

/// The tool registry + dispatch funnel: every call — whatever the source —
/// goes through `dispatch`, which runs normalize→hooks→execute and validates
/// the stream invariant. This is the M0 shape of grok's
/// "use_tool-style single dispatch funnel" (MASTER-PLAN §3 #47).
pub struct Registry {
    tools: HashMap<String, ErasedTool>,
    normalizers: Vec<Box<dyn ArgumentNormalizer>>,
    hooks: Vec<Box<dyn PreToolUseHook>>,
    /// External hook system (#45): 20-event set, deny>ask>allow, contained
    /// failures. Wired into the dispatch funnel below.
    pub hook_system: crate::hooks::HookSystem,
    /// When a hook verdict is Ask, dispatch returns this error and the
    /// executor routes it through the approval service (prompt gate).
    pub hook_ask_rejects: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            tools: HashMap::new(),
            normalizers: Vec::new(),
            hooks: Vec::new(),
            hook_system: crate::hooks::HookSystem::new(),
            hook_ask_rejects: false,
        }
    }

    pub fn register(&mut self, tool: ErasedTool) -> Result<(), RegistryError> {
        let name = tool.entry.spec.name.clone();
        if self.tools.insert(name.clone(), tool).is_some() {
            return Err(RegistryError::Duplicate(name));
        }
        Ok(())
    }

    pub fn add_normalizer(&mut self, n: Box<dyn ArgumentNormalizer>) {
        self.normalizers.push(n);
    }

    pub fn add_hook(&mut self, h: Box<dyn PreToolUseHook>) {
        self.hooks.push(h);
    }

    pub fn normalizers_iter(&self) -> impl Iterator<Item = &dyn ArgumentNormalizer> {
        self.normalizers.iter().map(|b| b.as_ref())
    }

    pub fn hooks_iter(&self) -> impl Iterator<Item = &dyn PreToolUseHook> {
        self.hooks.iter().map(|b| b.as_ref())
    }

    /// Keep only tools whose name matches one of `patterns` (exact or
    /// `*` wildcard — the headless `--tools` filter, MASTER-PLAN #56).
    /// Returns the dropped names (for honest CLI reporting).
    pub fn retain_matching(&mut self, patterns: &[String]) -> Vec<String> {
        let matches = |n: &str| patterns.iter().any(|p| star_match(p, n));
        let dropped: Vec<String> = self
            .tools
            .keys()
            .filter(|n| !matches(n))
            .cloned()
            .collect();
        self.tools.retain(|n, _| matches(n));
        dropped
    }

    pub fn entries(&self) -> Vec<&ToolEntry> {
        let mut v: Vec<&ToolEntry> = self.tools.values().map(|t| &t.entry).collect();
        v.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        v
    }

    pub fn get(&self, name: &str) -> Result<&ErasedTool, RegistryError> {
        self.tools.get(name).ok_or_else(|| RegistryError::UnknownTool(name.to_string()))
    }

    /// normalize → hooks → execute(approved bytes), then validate the stream
    /// invariant ([Progress*, exactly one Terminal]).
    pub fn dispatch(
        &self,
        tool_name: &str,
        raw_args: &Value,
        extra_hooks: &[&dyn PreToolUseHook],
    ) -> Result<ToolStream, RegistryError> {
        self.dispatch_inner(tool_name, raw_args, extra_hooks, false)
    }

    fn dispatch_inner(
        &self,
        tool_name: &str,
        raw_args: &Value,
        extra_hooks: &[&dyn PreToolUseHook],
        allow_ask: bool,
    ) -> Result<ToolStream, RegistryError> {
        let tool = self.get(tool_name)?;
        let normalizers: Vec<&dyn ArgumentNormalizer> =
            self.normalizers.iter().map(|b| b.as_ref()).collect();
        let mut hooks: Vec<&dyn PreToolUseHook> =
            self.hooks.iter().map(|b| b.as_ref()).collect();
        for h in extra_hooks {
            hooks.push(*h);
        }
        let approved =
            normalize_before_hooks(&tool.entry, raw_args, &normalizers, &hooks)?;

        // external hooks (#45): PreToolUse — deny > ask > allow. Deny is
        // fatal; Ask surfaces as HookAsk for the executor's prompt gate.
        let pre_verdict = self.hook_system.emit(
            "PreToolUse",
            Some(tool_name),
            &serde_json::json!({ "tool": tool_name, "args": approved.args() }),
        );
        match pre_verdict {
            crate::hooks::HookVerdict::Deny { reason } => {
                return Err(RegistryError::Pipeline(PipelineError::HookDenied {
                    hook: "hooks:PreToolUse".into(),
                    tool: tool_name.into(),
                    reason,
                }));
            }
            crate::hooks::HookVerdict::Ask { reason }
                if self.hook_ask_rejects && !allow_ask =>
            {
                return Err(RegistryError::HookAsk { tool: tool_name.into(), reason });
            }
            _ => {}
        }

        let args = approved.args();
        let stream = (tool.execute)(&args);
        stream.validate().map_err(|e| RegistryError::StreamProtocol(match &e {
            crate::stream::ToolError::Custom { code, message } => format!("{code}: {message}"),
            other => format!("{other:?}"),
        }))?;

        // PostToolUse: observe/deny-capable telemetry; never fatal here
        let _ = self.hook_system.emit(
            "PostToolUse",
            Some(tool_name),
            &serde_json::json!({ "tool": tool_name }),
        );
        Ok(stream)
    }

    /// Like `dispatch`, but an Ask verdict from the external hook system is
    /// treated as satisfied (the caller recorded the prompt outcome) instead
    /// of surfacing `HookAsk` again.
    pub fn dispatch_allow_ask(
        &self,
        tool_name: &str,
        raw_args: &Value,
        extra_hooks: &[&dyn PreToolUseHook],
    ) -> Result<ToolStream, RegistryError> {
        match self.dispatch_inner(tool_name, raw_args, extra_hooks, true) {
            Err(RegistryError::HookAsk { .. }) => {
                // a hook still asks even with the override: obey it
                self.dispatch_inner(tool_name, raw_args, extra_hooks, false)
            }
            other => other,
        }
    }

    /// Registry-scoped hook verdict without executing (policy probes).
    pub fn probe(&self, tool_name: &str, raw_args: &Value) -> Result<Vec<String>, RegistryError> {
        let tool = self.get(tool_name)?;
        let normalizers: Vec<&dyn ArgumentNormalizer> =
            self.normalizers.iter().map(|b| b.as_ref()).collect();
        let hooks: Vec<&dyn PreToolUseHook> = self.hooks.iter().map(|b| b.as_ref()).collect();
        let approved =
            normalize_before_hooks(&tool.entry, raw_args, &normalizers, &hooks)?;
        Ok(approved.approved_by)
    }
}

/// `*`-only wildcard match (existence semantics: `a*bc` matches any text
/// starting with `a` and ending with `bc`).
fn star_match(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == b'*' {
            let mut rest = 1;
            while rest < p.len() && p[rest] == b'*' {
                rest += 1;
            }
            if rest == p.len() {
                return true;
            }
            return (0..=t.len()).any(|j| rec(&p[rest..], &t[j..]));
        }
        !t.is_empty() && p[0] == t[0] && rec(&p[1..], &t[1..])
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{ToolMetadata, ToolSpec};

    fn named_registry(names: &[&str]) -> Registry {
        let mut r = Registry::new();
        for n in names {
            let spec = ToolSpec {
                name: (*n).into(),
                description: String::new(),
                arguments_schema: None,
                namespace: None,
                title: None,
                kind: None,
                behavior_version: None,
                idempotent: true,
                read_only: true,
                timeout_ms: None,
                max_concurrency: None,
            };
            r.register(ErasedTool::simple(
                ToolEntry::new(spec, ToolMetadata::default()),
                vec![],
                |_| ToolStream::terminal_only(Ok(crate::ToolOutput::text(""))),
            ))
            .unwrap();
        }
        r
    }

    #[test]
    fn star_match_semantics() {
        assert!(star_match("read_file", "read_file"));
        assert!(!star_match("read_file", "write_file"));
        assert!(star_match("read_*", "read_file"));
        assert!(star_match("*_file", "read_file"));
        assert!(star_match("a*bc", "abcbc"), "ends-with anchoring");
        assert!(!star_match("*_file", "read_dir"));
        assert!(star_match("*", "anything"));
    }

    #[test]
    fn retain_matching_filters_and_reports_drops() {
        let mut r = named_registry(&["read_file", "list_dir", "write_file", "edit_file"]);
        let dropped = r.retain_matching(&["read_*".to_string(), "list_dir".to_string()]);
        assert_eq!(drobed_sorted(dropped), vec!["edit_file", "write_file"]);
        let names: Vec<String> = r.entries().iter().map(|e| e.spec.name.clone()).collect();
        assert_eq!(names, vec!["list_dir", "read_file"]);
    }

    fn drobed_sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }
}
