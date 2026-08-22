//! The Tea execution kernel.
//!
//! This crate is the deliberately small boundary between a caller-owned executor and the
//! agent state machine.  It does not create an executor, discover configuration, parse a
//! workspace, or own a model provider.  The modules below are scaffolding for the V0 loop;
//! each transition is represented by a typed operation so an implementation can be checked
//! against the pinned upstream SDK without leaking policy into the scheduler.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod compaction;
pub mod default_tools;
pub mod error;
pub mod event;
pub mod hooks;
#[cfg(any(
    feature = "eval-runner",
    feature = "provider-commandcode",
    feature = "provider-openrouter",
    feature = "provider-local"
))]
mod json;
pub mod measurement;
pub mod profile;
pub mod provider;
pub mod queue;
pub mod run;
pub mod scheduler;
mod schema_validation;
pub mod state;
pub mod tool;
mod tools;
#[cfg(feature = "trace")]
pub mod trace;

#[cfg(test)]
mod tests;

pub use agent::{
    Agent, AgentBuilder, AgentConfiguration, EventSubscription, LosslessEventSubscription,
    ObserverSubscription,
};
pub use compaction::{
    AutomaticCompactionPolicy, AutomaticCompactionReason, AutomaticCompactionRequest,
    CompactionContext, CompactionError, CompactionFuture, CompactionHandle, CompactionId,
    CompactionImplementation, CompactionLifecycleRecord, CompactionOperation, CompactionPhase,
    CompactionProposalObservation, CompactionReason, CompactionRejection, CompactionRequestLayout,
    CompactionResult, CompactionSourceObservation, CompactionStrategy, CompactionTerminalOutcome,
    CompactionTrigger, Compactor, CompactorRequestObservation, ContextBudgetSource,
    OverflowRecovery, ProviderContext, CACHE_REPLAY_SUMMARY_V0, COMPACTION_CONTEXT_VERSION,
    INCREMENTAL_CHECKPOINT_UPDATE_V1, STRUCTURED_CHECKPOINT_V1, TOOL_FREE_REPLAY_SUMMARY_V1,
};
pub use default_tools::{
    CodingOperations, DefaultCodingTools, LocalCodingOperations, WorkspaceRoot,
};
pub use error::CoreError;
pub use event::{
    AgentEvent, AgentEventKind, AgentEventPayload, AutomaticCompactionOutcome, CompactionOutcome,
    EventObserver, EventSequence, ObserverFuture, ProviderRequestSkipReason,
};
pub use hooks::AgentLoopTurnUpdate;
pub use measurement::{
    measure_prompt_cacheability, measure_request_layout, CacheAccountingStatus,
    PromptCacheMeasurement,
};
pub use run::RunHandle;
pub use state::{
    AgentMessage, AgentSnapshot, AgentToolCall, Message, MessageId, ModelAccountingSnapshot,
    ModelDescriptor, ModelTurnAccounting, RunId, RunSnapshot, ThinkingLevel, TurnId, Usage,
};
pub use tool::{
    AgentToolResult, FailureSignature, ModelToolResult, ToolFailure, ToolFailureCircuitBreaker,
    ToolFailureDisposition, ToolResultProjectionPolicy,
};
