//! OAuth domain tests against a LOCAL mock issuer (no network in CI):
//! client-credentials, refresh, device flow (authorize + poll-until-
//! approved + expiry), and the token cache.

use okra_host::oauth::{IssuerConfig, OAuthClient, OAuthError};
use std::io::prelude::*;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One-shot mock issuer: reads the request body, responds with a
/// scripted JSON (or status) for a configurable number of requests.
struct MockIssuer {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl MockIssuer {
    fn spawn(scripted: Vec<(u16, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let responses_queue = Arc::new(Mutex::new(
            scripted.into_iter().map(|(s, b)| (s, b.to_string())).collect::<Vec<_>>(),
        ));
        let (r, b) = (Arc::clone(&responses_queue), Arc::clone(&bodies));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                // read the whole request (headers + content-length body)
                let mut raw = Vec::new();
                let mut buf = [0u8; 16384];
                let mut content_len = 0usize;
                let mut header_end = None;
                loop {
                    let n = match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if header_end.is_none() {
                        if let Some(i) = text.find("\r\n\r\n") {
                            header_end = Some(i + 4);
                            for line in text[..i].lines() {
                                let lower = line.to_lowercase();
                                if let Some(v) = lower.strip_prefix("content-length:") {
                                    content_len = v.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                    if let Some(he) = header_end {
                        if raw.len() >= he + content_len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&raw).into_owned();
                let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
                b.lock().unwrap().push(text[body_start..].to_string());
                let (status, body) = {
                    let mut q = r.lock().unwrap();
                    if q.is_empty() {
                        (500u16, "{}".to_string())
                    } else {
                        q.remove(0)
                    }
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.flush();
            }
        });
        MockIssuer { port, bodies }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{port}{path}", port = self.port)
    }

    fn last_body(&self) -> String {
        self.bodies.lock().unwrap().last().cloned().unwrap_or_default()
    }
}

const TOKEN_OK: &str = r#"{"access_token":"at-123","refresh_token":"rt-456","token_type":"Bearer","expires_in":3600,"scope":"read write"}"#;
const TOKEN_REFRESHED: &str = r#"{"access_token":"at-789","token_type":"Bearer","expires_in":3600}"#;

fn issuer_with_client_id() -> (IssuerConfig, MockIssuer) {
    let issuer = MockIssuer::spawn(vec![(200, TOKEN_OK)]);
    let cfg = IssuerConfig::new(issuer.url("/token"), "okra-client");
    (cfg, issuer)
}

#[test]
fn client_credentials_roundtrip() {
    let (cfg, issuer) = issuer_with_client_id();
    let client = OAuthClient::new(cfg);
    let token = client.client_credentials(Some("read write")).unwrap();
    assert_eq!(token.access_token, "at-123");
    assert_eq!(token.refresh_token.as_deref(), Some("rt-456"));
    assert!(token.expires_at > token.expires_at - 3600, "expiry tracked");
    assert_eq!(token.scope, vec!["read".to_string(), "write".to_string()]);
    // the wire form carried the grant + credentials
    let body = issuer.last_body();
    assert!(body.contains("grant_type=client_credentials"));
    assert!(body.contains("client_id=okra-client"));
    assert!(body.contains("scope=read%20write"));
}

#[test]
fn refresh_roundtrip_swaps_access_token() {
    let issuer = MockIssuer::spawn(vec![(200, TOKEN_OK), (200, TOKEN_REFRESHED)]);
    let client = OAuthClient::new(IssuerConfig::new(issuer.url("/token"), "c1"));
    let first = client.client_credentials(None).unwrap();
    let second = client.refresh(first.refresh_token.as_deref().unwrap(), None).unwrap();
    assert_eq!(second.access_token, "at-789");
}

#[test]
fn invalid_client_fails_closed() {
    let issuer = MockIssuer::spawn(vec![(401, r#"{"error":"invalid_client"}"#)]);
    let client = OAuthClient::new(IssuerConfig::new(issuer.url("/token"), "bad"));
    match client.client_credentials(None).unwrap_err() {
        OAuthError::InvalidClient(body) => assert!(body.contains("invalid_client")),
        e => panic!("unexpected: {e:?}"),
    }
}

#[test]
fn device_flow_start_poll_and_expiry_mapping() {
    let device_body = r#"{"device_code":"dc-1","user_code":"ABCD-EFGH","verification_uri":"https://example.com/activate","expires_in":600,"interval":1}"#;
    let pending_body = r#"{"error":"authorization_pending"}"#;
    let ok_body = TOKEN_OK;

    // device endpoint then 2 token polls: pending, pending, then approved
    let issuer = MockIssuer::spawn(vec![
        (200, device_body),
        (400, pending_body),
        (200, ok_body),
    ]);
    let cfg = IssuerConfig::new(issuer.url("/token"), "c1");
    let cfg = IssuerConfig {
        device_endpoint: Some(issuer.url("/device")),
        ..cfg
    };
    let client = OAuthClient::new(cfg);

    let auth = client.device_start(Some("read")).unwrap();
    assert_eq!(auth.user_code, "ABCD-EFGH");
    assert_eq!(auth.interval_secs, 1);

    // poll 1: pending
    match client.device_poll(&auth) {
        Err(OAuthError::AuthorizationPending) => {}
        other => panic!("expected pending, got {other:?}"),
    }
    // poll 2: approved
    let token = client.device_poll(&auth).unwrap();
    assert_eq!(token.access_token, "at-123");
}

#[test]
fn token_cache_roundtrip_and_expiry() {
    let td = tempfile::tempdir().unwrap();
    let cache = td.path().join("tokens.json");
    let (cfg, _issuer) = issuer_with_client_id();
    let client = OAuthClient::new(cfg).with_cache(cache.clone());
    client.client_credentials(None).unwrap();

    // cache file exists, mode 600, readable, and the cached token returns
    let meta = std::fs::metadata(&cache).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "cache is mode 600");
    }
    let cached = client.cached_token().unwrap();
    assert_eq!(cached.access_token, "at-123");
}
