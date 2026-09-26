//! Conversation-share domain end-to-end (MASTER-PLAN §3 #48, from ZCode
//! conversation-share): local conversation rows → public projection
//! (pseudonymized, filtered, allow-listed) → integrity hashes → verify.
//! This is the whole share story short of transport.

use okra_host::conversation_share::{
    build_integrity, build_public_projection, verify_integrity, ProjectionErrorKind, ShareError,
};
use serde_json::{json, Value};

fn local_conversation() -> Vec<serde_json::Value> {
    vec![
        json!({
            "kind": "turnHeader", "rowId": 1,
            "turnId": "turn-8f2", "productTurnId": "pt-1",
            "origin": "user", "state": "complete",
            "createdAt": 1000.0, "createdAtSeq": 1
        }),
        json!({
            "kind": "userInput", "rowId": 2,
            "turnId": "turn-8f2", "productTurnId": "pt-1",
            "text": "build the thing",
            "origin": "user",
            // local write identity — must never survive projection
            "clientId": "desktop-123", "sourceCommandId": "cmd-9",
            "createdAt": 1001.0, "createdAtSeq": 2
        }),
        json!({
            "kind": "toolCall", "rowId": 3,
            "turnId": "turn-8f2", "productTurnId": "pt-1",
            "toolCallId": "call-local-77", "toolName": "write_file",
            "status": "completed", "inputText": "{\"path\":\"x\"}",
            "output": { "text": "wrote x", "truncated": false },
            // local runtime identity — must never survive projection
            "progress": { "percent": 50 }, "workId": "w-3",
            "createdAt": 1002.0, "createdAtSeq": 3
        }),
        // subagent + hook rows are filtered before the public closure
        json!({
            "kind": "subagent", "rowId": 4,
            "turnId": "turn-8f2", "productTurnId": "pt-1",
            "status": "completed",
            "createdAt": 1003.0, "createdAtSeq": 4
        }),
        json!({
            "kind": "hookInvocation", "rowId": 5,
            "turnId": "turn-8f2", "productTurnId": "pt-1",
            "createdAt": 1004.0, "createdAtSeq": 5
        }),
        json!({
            "kind": "artifact", "rowId": 6,
            "turnId": "turn-8f2", "productTurnId": "pt-1",
            "artifactVersionId": "ver-local-42", "logicalArtifactKey": "src/main.rs",
            "displayName": "main.rs", "artifactType": "file",
            "mimeType": "text/x-rust", "sizeBytes": 120, "sha256": "abc",
            "ref": "file:///workspace/.okra/artifacts/ver-local-42",
            "state": "complete",
            "createdAt": 1005.0, "createdAtSeq": 6
        }),
        json!({
            "kind": "assistantText", "rowId": 7,
            "turnId": "turn-8f2", "productTurnId": "pt-1",
            "text": "done — see main.rs", "state": "complete",
            "createdAt": 1006.0, "createdAtSeq": 7
        }),
    ]
}

#[test]
fn projects_local_rows_into_closed_public_payload() {
    let rows = local_conversation();
    let projection = build_public_projection(&rows, &["pt-1".to_string()]).unwrap();

    assert_eq!(projection.selected_product_turn_ids, vec!["share-product-turn-1"]);
    assert_eq!(projection.rows.len(), 5, "subagent + hook filtered");
    let kinds: Vec<&str> = projection
        .rows
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["turnHeader", "userInput", "toolCall", "artifact", "assistantText"]
    );

    // contiguous public row ids
    for (i, row) in projection.rows.iter().enumerate() {
        assert_eq!(row["rowId"], json!(i + 1));
        assert_eq!(row["turnId"], json!("share-turn-1"));
        assert_eq!(row["productTurnId"], json!("share-product-turn-1"));
    }

    // pseudonymized local identities
    assert_eq!(projection.rows[2]["toolCallId"], json!("share-tool-call-1"));
    assert_eq!(projection.rows[3]["artifactVersionId"], json!("share-artifact-1"));
    // artifact ref rewritten to the public scheme
    assert_eq!(
        projection.rows[3]["ref"],
        json!("okra-artifact://share/share-artifact-1")
    );

    // local-only fields stripped
    let input = &projection.rows[1];
    assert!(input.get("clientId").is_none());
    assert!(input.get("sourceCommandId").is_none());
    let tool_call = &projection.rows[2];
    assert!(tool_call.get("progress").is_none());
    assert!(tool_call.get("workId").is_none());
    assert_eq!(tool_call["output"], json!({ "text": "wrote x" }), "output text only");

    // artifact descriptor manifest with public identity
    assert_eq!(projection.artifacts.len(), 1);
    let descriptor = &projection.artifacts[0];
    assert_eq!(descriptor["artifact_id"], json!("share-artifact-1"));
    assert_eq!(descriptor["producer_product_turn_id"], json!("share-product-turn-1"));
    assert_eq!(descriptor["extension_missing"], json!(null), "no invented fields");
}

#[test]
fn integrity_closes_the_share_round_trip() {
    let rows = local_conversation();
    let projection = build_public_projection(&rows, &["pt-1".to_string()]).unwrap();
    let rows_value = json!({
        "rows": projection.rows,
        "selectedProductTurnIds": projection.selected_product_turn_ids,
    });
    let integrity = build_integrity(&rows_value, &projection.artifacts).unwrap();

    // the exported payload verifies
    assert!(verify_integrity(&rows_value, &Value::Array(projection.artifacts.clone()), &integrity).unwrap());

    // a served copy with signed URLs appended still verifies
    let mut served = projection.artifacts.clone();
    served[0]["download_url"] = json!("https://cdn.example.com/a1");
    served[0]["download_url_expires_at"] = json!(4_000_000_000u64);
    assert!(verify_integrity(&rows_value, &Value::Array(served), &integrity).unwrap());

    // tampered text fails
    let mut tampered = rows_value.clone();
    tampered["rows"][4]["text"] = json!("malicious replacement");
    assert!(!verify_integrity(&tampered, &Value::Array(projection.artifacts.clone()), &integrity).unwrap());
}

#[test]
fn rejects_unsafe_or_unsettled_conversations() {
    let base = json!({
        "kind": "assistantText", "turnId": "t1", "productTurnId": "pt-1",
        "text": "ok", "state": "complete", "createdAt": 1.0, "createdAtSeq": 1
    });
    let err = |e: ShareError, want: ProjectionErrorKind| {
        assert!(matches!(e, ShareError::Projection { kind, .. } if kind == want), "{e:?}");
    };

    // running turn
    let mut rows = base.clone();
    rows["kind"] = json!("turnHeader");
    rows["state"] = json!("running");
    err(
        build_public_projection(&[rows], &["pt-1".into()]).unwrap_err(),
        ProjectionErrorKind::InvalidConversation,
    );

    // streaming assistant text
    let mut rows = base.clone();
    rows["state"] = json!("streaming");
    err(
        build_public_projection(&[rows], &["pt-1".into()]).unwrap_err(),
        ProjectionErrorKind::InvalidConversation,
    );

    // active tool call
    let mut rows = base.clone();
    rows["kind"] = json!("toolCall");
    rows["status"] = json!("running");
    err(
        build_public_projection(&[rows], &["pt-1".into()]).unwrap_err(),
        ProjectionErrorKind::InvalidConversation,
    );

    // a data: URL as a string value (donor check is prefix-anchored per
    // string: a value that IS a data:/file: URL is rejected)
    let mut rows = base.clone();
    rows["text"] = json!("data:text/html,<script>alert(1)</script>");
    err(
        build_public_projection(&[rows], &["pt-1".into()]).unwrap_err(),
        ProjectionErrorKind::UnsafeStructure,
    );

    // file: URL in tool output text
    let mut rows = base.clone();
    rows["kind"] = json!("toolCall");
    rows["status"] = json!("completed");
    rows["output"] = json!({ "text": "file:///etc/passwd" });
    err(
        build_public_projection(&[rows], &["pt-1".into()]).unwrap_err(),
        ProjectionErrorKind::UnsafeStructure,
    );

    // non-public artifact ref as a string value (internal scheme leak)
    let mut rows = base.clone();
    rows["text"] = json!("okra-artifact://internal/secret");
    err(
        build_public_projection(&[rows], &["pt-1".into()]).unwrap_err(),
        ProjectionErrorKind::ArtifactProtocolNotReady,
    );

    // empty selection
    err(
        build_public_projection(&[base.clone()], &[]).unwrap_err(),
        ProjectionErrorKind::InvalidConversation,
    );

    // rows outside the selected product turns
    err(
        build_public_projection(&[base], &["pt-other".into()]).unwrap_err(),
        ProjectionErrorKind::InvalidConversation,
    );
}

#[test]
fn validator_enforces_closure_on_finished_payload() {
    // two headers for one product turn: projection input dedupes nothing —
    // the validator must reject the built payload
    let rows = vec![
        json!({
            "kind": "turnHeader", "turnId": "t1", "productTurnId": "pt-1",
            "state": "complete", "createdAt": 1.0, "createdAtSeq": 1
        }),
        json!({
            "kind": "turnHeader", "turnId": "t2", "productTurnId": "pt-1",
            "state": "complete", "createdAt": 2.0, "createdAtSeq": 2
        }),
    ];
    let err = build_public_projection(&rows, &["pt-1".to_string()]).unwrap_err();
    assert!(matches!(
        err,
        ShareError::Projection { kind: ProjectionErrorKind::InvalidConversation, .. }
    ), "{err}");
    assert!(err.to_string().contains("exactly one header"), "{err}");
}
