//! Conversation-scoped attachments (MASTER-PLAN §3 #52, ChatGPT2 docs/03
//! §5): pasted text, goals, and context files held as per-conversation
//! state in the host — the donor holds these in its thread store; okra's
//! daemon owns them so every surface sees the same attachment set.
//!
//! Context files carry a path + origin (the donor's
//! `add-context-file {hostId, path, origin}` shape); the FS access itself
//! stays behind `safe_fs` / the tool plane — this module only tracks
//! what is attached where.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentOrigin {
    UserPick,
    DragDrop,
    Paste,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AttachmentKind {
    /// A file attached as context (`add-context-file`).
    ContextFile { path: PathBuf, origin: AttachmentOrigin },
    /// Pasted text held for the next turn.
    PastedText { content: String },
    /// A goal statement pinned to the conversation.
    Goal { content: String },
    /// An image attachment (path to the local image).
    Image { path: PathBuf, origin: AttachmentOrigin },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub conversation_id: String,
    pub kind: AttachmentKind,
    pub added_at_epoch_ms: u64,
}

/// Per-conversation attachment sets. `clear_conversation` runs on
/// conversation discard so nothing outlives its thread.
#[derive(Debug, Default)]
pub struct AttachmentStore {
    by_conversation: BTreeMap<String, Vec<Attachment>>,
    next_id: u64,
}

impl AttachmentStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn add(
        &mut self,
        conversation_id: impl Into<String>,
        kind: AttachmentKind,
    ) -> Attachment {
        self.next_id += 1;
        let attachment = Attachment {
            id: format!("att-{}", self.next_id),
            conversation_id: conversation_id.into(),
            kind,
            added_at_epoch_ms: Self::now_ms(),
        };
        let conversation = attachment.conversation_id.clone();
        self.by_conversation.entry(conversation).or_default().push(attachment.clone());
        attachment
    }

    pub fn list(&self, conversation_id: &str) -> Vec<Attachment> {
        self.by_conversation
            .get(conversation_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn remove(&mut self, conversation_id: &str, attachment_id: &str) -> bool {
        let Some(list) = self.by_conversation.get_mut(conversation_id) else {
            return false;
        };
        let before = list.len();
        list.retain(|a| a.id != attachment_id);
        if list.len() != before {
            return true;
        }
        false
    }

    /// Conversation discarded: drop its whole attachment set.
    pub fn clear_conversation(&mut self, conversation_id: &str) -> usize {
        self.by_conversation
            .remove(conversation_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_are_scoped_per_conversation() {
        let mut s = AttachmentStore::new();
        let a = s.add(
            "c1",
            AttachmentKind::ContextFile {
                path: "/tmp/notes.md".into(),
                origin: AttachmentOrigin::UserPick,
            },
        );
        s.add("c1", AttachmentKind::PastedText { content: "hello".into() });
        s.add(
            "c2",
            AttachmentKind::Goal { content: "ship it".into() },
        );
        assert_eq!(s.list("c1").len(), 2);
        assert_eq!(s.list("c2").len(), 1);
        assert!(s.list("c1").iter().all(|x| x.conversation_id == "c1"));
        match &s.list("c1")[0].kind {
            AttachmentKind::ContextFile { path, origin } => {
                assert!(path.ends_with("notes.md"));
                assert_eq!(*origin, AttachmentOrigin::UserPick);
            }
            other => panic!("{other:?}"),
        }
        assert!(s.remove("c1", &a.id));
        assert!(!s.remove("c1", &a.id), "idempotent remove");
        assert_eq!(s.list("c1").len(), 1);
    }

    #[test]
    fn discard_clears_only_that_conversation() {
        let mut s = AttachmentStore::new();
        s.add("c1", AttachmentKind::PastedText { content: "x".into() });
        s.add("c2", AttachmentKind::PastedText { content: "y".into() });
        assert_eq!(s.clear_conversation("c1"), 1);
        assert!(s.list("c1").is_empty());
        assert_eq!(s.list("c2").len(), 1);
        assert_eq!(s.clear_conversation("c1"), 0);
    }
}
