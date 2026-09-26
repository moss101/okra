//! Byte-stable `world_state` prompt projection (MASTER-PLAN §3 #32, Codex
//! dossier): the model-facing state section serializes to IDENTICAL bytes
//! when content is unchanged, so provider prefix caches hit.
//!
//! Rules:
//! - fixed section ORDER (never sorted by runtime state),
//! - fixed field order (serialize via ordered Vec of pairs, not HashMap),
//! - unchanged content → unchanged bytes; additions append only at section
//!   ends with their own stable framing.

/// One projection section, in canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Workspace,
    SessionGoal,
    OpenTodos,
    FileStates,
    Skills,
    Permissions,
    Environment,
}

pub const SECTION_ORDER: [Section; 7] = [
    Section::Workspace,
    Section::SessionGoal,
    Section::OpenTodos,
    Section::FileStates,
    Section::Skills,
    Section::Permissions,
    Section::Environment,
];

/// A key-value entry (insertion-ordered; NEVER re-sorted between renders).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorldState {
    pub sections: Vec<(Section, Vec<Entry>)>,
}

impl WorldState {
    pub fn set(&mut self, section: Section, key: impl Into<String>, value: impl Into<String>) {
        let entries = self.entries_mut(section);
        let key = key.into();
        if let Some(slot) = entries.iter_mut().find(|e| e.key == key) {
            slot.value = value.into();
        } else {
            entries.push(Entry { key, value: value.into() });
        }
    }

    pub fn remove(&mut self, section: Section, key: &str) {
        if let Some(entries) = self.sections.iter_mut().find(|(s, _)| *s == section) {
            entries.1.retain(|e| e.key != key);
        }
    }

    fn entries_mut(&mut self, section: Section) -> &mut Vec<Entry> {
        if !self.sections.iter().any(|(s, _)| *s == section) {
            self.sections.push((section, Vec::new()));
        }
        let idx = self
            .sections
            .iter()
            .position(|(s, _)| *s == section)
            .expect("section exists");
        &mut self.sections[idx].1
    }

    /// Byte-stable rendering. Sections in SECTION_ORDER (missing sections
    /// render as empty), entries in insertion order.
    pub fn render(&self) -> String {
        let mut out = String::from("<world_state>\n");
        for section in SECTION_ORDER {
            out.push_str(&format!("# {:?}\n", section));
            if let Some((_, entries)) = self.sections.iter().find(|(s, _)| *s == section) {
                for e in entries {
                    out.push_str(&format!("{}: {}\n", e.key, e.value));
                }
            }
        }
        out.push_str("</world_state>\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_content_renders_identical_bytes() {
        let mut a = WorldState::default();
        a.set(Section::Workspace, "root", "/tmp/ws");
        a.set(Section::SessionGoal, "goal", "refactor scheduler");
        let mut b = WorldState::default();
        b.set(Section::SessionGoal, "goal", "refactor scheduler");
        b.set(Section::Workspace, "root", "/tmp/ws");
        // different insertion order across sections → same bytes
        assert_eq!(a.render(), b.render());

        // update in place (same key) → byte-stable framing
        a.set(Section::OpenTodos, "1", "run tests");
        a.set(Section::OpenTodos, "2", "ship");
        let before = a.render();
        a.set(Section::OpenTodos, "1", "run ALL tests");
        let after = a.render();
        assert_ne!(before, after);
        assert!(after.contains("1: run ALL tests"));
        assert!(after.contains("2: ship"), "other entries untouched");
    }

    #[test]
    fn section_order_is_fixed_not_runtime_sorted() {
        let mut ws = WorldState::default();
        ws.set(Section::Environment, "os", "macos");
        ws.set(Section::Workspace, "root", "/w");
        let r = ws.render();
        let workspace_pos = r.find("# Workspace").unwrap();
        let env_pos = r.find("# Environment").unwrap();
        assert!(workspace_pos < env_pos, "canonical section order");
    }
}
