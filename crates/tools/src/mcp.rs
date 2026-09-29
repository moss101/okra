//! MCP client — MASTER-PLAN §3 #47: "MCP client + `use_tool`-style single
//! dispatch funnel + deferred schemas" (grok `xai-grok-mcp` + qwen
//! deferral).
//!
//! - Speaks JSON-RPC 2.0 over a `Transport` (stdio child per server, or an
//!   in-process transport for tests/offline hosts).
//! - `initialize` → `tools/list` → `tools/call`, per the MCP basics.
//! - **Single dispatch funnel**: MCP tools are NOT exposed to the model
//!   individually. One `use_tool` tool routes `{server, tool, arguments}`
//!   into the right server — the model-facing surface stays fixed no matter
//!   how many MCP servers are attached.
//! - **Deferred schemas**: full JSON schemas are not in the system prompt;
//!   a `tool_directory` tool serves them on demand (qwen `shouldDefer` +
//!   `tool_search` semantics).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::process::{self, PersistentChild};
use crate::stream::{ToolError, ToolOutput, ToolStream};

// ---- transport ----

pub trait Transport: Send {
    /// Send one JSON-RPC request object, return the response's `result` or
    /// `error` field (already extracted).
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String>;
}

/// Stdio transport: one child process per JSON-RPC exchange (sanctioned
/// spawn site). Covers stateless/one-shot MCP servers — the scriptable
/// pattern for M2; persistent-session servers land with the gateway.
pub struct StdioTransport {
    program: String,
    args: Vec<String>,
    next_id: AtomicU64,
}

impl StdioTransport {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        StdioTransport { program: program.into(), args, next_id: AtomicU64::new(0) }
    }
}

impl Transport for StdioTransport {
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let out = process::run_captured(
            &self.program,
            &self.args,
            std::time::Duration::from_secs(30),
            Some(&request.to_string()),
        );
        if out.timed_out {
            return Err(format!("mcp `{method}` timed out"));
        }
        // find the response line with the matching id
        for line in out.stdout.lines() {
            let Ok(msg) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return Ok(msg);
            }
        }
        Err(format!(
            "mcp `{method}`: no matching response (stdout {} bytes, stderr: {})",
            out.stdout.len(),
            out.stderr.trim()
        ))
    }
}

/// Handler signature for the in-process MCP server stub.
pub type McpHandlerFn = dyn FnMut(&str, Value) -> Result<Value, String> + Send;

/// In-process transport: a handler closure stands in for the MCP server
/// (tests, offline benchmark). The handler returns the RESULT object; this
/// transport wraps it into the JSON-RPC response envelope, identical to
/// what `rpc` expects from the stdio transport.
pub struct InProcessTransport {
    pub handler: Box<McpHandlerFn>,
}

impl Transport for InProcessTransport {
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        match (self.handler)(method, params) {
            Ok(result) => Ok(serde_json::json!({ "result": result })),
            Err(message) => Ok(serde_json::json!({ "error": { "message": message } })),
        }
    }
}

/// An MCP server connection: persistent JSON-RPC over any transport with
/// id-matched requests.
pub struct McpClient {
    transport: Box<dyn Transport>,
    pub server_name: String,
    pub protocol_version: String,
    pub server_info: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

impl McpClient {
    /// Connect over stdio (one-shot JSON-RPC exchanges through the
    /// sanctioned runner; persistent-session servers land with the
    /// gateway at M4).
    pub fn stdio(server_name: &str, program: &str, args: &[String]) -> McpClient {
        McpClient {
            transport: Box::new(StdioTransport::new(program, args.to_vec())),
            server_name: server_name.to_string(),
            protocol_version: "2024-11-05".into(),
            server_info: Value::Null,
        }
    }

    /// Connect over a PERSISTENT stdio session (N0022): one child process
    /// for the session's lifetime — stateful servers keep state, stateless
    /// ones pay startup once. Callers keep the client (or share it via
    /// `SharedMcpSession`) and `kill` it on teardown.
    pub fn stdio_persistent(server_name: &str, program: &str, args: &[String]) -> Result<McpClient, String> {
        let transport = PersistentTransport::spawn(program, args)?;
        Ok(McpClient {
            transport: Box::new(transport),
            server_name: server_name.to_string(),
            protocol_version: "2024-11-05".into(),
            server_info: Value::Null,
        })
    }

    pub fn in_process(
        server_name: &str,
        handler: Box<McpHandlerFn>,
    ) -> McpClient {
        McpClient {
            transport: Box::new(InProcessTransport { handler }),
            server_name: server_name.to_string(),
            protocol_version: "2024-11-05".into(),
            server_info: Value::Null,
        }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let response = self.transport.request(method, params)?;
        if let Some(err) = response.get("error") {
            return Err(format!("mcp error: {err}"));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// MCP `initialize` handshake.
    pub fn initialize(&mut self) -> Result<(), String> {
        let result = self.rpc(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
            }),
        )?;
        if let Some(v) = result.get("protocolVersion").and_then(|v| v.as_str()) {
            self.protocol_version = v.to_string();
        }
        self.server_info = result.get("serverInfo").cloned().unwrap_or(Value::Null);
        Ok(())
    }

    /// `tools/list` — the full descriptor set (deferred: served on demand).
    pub fn tools_list(&mut self) -> Result<Vec<McpToolDescriptor>, String> {
        let result = self.rpc("tools/list", serde_json::json!({}))?;
        let tools = result
            .get("tools")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        serde_json::from_value(tools).map_err(|e| format!("tools/list decode: {e}"))
    }

    /// `tools/call` — returns the text content + error flag.
    pub fn tools_call(&mut self, tool: &str, arguments: Value) -> Result<(String, bool), String> {
        let result = self.rpc(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": arguments }),
        )?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut text = String::new();
        if let Some(items) = result.get("content").and_then(|v| v.as_array()) {
            for item in items {
                if item.get("type").and_then(|v| v.as_str()) == Some("text")
                    && let Some(t) = item.get("text").and_then(|v| v.as_str())
                {
                    text.push_str(t);
                }
            }
        }
        Ok((text, is_error))
    }
}

// ---- the dispatch funnel ----

struct ServerEntry {
    client: Mutex<McpClient>,
    /// Deferred tool descriptors: NOT in the model prompt; served on demand.
    tools: Vec<McpToolDescriptor>,
}

/// The funnel: owns MCP server connections; builds the two model-facing
/// tools (`use_tool` + `tool_directory`) as erased registry tools.
/// Persistent stdio transport: one child process for the whole session
/// (N0022). The client holds the child; every request reuses it, so
/// stateful servers keep their state and stateless ones pay startup once.
pub struct PersistentTransport {
    child: Arc<PersistentChild>,
    timeout: Duration,
}

impl PersistentTransport {
    pub fn spawn(program: &str, args: &[String]) -> Result<PersistentTransport, String> {
        Ok(PersistentTransport {
            child: Arc::new(PersistentChild::spawn(program, args)?),
            timeout: Duration::from_secs(30),
        })
    }

    pub fn is_dead(&self) -> bool {
        self.child.is_dead()
    }

    pub fn kill(&self) {
        self.child.kill();
    }
}

impl Transport for PersistentTransport {
    /// Same contract as the one-shot transport: the FULL response envelope
    /// (rpc extracts result/error).
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.child.request(method, &params, self.timeout)
    }
}

/// A shared persistent client: one child per server, Arc'd across that
/// server's tools (the registry closure clones the Arc).
pub struct SharedMcpSession {
    pub client: Mutex<McpClient>,
}

pub struct McpFunnel {
    servers: Vec<(String, ServerEntry)>,
}

impl McpFunnel {
    pub fn new() -> Self {
        McpFunnel { servers: Vec::new() }
    }

    /// Attach an already-connected server; fetches its tool list once
    /// (deferred: kept server-side, not surfaced to the model).
    pub fn attach(&mut self, mut client: McpClient) -> Result<usize, String> {
        client.initialize()?;
        let tools = client.tools_list()?;
        let n = tools.len();
        self.servers
            .push((client.server_name.clone(), ServerEntry { client: Mutex::new(client), tools }));
        Ok(n)
    }

    pub fn server_names(&self) -> Vec<String> {
        self.servers.iter().map(|(n, _)| n.clone()).collect()
    }

    fn route(&self, server: &str, tool: &str, arguments: Value) -> Result<(String, bool), String> {
        let (_, entry) = self
            .servers
            .iter()
            .find(|(n, _)| n == server)
            .ok_or_else(|| format!("unknown mcp server `{server}`"))?;
        entry
            .tools
            .iter()
            .find(|t| t.name == tool)
            .ok_or_else(|| format!("server `{server}` has no tool `{tool}`"))?;
        let mut client = entry.client.lock().unwrap();
        client.tools_call(tool, arguments)
    }

    /// The directory payload: per server, tool names + schemas + docs.
    fn directory(&self) -> Value {
        let servers: Vec<Value> = self
            .servers
            .iter()
            .map(|(name, entry)| {
                serde_json::json!({
                    "server": name,
                    "tools": entry.tools.iter().map(|t| serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": t.input_schema,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::json!({ "servers": servers })
    }
}

impl Default for McpFunnel {
    fn default() -> Self {
        Self::new()
    }
}

/// The model-facing funnel tool: ONE tool for every MCP server.
pub struct UseToolFunnel {
    pub funnel: std::sync::Arc<McpFunnel>,
}

impl UseToolFunnel {
    pub fn entry(&self) -> crate::spec::ToolEntry {
        crate::spec::ToolEntry {
            spec: crate::spec::ToolSpec {
                name: "use_tool".into(),
                namespace: None,
                title: Some("MCP tool".into()),
                description: format!(
                    "Call a tool from an attached MCP server. Servers: {}. \
                     Use tool_directory to discover tools and schemas.",
                    self.funnel.server_names().join(", ")
                ),
                arguments_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "server": { "type": "string" },
                        "tool": { "type": "string" },
                        "arguments": { "type": "object" }
                    },
                    "required": ["server", "tool", "arguments"]
                })),
                kind: Some("mcp".into()),
                behavior_version: Some("1".into()),
                idempotent: false,
                read_only: false,
                timeout_ms: Some(30_000),
                max_concurrency: None,
            },
            metadata: crate::spec::ToolMetadata {
                read_only: false,
                needs_approval: true,
                side_effect_scope: crate::spec::SideEffectScope::External,
                risk_level: crate::spec::RiskLevel::Medium,
                ..Default::default()
            },
        }
    }

    pub fn accesses(&self) -> crate::scheduler::ToolAccesses {
        vec![crate::scheduler::ResourceAccess::All]
    }

    pub fn execute(&self, args: &Value) -> ToolStream {
        let server = args.get("server").and_then(|v| v.as_str()).unwrap_or_default();
        let tool = args.get("tool").and_then(|v| v.as_str()).unwrap_or_default();
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| Value::Null);
        if server.is_empty() || tool.is_empty() {
            return ToolStream::terminal_only(Err(ToolError::invalid_input(
                "server and tool are required",
            )));
        }
        match self.funnel.route(server, tool, arguments) {
            Ok((text, is_error)) => {
                if is_error {
                    ToolStream::terminal_only(Err(ToolError::tool_failed(text)))
                } else {
                    ToolStream::terminal_only(Ok(ToolOutput::text(text)))
                }
            }
            Err(e) => ToolStream::terminal_only(Err(ToolError::tool_failed(e))),
        }
    }
}

/// `tool_directory` — deferred schema discovery (qwen tool_search analog).
pub struct ToolDirectory {
    pub funnel: std::sync::Arc<McpFunnel>,
}

impl ToolDirectory {
    pub fn entry(&self) -> crate::spec::ToolEntry {
        crate::spec::ToolEntry {
            spec: crate::spec::ToolSpec {
                name: "tool_directory".into(),
                namespace: None,
                title: Some("Tool directory".into()),
                description:
                    "List attached MCP servers with their tools and full JSON schemas.".into(),
                arguments_schema: Some(serde_json::json!({ "type": "object" })),
                kind: Some("mcp".into()),
                behavior_version: Some("1".into()),
                idempotent: true,
                read_only: true,
                timeout_ms: Some(10_000),
                max_concurrency: None,
            },
            metadata: crate::spec::ToolMetadata {
                read_only: true,
                concurrent_safe: true,
                allowed_in_plan_mode: Some(true),
                ..Default::default()
            },
        }
    }

    pub fn accesses(&self) -> crate::scheduler::ToolAccesses {
        vec![]
    }

    pub fn execute(&self, _args: &Value) -> ToolStream {
        ToolStream::terminal_only(Ok(ToolOutput::from_value(self.funnel.directory())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mock_handler(method: &str, params: Value) -> Result<Value, String> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "mock-mcp" }
            })),
            "tools/list" => Ok(json!({
                "tools": [
                    { "name": "mcp_echo", "description": "echo text",
                      "inputSchema": { "type": "object" } },
                    { "name": "mcp_len", "description": "text length",
                      "inputSchema": { "type": "object" } }
                ]
            })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let args = params["arguments"].clone();
                let text = args["text"].as_str().unwrap_or_default();
                match name {
                    "mcp_echo" => Ok(json!({
                        "content": [{ "type": "text", "text": format!("mcp-echo: {text}") }]
                    })),
                    "mcp_len" => Ok(json!({
                        "content": [{ "type": "text", "text": text.chars().count().to_string() }]
                    })),
                    _ => Ok(json!({ "content": [], "isError": true })),
                }
            }
            _ => Err(format!("unknown method {method}")),
        }
    }

    #[test]
    fn mcp_client_roundtrip_over_in_process_transport() {
        let mut client = McpClient::in_process("mock", Box::new(mock_handler));
        client.initialize().unwrap();
        assert_eq!(client.protocol_version, "2024-11-05");

        let tools = client.tools_list().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "mcp_echo");

        let (text, is_error) = client.tools_call("mcp_echo", json!({ "text": "okra" })).unwrap();
        assert!(!is_error);
        assert_eq!(text, "mcp-echo: okra");
    }

    #[test]
    fn funnel_routes_use_tool_and_serves_deferred_directory() {
        let mut funnel = McpFunnel::new();
        let client = McpClient::in_process("mock", Box::new(mock_handler));
        let attached = funnel.attach(client).unwrap();
        assert_eq!(attached, 2, "two deferred tools registered");
        let funnel = std::sync::Arc::new(funnel);
        let directory = ToolDirectory { funnel: std::sync::Arc::clone(&funnel) };

        // deferred schemas: served by the directory tool, not the prompt
        let dir = directory.execute(&json!({}));
        let out = dir.terminal().unwrap().as_ref().unwrap();
        let text = format!("{:?}", out.model_output);
        assert!(text.contains("mcp_echo") && text.contains("inputSchema"), "{text}");

        // single dispatch funnel: use_tool routes server+tool+arguments
        let funnel_tool = UseToolFunnel { funnel: std::sync::Arc::clone(&funnel) };
        let stream = funnel_tool.execute(&json!({
            "server": "mock", "tool": "mcp_echo", "arguments": { "text": "hello" }
        }));
        let out = stream.terminal().unwrap().as_ref().unwrap();
        assert_eq!(out.value, "mcp-echo: hello");

        // unknown server/tool are typed errors, not panics
        let stream = funnel_tool.execute(&json!({
            "server": "nope", "tool": "mcp_echo", "arguments": {}
        }));
        assert!(stream.terminal().unwrap().is_err());
        let stream = funnel_tool.execute(&json!({
            "server": "mock", "tool": "nope", "arguments": {}
        }));
        assert!(stream.terminal().unwrap().is_err());
    }

    #[test]
    fn stdio_transport_speaks_json_rpc_with_a_real_subprocess() {
        let mut client = McpClient::stdio(
            "sh-echo",
            "sh",
            &[
                "-c".into(),
                "read line; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"protocolVersion\":\"2024-11-05\"}}'".into(),
            ],
        );
        client.initialize().unwrap();
        assert_eq!(client.protocol_version, "2024-11-05");
    }
}
