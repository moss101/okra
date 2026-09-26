//! External hooks — port of kimi `externalHooks/types.ts:3-24` (the
//! 20-event set) + qwen hook config + deepseek containment
//! (MASTER-PLAN §3 #45).
//!
//! - **20-event closed set** (`HOOK_EVENT_TYPES`, kimi names verbatim).
//! - **command/HTTP kinds**: a hook runs a subprocess (sanctioned runner in
//!   `process.rs`) or posts a webhook; the verdict comes back as JSON
//!   (`{"action": "allow"|"block"|"ask", "reason"}`).
//! - **deny > ask > allow**: when several hooks match one event, the
//!   combined verdict follows that fixed precedence.
//! - **hooks never crash a turn**: a failing/timeout hook is contained as
//!   `Observe` with a failure counter — never an error out of the dispatch
//!   pipeline.
//! - **prompt gate**: an `Ask` verdict routes through the approval service
//!   (prompt gate) in the executor — a hook can force a prompt but cannot
//!   grant by itself.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Handler signature for in-process runners.
pub type HookHandlerFn =
    dyn Fn(&HookDef, &Value) -> Result<HookVerdict, String> + Send + Sync;
use serde_json::Value;
use std::sync::{Arc, Mutex};

use crate::process;

/// `HOOK_EVENT_TYPES` (`externalHooks/types.ts:3-24`), kimi names verbatim.
pub const HOOK_EVENT_TYPES: [&str; 20] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionResult",
    "UserPromptSubmit",
    "UserPromptQueued",
    "TurnStarted",
    "Stop",
    "StopFailure",
    "Interrupt",
    "SessionStart",
    "SessionEnd",
    "SessionHeartbeat",
    "SubagentStart",
    "SubagentStop",
    "TaskStarted",
    "PreCompact",
    "PostCompact",
    "Notification",
];

pub fn is_valid_event(event: &str) -> bool {
    HOOK_EVENT_TYPES.contains(&event)
}

/// What a hook does when its event fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookAction {
    /// Run a program with the event payload on stdin; the JSON stdout
    /// carries the verdict.
    Command { program: String, args: Vec<String> },
    /// POST the event payload as JSON; the response body carries the
    /// verdict.
    Http { url: String },
}

/// The effect a hook requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEffect {
    Deny,
    Ask,
    Allow,
    Observe,
}

/// A configured hook: which event, which tool matcher, what to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookDef {
    pub event: String,
    /// Tool-name matcher for tool events; absent = matches all tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub action: HookAction,
    pub effect: HookEffect,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    5_000
}

/// The outcome of ONE hook execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookVerdict {
    Deny { reason: String },
    Ask { reason: String },
    Allow,
    /// Fired, produced no gate decision (e.g. PostToolUse telemetry).
    Observe,
}

/// Combining verdicts: deny > ask > allow > observe (`deny > ask > allow`,
/// MASTER-PLAN #45).
pub fn combine(verdicts: &[HookVerdict]) -> HookVerdict {
    let mut best = HookVerdict::Observe;
    for v in verdicts {
        let rank = |v: &HookVerdict| match v {
            HookVerdict::Deny { .. } => 3,
            HookVerdict::Ask { .. } => 2,
            HookVerdict::Allow => 1,
            HookVerdict::Observe => 0,
        };
        if rank(v) > rank(&best) {
            best = v.clone();
        }
    }
    best
}

/// Runs a hook's action. Real runners: subprocess + HTTP (here); tests can
/// inject closures.
pub trait HookRunner: Send + Sync {
    fn run(&self, def: &HookDef, payload: &Value) -> Result<HookVerdict, String>;
}

fn verdict_from_output(stdout: &str) -> HookVerdict {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return HookVerdict::Observe;
    }
    #[derive(Deserialize)]
    struct Wire {
        #[serde(default)]
        action: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }
    match serde_json::from_str::<Wire>(trimmed) {
        Ok(w) => match w.action.as_deref() {
            Some("block") | Some("deny") => HookVerdict::Deny {
                reason: w.reason.unwrap_or_else(|| "hook blocked".into()),
            },
            Some("ask") => HookVerdict::Ask {
                reason: w.reason.unwrap_or_else(|| "hook asks".into()),
            },
            _ => HookVerdict::Observe,
        },
        Err(_) => HookVerdict::Observe, // non-JSON output = telemetry only
    }
}

/// Real runner: command (sanctioned subprocess, timeout) or HTTP webhook.
#[derive(Debug, Default)]
pub struct RealHookRunner;

impl HookRunner for RealHookRunner {
    fn run(&self, def: &HookDef, payload: &Value) -> Result<HookVerdict, String> {
        let payload = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
        match &def.action {
            HookAction::Command { program, args } => {
                let out = process::run_captured(
                    program,
                    args,
                    Duration::from_millis(def.timeout_ms),
                    Some(&payload),
                );
                if out.timed_out {
                    return Err(format!("hook timed out after {}ms", def.timeout_ms));
                }
                Ok(verdict_from_output(&out.stdout))
            }
            HookAction::Http { url } => {
                let (status, body) = process::http_post_json(
                    url,
                    &payload,
                    Duration::from_millis(def.timeout_ms),
                )?;
                if !(200..300).contains(&status) {
                    return Err(format!("hook http status {status}"));
                }
                Ok(verdict_from_output(&body))
            }
        }
    }
}

/// In-process runner for tests/offline hosts: maps (event, tool) → verdict.
pub struct ClosureHookRunner {
    pub handler: Box<HookHandlerFn>,
}

impl HookRunner for ClosureHookRunner {
    fn run(&self, def: &HookDef, payload: &Value) -> Result<HookVerdict, String> {
        (self.handler)(def, payload)
    }
}

/// The hook system: registered defs + one runner; emits events and folds
/// verdicts with deny > ask > allow. Failure containment lives here.
#[derive(Default)]
pub struct HookSystem {
    defs: Vec<HookDef>,
    runner: Option<Arc<dyn HookRunner>>,
    events_fired: Mutex<u64>,
    failures: Mutex<u64>,
}

impl HookSystem {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with_runner(runner: Arc<dyn HookRunner>) -> Self {
        let mut s = Self::new();
        s.runner = Some(runner);
        s
    }

    pub fn register(&mut self, def: HookDef) -> Result<(), String> {
        if !is_valid_event(&def.event) {
            return Err(format!("unknown hook event `{}`", def.event));
        }
        self.defs.push(def);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    pub fn events_fired(&self) -> u64 {
        *self.events_fired.lock().unwrap()
    }

    pub fn failures(&self) -> u64 {
        *self.failures.lock().unwrap()
    }

    /// Fire an event: run every matching hook, fold verdicts. A runner
    /// failure on ANY hook is contained (counted, treated as Observe) —
    /// hooks never crash a turn. When no runner is installed, hooks that
    /// would run are counted but cannot produce verdicts.
    pub fn emit(&self, event: &str, tool: Option<&str>, payload: &Value) -> HookVerdict {
        debug_assert!(is_valid_event(event));
        let matching: Vec<&HookDef> = self
            .defs
            .iter()
            .filter(|d| d.event == event)
            .filter(|d| match (&d.matcher, tool) {
                (Some(m), Some(t)) => t == m.as_str(),
                (Some(_), None) => false,
                (None, _) => true,
            })
            .collect();
        if matching.is_empty() {
            return HookVerdict::Observe;
        }
        *self.events_fired.lock().unwrap() += 1;
        let Some(runner) = &self.runner else {
            return HookVerdict::Observe;
        };
        let mut verdicts = Vec::new();
        for def in matching {
            match runner.run(def, payload) {
                Ok(v) => verdicts.push(v),
                Err(e) => {
                    *self.failures.lock().unwrap() += 1;
                    verdicts.push(HookVerdict::Observe); // contained
                    let _ = e;
                }
            }
        }
        combine(&verdicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(event: &str, effect: HookEffect) -> HookDef {
        HookDef {
            event: event.into(),
            matcher: None,
            action: HookAction::Command { program: "true".into(), args: vec![] },
            effect,
            timeout_ms: 5_000,
        }
    }

    #[test]
    fn event_set_is_the_donor_s_twenty() {
        assert_eq!(HOOK_EVENT_TYPES.len(), 20);
        assert!(is_valid_event("PreToolUse"));
        assert!(is_valid_event("PostCompact"));
        assert!(!is_valid_event("MadeUpEvent"));
    }

    #[test]
    fn verdict_precedence_deny_over_ask_over_allow() {
        use HookVerdict::*;
        assert!(matches!(
            combine(&[Allow, Ask { reason: "a".into() }, Deny { reason: "d".into() }]),
            Deny { .. }
        ));
        assert!(matches!(
            combine(&[Allow, Ask { reason: "a".into() }]),
            Ask { .. }
        ));
        assert!(matches!(combine(&[Allow, Observe]), Allow));
        assert!(matches!(combine(&[]), Observe));
    }

    #[test]
    fn hook_failures_are_contained_never_fatal() {
        let mut hooks = HookSystem::new();
        hooks.register(def("PreToolUse", HookEffect::Deny)).unwrap();
        hooks.runner = Some(Arc::new(ClosureHookRunner {
            handler: Box::new(|_d, _p| Err("boom".into())),
        }));
        let verdict = hooks.emit(
            "PreToolUse",
            Some("read_file"),
            &serde_json::json!({ "tool": "read_file" }),
        );
        assert!(matches!(verdict, HookVerdict::Observe), "contained");
        assert_eq!(hooks.failures(), 1);
    }
}

#[cfg(test)]
mod real_runner_tests {
    use super::*;

    #[test]
    fn command_hook_produces_ask_verdict_through_a_real_subprocess() {
        let mut hooks = HookSystem::new();
        hooks.register(HookDef {
            event: "PreToolUse".into(),
            matcher: None,
            action: HookAction::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "echo '{\"action\":\"ask\",\"reason\":\"confirm with user\"}'".into()],
            },
            effect: HookEffect::Ask,
            timeout_ms: 5_000,
        }).unwrap();
        hooks.runner = Some(Arc::new(RealHookRunner));
        let verdict = hooks.emit("PreToolUse", Some("write_file"), &serde_json::json!({}));
        assert!(
            matches!(verdict, HookVerdict::Ask { ref reason } if reason.contains("confirm")),
            "{verdict:?}"
        );
    }

    #[test]
    fn command_timeout_is_contained() {
        let mut hooks = HookSystem::new();
        hooks.register(HookDef {
            event: "PreToolUse".into(),
            matcher: None,
            action: HookAction::Command {
                program: "sleep".into(),
                args: vec!["5".into()],
            },
            effect: HookEffect::Deny,
            timeout_ms: 200,
        }).unwrap();
        hooks.runner = Some(Arc::new(RealHookRunner));
        let verdict = hooks.emit("PreToolUse", Some("bash"), &serde_json::json!({}));
        assert!(matches!(verdict, HookVerdict::Observe), "timeout contained: {verdict:?}");
        assert_eq!(hooks.failures(), 1);
    }
}
