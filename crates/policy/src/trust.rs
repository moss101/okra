//! Project trust gating (MASTER-PLAN §3 #25, the Codex dossier gap):
//! project-provided content — skills, MCP server configs, hooks, settings
//! overrides — stays INERT until the user trusts the project. Running
//! someone's repo is running their code.
//!
//! Trust binds to a DIGEST over the project's activatable content, not to
//! the path alone: one byte of drift in any gated file re-gates the
//! project (a malicious PR cannot ride in on an old click). The store is
//! user-scope (`~/.okra/trusted-projects.json` by convention — the caller
//! passes the path); a corrupt store fails closed (no trust) and is
//! quarantined with the kernel's `.corrupt` convention.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// What the gate decided for one project root right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustVerdict {
    /// Digest matches a stored trust record — content may activate.
    Trusted { digest: String },
    /// Trusted before, but the gated content changed since.
    Changed { stored: String, current: String },
    /// Never trusted.
    Untrusted,
}

/// A stored trust record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustRecord {
    pub digest: String,
    /// Unix seconds at the trust decision.
    pub trusted_at: u64,
}

/// The user-scope trust store. All methods fail closed: any read/parse
/// problem means "no trust", never a panic and never a stale grant.
#[derive(Debug, Clone)]
pub struct ProjectTrustStore {
    path: PathBuf,
}

impl ProjectTrustStore {
    pub fn at(path: impl Into<PathBuf>) -> ProjectTrustStore {
        ProjectTrustStore { path: path.into() }
    }

    fn load(&self) -> BTreeMap<String, TrustRecord> {
        let mut text = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.path) {
            if f.read_to_string(&mut text).is_err() {
                return BTreeMap::new();
            }
        } else {
            return BTreeMap::new();
        }
        match serde_json::from_str(&text) {
            Ok(map) => map,
            Err(_) => {
                // quarantine (kernel `.corrupt` convention) and fail closed
                let quarantined = self.path.with_extension("json.corrupt");
                let _ = std::fs::rename(&self.path, &quarantined);
                BTreeMap::new()
            }
        }
    }

    fn save(&self, map: &BTreeMap<String, TrustRecord>) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(map)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&self.path, text)
    }

    fn key(root: &Path) -> String {
        root.to_string_lossy().replace('\\', "/")
    }

    /// The verdict for `root` with the given content digest.
    pub fn verdict(&self, root: &Path, digest: &str) -> TrustVerdict {
        match self.load().get(&Self::key(root)) {
            None => TrustVerdict::Untrusted,
            Some(rec) if rec.digest == digest => TrustVerdict::Trusted { digest: digest.to_string() },
            Some(rec) => TrustVerdict::Changed { stored: rec.digest.clone(), current: digest.to_string() },
        }
    }

    /// Record trust for `root` at exactly this digest.
    pub fn trust(&self, root: &Path, digest: &str) -> std::io::Result<TrustRecord> {
        let mut map = self.load();
        let rec = TrustRecord { digest: digest.to_string(), trusted_at: unix_now() };
        map.insert(Self::key(root), rec.clone());
        self.save(&map)?;
        Ok(rec)
    }

    /// Revoke: the project re-gates (next activation is refused).
    pub fn revoke(&self, root: &Path) -> std::io::Result<bool> {
        let mut map = self.load();
        let removed = map.remove(&Self::key(root)).is_some();
        if removed {
            self.save(&map)?;
        }
        Ok(removed)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Digest the project's activatable content. `files` are the gated files
/// (paths already resolved); entries that vanish between listing and
/// hashing are skipped honestly (they re-gate on the next check), and the
/// digest covers RELATIVE paths so moving the project directory does not
/// invalidate trust.
pub fn content_digest(root: &Path, files: &[PathBuf]) -> String {
    let mut entries: Vec<(String, String)> = Vec::new();
    for f in files {
        let Ok(rel) = f.strip_prefix(root) else { continue };
        let Ok(mut file) = std::fs::File::open(f) else { continue };
        let mut h = Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => h.update(&buf[..n]),
                Err(_) => break,
            };
        }
        entries.push((
            rel.to_string_lossy().replace('\\', "/"),
            format!("{:x}", h.finalize()),
        ));
    }
    entries.sort();
    let mut top = Sha256::new();
    for (rel, hash) in &entries {
        top.update(rel.as_bytes());
        top.update([0u8]);
        top.update(hash.as_bytes());
        top.update(*b"\n");
    }
    format!("{:x}", top.finalize())
}

/// The gate: is this project's content activatable? `gated_files` is the
/// listing of project-provided content (skills, `.okra/config.json`, …).
/// An EMPTY gated set needs no trust (nothing to activate).
pub fn ensure_trusted(store: &ProjectTrustStore, root: &Path, gated_files: &[PathBuf]) -> TrustVerdict {
    if gated_files.is_empty() {
        // nothing project-provided to activate — no gate to pass
        return TrustVerdict::Trusted { digest: content_digest(root, gated_files) };
    }
    let digest = content_digest(root, gated_files);
    store.verdict(root, &digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, s: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn untrusted_until_trusted_and_regated_on_drift() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("proj");
        let store_path = td.path().join("trust.json");
        let store = ProjectTrustStore::at(&store_path);

        let skill = root.join(".okra/skills/SKILL-a.md");
        write(&skill, "do a thing");
        let cfg = root.join(".okra/config.json");
        write(&cfg, "{}");
        let files = vec![skill.clone(), cfg.clone()];

        // never trusted → gate refuses
        let v = ensure_trusted(&store, &root, &files);
        assert_eq!(v, TrustVerdict::Untrusted);

        // trust at the current digest → gate passes
        let digest = content_digest(&root, &files);
        store.trust(&root, &digest).unwrap();
        assert_eq!(
            ensure_trusted(&store, &root, &files),
            TrustVerdict::Trusted { digest: digest.clone() }
        );

        // one byte of project-content drift re-gates
        write(&skill, "do a DIFFERENT thing");
        match ensure_trusted(&store, &root, &files) {
            TrustVerdict::Changed { stored, current } => {
                assert_eq!(stored, digest);
                assert_ne!(stored, current);
            }
            other => panic!("expected Changed, got {other:?}"),
        }

        // re-trusting the new content passes again
        let d2 = content_digest(&root, &files);
        store.trust(&root, &d2).unwrap();
        assert!(matches!(
            ensure_trusted(&store, &root, &files),
            TrustVerdict::Trusted { .. }
        ));

        // revoke → refuses again
        assert!(store.revoke(&root).unwrap());
        assert_eq!(ensure_trusted(&store, &root, &files), {
            let d3 = content_digest(&root, &files);
            // after revoke the stored record is gone; verdict is Untrusted
            let _ = d3;
            TrustVerdict::Untrusted
        });
    }

    #[test]
    fn moving_the_project_directory_keeps_trust() {
        let td = tempfile::tempdir().unwrap();
        let a = td.path().join("a");
        write(&a.join(".okra/skills/SKILL-x.md"), "x");
        let files = vec![a.join(".okra/skills/SKILL-x.md")];
        let da = content_digest(&a, &files);

        let b = td.path().join("b");
        write(&b.join(".okra/skills/SKILL-x.md"), "x");
        let files_b = vec![b.join(".okra/skills/SKILL-x.md")];
        assert_eq!(da, content_digest(&b, &files_b), "relative-path digest is move-stable");
    }

    #[test]
    fn empty_gated_set_needs_no_trust() {
        let td = tempfile::tempdir().unwrap();
        let store = ProjectTrustStore::at(td.path().join("trust.json"));
        let root = td.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        assert!(matches!(
            ensure_trusted(&store, &root, &[]),
            TrustVerdict::Trusted { .. }
        ));
    }

    #[test]
    fn corrupt_store_fails_closed_and_quarantines() {
        let td = tempfile::tempdir().unwrap();
        let store_path = td.path().join("trust.json");
        write(&store_path, "{ not json !!!");
        let store = ProjectTrustStore::at(&store_path);
        let root = td.path().join("proj");
        write(&root.join(".okra/skills/SKILL-a.md"), "a");
        let files = vec![root.join(".okra/skills/SKILL-a.md")];
        assert_eq!(ensure_trusted(&store, &root, &files), TrustVerdict::Untrusted);
        // the corrupt file was moved aside, not trusted through
        assert!(!store_path.exists());
        assert!(td.path().join("trust.json.corrupt").exists());
    }
}
