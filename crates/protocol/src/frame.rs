//! Port of `wire.ts` + `wire-codec.ts` — the V4 physical wire schema.
//!
//! `wire.ts:1-2`: a logical topic frame keeps atomic seq semantics; oversized
//! frames fragment **only at the UTF-8 byte layer**, never changing
//! seq/apply semantics.
//!
//! `wire.ts:45-48`: the service boundary validates only the outer shape that
//! is safe to route/meter; full validation of ranges/base64/checksums and the
//! logical payload happens in the ownership-filtered assembler, which
//! produces typed faults. An early warn/drop at the service boundary would
//! leave the store waiting forever.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::{ProtocolV4Limits, V4_WIRE_PROTOCOL_VERSION};

/// Hard ceiling applied when a caller passes a larger `max_physical_frame_bytes`
/// (`wire-codec.ts:96-99`, `hardBound` with `PROTOCOL_V4_LIMITS.maxFrameBytes`).
pub const MAX_FRAME_BYTES_LIMIT: usize = ProtocolV4Limits::MAX_FRAME_BYTES;

/// publisher-reserved marker for a physical frame's purpose (`wire.ts:15-17`);
/// consumers must never infer it from RPC timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TopicFrameDeliveryKind {
    Initial,
    Online,
    Recovery,
}

/// `topicWireChecksumSchema` (`wire.ts:7-13`): crc32, 8 lowercase hex chars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TopicWireChecksum {
    pub algorithm: Crc32Algorithm,
    /// 8 lowercase hex digits, regex `^[0-9a-f]{8}$` — enforced on
    /// deserialization exactly like the donor schema.
    pub value: String,
}

impl<'de> Deserialize<'de> for TopicWireChecksum {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            algorithm: Crc32Algorithm,
            value: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        let valid = raw.value.len() == 8
            && raw
                .value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid {
            return Err(serde::de::Error::custom(
                "checksum value must match ^[0-9a-f]{8}$",
            ));
        }
        Ok(TopicWireChecksum { algorithm: raw.algorithm, value: raw.value })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Crc32Algorithm {
    Crc32,
}

impl TopicWireChecksum {
    pub fn crc32(value: String) -> Self {
        Self { algorithm: Crc32Algorithm::Crc32, value }
    }
}

/// `TopicWireFrame` (`wire.ts:19-43`): either a complete frame or one
/// fragment of a logical frame's serialized JSON bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TopicWireFrame {
    Complete {
        #[serde(rename = "wireVersion")]
        wire_version: WireVersion,
        #[serde(rename = "deliveryKind")]
        delivery_kind: TopicFrameDeliveryKind,
        #[serde(rename = "logicalFrameId")]
        logical_frame_id: String,
        #[serde(rename = "logicalFrameOrdinal")]
        logical_frame_ordinal: u64,
        topic: String,
        #[serde(rename = "subscriptionId")]
        subscription_id: String,
        frame: Value,
    },
    Fragment {
        #[serde(rename = "wireVersion")]
        wire_version: WireVersion,
        #[serde(rename = "deliveryKind")]
        delivery_kind: TopicFrameDeliveryKind,
        #[serde(rename = "logicalFrameId")]
        logical_frame_id: String,
        #[serde(rename = "logicalFrameOrdinal")]
        logical_frame_ordinal: u64,
        topic: String,
        #[serde(rename = "subscriptionId")]
        subscription_id: String,
        #[serde(rename = "fragmentIndex")]
        fragment_index: u64,
        #[serde(rename = "fragmentCount")]
        fragment_count: u64,
        #[serde(rename = "logicalBytes")]
        logical_bytes: u64,
        checksum: TopicWireChecksum,
        #[serde(rename = "dataBase64")]
        data_base64: String,
    },
}

/// Literal wire version marker — serializes as `3` and deserialization
/// rejects any other value (`z.literal(V4_WIRE_PROTOCOL_VERSION)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireVersion;

impl Serialize for WireVersion {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(V4_WIRE_PROTOCOL_VERSION)
    }
}

impl<'de> Deserialize<'de> for WireVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = u32::deserialize(deserializer)?;
        if v == V4_WIRE_PROTOCOL_VERSION {
            Ok(WireVersion)
        } else {
            Err(serde::de::Error::custom(format!(
                "wireVersion must be {V4_WIRE_PROTOCOL_VERSION}"
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireFrameError {
    #[error("invalid limit")]
    InvalidLimit(&'static str),
    #[error("logical frame exceeds assembly budget")]
    FrameAssemblyTooLarge,
    #[error("fragment envelope cannot fit within the physical frame budget")]
    FrameEnvelopeTooLarge,
    #[error("fragment count exceeded")]
    FrameFragmentCountExceeded,
    #[error("invalid base64")]
    InvalidBase64,
}

/// `hardBound` (`wire-codec.ts:94-100`): default to the maximum, reject
/// non-positive, floor, and cap.
fn hard_bound(value: Option<usize>, maximum: usize, name: &'static str) -> Result<usize, WireFrameError> {
    let resolved = value.unwrap_or(maximum);
    if resolved == 0 {
        return Err(WireFrameError::InvalidLimit(name));
    }
    Ok(resolved.min(maximum))
}

/// Options mirror `EncodeTopicWireFramesOptions` (`wire-codec.ts:102-111`).
/// `measure_physical_frame_bytes` is caller-supplied: encoding is measured
/// against the caller's *real* JSON/RPC envelope (`wire-codec.ts:1-2`) —
/// mobile relay base64s the channel payload again, so only the caller knows
/// its own worst-case shape.
pub struct EncodeTopicWireFramesOptions<'m, F> {
    pub delivery_kind: TopicFrameDeliveryKind,
    pub topic: String,
    pub subscription_id: String,
    pub logical_frame_id: String,
    pub logical_frame_ordinal: u64,
    pub max_physical_frame_bytes: Option<usize>,
    pub max_assembly_bytes: Option<usize>,
    pub measure_physical_frame_bytes: &'m dyn Fn(&TopicWireFrame, &F) -> usize,
}

fn make_fragment<'m, F>(
    options: &EncodeTopicWireFramesOptions<'m, F>,
    fragment_index: u64,
    fragment_count: u64,
    logical_bytes: u64,
    checksum: &TopicWireChecksum,
    data_base64: String,
) -> TopicWireFrame {
    TopicWireFrame::Fragment {
        wire_version: WireVersion,
        delivery_kind: options.delivery_kind,
        logical_frame_id: options.logical_frame_id.clone(),
        logical_frame_ordinal: options.logical_frame_ordinal,
        topic: options.topic.clone(),
        subscription_id: options.subscription_id.clone(),
        fragment_index,
        fragment_count,
        logical_bytes,
        checksum: TopicWireChecksum::crc32(checksum.value.clone()),
        data_base64,
    }
}

/// `findFragmentByteBudget` (`wire-codec.ts:137-167`): binary search the
/// largest payload chunk that still fits, measuring with the *worst-case*
/// index width (actual fragment envelopes can only be smaller).
fn find_fragment_byte_budget<F>(
    options: &EncodeTopicWireFramesOptions<'_, F>,
    frame: &F,
    logical_bytes: usize,
    checksum: &TopicWireChecksum,
    max_physical_frame_bytes: usize,
) -> usize {
    let mut low: usize = 1;
    let mut high: usize = logical_bytes.min(max_physical_frame_bytes);
    let mut best: usize = 0;
    let worst_count = logical_bytes as u64;
    while low <= high {
        let candidate = (low + high) / 2;
        let data_base64 = "A".repeat(candidate.div_ceil(3) * 4);
        let wire = make_fragment(
            options,
            worst_count - 1,
            worst_count,
            logical_bytes as u64,
            checksum,
            data_base64,
        );
        if (options.measure_physical_frame_bytes)(&wire, frame) <= max_physical_frame_bytes {
            best = candidate;
            low = candidate + 1;
        } else if candidate == 0 {
            break;
        } else {
            high = candidate - 1;
        }
    }
    best
}

/// `encodeTopicWireFrames` (`wire-codec.ts:169-240`).
///
/// Measures the caller's real JSON envelope; fragments the logical frame's
/// UTF-8 JSON bytes when the complete frame exceeds the physical budget; any
/// estimation drift fails closed (`wire-codec.ts:232-236`) — an oversized
/// frame must never reach downstream for silent truncation.
pub fn encode_topic_wire_frames<F: serde::Serialize>(
    frame: &F,
    options: EncodeTopicWireFramesOptions<'_, F>,
) -> Result<Vec<TopicWireFrame>, WireFrameError> {
    let max_physical_frame_bytes = hard_bound(
        options.max_physical_frame_bytes,
        ProtocolV4Limits::MAX_FRAME_BYTES,
        "maxPhysicalFrameBytes",
    )?;
    let max_assembly_bytes = hard_bound(
        options.max_assembly_bytes,
        ProtocolV4Limits::LOGICAL_FRAME_ASSEMBLY_MAX_BYTES,
        "maxAssemblyBytes",
    )?;

    let logical = serde_json::to_vec(frame)
        .map_err(|_| WireFrameError::FrameAssemblyTooLarge)?;
    if logical.len() > max_assembly_bytes {
        return Err(WireFrameError::FrameAssemblyTooLarge);
    }

    let frame_value = serde_json::to_value(frame)
        .map_err(|_| WireFrameError::FrameAssemblyTooLarge)?;
    let complete = TopicWireFrame::Complete {
        wire_version: WireVersion,
        delivery_kind: options.delivery_kind,
        logical_frame_id: options.logical_frame_id.clone(),
        logical_frame_ordinal: options.logical_frame_ordinal,
        topic: options.topic.clone(),
        subscription_id: options.subscription_id.clone(),
        frame: frame_value,
    };
    if (options.measure_physical_frame_bytes)(&complete, frame) <= max_physical_frame_bytes {
        return Ok(vec![complete]);
    }

    let checksum = TopicWireChecksum::crc32(crate::codec::crc32_wire_bytes(&logical));
    let chunk_bytes = find_fragment_byte_budget(
        &options,
        frame,
        logical.len(),
        &checksum,
        max_physical_frame_bytes,
    );
    if chunk_bytes < 1 {
        return Err(WireFrameError::FrameEnvelopeTooLarge);
    }
    let fragment_count = logical.len().div_ceil(chunk_bytes) as u64;
    if fragment_count as usize > ProtocolV4Limits::LOGICAL_FRAME_ASSEMBLY_MAX_FRAGMENTS {
        return Err(WireFrameError::FrameFragmentCountExceeded);
    }

    let mut frames = Vec::with_capacity(fragment_count as usize);
    for fragment_index in 0..fragment_count {
        let start = fragment_index as usize * chunk_bytes;
        let end = (start + chunk_bytes).min(logical.len());
        let wire = make_fragment(
            &options,
            fragment_index,
            fragment_count,
            logical.len() as u64,
            &checksum,
            crate::codec::encode_wire_bytes_base64(&logical[start..end]),
        );
        if (options.measure_physical_frame_bytes)(&wire, frame) > max_physical_frame_bytes {
            return Err(WireFrameError::FrameEnvelopeTooLarge);
        }
        frames.push(wire);
    }
    Ok(frames)
}

/// Reassemble fragments of one logical frame (`wire-reassembly.ts` semantics,
/// assembler-side validation from `wire.ts:45-48`). Typed faults, never
/// silent drops: wrong counts, duplicate or out-of-range indices, and
/// checksum mismatches are errors; the caller restarts the logical frame.
pub fn assemble_fragments(fragments: &[TopicWireFrame]) -> Result<Vec<u8>, WireFrameError> {
    let mut sorted: Vec<&TopicWireFrame> = fragments.iter().collect();
    sorted.sort_by_key(|f| match f {
        TopicWireFrame::Fragment { fragment_index, .. } => *fragment_index,
        TopicWireFrame::Complete { .. } => 0,
    });

    let (logical_bytes, expected_count, checksum) = match sorted.first() {
        Some(TopicWireFrame::Fragment {
            logical_bytes,
            fragment_count,
            checksum,
            ..
        }) => (*logical_bytes, *fragment_count, checksum.clone()),
        _ => return Err(WireFrameError::InvalidBase64), // no fragments: typed fault
    };
    if sorted.len() != expected_count as usize
        || expected_count as usize > ProtocolV4Limits::LOGICAL_FRAME_ASSEMBLY_MAX_FRAGMENTS
    {
        return Err(WireFrameError::FrameFragmentCountExceeded);
    }

    let mut logical = Vec::with_capacity(logical_bytes as usize);
    for (idx, frag) in sorted.iter().enumerate() {
        match frag {
            TopicWireFrame::Fragment {
                fragment_index,
                data_base64,
                ..
            } => {
                if *fragment_index != idx as u64 {
                    return Err(WireFrameError::FrameFragmentCountExceeded);
                }
                logical.extend_from_slice(&crate::codec::decode_wire_base64(data_base64)?);
            }
            TopicWireFrame::Complete { .. } => return Err(WireFrameError::InvalidBase64),
        }
    }
    if logical.len() as u64 != logical_bytes {
        return Err(WireFrameError::FrameAssemblyTooLarge);
    }
    if crate::codec::crc32_wire_bytes(&logical) != checksum.value {
        return Err(WireFrameError::FrameAssemblyTooLarge);
    }
    Ok(logical)
}
