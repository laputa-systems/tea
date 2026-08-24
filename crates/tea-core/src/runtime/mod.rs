//! Managed sessions and externally hosted epochs built from immutable harness revisions.

#![allow(missing_docs)]

pub(crate) mod artifact;
pub(crate) mod artifact_tools;
pub(crate) mod context;
pub(crate) mod events;
pub(crate) mod harness_tool;
mod hosted;
pub(crate) mod lifecycle;
pub(crate) mod services;
pub(crate) mod subagents;
pub(crate) mod supervisor;
pub(crate) mod trace;

#[cfg(test)]
mod tests;

pub use artifact::{
    RetainedToolResult, ToolResultRetentionError, retain_tool_result_with_projection,
};
pub use context::{
    ContextAnnotation, ContextProjectionPatch, DerivedContext, ProviderLimits,
    derive_model_context, derive_model_context_with_patch,
};
pub use events::{
    ArtifactEvent, DiagnosticCode, HarnessEvent, HarnessSnapshotView, LaneSnapshotView,
    SessionEvent, TeaEvent, TeaEventSubscription, ValidationStage,
};
pub use hosted::{HostedEpoch, HostedEpochInput};
pub use services::{RuntimePolicyIdentities, RuntimeServices};
pub use subagents::{
    ApplyAgentChangesResult, ApplyWorkspaceDeltaRequest, CHILD_SUBAGENT_INSTRUCTION_SUFFIX,
    FinalizeSubagentRequest, InterruptAgentResult, PrepareSubagentRequest, PreparedSubagent,
    ROOT_SUBAGENT_INSTRUCTION_SUFFIX, ReopenSubagentRequest, SpawnAgentRequest, SpawnedAgentHandle,
    SubagentHost, SubagentHostError, SubagentHostFuture, SubagentModel, SubagentPolicy,
    SubagentPolicyError, SubagentReport, SubagentServices, SubagentStatus, SubagentTaskError,
    SubagentWorkspaceChange, TaskHandle, TaskRuntime, WaitAgentsRequest, WaitAgentsResult,
    WaitReturnWhen, WaitedSubagent, WorkspaceApplyOutcome, WorkspaceDelta, WorkspaceFinalization,
    WorkspaceLease, append_child_subagent_instruction_suffix, append_root_subagent_surface,
    child_subagent_tool_definitions, root_subagent_tool_definitions,
    root_subagent_tool_presentations, root_subagent_tool_surface_digest,
};
pub use supervisor::{
    DurableOperation, HarnessIdentity, SessionSupervisor, SessionSupervisorInput,
    SessionSupervisorReopenInput,
};
