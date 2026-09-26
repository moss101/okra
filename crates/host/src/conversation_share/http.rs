//! Share HTTP client (MASTER-PLAN §3 #48, from ZCode
//! `conversationShareHttpClient.ts`): one endpoint per method behind one
//! `request_data` funnel with unified auth / error normalization.
//!
//! Donor contracts kept:
//! - **error taxonomy**: 17 client error kinds; known numeric API codes
//!   (3001-3215) map onto them, unknown codes degrade to `unknown` but
//!   keep the server's message and status;
//! - **response classification**: 401 with an empty body is
//!   `authentication_required`; non-JSON 5xx bodies are `network`
//!   (infrastructure failure, not a contract violation); non-JSON other
//!   statuses are `invalid_contract`; empty-body fallback: 5xx -> network,
//!   429 -> rate_limited, else unknown;
//! - **timeouts**: 30 s default; confirm gets 120 s (server-side safety
//!   checks hang single requests well past 30 s); uploads scale with size
//!   (30 s base + 1 s per 128 KiB, floored at the default);
//! - **Retry-After** parsed only for code 3215 (safety_check_pending):
//!   delta-seconds or a strict HTTP-date (IMF-fixdate / RFC 850 /
//!   asctime); loose formats are rejected - a lenient parser would
//!   normalize impossible dates into bogus delays;
//! - **auth**: bearer token; `required` endpoints refuse locally (before
//!   any network) when no token; preview/continuation are `optional` so
//!   public shares work logged-out;
//! - **integrity on raw values** in continuation, before any decoding;
//! - request ids: generated per request, echoed back validated as
//!   `[A-Za-z0-9._:-]{1,128}` for log correlation.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::integrity::verify_integrity;
use super::ShareIntegrity;

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const CONFIRM_TIMEOUT_MS: u64 = 120_000;
const UPLOAD_TIMEOUT_BASE_MS: u64 = 30_000;
const UPLOAD_MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 128 * 1024;

const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientErrorKind {
    AuthenticationRequired,
    FeatureDisabled,
    InvalidContract,
    InvalidConversation,
    DisclosureRequired,
    UnsafeStructure,
    ArtifactNotAllowed,
    LimitExceeded,
    UploadIncomplete,
    NotFound,
    Expired,
    ImportNotAllowed,
    RateLimited,
    Network,
    SafetyCheckPending,
    UnsupportedSchemaVersion,
    Unknown,
}

impl ClientErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClientErrorKind::AuthenticationRequired => "authentication_required",
            ClientErrorKind::FeatureDisabled => "feature_disabled",
            ClientErrorKind::InvalidContract => "invalid_contract",
            ClientErrorKind::InvalidConversation => "invalid_conversation",
            ClientErrorKind::DisclosureRequired => "disclosure_required",
            ClientErrorKind::UnsafeStructure => "unsafe_structure",
            ClientErrorKind::ArtifactNotAllowed => "artifact_not_allowed",
            ClientErrorKind::LimitExceeded => "limit_exceeded",
            ClientErrorKind::UploadIncomplete => "upload_incomplete",
            ClientErrorKind::NotFound => "not_found",
            ClientErrorKind::Expired => "expired",
            ClientErrorKind::ImportNotAllowed => "import_not_allowed",
            ClientErrorKind::RateLimited => "rate_limited",
            ClientErrorKind::Network => "network",
            ClientErrorKind::SafetyCheckPending => "safety_check_pending",
            ClientErrorKind::UnsupportedSchemaVersion => "unsupported_schema_version",
            ClientErrorKind::Unknown => "unknown",
        }
    }
}

/// Donor `ERROR_KIND_BY_CODE`.
fn kind_for_code(code: u32) -> Option<ClientErrorKind> {
    Some(match code {
        3001 => ClientErrorKind::InvalidContract,
        3002 => ClientErrorKind::RateLimited,
        3200 => ClientErrorKind::FeatureDisabled,
        3201 => ClientErrorKind::AuthenticationRequired,
        3203 | 3204 => ClientErrorKind::InvalidContract,
        3205 => ClientErrorKind::InvalidConversation,
        3206 => ClientErrorKind::DisclosureRequired,
        3207 => ClientErrorKind::UnsafeStructure,
        3208 => ClientErrorKind::ArtifactNotAllowed,
        3209 => ClientErrorKind::LimitExceeded,
        3210 => ClientErrorKind::UploadIncomplete,
        3211 => ClientErrorKind::NotFound,
        3212 => ClientErrorKind::Expired,
        3213 => ClientErrorKind::AuthenticationRequired,
        3214 => ClientErrorKind::ImportNotAllowed,
        3215 => ClientErrorKind::SafetyCheckPending,
        _ => return None,
    })
}

/// The normalized client error (donor `ConversationShareClientError`).
#[derive(Debug, Clone, PartialEq)]
pub struct ShareClientError {
    pub kind: ClientErrorKind,
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<u32>,
    pub retry_after_ms: Option<u64>,
    pub request_id: Option<String>,
}

impl std::fmt::Display for ShareClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)?;
        if let Some(status) = self.status {
            write!(f, " (HTTP {status})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ShareClientError {}

fn client_error(kind: ClientErrorKind, message: impl Into<String>) -> ShareClientError {
    ShareClientError {
        kind,
        message: message.into(),
        status: None,
        code: None,
        retry_after_ms: None,
        request_id: None,
    }
}

// ---------------------------------------------------------------------------
// Retry-After (delta-seconds + strict HTTP dates)
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];
const WEEKDAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Days-from-civil (Howard Hinnant's algorithm) -> epoch seconds.
fn epoch_seconds(year: i64, month: u32, day: u32, h: u32, m: u32, s: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + i64::from(h) * 3_600 + i64::from(m) * 60 + i64::from(s)
}

fn parse_time(time: &str) -> Option<(u32, u32, u32)> {
    let mut parts = time.split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let s: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || h > 23 || m > 59 || s > 60 {
        return None;
    }
    Some((h, m, s))
}

fn two_digit_year(yy: i64) -> Option<i64> {
    if (0..50).contains(&yy) {
        Some(2000 + yy)
    } else if (50..100).contains(&yy) {
        Some(1900 + yy)
    } else {
        None
    }
}

/// Strict `parseHttpDate`: IMF-fixdate, RFC 850, asctime.
fn parse_http_date(value: &str) -> Option<i64> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    match tokens.first().copied()? {
        wd if wd.ends_with(',') => {
            // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT
            // RFC 850:     Sunday, 06-Nov-94 08:49:37 GMT
            let weekday = wd.trim_end_matches(',');
            if !WEEKDAYS.contains(&weekday) && !WEEKDAYS_SHORT.contains(&weekday) {
                return None;
            }
            // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT
            if tokens.len() == 6 && tokens[5] == "GMT" {
                if tokens[1].len() != 2 || tokens[3].len() != 4 {
                    return None;
                }
                let month_idx = MONTHS.iter().position(|m| *m == tokens[2])?;
                let (h, m, s) = parse_time(tokens[4])?;
                return Some(epoch_seconds(
                    tokens[3].parse().ok()?,
                    month_idx as u32 + 1,
                    tokens[1].parse().ok()?,
                    h,
                    m,
                    s,
                ));
            }
            // RFC 850: Sunday, 06-Nov-94 08:49:37 GMT
            if tokens.len() == 4 && tokens[3] == "GMT" {
                let (dd, mon_yy) = tokens[1].split_once('-')?;
                let (mon, yy) = mon_yy.rsplit_once('-')?;
                if yy.len() != 2 {
                    return None;
                }
                let month_idx = MONTHS.iter().position(|m| *m == mon)?;
                let (h, m, s) = parse_time(tokens[2])?;
                return Some(epoch_seconds(
                    two_digit_year(yy.parse().ok()?)?,
                    month_idx as u32 + 1,
                    dd.parse().ok()?,
                    h,
                    m,
                    s,
                ));
            }
            None
        }
        _ => {
            // asctime: Sun Nov  6 08:49:37 1994
            if tokens.len() != 5 {
                return None;
            }
            let month_idx = MONTHS.iter().position(|m| *m == tokens[1])?;
            let day: u32 = tokens[2].parse().ok()?;
            let (h, m, s) = parse_time(tokens[3])?;
            let year: i64 = tokens[4].parse().ok()?;
            Some(epoch_seconds(year, month_idx as u32 + 1, day, h, m, s))
        }
    }
}

/// `parseRetryAfterMs`: integer seconds or a strict HTTP-date; loose
/// formats are rejected, non-positive delays yield None.
pub fn parse_retry_after_ms(value: &str, now_epoch_ms: i64) -> Option<u64> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return None;
    }
    if normalized.bytes().all(|b| b.is_ascii_digit()) {
        let secs: i64 = normalized.parse().ok()?;
        let delay = secs.checked_mul(1_000)?;
        return u64::try_from(delay).ok().filter(|d| *d > 0);
    }
    let retry_at = parse_http_date(normalized)?;
    let delay = retry_at * 1_000 - now_epoch_ms;
    u64::try_from(delay).ok().filter(|d| *d > 0)
}

/// `computeUploadTimeoutMs`: 30 s base + 1 s per 128 KiB, floored.
pub fn compute_upload_timeout_ms(file_size_bytes: u64, floor_ms: u64) -> u64 {
    let per_file = UPLOAD_TIMEOUT_BASE_MS
        + file_size_bytes
            .div_ceil(UPLOAD_MIN_THROUGHPUT_BYTES_PER_SEC)
            .saturating_mul(1_000);
    floor_ms.max(per_file)
}

// ---------------------------------------------------------------------------
// Transport + client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// The wire seam - production plugs an HTTP backend here; tests inject
/// canned responses and assert on the exact requests.
pub trait ShareTransport: Send + Sync {
    fn request(&self, req: HttpRequest) -> Result<HttpResponse, String>;
}

pub type TokenProvider = Box<dyn Fn() -> Option<String> + Send + Sync>;

pub struct ShareHttpClient {
    base_url: String,
    token_provider: TokenProvider,
    transport: Box<dyn ShareTransport>,
    timeout_ms: u64,
    confirm_timeout_ms: u64,
    now_epoch_ms: Box<dyn Fn() -> i64 + Send + Sync>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    Required,
    Optional,
}

impl ShareHttpClient {
    pub fn new(
        base_url: impl Into<String>,
        token_provider: TokenProvider,
        transport: Box<dyn ShareTransport>,
    ) -> Self {
        ShareHttpClient {
            base_url: base_url.into(),
            token_provider,
            transport,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            confirm_timeout_ms: CONFIRM_TIMEOUT_MS,
            now_epoch_ms: Box::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            }),
        }
    }

    pub fn timeouts(mut self, timeout_ms: u64, confirm_timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self.confirm_timeout_ms = confirm_timeout_ms;
        self
    }

    fn join_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn request_id() -> String {
        let mut bytes = [0u8; 16];
        let _ = getrandom::getrandom(&mut bytes);
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn normalize_request_id(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty()
            || trimmed.len() > 128
            || !trimmed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
        {
            return None;
        }
        Some(trimmed.to_string())
    }

    /// The single response-normalization funnel (`requestData`).
    fn request_data(
        &self,
        path: &str,
        method: &'static str,
        body: Option<(Vec<u8>, String)>,
        auth: Auth,
        timeout_override: Option<u64>,
    ) -> Result<Value, ShareClientError> {
        let token = (self.token_provider)().map(|t| t.trim().to_string());
        let token = token.as_deref().filter(|t| !t.is_empty());
        if auth == Auth::Required && token.is_none() {
            return Err(client_error(
                ClientErrorKind::AuthenticationRequired,
                "Conversation share authentication required",
            ));
        }

        let mut headers = BTreeMap::new();
        headers.insert(REQUEST_ID_HEADER.to_string(), Self::request_id());
        let body_bytes = match body {
            Some((bytes, content_type)) => {
                headers.insert("Content-Type".to_string(), content_type);
                bytes
            }
            None => Vec::new(),
        };
        if let Some(token) = token {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        }

        let response = self
            .transport
            .request(HttpRequest {
                method,
                url: self.join_url(path),
                headers,
                body: body_bytes,
                timeout_ms: timeout_override.unwrap_or(self.timeout_ms),
            })
            .map_err(|e| client_error(ClientErrorKind::Network, e))?;

        let status = response.status;
        let response_request_id = response
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|v| Self::normalize_request_id(v));
        let text = String::from_utf8_lossy(&response.body).into_owned();
        let with_request_id = |mut e: ShareClientError| {
            e.status = Some(status);
            e.request_id = response_request_id.clone();
            e
        };

        if !(200..300).contains(&status) {
            if status == 401 && text.trim().is_empty() {
                return Err(with_request_id(client_error(
                    ClientErrorKind::AuthenticationRequired,
                    "Conversation share authentication required",
                )));
            }
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                if let Ok(envelope) = serde_json::from_str::<Value>(trimmed) {
                    let code = envelope.get("code").and_then(Value::as_u64);
                    let code = code.and_then(|c| u32::try_from(c).ok());
                    let msg = envelope
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("rejected")
                        .to_string();
                    let kind = match code {
                        Some(c) => kind_for_code(c).unwrap_or(ClientErrorKind::Unknown),
                        None => ClientErrorKind::Unknown,
                    };
                    let mut error = with_request_id(client_error(kind, msg));
                    error.code = code;
                    if code == Some(3215) {
                        error.retry_after_ms = response
                            .headers
                            .get("retry-after")
                            .and_then(|v| parse_retry_after_ms(v, (self.now_epoch_ms)()));
                    }
                    return Err(error);
                }
                // non-JSON error body: 5xx is infrastructure, else contract
                if status >= 500 {
                    return Err(with_request_id(client_error(
                        ClientErrorKind::Network,
                        format!("Conversation share API upstream failed with HTTP {status}"),
                    )));
                }
                return Err(with_request_id(client_error(
                    ClientErrorKind::InvalidContract,
                    "Conversation share API returned invalid JSON",
                )));
            }
            let kind = if status >= 500 {
                ClientErrorKind::Network
            } else if status == 429 {
                ClientErrorKind::RateLimited
            } else {
                ClientErrorKind::Unknown
            };
            return Err(with_request_id(client_error(
                kind,
                format!("Conversation share API failed with HTTP {status}"),
            )));
        }

        // success: unwrap the `{ data: ... }` envelope
        let parsed: Value = serde_json::from_str(&text).map_err(|e| {
            with_request_id(client_error(
                ClientErrorKind::InvalidContract,
                format!("Conversation share API response is not JSON: {e}"),
            ))
        })?;
        parsed.get("data").cloned().ok_or_else(|| {
            with_request_id(client_error(
                ClientErrorKind::InvalidContract,
                "Conversation share API response does not match the client contract",
            ))
        })
    }

    fn get(&self, path: &str, auth: Auth) -> Result<Value, ShareClientError> {
        self.request_data(path, "GET", None, auth, None)
    }

    fn post_json(
        &self,
        path: &str,
        body: Value,
        auth: Auth,
        timeout: Option<u64>,
    ) -> Result<Value, ShareClientError> {
        let bytes = serde_json::to_vec(&body).unwrap_or_default();
        self.request_data(
            path,
            "POST",
            Some((bytes, "application/json".to_string())),
            auth,
            timeout,
        )
    }

    /// GET /shares/capabilities
    pub fn get_capabilities(
        &self,
    ) -> Result<super::artifacts::ShareCapabilities, ShareClientError> {
        let wire = self.get("/shares/capabilities", Auth::Required)?;
        Ok(super::artifacts::ShareCapabilities::from_wire(&wire))
    }

    /// POST /shares/preparations
    pub fn create_preparation(&self, request: Value) -> Result<Value, ShareClientError> {
        self.post_json("/shares/preparations", request, Auth::Required, None)
    }

    /// POST /shares/preparations/:id/artifacts - multipart upload with the
    /// size-scaled timeout.
    pub fn upload_artifact(
        &self,
        preparation_id: &str,
        descriptor: &Value,
        file_name: &str,
        file: &[u8],
    ) -> Result<Value, ShareClientError> {
        let boundary = format!("okra-share-{}", Self::request_id());
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"descriptor\"\r\n\r\n");
        body.extend_from_slice(serde_json::to_string(descriptor).unwrap_or_default().as_bytes());
        body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(file);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let timeout = compute_upload_timeout_ms(file.len() as u64, self.timeout_ms);
        let path = format!(
            "/shares/preparations/{}/artifacts",
            percent_encode(preparation_id)
        );
        self.request_data(
            &path,
            "POST",
            Some((body, format!("multipart/form-data; boundary={boundary}"))),
            Auth::Required,
            Some(timeout),
        )
    }

    /// POST /shares/preparations/:id/confirm - long safety checks tolerated.
    pub fn confirm(&self, preparation_id: &str, request: Value) -> Result<Value, ShareClientError> {
        let path = format!(
            "/shares/preparations/{}/confirm",
            percent_encode(preparation_id)
        );
        self.post_json(&path, request, Auth::Required, Some(self.confirm_timeout_ms))
    }

    /// GET /shares/:code/preview - auth optional.
    pub fn get_preview(&self, share_code: &str) -> Result<Value, ShareClientError> {
        self.get(
            &format!("/shares/{}/preview", percent_encode(share_code)),
            Auth::Optional,
        )
    }

    /// POST /shares/:code/continuation - auth optional, integrity verified
    /// on the RAW values before any decoding.
    pub fn get_continuation(
        &self,
        share_code: &str,
        request: Value,
    ) -> Result<Value, ShareClientError> {
        let path = format!("/shares/{}/continuation", percent_encode(share_code));
        let wire = self.post_json(&path, request, Auth::Optional, None)?;
        let integrity: ShareIntegrity =
            serde_json::from_value(wire.get("integrity").cloned().unwrap_or(Value::Null))
                .map_err(|e| {
                    client_error(
                        ClientErrorKind::InvalidContract,
                        format!("continuation integrity block unreadable: {e}"),
                    )
                })?;
        let rows = wire.get("rows").cloned().unwrap_or(Value::Array(Vec::new()));
        let artifacts = wire
            .get("artifacts")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        let intact = verify_integrity(&rows, &artifacts, &integrity)
            .map_err(|e| {
                client_error(
                    ClientErrorKind::InvalidContract,
                    format!("continuation integrity unusable: {e}"),
                )
            })?;
        if !intact {
            return Err(client_error(
                ClientErrorKind::InvalidContract,
                "Conversation share integrity check failed",
            ));
        }
        Ok(wire)
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::build_integrity;
    use std::sync::{Arc, Mutex};

    /// Canned transport; `seen` is shared with the test for assertions.
    struct FakeTransport {
        respond: Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, String> + Send + Sync>,
        seen: Arc<Mutex<Vec<HttpRequest>>>,
    }

    impl ShareTransport for FakeTransport {
        fn request(&self, req: HttpRequest) -> Result<HttpResponse, String> {
            let result = (self.respond)(&req);
            self.seen.lock().unwrap().push(req);
            result
        }
    }

    fn transport_with(
        respond: impl Fn(&HttpRequest) -> Result<HttpResponse, String> + Send + Sync + 'static,
    ) -> (Arc<Mutex<Vec<HttpRequest>>>, Box<FakeTransport>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let transport = Box::new(FakeTransport {
            respond: Box::new(respond),
            seen: Arc::clone(&seen),
        });
        (seen, transport)
    }

    fn ok(body: Value) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&serde_json::json!({ "data": body })).unwrap(),
        })
    }

    fn make_client(transport: Box<FakeTransport>, token: Option<String>) -> ShareHttpClient {
        ShareHttpClient::new(
            "https://api.example.com/",
            Box::new(move || token.clone()),
            transport,
        )
    }

    #[test]
    fn capabilities_requires_token_and_unwraps_envelope() {
        let (seen, transport) = transport_with(|_| {
            ok(serde_json::json!({
                "allowed_artifacts": [
                    { "type": "document", "extensions": [".md"], "mime_types": ["text/markdown"] }
                ],
                "access_modes": ["view"]
            }))
        });
        let client = make_client(transport, Some("tok-1".to_string()));
        let caps = client.get_capabilities().unwrap();
        assert_eq!(caps.allowed_artifacts.len(), 1);
        let requests = seen.lock().unwrap();
        let req = requests.last().unwrap();
        assert_eq!(req.url, "https://api.example.com/shares/capabilities");
        assert_eq!(req.headers.get("Authorization").unwrap(), "Bearer tok-1");
        assert!(req.headers.contains_key(REQUEST_ID_HEADER));
    }

    #[test]
    fn required_endpoints_refuse_locally_without_token() {
        let (seen, transport) = transport_with(|_| ok(serde_json::json!({})));
        let client = make_client(transport, None);
        let e = client.get_capabilities().unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::AuthenticationRequired);
        assert_eq!(e.status, None, "refused before any network");
        assert!(seen.lock().unwrap().is_empty(), "no request was made");

        // auth-optional endpoints still go out
        let (seen, transport) = transport_with(|_| ok(serde_json::json!({ "rows": [] })));
        let client = make_client(transport, None);
        client.get_preview("abc").unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn error_envelope_mapping() {
        let cases: Vec<(u32, ClientErrorKind)> = vec![
            (3205, ClientErrorKind::InvalidConversation),
            (3209, ClientErrorKind::LimitExceeded),
            (3002, ClientErrorKind::RateLimited),
            (3212, ClientErrorKind::Expired),
        ];
        for (code, want_kind) in cases {
            let (seen, transport) = transport_with(move |_| {
                Ok(HttpResponse {
                    status: 422,
                    headers: BTreeMap::new(),
                    body: serde_json::to_vec(
                        &serde_json::json!({ "code": code, "msg": "server says no" }),
                    )
                    .unwrap(),
                })
            });
            let e = make_client(transport, Some("t".to_string()))
                .create_preparation(serde_json::json!({}))
                .unwrap_err();
            assert_eq!(e.kind, want_kind);
            assert_eq!(e.status, Some(422));
            assert_eq!(e.code, Some(code));
            assert_eq!(e.message, "server says no");
            assert_eq!(seen.lock().unwrap().len(), 1);
        }

        // unknown code degrades to unknown but keeps msg + status
        let (_, transport) = transport_with(|_| {
            Ok(HttpResponse {
                status: 422,
                headers: BTreeMap::new(),
                body: serde_json::to_vec(&serde_json::json!({ "code": 9999, "msg": "mystery" }))
                    .unwrap(),
            })
        });
        let e = make_client(transport, Some("t".to_string()))
            .create_preparation(serde_json::json!({}))
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::Unknown);
        assert_eq!(e.code, Some(9999));
        assert_eq!(e.message, "mystery");
    }

    #[test]
    fn response_classification_matches_donor_fallbacks() {
        // 401 + empty body -> authentication_required
        let (_, transport) = transport_with(|_| {
            Ok(HttpResponse { status: 401, headers: BTreeMap::new(), body: Vec::new() })
        });
        let e = make_client(transport, Some("t".to_string()))
            .get_preview("c")
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::AuthenticationRequired);
        assert_eq!(e.status, Some(401));

        // 502 non-JSON (gateway HTML) -> network
        let (_, transport) = transport_with(|_| {
            Ok(HttpResponse {
                status: 502,
                headers: BTreeMap::new(),
                body: b"<html>bad gateway</html>".to_vec(),
            })
        });
        let e = make_client(transport, Some("t".to_string()))
            .get_preview("c")
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::Network);

        // 400 non-JSON -> invalid_contract
        let (_, transport) = transport_with(|_| {
            Ok(HttpResponse { status: 400, headers: BTreeMap::new(), body: b"nope".to_vec() })
        });
        let e = make_client(transport, Some("t".to_string()))
            .get_preview("c")
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::InvalidContract);

        // 429 empty -> rate_limited
        let (_, transport) = transport_with(|_| {
            Ok(HttpResponse { status: 429, headers: BTreeMap::new(), body: Vec::new() })
        });
        let e = make_client(transport, Some("t".to_string()))
            .get_preview("c")
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::RateLimited);

        // transport failure -> network
        let (_, transport) = transport_with(|_| Err("connection reset".to_string()));
        let e = make_client(transport, Some("t".to_string()))
            .get_preview("c")
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::Network);
        assert!(e.message.contains("connection reset"));
    }

    #[test]
    fn retry_after_parsed_for_safety_check_pending() {
        let (_, transport) = transport_with(|_| {
            let mut headers = BTreeMap::new();
            headers.insert("retry-after".to_string(), "42".to_string());
            Ok(HttpResponse {
                status: 429,
                headers,
                body: serde_json::to_vec(&serde_json::json!({ "code": 3215, "msg": "pending" }))
                    .unwrap(),
            })
        });
        let e = make_client(transport, Some("t".to_string()))
            .confirm("p1", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::SafetyCheckPending);
        assert_eq!(e.retry_after_ms, Some(42_000));
    }

    #[test]
    fn retry_after_http_dates_are_strict() {
        let now = epoch_seconds(1994, 11, 6, 8, 49, 37) * 1_000;
        // IMF-fixdate at exactly now -> non-positive -> None
        assert_eq!(parse_retry_after_ms("Sun, 06 Nov 1994 08:49:37 GMT", now), None);
        assert_eq!(
            parse_retry_after_ms("Sun, 06 Nov 1994 08:50:14 GMT", now),
            Some(37_000)
        );
        // asctime form (space-padded single-digit day)
        assert_eq!(
            parse_retry_after_ms("Sun Nov  6 08:50:14 1994", now),
            Some(37_000)
        );
        // RFC 850 two-digit year
        assert_eq!(
            parse_retry_after_ms("Sunday, 06-Nov-94 08:50:14 GMT", now),
            Some(37_000)
        );
        // loose formats rejected
        assert_eq!(parse_retry_after_ms("soon", now), None);
        assert_eq!(parse_retry_after_ms("06 Nov 1994", now), None);
        assert_eq!(parse_retry_after_ms("", now), None);
        assert_eq!(parse_retry_after_ms(" 30 ", now), Some(30_000));
        assert_eq!(parse_retry_after_ms("0", now), None);
    }

    #[test]
    fn upload_timeout_scales_with_size_and_confirm_is_long() {
        // the formula always adds at least one second of budget on top of
        // the 30 s base; the floor matters for tiny caller-supplied floors
        assert_eq!(compute_upload_timeout_ms(0, 0), UPLOAD_TIMEOUT_BASE_MS);
        assert_eq!(compute_upload_timeout_ms(1, 0), UPLOAD_TIMEOUT_BASE_MS + 1_000);
        // 1 MiB at 128 KiB/s -> 8 s of budget
        assert_eq!(
            compute_upload_timeout_ms(1024 * 1024, 0),
            UPLOAD_TIMEOUT_BASE_MS + 8_000
        );
        // 64 MiB -> 512 s of budget + base
        assert_eq!(
            compute_upload_timeout_ms(64 * 1024 * 1024, DEFAULT_TIMEOUT_MS),
            UPLOAD_TIMEOUT_BASE_MS + 512_000
        );
        // a small upload keeps a bigger caller floor
        assert_eq!(compute_upload_timeout_ms(1, 60_000), 60_000);
        assert_eq!(CONFIRM_TIMEOUT_MS, 120_000);
    }

    #[test]
    fn upload_sends_multipart_with_scaled_timeout() {
        let (seen, transport) = transport_with(|_| ok(serde_json::json!({ "accepted": true })));
        let client = make_client(transport, Some("t".to_string()));
        let descriptor = serde_json::json!({ "artifact_id": "share-artifact-1" });
        let file = vec![0u8; 10];
        client
            .upload_artifact("prep/1", &descriptor, "notes.md", &file)
            .unwrap();
        let requests = seen.lock().unwrap();
        let req = requests.last().unwrap();
        assert!(req.url.ends_with("/shares/preparations/prep%2F1/artifacts"));
        let content_type = req.headers.get("Content-Type").unwrap();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let body = String::from_utf8_lossy(&req.body).into_owned();
        assert!(body.contains("name=\"descriptor\""));
        assert!(body.contains("share-artifact-1"));
        assert!(body.contains("filename=\"notes.md\""));
        assert!(body.starts_with(&format!("--{boundary}\r\n")));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
        assert_eq!(
            req.timeout_ms,
            UPLOAD_TIMEOUT_BASE_MS + 1_000,
            "10-byte file gets the base plus one throughput slice"
        );
    }

    #[test]
    fn continuation_verifies_integrity_on_raw_values() {
        let rows = serde_json::json!([{ "rowId": 1, "text": "hi" }]);
        let artifacts = vec![serde_json::json!({ "artifact_id": "a1", "size_bytes": 3 })];
        let integrity = build_integrity(&rows, &artifacts).unwrap();
        let wire = serde_json::json!({
            "schema_version": 1,
            "rows": rows,
            "artifacts": artifacts,
            "integrity": {
                "projection_sha256": integrity.projection_sha256,
                "artifact_set_sha256": integrity.artifact_set_sha256
            }
        });
        let (_, transport) = transport_with(move |_| ok(wire.clone()));
        let wire = make_client(transport, None)
            .get_continuation("abc123", serde_json::json!({}))
            .unwrap();
        assert_eq!(wire["rows"][0]["text"], "hi");

        // tampered rows fail with invalid_contract
        let mut tampered = wire.clone();
        tampered["rows"][0]["text"] = serde_json::json!("evil");
        let (_, transport) = transport_with(move |_| ok(tampered.clone()));
        let e = make_client(transport, None)
            .get_continuation("abc123", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(e.kind, ClientErrorKind::InvalidContract);
        assert!(e.message.contains("integrity"));
    }
}
