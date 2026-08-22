//! Lifecycle events and awaited observer boundaries.
//!
//! Event construction is intentionally separate from state mutation.  The run loop must first
//! settle state, then emit the corresponding event in this order:
//! `agent_start → turn_start → message* / tool_execution* → model_turn_usage? → turn_end → agent_end`.

use crate::error::CoreError;
use crate::scheduler::CancellationToken;
use crate::state::{
    AgentMessage, ModelTurnAccounting, RunId, SerializedJson, StopReason, ToolCallId, TurnId,
};
use crate::tool::{AgentToolResult, ToolUpdate};
use std::future::Future;
use std::pin::Pin;

/// Monotonic sequence assigned by one run.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct EventSequence(pub u64);

/// An event envelope with stable run identity and local ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEvent {
    /// Run that emitted the event.
    pub run_id: RunId,
    /// Sequence within that run.
    pub sequence: EventSequence,
    /// Event payload.
    pub kind: AgentEventKind,
}

/// Terminal outcome of one manual compaction operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionOutcome {
    /// The replacement was validated and atomically committed.
    Succeeded {
        /// Number of messages retained after compaction.
        retained_message_count: usize,
    },
    /// The compactor or its replacement failed before any history changed.
    Failed {
        /// Redacted failure diagnostic.
        message: String,
    },
    /// Host cancellation won before the replacement commit.
    Cancelled,
}

/// Outcome of one in-run automatic compaction transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AutomaticCompactionOutcome {
    /// A validated replacement was committed.
    Succeeded {
        /// Estimated context tokens after commit, when available.
        estimated_tokens_after: Option<u64>,
    },
    /// The transaction failed before history changed.
    Failed {
        /// Bounded redacted diagnostic.
        message: String,
    },
    /// Cancellation won before commit and history stayed unchanged.
    Cancelled,
    /// An enabled policy reached its configured attempt limit.
    LimitReached,
    /// A retained replacement could not reduce the request below threshold.
    StillAboveThreshold,
    /// No caller-owned compactor was configured.
    Unavailable,
}

/// Why the core did not start a provider request at its normal boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRequestSkipReason {
    /// The request was deferred while automatic compaction ran.
    AutomaticCompaction,
    /// A fatal or tripped tool circuit breaker ended the run.
    ToolCircuitBreaker,
}

/// Meaningful lifecycle payloads emitted by the core.
#[allow(missing_docs)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentEventKind {
    /// One append-only, content-free compaction lifecycle record.
    CompactionLifecycle {
        /// Record joined by its stable compaction identity.
        record: crate::compaction::CompactionLifecycleRecord,
    },
    /// Content-safe adapter facts for the exact request used by a model turn.
    ///
    /// The observation is emitted from the provider stream, not by rebuilding
    /// the request for telemetry. It does not assert a provider cache hit.
    ProviderRequestObserved {
        /// Model turn that owns the prepared request.
        turn_id: TurnId,
        /// Adapter boundary observation.
        observation: crate::scheduler::AdapterRequestObservation,
    },
    /// A manual compaction operation reserved an idle agent.
    CompactionStart {
        /// Messages held by the original retained context.
        source_message_count: usize,
    },
    /// A compaction replacement was validated and committed.
    CompactionResult {
        /// Messages retained by the committed replacement.
        retained_message_count: usize,
        /// Optional provider-reported usage of the compactor operation.
        usage: Option<crate::state::Usage>,
    },
    /// A manual compaction operation reached a terminal outcome.
    CompactionEnd {
        /// Transaction outcome.
        outcome: CompactionOutcome,
    },
    /// An in-run automatic compaction transaction started.
    AutomaticCompactionStart {
        /// Threshold pressure or explicit provider overflow.
        reason: crate::compaction::AutomaticCompactionReason,
        /// Number of canonical messages considered.
        source_message_count: usize,
        /// Estimated next-request tokens before compaction, when known.
        estimated_tokens_before: Option<u64>,
        /// Whether success will retry an incomplete provider continuation.
        retry_provider_request: bool,
        /// One-based automatic compaction count in this run.
        count: u32,
    },
    /// An in-run automatic compaction transaction settled.
    AutomaticCompactionEnd {
        /// Trigger that selected the transaction.
        reason: crate::compaction::AutomaticCompactionReason,
        /// Whether the same continuation was intended to be retried.
        retry_provider_request: bool,
        /// Transaction outcome.
        outcome: AutomaticCompactionOutcome,
    },
    /// Context and payload estimates immediately before a provider request.
    ContextEstimate {
        /// Estimated next-request context tokens, when available.
        estimated_context_tokens: Option<u64>,
        /// Byte length of serialized provider input plus system prompt.
        input_bytes: usize,
        /// Number of canonical conversation messages.
        message_count: usize,
        /// Approximate canonical message bytes.
        message_bytes: usize,
        /// Approximate raw tool-result bytes within the canonical transcript.
        tool_result_bytes: usize,
    },
    /// The normal provider request boundary was deliberately not entered.
    ProviderRequestSkipped { reason: ProviderRequestSkipReason },
    /// Run ownership began.
    AgentStart,
    /// A model turn began.
    TurnStart { turn_id: TurnId },
    /// A message became visible.
    MessageStart { message: AgentMessage },
    /// A partial message update.
    ///
    /// `text_delta` is the provider event payload, while `message` is the
    /// reduced assistant snapshot after that delta. Keeping both prevents an
    /// observer from having to diff snapshots (which is wrong for repeated
    /// text, thinking, or future interleaved content blocks).
    MessageUpdate {
        /// Reduced message snapshot after this update.
        message: AgentMessage,
        /// Exact text fragment delivered by the current V0 stream event.
        text_delta: Option<String>,
    },
    /// A message settled.
    MessageEnd { message: AgentMessage },
    /// Tool execution began.
    ToolExecutionStart {
        tool_call_id: ToolCallId,
        tool_name: String,
        /// Exact serialized JSON arguments supplied by the model.
        ///
        /// This is emitted before validation, hooks, or capability dispatch.
        /// Hosts that persist or forward events must apply their redaction
        /// policy before crossing their own trace boundary.
        arguments: SerializedJson,
    },
    /// Tool emitted a partial update.
    ToolExecutionUpdate {
        /// Call that produced the partial update.
        tool_call_id: ToolCallId,
        /// Stable tool name for the update's executable capability.
        tool_name: String,
        /// Partial result supplied by the tool.
        update: ToolUpdate,
    },
    /// Tool execution settled.
    ToolExecutionEnd {
        tool_call_id: ToolCallId,
        tool_name: String,
        result: AgentToolResult,
    },
    /// A typed failure was observed by the run-local tool circuit breaker.
    ToolFailureObserved {
        /// Correlated assistant tool call.
        tool_call_id: ToolCallId,
        /// Host-visible recovery semantics.
        disposition: crate::tool::ToolFailureDisposition,
        /// Stable host-supplied capability signature, when relevant.
        signature: Option<String>,
        /// Consecutive count for the current signature.
        consecutive_count: u32,
        /// Whether this result ends the run after batch recording.
        terminal: bool,
        /// Bounded model-visible recovery/terminal reason.
        message: String,
    },
    /// A model turn settled.
    TurnEnd { turn_id: TurnId, reason: StopReason },
    /// Provider-reported accounting for a settled model turn.
    ///
    /// This event is emitted after the assistant message has settled and before `TurnEnd`.
    /// Missing fields remain unknown (`None`); the core never estimates a value.
    ModelTurnUsage { accounting: ModelTurnAccounting },
    /// The loop emitted its final event. Awaited observers may still keep the
    /// agent active before terminal settlement makes it idle.
    AgentEnd { messages: Vec<AgentMessage> },
}

/// Payload-only view of an [`AgentEvent`].
///
/// Upstream Pi exposes the event union directly as `AgentEvent`; Rust keeps a
/// stable envelope for run identity and ordering, so the payload retains the
/// explicit `AgentEventKind` name.
pub type AgentEventPayload = AgentEventKind;

/// A boxed observer future that is settled before the run advances.
pub type ObserverFuture<'a> = Pin<Box<dyn Future<Output = Result<(), CoreError>> + Send + 'a>>;

/// An awaited lifecycle observer.
///
/// Observers see state after the event reducer has applied the event. Their
/// futures are awaited in registration order, including for `AgentEnd`, so an
/// unfinished terminal observer keeps the run active. The bounded,
/// non-blocking [`crate::EventSubscription`] returned by
/// [`crate::Agent::subscribe_nonblocking`] is a separate lossy channel
/// contract, while [`crate::LosslessEventSubscription`] returned by
/// [`crate::Agent::subscribe_lossless`] is an explicitly unbounded channel;
/// neither must be silently substituted for this one.
pub trait EventObserver: Send + Sync {
    /// Observe one reduced event using the run's cancellation scope.
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        cancellation: CancellationToken,
    ) -> ObserverFuture<'a>;
}

/// A bounded, owned event collection useful for deterministic providers and tests.
#[derive(Clone, Debug, Default)]
pub struct EventLog {
    events: Vec<AgentEvent>,
}

impl EventLog {
    /// Append one event in sequence order.
    pub fn push(&mut self, event: AgentEvent) {
        self.events.push(event);
    }

    /// Borrow the ordered event view.
    pub fn as_slice(&self) -> &[AgentEvent] {
        &self.events
    }

    /// Consume the log into owned events.
    pub fn into_events(self) -> Vec<AgentEvent> {
        self.events
    }
}
