//! OpenAI-compatible network provider (MASTER-PLAN §3 #35, M1).
//!
//! One sync HTTP client (`ureq`, rustls) speaking the OpenAI
//! `/chat/completions` wire — the de-facto compatible surface (OpenAI,
//! vLLM, Ollama, OpenRouter, …) — behind the same `Sampler` seam, with
//! errors classified into the closed taxonomy the turn-loop governors
//! already understand:
//!
//! - 401 → `Unauthorized` (uncharged park, grok auth_retry semantics)
//! - 429 → `RateLimited { retry_after }` (rate-limit park governor)
//! - context-length markers → `ContextLength` (compaction path)
//! - 5xx / connect timeouts → `Transient` (bounded 1s/2s/4s retry)
//! - everything else → `Permanent`
//!
//! Network is only touched when a provider is explicitly configured; the
//! offline demo/task paths never construct this type.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::messages::{ContentBlock, Message, Role, ToolCall};
use crate::sampler::{
    SampleRequest, SampleResponse, Sampler, SamplerError, StopReason, Usage,
};

/// Default base URL; override for compatible servers (vLLM, Ollama, …).
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
    /// Extra headers (e.g. gateway session-affinity like
    /// `x-opencode-session`), set via `OKRA_EXTRA_HEADERS` as
    /// `Name: value; Name2: value2`.
    pub extra_headers: Vec<(String, String)>,
}

impl OpenAiConfig {
    pub fn from_env(model: impl Into<String>) -> Option<OpenAiConfig> {
        let api_key = std::env::var("OKRA_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok()?;
        let extra_headers = std::env::var("OKRA_EXTRA_HEADERS")
            .map(|raw| {
                raw.split(';')
                    .filter_map(|pair| {
                        let (k, v) = pair.split_once(':')?;
                        Some((k.trim().to_string(), v.trim().to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(OpenAiConfig {
            base_url: std::env::var("OKRA_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            api_key,
            model: model.into(),
            timeout_secs: 120,
            extra_headers,
        })
    }
}

// ---- wire types (request) ----

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WireTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
struct WireMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct WireTool {
    r#type: &'static str,
    function: WireFunction,
}

#[derive(Debug, Serialize)]
struct WireFunction {
    name: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct WireToolCall {
    id: String,
    r#type: &'static str,
    function: WireFunctionCall,
}

#[derive(Debug, Serialize)]
struct WireFunctionCall {
    name: String,
    arguments: String,
}

// ---- wire types (response) ----

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<RespToolCall>>,
}

#[derive(Debug, Deserialize)]
struct RespToolCall {
    id: String,
    #[serde(default)]
    function: Option<RespFunction>,
}

#[derive(Debug, Clone, Deserialize)]
struct RespFunction {
    name: String,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

fn role_wire(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn to_wire_messages(messages: &[Message]) -> Vec<WireMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        let role = role_wire(m.role);
        let text: String = m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tool_calls: Option<Vec<WireToolCall>> = {
            let calls: Vec<WireToolCall> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { call } => Some(WireToolCall {
                        id: call.id.clone(),
                        r#type: "function",
                        function: WireFunctionCall {
                            name: call.name.clone(),
                            arguments: call.args_json.clone(),
                        },
                    }),
                    _ => None,
                })
                .collect();
            if calls.is_empty() { None } else { Some(calls) }
        };
        match m.role {
            Role::Tool => {
                // one wire message per tool result block
                for b in &m.content {
                    if let ContentBlock::ToolResponse { result } = b {
                        out.push(WireMessage {
                            role: "tool",
                            content: Some(result.content.clone()),
                            tool_calls: None,
                            tool_call_id: Some(result.call_id.clone()),
                        });
                    }
                }
                if out.last().map(|w| w.role) != Some("tool") {
                    out.push(WireMessage { role, content: Some(text), tool_calls: None, tool_call_id: None });
                }
            }
            _ => {
                out.push(WireMessage {
                    role,
                    content: if text.is_empty() { None } else { Some(text) },
                    tool_calls,
                    tool_call_id: None,
                });
            }
        }
    }
    out
}

fn to_wire_tools(tools: &[crate::sampler::ToolView]) -> Option<Vec<WireTool>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|t| WireTool {
                r#type: "function",
                function: WireFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.arguments_schema.clone(),
                },
            })
            .collect(),
    )
}

/// Map an HTTP failure to the closed sampler error taxonomy.
fn classify_status(status: u16, body: &str, retry_after: Option<u64>) -> SamplerError {
    match status {
        401 | 403 => SamplerError::Unauthorized,
        429 => SamplerError::RateLimited { retry_after_secs: retry_after },
        500..=599 => SamplerError::Transient(format!("server error {status}: {}", truncate_body(body))),
        _ => {
            let lowered = body.to_lowercase();
            if lowered.contains("context length")
                || lowered.contains("maximum context")
                || lowered.contains("too many tokens")
            {
                SamplerError::ContextLength
            } else {
                SamplerError::Permanent(format!("http {status}: {}", truncate_body(body)))
            }
        }
    }
}

fn truncate_body(body: &str) -> String {
    body.chars().take(200).collect()
}

/// The provider. Cloneable; cheap (config only).
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    config: OpenAiConfig,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Self {
        OpenAiProvider { config }
    }

    /// Build from `OKRA_API_KEY` / `OPENAI_API_KEY` and `OKRA_BASE_URL`.
    pub fn from_env(model: impl Into<String>) -> Option<Self> {
        OpenAiConfig::from_env(model).map(OpenAiProvider::new)
    }

    fn agent(&self) -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(self.config.timeout_secs))
            .build()
    }

    fn parse_response(&self, body: &str) -> Result<SampleResponse, SamplerError> {
        let parsed: ChatResponse = serde_json::from_str(body)
            .map_err(|e| SamplerError::Permanent(format!("unparseable response: {e}")))?;
        let choice = parsed
            .choices
            .first()
            .ok_or_else(|| SamplerError::Permanent("empty choices".into()))?;
        let text = choice.message.content.clone().unwrap_or_default();
        let tool_calls: Vec<ToolCall> = choice
            .message
            .tool_calls
            .iter()
            .flatten()
            .map(|c: &RespToolCall| {
                let function = c.function.clone().unwrap_or_else(|| RespFunction {
                    name: String::new(),
                    arguments: None,
                });
                ToolCall {
                    id: c.id.clone(),
                    name: function.name,
                    args_json: function.arguments.unwrap_or_else(|| "{}".into()),
                }
            })
            .collect();
        let stop_reason = match choice.finish_reason.as_deref() {
            Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            _ => {
                if tool_calls.is_empty() {
                    StopReason::EndTurn
                } else {
                    StopReason::ToolUse
                }
            }
        };
        let usage = parsed
            .usage
            .map(|u| Usage { input_tokens: u.prompt_tokens, output_tokens: u.completion_tokens })
            .unwrap_or_default();
        Ok(SampleResponse { text, tool_calls, stop_reason, usage })
    }
}

impl Sampler for OpenAiProvider {
    fn sample(&self, request: &SampleRequest) -> Result<SampleResponse, SamplerError> {
        let body = ChatRequest {
            model: &self.config.model,
            messages: to_wire_messages(&request.messages),
            tools: to_wire_tools(&request.tools),
            max_tokens: request.max_tokens,
        };
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut req = self
            .agent()
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key));
        for (name, value) in &self.config.extra_headers {
            req = req.set(name, value);
        }
        let response = req.send_json(&body);

        match response {
            Ok(deserialized) => {
                let body = deserialized.into_string().map_err(|e| {
                    SamplerError::Transient(format!("failed to read response body: {e}"))
                })?;
                self.parse_response(&body)
            }
            Err(ureq::Error::Status(code, response)) => {
                let retry_after = response
                    .header("retry-after")
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let body = response.into_string().unwrap_or_default();
                Err(classify_status(code, &body, retry_after))
            }
            Err(ureq::Error::Transport(t)) => Err(SamplerError::Transient(format!(
                "transport: {}",
                t
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sampler::ToolView;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot local HTTP server: captures the request body, replies with
    /// `response` (+ status). Lets the provider be tested against REAL http.
    struct LocalServer {
        port: u16,
        captured: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    impl LocalServer {
        /// Spawn with a full raw HTTP response (status line + headers + body).
        fn spawn_raw(response: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
            let thread_captured = std::sync::Arc::clone(&captured);
            let server = LocalServer { port, captured };
            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    // read the entire request before responding (see spawn)
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
                            let headers = &text[..hend];
                            let declared = headers
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
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    let _ = thread_captured;
                }
            });
            server
        }

        fn spawn(response_status: u16, response_body: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
            let thread_captured = std::sync::Arc::clone(&captured);
            let server = LocalServer { port, captured };
            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    // Read the ENTIRE request (headers + Content-Length body).
                    // Responding early would drop the socket with unread data,
                    // sending RST and killing the client's response read.
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
                            let headers = &text[..hend];
                            let declared = headers
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
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                    *thread_captured.lock().unwrap() = Some(text[body_start..].to_string());
                    let reason = if response_status == 200 { "OK" } else { "Error" };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                        status = response_status,
                        len = response_body.len(),
                        body = response_body,
                    );
                    let _ = stream.flush();
                }
            });
            server
        }
    }

    fn provider(port: u16) -> OpenAiProvider {
        OpenAiProvider::new(OpenAiConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "test-key".into(),
            model: "test-model".into(),
            timeout_secs: 5,
            extra_headers: vec![],
        })
    }

    fn sample_request() -> SampleRequest {
        SampleRequest {
            messages: vec![Message::user("hello")],
            tools: vec![],
            max_tokens: None,
            structured_output_schema: None,
        }
    }

    #[test]
    fn happy_path_maps_tool_calls_and_usage() {
        let body = r#"{
            "choices": [{
                "message": {
                    "content": "Let me check.",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7 }
        }"#;
        let server = LocalServer::spawn(200, body);
        let resp = provider(server.port).sample(&sample_request()).unwrap();
        assert_eq!(resp.text, "Let me check.");
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "read_file");
        assert_eq!(resp.usage.input_tokens, 11);
        // request carried bearer auth + the model
        let captured = server.captured.lock().unwrap().clone().unwrap();
        assert!(captured.contains("\"model\":\"test-model\""), "{captured}");
    }

    #[test]
    fn stop_and_length_reasons_map() {
        let server = LocalServer::spawn(
            200,
            r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}]}"#,
        );
        let resp = provider(server.port).sample(&sample_request()).unwrap();
        assert_eq!(resp.stop_reason, StopReason::EndTurn);

        let server = LocalServer::spawn(
            200,
            r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}"#,
        );
        let resp = provider(server.port).sample(&sample_request()).unwrap();
        assert_eq!(resp.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn unauthorized_maps_to_uncharged_park_error() {
        let server = LocalServer::spawn(401, r#"{"error":{"message":"bad key"}}"#);
        let err = provider(server.port).sample(&sample_request()).unwrap_err();
        assert!(matches!(err, SamplerError::Unauthorized), "{err:?}");
    }

    #[test]
    fn rate_limit_maps_with_retry_after() {
        let body = r#"{"error":"rate limited"}"#;
        let raw: &'static str = Box::leak(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                len = body.len()
            )
            .into_boxed_str(),
        );
        let server = LocalServer::spawn_raw(raw);
        let err = provider(server.port).sample(&sample_request()).unwrap_err();
        assert!(
            matches!(err, SamplerError::RateLimited { retry_after_secs: Some(3) }),
            "{err:?}"
        );
    }

    #[test]
    fn server_errors_are_transient_for_the_retry_budget() {
        let server = LocalServer::spawn(503, "overloaded");
        let err = provider(server.port).sample(&sample_request()).unwrap_err();
        assert!(matches!(err, SamplerError::Transient(_)), "{err:?}");
    }

    #[test]
    fn context_length_markers_map_to_compaction_path() {
        let server = LocalServer::spawn(
            400,
            r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
        );
        let err = provider(server.port).sample(&sample_request()).unwrap_err();
        assert!(matches!(err, SamplerError::ContextLength), "{err:?}");
    }

    #[test]
    fn tools_are_sent_as_function_definitions() {
        let server = LocalServer::spawn(
            200,
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
        );
        let req = SampleRequest {
            messages: vec![Message::user("hi")],
            tools: vec![ToolView {
                name: "read_file".into(),
                description: "Read a file".into(),
                arguments_schema: Some(serde_json::json!({"type":"object"})),
            }],
            max_tokens: None,
            structured_output_schema: None,
        };
        provider(server.port).sample(&req).unwrap();
        let captured = server.captured.lock().unwrap().clone().unwrap();
        assert!(captured.contains("\"type\":\"function\""), "{captured}");
        assert!(captured.contains("read_file"), "{captured}");
    }
}
