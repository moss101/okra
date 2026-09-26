//! Public projection (MASTER-PLAN §3 #48, from ZCode
//! `conversationSharePublicProjection.ts`): local rows become a closed,
//! cross-version-safe payload — terminal-only, URL-leak scanned,
//! subagent/hook rows filtered, identities pseudonymized into a closed
//! closure, every row rebuilt from a per-kind allow-list.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::integrity::{projection_error, ProjectionErrorKind, ShareError};


const ACTIVE_TOOL_STATUSES: [&str; 3] = ["inputStreaming", "pendingApproval", "running"];
/// Okra's public artifact ref scheme (donor: `zcode-artifact://share/…`).
const PUBLIC_ARTIFACT_REF_PREFIX: &str = "okra-artifact://share/";

fn is_public_artifact_ref(value: &str) -> bool {
    value
        .strip_prefix(PUBLIC_ARTIFACT_REF_PREFIX)
        .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-')))
        .unwrap_or(false)
}

/// Visit every string in a JSON tree.
fn visit_strings(value: &Value, visitor: &mut impl FnMut(&str)) {
    match value {
        Value::String(s) => visitor(s),
        Value::Array(items) => items.iter().for_each(|v| visit_strings(v, visitor)),
        Value::Object(map) => map.values().for_each(|v| visit_strings(v, visitor)),
        _ => {}
    }
}

fn assert_safe_string(value: &str) -> Result<(), ShareError> {
    if value.starts_with("okra-artifact://") {
        return Err(projection_error(
            ProjectionErrorKind::ArtifactProtocolNotReady,
            "Artifact references must be represented by formal conversation artifact rows",
        ));
    }
    if value.starts_with("data:") || value.starts_with("file:") {
        return Err(projection_error(
            ProjectionErrorKind::UnsafeStructure,
            "Local and inline URLs cannot be shared",
        ));
    }
    Ok(())
}

fn first_unsafe_string(value: &Value) -> Result<(), ShareError> {
    let mut err = None;
    visit_strings(value, &mut |s| {
        if err.is_none() {
            err = assert_safe_string(s).err();
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `assertTerminalAndSafe`: only settled conversations may be shared, and
/// no string may carry a local or inline URL (artifact rows are exempt —
/// their internal `ref` is replaced by the projection; tool outputs are
/// exempt for their truncated-output refs, which never surface).
fn assert_terminal_and_safe(rows: &[Value]) -> Result<(), ShareError> {
    for row in rows {
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
        let state = row.get("state").and_then(Value::as_str);
        let status = row.get("status").and_then(Value::as_str);
        if row.get("productTurnId").and_then(Value::as_str).unwrap_or("").is_empty() {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation row is missing its product turn identity",
            ));
        }
        if kind == "turnHeader" && state == Some("running") {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Running turns cannot be shared",
            ));
        }
        if (kind == "assistantText" || kind == "reasoning") && state == Some("streaming") {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Streaming rows cannot be shared",
            ));
        }
        if kind == "toolCall"
            && status.map(|s| ACTIVE_TOOL_STATUSES.contains(&s)).unwrap_or(false)
        {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Active tool calls cannot be shared",
            ));
        }
        if kind == "subagent" && status == Some("running") {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Active subagents cannot be shared",
            ));
        }
        if kind == "artifact" {
            let mut safe = row.clone();
            if let Some(map) = safe.as_object_mut() {
                map.remove("ref");
            }
            first_unsafe_string(&safe)?;
            continue;
        }
        first_unsafe_string(row)?;
    }
    Ok(())
}

fn assert_unique(values: &[String], label: &str) -> Result<(), ShareError> {
    let set: BTreeSet<&String> = values.iter().collect();
    if set.len() != values.len() {
        return Err(projection_error(
            ProjectionErrorKind::InvalidConversation,
            format!("Conversation public {label} identities must be unique"),
        ));
    }
    Ok(())
}

/// First-appearance pseudonym allocation (`share-turn-1`, …).
fn allocate_ids<'a>(
    values: impl IntoIterator<Item = &'a str>,
    prefix: &str,
) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for value in values {
        if !result.contains_key(value) {
            result.insert(value.to_string(), format!("{prefix}-{}", result.len() + 1));
        }
    }
    result
}

fn required_mapped_id(
    ids: &BTreeMap<String, String>,
    source: &str,
    label: &str,
) -> Result<String, ShareError> {
    ids.get(source).cloned().ok_or_else(|| {
        projection_error(
            ProjectionErrorKind::InvalidConversation,
            format!("Conversation {label} reference is outside the shared projection"),
        )
    })
}

/// The public projection payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicProjection {
    pub rows: Vec<Value>,
    pub selected_product_turn_ids: Vec<String>,
    pub artifacts: Vec<Value>,
}

/// `buildConversationSharePublicProjection`.
pub fn build_public_projection(
    rows: &[Value],
    selected_product_turn_ids: &[String],
) -> Result<PublicProjection, ShareError> {
    assert_terminal_and_safe(rows)?;
    assert_unique(selected_product_turn_ids, "source product turn")?;
    if selected_product_turn_ids.is_empty() {
        return Err(projection_error(
            ProjectionErrorKind::InvalidConversation,
            "Conversation product turn selection cannot be empty",
        ));
    }
    let selected: BTreeSet<&String> = selected_product_turn_ids.iter().collect();
    for row in rows {
        let pt = row.get("productTurnId").and_then(Value::as_str).unwrap_or("");
        if pt.is_empty() || !selected.contains(&pt.to_string()) {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation rows fall outside the selected product turns",
            ));
        }
    }

    // Non-V1 rows are filtered before the public ID closure is generated.
    let retained: Vec<&Value> = rows
        .iter()
        .filter(|r| {
            !matches!(
                r.get("kind").and_then(Value::as_str),
                Some("subagent") | Some("hookInvocation")
            )
        })
        .collect();

    // Pseudonym maps by first appearance over the retained rows.
    let mut product_turns: Vec<&str> = selected_product_turn_ids.iter().map(String::as_str).collect();
    let mut turns: Vec<&str> = Vec::new();
    let mut entities: Vec<&str> = Vec::new();
    let mut tool_calls: Vec<&str> = Vec::new();
    let mut artifact_ids: Vec<&str> = Vec::new();
    let mut artifact_keys: Vec<&str> = Vec::new();
    for row in &retained {
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
        if let Some(t) = row.get("turnId").and_then(Value::as_str) {
            turns.push(t);
        }
        if let Some(e) = row.get("entityId").and_then(Value::as_str) {
            entities.push(e);
        }
        if kind == "toolCall"
            && let Some(id) = row.get("toolCallId").and_then(Value::as_str)
        {
            tool_calls.push(id);
        }
        if kind == "artifact" {
            if let Some(id) = row.get("artifactVersionId").and_then(Value::as_str) {
                artifact_ids.push(id);
            }
            if let Some(k) = row.get("logicalArtifactKey").and_then(Value::as_str) {
                artifact_keys.push(k);
            }
        }
    }
    product_turns.dedup();
    let product_map = allocate_ids(product_turns.clone(), "share-product-turn");    let turn_map = allocate_ids(turns, "share-turn");
    let entity_map = allocate_ids(entities, "share-entity");
    let tool_call_map = allocate_ids(tool_calls, "share-tool-call");
    let artifact_map = allocate_ids(artifact_ids, "share-artifact");
    let artifact_key_map = allocate_ids(artifact_keys, "share-artifact-key");

    // Allow-list rebuild, contiguous rowId from 1.
    let mut out_rows: Vec<Value> = Vec::with_capacity(retained.len());
    let mut artifacts_manifest: Vec<Value> = Vec::new();
    for (index, row) in retained.iter().enumerate() {
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
        let mut projected = Map::new();
        projected.insert("rowId".into(), Value::from(index as u64 + 1));
        let turn_id = row.get("turnId").and_then(Value::as_str).unwrap_or("");
        projected.insert(
            "turnId".into(),
            Value::String(required_mapped_id(&turn_map, turn_id, "turn")?),
        );
        if let Some(e) = row.get("entityId").and_then(Value::as_str) {
            projected.insert(
                "entityId".into(),
                Value::String(required_mapped_id(&entity_map, e, "entity")?),
            );
        }
        let pt = row.get("productTurnId").and_then(Value::as_str).unwrap_or("");
        let public_pt = required_mapped_id(&product_map, pt, "product turn")?;
        projected.insert("productTurnId".into(), Value::String(public_pt.clone()));
        if let Some(v) = row.get("visibility") {
            projected.insert("visibility".into(), v.clone());
        }
        projected.insert("createdAt".into(), row.get("createdAt").cloned().unwrap_or(Value::Null));
        projected.insert(
            "createdAtSeq".into(),
            row.get("createdAtSeq").cloned().unwrap_or(Value::Null),
        );
        projected.insert("kind".into(), Value::String(kind.to_string()));

        match kind {
            "toolCall" => {
                let id = row.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                projected.insert(
                    "toolCallId".into(),
                    Value::String(required_mapped_id(&tool_call_map, id, "tool call")?),
                );
                for field in ["toolName", "status", "inputText"] {
                    if let Some(v) = row.get(field) {
                        projected.insert(field.into(), v.clone());
                    }
                }
                if let Some(v) = row.get("input") {
                    projected.insert("input".into(), v.clone());
                }
                if let Some(output) = row.get("output") {
                    // Only the text survives; truncated-output refs are local.
                    let mut text_only = Map::new();
                    text_only.insert("text".into(), output.get("text").cloned().unwrap_or(Value::Null));
                    projected.insert("output".into(), Value::Object(text_only));
                }
                for field in ["error", "backgrounded", "startedAt", "endedAt"] {
                    if let Some(v) = row.get(field) {
                        projected.insert(field.into(), v.clone());
                    }
                }
            }
            "artifact" => {
                let version_id = row.get("artifactVersionId").and_then(Value::as_str).unwrap_or("");
                let logical_key = row.get("logicalArtifactKey").and_then(Value::as_str).unwrap_or("");
                let public_id = required_mapped_id(&artifact_map, version_id, "artifact")?;
                let public_key =
                    required_mapped_id(&artifact_key_map, logical_key, "artifact key")?;
                projected.insert("artifactVersionId".into(), Value::String(public_id.clone()));
                projected.insert("logicalArtifactKey".into(), Value::String(public_key));
                for field in [
                    "displayName",
                    "artifactType",
                    "mimeType",
                    "sizeBytes",
                    "sha256",
                    "state",
                ] {
                    if let Some(v) = row.get(field) {
                        projected.insert(field.into(), v.clone());
                    }
                }
                projected.insert(
                    "ref".into(),
                    Value::String(format!("{PUBLIC_ARTIFACT_REF_PREFIX}{public_id}")),
                );
                let mut descriptor = Map::new();
                descriptor.insert("artifact_id".into(), Value::String(public_id.clone()));
                descriptor.insert(
                    "logical_artifact_key".into(),
                    row.get("logicalArtifactKey").cloned().unwrap_or(Value::Null),
                );
                descriptor.insert("producer_product_turn_id".into(), Value::String(public_pt.clone()));
                descriptor.insert("artifact_version".into(), Value::from(1));
                descriptor.insert("state".into(), row.get("state").cloned().unwrap_or(Value::Null));
                descriptor.insert(
                    "ref".into(),
                    Value::String(format!("{PUBLIC_ARTIFACT_REF_PREFIX}{public_id}")),
                );
                descriptor.insert(
                    "display_name".into(),
                    row.get("displayName").cloned().unwrap_or(Value::Null),
                );
                descriptor.insert(
                    "artifact_type".into(),
                    row.get("artifactType").cloned().unwrap_or(Value::Null),
                );
                descriptor.insert(
                    "mime_type".into(),
                    row.get("mimeType").cloned().unwrap_or(Value::Null),
                );
                descriptor.insert(
                    "size_bytes".into(),
                    row.get("sizeBytes").cloned().unwrap_or(Value::Null),
                );
                descriptor.insert("sha256".into(), row.get("sha256").cloned().unwrap_or(Value::Null));
                artifacts_manifest.push(Value::Object(descriptor));
            }
            "userInput" => {
                for field in ["text", "origin", "guided"] {
                    if let Some(v) = row.get(field) {
                        projected.insert(field.into(), v.clone());
                    }
                }
                // Attachments survive only with public artifact refs
                // (donor's `isPublicArtifactRef` filter); local file refs
                // are dropped, never rewritten.
                if let Some(attachments) = row.get("attachments").and_then(Value::as_array) {
                    let public: Vec<Value> = attachments
                        .iter()
                        .filter(|a| {
                            a.get("ref")
                                .and_then(Value::as_str)
                                .map(is_public_artifact_ref)
                                .unwrap_or(false)
                        })
                        .map(|a| {
                            let mut kept = Map::new();
                            for field in ["ref", "fileName", "mime", "bytes", "previewRef"] {
                                if let Some(v) = a.get(field) {
                                    kept.insert(field.into(), v.clone());
                                }
                            }
                            Value::Object(kept)
                        })
                        .collect();
                    if !public.is_empty() {
                        projected.insert("attachments".into(), Value::Array(public));
                    }
                }
            }
            // Turn headers, assistant text, reasoning, and timeline
            // markers carry only their base allow-list plus these public
            // JSON fields, which the safety scan already validated.
            _ => {
                if let Some(map) = row.as_object() {
                    for field in [
                        "origin",
                        "executionKind",
                        "state",
                        "startedAt",
                        "endedAt",
                        "activeMs",
                        "text",
                        "model",
                        "durationMs",
                        "lane",
                        "marker",
                    ] {
                        if let Some(v) = row.get(field) {
                            projected.insert(field.into(), v.clone());
                        }
                    }
                    let _ = map;
                }
            }
        }
        out_rows.push(Value::Object(projected));
    }

    let projected_turn_ids: Vec<String> = selected_product_turn_ids
        .iter()
        .map(|pt| required_mapped_id(&product_map, pt, "product turn"))
        .collect::<Result<Vec<_>, _>>()?;

    let projection = PublicProjection {
        rows: out_rows,
        selected_product_turn_ids: projected_turn_ids,
        artifacts: artifacts_manifest,
    };
    validate_public_projection(&projection)?;
    Ok(projection)
}

/// `assertConversationSharePublicProjection`: the closure check on the
/// FINISHED payload — a validator that must stay in lockstep with the
/// builder above (donor keeps both behind one policy boundary).
fn validate_public_projection(projection: &PublicProjection) -> Result<(), ShareError> {
    if projection.rows.is_empty() || projection.selected_product_turn_ids.is_empty() {
        return Err(projection_error(
            ProjectionErrorKind::InvalidConversation,
            "Conversation public projection cannot be empty",
        ));
    }
    let selected: Vec<String> = projection.selected_product_turn_ids.clone();
    let selected: BTreeSet<&String> = selected.iter().collect();
    let mut header_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut tool_call_ids: Vec<&str> = Vec::new();
    let mut artifact_ids: Vec<&str> = Vec::new();
    let mut turn_ids: BTreeSet<&str> = BTreeSet::new();

    for (index, row) in projection.rows.iter().enumerate() {
        let row_id = row.get("rowId").and_then(Value::as_u64).unwrap_or(0);
        if row_id != index as u64 + 1 {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation public row identities must be contiguous",
            ));
        }
        let turn = row.get("turnId").and_then(Value::as_str).unwrap_or("");
        if !turn.starts_with("share-turn-") {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation public turn identity is invalid",
            ));
        }
        turn_ids.insert(turn);
        let pt = row.get("productTurnId").and_then(Value::as_str).unwrap_or("");
        if !pt.starts_with("share-product-turn-") || !selected.contains(&pt.to_string()) {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation row belongs to an unselected product turn",
            ));
        }
        if row.get("actions").is_some() {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation public rows cannot contain local actions",
            ));
        }
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind == "turnHeader" {
            *header_counts.entry(pt.to_string()).or_insert(0) += 1;
            if row.get("originMeta").is_some() {
                return Err(projection_error(
                    ProjectionErrorKind::InvalidConversation,
                    "Conversation public turn headers cannot contain local work identity",
                ));
            }
        } else if kind == "userInput" {
            for field in ["sourceCommandId", "rootSourceCommandId", "clientId"] {
                if row.get(field).is_some() {
                    return Err(projection_error(
                        ProjectionErrorKind::InvalidConversation,
                        "Conversation public input contains local write identity",
                    ));
                }
            }
        } else if kind == "assistantText" && row.get("feedback").is_some() {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation public assistant text contains local feedback",
            ));
        } else if kind == "toolCall" {
            let id = row.get("toolCallId").and_then(Value::as_str).unwrap_or("");
            if !id.starts_with("share-tool-call-") {
                return Err(projection_error(
                    ProjectionErrorKind::InvalidConversation,
                    "Conversation public tool call identity is invalid",
                ));
            }
            tool_call_ids.push(id);
            for field in ["progress", "approvalInteractionId", "workId"] {
                if row.get(field).is_some() {
                    return Err(projection_error(
                        ProjectionErrorKind::InvalidConversation,
                        "Conversation public tool call contains local runtime identity",
                    ));
                }
            }
        } else if kind == "artifact" {
            let id = row.get("artifactVersionId").and_then(Value::as_str).unwrap_or("");
            if !id.starts_with("share-artifact-")
                || row.get("ref").and_then(Value::as_str)
                    != Some(format!("{PUBLIC_ARTIFACT_REF_PREFIX}{id}").as_str())
            {
                return Err(projection_error(
                    ProjectionErrorKind::InvalidConversation,
                    "Conversation public artifact identity is invalid",
                ));
            }
            artifact_ids.push(id);
        } else if kind == "subagent" || kind == "hookInvocation" {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation public projection cannot contain subagent details",
            ));
        }
    }

    let ids_strings: Vec<String> = tool_call_ids.iter().map(|s| (*s).to_string()).collect();
    assert_unique(&ids_strings, "tool call")?;
    let ids_strings: Vec<String> = artifact_ids.iter().map(|s| (*s).to_string()).collect();
    assert_unique(&ids_strings, "artifact")?;
    if turn_ids.is_empty() {
        return Err(projection_error(
            ProjectionErrorKind::InvalidConversation,
            "Conversation public projection has no turns",
        ));
    }
    for pt in &projection.selected_product_turn_ids {
        if header_counts.get(pt) != Some(&1) {
            return Err(projection_error(
                ProjectionErrorKind::InvalidConversation,
                "Conversation public product turns require exactly one header",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::integrity::{build_integrity, canonical_json, verify_integrity};
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys_and_is_stable() {
        let value = json!({ "b": [1, {"d": 2, "c": true}], "a": null, "e": "x\"y" });
        let canonical = canonical_json(&value).unwrap();
        assert_eq!(canonical, r#"{"a":null,"b":[1,{"c":true,"d":2}],"e":"x\"y"}"#);
        // key order in the source is irrelevant
        let value2 = json!({ "a": null, "e": "x\"y", "b": [1, {"c": true, "d": 2}] });
        assert_eq!(canonical, canonical_json(&value2).unwrap());
    }

    #[test]
    fn integrity_build_and_verify_round_trip() {
        let rows = json!([{ "rowId": 1, "kind": "assistantText", "text": "hi" }]);
        let artifacts = vec![
            json!({ "artifact_id": "share-artifact-2", "sha256": "ff" }),
            json!({ "artifact_id": "share-artifact-1", "sha256": "ee" }),
        ];
        let integrity = build_integrity(&rows, &artifacts).unwrap();

        // raw round trip verifies
        let raw_artifacts = Value::Array(artifacts.clone());
        assert!(verify_integrity(&rows, &raw_artifacts, &integrity).unwrap());

        // server adds a signed URL at read time: still verifies (stripped)
        let served = Value::Array(vec![
            json!({ "artifact_id": "share-artifact-2", "sha256": "ff", "download_url": "https://x", "download_url_expires_at": 42 }),
            json!({ "artifact_id": "share-artifact-1", "sha256": "ee", "download_url": "https://y" }),
        ]);
        assert!(verify_integrity(&rows, &served, &integrity).unwrap());

        // additive publisher evolution: the publisher adds an optional row
        // field and re-issues integrity — an OLD importer verifies against
        // the NEW integrity because hashing sees the raw bytes, not a
        // schema-projected form that would strip the unknown field
        let mut evolved = rows.clone();
        evolved[0]["newField"] = json!("added by newer publisher");
        let evolved_integrity = build_integrity(&evolved, &artifacts).unwrap();
        assert!(verify_integrity(&evolved, &raw_artifacts, &evolved_integrity).unwrap());

        // genuine content changes break verification
        let mut tampered = rows.clone();
        tampered[0]["text"] = json!("evil");
        assert!(!verify_integrity(&tampered, &raw_artifacts, &integrity).unwrap());
    }

    #[test]
    fn integrity_detects_tampered_artifacts() {
        let rows = json!([]);
        let artifacts = vec![json!({ "artifact_id": "a1", "size_bytes": 10 })];
        let integrity = build_integrity(&rows, &artifacts).unwrap();
        let tampered = Value::Array(vec![json!({ "artifact_id": "a1", "size_bytes": 999 })]);
        assert!(!verify_integrity(&rows, &tampered, &integrity).unwrap());
        // unknown fields count toward the digest (raw-value hashing)
        let with_extra = Value::Array(vec![json!({ "artifact_id": "a1", "size_bytes": 10, "future": true })]);
        assert!(!verify_integrity(&rows, &with_extra, &integrity).unwrap());
    }
}
