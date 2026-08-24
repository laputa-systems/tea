//! Explicit optional host capabilities for durable child lanes.

mod coordinator;
mod host;
mod tools;
mod types;

#[cfg(test)]
mod tests;

pub(crate) use coordinator::{ActivityWake, SubagentCoordinator};
pub use host::{SubagentHost, SubagentHostFuture, TaskHandle, TaskRuntime};
pub use tools::{
    CHILD_SUBAGENT_INSTRUCTION_SUFFIX, ROOT_SUBAGENT_INSTRUCTION_SUFFIX,
    append_child_subagent_instruction_suffix, append_root_subagent_surface,
    child_subagent_tool_definitions, root_subagent_tool_definitions,
    root_subagent_tool_presentations, root_subagent_tool_surface_digest,
};
pub(crate) use tools::{ROOT_SUBAGENT_TOOL_NAMES, root_subagent_runtime_tools};
pub use types::{
    ApplyAgentChangesResult, ApplyWorkspaceDeltaRequest, FinalizeSubagentRequest,
    InterruptAgentResult, PrepareSubagentRequest, PreparedSubagent, ReopenSubagentRequest,
    SpawnAgentRequest, SpawnedAgentHandle, SubagentHostError, SubagentModel, SubagentPolicy,
    SubagentPolicyError, SubagentReport, SubagentServices, SubagentStatus, SubagentTaskError,
    SubagentWorkspaceChange, WaitAgentsRequest, WaitAgentsResult, WaitReturnWhen, WaitedSubagent,
    WorkspaceApplyOutcome, WorkspaceDelta, WorkspaceFinalization, WorkspaceLease,
};
