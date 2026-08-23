//! Ownership handle for one active run.
//!
//! `RunHandle` is deliberately small: it owns lifecycle settlement and delegates model/tool
//! work to the caller-owned executor.  Dropping an unfinished handle requests cancellation and
//! settles the agent as cancelled, ensuring an abandoned run cannot leave the agent busy.

use crate::agent::{AgentConfiguration, AgentInner};
use crate::effect::{
    EffectAction, EffectId, EffectOutcome, EffectSubject, HookEffectOutcome, HookInvocation,
    ProviderEffectOutcome, ProviderResponse,
};
use crate::error::CoreError;
use crate::event::{
    AgentEvent, AgentEventKind, AutomaticCompactionOutcome, EventSequence,
    ProviderRequestSkipReason,
};
use crate::hooks::{AfterToolCall, AgentLoopTurnUpdate, Replacement};
use crate::scheduler::{
    CancellationToken, ModelEventStream, ModelRequest, ModelStreamEvent,
};
use crate::state::{
    AgentMessage, AgentPhase, AgentToolCall, ModelDescriptor, RunId, RunPhase, RunSnapshot,
    RunState, StopReason, ThinkingLevel, ToolCallId, TurnId, Usage,
};
use crate::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolFuture, ToolUpdate, project_tool_result_as_text,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Poll, Waker};

mod tool_execution;

enum PreparedToolCall {
    Immediate {
        result: AgentToolResult,
        terminate: bool,
    },
    Execute {
        tool: Arc<dyn AgentTool>,
        effect: EffectTicket,
    },
}

struct PreparedToolExecution {
    source_index: usize,
    call: ToolCall,
    preparation: PreparedToolCall,
}

/// One update captured by a tool callback and waiting for lifecycle delivery.
type PendingToolUpdate = (ToolCallId, String, ToolUpdate);

/// Tool updates captured by capability callbacks.
///
/// The callback API is synchronous, while lifecycle observers are awaited. The
/// queue bridges those boundaries without requiring a runtime: callbacks wake
/// the caller-owned run future, which drains updates as a first-class scheduler
/// step before polling another tool or settling the current tool. Call IDs are
/// closed as soon as their futures resolve so late callbacks are ignored, as in
/// Pi's `executePreparedToolCall` lifecycle.
#[derive(Clone, Default)]
struct PendingToolUpdates {
    state: Arc<Mutex<PendingToolUpdateState>>,
}

#[derive(Default)]
struct PendingToolUpdateState {
    updates: Vec<PendingToolUpdate>,
    closed_calls: BTreeSet<ToolCallId>,
    waker: Option<Waker>,
}

impl PendingToolUpdates {
    fn push(&self, update: PendingToolUpdate) {
        let waker = {
            let mut state = self.state.lock().expect("tool update mutex poisoned");
            if state.closed_calls.contains(&update.0) {
                return;
            }
            state.updates.push(update);
            state.waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn take(&self) -> Option<Vec<PendingToolUpdate>> {
        let mut state = self.state.lock().expect("tool update mutex poisoned");
        if state.updates.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut state.updates))
        }
    }

    fn has_updates(&self) -> bool {
        !self
            .state
            .lock()
            .expect("tool update mutex poisoned")
            .updates
            .is_empty()
    }

    fn close(&self, call_id: &ToolCallId) {
        let mut state = self.state.lock().expect("tool update mutex poisoned");
        state.closed_calls.insert(call_id.clone());
    }

    fn register_waker(&self, waker: &Waker) {
        self.state.lock().expect("tool update mutex poisoned").waker = Some(waker.clone());
    }
}

struct PendingToolExecution<'a> {
    source_index: usize,
    call: ToolCall,
    effect: EffectTicket,
    cancellation_settlement_mode: crate::tool::CancellationSettlementMode,
    future: ToolFuture<'a>,
}

struct CompletedToolExecution {
    source_index: usize,
    call: ToolCall,
    effect: EffectTicket,
    result: Result<AgentToolResult, crate::error::ToolError>,
}

/// One successfully accepted before-effect boundary awaiting settlement.
///
/// This stays private to the core run so a tool/provider path cannot settle an
/// effect that was never admitted by its configured gate.
#[derive(Clone)]
struct EffectTicket {
    action: EffectAction,
}

/// Context accounting uses a provider-confirmed input checkpoint plus only the
/// canonical messages appended after that request. This avoids a zero/error
/// response accidentally resetting a useful context estimate.
#[derive(Default)]
struct ContextEstimator {
    last_valid_input: Option<(u64, usize)>,
}

impl ContextEstimator {
    fn observe_valid_input(&mut self, input_tokens: Option<u64>, request_message_count: usize) {
        if let Some(input_tokens) = input_tokens.filter(|tokens| *tokens != 0) {
            self.last_valid_input = Some((input_tokens, request_message_count));
        }
    }

    fn estimate(&self, agent: &AgentInner) -> u64 {
        let state = agent.state.lock().expect("agent state mutex poisoned");
        let raw_canonical_estimate = estimate_messages_tokens(&state.messages);
        match self.last_valid_input {
            Some((input_tokens, source_message_count))
                if source_message_count <= state.messages.len() =>
            {
                // A provider usage checkpoint can be lower than the raw
                // canonical estimate when a provider omits system/tool input,
                // normalizes a request, or reports a partial usage shape.
                // Never let that lower report suppress an explicit host
                // capacity policy: retain the conservative maximum until a
                // provider supplies a complete, trusted accounting contract.
                input_tokens
                    .saturating_add(estimate_messages_tokens(
                        &state.messages[source_message_count..],
                    ))
                    .max(raw_canonical_estimate)
            }
            _ => raw_canonical_estimate,
        }
    }

    fn reset_after_compaction(&mut self) {
        self.last_valid_input = None;
    }
}

/// Mutable policy state owned by exactly one run handle.
#[derive(Default)]
pub(crate) struct RunPolicyState {
    failure_streak: Option<(crate::tool::FailureSignature, u32)>,
    automatic_compactions: u32,
    overflow_retries: u32,
    /// An incomplete provider continuation is retried at most once, even if
    /// the host's total per-run retry budget permits later turns to recover.
    overflow_retried_this_continuation: bool,
    /// A successful but insufficient compaction should not immediately loop
    /// against the exact same retained transcript.
    compaction_blocked_message_count: Option<usize>,
    compaction_cancelled: bool,
}

#[derive(Clone, Debug)]
pub(super) struct TerminalToolFailure {
    pub(super) message: String,
}

#[derive(Default)]
pub(super) struct ToolBatchOutcome {
    pub(super) all_terminate: bool,
    pub(super) terminal_failure: Option<TerminalToolFailure>,
}

/// State carried from one settled assistant turn to the next model request.
struct NextTurn {
    turn_id: TurnId,
    context: Option<crate::hooks::ContextEnvelope>,
    model_override: Option<ModelDescriptor>,
    thinking_override: Option<ThinkingLevel>,
}

/// A handle to the one run currently owning an agent.
pub struct RunHandle {
    pub(crate) agent: Weak<AgentInner>,
    pub(crate) state: Arc<Mutex<RunState>>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) initial_messages: Vec<AgentMessage>,
    /// Index of the first message created by this invocation. `AgentEnd`
    /// reports this suffix, matching Pi continuation semantics.
    pub(crate) message_start_index: usize,
    pub(crate) skip_initial_steering: bool,
    /// Immutable prompt/tool/hook configuration captured when this run claimed the agent.
    pub(crate) configuration: Arc<AgentConfiguration>,
    pub(crate) policy: Mutex<RunPolicyState>,
    /// Monotonic process-local correlation IDs for paired gate calls.
    pub(crate) next_effect_id: AtomicU64,
    /// Assistant calls restored from durable state that must run before the
    /// first provider request of this core epoch.
    pub(crate) recovery_tool_calls: Option<Vec<AgentToolCall>>,
    /// Whether the committed source-order result prefix of a recovered batch
    /// already requested termination. The normal scheduler combines this with
    /// the recovered suffix instead of treating the suffix as a new batch.
    pub(crate) recovery_prior_all_terminate: Option<bool>,
}

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHandle")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl RunHandle {
    async fn begin_effect(&self, subject: EffectSubject) -> Result<EffectTicket, CoreError> {
        let effect_id = EffectId(
            self.next_effect_id
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1),
        );
        let action = EffectAction::new(
            effect_id,
            self.id(),
            self.configuration.provenance.clone(),
            subject,
        );
        self.configuration
            .effect_gate
            .before(action.clone())
            .await?;
        Ok(EffectTicket { action })
    }

    async fn settle_effect(
        &self,
        ticket: EffectTicket,
        outcome: EffectOutcome,
    ) -> Result<(), CoreError> {
        self.configuration
            .effect_gate
            .after(ticket.action, outcome)
            .await?;
        Ok(())
    }

    async fn settle_hook<T>(
        &self,
        ticket: EffectTicket,
        outcome: &Result<T, crate::error::HookError>,
    ) -> Result<(), CoreError> {
        let outcome = match outcome {
            Ok(_) => HookEffectOutcome::Succeeded,
            Err(error) => HookEffectOutcome::Failed {
                message: error.message.clone(),
            },
        };
        self.settle_effect(ticket, EffectOutcome::HookInvocation(outcome))
            .await
    }

    /// Stable identifier for this run.
    pub fn id(&self) -> RunId {
        self.state.lock().expect("run state mutex poisoned").id
    }

    /// Return an owned snapshot that cannot mutate the run.
    pub fn snapshot(&self) -> RunSnapshot {
        self.state
            .lock()
            .expect("run state mutex poisoned")
            .snapshot()
    }

    /// Cancellation token shared with model and tool operations.
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Return events emitted by this run in their immutable source order.
    pub fn events(&self) -> Vec<AgentEvent> {
        self.state
            .lock()
            .expect("run state mutex poisoned")
            .events
            .clone()
    }

    /// Drive a complete caller-owned run.
    ///
    /// The caller polls this future on its own executor; the core neither
    /// creates an executor nor spawns detached work. A tool-use turn executes
    /// its calls, records their results, and then drives the next model turn.
    pub async fn drive(&self) -> Result<(), CoreError> {
        let result = self.drive_inner().await;
        if let Err(error) = &result
            && !self.snapshot().phase.is_terminal() {
                if self.cancellation.is_cancelled() {
                    if self
                        .policy
                        .lock()
                        .expect("run policy mutex poisoned")
                        .compaction_cancelled
                    {
                        self.settle_compaction_boundary_cancellation().await;
                    } else {
                        self.settle_cancellation().await;
                    }
                } else if matches!(
                    error,
                    CoreError::AutomaticCompaction { .. }
                        | CoreError::AutomaticCompactionUnavailable { .. }
                ) {
                    self.settle_compaction_boundary_failure(error).await;
                } else {
                    self.settle_failure(error).await;
                }
            }
        result
    }

    async fn settle_failure(&self, error: &CoreError) {
        let Some(agent) = self.agent.upgrade() else {
            let _ = self.fail(error.to_string());
            return;
        };
        let failure = {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            let message = AgentMessage::Assistant {
                id: state.allocate_message_id(),
                content: String::new(),
                tool_calls: Vec::new(),
                stop_reason: Some(StopReason::Error),
                error_message: Some(error.to_string()),
            };
            state.partial_response = None;
            state.pending_tool_calls.clear();
            state.append_message(message.clone());
            message
        };
        let turn_id = self.snapshot().turn_id.unwrap_or(TurnId(1));

        let _ = self
            .emit(
                &agent,
                AgentEventKind::MessageStart {
                    message: failure.clone(),
                },
            )
            .await;
        let _ = self
            .emit(&agent, AgentEventKind::MessageEnd { message: failure })
            .await;
        let _ = self
            .emit(
                &agent,
                AgentEventKind::TurnEnd {
                    turn_id,
                    reason: StopReason::Error,
                },
            )
            .await;
        let messages = self.new_messages(&agent);
        let _ = self
            .emit(&agent, AgentEventKind::AgentEnd { messages })
            .await;
        let _ = self.fail(error.to_string());
    }

    /// Settle an automatic-policy boundary without adding a synthetic
    /// assistant message. A compaction failure/cancellation must leave the
    /// pre-transaction canonical transcript untouched.
    async fn settle_compaction_boundary_failure(&self, error: &CoreError) {
        let Some(agent) = self.agent.upgrade() else {
            let _ = self.fail(error.to_string());
            return;
        };
        {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            state.partial_response = None;
            state.is_streaming = false;
            state.pending_tool_calls.clear();
        }
        let turn_id = self.snapshot().turn_id.unwrap_or(TurnId(1));
        let _ = self
            .emit(
                &agent,
                AgentEventKind::TurnEnd {
                    turn_id,
                    reason: StopReason::Error,
                },
            )
            .await;
        let messages = self.new_messages(&agent);
        let _ = self
            .emit(&agent, AgentEventKind::AgentEnd { messages })
            .await;
        let _ = self.fail(error.to_string());
    }

    /// Settle automatic compaction cancellation without adding a synthetic
    /// assistant message to the pre-transaction transcript.
    async fn settle_compaction_boundary_cancellation(&self) {
        let Some(agent) = self.agent.upgrade() else {
            let _ = self.finish(RunPhase::Cancelled, StopReason::Cancelled, None);
            return;
        };
        {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            state.partial_response = None;
            state.is_streaming = false;
            state.pending_tool_calls.clear();
        }
        let turn_id = self.snapshot().turn_id.unwrap_or(TurnId(1));
        let _ = self
            .emit(
                &agent,
                AgentEventKind::TurnEnd {
                    turn_id,
                    reason: StopReason::Cancelled,
                },
            )
            .await;
        let messages = self.new_messages(&agent);
        let _ = self
            .emit(&agent, AgentEventKind::AgentEnd { messages })
            .await;
        let _ = self.finish(RunPhase::Cancelled, StopReason::Cancelled, None);
    }

    async fn drive_inner(&self) -> Result<(), CoreError> {
        let agent = self.agent.upgrade().ok_or(CoreError::InvalidTransition(
            crate::error::StateTransitionError::new("run", "orphaned", "drive"),
        ))?;
        let run_id = self.id();
        self.start_turn(TurnId(1))?;

        {
            let state = agent.state.lock().expect("agent state mutex poisoned");
            if !matches!(state.phase, AgentPhase::Running(id) | AgentPhase::Cancelling(id) if id == run_id)
            {
                return Err(CoreError::InvalidTransition(
                    crate::error::StateTransitionError::new("agent", "not-running", "drive"),
                ));
            }
        }
        self.emit(&agent, AgentEventKind::AgentStart).await?;
        self.emit(&agent, AgentEventKind::TurnStart { turn_id: TurnId(1) })
            .await?;
        for message in &self.initial_messages {
            self.emit(
                &agent,
                AgentEventKind::MessageStart {
                    message: message.clone(),
                },
            )
            .await?;
            self.emit(
                &agent,
                AgentEventKind::MessageEnd {
                    message: message.clone(),
                },
            )
            .await?;
        }
        let mut turn_id = TurnId(1);
        let mut model_override = None::<ModelDescriptor>;
        let mut thinking_override = None::<ThinkingLevel>;
        let mut context_estimator = ContextEstimator::default();
        let mut completed_assistant_turn = false;
        let mut next_context = if self.skip_initial_steering {
            None
        } else {
            self.inject_queued_messages(
                &agent,
                self.current_context(&agent)?,
                self.drain_steering(&agent),
            )
            .await?
        };
        if let Some(tool_calls) = self.recovery_tool_calls.clone() {
            // Recovered assistant calls use the normal tool scheduler and the
            // same post-turn continuation procedure as fresh model output.
            let mut tool_batch = self.execute_tool_calls(&agent, &tool_calls).await?;
            if let Some(prior_all_terminate) = self.recovery_prior_all_terminate {
                tool_batch.all_terminate &= prior_all_terminate;
            }
            completed_assistant_turn = true;
            let Some(next_turn) = self
                .advance_after_assistant_turn(
                    &agent,
                    turn_id,
                    StopReason::ToolUse,
                    &tool_calls,
                    tool_batch,
                    model_override,
                    thinking_override,
                )
                .await?
            else {
                return Ok(());
            };
            turn_id = next_turn.turn_id;
            next_context = next_turn.context;
            model_override = next_turn.model_override;
            thinking_override = next_turn.thinking_override;
        }
        loop {
            if completed_assistant_turn
                && self
                    .maybe_automatic_compaction(
                        &agent,
                        &mut context_estimator,
                        crate::compaction::AutomaticCompactionReason::Threshold,
                        false,
                    )
                    .await?
            {
                // A hook-provided next context is a request-scoped clone of
                // the old canonical transcript. The compaction transaction is
                // authoritative for the next request, so rebuild from the
                // committed canonical history.
                next_context = None;
            }
            let request_message_count = {
                let state = agent.state.lock().expect("agent state mutex poisoned");
                state.messages.len()
            };
            let request = self
                .model_request(
                    &agent,
                    run_id,
                    next_context.take(),
                    model_override.as_ref(),
                    thinking_override,
                )
                .await?;
            let automatic_compaction = agent
                .automatic_compaction
                .read()
                .expect("automatic compaction policy lock poisoned")
                .clone();
            if automatic_compaction.enabled {
                self.emit_context_estimate(&agent, &request, context_estimator.estimate(&agent))
                    .await?;
            }
            let turn_model = request.model.clone();
            let request_layout = agent.prompt_layout_ledger.measure(&request);
            let request_continuity = request_layout.continuity;
            let provider = agent
                .provider
                .read()
                .expect("agent provider lock poisoned")
                .clone()
                .ok_or(CoreError::MissingModelProvider)?;
            self.emit(
                &agent,
                AgentEventKind::PromptLayoutObserved {
                    turn_id,
                    measurement: request_layout,
                },
            )
            .await?;
            if matches!(
                agent.prompt_layout_ledger.policy_value(),
                crate::measurement::PromptLayoutPolicy::RejectUnexpectedRebase
            ) && matches!(
                request_continuity,
                crate::measurement::PromptContinuity::Rebased
                    | crate::measurement::PromptContinuity::Discontinuous
            ) || matches!(
                agent.prompt_layout_ledger.policy_value(),
                crate::measurement::PromptLayoutPolicy::RequireExactExtension
            ) && matches!(
                request_continuity,
                crate::measurement::PromptContinuity::DomainChanged
                    | crate::measurement::PromptContinuity::Rebased
                    | crate::measurement::PromptContinuity::Discontinuous
            ) {
                return Err(CoreError::PromptLayoutRejected {
                    continuity: request_continuity,
                });
            }
            let provider_effect = self
                .begin_effect(EffectSubject::ProviderRequest {
                    request: request.clone(),
                })
                .await?;
            // The exact request has crossed the durable effect-intent and
            // content-free observation boundary. Commit it immediately before
            // transport dispatch so failed preparation never becomes a
            // predecessor for the next logical request.
            agent.prompt_layout_ledger.commit(&request);
            let mut stream = match provider.stream(request, self.cancellation.clone()).await {
                Ok(stream) => stream,
                Err(error) => {
                    let message = error.to_string();
                    self.settle_effect(
                        provider_effect,
                        EffectOutcome::ProviderRequest(ProviderEffectOutcome::Failed {
                            message: message.clone(),
                        }),
                    )
                    .await?;
                    return Err(CoreError::ModelProvider { message });
                }
            };
            let (response, valid_input_tokens) = match self
                .consume_assistant_stream(&agent, stream.as_mut(), turn_id, turn_model)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.settle_effect(
                        provider_effect,
                        EffectOutcome::ProviderRequest(ProviderEffectOutcome::Failed {
                            message: error.to_string(),
                        }),
                    )
                    .await?;
                    return Err(error);
                }
            };
            self.settle_effect(
                provider_effect,
                EffectOutcome::ProviderRequest(ProviderEffectOutcome::Settled(response.clone())),
            )
            .await?;
            let reason = response.stop_reason;
            let tool_calls = response.tool_calls;
            let error_message = response.error_message;
            let provider_context_overflow = response.context_overflow;

            context_estimator.observe_valid_input(valid_input_tokens, request_message_count);

            if !provider_context_overflow {
                self.policy
                    .lock()
                    .expect("run policy mutex poisoned")
                    .overflow_retried_this_continuation = false;
            }

            if provider_context_overflow
                && automatic_compaction.enabled
                && automatic_compaction.overflow_recovery
                    == crate::compaction::OverflowRecovery::CompactAndRetry
            {
                let retry_allowed = {
                    let mut policy = self.policy.lock().expect("run policy mutex poisoned");
                    if policy.overflow_retried_this_continuation
                        || policy.overflow_retries
                            >= automatic_compaction.max_overflow_retries_per_run
                    {
                        false
                    } else {
                        policy.overflow_retries = policy.overflow_retries.saturating_add(1);
                        policy.overflow_retried_this_continuation = true;
                        true
                    }
                };
                if retry_allowed {
                    // The failed provider response is not a model-visible
                    // continuation. Restore the exact pre-request transcript
                    // before passing it to the transactional compactor.
                    {
                        let mut state = agent.state.lock().expect("agent state mutex poisoned");
                        state.truncate_messages(request_message_count);
                        state.partial_response = None;
                        state.is_streaming = false;
                    }
                    self.maybe_automatic_compaction(
                        &agent,
                        &mut context_estimator,
                        crate::compaction::AutomaticCompactionReason::Overflow,
                        true,
                    )
                    .await?;
                    next_context = None;
                    completed_assistant_turn = false;
                    continue;
                }
                self.emit(
                    &agent,
                    AgentEventKind::AutomaticCompactionEnd {
                        reason: crate::compaction::AutomaticCompactionReason::Overflow,
                        retry_provider_request: true,
                        outcome: AutomaticCompactionOutcome::LimitReached,
                    },
                )
                .await?;
            }

            if matches!(reason, StopReason::Error | StopReason::Aborted) {
                self.emit(&agent, AgentEventKind::TurnEnd { turn_id, reason })
                    .await?;
                let messages = self.new_messages(&agent);
                self.emit(&agent, AgentEventKind::AgentEnd { messages })
                    .await?;
                let message = error_message.unwrap_or_else(|| {
                    if reason == StopReason::Aborted {
                        "model response was aborted".into()
                    } else {
                        "model response failed".into()
                    }
                });
                let error = if reason == StopReason::Aborted {
                    CoreError::ModelAborted {
                        message: message.clone(),
                    }
                } else {
                    CoreError::ModelError {
                        message: message.clone(),
                    }
                };
                if reason == StopReason::Aborted && self.cancellation.is_cancelled() {
                    self.finish(RunPhase::Cancelled, StopReason::Aborted, None)?;
                    return Err(CoreError::Cancelled);
                }
                self.fail(message)?;
                return Err(error);
            }

            if reason == StopReason::Cancelled && self.cancellation.is_cancelled() {
                self.emit(&agent, AgentEventKind::TurnEnd { turn_id, reason })
                    .await?;
                let messages = self.new_messages(&agent);
                self.emit(&agent, AgentEventKind::AgentEnd { messages })
                    .await?;
                self.finish(RunPhase::Cancelled, StopReason::Cancelled, None)?;
                return Err(CoreError::Cancelled);
            }

            completed_assistant_turn = true;

            let tool_batch = if tool_calls.is_empty() {
                ToolBatchOutcome::default()
            } else if reason == StopReason::Length {
                ToolBatchOutcome {
                    all_terminate: self.fail_truncated_tool_calls(&agent, &tool_calls).await?,
                    terminal_failure: None,
                }
            } else {
                if reason != StopReason::ToolUse {
                    return Err(CoreError::UnsupportedModelStream {
                        message: format!(
                            "assistant emitted tool calls with terminal reason {reason:?}, expected ToolUse"
                        ),
                    });
                }
                self.execute_tool_calls(&agent, &tool_calls).await?
            };

            let Some(next_turn) = self
                .advance_after_assistant_turn(
                    &agent,
                    turn_id,
                    reason,
                    &tool_calls,
                    tool_batch,
                    model_override,
                    thinking_override,
                )
                .await?
            else {
                return Ok(());
            };
            turn_id = next_turn.turn_id;
            next_context = next_turn.context;
            model_override = next_turn.model_override;
            thinking_override = next_turn.thinking_override;
        }
    }

    /// Run the shared post-assistant continuation procedure for a fresh or
    /// recovered tool batch. The caller supplies the already-settled assistant
    /// reason and source-order calls; this helper owns hooks, queue drains,
    /// turn advancement, and terminal settlement.
    async fn advance_after_assistant_turn(
        &self,
        agent: &AgentInner,
        turn_id: TurnId,
        reason: StopReason,
        tool_calls: &[AgentToolCall],
        tool_batch: ToolBatchOutcome,
        mut model_override: Option<ModelDescriptor>,
        mut thinking_override: Option<ThinkingLevel>,
    ) -> Result<Option<NextTurn>, CoreError> {
        if let Some(terminal_failure) = tool_batch.terminal_failure {
            self.emit(
                agent,
                AgentEventKind::ProviderRequestSkipped {
                    reason: ProviderRequestSkipReason::ToolCircuitBreaker,
                },
            )
            .await?;
            self.emit(
                agent,
                AgentEventKind::TurnEnd {
                    turn_id,
                    reason: StopReason::Error,
                },
            )
            .await?;
            let messages = self.new_messages(agent);
            self.emit(agent, AgentEventKind::AgentEnd { messages })
                .await?;
            let error = CoreError::ToolCircuitBreaker {
                message: terminal_failure.message,
            };
            self.fail(error.to_string())?;
            return Err(error);
        }

        self.emit(agent, AgentEventKind::TurnEnd { turn_id, reason })
            .await?;
        let current_context = self.current_context(agent)?;
        let prepare_next_turn_effect = self
            .begin_effect(EffectSubject::HookInvocation {
                hook: HookInvocation::PrepareNextTurn,
            })
            .await?;
        let prepared_turn = self
            .configuration
            .hooks
            .prepare_next_turn_async(current_context.clone(), self.cancellation.clone())
            .await;
        self.settle_hook(prepare_next_turn_effect, &prepared_turn)
            .await?;
        let AgentLoopTurnUpdate {
            context,
            model,
            thinking_level,
        } = prepared_turn?;
        let prepared_context = context.unwrap_or(current_context);
        if let Some(model) = model {
            model_override = Some(model);
        }
        if let Some(thinking_level) = thinking_level {
            thinking_override = Some(thinking_level);
        }

        let should_stop_effect = self
            .begin_effect(EffectSubject::HookInvocation {
                hook: HookInvocation::ShouldStopAfterTurn,
            })
            .await?;
        let should_stop = self
            .configuration
            .hooks
            .should_stop_after_turn_async(&prepared_context, self.cancellation.clone())
            .await;
        self.settle_hook(should_stop_effect, &should_stop).await?;
        if should_stop? {
            self.emit_agent_end_and_succeed(agent, reason).await?;
            return Ok(None);
        }

        let mut queued = self.drain_steering(agent);
        let has_more_tool_calls = !tool_calls.is_empty() && !tool_batch.all_terminate;
        if !has_more_tool_calls && queued.is_empty() {
            queued = self.drain_follow_up(agent);
        }
        if has_more_tool_calls || !queued.is_empty() {
            let next_turn_id = TurnId(turn_id.0.saturating_add(1));
            self.advance_turn(next_turn_id)?;
            self.emit(
                agent,
                AgentEventKind::TurnStart {
                    turn_id: next_turn_id,
                },
            )
            .await?;
            let context = self
                .inject_queued_messages(agent, prepared_context, queued)
                .await?;
            return Ok(Some(NextTurn {
                turn_id: next_turn_id,
                context,
                model_override,
                thinking_override,
            }));
        }

        self.emit_agent_end_and_succeed(agent, reason).await?;
        Ok(None)
    }

    async fn model_request(
        &self,
        agent: &AgentInner,
        run_id: RunId,
        context: Option<crate::hooks::ContextEnvelope>,
        model_override: Option<&ModelDescriptor>,
        thinking_override: Option<ThinkingLevel>,
    ) -> Result<ModelRequest, CoreError> {
        let (context, system_prompt, model, thinking_level, tools) = {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            if !matches!(state.phase, AgentPhase::Running(id) | AgentPhase::Cancelling(id) if id == run_id)
            {
                return Err(CoreError::InvalidTransition(
                    crate::error::StateTransitionError::new(
                        "agent",
                        "not-running",
                        "model_request",
                    ),
                ));
            }
            state.is_streaming = true;
            (
                context.unwrap_or_else(|| crate::hooks::ContextEnvelope {
                    version: 1,
                    messages: state.messages.clone(),
                    host_messages: state.host_messages.clone(),
                }),
                self.configuration.system_prompt.clone(),
                model_override.cloned().or_else(|| state.model.clone()),
                thinking_override.unwrap_or(state.thinking_level),
                self.configuration.tools.definitions(),
            )
        };
        let provider_context = self.build_provider_context(agent, context).await?;
        Ok(ModelRequest {
            system_prompt,
            context: provider_context.context,
            tools,
            model,
            thinking_level,
        })
    }

    /// Build the exact provider-facing prompt used by a request or by an automatic compactor.
    /// Keeping this pipeline in one helper prevents compaction from silently skipping model
    /// projection or host context transforms.
    async fn build_provider_context(
        &self,
        agent: &AgentInner,
        context: crate::hooks::ContextEnvelope,
    ) -> Result<crate::compaction::ProviderContext, CoreError> {
        let projected_context = project_model_context(context, &agent.tool_result_projection);
        let transform_effect = self
            .begin_effect(EffectSubject::HookInvocation {
                hook: HookInvocation::TransformContext,
            })
            .await?;
        let transformed = self
            .configuration
            .hooks
            .transform_context_async(projected_context, self.cancellation.clone())
            .await;
        self.settle_hook(transform_effect, &transformed).await?;
        let transformed = transformed?;
        let convert_effect = self
            .begin_effect(EffectSubject::HookInvocation {
                hook: HookInvocation::ConvertToLlm,
            })
            .await?;
        let converted = self
            .configuration
            .hooks
            .convert_to_llm_async(transformed, self.cancellation.clone())
            .await;
        self.settle_hook(convert_effect, &converted).await?;
        Ok(crate::compaction::ProviderContext {
            system_prompt: self.configuration.system_prompt.clone(),
            context: converted?,
            tools: self.configuration.tools.definitions(),
            active_context: None,
        })
    }

    /// Compact at the next-request boundary when the explicit automatic
    /// policy says context pressure requires it. The transaction is performed
    /// under this run's cancellation scope rather than via an idle-only
    /// `CompactionHandle`.
    async fn maybe_automatic_compaction(
        &self,
        agent: &AgentInner,
        estimator: &mut ContextEstimator,
        reason: crate::compaction::AutomaticCompactionReason,
        retry_provider_request: bool,
    ) -> Result<bool, CoreError> {
        let policy = agent
            .automatic_compaction
            .read()
            .expect("automatic compaction policy lock poisoned")
            .clone();
        if !policy.enabled {
            return Ok(false);
        }
        let estimated_tokens_before = estimator.estimate(agent);
        if reason == crate::compaction::AutomaticCompactionReason::Threshold
            && estimated_tokens_before < policy.threshold_tokens()
        {
            return Ok(false);
        }
        let source_message_count = {
            let state = agent.state.lock().expect("agent state mutex poisoned");
            state.messages.len()
        };
        {
            let policy_state = self.policy.lock().expect("run policy mutex poisoned");
            if reason == crate::compaction::AutomaticCompactionReason::Threshold
                && policy_state.compaction_blocked_message_count == Some(source_message_count)
            {
                return Ok(false);
            }
        }
        let (count, limit_reached) = {
            let mut policy_state = self.policy.lock().expect("run policy mutex poisoned");
            if policy_state.automatic_compactions >= policy.max_compactions_per_run {
                (policy_state.automatic_compactions, true)
            } else {
                policy_state.automatic_compactions =
                    policy_state.automatic_compactions.saturating_add(1);
                (policy_state.automatic_compactions, false)
            }
        };
        if limit_reached {
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::LimitReached,
                },
            )
            .await?;
            return Err(CoreError::AutomaticCompaction {
                reason,
                message: "automatic compaction limit reached".into(),
            });
        }
        self.emit(
            agent,
            AgentEventKind::AutomaticCompactionStart {
                reason,
                source_message_count,
                estimated_tokens_before: Some(estimated_tokens_before),
                retry_provider_request,
                count,
            },
        )
        .await?;
        self.emit(
            agent,
            AgentEventKind::ProviderRequestSkipped {
                reason: ProviderRequestSkipReason::AutomaticCompaction,
            },
        )
        .await?;

        // Snapshot before the compactor yields. This generation is the CAS
        // precondition for the later replacement commit.
        let mut context = crate::compaction::snapshot_context(agent);
        let (prefix_messages, retained_messages, split_turn_prefix) =
            automatic_compaction_split(&context.messages, policy.recent_tokens);
        let mut source_messages = prefix_messages.clone();
        source_messages.extend(split_turn_prefix.iter().cloned());
        let overflow_retry_ordinal = retry_provider_request.then(|| {
            self.policy
                .lock()
                .expect("run policy mutex poisoned")
                .overflow_retries
        });

        let Some(compactor) = agent
            .compactor
            .read()
            .expect("agent compactor lock poisoned")
            .clone()
        else {
            let operation = automatic_operation(
                self.id(),
                count,
                reason,
                retry_provider_request,
                overflow_retry_ordinal,
                context.source_history_revision,
                crate::compaction::CompactionStrategy::caller_supplied(),
            );
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Started {
                    operation: operation.clone(),
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::SourceSelected {
                    id: operation.id,
                    source: crate::compaction::observe_source(
                        &context.messages,
                        &source_messages,
                        &retained_messages,
                        &split_turn_prefix,
                    ),
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Terminal {
                    id: operation.id,
                    outcome: crate::compaction::CompactionTerminalOutcome::Unavailable,
                },
            )
            .await?;
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::Unavailable,
                },
            )
            .await?;
            return Err(CoreError::AutomaticCompactionUnavailable { reason });
        };
        let operation = automatic_operation(
            self.id(),
            count,
            reason,
            retry_provider_request,
            overflow_retry_ordinal,
            context.source_history_revision,
            compactor.strategy(),
        );
        self.emit_compaction_lifecycle(
            agent,
            crate::compaction::CompactionLifecycleRecord::Started {
                operation: operation.clone(),
            },
        )
        .await?;
        self.emit_compaction_lifecycle(
            agent,
            crate::compaction::CompactionLifecycleRecord::SourceSelected {
                id: operation.id,
                source: crate::compaction::observe_source(
                    &context.messages,
                    &source_messages,
                    &retained_messages,
                    &split_turn_prefix,
                ),
            },
        )
        .await?;
        let provider_context = async {
            let source_provider_context = self
                .build_provider_context(
                    agent,
                    crate::hooks::ContextEnvelope {
                        version: context.version as u16,
                        messages: source_messages,
                        host_messages: context.host_messages.clone(),
                    },
                )
                .await?;
            let active_provider_context = self
                .build_provider_context(
                    agent,
                    crate::hooks::ContextEnvelope {
                        version: context.version as u16,
                        messages: context.messages.clone(),
                        host_messages: context.host_messages.clone(),
                    },
                )
                .await?;
            Ok::<_, CoreError>(crate::compaction::ProviderContext {
                active_context: Some(active_provider_context.context),
                ..source_provider_context
            })
        }
        .await;
        let provider_context = match provider_context {
            Ok(provider_context) => provider_context,
            Err(error) => {
                if self.cancellation.is_cancelled() {
                    self.policy
                        .lock()
                        .expect("run policy mutex poisoned")
                        .compaction_cancelled = true;
                    self.emit(
                        agent,
                        AgentEventKind::AutomaticCompactionEnd {
                            reason,
                            retry_provider_request,
                            outcome: AutomaticCompactionOutcome::Cancelled,
                        },
                    )
                    .await?;
                    self.emit_compaction_lifecycle(
                        agent,
                        crate::compaction::CompactionLifecycleRecord::Terminal {
                            id: operation.id,
                            outcome: crate::compaction::CompactionTerminalOutcome::Cancelled,
                        },
                    )
                    .await?;
                    return Err(CoreError::Cancelled);
                }
                let message = crate::tool::truncate_middle(&error.to_string(), 1024);
                self.emit(
                    agent,
                    AgentEventKind::AutomaticCompactionEnd {
                        reason,
                        retry_provider_request,
                        outcome: AutomaticCompactionOutcome::Failed {
                            message: message.clone(),
                        },
                    },
                )
                .await?;
                self.emit_compaction_lifecycle(
                    agent,
                    crate::compaction::CompactionLifecycleRecord::Terminal {
                        id: operation.id,
                        outcome: crate::compaction::CompactionTerminalOutcome::Failed,
                    },
                )
                .await?;
                return Err(CoreError::AutomaticCompaction { reason, message });
            }
        };
        let request_layout = if operation.strategy.request_layout
            == crate::compaction::CompactionRequestLayout::ExactReplay
        {
            crate::compaction::CompactionRequestLayout::ExactReplay
        } else {
            crate::compaction::CompactionRequestLayout::Unobserved
        };
        self.emit_compaction_lifecycle(
            agent,
            crate::compaction::CompactionLifecycleRecord::RequestPrepared {
                id: operation.id,
                request: crate::compaction::CompactorRequestObservation {
                    layout: request_layout,
                    provider_context_bytes: Some(provider_context.context.len()),
                    tool_count: Some(provider_context.tools.len()),
                    tools_execution_prohibited: true,
                    // Provider contexts are opaque to tea-core. A concrete
                    // compactor may record a stronger provider-format check.
                    source_is_active_context_prefix: None,
                },
            },
        )
        .await?;
        context.provider_context = Some(provider_context);
        let request = crate::compaction::AutomaticCompactionRequest {
            reason,
            estimated_tokens_before: Some(estimated_tokens_before),
            context_budget_tokens: policy.context_budget.tokens(),
            reserved_tokens: policy.reserved_tokens,
            recent_tokens: policy.recent_tokens,
            prefix_messages,
            retained_messages: retained_messages.clone(),
            split_turn_prefix,
            retry_provider_request,
        };
        let source_history_revision = context.source_history_revision;
        let canonical_source_bytes = crate::compaction::messages_bytes(&context.messages);
        let replacement = match compactor
            .compact_automatic(context, request, self.cancellation.clone())
            .await
        {
            Ok(replacement) => replacement,
            Err(error) => {
                if self.cancellation.is_cancelled() {
                    self.policy
                        .lock()
                        .expect("run policy mutex poisoned")
                        .compaction_cancelled = true;
                    self.emit(
                        agent,
                        AgentEventKind::AutomaticCompactionEnd {
                            reason,
                            retry_provider_request,
                            outcome: AutomaticCompactionOutcome::Cancelled,
                        },
                    )
                    .await?;
                    self.emit_compaction_lifecycle(
                        agent,
                        crate::compaction::CompactionLifecycleRecord::Terminal {
                            id: operation.id,
                            outcome: crate::compaction::CompactionTerminalOutcome::Cancelled,
                        },
                    )
                    .await?;
                    return Err(CoreError::Cancelled);
                }
                let terminal_outcome =
                    if matches!(error, crate::compaction::CompactionError::TimedOut { .. }) {
                        crate::compaction::CompactionTerminalOutcome::TimedOut
                    } else {
                        crate::compaction::CompactionTerminalOutcome::Failed
                    };
                let message = crate::tool::truncate_middle(&error.to_string(), 1024);
                self.emit(
                    agent,
                    AgentEventKind::AutomaticCompactionEnd {
                        reason,
                        retry_provider_request,
                        outcome: AutomaticCompactionOutcome::Failed {
                            message: message.clone(),
                        },
                    },
                )
                .await?;
                self.emit_compaction_lifecycle(
                    agent,
                    crate::compaction::CompactionLifecycleRecord::Terminal {
                        id: operation.id,
                        outcome: terminal_outcome,
                    },
                )
                .await?;
                return Err(CoreError::AutomaticCompaction { reason, message });
            }
        };
        if self.cancellation.is_cancelled() {
            self.policy
                .lock()
                .expect("run policy mutex poisoned")
                .compaction_cancelled = true;
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::Cancelled,
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Terminal {
                    id: operation.id,
                    outcome: crate::compaction::CompactionTerminalOutcome::Cancelled,
                },
            )
            .await?;
            return Err(CoreError::Cancelled);
        }
        self.emit_compaction_lifecycle(
            agent,
            crate::compaction::CompactionLifecycleRecord::ProviderUsageObserved {
                id: operation.id,
                usage: replacement.usage.clone(),
                request_observation: replacement.request_observation.clone(),
                request: replacement.request_layout.map(|layout| {
                    crate::compaction::CompactorRequestObservation {
                        layout,
                        provider_context_bytes: None,
                        tool_count: None,
                        tools_execution_prohibited: true,
                        source_is_active_context_prefix: replacement
                            .source_is_active_context_prefix,
                    }
                }),
            },
        )
        .await?;
        if let Err(error) = crate::compaction::validate_messages(&replacement.messages) {
            let message = crate::tool::truncate_middle(&error.to_string(), 1024);
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::Failed {
                        message: message.clone(),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::ReplacementProposed {
                    id: operation.id,
                    proposal: crate::compaction::CompactionProposalObservation {
                        replacement_message_count: replacement.messages.len(),
                        replacement_bytes: crate::compaction::messages_bytes(&replacement.messages),
                        estimated_context_tokens_after: None,
                        headroom_tokens: None,
                        structural_validation_passed: false,
                        retained_suffix_exact: false,
                        source_generation_matches: automatic_source_generation_matches(
                            agent,
                            source_history_revision,
                        ),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Terminal {
                    id: operation.id,
                    outcome: crate::compaction::CompactionTerminalOutcome::Rejected(
                        crate::compaction::CompactionRejection::InvalidStructure,
                    ),
                },
            )
            .await?;
            return Err(CoreError::AutomaticCompaction { reason, message });
        }
        if !retained_messages.is_empty()
            && !replacement.messages.ends_with(retained_messages.as_slice())
        {
            let message = "automatic compactor did not preserve the requested intact recent suffix";
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::Failed {
                        message: message.into(),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::ReplacementProposed {
                    id: operation.id,
                    proposal: crate::compaction::CompactionProposalObservation {
                        replacement_message_count: replacement.messages.len(),
                        replacement_bytes: crate::compaction::messages_bytes(&replacement.messages),
                        estimated_context_tokens_after: None,
                        headroom_tokens: None,
                        structural_validation_passed: true,
                        retained_suffix_exact: false,
                        source_generation_matches: automatic_source_generation_matches(
                            agent,
                            source_history_revision,
                        ),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Terminal {
                    id: operation.id,
                    outcome: crate::compaction::CompactionTerminalOutcome::Rejected(
                        crate::compaction::CompactionRejection::RetainedSuffixMismatch,
                    ),
                },
            )
            .await?;
            return Err(CoreError::AutomaticCompaction {
                reason,
                message: message.into(),
            });
        }
        if let Some(rejection) =
            automatic_checkpoint_rejection(&replacement.messages, retained_messages.len())
        {
            let message = match rejection {
                crate::compaction::CompactionRejection::EmptyCheckpoint => {
                    "automatic compactor returned an empty checkpoint"
                }
                crate::compaction::CompactionRejection::UnexpectedToolCall => {
                    "automatic compactor checkpoint contained a tool call or result"
                }
                _ => unreachable!("checkpoint validation returns only checkpoint rejections"),
            };
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::Failed {
                        message: message.into(),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::ReplacementProposed {
                    id: operation.id,
                    proposal: crate::compaction::CompactionProposalObservation {
                        replacement_message_count: replacement.messages.len(),
                        replacement_bytes: crate::compaction::messages_bytes(&replacement.messages),
                        estimated_context_tokens_after: None,
                        headroom_tokens: None,
                        structural_validation_passed: true,
                        retained_suffix_exact: true,
                        source_generation_matches: automatic_source_generation_matches(
                            agent,
                            source_history_revision,
                        ),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Terminal {
                    id: operation.id,
                    outcome: crate::compaction::CompactionTerminalOutcome::Rejected(rejection),
                },
            )
            .await?;
            return Err(CoreError::AutomaticCompaction {
                reason,
                message: message.into(),
            });
        }
        let replacement_bytes = crate::compaction::messages_bytes(&replacement.messages);
        if replacement_bytes >= canonical_source_bytes {
            let message =
                "automatic compactor replacement did not strictly reduce canonical history";
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::Failed {
                        message: message.into(),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::ReplacementProposed {
                    id: operation.id,
                    proposal: crate::compaction::CompactionProposalObservation {
                        replacement_message_count: replacement.messages.len(),
                        replacement_bytes,
                        estimated_context_tokens_after: None,
                        headroom_tokens: None,
                        structural_validation_passed: true,
                        retained_suffix_exact: true,
                        source_generation_matches: automatic_source_generation_matches(
                            agent,
                            source_history_revision,
                        ),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Terminal {
                    id: operation.id,
                    outcome: crate::compaction::CompactionTerminalOutcome::Rejected(
                        crate::compaction::CompactionRejection::NonShrinkingReplacement,
                    ),
                },
            )
            .await?;
            return Err(CoreError::AutomaticCompaction {
                reason,
                message: message.into(),
            });
        }
        let estimated_replacement_tokens = estimate_messages_tokens(&replacement.messages);
        let replacement_headroom = policy
            .context_budget
            .tokens()
            .saturating_sub(estimated_replacement_tokens);
        if replacement_headroom < policy.minimum_headroom_tokens {
            let message = "automatic compactor replacement left insufficient working headroom";
            self.emit(
                agent,
                AgentEventKind::AutomaticCompactionEnd {
                    reason,
                    retry_provider_request,
                    outcome: AutomaticCompactionOutcome::Failed {
                        message: message.into(),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::ReplacementProposed {
                    id: operation.id,
                    proposal: crate::compaction::CompactionProposalObservation {
                        replacement_message_count: replacement.messages.len(),
                        replacement_bytes,
                        estimated_context_tokens_after: Some(estimated_replacement_tokens),
                        headroom_tokens: Some(replacement_headroom),
                        structural_validation_passed: true,
                        retained_suffix_exact: true,
                        source_generation_matches: automatic_source_generation_matches(
                            agent,
                            source_history_revision,
                        ),
                    },
                },
            )
            .await?;
            self.emit_compaction_lifecycle(
                agent,
                crate::compaction::CompactionLifecycleRecord::Terminal {
                    id: operation.id,
                    outcome: crate::compaction::CompactionTerminalOutcome::Rejected(
                        crate::compaction::CompactionRejection::InsufficientHeadroom,
                    ),
                },
            )
            .await?;
            return Err(CoreError::AutomaticCompaction {
                reason,
                message: message.into(),
            });
        }
        self.emit_compaction_lifecycle(
            agent,
            crate::compaction::CompactionLifecycleRecord::ReplacementProposed {
                id: operation.id,
                proposal: crate::compaction::CompactionProposalObservation {
                    replacement_message_count: replacement.messages.len(),
                    replacement_bytes,
                    estimated_context_tokens_after: Some(estimated_replacement_tokens),
                    headroom_tokens: Some(replacement_headroom),
                    structural_validation_passed: true,
                    retained_suffix_exact: true,
                    source_generation_matches: automatic_source_generation_matches(
                        agent,
                        source_history_revision,
                    ),
                },
            },
        )
        .await?;
        if let Err(error) = crate::compaction::commit_replacement(
            agent,
            self.id(),
            &self.cancellation,
            source_history_revision,
            replacement.messages,
        ) {
            if matches!(error, CoreError::Cancelled) {
                self.policy
                    .lock()
                    .expect("run policy mutex poisoned")
                    .compaction_cancelled = true;
                self.emit(
                    agent,
                    AgentEventKind::AutomaticCompactionEnd {
                        reason,
                        retry_provider_request,
                        outcome: AutomaticCompactionOutcome::Cancelled,
                    },
                )
                .await?;
                self.emit_compaction_lifecycle(
                    agent,
                    crate::compaction::CompactionLifecycleRecord::Terminal {
                        id: operation.id,
                        outcome: crate::compaction::CompactionTerminalOutcome::Cancelled,
                    },
                )
                .await?;
            } else if matches!(
                error,
                CoreError::Compaction(crate::compaction::CompactionError::StaleSource { .. })
            ) {
                self.emit_compaction_lifecycle(
                    agent,
                    crate::compaction::CompactionLifecycleRecord::Terminal {
                        id: operation.id,
                        outcome: crate::compaction::CompactionTerminalOutcome::Rejected(
                            crate::compaction::CompactionRejection::StaleSourceGeneration,
                        ),
                    },
                )
                .await?;
            }
            return Err(error);
        }
        self.emit_compaction_lifecycle(
            agent,
            crate::compaction::CompactionLifecycleRecord::Terminal {
                id: operation.id,
                outcome: crate::compaction::CompactionTerminalOutcome::Committed,
            },
        )
        .await?;
        estimator.reset_after_compaction();
        let estimated_tokens_after = estimator.estimate(agent);
        let still_above = estimated_tokens_after >= policy.threshold_tokens();
        if still_above {
            let message_count = {
                let state = agent.state.lock().expect("agent state mutex poisoned");
                state.messages.len()
            };
            self.policy
                .lock()
                .expect("run policy mutex poisoned")
                .compaction_blocked_message_count = Some(message_count);
        }
        self.emit(
            agent,
            AgentEventKind::AutomaticCompactionEnd {
                reason,
                retry_provider_request,
                outcome: if still_above {
                    AutomaticCompactionOutcome::StillAboveThreshold
                } else {
                    AutomaticCompactionOutcome::Succeeded {
                        estimated_tokens_after: Some(estimated_tokens_after),
                    }
                },
            },
        )
        .await?;
        Ok(true)
    }

    async fn emit_context_estimate(
        &self,
        agent: &AgentInner,
        request: &ModelRequest,
        estimated_context_tokens: u64,
    ) -> Result<(), CoreError> {
        let (message_count, message_bytes, tool_result_bytes) = {
            let state = agent.state.lock().expect("agent state mutex poisoned");
            (
                state.messages.len(),
                estimate_messages_bytes(&state.messages),
                estimate_tool_result_bytes(&state.messages),
            )
        };
        self.emit(
            agent,
            AgentEventKind::ContextEstimate {
                estimated_context_tokens: Some(estimated_context_tokens),
                input_bytes: request
                    .system_prompt
                    .len()
                    .saturating_add(request.context.len()),
                message_count,
                message_bytes,
                tool_result_bytes,
            },
        )
        .await?;
        Ok(())
    }

    async fn emit_compaction_lifecycle(
        &self,
        agent: &AgentInner,
        record: crate::compaction::CompactionLifecycleRecord,
    ) -> Result<(), CoreError> {
        self.emit(agent, AgentEventKind::CompactionLifecycle { record })
            .await
            .map(|_| ())
    }

    fn current_context(
        &self,
        agent: &AgentInner,
    ) -> Result<crate::hooks::ContextEnvelope, CoreError> {
        let state = agent.state.lock().expect("agent state mutex poisoned");
        if !matches!(
            state.phase,
            AgentPhase::Running(_) | AgentPhase::Cancelling(_)
        ) {
            return Err(CoreError::InvalidTransition(
                crate::error::StateTransitionError::new("agent", "not-running", "current_context"),
            ));
        }
        Ok(crate::hooks::ContextEnvelope {
            version: 1,
            messages: state.messages.clone(),
            host_messages: state.host_messages.clone(),
        })
    }

    fn drain_steering(&self, agent: &AgentInner) -> Vec<crate::queue::QueuedMessage> {
        let mode = *agent
            .steering_mode
            .lock()
            .expect("agent steering mode mutex poisoned");
        agent
            .queues
            .lock()
            .expect("agent queue mutex poisoned")
            .steering
            .drain(mode)
    }

    fn drain_follow_up(&self, agent: &AgentInner) -> Vec<crate::queue::QueuedMessage> {
        let mode = *agent
            .follow_up_mode
            .lock()
            .expect("agent follow-up mode mutex poisoned");
        agent
            .queues
            .lock()
            .expect("agent queue mutex poisoned")
            .follow_up
            .drain(mode)
    }

    async fn inject_queued_messages(
        &self,
        agent: &AgentInner,
        mut context: crate::hooks::ContextEnvelope,
        queued: Vec<crate::queue::QueuedMessage>,
    ) -> Result<Option<crate::hooks::ContextEnvelope>, CoreError> {
        if queued.is_empty() {
            // Preserve the context prepared by `prepare_next_turn`, even when
            // no user message is waiting. Tool continuations still consume
            // this replacement before the next model request.
            return Ok(Some(context));
        }
        for queued_message in queued {
            let message = {
                let mut state = agent.state.lock().expect("agent state mutex poisoned");
                let message = AgentMessage::User {
                    id: state.allocate_message_id(),
                    content: queued_message.content,
                };
                state.append_message(message.clone());
                message
            };
            context.messages.push(message.clone());
            self.emit(
                agent,
                AgentEventKind::MessageStart {
                    message: message.clone(),
                },
            )
            .await?;
            self.emit(agent, AgentEventKind::MessageEnd { message })
                .await?;
        }
        Ok(Some(context))
    }

    async fn consume_assistant_stream(
        &self,
        agent: &AgentInner,
        stream: &mut dyn ModelEventStream,
        turn_id: TurnId,
        model: Option<ModelDescriptor>,
    ) -> Result<(ProviderResponse, Option<u64>), CoreError> {
        let mut assistant_id = None;
        let mut assistant_text = String::new();
        let mut tool_calls = Vec::new();
        let mut reason = None;
        let mut error_message = None;
        let mut usage: Option<Usage> = None;
        let mut context_overflow = false;

        loop {
            let Some(item) =
                stream
                    .next_event(self.cancellation.clone())
                    .await
                    .map_err(|error| CoreError::ModelProvider {
                        message: error.to_string(),
                    })?
            else {
                break;
            };
            if reason.is_some() {
                return Err(CoreError::UnsupportedModelStream {
                    message: "model stream contained events after its terminal event".into(),
                });
            }
            match item {
                ModelStreamEvent::RequestObservation(observation) => {
                    self.emit(
                        agent,
                        AgentEventKind::ProviderRequestObserved {
                            turn_id,
                            observation,
                        },
                    )
                    .await?;
                }
                ModelStreamEvent::TextDelta(delta) => {
                    let (message, message_id, first_delta) = {
                        let mut state = agent.state.lock().expect("agent state mutex poisoned");
                        let first_delta = assistant_id.is_none();
                        let id = *assistant_id.get_or_insert_with(|| state.allocate_message_id());
                        if first_delta {
                            state.append_message(AgentMessage::Assistant {
                                id,
                                content: String::new(),
                                tool_calls: Vec::new(),
                                stop_reason: None,
                                error_message: None,
                            });
                        }
                        assistant_text.push_str(&delta);
                        state.partial_response = Some(assistant_text.clone());
                        let message = AgentMessage::Assistant {
                            id,
                            content: assistant_text.clone(),
                            tool_calls: Vec::new(),
                            stop_reason: None,
                            error_message: None,
                        };
                        state.replace_last_message(message.clone());
                        (message, id, first_delta)
                    };
                    if first_delta {
                        self.emit(
                            agent,
                            AgentEventKind::MessageStart {
                                message: AgentMessage::Assistant {
                                    id: message_id,
                                    content: String::new(),
                                    tool_calls: Vec::new(),
                                    stop_reason: None,
                                    error_message: None,
                                },
                            },
                        )
                        .await?;
                    }
                    self.emit(
                        agent,
                        AgentEventKind::MessageUpdate {
                            message,
                            text_delta: Some(delta),
                        },
                    )
                    .await?;
                }
                ModelStreamEvent::ToolCall(call) => tool_calls.push(call),
                ModelStreamEvent::Usage(update) => {
                    if let Some(current) = usage.as_mut() {
                        current.merge(update);
                    } else {
                        usage = Some(update);
                    }
                }
                ModelStreamEvent::Error { message } => {
                    reason = Some(StopReason::Error);
                    error_message = Some(message);
                }
                ModelStreamEvent::ContextOverflow { message } => {
                    reason = Some(StopReason::Error);
                    error_message = Some(message);
                    context_overflow = true;
                }
                ModelStreamEvent::Aborted { message } => {
                    reason = Some(StopReason::Aborted);
                    error_message = Some(message);
                }
                ModelStreamEvent::End(next_reason) => reason = Some(next_reason),
            }
        }

        let reason = reason.ok_or(CoreError::UnsupportedModelStream {
            message: "model stream ended without a terminal event".into(),
        })?;
        let response = ProviderResponse {
            stop_reason: reason,
            assistant_text: assistant_text.clone(),
            tool_calls: tool_calls.clone(),
            error_message: error_message.clone(),
            usage: usage.clone(),
            context_overflow,
        };
        let assistant = {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            let id = assistant_id.unwrap_or_else(|| state.allocate_message_id());
            let assistant = AgentMessage::Assistant {
                id,
                content: assistant_text,
                tool_calls: tool_calls.clone(),
                stop_reason: Some(reason),
                error_message: error_message.clone(),
            };
            state.partial_response = None;
            state.is_streaming = false;
            if assistant_id.is_some() {
                state.replace_last_message(assistant.clone());
            } else {
                state.append_message(assistant.clone());
            }
            assistant
        };
        if assistant_id.is_none() {
            self.emit(
                agent,
                AgentEventKind::MessageStart {
                    message: assistant.clone(),
                },
            )
            .await?;
        }
        self.emit(agent, AgentEventKind::MessageEnd { message: assistant })
            .await?;
        let valid_input_tokens = if matches!(reason, StopReason::Error | StopReason::Aborted) {
            None
        } else {
            usage
                .as_ref()
                .and_then(|usage| usage.input_tokens)
                .filter(|tokens| *tokens != 0)
        };
        if let Some(usage) = usage {
            let accounting = crate::state::ModelTurnAccounting {
                run_id: self.id(),
                turn_id,
                model,
                usage,
            };
            {
                let mut state = agent.state.lock().expect("agent state mutex poisoned");
                state.accounting.record(accounting.clone());
            }
            self.emit(agent, AgentEventKind::ModelTurnUsage { accounting })
                .await?;
        }
        Ok((response, valid_input_tokens))
    }

    /// Refuse tool calls from a length-truncated assistant response. The
    /// provider may have emitted syntactically plausible JSON after a partial
    /// argument stream, but upstream treats every such call as unsafe.
    async fn fail_truncated_tool_calls(
        &self,
        agent: &AgentInner,
        tool_calls: &[AgentToolCall],
    ) -> Result<bool, CoreError> {
        for assistant_call in tool_calls {
            let call = ToolCall {
                id: assistant_call.id.clone(),
                name: assistant_call.name.clone(),
                arguments: assistant_call.arguments.clone(),
            };
            {
                let mut state = agent.state.lock().expect("agent state mutex poisoned");
                state.pending_tool_calls.insert(call.id.clone());
            }
            self.emit(
                agent,
                AgentEventKind::ToolExecutionStart {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            )
            .await?;
            let result = error_tool_result(
                &call,
                format!(
                    "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    call.name
                ),
            );
            self.emit_tool_execution_end(agent, &call, &result).await?;
            self.append_tool_result_message(agent, call, result).await?;
        }
        Ok(false)
    }

    async fn emit_agent_end_and_succeed(
        &self,
        agent: &AgentInner,
        reason: StopReason,
    ) -> Result<(), CoreError> {
        let messages = self.new_messages(agent);
        self.emit(agent, AgentEventKind::AgentEnd { messages })
            .await?;
        self.succeed(reason)
    }

    async fn settle_cancellation(&self) {
        let Some(agent) = self.agent.upgrade() else {
            let _ = self.finish(RunPhase::Cancelled, StopReason::Aborted, None);
            return;
        };
        let turn_id = self.snapshot().turn_id.unwrap_or(TurnId(1));
        let failure = {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            state.partial_response = None;
            state.is_streaming = false;
            state.pending_tool_calls.clear();
            if matches!(
                state.messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Aborted),
                    ..
                })
            ) {
                None
            } else {
                let message = AgentMessage::Assistant {
                    id: state.allocate_message_id(),
                    content: String::new(),
                    tool_calls: Vec::new(),
                    stop_reason: Some(StopReason::Aborted),
                    error_message: Some("Operation aborted".into()),
                };
                state.append_message(message.clone());
                Some(message)
            }
        };
        if let Some(message) = failure {
            let _ = self
                .emit(
                    &agent,
                    AgentEventKind::MessageStart {
                        message: message.clone(),
                    },
                )
                .await;
            let _ = self
                .emit(&agent, AgentEventKind::MessageEnd { message })
                .await;
        }
        let _ = self
            .emit(
                &agent,
                AgentEventKind::TurnEnd {
                    turn_id,
                    reason: StopReason::Aborted,
                },
            )
            .await;
        let messages = self.new_messages(&agent);
        let _ = self
            .emit(&agent, AgentEventKind::AgentEnd { messages })
            .await;
        let _ = self.finish(RunPhase::Cancelled, StopReason::Aborted, None);
    }

    /// Return exactly the messages created by this run invocation.
    ///
    /// Pi's low-level loop returns `newMessages`, which includes the prompt
    /// supplied to a prompt run but excludes durable context supplied to a
    /// continuation run. The durable transcript remains available through an
    /// agent snapshot; `AgentEnd` is the invocation result.
    fn new_messages(&self, agent: &AgentInner) -> Vec<AgentMessage> {
        agent
            .state
            .lock()
            .expect("agent state mutex poisoned")
            .messages
            .get(self.message_start_index..)
            .unwrap_or_default()
            .to_vec()
    }

    /// Begin the first model turn.
    pub fn start_turn(&self, turn_id: TurnId) -> Result<(), CoreError> {
        let mut state = self.state.lock().expect("run state mutex poisoned");
        if state.phase != RunPhase::Created {
            return Err(CoreError::InvalidTransition(
                crate::error::StateTransitionError::new(
                    "run",
                    phase_name(state.phase),
                    "start_turn",
                ),
            ));
        }
        state.phase = RunPhase::Running;
        state.turn_id = Some(turn_id);
        Ok(())
    }

    fn advance_turn(&self, turn_id: TurnId) -> Result<(), CoreError> {
        let mut state = self.state.lock().expect("run state mutex poisoned");
        if state.phase != RunPhase::Running {
            return Err(CoreError::InvalidTransition(
                crate::error::StateTransitionError::new(
                    "run",
                    phase_name(state.phase),
                    "advance_turn",
                ),
            ));
        }
        state.turn_id = Some(turn_id);
        Ok(())
    }

    /// Request cancellation. This operation is idempotent after settlement.
    ///
    /// A running handle is settled by [`Self::drive`] so its terminal events
    /// remain ordered and observers remain awaited. An un-driven handle has no
    /// active caller-owned future, so it settles immediately.
    pub fn abort(&self) -> Result<(), CoreError> {
        self.cancellation.cancel();
        let mut state = self.state.lock().expect("run state mutex poisoned");
        if state.phase.is_terminal() {
            return Ok(());
        }
        let settle_immediately = state.phase == RunPhase::Created;
        if settle_immediately {
            state.phase = RunPhase::Cancelled;
            state.stop_reason = Some(StopReason::Cancelled);
        }
        drop(state);
        if settle_immediately {
            self.settle_agent(AgentPhase::Idle, None);
        } else if let Some(agent) = self.agent.upgrade() {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            state.phase = AgentPhase::Cancelling(self.id());
        }
        Ok(())
    }

    /// Enter observer settlement before selecting a terminal outcome.
    pub fn begin_settlement(&self) -> Result<(), CoreError> {
        let mut state = self.state.lock().expect("run state mutex poisoned");
        if state.phase.is_terminal() {
            return Err(CoreError::RunFinished { run_id: state.id });
        }
        state.phase = RunPhase::Settling;
        Ok(())
    }

    /// Settle a successful run and clear transient agent state first.
    pub fn succeed(&self, reason: StopReason) -> Result<(), CoreError> {
        self.finish(RunPhase::Succeeded, reason, None)
    }

    /// Settle a failed run and clear transient agent state first.
    pub fn fail(&self, message: impl Into<String>) -> Result<(), CoreError> {
        self.finish(RunPhase::Failed, StopReason::Error, Some(message.into()))
    }

    fn finish(
        &self,
        phase: RunPhase,
        reason: StopReason,
        error: Option<String>,
    ) -> Result<(), CoreError> {
        let mut state = self.state.lock().expect("run state mutex poisoned");
        if state.phase.is_terminal() {
            return Err(CoreError::RunFinished { run_id: state.id });
        }
        state.phase = phase;
        state.stop_reason = Some(reason);
        state.error = error.clone();
        let id = state.id;
        drop(state);
        self.settle_agent(AgentPhase::Idle, error);
        let _ = id;
        Ok(())
    }

    fn settle_agent(&self, phase: AgentPhase, error: Option<String>) {
        if let Some(agent) = self.agent.upgrade() {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            // Transient state is cleared before the agent becomes idle.  Durable messages are
            // intentionally retained for the next `continue` operation.
            state.partial_response = None;
            state.is_streaming = false;
            state.pending_tool_calls.clear();
            state.last_error = error;
            state.phase = phase;
            agent
                .active_run
                .lock()
                .expect("active run mutex poisoned")
                .take();
            drop(state);
            agent.idle_notifier.notify();
        }
    }

    /// Construct an event envelope using the run's next local sequence.
    pub fn event(&self, kind: AgentEventKind) -> AgentEvent {
        self.record_event(kind)
    }

    fn record_event(&self, kind: AgentEventKind) -> AgentEvent {
        let mut state = self.state.lock().expect("run state mutex poisoned");
        state.event_count = state.event_count.saturating_add(1);
        let event = AgentEvent {
            run_id: state.id,
            sequence: EventSequence(state.event_count),
            kind,
        };
        state.events.push(event.clone());
        event
    }

    pub(crate) async fn emit(
        &self,
        agent: &AgentInner,
        kind: AgentEventKind,
    ) -> Result<AgentEvent, CoreError> {
        let event = self.record_event(kind);

        // Lossless subscriptions use an explicitly caller-owned unbounded
        // queue. Publish this copy before awaited observers so a live host is
        // not held behind an arbitrary observer future. Sending to one cannot
        // drop for capacity and does not wait for the receiver to drain; a
        // disconnected receiver is cleaned up after this event.
        let lossless_subscribers = agent
            .lossless_subscribers
            .lock()
            .expect("lossless subscriber mutex poisoned")
            .clone();
        let mut disconnected = Vec::new();
        for registration in lossless_subscribers {
            if registration.sender.send(event.clone()).is_err() {
                disconnected.push(registration.id);
            }
        }
        if !disconnected.is_empty() {
            agent
                .lossless_subscribers
                .lock()
                .expect("lossless subscriber mutex poisoned")
                .retain(|registration| !disconnected.contains(&registration.id));
        }

        // Clone a registration snapshot before awaiting callbacks. This avoids
        // retaining the registry mutex across an await and defines reentrant
        // subscribe/unsubscribe precisely: changes apply to the next event.
        let observers = agent
            .observers
            .lock()
            .expect("observer mutex poisoned")
            .clone();
        for registration in observers {
            registration
                .observer
                .observe(&event, self.cancellation.clone())
                .await?;
        }
        // These subscriptions deliberately have a distinct contract from
        // awaited observers. Snapshot their registrations so a receiver can
        // drop itself while delivery is in progress, then use `try_send` so a
        // slow receiver never holds the run open.
        let subscribers = agent
            .subscribers
            .lock()
            .expect("subscriber mutex poisoned")
            .clone();
        let mut disconnected = Vec::new();
        for registration in subscribers {
            match registration.sender.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    registration
                        .dropped
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                Err(TrySendError::Disconnected(_)) => disconnected.push(registration.id),
            }
        }
        if !disconnected.is_empty() {
            agent
                .subscribers
                .lock()
                .expect("subscriber mutex poisoned")
                .retain(|registration| !disconnected.contains(&registration.id));
        }

        Ok(event)
    }

    pub(crate) fn settle_cancelled(&self) -> Result<(), CoreError> {
        self.finish(RunPhase::Cancelled, StopReason::Cancelled, None)
    }
}

/// Clone-and-curate the canonical conversation at the model boundary.
///
/// The canonical transcript remains raw. In-tree provider adapters currently
/// have no portable structured tool-details field, so details are encoded into
/// bounded marked text here while the `is_error` bit stays available to native
/// adapters such as Command Code.
fn project_model_context(
    mut context: crate::hooks::ContextEnvelope,
    policy: &crate::tool::ToolResultProjectionPolicy,
) -> crate::hooks::ContextEnvelope {
    let mut seen_error_payloads = BTreeMap::new();
    for message in &mut context.messages {
        if let AgentMessage::ToolResult {
            content,
            details,
            is_error,
            failure,
            ..
        } = message
        {
            let projection = project_tool_result_as_text(
                content,
                details.as_ref(),
                *is_error,
                failure.as_ref(),
                policy,
                &mut seen_error_payloads,
            );
            *content = projection.content;
            *details = None;
        }
    }
    context
}

fn automatic_compaction_split(
    messages: &[AgentMessage],
    recent_tokens: u64,
) -> (Vec<AgentMessage>, Vec<AgentMessage>, Vec<AgentMessage>) {
    if messages.is_empty() || recent_tokens == 0 {
        return (messages.to_vec(), Vec::new(), Vec::new());
    }
    let mut start = messages.len();
    let mut retained_tokens = 0_u64;
    while start > 0 && retained_tokens < recent_tokens {
        start -= 1;
        retained_tokens = retained_tokens.saturating_add(estimate_message_tokens(&messages[start]));
    }
    // A tool result cannot be the beginning of a retained transcript. Move to
    // the assistant that owns the retained result(s), which keeps every
    // assistant tool call paired with its result in the suffix.
    while start > 0 && matches!(messages[start], AgentMessage::ToolResult { .. }) {
        start -= 1;
    }
    if matches!(messages[start], AgentMessage::Assistant { .. })
        && let Some(turn_start) = (0..start)
            .rev()
            .find(|index| matches!(messages[*index], AgentMessage::User { .. }))
        {
            return (
                messages[..turn_start].to_vec(),
                messages[start..].to_vec(),
                messages[turn_start..start].to_vec(),
            );
        }
    (
        messages[..start].to_vec(),
        messages[start..].to_vec(),
        Vec::new(),
    )
}

fn estimate_messages_tokens(messages: &[AgentMessage]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

fn estimate_message_tokens(message: &AgentMessage) -> u64 {
    (estimate_message_bytes(message) as u64).saturating_add(3) / 4
}

fn estimate_messages_bytes(messages: &[AgentMessage]) -> usize {
    messages.iter().map(estimate_message_bytes).sum()
}

fn estimate_tool_result_bytes(messages: &[AgentMessage]) -> usize {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult {
                content, details, ..
            } => Some(
                content
                    .len()
                    .saturating_add(details.as_ref().map_or(0, |details| details.as_str().len())),
            ),
            _ => None,
        })
        .sum()
}

fn estimate_message_bytes(message: &AgentMessage) -> usize {
    match message {
        AgentMessage::User { content, .. } => content.len().saturating_add(16),
        AgentMessage::Assistant {
            content,
            tool_calls,
            error_message,
            ..
        } => content
            .len()
            .saturating_add(error_message.as_ref().map_or(0, String::len))
            .saturating_add(
                tool_calls
                    .iter()
                    .map(|call| {
                        call.id
                            .as_str()
                            .len()
                            .saturating_add(call.name.len())
                            .saturating_add(call.arguments.as_str().len())
                    })
                    .sum::<usize>(),
            )
            .saturating_add(32),
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            details,
            failure,
            ..
        } => tool_call_id
            .as_str()
            .len()
            .saturating_add(tool_name.len())
            .saturating_add(content.len())
            .saturating_add(details.as_ref().map_or(0, |details| details.as_str().len()))
            .saturating_add(
                failure
                    .as_ref()
                    .and_then(crate::tool::ToolFailure::recovery_guidance)
                    .map_or(0, str::len),
            )
            .saturating_add(32),
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        let should_abort = self
            .state
            .lock()
            .map(|state| !state.phase.is_terminal())
            .unwrap_or(false);
        if should_abort {
            let _ = self.abort();
        }
    }
}

enum ToolStep {
    Updates(Vec<PendingToolUpdate>),
    Completed {
        result: Result<AgentToolResult, crate::error::ToolError>,
        updates: Vec<PendingToolUpdate>,
    },
}

enum ParallelToolStep {
    /// The caller cancelled while at least one parallel capability was pending.
    Cancelled {
        updates: Vec<PendingToolUpdate>,
    },
    Updates(Vec<PendingToolUpdate>),
    Completed {
        completed: Box<CompletedToolExecution>,
        updates: Vec<PendingToolUpdate>,
    },
}

/// Poll one sequential tool until either its callback queue has work or its
/// future settles. A callback may happen from another thread while the tool is
/// pending; `PendingToolUpdates` wakes this poll so the event is not delayed
/// until settlement.
async fn next_tool_step<'a>(
    future: &mut ToolFuture<'a>,
    updates: &PendingToolUpdates,
    call_id: &ToolCallId,
    cancellation: &CancellationToken,
    allow_one_poll_after_cancellation: bool,
    cancellation_settlement_mode: crate::tool::CancellationSettlementMode,
) -> ToolStep {
    std::future::poll_fn(|context| {
        if cancellation.is_cancelled()
            && !allow_one_poll_after_cancellation
            && cancellation_settlement_mode == crate::tool::CancellationSettlementMode::DropFuture
        {
            updates.close(call_id);
            return Poll::Ready(ToolStep::Completed {
                result: Err(crate::error::ToolError::Cancelled {
                    tool: "cancelled operation".into(),
                }),
                updates: updates.take().unwrap_or_default(),
            });
        }
        if let Some(updates) = updates.take() {
            return Poll::Ready(ToolStep::Updates(updates));
        }
        updates.register_waker(context.waker());
        cancellation.register_waker(context.waker());
        // Close the check/register race: a callback arriving immediately
        // before registration must still be observed on this poll.
        if let Some(updates) = updates.take() {
            return Poll::Ready(ToolStep::Updates(updates));
        }
        if cancellation.is_cancelled()
            && !allow_one_poll_after_cancellation
            && cancellation_settlement_mode == crate::tool::CancellationSettlementMode::DropFuture
        {
            updates.close(call_id);
            return Poll::Ready(ToolStep::Completed {
                result: Err(crate::error::ToolError::Cancelled {
                    tool: "cancelled operation".into(),
                }),
                updates: updates.take().unwrap_or_default(),
            });
        }
        match future.as_mut().poll(context) {
            Poll::Ready(result) => {
                updates.close(call_id);
                Poll::Ready(ToolStep::Completed {
                    result,
                    updates: updates.take().unwrap_or_default(),
                })
            }
            Poll::Pending => {
                if let Some(updates) = updates.take() {
                    Poll::Ready(ToolStep::Updates(updates))
                } else if cancellation.is_cancelled()
                    && cancellation_settlement_mode
                        == crate::tool::CancellationSettlementMode::DropFuture
                {
                    updates.close(call_id);
                    Poll::Ready(ToolStep::Completed {
                        result: Err(crate::error::ToolError::Cancelled {
                            tool: "cancelled operation".into(),
                        }),
                        updates: Vec::new(),
                    })
                } else {
                    Poll::Pending
                }
            }
        }
    })
    .await
}

/// Poll one parallel batch until either a callback queue has work or one tool
/// settles. Updates found after each individual future poll are returned before
/// the next future is polled, which preserves callback-before-completion order.
async fn next_parallel_step<'a>(
    pending: &mut Vec<PendingToolExecution<'a>>,
    updates: &PendingToolUpdates,
    cancellation: &CancellationToken,
    allowed_after_cancellation: &mut BTreeSet<ToolCallId>,
) -> ParallelToolStep {
    std::future::poll_fn(|context| {
        if let Some(updates) = updates.take() {
            return Poll::Ready(ParallelToolStep::Updates(updates));
        }
        if cancellation.is_cancelled()
            && allowed_after_cancellation.is_empty()
            && !pending.iter().any(|pending_call| {
                pending_call.cancellation_settlement_mode
                    == crate::tool::CancellationSettlementMode::AwaitFuture
            })
        {
            return Poll::Ready(ParallelToolStep::Cancelled {
                updates: Vec::new(),
            });
        }
        updates.register_waker(context.waker());
        cancellation.register_waker(context.waker());
        if let Some(updates) = updates.take() {
            return Poll::Ready(ParallelToolStep::Updates(updates));
        }
        if cancellation.is_cancelled()
            && allowed_after_cancellation.is_empty()
            && !pending.iter().any(|pending_call| {
                pending_call.cancellation_settlement_mode
                    == crate::tool::CancellationSettlementMode::AwaitFuture
            })
        {
            return Poll::Ready(ParallelToolStep::Cancelled {
                updates: updates.take().unwrap_or_default(),
            });
        }
        let mut index = 0;
        while index < pending.len() {
            if cancellation.is_cancelled()
                && pending[index].cancellation_settlement_mode
                    == crate::tool::CancellationSettlementMode::DropFuture
                && !allowed_after_cancellation.remove(&pending[index].call.id)
            {
                index = index.saturating_add(1);
                continue;
            }
            if let Poll::Ready(result) = pending[index].future.as_mut().poll(context) {
                let pending = pending.swap_remove(index);
                updates.close(&pending.call.id);
                return Poll::Ready(ParallelToolStep::Completed {
                    completed: Box::new(CompletedToolExecution {
                        source_index: pending.source_index,
                        call: pending.call,
                        effect: pending.effect,
                        result,
                    }),
                    updates: updates.take().unwrap_or_default(),
                });
            }
            if let Some(updates) = updates.take() {
                return Poll::Ready(ParallelToolStep::Updates(updates));
            }
            index = index.saturating_add(1);
        }
        if cancellation.is_cancelled()
            && !pending.iter().any(|pending_call| {
                pending_call.cancellation_settlement_mode
                    == crate::tool::CancellationSettlementMode::AwaitFuture
            })
        {
            return Poll::Ready(ParallelToolStep::Cancelled {
                updates: updates.take().unwrap_or_default(),
            });
        }
        updates
            .take()
            .map(ParallelToolStep::Updates)
            .map_or(Poll::Pending, Poll::Ready)
    })
    .await
}

fn error_tool_result(call: &ToolCall, content: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id.clone(),
        content: content.into(),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: true,
        failure: Some(crate::tool::ToolFailure::recoverable()),
    }
}

fn automatic_operation(
    run_id: RunId,
    automatic_ordinal: u32,
    reason: crate::compaction::AutomaticCompactionReason,
    retry_provider_request: bool,
    overflow_retry_ordinal: Option<u32>,
    source_history_revision: u64,
    strategy: crate::compaction::CompactionStrategy,
) -> crate::compaction::CompactionOperation {
    crate::compaction::CompactionOperation {
        id: crate::compaction::CompactionId {
            run_id,
            ordinal: automatic_ordinal,
        },
        trigger: crate::compaction::CompactionTrigger::Automatic,
        reason: match reason {
            crate::compaction::AutomaticCompactionReason::Threshold => {
                crate::compaction::CompactionReason::Threshold
            }
            crate::compaction::AutomaticCompactionReason::Overflow => {
                crate::compaction::CompactionReason::ProviderOverflow
            }
        },
        phase: if retry_provider_request {
            crate::compaction::CompactionPhase::BetweenModelCalls
        } else {
            crate::compaction::CompactionPhase::BeforeModelRequest
        },
        strategy,
        source_history_revision,
        attempt: automatic_ordinal,
        automatic_ordinal: Some(automatic_ordinal),
        overflow_retry_ordinal,
        retry_provider_request,
    }
}

fn automatic_source_generation_matches(agent: &AgentInner, expected: u64) -> bool {
    agent
        .state
        .lock()
        .expect("agent state mutex poisoned")
        .history_revision
        == expected
}

fn automatic_checkpoint_rejection(
    replacement: &[AgentMessage],
    retained_suffix_len: usize,
) -> Option<crate::compaction::CompactionRejection> {
    let checkpoint_len = replacement.len().saturating_sub(retained_suffix_len);
    let checkpoint = &replacement[..checkpoint_len];
    if checkpoint.is_empty() {
        return Some(crate::compaction::CompactionRejection::EmptyCheckpoint);
    }
    let mut text_bytes = 0_usize;
    for message in checkpoint {
        match message {
            AgentMessage::User { content, .. } => {
                text_bytes = text_bytes.saturating_add(content.trim().len())
            }
            AgentMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if !tool_calls.is_empty() {
                    return Some(crate::compaction::CompactionRejection::UnexpectedToolCall);
                }
                text_bytes = text_bytes.saturating_add(content.trim().len());
            }
            AgentMessage::ToolResult { .. } => {
                return Some(crate::compaction::CompactionRejection::UnexpectedToolCall);
            }
        }
    }
    (text_bytes == 0).then_some(crate::compaction::CompactionRejection::EmptyCheckpoint)
}

fn tool_error_message(error: crate::error::ToolError) -> String {
    match error {
        crate::error::ToolError::InvalidArguments { message, .. }
        | crate::error::ToolError::Execution { message, .. }
        | crate::error::ToolError::Classified { message, .. } => message,
        crate::error::ToolError::Blocked { reason, .. } => reason,
        crate::error::ToolError::Cancelled { .. } => "Operation aborted".into(),
    }
}

fn error_tool_result_from_error(
    call: &ToolCall,
    error: crate::error::ToolError,
) -> AgentToolResult {
    let failure = match &error {
        crate::error::ToolError::InvalidArguments { .. } => {
            Some(crate::tool::ToolFailure::invalid_arguments())
        }
        crate::error::ToolError::Classified { failure, .. } => Some(failure.clone()),
        crate::error::ToolError::Cancelled { .. } => Some(crate::tool::ToolFailure::cancelled()),
        crate::error::ToolError::Blocked { .. } | crate::error::ToolError::Execution { .. } => {
            Some(crate::tool::ToolFailure::recoverable())
        }
    };
    let mut result = error_tool_result(call, tool_error_message(error));
    result.failure = failure;
    result
}

fn apply_after_tool_call(result: &mut AgentToolResult, after: AfterToolCall) {
    if let Replacement::Replace(content) = after.content {
        result.content = content;
    }
    if let Replacement::Replace(details) = after.details {
        result.details = details;
    }
    if let Replacement::Replace(usage) = after.usage {
        result.usage = Some(usage);
    }
    if let Replacement::Replace(added_tool_names) = after.added_tool_names {
        result.added_tool_names = added_tool_names;
    }
    if let Some(terminate) = after.terminate {
        result.terminate = terminate;
    }
    if let Replacement::Replace(is_error) = after.is_error {
        result.is_error = is_error;
        if !is_error {
            result.failure = None;
        }
    }
    if let Replacement::Replace(failure) = after.failure {
        result.failure = failure;
    }
}

fn phase_name(phase: RunPhase) -> &'static str {
    match phase {
        RunPhase::Created => "created",
        RunPhase::Running => "running",
        RunPhase::Settling => "settling",
        RunPhase::Succeeded => "succeeded",
        RunPhase::Failed => "failed",
        RunPhase::Cancelled => "cancelled",
    }
}
