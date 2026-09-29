//! The sync turn loop (grok SessionActor's turn loop, decision N0001):
//! sampler + tool plane + policy plane + kernel event log + steering +
//! governors + backstops, driven by one struct.
//!
//! Every durable transition lands in the kernel log FIRST ("model-visible
//! means logged"), then surfaces. `OKRA_KILL_AT_PHASE` fault injection
//! aborts the process at a phase boundary for the crash-recovery harness
//! (§3 #63).

use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::tasks::{automation_mutation_allowed, AutomationGuardDecision, TurnDispatch};
use okra_compaction::OriginTag;
use crate::turn::{CancellationCategory, CompletedStop, TurnOutcome, TurnPhase, TurnMachine};

/// Configuration for one agent.
impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("max_steps", &self.max_steps)
            .field("stationarity", &self.stationarity)
            .field("salvage_budget", &self.salvage_budget)
            .field("backstops", &self.backstops)
            .field("unattended", &self.unattended)
            .field("semantic_judge", &self.semantic_judge.is_some())
            .finish()
    }
}

impl Clone for AgentConfig {
    fn clone(&self) -> Self {
        AgentConfig {
            max_steps: self.max_steps,
            stationarity: self.stationarity.clone(),
            salvage_budget: self.salvage_budget,
            backstops: self.backstops.clone(),
            unattended: self.unattended,
            semantic_judge: self.semantic_judge.clone(),
        }
    }
}

pub struct AgentConfig {
    pub max_steps: usize,
    pub stationarity: crate::governors::StationarityConfig,
    pub salvage_budget: u32,
    pub backstops: BackstopConfig,
    /// Structured callables: whether the agent runs unattended (yolo-capable).
    pub unattended: bool,
    /// Semantic wander governor (TypeSafe Jev). None → pure-code path.
    pub semantic_judge: Option<std::sync::Arc<dyn crate::semantics::SemanticJudge>>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            max_steps: 32,
            stationarity: Default::default(),
            salvage_budget: 2,
            backstops: Default::default(),
            unattended: false,
            semantic_judge: None,
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
    /// Raw call arguments as sampled — surfaces use them for tool-card
    /// context; the approval card shows the APPROVED (normalized,
    /// post-hooks) bytes from the approval request instead.
    ToolCallStarted { id: String, name: String, args_json: String },
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

    /// Hook (#45) telemetry: (events fired, contained failures).
    fn hook_stats(&self) -> (u64, u64) {
        (0, 0)
    }

    /// Per-tool execution counts (read counters for benchmarks).
    fn execution_counts(&self) -> Vec<(String, u64)> {
        Vec::new()
    }

    /// The approval audit pair (asked/decided) accumulated so far — the
    /// loop drains it into the kernel log (log-only, ignorable) so
    /// approval cards survive replay.
    fn drain_approval_audit(&mut self) -> Vec<okra_policy::ApprovalAuditEvent> {
        Vec::new()
    }
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
    /// Automation self-mutation guard (#40): how THIS turn was dispatched.
    /// Ordinary for interactive turns; set before running automation turns.
    pub turn_dispatch: TurnDispatch,
    /// The audit pair (asked/decided) from every `decide`, drained by the
    /// loop into the kernel log (log-only, ignorable).
    approval_audit: std::sync::Arc<std::sync::Mutex<Vec<okra_policy::ApprovalAuditEvent>>>,
    executions: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl PolicyToolExecutor {
    pub fn new(registry: okra_tools::Registry, approvals: ApprovalService) -> Self {
        PolicyToolExecutor {
            registry,
            approvals,
            grants: GrantStore::new(1),
            lattice: PermissionLattice::new(),
            ceiling: ToolApprovalCeiling::GrantsAllowed,
            turn_dispatch: TurnDispatch::Ordinary,
            approval_audit: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            executions: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl ToolExecutor for PolicyToolExecutor {
    fn hook_stats(&self) -> (u64, u64) {
        (
            self.registry.hook_system.events_fired(),
            self.registry.hook_system.failures(),
        )
    }

    fn execution_counts(&self) -> Vec<(String, u64)> {
        self.executions
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    fn drain_approval_audit(&mut self) -> Vec<okra_policy::ApprovalAuditEvent> {
        std::mem::take(&mut *self.approval_audit.lock().unwrap())
    }

    fn execute(
        &mut self,
        call: &ToolCall,
        events: &mut dyn FnMut(LoopEvent),
    ) -> Result<(String, bool, bool), String> {
        *self
            .executions
            .lock()
            .unwrap()
            .entry(call.name.clone())
            .or_insert(0) += 1;
        // #40 automation self-mutation guard, first check of the turn:
        // a scheduled turn must not reschedule itself; an idle turn must not
        // spawn idle tasks. Pure deny on the name, before hooks/approval and
        // independent of registry registration (the loop's ToolCallFinished
        // fires from the returned tuple).
        if automation_mutation_allowed(self.turn_dispatch, &call.name)
            == AutomationGuardDecision::Denied
        {
            let reason = format!(
                "denied by automation self-mutation guard ({:?} turn may not call {})",
                self.turn_dispatch, call.name
            );
            return Ok((reason, true, false));
        }

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
            let unattended_yolo = self.ceiling == ToolApprovalCeiling::UnattendedAllowed
                && self.approvals.policy() == ApprovalPolicy::Ask;
            if self.ceiling != ToolApprovalCeiling::AlwaysPrompt
                && self.grants.check(&call.name, &args_json, &behavior, GrantScope::Conversation)
                    == GrantDecision::Granted
            {
                // granted for exactly these bytes
            } else if unattended_yolo {
                // UnattendedAllowed ceiling: hosts with no session owner may
                // honour yolo; the arg-hash grant still records the decision
                // so the audit trail stays complete.
            } else {
                let (outcome, audit) = self
                    .approvals
                    .decide(&call.name, &call.id, &args_json);
                self.approval_audit.lock().unwrap().extend(audit);
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
        // same canonical bytes — approved bytes = executed bytes).
        // A hook `Ask` verdict routes through the approval service
        // (prompt gate): denied -> blocked; granted -> re-dispatch with the
        // ask satisfied (recorded in the audit trail).
        let stream = match self.registry.dispatch(&call.name, &approved.args(), &[]) {
            Ok(stream) => stream,
            Err(okra_tools::RegistryError::HookAsk { .. }) => {
                let (outcome, audit) = self
                    .approvals
                    .decide(&call.name, &call.id, &args_json);
                self.approval_audit.lock().unwrap().extend(audit);
                if !outcome.grants() {
                    let reason = format!("hook asked; {}", outcome.denial_reason());
                    events(LoopEvent::ToolCallFinished {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        is_error: true,
                        output: reason.clone(),
                    });
                    return Ok((reason, true, false));
                }
                match self.registry.dispatch_allow_ask(&call.name, &approved.args(), &[]) {
                    Ok(stream) => stream,
                    Err(e) => return Err(format!("dispatch: {e}")),
                }
            }
            Err(e) => return Err(format!("dispatch: {e}")),
        };
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
pub struct Agent<S: Sampler + ?Sized> {
    pub config: AgentConfig,
    sampler: Arc<S>,
    executor: Box<dyn ToolExecutor>,
    session: kernel::SessionHandle,
    steering: SteeringInbox,
    machine: TurnMachine,
    turn_counter: u64,
    /// Set from any surface thread via [`Agent::request_stop`]; observed at
    /// every step boundary. A stopped turn is Cancelled(UserRequested) and
    /// recovers through the standard repair path on the next turn.
    stop: Arc<AtomicBool>,
    /// Semantic wander governor (N0021): Jev-judged progress signals at
    /// step boundaries, fail-open, inert when unconfigured.
    wander: crate::semantics::WanderGovernor,
}

impl<S: Sampler + ?Sized> Agent<S> {
    pub fn new(
        mut config: AgentConfig,
        sampler: Arc<S>,
        executor: Box<dyn ToolExecutor>,
        session: kernel::SessionHandle,
    ) -> Self {
        let wander = {
            let judge = config.semantic_judge.take()
                .unwrap_or_else(|| std::sync::Arc::new(
                    crate::semantics::JevSemanticJudge::inert(),
                ));
            crate::semantics::WanderGovernor::new(judge)
        };
        Agent {
            config,
            sampler,
            executor,
            session,
            steering: SteeringInbox::new(),
            machine: TurnMachine::new(),
            turn_counter: 0,
            stop: Arc::new(AtomicBool::new(false)),
            wander,
        }
    }

    pub fn steering_sender(&self) -> std::sync::mpsc::Sender<crate::steering::Tagged> {
        self.steering.sender()
    }

    /// Surface-facing stop: flips the flag the turn loop checks at each
    /// step boundary (never blocks; safe from any thread).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// The shared stop flag, so a surface can request a stop before/while
    /// `run_turn` owns the agent on another thread.
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Install an externally-held stop flag (the surface keeps a clone and
    /// flips it while this agent is mid-turn on another thread).
    pub fn set_stop_flag(&mut self, flag: Arc<AtomicBool>) {
        self.stop = flag;
    }

    /// Hook (#45) telemetry from the executor: (events fired, failures).
    pub fn hook_stats(&self) -> (u64, u64) {
        self.executor.hook_stats()
    }

    /// Per-tool execution counts from the executor.
    pub fn execution_counts(&self) -> Vec<(String, u64)> {
        self.executor.execution_counts()
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

    /// One conversational turn (delegates to the seeded core).
    pub fn run_turn(
        &mut self,
        input: &str,
        events: &mut dyn FnMut(LoopEvent),
    ) -> Result<TurnOutcome, String> {
        let (outcome, _) = self.run_turn_core(Vec::new(), input, events)?;
        Ok(outcome)
    }

    /// Agent-continuation turn (G2): seeds the model context from the
    /// session context (compacted prefix + tail), runs the turn, then folds
    /// the resulting messages back through the two-pass compaction cycle.
    /// `memory` (tiered memory recall, §3 #33) is optional: when present its
    /// redacted recall is folded into the byte-stable head.
    pub fn run_turn_continuation(
        &mut self,
        context: &mut okra_compaction::SessionContext,
        compactor: &dyn okra_compaction::Compactor,
        memory: Option<&okra_memory::TieredReader>,
        skills: Option<&okra_memory::SkillCatalog>,
        input: &str,
        events: &mut dyn FnMut(LoopEvent),
    ) -> Result<TurnOutcome, String> {
        // tiered memory recall (#33): redacted, folded into the stable head
        if let Some(reader) = memory {
            let recall = reader.recall();
            let recall = okra_memory::redact_secrets(&recall);
            if !recall.trim().is_empty() {
                context.set_memory_recall(&recall);
            }
        }
        // progressive disclosure (#M2 skills): layer 1 index in the head;
        // layer 2 bodies activate when noted paths match their patterns
        if let Some(catalog) = skills {
            if context.skill_index().is_none() {
                context.set_skill_index(&catalog.disclosure_index());
            }
            let paths = context.noted_paths();
            if !paths.is_empty() {
                for skill in catalog.active_for_paths(&paths) {
                    let digest: String = skill.body.lines().next().unwrap_or("").to_string();
                    context.activate_skill(&skill.name, &digest);
                }
            }
        }
        let seed = context.messages().to_vec();
        let seed_len = seed.len();
        let events_before_compactions = context.events().len();
        let (outcome, history) = self.run_turn_core(seed, input, events)?;
        let new_messages: Vec<Message> = history[seed_len..].to_vec();
        // note filesystem paths touched by fs tools → hydration at install
        for msg in &new_messages {
            for call in msg.tool_calls() {
                if matches!(call.name.as_str(), "read_file" | "write_file" | "edit_file")
                    && let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.args_json)
                    && let Some(path) = args.get("path").and_then(|v| v.as_str())
                {
                    context.note_file(path);
                }
            }
        }
        context.extend(new_messages, compactor);
        for ev in &context.events()[events_before_compactions..] {
            if matches!(
                ev.kind,
                okra_compaction::CompactionKind::Install
                    | okra_compaction::CompactionKind::Emergency
            ) {
                events(LoopEvent::CompactionNotice {
                    note: format!(
                        "context compacted ({:?}): {} -> {} messages",
                        ev.kind, ev.messages_before, ev.messages_after
                    ),
                });
            }
        }
        Ok(outcome)
    }

    /// The seeded turn core.
    fn run_turn_core(
        &mut self,
        seed_history: Vec<Message>,
        input: &str,
        events: &mut dyn FnMut(LoopEvent),
    ) -> Result<(TurnOutcome, Vec<Message>), String> {
        self.turn_counter += 1;
        let turn_no = self.turn_counter;
        let mut out: Vec<LoopEvent> = Vec::new();
        let clock = kernel::wall_clock;

        // ---- phase: enter ----
        self.phase_transition(TurnPhase::ProcessingInput, &mut out)?;
        Self::emit(&mut out, LoopEvent::TurnStarted { turn: turn_no });
        // Durable boundary 1: turn/start
        self.log(vec![kernel::make_log_only_event(
            "turn/start",
            serde_json::json!({ "turn": turn_no }),
            clock,
        )])?;
        fault_inject_at_boundary("turn_start_logged");
        // Durable boundary 2: the user message is on the surface
        self.log(vec![kernel::make_event(
            "user/message",
            serde_json::json!({ "text": input }),
            clock,
        )])?;
        fault_inject_at_boundary("user_message_logged");

        // pending tool calls awaiting results (assistant calls from last step)
        let mut history: Vec<Message> = seed_history;
        history.push(Message::user(input));
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
            // ---- user stop (surface-requested) ----
            if self.stop.load(Ordering::Relaxed) {
                break TurnOutcome::Cancelled {
                    category: Some(CancellationCategory::UserRequested),
                };
            }
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
                fault_inject_at_boundary("assistant_message_logged");
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

                // semantic wander check (N0021): a Jev judgment over the
                // recent activity at step boundaries; fail-open, bounded
                if self.wander.is_active() {
                    let recent = call_tuples
                        .iter()
                        .rev()
                        .take(6)
                        .rev()
                        .map(|(name, args)| {
                            format!(
                                "- {name} {}",
                                args.chars().take(120).collect::<String>()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let user_text = history
                        .iter()
                        .rev()
                        .find_map(|m| {
                            if m.role == okra_providers::Role::User {
                                Some(m.text_content())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    if let Some(reminder) =
                        self.wander.observe_step(steps, &recent, &user_text)
                    {
                        Self::emit(
                            &mut out,
                            LoopEvent::Nudge { reason: "semantic wander check".into() },
                        );
                        // model-visible means logged: the nudge enters the
                        // history, so it enters the log (ignorable, tagged)
                        self.log(vec![kernel::make_event(
                            "user/message",
                            serde_json::json!({
                                "text": reminder,
                                "origin": "semantic-wander",
                            }),
                            clock,
                        )])?;
                        history.push(Message::user(reminder));
                    }
                }

                let mut results: Vec<Message> = Vec::new();
                for call in &calls {
                    Self::emit(
                        &mut out,
                        LoopEvent::ToolCallStarted {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args_json: call.args_json.clone(),
                        },
                    );
                    self.log(vec![kernel::make_log_only_event(
                        "tool/call",
                        serde_json::json!({ "callId": call.id, "tool": call.name }),
                        clock,
                    )])?;
                    // Durable boundary: the call intent is on disk BEFORE any
                    // side effect — recovery marks it outcome-unknown.
                    fault_inject_at_boundary("tool_call_logged");
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
                // logged) — it carries a surfaceOp; tool/call stays log-only.
                // The FULL output text is logged: it is model-visible content
                // (the next request carries it), so the log is the transcript
                // of record; the 400-char cut belongs to the live stream
                // view, never to the durable event.
                self.log(results.iter().map(|r| {
                    let ContentBlock::ToolResponse { result } = &r.content[0] else {
                        unreachable!("just built")
                    };
                    kernel::make_event(
                        "tool/result",
                        serde_json::json!({
                            "callId": result.call_id,
                            "isError": result.is_error,
                            "output": result.content,
                            "origin": OriginTag::ToolContext,
                        }),
                        clock,
                    )
                })
                .collect())?;
                fault_inject_at_boundary("tool_result_logged");

                // approval audit pair → kernel log (log-only, ignorable):
                // approval cards survive replay
                for audit in self.executor.drain_approval_audit() {
                    let (ty, data): (&str, serde_json::Value) = match &audit {
                        okra_policy::ApprovalAuditEvent::Asked {
                            id,
                            tool_name,
                            call_id,
                            args_json,
                        } => (
                            "approval/asked",
                            serde_json::json!({
                                "approvalId": id,
                                "toolName": tool_name,
                                "callId": call_id,
                                "args": args_json.chars().take(400).collect::<String>(),
                            }),
                        ),
                        okra_policy::ApprovalAuditEvent::Decided { id, outcome } => (
                            "approval/decided",
                            serde_json::json!({
                                "approvalId": id,
                                "outcome": serde_json::to_value(outcome).unwrap_or_default(),
                            }),
                        ),
                    };
                    let mut ev = kernel::make_log_only_event(ty, data, clock);
                    ev.ignorable = Some(true);
                    self.log(vec![ev])?;
                }

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
        // Durable boundary: the turn is closed on disk on EVERY path.
        {
            let (kind, stop_label): (&str, Option<&str>) = match &outcome {
                TurnOutcome::Completed { stop, .. } => (
                    "completed",
                    Some(match stop {
                        CompletedStop::EndTurn => "end_turn",
                        CompletedStop::MaxTokens => "max_tokens",
                        CompletedStop::Refusal => "refusal",
                    }),
                ),
                TurnOutcome::Cancelled { .. } => ("cancelled", None),
                TurnOutcome::MaxTurnsReached { .. } => ("max_turns", None),
                TurnOutcome::StationarityEnded => ("stationarity", None),
            };
            self.log(vec![kernel::make_log_only_event(
                "turn/end",
                serde_json::json!({ "turn": turn_no, "kind": kind, "stop": stop_label }),
                clock,
            )])
            .ok();
            fault_inject_at_boundary("turn_end_logged");
        }
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
        let final_history = history;
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
        Ok((outcome, final_history))
    }
}

/// Fault injection: `OKRA_KILL_AT_BOUNDARY="<name>:<n>"` aborts the process
/// right AFTER the named durable boundary is fsynced (G1 matrix):
/// turn_start_logged | user_message_logged | assistant_message_logged |
/// tool_call_logged | tool_result_logged | turn_end_logged.
fn fault_inject_at_boundary(name: &str) {
    let Ok(spec) = std::env::var("OKRA_KILL_AT_BOUNDARY") else {
        return;
    };
    let Some((want, nth)) = spec.rsplit_once(':') else {
        return;
    };
    let Ok(nth) = nth.parse::<u64>() else {
        return;
    };
    if want != name {
        return;
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    if n == nth {
        eprintln!("[killAtBoundary] aborting after {name} (occurrence {n})");
        std::process::abort();
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
