//! Embeddings client (M5 retrieval's network tier): OpenAI-compatible
//! `POST /v1/embeddings` over the same ureq/rustls client shape as the
//! chat provider (`openai.rs`); env and error taxonomy shared with it.
//! The offline tier lives in `okra-memory::retrieval` — this client is
//! optional, never a dependency of correctness.

use crate::openai::OpenAiConfig;
use crate::sampler::SamplerError;

pub struct EmbeddingClient {
    config: OpenAiConfig,
}

impl EmbeddingClient {
    /// None when no credential exists (the offline tier takes over —
    /// absence is not an error).
    pub fn from_env(model: impl Into<String>) -> Option<Self> {
        OpenAiConfig::from_env(model).map(|config| EmbeddingClient { config })
    }

    pub fn with_config(config: OpenAiConfig) -> Self {
        EmbeddingClient { config }
    }

    pub fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SamplerError> {
        let url = format!("{}/embeddings", self.config.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.config.model,
            "input": texts,
        });
        // the same agent shape as the chat provider (timeout-scoped,
        // rustls via the workspace ureq)
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(self.config.timeout_secs))
            .build();
        let mut req = agent
            .post(&url)
            .set("content-type", "application/json")
            .set("authorization", &format!("Bearer {}", self.config.api_key));
        for (k, v) in &self.config.extra_headers {
            req = req.set(k, v);
        }
        let payload = serde_json::to_string(&body).unwrap_or_default();
        let resp = req
            .send_string(&payload)
            .map_err(|e| SamplerError::Transient(e.to_string()))?;
        match resp.status() {
            200..=299 => {}
            401 => return Err(SamplerError::Unauthorized),
            429 => return Err(SamplerError::RateLimited { retry_after_secs: None }),
            _ => return Err(SamplerError::Permanent(format!("embeddings: {}", resp.status()))),
        }
        let parsed: serde_json::Value = resp
            .into_json()
            .map_err(|e| SamplerError::Permanent(format!("embeddings decode: {e}")))?;
        let mut out = Vec::new();
        let Some(data) = parsed["data"].as_array() else {
            return Err(SamplerError::Permanent("embeddings: no data array".into()));
        };
        for item in data {
            let Some(vector) = item["embedding"].as_array() else {
                return Err(SamplerError::Permanent("embeddings: missing vector".into()));
            };
            let v: Vec<f32> = vector
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            let v = if norm > 0.0 { v.iter().map(|x| x / norm).collect() } else { v };
            out.push(v);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_mock_server(response: &'static str) -> (String, std::thread::JoinHandle<()>) {
        // robust request read: consume headers + declared content-length
        // (same pattern as openai.rs's LocalServer — fixed line counts
        // break when the client's header set shifts between builds)
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 16384];
                let mut raw = Vec::new();
                loop {
                    let n = match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if let Some(hend) = text.find("\r\n\r\n") {
                        let declared = text[..hend]
                            .lines()
                            .find(|l| l.to_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if raw.len() >= hend + 4 + declared {
                            break;
                        }
                    }
                }
                let http = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                let _ = stream.write_all(http.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://{}", addr), handle)
    }

    #[test]
    fn embeds_and_normalizes() {
        // two near-orthogonal unit vectors, pre-normalized by the server
        let response = r#"{"data":[{"embedding":[3.0,0.0,0.0]},{"embedding":[0.0,5.0,0.0]}]}"#;
        let (base, _server) = spawn_mock_server(response);
        let client = EmbeddingClient::with_config(OpenAiConfig {
            base_url: format!("{base}/v1"),
            api_key: "test".into(),
            model: "text-embedding-3-small".into(),
            timeout_secs: 5,
            extra_headers: vec![],
        });
        let vectors = client.embed(&["hello", "world"]).unwrap();
        assert_eq!(vectors.len(), 2);
        assert!((vectors[0][0] - 1.0).abs() < 1e-5, "normalized: {:?}", vectors[0]);
        assert!((vectors[1][1] - 1.0).abs() < 1e-5, "normalized: {:?}", vectors[1]);
    }
}
