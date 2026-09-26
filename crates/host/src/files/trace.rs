//! Trace upload packaging (MASTER-PLAN §3 #52, ChatGPT2 docs/03 §5 —
//! the donor's `uploadTraceRecording` flow): a recorded trace is gzipped,
//! a sidecar metadata JSON is written alongside it (classification
//! `trace-recording`, sizes, recording duration, correlation), the pair
//! goes to the feedback upload with a tag set, and temp staging is
//! cleaned up afterwards — on success AND on failure.
//!
//! okra defaults (clean-room; the study documents the flow, not the
//! numbers): 50 MiB raw cap before compression, 20 MiB compressed cap,
//! 0o600 temp files — traces carry tool inputs/outputs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::{json, Value};
use std::io::Read as _;

/// The donor's classification string for trace sidecars.
pub const TRACE_CLASSIFICATION: &str = "trace-recording";
/// Sidecar schema version (okra's own, additive evolution).
pub const TRACE_SIDECAR_VERSION: u32 = 1;
/// Raw recording cap, checked before compression.
pub const DEFAULT_MAX_TRACE_BYTES: u64 = 50 * 1024 * 1024;
/// Compressed payload cap, checked after gzip.
pub const DEFAULT_MAX_PACKAGED_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("trace exceeds the raw size cap: {actual} > {max}")]
    RawTooLarge { actual: u64, max: u64 },
    #[error("packaged trace exceeds the compressed size cap: {actual} > {max}")]
    PackagedTooLarge { actual: u64, max: u64 },
    #[error("trace io: {0}")]
    Io(#[from] std::io::Error),
    #[error("trace upload failed: {0}")]
    Upload(String),
    #[error("trace codec: {0}")]
    Codec(#[from] serde_json::Error),
}

/// One recorded trace: the raw bytes plus the recording context the
/// sidecar must carry.
#[derive(Debug, Clone)]
pub struct TraceRecording {
    pub bytes: Vec<u8>,
    /// Wall-clock length of the recording.
    pub duration_ms: u64,
    /// Correlation id linking the trace to its conversation/session.
    pub correlation_id: String,
    pub recorded_at_epoch_ms: u64,
}

/// The packaged result: gzip payload + sidecar metadata + the feedback
/// tag set, staged in temp files until uploaded.
#[derive(Debug, Clone)]
pub struct TracePackage {
    pub gzipped: Vec<u8>,
    pub sidecar: Value,
    pub tags: Vec<String>,
    pub gzip_path: PathBuf,
    pub sidecar_path: PathBuf,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `uploadTraceRecording` steps 1–2: gzip the trace, write the sidecar
/// metadata JSON, stage both in temp files (0o600).
pub fn package_trace(
    recording: &TraceRecording,
    tags: &[String],
    staging_dir: &Path,
    max_raw_bytes: u64,
    max_packaged_bytes: u64,
) -> Result<TracePackage, TraceError> {
    if recording.bytes.len() as u64 > max_raw_bytes {
        return Err(TraceError::RawTooLarge {
            actual: recording.bytes.len() as u64,
            max: max_raw_bytes,
        });
    }

    // gzip
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, &recording.bytes)?;
    let gzipped = encoder.finish()?;
    if gzipped.len() as u64 > max_packaged_bytes {
        return Err(TraceError::PackagedTooLarge {
            actual: gzipped.len() as u64,
            max: max_packaged_bytes,
        });
    }

    // sidecar metadata JSON (classification, sizes, duration, correlation)
    let sidecar = json!({
        "classification": TRACE_CLASSIFICATION,
        "sidecar_version": TRACE_SIDECAR_VERSION,
        "correlation_id": recording.correlation_id,
        "duration_ms": recording.duration_ms,
        "recorded_at_epoch_ms": recording.recorded_at_epoch_ms,
        "packaged_at_epoch_ms": now_ms(),
        "size_bytes_raw": recording.bytes.len(),
        "size_bytes_gzip": gzipped.len(),
        "tags": tags,
    });

    // temp staging, 0o600
    std::fs::create_dir_all(staging_dir)?;
    let serial = next_serial();
    let gzip_path = staging_dir.join(format!(
        "trace-{}-{serial}.json.gz",
        recording.correlation_id
    ));
    let sidecar_path = staging_dir.join(format!(
        "trace-{}-{serial}.meta.json",
        recording.correlation_id
    ));
    write_private(&gzip_path, &gzipped)?;
    write_private(
        &sidecar_path,
        serde_json::to_vec_pretty(&sidecar)?.as_slice(),
    )?;

    Ok(TracePackage {
        gzipped,
        sidecar,
        tags: tags.to_vec(),
        gzip_path,
        sidecar_path,
    })
}

fn next_serial() -> u64 {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    Ok(())
}

/// The feedback upload seam — the donor uploads traces with its feedback
/// pipeline; okra injects the implementation.
pub trait TraceUpload: Send + Sync {
    /// Upload the gzipped trace + sidecar with the tag set; returns the
    /// remote artifact id.
    fn upload(&self, package: &TracePackage) -> Result<String, String>;
}

/// `uploadTraceRecording` steps 3–4: feedback upload with the tag set,
/// then temp cleanup — guaranteed on success AND failure, so a failed
/// upload never leaves staged traces behind.
pub fn upload_trace(
    package: &TracePackage,
    uploader: &dyn TraceUpload,
) -> Result<String, TraceError> {
    let result = uploader.upload(package);
    let cleanup = |package: &TracePackage| {
        let _ = std::fs::remove_file(&package.gzip_path);
        let _ = std::fs::remove_file(&package.sidecar_path);
    };
    match result {
        Ok(id) => {
            cleanup(package);
            Ok(id)
        }
        Err(message) => {
            cleanup(package);
            Err(TraceError::Upload(message))
        }
    }
}

/// Verify a gzipped payload decompresses back to `expected` — used by
/// tests and by receivers validating a sidecar against its payload.
pub fn gunzip_equals(gzipped: &[u8], expected: &[u8]) -> bool {
    let mut decoder = GzDecoder::new(gzipped);
    let mut raw = Vec::new();
    if decoder.read_to_end(&mut raw).is_err() {
        return false;
    }
    raw == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(bytes: Vec<u8>) -> TraceRecording {
        TraceRecording {
            bytes,
            duration_ms: 4_200,
            correlation_id: "conv-17".into(),
            recorded_at_epoch_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn packaging_gzips_and_writes_sidecar() {
        let td = tempfile::tempdir().unwrap();
        let raw = b"trace line\n".repeat(1000);
        let tags = vec!["feedback".to_string(), "beta".to_string()];
        let package =
            package_trace(&recording(raw.clone()), &tags, td.path(), 1 << 20, 1 << 20).unwrap();

        // gzip round trip + real gzip magic
        assert_eq!(&package.gzipped[..2], &[0x1f, 0x8b]);
        assert!(gunzip_equals(&package.gzipped, &raw));

        // sidecar contract: classification, sizes, duration, correlation
        let sidecar = &package.sidecar;
        assert_eq!(sidecar["classification"], "trace-recording");
        assert_eq!(sidecar["correlation_id"], "conv-17");
        assert_eq!(sidecar["duration_ms"], 4_200);
        assert_eq!(sidecar["size_bytes_raw"], raw.len());
        assert_eq!(sidecar["size_bytes_gzip"], package.gzipped.len());
        assert_eq!(sidecar["tags"], serde_json::to_value(&tags).unwrap());

        // staged files exist and are 0o600
        assert!(package.gzip_path.exists());
        assert!(package.sidecar_path.exists());
        #[cfg(unix)]
        for path in [&package.gzip_path, &package.sidecar_path] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{path:?}");
        }
    }

    #[test]
    fn size_caps_refuse_before_and_after_compression() {
        let td = tempfile::tempdir().unwrap();
        // xorshift PRNG: deterministic bytes that gzip cannot shrink
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let incompressible: Vec<u8> = (0..10_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        // raw cap
        let err = package_trace(
            &recording(incompressible.clone()),
            &[],
            td.path(),
            1_000,
            1 << 20,
        )
        .unwrap_err();
        assert!(matches!(err, TraceError::RawTooLarge { .. }));
        // compressed cap (the incompressible payload barely shrinks)
        let err = package_trace(
            &recording(incompressible),
            &[],
            td.path(),
            1 << 20,
            1_000,
        )
        .unwrap_err();
        assert!(matches!(err, TraceError::PackagedTooLarge { .. }));
    }

    struct RecordingUploader {
        fail: bool,
    }

    impl TraceUpload for RecordingUploader {
        fn upload(&self, package: &TracePackage) -> Result<String, String> {
            // sidecar must be readable from disk at upload time
            assert!(package.gzip_path.exists());
            assert!(package.sidecar_path.exists());
            if self.fail {
                Err("feedback endpoint 503".into())
            } else {
                Ok("remote-trace-9".into())
            }
        }
    }

    #[test]
    fn upload_cleans_temp_on_success_and_failure() {
        let td = tempfile::tempdir().unwrap();
        let package = package_trace(
            &recording(b"trace".to_vec()),
            &["feedback".to_string()],
            td.path(),
            1 << 20,
            1 << 20,
        )
        .unwrap();
        let gzip_path = package.gzip_path.clone();
        let sidecar_path = package.sidecar_path.clone();

        // success
        let id = upload_trace(&package, &RecordingUploader { fail: false }).unwrap();
        assert_eq!(id, "remote-trace-9");
        assert!(!gzip_path.exists(), "temp cleaned after success");
        assert!(!sidecar_path.exists(), "temp cleaned after success");

        // failure: same cleanup guarantee
        let package = package_trace(
            &recording(b"trace-2".to_vec()),
            &[],
            td.path(),
            1 << 20,
            1 << 20,
        )
        .unwrap();
        let gzip_path = package.gzip_path.clone();
        let err = upload_trace(&package, &RecordingUploader { fail: true }).unwrap_err();
        assert!(err.to_string().contains("503"));
        assert!(!gzip_path.exists(), "temp cleaned after failure");
    }
}
