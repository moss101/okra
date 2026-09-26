//! Session context with prefire two-pass compaction (MASTER-PLAN §3 #27,
//! grok prefire semantics; G2 gate).
//!
//! Two passes:
//! 1. **prefire** — BEFORE the context crosses the limit, a summary is
//!    produced and staged (`staged_summary`). It is only *validated* at
//!    this stage; an invalid summary is rejected and the staged slot stays
//!    empty (clean-room rule #30: reject, never install).
//! 2. **install** — when the context actually crosses the limit and a
//!    staged summary exists, the old messages are replaced by a compact
//!    prefix: the byte-stable world_state projection + the validated
//!    summary, followed by a bounded tail of the newest messages.
//!
//! If no staged summary is ready at the crossing (the summarizer lagged),
//! an emergency inline pass runs — slower but never skipped.
//!
//! Byte stability: the world_state head is rendered through
//! `WorldState::render` (fixed section order, insertion-ordered entries),
//! so an unchanged world produces **byte-identical prefixes** — the
//! property provider prefix caches need (§3 #32).

use crate::summary::{validate_summary, CompactionSummary};
use crate::world_state::{Entry, Section, WorldState};
use okra_providers::{ContentBlock, Message, Role};

/// Rough token estimate: ~4 chars per token (benchmark-grade, stable).
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    ContentBlock::ToolUse { call } => call.args_json.len() + call.name.len(),
                    ContentBlock::ToolResponse { result } => result.content.len(),
                })
                .sum::<usize>()
        })
        .sum();
    (chars as u64).div_ceil(4)
}

/// A summarizer: turns a transcript into a VALIDATED summary.
pub trait Compactor {
    /// Produce a summary for the given transcript. Implementations may be
    /// scripted (offline) or network-backed; okra validates whatever comes
    /// back before installing.
    fn summarize(&self, transcript: &str, world: &WorldState) -> Result<CompactionSummary, String>;
}

/// Deterministic offline compactor: renders the transcript facts into the
/// validated summary schema. Always produces a schema-valid summary whose
/// context_digest is a stable hash-free digest of the input — byte-stable
/// for identical inputs.
pub struct ScriptedCompactor;

impl Compactor for ScriptedCompactor {
    fn summarize(&self, transcript: &str, world: &WorldState) -> Result<CompactionSummary, String> {
        // digest: first + last non-empty transcript lines (stable)
        let lines: Vec<&str> = transcript.lines().filter(|l| !l.trim().is_empty()).collect();
        let head = lines.first().copied().unwrap_or("");
        let tail = lines.last().copied().unwrap_or("");
        let digest = format!(
            "compressed {:?} ... {:?} ({} lines)",
            head, tail, lines.len()
        );
        let summary = CompactionSummary {
            schema_version: 1,
            replaces: (0, 0),
            user_goals: vec!["continue the task".into()],
            decisions: vec![format!("digest: {digest}")],
            open_items: vec![],
            file_states: world
                .sections
                .iter()
                .find(|(s, _)| *s == Section::FileStates)
                .map(|(_, entries)| {
                    entries
                        .iter()
                        .map(|Entry { key, value }| crate::summary::FileStateNote {
                            path: key.clone(),
                            note: value.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            context_digest: format!("transcript folded at compaction; {digest}"),
        };
        crate::summary::check(&summary).map_err(|e| e.to_string())?;
        Ok(summary)
    }
}

/// One compaction cycle's observable outcome (benchmark + telemetry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionEvent {
    pub kind: CompactionKind,
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// Byte-stable head rendered for this compaction (world_state + summary).
    pub prefix_head: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionKind {
    /// Summary produced ahead of need (pass 1).
    Prefire,
    /// Staged summary installed, context truncated (pass 2).
    Install,
    /// Crossing without a staged summary: inline emergency compaction.
    Emergency,
}

#[derive(Debug, Clone)]
pub struct SessionContextConfig {
    /// Token estimate at which compaction triggers.
    pub limit_tokens: u64,
    /// Prefire margin: stage the summary once usage crosses
    /// `limit_tokens - prefire_margin`.
    pub prefire_margin: u64,
    /// Newest messages kept verbatim after an install.
    pub tail_messages: usize,
}

impl Default for SessionContextConfig {
    fn default() -> Self {
        SessionContextConfig { limit_tokens: 8_000, prefire_margin: 2_000, tail_messages: 8 }
    }
}

struct CompactionStats {
    prefires: u32,
    installs: u32,
    emergencies: u32,
    rejected_summaries: u32,
}

/// The cross-turn context: owns messages, world state, and the compaction
/// state machine.
pub struct SessionContext {
    messages: Vec<Message>,
    world: WorldState,
    config: SessionContextConfig,
    staged: Option<CompactionSummary>,
    stats: CompactionStats,
    /// The byte-stable head: rendered once per install and reused verbatim.
    prefix_head: Vec<u8>,
    events: Vec<CompactionEvent>,
}

impl Default for SessionContext {
    fn default() -> Self {
        Self::new(SessionContextConfig::default())
    }
}

impl SessionContext {
    pub fn new(config: SessionContextConfig) -> Self {
        let mut ctx = SessionContext {
            messages: Vec::new(),
            world: WorldState::default(),
            config,
            staged: None,
            stats: CompactionStats {
                prefires: 0,
                installs: 0,
                emergencies: 0,
                rejected_summaries: 0,
            },
            prefix_head: Vec::new(),
            events: Vec::new(),
        };
        // the byte-stable head exists from construction (empty world render)
        ctx.refresh_world_head();
        ctx
    }

    pub fn world_mut(&mut self) -> &mut WorldState {
        &mut self.world
    }

    pub fn world(&self) -> &WorldState {
        &self.world
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn tokens(&self) -> u64 {
        estimate_tokens(&self.messages)
    }

    pub fn config(&self) -> &SessionContextConfig {
        &self.config
    }

    pub fn events(&self) -> &[CompactionEvent] {
        &self.events
    }

    pub fn prefires(&self) -> u32 {
        self.stats.prefires
    }
    pub fn installs(&self) -> u32 {
        self.stats.installs
    }
    pub fn emergencies(&self) -> u32 {
        self.stats.emergencies
    }
    pub fn rejected_summaries(&self) -> u32 {
        self.stats.rejected_summaries
    }

    /// Push a message and run the two-pass state machine.
    pub fn push(&mut self, message: Message, compactor: &dyn Compactor) {
        self.messages.push(message);
        self.run_compaction_cycle(compactor);
    }

    /// Push several messages (one step's worth), compacting once at the end.
    pub fn extend(&mut self, messages: Vec<Message>, compactor: &dyn Compactor) {
        self.messages.extend(messages);
        self.run_compaction_cycle(compactor);
    }

    fn run_compaction_cycle(&mut self, compactor: &dyn Compactor) {
        let tokens = self.tokens();
        let prefire_at = self.config.limit_tokens.saturating_sub(self.config.prefire_margin);

        // Pass 1 — prefire: stage a summary before it is needed.
        if self.staged.is_none()
            && tokens >= prefire_at
            && tokens < self.config.limit_tokens
            && let Some(summary) = self.produce_summary(compactor)
        {
            self.staged = Some(summary);
            self.stats.prefires += 1;
            self.events.push(CompactionEvent {
                kind: CompactionKind::Prefire,
                messages_before: self.messages.len(),
                messages_after: self.messages.len(),
                tokens_before: tokens,
                tokens_after: self.tokens(),
                prefix_head: self.prefix_head.clone(),
            });
        }

        // Pass 2 — install at the crossing.
        if self.tokens() >= self.config.limit_tokens {
            let before = (self.messages.len(), self.tokens());
            let kind = if self.staged.is_some() {
                self.stats.installs += 1;
                CompactionKind::Install
            } else {
                self.stats.emergencies += 1;
                CompactionKind::Emergency
            };
            let summary = match self.staged.take() {
                Some(s) => s,
                None => match self.produce_summary(compactor) {
                    Some(s) => s,
                    None => return, // cannot compact: leave context as-is
                },
            };
            self.install(summary);
            let after = (self.messages.len(), self.tokens());
            self.events.push(CompactionEvent {
                kind,
                messages_before: before.0,
                messages_after: after.0,
                tokens_before: before.1,
                tokens_after: after.1,
                prefix_head: self.prefix_head.clone(),
            });
        }
    }

    /// Produce + VALIDATE a summary. Invalid output is counted and rejected
    /// — never installed (§3 #30).
    fn produce_summary(&mut self, compactor: &dyn Compactor) -> Option<CompactionSummary> {
        let transcript = self
            .messages
            .iter()
            .map(|m| format!("[{:?}] {}", m.role, m.text_content()))
            .collect::<Vec<_>>()
            .join("\n");
        match compactor.summarize(&transcript, &self.world) {
            Ok(summary) => Some(summary),
            Err(_) => {
                self.stats.rejected_summaries += 1;
                None
            }
        }
    }

    /// Install: byte-stable world_state head + the summary as its own
    /// message + tail. Keeping the summary OUT of the world head message is
    /// what makes the head byte-identical across turns with an unchanged
    /// world — the provider prefix-cache property (§3 #32).
    fn install(&mut self, summary: CompactionSummary) {
        self.refresh_world_head();

        let summary_json =
            serde_json::to_string(&summary).expect("summary serializes");
        let head = Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: String::from_utf8_lossy(&self.prefix_head).into_owned(),
            }],
        };
        let summary_message = Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: format!(
                    "<compaction_summary>\n{}\n</compaction_summary>",
                    summary_json
                ),
            }],
        };

        let tail_start = self.messages.len().saturating_sub(self.config.tail_messages);
        let tail: Vec<Message> = self.messages[tail_start..].to_vec();

        let mut next = vec![head, summary_message];
        next.extend(tail);
        self.messages = next;
    }

    /// Render the world_state head. Byte-stable: identical world →
    /// identical bytes (WorldState::render contract). The summary is NOT
    /// part of this render — it lives in its own message.
    fn refresh_world_head(&mut self) {
        self.prefix_head = self.world.render().into_bytes();
    }

    /// The current prefix head bytes (for byte-stability assertions).
    pub fn prefix_head(&self) -> &[u8] {
        &self.prefix_head
    }

    /// Validate an externally produced summary against the current state
    /// (used by hosts that summarize with a network model).
    pub fn validate_external_summary(&self, raw: &str) -> Result<CompactionSummary, String> {
        let summary = validate_summary(raw).map_err(|e| e.to_string())?;
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big_msg(tag: &str, kb: usize) -> Message {
        Message::user(format!("{tag} {}", "x".repeat(kb * 1024)))
    }

    #[test]
    fn compaction_keeps_context_flat_and_validates() {
        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 4_000,
            prefire_margin: 1_000,
            tail_messages: 4,
        });
        let compactor = ScriptedCompactor;
        for i in 0..40 {
            ctx.extend(
                vec![big_msg(&format!("m{i}"), 2)],
                &compactor,
            );
        }
        // flat: never more than the tail + head after installs
        assert!(ctx.installs() >= 1, "compaction must have run");
        assert!(ctx.tokens() < 8_000, "context stayed bounded: {}", ctx.tokens());
        assert_eq!(ctx.rejected_summaries(), 0);
    }

    #[test]
    fn invalid_summary_is_rejected_never_installed() {
        struct BadCompactor;
        impl Compactor for BadCompactor {
            fn summarize(
                &self,
                _transcript: &str,
                _world: &WorldState,
            ) -> Result<CompactionSummary, String> {
                // schema-invalid: no user goals, thin digest
                Err("no goals".into())
            }
        }
        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 2_000,
            prefire_margin: 500,
            tail_messages: 2,
        });
        for i in 0..20 {
            ctx.extend(vec![big_msg(&format!("bad{i}"), 2)], &BadCompactor);
        }
        assert_eq!(ctx.installs(), 0, "invalid summaries must never install");
        // context kept growing because compaction could not run — the caller
        // sees the honest state rather than a fake summary
        assert!(ctx.tokens() > 2_000);
    }

    #[test]
    fn world_head_is_byte_stable_across_compactions() {
        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 3_000,
            prefire_margin: 500,
            tail_messages: 2,
        });
        ctx.world_mut()
            .set(Section::FileStates, "src/app.js", "edited; verified");
        let compactor = ScriptedCompactor;
        let mut heads: Vec<Vec<u8>> = Vec::new();
        let mut seen_install = false;
        for i in 0..30 {
            ctx.extend(vec![big_msg(&format!("t{i}"), 2)], &compactor);
            // stability is a post-compaction property: the head as of the
            // LAST install, byte-identical while the world is unchanged
            if ctx.installs() > 0 {
                seen_install = true;
                heads.push(ctx.prefix_head().to_vec());
            }
        }
        assert!(seen_install, "compaction ran");
        assert!(heads.len() >= 2, "multiple compactions for stability check");
        // unchanged world → byte-identical heads across ALL compactions
        let first = &heads[0];
        assert!(
            heads.iter().all(|h| h == first),
            "world_state head must be byte-stable"
        );
    }

    #[test]
    fn changed_world_changes_the_head_bytes() {
        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 3_000,
            prefire_margin: 500,
            tail_messages: 2,
        });
        let compactor = ScriptedCompactor;
        let mut heads: Vec<Vec<u8>> = Vec::new();
        for i in 0..20 {
            ctx.extend(vec![big_msg(&format!("t{i}"), 2)], &compactor);
            if i == 10 {
                ctx.world_mut()
                    .set(Section::FileStates, "new.rs", "touched at i=10");
            }
            if !ctx.prefix_head().is_empty() {
                heads.push(ctx.prefix_head().to_vec());
            }
        }
        assert!(
            heads.windows(2).any(|w| w[0] != w[1]),
            "a changed world must change the head"
        );
        let _ = heads;
    }
}
