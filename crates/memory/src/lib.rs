//! okra-memory — tiered file memory (MASTER-PLAN §3 #33/#34, qwen
//! `packages/core/src/memory/`): user / project / team-via-git tiers +
//! secret scanning before anything is persisted or recalled; the extract
//! and dream memory AGENTS as deterministic offline functions
//! (`agents.rs`); embedding retrieval with MMR diversity re-ranking
//! (`retrieval.rs`).

pub mod agents;
pub mod retrieval;
pub mod secrets;
pub mod skills;
pub mod tiers;

pub use agents::{dream, extract_memories, rank_mmr, DreamCluster, DreamContradiction, DreamReport, MemoryCandidate, MemoryKind};
pub use secrets::{redact_secrets, scan_secrets, SecretHit};
pub use skills::{glob_match, SkillCatalog, SkillDef, SkillParseError};
pub use tiers::{MemoryTier, TieredReader};
