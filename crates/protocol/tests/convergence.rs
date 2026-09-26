//! Byte-convergence golden tests — RECREATED (MASTER-PLAN §3 #3).
//!
//! ZCode references this suite at `profiles.ts:5-8` ("两 profile 终态逐字节
//! 一致") but the public tree does not contain it. The golden vectors here are
//! generated from the TS *originals* (`tests/golden/generate.mjs` runs the
//! verbatim donor functions under node); this suite proves the Rust port
//! converges byte-for-byte with that ground truth:
//!
//! 1. crc32/base64 byte-exactness against `wire-binary.ts`
//! 2. coalesce output equality against `coalesce.ts` over 200 seeded random
//!    delta sequences
//! 3. the profile-closure invariant: coalesce(apply) state == apply-state,
//!    i.e. merging never changes final state (the donor's core contract,
//!    `coalesce.ts:2`)
//! 4. wire fragmentation: fragment → reassemble roundtrip, checksum faults,
//!    and byte-budget fail-closed behavior

use okra_protocol as proto;

use serde_json::Value;
use proto::{
    apply_delta, coalesce_conversation_deltas, conflate_by_key, ConversationDelta,
    ConversationState, ConversationRow, RowBase, StreamablePath, TopicWireChecksum,
    TopicWireFrame, TopicFrameDeliveryKind, WireVersion, ProtocolV4Limits,
    crc32_wire_bytes, decode_wire_base64, encode_wire_bytes_base64,
};

fn golden() -> serde_json::Value {
    let raw = include_str!("golden/golden-vectors.json");
    serde_json::from_str(raw).expect("golden vectors parse")
}

// ---- Rust-side replicas of the golden generator's helpers ----

fn row(row_id: u64) -> ConversationRow {
    ConversationRow::UserMessage {
        base: RowBase { row_id, entity_id: Some(format!("e{row_id}")), product_turn_id: None, edit_disposition: None },
        text: format!("r{row_id}"),
    }
}

fn replay_input(v: &serde_json::Value) -> Vec<ConversationDelta> {
    let mut out = Vec::new();
    for d in v["input"].as_array().expect("input array") {
        out.push(serde_json::from_value(d.clone()).expect("delta decode"));
    }
    out
}

/// The generator's `applyDeltas`, reproduced with okra's reducer.
fn apply_all(deltas: &[ConversationDelta]) -> ConversationState {
    let mut state = ConversationState::default();
    for d in deltas {
        apply_delta(&mut state, d);
    }
    state
}

fn canon(state: &ConversationState) -> String {
    // canonical form matching the generator's rows/state shape; state keys
    // are SORTED explicitly (serde_json Map ordering can become
    // insertion-ordered when another dependency enables `preserve_order`)
    let rows: Vec<(u64, String)> = state
        .rows
        .iter()
        .map(|r| {
            let text = match r {
                ConversationRow::UserMessage { text, .. } => text.clone(),
                ConversationRow::Response { text, .. } => text.clone(),
                ConversationRow::ToolInvocation { tool_name, .. } => tool_name.clone(),
            };
            (r.row_id(), text)
        })
        .collect();
    let sorted: std::collections::BTreeMap<&String, &Value> =
        state.state.iter().collect();
    format!("{rows:?}|{sorted:?}")
}

#[test]
fn crc32_and_base64_match_ts_ground_truth() {
    let golden = golden();
    let vectors = golden["crcBase64"].as_array().unwrap();
    assert!(vectors.len() >= 6, "expected the full vector set");
    for v in vectors {
        let hex = v["hex"].as_str().unwrap();
        let bytes = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
            .collect::<Vec<u8>>();
        assert_eq!(crc32_wire_bytes(&bytes), v["crc32"].as_str().unwrap(), "crc32 of {hex}");
        let b64 = encode_wire_bytes_base64(&bytes);
        assert_eq!(b64, v["base64"].as_str().unwrap(), "base64 of {hex}");
        if bytes.is_empty() {
            // donor schema requires >= 4 chars; empty decode is invalid input
            assert!(decode_wire_base64(&b64).is_err());
        } else {
            assert_eq!(
                hex::encode(&decode_wire_base64(&b64).unwrap()),
                v["roundtrip"].as_str().unwrap()
            );
        }
    }
}

// minimal hex helper (no hex crate dependency for one test file)
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[test]
fn crc32_known_check_value() {
    // CRC-32/ISO-HDLC("abc") = 0xc241243c
    assert_eq!(crc32_wire_bytes(b"abc"), "352441c2");
}

#[test]
fn coalesce_matches_ts_ground_truth_on_all_seeded_cases() {
    let golden = golden();
    let cases = golden["coalesceCases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 200, "golden case count");
    for (i, case) in cases.iter().enumerate() {
        let deltas = replay_input(case);

        // 1. coalesce output equals the TS coalesce output (as wire JSON —
        //    delta ops carry stable field names, so Value equality is the
        //    byte-convergence proxy).
        let rust_out = coalesce_conversation_deltas(&deltas);
        let rust_json: Vec<serde_json::Value> = rust_out
            .iter()
            .map(|d| serde_json::to_value(d).unwrap())
            .collect();
        let ts_json: Vec<serde_json::Value> = case["coalesced"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            rust_json.len(),
            ts_json.len(),
            "case {i}: coalesced length differs"
        );
        for (r, t) in rust_json.iter().zip(ts_json.iter()) {
            assert_eq!(r, t, "case {i}: coalesced delta differs\n rust={r}\n ts={t}");
        }

        // 2. Equivalence invariant: applying coalesced == applying input.
        assert_eq!(
            canon(&apply_all(&deltas)),
            canon(&apply_all(&rust_out)),
            "case {i}: coalesce changed final state"
        );
        // 3. The generator verified the same invariant in TS; both must hold
        //    on the same data (defense against golden-file staleness).
        assert_eq!(
            case["applyInput"],
            case["applyCoalesced"],
            "case {i}: golden file self-inconsistent"
        );
    }
}

#[test]
fn conflate_by_key_matches_ts_ground_truth() {
    #[derive(Clone)]
    struct Item {
        k: String,
        v: u64,
    }
    let items = [
        Item { k: "a".into(), v: 1 },
        Item { k: "b".into(), v: 2 },
        Item { k: "a".into(), v: 3 },
        Item { k: "c".into(), v: 4 },
        Item { k: "b".into(), v: 5 },
    ];
    let out = conflate_by_key(&items, |i| &i.k);
    let got: Vec<(String, u64)> = out.iter().map(|i| (i.k.clone(), i.v)).collect();
    assert_eq!(got, vec![("a".into(), 3), ("c".into(), 4), ("b".into(), 5)]);
}

#[test]
fn delivery_profiles_match_donor_values() {
    // continuous (`core.ts:36-46`)
    let c = proto::delivery_profile(proto::DeliveryProfileName::Continuous);
    assert!(c.desktop_only_rows);
    assert_eq!(c.flush_window_ms, 30);
    assert!(c.stream_paths.get(StreamablePath::InputText));
    assert!(c.stream_paths.get(StreamablePath::OutputText));
    assert_eq!(c.stream_output_cap_bytes, 262_144);
    assert!(!c.tool_progress);
    // replayable (`core.ts:47-57`)
    let r = proto::delivery_profile(proto::DeliveryProfileName::Replayable);
    assert!(!r.desktop_only_rows);
    assert_eq!(r.flush_window_ms, 150);
    assert!(!r.stream_paths.get(StreamablePath::InputText));
    assert_eq!(r.stream_output_cap_bytes, 0);
    assert!(r.tool_progress);
}

#[test]
fn profile_filter_closure_replayable() {
    // profiles.ts:27-35: row.delta filtered by streamPaths; structural ops
    // always pass. Under `replayable`, inputText deltas are dropped — and
    // the closure invariant (the row.upserted that finalizes input carries
    // the full inputText) is what keeps both profiles' final state identical.
    let r = proto::delivery_profile(proto::DeliveryProfileName::Replayable);
    let deltas = vec![
        ConversationDelta::RowAppended { row: row(1) },
        ConversationDelta::RowDelta {
            row_id: 1,
            path: StreamablePath::InputText,
            append: "typed ".into(),
        },
        ConversationDelta::RowDelta {
            row_id: 1,
            path: StreamablePath::Text,
            append: "streamed".into(),
        },
        ConversationDelta::RowUpserted {
            row: ConversationRow::Response {
                base: RowBase { row_id: 1, entity_id: Some("e1".into()), product_turn_id: None, edit_disposition: None },
                text: "typed full".into(),
                state: proto::ResponseState::Complete,
            },
        },
    ];
    let filtered: Vec<ConversationDelta> = deltas
        .iter()
        .filter(|d| match d {
            ConversationDelta::RowDelta { path, .. } => r.stream_paths.get(*path),
            _ => true,
        })
        .cloned()
        .collect();
    assert_eq!(filtered.len(), 3, "inputText delta must be filtered under replayable");
    // final states still converge because the upsert carries the full text
    assert_eq!(
        canon(&apply_all(&deltas)),
        canon(&apply_all(&filtered))
    );
}

// ---- wire fragmentation ----

fn count_envelope_bytes(wire: &TopicWireFrame, _frame: &serde_json::Value) -> usize {
    // the CLI NDJSON envelope shape from wire-codec.ts:31-35
    let body = serde_json::json!({ "method": "v4/conversation/frame", "params": wire });
    serde_json::to_vec(&body).unwrap().len() + 1
}

fn encode(frame: &serde_json::Value, max: usize) -> Vec<TopicWireFrame> {
    proto::encode_topic_wire_frames(
        frame,
        proto::EncodeTopicWireFramesOptions {
            delivery_kind: TopicFrameDeliveryKind::Online,
            topic: "conversation/s1".into(),
            subscription_id: "sub-1".into(),
            logical_frame_id: "lf-1".into(),
            logical_frame_ordinal: 7,
            max_physical_frame_bytes: Some(max),
            max_assembly_bytes: None,
            measure_physical_frame_bytes: &count_envelope_bytes,
        },
    )
    .unwrap()
}

#[test]
fn small_frame_travels_complete() {
    let frame = serde_json::json!({ "hello": "okra" });
    let wires = encode(&frame, ProtocolV4Limits::MAX_FRAME_BYTES);
    assert_eq!(wires.len(), 1);
    assert!(matches!(wires[0], TopicWireFrame::Complete { .. }));
}

#[test]
fn oversized_frame_fragments_and_reassembles_byte_identically() {
    let payload: String = "okra🦀".repeat(50_000); // multi-byte UTF-8, > 1MiB JSON
    let frame = serde_json::json!({ "rows": payload });

    let logical = serde_json::to_vec(&frame).unwrap();
    let wires = encode(&frame, 64 * 1024);
    assert!(wires.len() > 1, "must fragment");

    // every fragment respects the physical budget
    for w in &wires {
        assert!(count_envelope_bytes(w, &frame) <= 64 * 1024);
    }
    // reassembly is byte-identical to the original JSON
    let rebuilt = proto::assemble_fragments(&wires).unwrap();
    assert_eq!(rebuilt, logical);
    let decoded: serde_json::Value = serde_json::from_slice(&rebuilt).unwrap();
    assert_eq!(decoded["rows"], payload);
}

#[test]
fn fragment_corruption_is_a_typed_fault() {
    let frame = serde_json::json!({ "rows": "x".repeat(200_000) });
    let mut wires = encode(&frame, 32 * 1024);

    // drop one fragment → count mismatch
    let dropped = wires.pop().unwrap();
    assert!(proto::assemble_fragments(&wires).is_err());
    wires.push(dropped);

    // flip payload bytes → checksum fault
    if let TopicWireFrame::Fragment { data_base64, checksum, .. } = &mut wires[1] {
        let mut bytes = decode_wire_base64(data_base64).unwrap();
        bytes[0] ^= 0xff;
        *data_base64 = encode_wire_bytes_base64(&bytes);
        let _ = checksum;
        assert!(proto::assemble_fragments(&wires).is_err());
    } else {
        panic!("expected fragment");
    }
}

#[test]
fn assembly_budget_fail_closed() {
    let frame = serde_json::json!({ "rows": "x".repeat(200) });
    let err = proto::encode_topic_wire_frames(
        &frame,
        proto::EncodeTopicWireFramesOptions {
            delivery_kind: TopicFrameDeliveryKind::Initial,
            topic: "t".into(),
            subscription_id: "s".into(),
            logical_frame_id: "lf".into(),
            logical_frame_ordinal: 1,
            max_physical_frame_bytes: None,
            max_assembly_bytes: Some(10), // logical JSON far exceeds 10 bytes
            measure_physical_frame_bytes: &count_envelope_bytes,
        },
    )
    .unwrap_err();
    assert!(matches!(err, proto::WireFrameError::FrameAssemblyTooLarge));
}

#[test]
fn wire_version_is_literal() {
    // WireVersion is a bare literal on the wire (z.literal(3)).
    assert!(serde_json::from_value::<WireVersion>(serde_json::json!(2)).is_err());
    assert!(serde_json::from_value::<WireVersion>(serde_json::json!(3)).is_ok());
    assert!(serde_json::from_value::<WireVersion>(serde_json::json!("3")).is_err());
}

#[test]
fn checksum_schema_is_strict() {
    let ok: TopicWireChecksum = serde_json::from_value(serde_json::json!({
        "algorithm": "crc32", "value": "352441c2"
    }))
    .unwrap();
    assert_eq!(ok.value, "352441c2");
    // uppercase rejected (regex ^[0-9a-f]{8}$)
    assert!(
        serde_json::from_value::<TopicWireChecksum>(serde_json::json!({
            "algorithm": "crc32", "value": "352441C2"
        }))
        .is_err()
    );
}
