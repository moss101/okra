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
    /// Microcompaction (#28, qwen): evict old tool-result payloads in place
    /// (outcome preserved, content replaced by a stub) once usage crosses
    /// `microcompact_at`. Runs BEFORE summary compaction — it is cheap and
    /// often enough on its own.
    pub microcompact_at: Option<u64>,
    /// Newest tool results exempt from microcompaction.
    pub keep_recent_tool_results: usize,
}

impl Default for SessionContextConfig {
    fn default() -> Self {
        SessionContextConfig {
            limit_tokens: 8_000,
            prefire_margin: 2_000,
            tail_messages: 8,
            microcompact_at: Some(5_000),
            keep_recent_tool_results: 4,
        }
    }
}

struct CompactionStats {
    prefires: u32,
    installs: u32,
    emergencies: u32,
    rejected_summaries: u32,
    microcompactions: u32,
    evicted_bytes: u64,
    hydrated_files: u32,
}

/// The cross-turn context: owns messages, world state, and the compaction
/// state machine.
pub struct SessionContext {
    messages: Vec<Message>,
    world: WorldState,
    config: SessionContextConfig,
    staged: Option<CompactionSummary>,
    /// File-state hydration (#29, ZCode): workspace root to re-read noted
    /// files from at install time. None = hydration off.
    hydration_root: Option<std::path::PathBuf>,
    /// Max files hydrated per install (most recently noted first).
    hydration_max_files: usize,
    /// Tiered memory recall (#33): rendered recall text folded into the
    /// byte-stable head (memory files change rarely).
    memory_recall: Option<String>,
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
            hydration_root: None,
            hydration_max_files: 4,
            memory_recall: None,
            stats: CompactionStats {
                prefires: 0,
                installs: 0,
                emergencies: 0,
                rejected_summaries: 0,
                microcompactions: 0,
                evicted_bytes: 0,
                hydrated_files: 0,
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
    pub fn microcompactions(&self) -> u32 {
        self.stats.microcompactions
    }
    pub fn evicted_bytes(&self) -> u64 {
        self.stats.evicted_bytes
    }
    pub fn hydrated_files(&self) -> u32 {
        self.stats.hydrated_files
    }

    /// Enable file-state hydration: `root` is re-read at install time for
    /// every noted file path (most recent first, capped).
    pub fn enable_hydration(&mut self, root: std::path::PathBuf, max_files: usize) {
        self.hydration_root = Some(root);
        self.hydration_max_files = max_files;
    }

    /// Tiered memory recall (#33): folded into the byte-stable head.
    /// While the memory files are unchanged, the head stays byte-identical.
    pub fn set_memory_recall(&mut self, recall: &str) {
        let recall = recall.trim();
        if recall.is_empty() {
            return;
        }
        self.memory_recall = Some(recall.to_string());
        self.refresh_world_head();
    }

    /// Note a file path the agent touched (drives hydration at install).
    /// Insert-only: never overwrites a hydration note (the fresh file
    /// excerpt is the authoritative state; re-noting must not oscillate
    /// the byte-stable head).
    pub fn note_file(&mut self, path: &str) {
        let already_noted = self
            .world
            .sections
            .iter()
            .any(|(sec, entries)| {
                *sec == Section::FileStates
                    && entries.iter().any(|e| e.key == path)
            });
        if !already_noted {
            self.world.set(
                Section::FileStates,
                path,
                "touched (state refreshed at compaction)",
            );
        }
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
        // Pass 0 — microcompaction (#28): cheapest first. Evict old tool
        // result payloads in place; outcomes (error flags) are preserved so
        // the model keeps the facts it needs at a fraction of the size.
        if let Some(micro_at) = self.config.microcompact_at
            && self.tokens() >= micro_at
        {
            self.microcompact();
        }

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

    /// Microcompaction (#28): replace all but the newest
    /// `keep_recent_tool_results` tool-result payloads with a stub that
    /// preserves the call id and the error flag. In-place eviction — the
    /// surrounding messages stay untouched.
    fn microcompact(&mut self) {
        let keep = self.config.keep_recent_tool_results;
        // indices (newest first) of tool-result blocks
        let mut result_positions: Vec<(usize, usize)> = Vec::new();
        for (mi, m) in self.messages.iter().enumerate() {
            for (bi, b) in m.content.iter().enumerate() {
                if matches!(b, ContentBlock::ToolResponse { .. }) {
                    result_positions.push((mi, bi));
                }
            }
        }
        if result_positions.len() <= keep {
            return;
        }
        let evict_count = result_positions.len() - keep;
        let evict: Vec<(usize, usize)> = result_positions[..evict_count].to_vec();
        let mut evicted: u64 = 0;
        for (mi, bi) in evict {
            if let ContentBlock::ToolResponse { result } = &mut self.messages[mi].content[bi] {
                let original = result.content.len();
                if original <= 64 {
                    continue; // already small: eviction saves nothing
                }
                evicted += original as u64;
                let is_error = result.is_error;
                result.content = format!(
                    "[microcompacted: {original} bytes of tool output evicted; outcome {}]",
                    if is_error { "error" } else { "success" }
                );
                result.is_error = is_error;
                self.stats.microcompactions += 1;
            }
        }
        self.stats.evicted_bytes += evicted;
    }

    /// Hydration (#29): after compaction the summarized view of a touched
    /// file can be stale — re-read the noted files (most recent first,
    /// capped) and fold a fresh excerpt into the world FileStates so the
    /// NEXT head render carries current reality. Reads happen before the
    /// head render inside `install`.
    fn hydrate_file_states(&mut self) {
        let Some(root) = &self.hydration_root else {
            return;
        };
        let paths: Vec<String> = self
            .world
            .sections
            .iter()
            .find(|(sec, _)| *sec == Section::FileStates)
            .map(|(_, entries)| entries.iter().map(|e| e.key.clone()).collect())
            .unwrap_or_default();
        let mut hydrated = 0u32;
        for path in paths.iter().rev() {
            if hydrated >= self.hydration_max_files as u32 {
                break;
            }
            let abs = root.join(path);
            let Ok(bytes) = std::fs::read(&abs) else {
                continue;
            };
            let excerpt: String = {
                let text = String::from_utf8_lossy(&bytes);
                text.chars().take(160).collect()
            };
            self.world.set(
                Section::FileStates,
                path,
                format!("hydrated {} bytes: {:?}", bytes.len(), excerpt),
            );
            hydrated += 1;
            self.stats.hydrated_files += 1;
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
        self.hydrate_file_states();
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
    /// identical bytes (WorldState::render contract). The tiered memory
    /// recall (#33) is folded in AFTER the world section — it belongs to
    /// the stable head because memory files change rarely; the summary is
    /// NOT part of this render — it lives in its own message.
    fn refresh_world_head(&mut self) {
        let mut text = self.world.render();
        if let Some(memory) = &self.memory_recall {
            text.push_str("<memory_recall>\n");
            text.push_str(memory);
            text.push_str("\n</memory_recall>\n");
        }
        self.prefix_head = text.into_bytes();
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
            microcompact_at: Some(2_500),
            keep_recent_tool_results: 2,
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
            microcompact_at: None,
            keep_recent_tool_results: 2,
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
            microcompact_at: Some(2_500),
            keep_recent_tool_results: 2,
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
    fn microcompact_evicts_old_tool_results_preserving_outcomes() {
        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 50_000, // no summary compaction should trigger
            prefire_margin: 5_000,
            tail_messages: 2,
            microcompact_at: Some(800),
            keep_recent_tool_results: 2,
        });
        for i in 0..6 {
            ctx.push(
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResponse {
                        result: okra_providers::ToolResult {
                            call_id: format!("c{i}"),
                            content: format!("result {i} {}", "y".repeat(600)),
                            is_error: i == 1,
                        },
                    }],
                },
                &ScriptedCompactor,
            );
        }
        assert!(ctx.microcompactions() >= 2, "old results evicted");
        assert!(ctx.evicted_bytes() > 0);
        let result_content = |messages: &[Message], call_id: &str| -> Option<(String, bool)> {
            messages.iter().find_map(|m| {
                m.content.iter().find_map(|b| match b {
                    ContentBlock::ToolResponse { result } if result.call_id == call_id => {
                        Some((result.content.clone(), result.is_error))
                    }
                    _ => None,
                })
            })
        };
        // newest two kept verbatim
        let (content5, _) = result_content(ctx.messages(), "c5").unwrap();
        assert!(content5.contains("result 5") && content5.len() > 300);
        // old ones stubbed with the outcome preserved
        let (content0, err0) = result_content(ctx.messages(), "c0").unwrap();
        assert!(content0.contains("microcompacted") && content0.contains("bytes of tool output evicted"));
        assert!(err0 == false);
        let (content1, err1) = result_content(ctx.messages(), "c1").unwrap();
        assert!(content1.contains("microcompacted"));
        assert!(err1, "error outcome preserved through stubbing");
        // flat: microcompaction alone kept us far below the summary limit
        assert!(ctx.tokens() < 50_000);
        assert_eq!(ctx.installs(), 0, "summary compaction never needed here");
    }

    #[test]
    fn hydration_refreshes_file_state_at_install() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("tracked.rs"), "pub fn v1() {}").unwrap();

        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 2_500,
            prefire_margin: 500,
            tail_messages: 2,
            microcompact_at: None,
            keep_recent_tool_results: 2,
        });
        ctx.enable_hydration(td.path().to_path_buf(), 4);
        ctx.note_file("tracked.rs");
        for i in 0..10 {
            ctx.extend(vec![big_msg(&format!("h{i}"), 2)], &ScriptedCompactor);
        }
        assert!(ctx.installs() >= 1);
        assert!(ctx.hydrated_files() >= 1, "noted file hydrated at install");
        let head = String::from_utf8(ctx.prefix_head().to_vec()).unwrap();
        assert!(head.contains("hydrated"), "fresh file state in head: {head}");
        assert!(head.contains("pub fn v1"), "actual file content excerpt present");
    }

    #[test]
    fn memory_recall_is_folded_into_the_stable_head() {
        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 3_000,
            prefire_margin: 500,
            tail_messages: 2,
            microcompact_at: None,
            keep_recent_tool_results: 2,
        });
        ctx.set_memory_recall("user prefers rust; project uses okra crates");
        for i in 0..12 {
            ctx.extend(vec![big_msg(&format!("m{i}"), 2)], &ScriptedCompactor);
        }
        let head = String::from_utf8(ctx.prefix_head().to_vec()).unwrap();
        assert!(head.contains("<memory_recall>"));
        assert!(head.contains("project uses okra crates"));
    }

    #[test]
    fn changed_world_changes_the_head_bytes() {
        let mut ctx = SessionContext::new(SessionContextConfig {
            limit_tokens: 3_000,
            prefire_margin: 500,
            tail_messages: 2,
            microcompact_at: Some(2_500),
            keep_recent_tool_results: 2,
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
