//! Host domain: BigModel team-plan entitlement + project API keys — row-48
//! re-homed from ZCode `packages/services/src/bigmodel/` (codingPlanEntitlement.ts
//! + teamPlanApiKey.ts).
//!
//! Contracts ported verbatim (the comments carry the reasoning):
//! - **biz envelope**: `success === false` fails; a numeric `code` must be
//!   0 or 200; an omitted code is NOT a business failure. Diagnostics are
//!   captured for every call so support sees the raw shape, not a bare error.
//! - **entitlement is three-valued**: available / unavailable(reason) /
//!   unknown. A heterogeneous subscription list only strictly validates the
//!   Coding entries it adopts — unrelated entries must not shadow a valid
//!   entitlement, and SUSPECTED-Coding-but-malformed data must degrade to
//!   unknown, never to "definitely not subscribed". Unverified new enum
//!   values stay unknown for the same reason.
//! - **team plan keys**: every team project needs its own keyType=2 project
//!   key (creating one for the selected team only breaks the projection
//!   when the user switches teams) — `ensure` lists first, creates only
//!   when no usable key exists, and copies the secret separately.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// biz client seam (ZCode injects ApiClient; tests inject a fake)
// ---------------------------------------------------------------------------

pub trait BizClient {
    /// One biz API call: returns the parsed JSON envelope body.
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, String>;
}

/// Real transport (ureq, same stack as the provider sampler).
pub struct UreqBizClient {
    pub timeout_ms: u64,
}

impl BizClient for UreqBizClient {
    fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_millis(self.timeout_ms))
            .build();
        let mut req = match method.to_ascii_uppercase().as_str() {
            "POST" => agent.post(url),
            _ => agent.get(url),
        };
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let response = match body {
            Some(b) => req.send_json(b.clone()),
            None => req.call(),
        };
        match response {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| format!("decode biz response: {e}")),
            Err(ureq::Error::Status(code, r)) => {
                // the biz plane reports failures inside 200 envelopes too;
                // surface the body when there is one
                let text = r.into_string().unwrap_or_default();
                Err(format!("biz http {code}: {text}"))
            }
            Err(ureq::Error::Transport(t)) => Err(format!("biz transport: {t}")),
        }
    }
}

// ---------------------------------------------------------------------------
// envelope classification (teamPlanApiKey.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamPlanBizContext {
    pub organization_id: String,
    pub project_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BizEnvelopeDiagnostics {
    pub code: Option<i64>,
    pub msg: Option<String>,
    pub success: Option<bool>,
}

pub fn create_biz_headers(
    authorization: &str,
    team_context: Option<&TeamPlanBizContext>,
) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Authorization".to_string(), authorization.to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    if let Some(ctx) = team_context {
        headers.push(("bigmodel-organization".to_string(), ctx.organization_id.clone()));
        headers.push(("bigmodel-project".to_string(), ctx.project_id.clone()));
    }
    headers
}

pub fn is_successful_biz_envelope(envelope: &Value) -> bool {
    if envelope.get("success") == Some(&json!(false)) {
        return false;
    }
    match envelope.get("code").and_then(Value::as_i64) {
        Some(code) => code == 0 || code == 200,
        // an omitted success code must not be read as a business failure
        None => envelope.get("success") == Some(&json!(true)) || envelope.get("data").is_some(),
    }
}

pub fn biz_envelope_diagnostics(envelope: &Value) -> BizEnvelopeDiagnostics {
    BizEnvelopeDiagnostics {
        code: envelope.get("code").and_then(Value::as_i64),
        msg: envelope
            .get("msg")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        success: envelope.get("success").and_then(Value::as_bool),
    }
}

fn successful_data(envelope: &Value) -> Option<&Value> {
    if is_successful_biz_envelope(envelope) {
        envelope.get("data")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// coding-plan entitlement (codingPlanEntitlement.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    Expired,
    Unassigned,
}

/// Three-valued on purpose: unknown ≠ unavailable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodingPlanEntitlement {
    Available,
    Unavailable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<UnavailableReason>,
    },
    Unknown,
}

fn is_coding_plan_product(value: &Value) -> bool {
    ["productId", "productName"].iter().any(|field| {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(|s| s.to_ascii_lowercase().contains("coding"))
            .unwrap_or(false)
    })
}

/// personal schema (zod): requires `status: string` + `inCurrentPeriod: bool`.
fn personal_subscription_valid(item: &Value) -> bool {
    let Some(map) = item.as_object() else { return false };
    map.get("status").and_then(Value::as_str).is_some()
        && map.get("inCurrentPeriod").and_then(Value::as_bool).is_some()
}

pub fn is_active_personal_coding_plan(subscription: &Value) -> bool {
    is_coding_plan_product(subscription)
        && subscription.get("status").and_then(Value::as_str) == Some("VALID")
        && subscription.get("inCurrentPeriod") == Some(&json!(true))
}

/// The personal entitlement endpoint returns a LIST of heterogeneous
/// products; only strictly-valid ACTIVE Coding entries count.
pub fn classify_personal_entitlement(payload: &Value) -> CodingPlanEntitlement {
    let Some(list) = successful_data(payload).and_then(Value::as_array) else {
        return CodingPlanEntitlement::Unknown;
    };
    let mut malformed_coding_entry = false;
    for item in list {
        if personal_subscription_valid(item) && is_active_personal_coding_plan(item) {
            return CodingPlanEntitlement::Available;
        }
        if !personal_subscription_valid(item) && is_coding_plan_product(item) {
            malformed_coding_entry = true;
        }
    }
    // suspected-Coding-but-corrupt data must not read as "definitely none"
    if malformed_coding_entry {
        CodingPlanEntitlement::Unknown
    } else {
        CodingPlanEntitlement::Unavailable { reason: None }
    }
}

pub fn fetch_personal_coding_plan_entitlement(
    client: &dyn BizClient,
    authorization: &str,
    url: &str,
) -> Result<CodingPlanEntitlement, String> {
    if authorization.trim().is_empty() {
        return Ok(CodingPlanEntitlement::Unknown);
    }
    let payload = client.request(
        "GET",
        url,
        &[("Authorization".to_string(), authorization.to_string())],
        None,
    )?;
    Ok(classify_personal_entitlement(&payload))
}

/// team schema (zod): `hasSubscription: boolean` required, everything else
/// nullish-optional.
fn team_subscription_valid(item: &Value) -> bool {
    item.get("hasSubscription").and_then(Value::as_bool).is_some()
}

pub fn classify_team_entitlement(payload: &Value) -> CodingPlanEntitlement {
    let Some(data) = successful_data(payload) else {
        return CodingPlanEntitlement::Unknown;
    };
    if !team_subscription_valid(data) {
        return CodingPlanEntitlement::Unknown;
    }
    if data.get("hasSubscription") != Some(&json!(true)) {
        return CodingPlanEntitlement::Unavailable { reason: None };
    }
    let status = data.get("status").and_then(Value::as_str);
    let grant = data.get("memberGrantStatus").and_then(Value::as_str);
    // a subscription RECORD is not a currently-valid entitlement: the real
    // API returns hasSubscription=true for expired plans
    if status == Some("EXPIRED") {
        return CodingPlanEntitlement::Unavailable {
            reason: Some(UnavailableReason::Expired),
        };
    }
    if status == Some("EFFECTIVE") && grant == Some("UNASSIGNED") {
        return CodingPlanEntitlement::Unavailable {
            reason: Some(UnavailableReason::Unassigned),
        };
    }
    // unverified new enums stay unknown, never "no entitlement"
    if status != Some("EFFECTIVE") || grant != Some("VALID") {
        return CodingPlanEntitlement::Unknown;
    }
    CodingPlanEntitlement::Available
}

pub fn fetch_team_coding_plan_entitlement(
    client: &dyn BizClient,
    authorization: &str,
    host: &str,
    team_context: &TeamPlanBizContext,
) -> Result<CodingPlanEntitlement, String> {
    if authorization.trim().is_empty() {
        return Ok(CodingPlanEntitlement::Unknown);
    }
    let url = format!(
        "{}/api/biz/team/subscribe/product/querySubscribeDetail",
        host.trim_end_matches('/')
    );
    let payload = client.request(
        "GET",
        &url,
        &create_biz_headers(authorization, Some(team_context)),
        None,
    )?;
    Ok(classify_team_entitlement(&payload))
}

// ---------------------------------------------------------------------------
// team-plan project API key (teamPlanApiKey.ts)
// ---------------------------------------------------------------------------

pub const TEAM_PLAN_API_KEY_NAME: &str = "zcode-team-api-key";
pub const TEAM_PLAN_API_KEY_TYPE: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyEnsureStatus {
    Existing,
    Created,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyEnsureResult {
    pub api_key: Option<Value>,
    pub status: ApiKeyEnsureStatus,
    pub list_diagnostics: BizEnvelopeDiagnostics,
    pub list_api_key_count: usize,
    pub list_usable_api_key_count: usize,
    pub create_diagnostics: Option<BizEnvelopeDiagnostics>,
}

pub fn build_team_plan_api_keys_url(host: &str, ctx: &TeamPlanBizContext) -> String {
    format!(
        "{}/api/biz/v1/organization/{}/projects/{}/api_keys",
        host.trim_end_matches('/'),
        encodeURIComponent(&ctx.organization_id),
        encodeURIComponent(&ctx.project_id)
    )
}

/// Minimal percent-encoding for path segments (RFC 3986 unreserved + safe
/// passthrough); the TS service uses encodeURIComponent.
#[allow(non_snake_case)] // the TS name this ports (encodeURIComponent)
pub fn encodeURIComponent(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn is_usable_team_plan_api_key(item: &Value) -> bool {
    item.get("name").and_then(Value::as_str) == Some(TEAM_PLAN_API_KEY_NAME)
        && item.get("keyType").and_then(Value::as_i64) == Some(TEAM_PLAN_API_KEY_TYPE)
        && item
            .get("apiKey")
            .and_then(Value::as_str)
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
}

/// List → use an existing usable key; otherwise create one (keyType=2,
/// reserved name). Every stage records envelope diagnostics.
pub fn ensure_team_plan_project_api_key(
    client: &dyn BizClient,
    authorization: &str,
    host: &str,
    team_context: &TeamPlanBizContext,
) -> Result<ApiKeyEnsureResult, String> {
    let url = build_team_plan_api_keys_url(host, team_context);
    let list_payload = client.request(
        "GET",
        &url,
        &create_biz_headers(authorization, Some(team_context)),
        None,
    )?;
    let keys = successful_data(&list_payload)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let usable_count = keys.iter().filter(|k| is_usable_team_plan_api_key(k)).count();
    let list_diagnostics = biz_envelope_diagnostics(&list_payload);
    if let Some(existing) = keys.iter().find(|k| is_usable_team_plan_api_key(k)) {
        return Ok(ApiKeyEnsureResult {
            api_key: Some(existing.clone()),
            status: ApiKeyEnsureStatus::Existing,
            list_diagnostics,
            list_api_key_count: keys.len(),
            list_usable_api_key_count: usable_count,
            create_diagnostics: None,
        });
    }

    let create_payload = client.request(
        "POST",
        &url,
        &create_biz_headers(authorization, Some(team_context)),
        Some(&json!({
            "name": TEAM_PLAN_API_KEY_NAME,
            "keyType": TEAM_PLAN_API_KEY_TYPE
        })),
    )?;
    let created = successful_data(&create_payload)
        .filter(|d| is_usable_team_plan_api_key(d))
        .cloned();
    let status = if created.is_some() {
        ApiKeyEnsureStatus::Created
    } else {
        ApiKeyEnsureStatus::Missing
    };
    Ok(ApiKeyEnsureResult {
        api_key: created,
        status,
        list_diagnostics,
        list_api_key_count: keys.len(),
        list_usable_api_key_count: usable_count,
        create_diagnostics: Some(biz_envelope_diagnostics(&create_payload)),
    })
}

/// The project key's SECRET lives behind a separate copy call (the list
/// only ever carries the masked key).
pub fn copy_team_plan_project_api_key_secret(
    client: &dyn BizClient,
    authorization: &str,
    host: &str,
    team_context: &TeamPlanBizContext,
    api_key: &str,
) -> Result<Option<String>, String> {
    let url = format!(
        "{}/copy/{}",
        build_team_plan_api_keys_url(host, team_context),
        encodeURIComponent(api_key)
    );
    let payload = client.request(
        "GET",
        &url,
        &create_biz_headers(authorization, Some(team_context)),
        None,
    )?;
    let secret = successful_data(&payload)
        .and_then(|d| d.get("secretKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(code: Value, success: Value, data: Value) -> Value {
        json!({ "code": code, "success": success, "data": data })
    }

    // replies in sequence; records (method, body) for later inspection
    struct SeqFake {
        replies: Vec<Value>,
        calls: std::sync::Mutex<Vec<(String, Option<Value>)>>,
    }
    impl BizClient for SeqFake {
        fn request(&self, method: &str, _url: &str, _h: &[(String, String)], body: Option<&Value>) -> Result<Value, String> {
            self.calls.lock().unwrap().push((method.to_string(), body.cloned()));
            let idx = self.calls.lock().unwrap().len() - 1;
            Ok(self.replies[idx].clone())
        }
    }
    fn calls(f: &SeqFake) -> Vec<(String, Option<Value>)> {
        f.calls.lock().unwrap().clone()
    }

    #[test]
    fn envelope_classification_matches_the_ts_contract() {
        assert!(is_successful_biz_envelope(&json!({ "code": 0, "data": {} })));
        assert!(is_successful_biz_envelope(&json!({ "code": 200 })));
        assert!(is_successful_biz_envelope(&json!({ "success": true })));
        assert!(is_successful_biz_envelope(&json!({ "data": [1] })));
        assert!(!is_successful_biz_envelope(&json!({ "code": 500 })));
        assert!(!is_successful_biz_envelope(&json!({ "success": false, "code": 0 })), "explicit failure wins");
        assert!(!is_successful_biz_envelope(&json!({})));

        let d = biz_envelope_diagnostics(&json!({ "code": 401, "msg": " bad auth ", "success": false }));
        assert_eq!(d.code, Some(401));
        assert_eq!(d.msg.as_deref(), Some("bad auth"));
        assert_eq!(d.success, Some(false));
    }

    #[test]
    fn personal_entitlement_ignores_foreign_products_and_distrusts_corrupt_coding_entries() {
        let active = json!({ "productId": "g-coding", "status": "VALID", "inCurrentPeriod": true });
        // a valid ACTIVE coding entry anywhere in a heterogeneous list wins
        let mixed = envelope(
            json!(0),
            json!(null),
            json!([active, { "productId": "chat-pro", "status": "VALID", "inCurrentPeriod": false }]),
        );
        assert_eq!(classify_personal_entitlement(&mixed), CodingPlanEntitlement::Available);

        // corrupt Coding-shaped data → unknown, never "unavailable"
        let corrupt = envelope(json!(0), json!(null), json!([{ "productId": "coding-x" }]));
        assert_eq!(classify_personal_entitlement(&corrupt), CodingPlanEntitlement::Unknown);

        // no coding-shaped entries at all → plainly unavailable
        let none = envelope(json!(0), json!(null), json!([{ "productId": "chat-pro" }]));
        assert_eq!(
            classify_personal_entitlement(&none),
            CodingPlanEntitlement::Unavailable { reason: None }
        );

        // a VALID but out-of-period coding entry is not an active entitlement
        let expired = envelope(
            json!(0),
            json!(null),
            json!([{ "productId": "coding", "status": "VALID", "inCurrentPeriod": false }]),
        );
        assert_eq!(classify_personal_entitlement(&expired), CodingPlanEntitlement::Unavailable { reason: None });
    }

    #[test]
    fn team_entitlement_follows_the_status_ladder() {
        let ok = envelope(
            json!(0),
            json!(null),
            json!({ "hasSubscription": true, "status": "EFFECTIVE", "memberGrantStatus": "VALID" }),
        );
        assert_eq!(classify_team_entitlement(&ok), CodingPlanEntitlement::Available);

        let expired = envelope(
            json!(0),
            json!(null),
            json!({ "hasSubscription": true, "status": "EXPIRED" }),
        );
        assert_eq!(
            classify_team_entitlement(&expired),
            CodingPlanEntitlement::Unavailable { reason: Some(UnavailableReason::Expired) }
        );

        let unassigned = envelope(
            json!(0),
            json!(null),
            json!({ "hasSubscription": true, "status": "EFFECTIVE", "memberGrantStatus": "UNASSIGNED" }),
        );
        assert_eq!(
            classify_team_entitlement(&unassigned),
            CodingPlanEntitlement::Unavailable { reason: Some(UnavailableReason::Unassigned) }
        );

        // unverified enum values stay unknown
        let novel = envelope(
            json!(0),
            json!(null),
            json!({ "hasSubscription": true, "status": "SOMETHING_NEW" }),
        );
        assert_eq!(classify_team_entitlement(&novel), CodingPlanEntitlement::Unknown);

        let no_sub = envelope(json!(0), json!(null), json!({ "hasSubscription": false }));
        assert_eq!(
            classify_team_entitlement(&no_sub),
            CodingPlanEntitlement::Unavailable { reason: None }
        );

        // malformed data → unknown
        let junk = envelope(json!(0), json!(null), json!("not an object"));
        assert_eq!(classify_team_entitlement(&junk), CodingPlanEntitlement::Unknown);
    }

    #[test]
    fn ensure_uses_existing_key_before_creating_one() {
        let usable = json!({ "apiKey": "k-123", "keyType": 2, "name": TEAM_PLAN_API_KEY_NAME });
        let ctx = TeamPlanBizContext {
            organization_id: "org 1".into(),
            project_id: "p/1".into(),
        };
        let url = build_team_plan_api_keys_url("https://api.example.com/", &ctx);
        assert_eq!(
            url,
            "https://api.example.com/api/biz/v1/organization/org%201/projects/p%2F1/api_keys"
        );

        let list_only = SeqFake {
            replies: vec![envelope(json!(0), json!(null), json!([usable.clone()]))],
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let existing = ensure_team_plan_project_api_key(
            &list_only,
            "Bearer t",
            "https://api.example.com",
            &ctx,
        )
        .unwrap();
        assert_eq!(existing.status, ApiKeyEnsureStatus::Existing);
        assert_eq!(existing.list_usable_api_key_count, 1);
        assert!(existing.create_diagnostics.is_none());
        assert_eq!(existing.api_key, Some(usable));
        // an existing key short-circuits: no create call
        assert_eq!(calls(&list_only).len(), 1);
        assert_eq!(calls(&list_only)[0].0, "GET");

        // unusable entries do not count (wrong type / empty key / wrong name)
        let none = ensure_team_plan_project_api_key(
            &SeqFake {
                replies: vec![
                    envelope(
                json!(0),
                json!(null),
                json!([
                    { "apiKey": "k", "keyType": 1, "name": TEAM_PLAN_API_KEY_NAME },
                    { "apiKey": "  ", "keyType": 2, "name": TEAM_PLAN_API_KEY_NAME },
                        { "apiKey": "k", "keyType": 2, "name": "other" }
                    ])),
                    // the follow-up create also reports unusable → Missing
                    envelope(json!(0), json!(null), json!({ "apiKey": "", "keyType": 2, "name": TEAM_PLAN_API_KEY_NAME })),
                ],
                calls: std::sync::Mutex::new(Vec::new()),
            },
            "Bearer t",
            "https://api.example.com",
            &ctx,
        )
        .unwrap();
        assert_eq!(none.status, ApiKeyEnsureStatus::Missing);
        assert_eq!(none.list_api_key_count, 3);
        assert_eq!(none.list_usable_api_key_count, 0);
    }

    #[test]
    fn ensure_creates_only_when_no_usable_key_and_copies_secret_separately() {
        let ctx = TeamPlanBizContext { organization_id: "org".into(), project_id: "proj".into() };
        let client = SeqFake {
            replies: vec![
                envelope(json!(0), json!(null), json!([])),
                envelope(json!(0), json!(null), json!({ "apiKey": "masked", "keyType": 2, "name": TEAM_PLAN_API_KEY_NAME })),
                envelope(json!(0), json!(null), json!({ "secretKey": " secret-9 " })),
            ],
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let ensured = ensure_team_plan_project_api_key(&client, "Bearer t", "https://h", &ctx).unwrap();
        assert_eq!(ensured.status, ApiKeyEnsureStatus::Created);
        assert_eq!(ensured.create_diagnostics.unwrap().code, Some(0));
        // the create call carries exactly the reserved payload
        let all = calls(&client);
        assert_eq!(all[1].0, "POST");
        assert_eq!(
            all[1].1.as_ref().unwrap(),
            &json!({ "name": TEAM_PLAN_API_KEY_NAME, "keyType": TEAM_PLAN_API_KEY_TYPE })
        );
        let secret = copy_team_plan_project_api_key_secret(&client, "Bearer t", "https://h", &ctx, "masked").unwrap();
        assert_eq!(secret.as_deref(), Some("secret-9"));
    }
}
