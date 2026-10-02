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

    /// Append one line to a tier (the accept path of the extract agent —
    /// a memory FILE is a list of lines; an append never rewrites what is
    /// stored). Returns the line count after the append.
    pub fn append_tier(&self, tier: MemoryTier, line: &str) -> std::io::Result<usize> {
        let line = line.trim();
        if line.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty memory line"));
        }
        let existing = self.read_tier(tier).unwrap_or_default();
        let mut lines: Vec<&str> = existing.lines().collect();
        if lines.iter().any(|l| l.trim() == line) {
            // already stored — idempotent accept
            return Ok(lines.len());
        }
        lines.push(line);
        let mut content = lines.join("\n");
        content.push('\n');
        self.write_tier(tier, &content)?;
        Ok(lines.len())
    }

    /// The stored lines of a tier (the dream agent's input).
    pub fn lines_of(&self, tier: MemoryTier) -> Vec<String> {
        self.read_tier(tier)
            .map(|c| c.lines().map(str::to_string).collect())
            .unwrap_or_default()
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

#[cfg(test)]
mod append_tests {
    use super::*;

    #[test]
    fn append_is_idempotent_and_never_rewrites() {
        let td = tempfile::tempdir().unwrap();
        let reader = TieredReader::new(td.path().join("h"), td.path().join("w"));
        assert_eq!(reader.append_tier(MemoryTier::User, "Run cargo clippy before commits").unwrap(), 1);
        assert_eq!(reader.append_tier(MemoryTier::User, "Prefer small PRs").unwrap(), 2);
        // duplicate accept: no growth
        assert_eq!(reader.append_tier(MemoryTier::User, "Prefer small PRs").unwrap(), 2);
        let lines = reader.lines_of(MemoryTier::User);
        assert_eq!(lines, vec!["Run cargo clippy before commits", "Prefer small PRs"]);
        assert!(reader.append_tier(MemoryTier::User, "   ").is_err(), "empty line refused");
    }
}
