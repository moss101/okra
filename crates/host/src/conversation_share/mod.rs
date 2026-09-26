//! Conversation share (MASTER-PLAN §3 #48, from ZCode
//! `conversation-share/`): integrity contracts, public projection,
//! artifact discovery kernels, and the share HTTP client.

pub mod artifact_source;
pub mod artifacts;
pub mod http;
pub mod integrity;
pub mod projection;

pub use artifact_source::{
    ArtifactRead, ArtifactSourceError, ArtifactStat, LocalArtifactSource, MaterializedArtifact,
    RemoteArtifactSource, RemoteFileService,
};
pub use integrity::{
    build_integrity, canonical_json, sha256_canonical, verify_integrity, ShareError,
    ShareIntegrity,
};
pub use integrity::ProjectionErrorKind;
pub use projection::{build_public_projection, PublicProjection};
