//! Optional adapter from core lifecycle events to the compact trace contract.
//!
//! Enable the `trace` feature to use [`TraceObserver`].  The adapter consumes
//! events after the core reducer has applied them and writes one linear
//! episode to a caller-owned [`tea_trace::TraceSink`].  It does not own a
//! clock, executor, task, or runtime state transition.
//!
//! Trace sinks are wrapped in [`tea_trace::IsolatedSink`].  A sink failure
//! is therefore observable through [`TraceObserver::failed_events`] but cannot
//! change the agent result.  The compact trace records the exact serialized
//! arguments from the pre-dispatch tool-start event, so callers must wrap
//! their sink in [`tea_trace::RedactingSink`] before persistence.

use crate::effect::RunProvenance;
use crate::event::{AgentEvent, AgentEventKind, EventObserver, ObserverFuture};
use crate::scheduler::CancellationToken;
use crate::state::{AgentMessage, MessageId, StopReason, ToolCallId};
use std::collections::BTreeMap;
use std::sync::Mutex;
use tea_trace::{
    CacheEvidence, Compaction, CompactionStage, EndReason, EpisodeEnd, EpisodeHeader, IsolatedSink,
    Tool, TraceEvent, TraceProvenance, TraceSink, Turn,
};

/// An awaited core observer that records a compact linear trace episode.
///
/// One observer may be attached to an agent and reused for multiple runs.  A
/// new [`AgentEventKind::AgentStart`] starts a new episode in the supplied
/// sink.  The episode identifier is supplied by the host because the core
/// does not own session or persistence identity.
pub struct TraceObserver<S> {
    episode_id: String,
    provenance: TraceProvenance,
    provider_surface_digest: Option<String>,
    state: Mutex<TraceState<S>>,
}

struct TraceState<S> {
    sink: IsolatedSink<S>,
    current_turn: Option<PendingTurn>,
    pending_tools: BTreeMap<ToolCallId, Tool>,
    pending_cache_evidence: BTreeMap<u64, CacheEvidence>,
    last_committed_compaction: Option<String>,
    end_reason: EndReason,
    error: Option<String>,
}

struct PendingTurn {
    index: u32,
    core_turn_id: u64,
    input: String,
    output: Option<String>,
    last_input_message: Option<MessageId>,
    cache_evidence: Option<CacheEvidence>,
}

impl<S: TraceSink> TraceObserver<S> {
    /// Creates an observer writing to `sink` under the host-assigned episode ID.
    pub fn new(episode_id: impl Into<String>, sink: S) -> Self {
        Self::new_with_provenance(episode_id, RunProvenance::default(), sink)
    }

    /// Creates an observer with the exact host-owned attribution carried by
    /// the core run's effect boundary. The observer remains passive: it
    /// copies only identifier strings into the trace header and never owns
    /// the durable session or experiment state behind them.
    pub fn new_with_provenance(
        episode_id: impl Into<String>,
        provenance: RunProvenance,
        sink: S,
    ) -> Self {
        let provider_surface_digest = provenance.provider_surface_digest.clone();
        Self {
            episode_id: episode_id.into(),
            provenance: trace_provenance(provenance),
            provider_surface_digest,
            state: Mutex::new(TraceState {
                sink: IsolatedSink::new(sink),
                current_turn: None,
                pending_tools: BTreeMap::new(),
                pending_cache_evidence: BTreeMap::new(),
                last_committed_compaction: None,
                end_reason: EndReason::Completed,
                error: None,
            }),
        }
    }

    /// Number of events rejected by the wrapped sink.
    pub fn failed_events(&self) -> u64 {
        self.state
            .lock()
            .expect("trace observer mutex poisoned")
            .sink
            .failed_events()
    }

    /// Inspect the caller-owned sink without exposing the adapter's state.
    ///
    /// The callback runs while the observer lock is held and should not call
    /// back into the agent.
    pub fn with_sink<R>(&self, inspect: impl FnOnce(&S) -> R) -> R {
        let state = self.state.lock().expect("trace observer mutex poisoned");
        inspect(state.sink.inner())
    }
}

impl<S: TraceSink + Send + 'static> EventObserver for TraceObserver<S> {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: CancellationToken,
    ) -> ObserverFuture<'a> {
        self.record(event);
        Box::pin(std::future::ready(Ok(())))
    }
}

impl<S: TraceSink> TraceObserver<S> {
    fn record(&self, event: &AgentEvent) {
        let mut state = self.state.lock().expect("trace observer mutex poisoned");
        match &event.kind {
            AgentEventKind::CompactionLifecycle { record } => {
                let (compaction, committed) = trace_compaction(record);
                if committed {
                    state.last_committed_compaction = Some(compaction.compaction_id.clone());
                }
                state.sink.append(TraceEvent::from(compaction));
            }
            AgentEventKind::ProviderRequestObserved {
                turn_id,
                observation,
            } => {
                let mut evidence = evidence_from_request_observation(observation);
                evidence.provider_surface_digest = self.provider_surface_digest.clone();
                record_cache_evidence(&mut state, turn_id.0, evidence);
                if let Some(compaction_id) = state.last_committed_compaction.take() {
                    let mut compaction = Compaction::new(
                        compaction_id,
                        CompactionStage::PostCompactionRequestObserved,
                    );
                    compaction.post_compaction_turn_index = Some(trace_turn_index(turn_id.0));
                    compaction.serialized_request_bytes = observation.serialized_request_bytes;
                    compaction.cache_domain_fingerprint = observation.cache_domain_fingerprint;
                    state.sink.append(TraceEvent::from(compaction));
                }
            }
            AgentEventKind::CompactionStart { .. }
            | AgentEventKind::CompactionResult { .. }
            | AgentEventKind::CompactionEnd { .. } => {
                // The compact v1 trace schema has no context-replacement record. The
                // authoritative lifecycle event stream remains available to the host.
            }
            AgentEventKind::AutomaticCompactionStart { .. }
            | AgentEventKind::ContextEstimate { .. }
            | AgentEventKind::ProviderRequestSkipped { .. } => {
                // These are structured observability events. The compact v1
                // trace remains content-oriented; hosts that need policy
                // metrics retain the core lifecycle stream.
            }
            AgentEventKind::AutomaticCompactionEnd { outcome, .. } => {
                if let crate::event::AutomaticCompactionOutcome::Failed { message } = outcome {
                    state.error = Some(message.clone());
                }
            }
            AgentEventKind::AgentStart => {
                state.current_turn = None;
                state.pending_tools.clear();
                state.pending_cache_evidence.clear();
                state.last_committed_compaction = None;
                state.end_reason = EndReason::Completed;
                state.error = None;
                state.sink.append(TraceEvent::from(
                    EpisodeHeader::new(self.episode_id.clone())
                        .with_provenance(self.provenance.clone()),
                ));
            }
            AgentEventKind::TurnStart { turn_id } => {
                state.current_turn = Some(PendingTurn {
                    index: trace_turn_index(turn_id.0),
                    core_turn_id: turn_id.0,
                    input: String::new(),
                    output: None,
                    last_input_message: None,
                    cache_evidence: state.pending_cache_evidence.remove(&turn_id.0),
                });
            }
            AgentEventKind::MessageStart { message }
            | AgentEventKind::MessageUpdate { message, .. }
            | AgentEventKind::MessageEnd { message } => {
                if let AgentMessage::Assistant { error_message, .. } = message {
                    state.error = error_message.clone();
                }
                record_message(&mut state.current_turn, message);
            }
            AgentEventKind::ToolExecutionStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                let turn_index = state.current_turn.as_ref().map_or(0, |turn| turn.index);
                state.pending_tools.insert(
                    tool_call_id.clone(),
                    Tool::new(
                        turn_index,
                        tool_call_id.to_string(),
                        tool_name.clone(),
                        arguments.as_str(),
                    ),
                );
            }
            AgentEventKind::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
            } => {
                let turn_index = state.current_turn.as_ref().map_or(0, |turn| turn.index);
                let tool = state.pending_tools.remove(tool_call_id).unwrap_or_else(|| {
                    Tool::new(turn_index, tool_call_id.to_string(), tool_name.clone(), "")
                });
                let tool = if result.is_error {
                    tool.with_error(result.content.clone())
                } else {
                    tool.with_output(result.content.clone())
                };
                state.sink.append(TraceEvent::from(tool));
            }
            AgentEventKind::ToolExecutionUpdate { .. } => {
                // The compact v1 trace stores the settled tool record only.
                // Streaming updates remain available through core observers.
            }
            AgentEventKind::ToolFailureObserved {
                terminal: true,
                message,
                ..
            } => state.error = Some(message.clone()),
            AgentEventKind::ToolFailureObserved { .. } => {}
            AgentEventKind::TurnEnd { turn_id, reason } => {
                let pending = state.current_turn.take().unwrap_or(PendingTurn {
                    index: trace_turn_index(turn_id.0),
                    core_turn_id: turn_id.0,
                    input: String::new(),
                    output: None,
                    last_input_message: None,
                    cache_evidence: state.pending_cache_evidence.remove(&turn_id.0),
                });
                let mut turn = Turn::new(pending.index, pending.input)
                    .with_stop_reason(stop_reason_name(*reason));
                if let Some(output) = pending.output {
                    turn = turn.with_output(output);
                }
                if let Some(cache_evidence) = pending.cache_evidence {
                    turn = turn.with_cache_evidence(cache_evidence);
                }
                state.sink.append(TraceEvent::from(turn));
                state.end_reason = end_reason(*reason);
            }
            AgentEventKind::ModelTurnUsage { accounting } => {
                let evidence = CacheEvidence {
                    provider_cache_read_tokens: accounting.usage.cache_read_tokens,
                    provider_cache_write_tokens: accounting.usage.cache_write_tokens,
                    provider_surface_digest: self.provider_surface_digest.clone(),
                    ..CacheEvidence::default()
                };
                record_cache_evidence(&mut state, accounting.turn_id.0, evidence);
            }
            AgentEventKind::AgentEnd { .. } => {
                let reason = state.end_reason.clone();
                let error = state.error.clone();
                state.sink.append(TraceEvent::from(EpisodeEnd {
                    reason,
                    error,
                    finished_at_ms: None,
                }));
            }
        }
    }
}

fn trace_provenance(provenance: RunProvenance) -> TraceProvenance {
    TraceProvenance {
        session_id: provenance.session_id,
        lane_id: provenance.lane_id,
        operation_id: provenance.operation_id,
        epoch_id: provenance.epoch_id,
        core_run_id: provenance.core_run_id,
        harness_snapshot_id: provenance.harness_snapshot_id,
        harness_revision_id: provenance.harness_revision_id,
        model_harness_profile_id: provenance.model_harness_profile_id,
        experiment_id: provenance.experiment_id,
    }
}

fn evidence_from_request_observation(
    observation: &crate::scheduler::AdapterRequestObservation,
) -> CacheEvidence {
    // Adapters intentionally do not expose request bodies to the trace
    // observer. The core contributes only a content-free deterministic prefix
    // measurement at request construction; provider usage is joined below by
    // exact turn ID and remains separate evidence.
    CacheEvidence {
        deterministic_common_prefix_bytes: observation.deterministic_common_prefix_bytes,
        deterministic_common_prefix_tokens_estimate: observation
            .deterministic_common_prefix_tokens_estimate,
        ..CacheEvidence::default()
    }
}

fn record_cache_evidence<S>(state: &mut TraceState<S>, turn_id: u64, evidence: CacheEvidence) {
    if cache_evidence_is_empty(&evidence) {
        return;
    }
    if let Some(current) = state
        .current_turn
        .as_mut()
        .filter(|turn| turn.core_turn_id == turn_id)
    {
        merge_cache_evidence(&mut current.cache_evidence, evidence);
        return;
    }
    let pending = state.pending_cache_evidence.entry(turn_id).or_default();
    merge_cache_evidence_value(pending, evidence);
}

fn merge_cache_evidence(target: &mut Option<CacheEvidence>, update: CacheEvidence) {
    if let Some(existing) = target {
        merge_cache_evidence_value(existing, update);
    } else {
        *target = Some(update);
    }
}

fn merge_cache_evidence_value(target: &mut CacheEvidence, update: CacheEvidence) {
    if update.deterministic_common_prefix_bytes.is_some() {
        target.deterministic_common_prefix_bytes = update.deterministic_common_prefix_bytes;
    }
    if update.deterministic_common_prefix_tokens_estimate.is_some() {
        target.deterministic_common_prefix_tokens_estimate =
            update.deterministic_common_prefix_tokens_estimate;
    }
    if update.provider_cache_read_tokens.is_some() {
        target.provider_cache_read_tokens = update.provider_cache_read_tokens;
    }
    if update.provider_cache_write_tokens.is_some() {
        target.provider_cache_write_tokens = update.provider_cache_write_tokens;
    }
    if update.provider_surface_digest.is_some() {
        target.provider_surface_digest = update.provider_surface_digest;
    }
}

fn cache_evidence_is_empty(evidence: &CacheEvidence) -> bool {
    evidence.deterministic_common_prefix_bytes.is_none()
        && evidence
            .deterministic_common_prefix_tokens_estimate
            .is_none()
        && evidence.provider_cache_read_tokens.is_none()
        && evidence.provider_cache_write_tokens.is_none()
        && evidence.provider_surface_digest.is_none()
}

/// Translates a core lifecycle record into the additive V1 trace record.
///
/// Keep this projection deliberately content-free: the core record has no raw
/// checkpoint, prompt, request body, arguments, or tool-result content, and
/// this adapter only copies scalar facts from it.
fn trace_compaction(record: &crate::compaction::CompactionLifecycleRecord) -> (Compaction, bool) {
    use crate::compaction::CompactionLifecycleRecord;

    match record {
        CompactionLifecycleRecord::Started { operation } => {
            let mut trace = Compaction::new(operation.id.to_string(), CompactionStage::Started);
            trace.trigger = Some(compaction_trigger_name(operation.trigger).into());
            trace.reason = Some(compaction_reason_name(operation.reason).into());
            trace.phase = Some(compaction_phase_name(operation.phase).into());
            trace.strategy_id = Some(operation.strategy.id.clone());
            trace.strategy_schema_version = Some(operation.strategy.schema_version);
            trace.request_layout =
                Some(compaction_layout_name(operation.strategy.request_layout).into());
            trace.prompt_fingerprint = operation.strategy.prompt_fingerprint;
            trace.source_history_revision = Some(operation.source_history_revision);
            trace.attempt = Some(operation.attempt);
            trace.automatic_ordinal = operation.automatic_ordinal;
            trace.overflow_retry_ordinal = operation.overflow_retry_ordinal;
            trace.retry_provider_request = Some(operation.retry_provider_request);
            (trace, false)
        }
        CompactionLifecycleRecord::SourceSelected { id, source } => {
            let mut trace = Compaction::new(id.to_string(), CompactionStage::SourceSelected);
            trace.source_message_count = Some(source.canonical_message_count);
            trace.source_message_bytes = Some(source.canonical_message_bytes);
            trace.retained_message_count = Some(source.retained_message_ids.len());
            trace.retained_suffix_bytes = Some(source.retained_suffix_bytes);
            trace.tool_result_bytes = Some(source.tool_result_bytes);
            (trace, false)
        }
        CompactionLifecycleRecord::RequestPrepared { id, request } => {
            let mut trace = Compaction::new(id.to_string(), CompactionStage::RequestPrepared);
            trace.request_layout = Some(compaction_layout_name(request.layout).into());
            trace.compactor_context_bytes = request.provider_context_bytes;
            trace.compactor_tool_count = request.tool_count;
            trace.tools_execution_prohibited = Some(request.tools_execution_prohibited);
            trace.source_is_active_context_prefix = request.source_is_active_context_prefix;
            (trace, false)
        }
        CompactionLifecycleRecord::ProviderUsageObserved {
            id,
            usage,
            request_observation,
            request,
        } => {
            let mut trace = Compaction::new(id.to_string(), CompactionStage::ProviderUsageObserved);
            if let Some(usage) = usage {
                trace.provider_input_tokens = usage.input_tokens;
                trace.provider_output_tokens = usage.output_tokens;
                trace.provider_cache_read_tokens = usage.cache_read_tokens;
                trace.provider_cache_write_tokens = usage.cache_write_tokens;
            }
            if let Some(observation) = request_observation {
                trace.serialized_request_bytes = observation.serialized_request_bytes;
                trace.cache_domain_fingerprint = observation.cache_domain_fingerprint;
            }
            if let Some(request) = request {
                trace.request_layout = Some(compaction_layout_name(request.layout).into());
                trace.source_is_active_context_prefix = request.source_is_active_context_prefix;
            }
            (trace, false)
        }
        CompactionLifecycleRecord::ReplacementProposed { id, proposal } => {
            let mut trace = Compaction::new(id.to_string(), CompactionStage::ReplacementProposed);
            trace.replacement_message_count = Some(proposal.replacement_message_count);
            trace.replacement_bytes = Some(proposal.replacement_bytes);
            trace.estimated_context_tokens_after = proposal.estimated_context_tokens_after;
            trace.headroom_tokens = proposal.headroom_tokens;
            trace.structural_validation_passed = Some(proposal.structural_validation_passed);
            trace.retained_suffix_exact = Some(proposal.retained_suffix_exact);
            trace.source_generation_matches = Some(proposal.source_generation_matches);
            (trace, false)
        }
        CompactionLifecycleRecord::Terminal { id, outcome } => {
            let mut trace = Compaction::new(id.to_string(), CompactionStage::Terminal);
            trace.terminal_outcome = Some(compaction_terminal_outcome_name(outcome).into());
            let committed = matches!(
                outcome,
                crate::compaction::CompactionTerminalOutcome::Committed
            );
            (trace, committed)
        }
    }
}

fn compaction_trigger_name(trigger: crate::compaction::CompactionTrigger) -> &'static str {
    match trigger {
        crate::compaction::CompactionTrigger::Manual => "manual",
        crate::compaction::CompactionTrigger::Automatic => "automatic",
    }
}

fn compaction_reason_name(reason: crate::compaction::CompactionReason) -> &'static str {
    match reason {
        crate::compaction::CompactionReason::UserRequest => "user_request",
        crate::compaction::CompactionReason::Threshold => "threshold",
        crate::compaction::CompactionReason::ProviderOverflow => "provider_overflow",
    }
}

fn compaction_phase_name(phase: crate::compaction::CompactionPhase) -> &'static str {
    match phase {
        crate::compaction::CompactionPhase::Standalone => "standalone",
        crate::compaction::CompactionPhase::BeforeModelRequest => "before_model_request",
        crate::compaction::CompactionPhase::BetweenModelCalls => "between_model_calls",
    }
}

fn compaction_layout_name(layout: crate::compaction::CompactionRequestLayout) -> &'static str {
    match layout {
        crate::compaction::CompactionRequestLayout::ExactReplay => "exact_replay",
        crate::compaction::CompactionRequestLayout::StandaloneFallback => "standalone_fallback",
        crate::compaction::CompactionRequestLayout::Unobserved => "unobserved",
    }
}

fn compaction_terminal_outcome_name(
    outcome: &crate::compaction::CompactionTerminalOutcome,
) -> &'static str {
    use crate::compaction::{CompactionRejection, CompactionTerminalOutcome};

    match outcome {
        CompactionTerminalOutcome::Committed => "committed",
        CompactionTerminalOutcome::Rejected(CompactionRejection::StaleSourceGeneration) => {
            "rejected_stale_source_generation"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::RetainedSuffixMismatch) => {
            "rejected_retained_suffix_mismatch"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::InvalidStructure) => {
            "rejected_invalid_structure"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::EmptyCheckpoint) => {
            "rejected_empty_checkpoint"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::UnexpectedToolCall) => {
            "rejected_unexpected_tool_call"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::NonShrinkingReplacement) => {
            "rejected_non_shrinking_replacement"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::InsufficientHeadroom) => {
            "rejected_insufficient_headroom"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::PolicyCapReached) => {
            "rejected_policy_cap_reached"
        }
        CompactionTerminalOutcome::Rejected(CompactionRejection::Cancelled) => "rejected_cancelled",
        CompactionTerminalOutcome::Rejected(CompactionRejection::TimedOut) => "rejected_timed_out",
        CompactionTerminalOutcome::Failed => "failed",
        CompactionTerminalOutcome::Cancelled => "cancelled",
        CompactionTerminalOutcome::TimedOut => "timed_out",
        CompactionTerminalOutcome::Unavailable => "unavailable",
    }
}

fn record_message(turn: &mut Option<PendingTurn>, message: &AgentMessage) {
    let Some(turn) = turn.as_mut() else {
        return;
    };
    match message {
        AgentMessage::User { id, content } if turn.last_input_message != Some(*id) => {
            if !turn.input.is_empty() {
                turn.input.push('\n');
            }
            turn.input.push_str(content);
            turn.last_input_message = Some(*id);
        }
        AgentMessage::User { .. } => {}
        AgentMessage::Assistant { content, .. } => {
            turn.output = Some(content.clone());
        }
        AgentMessage::ToolResult { .. } => {}
    }
}

fn trace_turn_index(turn_id: u64) -> u32 {
    turn_id.saturating_sub(1).min(u32::MAX as u64) as u32
}

fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::Length => "length",
        StopReason::Aborted => "aborted",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}

fn end_reason(reason: StopReason) -> EndReason {
    match reason {
        StopReason::Stop | StopReason::ToolUse | StopReason::Length => EndReason::Completed,
        StopReason::Aborted => EndReason::Aborted,
        StopReason::Cancelled => EndReason::Cancelled,
        StopReason::Error => EndReason::Failed,
    }
}

impl<S: TraceSink> std::fmt::Debug for TraceObserver<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TraceObserver")
            .field("episode_id", &self.episode_id)
            .field("failed_events", &self.failed_events())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::{
        CompactionId, CompactionLifecycleRecord, CompactionOperation, CompactionPhase,
        CompactionReason, CompactionStrategy, CompactionTerminalOutcome, CompactionTrigger,
    };
    use crate::event::AgentEvent;
    use crate::scheduler::AdapterRequestObservation;
    use crate::state::{AgentToolCall, MessageId, ModelTurnAccounting, RunId, TurnId, Usage};
    use crate::tool::{AgentToolResult, ToolUpdate};

    fn observe<S: TraceSink + Send + 'static>(observer: &TraceObserver<S>, kind: AgentEventKind) {
        let event = AgentEvent {
            run_id: RunId(1),
            sequence: crate::event::EventSequence(1),
            kind,
        };
        smol::block_on(observer.observe(&event, CancellationToken::new()))
            .expect("trace observer is best effort");
    }

    #[test]
    fn maps_lifecycle_events_to_one_linear_episode() {
        let observer = TraceObserver::new("episode-1", Vec::<TraceEvent>::new());
        let call_id = ToolCallId::new("call-1").expect("fixed ID");
        observe(&observer, AgentEventKind::AgentStart);
        observe(&observer, AgentEventKind::TurnStart { turn_id: TurnId(1) });
        observe(
            &observer,
            AgentEventKind::MessageStart {
                message: AgentMessage::User {
                    id: MessageId(1),
                    content: "hello".into(),
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::MessageEnd {
                message: AgentMessage::User {
                    id: MessageId(1),
                    content: "hello".into(),
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::MessageEnd {
                message: AgentMessage::Assistant {
                    id: MessageId(2),
                    content: "world".into(),
                    tool_calls: vec![AgentToolCall {
                        id: call_id.clone(),
                        name: "echo".into(),
                        arguments: crate::state::SerializedJson::new("{}"),
                    }],
                    stop_reason: Some(StopReason::ToolUse),
                    error_message: None,
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::ToolExecutionStart {
                tool_call_id: call_id.clone(),
                tool_name: "echo".into(),
                arguments: crate::state::SerializedJson::new(r#"{"secret":"value"}"#),
            },
        );
        observe(
            &observer,
            AgentEventKind::ToolExecutionUpdate {
                tool_call_id: call_id.clone(),
                tool_name: "echo".into(),
                update: ToolUpdate {
                    content: "partial".into(),
                    details: Some(crate::state::SerializedJson::new("null")),
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::ToolExecutionEnd {
                tool_call_id: call_id,
                tool_name: "echo".into(),
                result: AgentToolResult {
                    tool_call_id: ToolCallId::new("call-1").expect("fixed ID"),
                    content: "result".into(),
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::TurnEnd {
                turn_id: TurnId(1),
                reason: StopReason::ToolUse,
            },
        );
        observe(&observer, AgentEventKind::AgentEnd { messages: vec![] });

        observer.with_sink(|events| {
            assert_eq!(events.len(), 4);
            assert!(matches!(events[0], TraceEvent::EpisodeHeader(_)));
            assert!(matches!(events[1], TraceEvent::Tool(_)));
            assert!(matches!(events[2], TraceEvent::Turn(_)));
            assert!(matches!(events[3], TraceEvent::EpisodeEnd(_)));
            let TraceEvent::Tool(tool) = &events[1] else {
                unreachable!()
            };
            assert_eq!(tool.input, r#"{"secret":"value"}"#);
            assert_eq!(tool.output.as_deref(), Some("result"));
            let TraceEvent::Turn(turn) = &events[2] else {
                unreachable!()
            };
            assert_eq!(turn.input, "hello");
            assert_eq!(turn.output.as_deref(), Some("world"));
        });
    }

    #[test]
    fn compaction_trace_records_are_content_free_and_join_the_next_request() {
        let observer = TraceObserver::new("episode-compact", Vec::<TraceEvent>::new());
        let id = CompactionId {
            run_id: RunId(1),
            ordinal: 1,
        };
        observe(&observer, AgentEventKind::AgentStart);
        observe(
            &observer,
            AgentEventKind::CompactionLifecycle {
                record: CompactionLifecycleRecord::Started {
                    operation: CompactionOperation {
                        id,
                        trigger: CompactionTrigger::Automatic,
                        reason: CompactionReason::Threshold,
                        phase: CompactionPhase::BeforeModelRequest,
                        strategy: CompactionStrategy::cache_replay_summary_v1(42),
                        source_history_revision: 9,
                        attempt: 1,
                        automatic_ordinal: Some(1),
                        overflow_retry_ordinal: None,
                        retry_provider_request: false,
                    },
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::CompactionLifecycle {
                record: CompactionLifecycleRecord::Terminal {
                    id,
                    outcome: CompactionTerminalOutcome::Committed,
                },
            },
        );
        observe(&observer, AgentEventKind::TurnStart { turn_id: TurnId(2) });
        observe(
            &observer,
            AgentEventKind::ProviderRequestObserved {
                turn_id: TurnId(2),
                observation: AdapterRequestObservation {
                    serialized_request_bytes: Some(321),
                    cache_domain_fingerprint: Some(123),
                    ..AdapterRequestObservation::default()
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::AgentEnd {
                messages: Vec::new(),
            },
        );

        observer.with_sink(|events| {
            let compactions: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    TraceEvent::Compaction(compaction) => Some(compaction),
                    _ => None,
                })
                .collect();
            assert_eq!(compactions.len(), 3);
            assert!(compactions
                .iter()
                .all(|record| record.compaction_id == id.to_string()));
            assert_eq!(
                compactions[0].strategy_id.as_deref(),
                Some("cache_replay_summary_v1")
            );
            assert_eq!(
                compactions[1].terminal_outcome.as_deref(),
                Some("committed")
            );
            assert_eq!(
                compactions[2].stage,
                CompactionStage::PostCompactionRequestObserved
            );
            assert_eq!(compactions[2].post_compaction_turn_index, Some(1));
            assert_eq!(compactions[2].serialized_request_bytes, Some(321));
        });
    }

    #[test]
    fn sink_failures_are_isolated_from_event_observation() {
        struct FailingSink;
        impl TraceSink for FailingSink {
            type Error = ();

            fn append(&mut self, _event: TraceEvent) -> Result<(), Self::Error> {
                Err(())
            }
        }

        let observer = TraceObserver::new("episode-1", FailingSink);
        observe(&observer, AgentEventKind::AgentStart);
        observe(&observer, AgentEventKind::AgentEnd { messages: vec![] });
        assert_eq!(observer.failed_events(), 2);
    }

    #[test]
    fn durable_provenance_and_provider_cache_usage_stay_attached_to_the_exact_turn() {
        let observer = TraceObserver::new_with_provenance(
            "episode-provenance",
            RunProvenance {
                session_id: Some("session-1".into()),
                lane_id: Some("main".into()),
                operation_id: Some("operation-1".into()),
                epoch_id: Some("epoch-1".into()),
                core_run_id: Some("core-run-1".into()),
                harness_snapshot_id: Some("snapshot-1".into()),
                harness_revision_id: Some("revision-1".into()),
                model_harness_profile_id: Some("profile-1".into()),
                provider_surface_digest: Some("surface-1".into()),
                experiment_id: Some("experiment-1".into()),
            },
            Vec::<TraceEvent>::new(),
        );
        observe(&observer, AgentEventKind::AgentStart);
        observe(&observer, AgentEventKind::TurnStart { turn_id: TurnId(1) });
        observe(
            &observer,
            AgentEventKind::ProviderRequestObserved {
                turn_id: TurnId(1),
                observation: AdapterRequestObservation {
                    deterministic_common_prefix_bytes: Some(148),
                    deterministic_common_prefix_tokens_estimate: Some(37),
                    ..AdapterRequestObservation::default()
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::ModelTurnUsage {
                accounting: ModelTurnAccounting {
                    run_id: RunId(1),
                    turn_id: TurnId(1),
                    model: None,
                    usage: Usage {
                        cache_read_tokens: Some(34),
                        cache_write_tokens: Some(8),
                        ..Usage::default()
                    },
                },
            },
        );
        observe(
            &observer,
            AgentEventKind::TurnEnd {
                turn_id: TurnId(1),
                reason: StopReason::Stop,
            },
        );

        observer.with_sink(|events| {
            let TraceEvent::EpisodeHeader(header) = &events[0] else {
                panic!("the first trace record must be the episode header");
            };
            let provenance = header
                .provenance
                .as_ref()
                .expect("durable host provenance is retained in the header");
            assert_eq!(provenance.core_run_id.as_deref(), Some("core-run-1"));
            assert_eq!(provenance.experiment_id.as_deref(), Some("experiment-1"));

            let TraceEvent::Turn(turn) = &events[1] else {
                panic!("the completed turn follows the header");
            };
            let cache = turn
                .cache_evidence
                .as_ref()
                .expect("cache evidence remains tied to the matching turn");
            assert_eq!(cache.provider_surface_digest.as_deref(), Some("surface-1"));
            assert_eq!(cache.deterministic_common_prefix_bytes, Some(148));
            assert_eq!(cache.deterministic_common_prefix_tokens_estimate, Some(37));
            assert_eq!(cache.provider_cache_read_tokens, Some(34));
            assert_eq!(cache.provider_cache_write_tokens, Some(8));
        });
    }
}
