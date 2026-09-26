//! okra-computer — computer/browser control contracts (MASTER-PLAN §3 #60,
//! Claude2 study docs/02-03, M5). The MCP servers land in M5; M1 carries
//! the load-bearing contracts so hosts cannot invent weaker variants:
//!
//! - **split consent**: per-app capability grants are SEPARATE from
//!   screen-takeover consent (the glow). One never implies the other.
//! - **batch families**: a batch executes in order, stopping at the first
//!   error (stop-on-first-error).
//! - **pixel guard**: pixel-based operations fail HONESTLY when vision
//!   confidence is low, instead of clicking blind.
//! - **no-raise mode**: background app control must not steal focus.
//! - **user_actively_typing** guard: input injection pauses while the user
//!   types.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentKind {
    /// Per-app automation capability (e.g. control Terminal).
    AppCapability,
    /// Screen recording / takeover consent (the glow).
    ScreenTakeover,
    /// Browser control with per-conversation partition.
    BrowserPartition,
}

/// Consent ledger: split by kind; granting one kind never grants another.
#[derive(Debug, Default)]
pub struct ConsentLedger {
    granted: Vec<(ConsentKind, String)>,
}

impl ConsentLedger {
    pub fn grant(&mut self, kind: ConsentKind, subject: &str) {
        if !self.has(kind, subject) {
            self.granted.push((kind, subject.to_string()));
        }
    }

    pub fn has(&self, kind: ConsentKind, subject: &str) -> bool {
        self.granted.iter().any(|(k, s)| *k == kind && s == subject)
    }
}

/// One element-targeted action (AX-tree-first: elements, never pixels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AxAction {
    Click { app: String, element_id: String },
    Type { app: String, element_id: String, text: String },
    PressKey { app: String, key: String },
    Scroll { app: String, element_id: String, dx: i32, dy: i32 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AxActionResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Re-observation after the action (diffed state, AX-first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<serde_json::Value>,
}

/// Execute a batch with stop-on-first-error; every executed action is
/// followed by a re-observe (element targeting + diffed state).
pub fn execute_batch(
    actions: &[AxAction],
    consent: &ConsentLedger,
    user_actively_typing: bool,
) -> Vec<AxActionResult> {
    let mut results = Vec::new();
    for action in actions {
        let app = match action {
            AxAction::Click { app, .. }
            | AxAction::Type { app, .. }
            | AxAction::PressKey { app, .. }
            | AxAction::Scroll { app, .. } => app,
        };
        // split consent: app capability required per app
        if !consent.has(ConsentKind::AppCapability, app) {
            results.push(AxActionResult {
                ok: false,
                error: Some(format!("no app capability grant for {app}")),
                observed: None,
            });
            break; // stop-on-first-error
        }
        // user_actively_typing guard: refuse input injection while typing
        if user_actively_typing && matches!(action, AxAction::Type { .. } | AxAction::PressKey { .. }) {
            results.push(AxActionResult {
                ok: false,
                error: Some("user_actively_typing: input injection paused".into()),
                observed: None,
            });
            break;
        }
        // the real executor lands in M5; the contract shape is what M1 pins
        results.push(AxActionResult { ok: true, error: None, observed: Some(serde_json::json!({})) });
    }
    results
}

/// Pixel guard: honest failure text when vision confidence is low — the
/// agent must re-observe, not click blind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelGuard;

impl PixelGuard {
    pub const MIN_CONFIDENCE: f32 = 0.85;

    pub fn check(confidence: f32) -> Result<(), String> {
        if confidence >= Self::MIN_CONFIDENCE {
            Ok(())
        } else {
            Err(format!(
                "pixel confidence {confidence:.2} below {0:.2}; the target element cannot be \
                 located reliably. Re-observe the accessibility tree instead of retrying blindly.",
                Self::MIN_CONFIDENCE
            ))
        }
    }
}

/// No-raise background control: app_* background mode must not steal focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRaiseMode {
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_kinds_are_split() {
        let mut ledger = ConsentLedger::default();
        ledger.grant(ConsentKind::AppCapability, "Terminal");
        assert!(ledger.has(ConsentKind::AppCapability, "Terminal"));
        // capability does NOT imply takeover or browser
        assert!(!ledger.has(ConsentKind::ScreenTakeover, "Terminal"));
        assert!(!ledger.has(ConsentKind::BrowserPartition, "Safari"));
    }

    #[test]
    fn batch_stops_at_first_error() {
        let mut ledger = ConsentLedger::default();
        ledger.grant(ConsentKind::AppCapability, "Finder");
        let actions = vec![
            AxAction::Click { app: "Finder".into(), element_id: "e1".into() },
            AxAction::Click { app: "Terminal".into(), element_id: "e2".into() }, // ungranted
            AxAction::Click { app: "Finder".into(), element_id: "e3".into() },
        ];
        let results = execute_batch(&actions, &ledger, false);
        assert_eq!(results.len(), 2, "stopped at first error");
        assert!(results[0].ok);
        assert!(!results[1].ok);
        assert!(results[1].error.as_deref().unwrap().contains("no app capability grant"));
    }

    #[test]
    fn typing_guard_pauses_injection() {
        let mut ledger = ConsentLedger::default();
        ledger.grant(ConsentKind::AppCapability, "Notes");
        let actions = vec![AxAction::Type { app: "Notes".into(), element_id: "e".into(), text: "hi".into() }];
        let results = execute_batch(&actions, &ledger, true);
        assert!(!results[0].ok);
        assert!(results[0].error.as_deref().unwrap().contains("user_actively_typing"));
        // not typing → proceeds
        let results = execute_batch(&actions, &ledger, false);
        assert!(results[0].ok);
    }

    #[test]
    fn pixel_guard_fails_honestly() {
        assert!(PixelGuard::check(0.95).is_ok());
        let err = PixelGuard::check(0.4).unwrap_err();
        assert!(err.contains("Re-observe"), "{err}");
    }
}
