//! Offline demo sampler — the M0 acceptance shape: an agent that really
//! calls `read_file` (or `list_dir`) through the policy pipeline and answers
//! from the result, with no network. Replaced by real providers at M1.

use okra_providers::{Message, SampleRequest, SampleResponse, Sampler, SamplerError, StopReason, ToolCall, Usage};
use std::sync::Mutex;

/// A deterministic two-step planner:
/// step 0: find a file named in the prompt (or fall back to list_dir) and
///         emit the tool call;
/// step 1: summarize the tool result and end the turn.
pub struct DemoPlanner {
    turn: Mutex<usize>,
    cwd: std::path::PathBuf,
    /// Scripted-stub `delayMs` (MASTER-PLAN block #63): per-sample pause so
    /// harnesses have a window to steer mid-turn. `OKRA_DEMO_DELAY_MS`, 0 by
    /// default.
    delay_ms: u64,
}

impl DemoPlanner {
    pub fn new(cwd: std::path::PathBuf) -> Self {
        let delay_ms = std::env::var("OKRA_DEMO_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        DemoPlanner { turn: Mutex::new(0), cwd, delay_ms }
    }

    /// "create <file>" / "write <file>" style prompts → the target file
    /// name (offline-approvable: write_file is side-effecting, so the turn
    /// pauses on the approval bridge).
    fn find_create_target(&self, prompt: &str) -> Option<String> {
        let lower = prompt.to_lowercase();
        let verb_pos = ["create ", "write ", "make "]
            .iter()
            .find_map(|v| lower.find(v).map(|i| i + v.len()))
            .or_else(|| {
                lower
                    .strip_suffix(" file")
                    .and_then(|_| lower.find("new ").map(|i| i + 4))
            })?;
        let rest = &prompt[verb_pos.min(prompt.len())..];
        let token = rest
            .split_whitespace()
            .find(|t| t.contains('.'))
            .map(|t| t.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '_' && c != '/').to_string())
            .or_else(|| {
                rest.split_whitespace()
                    .next()
                    .map(|t| format!("{t}.txt"))
            })?;
        let token = token.trim().to_string();
        if token.is_empty() {
            None
        } else {
            Some(token)
        }
    }

    fn find_named_file(&self, prompt: &str) -> Option<String> {
        // tokens that look like file names
        for token in prompt.split_whitespace() {
            let clean = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-' && c != '_' && c != '/');
            if clean.is_empty() {
                continue;
            }
            let candidate = self.cwd.join(clean);
            if candidate.is_file() {
                return Some(clean.to_string());
            }
            // also allow a bare stem match ("read hello" -> hello.txt)
            if let Some(name) = self.cwd.read_dir().ok().and_then(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .find(|n| {
                        let stem = n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n);
                        stem == clean || n.split('.').next() == Some(clean)
                    })
            }) {
                return Some(name);
            }
        }
        None
    }
}

fn last_tool_output(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| {
            if m.role != okra_providers::Role::Tool {
                return None;
            }
            m.content.iter().find_map(|b| match b {
                okra_providers::ContentBlock::ToolResponse { result } => {
                    Some(result.content.clone())
                }
                _ => None,
            })
        })
        .unwrap_or_default()
}

impl Sampler for DemoPlanner {
    fn sample(&self, request: &SampleRequest) -> Result<SampleResponse, SamplerError> {
        if self.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
        }
        let mut turn = self.turn.lock().unwrap();
        *turn += 1;
        let n = *turn;
        let prompt = request
            .messages
            .iter()
            .find(|m| m.role == okra_providers::Role::User)
            .map(|m| m.text_content())
            .unwrap_or_default();

        if n == 1 {
            // plan: create the named file (write path — drives the approval
            // bridge offline), or read the named file, or list the directory
            let call = if let Some(name) = self.find_create_target(&prompt) {
                ToolCall {
                    id: "demo-call-1".into(),
                    name: "write_file".into(),
                    args_json: serde_json::json!({
                        "path": name,
                        "content": format!("# {name}\nCreated by the okra demo planner.\n"),
                    })
                    .to_string(),
                }
            } else {
                match self.find_named_file(&prompt) {
                    Some(path) => ToolCall {
                        id: "demo-call-1".into(),
                        name: "read_file".into(),
                        args_json: serde_json::json!({ "path": path }).to_string(),
                    },
                    None => ToolCall {
                        id: "demo-call-1".into(),
                        name: "list_dir".into(),
                        args_json: serde_json::json!({ "path": "." }).to_string(),
                    },
                }
            };
            return Ok(SampleResponse {
                text: if call.name == "write_file" {
                    "I will create that file now.".into()
                } else {
                    "Let me look at the workspace first.".into()
                },
                tool_calls: vec![call],
                stop_reason: StopReason::ToolUse,
                usage: Usage { input_tokens: 24, output_tokens: 12 },
            });
        }

        // step 2: summarize the last tool output
        let tool_output = last_tool_output(&request.messages);
        let summary = if tool_output.is_empty() {
            "I could not inspect the workspace (no tool result reached me).".to_string()
        } else {
            let excerpt: String = tool_output.chars().take(400).collect();
            format!("Here is what I found:\n\n{excerpt}")
        };
        Ok(SampleResponse {
            text: summary,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage { input_tokens: 180, output_tokens: 60 },
        })
    }
}
