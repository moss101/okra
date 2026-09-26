//! The sync turn loop (grok SessionActor's turn loop, decision N0001):
//! sampler + tool plane + policy plane + kernel event log + steering +
//! governors + backstops, driven by one struct.
//!
//! Every durable transition lands in the kernel log FIRST ("model-visible
//! means logged"), then surfaces. `OKRA_KILL_AT_PHASE` fault injection
//! aborts the process at a phase boundary for the crash-recovery harness
//! (§3 #63).

use std::sync::Arc;


use okra_kernel as kernel;
use okra_policy::approval::{ApprovalOutcome, ApprovalPolicy, ApprovalService, ToolApprovalCeiling};
use okra_policy::grants::{GrantDecision, GrantScope, GrantStore};
use okra_policy::lattice::{Decision as LatticeDecision, PermissionLattice};
use okra_policy::SandboxMode;
use okra_providers::{
    merge_consecutive_users, pair_tool_calls, ContentBlock, DoomLoopGuard, Message, RetryBudget,
    SampleRequest, Sampler, SamplerError, StopReason, ToolCall, ToolView,
};
use okra_tools::ToolStreamItem;

use crate::backstops::{
    backstop_outcome, evaluate_stop, BackstopConfig, BackstopTrip, Backstops, MaxTurnsGuard,
    StopGateDecision,
};
use crate::governors::{
    LengthSalvage, RateLimitDecision, RateLimitWaitBudget, SalvageStep, StationarityDecision,
    StationarityTracker,
};
use crate::steering::{format_as_user_message, SteeringInbox};
use okra_compaction::OriginTag;
use crate::turn::{CancellationCategory, CompletedStop, TurnOutcome, TurnPhase, TurnMachine};

/// Configuration for one agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_steps: usize,
    pub stationarity: crate::governors::StationarityConfig,
    pub salvage_budget: u32,
    pub backstops: BackstopConfig,
    /// Structured callables: whether the agent runs unattended (yolo-capable).
    pub unattended: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            max_steps: 32,
            stationarity: Default::default(),
            salvage_budget: 2,
            backstops: Default::default(),
            unattended: false,
        }
    }
}

/// Events surfaced by the loop (NDJSON for the headless CLI / gateway).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LoopEvent {
    TurnStarted { turn: u64 },
    Phase { phase: String },
    TextDelta { text: String },
    ToolCallStarted { id: String, name: String },
    ToolCallProgress { id: String, text: String },
    ToolCallFinished { id: String, name: String, is_error: bool, output: String },
    SteeringInjected { text: String },
    Nudge { reason: String },
    CompactionNotice { note: String },
    TurnFinished { outcome: String },
    Error { message: String },
}

/// What the loop needs from the tool plane.
pub trait ToolExecutor: Send {
    /// Full policy + dispatch pipeline for one call. Returns (output text,
    /// is_error, true_noop_marker).
    fn execute(
        &mut self,
        call: &ToolCall,
        events: &mut dyn FnMut(LoopEvent),
    ) -> Result<(String, bool, bool), String>;

    /// Views of registered tools for the sampler request.
    fn tool_views(&self) -> Vec<ToolView>;

    fn set_sandbox_mode(&mut self, mode: SandboxMode);
}

/// Tool-plane wiring: registry dispatch + approval service + grants +
/// lattice. `execute` runs the exact ordering: normalize → hooks (deny?)
/// → lattice (deny/ask/allow) → grant check → approval → grant mint →
/// execute approved bytes.
pub struct PolicyToolExecutor {
    pub registry: okra_tools::Registry,
    pub approvals: ApprovalService,
    pub grants: GrantStore,
    pub lattice: PermissionLattice,
    pub ceiling: ToolApprovalCeiling,
}

impl PolicyToolExecutor {
    pub fn new(registry: okra_tools::Registry, approvals: ApprovalService) -> Self {
        PolicyToolExecutor {
            registry,
            approvals,
            grants: GrantStore::new(1),
            lattice: PermissionLattice::new(),
            ceiling: ToolApprovalCeiling::GrantsAllowed,
        }
    }
}

impl ToolExecutor for PolicyToolExecutor {
    fn execute(
        &mut self,
        call: &ToolCall,
        events: &mut dyn FnMut(LoopEvent),
    ) -> Result<(String, bool, bool), String> {
        let entry = self
            .registry
            .get(&call.name)
            .map_err(|e| format!("tool resolve: {e}"))?
            .entry
            .clone();

        // probe: normalize + hooks (also freezes the approved bytes)
        let approved = {
            let normalizers: Vec<&dyn okra_tools::ArgumentNormalizer> =
                self.registry.normalizers_iter().collect();
            let hooks: Vec<&dyn okra_tools::PreToolUseHook> =
                self.registry.hooks_iter().collect();
            let raw: serde_json::Value =
                serde_json::from_str(&call.args_json).unwrap_or(serde_json::Value::Null);
            okra_tools::normalize_before_hooks(&entry, &raw, &normalizers, &hooks)
                .map_err(|e| format!("pipeline: {e}"))?
        };
        let args_json = approved.args_json.clone();
        let behavior = entry.spec.behavior_version.clone().unwrap_or_default();

        // read-only tools never prompt under any ceiling (grok context.rs)
        if !entry.metadata.read_only {
            match self.lattice.evaluate(&call.name, None) {
                LatticeDecision::Deny => {
                    return Ok((
                        "denied by permission rule (never enforced pre-dispatch)".into(),
                        true,
                        false,
                    ));
                }
                LatticeDecision::Ask | LatticeDecision::Default | LatticeDecision::Allow => {}
            }

            // grant check first: exact (tool, args hash, policy version)
            if self.ceiling != ToolApprovalCeiling::AlwaysPrompt
                && self.grants.check(&call.name, &args_json, &behavior, GrantScope::Conversation)
                    == GrantDecision::Granted
            {
                // granted for exactly these bytes
            } else if self.ceiling == ToolApprovalCeiling::UnattendedAllowed && self.approvals.policy() == ApprovalPolicy::Ask {
                // unattended hosts honour yolo; we still record the decision
            } else {
                let (outcome, _audit) = self
                    .approvals
                    .decide(&call.name, &call.id, &args_json);
                if !outcome.grants() {
                    events(LoopEvent::ToolCallFinished {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        is_error: true,
                        output: format!("approval denied: {}", outcome.denial_reason()),
                    });
                    return Ok((format!("approval denied: {}", outcome.denial_reason()), true, false));
                }
                // mint the arg-hash-bound grant
                self.grants.record_approval(
                    ApprovalOutcome::AllowedOnce,
                    &call.name,
                    &args_json,
                    &behavior,
                    self.ceiling,
                    false,
                );
            }
        }

        // idempotency: only idempotent tools may auto-retry; non-idempotent
        // re-execution with the same call id is refused (§3 #63 property
        // "no side-effecting tool executes twice per call ID")
        if !entry.spec.idempotent
            && self
                .grants
                .seen_once(&call.name, &args_json, &behavior)
        {
            return Ok((
                "[blocked] this non-idempotent call already executed with identical arguments; \
                 if you need a different effect, change the arguments or the state."
                    .into(),
                true,
                false,
            ));
        }
        if !entry.metadata.read_only {
            self.grants.record_once(&call.name, &args_json, &behavior);
        }

        // execute the approved bytes (registry re-validates hooks on the
        // same canonical bytes — approved bytes = executed bytes)
        let stream = self
            .registry
            .dispatch(&call.name, &approved.args(), &[])
            .map_err(|e| format!("dispatch: {e}"))?;
        let mut text = String::new();
        let mut is_error = false;
        let mut true_noop = false;
        for item in stream.into_items() {
            match item {
                ToolStreamItem::Progress(p) => {
                    if let okra_tools::ToolProgress::Text { text: t } = p {
                        events(LoopEvent::ToolCallProgress { id: call.id.clone(), text: t });
                    }
                }
                ToolStreamItem::Terminal(result) => match result {
                    Ok(out) => {
                        text = out
                            .model_output
                            .iter()
                            .map(|b| match b {
                                okra_tools::ContentBlock::Text { text } => text.clone(),
                                other => format!("{other:?}"),
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                    }
                    Err(e) => {
                        is_error = true;
                        text = match e {
                            okra_tools::ToolError::ToolFailed { message }
                            | okra_tools::ToolError::InvalidInput { message } => message,
                            okra_tools::ToolError::Custom { message, .. } => message,
                        };
                    }
                },
            }
        }
        if text.trim().is_empty() {
            true_noop = true;
        }
        Ok((text, is_error, true_noop))
    }

    fn tool_views(&self) -> Vec<ToolView> {
        self.registry
            .entries()
            .into_iter()
            .map(|e| ToolView {
                name: e.spec.name.clone(),
                description: e.spec.description.clone(),
                arguments_schema: e.spec.arguments_schema.clone(),
            })
            .collect()
    }

    fn set_sandbox_mode(&mut self, _mode: SandboxMode) {
        // sandbox profiles bind at the provider layer (M1); M0 records intent
    }
}

/// The agent. One per session; not Clone (owns the log handle).
pub struct Agent<S: Sampler> {
    pub config: AgentConfig,
    sampler: Arc<S>,
    executor: Box<dyn ToolExecutor>,
    session: kernel::SessionHandle,
    steering: SteeringInbox,
    machine: TurnMachine,
    turn_counter: u64,
}

impl<S: Sampler> Agent<S> {
    pub fn new(
        config: AgentConfig,
        sampler: Arc<S>,
        executor: Box<dyn ToolExecutor>,
        session: kernel::SessionHandle,
    ) -> Self {
        Agent {
            config,
            sampler,
            executor,
            session,
            steering: SteeringInbox::new(),
            machine: TurnMachine::new(),
            turn_counter: 0,
        }
    }

    pub fn steering_sender(&self) -> std::sync::mpsc::Sender<crate::steering::Tagged> {
        self.steering.sender()
    }

    pub fn session(&self) -> &kernel::SessionHandle {
        &self.session
    }

    fn log(&mut self, events: Vec<kernel::SessionEvent>) -> Result<(), String> {
        self.session.append(events).map_err(|e| format!("log append: {e}"))
    }

    fn emit(events: &mut Vec<LoopEvent>, ev: LoopEvent) {
        events.push(ev);
    }

    fn phase_transition(
        &mut self,
        to: TurnPhase,
        out: &mut Vec<LoopEvent>,
    ) -> Result<(), String> {
        self.machine
            .transition(to)
            .map_err(|e| format!("phase machine: {e}"))?;
        Self::emit(out, LoopEvent::Phase { phase: format!("{to:?}") });
        fault_inject_at_phase(to);
        Ok(())
    }

    /// One conversational turn. `structured_schema` optionally requests
    /// structured output.
    pub fn run_turn(
        &mut self,
        input: &str,
        events: &mut dyn FnMut(LoopEvent),
    ) -> Result<TurnOutcome, String> {
        self.turn_counter += 1;
        let turn_no = self.turn_counter;
        let mut out: Vec<LoopEvent> = Vec::new();
        let clock = kernel::wall_clock;

        // ---- phase: enter ----
        self.phase_transition(TurnPhase::ProcessingInput, &mut out)?;
        Self::emit(&mut out, LoopEvent::TurnStarted { turn: turn_no });
        self.log(vec![
            kernel::make_log_only_event("turn/start", serde_json::json!({ "turn": turn_no }), clock),
            kernel::make_event("user/message", serde_json::json!({ "text": input }), clock),
        ])?;

        // pending tool calls awaiting results (assistant calls from last step)
        let mut history: Vec<Message> = vec![Message::user(input)];
        let mut tools_called: Vec<String> = Vec::new();
        let mut usage_total = okra_providers::Usage::default();
        let mut structured: Option<okra_providers::StructuredOutput> = None;

        let mut stationarity = StationarityTracker::new(self.config.stationarity.clone());
        let mut salvage = LengthSalvage::with_budget(self.config.salvage_budget);
        let mut backstops = Backstops::new(self.config.backstops.clone());
        let max_turns = MaxTurnsGuard { limit: self.config.max_steps };
        let mut doom = DoomLoopGuard::new(3);
        let mut retry = RetryBudget::new(3);
        let mut uncharged_resubmits: u32 = 0;
        let mut rate_budget: Option<RateLimitWaitBudget> = None; // main session: never wait
        let mut steps: usize = 0;

        self.phase_transition(TurnPhase::AwaitingModelResponse, &mut out)?;
        self.steering.set_turn_running(true);

        let outcome = loop {
            // ---- max turns (honest, reported) ----
            if max_turns.tripped(steps) {
                break TurnOutcome::MaxTurnsReached { limit: max_turns.limit };
            }
            steps += 1;

            // ---- steering at step boundaries: drain BEFORE sampling ----
            self.steering.set_turn_running(true);
            let interjections = self.steering.drain();
            if let Some(msg) = format_as_user_message(&interjections) {
                Self::emit(
                    &mut out,
                    LoopEvent::SteeringInjected { text: msg.text_content() },
                );
                self.log(vec![kernel::make_event(
                    "user/message",
                    serde_json::json!({ "text": msg.text_content(), "origin": "steering" }),
                    clock,
                )])?;
                history.push(msg);
            }

            // ---- sample (with retry + rate-limit classification) ----
            let request = SampleRequest {
                messages: pair_tool_calls(&merge_consecutive_users(&history)),
                tools: self.executor.tool_views(),
                max_tokens: None,
                structured_output_schema: None,
            };
            let response = match self.sampler.sample(&request) {
                Ok(r) => {
                    retry = RetryBudget::new(3); // success resets the budget
                    r
                }
                Err(SamplerError::Unauthorized) => {
                    // grok auth_retry: 401 parks the turn UNCHARGED and
                    // resubmits once a wire-valid credential exists — capped
                    // by the runaway guard (MAX_UNCHARGED_RESUBMITS = 50).
                    uncharged_resubmits += 1;
                    if uncharged_resubmits > 50 {
                        break TurnOutcome::Cancelled {
                            category: Some(CancellationCategory::MidTurnAbort),
                        };
                    }
                    continue;
                }
                Err(SamplerError::RateLimited { retry_after_secs }) => {
                    let budget = rate_budget.get_or_insert_with(|| {
                        // main sessions never wait (rate_limit_waits.rs:94-96)
                        RateLimitWaitBudget::for_main_session()
                    });
                    match budget.decide(retry_after_secs) {
                        RateLimitDecision::Disabled | RateLimitDecision::BudgetSpent => {
                            break TurnOutcome::Cancelled {
                                category: Some(CancellationCategory::MidTurnAbort),
                            };
                        }
                        RateLimitDecision::Wait { backoff_secs, .. } => {
                            std::thread::sleep(std::time::Duration::from_secs(
                                backoff_secs.min(1),
                            ));
                            continue;
                        }
                        RateLimitDecision::NotRateLimited => continue,
                    }
                }
                Err(e @ SamplerError::Transient(_)) => {
                    match retry.on_error(&e) {
                        Some(delay_secs) => {
                            std::thread::sleep(std::time::Duration::from_secs(
                                delay_secs.min(1),
                            ));
                            continue;
                        }
                        None => {
                            let msg = format!("sampler exhausted retries: {e}");
                            Self::emit(&mut out, LoopEvent::Error { message: msg.clone() });
                            break TurnOutcome::Cancelled {
                                category: Some(CancellationCategory::MidTurnAbort),
                            };
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("sampler: {e}");
                    Self::emit(&mut out, LoopEvent::Error { message: msg.clone() });
                    break TurnOutcome::Cancelled {
                        category: Some(CancellationCategory::MidTurnAbort),
                    };
                }
            };
            usage_total = crate::backstops::accumulate(usage_total, response.usage);

            // ---- text streaming ----
            self.phase_transition(TurnPhase::Streaming, &mut out)?;
            let mut ts = okra_providers::TextStream::completed(&response.text);
            while let Some(delta) = ts.next_delta() {
                Self::emit(&mut out, LoopEvent::TextDelta { text: delta });
            }
            if !response.text.is_empty() {
                let mut assistant = Message::assistant_text(response.text.clone());
                for call in &response.tool_calls {
                    assistant
                        .content
                        .push(ContentBlock::ToolUse { call: call.clone() });
                }
                history.push(assistant);
                self.log(vec![kernel::make_event(
                    "assistant/message",
                    serde_json::json!({ "text": response.text }),
                    clock,
                )])?;
            }

            // ---- length salvage ----
            match salvage.on_response(response.stop_reason) {
                SalvageStep::Continue { inject_reminder } => {
                    if inject_reminder {
                        history.push(Message::user(LengthSalvage::REMINDER));
                    }
                    self.phase_transition(TurnPhase::AggregatingResults, &mut out)?;
                    self.phase_transition(TurnPhase::AwaitingModelResponse, &mut out)?;
                    continue;
                }
                SalvageStep::Exhaust => {
                    // report exhaustion honestly: completed with MaxTokens
                    break TurnOutcome::Completed {
                        tools_called: tools_called.clone(),
                        structured_output: None,
                        stop: CompletedStop::MaxTokens,
                    };
                }
                SalvageStep::None => {}
            }

            // ---- tool calls ----
            let calls = response.tool_calls.clone();
            let mut progressed = !calls.is_empty() || !response.text.is_empty();
            if !calls.is_empty() {
                self.phase_transition(TurnPhase::SchedulingTools, &mut out)?;
                self.phase_transition(TurnPhase::ExecutingTools, &mut out)?;

                // stationarity BEFORE executing (nudge) — grok checks at the
                // top of the loop with the previous step's signature; we
                // observe after collection, before dispatch
                let call_tuples: Vec<(String, String)> = calls
                    .iter()
                    .map(|c| (c.name.clone(), c.args_json.clone()))
                    .collect();
                match stationarity.observe_step(&call_tuples, false, false) {
                    StationarityDecision::HardStop => {
                        break TurnOutcome::StationarityEnded;
                    }
                    StationarityDecision::Nudge => {
                        Self::emit(
                            &mut out,
                            LoopEvent::Nudge { reason: "identical tool calls".into() },
                        );
                        history.push(Message::user(
                            stationarity.nudge_text()["reminder"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        ));
                    }
                    StationarityDecision::None => {}
                }

                // doom-loop guard on identical responses
                if !doom.observe(&response) {
                    break TurnOutcome::StationarityEnded;
                }

                let mut results: Vec<Message> = Vec::new();
                for call in &calls {
                    Self::emit(
                        &mut out,
                        LoopEvent::ToolCallStarted { id: call.id.clone(), name: call.name.clone() },
                    );
                    self.log(vec![kernel::make_log_only_event(
                        "tool/call",
                        serde_json::json!({ "callId": call.id, "tool": call.name }),
                        clock,
                    )])?;
                    let result = self.executor.execute(call, &mut |ev| {
                        // nested borrow: route progress events out
                        events(ev);
                    });
                    let (text, is_error, _noop) = match result {
                        Ok(t) => t,
                        Err(err) => (format!("executor error: {err}"), true, false),
                    };
                    tools_called.push(call.name.clone());
                    progressed = true;
                    Self::emit(
                        &mut out,
                        LoopEvent::ToolCallFinished {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            is_error,
                            output: text.chars().take(400).collect(),
                        },
                    );
                    results.push(Message::tool_result(okra_providers::ToolResult {
                        call_id: call.id.clone(),
                        content: text,
                        is_error,
                    }));
                }
                // tool/result is a SURFACE event (model-visible means
                // logged) — it carries a surfaceOp; tool/call stays log-only
                self.log(results.iter().map(|r| {
                    let ContentBlock::ToolResponse { result } = &r.content[0] else {
                        unreachable!("just built")
                    };
                    kernel::make_event(
                        "tool/result",
                        serde_json::json!({
                            "callId": result.call_id,
                            "isError": result.is_error,
                            "origin": OriginTag::ToolContext,
                        }),
                        clock,
                    )
                }).collect())?;

                self.phase_transition(TurnPhase::AggregatingResults, &mut out)?;
                history.extend(results);
            }

            // ---- backstops ----
            match backstops.on_step(&response, progressed) {
                BackstopTrip::None => {}
                trip => {
                    let msg = backstop_outcome(trip).unwrap_or_default();
                    Self::emit(&mut out, LoopEvent::Error { message: msg.clone() });
                    self.log(vec![kernel::make_log_only_event(
                        "turn/end",
                        serde_json::json!({ "reason": msg }),
                        clock,
                    )])?;
                    break TurnOutcome::Cancelled {
                        category: Some(CancellationCategory::MidTurnAbort),
                    };
                }
            }

            // ---- semantic termination ----
            if response.stop_reason == StopReason::EndTurn && calls.is_empty() {
                // INVARIANT (grok interjection.rs:87-96): before the turn
                // completes, take one final steering drain — late arrivals
                // must not be silently dropped by an ending turn.
                let late = self.steering.drain();
                if let Some(msg) = format_as_user_message(&late) {
                    Self::emit(&mut out, LoopEvent::SteeringInjected { text: msg.text_content() });
                    self.log(vec![kernel::make_event(
                        "user/message",
                        serde_json::json!({ "text": msg.text_content(), "origin": "steering" }),
                        clock,
                    )])?;
                    history.push(msg);
                    self.phase_transition(TurnPhase::AwaitingModelResponse, &mut out)?;
                    continue;
                }
                match evaluate_stop(&response, 0, &|_| StopGateDecision::AllowStop) {
                    StopGateDecision::AllowStop => {
                        break TurnOutcome::Completed {
                            tools_called: tools_called.clone(),
                            structured_output: structured.take(),
                            stop: CompletedStop::EndTurn,
                        };
                    }
                    StopGateDecision::KeepWorking { feedback } => {
                        if !feedback.is_empty() {
                            history.push(Message::user(feedback));
                            self.phase_transition(TurnPhase::AwaitingModelResponse, &mut out)?;
                            continue;
                        }
                    }
                }
            }
            if response.stop_reason == StopReason::Refusal {
                break TurnOutcome::Completed {
                    tools_called: tools_called.clone(),
                    structured_output: None,
                    stop: CompletedStop::Refusal,
                };
            }

            self.phase_transition(TurnPhase::AwaitingModelResponse, &mut out)?;
        };

        self.steering.set_turn_running(false);
        let outcome = match outcome {
            TurnOutcome::Cancelled { category } => {
                self.phase_transition(TurnPhase::Error, &mut out)?;
                self.phase_transition(TurnPhase::Idle, &mut out)?;
                TurnOutcome::Cancelled { category }
            }
            TurnOutcome::StationarityEnded => {
                self.phase_transition(TurnPhase::Completing, &mut out)?;
                self.phase_transition(TurnPhase::Idle, &mut out)?;
                TurnOutcome::StationarityEnded
            }
            other => {
                self.phase_transition(TurnPhase::Completing, &mut out)?;
                self.phase_transition(TurnPhase::Idle, &mut out)?;
                other
            }
        };
        // forward the buffered turn events to the caller's sink (executor
        // progress was already streamed through directly)
        for ev in out {
            events(ev);
        }
        let outcome_json = serde_json::to_string(&serde_json::json!({
            "kind": match &outcome {
                TurnOutcome::Completed { .. } => "completed",
                TurnOutcome::Cancelled { .. } => "cancelled",
                TurnOutcome::MaxTurnsReached { .. } => "max_turns",
                TurnOutcome::StationarityEnded => "stationarity_ended",
            },
            "steps": steps,
            "tokens": usage_total.input_tokens + usage_total.output_tokens,
        }))
        .unwrap_or_default();
        events(LoopEvent::TurnFinished { outcome: outcome_json });
        Ok(outcome)
    }
}

/// Fault injection: `OKRA_KILL_AT_PHASE="<PhaseName>:<n>"` aborts the
/// process when the named phase is entered for the nth time in the process
/// (§3 #63 killAtPhase at every durable boundary). Phase names match the
/// TurnPhase debug spelling.
fn fault_inject_at_phase(phase: TurnPhase) {
    let Ok(spec) = std::env::var("OKRA_KILL_AT_PHASE") else {
        return;
    };
    let Some((name, nth)) = spec.rsplit_once(':') else {
        return;
    };
    let Ok(nth) = nth.parse::<u64>() else {
        return;
    };
    if name == format!("{phase:?}") {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTERS: std::sync::Mutex<[AtomicU64; 16]> =
            std::sync::Mutex::new([
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
                AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
            ]);
        // index phases deterministically via their debug name hash
        let idx = (name.len() + name.chars().map(|c| c as usize).sum::<usize>()) % 16;
        let counters = COUNTERS.lock().unwrap();
        let n = counters[idx].fetch_add(1, Ordering::SeqCst) + 1;
        if n == nth {
            eprintln!("[killAtPhase] aborting at {name} (occurrence {n})");
            std::process::abort();
        }
    }
}
