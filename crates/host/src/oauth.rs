//! OAuth2 host domain — the M3 strangler's next re-homed domain
//! (MASTER-PLAN §3 #48 lists "oauth" among the ZCode host domains).
//!
//! Flows implemented against any standard issuer (token endpoint JSON):
//! - **client-credentials** (machine-to-machine)
//! - **device authorization** (CLI/desktop: user approves on a second
//!   device; okra polls the token endpoint)
//! - **refresh-token**
//!
//! Tokens cache to a local file (mode 600) so host restarts reuse them;
//! expiry is tracked with a 30s clock-skew margin. HTTP goes through the
//! sanctioned ureq pattern (same as providers). Tests run against a local
//! mock issuer — no network in CI.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: String,
    /// Absolute expiry (unix seconds) with a 30s skew margin applied.
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IssuerConfig {
    pub token_endpoint: String,
    pub device_endpoint: Option<String>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub timeout_secs: u64,
}

impl IssuerConfig {
    pub fn new(token_endpoint: impl Into<String>, client_id: impl Into<String>) -> Self {
        IssuerConfig {
            token_endpoint: token_endpoint.into(),
            device_endpoint: None,
            client_id: client_id.into(),
            client_secret: None,
            timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OAuthError {
    #[error("issuer rejected the client: {0}")]
    InvalidClient(String),
    #[error("authorization pending (user has not approved yet)")]
    AuthorizationPending,
    #[error("the device code expired")]
    SlowDownExpired,
    #[error("access denied by the user")]
    AccessDenied,
    #[error("http status {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(String),
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_token(body: &str) -> Result<TokenSet, OAuthError> {
    #[derive(Deserialize)]
    struct Wire {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        token_type: Option<String>,
        #[serde(default)]
        expires_in: Option<u64>,
        #[serde(default)]
        scope: Option<String>,
    }
    let w: Wire = serde_json::from_str(body)
        .map_err(|_| OAuthError::Http { status: 200, body: format!("unparseable token response: {body}") })?;
    Ok(TokenSet {
        access_token: w.access_token,
        refresh_token: w.refresh_token,
        token_type: w.token_type.unwrap_or_else(|| "Bearer".into()),
        expires_at: now().saturating_add(w.expires_in.unwrap_or(3600).saturating_sub(30)),
        scope: w.scope.map(|s| s.split(' ').map(str::to_string).collect()).unwrap_or_default(),
    })
}

fn map_error(status: u16, body: &str) -> OAuthError {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(code) = v["error"].as_str() {
            return match code {
                "invalid_client" => OAuthError::InvalidClient(body.to_string()),
                "authorization_pending" => OAuthError::AuthorizationPending,
                "slow_down" | "expired_token" => OAuthError::SlowDownExpired,
                "access_denied" => OAuthError::AccessDenied,
                _ => OAuthError::Http { status, body: body.to_string() },
            };
        }
    OAuthError::Http { status, body: body.to_string() }
}

/// Token POST helper (form-encoded, per RFC 6749).
fn post_form(endpoint: &str, form: &str, timeout: u64) -> Result<TokenSet, OAuthError> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout))
        .build();
    match agent.post(endpoint).send_string(form) {
        Ok(resp) => {
            let status = resp.status();
            let mut body = String::new();
            let _ = resp.into_reader().read_to_string(&mut body);
            if (200..300).contains(&status) {
                parse_token(&body)
            } else {
                Err(map_error(status, &body))
            }
        }
        Err(ureq::Error::Status(status, resp)) => {
            let mut body = String::new();
            let _ = resp.into_reader().read_to_string(&mut body);
            Err(map_error(status, &body))
        }
        Err(e) => Err(OAuthError::Transport(format!("{e}"))),
    }
}

use std::io::Read;

/// The OAuth client for one issuer.
#[derive(Debug, Clone)]
pub struct OAuthClient {
    pub issuer: IssuerConfig,
    /// Token cache file (mode 600). None = no persistence.
    pub cache_path: Option<PathBuf>,
}

impl OAuthClient {
    pub fn new(issuer: IssuerConfig) -> Self {
        OAuthClient { issuer, cache_path: None }
    }

    pub fn with_cache(mut self, path: PathBuf) -> Self {
        self.cache_path = Some(path);
        self
    }

    /// RFC 6749 §4.4: client-credentials grant.
    pub fn client_credentials(&self, scope: Option<&str>) -> Result<TokenSet, OAuthError> {
        let mut form = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            urlencode(&self.issuer.client_id),
            urlencode(self.issuer.client_secret.as_deref().unwrap_or_default()),
        );
        if let Some(s) = scope {
            form.push_str(&format!("&scope={}", urlencode(s)));
        }
        let token = post_form(&self.issuer.token_endpoint, &form, self.issuer.timeout_secs)?;
        self.cache(&token)?;
        Ok(token)
    }

    /// RFC 6749 §6: refresh an access token.
    pub fn refresh(&self, refresh_token: &str, scope: Option<&str>) -> Result<TokenSet, OAuthError> {
        let mut form = format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            urlencode(refresh_token),
            urlencode(&self.issuer.client_id),
        );
        if let Some(secret) = &self.issuer.client_secret {
            form.push_str(&format!("&client_secret={}", urlencode(secret)));
        }
        if let Some(s) = scope {
            form.push_str(&format!("&scope={}", urlencode(s)));
        }
        let token = post_form(&self.issuer.token_endpoint, &form, self.issuer.timeout_secs)?;
        self.cache(&token)?;
        Ok(token)
    }

    /// RFC 8628 §3.1: start a device authorization.
    pub fn device_start(&self, scope: Option<&str>) -> Result<DeviceAuthorization, OAuthError> {
        let endpoint = self
            .issuer
            .device_endpoint
            .clone()
            .ok_or(OAuthError::Transport("no device endpoint configured".into()))?;
        let mut form = format!("client_id={}", urlencode(&self.issuer.client_id));
        if let Some(s) = scope {
            form.push_str(&format!("&scope={}", urlencode(s)));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(self.issuer.timeout_secs))
            .build();
        let resp = agent
            .post(&endpoint)
            .send_string(&form)
            .map_err(|e| match e {
                ureq::Error::Status(status, r) => {
                    let mut body = String::new();
                    let _ = r.into_reader().read_to_string(&mut body);
                    OAuthError::Http { status, body }
                }
                other => OAuthError::Transport(format!("{other}")),
            })?;
        let mut body = String::new();
        let _ = resp.into_reader().read_to_string(&mut body);
        #[derive(Deserialize)]
        struct Wire {
            device_code: String,
            user_code: String,
            verification_uri: String,
            #[serde(default)]
            verification_uri_complete: Option<String>,
            #[serde(default)]
            expires_in: Option<u64>,
            #[serde(default)]
            interval: Option<u64>,
        }
        let w: Wire = serde_json::from_str(&body)
            .map_err(|e| OAuthError::Http { status: 200, body: format!("unparseable device response: {e}") })?;
        Ok(DeviceAuthorization {
            device_code: w.device_code,
            user_code: w.user_code,
            verification_uri: w.verification_uri,
            verification_uri_complete: w.verification_uri_complete,
            expires_at: now().saturating_add(w.expires_in.unwrap_or(600)),
            interval_secs: w.interval.unwrap_or(5),
        })
    }

    /// RFC 8628 §3.4: one token poll for a device authorization.
    pub fn device_poll(&self, auth: &DeviceAuthorization) -> Result<TokenSet, OAuthError> {
        let form = format!(
            "grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code={}&client_id={}",
            urlencode(&auth.device_code),
            urlencode(&self.issuer.client_id),
        );
        post_form(&self.issuer.token_endpoint, &form, self.issuer.timeout_secs)
    }

    /// Return a cached, still-valid token (never refreshes).
    pub fn cached_token(&self) -> Option<TokenSet> {
        let path = self.cache_path.as_ref()?;
        let raw = std::fs::read_to_string(path).ok()?;
        let token: TokenSet = serde_json::from_str(&raw).ok()?;
        (token.expires_at > now()).then_some(token)
    }

    /// Cached-or-fresh: refresh when a cached refresh token exists and the
    /// access token expired; fall back to client-credentials.
    pub fn token(&self) -> Result<TokenSet, OAuthError> {
        if let Some(t) = self.cached_token() {
            return Ok(t);
        }
        if let Some(path) = &self.cache_path
            && let Ok(raw) = std::fs::read_to_string(path)
                && let Ok(old) = serde_json::from_str::<TokenSet>(&raw)
                    && let Some(rt) = &old.refresh_token {
                        return self.refresh(rt, None);
                    }
        self.client_credentials(None)
    }

    fn cache(&self, token: &TokenSet) -> Result<(), OAuthError> {
        let Some(path) = &self.cache_path else { return Ok(()) };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = serde_json::to_string(token).map_err(|e| OAuthError::Transport(e.to_string()))?;
        std::fs::write(path, body).map_err(|e| OAuthError::Transport(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    pub expires_at: u64,
    pub interval_secs: u64,
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Host-domain facade: the oauth service registered under the M3 domain.
pub struct OAuthDomain {
    pub client: OAuthClient,
}

impl OAuthDomain {
    pub fn new(client: OAuthClient) -> Self {
        OAuthDomain { client }
    }
}
