//! Media preview (MASTER-PLAN §3 #48, from ZCode `media-preview/`): the
//! metadata a surface needs to render an image attachment without loading
//! or decoding the full file.
//!
//! Contracts:
//! - **magic-byte mime sniffing** — never trust the file extension:
//!   PNG/JPEG/GIF/WEBP/PDF are detected from their leading bytes;
//! - **dimension probing** for the common raster formats straight from
//!   the headers (PNG IHDR, GIF logical screen descriptor, JPEG SOFn
//!   scan) — no image decoder dependency, bounded reads only;
//! - **size caps**: an attachment over `max_bytes` is refused before any
//!   parsing work (the preview pipeline reads bounded).

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub mime: &'static str,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub size_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("media file exceeds the preview cap: {actual} > {max}")]
    TooLarge { actual: u64, max: u64 },
    #[error("media file is not a readable file")]
    NotAFile,
    #[error("media io: {0}")]
    Io(#[from] std::io::Error),
}

/// Magic-byte mime sniffing (extension-independent).
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    const SIGS: &[(&[u8], &str)] = &[
        (&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A], "image/png"),
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"%PDF-", "application/pdf"),
        (b"RIFF", "image/webp"), // + WEBP at offset 8
    ];
    for (sig, mime) in SIGS {
        if bytes.starts_with(sig) {
            // RIFF is only WEBP when the magic sits at offset 8
            if *mime == "image/webp" && (bytes.len() < 12 || &bytes[8..12] != b"WEBP") {
                continue;
            }
            return Some(mime);
        }
    }
    None
}

/// PNG IHDR: width/height are big-endian u32s at offsets 16/20.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

/// GIF logical screen descriptor: little-endian u16s at offsets 6/8.
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 {
        return None;
    }
    let w = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
    let h = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
    Some((w, h))
}

/// JPEG: scan for a SOFn (start-of-frame) marker and read its
/// big-endian u16 height/width. Bounded to the first 64 KiB of markers.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut pos = 2usize; // skip SOI
    while pos + 9 <= bytes.len().min(64 * 1024) {
        if bytes[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = bytes[pos + 1];
        // SOF0-SOF15 except DHT (C4), JPG (C8), DAC (CC)
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            if pos + 9 > bytes.len() {
                return None;
            }
            let h = u16::from_be_bytes(bytes[pos + 5..pos + 7].try_into().ok()?) as u32;
            let w = u16::from_be_bytes(bytes[pos + 7..pos + 9].try_into().ok()?) as u32;
            return Some((w, h));
        }
        // skip this segment: 2 marker bytes + 2 length bytes + payload
        if pos + 4 > bytes.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes(bytes[pos + 2..pos + 4].try_into().ok()?) as usize;
        pos += 2 + seg_len;
    }
    None
}

/// Dimensions for a sniffed raster format; None for non-image mimes.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match sniff_mime(bytes)? {
        "image/png" => png_dimensions(bytes),
        "image/gif" => gif_dimensions(bytes),
        "image/jpeg" => jpeg_dimensions(bytes),
        _ => None,
    }
}

/// Probe a media file: mime + dimensions + size, bounded by `max_bytes`.
pub fn probe(path: &Path, max_bytes: u64) -> Result<MediaInfo, MediaError> {
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => MediaError::NotAFile,
        _ => MediaError::Io(e),
    })?;
    if !meta.is_file() {
        return Err(MediaError::NotAFile);
    }
    if meta.len() > max_bytes {
        return Err(MediaError::TooLarge {
            actual: meta.len(),
            max: max_bytes,
        });
    }
    let bytes = std::fs::read(path)?;
    let mime = sniff_mime(&bytes).unwrap_or("application/octet-stream");
    let (width, height) = image_dimensions(&bytes)
        .map(|(w, h)| (Some(w), Some(h)))
        .unwrap_or((None, None));
    Ok(MediaInfo {
        mime,
        width,
        height,
        size_bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, // signature
        ];
        bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR length
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&w.to_be_bytes());
        bytes.extend_from_slice(&h.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]); // depth/color/compress/filter/interlace
        bytes.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
        bytes
    }

    fn jpeg_bytes(w: u16, h: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF]; // SOI
        bytes.extend_from_slice(&[0xE0, 0x00, 0x10]); // APP0 segment (len 16)
        bytes.extend_from_slice(&[0u8; 14]); // APP0 payload
        bytes.extend_from_slice(&[0xFF, 0xC0]); // SOF0
        bytes.extend_from_slice(&[0x00, 0x11]); // segment length 17
        bytes.extend_from_slice(&[8]); // precision
        bytes.extend_from_slice(&h.to_be_bytes());
        bytes.extend_from_slice(&w.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0, 2, 0]); // components
        bytes
    }

    #[test]
    fn sniffs_mime_by_magic_bytes_not_extension() {
        assert_eq!(
            sniff_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(sniff_mime(b"\xFF\xD8\xFF\xE0whatever"), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_mime(b"%PDF-1.7"), Some("application/pdf"));
        // RIFF without WEBP at offset 8 is not webp
        assert_eq!(sniff_mime(b"RIFFxxxxNOTW"), None);
        assert_eq!(sniff_mime(b"plain text"), None);
    }

    #[test]
    fn png_gif_jpeg_dimensions_probe() {
        let png = png_bytes(1920, 1080);
        assert_eq!(image_dimensions(&png), Some((1920, 1080)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&240u16.to_le_bytes());
        gif.extend_from_slice(&[0xF0, 0, 0]); // GCT flag + bg + aspect
        assert_eq!(image_dimensions(&gif), Some((320, 240)));

        let jpeg = jpeg_bytes(640, 480);
        assert_eq!(image_dimensions(&jpeg), Some((640, 480)));

        // non-image: no dimensions
        assert_eq!(image_dimensions(b"%PDF-1.7"), None);
    }

    #[test]
    fn probe_reports_mime_dimensions_and_enforces_the_cap() {
        let td = tempfile::tempdir().unwrap();
        let png = td.path().join("shot.png");
        std::fs::write(&png, png_bytes(800, 600)).unwrap();
        let info = probe(&png, 1 << 20).unwrap();
        assert_eq!(info.mime, "image/png");
        assert_eq!(info.width, Some(800));
        assert_eq!(info.height, Some(600));

        // cap: oversize media refuses before parsing
        let e = probe(&png, 10).unwrap_err();
        assert!(matches!(e, MediaError::TooLarge { .. }));

        // a text file sniffs as octet-stream with no dimensions
        let txt = td.path().join("notes.txt");
        std::fs::write(&txt, "plain").unwrap();
        let info = probe(&txt, 1 << 20).unwrap();
        assert_eq!(info.mime, "application/octet-stream");
        assert_eq!(info.width, None);
    }

    #[test]
    fn missing_and_non_files_fail() {
        let td = tempfile::tempdir().unwrap();
        assert!(matches!(probe(&td.path().join("nope.png"), 100), Err(MediaError::NotAFile)));
        std::fs::create_dir(td.path().join("dir")).unwrap();
        assert!(matches!(probe(&td.path().join("dir"), 100), Err(MediaError::NotAFile)));
    }
}
