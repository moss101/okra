//! okra-memory — tiered file memory (MASTER-PLAN §3 #33, qwen
//! `packages/core/src/memory/`): user / project / team-via-git tiers +
//! secret scanning before anything is persisted or recalled.

pub mod secrets;
pub mod tiers;

pub use secrets::{redact_secrets, scan_secrets, SecretHit};
pub use tiers::{MemoryTier, TieredReader};
