// Golden-vector generator for okra-protocol convergence tests.
//
// The functions below are copied VERBATIM (bodies untouched) from ZCode
// packages/shared/src/zcode-protocol-v4/wire-binary.ts and coalesce.ts
// (Apache-2.0), with only the zod schema declarations and the
// workflow-run merge (rule 6, not ported to okra yet) stripped so the
// script runs standalone under `node`. This is the TS ground truth the
// Rust implementation must converge with byte-for-byte.
//
// Usage: node generate.mjs > golden-vectors.json
// Regenerate only when the upstream source of truth changes; the checked-in
// golden-vectors.json is what CI tests against.

// ---- verbatim from wire-binary.ts (minus zod schema) ----
const BASE64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

export function crc32WireBytes(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = (crc >>> 1) ^ (crc & 1 ? 0xedb88320 : 0);
    }
  }
  return ((crc ^ 0xffffffff) >>> 0).toString(16).padStart(8, "0");
}

export function encodeWireBytesBase64(bytes) {
  let result = "";
  let block = [];
  for (let offset = 0; offset < bytes.byteLength; offset += 3) {
    const a = bytes[offset] ?? 0;
    const hasB = offset + 1 < bytes.byteLength;
    const hasC = offset + 2 < bytes.byteLength;
    const b = hasB ? (bytes[offset + 1] ?? 0) : 0;
    const c = hasC ? (bytes[offset + 2] ?? 0) : 0;
    block.push(
      BASE64_ALPHABET[a >>> 2],
      BASE64_ALPHABET[((a & 0x03) << 4) | (b >>> 4)],
      hasB ? BASE64_ALPHABET[((b & 0x0f) << 2) | (c >>> 6)] : "=",
      hasC ? BASE64_ALPHABET[c & 0x3f] : "=",
    );
    if (block.length >= 16_384) {
      result += block.join("");
      block = [];
    }
  }
  return result + block.join("");
}

export function decodeWireBase64(value) {
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0;
  const output = new Uint8Array((value.length / 4) * 3 - padding);
  let writeOffset = 0;
  for (let offset = 0; offset < value.length; offset += 4) {
    const a = BASE64_ALPHABET.indexOf(value[offset]);
    const b = BASE64_ALPHABET.indexOf(value[offset + 1]);
    const c = value[offset + 2] === "=" ? 0 : BASE64_ALPHABET.indexOf(value[offset + 2]);
    const d = value[offset + 3] === "=" ? 0 : BASE64_ALPHABET.indexOf(value[offset + 3]);
    if (a < 0 || b < 0 || c < 0 || d < 0) return null;
    const combined = (a << 18) | (b << 12) | (c << 6) | d;
    if (writeOffset < output.length) output[writeOffset++] = combined >>> 16;
    if (writeOffset < output.length) {
      output[writeOffset++] = (combined >>> 8) & 0xff;
    }
    if (writeOffset < output.length) output[writeOffset++] = combined & 0xff;
  }
  return output;
}

// ---- verbatim from coalesce.ts (minus workflow-run rule 6) ----
function isBarrier(delta) {
  return delta.op === "row.removed";
}

export function coalesceConversationDeltas(deltas) {
  const result = [];
  for (const delta of deltas) {
    if (delta.op === "row.upserted") {
      for (let i = result.length - 1; i >= 0; i--) {
        const prev = result[i];
        if (prev === undefined || isBarrier(prev)) break;
        if (prev.op === "row.delta" && prev.rowId === delta.row.rowId) {
          result.splice(i, 1);
          continue;
        }
        if (
          (prev.op === "row.upserted" || prev.op === "row.appended") &&
          prev.row.rowId === delta.row.rowId
        ) {
          break;
        }
      }
    }
    const last = result[result.length - 1];
    if (
      delta.op === "row.delta" &&
      last?.op === "row.delta" &&
      last.rowId === delta.rowId &&
      last.path === delta.path
    ) {
      result[result.length - 1] = {
        op: "row.delta",
        rowId: delta.rowId,
        path: delta.path,
        append: last.append + delta.append,
      };
      continue;
    }
    if (delta.op === "state.updated" && last?.op === "state.updated") {
      result[result.length - 1] = {
        op: "state.updated",
        patch: { ...last.patch, ...delta.patch },
      };
      continue;
    }
    if (
      delta.op === "row.upserted" &&
      last?.op === "row.upserted" &&
      last.row.rowId === delta.row.rowId
    ) {
      result[result.length - 1] = delta;
      continue;
    }
    result.push(delta);
  }
  return result;
}

// conflateByKey, verbatim from coalesce.ts.
export function conflateByKey(items, keyOf) {
  const lastIndexByKey = new Map();
  items.forEach((item, index) => {
    lastIndexByKey.set(keyOf(item), index);
  });
  return items.filter((item, index) => lastIndexByKey.get(keyOf(item)) === index);
}

// ---- vector generation ----
// Deterministic xorshift PRNG so the Rust side replays the identical sequence.
function makeRng(seed) {
  let s = seed >>> 0;
  return () => {
    s ^= s << 13; s >>>= 0;
    s ^= s >>> 17;
    s ^= s << 5; s >>>= 0;
    return s / 0x100000000;
  };
}

const DELTA_OPS = ["row.appended", "row.upserted", "row.delta", "row.removed", "state.updated"];
const PATHS = ["text", "inputText", "output.text", "summaryText"];

function randomDelta(rng, maxRowId) {
  const op = DELTA_OPS[Math.floor(rng() * DELTA_OPS.length)];
  switch (op) {
    case "row.appended":
    case "row.upserted": {
      const rowId = Math.floor(rng() * maxRowId);
      // kind tag: fixtures are encoded in okra's row wire shape so the Rust
      // side can deserialize them; the donor functions treat rows opaquely.
      return { op, row: { rowId, entityId: `e${rowId}`, text: `r${rowId}`, kind: "userMessage" } };
    }
    case "row.delta": {
      const rowId = Math.floor(rng() * maxRowId);
      return {
        op,
        rowId,
        path: PATHS[Math.floor(rng() * PATHS.length)],
        append: "frag" + Math.floor(rng() * 100),
      };
    }
    case "row.removed":
      return { op, fromRowId: Math.floor(rng() * maxRowId) };
    case "state.updated": {
      const keys = ["a", "b", "c"];
      const patch = {};
      patch[keys[Math.floor(rng() * keys.length)]] = Math.floor(rng() * 1000);
      return { op, patch };
    }
  }
}

// Authoritative reducer: applies deltas to (rows, state). Same shape the Rust
// side implements; both must agree after coalesce or without it.
function applyDeltas(deltas) {
  const rows = new Map();
  const state = {};
  for (const d of deltas) {
    switch (d.op) {
      case "row.appended":
        rows.set(d.row.rowId, d.row);
        break;
      case "row.upserted":
        rows.set(d.row.rowId, d.row);
        break;
      case "row.removed":
        for (const id of [...rows.keys()]) if (id >= d.fromRowId) rows.delete(id);
        break;
      case "row.delta": {
        const row = rows.get(d.rowId);
        if (row) row.text = (row.text ?? "") + d.append;
        break;
      }
      case "state.updated":
        Object.assign(state, d.patch);
        break;
    }
  }
  return {
    rows: [...rows.entries()].sort((x, y) => x[0] - y[0]),
    state,
  };
}

const rng = makeRng(42);
const cases = [];
for (let i = 0; i < 200; i += 1) {
  const count = 1 + Math.floor(rng() * 24);
  const deltas = [];
  for (let j = 0; j < count; j += 1) deltas.push(randomDelta(rng, 6));
  const coalesced = coalesceConversationDeltas(deltas);
  cases.push({
    input: deltas,
    coalesced,
    applyInput: applyDeltas(deltas),
    applyCoalesced: applyDeltas(coalesced),
  });
}

const crcVectors = [];
const byteSets = [
  [],
  [0],
  [0x61, 0x62, 0x63], // "abc" — classic crc32 check value c241243c
  Buffer.from("okra protocol v4 wire", "utf8"),
  Array.from({ length: 256 }, (_, k) => k),
  Array.from({ length: 4096 }, (_, k) => (k * 7 + 13) & 0xff),
];
for (const bytes of byteSets) {
  const u8 = Uint8Array.from(bytes);
  const b64 = encodeWireBytesBase64(u8);
  crcVectors.push({
    hex: Buffer.from(u8).toString("hex"),
    crc32: crc32WireBytes(u8),
    base64: b64,
    roundtrip: decodeWireBase64(b64) ? Buffer.from(decodeWireBase64(b64)).toString("hex") : null,
  });
}

const conflated = conflateByKey(
  [{ k: "a", v: 1 }, { k: "b", v: 2 }, { k: "a", v: 3 }, { k: "c", v: 4 }, { k: "b", v: 5 }],
  (item) => item.k,
);

console.log(JSON.stringify({
  generatedBy: "generate.mjs — TS ground truth from ZCode zcode-protocol-v4",
  crcBase64: crcVectors,
  coalesceCases: cases,
  conflateByKey: conflated,
}, null, 1));
