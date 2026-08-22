//! Caller-supplied, transactional conversation compaction.
//!
//! The core owns the retained conversation and therefore owns the compaction
//! transaction. A [`Compactor`] receives an owned snapshot, but cannot mutate
//! the agent directly. Its proposed replacement is validated and committed
//! only when the owning [`CompactionHandle`] is still active and uncancelled.

use crate::agent::{ActiveRun, Agent, AgentInner};
use crate::error::CoreError;
use crate::event::{AgentEvent, AgentEventKind, CompactionOutcome};
use crate::run::RunHandle;
use crate::scheduler::CancellationToken;
use crate::state::{
    AgentMessage, AgentPhase, MessageId, ModelDescriptor, RunPhase, RunState, StopReason,
};
use crate::tool::ToolDefinition;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Version of the context shape supplied to [`Compactor`].
pub const COMPACTION_CONTEXT_VERSION: u32 = 1;

/// Stable identifier for the checked-in, current model-facing compaction layout.
///
/// The TUI's provider-backed compactor implements this strategy. Its prompt
/// and request construction are part of the host contract.
pub const CACHE_REPLAY_SUMMARY_V0: &str = "cache_replay_summary_v0";

/// Stable identity of one compaction attempt within a run.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct CompactionId {
    /// Run that owns the attempt.
    pub run_id: crate::state::RunId,
    /// Zero for an idle manual operation, otherwise its one-based automatic ordinal.
    pub ordinal: u32,
}

impl fmt::Display for CompactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:compact-{}", self.run_id, self.ordinal)
    }
}

/// Why a compaction attempt exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionTrigger {
    /// An idle caller explicitly requested maintenance.
    Manual,
    /// The in-run automatic policy claimed context pressure.
    Automatic,
}

/// Concrete trigger condition recorded independently from [`CompactionTrigger`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionReason {
    /// A caller explicitly requested compaction.
    UserRequest,
    /// The host-provided threshold was crossed.
    Threshold,
    /// A provider explicitly rejected a request for context overflow.
    ProviderOverflow,
}

/// Location of the replacement in tea's agent loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionPhase {
    /// An idle/manual operation outside a model turn.
    Standalone,
    /// The next ordinary request has not yet been constructed.
    BeforeModelRequest,
    /// A failed provider continuation is about to be retried.
    BetweenModelCalls,
}

/// How a compactor produced a checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionImplementation {
    /// A caller owns the algorithm; the core only validates and commits it.
    CallerSupplied,
    /// A deterministic provider-free fixture compactor.
    DeterministicFixture,
    /// A provider stream generated the checkpoint.
    ProviderSummarization,
}

/// Model-visible request layout selected by a compactor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionRequestLayout {
    /// The selected provider context was preserved and one instruction appended.
    ExactReplay,
    /// A self-contained summary request was constructed without a reusable prefix.
    StandaloneFallback,
    /// The core could not observe a caller-owned compactor's request layout.
    Unobserved,
}

/// Versioned descriptor for a concrete compaction strategy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionStrategy {
    /// Stable strategy identifier.
    pub id: String,
    /// Version of the strategy's checkpoint and request-layout contract.
    pub schema_version: u32,
    /// Implementation class without provider-specific types.
    pub implementation: CompactionImplementation,
    /// Request layout selected by this strategy when the core can observe it.
    pub request_layout: CompactionRequestLayout,
    /// Optional fingerprint of the model-facing instruction template.
    pub prompt_fingerprint: Option<u64>,
}

impl CompactionStrategy {
    /// Descriptor for an opaque caller-supplied compactor.
    pub fn caller_supplied() -> Self {
        Self {
            id: "caller_supplied_v1".into(),
            schema_version: 1,
            implementation: CompactionImplementation::CallerSupplied,
            request_layout: CompactionRequestLayout::Unobserved,
            prompt_fingerprint: None,
        }
    }

    /// Descriptor for the provider-backed baseline preserved by `tea-agent`.
    pub fn cache_replay_summary_v0(prompt_fingerprint: u64) -> Self {
        Self {
            id: CACHE_REPLAY_SUMMARY_V0.into(),
            schema_version: 0,
            implementation: CompactionImplementation::ProviderSummarization,
            request_layout: CompactionRequestLayout::ExactReplay,
            prompt_fingerprint: Some(prompt_fingerprint),
        }
    }

}

/// Immutable identity and policy state for one attempted compaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionOperation {
    /// Stable join key used by every lifecycle record for this attempt.
    pub id: CompactionId,
    /// Manual or automatic ownership boundary.
    pub trigger: CompactionTrigger,
    /// Specific policy or provider condition that requested compaction.
    pub reason: CompactionReason,
    /// Agent-loop position of the replacement.
    pub phase: CompactionPhase,
    /// Versioned compactor descriptor.
    pub strategy: CompactionStrategy,
    /// Canonical history generation captured for the source.
    pub source_history_revision: u64,
    /// One-based attempt count for this operation kind in its run.
    pub attempt: u32,
    /// One-based automatic compaction count, when this is automatic.
    pub automatic_ordinal: Option<u32>,
    /// One-based overflow-retry count, when this retries a rejected request.
    pub overflow_retry_ordinal: Option<u32>,
    /// Whether a successful commit resumes a previously interrupted provider request.
    pub retry_provider_request: bool,
}

/// Content-free source metadata for a compaction attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionSourceObservation {
    /// Canonical messages visible when the source was selected.
    pub canonical_message_count: usize,
    /// Approximate byte size of those canonical messages.
    pub canonical_message_bytes: usize,
    /// IDs in the selected source prefix, in canonical order.
    pub source_message_ids: Vec<MessageId>,
    /// IDs in the exact retained suffix, in canonical order.
    pub retained_message_ids: Vec<MessageId>,
    /// IDs from an explicitly summarized split turn prefix.
    pub split_turn_prefix_ids: Vec<MessageId>,
    /// Byte size of the retained suffix.
    pub retained_suffix_bytes: usize,
    /// Byte size of tool-result content in the selected canonical history.
    pub tool_result_bytes: usize,
}

/// Content-free facts about the request a compactor was given.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactorRequestObservation {
    /// Selected model-facing layout.
    pub layout: CompactionRequestLayout,
    /// Byte size of the selected provider-visible context, when one existed.
    pub provider_context_bytes: Option<usize>,
    /// Ordered prompt-facing tool count, when one existed.
    pub tool_count: Option<usize>,
    /// Whether tools remain defined while execution is prohibited by the compactor.
    pub tools_execution_prohibited: bool,
    /// Whether the selected source was an exact prefix of active provider context.
    pub source_is_active_context_prefix: Option<bool>,
}

/// Content-free validation facts for a proposed replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionProposalObservation {
    /// Number of canonical messages after replacement.
    pub replacement_message_count: usize,
    /// Approximate canonical replacement size.
    pub replacement_bytes: usize,
    /// Estimated canonical context tokens after replacement, when the policy owns a budget.
    pub estimated_context_tokens_after: Option<u64>,
    /// Budget minus the estimated replacement context, when the policy owns a budget.
    pub headroom_tokens: Option<u64>,
    /// Whether message/tool structure validated before commit.
    pub structural_validation_passed: bool,
    /// Whether the selected retained suffix exactly matches the proposal tail.
    pub retained_suffix_exact: bool,
    /// Whether the source generation still matches at proposal observation time.
    pub source_generation_matches: bool,
}

/// Typed reason why a proposal was not allowed to commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactionRejection {
    /// Canonical history changed after the snapshot.
    StaleSourceGeneration,
    /// The required exact recent suffix was not retained.
    RetainedSuffixMismatch,
    /// Tool-call/result or message-ID structure was invalid.
    InvalidStructure,
    /// The checkpoint had no non-whitespace content.
    EmptyCheckpoint,
    /// The proposed checkpoint included a tool call or tool-result message.
    UnexpectedToolCall,
    /// The replacement did not reduce the selected source.
    NonShrinkingReplacement,
    /// The replacement did not create the policy's minimum working headroom.
    InsufficientHeadroom,
    /// A policy cap disallowed another attempt.
    PolicyCapReached,
    /// Cancellation won before commit.
    Cancelled,
    /// A host-owned compactor deadline elapsed before commit.
    TimedOut,
}

/// Terminal state for one lifecycle identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionTerminalOutcome {
    /// The validated replacement committed atomically.
    Committed,
    /// The proposal was intentionally rejected without mutation.
    Rejected(CompactionRejection),
    /// The compactor/provider failed before a proposal could commit.
    Failed,
    /// Cancellation won before commit.
    Cancelled,
    /// A host-owned deadline fired before commit.
    TimedOut,
    /// Policy required a compactor that the host did not configure.
    Unavailable,
}

/// Append-only, content-free lifecycle record for compaction observability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionLifecycleRecord {
    /// A compaction identity was allocated.
    Started {
        /// Immutable operation identity.
        operation: CompactionOperation,
    },
    /// Canonical source and exact retained boundary were selected.
    SourceSelected {
        /// Lifecycle join key.
        id: CompactionId,
        /// Content-free source facts.
        source: CompactionSourceObservation,
    },
    /// The compactor received its request boundary.
    RequestPrepared {
        /// Lifecycle join key.
        id: CompactionId,
        /// Content-free request facts.
        request: CompactorRequestObservation,
    },
    /// The compactor provider supplied usage or exact serialized-request facts.
    ProviderUsageObserved {
        /// Lifecycle join key.
        id: CompactionId,
        /// Provider-reported usage, when available.
        usage: Option<crate::state::Usage>,
        /// Adapter-bound request observation, when available.
        request_observation: Option<crate::scheduler::AdapterRequestObservation>,
        /// Actual request layout reported by a concrete compactor.
        request: Option<CompactorRequestObservation>,
    },
    /// A candidate replacement was validated before commit.
    ReplacementProposed {
        /// Lifecycle join key.
        id: CompactionId,
        /// Content-free proposal facts.
        proposal: CompactionProposalObservation,
    },
    /// The attempt reached one explicit terminal state.
    Terminal {
        /// Lifecycle join key.
        id: CompactionId,
        /// Terminal outcome.
        outcome: CompactionTerminalOutcome,
    },
}

/// Why an automatic compaction transaction was requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomaticCompactionReason {
    /// The estimated next-request context crossed the configured threshold.
    Threshold,
    /// A provider explicitly reported that its context capacity was exceeded.
    Overflow,
}

/// The explicit source of a context capacity used by automatic compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextBudgetSource {
    /// A model/provider context window supplied by the embedding.
    ContextWindow(NonZeroU64),
    /// A host-selected request budget that may be smaller than a context window.
    ContextBudget(NonZeroU64),
}

impl ContextBudgetSource {
    /// Return the usable input capacity before the compaction reserve is deducted.
    pub const fn tokens(self) -> u64 {
        match self {
            Self::ContextWindow(tokens) | Self::ContextBudget(tokens) => tokens.get(),
        }
    }
}

/// What to do after an explicit provider context-overflow signal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverflowRecovery {
    /// Preserve the provider error; do not compact or retry automatically.
    #[default]
    Disabled,
    /// Compact the prior transcript then retry the incomplete continuation once.
    CompactAndRetry,
}

/// Opt-in automatic compaction configuration.
///
/// The configuration deliberately has no provider or summary-prompt fields:
/// a caller must still configure a [`Compactor`] explicitly. `recent_tokens`
/// is supplied to that compactor along with an exact safe retained suffix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomaticCompactionPolicy {
    /// Whether automatic compaction participates in normal run progression.
    pub enabled: bool,
    /// Explicit context capacity source; the core never guesses from a model name.
    pub context_budget: ContextBudgetSource,
    /// Tokens reserved for the compactor request/output and not available to the next request.
    pub reserved_tokens: u64,
    /// Minimum usable context remaining after an automatic replacement commits.
    ///
    /// This is a host policy value, not an inferred model limit. A proposal
    /// that would leave less headroom is rejected even when it is smaller.
    pub minimum_headroom_tokens: u64,
    /// Approximate number of recent transcript tokens selected as an intact retained suffix.
    pub recent_tokens: u64,
    /// Typed overflow recovery policy.
    pub overflow_recovery: OverflowRecovery,
    /// Maximum successful or attempted automatic compaction transactions in one run.
    pub max_compactions_per_run: u32,
    /// Maximum overflow-recovery retries across distinct continuations in one run.
    ///
    /// Each incomplete continuation can still be retried at most once.
    pub max_overflow_retries_per_run: u32,
}

impl AutomaticCompactionPolicy {
    /// Construct a disabled policy with an explicit inert capacity placeholder.
    ///
    /// Use [`Self::enabled`] to opt in after selecting a real capacity.
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            // One is inert while disabled and keeps this type free of an
            // invalid zero-capacity state.
            context_budget: ContextBudgetSource::ContextBudget(NonZeroU64::MIN),
            reserved_tokens: 0,
            minimum_headroom_tokens: 0,
            recent_tokens: 0,
            overflow_recovery: OverflowRecovery::Disabled,
            max_compactions_per_run: 0,
            max_overflow_retries_per_run: 0,
        }
    }

    /// Validate cross-field policy invariants before an agent is built.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.enabled {
            return Ok(());
        }
        if self.reserved_tokens >= self.context_budget.tokens() {
            return Err("automatic compaction reserve must be smaller than the context budget");
        }
        if self.minimum_headroom_tokens >= self.context_budget.tokens() {
            return Err(
                "automatic compaction minimum headroom must be smaller than the context budget",
            );
        }
        if self.max_compactions_per_run == 0 {
            return Err(
                "enabled automatic compaction requires a non-zero per-run compaction limit",
            );
        }
        if self.overflow_recovery == OverflowRecovery::CompactAndRetry
            && self.max_overflow_retries_per_run == 0
        {
            return Err("overflow retry recovery requires a non-zero retry limit");
        }
        Ok(())
    }

    /// Return the threshold for a normal next model request.
    pub const fn threshold_tokens(&self) -> u64 {
        self.context_budget
            .tokens()
            .saturating_sub(self.reserved_tokens)
    }
}

impl Default for AutomaticCompactionPolicy {
    fn default() -> Self {
        Self::disabled()
    }
}

/// The exact automatic split supplied to a compactor.
///
/// `retained_messages` is an intact suffix that keeps assistant tool calls
/// paired with results. `prefix_messages` may end during a tool turn only when
/// `split_turn_prefix` names the assistant content the compactor should retain
/// in its summary. The core never fabricates summary text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomaticCompactionRequest {
    /// Trigger that selected this transaction.
    pub reason: AutomaticCompactionReason,
    /// Estimated context tokens before compaction, if known.
    pub estimated_tokens_before: Option<u64>,
    /// Configured capacity before the reserve is deducted.
    pub context_budget_tokens: u64,
    /// Tokens reserved for the compactor operation.
    pub reserved_tokens: u64,
    /// Requested approximate tail size.
    pub recent_tokens: u64,
    /// Prefix selected for summary/reduction.
    pub prefix_messages: Vec<AgentMessage>,
    /// Intact suffix that the replacement must preserve exactly.
    pub retained_messages: Vec<AgentMessage>,
    /// A partial user/assistant/tool turn in the summarized prefix, when the
    /// intact retained suffix begins at an assistant message.
    pub split_turn_prefix: Vec<AgentMessage>,
    /// Whether a successful compaction will retry the same provider continuation.
    pub retry_provider_request: bool,
}

/// An owned, versioned view of the conversation a compactor may replace.
///
/// It contains only data retained by the core. The selected model remains
/// informational: choosing or replacing a provider is a separate idle-only
/// operation on [`Agent`].
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionContext {
    /// Version of this request shape.
    pub version: u32,
    /// Static instructions associated with this conversation.
    pub system_prompt: String,
    /// Selected model identity, if the host configured one.
    pub model: Option<ModelDescriptor>,
    /// Canonical retained conversation.
    pub messages: Vec<AgentMessage>,
    /// Canonical-history generation captured with `messages`.
    ///
    /// A proposal may commit only while the agent still has this generation.
    pub source_history_revision: u64,
    /// Explicit host-only context retained beside the conversation.
    pub host_messages: Vec<crate::state::SerializedJson>,
    /// Provider-visible context built through the active projection and hook pipeline when the
    /// compaction was requested from a running model turn. `None` means the compactor must use
    /// the standalone summary path; idle manual compaction intentionally has no request snapshot.
    pub provider_context: Option<ProviderContext>,
}

/// The provider-visible prompt snapshot available to an automatic compactor.
///
/// `context` is intentionally opaque: the core does not impose a provider message schema. A
/// host that understands its own conversion (for example the TUI's OpenAI-compatible adapter)
/// may append a single summary instruction while preserving the exact preceding context bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderContext {
    /// System instructions used for the active request.
    pub system_prompt: String,
    /// Converted provider conversation/context.
    pub context: String,
    /// Complete active provider context used to verify that `context` is an exact message-prefix.
    /// Hosts that understand the conversion can reject cache-friendly summarization when a
    /// transform reordered or injected content into the candidate source.
    pub active_context: Option<String>,
    /// Ordered prompt-facing tool definitions used for the active request.
    pub tools: Vec<ToolDefinition>,
}

/// A validated-on-return proposal from a [`Compactor`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResult {
    /// Replacement canonical conversation.
    pub messages: Vec<AgentMessage>,
    /// Optional accounting reported by the compactor's own provider call.
    ///
    /// This stays attached to the compaction event because compaction is not a
    /// normal model turn. The core does not estimate, aggregate, or price it.
    pub usage: Option<crate::state::Usage>,
    /// Content-safe observation emitted by the adapter that sent the compactor request.
    pub request_observation: Option<crate::scheduler::AdapterRequestObservation>,
    /// Actual request layout selected by a concrete compactor, when it can report one.
    pub request_layout: Option<CompactionRequestLayout>,
    /// Whether the concrete compactor verified an exact active-context prefix.
    pub source_is_active_context_prefix: Option<bool>,
}

impl CompactionResult {
    /// Construct a result with no provider-reported compaction accounting.
    pub fn new(messages: Vec<AgentMessage>) -> Self {
        Self {
            messages,
            usage: None,
            request_observation: None,
            request_layout: None,
            source_is_active_context_prefix: None,
        }
    }

    /// Attach provider-reported compaction accounting without deriving a price.
    pub fn with_usage(mut self, usage: crate::state::Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Attach the exact adapter-bound request observation for this compactor call.
    pub fn with_request_observation(
        mut self,
        request_observation: crate::scheduler::AdapterRequestObservation,
    ) -> Self {
        self.request_observation = Some(request_observation);
        self
    }

    /// Attach the actual request layout selected during compactor preparation.
    pub fn with_request_layout(
        mut self,
        request_layout: CompactionRequestLayout,
        source_is_active_context_prefix: Option<bool>,
    ) -> Self {
        self.request_layout = Some(request_layout);
        self.source_is_active_context_prefix = source_is_active_context_prefix;
        self
    }
}

/// A typed compactor failure returned to the core transaction boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionError {
    /// The caller-supplied compactor could not produce a replacement.
    Failed {
        /// Redacted, host-supplied diagnostic.
        message: String,
    },
    /// The compactor returned a replacement that violates conversation invariants.
    InvalidReplacement {
        /// Stable explanation of the rejected relationship or identifier.
        message: String,
    },
    /// Canonical history changed after the compactor took its owned snapshot.
    StaleSource {
        /// Generation captured by the compactor.
        expected_revision: u64,
        /// Generation currently owned by the agent.
        actual_revision: u64,
    },
    /// A host-owned compactor deadline elapsed before a proposal was available.
    TimedOut {
        /// Redacted timeout diagnostic supplied by the host boundary.
        message: String,
    },
}

impl CompactionError {
    /// Construct a caller-supplied failure without exposing a provider type.
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }

    /// Construct an explicit host-owned compactor deadline result.
    pub fn timed_out(message: impl Into<String>) -> Self {
        Self::TimedOut {
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidReplacement {
            message: message.into(),
        }
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed { message } => write!(formatter, "compactor failed: {message}"),
            Self::InvalidReplacement { message } => {
                write!(formatter, "invalid compacted conversation: {message}")
            }
            Self::StaleSource {
                expected_revision,
                actual_revision,
            } => write!(
                formatter,
                "compaction source changed from generation {expected_revision} to {actual_revision}"
            ),
            Self::TimedOut { message } => write!(formatter, "compactor timed out: {message}"),
        }
    }
}

impl std::error::Error for CompactionError {}

/// A caller-polled compactor operation.
pub type CompactionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompactionResult, CompactionError>> + Send + 'a>>;

/// A caller-supplied policy and execution boundary for manual compaction.
///
/// Implementations may call a model, use a local algorithm, or reject the
/// request. They receive cancellation and must not assume an executor owned
/// by the core. There is no implicit summary prompt or provider fallback.
pub trait Compactor: Send + Sync {
    /// Return the immutable descriptor for this compactor's model-facing strategy.
    ///
    /// The default deliberately describes an opaque caller algorithm. Hosts
    /// with a concrete prompt or deterministic fixture override it so traces
    /// can distinguish strategy versions without introducing a registry.
    fn strategy(&self) -> CompactionStrategy {
        CompactionStrategy::caller_supplied()
    }

    /// Produce a replacement for this owned context.
    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        cancellation: CancellationToken,
    ) -> CompactionFuture<'a>;

    /// Produce an automatic replacement using the core-selected safe split.
    ///
    /// Existing manual compactors remain valid: by default they receive the
    /// same complete snapshot through [`Self::compact`]. Compactors that can
    /// preserve a prior summary and exact recent tail should override this
    /// method and use `request` rather than inferring boundaries themselves.
    fn compact_automatic<'a>(
        &'a self,
        context: CompactionContext,
        _request: AutomaticCompactionRequest,
        cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        self.compact(context, cancellation)
    }
}

/// A reserved, caller-driven manual compaction operation.
///
/// The handle has the same ownership and cancellation rules as a normal run:
/// construct it while idle, then drive it on the embedding's executor. Its
/// events are intentionally a separate `Compaction*` grammar rather than a
/// synthetic assistant response.
pub struct CompactionHandle {
    run: RunHandle,
    compactor: Arc<dyn Compactor>,
}

impl fmt::Debug for CompactionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionHandle")
            .field("snapshot", &self.run.snapshot())
            .finish()
    }
}

impl CompactionHandle {
    /// Stable operation ID, allocated from the agent's run-ID sequence.
    pub fn id(&self) -> crate::state::RunId {
        self.run.id()
    }

    /// Return the ordered lifecycle events emitted by this compaction.
    pub fn events(&self) -> Vec<AgentEvent> {
        self.run.events()
    }

    /// Request cancellation. This is idempotent after terminal settlement.
    pub fn abort(&self) -> Result<(), CoreError> {
        self.run.abort()
    }

    /// Drive the transaction to a terminal outcome on the caller's executor.
    pub async fn drive(&self) -> Result<(), CoreError> {
        let agent = self
            .run
            .agent
            .upgrade()
            .ok_or(CoreError::InvalidTransition(
                crate::error::StateTransitionError::new("compaction", "orphaned", "drive"),
            ))?;
        // Capture before notifying observers. A lifecycle observer may steer
        // or otherwise change canonical state; the CAS commit below must then
        // reject rather than silently replacing that newer history.
        let context = snapshot_context(&agent);
        let source_message_count = context.messages.len();
        let operation = CompactionOperation {
            id: CompactionId {
                run_id: self.id(),
                ordinal: 0,
            },
            trigger: CompactionTrigger::Manual,
            reason: CompactionReason::UserRequest,
            phase: CompactionPhase::Standalone,
            strategy: self.compactor.strategy(),
            source_history_revision: context.source_history_revision,
            attempt: 1,
            automatic_ordinal: None,
            overflow_retry_ordinal: None,
            retry_provider_request: false,
        };
        let source = observe_source(&context.messages, &context.messages, &[], &[]);

        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionStart {
                    source_message_count,
                },
            )
            .await
            .map(|_| ())
        {
            return self.settle_emit_failure(error);
        }
        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionLifecycle {
                    record: CompactionLifecycleRecord::Started {
                        operation: operation.clone(),
                    },
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }
        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionLifecycle {
                    record: CompactionLifecycleRecord::SourceSelected {
                        id: operation.id,
                        source,
                    },
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }
        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionLifecycle {
                    record: CompactionLifecycleRecord::RequestPrepared {
                        id: operation.id,
                        request: CompactorRequestObservation {
                            layout: operation.strategy.request_layout,
                            provider_context_bytes: None,
                            tool_count: None,
                            tools_execution_prohibited: true,
                            source_is_active_context_prefix: None,
                        },
                    },
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }
        if self.run.cancellation.is_cancelled() {
            let _ = self
                .emit_lifecycle_terminal(&agent, operation.id, CompactionTerminalOutcome::Cancelled)
                .await;
            return self.settle_cancelled(&agent).await;
        }

        let source_history_revision = context.source_history_revision;
        let replacement = match self
            .compactor
            .compact(context, self.run.cancellation.clone())
            .await
        {
            Ok(replacement) => replacement,
            Err(error) => {
                let outcome = if matches!(error, CompactionError::TimedOut { .. }) {
                    CompactionTerminalOutcome::TimedOut
                } else {
                    CompactionTerminalOutcome::Failed
                };
                let _ = self
                    .emit_lifecycle_terminal(&agent, operation.id, outcome)
                    .await;
                return self.settle_failure(&agent, error).await;
            }
        };
        if self.run.cancellation.is_cancelled() {
            let _ = self
                .emit_lifecycle_terminal(&agent, operation.id, CompactionTerminalOutcome::Cancelled)
                .await;
            return self.settle_cancelled(&agent).await;
        }
        if let Err(error) = validate_messages(&replacement.messages) {
            let _ = self
                .emit_lifecycle_terminal(
                    &agent,
                    operation.id,
                    CompactionTerminalOutcome::Rejected(CompactionRejection::InvalidStructure),
                )
                .await;
            return self.settle_failure(&agent, error).await;
        }

        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionLifecycle {
                    record: CompactionLifecycleRecord::ProviderUsageObserved {
                        id: operation.id,
                        usage: replacement.usage.clone(),
                        request_observation: replacement.request_observation.clone(),
                        request: replacement.request_layout.map(|layout| {
                            CompactorRequestObservation {
                                layout,
                                provider_context_bytes: None,
                                tool_count: None,
                                tools_execution_prohibited: true,
                                source_is_active_context_prefix: replacement
                                    .source_is_active_context_prefix,
                            }
                        }),
                    },
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }

        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionLifecycle {
                    record: CompactionLifecycleRecord::ReplacementProposed {
                        id: operation.id,
                        proposal: CompactionProposalObservation {
                            replacement_message_count: replacement.messages.len(),
                            replacement_bytes: messages_bytes(&replacement.messages),
                            estimated_context_tokens_after: None,
                            headroom_tokens: None,
                            structural_validation_passed: true,
                            retained_suffix_exact: true,
                            source_generation_matches: context_generation_matches(
                                &agent,
                                source_history_revision,
                            ),
                        },
                    },
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }

        let retained_message_count = replacement.messages.len();
        if let Err(error) = commit_replacement(
            &agent,
            self.id(),
            &self.run.cancellation,
            source_history_revision,
            replacement.messages,
        ) {
            return match error {
                CoreError::Cancelled => {
                    let _ = self
                        .emit_lifecycle_terminal(
                            &agent,
                            operation.id,
                            CompactionTerminalOutcome::Cancelled,
                        )
                        .await;
                    self.settle_cancelled(&agent).await
                }
                CoreError::Compaction(error @ CompactionError::StaleSource { .. }) => {
                    let _ = self
                        .emit_lifecycle_terminal(
                            &agent,
                            operation.id,
                            CompactionTerminalOutcome::Rejected(
                                CompactionRejection::StaleSourceGeneration,
                            ),
                        )
                        .await;
                    self.settle_failure(&agent, error).await
                }
                error => self.settle_emit_failure(error),
            };
        }
        if let Err(error) = self
            .emit_lifecycle_terminal(&agent, operation.id, CompactionTerminalOutcome::Committed)
            .await
        {
            return self.settle_emit_failure(error);
        }
        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionResult {
                    retained_message_count,
                    usage: replacement.usage,
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }
        if let Err(error) = self
            .run
            .emit(
                &agent,
                AgentEventKind::CompactionEnd {
                    outcome: CompactionOutcome::Succeeded {
                        retained_message_count,
                    },
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }
        self.run.succeed(StopReason::Stop)
    }

    async fn emit_lifecycle_terminal(
        &self,
        agent: &AgentInner,
        id: CompactionId,
        outcome: CompactionTerminalOutcome,
    ) -> Result<(), CoreError> {
        self.run
            .emit(
                agent,
                AgentEventKind::CompactionLifecycle {
                    record: CompactionLifecycleRecord::Terminal { id, outcome },
                },
            )
            .await
            .map(|_| ())
    }

    async fn settle_failure(
        &self,
        agent: &AgentInner,
        error: CompactionError,
    ) -> Result<(), CoreError> {
        let message = error.to_string();
        if let Err(observer_error) = self
            .run
            .emit(
                agent,
                AgentEventKind::CompactionEnd {
                    outcome: CompactionOutcome::Failed {
                        message: message.clone(),
                    },
                },
            )
            .await
        {
            return self.settle_emit_failure(observer_error);
        }
        let _ = self.run.fail(message);
        Err(CoreError::Compaction(error))
    }

    async fn settle_cancelled(&self, agent: &AgentInner) -> Result<(), CoreError> {
        if let Err(error) = self
            .run
            .emit(
                agent,
                AgentEventKind::CompactionEnd {
                    outcome: CompactionOutcome::Cancelled,
                },
            )
            .await
        {
            return self.settle_emit_failure(error);
        }
        self.run.settle_cancelled()?;
        Err(CoreError::Cancelled)
    }

    fn settle_emit_failure(&self, error: CoreError) -> Result<(), CoreError> {
        let _ = self.run.fail(error.to_string());
        Err(error)
    }
}

impl Agent {
    /// Reserve an idle agent for a caller-driven manual compaction operation.
    ///
    /// This rejects active and cancelling agents without changing their state.
    /// An agent without an explicit [`Compactor`] also rejects the request;
    /// hosts must not invent a summary policy at this boundary.
    pub fn start_compaction(&self) -> Result<CompactionHandle, CoreError> {
        let run_number = self
            .inner
            .next_run_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        let run_id = crate::state::RunId(run_number);
        {
            let state = self.inner.state.lock().expect("agent state mutex poisoned");
            if let AgentPhase::Running(active) | AgentPhase::Cancelling(active) = state.phase {
                return Err(CoreError::ActiveRun { run_id: active });
            }
        }
        let compactor = self
            .inner
            .compactor
            .read()
            .expect("agent compactor lock poisoned")
            .clone()
            .ok_or(CoreError::MissingCompactor)?;
        let mut state = self.inner.state.lock().expect("agent state mutex poisoned");
        if let AgentPhase::Running(active) | AgentPhase::Cancelling(active) = state.phase {
            return Err(CoreError::ActiveRun { run_id: active });
        }
        let configuration = Arc::clone(
            &*self
                .inner
                .configuration
                .read()
                .expect("agent configuration lock poisoned"),
        );
        state.phase = AgentPhase::Running(run_id);
        state.last_error = None;
        state.partial_response = None;
        state.is_streaming = false;
        state.pending_tool_calls.clear();
        drop(state);

        let run_state = Arc::new(Mutex::new(RunState::created(run_id)));
        run_state
            .lock()
            .expect("compaction run mutex poisoned")
            .phase = RunPhase::Running;
        let cancellation = CancellationToken::new();
        *self
            .inner
            .active_run
            .lock()
            .expect("active run mutex poisoned") = Some(ActiveRun {
            id: run_id,
            state: Arc::clone(&run_state),
            cancellation: cancellation.clone(),
        });

        Ok(CompactionHandle {
            run: RunHandle {
                agent: Arc::downgrade(&self.inner),
                state: run_state,
                cancellation,
                initial_messages: Vec::new(),
                message_start_index: 0,
                skip_initial_steering: true,
                configuration,
                policy: Mutex::new(crate::run::RunPolicyState::default()),
            },
            compactor,
        })
    }
}

pub(crate) fn snapshot_context(agent: &AgentInner) -> CompactionContext {
    let state = agent.state.lock().expect("agent state mutex poisoned");
    CompactionContext {
        version: COMPACTION_CONTEXT_VERSION,
        system_prompt: state.system_prompt.clone(),
        model: state.model.clone(),
        messages: state.messages.clone(),
        source_history_revision: state.history_revision,
        host_messages: state.host_messages.clone(),
        provider_context: None,
    }
}

pub(crate) fn commit_replacement(
    agent: &AgentInner,
    run_id: crate::state::RunId,
    cancellation: &CancellationToken,
    expected_history_revision: u64,
    replacement: Vec<AgentMessage>,
) -> Result<(), CoreError> {
    let mut state = agent.state.lock().expect("agent state mutex poisoned");
    if cancellation.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    if !matches!(state.phase, AgentPhase::Running(id) if id == run_id) {
        return Err(CoreError::Cancelled);
    }
    if state.history_revision != expected_history_revision {
        return Err(CoreError::Compaction(CompactionError::StaleSource {
            expected_revision: expected_history_revision,
            actual_revision: state.history_revision,
        }));
    }
    state.replace_messages(replacement);
    Ok(())
}

pub(crate) fn validate_messages(messages: &[AgentMessage]) -> Result<(), CompactionError> {
    let mut message_ids = BTreeSet::new();
    let mut tool_calls = BTreeMap::new();
    let mut tool_results = BTreeSet::new();
    for message in messages {
        let id = message_id(message);
        if id.0 == 0 || id.0 == u64::MAX {
            return Err(CompactionError::invalid(
                "message IDs zero and u64::MAX are reserved",
            ));
        }
        if !message_ids.insert(id) {
            return Err(CompactionError::invalid(format!(
                "message ID {} occurs more than once",
                id.0
            )));
        }
        match message {
            AgentMessage::Assistant {
                tool_calls: calls, ..
            } => {
                for call in calls {
                    if let Some(previous_name) =
                        tool_calls.insert(call.id.clone(), call.name.as_str())
                    {
                        return Err(CompactionError::invalid(format!(
                            "tool call ID {} is reused by {previous_name:?} and {:?}",
                            call.id, call.name
                        )));
                    }
                }
            }
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                ..
            } => match tool_calls.get(tool_call_id) {
                Some(call_name) if *call_name == tool_name => {
                    if !tool_results.insert(tool_call_id.clone()) {
                        return Err(CompactionError::invalid(format!(
                            "tool call {} has more than one retained result",
                            tool_call_id
                        )));
                    }
                }
                Some(call_name) => {
                    return Err(CompactionError::invalid(format!(
                        "tool result {} names {tool_name:?}, but its call names {call_name:?}",
                        tool_call_id
                    )));
                }
                None => {
                    return Err(CompactionError::invalid(format!(
                        "tool result {} has no preceding assistant call",
                        tool_call_id
                    )));
                }
            },
            AgentMessage::User { .. } => {}
        }
    }
    let missing_results = tool_calls
        .keys()
        .filter(|tool_call_id| !tool_results.contains(*tool_call_id))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if !missing_results.is_empty() {
        return Err(CompactionError::invalid(format!(
            "retained assistant tool calls have no result: {}",
            missing_results.join(", ")
        )));
    }
    Ok(())
}

fn message_id(message: &AgentMessage) -> MessageId {
    match message {
        AgentMessage::User { id, .. }
        | AgentMessage::Assistant { id, .. }
        | AgentMessage::ToolResult { id, .. } => *id,
    }
}

/// Construct privacy-safe source facts without retaining prompt or tool-result content.
pub(crate) fn observe_source(
    canonical: &[AgentMessage],
    source: &[AgentMessage],
    retained: &[AgentMessage],
    split_turn_prefix: &[AgentMessage],
) -> CompactionSourceObservation {
    CompactionSourceObservation {
        canonical_message_count: canonical.len(),
        canonical_message_bytes: messages_bytes(canonical),
        source_message_ids: source.iter().map(message_id).collect(),
        retained_message_ids: retained.iter().map(message_id).collect(),
        split_turn_prefix_ids: split_turn_prefix.iter().map(message_id).collect(),
        retained_suffix_bytes: messages_bytes(retained),
        tool_result_bytes: tool_result_bytes(source),
    }
}

/// Approximate canonical message bytes without serializing through a provider projection.
pub(crate) fn messages_bytes(messages: &[AgentMessage]) -> usize {
    messages.iter().fold(0_usize, |total, message| {
        let body = match message {
            AgentMessage::User { content, .. } => content.len(),
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
                                .to_string()
                                .len()
                                .saturating_add(call.name.len())
                                .saturating_add(call.arguments.as_str().len())
                        })
                        .sum::<usize>(),
                ),
            AgentMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
                details,
                ..
            } => tool_call_id
                .to_string()
                .len()
                .saturating_add(tool_name.len())
                .saturating_add(content.len())
                .saturating_add(details.as_ref().map_or(0, |details| details.as_str().len())),
        };
        total.saturating_add(body).saturating_add(16)
    })
}

fn tool_result_bytes(messages: &[AgentMessage]) -> usize {
    messages
        .iter()
        .fold(0_usize, |total, message| match message {
            AgentMessage::ToolResult {
                content, details, ..
            } => total
                .saturating_add(content.len())
                .saturating_add(details.as_ref().map_or(0, |details| details.as_str().len())),
            _ => total,
        })
}

fn context_generation_matches(agent: &AgentInner, expected: u64) -> bool {
    agent
        .state
        .lock()
        .expect("agent state mutex poisoned")
        .history_revision
        == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_generation_cannot_replace_newer_canonical_history() {
        let agent = Agent::builder().build();
        let run_id = crate::state::RunId(7);
        {
            let mut state = agent
                .inner
                .state
                .lock()
                .expect("fixture state mutex poisoned");
            state.phase = AgentPhase::Running(run_id);
            let id = state.allocate_message_id();
            state.append_message(AgentMessage::User {
                id,
                content: "source".into(),
            });
        }
        let source = snapshot_context(&agent.inner);
        {
            let mut state = agent
                .inner
                .state
                .lock()
                .expect("fixture state mutex poisoned");
            let id = state.allocate_message_id();
            state.append_message(AgentMessage::User {
                id,
                content: "newer".into(),
            });
        }

        let error = commit_replacement(
            &agent.inner,
            run_id,
            &CancellationToken::new(),
            source.source_history_revision,
            source.messages,
        )
        .expect_err("a stale snapshot must never overwrite newer history");
        assert!(matches!(
            error,
            CoreError::Compaction(CompactionError::StaleSource {
                expected_revision: 1,
                actual_revision: 2,
            })
        ));
        assert_eq!(agent.snapshot().messages.len(), 2);
    }
}
