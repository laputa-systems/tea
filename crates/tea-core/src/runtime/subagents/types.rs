//! Provider-neutral values exchanged at the subagent host boundary.

use super::{SubagentHost, TaskRuntime};
use crate::runtime::RuntimeServices;
use crate::runtime::HarnessIdentity;
use crate::state::{ModelDescriptor, ThinkingLevel};
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;
use std::time::Duration;
use tea_session::{
    AgentContextMode, AgentId, AgentState, ArtifactId, EntryId, LaneId, OperationId, SessionId,
    Usage, WorkspaceDeltaId, WorkspaceLeaseId,
};

const MAX_MODEL_IDENTIFIER_BYTES: usize = 512;
const MAX_DISPLAY_NAME_BYTES: usize = 256;

/// One model the host permits a root lane to select for a child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentModel {
    /// Exact provider/model/revision descriptor selected by the host.
    pub descriptor: ModelDescriptor,
    /// Bounded user-facing name retained with the session policy.
    pub display_name: String,
    /// Known capacity when the host can state one without a live provider query.
    pub context_window: Option<NonZeroU64>,
}

/// Fixed optional-child policy selected by the host before a session runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentPolicy {
    /// Ordered, host-authorized model catalog.
    pub models: Vec<SubagentModel>,
    /// Maximum concurrently active child operations.
    pub max_concurrent: NonZeroU32,
    /// Maximum number of children one root operation may create.
    pub max_total_per_operation: NonZeroU32,
    /// Per-child wall-clock execution limit owned by the task host.
    pub timeout: Duration,
}

/// Validation failure for a provider-neutral child policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentPolicyError {
    message: String,
}

impl SubagentPolicyError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SubagentPolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SubagentPolicyError {}

impl SubagentPolicy {
    /// Validate a policy without contacting a provider or inspecting host state.
    pub fn validate(&self) -> Result<(), SubagentPolicyError> {
        let first = self
            .models
            .first()
            .ok_or_else(|| SubagentPolicyError::new("subagent model catalog cannot be empty"))?;
        if !(1..=16).contains(&self.max_concurrent.get()) {
            return Err(SubagentPolicyError::new(
                "subagent concurrent limit must be within 1..=16",
            ));
        }
        if !(self.max_concurrent.get()..=64).contains(&self.max_total_per_operation.get()) {
            return Err(SubagentPolicyError::new(
                "subagent total limit must be within max_concurrent..=64",
            ));
        }
        if !(Duration::from_secs(30)..=Duration::from_secs(7_200)).contains(&self.timeout) {
            return Err(SubagentPolicyError::new(
                "subagent timeout must be within 30..=7200 seconds",
            ));
        }
        let provider = &first.descriptor.provider;
        let mut identities = std::collections::BTreeSet::new();
        let mut model_ids = std::collections::BTreeSet::new();
        for model in &self.models {
            if !stable_subagent_text(&model.descriptor.provider, MAX_MODEL_IDENTIFIER_BYTES)
                || !stable_subagent_text(&model.descriptor.model, MAX_MODEL_IDENTIFIER_BYTES)
                || model
                    .descriptor
                    .revision
                    .as_ref()
                    .is_some_and(|value| !stable_subagent_text(value, MAX_MODEL_IDENTIFIER_BYTES))
            {
                return Err(SubagentPolicyError::new(
                    "subagent model descriptor contains an empty, padded, control, or oversized identifier",
                ));
            }
            if !stable_subagent_text(&model.display_name, MAX_DISPLAY_NAME_BYTES) {
                return Err(SubagentPolicyError::new(
                    "subagent display name must be non-empty, unpadded, control-free, and bounded",
                ));
            }
            if &model.descriptor.provider != provider {
                return Err(SubagentPolicyError::new(
                    "all subagent models must belong to one provider family",
                ));
            }
            let identity = (
                model.descriptor.provider.clone(),
                model.descriptor.model.clone(),
                model.descriptor.revision.clone(),
            );
            if !identities.insert(identity) {
                return Err(SubagentPolicyError::new(
                    "subagent model catalog contains a duplicate descriptor",
                ));
            }
            if !model_ids.insert(model.descriptor.model.clone()) {
                return Err(SubagentPolicyError::new(
                    "subagent model catalog contains a duplicate model ID",
                ));
            }
        }
        Ok(())
    }
}

fn stable_subagent_text(value: &str, maximum_bytes: usize) -> bool {
    !value.is_empty()
        && value == value.trim()
        && value.len() <= maximum_bytes
        && !value.chars().any(char::is_control)
}

/// Optional child execution services. `None` at the supervisor boundary means
/// there is no coordinator, child tool, provider factory, or hidden fallback.
#[derive(Clone)]
pub struct SubagentServices {
    /// Immutable host policy persisted by the durable session layer.
    pub policy: SubagentPolicy,
    /// Workspace and delta authority owned by the embedding host.
    pub host: Arc<dyn SubagentHost>,
    /// Structured-concurrency executor port owned by the embedding host.
    pub tasks: Arc<dyn TaskRuntime>,
}

impl std::fmt::Debug for SubagentServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentServices")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// Stable opaque logical workspace authority retained by the host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceLease {
    /// Deterministic durable lease identifier.
    pub id: WorkspaceLeaseId,
    /// Stable model-facing workspace label, never an operational temporary path.
    pub logical_workspace: String,
}

/// Durable description of one isolated child change set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceDelta {
    /// Deterministic durable delta identifier.
    pub id: WorkspaceDeltaId,
    /// Child that owns this immutable isolated result.
    pub agent_id: AgentId,
    /// Exact host workspace lease from which the result was finalized.
    pub workspace_lease_id: WorkspaceLeaseId,
    /// Host-proven base tree or commit identity.
    pub base_commit: String,
    /// Host-proven resulting tree or commit identity.
    pub result_commit: String,
    /// Deterministically ordered repository-relative changed paths.
    pub changed_paths: Vec<String>,
    /// Immutable host artifact that contains the patch bytes.
    pub patch_artifact: ArtifactId,
}

/// Exact outcome of freezing one isolated child workspace.
///
/// `NoChanges` is a successful, durable completion state, not a malformed
/// empty patch. Only [`Self::Delta`] authorizes a later parent application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceFinalization {
    /// The isolated workspace contains no child-owned repository changes.
    NoChanges,
    /// One immutable, nonempty isolated change set is ready for retention.
    Delta(WorkspaceDelta),
}

/// Host-owned result after an application attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceApplyOutcome {
    /// The durable patch committed completely.
    Applied { changed_paths: Vec<String> },
    /// Preflight classified conflicts without mutating the parent workspace.
    Conflict { conflicting_paths: Vec<String> },
    /// The host proved that every attempted mutation was rolled back to the
    /// original parent state. This is distinct from an indeterminate attempt:
    /// callers may safely inspect or retry according to their own policy.
    RolledBack { diagnostic: String },
    /// A crash or partial mutation cannot be proven safe to retry.
    Indeterminate { diagnostic: String },
}

/// Parent-visible classified result of one `apply_agent_changes` request.
///
/// Only `Applied` adds a durable application fact. The remaining variants
/// deliberately retain the host's honest classification without inventing a
/// durable success for a repository mutation that did not commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyAgentChangesResult {
    /// The immutable child delta is proven present in the root workspace.
    Applied {
        /// Applied durable delta.
        delta_id: WorkspaceDeltaId,
        /// Exact paths from the immutable delta.
        changed_paths: Vec<String>,
    },
    /// Preflight found a non-mutating merge conflict.
    Conflict {
        /// Delta whose application conflicted.
        delta_id: WorkspaceDeltaId,
        /// Paths the host classified as conflicting.
        conflicting_paths: Vec<String>,
        /// Immutable patch retained for explicit inspection.
        patch_artifact: ArtifactId,
    },
    /// The host proved an attempted application returned to the original tree.
    RolledBack {
        /// Delta whose attempted application rolled back.
        delta_id: WorkspaceDeltaId,
        /// Bounded host diagnostic.
        diagnostic: String,
    },
    /// The host cannot prove a safe repository state after an application attempt.
    Indeterminate {
        /// Delta whose attempted application is ambiguous.
        delta_id: WorkspaceDeltaId,
        /// Bounded host diagnostic requiring explicit user inspection.
        diagnostic: String,
    },
}

/// Data required before a child lane can become durable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareSubagentRequest {
    /// Durable session identity in the host's own stable spelling.
    pub session_id: SessionId,
    /// Deterministic child agent identity.
    pub agent_id: AgentId,
    /// Parent lane that created the child.
    pub parent_lane_id: LaneId,
    /// Parent root operation that owns the child task.
    pub parent_operation_id: OperationId,
    /// Child model chosen from the fixed host catalog.
    pub model: SubagentModel,
    /// Child semantic-context mode selected by the root tool call.
    pub context_mode: AgentContextMode,
    /// Resolved child reasoning level after explicit-or-inherited selection.
    /// The host uses this while constructing lane-local runtime services.
    pub thinking: ThinkingLevel,
    /// Exact parent epoch source leaf for a `parent` context child.
    pub parent_source_leaf_id: Option<EntryId>,
    /// Current parent branch leaf used to snapshot the isolated workspace.
    /// This may advance between sequential tool calls in one assistant batch.
    pub workspace_source_leaf_id: Option<EntryId>,
    /// Parent durable tool-start idempotency key that derived this agent.
    pub spawn_idempotency_key: String,
}

/// Parsed, provider-neutral request accepted by the root `spawn_agent` tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnAgentRequest {
    /// Stable task label unique within one root operation.
    pub task_name: String,
    /// Complete child assignment, retained as the child operation's sole input.
    pub task: String,
    /// One exact provider-local model ID from the fixed policy catalog.
    pub model: String,
    /// Explicit reasoning level, or `None` to inherit the current parent lane.
    pub thinking: Option<ThinkingLevel>,
    /// Whether the child starts clean or from the parent epoch's source leaf.
    pub context_mode: AgentContextMode,
}

/// Immediate durable handle returned after a child operation has been accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnedAgentHandle {
    /// Stable child identity derived from the parent durable spawn intent.
    pub agent_id: AgentId,
    /// Accepted child operation identity.
    pub operation_id: OperationId,
    /// The caller-supplied stable task label.
    pub task_name: String,
    /// Current durable child state when the handle was produced.
    pub state: AgentState,
}

/// Requested settlement condition for [`WaitAgentsRequest`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitReturnWhen {
    /// Return after at least one requested child has a durable terminal fact.
    Any,
    /// Return only after every requested child has a durable terminal fact.
    All,
}

/// Provider-neutral root request for selected child results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitAgentsRequest {
    /// Agent IDs or stable task names, retained in caller-requested order.
    pub targets: Vec<String>,
    /// Whether one or all selected children must finish.
    pub return_when: WaitReturnWhen,
    /// Maximum event-driven wait interval owned by the task host.
    pub timeout: Duration,
}

/// Bounded parent-facing report locator for one settled child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentReport {
    /// Deterministic middle-truncated preview, never more than 16 KiB UTF-8.
    pub preview: String,
    /// Immutable full-report artifact when the durable report was oversized.
    pub artifact_id: Option<ArtifactId>,
}

/// Metadata about one optional immutable workspace result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentWorkspaceChange {
    /// Durable isolated workspace result identity.
    pub delta_id: WorkspaceDeltaId,
    /// Ordered repository-relative paths, available only to `wait_agent`.
    pub changed_paths: Vec<String>,
    /// Immutable patch bytes, never expanded into a tool result.
    pub patch_artifact: ArtifactId,
}

/// Durable child observation shared by wait, list, and interrupt results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentStatus {
    /// Deterministic child identity.
    pub agent_id: AgentId,
    /// Accepted child operation.
    pub operation_id: OperationId,
    /// Stable name unique within the owning root operation.
    pub task_name: String,
    /// Full configured provider/model/revision descriptor.
    pub model: ModelDescriptor,
    /// Persisted child thinking spelling.
    pub thinking: String,
    /// Current durable graph state.
    pub state: AgentState,
    /// Immutable semantic-context choice at spawn time.
    pub context_mode: AgentContextMode,
    /// Aggregate durable usage for the child operation.
    pub usage: Usage,
    /// Isolated result if it has become durable.
    pub workspace_change: Option<SubagentWorkspaceChange>,
}

/// One selected durable child result returned by event-driven waiting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitedSubagent {
    /// Durable child status at the moment the condition became true.
    pub status: SubagentStatus,
    /// Bounded final report locator. This exists only after terminality.
    pub report: SubagentReport,
}

/// Stable result for one `wait_agent` execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitAgentsResult {
    /// Completed selected children, in exactly caller-requested order.
    pub completed: Vec<WaitedSubagent>,
    /// Still-open selected children, in exactly caller-requested order.
    pub pending: Vec<SubagentStatus>,
    /// True only when the host timer expired before the requested condition.
    pub timed_out: bool,
}

/// Durable before-and-after observation from one idempotent interruption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterruptAgentResult {
    /// Status before cancellation was requested.
    pub previous: AgentState,
    /// Status after terminal finalization and workspace cleanup.
    pub resulting: AgentState,
    /// The child selected by stable ID or task name.
    pub agent_id: AgentId,
}

/// Data required to regain host authority for an already durable child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReopenSubagentRequest {
    /// Durable session identity in the host's own stable spelling.
    pub session_id: SessionId,
    /// Deterministic child agent identity.
    pub agent_id: AgentId,
    /// Existing durable lease identifier.
    pub workspace_lease_id: WorkspaceLeaseId,
    /// Child model chosen when the lane was created.
    pub model: SubagentModel,
    /// Persisted child reasoning level that must be reinstalled before drive.
    pub thinking: ThinkingLevel,
}

/// Data required to freeze child workspace output after operation settlement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizeSubagentRequest {
    /// Deterministic child agent identity.
    pub agent_id: AgentId,
    /// Workspace lease to finalize exactly once.
    pub workspace: WorkspaceLease,
}

/// Data required to apply exactly one durable child delta to its parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyWorkspaceDeltaRequest {
    /// Exact delta to preflight and apply.
    pub delta: WorkspaceDelta,
    /// Target root lane.
    pub target_lane_id: LaneId,
}

/// Child workspace plus the lane-local executable services bound to it.
#[derive(Clone)]
pub struct PreparedSubagent {
    /// The host-owned isolated workspace authority.
    pub workspace: WorkspaceLease,
    /// Pre-seeded immutable child harness selected for this exact model.
    pub harness_identity: HarnessIdentity,
    /// Provider, compactor, tools, and policies for this one child lane.
    pub runtime_services: RuntimeServices,
}

impl std::fmt::Debug for PreparedSubagent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedSubagent")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

/// A host-side workspace failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentHostError {
    /// Stable bounded diagnostic supplied by the host.
    pub message: String,
}

impl std::fmt::Display for SubagentHostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SubagentHostError {}

/// A task host refused structured ownership of child work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentTaskError {
    /// Stable bounded diagnostic supplied by the host.
    pub message: String,
}

impl std::fmt::Display for SubagentTaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SubagentTaskError {}
