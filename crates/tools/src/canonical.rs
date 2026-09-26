//! Sanctioned canonicalize wrapper for the tools crate (clippy.toml bans
//! raw `Path::canonicalize`: Windows verbatim `\\?\` paths poison
//! path-equality keys — M6 note). The host crate owns the global wrapper
//! (`okra_host::fsutil`); tools sits BELOW host in the dependency graph, so
//! this module is the crate-local sanctioned site until the M3 host seam.

/// Resolve symlinks for containment checks.
#[allow(clippy::disallowed_methods)] // sanctioned wrapper (this module)
pub fn canonicalize(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    path.canonicalize()
}
