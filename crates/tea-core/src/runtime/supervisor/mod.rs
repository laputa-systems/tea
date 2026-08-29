//! Operation orchestration that binds core effects to the session WAL.

use super::artifact::{
    RetainedToolResult, projection_content, retain_direct_recovery_result_with_projection,
    retain_tool_result_with_projection,
};
use super::artifact_tools::{STABLE_ARTIFACT_TOOL_NAMES, stable_artifact_tools};
use super::context::derive_snapshot_context_with_policies;
use super::events::EventHub;
use super::harness_tool::{STABLE_HARNESS_TOOL_NAME, stable_harness_tools};
use super::subagents::{
    ActivityWake, ApplyAgentChangesResult, ApplyWorkspaceDeltaRequest, FinalizeSubagentRequest,
    InterruptAgentResult, PreparedSubagent, ROOT_SUBAGENT_TOOL_NAMES, ReopenSubagentRequest,
    SpawnAgentRequest, SpawnedAgentHandle, SubagentCoordinator, SubagentModel, SubagentReport,
    SubagentStatus, SubagentWorkspaceChange, WaitAgentsRequest, WaitAgentsResult, WaitReturnWhen,
    WaitedSubagent, WorkspaceDelta, WorkspaceFinalization, root_subagent_runtime_tools,
};
use super::trace::{DurableTraceRedactor, TraceCaptureSink};
mod lane;
mod operation;
mod recovery;

use crate::agent::Agent;
use crate::effect::EffectId;
use crate::error::CoreError;
use crate::event::{AgentEvent, EventObserver, ObserverFuture};
use crate::harness::{
    AUTHORING_AUTHORIZATION_METADATA_KEY, HarnessActor, HarnessError, HarnessResolver,
    HarnessRevisionReason, HarnessRevisionV1, HarnessSurface, ResolvedHarness,
    SELF_EXTENSION_MODE_METADATA_KEY, SelfExtensionMode, SubagentRecoveryStage,
    inspect_tool_schema_deviation,
};
use crate::runtime::{
    ArtifactEvent, HarnessEvent, HarnessSnapshotView, ProviderLimits, RuntimeServices,
    SessionEvent, TeaEvent, TeaEventSubscription,
};
use crate::scheduler::CancellationToken;
use crate::tool::truncate_middle;
use lane::LaneRuntime;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::effect::{
    DurableWriteRequest, EffectAction, EffectCompletion, EffectFuture, EffectGate, EffectGateError,
    EffectOutcome, EffectSubject, HookEffectOutcome, HookInvocation, ProviderEffectOutcome,
    ProviderResponse, RunProvenance, ToolEffectOutcome,
};
use tea_core::harness::extension::{
    CollectedExtensionMemoryProposal, ExtensionCommandInput, ExtensionCommandResult,
    ExtensionError, ExtensionHostCommandDescription, ExtensionIdleInput, ExtensionMemoryCollector,
    ExtensionMemoryRetention, ExtensionMemoryVisibility, ExtensionOperationOutcome,
    ExtensionStateStore, ExtensionStateUpdate, ExtensionStateView,
};
use tea_core::state::{
    AgentMessage, AgentSnapshot, AgentToolCall, MessageId, ModelDescriptor, SerializedJson,
    StopReason, ThinkingLevel, ToolCallId,
};
use tea_core::tool::{AgentTool, AgentToolResult, ToolCall, ToolFailureDisposition, ToolRegistry};
use tea_core::trace::TraceObserver;
use tea_protocol::JsonValue;
use tea_session::{
    AgentContextMode, AgentId, AgentSpawnedFact, AgentState, AgentTaskFinishedFact, ArtifactStore,
    CanonicalHashWriter, CoreRunId, Digest, EntryId, EpochFinishReason, EpochFinishedRecord,
    EpochId, EpochStartedRecord, HarnessRevisionChangedEntry, HarnessRevisionId, HarnessSnapshotId,
    LaneId, LaneMutation, LaneRecord, MemoryRetention, MemoryVisibility, ModelChangedEntry,
    ModelHarnessProfileId, OperationFinishedRecord, OperationId, OperationKind, OperationOutcome,
    OperationStartedRecord, PayloadRef, PluginMemoryEntry, ProviderRequestId,
    ProviderRequestSettledRecord, ProviderRequestStartedRecord, ProviderSettlementClassification,
    ProvisionedEntry, RecoveryPlan, SchemaFieldMismatch, SessionEntry, SessionFact,
    SessionSnapshot, SessionWriter, StepAttemptedRecord, StepId, StepKind, SubagentModelRecord,
    ThinkingChangedEntry, ToolReplayPolicy, ToolResultEntry, ToolSchemaDeviationFact,
    ToolStartedRecord, TraceArtifactFact, Usage, WorkspaceDeltaAppliedFact, WorkspaceDeltaFact,
    WorkspaceDeltaId, WorkspaceLeaseId, derive_subagent_operation_id, reduce_agent_graph,
    reduce_lane,
};
use tea_trace::{JsonLinesSink, RedactingSink, TraceEvent, TraceSink};

/// Exact immutable identity selected for an operation's core epochs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessIdentity {
    revision_id: HarnessRevisionId,
    snapshot_id: HarnessSnapshotId,
    profile_id: ModelHarnessProfileId,
}

impl HarnessIdentity {
    /// Construct an identity from validated durable values.
    pub fn new(
        revision_id: HarnessRevisionId,
        snapshot_id: HarnessSnapshotId,
        profile_id: ModelHarnessProfileId,
    ) -> Self {
        Self {
            revision_id,
            snapshot_id,
            profile_id,
        }
    }

    /// Validate and construct the three immutable identity values.
    pub fn from_strings(
        revision_id: impl Into<String>,
        snapshot_id: impl Into<String>,
        profile_id: impl Into<String>,
    ) -> Result<Self, HarnessError> {
        Ok(Self::new(
            HarnessRevisionId::new(revision_id.into())
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
            HarnessSnapshotId::new(snapshot_id.into())
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
            ModelHarnessProfileId::new(profile_id.into())
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
        ))
    }

    /// Borrow the immutable revision identity.
    pub fn revision_id(&self) -> &HarnessRevisionId {
        &self.revision_id
    }

    /// Borrow the immutable snapshot identity.
    pub fn snapshot_id(&self) -> &HarnessSnapshotId {
        &self.snapshot_id
    }

    /// Borrow the immutable model-harness profile identity.
    pub fn profile_id(&self) -> &ModelHarnessProfileId {
        &self.profile_id
    }
}

/// The terminal durable outcome reported for one caller-visible operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableOperation {
    id: OperationId,
    outcome: OperationOutcome,
}

/// Durable result from one extension host command.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionCommandDispatch {
    /// Immutable extension namespace that owned the handler.
    pub extension_id: String,
    /// Constrained command output already persisted by the durable runtime.
    pub result: ExtensionCommandResult,
}

/// One idle-approved follow-up operation for an extension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionContinuation {
    /// Immutable extension namespace that requested the follow-up.
    pub extension_id: String,
    /// Internal model context, never a user-authored message.
    pub input: String,
}

impl DurableOperation {
    /// Borrow the durable operation identity.
    pub fn id(&self) -> &OperationId {
        &self.id
    }

    /// Borrow the terminal durable outcome.
    pub fn outcome(&self) -> &OperationOutcome {
        &self.outcome
    }

    /// Return whether the operation settled normally.
    pub fn is_completed(&self) -> bool {
        self.outcome == OperationOutcome::Completed
    }
}

/// Explicit inputs for one durable multi-lane supervisor.
pub struct SessionSupervisorInput<S> {
    /// The sole serialized durable-session writer.
    pub session: S,
    /// Immutable harness repository and resolver shared by every lane.
    pub resolver: Arc<HarnessResolver>,
    /// The root lane's initially committed immutable harness identity.
    pub root_identity: HarnessIdentity,
    /// Executable authority for the root lane only.
    pub root_services: RuntimeServices,
    /// Immutable artifact authority shared by the one durable session.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Maximum automatic immutable harness activations allowed per operation.
    /// Zero disables automatic activation.
    pub rollover_budget: u32,
    /// Optional explicit child-lane capability. `None` installs nothing.
    pub subagents: Option<super::subagents::SubagentServices>,
}

/// Inputs for reopening a durable supervisor from its committed session prefix.
///
/// The active root identity is deliberately absent: [`SessionSupervisor::reopen`]
/// derives it from the root lane's committed harness revision. Child lane
/// services are supplied afresh by the host and are never reconstructed from
/// durable session facts.
pub struct SessionSupervisorReopenInput<S> {
    /// The already-open durable session writer.
    pub session: S,
    /// Immutable harness repository and resolver shared by every lane.
    pub resolver: Arc<HarnessResolver>,
    /// Fresh executable authority for the root lane.
    pub root_services: RuntimeServices,
    /// Fresh executable authority for already durable non-root lanes.
    pub lane_services: BTreeMap<LaneId, RuntimeServices>,
    /// Immutable artifact authority shared by the one durable session.
    pub artifacts: Arc<dyn ArtifactStore>,
    /// Maximum automatic immutable harness activations allowed per operation.
    pub rollover_budget: u32,
    /// Optional explicit child-lane capability. `None` installs nothing.
    pub subagents: Option<super::subagents::SubagentServices>,
}

/// One durable session with independently executable agent lanes.
pub struct SessionSupervisor<S> {
    session: Arc<Mutex<S>>,
    /// Every epoch resolves its provider-independent harness from this
    /// semantic branch manager and its committed immutable revision.
    manager: Arc<HarnessResolver>,
    artifacts: Arc<dyn ArtifactStore>,
    rollover_budget: u32,
    /// Each lane has independently owned active state, services, and prompt
    /// cache continuity. The session writer remains shared and serialized.
    lanes: Mutex<BTreeMap<LaneId, Arc<LaneRuntime>>>,
    root_lane_id: LaneId,
    /// Optional explicit child capability. It has no default construction.
    subagents: Option<super::subagents::SubagentServices>,
    /// Process-local child task ownership, present only with explicit services.
    coordinator: Mutex<Option<Arc<SubagentCoordinator<S>>>>,
    /// Process-local live-event fanout. It never owns durable state.
    events: Arc<EventHub>,
    /// Serializes snapshot registration with post-commit publication so a UI
    /// receives one atomic view and then only events beyond that view.
    publication: Mutex<()>,
}

impl<S> std::fmt::Debug for SessionSupervisor<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionSupervisor")
            .field("root_lane", &self.root_lane_id)
            .field("rollover_budget", &self.rollover_budget)
            .field(
                "lane_count",
                &self.lanes.lock().map(|lanes| lanes.len()).unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

/// Fully derived input to the one durable child-spawn transaction.
///
/// This is intentionally private: host ports receive only the minimum
/// workspace preparation request, while durable graph linkage stays owned by
/// the supervisor and cannot be supplied by a provider or task runtime.
struct SubagentSpawnIntent {
    session_id: tea_session::SessionId,
    parent_lane_id: LaneId,
    parent_operation_id: OperationId,
    agent_id: AgentId,
    lane_id: LaneId,
    operation_id: OperationId,
    task_name: String,
    task: String,
    model: SubagentModel,
    thinking: ThinkingLevel,
    context_mode: AgentContextMode,
    parent_source_leaf_id: Option<EntryId>,
    workspace_source_leaf_id: Option<EntryId>,
    spawn_tool_call_id: String,
    spawn_idempotency_key: String,
    durable_active: u32,
    durable_total: u32,
    existing: Option<tea_session::AgentGraphNode>,
}

impl SubagentSpawnIntent {
    fn prepare_request(&self) -> super::subagents::PrepareSubagentRequest {
        super::subagents::PrepareSubagentRequest {
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            parent_lane_id: self.parent_lane_id.clone(),
            parent_operation_id: self.parent_operation_id.clone(),
            model: self.model.clone(),
            context_mode: self.context_mode,
            thinking: self.thinking,
            parent_source_leaf_id: self.parent_source_leaf_id.clone(),
            workspace_source_leaf_id: self.workspace_source_leaf_id.clone(),
            spawn_idempotency_key: self.spawn_idempotency_key.clone(),
        }
    }
}

/// Post-commit evidence needed before publication and task handoff.
struct AcceptedSubagentOperation {
    sequence: tea_session::Sequence,
}

fn required_provenance_id<T, E>(
    value: &Option<String>,
    name: &str,
    parse: impl FnOnce(String) -> Result<T, E>,
) -> Result<T, HarnessError>
where
    E: std::fmt::Display,
{
    let value = value.as_ref().ok_or_else(|| {
        HarnessError::invalid_state(format!("spawn_agent is missing {name} provenance"))
    })?;
    parse(value.clone()).map_err(|error| HarnessError::invalid_state(error.to_string()))
}

fn validate_subagent_spawn_request(request: &SpawnAgentRequest) -> Result<(), HarnessError> {
    let valid_task_name = request.task_name.len() <= 64
        && request
            .task_name
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                0 => byte.is_ascii_lowercase(),
                _ => byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_',
            });
    if !valid_task_name {
        return Err(HarnessError::invalid_state(
            "spawn_agent task_name must match ^[a-z][a-z0-9_]{0,63}$",
        ));
    }
    if request.task.trim().is_empty() || request.task.len() > 64 * 1024 {
        return Err(HarnessError::invalid_state(
            "spawn_agent task must be trimmed non-empty UTF-8 within 65536 bytes",
        ));
    }
    Ok(())
}

fn durable_subagent_model(model: &SubagentModel) -> SubagentModelRecord {
    SubagentModelRecord {
        provider: model.descriptor.provider.clone(),
        model: model.descriptor.model.clone(),
        revision: model.descriptor.revision.clone(),
        display_name: model.display_name.clone(),
        context_window: model.context_window.map(|value| value.get()),
    }
}

fn verify_subagent_policy_fact(
    fact: &tea_session::SubagentPolicyFact,
    policy: &super::subagents::SubagentPolicy,
) -> Result<(), HarnessError> {
    let expected_models = policy
        .models
        .iter()
        .map(durable_subagent_model)
        .collect::<Vec<_>>();
    let expected_digest = super::subagents::root_subagent_tool_surface_digest(policy)
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
    if fact.schema_version != 1
        || fact.models != expected_models
        || fact.max_concurrent != policy.max_concurrent.get()
        || fact.max_total_per_operation != policy.max_total_per_operation.get()
        || fact.timeout_ms != policy.timeout.as_millis().min(u128::from(u64::MAX)) as u64
        || fact.tool_surface_digest != expected_digest
    {
        return Err(HarnessError::invalid_state(
            "explicit subagent services do not match the persisted immutable policy",
        ));
    }
    Ok(())
}

fn validate_enabled_root_subagent_surface(
    resolved: &ResolvedHarness,
    policy: &super::subagents::SubagentPolicy,
    services: &RuntimeServices,
) -> Result<(), HarnessError> {
    let snapshot = resolved.harness_snapshot.as_ref().ok_or_else(|| {
        HarnessError::invalid_state(
            "subagent-enabled root execution requires an immutable harness snapshot",
        )
    })?;
    let suffix = super::subagents::ROOT_SUBAGENT_INSTRUCTION_SUFFIX;
    if !snapshot.spec.base_system_prompt.ends_with(suffix)
        || snapshot.spec.base_system_prompt.matches(suffix).count() != 1
    {
        return Err(HarnessError::invalid_state(
            "subagent-enabled root harness must seed the exact root instruction suffix once",
        ));
    }
    let expected = super::subagents::root_subagent_tool_presentations(policy)
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
    let presentations = &snapshot.spec.tool_presentations;
    if presentations.len() < expected.len()
        || presentations[presentations.len() - expected.len()..] != expected
    {
        return Err(HarnessError::invalid_state(
            "subagent-enabled root harness must end with the exact ordered collaboration presentations",
        ));
    }
    let trusted_prefix = &presentations[..presentations.len() - expected.len()];
    for name in ROOT_SUBAGENT_TOOL_NAMES {
        if trusted_prefix.iter().any(|tool| tool.name == name)
            || snapshot
                .spec
                .plugin_tool_presentations
                .iter()
                .any(|tool| tool.name == name)
            || services.trusted_tools().get(name).is_some()
            || resolved.extension_tools().get(name).is_some()
        {
            return Err(HarnessError::invalid_state(format!(
                "subagent-enabled root harness must expose exactly one host-owned collaboration tool {name}",
            )));
        }
    }
    Ok(())
}

fn validate_child_subagent_surface(
    resolved: &ResolvedHarness,
    services: &RuntimeServices,
) -> Result<(), HarnessError> {
    let snapshot = resolved.harness_snapshot.as_ref().ok_or_else(|| {
        HarnessError::invalid_state(
            "subagent child execution requires an immutable harness snapshot",
        )
    })?;
    let suffix = super::subagents::CHILD_SUBAGENT_INSTRUCTION_SUFFIX;
    if !snapshot.spec.base_system_prompt.ends_with(suffix)
        || snapshot.spec.base_system_prompt.matches(suffix).count() != 1
    {
        return Err(HarnessError::invalid_state(
            "subagent child harness must seed the exact child instruction suffix once",
        ));
    }
    for name in ROOT_SUBAGENT_TOOL_NAMES {
        if snapshot
            .spec
            .tool_presentations
            .iter()
            .any(|tool| tool.name == name)
            || snapshot
                .spec
                .plugin_tool_presentations
                .iter()
                .any(|tool| tool.name == name)
            || services.trusted_tools().get(name).is_some()
            || resolved.extension_tools().get(name).is_some()
        {
            return Err(HarnessError::invalid_state(format!(
                "subagent child harness cannot expose root collaboration tool {name}",
            )));
        }
    }
    Ok(())
}

fn subagent_entry_id(agent_id: &AgentId, kind: &str) -> Result<EntryId, HarnessError> {
    EntryId::new(durable_identifier(
        "subagent-entry",
        [agent_id.as_str(), kind],
    ))
    .map_err(|error| HarnessError::invalid_state(error.to_string()))
}

impl<S> SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Construct a supervisor whose future epochs are resolved from immutable
    /// harness lineage.  The caller must seed the initial
    /// `HarnessRevisionChanged` entry before construction; the manager rejects
    /// a branch that has no durable active revision rather than relying on an
    /// in-memory pointer.
    pub fn create(input: SessionSupervisorInput<S>) -> Result<Arc<Self>, HarnessError> {
        let SessionSupervisorInput {
            mut session,
            resolver: manager,
            root_identity: initial_identity,
            mut root_services,
            artifacts,
            rollover_budget,
            subagents,
        } = input;
        if let Some(services) = &subagents {
            services
                .policy
                .validate()
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        }
        let snapshot = session.snapshot()?;
        let agent_graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        match (&subagents, &agent_graph.policy) {
            (Some(services), Some(fact)) => verify_subagent_policy_fact(fact, &services.policy)?,
            (Some(_), None) => {
                return Err(HarnessError::invalid_state(
                    "explicit subagent services require a persisted immutable subagent policy",
                ));
            }
            (None, Some(_)) => {
                return Err(HarnessError::invalid_state(
                    "a subagent-enabled session requires explicit subagent services before execution",
                ));
            }
            (None, None) => {}
        }
        let persisted_mode = snapshot
            .header()
            .metadata
            .get(SELF_EXTENSION_MODE_METADATA_KEY)
            .and_then(JsonValue::as_str)
            .and_then(SelfExtensionMode::parse)
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "managed harness session metadata must contain {} as off, author, or adaptive",
                    SELF_EXTENSION_MODE_METADATA_KEY,
                ))
            })?;
        if persisted_mode != manager.self_extension_mode_value() {
            return Err(HarnessError::invalid_state(format!(
                "managed harness session mode {} does not match manager mode {}",
                persisted_mode.as_str(),
                manager.self_extension_mode_value().as_str(),
            )));
        }
        let root_lane_id = LaneId::main();
        let reduction = reduce_lane(snapshot.clone(), root_lane_id.clone())?;
        if let Some(level) = reduction.effective_configuration.thinking_level.as_deref() {
            root_services = root_services.thinking_level(thinking_level_from_name(level)?);
        }
        if reduction.lane_state.active_harness_revision.as_ref()
            != Some(initial_identity.revision_id())
        {
            return Err(HarnessError::invalid_state(
                "managed harness requires a committed initial HarnessRevisionChanged entry",
            ));
        }
        if let Some(catalog) = latest_harness_catalog(&snapshot) {
            manager.restore_catalog(catalog, Arc::clone(&artifacts))?;
        } else {
            // The initial branch revision is already durable above. Persist
            // the immutable source/catalog index before any operation can
            // refer to it, so a later reopen does not rely on the host's
            // mutable worktree or an in-memory manager cache.
            manager.persist_catalog(&mut session, artifacts.as_ref())?;
        }
        let resolved = manager.resolve_revision(initial_identity.revision_id(), &root_services)?;
        if resolved.identity != initial_identity {
            return Err(HarnessError::invalid_state(
                "managed harness initial identity does not match its immutable revision",
            ));
        }
        if let Some(services) = &subagents {
            validate_enabled_root_subagent_surface(&resolved, &services.policy, &root_services)?;
        }
        validate_reserved_host_tool_names(&root_services, &resolved)?;
        let root_lane = Arc::new(LaneRuntime::new(root_lane_id.clone(), root_services));
        let supervisor = Arc::new(Self {
            session: Arc::new(Mutex::new(session)),
            manager,
            artifacts,
            rollover_budget,
            lanes: Mutex::new([(root_lane_id.clone(), root_lane)].into_iter().collect()),
            root_lane_id,
            subagents: subagents.clone(),
            coordinator: Mutex::new(None),
            events: Arc::new(EventHub::default()),
            publication: Mutex::new(()),
        });
        if let Some(services) = subagents {
            let coordinator = Arc::new(SubagentCoordinator::new(
                Arc::downgrade(&supervisor),
                services,
            ));
            *supervisor.coordinator.lock().map_err(|_| {
                HarnessError::invalid_state("subagent coordinator mutex is poisoned")
            })? = Some(coordinator);
        }
        Ok(supervisor)
    }

    /// Reopen a managed harness from its committed semantic branch and
    /// immutable catalog.
    ///
    /// This derives the root's active revision, snapshot, and model profile
    /// from durable state. The host must supply executable authority afresh;
    /// no provider, workspace, task executor, or tool is restored from the
    /// session log.
    pub fn reopen(input: SessionSupervisorReopenInput<S>) -> Result<Arc<Self>, HarnessError> {
        let SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            lane_services,
            artifacts,
            rollover_budget,
            subagents,
        } = input;
        let snapshot = session.snapshot()?;
        let catalog = latest_harness_catalog(&snapshot).ok_or_else(|| {
            HarnessError::invalid_state(
                "managed harness reopen requires a committed immutable harness catalog",
            )
        })?;
        resolver.restore_catalog(catalog, Arc::clone(&artifacts))?;
        let root_lane_id = LaneId::main();
        let reduction = reduce_lane(snapshot, root_lane_id)?;
        let revision_id = reduction
            .lane_state
            .active_harness_revision
            .ok_or_else(|| {
                HarnessError::invalid_state(
                    "managed harness reopen requires a committed root harness revision",
                )
            })?;
        let revision = resolver.revision(&revision_id)?;
        let harness_snapshot = resolver.snapshot(&revision.snapshot_id)?;
        let root_identity = HarnessIdentity::new(
            revision.revision_id,
            harness_snapshot.id,
            harness_snapshot.spec.model_harness_profile,
        );
        let supervisor = Self::create(SessionSupervisorInput {
            session,
            resolver,
            root_identity,
            root_services,
            artifacts,
            rollover_budget,
            subagents,
        })?;
        for (lane_id, services) in lane_services {
            supervisor.register_lane(lane_id, services)?;
        }
        Ok(supervisor)
    }

    /// Return one atomic snapshot of the authoritative durable session prefix.
    pub fn snapshot(&self) -> Result<SessionSnapshot, HarnessError> {
        self.session_lock()?.snapshot().map_err(Into::into)
    }

    /// Return one content-free atomic reconnect view of the authoritative
    /// durable session state.
    pub fn snapshot_view(&self) -> Result<HarnessSnapshotView, HarnessError> {
        HarnessSnapshotView::from_session(&self.snapshot()?)
    }

    /// Recompute the durable reducer and verify every direct and retained
    /// harness-source artifact reachable from this supervisor. This is
    /// read-only and can be run after reopen or before an operator export.
    pub fn verify_durable_state(&self) -> Result<tea_session::SessionVerification, HarnessError> {
        let snapshot = self.snapshot()?;
        let additional_roots = self.manager.artifact_roots()?;
        tea_session::verify_session(&snapshot, self.artifacts.as_ref(), additional_roots)
            .map_err(Into::into)
    }

    /// Subscribe a local application consumer to one atomic reconnect view
    /// followed by live events. Events already represented in the snapshot
    /// are suppressed; reconnecting callers receive a new snapshot instead
    /// of a replay log.
    pub fn subscribe_events(&self) -> Result<TeaEventSubscription, HarnessError> {
        let _publication = self
            .publication
            .lock()
            .map_err(|_| HarnessError::invalid_state("harness publication mutex is poisoned"))?;
        let snapshot = HarnessSnapshotView::from_session(&self.snapshot()?)?;
        self.events.subscribe(snapshot)
    }

    /// Return whether a caller-visible durable operation currently owns this
    /// harness. This is a process-local UI/control observation; recovery still
    /// derives authority from the session reducer.
    pub fn is_active(&self) -> bool {
        self.root_lane()
            .map(|lane| lane.active.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// Return the explicit optional child capability, if this host installed
    /// one. No implicit default coordinator or provider factory exists.
    pub fn subagent_services(&self) -> Option<&super::subagents::SubagentServices> {
        self.subagents.as_ref()
    }

    fn subagent_coordinator(&self) -> Result<Option<Arc<SubagentCoordinator<S>>>, HarnessError> {
        self.coordinator
            .lock()
            .map(|coordinator| coordinator.clone())
            .map_err(|_| HarnessError::invalid_state("subagent coordinator mutex is poisoned"))
    }

    #[cfg(test)]
    pub(crate) fn subagent_coordinator_for_test(
        &self,
    ) -> Result<Arc<SubagentCoordinator<S>>, HarnessError> {
        self.subagent_coordinator()?.ok_or_else(|| {
            HarnessError::invalid_state("fixture supervisor has no subagent coordinator")
        })
    }

    #[cfg(test)]
    pub(crate) fn root_abort_requested_for_test(&self) -> Result<bool, HarnessError> {
        Ok(self.root_lane()?.abort_requested.load(Ordering::Acquire))
    }

    #[cfg(test)]
    pub(crate) fn lane_has_active_agent_for_test(
        &self,
        lane_id: &LaneId,
    ) -> Result<bool, HarnessError> {
        Ok(self
            .lane(lane_id)?
            .active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?
            .is_some())
    }

    #[cfg(test)]
    pub(crate) fn clone_session_for_test(&self) -> Result<S, HarnessError>
    where
        S: Clone,
    {
        Ok(self.session_lock()?.clone())
    }

    #[cfg(test)]
    pub(crate) fn reopen_parts_for_test(
        &self,
    ) -> Result<
        (
            Arc<HarnessResolver>,
            RuntimeServices,
            Arc<dyn ArtifactStore>,
            Option<super::subagents::SubagentServices>,
        ),
        HarnessError,
    > {
        Ok((
            Arc::clone(&self.manager),
            self.root_lane()?.runtime_services.clone(),
            Arc::clone(&self.artifacts),
            self.subagents.clone(),
        ))
    }

    #[cfg(test)]
    pub(crate) async fn recover_subagents_for_test(self: &Arc<Self>) -> Result<(), HarnessError> {
        if let Some(coordinator) = self.subagent_coordinator()? {
            self.recover_subagents_before_root_resume(&coordinator)
                .await
        } else {
            Ok(())
        }
    }

    /// Deterministic crash-prefix injection for recovery tests. Production
    /// code always calls `complete_subagent_lane_binding` atomically in its
    /// normal append sequence; this hook stops after the requested durable
    /// prefix so replay evidence covers every suffix boundary.
    #[cfg(test)]
    pub(crate) async fn persist_subagent_prefix_for_test(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
        call: crate::tool::ToolCall,
        provenance: RunProvenance,
        request: SpawnAgentRequest,
        configured_entries: usize,
        append_spawn_fact: bool,
    ) -> Result<AgentId, HarnessError> {
        let intent = self.subagent_spawn_intent(&call, &provenance, &request)?;
        if configured_entries > 3 {
            return Err(HarnessError::invalid_state(
                "fixture subagent prefix has at most three configuration entries",
            ));
        }
        let prepared = coordinator
            .services()
            .host
            .prepare(intent.prepare_request())
            .await
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        self.validate_prepared_subagent(&intent, &prepared)?;
        let mut session = self.session_lock()?;
        session.append_lane_mutation(LaneMutation::Created {
            lane_id: intent.lane_id.clone(),
            base_leaf_id: intent.parent_source_leaf_id.clone(),
        })?;
        for entry in self
            .subagent_lane_binding_entries(&intent, &prepared)?
            .into_iter()
            .take(configured_entries)
        {
            session.append_entry(&intent.lane_id, entry)?;
        }
        if append_spawn_fact {
            if configured_entries != 3 {
                return Err(HarnessError::invalid_state(
                    "fixture spawn fact requires a complete child lane configuration",
                ));
            }
            self.append_subagent_spawn_fact(&mut session, &intent, &prepared)?;
        }
        Ok(intent.agent_id)
    }

    #[cfg(test)]
    pub(crate) fn start_subagent_epoch_for_test(
        &self,
        agent_id: &AgentId,
    ) -> Result<EpochId, HarnessError> {
        let graph = reduce_agent_graph(&self.snapshot()?)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let node = graph.agents.get(agent_id).ok_or_else(|| {
            HarnessError::invalid_state("fixture child epoch has no durable spawn node")
        })?;
        let operation_id = node.operation_id.as_ref().ok_or_else(|| {
            HarnessError::invalid_state("fixture child epoch has no accepted operation")
        })?;
        let lane = self.lane(&node.spawned.lane_id)?;
        self.start_epoch(&lane, operation_id)
    }

    #[cfg(test)]
    pub(crate) fn append_subagent_assistant_for_test(
        &self,
        agent_id: &AgentId,
        content: String,
    ) -> Result<(), HarnessError> {
        let graph = reduce_agent_graph(&self.snapshot()?)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let node = graph.agents.get(agent_id).ok_or_else(|| {
            HarnessError::invalid_state("fixture child report has no durable spawn node")
        })?;
        self.session_lock()?.append_entry(
            &node.spawned.lane_id,
            ProvisionedEntry::assistant(
                subagent_entry_id(agent_id, "fixture-report")?,
                content,
                Vec::new(),
            ),
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn append_subagent_open_tool_prefix_for_test(
        &self,
        agent_id: &AgentId,
        epoch_id: &EpochId,
    ) -> Result<(), HarnessError> {
        let graph = reduce_agent_graph(&self.snapshot()?)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let node = graph.agents.get(agent_id).ok_or_else(|| {
            HarnessError::invalid_state("fixture child tool prefix has no durable spawn node")
        })?;
        let operation_id = node.operation_id.as_ref().ok_or_else(|| {
            HarnessError::invalid_state("fixture child tool prefix has no accepted operation")
        })?;
        let assistant_id = subagent_entry_id(agent_id, "fixture-tool-assistant")?;
        let result_id = subagent_entry_id(agent_id, "fixture-tool-result")?;
        self.session_lock()?.append_entry(
            &node.spawned.lane_id,
            ProvisionedEntry::assistant(
                assistant_id.clone(),
                String::new(),
                vec![tea_session::AssistantToolCall::new(
                    "fixture-child-open-tool-call",
                    "fixture_child_tool",
                    JsonValue::parse("{}").expect("fixture object is valid JSON"),
                )],
            ),
        )?;
        self.session_lock()?
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
                tea_session::RecordId::new(durable_identifier(
                    "subagent-tool-record",
                    [agent_id.as_str()],
                ))
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
                operation_id.clone(),
                epoch_id.clone(),
                assistant_id,
                0,
                "fixture-child-open-tool-call",
                "fixture_child_tool",
                JsonValue::parse("{}").expect("fixture object is valid JSON"),
                result_id,
                ToolReplayPolicy::Never,
                tea_session::Digest::from_bytes("fixture-child-open-tool-digest"),
                node.spawned.harness_revision_id.clone(),
                "fixture-child-open-tool-key",
            )))?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn persist_subagent_delta_without_terminal_for_test(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
        agent_id: &AgentId,
    ) -> Result<(), HarnessError> {
        let graph = reduce_agent_graph(&self.snapshot()?)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let node = graph.agents.get(agent_id).cloned().ok_or_else(|| {
            HarnessError::invalid_state("fixture child delta has no durable spawn node")
        })?;
        self.force_abort_open_subagent_operation(&node)?;
        let prepared = self.reopen_subagent_prepared(coordinator, &node).await?;
        self.validate_reopened_subagent(&node, &prepared)?;
        let WorkspaceFinalization::Delta(delta) = coordinator
            .services()
            .host
            .finalize(FinalizeSubagentRequest {
                agent_id: agent_id.clone(),
                workspace: prepared.workspace,
            })
            .await
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?
        else {
            return Err(HarnessError::invalid_state(
                "fixture delta prefix requires a changed child workspace",
            ));
        };
        self.session_lock()?
            .append_fact(SessionFact::WorkspaceDelta(
                self.workspace_delta_fact(&node, delta)?,
            ))?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn provenance_for_test(
        &self,
        lane_id: &LaneId,
        operation_id: &OperationId,
        epoch_id: &EpochId,
        identity: &HarnessIdentity,
    ) -> Result<RunProvenance, HarnessError> {
        let lane = self.lane(lane_id)?;
        self.provenance(&lane, operation_id, epoch_id, identity, None)
    }

    #[cfg(test)]
    pub(crate) fn artifact_bytes_for_test(
        &self,
        artifact_id: tea_session::ArtifactId,
    ) -> Result<Vec<u8>, HarnessError> {
        Ok(self.artifacts.get(artifact_id)?)
    }

    #[cfg(test)]
    pub(crate) fn claim_root_before_acceptance_for_test(&self) -> Result<(), HarnessError> {
        let claim = self.claim_lane_operation(self.root_lane()?)?;
        // The test needs the exact post-claim/pre-OperationStarted interval.
        // Keep that claim alive for its short-lived isolated supervisor.
        std::mem::forget(claim);
        Ok(())
    }

    /// Commit one root `spawn_agent` intent into a child lane and hand its
    /// accepted operation to the explicitly supplied task runtime.
    ///
    /// The coordinator retains only a short-lived reservation while the host
    /// prepares the isolated workspace.  Every replay identity, task-name
    /// uniqueness decision, parent-context fork, child configuration, and
    /// accepted operation below is derived from the serialized session
    /// prefix.  A restarted process therefore cannot manufacture a sibling
    /// child merely because it lost its volatile task map.
    pub(crate) async fn accept_subagent_spawn(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
        call: crate::tool::ToolCall,
        provenance: RunProvenance,
        request: SpawnAgentRequest,
    ) -> Result<SpawnedAgentHandle, HarnessError> {
        let intent = self.subagent_spawn_intent(&call, &provenance, &request)?;

        if let Some(existing) = intent.existing.clone() {
            self.validate_replayed_subagent(&intent, &existing)?;
            if existing.operation_id.is_none() {
                // The immutable spawn node committed before child operation
                // acceptance. Replay the original root tool intent to append
                // that exact assignment/operation suffix; never invent a new
                // task from volatile coordinator state.
                let prepared = coordinator
                    .services()
                    .host
                    .reopen(ReopenSubagentRequest {
                        session_id: intent.session_id.clone(),
                        agent_id: intent.agent_id.clone(),
                        workspace_lease_id: existing.spawned.workspace_lease_id.clone(),
                        model: intent.model.clone(),
                        thinking: intent.thinking,
                    })
                    .await
                    .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
                self.validate_prepared_subagent(&intent, &prepared)?;
                self.ensure_subagent_lane_registered(
                    intent.lane_id.clone(),
                    prepared
                        .runtime_services
                        .clone()
                        .thinking_level(intent.thinking),
                )?;
                let accepted = self.commit_subagent_operation(&intent, &prepared)?;
                self.publish_event(TeaEvent::Session(SessionEvent::OperationAccepted {
                    sequence: accepted.sequence,
                    lane_id: intent.lane_id.clone(),
                    operation_id: intent.operation_id.clone(),
                }))?;
                self.start_subagent_task(coordinator, &intent, prepared.workspace)?;
                return Ok(SpawnedAgentHandle {
                    agent_id: intent.agent_id,
                    operation_id: intent.operation_id,
                    task_name: request.task_name,
                    state: AgentState::Running,
                });
            }
            let operation_id = existing
                .operation_id
                .clone()
                .expect("checked durable child operation above");
            if matches!(
                existing.state,
                AgentState::Running | AgentState::Finalizing { .. }
            ) && !coordinator.has_handle(&existing.spawned.agent_id)
            {
                // An in-memory handle is intentionally not durable.  On a
                // restart, or after a task runtime refused a prior handoff,
                // reacquire the same host lease and make the executor accept
                // the existing operation before returning a running handle.
                let prepared = coordinator
                    .services()
                    .host
                    .reopen(super::subagents::ReopenSubagentRequest {
                        session_id: intent.session_id.clone(),
                        agent_id: intent.agent_id.clone(),
                        workspace_lease_id: existing.spawned.workspace_lease_id.clone(),
                        model: intent.model.clone(),
                        thinking: intent.thinking,
                    })
                    .await
                    .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
                self.validate_prepared_subagent(&intent, &prepared)?;
                self.ensure_subagent_lane_registered(
                    intent.lane_id.clone(),
                    prepared.runtime_services.thinking_level(intent.thinking),
                )?;
                self.start_recovered_subagent_task(coordinator, &existing, prepared.workspace)?;
            }
            // This is the crucial replay path: an already committed graph
            // node is authoritative.  In particular, do not reserve capacity
            // or prepare another workspace for this durable intent.
            return Ok(SpawnedAgentHandle {
                agent_id: existing.spawned.agent_id,
                operation_id,
                task_name: existing.spawned.task_name,
                state: existing.state,
            });
        }

        coordinator.reserve(
            intent.agent_id.clone(),
            &intent.parent_operation_id,
            intent.durable_active,
            intent.durable_total,
        )?;
        let prepared = match coordinator
            .services()
            .host
            .prepare(intent.prepare_request())
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                coordinator.release_reservation(&intent.agent_id, &intent.parent_operation_id);
                return Err(HarnessError::invalid_state(error.to_string()));
            }
        };
        if let Err(error) = self.validate_prepared_subagent(&intent, &prepared) {
            coordinator.release_reservation(&intent.agent_id, &intent.parent_operation_id);
            return Err(error);
        }

        let accepted = match self.commit_subagent_spawn(&intent, &prepared) {
            Ok(accepted) => accepted,
            Err(error) => {
                coordinator.release_reservation(&intent.agent_id, &intent.parent_operation_id);
                return Err(error);
            }
        };
        // The graph now counts the child durably.  Remove the provisional
        // count before task-runtime acceptance so a later spawn cannot see
        // the same child in both ledgers.
        coordinator.release_reservation(&intent.agent_id, &intent.parent_operation_id);

        self.ensure_subagent_lane_registered(
            intent.lane_id.clone(),
            prepared.runtime_services.thinking_level(intent.thinking),
        )?;
        self.publish_event(TeaEvent::Session(SessionEvent::OperationAccepted {
            sequence: accepted.sequence,
            lane_id: intent.lane_id.clone(),
            operation_id: intent.operation_id.clone(),
        }))?;

        self.start_subagent_task(coordinator, &intent, prepared.workspace)?;
        Ok(SpawnedAgentHandle {
            agent_id: intent.agent_id,
            operation_id: intent.operation_id,
            task_name: request.task_name,
            state: AgentState::Running,
        })
    }

    /// Drive a child only after its accepted operation and lane-local service
    /// bundle are durable.  The task runtime owns the future; this method
    /// does not create a detached executor task of its own.
    async fn drive_accepted_subagent(
        &self,
        lane_id: LaneId,
        operation_id: OperationId,
    ) -> Result<DurableOperation, HarnessError> {
        let lane = self.lane(&lane_id)?;
        let _claim = self.claim_lane_operation(Arc::clone(&lane))?;
        self.drive_fresh_epoch(&lane, operation_id).await
    }

    fn ensure_subagent_lane_registered(
        &self,
        lane_id: LaneId,
        services: RuntimeServices,
    ) -> Result<(), HarnessError> {
        match self.lane(&lane_id) {
            Ok(existing) => {
                // A crash may have left a durable child lane/configuration
                // prefix before its AgentSpawned fact. Once replay completes
                // that graph binding, do not trust a service bundle that was
                // registered while the lane was still graph-unclaimed.
                let snapshot = self.snapshot()?;
                let reduction = reduce_lane(snapshot, lane_id)?;
                let configuration = self
                    .configuration_for_reduction_services(&existing.runtime_services, &reduction)?;
                validate_reserved_host_tool_names(&existing.runtime_services, &configuration)?;
                validate_child_subagent_surface(&configuration, &existing.runtime_services)
            }
            Err(HarnessError::InvalidState { .. }) => self.register_lane(lane_id, services),
            Err(error) => Err(error),
        }
    }

    fn start_subagent_task(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
        intent: &SubagentSpawnIntent,
        workspace: super::subagents::WorkspaceLease,
    ) -> Result<(), HarnessError> {
        if coordinator.has_handle(&intent.agent_id) {
            return Ok(());
        }
        let task_supervisor = Arc::downgrade(self);
        let task_coordinator = Arc::downgrade(coordinator);
        let task_agent = intent.agent_id.clone();
        let task_lane = intent.lane_id.clone();
        let task_operation = intent.operation_id.clone();
        let task_workspace = workspace.clone();
        let task_timeout = coordinator.services().policy.timeout;
        let task = Box::pin(async move {
            if let (Some(supervisor), Some(coordinator)) =
                (task_supervisor.upgrade(), task_coordinator.upgrade())
            {
                let _ = supervisor
                    .drive_accepted_subagent_with_timeout(
                        task_lane,
                        task_operation,
                        task_timeout,
                        &coordinator,
                    )
                    .await;
                // A normal task return always reaches the durable completion
                // pipeline. Structured cancellation cannot rely on this
                // future's Drop implementation, so root and explicit
                // interruption also invoke the same idempotent path after
                // joining a cancelled task handle.
                let settled = supervisor
                    .settle_subagent_task(
                        &coordinator,
                        task_agent.clone(),
                        Some(task_workspace),
                        false,
                    )
                    .await;
                if settled.is_ok() {
                    coordinator.task_completed(&task_agent);
                } else {
                    coordinator.task_stopped_before_cleanup(&task_agent);
                }
            }
        });
        let handle = coordinator
            .services()
            .tasks
            .spawn(&format!("tea-subagent-{}", intent.agent_id), task)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        coordinator.install_handle(
            intent.agent_id.clone(),
            &intent.parent_operation_id,
            workspace,
            handle,
        );
        Ok(())
    }

    /// Race one child drive against its immutable policy timeout through the
    /// embedding task runtime.  A timeout is an execution failure, not a
    /// caller interruption: its durable outcome is therefore kept distinct
    /// from `interrupt_agent`'s `Aborted` classification.
    async fn drive_accepted_subagent_with_timeout(
        &self,
        lane_id: LaneId,
        operation_id: OperationId,
        timeout: Duration,
        coordinator: &SubagentCoordinator<S>,
    ) -> Result<DurableOperation, HarnessError> {
        let mut drive =
            Box::pin(self.drive_accepted_subagent(lane_id.clone(), operation_id.clone()));
        let mut deadline = coordinator.services().tasks.sleep(timeout);
        let completed = std::future::poll_fn(|context| {
            // Poll the deadline first so an already-expired deterministic
            // host timer cannot start a provider request after its budget has
            // elapsed.  Otherwise both futures are independently woken by
            // their executor-owned sources.
            if Pin::new(&mut deadline).poll(context).is_ready() {
                return Poll::Ready(None);
            }
            match drive.as_mut().poll(context) {
                Poll::Ready(result) => Poll::Ready(Some(result)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
        match completed {
            Some(result) => result,
            None => {
                self.force_finish_child_lane_operation(
                    &lane_id,
                    &operation_id,
                    OperationOutcome::Failed {
                        code: "subagent_timeout".into(),
                    },
                )?;
                Err(HarnessError::invalid_state(
                    "subagent child execution exceeded its configured timeout",
                ))
            }
        }
    }

    /// Resume an already durable child prefix rather than manufacturing a
    /// fresh epoch. The reduced `RecoveryPlan` decides whether that prefix
    /// still needs its original assignment, an epoch continuation, tool
    /// recovery, or only terminal completion.
    async fn resume_accepted_subagent_with_timeout(
        &self,
        lane_id: LaneId,
        operation_id: OperationId,
        timeout: Duration,
        coordinator: &SubagentCoordinator<S>,
    ) -> Result<DurableOperation, HarnessError> {
        let lane = self.lane(&lane_id)?;
        let mut drive = Box::pin(self.resume_lane_runtime(lane));
        let mut deadline = coordinator.services().tasks.sleep(timeout);
        let completed = std::future::poll_fn(|context| {
            if Pin::new(&mut deadline).poll(context).is_ready() {
                return Poll::Ready(None);
            }
            match drive.as_mut().poll(context) {
                Poll::Ready(result) => Poll::Ready(Some(result)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;
        match completed {
            Some(result) => result,
            None => {
                self.force_finish_child_lane_operation(
                    &lane_id,
                    &operation_id,
                    OperationOutcome::Failed {
                        code: "subagent_timeout".into(),
                    },
                )?;
                Err(HarnessError::invalid_state(
                    "subagent child execution exceeded its configured timeout",
                ))
            }
        }
    }

    /// Reinstall a lost process-local task handle for one durable open child
    /// operation. The graph facts, not volatile coordinator state, select the
    /// lane and operation being resumed.
    fn start_recovered_subagent_task(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
        node: &tea_session::AgentGraphNode,
        workspace: super::subagents::WorkspaceLease,
    ) -> Result<(), HarnessError> {
        let operation_id = node.operation_id.clone().ok_or_else(|| {
            HarnessError::invalid_state("recovered child task has no accepted operation")
        })?;
        if coordinator.has_handle(&node.spawned.agent_id) {
            return Ok(());
        }
        let task_supervisor = Arc::downgrade(self);
        let task_coordinator = Arc::downgrade(coordinator);
        let task_agent = node.spawned.agent_id.clone();
        let task_lane = node.spawned.lane_id.clone();
        let task_workspace = workspace.clone();
        let task_timeout = coordinator.services().policy.timeout;
        let task = Box::pin(async move {
            if let (Some(supervisor), Some(coordinator)) =
                (task_supervisor.upgrade(), task_coordinator.upgrade())
            {
                let _ = supervisor
                    .resume_accepted_subagent_with_timeout(
                        task_lane,
                        operation_id,
                        task_timeout,
                        &coordinator,
                    )
                    .await;
                let settled = supervisor
                    .settle_subagent_task(
                        &coordinator,
                        task_agent.clone(),
                        Some(task_workspace),
                        false,
                    )
                    .await;
                if settled.is_ok() {
                    coordinator.task_completed(&task_agent);
                } else {
                    coordinator.task_stopped_before_cleanup(&task_agent);
                }
            }
        });
        let handle = coordinator
            .services()
            .tasks
            .spawn(&format!("tea-subagent-{}", node.spawned.agent_id), task)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        coordinator.install_handle(
            node.spawned.agent_id.clone(),
            &node.spawned.parent_operation_id,
            workspace,
            handle,
        );
        Ok(())
    }

    /// Wait for selected children owned by the exact root operation carried in
    /// typed tool provenance. The session graph is reread after every
    /// notifier generation; no parent entry is appended by observation.
    pub(crate) async fn wait_subagents(
        &self,
        coordinator: &SubagentCoordinator<S>,
        provenance: &RunProvenance,
        cancellation: CancellationToken,
        request: WaitAgentsRequest,
    ) -> Result<WaitAgentsResult, HarnessError> {
        if request.targets.is_empty() || request.targets.len() > 16 {
            return Err(HarnessError::invalid_state(
                "wait_agent requires between one and sixteen targets",
            ));
        }
        let parent_operation_id = self.root_operation_from_provenance(provenance, "wait_agent")?;
        let mut timeout = coordinator.timeout(request.timeout);
        loop {
            // Read the generation first. A terminal fact committed before the
            // snapshot is visible in that snapshot; one committed after it
            // necessarily advances this generation or wakes this waiter.
            let observed_generation = coordinator.activity_generation();
            let snapshot = self.snapshot()?;
            let nodes = self.resolve_owned_subagent_targets(
                &snapshot,
                &parent_operation_id,
                &request.targets,
                "wait_agent",
            )?;
            let result = self.wait_result(coordinator, &snapshot, &nodes, false)?;
            let complete = match request.return_when {
                WaitReturnWhen::Any => !result.completed.is_empty(),
                WaitReturnWhen::All => result.pending.is_empty(),
            };
            if complete {
                return Ok(result);
            }
            match coordinator
                .wait_for_activity(observed_generation, cancellation.clone(), &mut timeout)
                .await
            {
                ActivityWake::Activity => continue,
                ActivityWake::TimedOut => {
                    let snapshot = self.snapshot()?;
                    let nodes = self.resolve_owned_subagent_targets(
                        &snapshot,
                        &parent_operation_id,
                        &request.targets,
                        "wait_agent",
                    )?;
                    return self.wait_result(coordinator, &snapshot, &nodes, true);
                }
                ActivityWake::Cancelled => {
                    return Err(HarnessError::invalid_state("wait_agent was cancelled"));
                }
            }
        }
    }

    /// List every child owned by the current root operation. Ordering is a
    /// durable presentation contract, independent of spawn or completion
    /// order and intentionally omits report and patch contents.
    pub(crate) fn list_subagents(
        &self,
        provenance: &RunProvenance,
    ) -> Result<Vec<SubagentStatus>, HarnessError> {
        let parent_operation_id = self.root_operation_from_provenance(provenance, "list_agents")?;
        let snapshot = self.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let mut nodes = graph
            .agents
            .values()
            .filter(|node| node.spawned.parent_operation_id == parent_operation_id)
            .cloned()
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| {
            left.spawned
                .task_name
                .cmp(&right.spawned.task_name)
                .then_with(|| left.spawned.agent_id.cmp(&right.spawned.agent_id))
        });
        nodes
            .iter()
            .map(|node| self.subagent_status(&snapshot, node))
            .collect()
    }

    /// Interrupt one owned child, join its host task if present, and run the
    /// same durable completion transaction used after an ordinary child drive.
    /// The terminal graph fact makes a repeated request idempotent.
    pub(crate) async fn interrupt_subagent(
        &self,
        coordinator: &SubagentCoordinator<S>,
        provenance: &RunProvenance,
        target: &str,
    ) -> Result<InterruptAgentResult, HarnessError> {
        let parent_operation_id =
            self.root_operation_from_provenance(provenance, "interrupt_agent")?;
        let snapshot = self.snapshot()?;
        let mut targets = self.resolve_owned_subagent_targets(
            &snapshot,
            &parent_operation_id,
            &[target.to_owned()],
            "interrupt_agent",
        )?;
        let node = targets
            .pop()
            .expect("one requested interrupt target resolves exactly once");
        let previous = node.state.clone();
        if node.terminal.is_none() || !coordinator.is_exposable(&node.spawned.agent_id) {
            self.cancel_join_and_settle_subagent(coordinator, &node)
                .await?;
        }
        let graph = reduce_agent_graph(&self.snapshot()?)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let resulting = graph
            .agents
            .get(&node.spawned.agent_id)
            .ok_or_else(|| HarnessError::invalid_state("interrupted child disappeared from graph"))?
            .state
            .clone();
        Ok(InterruptAgentResult {
            previous,
            resulting,
            agent_id: node.spawned.agent_id,
        })
    }

    /// Apply one terminal child delta through the host's root-workspace port.
    ///
    /// The tool intent and current root operation are both durable authority:
    /// an arbitrary delta ID, a foreign root operation, or a replayed call
    /// whose arguments differ from its recorded intent cannot reach the host.
    /// The host owns preflight and mutation classification; core records only
    /// a proven committed application.
    pub(crate) async fn apply_subagent_changes(
        &self,
        coordinator: &SubagentCoordinator<S>,
        provenance: &RunProvenance,
        call: &ToolCall,
        delta_id: WorkspaceDeltaId,
    ) -> Result<ApplyAgentChangesResult, HarnessError> {
        let parent_operation_id =
            self.root_operation_from_provenance(provenance, "apply_agent_changes")?;
        let parent_epoch_id = required_provenance_id(&provenance.epoch_id, "epoch", EpochId::new)?;
        let snapshot = self.snapshot()?;
        let started = snapshot
            .records()
            .iter()
            .filter_map(|stored| match &stored.record {
                LaneRecord::ToolStarted(record)
                    if record.operation_id == parent_operation_id
                        && record.epoch_id == parent_epoch_id
                        && record.tool_call_id == call.id.to_string()
                        && record.tool_name == "apply_agent_changes" =>
                {
                    Some(record)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [tool_started] = started.as_slice() else {
            return Err(HarnessError::invalid_state(
                "apply_agent_changes must follow exactly one durable root tool-start record",
            ));
        };
        let current_args = JsonValue::parse(call.arguments.as_str()).map_err(|_| {
            HarnessError::invalid_state("apply_agent_changes arguments are not valid durable JSON")
        })?;
        if current_args != tool_started.effective_args {
            return Err(HarnessError::invalid_state(
                "apply_agent_changes replay arguments do not match the durable tool intent",
            ));
        }

        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let node = graph
            .agents
            .values()
            .find(|node| {
                node.workspace_delta
                    .as_ref()
                    .is_some_and(|delta| delta.delta_id == delta_id)
            })
            .ok_or_else(|| HarnessError::invalid_state("apply_agent_changes delta is unknown"))?;
        if node.spawned.parent_lane_id != self.root_lane_id
            || node.spawned.parent_operation_id != parent_operation_id
        {
            return Err(HarnessError::invalid_state(
                "apply_agent_changes delta is not owned by the current root operation",
            ));
        }
        let delta_fact = node.workspace_delta.as_ref().ok_or_else(|| {
            HarnessError::invalid_state("apply_agent_changes delta has no durable workspace fact")
        })?;
        if node
            .terminal
            .as_ref()
            .is_none_or(|terminal| terminal.workspace_delta_id.as_ref() != Some(&delta_id))
        {
            return Err(HarnessError::invalid_state(
                "apply_agent_changes requires a terminal child workspace delta",
            ));
        }
        if let Some(applied) = &node.applied {
            return Ok(ApplyAgentChangesResult::Applied {
                delta_id: applied.delta_id.clone(),
                changed_paths: applied.changed_paths.clone(),
            });
        }
        let patch_artifact = delta_fact.patch.artifact_id().ok_or_else(|| {
            HarnessError::invalid_state(
                "apply_agent_changes delta patch is not an immutable artifact",
            )
        })?;
        self.artifacts.verify_object(patch_artifact)?;
        let delta = WorkspaceDelta {
            id: delta_fact.delta_id.clone(),
            agent_id: delta_fact.agent_id.clone(),
            workspace_lease_id: delta_fact.workspace_lease_id.clone(),
            base_commit: delta_fact.base_commit.clone(),
            result_commit: delta_fact.result_commit.clone(),
            changed_paths: delta_fact.changed_paths.clone(),
            patch_artifact,
        };

        match coordinator
            .services()
            .host
            .apply(ApplyWorkspaceDeltaRequest {
                delta,
                target_lane_id: self.root_lane_id.clone(),
            })
            .await
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?
        {
            super::subagents::WorkspaceApplyOutcome::Applied { changed_paths } => {
                if changed_paths != delta_fact.changed_paths {
                    return Err(HarnessError::invalid_state(
                        "host applied paths do not match the immutable workspace delta",
                    ));
                }
                let mut session = self.session_lock()?;
                let current = session.snapshot()?;
                let graph = reduce_agent_graph(&current)
                    .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
                let node = graph
                    .agents
                    .values()
                    .find(|node| {
                        node.workspace_delta
                            .as_ref()
                            .is_some_and(|delta| delta.delta_id == delta_id)
                    })
                    .ok_or_else(|| {
                        HarnessError::invalid_state(
                            "applied workspace delta disappeared before durable commit",
                        )
                    })?;
                if node.spawned.parent_lane_id != self.root_lane_id
                    || node.spawned.parent_operation_id != parent_operation_id
                {
                    return Err(HarnessError::invalid_state(
                        "applied workspace delta ownership changed before durable commit",
                    ));
                }
                if let Some(applied) = &node.applied {
                    return Ok(ApplyAgentChangesResult::Applied {
                        delta_id: applied.delta_id.clone(),
                        changed_paths: applied.changed_paths.clone(),
                    });
                }
                session.append_fact(SessionFact::WorkspaceDeltaApplied(
                    WorkspaceDeltaAppliedFact {
                        delta_id: delta_id.clone(),
                        target_lane_id: self.root_lane_id.clone(),
                        tool_call_id: call.id.to_string(),
                        changed_paths: changed_paths.clone(),
                    },
                ))?;
                Ok(ApplyAgentChangesResult::Applied {
                    delta_id,
                    changed_paths,
                })
            }
            super::subagents::WorkspaceApplyOutcome::Conflict { conflicting_paths } => {
                validate_host_conflicting_paths(&conflicting_paths, &delta_fact.changed_paths)?;
                Ok(ApplyAgentChangesResult::Conflict {
                    delta_id,
                    conflicting_paths,
                    patch_artifact,
                })
            }
            super::subagents::WorkspaceApplyOutcome::RolledBack { diagnostic } => {
                validate_host_apply_diagnostic(&diagnostic)?;
                Ok(ApplyAgentChangesResult::RolledBack {
                    delta_id,
                    diagnostic,
                })
            }
            super::subagents::WorkspaceApplyOutcome::Indeterminate { diagnostic } => {
                validate_host_apply_diagnostic(&diagnostic)?;
                Ok(ApplyAgentChangesResult::Indeterminate {
                    delta_id,
                    diagnostic,
                })
            }
        }
    }

    /// Settle all still-open children before the owning root operation can
    /// append its terminal WAL record. A dropped executor future is not a
    /// settlement mechanism: after every task join this explicitly closes a
    /// remaining child operation, finalizes its workspace, commits its report,
    /// and removes process-local task ownership.
    async fn settle_root_children_before_finish(
        &self,
        root_operation_id: &OperationId,
    ) -> Result<(), HarnessError> {
        let Some(coordinator) = self.subagent_coordinator()? else {
            return Ok(());
        };
        let snapshot = self.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let nodes = graph
            .agents
            .values()
            .filter(|node| {
                node.spawned.parent_lane_id == self.root_lane_id
                    && node.spawned.parent_operation_id == *root_operation_id
                    && !coordinator.is_exposable(&node.spawned.agent_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        for node in &nodes {
            if let Some(handle) = coordinator.handle(&node.spawned.agent_id) {
                handle.cancel();
            }
        }
        for node in &nodes {
            if let Ok(lane) = self.lane(&node.spawned.lane_id) {
                let _ = self.abort_lane_runtime(&lane)?;
            }
        }
        for node in &nodes {
            if let Some(handle) = coordinator.handle(&node.spawned.agent_id) {
                handle.join().await;
            }
        }
        for node in &nodes {
            self.settle_subagent_task(
                &coordinator,
                node.spawned.agent_id.clone(),
                coordinator.workspace(&node.spawned.agent_id),
                false,
            )
            .await?;
        }
        // Every handle is reaped from this root-owned external join boundary,
        // including a normally completed child that was already exposable.
        let all_owned = reduce_agent_graph(&self.snapshot()?)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?
            .agents
            .values()
            .filter(|node| {
                node.spawned.parent_lane_id == self.root_lane_id
                    && node.spawned.parent_operation_id == *root_operation_id
            })
            .map(|node| node.spawned.agent_id.clone())
            .collect::<Vec<_>>();
        for agent_id in all_owned {
            if let Some(handle) = coordinator.handle(&agent_id) {
                handle.join().await;
                coordinator.reap_task(&agent_id);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn settle_root_children_for_test(
        &self,
        root_operation_id: &OperationId,
    ) -> Result<(), HarnessError> {
        self.settle_root_children_before_finish(root_operation_id)
            .await
    }

    async fn cancel_join_and_settle_subagent(
        &self,
        coordinator: &SubagentCoordinator<S>,
        node: &tea_session::AgentGraphNode,
    ) -> Result<(), HarnessError> {
        if let Some(handle) = coordinator.handle(&node.spawned.agent_id) {
            handle.cancel();
        }
        if let Ok(lane) = self.lane(&node.spawned.lane_id) {
            let _ = self.abort_lane_runtime(&lane)?;
        }
        if let Some(handle) = coordinator.handle(&node.spawned.agent_id) {
            handle.join().await;
        }
        self.settle_subagent_task(
            coordinator,
            node.spawned.agent_id.clone(),
            coordinator.workspace(&node.spawned.agent_id),
            false,
        )
        .await?;
        coordinator.reap_task(&node.spawned.agent_id);
        Ok(())
    }

    /// Complete the child terminal transaction after either normal task return
    /// or structured cancellation. This method is deliberately idempotent so
    /// a joined cancellation can recover a task future that was dropped before
    /// its own cleanup branch was ever polled.
    async fn settle_subagent_task(
        &self,
        coordinator: &SubagentCoordinator<S>,
        agent_id: AgentId,
        workspace: Option<super::subagents::WorkspaceLease>,
        recovering: bool,
    ) -> Result<(), HarnessError> {
        let snapshot = self.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let initial = graph.agents.get(&agent_id).cloned().ok_or_else(|| {
            HarnessError::invalid_state(format!("unknown durable child {agent_id}"))
        })?;
        // A root-cleanup snapshot can race a child task that has already
        // committed its terminal fact, completed idempotent cleanup, and
        // removed its volatile workspace handle. Terminality is durable; do
        // not reopen a successfully cleaned worktree merely to satisfy a
        // stale parent snapshot. When this process still has the lease,
        // repeat host cleanup idempotently before returning.
        if initial.terminal.is_some() {
            if let Some(workspace) = workspace.or_else(|| coordinator.workspace(&agent_id)) {
                coordinator
                    .services()
                    .host
                    .cleanup(workspace)
                    .await
                    .map_err(|error| {
                        subagent_host_stage_error(
                            recovering,
                            &agent_id,
                            SubagentRecoveryStage::CleanupWorkspace,
                            error,
                        )
                    })?;
                coordinator.mark_exposable_and_notify(agent_id);
            } else if !coordinator.is_exposable(&agent_id) {
                return Err(HarnessError::invalid_state(
                    "terminal subagent still requires host workspace cleanup authority",
                ));
            }
            return Ok(());
        }
        let workspace = match workspace.or_else(|| coordinator.workspace(&agent_id)) {
            Some(workspace) => workspace,
            None => {
                self.reopen_subagent_workspace(coordinator, &initial)
                    .await?
            }
        };
        if workspace.id != initial.spawned.workspace_lease_id {
            return Err(HarnessError::invalid_state(
                "subagent completion workspace lease does not match durable child identity",
            ));
        }

        if initial.terminal.is_none() {
            self.force_abort_open_subagent_operation(&initial)?;
        }

        let snapshot = self.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let node = graph.agents.get(&agent_id).cloned().ok_or_else(|| {
            HarnessError::invalid_state(format!("unknown durable child {agent_id}"))
        })?;
        let operation_id = node.operation_id.clone().ok_or_else(|| {
            HarnessError::invalid_state("subagent completion has no accepted child operation")
        })?;
        let outcome = child_operation_outcome(&snapshot, &operation_id).ok_or_else(|| {
            HarnessError::invalid_state("subagent completion has no terminal operation outcome")
        })?;

        let delta = if node.workspace_delta.is_none() {
            match coordinator
                .services()
                .host
                .finalize(FinalizeSubagentRequest {
                    agent_id: agent_id.clone(),
                    workspace: workspace.clone(),
                })
                .await
                .map_err(|error| {
                    subagent_host_stage_error(
                        recovering,
                        &agent_id,
                        SubagentRecoveryStage::FinalizeWorkspace,
                        error,
                    )
                })? {
                WorkspaceFinalization::NoChanges => None,
                WorkspaceFinalization::Delta(delta) => {
                    Some(self.workspace_delta_fact(&node, delta)?)
                }
            }
        } else {
            None
        };

        let (final_entry_id, report) = self.subagent_report_payload(&snapshot, &node)?;
        self.append_subagent_terminal(&node, operation_id, outcome, final_entry_id, report, delta)?;
        // The report and optional patch are now durable. Only after that may
        // the host remove operational worktree state or a waiter observe the
        // completion generation.
        coordinator
            .services()
            .host
            .cleanup(workspace)
            .await
            .map_err(|error| {
                subagent_host_stage_error(
                    recovering,
                    &agent_id,
                    SubagentRecoveryStage::CleanupWorkspace,
                    error,
                )
            })?;
        // A concurrent idempotent settler may have committed the terminal
        // fact while this caller performed the cleanup that actually
        // succeeded. Visibility is therefore tied to successful cleanup, not
        // to which contender appended the already-durable terminal record.
        coordinator.mark_exposable_and_notify(agent_id);
        Ok(())
    }

    fn force_abort_open_subagent_operation(
        &self,
        node: &tea_session::AgentGraphNode,
    ) -> Result<(), HarnessError> {
        let operation_id = node.operation_id.as_ref().ok_or_else(|| {
            HarnessError::invalid_state("subagent cancellation has no accepted operation")
        })?;
        self.force_finish_child_lane_operation(
            &node.spawned.lane_id,
            operation_id,
            OperationOutcome::Aborted,
        )
    }

    /// Stop a durable child operation outside its owned task future.  This is
    /// used by structured cancellation and policy timeout after their drive
    /// future may have been dropped, so it must also clear the lane's volatile
    /// active-agent slot rather than relying on `drive_epoch`'s normal tail.
    fn force_finish_child_lane_operation(
        &self,
        lane_id: &LaneId,
        operation_id: &OperationId,
        outcome: OperationOutcome,
    ) -> Result<(), HarnessError> {
        let lane = self.lane(lane_id)?;
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
        match reduction.lane_state.active_operation {
            None => return Ok(()),
            Some(active) if active != *operation_id => {
                return Err(HarnessError::invalid_state(
                    "subagent lane has a different active operation during cancellation",
                ));
            }
            Some(_) => {}
        }
        let _ = self.abort_lane_runtime(&lane)?;
        // A cancelled executor drops the child drive before its usual
        // `clear_lane_active_agent` tail. This explicit forced path owns that
        // cleanup, preventing a terminal lane from retaining a stale agent.
        self.clear_lane_active_agent(&lane);
        if let Some(epoch_id) = open_epoch(&snapshot, operation_id) {
            self.finish_operation(&lane, operation_id, &epoch_id, outcome)?;
        } else {
            let sequence = self
                .session_lock()?
                .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                    operation_id: operation_id.clone(),
                    outcome: outcome.clone(),
                }))?
                .seq;
            self.publish_event(TeaEvent::Session(SessionEvent::OperationFinished {
                sequence,
                lane_id: lane_id.clone(),
                operation_id: operation_id.clone(),
                outcome: operation_outcome_name(&outcome).into(),
            }))?;
        }
        Ok(())
    }

    async fn reopen_subagent_workspace(
        &self,
        coordinator: &SubagentCoordinator<S>,
        node: &tea_session::AgentGraphNode,
    ) -> Result<super::subagents::WorkspaceLease, HarnessError> {
        Ok(self
            .reopen_subagent_prepared(coordinator, node)
            .await?
            .workspace)
    }

    /// Reacquire a child workspace through the host's typed recovery port.
    /// The durable graph supplies every identity; no logical path or model is
    /// reconstructed from process-local state.
    async fn reopen_subagent_prepared(
        &self,
        coordinator: &SubagentCoordinator<S>,
        node: &tea_session::AgentGraphNode,
    ) -> Result<PreparedSubagent, HarnessError> {
        let model = coordinator
            .services()
            .policy
            .models
            .iter()
            .find(|candidate| durable_subagent_model(candidate) == node.spawned.model)
            .cloned()
            .ok_or_else(|| HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            })?;
        let thinking = thinking_level_from_name(&node.spawned.thinking).map_err(|_| {
            HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            }
        })?;
        let session_id = self.snapshot()?.header().session_id.clone();
        let prepared = coordinator
            .services()
            .host
            .reopen(ReopenSubagentRequest {
                session_id,
                agent_id: node.spawned.agent_id.clone(),
                workspace_lease_id: node.spawned.workspace_lease_id.clone(),
                model,
                thinking,
            })
            .await
            .map_err(|_| HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            })?;
        if prepared.workspace.id != node.spawned.workspace_lease_id {
            return Err(HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            });
        }
        Ok(prepared)
    }

    /// Verify that a recovered host service bundle is exactly the one bound by
    /// the child graph fact before the lane may be registered or driven.
    fn validate_reopened_subagent(
        &self,
        node: &tea_session::AgentGraphNode,
        prepared: &PreparedSubagent,
    ) -> Result<(), HarnessError> {
        let expected_thinking = thinking_level_from_name(&node.spawned.thinking).map_err(|_| {
            HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            }
        })?;
        let expected_model = ModelDescriptor {
            provider: node.spawned.model.provider.clone(),
            model: node.spawned.model.model.clone(),
            revision: node.spawned.model.revision.clone(),
        };
        if prepared.workspace.id != node.spawned.workspace_lease_id
            || prepared.runtime_services.model_descriptor() != Some(&expected_model)
            || prepared.runtime_services.thinking_level_value() != expected_thinking
            || prepared.harness_identity.revision_id() != &node.spawned.harness_revision_id
            || prepared.harness_identity.snapshot_id() != &node.spawned.harness_snapshot_id
            || prepared.harness_identity.profile_id() != &node.spawned.model_harness_profile_id
        {
            return Err(HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            });
        }
        let resolved = self
            .manager
            .resolve_revision(
                prepared.harness_identity.revision_id(),
                &prepared.runtime_services,
            )
            .map_err(|_| HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            })?;
        if resolved.identity != prepared.harness_identity
            || validate_reserved_host_tool_names(&prepared.runtime_services, &resolved).is_err()
            || validate_child_subagent_surface(&resolved, &prepared.runtime_services).is_err()
        {
            return Err(HarnessError::SubagentRecovery {
                agent_id: node.spawned.agent_id.clone(),
                stage: SubagentRecoveryStage::ReopenWorkspace,
            });
        }
        Ok(())
    }

    fn workspace_delta_fact(
        &self,
        node: &tea_session::AgentGraphNode,
        delta: WorkspaceDelta,
    ) -> Result<WorkspaceDeltaFact, HarnessError> {
        if delta.agent_id != node.spawned.agent_id
            || delta.workspace_lease_id != node.spawned.workspace_lease_id
        {
            return Err(HarnessError::invalid_state(
                "host workspace delta does not belong to the completing child lease",
            ));
        }
        let byte_len = self.artifacts.verify_object(delta.patch_artifact)?;
        Ok(WorkspaceDeltaFact {
            delta_id: delta.id,
            agent_id: delta.agent_id,
            workspace_lease_id: delta.workspace_lease_id,
            base_commit: delta.base_commit,
            result_commit: delta.result_commit,
            changed_paths: delta.changed_paths,
            patch: PayloadRef::Artifact {
                artifact_id: delta.patch_artifact,
                byte_len,
                media_type: "application/x-git-diff".into(),
            },
        })
    }

    fn subagent_report_payload(
        &self,
        snapshot: &SessionSnapshot,
        node: &tea_session::AgentGraphNode,
    ) -> Result<(Option<EntryId>, PayloadRef), HarnessError> {
        let assistant = snapshot.entries().iter().rev().find_map(|entry| {
            (entry.lane_id == node.spawned.lane_id)
                .then_some(&entry.body)
                .and_then(|body| {
                    let SessionEntry::AssistantMessage(assistant) = body else {
                        return None;
                    };
                    Some((entry.header.id.clone(), assistant.content.clone()))
                })
        });
        let Some((entry_id, content)) = assistant else {
            return Ok((None, PayloadRef::Inline(JsonValue::String(String::new()))));
        };
        if content.len() <= 32 * 1024 {
            return Ok((
                Some(entry_id),
                PayloadRef::Inline(JsonValue::String(content)),
            ));
        }
        let artifact = self
            .artifacts
            .put(content.as_bytes(), "text/plain; charset=utf-8")?;
        Ok((
            Some(entry_id),
            PayloadRef::Artifact {
                artifact_id: artifact.artifact_id,
                byte_len: artifact.byte_len,
                media_type: artifact.media_type,
            },
        ))
    }

    fn append_subagent_terminal(
        &self,
        node: &tea_session::AgentGraphNode,
        operation_id: OperationId,
        outcome: OperationOutcome,
        final_entry_id: Option<EntryId>,
        report: PayloadRef,
        delta: Option<WorkspaceDeltaFact>,
    ) -> Result<bool, HarnessError> {
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let current = graph.agents.get(&node.spawned.agent_id).ok_or_else(|| {
            HarnessError::invalid_state("subagent terminal append lost its durable graph node")
        })?;
        if current.terminal.is_some() {
            return Ok(false);
        }
        let workspace_delta_id = match (current.workspace_delta.as_ref(), delta) {
            (Some(existing), Some(candidate)) if *existing != candidate => {
                return Err(HarnessError::invalid_state(
                    "subagent workspace finalization conflicts with its durable delta",
                ));
            }
            (Some(existing), _) => Some(existing.delta_id.clone()),
            (None, Some(candidate)) => {
                let delta_id = candidate.delta_id.clone();
                session.append_fact(SessionFact::WorkspaceDelta(candidate))?;
                Some(delta_id)
            }
            (None, None) => None,
        };
        session.append_fact(SessionFact::AgentTaskFinished(AgentTaskFinishedFact {
            agent_id: node.spawned.agent_id.clone(),
            operation_id,
            outcome,
            final_entry_id,
            report,
            workspace_delta_id,
        }))?;
        Ok(true)
    }

    fn root_operation_from_provenance(
        &self,
        provenance: &RunProvenance,
        tool_name: &str,
    ) -> Result<OperationId, HarnessError> {
        let snapshot = self.snapshot()?;
        let session_id = provenance.session_id.as_deref().ok_or_else(|| {
            HarnessError::invalid_state(format!("{tool_name} is missing session provenance"))
        })?;
        if session_id != snapshot.header().session_id.as_str()
            || provenance.lane_id.as_deref() != Some(self.root_lane_id.as_str())
        {
            return Err(HarnessError::invalid_state(format!(
                "{tool_name} is not attributed to this root lane",
            )));
        }
        let operation = provenance.operation_id.as_ref().ok_or_else(|| {
            HarnessError::invalid_state(format!("{tool_name} is missing operation provenance"))
        })?;
        let operation = OperationId::new(operation.clone())
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let reduction = reduce_lane(snapshot, self.root_lane_id.clone())?;
        if reduction.lane_state.active_operation.as_ref() != Some(&operation) {
            return Err(HarnessError::invalid_state(format!(
                "{tool_name} may only observe children owned by its active root operation",
            )));
        }
        Ok(operation)
    }

    fn resolve_owned_subagent_targets(
        &self,
        snapshot: &SessionSnapshot,
        parent_operation_id: &OperationId,
        targets: &[String],
        tool_name: &str,
    ) -> Result<Vec<tea_session::AgentGraphNode>, HarnessError> {
        let graph = reduce_agent_graph(snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let mut seen = BTreeSet::new();
        targets
            .iter()
            .map(|target| {
                let node = graph
                    .agents
                    .values()
                    .filter(|node| {
                        node.spawned.parent_lane_id == self.root_lane_id
                            && node.spawned.parent_operation_id == *parent_operation_id
                    })
                    .find(|node| {
                        node.spawned.agent_id.as_str() == target
                            || node.spawned.task_name == *target
                    });
                let node = match node {
                    Some(node) => node,
                    None if graph.agents.values().any(|node| {
                        node.spawned.agent_id.as_str() == target
                            || node.spawned.task_name == *target
                    }) =>
                    {
                        return Err(HarnessError::invalid_state(format!(
                            "{tool_name} target is not owned by the current root operation",
                        )));
                    }
                    None => {
                        return Err(HarnessError::invalid_state(format!(
                            "{tool_name} target is unknown",
                        )));
                    }
                };
                if !seen.insert(node.spawned.agent_id.clone()) {
                    return Err(HarnessError::invalid_state(format!(
                        "{tool_name} targets contain a duplicate child",
                    )));
                }
                Ok(node.clone())
            })
            .collect()
    }

    fn wait_result(
        &self,
        coordinator: &SubagentCoordinator<S>,
        snapshot: &SessionSnapshot,
        nodes: &[tea_session::AgentGraphNode],
        timed_out: bool,
    ) -> Result<WaitAgentsResult, HarnessError> {
        let mut completed = Vec::new();
        let mut pending = Vec::new();
        for node in nodes {
            let status = self.subagent_status(snapshot, node)?;
            if node.terminal.is_some() && coordinator.is_exposable(&node.spawned.agent_id) {
                completed.push(WaitedSubagent {
                    report: self.subagent_report(snapshot, node)?,
                    status,
                });
            } else {
                pending.push(status);
            }
        }
        Ok(WaitAgentsResult {
            completed,
            pending,
            timed_out,
        })
    }

    fn subagent_status(
        &self,
        snapshot: &SessionSnapshot,
        node: &tea_session::AgentGraphNode,
    ) -> Result<SubagentStatus, HarnessError> {
        let operation_id = node.operation_id.clone().ok_or_else(|| {
            HarnessError::invalid_state("subagent observation has no accepted child operation")
        })?;
        let workspace_change = node
            .terminal
            .as_ref()
            .and(node.workspace_delta.as_ref())
            .map(|delta| {
                let patch_artifact = delta.patch.artifact_id().ok_or_else(|| {
                    HarnessError::invalid_state(
                        "workspace delta patch is not an immutable artifact",
                    )
                })?;
                Ok::<SubagentWorkspaceChange, HarnessError>(SubagentWorkspaceChange {
                    delta_id: delta.delta_id.clone(),
                    changed_paths: delta.changed_paths.clone(),
                    patch_artifact,
                })
            })
            .transpose()?;
        Ok(SubagentStatus {
            agent_id: node.spawned.agent_id.clone(),
            operation_id: operation_id.clone(),
            task_name: node.spawned.task_name.clone(),
            model: crate::state::ModelDescriptor {
                provider: node.spawned.model.provider.clone(),
                model: node.spawned.model.model.clone(),
                revision: node.spawned.model.revision.clone(),
            },
            thinking: node.spawned.thinking.clone(),
            state: node.state.clone(),
            context_mode: node.spawned.context_mode,
            usage: operation_usage(snapshot, &operation_id),
            workspace_change,
        })
    }

    fn subagent_report(
        &self,
        _snapshot: &SessionSnapshot,
        node: &tea_session::AgentGraphNode,
    ) -> Result<SubagentReport, HarnessError> {
        let terminal = node
            .terminal
            .as_ref()
            .ok_or_else(|| HarnessError::invalid_state("pending child has no final report"))?;
        match &terminal.report {
            PayloadRef::Inline(JsonValue::String(report)) => Ok(SubagentReport {
                preview: truncate_middle(report, 16 * 1024),
                artifact_id: None,
            }),
            PayloadRef::Inline(_) => Err(HarnessError::invalid_state(
                "subagent terminal report must be a string payload",
            )),
            PayloadRef::Artifact { artifact_id, .. } => {
                let bytes = self.artifacts.get(*artifact_id)?;
                let report = String::from_utf8(bytes).map_err(|_| {
                    HarnessError::invalid_state("subagent report artifact is not UTF-8 text")
                })?;
                Ok(SubagentReport {
                    preview: truncate_middle(&report, 16 * 1024),
                    artifact_id: Some(*artifact_id),
                })
            }
        }
    }

    fn validate_replayed_subagent(
        &self,
        intent: &SubagentSpawnIntent,
        existing: &tea_session::AgentGraphNode,
    ) -> Result<(), HarnessError> {
        let expected_model = durable_subagent_model(&intent.model);
        if existing.spawned.agent_id != intent.agent_id
            || existing.spawned.parent_lane_id != intent.parent_lane_id
            || existing.spawned.parent_operation_id != intent.parent_operation_id
            || existing.spawned.lane_id != intent.lane_id
            || existing.spawned.workspace_lease_id != WorkspaceLeaseId::derive(&intent.agent_id)
            || existing.spawned.task_name != intent.task_name
            || existing.spawned.model != expected_model
            || existing.spawned.thinking != thinking_level_name(intent.thinking)
            || existing.spawned.context_mode != intent.context_mode
            || existing.spawned.base_leaf_id != intent.parent_source_leaf_id
            || existing.spawned.spawn_tool_call_id != intent.spawn_tool_call_id
            || existing
                .operation_id
                .as_ref()
                .is_some_and(|id| id != &intent.operation_id)
        {
            return Err(HarnessError::invalid_state(
                "spawn_agent replay does not match its existing durable child intent",
            ));
        }
        Ok(())
    }

    fn subagent_spawn_intent(
        &self,
        call: &crate::tool::ToolCall,
        provenance: &RunProvenance,
        request: &SpawnAgentRequest,
    ) -> Result<SubagentSpawnIntent, HarnessError> {
        validate_subagent_spawn_request(request)?;
        let session_id = required_provenance_id(
            &provenance.session_id,
            "session",
            tea_session::SessionId::new,
        )?;
        let parent_lane_id = required_provenance_id(&provenance.lane_id, "lane", LaneId::new)?;
        let parent_operation_id =
            required_provenance_id(&provenance.operation_id, "operation", OperationId::new)?;
        let parent_epoch_id = required_provenance_id(&provenance.epoch_id, "epoch", EpochId::new)?;
        if parent_lane_id != self.root_lane_id {
            return Err(HarnessError::invalid_state(
                "spawn_agent is available only to the root lane in subagent V1",
            ));
        }
        let parent_lane = self.lane(&parent_lane_id)?;
        let inherited_thinking = *parent_lane
            .thinking_level
            .lock()
            .map_err(|_| HarnessError::invalid_state("thinking level mutex is poisoned"))?;
        let thinking = request.thinking.unwrap_or(inherited_thinking);

        let snapshot = self.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let policy = graph.policy.as_ref().ok_or_else(|| {
            HarnessError::invalid_state("spawn_agent requires a durable subagent policy")
        })?;
        let services = self.subagents.as_ref().ok_or_else(|| {
            HarnessError::invalid_state("spawn_agent requires explicit subagent services")
        })?;
        verify_subagent_policy_fact(policy, &services.policy)?;
        let model = services
            .policy
            .models
            .iter()
            .find(|model| model.descriptor.model == request.model)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state("spawn_agent selected a disallowed model")
            })?;
        let parent_reduction = reduce_lane(snapshot.clone(), parent_lane_id.clone())?;
        if parent_reduction.lane_state.active_operation.as_ref() != Some(&parent_operation_id) {
            return Err(HarnessError::invalid_state(
                "spawn_agent provenance does not name the active root operation",
            ));
        }
        let started = snapshot
            .records()
            .iter()
            .filter_map(|stored| match &stored.record {
                LaneRecord::ToolStarted(record)
                    if record.operation_id == parent_operation_id
                        && record.epoch_id == parent_epoch_id
                        && record.tool_call_id == call.id.to_string()
                        && record.tool_name == "spawn_agent" =>
                {
                    Some(record)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [tool_started] = started.as_slice() else {
            return Err(HarnessError::invalid_state(
                "spawn_agent must follow exactly one durable root tool-start record",
            ));
        };
        let current_args = JsonValue::parse(call.arguments.as_str()).map_err(|_| {
            HarnessError::invalid_state("spawn_agent arguments are not valid durable JSON")
        })?;
        if current_args != tool_started.effective_args {
            return Err(HarnessError::invalid_state(
                "spawn_agent replay arguments do not match the durable tool intent",
            ));
        }
        let epoch_source_leaf = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(record)
                    if record.id == parent_epoch_id
                        && record.operation_id == parent_operation_id =>
                {
                    Some(record.source_leaf_id.clone())
                }
                _ => None,
            })
            .ok_or_else(|| HarnessError::invalid_state("spawn_agent provenance names no epoch"))?;
        let provenance_source = provenance
            .source_leaf_id
            .as_ref()
            .map(|value| EntryId::new(value.clone()))
            .transpose()
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        if provenance_source != epoch_source_leaf {
            return Err(HarnessError::invalid_state(
                "spawn_agent source provenance does not match the durable parent epoch leaf",
            ));
        }
        let agent_id = AgentId::derive(
            &session_id,
            &parent_lane_id,
            &parent_operation_id,
            &tool_started.idempotency_key,
        );
        let lane_id = agent_id.lane_id();
        let operation_id = derive_subagent_operation_id(&agent_id, &request.task);
        let existing = graph.agents.get(&agent_id).cloned();
        if existing.is_none()
            && graph.agents.values().any(|node| {
                node.spawned.parent_operation_id == parent_operation_id
                    && node.spawned.task_name == request.task_name
            })
        {
            return Err(HarnessError::invalid_state(
                "spawn_agent task_name is already owned by this root operation",
            ));
        }
        let durable_active = graph
            .agents
            .values()
            .filter(|node| {
                matches!(
                    node.state,
                    AgentState::Running | AgentState::Finalizing { .. }
                )
            })
            .count() as u32;
        let durable_total = graph
            .agents
            .values()
            .filter(|node| node.spawned.parent_operation_id == parent_operation_id)
            .count() as u32;
        Ok(SubagentSpawnIntent {
            session_id,
            parent_lane_id,
            parent_operation_id,
            agent_id,
            lane_id,
            operation_id,
            task_name: request.task_name.clone(),
            task: request.task.clone(),
            model,
            thinking,
            context_mode: request.context_mode,
            parent_source_leaf_id: match request.context_mode {
                AgentContextMode::Task => None,
                AgentContextMode::Parent => epoch_source_leaf,
            },
            workspace_source_leaf_id: parent_reduction.lane_state.leaf_id,
            spawn_tool_call_id: call.id.to_string(),
            spawn_idempotency_key: tool_started.idempotency_key.clone(),
            durable_active,
            durable_total,
            existing,
        })
    }

    fn validate_prepared_subagent(
        &self,
        intent: &SubagentSpawnIntent,
        prepared: &PreparedSubagent,
    ) -> Result<(), HarnessError> {
        if prepared.workspace.id != WorkspaceLeaseId::derive(&intent.agent_id) {
            return Err(HarnessError::invalid_state(
                "subagent host returned a workspace lease for a different child",
            ));
        }
        if prepared.runtime_services.model_descriptor() != Some(&intent.model.descriptor) {
            return Err(HarnessError::invalid_state(
                "subagent host runtime services do not bind the selected model descriptor",
            ));
        }
        if prepared.runtime_services.thinking_level_value() != intent.thinking {
            return Err(HarnessError::invalid_state(
                "subagent host runtime services do not bind the selected thinking level",
            ));
        }
        let resolved = self.manager.resolve_revision(
            prepared.harness_identity.revision_id(),
            &prepared.runtime_services,
        )?;
        if resolved.identity != prepared.harness_identity {
            return Err(HarnessError::invalid_state(
                "subagent host harness identity does not match its immutable revision",
            ));
        }
        validate_reserved_host_tool_names(&prepared.runtime_services, &resolved)?;
        validate_child_subagent_surface(&resolved, &prepared.runtime_services)
    }

    fn commit_subagent_spawn(
        &self,
        intent: &SubagentSpawnIntent,
        prepared: &PreparedSubagent,
    ) -> Result<AcceptedSubagentOperation, HarnessError> {
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        if graph.agents.contains_key(&intent.agent_id) {
            return Err(HarnessError::invalid_state(
                "subagent spawn became durable while its workspace was preparing",
            ));
        }
        if graph.agents.values().any(|node| {
            node.spawned.parent_operation_id == intent.parent_operation_id
                && node.spawned.task_name == intent.task_name
        }) {
            return Err(HarnessError::invalid_state(
                "spawn_agent task_name is already owned by this root operation",
            ));
        }
        self.complete_subagent_lane_binding(&mut session, intent, prepared)?;
        self.append_subagent_spawn_fact(&mut session, intent, prepared)?;
        self.append_subagent_operation(&mut session, intent, prepared)
    }

    fn append_subagent_spawn_fact(
        &self,
        session: &mut S,
        intent: &SubagentSpawnIntent,
        prepared: &PreparedSubagent,
    ) -> Result<(), HarnessError> {
        let model_record = durable_subagent_model(&intent.model);
        let thinking = thinking_level_name(intent.thinking).to_owned();
        session.append_fact(SessionFact::AgentSpawned(AgentSpawnedFact {
            agent_id: intent.agent_id.clone(),
            parent_lane_id: intent.parent_lane_id.clone(),
            parent_operation_id: intent.parent_operation_id.clone(),
            lane_id: intent.lane_id.clone(),
            task_name: intent.task_name.clone(),
            model: model_record,
            thinking,
            context_mode: intent.context_mode,
            base_leaf_id: intent.parent_source_leaf_id.clone(),
            workspace_lease_id: prepared.workspace.id.clone(),
            harness_revision_id: prepared.harness_identity.revision_id().clone(),
            harness_snapshot_id: prepared.harness_identity.snapshot_id().clone(),
            model_harness_profile_id: prepared.harness_identity.profile_id().clone(),
            spawn_tool_call_id: intent.spawn_tool_call_id.clone(),
        }))?;
        Ok(())
    }

    /// Complete the deterministic child topology/configuration prefix before
    /// the graph fact is appended. Every element has a stable ID, so replay of
    /// a crash between lane creation and `AgentSpawned` can append only the
    /// missing suffix and rejects conflicting durable bytes.
    fn complete_subagent_lane_binding(
        &self,
        session: &mut S,
        intent: &SubagentSpawnIntent,
        prepared: &PreparedSubagent,
    ) -> Result<(), HarnessError> {
        let snapshot = session.snapshot()?;
        match snapshot
            .lane_mutations()
            .iter()
            .find(|stored| matches!(&stored.mutation, LaneMutation::Created { lane_id, .. } if lane_id == &intent.lane_id))
            .map(|stored| &stored.mutation)
        {
            None => {
                session.append_lane_mutation(LaneMutation::Created {
                    lane_id: intent.lane_id.clone(),
                    base_leaf_id: intent.parent_source_leaf_id.clone(),
                })?;
            }
            Some(LaneMutation::Created { base_leaf_id, .. })
                if base_leaf_id == &intent.parent_source_leaf_id => {}
            Some(_) => {
                return Err(HarnessError::invalid_state(
                    "subagent durable lane binding disagrees with its parent context",
                ));
            }
        }
        for entry in self.subagent_lane_binding_entries(intent, prepared)? {
            let current = session
                .snapshot()?
                .entries()
                .iter()
                .find(|current| current.header.id == entry.id)
                .cloned();
            match current {
                None => {
                    session.append_entry(&intent.lane_id, entry)?;
                }
                Some(current)
                    if current.lane_id == intent.lane_id && current.body == entry.body => {}
                Some(_) => {
                    return Err(HarnessError::invalid_state(
                        "subagent durable lane configuration conflicts with its original intent",
                    ));
                }
            }
        }
        let reduction = reduce_lane(session.snapshot()?, intent.lane_id.clone())?;
        let expected_model = ModelChangedEntry {
            provider: intent.model.descriptor.provider.clone(),
            model: intent.model.descriptor.model.clone(),
            revision: intent.model.descriptor.revision.clone(),
        };
        if reduction.effective_configuration.model.as_ref() != Some(&expected_model)
            || reduction.effective_configuration.thinking_level.as_deref()
                != Some(thinking_level_name(intent.thinking))
            || reduction.effective_configuration.harness_revision.as_ref()
                != Some(prepared.harness_identity.revision_id())
        {
            return Err(HarnessError::invalid_state(
                "subagent lane configuration does not resolve to its immutable child intent",
            ));
        }
        Ok(())
    }

    fn subagent_lane_binding_entries(
        &self,
        intent: &SubagentSpawnIntent,
        prepared: &PreparedSubagent,
    ) -> Result<Vec<ProvisionedEntry>, HarnessError> {
        let model = durable_subagent_model(&intent.model);
        Ok(vec![
            ProvisionedEntry {
                id: subagent_entry_id(&intent.agent_id, "model")?,
                body: SessionEntry::ModelChanged(ModelChangedEntry {
                    provider: model.provider,
                    model: model.model,
                    revision: model.revision,
                }),
            },
            ProvisionedEntry {
                id: subagent_entry_id(&intent.agent_id, "thinking")?,
                body: SessionEntry::ThinkingChanged(ThinkingChangedEntry {
                    level: thinking_level_name(intent.thinking).to_owned(),
                }),
            },
            ProvisionedEntry {
                id: subagent_entry_id(&intent.agent_id, "harness")?,
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: prepared.harness_identity.revision_id().clone(),
                    snapshot_id: prepared.harness_identity.snapshot_id().clone(),
                    rollback_from: None,
                }),
            },
        ])
    }

    /// Append the accepted child operation and original assignment after a
    /// durable spawn fact exists. This is independently replayable because
    /// `OperationStarted` owns the original input and the assignment has a
    /// deterministic entry ID.
    fn commit_subagent_operation(
        &self,
        intent: &SubagentSpawnIntent,
        prepared: &PreparedSubagent,
    ) -> Result<AcceptedSubagentOperation, HarnessError> {
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let node = graph.agents.get(&intent.agent_id).ok_or_else(|| {
            HarnessError::invalid_state("subagent operation recovery lost its durable spawn fact")
        })?;
        if node.operation_id.is_some() {
            return Err(HarnessError::invalid_state(
                "subagent operation became durable while replay was reopening its lease",
            ));
        }
        self.append_subagent_operation(&mut session, intent, prepared)
    }

    fn append_subagent_operation(
        &self,
        session: &mut S,
        intent: &SubagentSpawnIntent,
        prepared: &PreparedSubagent,
    ) -> Result<AcceptedSubagentOperation, HarnessError> {
        let revision_entry_id = subagent_entry_id(&intent.agent_id, "harness")?;
        let assignment = ProvisionedEntry::user(
            subagent_entry_id(&intent.agent_id, "assignment")?,
            intent.task.clone(),
        );
        let configuration = self.manager.resolve_revision(
            prepared.harness_identity.revision_id(),
            &prepared.runtime_services,
        )?;
        let mut operation = OperationStartedRecord::new(
            intent.operation_id.clone(),
            intent.lane_id.clone(),
            Some(revision_entry_id),
            OperationKind::Subagent {
                agent_id: intent.agent_id.clone(),
                parent_operation_id: intent.parent_operation_id.clone(),
            },
            vec![assignment.clone()],
            prepared.harness_identity.revision_id().clone(),
            prepared.harness_identity.profile_id().clone(),
        );
        operation.operation_resume_data = configuration.lifecycle.before_operation()?;
        session.append_record(LaneRecord::OperationStarted(operation))?;
        let stored_assignment = session.append_entry(&intent.lane_id, assignment)?;
        Ok(AcceptedSubagentOperation {
            sequence: stored_assignment.header.seq,
        })
    }

    fn root_lane(&self) -> Result<Arc<LaneRuntime>, HarnessError> {
        self.lane(&self.root_lane_id)
    }

    fn lane(&self, lane_id: &LaneId) -> Result<Arc<LaneRuntime>, HarnessError> {
        self.lanes
            .lock()
            .map_err(|_| HarnessError::invalid_state("lane map mutex is poisoned"))?
            .get(lane_id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid_state(format!("unknown runtime lane {lane_id}")))
    }

    /// Register one explicit lane-local service bundle before its first drive.
    /// The durable session remains authoritative for whether the lane exists.
    pub fn register_lane(
        &self,
        lane_id: LaneId,
        mut services: RuntimeServices,
    ) -> Result<(), HarnessError> {
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
        if let Some(level) = reduction.effective_configuration.thinking_level.as_deref() {
            services = services.thinking_level(thinking_level_from_name(level)?);
        }
        let configuration = self.configuration_for_reduction_services(&services, &reduction)?;
        validate_reserved_host_tool_names(&services, &configuration)?;
        let graph = reduce_agent_graph(&snapshot)?;
        if graph
            .agents
            .values()
            .any(|node| node.spawned.lane_id == lane_id)
        {
            validate_child_subagent_surface(&configuration, &services)?;
        }
        let mut lanes = self
            .lanes
            .lock()
            .map_err(|_| HarnessError::invalid_state("lane map mutex is poisoned"))?;
        if lanes.contains_key(&lane_id) {
            return Err(HarnessError::invalid_state(format!(
                "runtime lane {lane_id} is already registered"
            )));
        }
        let lane = Arc::new(LaneRuntime::new(lane_id.clone(), services));
        lanes.insert(lane_id, Arc::clone(&lane));
        Ok(())
    }

    /// Request root cancellation for a durable open operation.
    ///
    /// The request is sticky across the short acceptance-to-agent-install
    /// window. It remains process-local because the operation WAL continues
    /// to own durable recovery and terminal classification.
    pub fn abort_root(&self) -> Result<bool, HarnessError> {
        let lane = self.root_lane()?;
        let active = {
            let session = self.session_lock()?;
            let reduction = reduce_lane(session.snapshot()?, lane.lane_id.clone())?;
            if reduction.lane_state.active_operation.is_none()
                && !lane.active.load(Ordering::Acquire)
            {
                false
            } else {
                // Keep the writer lock until the bit is installed. A terminal
                // commit therefore either wins before this check, or clears
                // the sticky request after its durable record commits.
                lane.abort_requested.store(true, Ordering::Release);
                true
            }
        };
        if !active {
            return Ok(false);
        }
        let _ = self.abort_lane_runtime(&lane)?;
        Ok(true)
    }

    /// Abort a live non-root lane.
    ///
    /// A coordinator must first make the lane durable and register its
    /// lane-local services; this only interrupts a currently installed agent.
    pub fn abort_lane(&self, lane_id: LaneId) -> Result<bool, HarnessError> {
        let lane = self.lane(&lane_id)?;
        self.abort_lane_runtime(&lane)
    }

    fn abort_lane_runtime(&self, lane: &LaneRuntime) -> Result<bool, HarnessError> {
        let agent = lane
            .active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?
            .clone();
        if let Some(agent) = agent {
            agent.abort();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Queue a steering prompt for the current durable core epoch. The core
    /// owns queue semantics; this host facade only prevents access before an
    /// epoch has been constructed or after it has settled.
    pub fn enqueue_steering(&self, content: impl Into<String>) -> Result<u64, HarnessError> {
        self.active_agent()?
            .enqueue_steering(content)
            .map_err(Into::into)
    }

    /// Queue a follow-up prompt for the current durable core epoch.
    pub fn enqueue_follow_up(&self, content: impl Into<String>) -> Result<u64, HarnessError> {
        self.active_agent()?
            .enqueue_follow_up(content)
            .map_err(Into::into)
    }

    /// Return a read-only core snapshot for the current epoch, when one is
    /// live. A terminal host may project it locally, but it must not persist
    /// this process-local snapshot as a session replacement.
    pub fn active_agent_snapshot(&self) -> Result<Option<AgentSnapshot>, HarnessError> {
        let agent = self
            .root_lane()?
            .active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?
            .clone();
        Ok(agent.map(|agent| agent.snapshot()))
    }

    /// Observe the current volatile prompt-layout predecessor without
    /// changing it. This content-free diagnostic is intended for host tests
    /// and reconnect health checks.
    #[cfg(test)]
    pub(crate) fn measure_prompt_layout(
        &self,
        request: &tea_core::scheduler::ModelRequest,
    ) -> crate::measurement::PromptCacheMeasurement {
        self.root_lane()
            .expect("root lane is registered for every supervisor")
            .prompt_layout_ledger
            .measure(request)
    }

    /// Replace the reasoning level for future epochs and append the semantic change while idle.
    pub fn replace_thinking_level(
        &self,
        thinking_level: ThinkingLevel,
    ) -> Result<(), HarnessError> {
        let lane = self.root_lane()?;
        if lane.active.load(Ordering::Acquire) {
            return Err(HarnessError::invalid_state(
                "thinking changes require an idle durable harness",
            ));
        }
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let entry_sequence = snapshot.next_sequence().0;
        let reduction = reduce_lane(snapshot, lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "thinking changes require no open durable operation",
            ));
        }
        let entry_id = EntryId::new(format!("thinking-change-{entry_sequence}"))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        session.append_entry(
            &lane.lane_id,
            ProvisionedEntry {
                id: entry_id,
                body: SessionEntry::ThinkingChanged(tea_session::ThinkingChangedEntry {
                    level: thinking_level_name(thinking_level).into(),
                }),
            },
        )?;
        *lane
            .thinking_level
            .lock()
            .map_err(|_| HarnessError::invalid_state("thinking level mutex is poisoned"))? =
            thinking_level;
        Ok(())
    }

    /// Return the reasoning level applied to future epochs.
    pub fn thinking_level(&self) -> Result<ThinkingLevel, HarnessError> {
        self.root_lane()?
            .thinking_level
            .lock()
            .map(|level| *level)
            .map_err(|_| HarnessError::invalid_state("thinking level mutex is poisoned"))
    }

    /// Return immutable extension commands from the active harness revision.
    pub fn extension_host_commands(
        &self,
    ) -> Result<Vec<ExtensionHostCommandDescription>, HarnessError> {
        let snapshot = self.snapshot()?;
        let lane = self.root_lane()?;
        let reduction = reduce_lane(snapshot, lane.lane_id.clone())?;
        let configuration = self.configuration_for_reduction(&lane, &reduction)?;
        Ok(configuration
            .host_commands()
            .iter()
            .map(|command| command.command.description().clone())
            .collect())
    }

    /// Execute one constrained extension command and persist its append-only
    /// local state update. The command never receives an application handle,
    /// session writer, path, or another extension's state.
    pub fn dispatch_extension_command(
        &self,
        name: &str,
        arguments: impl Into<String>,
    ) -> Result<ExtensionCommandDispatch, HarnessError> {
        let snapshot = self.snapshot()?;
        let lane = self.root_lane()?;
        let reduction = reduce_lane(snapshot.clone(), lane.lane_id.clone())?;
        let configuration = self.configuration_for_reduction(&lane, &reduction)?;
        let selected = configuration
            .host_commands()
            .iter()
            .find(|command| command.command.description().name == name)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!("unknown extension command {name}"))
            })?;
        if lane.active.load(Ordering::Acquire)
            && !selected.command.description().allowed_while_active
        {
            return Err(HarnessError::invalid_state(format!(
                "extension command {name} is unavailable while a durable operation is active",
            )));
        }
        let state = extension_state_view(&snapshot, &lane.lane_id, &selected.extension_id)?;
        let result = selected
            .command
            .invoke(&ExtensionCommandInput {
                arguments: arguments.into(),
                state,
            })
            .map_err(extension_error)?;
        if lane.active.load(Ordering::Acquire) && result.internal_input.is_some() {
            return Err(HarnessError::invalid_state(format!(
                "extension command {name} requested a continuation before the durable operation became idle",
            )));
        }
        if let Some(update) = result.state.clone() {
            self.append_extension_state_update(&lane, &selected.extension_id, update)?;
        }
        Ok(ExtensionCommandDispatch {
            extension_id: selected.extension_id,
            result,
        })
    }

    /// Evaluate every resolved extension's optional idle policy after a
    /// terminal operation. At most one continuation may be requested; callers
    /// must still re-check idle state immediately before starting it.
    pub fn evaluate_idle_extensions(&self) -> Result<Option<ExtensionContinuation>, HarnessError> {
        let lane = self.root_lane()?;
        if lane.active.load(Ordering::Acquire) {
            return Err(HarnessError::invalid_state(
                "extension idle hooks require an idle durable harness",
            ));
        }
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "extension idle hooks require no open durable operation",
            ));
        }
        let Some((operation_id, outcome, started_at_ms, finished_at_ms)) =
            terminal_operation(&snapshot, &lane.lane_id)
        else {
            return Ok(None);
        };
        if outcome != OperationOutcome::Completed {
            return Ok(None);
        }
        let configuration = self.configuration_for_reduction(&lane, &reduction)?;
        if configuration.idle_hooks().is_empty() {
            return Ok(None);
        }
        if !self.claim_idle_operation(&lane, &operation_id)? {
            return Ok(None);
        }
        let usage = operation_usage(&snapshot, &operation_id);
        let elapsed_active_seconds = finished_at_ms
            .saturating_sub(started_at_ms)
            .saturating_div(1000);
        let mut continuation = None;
        for idle in configuration.idle_hooks() {
            let state = extension_state_view(&snapshot, &lane.lane_id, &idle.extension_id)?;
            let result = idle
                .hook
                .on_idle(&ExtensionIdleInput {
                    operation_id: operation_id.to_string(),
                    outcome: extension_operation_outcome(&outcome),
                    usage: usage.clone(),
                    elapsed_active_seconds,
                    state,
                })
                .map_err(extension_error)?;
            if let Some(update) = result.state {
                self.append_extension_state_update(&lane, &idle.extension_id, update)?;
            }
            if let Some(input) = result.internal_input {
                if continuation.is_some() {
                    return Err(HarnessError::invalid_state(
                        "more than one extension requested an idle continuation",
                    ));
                }
                continuation = Some(ExtensionContinuation {
                    extension_id: idle.extension_id.clone(),
                    input,
                });
            }
        }
        Ok(continuation)
    }

    /// Build a reviewed, reference-aware artifact collection plan while the
    /// harness is idle. Active operations can materialize new references, so
    /// collection deliberately refuses to race an effect drive.
    pub fn plan_artifact_gc(
        &self,
        quota: tea_session::ArtifactQuota,
    ) -> Result<tea_session::ArtifactGcPlan, HarnessError> {
        self.ensure_idle_for_artifact_gc()?;
        let snapshot = self.snapshot()?;
        let additional_roots = self.manager.artifact_roots()?;
        Ok(tea_session::plan_artifact_gc(
            self.artifacts.as_ref(),
            &snapshot,
            additional_roots,
            quota,
        )?)
    }

    /// Apply exactly one previously reviewed collection plan while the
    /// harness is idle. The session helper revalidates every recorded root and
    /// planned object identity immediately before deletion.
    pub fn apply_artifact_gc(
        &self,
        plan: &tea_session::ArtifactGcPlan,
        quota: tea_session::ArtifactQuota,
    ) -> Result<tea_session::ArtifactGcReport, HarnessError> {
        self.ensure_idle_for_artifact_gc()?;
        let report = tea_session::apply_artifact_gc(self.artifacts.as_ref(), plan, quota)?;
        for object in &report.removed {
            self.publish_event(TeaEvent::Artifact(ArtifactEvent::Collected {
                artifact_id: object.artifact_id,
                byte_len: object.byte_len,
            }))?;
        }
        Ok(report)
    }

    fn ensure_idle_for_artifact_gc(&self) -> Result<(), HarnessError> {
        let lane = self.root_lane()?;
        if lane.active.load(Ordering::Acquire) {
            return Err(HarnessError::invalid_state(
                "artifact collection requires an idle durable harness",
            ));
        }
        let reduction = reduce_lane(self.snapshot()?, lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "artifact collection requires no open durable operation",
            ));
        }
        Ok(())
    }

    fn active_agent(&self) -> Result<Agent, HarnessError> {
        let lane = self.root_lane()?;
        self.active_lane_agent(&lane)
    }

    fn active_lane_agent(&self, lane: &LaneRuntime) -> Result<Agent, HarnessError> {
        lane.active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?
            .clone()
            .ok_or_else(|| HarnessError::invalid_state("no executable core epoch is active"))
    }

    fn install_lane_active_agent(
        &self,
        lane: &LaneRuntime,
        agent: Agent,
    ) -> Result<(), HarnessError> {
        let mut slot = lane
            .active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?;
        if slot.is_some() {
            return Err(HarnessError::invalid_state(
                "cannot install a second active core epoch",
            ));
        }
        *slot = Some(agent.clone());
        if lane.abort_requested.load(Ordering::Acquire) {
            // Root cancellation may arrive after durable operation acceptance
            // but before this core epoch has an executable agent. Installing
            // first preserves normal live-agent observation, then immediately
            // aborting makes that cancellation observable to core setup.
            agent.abort();
        }
        Ok(())
    }

    fn clear_lane_active_agent(&self, lane: &LaneRuntime) {
        if let Ok(mut slot) = lane.active_agent.lock() {
            // This is process-local observational state. If an unexpected
            // poison occurs after core settlement, it must not invalidate the
            // already committed session prefix.
            *slot = None;
        }
    }

    /// Publish a process-local application event after its durable mutation
    /// has committed. Writers release the session mutex before taking this
    /// lock: subscription takes the locks in the opposite order to create an
    /// atomic reconnect snapshot followed by live events, so holding both
    /// would deadlock.
    fn publish_event(&self, event: TeaEvent) -> Result<(), HarnessError> {
        let Ok(_publication) = self.publication.lock() else {
            // Publication is observational; a poisoned local event lock must
            // never invalidate an already durable state transition.
            return Ok(());
        };
        self.events.publish(event);
        Ok(())
    }

    /// Accept one user prompt durably, then drive its operation to settlement.
    ///
    /// The operation-start record commits before this method starts a core
    /// epoch. A storage failure therefore cannot be mistaken for acceptance.
    pub async fn run_root_prompt(
        &self,
        input: impl Into<String>,
    ) -> Result<DurableOperation, HarnessError> {
        self.run_root_prompt_with_authoring_authorization(input, false)
            .await
    }

    /// Start an extension-requested durable operation with host-only model
    /// context. The input is retained as external-only plugin memory and is
    /// never appended as a user message.
    pub async fn run_extension_continuation(
        &self,
        extension_id: impl Into<String>,
        input: impl Into<String>,
    ) -> Result<DurableOperation, HarnessError> {
        let lane = self.root_lane()?;
        let _claim = self.claim_lane_operation(Arc::clone(&lane))?;
        let operation =
            self.accept_extension_continuation(&lane, extension_id.into(), input.into())?;
        self.drive_fresh_epoch(&lane, operation).await
    }

    /// Accept and drive a prompt that the trusted application has explicitly
    /// marked as authoring-authorized. This is meaningful only in `Author`
    /// mode: the durable user-entry marker lets the stable control tool prove
    /// that a later harness mutation belongs to this user request instead of
    /// guessing from model or prompt text.
    pub async fn run_authoring_prompt(
        &self,
        input: impl Into<String>,
    ) -> Result<DurableOperation, HarnessError> {
        self.run_root_prompt_with_authoring_authorization(input, true)
            .await
    }

    async fn run_root_prompt_with_authoring_authorization(
        &self,
        input: impl Into<String>,
        authoring_authorized: bool,
    ) -> Result<DurableOperation, HarnessError> {
        let lane = self.root_lane()?;
        self.run_lane_prompt_with_authoring_authorization(lane, input.into(), authoring_authorized)
            .await
    }

    async fn run_lane_prompt_with_authoring_authorization(
        &self,
        lane: Arc<LaneRuntime>,
        input: String,
        authoring_authorized: bool,
    ) -> Result<DurableOperation, HarnessError> {
        // A fresh root operation inherits no cancellation request: a prior
        // request is cleared only by that prior operation's durable terminal
        // record.  In particular, never clear this bit before attempting the
        // claim: a rejected concurrent prompt must not erase cancellation for
        // the operation that already owns the lane.
        let _claim = self.claim_lane_operation(Arc::clone(&lane))?;
        let operation = self.accept_prompt(&lane, input, authoring_authorized)?;
        self.drive_fresh_epoch(&lane, operation).await
    }

    /// Drive one already durable non-root lane. Child coordination remains an
    /// explicit optional layer; this narrow internal entry point keeps the
    /// operation machine lane-generic and is exercised with scripted lanes.
    pub async fn run_lane_prompt(
        &self,
        lane_id: LaneId,
        input: impl Into<String>,
    ) -> Result<DurableOperation, HarnessError> {
        let lane = self.lane(&lane_id)?;
        self.run_lane_prompt_with_authoring_authorization(lane, input.into(), false)
            .await
    }

    /// Recover the one durable operation currently open on `main`.
    ///
    /// Recovery is derived exclusively from the session reducer. The harness
    /// never guesses whether an unrecorded provider request happened: that
    /// ambiguity is returned as [`HarnessError::RecoveryRequired`] until a
    /// host-specific reconciliation policy is supplied.
    pub async fn resume(self: &Arc<Self>) -> Result<DurableOperation, HarnessError> {
        if let Some(coordinator) = self.subagent_coordinator()? {
            self.recover_subagents_before_root_resume(&coordinator)
                .await?;
        }
        self.resume_lane_runtime(self.root_lane()?).await
    }

    /// Rebuild every recoverable child owned by the open root operation before
    /// a resumed root can execute a `wait_agent` effect. Durable graph facts
    /// select the required host lease; volatile task handles are recreated
    /// only after that lease and lane-local services are validated again.
    async fn recover_subagents_before_root_resume(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
    ) -> Result<(), HarnessError> {
        let snapshot = self.snapshot()?;
        let root = reduce_lane(snapshot.clone(), self.root_lane_id.clone())?;
        let Some(root_operation_id) = root.lane_state.active_operation else {
            return Ok(());
        };
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let nodes = graph
            .agents
            .values()
            .filter(|node| {
                node.spawned.parent_lane_id == self.root_lane_id
                    && node.spawned.parent_operation_id == root_operation_id
            })
            .cloned()
            .collect::<Vec<_>>();
        for node in nodes {
            // A terminal fact is durable proof that this child must never be
            // driven again. Cleanup is idempotent host work keyed by the
            // deterministic lease; do not demand `reopen` for a worktree a
            // prior successful cleanup may already have removed.
            if node.terminal.is_some() {
                let workspace = super::subagents::WorkspaceLease {
                    id: node.spawned.workspace_lease_id.clone(),
                    logical_workspace: snapshot.header().workspace.clone(),
                };
                coordinator
                    .services()
                    .host
                    .cleanup(workspace)
                    .await
                    .map_err(|_| HarnessError::SubagentRecovery {
                        agent_id: node.spawned.agent_id.clone(),
                        stage: SubagentRecoveryStage::CleanupWorkspace,
                    })?;
                coordinator.mark_exposable_and_notify(node.spawned.agent_id);
                continue;
            }
            // A spawn fact without operation acceptance is completed by the
            // root tool replay using its original durable arguments. There is
            // no assignment payload in the graph fact itself to invent here.
            let Some(operation_id) = node.operation_id.clone() else {
                continue;
            };
            let prepared = self.reopen_subagent_prepared(coordinator, &node).await?;
            self.validate_reopened_subagent(&node, &prepared)?;
            self.ensure_subagent_lane_registered(
                node.spawned.lane_id.clone(),
                prepared
                    .runtime_services
                    .thinking_level(thinking_level_from_name(&node.spawned.thinking)?),
            )?;
            let lane = self.lane(&node.spawned.lane_id)?;
            let reduction = reduce_lane(self.snapshot()?, node.spawned.lane_id.clone())?;
            if reduction.lane_state.active_operation.as_ref() == Some(&operation_id) {
                self.start_recovered_subagent_task(coordinator, &node, prepared.workspace)?;
            } else {
                // Child execution is terminal but report/delta finalization is
                // not. Reopen the same lease and complete that idempotent
                // durable suffix before the root can observe it.
                self.settle_subagent_task(
                    coordinator,
                    node.spawned.agent_id.clone(),
                    Some(prepared.workspace),
                    true,
                )
                .await?;
                let _ = lane;
            }
        }
        Ok(())
    }

    /// Resume one already registered child lane from its reducer-derived
    /// obligation. The coordinator owns which child lanes are eligible.
    pub async fn resume_lane(&self, lane_id: LaneId) -> Result<DurableOperation, HarnessError> {
        self.resume_lane_runtime(self.lane(&lane_id)?).await
    }

    async fn resume_lane_runtime(
        &self,
        lane: Arc<LaneRuntime>,
    ) -> Result<DurableOperation, HarnessError> {
        let _claim = self.claim_lane_operation(Arc::clone(&lane))?;
        // Rehydrate only process-local policy state before inspecting the
        // next durable obligation. This performs no session mutation, so a
        // crash before the next consumer commits can safely invoke the same
        // idempotent callback again on the next recovery attempt.
        let (_, _, snapshot) = self.active_recovery(&lane.lane_id)?;
        self.rebuild_lifecycle_state(&lane, &snapshot)?;
        loop {
            let (operation_id, plan, snapshot) = self.active_recovery(&lane.lane_id)?;
            match plan {
                RecoveryPlan::AppendAcceptedInput { entries, .. } => {
                    let sequence = {
                        let mut session = self.session_lock()?;
                        let mut sequence = None;
                        for entry in entries {
                            sequence = Some(session.append_entry(&lane.lane_id, entry)?.header.seq);
                        }
                        sequence
                    };
                    if let Some(sequence) = sequence {
                        self.publish_event(TeaEvent::Session(SessionEvent::OperationAccepted {
                            sequence,
                            lane_id: lane.lane_id.clone(),
                            operation_id: operation_id.clone(),
                        }))?;
                    }
                }
                RecoveryPlan::SynthesizeInterruptedToolResult { result_entry_id } => {
                    // A started workspace mutation is not equivalent to an
                    // interrupted read-only tool. Appending the generic error
                    // would let a later model turn retry an effect whose first
                    // outcome is unknown. Keep the durable prefix open until a
                    // host explicitly reconciles it instead.
                    if recovery_tool_start(&snapshot, &result_entry_id)?.tool_name
                        == "apply_agent_changes"
                    {
                        return Err(HarnessError::RecoveryRequired {
                            plan: RecoveryPlan::SynthesizeInterruptedToolResult { result_entry_id },
                        });
                    }
                    self.append_interrupted_tool_result(&lane, &snapshot, &result_entry_id)?;
                }
                RecoveryPlan::ReplayToolIfStillSafe { tool } => {
                    if !self.replay_is_still_safe(&lane, &tool) {
                        self.append_interrupted_tool_result(
                            &lane,
                            &snapshot,
                            &tool.result_entry_id,
                        )?;
                        continue;
                    }
                    let epoch_id = open_epoch(&snapshot, &operation_id).ok_or_else(|| {
                        HarnessError::invalid_state(
                            "replay-safe tool recovery has no open durable epoch",
                        )
                    })?;
                    let tool_calls = recovery_tool_calls(&snapshot, &tool.assistant_entry_id)?;
                    let mut replay_tool_starts = BTreeMap::new();
                    replay_tool_starts.insert(
                        (tool.assistant_entry_id.clone(), tool.tool_index),
                        tool.clone(),
                    );
                    return self
                        .drive_epoch(
                            &lane,
                            operation_id,
                            epoch_id,
                            Some(RecoveryToolDrive {
                                assistant_entry_id: tool.assistant_entry_id,
                                tool_calls,
                                replay_tool_starts,
                            }),
                        )
                        .await;
                }
                RecoveryPlan::ResumeAssistantToolPath { assistant_entry_id } => {
                    let epoch_id = open_epoch(&snapshot, &operation_id).ok_or_else(|| {
                        HarnessError::invalid_state(
                            "assistant tool recovery has no open durable epoch",
                        )
                    })?;
                    let tool_calls = recovery_tool_calls(&snapshot, &assistant_entry_id)?;
                    return self
                        .drive_epoch(
                            &lane,
                            operation_id,
                            epoch_id,
                            Some(RecoveryToolDrive {
                                assistant_entry_id,
                                tool_calls,
                                replay_tool_starts: BTreeMap::new(),
                            }),
                        )
                        .await;
                }
                RecoveryPlan::ActivateHarness { request } => {
                    self.activate_pending_harness(&lane, &operation_id, &request)?;
                    return self.drive_fresh_epoch(&lane, operation_id).await;
                }
                plan @ RecoveryPlan::ReconcileProviderRequest { .. } => {
                    return Err(HarnessError::RecoveryRequired { plan });
                }
                RecoveryPlan::StartEpoch { .. } => {
                    return self.drive_fresh_epoch(&lane, operation_id).await;
                }
                RecoveryPlan::ResumeOperation { .. } => {
                    let epoch_id = open_epoch(&snapshot, &operation_id).ok_or_else(|| {
                        HarnessError::invalid_state(
                            "ordinary operation recovery has no open durable epoch",
                        )
                    })?;
                    return self.drive_epoch(&lane, operation_id, epoch_id, None).await;
                }
            }
        }
    }

    fn claim_lane_operation(&self, lane: Arc<LaneRuntime>) -> Result<OperationClaim, HarnessError> {
        lane.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                HarnessError::invalid_state("durable harness already has an active drive")
            })?;
        Ok(OperationClaim { lane })
    }

    fn active_recovery(
        &self,
        lane: &LaneId,
    ) -> Result<(OperationId, RecoveryPlan, SessionSnapshot), HarnessError> {
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), lane.clone())?;
        let operation_id = reduction.lane_state.active_operation.ok_or_else(|| {
            HarnessError::invalid_state("main lane has no open operation to recover")
        })?;
        let plan = reduction.recovery_plan.ok_or_else(|| {
            HarnessError::invalid_state("open durable operation has no recovery plan")
        })?;
        Ok((operation_id, plan, snapshot))
    }

    fn session_lock(&self) -> Result<MutexGuard<'_, S>, HarnessError> {
        self.session
            .lock()
            .map_err(|_| HarnessError::invalid_state("durable session mutex is poisoned"))
    }

    fn accept_prompt(
        &self,
        lane_runtime: &LaneRuntime,
        input: String,
        authoring_authorized: bool,
    ) -> Result<OperationId, HarnessError> {
        let lane = lane_runtime.lane_id.clone();
        let (operation_id, sequence) = {
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let reduction = reduce_lane(snapshot.clone(), lane.clone())?;
            if reduction.lane_state.active_operation.is_some() {
                return Err(HarnessError::RecoveryRequired {
                    plan: reduction.recovery_plan.ok_or_else(|| {
                        HarnessError::invalid_state(
                            "main lane has an open operation without a recovery plan",
                        )
                    })?,
                });
            }
            let operation_id = OperationId::new(durable_identifier(
                "operation",
                [
                    snapshot.header().session_id.as_str(),
                    &snapshot.last_sequence().0.to_string(),
                    &input,
                ],
            ))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let input_entry_id = EntryId::new(durable_identifier(
                "entry-user",
                [operation_id.as_str(), "0"],
            ))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let mut input_entry = ProvisionedEntry::user(input_entry_id, input);
            if authoring_authorized {
                let SessionEntry::UserMessage(entry) = &mut input_entry.body else {
                    unreachable!("user entry constructor must produce a user message");
                };
                entry.metadata.insert(
                    AUTHORING_AUTHORIZATION_METADATA_KEY.into(),
                    JsonValue::Bool(true),
                );
            }
            let configuration = self.configuration_for_reduction(lane_runtime, &reduction)?;
            let mut record = OperationStartedRecord::new(
                operation_id.clone(),
                lane.clone(),
                reduction.lane_state.leaf_id,
                OperationKind::Run,
                vec![input_entry.clone()],
                configuration.identity.revision_id.clone(),
                configuration.identity.profile_id.clone(),
            );
            // The capability-free policy result becomes durable in the same
            // acceptance record. No caller can observe acceptance until this
            // write succeeds, and no core effect begins before it does.
            record.operation_resume_data = configuration.lifecycle.before_operation()?;
            session.append_record(LaneRecord::OperationStarted(record))?;
            let stored_input = session.append_entry(&lane, input_entry)?;
            (operation_id, stored_input.header.seq)
        };
        self.publish_event(TeaEvent::Session(SessionEvent::OperationAccepted {
            sequence,
            lane_id: lane,
            operation_id: operation_id.clone(),
        }))?;
        Ok(operation_id)
    }

    fn accept_extension_continuation(
        &self,
        lane_runtime: &LaneRuntime,
        extension_id: String,
        input: String,
    ) -> Result<OperationId, HarnessError> {
        if !portable_extension_label(&extension_id)
            || input.trim().is_empty()
            || input.len() > 16 * 1024
        {
            return Err(HarnessError::invalid_state(
                "extension continuation requires a portable extension ID and bounded non-empty input",
            ));
        }
        let lane = lane_runtime.lane_id.clone();
        let (operation_id, sequence) = {
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let reduction = reduce_lane(snapshot.clone(), lane.clone())?;
            if reduction.lane_state.active_operation.is_some() {
                return Err(HarnessError::RecoveryRequired {
                    plan: reduction.recovery_plan.ok_or_else(|| {
                        HarnessError::invalid_state(
                            "main lane has an open operation without a recovery plan",
                        )
                    })?,
                });
            }
            let configuration = self.configuration_for_reduction(lane_runtime, &reduction)?;
            let operation_id = OperationId::new(durable_identifier(
                "extension-operation",
                [
                    snapshot.header().session_id.as_str(),
                    extension_id.as_str(),
                    &snapshot.last_sequence().0.to_string(),
                    input.as_str(),
                ],
            ))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let entry_id = EntryId::new(durable_identifier(
                "entry-extension-continuation",
                [operation_id.as_str(), extension_id.as_str()],
            ))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let input_entry = ProvisionedEntry {
                id: entry_id,
                body: SessionEntry::PluginMemory(PluginMemoryEntry {
                    plugin_id: extension_id.clone(),
                    kind: "extension.continuation.v1".into(),
                    content: PayloadRef::Inline(JsonValue::object([(
                        "input",
                        JsonValue::String(input),
                    )])),
                    provenance: Vec::new(),
                    visibility: MemoryVisibility::ExternalOnly,
                    retention: MemoryRetention::Session,
                }),
            };
            let mut record = OperationStartedRecord::new(
                operation_id.clone(),
                lane.clone(),
                reduction.lane_state.leaf_id,
                OperationKind::Other("extension_continuation".into()),
                vec![input_entry.clone()],
                configuration.identity.revision_id().clone(),
                configuration.identity.profile_id().clone(),
            );
            record.operation_resume_data = configuration.lifecycle.before_operation()?;
            session.append_record(LaneRecord::OperationStarted(record))?;
            let stored = session.append_entry(&lane, input_entry)?;
            (operation_id, stored.header.seq)
        };
        self.publish_event(TeaEvent::Session(SessionEvent::OperationAccepted {
            sequence,
            lane_id: lane,
            operation_id: operation_id.clone(),
        }))?;
        Ok(operation_id)
    }

    fn append_extension_state_update(
        &self,
        lane: &LaneRuntime,
        extension_id: &str,
        update: ExtensionStateUpdate,
    ) -> Result<(), HarnessError> {
        validate_extension_state_update(extension_id, &update)?;
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let entry_id = EntryId::new(durable_identifier(
            "entry-extension-state",
            [
                extension_id,
                update.kind.as_str(),
                &snapshot.next_sequence().0.to_string(),
            ],
        ))
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        session.append_entry(
            &lane.lane_id,
            ProvisionedEntry {
                id: entry_id,
                body: SessionEntry::PluginMemory(PluginMemoryEntry {
                    plugin_id: extension_id.to_owned(),
                    kind: update.kind,
                    content: PayloadRef::Inline(update.content),
                    provenance: Vec::new(),
                    visibility: MemoryVisibility::ExternalOnly,
                    retention: MemoryRetention::Session,
                }),
            },
        )?;
        Ok(())
    }

    /// Persist the one-shot idle-decision claim before any extension callback
    /// runs. A crash may conservatively suppress that one continuation, but
    /// it can never cause a completed operation to launch duplicates after a
    /// reopen or a repeated terminal notification.
    fn claim_idle_operation(
        &self,
        lane: &LaneRuntime,
        operation_id: &OperationId,
    ) -> Result<bool, HarnessError> {
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "extension idle hooks require no open durable operation",
            ));
        }
        let Some((latest, outcome, _, _)) = terminal_operation(&snapshot, &lane.lane_id) else {
            return Ok(false);
        };
        if latest != *operation_id || outcome != OperationOutcome::Completed {
            return Ok(false);
        }
        if idle_operation_is_claimed(&snapshot, operation_id)? {
            return Ok(false);
        }
        let entry_id = EntryId::new(durable_identifier(
            "entry-extension-idle-claim",
            [operation_id.as_str()],
        ))
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        session.append_entry(
            &lane.lane_id,
            ProvisionedEntry {
                id: entry_id,
                body: SessionEntry::PluginMemory(PluginMemoryEntry {
                    plugin_id: "tea.extension.runtime".into(),
                    kind: "idle.evaluated.v1".into(),
                    content: PayloadRef::Inline(JsonValue::object([(
                        "operation_id",
                        JsonValue::String(operation_id.to_string()),
                    )])),
                    provenance: Vec::new(),
                    visibility: MemoryVisibility::ExternalOnly,
                    retention: MemoryRetention::Session,
                }),
            },
        )?;
        Ok(true)
    }

    async fn drive_fresh_epoch(
        &self,
        lane: &Arc<LaneRuntime>,
        operation_id: OperationId,
    ) -> Result<DurableOperation, HarnessError> {
        let epoch = self.start_epoch(lane, &operation_id)?;
        self.drive_epoch(lane, operation_id, epoch, None).await
    }

    async fn drive_epoch(
        &self,
        lane: &Arc<LaneRuntime>,
        operation_id: OperationId,
        epoch_id: EpochId,
        recovery: Option<RecoveryToolDrive>,
    ) -> Result<DurableOperation, HarnessError> {
        let lane_runtime = Arc::clone(lane);
        let configuration = self.epoch_configuration(&lane_runtime, &epoch_id)?;
        let thinking_level = *lane_runtime
            .thinking_level
            .lock()
            .map_err(|_| HarnessError::invalid_state("thinking level mutex is poisoned"))?;
        let runtime_services = lane_runtime
            .runtime_services
            .clone()
            .thinking_level(thinking_level);
        let messages = self.core_messages(&lane_runtime, &configuration, recovery.as_ref())?;
        let internal_input = extension_continuation_input(&self.snapshot()?, &operation_id)?;
        let provider_surface_digest = configuration
            .harness_snapshot
            .as_ref()
            .map(|snapshot| snapshot.fingerprints.provider_surface_digest.to_hex());
        let provenance = self.provenance(
            &lane_runtime,
            &operation_id,
            &epoch_id,
            &configuration.identity,
            provider_surface_digest,
        )?;
        let host_tools =
            self.host_tools_for_configuration(&lane_runtime, &configuration, &operation_id)?;
        let tool_definition_digests =
            all_tool_definition_digests(&runtime_services, &configuration, &host_tools)?;
        let tool_definition_schemas =
            all_tool_definition_schemas(&runtime_services, &configuration, &host_tools)?;
        let replay_safe_host_tools = replay_safe_host_tools(&host_tools);
        let recovery_assistant_entry = recovery
            .as_ref()
            .map(|recovery| recovery.assistant_entry_id.clone());
        let replay_tool_starts = recovery
            .as_ref()
            .map(|recovery| recovery.replay_tool_starts.clone())
            .unwrap_or_default();
        let runtime = Arc::new(Mutex::new(EpochRuntime::new(EpochRuntimeInit {
            session: Arc::clone(&self.session),
            artifacts: Arc::clone(&self.artifacts),
            events: Arc::clone(&self.events),
            lane: lane_runtime.lane_id.clone(),
            operation_id: operation_id.clone(),
            epoch_id: epoch_id.clone(),
            identity: configuration.identity.clone(),
            resolved_harness: configuration.clone(),
            memory_collector: Arc::clone(&configuration.memory_collector),
            tool_definition_digests,
            tool_definition_schemas,
            replay_safe_host_tools,
            last_assistant_entry: recovery_assistant_entry,
            replay_tool_starts,
        })));
        let gate: Arc<dyn EffectGate> = Arc::new(DurableEffectGate { runtime });
        let agent = runtime_services.build_agent_with_tools(
            &configuration,
            gate,
            provenance.clone(),
            Arc::clone(&lane_runtime.prompt_layout_ledger),
            host_tools,
            internal_input
                .as_deref()
                .map(SerializedJson::new)
                .into_iter()
                .collect(),
        )?;
        let trace_capture = TraceCaptureSink::default();
        let trace_episode_id = format!(
            "tea-core-run-v1:{}",
            provenance
                .core_run_id
                .as_deref()
                .ok_or_else(|| HarnessError::invalid_state("core trace has no run identity"))?
        );
        let trace_observer = Arc::new(TraceObserver::new_with_provenance(
            trace_episode_id,
            provenance.clone(),
            RedactingSink::new(trace_capture.clone(), DurableTraceRedactor),
        ));
        let _trace_events = agent.subscribe(trace_observer);
        // Core events are run-scoped and process-local. They are forwarded to
        // the application envelope only after core has reduced each event;
        // durable effect gates have already committed their corresponding
        // session facts before core can emit downstream lifecycle events.
        let _agent_events = agent.subscribe(Arc::new(HarnessAgentEventObserver {
            events: Arc::clone(&self.events),
            lane_id: lane_runtime.lane_id.clone(),
        }));
        let run = match recovery {
            Some(recovery) => {
                agent.restore_pending_tool_calls(messages, recovery.tool_calls.clone())?;
                agent.start_recover_tool_calls(recovery.tool_calls)?
            }
            None => {
                agent.restore_messages(messages)?;
                if internal_input.is_some() {
                    agent.start_internal()?
                } else {
                    agent.start_continue()?
                }
            }
        };
        self.install_lane_active_agent(&lane_runtime, agent)?;
        if lane_runtime.lane_id == self.root_lane_id
            && lane_runtime.abort_requested.load(Ordering::Acquire)
        {
            // A sticky root abort can be observed before the newly created
            // core run has emitted any trace lifecycle event. Do not enter a
            // trace-bearing drive in that interval: close the already durable
            // epoch and operation directly, exactly as an abort requested
            // before agent installation requires.
            self.clear_lane_active_agent(&lane_runtime);
            self.settle_root_children_before_finish(&operation_id)
                .await?;
            self.finish_operation(
                &lane_runtime,
                &operation_id,
                &epoch_id,
                OperationOutcome::Aborted,
            )?;
            return Err(HarnessError::Core(CoreError::Cancelled));
        }
        let drive_result = run.drive().await;
        self.clear_lane_active_agent(&lane_runtime);
        let trace_events = trace_capture.events()?;
        let complete_trace = matches!(trace_events.first(), Some(TraceEvent::EpisodeHeader(_)))
            && matches!(trace_events.last(), Some(TraceEvent::EpisodeEnd(_)));
        if complete_trace || !matches!(&drive_result, Err(CoreError::Cancelled)) {
            self.persist_trace_artifact(
                &operation_id,
                &epoch_id,
                &configuration.identity,
                &provenance,
                trace_events,
            )?;
        }
        // Cancellation can win immediately after installation, before core
        // has emitted its terminal trace event. There is no complete trace to
        // retain in that exact interval; the durable aborted operation remains
        // the authoritative evidence. Any non-cancelled drive still requires
        // a complete trace artifact.
        match drive_result {
            Ok(()) => {
                let reduction = reduce_lane(self.snapshot()?, lane_runtime.lane_id.clone())?;
                if let Some(pending) = reduction.pending_harness_activation {
                    self.finish_epoch(
                        &lane_runtime,
                        &operation_id,
                        &epoch_id,
                        EpochFinishReason::ActivationPending,
                    )?;
                    let revision = self.activate_pending_harness(
                        &lane_runtime,
                        &operation_id,
                        &pending.request,
                    )?;
                    self.publish_event(TeaEvent::Harness(HarnessEvent::RolloverStarted {
                        lane_id: lane_runtime.lane_id.clone(),
                        operation_id: operation_id.clone(),
                        from_epoch: epoch_id,
                        to_revision_id: revision.revision_id.clone(),
                    }))?;
                    let next_epoch = self.start_epoch(&lane_runtime, &operation_id)?;
                    self.publish_event(TeaEvent::Harness(HarnessEvent::RolloverCompleted {
                        lane_id: lane_runtime.lane_id.clone(),
                        operation_id: operation_id.clone(),
                        epoch_id: next_epoch.clone(),
                        revision_id: revision.revision_id,
                    }))?;
                    Box::pin(self.drive_epoch(&lane_runtime, operation_id, next_epoch, None)).await
                } else {
                    if lane_runtime.lane_id == self.root_lane_id {
                        self.settle_root_children_before_finish(&operation_id)
                            .await?;
                    }
                    self.finish_operation(
                        &lane_runtime,
                        &operation_id,
                        &epoch_id,
                        OperationOutcome::Completed,
                    )
                }
            }
            Err(error @ CoreError::EffectGate(_)) => Err(HarnessError::Core(error)),
            Err(error) => {
                let outcome = if matches!(error, CoreError::Cancelled) {
                    OperationOutcome::Aborted
                } else {
                    OperationOutcome::Failed {
                        code: core_failure_code(&error).into(),
                    }
                };
                if lane_runtime.lane_id == self.root_lane_id {
                    self.settle_root_children_before_finish(&operation_id)
                        .await?;
                }
                self.finish_operation(&lane_runtime, &operation_id, &epoch_id, outcome)?;
                Err(HarnessError::Core(error))
            }
        }
    }

    /// Assemble only Rust-owned stable tools for one immutable epoch. The
    /// control tool receives a frozen epoch identity and durable operation;
    /// it may stage a candidate but cannot activate it in place.
    fn host_tools_for_configuration(
        &self,
        lane: &LaneRuntime,
        configuration: &ResolvedHarness,
        operation_id: &OperationId,
    ) -> Result<ToolRegistry, HarnessError> {
        let artifact_tools = stable_artifact_tools(
            Arc::clone(&self.session),
            Arc::clone(&self.artifacts),
            configuration.artifact_policy_config().clone(),
        )?;
        let mut tools = ToolRegistry::default();
        if configuration.self_extension_mode.exposes_control_tool() {
            merge_tool_registries(
                &mut tools,
                stable_harness_tools(
                    Arc::clone(&self.session),
                    Arc::clone(&self.artifacts),
                    Arc::clone(&self.manager),
                    lane.lane_id.clone(),
                    configuration.identity.clone(),
                    operation_id.clone(),
                    self.rollover_budget,
                    lane.runtime_services.clone(),
                    Arc::clone(&self.events),
                ),
            )?;
        }
        merge_tool_registries(&mut tools, artifact_tools)?;
        // Root selection occurs exactly at this public-facing lane boundary.
        // All later tool execution receives the explicit lane attribution in
        // its typed provenance; child lanes therefore never inherit the five
        // collaboration capabilities merely by sharing a supervisor.
        if lane.lane_id == self.root_lane_id
            && let Some(coordinator) = self.subagent_coordinator()?
        {
            merge_tool_registries(
                &mut tools,
                root_subagent_runtime_tools(coordinator)
                    .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
            )?;
        }
        Ok(tools)
    }

    fn start_epoch(
        &self,
        lane_runtime: &LaneRuntime,
        operation_id: &OperationId,
    ) -> Result<EpochId, HarnessError> {
        let lane = lane_runtime.lane_id.clone();
        let (epoch_id, sequence, revision_id, snapshot_id, profile_id) = {
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let reduction = reduce_lane(snapshot.clone(), lane.clone())?;
            if reduction.lane_state.active_operation.as_ref() != Some(operation_id) {
                return Err(HarnessError::invalid_state(format!(
                    "operation {operation_id} is not active on lane {lane}"
                )));
            }
            if let Some(plan) = &reduction.recovery_plan
                && !matches!(&plan, RecoveryPlan::StartEpoch { operation_id: plan_operation } if plan_operation == operation_id)
            {
                return Err(HarnessError::RecoveryRequired { plan: plan.clone() });
            }
            let configuration = self.configuration_for_reduction(lane_runtime, &reduction)?;
            let epoch_index = snapshot
                .records()
                .iter()
                .filter(|record| {
                    matches!(
                        &record.record,
                        LaneRecord::EpochStarted(started) if &started.operation_id == operation_id
                    )
                })
                .count() as u32;
            let epoch_id = EpochId::new(durable_identifier(
                "epoch",
                [operation_id.as_str(), &epoch_index.to_string()],
            ))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let core_run_id =
                CoreRunId::new(durable_identifier("core-run", [epoch_id.as_str(), "v1"]))
                    .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let epoch_resume_data = configuration.lifecycle.before_epoch()?;
            let revision_id = configuration.identity.revision_id.clone();
            let snapshot_id = configuration.identity.snapshot_id.clone();
            let profile_id = configuration.identity.profile_id.clone();
            let stored = session.append_record(LaneRecord::EpochStarted(EpochStartedRecord {
                id: epoch_id.clone(),
                operation_id: operation_id.clone(),
                epoch_index,
                source_leaf_id: reduction.lane_state.leaf_id,
                harness_revision_id: revision_id.clone(),
                harness_snapshot_id: snapshot_id.clone(),
                model_harness_profile: profile_id.clone(),
                core_run_id,
                epoch_resume_data,
            }))?;
            (epoch_id, stored.seq, revision_id, snapshot_id, profile_id)
        };
        self.publish_event(TeaEvent::Session(SessionEvent::EpochStarted {
            sequence,
            lane_id: lane,
            operation_id: operation_id.clone(),
            revision_id,
            snapshot_id,
            profile_id,
        }))?;
        Ok(epoch_id)
    }

    fn finish_operation(
        &self,
        lane: &LaneRuntime,
        operation_id: &OperationId,
        epoch_id: &EpochId,
        outcome: OperationOutcome,
    ) -> Result<DurableOperation, HarnessError> {
        self.finish_epoch(
            lane,
            operation_id,
            epoch_id,
            match outcome {
                OperationOutcome::Completed => EpochFinishReason::Settled,
                OperationOutcome::Aborted | OperationOutcome::Failed { .. } => {
                    EpochFinishReason::Interrupted
                }
            },
        )?;
        let (sequence, reduction) = {
            let mut session = self.session_lock()?;
            let stored =
                session.append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                    operation_id: operation_id.clone(),
                    outcome: outcome.clone(),
                }))?;
            let snapshot = session.snapshot()?;
            (stored.seq, reduce_lane(snapshot, lane.lane_id.clone())?)
        };
        if reduction.lane_state.active_operation.is_some() || reduction.recovery_plan.is_some() {
            return Err(HarnessError::invalid_state(
                "terminal operation did not reduce to an idle lane",
            ));
        }
        if lane.lane_id == self.root_lane_id {
            // Only a confirmed durable terminal root operation clears the
            // pre-install cancellation request. Claim release alone is not a
            // settlement boundary: an effect-gate failure may leave recovery
            // work open and must retain this request in-process.
            lane.abort_requested.store(false, Ordering::Release);
        }
        self.publish_event(TeaEvent::Session(SessionEvent::OperationFinished {
            sequence,
            lane_id: lane.lane_id.clone(),
            operation_id: operation_id.clone(),
            outcome: format!("{:?}", outcome).to_ascii_lowercase(),
        }))?;
        Ok(DurableOperation {
            id: operation_id.clone(),
            outcome,
        })
    }

    fn finish_epoch(
        &self,
        _lane: &LaneRuntime,
        operation_id: &OperationId,
        epoch_id: &EpochId,
        reason: EpochFinishReason,
    ) -> Result<(), HarnessError> {
        self.session_lock()?
            .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
                epoch_id: epoch_id.clone(),
                operation_id: operation_id.clone(),
                reason,
            }))?;
        Ok(())
    }

    /// Commit the semantic revision transition after the previous core epoch
    /// has settled.  Candidate construction and revision derivation are
    /// idempotent; the only branch mutation here is the provisioned semantic
    /// entry, so a restart either sees the pending request or the exact same
    /// committed revision.
    fn activate_pending_harness(
        &self,
        lane: &LaneRuntime,
        operation_id: &OperationId,
        request: &tea_session::HarnessActivationRequestedRecord,
    ) -> Result<HarnessRevisionV1, HarnessError> {
        if &request.operation_id != operation_id {
            return Err(HarnessError::invalid_state(
                "harness activation request belongs to a different operation",
            ));
        }
        let used_rollovers = self
            .snapshot()?
            .records()
            .iter()
            .filter(|stored| {
                matches!(
                    &stored.record,
                    LaneRecord::EpochFinished(EpochFinishedRecord {
                        operation_id: finished_operation,
                        reason: EpochFinishReason::ActivationPending,
                        ..
                    }) if *finished_operation == *operation_id
                )
            })
            .count() as u32;
        if used_rollovers > self.rollover_budget {
            return Err(HarnessError::invalid_state(format!(
                "operation {operation_id} exhausted its {} automatic harness rollover budget",
                self.rollover_budget
            )));
        }
        let manager = &self.manager;
        let candidate = manager.candidate(&request.candidate_id)?;
        if !candidate.validation.accepted || candidate.validation.is_noop {
            return Err(HarnessError::invalid_state(
                "pending harness activation no longer names an accepted non-noop candidate",
            ));
        }
        if candidate.draft.parent_revision_id != request.parent_revision_id
            || candidate.draft.proposed_snapshot_id != request.proposed_snapshot_id
        {
            return Err(HarnessError::invalid_state(
                "pending harness activation disagrees with the immutable candidate",
            ));
        }
        let reduction = reduce_lane(self.snapshot()?, lane.lane_id.clone())?;
        if reduction.effective_configuration.harness_revision.as_ref()
            != Some(&request.parent_revision_id)
        {
            return Err(HarnessError::invalid_state(
                "pending harness activation parent is not the current branch revision",
            ));
        }
        let revision = manager.activate_candidate(
            &request.candidate_id,
            HarnessActor::Host,
            session_time_ms(),
        )?;
        if revision.snapshot_id != request.proposed_snapshot_id
            || revision.revision_id == request.parent_revision_id
        {
            return Err(HarnessError::invalid_state(
                "candidate activation did not produce the requested immutable child revision",
            ));
        }
        // The revision's catalog entry must commit before the branch semantic
        // pointer can name it. A crash before the pointer leaves only an
        // orphan immutable catalog revision, which recovery can derive again;
        // a crash after it leaves every active source object reconstructible.
        {
            let mut session = self.session_lock()?;
            manager.persist_catalog(&mut *session, self.artifacts.as_ref())?;
            session.append_entry(
                &lane.lane_id,
                ProvisionedEntry {
                    id: request.revision_entry_id.clone(),
                    body: SessionEntry::HarnessRevisionChanged(
                        tea_session::HarnessRevisionChangedEntry {
                            revision_id: revision.revision_id.clone(),
                            snapshot_id: revision.snapshot_id.clone(),
                            rollback_from: matches!(
                                revision.reason,
                                HarnessRevisionReason::Rollback
                            )
                            .then(|| request.parent_revision_id.clone()),
                        },
                    ),
                },
            )?;
        }
        let post_activation = reduce_lane(self.snapshot()?, lane.lane_id.clone())?;
        if post_activation.pending_harness_activation.is_some()
            || post_activation
                .effective_configuration
                .harness_revision
                .as_ref()
                == Some(&request.parent_revision_id)
        {
            return Err(HarnessError::invalid_state(
                "harness activation entry did not advance the reduced branch revision",
            ));
        }
        self.publish_event(TeaEvent::Harness(HarnessEvent::ActivationScheduled {
            lane_id: lane.lane_id.clone(),
            operation_id: operation_id.clone(),
            candidate_id: candidate.candidate_id,
            target_revision_id: revision.revision_id.clone(),
        }))?;
        let provider_surface_changed = candidate
            .draft
            .changed_surfaces
            .contains(&HarnessSurface::SystemPrompt)
            || candidate
                .draft
                .changed_surfaces
                .contains(&HarnessSurface::ToolDefinitions);
        self.publish_event(TeaEvent::Harness(HarnessEvent::SnapshotActivated {
            lane_id: lane.lane_id.clone(),
            operation_id: operation_id.clone(),
            previous_revision_id: request.parent_revision_id.clone(),
            revision_id: revision.revision_id.clone(),
            snapshot_id: revision.snapshot_id.clone(),
            provider_surface_changed,
            changed_surfaces: candidate.draft.changed_surfaces.clone(),
        }))?;
        if matches!(revision.reason, HarnessRevisionReason::Rollback) {
            self.publish_event(TeaEvent::Harness(HarnessEvent::RolledBack {
                lane_id: lane.lane_id.clone(),
                from_revision_id: request.parent_revision_id.clone(),
                to_revision_id: revision.revision_id.clone(),
            }))?;
        }
        Ok(revision)
    }

    fn provenance(
        &self,
        lane: &LaneRuntime,
        operation_id: &OperationId,
        epoch_id: &EpochId,
        identity: &HarnessIdentity,
        provider_surface_digest: Option<String>,
    ) -> Result<RunProvenance, HarnessError> {
        let snapshot = self.session_lock()?.snapshot()?;
        let session_id = snapshot.header().session_id.to_string();
        let (core_run_id, source_leaf_id) = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(record)
                    if &record.id == epoch_id && &record.operation_id == operation_id =>
                {
                    Some((
                        record.core_run_id.to_string(),
                        record.source_leaf_id.clone(),
                    ))
                }
                _ => None,
            })
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "epoch {epoch_id} has no durable core-run identity",
                ))
            })?;
        // A child lane has exactly one durable AgentSpawned binding. Trace
        // provenance carries that agent identity only when the graph proves
        // it; root and ordinary scripted lanes intentionally remain `None`.
        let agent_id = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?
            .agents
            .values()
            .find(|node| {
                node.spawned.lane_id == lane.lane_id
                    && node.operation_id.as_ref() == Some(operation_id)
            })
            .map(|node| node.spawned.agent_id.to_string());
        Ok(RunProvenance {
            session_id: Some(session_id),
            lane_id: Some(lane.lane_id.to_string()),
            agent_id,
            operation_id: Some(operation_id.to_string()),
            epoch_id: Some(epoch_id.to_string()),
            core_run_id: Some(core_run_id),
            harness_snapshot_id: Some(identity.snapshot_id.to_string()),
            harness_revision_id: Some(identity.revision_id.to_string()),
            model_harness_profile_id: Some(identity.profile_id.to_string()),
            provider_surface_digest,
            source_leaf_id: source_leaf_id.map(|id| id.to_string()),
            experiment_id: None,
        })
    }

    /// Persist one complete redacted trace before an epoch can receive its
    /// terminal durable record. The trace is evidence, not recovery state:
    /// a conflicting second trace for one durable core run is corruption
    /// rather than an invitation to silently replace historical evidence.
    fn persist_trace_artifact(
        &self,
        operation_id: &OperationId,
        epoch_id: &EpochId,
        identity: &HarnessIdentity,
        provenance: &RunProvenance,
        events: Vec<TraceEvent>,
    ) -> Result<(), HarnessError> {
        if !matches!(events.first(), Some(TraceEvent::EpisodeHeader(_)))
            || !matches!(events.last(), Some(TraceEvent::EpisodeEnd(_)))
        {
            return Err(HarnessError::invalid_state(
                "core trace must contain one header and one terminal episode record",
            ));
        }
        let mut sink = JsonLinesSink::new(Vec::new());
        for event in events {
            sink.append(event)
                .expect("writing a trace event into an in-memory buffer is infallible");
        }
        let bytes = sink.into_inner();
        let artifact = self.artifacts.put(&bytes, "application/x-ndjson")?;
        let core_run_id = provenance
            .core_run_id
            .as_ref()
            .ok_or_else(|| HarnessError::invalid_state("trace provenance has no core-run ID"))?;
        let fact = TraceArtifactFact {
            schema_version: tea_trace::TRACE_SCHEMA_VERSION,
            operation_id: operation_id.clone(),
            epoch_id: epoch_id.clone(),
            core_run_id: CoreRunId::new(core_run_id.clone())
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
            harness_revision_id: identity.revision_id.clone(),
            harness_snapshot_id: identity.snapshot_id.clone(),
            model_harness_profile: identity.profile_id.clone(),
            artifact_id: artifact.artifact_id,
            byte_len: artifact.byte_len,
            media_type: artifact.media_type,
        };
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let existing = snapshot
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                SessionFact::TraceArtifact(existing)
                    if existing.operation_id == fact.operation_id
                        && existing.epoch_id == fact.epoch_id
                        && existing.core_run_id == fact.core_run_id =>
                {
                    Some(existing)
                }
                SessionFact::HarnessCatalog(_)
                | SessionFact::ToolSchemaDeviation(_)
                | SessionFact::SubagentPolicy(_)
                | SessionFact::AgentSpawned(_)
                | SessionFact::WorkspaceDelta(_)
                | SessionFact::AgentTaskFinished(_)
                | SessionFact::WorkspaceDeltaApplied(_)
                | SessionFact::Custom { .. } => None,
                SessionFact::TraceArtifact(_) => None,
            });
        if let Some(existing) = existing {
            if existing == &fact {
                return Ok(());
            }
            return Err(HarnessError::invalid_state(format!(
                "durable core run {} already has a conflicting trace artifact",
                fact.core_run_id
            )));
        }
        session.append_fact(SessionFact::TraceArtifact(fact))?;
        Ok(())
    }

    fn core_messages(
        &self,
        lane: &LaneRuntime,
        configuration: &ResolvedHarness,
        recovery: Option<&RecoveryToolDrive>,
    ) -> Result<Vec<AgentMessage>, HarnessError> {
        let snapshot = self.snapshot()?;
        if let Some(harness_snapshot) = &configuration.harness_snapshot {
            let limits =
                ProviderLimits::new(harness_snapshot.spec.resource_limits.provider_surface_bytes)?;
            return Ok(derive_snapshot_context_with_policies(
                &snapshot,
                lane.lane_id.clone(),
                harness_snapshot,
                limits,
                &configuration.context_policies,
                recovery
                    .map(|recovery| (&recovery.assistant_entry_id, recovery.tool_calls.as_slice())),
            )?
            .messages);
        }
        derive_core_messages(&snapshot, &lane.lane_id)
    }

    /// Invoke only the currently relevant snapshot's process-local resume
    /// callbacks. The resolved registry converts each persisted global stable
    /// hook ID back into that plugin's local ID and never hands a policy data
    /// owned by another registration.
    fn rebuild_lifecycle_state(
        &self,
        lane: &LaneRuntime,
        snapshot: &SessionSnapshot,
    ) -> Result<(), HarnessError> {
        let reduction = reduce_lane(snapshot.clone(), lane.lane_id.clone())?;
        let operation_id = reduction.lane_state.active_operation.ok_or_else(|| {
            HarnessError::invalid_state("main lane has no open operation to recover")
        })?;
        let operation = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::OperationStarted(record) if record.id == operation_id => {
                    Some(record.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "open operation {operation_id} has no durable operation-start record",
                ))
            })?;
        let epoch = open_epoch(snapshot, &operation_id).and_then(|epoch_id| {
            snapshot
                .records()
                .iter()
                .find_map(|stored| match &stored.record {
                    LaneRecord::EpochStarted(record) if record.id == epoch_id => {
                        Some(record.clone())
                    }
                    _ => None,
                })
        });
        let configuration = match &epoch {
            Some(epoch) => {
                let configuration =
                    self.configuration_for_revision(lane, &epoch.harness_revision_id)?;
                if configuration.identity.snapshot_id() != &epoch.harness_snapshot_id
                    || configuration.identity.profile_id() != &epoch.model_harness_profile
                {
                    return Err(HarnessError::invalid_state(format!(
                        "epoch {} immutable revision no longer resolves to its recorded snapshot/profile",
                        epoch.id,
                    )));
                }
                configuration
            }
            None => self.configuration_for_revision(lane, &operation.initial_harness_revision)?,
        };
        configuration.lifecycle.before_resume(
            &operation.operation_resume_data,
            epoch
                .as_ref()
                .map(|epoch| &epoch.epoch_resume_data)
                .unwrap_or(&BTreeMap::new()),
        )
    }

    /// Verify the host-owned conditions for replaying an already-started
    /// effect. A persisted `Safe` bit is necessary but not sufficient: the
    /// currently resolved executable declaration and immutable revision must
    /// still be the exact declaration that was admitted before the crash.
    fn replay_is_still_safe(&self, lane: &LaneRuntime, tool: &ToolStartedRecord) -> bool {
        self.configuration_for_revision(lane, &tool.harness_revision_id)
            .is_ok_and(|configuration| {
                self.host_tools_for_configuration(lane, &configuration, &tool.operation_id)
                    .and_then(|host_tools| {
                        let definitions = all_tool_definition_digests(
                            &lane.runtime_services,
                            &configuration,
                            &host_tools,
                        )?;
                        let current = definitions.get(&tool.tool_name).ok_or_else(|| {
                            HarnessError::invalid_state(format!(
                                "recovery tool {} is no longer declared",
                                tool.tool_name,
                            ))
                        })?;
                        Ok((configuration.is_replay_safe(&tool.tool_name)
                            || replay_safe_host_tools(&host_tools).contains(&tool.tool_name))
                            && tool.harness_revision_id == *configuration.identity.revision_id()
                            && current == &tool.tool_definition_digest)
                    })
                    .unwrap_or(false)
            })
    }

    fn append_interrupted_tool_result(
        &self,
        lane: &LaneRuntime,
        snapshot: &SessionSnapshot,
        result_entry_id: &EntryId,
    ) -> Result<(), HarnessError> {
        let started = recovery_tool_start(snapshot, result_entry_id)?.clone();
        let tool_call_id = ToolCallId::new(started.tool_call_id.clone()).map_err(|error| {
            HarnessError::invalid_state(format!(
                "stored tool invocation has invalid call ID: {error}"
            ))
        })?;
        let result = AgentToolResult {
            tool_call_id,
            content: "Tool execution was interrupted. Tea cannot prove whether the external effect occurred, so it was not replayed.".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: true,
            failure: Some(tea_core::tool::ToolFailure::recoverable()),
        };
        let configuration = self.configuration_for_revision(lane, &started.harness_revision_id)?;
        let retained = retain_tool_result_with_projection(
            self.artifacts.as_ref(),
            configuration.artifact_policy_config(),
            &result,
            &result,
        )
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let entry = tool_result_entry(&result, &result, &started.tool_name, retained)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        self.session_lock()?.append_entry(
            &lane.lane_id,
            ProvisionedEntry {
                id: result_entry_id.clone(),
                body: SessionEntry::ToolResult(entry),
            },
        )?;
        Ok(())
    }

    fn configuration_for_reduction(
        &self,
        lane: &LaneRuntime,
        reduction: &tea_session::LaneReduction,
    ) -> Result<ResolvedHarness, HarnessError> {
        self.configuration_for_reduction_services(&lane.runtime_services, reduction)
    }

    fn configuration_for_reduction_services(
        &self,
        services: &RuntimeServices,
        reduction: &tea_session::LaneReduction,
    ) -> Result<ResolvedHarness, HarnessError> {
        let revision_id = reduction
            .lane_state
            .active_harness_revision
            .as_ref()
            .ok_or_else(|| {
                HarnessError::invalid_state("durable lane has no committed active harness revision")
            })?;
        self.manager.resolve_revision(revision_id, services)
    }

    fn configuration_for_revision(
        &self,
        lane: &LaneRuntime,
        revision_id: &HarnessRevisionId,
    ) -> Result<ResolvedHarness, HarnessError> {
        self.manager
            .resolve_revision(revision_id, &lane.runtime_services)
    }

    fn epoch_configuration(
        &self,
        lane: &LaneRuntime,
        epoch_id: &EpochId,
    ) -> Result<ResolvedHarness, HarnessError> {
        let snapshot = self.snapshot()?;
        let started = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(record) if &record.id == epoch_id => Some(record),
                _ => None,
            })
            .ok_or_else(|| {
                HarnessError::invalid_state(
                    format!("epoch {epoch_id} has no durable start record",),
                )
            })?;
        let configuration = self.configuration_for_revision(lane, &started.harness_revision_id)?;
        if configuration.identity.snapshot_id() != &started.harness_snapshot_id
            || configuration.identity.profile_id() != &started.model_harness_profile
        {
            return Err(HarnessError::invalid_state(format!(
                "epoch {epoch_id} immutable revision no longer resolves to its recorded snapshot/profile",
            )));
        }
        Ok(configuration)
    }
}

impl SessionSupervisor<tea_session::JsonlSession> {
    /// Export a closed immutable durable-session bundle with all harness
    /// source roots. The caller must first let any active operation settle so
    /// no new reference can race the selected snapshot.
    pub fn export_to(
        &self,
        destination: impl AsRef<std::path::Path>,
    ) -> Result<tea_session::SessionExport, HarnessError> {
        self.ensure_idle_for_artifact_gc()?;
        let additional_roots = self.manager.artifact_roots()?;
        self.session_lock()?
            .export_to(destination, additional_roots)
            .map_err(Into::into)
    }
}

fn validate_reserved_host_tool_names(
    runtime_services: &RuntimeServices,
    resolved: &ResolvedHarness,
) -> Result<(), HarnessError> {
    for name in STABLE_ARTIFACT_TOOL_NAMES {
        if runtime_services.trusted_tools().get(name).is_some()
            || resolved.extension_tools().get(name).is_some()
        {
            return Err(HarnessError::invalid_state(format!(
                "harness templates cannot replace the reserved host tool {name}"
            )));
        }
    }
    if runtime_services
        .trusted_tools()
        .get(STABLE_HARNESS_TOOL_NAME)
        .is_some()
        || resolved
            .extension_tools()
            .get(STABLE_HARNESS_TOOL_NAME)
            .is_some()
    {
        return Err(HarnessError::invalid_state(format!(
            "harness templates cannot replace the reserved host tool {STABLE_HARNESS_TOOL_NAME}"
        )));
    }
    Ok(())
}

fn latest_harness_catalog(snapshot: &SessionSnapshot) -> Option<&tea_session::HarnessCatalogFact> {
    snapshot
        .facts()
        .iter()
        .rev()
        .find_map(|stored| match &stored.fact {
            SessionFact::HarnessCatalog(catalog) => Some(catalog),
            SessionFact::ToolSchemaDeviation(_)
            | SessionFact::TraceArtifact(_)
            | SessionFact::SubagentPolicy(_)
            | SessionFact::AgentSpawned(_)
            | SessionFact::WorkspaceDelta(_)
            | SessionFact::AgentTaskFinished(_)
            | SessionFact::WorkspaceDeltaApplied(_)
            | SessionFact::Custom { .. } => None,
        })
}

fn merge_tool_registries(
    target: &mut ToolRegistry,
    additional: ToolRegistry,
) -> Result<(), HarnessError> {
    for name in additional.names().map(str::to_owned).collect::<Vec<_>>() {
        if target.get(&name).is_some() {
            return Err(HarnessError::invalid_state(format!(
                "stable host tool registry has a duplicate capability {name}",
            )));
        }
        let tool = additional
            .get(&name)
            .expect("registered stable host tool remains present")
            .clone();
        target.insert(tool);
    }
    Ok(())
}

fn replay_safe_host_tools(tools: &ToolRegistry) -> BTreeSet<String> {
    STABLE_ARTIFACT_TOOL_NAMES
        .into_iter()
        .chain(std::iter::once(STABLE_HARNESS_TOOL_NAME))
        // `spawn_agent` derives every identity from the already-durable tool
        // intent and completes an append-only, idempotent transaction.  It is
        // the one collaboration effect that must replay after an intent-only
        // crash; apply remains deliberately excluded because an external Git
        // mutation without its terminal fact is ambiguous.
        .chain(std::iter::once("spawn_agent"))
        .filter(|name| tools.get(name).is_some())
        .map(str::to_owned)
        .collect()
}

struct OperationClaim {
    lane: Arc<LaneRuntime>,
}

/// Bridges core's awaited run-local observer boundary into the bounded
/// application fanout. This intentionally has no session writer: core events
/// are observational, while durable state continues to flow through the
/// supervisor's explicit commit procedures.
struct HarnessAgentEventObserver {
    events: Arc<EventHub>,
    lane_id: LaneId,
}

impl EventObserver for HarnessAgentEventObserver {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: tea_core::scheduler::CancellationToken,
    ) -> ObserverFuture<'a> {
        self.events.publish(TeaEvent::Agent {
            lane_id: self.lane_id.clone(),
            event: event.clone(),
        });
        Box::pin(std::future::ready(Ok(())))
    }
}

impl Drop for OperationClaim {
    fn drop(&mut self) {
        self.lane.active.store(false, Ordering::Release);
    }
}

struct RecoveryToolDrive {
    assistant_entry_id: EntryId,
    tool_calls: Vec<AgentToolCall>,
    replay_tool_starts: BTreeMap<(EntryId, u32), ToolStartedRecord>,
}

struct DurableEffectGate<S> {
    runtime: Arc<Mutex<EpochRuntime<S>>>,
}

impl<S> EffectGate for DurableEffectGate<S>
where
    S: SessionWriter + Send + 'static,
{
    fn before<'a>(&'a self, action: EffectAction) -> EffectFuture<'a> {
        let result = self
            .runtime
            .lock()
            .map_err(|_| EffectGateError::new("durable effect state mutex is poisoned"))
            .and_then(|mut runtime| runtime.before(action));
        Box::pin(std::future::ready(result))
    }

    fn after<'a>(&'a self, action: EffectAction, outcome: EffectOutcome) -> EffectFuture<'a> {
        let result = self
            .runtime
            .lock()
            .map_err(|_| EffectGateError::new("durable effect state mutex is poisoned"))
            .and_then(|mut runtime| runtime.after(action, outcome));
        Box::pin(std::future::ready(result))
    }
}

struct EpochRuntime<S> {
    session: Arc<Mutex<S>>,
    artifacts: Arc<dyn ArtifactStore>,
    events: Arc<EventHub>,
    lane: LaneId,
    operation_id: OperationId,
    epoch_id: EpochId,
    identity: HarnessIdentity,
    resolved_harness: ResolvedHarness,
    memory_collector: Arc<ExtensionMemoryCollector>,
    /// Exact declarations for resolved extension capabilities, trusted base
    /// tools, and stable Rust host tools participating in this epoch.
    tool_definition_digests: BTreeMap<String, Digest>,
    /// Canonical schemas retained solely to turn invalid model arguments into
    /// content-free durable evidence. Raw argument bytes never enter this map
    /// or an application event.
    tool_definition_schemas: BTreeMap<String, JsonValue>,
    /// Host-owned read-only/idempotent capabilities permitted to replay after
    /// a crash once their exact declaration is revalidated.
    replay_safe_host_tools: BTreeSet<String>,
    /// Persisted Safe tool intents admitted by a recovery drive. They are
    /// consumed once by `before_tool`, rather than creating a second intent.
    replay_tool_starts: BTreeMap<(EntryId, u32), ToolStartedRecord>,
    pending_providers: BTreeMap<EffectId, PendingProvider>,
    pending_tools: BTreeMap<EffectId, PendingTool>,
    started_tool_indices: BTreeMap<EntryId, BTreeSet<u32>>,
    last_assistant_entry: Option<EntryId>,
    fault: Option<String>,
}

struct PendingProvider {
    request_id: ProviderRequestId,
    result_entry_id: EntryId,
}

#[derive(Clone)]
struct PendingTool {
    result_entry_id: EntryId,
    tool_name: String,
    tool_call_id: String,
}

struct EpochRuntimeInit<S> {
    session: Arc<Mutex<S>>,
    artifacts: Arc<dyn ArtifactStore>,
    events: Arc<EventHub>,
    lane: LaneId,
    operation_id: OperationId,
    epoch_id: EpochId,
    identity: HarnessIdentity,
    resolved_harness: ResolvedHarness,
    memory_collector: Arc<ExtensionMemoryCollector>,
    tool_definition_digests: BTreeMap<String, Digest>,
    tool_definition_schemas: BTreeMap<String, JsonValue>,
    replay_safe_host_tools: BTreeSet<String>,
    last_assistant_entry: Option<EntryId>,
    replay_tool_starts: BTreeMap<(EntryId, u32), ToolStartedRecord>,
}

impl<S> EpochRuntime<S>
where
    S: SessionWriter + Send + 'static,
{
    fn new(init: EpochRuntimeInit<S>) -> Self {
        Self {
            session: init.session,
            artifacts: init.artifacts,
            events: init.events,
            lane: init.lane,
            operation_id: init.operation_id,
            epoch_id: init.epoch_id,
            identity: init.identity,
            resolved_harness: init.resolved_harness,
            memory_collector: init.memory_collector,
            tool_definition_digests: init.tool_definition_digests,
            tool_definition_schemas: init.tool_definition_schemas,
            replay_safe_host_tools: init.replay_safe_host_tools,
            replay_tool_starts: init.replay_tool_starts,
            pending_providers: BTreeMap::new(),
            pending_tools: BTreeMap::new(),
            started_tool_indices: BTreeMap::new(),
            last_assistant_entry: init.last_assistant_entry,
            fault: None,
        }
    }

    fn before(&mut self, action: EffectAction) -> Result<(), EffectGateError> {
        self.ensure_healthy()?;
        match action.subject() {
            EffectSubject::DurableWrite { write } => self.before_durable_write(write),
            EffectSubject::ProviderRequest { request } => {
                self.before_provider(action.id(), request)
            }
            EffectSubject::ToolExecution { call } => self.before_tool(action.id(), call),
            EffectSubject::HookInvocation { hook } => {
                self.append_hook_fact(action.id(), hook, "started", None)
            }
            subject => Err(self.fault(format!(
                "core emitted host-only effect {:?} without a durable supervisor procedure",
                subject.kind()
            ))),
        }
    }

    fn after(
        &mut self,
        action: EffectAction,
        outcome: EffectOutcome,
    ) -> Result<(), EffectGateError> {
        self.ensure_healthy()?;
        match (action.subject(), outcome) {
            (EffectSubject::DurableWrite { .. }, EffectOutcome::DurableWrite(EffectCompletion::Succeeded)) => {
                Ok(())
            }
            (EffectSubject::DurableWrite { .. }, EffectOutcome::DurableWrite(completion)) => {
                Err(self.fault(format!(
                    "core settled a successful durable write with an invalid completion {completion:?}"
                )))
            }
            (EffectSubject::ProviderRequest { .. }, EffectOutcome::ProviderRequest(outcome)) => {
                self.after_provider(action.id(), outcome)
            }
            (EffectSubject::ToolExecution { call }, EffectOutcome::ToolExecution(outcome)) => {
                self.after_tool(action.id(), call, *outcome)
            }
            (EffectSubject::HookInvocation { hook }, EffectOutcome::HookInvocation(outcome)) => {
                self.append_hook_fact(action.id(), hook, "settled", Some(&outcome))
            }
            (subject, outcome) => Err(self.fault(format!(
                "effect subject {:?} settled with mismatched outcome {outcome:?}",
                subject.kind()
            ))),
        }
    }

    fn ensure_healthy(&self) -> Result<(), EffectGateError> {
        match &self.fault {
            Some(message) => Err(EffectGateError::new(message.clone())),
            None => Ok(()),
        }
    }

    fn before_durable_write(&mut self, write: &DurableWriteRequest) -> Result<(), EffectGateError> {
        match write {
            DurableWriteRequest::ToolResult { call, result } => {
                // Immediate schema/policy failures have no ToolExecution
                // settlement and therefore materialize here. Executed tools
                // were already persisted by `after_tool` before the core can
                // emit their end event; this call verifies that exact durable
                // entry instead of writing a second result.
                if result.failure.as_ref().is_some_and(|failure| {
                    failure.disposition() == ToolFailureDisposition::InvalidArguments
                }) {
                    self.persist_tool_schema_deviation(call)?;
                }
                self.persist_tool_result(call, result, result)
            }
        }
    }

    fn before_provider(
        &mut self,
        action_id: EffectId,
        request: &tea_core::scheduler::ModelRequest,
    ) -> Result<(), EffectGateError> {
        let snapshot = self.session_snapshot()?;
        let assistant_attempt = snapshot
            .records()
            .iter()
            .filter(|stored| {
                matches!(
                    &stored.record,
                    LaneRecord::StepAttempted(record)
                        if record.operation_id == self.operation_id
                            && record.epoch_id == self.epoch_id
                            && record.kind == StepKind::Assistant
                )
            })
            .count()
            .saturating_add(1) as u32;
        let step_id = StepId::new(durable_identifier(
            "step-assistant",
            [self.epoch_id.as_str(), &assistant_attempt.to_string()],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        let request_id = ProviderRequestId::new(durable_identifier(
            "provider-request",
            [step_id.as_str(), "1"],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        let result_entry_id =
            EntryId::new(durable_identifier("entry-assistant", [step_id.as_str()]))
                .map_err(|error| self.fault(error.to_string()))?;
        let operation_id = self.operation_id.clone();
        let epoch_id = self.epoch_id.clone();
        let profile_id = self.identity.profile_id.clone();
        let request_surface_digest = provider_request_digest(request);
        self.mutate(|session| {
            session.append_record(LaneRecord::StepAttempted(StepAttemptedRecord {
                id: step_id.clone(),
                operation_id: operation_id.clone(),
                epoch_id: epoch_id.clone(),
                kind: StepKind::Assistant,
                attempt: assistant_attempt,
                result_entry_id: result_entry_id.clone(),
                reason: None,
            }))?;
            session.append_record(LaneRecord::ProviderRequestStarted(
                ProviderRequestStartedRecord {
                    request_id: request_id.clone(),
                    operation_id,
                    epoch_id,
                    step_id,
                    physical_attempt: 1,
                    model_harness_profile: profile_id,
                    request_surface_digest,
                    idempotency_key: None,
                },
            ))?;
            Ok(())
        })?;
        if self
            .pending_providers
            .insert(
                action_id,
                PendingProvider {
                    request_id,
                    result_entry_id,
                },
            )
            .is_some()
        {
            return Err(self.fault("provider effect action ID was admitted twice"));
        }
        Ok(())
    }

    fn after_provider(
        &mut self,
        action_id: EffectId,
        outcome: ProviderEffectOutcome,
    ) -> Result<(), EffectGateError> {
        let pending = self
            .pending_providers
            .remove(&action_id)
            .ok_or_else(|| self.fault("provider settlement has no pending durable intent"))?;
        match outcome {
            ProviderEffectOutcome::Settled(response) => {
                let classification = if response.context_overflow {
                    ProviderSettlementClassification::Discarded
                } else {
                    ProviderSettlementClassification::Completed
                };
                let outcome = provider_outcome_json(&response);
                let assistant = if response.context_overflow {
                    None
                } else {
                    Some(assistant_entry(&response)?)
                };
                let operation_id = self.operation_id.clone();
                let lane = self.lane.clone();
                self.mutate(|session| {
                    session.append_record(LaneRecord::ProviderRequestSettled(
                        ProviderRequestSettledRecord {
                            request_id: pending.request_id.clone(),
                            operation_id: operation_id.clone(),
                            outcome,
                            provider_error: response.provider_error.clone(),
                            usage: response.usage.as_ref().map(core_usage),
                            response_artifact: None,
                            classification,
                        },
                    ))?;
                    if let Some(usage) = response.usage.as_ref() {
                        session.append_record(LaneRecord::Usage(tea_session::UsageRecord {
                            operation_id: operation_id.clone(),
                            request_id: Some(pending.request_id.clone()),
                            usage: core_usage(usage),
                        }))?;
                    }
                    if let Some(assistant) = assistant {
                        session.append_entry(
                            &lane,
                            ProvisionedEntry {
                                id: pending.result_entry_id.clone(),
                                body: SessionEntry::AssistantMessage(assistant),
                            },
                        )?;
                    }
                    Ok(())
                })?;
                if !response.context_overflow {
                    self.last_assistant_entry = Some(pending.result_entry_id);
                }
                Ok(())
            }
            ProviderEffectOutcome::Failed { message } => {
                let operation_id = self.operation_id.clone();
                self.mutate(|session| {
                    session.append_record(LaneRecord::ProviderRequestSettled(
                        ProviderRequestSettledRecord {
                            request_id: pending.request_id,
                            operation_id,
                            outcome: JsonValue::object([
                                ("status", JsonValue::String("interrupted".into())),
                                ("message", JsonValue::String(message)),
                            ]),
                            provider_error: None,
                            usage: None,
                            response_artifact: None,
                            classification: ProviderSettlementClassification::Interrupted,
                        },
                    ))?;
                    Ok(())
                })
            }
        }
    }

    fn before_tool(&mut self, action_id: EffectId, call: &ToolCall) -> Result<(), EffectGateError> {
        let assistant_entry_id = self
            .last_assistant_entry
            .clone()
            .ok_or_else(|| self.fault("tool execution has no durable assistant entry"))?;
        let snapshot = self.session_snapshot()?;
        let assistant = snapshot
            .entries()
            .iter()
            .find(|entry| entry.header.id == assistant_entry_id)
            .and_then(|entry| match &entry.body {
                SessionEntry::AssistantMessage(assistant) => Some(assistant),
                _ => None,
            })
            .ok_or_else(|| self.fault("last assistant entry is missing or has the wrong type"))?;
        let used = self
            .started_tool_indices
            .get(&assistant_entry_id)
            .cloned()
            .unwrap_or_default();
        let tool_index = assistant
            .tool_calls
            .iter()
            .enumerate()
            .find(|(index, candidate)| {
                !used.contains(&(*index as u32))
                    && candidate.id == call.id.as_str()
                    && candidate.name == call.name
            })
            .map(|(index, _)| index as u32)
            .ok_or_else(|| self.fault("tool execution does not match a remaining durable call"))?;
        let effective_args = JsonValue::parse(call.arguments.as_str()).map_err(|error| {
            self.fault(format!("tool arguments cannot enter durable WAL: {error}"))
        })?;
        let definition_digest = self
            .tool_definition_digests
            .get(&call.name)
            .cloned()
            .ok_or_else(|| {
                self.fault(format!(
                    "durable tool intent names unknown executable tool {}",
                    call.name,
                ))
            })?;
        let result_entry_id = EntryId::new(durable_identifier(
            "entry-tool-result",
            [assistant_entry_id.as_str(), &tool_index.to_string()],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        let record_id = tea_session::RecordId::new(durable_identifier(
            "record-tool-start",
            [assistant_entry_id.as_str(), &tool_index.to_string()],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        let idempotency_key = durable_identifier(
            "tool-invocation",
            [assistant_entry_id.as_str(), &tool_index.to_string()],
        );
        let replay = if self.resolved_harness.is_replay_safe(&call.name)
            || self.replay_safe_host_tools.contains(&call.name)
        {
            ToolReplayPolicy::Safe
        } else {
            ToolReplayPolicy::Never
        };
        if let Some(existing) = self
            .replay_tool_starts
            .remove(&(assistant_entry_id.clone(), tool_index))
        {
            if existing.tool_call_id != call.id.as_str()
                || existing.tool_name != call.name
                || existing.replay_policy_at_start != ToolReplayPolicy::Safe
                || replay != ToolReplayPolicy::Safe
                || existing.effective_args != effective_args
                || existing.tool_definition_digest != definition_digest
                || existing.harness_revision_id != self.identity.revision_id
            {
                return Err(self.fault(
                    "recovery tool replay declaration no longer permits the persisted effect",
                ));
            }
            self.started_tool_indices
                .entry(assistant_entry_id)
                .or_default()
                .insert(tool_index);
            if self
                .pending_tools
                .insert(
                    action_id,
                    PendingTool {
                        result_entry_id: existing.result_entry_id,
                        tool_name: existing.tool_name,
                        tool_call_id: existing.tool_call_id,
                    },
                )
                .is_some()
            {
                return Err(self.fault("replayed tool effect action ID was admitted twice"));
            }
            return Ok(());
        }
        let operation_id = self.operation_id.clone();
        let epoch_id = self.epoch_id.clone();
        let revision_id = self.identity.revision_id.clone();
        let call_id = call.id.to_string();
        let call_name = call.name.clone();
        self.mutate(|session| {
            session.append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
                record_id,
                operation_id,
                epoch_id,
                assistant_entry_id.clone(),
                tool_index,
                call_id,
                call_name,
                effective_args,
                result_entry_id.clone(),
                replay,
                definition_digest,
                revision_id,
                idempotency_key,
            )))?;
            Ok(())
        })?;
        self.started_tool_indices
            .entry(assistant_entry_id)
            .or_default()
            .insert(tool_index);
        if self
            .pending_tools
            .insert(
                action_id,
                PendingTool {
                    result_entry_id,
                    tool_name: call.name.clone(),
                    tool_call_id: call.id.to_string(),
                },
            )
            .is_some()
        {
            return Err(self.fault("tool effect action ID was admitted twice"));
        }
        Ok(())
    }

    /// Commit the exact post-policy tool result before core adds its in-memory
    /// message. The result entry identity is a pure function of the assistant
    /// entry and source ordinal, so a restart can distinguish a committed
    /// result from an ambiguous external effect without a mutable pointer.
    fn persist_tool_result(
        &mut self,
        call: &ToolCall,
        raw_result: &AgentToolResult,
        model_result: &AgentToolResult,
    ) -> Result<(), EffectGateError> {
        let assistant_entry_id = self
            .last_assistant_entry
            .clone()
            .ok_or_else(|| self.fault("durable tool result has no assistant entry"))?;
        let snapshot = self.session_snapshot()?;
        let assistant = snapshot
            .entries()
            .iter()
            .find(|entry| entry.header.id == assistant_entry_id)
            .and_then(|entry| match &entry.body {
                SessionEntry::AssistantMessage(assistant) => Some(assistant),
                _ => None,
            })
            .ok_or_else(|| self.fault("durable tool result source assistant is missing"))?;
        let tool_index = assistant
            .tool_calls
            .iter()
            .position(|candidate| candidate.id == call.id.as_str() && candidate.name == call.name)
            .ok_or_else(|| self.fault("durable tool result does not match its source call"))?
            as u32;
        let result_entry_id = EntryId::new(durable_identifier(
            "entry-tool-result",
            [assistant_entry_id.as_str(), &tool_index.to_string()],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        if let Some(existing) = snapshot
            .entries()
            .iter()
            .find(|entry| entry.header.id == result_entry_id)
        {
            return persisted_tool_result_matches(existing, model_result, &call.name)
                .map_err(|message| self.fault(message));
        }
        // A policy that requires pre-persistence redaction makes the
        // post-policy result the sole durable content source. Usage still
        // describes the completed effect below, but raw content never reaches
        // an inline payload or immutable artifact under that policy.
        let durable_result = if self
            .resolved_harness
            .artifact_policy_config()
            .redact_before_persist
        {
            model_result
        } else {
            raw_result
        };
        let retained = if is_direct_recovery_tool(&call.name) {
            retain_direct_recovery_result_with_projection(
                self.resolved_harness.artifact_policy_config(),
                durable_result,
                model_result,
            )
        } else {
            retain_tool_result_with_projection(
                self.artifacts.as_ref(),
                self.resolved_harness.artifact_policy_config(),
                durable_result,
                model_result,
            )
        }
        .map_err(|error| self.fault(error.to_string()))?;
        let entry = tool_result_entry(model_result, raw_result, &call.name, retained)
            .map_err(|error| self.fault(error.to_string()))?;
        let retained_artifact = entry.full_result.artifact_id().map(|artifact_id| {
            let byte_len = match &entry.full_result {
                PayloadRef::Artifact { byte_len, .. } => *byte_len,
                PayloadRef::Inline(_) => unreachable!("artifact ID requires artifact payload"),
            };
            (artifact_id, byte_len, entry.artifact_policy_id.clone())
        });
        let lane = self.lane.clone();
        self.mutate(|session| {
            session.append_entry(
                &lane,
                ProvisionedEntry {
                    id: result_entry_id,
                    body: SessionEntry::ToolResult(entry),
                },
            )?;
            Ok(())
        })?;
        if let Some((artifact_id, byte_len, policy_id)) = retained_artifact {
            self.events
                .publish(TeaEvent::Artifact(ArtifactEvent::Retained {
                    artifact_id,
                    byte_len,
                    policy_id,
                }));
        }
        Ok(())
    }

    /// Retain rejected raw arguments outside model context, then append one
    /// idempotent structured fact before the ordinary invalid tool result is
    /// materialized. This keeps schema evidence searchable after a crash
    /// without allowing malformed calls to acquire a `ToolStarted` effect
    /// intent.
    fn persist_tool_schema_deviation(&mut self, call: &ToolCall) -> Result<(), EffectGateError> {
        if self
            .resolved_harness
            .artifact_policy_config()
            .redact_before_persist
        {
            return Err(self.fault(
                "schema-deviation raw argument capture requires an installed host redactor when the artifact policy requires redaction",
            ));
        }
        let assistant_entry_id = self.last_assistant_entry.clone().ok_or_else(|| {
            self.fault("schema-deviation evidence has no durable assistant entry")
        })?;
        // Clone the canonical declaration before a fallible durable write.
        // `self.fault` needs mutable access to the epoch, so retaining a map
        // borrow across the artifact write would otherwise make the fault
        // boundary impossible to express.
        let Some(schema) = self.tool_definition_schemas.get(&call.name).cloned() else {
            return Err(self.fault(format!(
                "schema-deviation evidence names unknown executable tool {}",
                call.name
            )));
        };
        let raw_bytes = call.arguments.as_str().as_bytes();
        let parsed_arguments = JsonValue::parse(call.arguments.as_str());
        let descriptor = self
            .artifacts
            .put(
                raw_bytes,
                if parsed_arguments.is_ok() {
                    "application/json"
                } else {
                    "text/plain"
                },
            )
            .map_err(|error| self.fault(error.to_string()))?;
        let (arguments_valid_json, unknown_fields, missing_fields, type_mismatches) =
            match parsed_arguments {
                Ok(arguments) => inspect_tool_schema_deviation(
                    self.identity.profile_id.clone(),
                    call.name.clone(),
                    &schema,
                    &arguments,
                    descriptor.artifact_id,
                )
                .map_err(|error| self.fault(error.to_string()))?
                .map(|deviation| {
                    (
                        true,
                        deviation.unknown_fields,
                        deviation.missing_fields,
                        deviation
                            .type_mismatches
                            .into_iter()
                            .map(|mismatch| SchemaFieldMismatch {
                                field: mismatch.field,
                                expected: mismatch.expected,
                                actual: mismatch.actual,
                            })
                            .collect(),
                    )
                })
                .unwrap_or_else(|| (true, Vec::new(), Vec::new(), Vec::new())),
                Err(_) => (false, Vec::new(), Vec::new(), Vec::new()),
            };
        let fact = ToolSchemaDeviationFact {
            operation_id: self.operation_id.clone(),
            epoch_id: self.epoch_id.clone(),
            assistant_entry_id,
            tool_call_id: call.id.to_string(),
            tool_name: call.name.clone(),
            model_harness_profile: self.identity.profile_id.clone(),
            arguments_valid_json,
            unknown_fields,
            missing_fields,
            type_mismatches,
            raw_arguments: PayloadRef::Artifact {
                artifact_id: descriptor.artifact_id,
                byte_len: descriptor.byte_len,
                media_type: descriptor.media_type,
            },
        };
        let already_persisted = self
            .session_snapshot()?
            .facts()
            .iter()
            .any(|stored| stored.fact == SessionFact::ToolSchemaDeviation(fact.clone()));
        if !already_persisted {
            self.mutate(|session| {
                session.append_fact(SessionFact::ToolSchemaDeviation(fact))?;
                Ok(())
            })?;
            self.events
                .publish(TeaEvent::Artifact(ArtifactEvent::Retained {
                    artifact_id: descriptor.artifact_id,
                    byte_len: descriptor.byte_len,
                    policy_id: self
                        .resolved_harness
                        .artifact_policy_config()
                        .policy_id
                        .clone(),
                }));
        }
        Ok(())
    }

    /// Append typed policy memory only after the paired raw tool result has
    /// committed. The collector is process-local and keyed by the completed
    /// tool-call identity; it has no route to create a semantic parent or
    /// bypass the session writer.
    fn persist_plugin_memory(
        &mut self,
        pending: &PendingTool,
        call: &ToolCall,
    ) -> Result<(), EffectGateError> {
        let proposals = self
            .memory_collector
            .take_for_call(call.id.as_str())
            .map_err(|error| {
                self.fault(format!(
                    "cannot consume post-tool memory proposals: {error}"
                ))
            })?;
        if proposals.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::with_capacity(proposals.len());
        for (index, collected) in proposals.into_iter().enumerate() {
            let index = index.to_string();
            let id = EntryId::new(durable_identifier(
                "entry-plugin-memory",
                [
                    pending.result_entry_id.as_str(),
                    collected.extension_id.as_str(),
                    collected.proposal.kind.as_str(),
                    index.as_str(),
                ],
            ))
            .map_err(|error| self.fault(error.to_string()))?;
            entries.push((id, plugin_memory_entry(collected)?));
        }
        let lane = self.lane.clone();
        self.mutate(|session| {
            for (id, entry) in entries {
                session.append_entry(
                    &lane,
                    ProvisionedEntry {
                        id,
                        body: SessionEntry::PluginMemory(entry),
                    },
                )?;
            }
            Ok(())
        })
    }

    fn after_tool(
        &mut self,
        action_id: EffectId,
        call: &ToolCall,
        outcome: ToolEffectOutcome,
    ) -> Result<(), EffectGateError> {
        let pending = self
            .pending_tools
            .get(&action_id)
            .cloned()
            .ok_or_else(|| self.fault("tool settlement has no pending durable intent"))?;
        if outcome.result.tool_call_id.as_str().is_empty()
            || outcome.result.tool_call_id.as_str() != pending.tool_call_id
            || outcome.raw_result.tool_call_id.as_str() != pending.tool_call_id
            || call.id.as_str() != pending.tool_call_id
            || call.name != pending.tool_name
            || pending.tool_name.is_empty()
            || pending.result_entry_id.as_str().is_empty()
        {
            return Err(self.fault("tool settlement does not match its durable effect intent"));
        }
        self.persist_tool_result(call, &outcome.raw_result, &outcome.result)?;
        self.persist_plugin_memory(&pending, call)?;
        self.pending_tools.remove(&action_id);
        Ok(())
    }

    fn append_hook_fact(
        &mut self,
        action_id: EffectId,
        hook: &HookInvocation,
        phase: &str,
        outcome: Option<&HookEffectOutcome>,
    ) -> Result<(), EffectGateError> {
        let mut fields = vec![
            (
                "operation_id",
                JsonValue::String(self.operation_id.to_string()),
            ),
            ("epoch_id", JsonValue::String(self.epoch_id.to_string())),
            ("effect_id", JsonValue::String(action_id.0.to_string())),
            ("phase", JsonValue::String(phase.into())),
            ("hook", JsonValue::String(hook_label(hook))),
        ];
        if let Some(outcome) = outcome {
            fields.push(("outcome", hook_outcome_json(outcome)));
        }
        self.mutate(|session| {
            session.append_fact(SessionFact::Custom {
                type_name: "tea.hook-effect.v1".into(),
                payload: JsonValue::object(fields),
            })?;
            Ok(())
        })
    }

    fn session_snapshot(&mut self) -> Result<SessionSnapshot, EffectGateError> {
        self.mutate(|session| session.snapshot())
    }

    fn mutate<T>(
        &mut self,
        mutation: impl FnOnce(&mut S) -> Result<T, tea_session::SessionError>,
    ) -> Result<T, EffectGateError> {
        self.ensure_healthy()?;
        let result = self
            .session
            .lock()
            .map_err(|_| EffectGateError::new("durable session mutex is poisoned"))
            .and_then(|mut session| {
                mutation(&mut *session).map_err(|error| EffectGateError::new(error.to_string()))
            });
        if let Err(error) = &result {
            self.fault = Some(error.to_string());
        }
        result
    }

    fn fault(&mut self, message: impl Into<String>) -> EffectGateError {
        let message = message.into();
        self.fault = Some(message.clone());
        EffectGateError::new(message)
    }
}

pub(crate) fn durable_identifier<'a>(
    kind: &str,
    values: impl IntoIterator<Item = &'a str>,
) -> String {
    let mut writer = CanonicalHashWriter::new("tea-harness-durable-id-v1", 1, 1);
    writer.string("kind", kind);
    let values = values.into_iter().collect::<Vec<_>>();
    writer.u64("value_count", values.len() as u64);
    for (index, value) in values.into_iter().enumerate() {
        writer.string(&format!("value_{index}"), value);
    }
    format!("{kind}-{}", writer.finish().to_hex())
}

fn session_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn provider_request_digest(request: &tea_core::scheduler::ModelRequest) -> Digest {
    let mut writer = CanonicalHashWriter::new("tea-provider-request-surface-v1", 1, 1);
    writer.string("system_prompt", &request.system_prompt);
    writer.string("context", &request.context);
    writer.discriminant(
        "thinking_level",
        thinking_discriminant(request.thinking_level),
    );
    match &request.model {
        Some(model) => {
            writer.boolean("has_model", true);
            writer.string("model_provider", &model.provider);
            writer.string("model_name", &model.model);
            writer.boolean("has_model_revision", model.revision.is_some());
            if let Some(revision) = &model.revision {
                writer.string("model_revision", revision);
            }
        }
        None => writer.boolean("has_model", false),
    }
    writer.u64("tool_count", request.tools.len() as u64);
    for (index, tool) in request.tools.iter().enumerate() {
        writer.u64("tool_index", index as u64);
        writer.string("tool_name", &tool.name);
        writer.string("tool_description", &tool.description);
        writer.string(
            "tool_schema",
            &tool
                .schema
                .to_json_string()
                .expect("protocol JSON values always encode canonically"),
        );
        writer.discriminant(
            "tool_execution_mode",
            match tool.execution_mode {
                tea_core::tool::ToolExecutionMode::Sequential => 1,
                tea_core::tool::ToolExecutionMode::Parallel => 2,
            },
        );
    }
    writer.finish()
}

fn all_tool_definition_digests(
    runtime_services: &RuntimeServices,
    resolved: &ResolvedHarness,
    host_tools: &ToolRegistry,
) -> Result<BTreeMap<String, Digest>, HarnessError> {
    let mut digests = BTreeMap::new();
    for registry in [
        runtime_services.trusted_tools(),
        resolved.extension_tools(),
        host_tools,
    ] {
        for name in registry.names() {
            let tool = registry
                .get(name)
                .expect("registered executable tool remains present");
            let digest = tool_definition_digest(tool.as_ref())?;
            if digests.insert(name.to_owned(), digest).is_some() {
                return Err(HarnessError::invalid_state(format!(
                    "resolved or trusted runtime tool declarations both declare {name}",
                )));
            }
        }
    }
    Ok(digests)
}

fn all_tool_definition_schemas(
    runtime_services: &RuntimeServices,
    resolved: &ResolvedHarness,
    host_tools: &ToolRegistry,
) -> Result<BTreeMap<String, JsonValue>, HarnessError> {
    let mut schemas = BTreeMap::new();
    for registry in [
        runtime_services.trusted_tools(),
        resolved.extension_tools(),
        host_tools,
    ] {
        for name in registry.names() {
            let tool = registry
                .get(name)
                .expect("registered executable tool remains present");
            if schemas
                .insert(name.to_owned(), tool.schema().clone())
                .is_some()
            {
                return Err(HarnessError::invalid_state(format!(
                    "resolved or trusted runtime tool declarations both declare {name}",
                )));
            }
        }
    }
    Ok(schemas)
}

pub(super) fn tool_definition_digest(tool: &dyn AgentTool) -> Result<Digest, HarnessError> {
    let mut writer = CanonicalHashWriter::new("tea-tool-definition-v2", 2, 1);
    writer.string("name", tool.name());
    writer.string("description", tool.description());
    writer.string(
        "schema",
        &tool
            .schema()
            .to_json_string()
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
    );
    writer.discriminant(
        "execution_mode",
        match tool.execution_mode() {
            tea_core::tool::ToolExecutionMode::Sequential => 1,
            tea_core::tool::ToolExecutionMode::Parallel => 2,
        },
    );
    writer.boolean("requires_exclusive_batch", tool.requires_exclusive_batch());
    writer.discriminant(
        "cancellation_settlement_mode",
        match tool.cancellation_settlement_mode() {
            tea_core::tool::CancellationSettlementMode::DropFuture => 1,
            tea_core::tool::CancellationSettlementMode::AwaitFuture => 2,
        },
    );
    Ok(writer.finish())
}

fn assistant_entry(
    response: &ProviderResponse,
) -> Result<tea_session::AssistantMessageEntry, EffectGateError> {
    let tool_calls = response
        .tool_calls
        .iter()
        .map(|call| {
            Ok(tea_session::AssistantToolCall::new(
                call.id.as_str(),
                call.name.clone(),
                JsonValue::parse(call.arguments.as_str()).map_err(|error| {
                    EffectGateError::new(format!(
                        "provider tool call {} has non-JSON durable arguments: {error}",
                        call.id
                    ))
                })?,
            ))
        })
        .collect::<Result<Vec<_>, EffectGateError>>()?;
    Ok(tea_session::AssistantMessageEntry {
        content: response.assistant_text.clone(),
        tool_calls,
        stop_reason: Some(stop_reason_label(response.stop_reason).into()),
        error_message: response.error_message.clone(),
        metadata: BTreeMap::new(),
    })
}

fn tool_result_entry(
    model_result: &AgentToolResult,
    raw_result: &AgentToolResult,
    tool_name: &str,
    retained: RetainedToolResult,
) -> Result<ToolResultEntry, EffectGateError> {
    Ok(ToolResultEntry {
        tool_call_id: model_result.tool_call_id.to_string(),
        tool_name: tool_name.into(),
        full_result: retained.full_result,
        model_projection: retained.model_projection,
        is_error: model_result.is_error,
        terminate: model_result.terminate,
        // Usage describes the already completed capability effect and stays
        // attached to raw evidence rather than a policy's model projection.
        usage: raw_result
            .usage
            .as_ref()
            .map(core_usage)
            .unwrap_or_default(),
        projection_strategy_id: retained.projection_strategy_id,
        artifact_policy_id: retained.artifact_policy_id,
    })
}

/// Validate the host-independent portion of a policy proposal before Rust
/// gives it a durable semantic position. Parsing already rejects malformed
/// source output; repeating the bounded checks here protects this critical
/// boundary if another trusted in-process adapter constructs a collector
/// value in the future.
fn plugin_memory_entry(
    collected: CollectedExtensionMemoryProposal,
) -> Result<PluginMemoryEntry, EffectGateError> {
    let proposal = collected.proposal;
    if !portable_memory_label(&collected.extension_id) || !portable_memory_label(&proposal.kind) {
        return Err(EffectGateError::new(
            "post-tool memory proposal has an invalid plugin ID or kind",
        ));
    }
    let encoded = proposal.content.to_json_string().map_err(|error| {
        EffectGateError::new(format!("cannot encode post-tool memory: {error}"))
    })?;
    if encoded.len() > 16 * 1024 {
        return Err(EffectGateError::new(
            "post-tool memory proposal exceeds the 16384 byte inline limit",
        ));
    }
    if proposal.provenance.len() > 32
        || proposal.provenance.iter().any(|value| {
            value.is_empty() || value.len() > 200 || value.chars().any(char::is_control)
        })
    {
        return Err(EffectGateError::new(
            "post-tool memory proposal has invalid provenance values",
        ));
    }
    Ok(PluginMemoryEntry {
        plugin_id: collected.extension_id,
        kind: proposal.kind,
        content: PayloadRef::Inline(proposal.content),
        provenance: proposal.provenance,
        visibility: match proposal.visibility {
            ExtensionMemoryVisibility::ModelVisible => MemoryVisibility::ModelVisible,
            ExtensionMemoryVisibility::ExternalOnly => MemoryVisibility::ExternalOnly,
        },
        retention: match proposal.retention {
            ExtensionMemoryRetention::Session => MemoryRetention::Session,
            ExtensionMemoryRetention::Checkpoint => MemoryRetention::Checkpoint,
        },
    })
}

fn portable_memory_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 120
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn persisted_tool_result_matches(
    entry: &tea_session::StoredEntry,
    model_result: &AgentToolResult,
    tool_name: &str,
) -> Result<(), String> {
    let SessionEntry::ToolResult(stored) = &entry.body else {
        return Err(
            "durable tool-result identity was materialized with another semantic type".into(),
        );
    };
    if stored.tool_call_id != model_result.tool_call_id.as_str()
        || stored.tool_name != tool_name
        || stored.is_error != model_result.is_error
        || stored.terminate != model_result.terminate
    {
        return Err(
            "durable tool-result identity was materialized with a different tool settlement".into(),
        );
    }
    Ok(())
}

fn core_usage(usage: &tea_core::state::Usage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost: usage.cost.clone(),
    }
}

fn provider_outcome_json(response: &ProviderResponse) -> JsonValue {
    JsonValue::object([
        ("status", JsonValue::String("settled".into())),
        (
            "stop_reason",
            JsonValue::String(stop_reason_label(response.stop_reason).into()),
        ),
        (
            "context_overflow",
            JsonValue::Bool(response.context_overflow),
        ),
    ])
}

fn hook_label(hook: &HookInvocation) -> String {
    match hook {
        HookInvocation::BeforeTool {
            tool_call_id,
            tool_name,
        } => format!("before_tool:{tool_name}:{tool_call_id}"),
        HookInvocation::AfterTool {
            tool_call_id,
            tool_name,
        } => format!("after_tool:{tool_name}:{tool_call_id}"),
        HookInvocation::TransformContext => "transform_context".into(),
        HookInvocation::ConvertToLlm => "convert_to_llm".into(),
        HookInvocation::PrepareNextTurn => "prepare_next_turn".into(),
        HookInvocation::ShouldStopAfterTurn => "should_stop_after_turn".into(),
    }
}

fn hook_outcome_json(outcome: &HookEffectOutcome) -> JsonValue {
    match outcome {
        HookEffectOutcome::Succeeded => JsonValue::String("succeeded".into()),
        HookEffectOutcome::Failed { message } => JsonValue::object([
            ("status", JsonValue::String("failed".into())),
            ("message", JsonValue::String(message.clone())),
        ]),
    }
}

fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::ToolUse => "tool_use",
        StopReason::Length => "length",
        StopReason::Aborted => "aborted",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}

fn is_direct_recovery_tool(name: &str) -> bool {
    matches!(
        name,
        "tea_artifact_read" | "tea_artifact_search" | "tea_history_search"
    )
}

fn parse_stop_reason(value: &str) -> Result<StopReason, HarnessError> {
    match value {
        "stop" => Ok(StopReason::Stop),
        "tool_use" => Ok(StopReason::ToolUse),
        "length" => Ok(StopReason::Length),
        "aborted" => Ok(StopReason::Aborted),
        "cancelled" => Ok(StopReason::Cancelled),
        "error" => Ok(StopReason::Error),
        _ => Err(HarnessError::invalid_state(format!(
            "durable assistant entry has unknown stop reason {value:?}"
        ))),
    }
}

fn thinking_discriminant(level: ThinkingLevel) -> u16 {
    match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1,
        ThinkingLevel::Low => 2,
        ThinkingLevel::Medium => 3,
        ThinkingLevel::High => 4,
        ThinkingLevel::XHigh => 5,
        ThinkingLevel::Max => 6,
    }
}

fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn thinking_level_from_name(value: &str) -> Result<ThinkingLevel, HarnessError> {
    match value {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(HarnessError::invalid_state(
            "durable thinking level is not one documented value",
        )),
    }
}

fn child_operation_outcome(
    snapshot: &SessionSnapshot,
    operation_id: &OperationId,
) -> Option<OperationOutcome> {
    snapshot
        .records()
        .iter()
        .rev()
        .find_map(|stored| match &stored.record {
            LaneRecord::OperationFinished(record) if &record.operation_id == operation_id => {
                Some(record.outcome.clone())
            }
            _ => None,
        })
}

fn core_failure_code(error: &CoreError) -> &'static str {
    match error {
        CoreError::Cancelled => "cancelled",
        CoreError::ModelError { .. } => "model_error",
        CoreError::ModelAborted { .. } => "model_aborted",
        CoreError::ModelProvider { .. } => "provider_error",
        CoreError::ToolCircuitBreaker { .. } => "tool_circuit_breaker",
        CoreError::Hook(_) => "hook_error",
        CoreError::EffectGate(_) => "effect_gate_error",
        _ => "core_error",
    }
}

impl<S> ExtensionStateStore for SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    fn read_extension_state(
        &self,
        extension_id: &str,
    ) -> Result<ExtensionStateView, ExtensionError> {
        let snapshot = self
            .snapshot()
            .map_err(|error| ExtensionError::new(error.to_string()))?;
        let lane = self
            .root_lane()
            .map_err(|error| ExtensionError::new(error.to_string()))?;
        extension_state_view(&snapshot, &lane.lane_id, extension_id)
            .map_err(|error| ExtensionError::new(error.to_string()))
    }

    fn append_extension_state(
        &self,
        extension_id: &str,
        update: ExtensionStateUpdate,
    ) -> Result<(), ExtensionError> {
        let lane = self
            .root_lane()
            .map_err(|error| ExtensionError::new(error.to_string()))?;
        self.append_extension_state_update(&lane, extension_id, update)
            .map_err(|error| ExtensionError::new(error.to_string()))
    }
}

fn extension_state_view(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
    extension_id: &str,
) -> Result<ExtensionStateView, HarnessError> {
    if !portable_extension_label(extension_id) {
        return Err(HarnessError::invalid_state(
            "extension state namespace must use a portable extension ID",
        ));
    }
    let mut latest = BTreeMap::new();
    for entry in active_branch_entries(snapshot, lane_id)? {
        let SessionEntry::PluginMemory(memory) = entry.body else {
            continue;
        };
        if memory.plugin_id != extension_id {
            continue;
        }
        let PayloadRef::Inline(content) = memory.content else {
            continue;
        };
        latest.insert(memory.kind, content);
    }
    Ok(ExtensionStateView { latest })
}

fn active_branch_entries(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
) -> Result<Vec<tea_session::StoredEntry>, HarnessError> {
    let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
    let by_id = snapshot
        .entries()
        .iter()
        .map(|entry| (entry.header.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cursor = reduction.lane_state.leaf_id;
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            return Err(HarnessError::invalid_state(format!(
                "extension state branch contains a parent cycle at entry {id}",
            )));
        }
        let entry = by_id.get(&id).ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "extension state branch refers to missing entry {id}",
            ))
        })?;
        cursor = entry.header.parent_id.clone();
        chain.push((*entry).clone());
    }
    chain.reverse();
    Ok(chain)
}

fn validate_extension_state_update(
    extension_id: &str,
    update: &ExtensionStateUpdate,
) -> Result<(), HarnessError> {
    if !portable_extension_label(extension_id) || !portable_extension_label(&update.kind) {
        return Err(HarnessError::invalid_state(
            "extension state update requires portable extension ID and kind",
        ));
    }
    let bytes = update
        .content
        .to_json_string()
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
    if bytes.len() > 16 * 1024 {
        return Err(HarnessError::invalid_state(
            "extension state update content exceeds 16384 bytes",
        ));
    }
    Ok(())
}

fn portable_extension_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 120
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn terminal_operation(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
) -> Option<(OperationId, OperationOutcome, u64, u64)> {
    let finished = snapshot
        .records()
        .iter()
        .rev()
        .find_map(|stored| match &stored.record {
            LaneRecord::OperationFinished(record)
                if operation_lane(snapshot, &record.operation_id).as_ref() == Some(lane_id) =>
            {
                Some((
                    record.operation_id.clone(),
                    record.outcome.clone(),
                    stored.timestamp_ms,
                ))
            }
            _ => None,
        })?;
    let started_at_ms = snapshot
        .records()
        .iter()
        .find_map(|stored| match &stored.record {
            LaneRecord::OperationStarted(record) if record.id == finished.0 => {
                Some(stored.timestamp_ms)
            }
            _ => None,
        })?;
    Some((finished.0, finished.1, started_at_ms, finished.2))
}

fn operation_lane(snapshot: &SessionSnapshot, operation_id: &OperationId) -> Option<LaneId> {
    snapshot
        .records()
        .iter()
        .find_map(|stored| match &stored.record {
            LaneRecord::OperationStarted(record) if &record.id == operation_id => {
                Some(record.lane_id.clone())
            }
            _ => None,
        })
}

fn idle_operation_is_claimed(
    snapshot: &SessionSnapshot,
    operation_id: &OperationId,
) -> Result<bool, HarnessError> {
    for entry in snapshot.entries() {
        let SessionEntry::PluginMemory(memory) = &entry.body else {
            continue;
        };
        if memory.plugin_id != "tea.extension.runtime" || memory.kind != "idle.evaluated.v1" {
            continue;
        }
        let PayloadRef::Inline(content) = &memory.content else {
            return Err(HarnessError::invalid_state(
                "extension idle claim must be inline",
            ));
        };
        let claimed = content
            .as_object()
            .and_then(|object| object.get("operation_id"))
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                HarnessError::invalid_state("extension idle claim is missing operation_id")
            })?;
        if claimed == operation_id.as_str() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn operation_usage(snapshot: &SessionSnapshot, operation_id: &OperationId) -> Usage {
    let mut total = Usage::default();
    for stored in snapshot.records() {
        let LaneRecord::Usage(record) = &stored.record else {
            continue;
        };
        if &record.operation_id != operation_id {
            continue;
        }
        accumulate_usage(&mut total, &record.usage);
    }
    total
}

fn accumulate_usage(total: &mut Usage, next: &Usage) {
    for (total, next) in [
        (&mut total.input_tokens, next.input_tokens),
        (&mut total.output_tokens, next.output_tokens),
        (&mut total.reasoning_tokens, next.reasoning_tokens),
        (&mut total.cache_read_tokens, next.cache_read_tokens),
        (&mut total.cache_write_tokens, next.cache_write_tokens),
    ] {
        if let Some(next) = next {
            *total = Some(total.unwrap_or(0).saturating_add(next));
        }
    }
}

fn extension_operation_outcome(outcome: &OperationOutcome) -> ExtensionOperationOutcome {
    match outcome {
        OperationOutcome::Completed => ExtensionOperationOutcome::Completed,
        OperationOutcome::Aborted => ExtensionOperationOutcome::Aborted,
        OperationOutcome::Failed { code } => {
            ExtensionOperationOutcome::Failed { code: code.clone() }
        }
    }
}

fn operation_outcome_name(outcome: &OperationOutcome) -> &'static str {
    match outcome {
        OperationOutcome::Completed => "completed",
        OperationOutcome::Aborted => "aborted",
        OperationOutcome::Failed { .. } => "failed",
    }
}

fn subagent_host_stage_error(
    recovering: bool,
    agent_id: &AgentId,
    stage: SubagentRecoveryStage,
    error: super::subagents::SubagentHostError,
) -> HarnessError {
    if recovering {
        HarnessError::SubagentRecovery {
            agent_id: agent_id.clone(),
            stage,
        }
    } else {
        HarnessError::invalid_state(error.to_string())
    }
}

fn extension_continuation_input(
    snapshot: &SessionSnapshot,
    operation_id: &OperationId,
) -> Result<Option<String>, HarnessError> {
    let record = snapshot
        .records()
        .iter()
        .find_map(|stored| match &stored.record {
            LaneRecord::OperationStarted(record) if &record.id == operation_id => Some(record),
            _ => None,
        });
    let Some(record) = record else {
        return Err(HarnessError::invalid_state(format!(
            "operation {operation_id} has no durable operation-start record",
        )));
    };
    let mut input = None;
    for entry in &record.original_input {
        let SessionEntry::PluginMemory(memory) = &entry.body else {
            continue;
        };
        if memory.kind != "extension.continuation.v1" {
            continue;
        }
        let PayloadRef::Inline(content) = &memory.content else {
            return Err(HarnessError::invalid_state(
                "extension continuation input must be inline",
            ));
        };
        let value = content
            .as_object()
            .and_then(|object| object.get("input"))
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                HarnessError::invalid_state(
                    "extension continuation input is missing its bounded text",
                )
            })?;
        if input.replace(value.to_owned()).is_some() {
            return Err(HarnessError::invalid_state(
                "extension operation contains more than one continuation input",
            ));
        }
    }
    Ok(input)
}

fn extension_error(error: ExtensionError) -> HarnessError {
    HarnessError::invalid_state(format!("extension boundary rejected a value: {error}"))
}

fn derive_core_messages(
    snapshot: &SessionSnapshot,
    lane: &LaneId,
) -> Result<Vec<AgentMessage>, HarnessError> {
    let mut messages = Vec::new();
    for entry in snapshot
        .entries()
        .iter()
        .filter(|entry| &entry.lane_id == lane)
    {
        let message_id = MessageId(messages.len() as u64 + 1);
        match &entry.body {
            SessionEntry::UserMessage(user) => messages.push(AgentMessage::User {
                id: message_id,
                content: user.content.clone(),
            }),
            SessionEntry::AssistantMessage(assistant) => {
                let tool_calls = assistant
                    .tool_calls
                    .iter()
                    .map(|call| {
                        Ok(AgentToolCall {
                            id: ToolCallId::new(call.id.clone()).map_err(|error| {
                                HarnessError::invalid_state(format!(
                                    "durable assistant tool-call ID is invalid: {error}"
                                ))
                            })?,
                            name: call.name.clone(),
                            arguments: SerializedJson::new(
                                call.arguments.to_json_string().map_err(|error| {
                                    HarnessError::invalid_state(format!(
                                        "durable assistant arguments cannot encode: {error}"
                                    ))
                                })?,
                            ),
                        })
                    })
                    .collect::<Result<Vec<_>, HarnessError>>()?;
                let stop_reason = assistant
                    .stop_reason
                    .as_deref()
                    .map(parse_stop_reason)
                    .transpose()?;
                messages.push(AgentMessage::Assistant {
                    id: message_id,
                    content: assistant.content.clone(),
                    tool_calls,
                    stop_reason,
                    error_message: assistant.error_message.clone(),
                });
            }
            SessionEntry::ToolResult(result) => {
                let (content, details) = projection_content(&result.model_projection)
                    .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
                messages.push(AgentMessage::ToolResult {
                    id: message_id,
                    tool_call_id: ToolCallId::new(result.tool_call_id.clone()).map_err(
                        |error| {
                            HarnessError::invalid_state(format!(
                                "durable tool-result call ID is invalid: {error}"
                            ))
                        },
                    )?,
                    tool_name: result.tool_name.clone(),
                    content,
                    details: details.map(SerializedJson::new),
                    usage: Box::new(Some(tea_core::state::Usage {
                        input_tokens: result.usage.input_tokens,
                        output_tokens: result.usage.output_tokens,
                        reasoning_tokens: result.usage.reasoning_tokens,
                        cache_read_tokens: result.usage.cache_read_tokens,
                        cache_write_tokens: result.usage.cache_write_tokens,
                        cost: result.usage.cost.clone(),
                    })),
                    added_tool_names: Vec::new(),
                    terminate: result.terminate,
                    is_error: result.is_error,
                    failure: None,
                });
            }
            SessionEntry::ModelChanged(_)
            | SessionEntry::ThinkingChanged(_)
            | SessionEntry::ToolActivationChanged(_)
            | SessionEntry::HarnessRevisionChanged(_) => {}
            SessionEntry::PluginMemory(tea_session::PluginMemoryEntry {
                plugin_id,
                kind,
                content: PayloadRef::Inline(content),
                visibility: MemoryVisibility::ModelVisible,
                ..
            }) => {
                let content = content.to_json_string().map_err(|error| {
                    HarnessError::invalid_state(format!(
                        "model-visible plugin memory {}:{} cannot encode: {error}",
                        plugin_id, kind,
                    ))
                })?;
                messages.push(AgentMessage::User {
                    id: message_id,
                    content: format!("[Plugin memory {plugin_id}:{kind}]\n{content}"),
                });
            }
            SessionEntry::PluginMemory(tea_session::PluginMemoryEntry {
                visibility: MemoryVisibility::ModelVisible,
                content: PayloadRef::Artifact { .. },
                ..
            })
            | SessionEntry::Compaction(_)
            | SessionEntry::BranchSummary(_)
            | SessionEntry::Custom(tea_session::CustomEntry {
                model_visible: true,
                ..
            }) => {
                return Err(HarnessError::invalid_state(format!(
                    "model-visible durable entry {} requires a harness context derivation policy",
                    entry.header.id
                )));
            }
            SessionEntry::PluginMemory(_) | SessionEntry::Custom(_) => {}
        }
    }
    Ok(messages)
}

fn recovery_tool_start<'a>(
    snapshot: &'a SessionSnapshot,
    result_entry_id: &EntryId,
) -> Result<&'a ToolStartedRecord, HarnessError> {
    snapshot
        .records()
        .iter()
        .find_map(|stored| match &stored.record {
            LaneRecord::ToolStarted(started) if &started.result_entry_id == result_entry_id => {
                Some(started)
            }
            _ => None,
        })
        .ok_or_else(|| {
            HarnessError::invalid_state(
                "recovery requested an interrupted tool result with no durable intent",
            )
        })
}

fn open_epoch(snapshot: &SessionSnapshot, operation_id: &OperationId) -> Option<EpochId> {
    let finished = snapshot
        .records()
        .iter()
        .filter_map(|stored| match &stored.record {
            LaneRecord::EpochFinished(record) if &record.operation_id == operation_id => {
                Some(record.epoch_id.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    snapshot
        .records()
        .iter()
        .rev()
        .find_map(|stored| match &stored.record {
            LaneRecord::EpochStarted(record)
                if &record.operation_id == operation_id && !finished.contains(&record.id) =>
            {
                Some(record.id.clone())
            }
            _ => None,
        })
}

/// Validate paths supplied by the workspace host before they become a
/// model-facing conflict result. Durable delta paths were validated by the
/// session reducer; host classifications must be no broader, no less
/// normalized, and deterministic as well.
fn validate_host_conflicting_paths(
    paths: &[String],
    immutable_paths: &[String],
) -> Result<(), HarnessError> {
    if paths.is_empty() {
        return Err(HarnessError::invalid_state(
            "host conflict outcome must name at least one immutable changed path",
        ));
    }
    let immutable = immutable_paths.iter().collect::<BTreeSet<_>>();
    let mut previous = None::<&String>;
    for path in paths {
        let valid_shape = !path.is_empty()
            && path.len() <= 4_096
            && !path.contains('\0')
            && !path.starts_with('/')
            && !path.starts_with('\\')
            && path.as_bytes().get(1) != Some(&b':')
            && !path.contains('\\')
            && !path
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..");
        if !valid_shape || !immutable.contains(path) {
            return Err(HarnessError::invalid_state(
                "host conflict outcome contains a non-durable repository path",
            ));
        }
        if previous.is_some_and(|prior| prior >= path) {
            return Err(HarnessError::invalid_state(
                "host conflict paths must be sorted and unique",
            ));
        }
        previous = Some(path);
    }
    Ok(())
}

/// Host diagnostics are an application boundary, not trusted model output.
/// Preserve useful classifications without allowing unbounded or control-byte
/// strings into durable tool result context.
fn validate_host_apply_diagnostic(diagnostic: &str) -> Result<(), HarnessError> {
    if diagnostic.is_empty()
        || diagnostic != diagnostic.trim()
        || diagnostic.len() > 4_096
        || diagnostic.chars().any(char::is_control)
    {
        return Err(HarnessError::invalid_state(
            "host application diagnostic must be trimmed bounded text without controls",
        ));
    }
    Ok(())
}

/// Return only the unresolved source-order suffix of the final assistant tool
/// batch. A crash may happen after a result prefix committed, especially when
/// parallel tools settle; those entries remain part of restored context and
/// must not cause the host effect to execute again.
fn recovery_tool_calls(
    snapshot: &SessionSnapshot,
    assistant_entry_id: &EntryId,
) -> Result<Vec<AgentToolCall>, HarnessError> {
    let assistant_index = snapshot
        .entries()
        .iter()
        .position(|entry| &entry.header.id == assistant_entry_id)
        .ok_or_else(|| {
            HarnessError::invalid_state("recovery assistant entry is missing or has the wrong type")
        })?;
    let SessionEntry::AssistantMessage(assistant) = &snapshot.entries()[assistant_index].body
    else {
        return Err(HarnessError::invalid_state(
            "recovery assistant entry is missing or has the wrong type",
        ));
    };
    let trailing_results = snapshot.entries()[assistant_index.saturating_add(1)..]
        .iter()
        .filter_map(|entry| match &entry.body {
            SessionEntry::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut first_missing = None;
    for (index, call) in assistant.tool_calls.iter().enumerate() {
        let result = trailing_results
            .iter()
            .find(|result| result.tool_call_id == call.id && result.tool_name == call.name);
        match (first_missing, result) {
            (None, Some(_)) => {}
            (None, None) => first_missing = Some(index),
            (Some(_), None) => {}
            (Some(_), Some(_)) => {
                return Err(HarnessError::invalid_state(
                    "durable recovered tool results are not a source-order prefix",
                ));
            }
        }
    }
    let first_missing = first_missing.ok_or_else(|| {
        HarnessError::invalid_state("recovery assistant has no unresolved tool calls")
    })?;
    assistant
        .tool_calls
        .iter()
        .skip(first_missing)
        .map(|call| {
            Ok(AgentToolCall {
                id: ToolCallId::new(call.id.clone()).map_err(|error| {
                    HarnessError::invalid_state(format!(
                        "recovery assistant call ID is invalid: {error}"
                    ))
                })?,
                name: call.name.clone(),
                arguments: SerializedJson::new(call.arguments.to_json_string().map_err(
                    |error| {
                        HarnessError::invalid_state(format!(
                            "recovery assistant arguments cannot encode: {error}"
                        ))
                    },
                )?),
            })
        })
        .collect()
}
