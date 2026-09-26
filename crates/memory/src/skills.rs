//! Skills — MASTER-PLAN §3 (qwen path-conditional skill activation +
//! progressive disclosure; skill curator lifecycle lands later).
//!
//! - A skill is a `SKILL.md` file: frontmatter between `---` markers
//!   (`name`, `description`, `match` — one glob per line is allowed via a
//!   comma/space separated list), then the body.
//! - **Progressive disclosure**: the model's prompt carries only the L1
//!   index (name + one-line description per skill). The L2 body is loaded
//!   only for skills that ACTIVATE.
//! - **Path-conditional activation**: a skill activates when a path the
//!   agent touched matches one of its `match` patterns. Activations enter
//!   the context head (world Skills section) — bounded churn, byte-stable
//!   while the activation set is unchanged.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    /// Glob-style patterns (implement-minimal: `*` wildcard per segment).
    pub match_patterns: Vec<String>,
    /// Full instructions — disclosure layer 2.
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillParseError {
    #[error("missing frontmatter markers `---`")]
    MissingFrontmatter,
    #[error("frontmatter must define `name`")]
    MissingName,
}

/// Minimal glob: `*` matches within a segment, `**` across separators.
/// Compiled ad hoc — no regex dependency for M2.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p[0] == b'*' {
            // `**` crosses separators, `*` does not
            let double = p.len() >= 2 && p[1] == b'*';
            if double {
                let rest = &p[2..];
                for i in 0..=t.len() {
                    if rec(rest, &t[i..]) {
                        return true;
                    }
                }
                return false;
            }
            let rest = &p[1..];
            for i in 0..=t.len() {
                if i > 0 && t[i - 1] == b'/' {
                    break; // single star cannot cross `/`
                }
                if rec(rest, &t[i..]) {
                    return true;
                }
            }
            return false;
        }
        if !t.is_empty() && (p[0] == b'?' || p[0] == t[0]) {
            return rec(&p[1..], &t[1..]);
        }
        false
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

impl SkillDef {
    /// Parse a SKILL.md document.
    pub fn parse(source: &str) -> Result<SkillDef, SkillParseError> {
        let trimmed = source.trim_start();
        let rest = trimmed
            .strip_prefix("---\n")
            .or_else(|| trimmed.strip_prefix("---\r\n"))
            .ok_or(SkillParseError::MissingFrontmatter)?;
        let (frontmatter, body) = rest
            .split_once("\n---")
            .ok_or(SkillParseError::MissingFrontmatter)?;
        let mut name = None;
        let mut description = String::new();
        let mut patterns = Vec::new();
        for line in frontmatter.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("name:") {
                name = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("description:") {
                description = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("match:") {
                for pat in v.split([',', ' ']) {
                    let pat = pat.trim();
                    if !pat.is_empty() {
                        patterns.push(pat.to_string());
                    }
                }
            }
        }
        let name = name.ok_or(SkillParseError::MissingName)?;
        Ok(SkillDef {
            name,
            description,
            match_patterns: patterns,
            body: body.trim_start().to_string(),
        })
    }

    pub fn matches_path(&self, path: &str) -> bool {
        let base = path.rsplit('/').next().unwrap_or(path);
        self.match_patterns.iter().any(|p| {
            if p.contains('/') {
                glob_match(p, path)
            } else {
                // separator-free patterns match basenames (qwen semantics):
                // `*.rs` activates on any .rs file at any depth
                glob_match(p, base)
            }
        })
    }
}

/// A directory of skills (L1 index + L2 bodies).
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    pub skills: Vec<SkillDef>,
}

impl SkillCatalog {
    /// Load every `*.md` file in `dir` (non-recursive) as a skill.
    pub fn load_dir(dir: &Path) -> SkillCatalog {
        let mut skills = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            files.sort();
            for path in files {
                let is_md = path.extension().map(|e| e == "md").unwrap_or(false);
                if is_md
                    && let Ok(src) = std::fs::read_to_string(&path)
                    && let Ok(skill) = SkillDef::parse(&src)
                {
                    skills.push(skill);
                }
            }
        }
        SkillCatalog { skills }
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Disclosure layer 1: the tiny index that lives in the prompt head.
    pub fn disclosure_index(&self) -> String {
        let mut out = String::from("# Skills\n");
        for s in &self.skills {
            out.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        out
    }

    /// Path-conditional activation: skills whose patterns match any touched
    /// path.
    pub fn active_for_paths(&self, touched: &[String]) -> Vec<&SkillDef> {
        self.skills
            .iter()
            .filter(|s| touched.iter().any(|p| s.matches_path(p)))
            .collect()
    }

    /// Disclosure layer 2 for one skill (body on demand).
    pub fn full_body(&self, name: &str) -> Option<&str> {
        self.skills.iter().find(|s| s.name == name).map(|s| s.body.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\nname: rust-testing\ndescription: how we test rust code\nmatch: *.rs tests/**\n---\n\n# Rust testing\n\nUse okra conventions: one module per file.\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let skill = SkillDef::parse(DOC).unwrap();
        assert_eq!(skill.name, "rust-testing");
        assert_eq!(skill.description, "how we test rust code");
        assert_eq!(skill.match_patterns, vec!["*.rs", "tests/**"]);
        assert!(skill.body.contains("one module per file"));
        assert!(matches!(
            SkillDef::parse("no frontmatter here"),
            Err(SkillParseError::MissingFrontmatter)
        ));
    }

    #[test]
    fn path_conditional_activation() {
        let skill = SkillDef::parse(DOC).unwrap();
        assert!(skill.matches_path("src/lib.rs"));
        assert!(skill.matches_path("tests/it/main.rs"), "** crosses /");
        assert!(!skill.matches_path("docs/readme.md"));
    }

    #[test]
    fn catalog_disclosure_and_activation() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("SKILL-rust.md"), DOC).unwrap();
        std::fs::write(
            td.path().join("SKILL-docker.md"),
            "---\nname: docker\ndescription: container builds\nmatch: Dockerfile*\n---\nUse buildkit.\n",
        )
        .unwrap();
        let catalog = SkillCatalog::load_dir(td.path());
        assert_eq!(catalog.len(), 2);

        // L1 index: names + descriptions only, no bodies
        let index = catalog.disclosure_index();
        assert!(index.contains("rust-testing"));
        assert!(index.contains("docker"));
        assert!(!index.contains("Use buildkit"), "L2 body stays out of L1");

        // activation by touched paths
        let active = catalog.active_for_paths(&["src/main.rs".into()]);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "rust-testing");
        assert!(catalog.full_body("rust-testing").unwrap().contains("okra conventions"));
    }
}
