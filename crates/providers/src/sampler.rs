//! Sampler — port of grok's sampler shape (3 wire APIs behind one trait) +
//! retry/doom-loop discipline + the scripted model stub (MASTER-PLAN §3
//! #63: `delayMs`, `truncateStreamAt` fault injection for tests).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;

use crate::messages::{Message, ToolCall};

/// Tool descriptors sent to the model (from tools::ToolEntry projections).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolView {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_schema: Option<Value>,
}

/// One sampling request.
#[derive(Debug, Clone)]
pub struct SampleRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolView>,
    pub max_tokens: Option<u32>,
    /// When set, the model must produce JSON matching the schema.
    pub structured_output_schema: Option<Value>,
}

/// Stop reason, 1:1 with grok `CompletedStop` (+ tool use).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Refusal,
}

/// The assistant turn's response.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    /// Token usage for spend backstops.
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SamplerError {
    /// HTTP 401: the turn parks uncharged (grok auth_retry).
    #[error("unauthorized (401)")]
    Unauthorized,
    /// HTTP 429: rate-limit park decision goes to the governor.
    #[error("rate limited (429), retry after {retry_after_secs:?}s")]
    RateLimited { retry_after_secs: Option<u64> },
    /// Context length exceeded: compaction path.
    #[error("context length exceeded")]
    ContextLength,
    /// Transient: bounded retry.
    #[error("transient failure: {0}")]
    Transient(String),
    /// Permanent: turn fails.
    #[error("permanent failure: {0}")]
    Permanent(String),
}

/// The sampler seam — grok runs three wire APIs behind one trait; okra's M0
/// implementation is the scripted stub plus this contract for real
/// providers at M1+.
pub trait Sampler: Send + Sync {
    fn sample(&self, request: &SampleRequest) -> Result<SampleResponse, SamplerError>;
    /// Streaming: text deltas then a final response. Default wraps `sample`.
    fn sample_stream(
        &self,
        request: &SampleRequest,
    ) -> Result<(TextStream, SampleResponse), SamplerError> {
        let resp = self.sample(request)?;
        Ok((TextStream::completed(&resp.text), resp))
    }
    /// #37: drain model-fallback switches recorded since the last call.
    /// Default: none (a plain sampler never switches). The turn loop logs
    /// these and surfaces show the switch — a model change is never silent.
    fn drain_fallback_events(&self) -> Vec<crate::fallback::FallbackEvent> {
        Vec::new()
    }
}

/// Text deltas (already-produced form; the sync runtime replays them).
#[derive(Debug, Clone)]
pub struct TextStream {
    deltas: Vec<String>,
    next: usize,
}

impl TextStream {
    pub fn completed(text: &str) -> Self {
        // one delta per ~64-char chunk to exercise streaming paths
        let bytes = text.as_bytes();
        let mut deltas = Vec::new();
        let mut i = 0usize;
        while i < bytes.len() {
            let mut j = (i + 64).min(bytes.len());
            while j < bytes.len() && (bytes[j] & 0xC0) == 0x80 {
                j += 1;
            }
            deltas.push(String::from_utf8_lossy(&bytes[i..j]).into_owned());
            i = j;
        }
        TextStream { deltas, next: 0 }
    }

    pub fn from_deltas(deltas: Vec<String>) -> Self {
        TextStream { deltas, next: 0 }
    }

    pub fn next_delta(&mut self) -> Option<String> {
        if self.next < self.deltas.len() {
            let d = self.deltas[self.next].clone();
            self.next += 1;
            Some(d)
        } else {
            None
        }
    }
}

/// Scripted model stub (`llm-replay`/`llm-mock-server` analog): each step
/// plays one scripted response; `delay_ms` throttles; `truncate_at` forces
/// `MaxTokens` mid-text (length-salvage testing).
#[derive(Default)]
pub struct ScriptedModel {
    steps: Vec<ScriptedStep>,
    call_count: Mutex<usize>,
    pub requests: Mutex<Vec<SampleRequest>>,
}

#[derive(Clone, Default)]
pub struct ScriptedStep {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: Option<StopReason>,
    /// Fault injection: pretend a truncation at N chars (length salvage).
    pub truncate_at: Option<usize>,
    pub delay_ms: u64,
    pub error: Option<SamplerError>,
    pub usage: Usage,
}

impl ScriptedModel {
    pub fn new(steps: Vec<ScriptedStep>) -> Self {
        ScriptedModel { steps, call_count: Mutex::new(0), requests: Mutex::new(Vec::new()) }
    }

    fn step_for(&self, n: usize) -> ScriptedStep {
        self.steps
            .get(n)
            .cloned()
            .unwrap_or_else(|| ScriptedStep {
                text: String::new(),
                tool_calls: vec![],
                stop_reason: Some(StopReason::EndTurn),
                ..Default::default()
            })
    }
}

impl Sampler for ScriptedModel {
    fn sample(&self, request: &SampleRequest) -> Result<SampleResponse, SamplerError> {
        let mut count = self.call_count.lock().unwrap();
        let n = *count;
        *count += 1;
        drop(count);
        self.requests.lock().unwrap().push(request.clone());
        let step = self.step_for(n);
        if step.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(step.delay_ms));
        }
        if let Some(err) = step.error {
            return Err(err);
        }
        let (text, stop) = match step.truncate_at {
            Some(at) if at < step.text.len() => {
                (step.text[..at].to_string(), StopReason::MaxTokens)
            }
            _ => (
                step.text.clone(),
                step.stop_reason.unwrap_or(if step.tool_calls.is_empty() {
                    StopReason::EndTurn
                } else {
                    StopReason::ToolUse
                }),
            ),
        };
        Ok(SampleResponse {
            text,
            tool_calls: step.tool_calls,
            stop_reason: stop,
            usage: step.usage,
        })
    }
}

/// Doom-loop discipline (grok turn loop): bounded identical-response retry.
/// After `max_identical` consecutive identical (text + tool calls + stop)
/// responses, the caller must stop rather than spin.
#[derive(Debug, Clone)]
pub struct DoomLoopGuard {
    max_identical: usize,
    last_signature: Option<u64>,
    run_len: usize,
}

impl DoomLoopGuard {
    pub fn new(max_identical: usize) -> Self {
        DoomLoopGuard { max_identical, last_signature: None, run_len: 0 }
    }

    pub fn observe(&mut self, resp: &SampleResponse) -> bool {
        // FNV-1a over the response signature
        let mut h: u64 = 0xcbf29ce484222325;
        let feed = |bytes: &[u8], h: &mut u64| {
            for b in bytes {
                *h ^= *b as u64;
                *h = h.wrapping_mul(0x100000001b3);
            }
        };
        feed(resp.text.as_bytes(), &mut h);
        for c in &resp.tool_calls {
            feed(c.id.as_bytes(), &mut h);
            feed(c.name.as_bytes(), &mut h);
            feed(c.args_json.as_bytes(), &mut h);
        }
        feed(format!("{:?}", resp.stop_reason).as_bytes(), &mut h);
        let sig = h;
        self.run_len = if self.last_signature == Some(sig) { self.run_len + 1 } else { 1 };
        self.last_signature = Some(sig);
        self.run_len < self.max_identical
    }

    pub fn run_len(&self) -> usize {
        self.run_len
    }
}

/// Transient retry budget (grok AuthRetrySchedule analog, simplified):
/// bounded attempts with 1s/2s/4s backoff for Transient errors only.
#[derive(Debug, Clone, Copy)]
pub struct RetryBudget {
    pub max_attempts: u32,
    pub attempt: u32,
}

impl RetryBudget {
    pub fn new(max_attempts: u32) -> Self {
        RetryBudget { max_attempts, attempt: 0 }
    }

    /// Returns the backoff delay in seconds if a retry remains.
    pub fn on_error(&mut self, err: &SamplerError) -> Option<u64> {
        if !matches!(err, SamplerError::Transient(_)) {
            return None;
        }
        if self.attempt >= self.max_attempts {
            return None;
        }
        let delay = 1u64 << self.attempt; // 1, 2, 4
        self.attempt += 1;
        Some(delay)
    }
}
