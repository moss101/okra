//! Tier resolution (qwen memory design): USER (home), PROJECT (workspace,
//! gitignored), TEAM (project, committed via git). Precedence on read:
//! PROJECT > TEAM > USER (most specific wins); writes go to PROJECT by
//! default and TEAM explicitly.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    User,
    Project,
    Team,
}

impl MemoryTier {
    pub fn file_name(self) -> &'static str {
        match self {
            MemoryTier::User => "memory.md",
            MemoryTier::Project => "okra.memory.md",
            MemoryTier::Team => "okra.team.md",
        }
    }

    /// Where the tier's file lives.
    pub fn resolve_path(self, home: &Path, workspace: &Path) -> PathBuf {
        match self {
            MemoryTier::User => home.join(".okra").join(self.file_name()),
            MemoryTier::Project | MemoryTier::Team => {
                workspace.join(".okra").join(self.file_name())
            }
        }
    }
}

/// Layered reader: recall concatenates tiers PROJECT > TEAM > USER with
/// section headers, so specific knowledge lands last (nearest the model).
pub struct TieredReader {
    home: PathBuf,
    workspace: PathBuf,
}

impl TieredReader {
    pub fn new(home: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        TieredReader { home: home.into(), workspace: workspace.into() }
    }

    pub fn read_tier(&self, tier: MemoryTier) -> Option<String> {
        let path = tier.resolve_path(&self.home, &self.workspace);
        std::fs::read_to_string(path).ok().filter(|s| !s.trim().is_empty())
    }

    /// Full recall in precedence order (least specific first).
    pub fn recall(&self) -> String {
        let mut out = String::new();
        for tier in [MemoryTier::User, MemoryTier::Team, MemoryTier::Project] {
            if let Some(content) = self.read_tier(tier) {
                out.push_str(&format!("## {:?} memory\n{content}\n", tier));
            }
        }
        out
    }

    pub fn write_tier(&self, tier: MemoryTier, content: &str) -> std::io::Result<PathBuf> {
        let path = tier.resolve_path(&self.home, &self.workspace);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_layers_project_over_user() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("home");
        let ws = td.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();

        let reader = TieredReader::new(&home, &ws);
        reader.write_tier(MemoryTier::User, "prefers rust").unwrap();
        reader.write_tier(MemoryTier::Project, "this repo uses okra crates").unwrap();

        let recall = reader.recall();
        assert!(recall.contains("prefers rust"));
        assert!(recall.contains("this repo uses okra crates"));
        let project_pos = recall.find("this repo uses").unwrap();
        let user_pos = recall.find("prefers rust").unwrap();
        assert!(project_pos > user_pos, "project tier lands nearest the model");
    }

    #[test]
    fn empty_tiers_are_skipped() {
        let td = tempfile::tempdir().unwrap();
        let reader = TieredReader::new(td.path().join("h"), td.path().join("w"));
        assert_eq!(reader.recall(), "");
    }
}
