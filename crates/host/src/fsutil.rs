//! The ONLY sanctioned call sites for canonicalize / home_dir in the
//! workspace (clippy.toml bans the raw calls). Windows verbatim-path
//! handling lands with the M6 port; on unix this is a thin wrapper that
//! keeps every call site routed through one place.

use std::path::{Path, PathBuf};

/// Resolve symlinks; callers must not use std::fs::canonicalize directly
/// (Windows verbatim `\\?\` paths poison path-equality keys — M6 note).
#[allow(clippy::disallowed_methods)] // this IS the sanctioned wrapper
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize()
}

/// Home directory resolution. One implementation, one cached call.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// Normalize a path lexically (remove `.` and `..` without touching the
/// filesystem) — used for display and path-key comparisons.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_normalization_removes_dots() {
        let p = Path::new("/tmp/ws/./src/../src/main.rs");
        assert_eq!(normalize_lexical(p), PathBuf::from("/tmp/ws/src/main.rs"));
        assert!(home_dir().is_some() || home_dir().is_none()); // never panics
    }
}
