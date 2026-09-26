//! Port of `wire-binary.ts` — cross-runtime binary pure functions for the v4
//! wire. Kept byte-exact with the donor; the golden vectors in `tests/` are
//! generated from the TS originals (see `tests/golden/generate.mjs`).

use crate::frame::WireFrameError;

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `crc32WireBytes` (`wire-binary.ts:10-19`): CRC-32/ISO-HDLC, emitted as
/// lowercase 8-hex-digit string.
pub fn crc32_wire_bytes(bytes: &[u8]) -> String {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (if crc & 1 != 0 { 0xedb88320 } else { 0 });
        }
    }
    format!("{:08x}", crc ^ 0xffff_ffff)
}

/// `encodeWireBytesBase64` (`wire-binary.ts:23-44`): standard alphabet with
/// `=` padding.
pub fn encode_wire_bytes_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let has_b = chunk.len() > 1;
        let has_c = chunk.len() > 2;
        let b = if has_b { chunk[1] as u32 } else { 0 };
        let c = if has_c { chunk[2] as u32 } else { 0 };
        out.push(BASE64_ALPHABET[(a >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(if has_b {
            BASE64_ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if has_c {
            BASE64_ALPHABET[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn alphabet_index(b: u8) -> Result<u32, WireFrameError> {
    BASE64_ALPHABET
        .iter()
        .position(|&a| a == b)
        .map(|p| p as u32)
        .ok_or(WireFrameError::InvalidBase64)
}

/// `decodeWireBase64` (`wire-binary.ts:46-65`): strict — canonical padding,
/// standard alphabet, no whitespace. Returns `Err` on any invalid input
/// (donor returns null).
pub fn decode_wire_base64(value: &str) -> Result<Vec<u8>, WireFrameError> {
    let bytes = value.as_bytes();
    if !bytes.len().is_multiple_of(4) || bytes.len() < 4 {
        return Err(WireFrameError::InvalidBase64);
    }
    // Canonical padding: `==` or `=` suffix only, none in the body
    // (`topicWireBase64Schema`, `wire-binary.ts:5-8`).
    let padding = if value.ends_with("==") {
        2
    } else if value.ends_with('=') {
        1
    } else {
        0
    };
    if bytes[..bytes.len() - padding].contains(&b'=') {
        return Err(WireFrameError::InvalidBase64);
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3 - padding);
    for quad in bytes.chunks(4) {
        let a = alphabet_index(quad[0])?;
        let b = alphabet_index(quad[1])?;
        let c = if quad[2] == b'=' { 0 } else { alphabet_index(quad[2])? };
        let d = if quad[3] == b'=' { 0 } else { alphabet_index(quad[3])? };
        let combined = (a << 18) | (b << 12) | (c << 6) | d;
        let take = 3 - padding_at(quad)?;
        out.extend_from_slice(&[(combined >> 16) as u8, (combined >> 8) as u8, combined as u8][..take]);
    }
    Ok(out)
}

/// Number of trailing `=` in one quad must match the file-wide padding count.
fn padding_at(quad: &[u8]) -> Result<usize, WireFrameError> {
    let eq = quad.iter().filter(|&&q| q == b'=').count();
    // padding chars may only appear in the final quad; enforced by the
    // body scan above, so `eq` here is authoritative for this quad.
    Ok(eq)
}
