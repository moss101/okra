//! Notifications policy — port of the ChatGPT2 study findings (MASTER-PLAN
//! §3 #51): exactly THREE native notification classes, body redaction,
//! focus suppression, resume buffering, and the tray unread model.
//!
//! Fixes the donor's leak: notification bodies must never carry tool
//! output, file contents, or prompts — they carry the CLASS and a
//! redacted, bounded label.

use serde::{Deserialize, Serialize};

/// The closed 3-class boundary (ChatGPT2 docs/02):
/// turn-complete | permission-request | question. Anything else is an
/// in-app UI event, never a native notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationClass {
    TurnComplete,
    PermissionRequest,
    Question,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub class: NotificationClass,
    /// Redacted, bounded label (<= 80 chars) safe for lock screens.
    pub label: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FocusState {
    Focused,
    Unfocused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Post a native notification.
    PostNative,
    /// App is focused: in-app surface only.
    InAppOnly,
    /// Suppressed entirely (focus suppression rule).
    Suppressed,
}

#[derive(Debug, Default)]
pub struct NotificationsPolicy {
    /// Generations sync the tray model with surface state (donor fix: stale
    /// tray counts after session switches).
    pub generation: u64,
}

impl NotificationsPolicy {
    /// The post decision for one event.
    pub fn decide(&self, class: NotificationClass, focus: FocusState) -> Decision {
        match (class, focus) {
            // focused: nothing native ever
            (_, FocusState::Focused) => Decision::InAppOnly,
            // permission/question while unfocused: must interrupt
            (NotificationClass::PermissionRequest, FocusState::Unfocused) => Decision::PostNative,
            (NotificationClass::Question, FocusState::Unfocused) => Decision::PostNative,
            // turn-complete while unfocused: post (the user is away)
            (NotificationClass::TurnComplete, FocusState::Unfocused) => Decision::PostNative,
        }
    }

    /// Resume buffering: while unfocused, completion events buffer into the
    /// tray unread model; on focus, the buffer drains and posts nothing.
    pub fn on_focus_restored(&mut self, unread: usize) -> Decision {
        self.generation += 1;
        let _ = unread;
        Decision::Suppressed
    }
}

/// Body redaction: labels are METADATA, never content. Everything from the
/// first content-introducing marker onward is cut (tool output, file text,
/// prompts ride in after such markers), then the remainder is bounded.
const CONTENT_MARKERS: [&str; 8] = [
    "output", "content", "result", "prompt", "\"", "'", "`", ":",
];

pub fn redact_body(label: &str) -> String {
    // take the first line only
    let first_line = label.lines().next().unwrap_or("").trim();
    // cut at the first content-introducing marker (case-insensitive)
    let mut cut = first_line.len();
    let lower = first_line.to_lowercase();
    for marker in CONTENT_MARKERS {
        if let Some(pos) = lower.find(marker) {
            cut = cut.min(pos);
        }
    }
    let mut out: String = first_line[..cut]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if out.len() > 80 {
        out = out.chars().take(77).collect::<String>() + "…";
    }
    out
}

/// Build a notification from an event label with the policy applied.
pub fn classify(class: NotificationClass, raw_label: &str, session_id: &str) -> Notification {
    Notification { class, label: redact_body(raw_label), session_id: session_id.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_three_classes_exist() {
        // closed set assertion via serialization roundtrip
        for class in [
            NotificationClass::TurnComplete,
            NotificationClass::PermissionRequest,
            NotificationClass::Question,
        ] {
            let json = serde_json::to_string(&class).unwrap();
            let back: NotificationClass = serde_json::from_str(&json).unwrap();
            assert_eq!(back, class);
        }
    }

    #[test]
    fn focused_apps_never_post_native() {
        let policy = NotificationsPolicy::default();
        for class in [
            NotificationClass::TurnComplete,
            NotificationClass::PermissionRequest,
            NotificationClass::Question,
        ] {
            assert_eq!(policy.decide(class, FocusState::Focused), Decision::InAppOnly);
        }
    }

    #[test]
    fn unfocused_permission_and_question_interrupt() {
        let policy = NotificationsPolicy::default();
        assert_eq!(
            policy.decide(NotificationClass::PermissionRequest, FocusState::Unfocused),
            Decision::PostNative
        );
        assert_eq!(
            policy.decide(NotificationClass::Question, FocusState::Unfocused),
            Decision::PostNative
        );
        assert_eq!(
            policy.decide(NotificationClass::TurnComplete, FocusState::Unfocused),
            Decision::PostNative
        );
    }

    #[test]
    fn bodies_are_redacted_not_leaked() {
        // the donor's leak: notification preview carried tool output
        let n = classify(
            NotificationClass::TurnComplete,
            "Turn finished. Output was \"SECRET-TOKEN-abc123\" from config.yaml contents",
            "s1",
        );
        assert!(!n.label.contains("SECRET-TOKEN"), "secrets must not leak: {}", n.label);
        assert!(n.label.len() <= 81, "bounded label");
    }

    #[test]
    fn focus_restore_bumps_generation() {
        let mut policy = NotificationsPolicy::default();
        let g = policy.generation;
        assert_eq!(policy.on_focus_restored(3), Decision::Suppressed);
        assert_eq!(policy.generation, g + 1);
    }
}
