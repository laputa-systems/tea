use super::{
    HarnessIdentity, RuntimeServices, SessionSupervisor, SessionSupervisorInput,
    SessionSupervisorReopenInput, append_child_subagent_instruction_suffix,
    append_root_subagent_surface, root_subagent_tool_definitions,
    root_subagent_tool_presentations,
};
use super::subagents::{
    ApplyAgentChangesResult, ApplyWorkspaceDeltaRequest, FinalizeSubagentRequest, PreparedSubagent,
    PrepareSubagentRequest, ReopenSubagentRequest, SubagentHost, SubagentHostError,
    SpawnAgentRequest, SubagentHostFuture, SubagentModel, SubagentPolicy, SubagentServices,
    SubagentTaskError, TaskHandle, TaskRuntime, WaitAgentsRequest, WaitReturnWhen,
    WorkspaceApplyOutcome, WorkspaceDelta, WorkspaceFinalization,
    WorkspaceLease,
};
use crate::harness::extension::NoExtensions;
use crate::harness::{
    HarnessActor, HarnessError, HarnessRepository, HarnessResolver, HarnessResourceLimits, HarnessSnapshotSpec,
    PromptSectionDescriptor, SELF_EXTENSION_MODE_METADATA_KEY, SelfExtensionMode,
    ToolPresentationDescriptor,
};
use crate::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use crate::effect::RunProvenance;
use crate::state::{AgentToolCall, ModelDescriptor, SerializedJson, StopReason, ToolCallId};
use crate::tool::{
    AgentTool, AgentToolResult, CancellationSettlementMode, ToolCall, ToolContext, ToolDefinition,
    ToolExecutionMode, ToolFuture, ToolRegistry, ToolUpdateSink,
};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tea_protocol::JsonValue;
use tea_session::{
    AgentContextMode, AgentId, AgentState, ArtifactPolicyId, ArtifactStore, AssistantToolCall, CoreRunId, Digest, DurabilityMode, EntryId,
    EpochFinishReason, EpochFinishedRecord, EpochId, EpochStartedRecord, HarnessRevisionChangedEntry, JsonlSession, LaneId,
    LaneMutation, LaneRecord, MemoryArtifactStore, MemorySession, ModelHarnessProfileId,
    ModelChangedEntry, OperationFinishedRecord, OperationId, OperationKind, OperationOutcome, PayloadRef,
    OperationStartedRecord, ProvisionedEntry, RecordId, SessionEntry, SessionFact,
    SessionHeader, SessionId, SessionWriter, SubagentModelRecord, SubagentPolicyFact,
    ThinkingChangedEntry, ToolReplayPolicy, ToolResultEntry, ToolStartedRecord, Usage, WorkspaceDeltaId, WorkspaceLeaseId,
    reduce_agent_graph, reduce_lane,
};

#[derive(Debug)]
struct QueuedProvider {
    streams: Mutex<VecDeque<ModelStream>>,
}

impl ModelProvider for QueuedProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = self
            .streams
            .lock()
            .expect("fixture provider queue mutex")
            .pop_front()
            .expect("fixture provider has a response stream");
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[derive(Clone)]
struct FixtureSubagentHost {
    child_identity: HarnessIdentity,
    allowed_models: BTreeMap<String, ModelDescriptor>,
    requests: Arc<Mutex<Vec<PrepareSubagentRequest>>>,
    reopen_count: Arc<Mutex<u32>>,
    cleanup_fail: Arc<Mutex<bool>>,
    finalizations: Arc<Mutex<VecDeque<FixtureFinalization>>>,
    delta_patch_artifact: tea_session::ArtifactId,
    apply_outcomes: Arc<Mutex<VecDeque<Result<WorkspaceApplyOutcome, SubagentHostError>>>>,
    apply_requests: Arc<Mutex<Vec<ApplyWorkspaceDeltaRequest>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureFinalization {
    NoChanges,
    Delta,
}

struct DefinitionOnlyTool(ToolDefinition);

impl AgentTool for DefinitionOnlyTool {
    fn name(&self) -> &str {
        &self.0.name
    }

    fn description(&self) -> &str {
        &self.0.description
    }

    fn schema(&self) -> &JsonValue {
        &self.0.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.0.execution_mode
    }

    fn requires_exclusive_batch(&self) -> bool {
        self.0.requires_exclusive_batch
    }

    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        self.0.cancellation_settlement_mode
    }

    fn execute<'a>(
        &'a self,
        _call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async { unreachable!("definition-only fixture tool is never executed") })
    }
}

impl SubagentHost for FixtureSubagentHost {
    fn prepare<'a>(
        &'a self,
        request: PrepareSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent> {
        let child_identity = self.child_identity.clone();
        let allowed_models = self.allowed_models.clone();
        let requests = Arc::clone(&self.requests);
        Box::pin(async move {
            let descriptor = allowed_models.get(&request.model.descriptor.model).ok_or_else(|| {
                SubagentHostError {
                    message: "fixture host rejected an unlisted child model".into(),
                }
            })?;
            let prepared = fixture_prepared_subagent(
                child_identity,
                descriptor.clone(),
                request.agent_id.clone(),
                request.thinking,
            );
            requests
                .lock()
                .expect("fixture child-host request mutex")
                .push(request);
            Ok(prepared)
        })
    }

    fn reopen<'a>(
        &'a self,
        request: ReopenSubagentRequest,
    ) -> SubagentHostFuture<'a, PreparedSubagent> {
        let child_identity = self.child_identity.clone();
        let count = Arc::clone(&self.reopen_count);
        let fail = Arc::clone(&self.cleanup_fail);
        Box::pin(async move {
            if *fail.lock().expect("fixture child-host reopen mutex") {
                return Err(SubagentHostError {
                    message: "fixture reopen is intentionally unavailable".into(),
                });
            }
            let prepared = fixture_prepared_subagent(
                child_identity,
                request.model.descriptor,
                request.agent_id,
                request.thinking,
            );
            *count.lock().expect("fixture child-host reopen mutex") += 1;
            Ok(prepared)
        })
    }

    fn finalize<'a>(
        &'a self,
        request: FinalizeSubagentRequest,
    ) -> SubagentHostFuture<'a, WorkspaceFinalization> {
        let finalizations = Arc::clone(&self.finalizations);
        let patch_artifact = self.delta_patch_artifact.clone();
        Box::pin(async move {
            match finalizations
                .lock()
                .expect("fixture finalization mutex")
                .pop_front()
                .unwrap_or(FixtureFinalization::NoChanges)
            {
                FixtureFinalization::NoChanges => Ok(WorkspaceFinalization::NoChanges),
                FixtureFinalization::Delta => Ok(WorkspaceFinalization::Delta(WorkspaceDelta {
                    id: tea_session::WorkspaceDeltaId::derive(
                        &request.workspace.id,
                        "fixture-child-base",
                        "fixture-child-result",
                    ),
                    agent_id: request.agent_id,
                    workspace_lease_id: request.workspace.id,
                    base_commit: "fixture-child-base".into(),
                    result_commit: "fixture-child-result".into(),
                    changed_paths: vec!["src/fixture.rs".into()],
                    patch_artifact,
                })),
            }
        })
    }

    fn apply<'a>(
        &'a self,
        request: ApplyWorkspaceDeltaRequest,
    ) -> SubagentHostFuture<'a, WorkspaceApplyOutcome> {
        let outcomes = Arc::clone(&self.apply_outcomes);
        let requests = Arc::clone(&self.apply_requests);
        Box::pin(async move {
            requests
                .lock()
                .expect("fixture apply request mutex")
                .push(request);
            outcomes
                .lock()
                .expect("fixture apply outcome mutex")
                .pop_front()
                .unwrap_or_else(|| {
                    Err(SubagentHostError {
                        message: "fixture apply outcome was not configured".into(),
                    })
                })
        })
    }

    fn cleanup<'a>(&'a self, _lease: WorkspaceLease) -> SubagentHostFuture<'a, ()> {
        let cleanup_fail = Arc::clone(&self.cleanup_fail);
        Box::pin(async move {
            if *cleanup_fail.lock().expect("fixture cleanup failure mutex") {
                return Err(SubagentHostError {
                    message: "fixture cleanup is intentionally blocked".into(),
                });
            }
            Ok(())
        })
    }
}

fn fixture_prepared_subagent(
    harness_identity: HarnessIdentity,
    descriptor: ModelDescriptor,
    agent_id: tea_session::AgentId,
    thinking: tea_core::state::ThinkingLevel,
) -> PreparedSubagent {
    PreparedSubagent {
        workspace: WorkspaceLease {
            id: WorkspaceLeaseId::derive(&agent_id),
            logical_workspace: format!("fixture://{agent_id}"),
        },
        harness_identity,
        runtime_services: RuntimeServices::new(
            Arc::new(QueuedProvider {
                streams: Mutex::new(VecDeque::from([completion_stream()])),
            }),
            ToolRegistry::default(),
        )
        .model(descriptor)
        .thinking_level(thinking),
    }
}

struct FixtureTaskHandle {
    task_id: u64,
    tasks: std::sync::Weak<Mutex<BTreeMap<u64, Pin<Box<dyn Future<Output = ()> + Send + 'static>>>>>,
}

impl TaskHandle for FixtureTaskHandle {
    fn cancel(&self) {
        if let Some(tasks) = self.tasks.upgrade() {
            tasks
                .lock()
                .expect("fixture task ownership mutex")
                .remove(&self.task_id);
        }
    }

    fn join<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::ready(()))
    }
}

struct FixtureTaskRuntime {
    tasks: Arc<Mutex<BTreeMap<u64, Pin<Box<dyn Future<Output = ()> + Send + 'static>>>>>,
    accepted: Mutex<u32>,
    reject_next: Mutex<bool>,
    next_task_id: Mutex<u64>,
    immediate_sleep: Mutex<bool>,
    next_sleep_action: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct FixtureSleep {
    ready: bool,
    action: Option<Box<dyn FnOnce() + Send>>,
}

impl Future for FixtureSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _context: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if let Some(action) = self.action.take() {
            action();
        }
        if self.ready {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }
}

impl FixtureTaskRuntime {
    fn accepting() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(BTreeMap::new())),
            accepted: Mutex::new(0),
            reject_next: Mutex::new(false),
            next_task_id: Mutex::new(0),
            immediate_sleep: Mutex::new(true),
            next_sleep_action: Mutex::new(None),
        }
    }

    fn reject_once(&self) {
        *self.reject_next.lock().expect("fixture task reject mutex") = true;
    }

    fn owned_task_count(&self) -> usize {
        self.tasks
            .lock()
            .expect("fixture task ownership mutex")
            .len()
    }

    fn lose_owned_tasks_for_restart(&self) {
        self.tasks
            .lock()
            .expect("fixture task ownership mutex")
            .clear();
    }

    fn hold_wait_timeouts(&self) {
        *self
            .immediate_sleep
            .lock()
            .expect("fixture task sleep mutex") = false;
    }

    fn act_on_next_sleep(&self, action: impl FnOnce() + Send + 'static) {
        *self
            .next_sleep_action
            .lock()
            .expect("fixture task sleep action mutex") = Some(Box::new(action));
    }

    /// Poll one supervisor-owned child once without selecting an executor.
    /// Tests use an immediately-ready policy timer to drive the timeout branch
    /// before the child would make any provider request.
    fn poll_one_owned_task(&self) {
        let (task_id, mut task) = {
            let mut tasks = self.tasks.lock().expect("fixture task ownership mutex");
            let task_id = *tasks
                .keys()
                .next()
                .expect("fixture has one owned child task to poll");
            let task = tasks
                .remove(&task_id)
                .expect("fixture task remains present while locked");
            (task_id, task)
        };
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        if task.as_mut().poll(&mut context).is_pending() {
            self.tasks
                .lock()
                .expect("fixture task ownership mutex")
                .insert(task_id, task);
        }
    }

}

impl TaskRuntime for FixtureTaskRuntime {
    fn spawn(
        &self,
        _name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<Arc<dyn TaskHandle>, SubagentTaskError> {
        let mut reject_next = self.reject_next.lock().expect("fixture task reject mutex");
        if *reject_next {
            *reject_next = false;
            return Err(SubagentTaskError {
                message: "fixture task runtime refused this handoff".into(),
            });
        }
        let task_id = {
            let mut next = self.next_task_id.lock().expect("fixture task ID mutex");
            let task_id = *next;
            *next = next.wrapping_add(1);
            task_id
        };
        self.tasks
            .lock()
            .expect("fixture task ownership mutex")
            .insert(task_id, task);
        *self.accepted.lock().expect("fixture task count mutex") += 1;
        Ok(Arc::new(FixtureTaskHandle {
            task_id,
            tasks: Arc::downgrade(&self.tasks),
        }))
    }

    fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let ready = *self
            .immediate_sleep
            .lock()
            .expect("fixture task sleep mutex");
        let action = self
            .next_sleep_action
            .lock()
            .expect("fixture task sleep action mutex")
            .take();
        Box::pin(FixtureSleep { ready, action })
    }
}

#[derive(Debug)]
struct RecordingTool;

impl AgentTool for RecordingTool {
    fn name(&self) -> &str {
        "record"
    }

    fn description(&self) -> &str {
        "records one durable tool intent"
    }

    fn schema(&self) -> &JsonValue {
        static SCHEMA: std::sync::LazyLock<JsonValue> =
            std::sync::LazyLock::new(|| JsonValue::parse(r#"{"type":"object"}"#).unwrap());
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "recorded".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

fn snapshot_spec(identities: super::RuntimePolicyIdentities) -> HarnessSnapshotSpec {
    HarnessSnapshotSpec {
        base_profile_digest: Digest::from_bytes("runtime-test-host-profile"),
        base_system_prompt: "Use the durable runtime fixture.".into(),
        model_harness_profile: ModelHarnessProfileId::new("runtime-test-profile")
            .expect("fixture profile ID"),
        self_extension_addendum: None,
        ordered_global_plugins: Vec::new(),
        ordered_session_plugins: Vec::new(),
        prompt_sections: vec![PromptSectionDescriptor {
            id: "runtime-test".into(),
            content: "Keep durable evidence ordered.".into(),
        }],
        plugin_prompt_sections: Vec::new(),
        tool_presentations: vec![ToolPresentationDescriptor {
            name: "record".into(),
            description: "records one durable tool intent".into(),
            schema: JsonValue::parse(r#"{"type":"object"}"#).expect("fixture schema"),
            execution_mode: "parallel".into(),
            requires_exclusive_batch: false,
            cancellation_settlement_mode: "drop_future".into(),
        }],
        plugin_tool_presentations: Vec::new(),
        hook_bundle_digest: identities.hook_bundle_digest,
        capability_bindings: Vec::new(),
        resource_limits: HarnessResourceLimits::default(),
        compaction_policy_digest: identities.compaction_policy_digest,
        tool_projection_digest: identities.tool_projection_digest,
        failure_policy_digest: identities.failure_policy_digest,
    }
}

fn fixture_manager(
    provider: Arc<dyn ModelProvider>,
    store: Arc<MemoryArtifactStore>,
) -> (Arc<HarnessResolver>, HarnessIdentity, RuntimeServices) {
    let mut repository =
        HarnessRepository::with_extension_engine(store.clone(), Arc::new(NoExtensions));
    let mut tools = ToolRegistry::default();
    tools.insert(Arc::new(RecordingTool));
    let services = RuntimeServices::new(provider, tools);
    let snapshot = repository
        .stage_snapshot(snapshot_spec(services.runtime_policy_identities()))
        .expect("no-extension snapshot stages");
    let revision = repository
        .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
        .expect("initial fixture revision stages");
    let identity = HarnessIdentity::new(
        revision.revision_id.clone(),
        snapshot.id.clone(),
        snapshot.spec.model_harness_profile.clone(),
    );
    (
        Arc::new(HarnessResolver::new(repository, Default::default())),
        identity,
        services,
    )
}

#[test]
fn resolver_rejects_snapshot_runtime_policy_identity_mismatch() {
    let provider: Arc<dyn ModelProvider> = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let store = Arc::new(MemoryArtifactStore::default());
    let services = RuntimeServices::new(provider, ToolRegistry::default());
    let mut identities = services.runtime_policy_identities();
    identities.tool_projection_digest = Digest::from_bytes("runtime-test-wrong-projection");
    let mut repository =
        HarnessRepository::with_extension_engine(store, Arc::new(NoExtensions));
    let snapshot = repository
        .stage_snapshot(snapshot_spec(identities))
        .expect("mismatched fixture snapshot stages");
    let revision = repository
        .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
        .expect("mismatched fixture revision stages");
    let resolver = HarnessResolver::new(repository, Default::default());
    let error = resolver
        .resolve_revision(&revision.revision_id, &services)
        .expect_err("runtime policy identity mismatch is rejected");
    assert!(error.to_string().contains("tool-result projection identity"));
}

#[test]
fn runtime_policy_default_identities_are_stable() {
    let first = RuntimeServices::new(
        Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::new()),
        }),
        ToolRegistry::default(),
    )
    .runtime_policy_identities();
    let second = RuntimeServices::new(
        Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::new()),
        }),
        ToolRegistry::default(),
    )
    .runtime_policy_identities();
    assert_eq!(first, second);
}

fn fixture_metadata() -> tea_session::Metadata {
    [(
        SELF_EXTENSION_MODE_METADATA_KEY.into(),
        SelfExtensionMode::Off.metadata_value(),
    )]
    .into_iter()
    .collect()
}

fn append_initial_revision<S: SessionWriter>(session: &mut S, identity: &HarnessIdentity) {
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("runtime-test-initial-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: identity.revision_id().clone(),
                    snapshot_id: identity.snapshot_id().clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("initial revision entry commits");
}

fn build_runtime(
    session_id: &str,
    provider: Arc<dyn ModelProvider>,
    store: Arc<MemoryArtifactStore>,
) -> (Arc<SessionSupervisor<MemorySession>>, HarnessIdentity) {
    let (manager, identity, services) = fixture_manager(provider, store.clone());
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new(session_id).expect("fixture session ID"),
        "runtime-test-workspace",
        fixture_metadata(),
    ))
    .expect("fixture session creates");
    append_initial_revision(&mut session, &identity);
    (
        SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity.clone(),
            root_services: services,
            artifacts: store,
            rollover_budget: 1,
            subagents: None,
        })
        .expect("supervisor creates from immutable no-extension lineage"),
        identity,
    )
}

fn fixture_subagent_policy() -> SubagentPolicy {
    SubagentPolicy {
        models: vec![SubagentModel {
            descriptor: ModelDescriptor {
                provider: "fixture".into(),
                model: "child-model".into(),
                revision: Some("fixture-child-r1".into()),
            },
            display_name: "Fixture child".into(),
            context_window: Some(NonZeroU64::new(32_768).expect("nonzero fixture context")),
        }],
        max_concurrent: NonZeroU32::new(2).expect("nonzero fixture limit"),
        max_total_per_operation: NonZeroU32::new(4).expect("nonzero fixture limit"),
        timeout: Duration::from_secs(900),
    }
}

fn append_subagent_policy<S: SessionWriter>(session: &mut S, policy: &SubagentPolicy) {
    session
        .append_fact(SessionFact::SubagentPolicy(SubagentPolicyFact {
            schema_version: 1,
            models: policy
                .models
                .iter()
                .map(|model| SubagentModelRecord {
                    provider: model.descriptor.provider.clone(),
                    model: model.descriptor.model.clone(),
                    revision: model.descriptor.revision.clone(),
                    display_name: model.display_name.clone(),
                    context_window: model.context_window.map(|value| value.get()),
                })
                .collect(),
            max_concurrent: policy.max_concurrent.get(),
            max_total_per_operation: policy.max_total_per_operation.get(),
            timeout_ms: policy.timeout.as_millis() as u64,
            tool_surface_digest: super::root_subagent_tool_surface_digest(policy)
                .expect("fixture subagent surface digest"),
        }))
        .expect("subagent policy commits before root harness revision");
}

fn build_subagent_runtime(
    session_id: &str,
    root_streams: Vec<ModelStream>,
) -> (
    Arc<SessionSupervisor<MemorySession>>,
    Arc<Mutex<Vec<PrepareSubagentRequest>>>,
    Arc<FixtureTaskRuntime>,
) {
    build_subagent_runtime_with_policy(session_id, root_streams, fixture_subagent_policy())
}

fn build_subagent_runtime_with_policy(
    session_id: &str,
    root_streams: Vec<ModelStream>,
    policy: SubagentPolicy,
) -> (
    Arc<SessionSupervisor<MemorySession>>,
    Arc<Mutex<Vec<PrepareSubagentRequest>>>,
    Arc<FixtureTaskRuntime>,
) {
    build_subagent_runtime_with_child_surface(
        session_id,
        root_streams,
        policy,
        false,
    )
}

fn build_subagent_runtime_with_child_surface(
    session_id: &str,
    root_streams: Vec<ModelStream>,
    policy: SubagentPolicy,
    use_root_surface_for_child: bool,
) -> (
    Arc<SessionSupervisor<MemorySession>>,
    Arc<Mutex<Vec<PrepareSubagentRequest>>>,
    Arc<FixtureTaskRuntime>,
) {
    let store = Arc::new(MemoryArtifactStore::default());
    let delta_patch_artifact = store
        .put(b"fixture child patch", "application/x-git-diff")
        .expect("fixture patch artifact stores")
        .artifact_id;
    let root_provider = Arc::new(QueuedProvider {
        streams: Mutex::new(root_streams.into()),
    });
    let child_provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let (manager, root_identity, root_services, child_identity) =
        fixture_subagent_manager(root_provider, child_provider, store.clone(), &policy);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reopen_count = Arc::new(Mutex::new(0));
    let host = Arc::new(FixtureSubagentHost {
        child_identity: if use_root_surface_for_child {
            root_identity.clone()
        } else {
            child_identity
        },
        allowed_models: policy
            .models
            .iter()
            .map(|model| (model.descriptor.model.clone(), model.descriptor.clone()))
            .collect(),
        requests: Arc::clone(&requests),
        reopen_count,
        cleanup_fail: Arc::new(Mutex::new(false)),
        finalizations: Arc::new(Mutex::new(VecDeque::new())),
        delta_patch_artifact,
        apply_outcomes: Arc::new(Mutex::new(VecDeque::new())),
        apply_requests: Arc::new(Mutex::new(Vec::new())),
    });
    let tasks = Arc::new(FixtureTaskRuntime::accepting());
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new(session_id).expect("fixture session ID"),
        "runtime-subagent-workspace",
        fixture_metadata(),
    ))
    .expect("fixture subagent session creates");
    append_subagent_policy(&mut session, &policy);
    append_initial_revision(&mut session, &root_identity);
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity,
        root_services,
        artifacts: store,
        rollover_budget: 1,
        subagents: Some(SubagentServices {
            policy,
            host,
            tasks: Arc::clone(&tasks) as Arc<dyn TaskRuntime>,
        }),
    })
    .expect("subagent supervisor creates from matching durable policy");
    (runtime, requests, tasks)
}

fn fixture_subagent_manager(
    root_provider: Arc<dyn ModelProvider>,
    child_provider: Arc<dyn ModelProvider>,
    store: Arc<MemoryArtifactStore>,
    policy: &SubagentPolicy,
) -> (
    Arc<HarnessResolver>,
    HarnessIdentity,
    RuntimeServices,
    HarnessIdentity,
) {
    let mut repository =
        HarnessRepository::with_extension_engine(store.clone(), Arc::new(NoExtensions));
    let mut root_tools = ToolRegistry::default();
    root_tools.insert(Arc::new(RecordingTool));
    let root_services = RuntimeServices::new(root_provider, root_tools);
    let mut root_spec = snapshot_spec(root_services.runtime_policy_identities());
    let mut ignored_definitions = Vec::new();
    append_root_subagent_surface(
        &mut root_spec.base_system_prompt,
        &mut ignored_definitions,
        Some(policy),
    )
    .expect("fixture root collaboration surface builds");
    root_spec.tool_presentations.extend(
        root_subagent_tool_presentations(policy).expect("fixture root presentations build"),
    );
    let root_snapshot = repository
        .stage_snapshot(root_spec)
        .expect("fixture root snapshot stages");
    let root_revision = repository
        .seed_revision(root_snapshot.id.clone(), HarnessActor::Host, 1)
        .expect("fixture root revision stages");
    let root_identity = HarnessIdentity::new(
        root_revision.revision_id,
        root_snapshot.id,
        root_snapshot.spec.model_harness_profile,
    );
    let child_services = RuntimeServices::new(child_provider, ToolRegistry::default())
        .model(policy.models[0].descriptor.clone());
    let mut child_spec = snapshot_spec(child_services.runtime_policy_identities());
    append_child_subagent_instruction_suffix(&mut child_spec.base_system_prompt);
    let child_snapshot = repository
        .stage_snapshot(child_spec)
        .expect("fixture child snapshot stages");
    let child_revision = repository
        .seed_revision(child_snapshot.id.clone(), HarnessActor::Host, 2)
        .expect("fixture child revision stages");
    let child_identity = HarnessIdentity::new(
        child_revision.revision_id,
        child_snapshot.id,
        child_snapshot.spec.model_harness_profile,
    );
    (
        Arc::new(HarnessResolver::new(repository, Default::default())),
        root_identity,
        root_services,
        child_identity,
    )
}

fn build_active_spawn_replay_fixture() -> (
    Arc<SessionSupervisor<MemorySession>>,
    Arc<FixtureTaskRuntime>,
    Arc<Mutex<Vec<PrepareSubagentRequest>>>,
    Arc<Mutex<u32>>,
    Arc<Mutex<bool>>,
    Arc<Mutex<VecDeque<FixtureFinalization>>>,
    Arc<Mutex<VecDeque<Result<WorkspaceApplyOutcome, SubagentHostError>>>>,
    Arc<Mutex<Vec<ApplyWorkspaceDeltaRequest>>>,
    ToolCall,
    RunProvenance,
    SpawnAgentRequest,
) {
    build_active_spawn_replay_fixture_with_options(false, false, true, None)
}

fn build_active_spawn_replay_fixture_with_foreign_reused_task(include_foreign: bool) -> (
    Arc<SessionSupervisor<MemorySession>>,
    Arc<FixtureTaskRuntime>,
    Arc<Mutex<Vec<PrepareSubagentRequest>>>,
    Arc<Mutex<u32>>,
    Arc<Mutex<bool>>,
    Arc<Mutex<VecDeque<FixtureFinalization>>>,
    Arc<Mutex<VecDeque<Result<WorkspaceApplyOutcome, SubagentHostError>>>>,
    Arc<Mutex<Vec<ApplyWorkspaceDeltaRequest>>>,
    ToolCall,
    RunProvenance,
    SpawnAgentRequest,
) {
    build_active_spawn_replay_fixture_with_options(include_foreign, false, true, None)
}

fn build_active_spawn_replay_fixture_with_options(
    include_foreign: bool,
    settle_spawn_intent: bool,
    include_apply_intent: bool,
    parent_thinking: Option<crate::state::ThinkingLevel>,
) -> (
    Arc<SessionSupervisor<MemorySession>>,
    Arc<FixtureTaskRuntime>,
    Arc<Mutex<Vec<PrepareSubagentRequest>>>,
    Arc<Mutex<u32>>,
    Arc<Mutex<bool>>,
    Arc<Mutex<VecDeque<FixtureFinalization>>>,
    Arc<Mutex<VecDeque<Result<WorkspaceApplyOutcome, SubagentHostError>>>>,
    Arc<Mutex<Vec<ApplyWorkspaceDeltaRequest>>>,
    ToolCall,
    RunProvenance,
    SpawnAgentRequest,
) {
    let store = Arc::new(MemoryArtifactStore::default());
    let delta_patch_artifact = store
        .put(b"fixture child patch", "application/x-git-diff")
        .expect("fixture patch artifact stores")
        .artifact_id;
    let policy = fixture_subagent_policy();
    let spawn_definition = root_subagent_tool_definitions(&policy)
        .expect("fixture root collaboration definitions resolve")
        .into_iter()
        .find(|definition| definition.name == "spawn_agent")
        .expect("fixture spawn definition exists");
    let spawn_definition_digest =
        super::supervisor::tool_definition_digest(&DefinitionOnlyTool(spawn_definition))
            .expect("fixture spawn definition hashes");
    let (manager, root_identity, root_services, child_identity) = fixture_subagent_manager(
        Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([completion_stream()])),
        }),
        Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::new()),
        }),
        store.clone(),
        &policy,
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reopen_count = Arc::new(Mutex::new(0));
    let cleanup_fail = Arc::new(Mutex::new(false));
    let finalizations = Arc::new(Mutex::new(VecDeque::new()));
    let apply_outcomes = Arc::new(Mutex::new(VecDeque::new()));
    let apply_requests = Arc::new(Mutex::new(Vec::new()));
    let host = Arc::new(FixtureSubagentHost {
        child_identity: child_identity.clone(),
        allowed_models: policy
            .models
            .iter()
            .map(|model| (model.descriptor.model.clone(), model.descriptor.clone()))
            .collect(),
        requests: Arc::clone(&requests),
        reopen_count: Arc::clone(&reopen_count),
        cleanup_fail: Arc::clone(&cleanup_fail),
        finalizations: Arc::clone(&finalizations),
        delta_patch_artifact,
        apply_outcomes: Arc::clone(&apply_outcomes),
        apply_requests: Arc::clone(&apply_requests),
    });
    let tasks = Arc::new(FixtureTaskRuntime::accepting());
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("runtime-subagent-replay").expect("fixture session ID"),
        "runtime-subagent-replay-workspace",
        fixture_metadata(),
    ))
    .expect("fixture session creates");
    append_subagent_policy(&mut session, &policy);
    append_initial_revision(&mut session, &root_identity);
    let mut root_source = EntryId::new("runtime-test-initial-revision").expect("root source ID");
    if include_foreign {
        append_finished_foreign_spawn_with_reused_task_name(
            &mut session,
            &policy,
            &root_identity,
            &child_identity,
            &root_source,
        );
        root_source = EntryId::new("fixture-older-root-assistant").expect("older root leaf ID");
    }
    if let Some(thinking) = parent_thinking {
        let thinking_id =
            EntryId::new("fixture-root-replay-thinking").expect("thinking entry ID");
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry {
                    id: thinking_id.clone(),
                    body: SessionEntry::ThinkingChanged(ThinkingChangedEntry {
                        level: match thinking {
                            crate::state::ThinkingLevel::Off => "off",
                            crate::state::ThinkingLevel::Minimal => "minimal",
                            crate::state::ThinkingLevel::Low => "low",
                            crate::state::ThinkingLevel::Medium => "medium",
                            crate::state::ThinkingLevel::High => "high",
                            crate::state::ThinkingLevel::XHigh => "xhigh",
                            crate::state::ThinkingLevel::Max => "max",
                        }
                        .into(),
                    }),
                },
            )
            .expect("root thinking commits before the replay operation");
        root_source = thinking_id;
    }
    let operation_id = OperationId::new("fixture-root-replay-operation").expect("operation ID");
    let epoch_id = EpochId::new("fixture-root-replay-epoch").expect("epoch ID");
    let assistant_id = EntryId::new("fixture-root-replay-assistant").expect("assistant ID");
    let call = ToolCall {
        id: ToolCallId::new("fixture-root-replay-call").expect("tool call ID"),
        name: "spawn_agent".into(),
        arguments: SerializedJson::new(
            r#"{"task_name":"replay_task","task":"inspect the replay prefix","model":"child-model","context":"task"}"#,
        ),
    };
    let request = SpawnAgentRequest {
        task_name: "replay_task".into(),
        task: "inspect the replay prefix".into(),
        model: "child-model".into(),
        thinking: None,
        context_mode: AgentContextMode::Task,
    };
    let child_agent_id = AgentId::derive(
        &SessionId::new("runtime-subagent-replay").expect("fixture session ID"),
        &LaneId::main(),
        &operation_id,
        "fixture-root-replay-key",
    );
    let apply_call = fixture_apply_call(&child_agent_id);
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            operation_id.clone(),
            LaneId::main(),
            Some(root_source.clone()),
            OperationKind::Run,
            Vec::new(),
            root_identity.revision_id().clone(),
            root_identity.profile_id().clone(),
        )))
        .expect("root operation starts");
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(root_source.clone()),
            harness_revision_id: root_identity.revision_id().clone(),
            harness_snapshot_id: root_identity.snapshot_id().clone(),
            model_harness_profile: root_identity.profile_id().clone(),
            core_run_id: CoreRunId::new("fixture-root-replay-core-run").expect("core run ID"),
            epoch_resume_data: BTreeMap::new(),
        }))
        .expect("root epoch starts");
    let mut assistant_tool_calls = vec![AssistantToolCall::new(
        call.id.to_string(),
        call.name.clone(),
        JsonValue::parse(call.arguments.as_str()).expect("fixture tool args parse"),
    )];
    if include_apply_intent {
        assistant_tool_calls.push(AssistantToolCall::new(
            apply_call.id.to_string(),
            apply_call.name.clone(),
            JsonValue::parse(apply_call.arguments.as_str()).expect("fixture apply args parse"),
        ));
    }
    let mut assistant_entry =
        ProvisionedEntry::assistant(assistant_id.clone(), "", assistant_tool_calls);
    let SessionEntry::AssistantMessage(assistant) = &mut assistant_entry.body else {
        unreachable!("assistant fixture constructor returns an assistant entry")
    };
    assistant.stop_reason = Some("tool_use".into());
    session
        .append_entry(&LaneId::main(), assistant_entry)
        .expect("assistant spawn call commits");
    session
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new("fixture-root-replay-tool-record").expect("tool record ID"),
            operation_id.clone(),
            epoch_id.clone(),
            assistant_id.clone(),
            0,
            call.id.to_string(),
            "spawn_agent",
            JsonValue::parse(call.arguments.as_str()).expect("fixture tool args parse"),
            EntryId::new("fixture-root-replay-tool-result").expect("tool result ID"),
            ToolReplayPolicy::Safe,
            spawn_definition_digest,
            root_identity.revision_id().clone(),
            "fixture-root-replay-key",
        )))
        .expect("spawn tool intent commits");
    if settle_spawn_intent {
        session
            .append_entry(
                &LaneId::main(),
                ProvisionedEntry {
                    id: EntryId::new("fixture-root-replay-tool-result").expect("tool result ID"),
                    body: SessionEntry::ToolResult(ToolResultEntry {
                        tool_call_id: call.id.to_string(),
                        tool_name: call.name.clone(),
                        full_result: PayloadRef::Inline(JsonValue::String("spawn settled".into())),
                        model_projection: JsonValue::String("spawn settled".into()),
                        is_error: false,
                        terminate: false,
                        usage: Usage::default(),
                        projection_strategy_id: "fixture-tool-result".into(),
                        artifact_policy_id: ArtifactPolicyId::new("fixture-artifact-policy")
                            .expect("artifact policy ID"),
                    }),
                },
            )
            .expect("spawn tool result commits");
    }
    if include_apply_intent {
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
                RecordId::new("fixture-root-replay-apply-record").expect("tool record ID"),
                operation_id.clone(),
                epoch_id.clone(),
                assistant_id,
                1,
                apply_call.id.to_string(),
                "apply_agent_changes",
                JsonValue::parse(apply_call.arguments.as_str()).expect("fixture apply args parse"),
                EntryId::new("fixture-root-replay-apply-result").expect("tool result ID"),
                ToolReplayPolicy::Never,
                Digest::from_bytes("fixture apply definition"),
                root_identity.revision_id().clone(),
                "fixture-root-replay-apply-key",
            )))
            .expect("apply tool intent commits");
    }
    let provenance = RunProvenance {
        session_id: Some("runtime-subagent-replay".into()),
        lane_id: Some(LaneId::main().to_string()),
        agent_id: None,
        operation_id: Some(operation_id.to_string()),
        epoch_id: Some(epoch_id.to_string()),
        source_leaf_id: Some(root_source.to_string()),
        core_run_id: Some("fixture-root-replay-core-run".into()),
        harness_snapshot_id: Some(root_identity.snapshot_id().to_string()),
        harness_revision_id: Some(root_identity.revision_id().to_string()),
        model_harness_profile_id: Some(root_identity.profile_id().to_string()),
        provider_surface_digest: None,
        experiment_id: None,
    };
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity,
        root_services,
        artifacts: store,
        rollover_budget: 1,
        subagents: Some(SubagentServices {
            policy,
            host,
            tasks: Arc::clone(&tasks) as Arc<dyn TaskRuntime>,
        }),
    })
    .expect("fixture supervisor creates");
    (runtime, tasks, requests, reopen_count, cleanup_fail, finalizations, apply_outcomes, apply_requests, call, provenance, request)
}

#[test]
fn apply_agent_changes_intent_only_recovery_requires_host_reconciliation() {
    smol::block_on(async {
        let (runtime, _tasks, _requests, _reopens, _cleanup, _finalizations, _outcomes, apply_requests, ..) =
            build_active_spawn_replay_fixture_with_options(false, true, true, None);
        let before = runtime.snapshot().expect("snapshot reads").last_sequence();
        for _ in 0..2 {
            let error = runtime
                .resume()
                .await
                .expect_err("an ambiguous workspace mutation must stop typed recovery");
            assert!(matches!(
                error,
                HarnessError::RecoveryRequired {
                    plan: tea_session::RecoveryPlan::SynthesizeInterruptedToolResult {
                        ref result_entry_id,
                    },
                } if result_entry_id.as_str() == "fixture-root-replay-apply-result"
            ));
        }
        assert_eq!(
            runtime.snapshot().expect("snapshot reads").last_sequence(),
            before,
            "recovery-required classification must not append a generic retryable result"
        );
        assert!(
            apply_requests.lock().expect("apply request mutex").is_empty(),
            "ambiguous recovery must not call the workspace mutation port"
        );
    });
}

/// Seed a completed historical root operation whose child reuses the current
/// operation's task name. The graph deliberately orders this foreign node
/// before the current one so wait target resolution must filter ownership
/// before name matching.
fn append_finished_foreign_spawn_with_reused_task_name(
    session: &mut MemorySession,
    policy: &SubagentPolicy,
    root_identity: &HarnessIdentity,
    child_identity: &HarnessIdentity,
    root_source: &EntryId,
) {
    let parent_operation_id =
        OperationId::new("fixture-older-root-operation").expect("older operation ID");
    let epoch_id = EpochId::new("fixture-older-root-epoch").expect("older epoch ID");
    let assistant_id = EntryId::new("fixture-older-root-assistant").expect("older assistant ID");
    let tool_call_id = "fixture-older-root-spawn-call";
    let tool_args = JsonValue::parse(
        r#"{"task_name":"replay_task","task":"historical task","model":"child-model","context":"task"}"#,
    )
    .expect("older tool arguments parse");
    let agent_id = AgentId::derive(
        &SessionId::new("runtime-subagent-replay").expect("fixture session ID"),
        &LaneId::main(),
        &parent_operation_id,
        "fixture-older-root-spawn-key",
    );
    let lane_id = agent_id.lane_id();
    let model = SubagentModelRecord {
        provider: policy.models[0].descriptor.provider.clone(),
        model: policy.models[0].descriptor.model.clone(),
        revision: policy.models[0].descriptor.revision.clone(),
        display_name: policy.models[0].display_name.clone(),
        context_window: policy.models[0].context_window.map(std::num::NonZeroU64::get),
    };
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            parent_operation_id.clone(),
            LaneId::main(),
            Some(root_source.clone()),
            OperationKind::Run,
            Vec::new(),
            root_identity.revision_id().clone(),
            root_identity.profile_id().clone(),
        )))
        .expect("older root operation starts");
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: parent_operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(root_source.clone()),
            harness_revision_id: root_identity.revision_id().clone(),
            harness_snapshot_id: root_identity.snapshot_id().clone(),
            model_harness_profile: root_identity.profile_id().clone(),
            core_run_id: CoreRunId::new("fixture-older-root-core-run").expect("older core run ID"),
            epoch_resume_data: BTreeMap::new(),
        }))
        .expect("older root epoch starts");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::assistant(
                assistant_id.clone(),
                "",
                vec![AssistantToolCall::new(
                    tool_call_id,
                    "spawn_agent",
                    tool_args.clone(),
                )],
            ),
        )
        .expect("older spawn assistant commits");
    session
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new("fixture-older-root-spawn-record").expect("older tool record ID"),
            parent_operation_id.clone(),
            epoch_id,
            assistant_id,
            0,
            tool_call_id,
            "spawn_agent",
            tool_args,
            EntryId::new("fixture-older-root-spawn-result").expect("older result ID"),
            ToolReplayPolicy::Never,
            Digest::from_bytes("fixture older spawn definition"),
            root_identity.revision_id().clone(),
            "fixture-older-root-spawn-key",
        )))
        .expect("older spawn intent commits");
    session
        .append_lane_mutation(LaneMutation::Created {
            lane_id: lane_id.clone(),
            base_leaf_id: None,
        })
        .expect("older child lane creates");
    session
        .append_entry(
            &lane_id,
            ProvisionedEntry {
                id: EntryId::new("fixture-older-child-model").expect("older model entry ID"),
                body: SessionEntry::ModelChanged(ModelChangedEntry {
                    provider: model.provider.clone(),
                    model: model.model.clone(),
                    revision: model.revision.clone(),
                }),
            },
        )
        .expect("older child model commits");
    session
        .append_entry(
            &lane_id,
            ProvisionedEntry {
                id: EntryId::new("fixture-older-child-thinking").expect("older thinking entry ID"),
                body: SessionEntry::ThinkingChanged(ThinkingChangedEntry { level: "off".into() }),
            },
        )
        .expect("older child thinking commits");
    session
        .append_entry(
            &lane_id,
            ProvisionedEntry {
                id: EntryId::new("fixture-older-child-harness").expect("older harness entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: child_identity.revision_id().clone(),
                    snapshot_id: child_identity.snapshot_id().clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("older child harness commits");
    session
        .append_fact(SessionFact::AgentSpawned(tea_session::AgentSpawnedFact {
            agent_id: agent_id.clone(),
            parent_lane_id: LaneId::main(),
            parent_operation_id: parent_operation_id.clone(),
            lane_id,
            task_name: "replay_task".into(),
            model,
            thinking: "off".into(),
            context_mode: AgentContextMode::Task,
            base_leaf_id: None,
            workspace_lease_id: WorkspaceLeaseId::derive(&agent_id),
            harness_revision_id: child_identity.revision_id().clone(),
            harness_snapshot_id: child_identity.snapshot_id().clone(),
            model_harness_profile_id: child_identity.profile_id().clone(),
            spawn_tool_call_id: tool_call_id.into(),
        }))
        .expect("older child fact commits");
    session
        .append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
            epoch_id: EpochId::new("fixture-older-root-epoch").expect("older epoch ID"),
            operation_id: parent_operation_id.clone(),
            reason: EpochFinishReason::Settled,
        }))
        .expect("older root epoch finishes");
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            operation_id: parent_operation_id,
            outcome: OperationOutcome::Completed,
        }))
        .expect("older root finishes");
}

fn build_active_two_spawn_fixture() -> (
    Arc<SessionSupervisor<MemorySession>>,
    Arc<FixtureTaskRuntime>,
    RunProvenance,
    Vec<(ToolCall, SpawnAgentRequest)>,
) {
    let store = Arc::new(MemoryArtifactStore::default());
    let delta_patch_artifact = store
        .put(b"fixture child patch", "application/x-git-diff")
        .expect("fixture patch artifact stores")
        .artifact_id;
    let policy = fixture_subagent_policy();
    let (manager, root_identity, root_services, child_identity) = fixture_subagent_manager(
        Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::new()),
        }),
        Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::new()),
        }),
        store.clone(),
        &policy,
    );
    let host = Arc::new(FixtureSubagentHost {
        child_identity,
        allowed_models: policy
            .models
            .iter()
            .map(|model| (model.descriptor.model.clone(), model.descriptor.clone()))
            .collect(),
        requests: Arc::new(Mutex::new(Vec::new())),
        reopen_count: Arc::new(Mutex::new(0)),
        cleanup_fail: Arc::new(Mutex::new(false)),
        finalizations: Arc::new(Mutex::new(VecDeque::new())),
        delta_patch_artifact,
        apply_outcomes: Arc::new(Mutex::new(VecDeque::new())),
        apply_requests: Arc::new(Mutex::new(Vec::new())),
    });
    let tasks = Arc::new(FixtureTaskRuntime::accepting());
    let calls = vec![
        (
            ToolCall {
                id: ToolCallId::new("fixture-root-multi-z").expect("tool call ID"),
                name: "spawn_agent".into(),
                arguments: SerializedJson::new(
                    r#"{"task_name":"z_task","task":"inspect z","model":"child-model","context":"task"}"#,
                ),
            },
            SpawnAgentRequest {
                task_name: "z_task".into(),
                task: "inspect z".into(),
                model: "child-model".into(),
                thinking: None,
                context_mode: AgentContextMode::Task,
            },
        ),
        (
            ToolCall {
                id: ToolCallId::new("fixture-root-multi-a").expect("tool call ID"),
                name: "spawn_agent".into(),
                arguments: SerializedJson::new(
                    r#"{"task_name":"a_task","task":"inspect a","model":"child-model","context":"task"}"#,
                ),
            },
            SpawnAgentRequest {
                task_name: "a_task".into(),
                task: "inspect a".into(),
                model: "child-model".into(),
                thinking: None,
                context_mode: AgentContextMode::Task,
            },
        ),
    ];
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("runtime-subagent-multi").expect("fixture session ID"),
        "runtime-subagent-multi-workspace",
        fixture_metadata(),
    ))
    .expect("fixture session creates");
    append_subagent_policy(&mut session, &policy);
    append_initial_revision(&mut session, &root_identity);
    let root_source = EntryId::new("runtime-test-initial-revision").expect("root source ID");
    let operation_id = OperationId::new("fixture-root-multi-operation").expect("operation ID");
    let epoch_id = EpochId::new("fixture-root-multi-epoch").expect("epoch ID");
    let assistant_id = EntryId::new("fixture-root-multi-assistant").expect("assistant ID");
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            operation_id.clone(),
            LaneId::main(),
            Some(root_source.clone()),
            OperationKind::Run,
            Vec::new(),
            root_identity.revision_id().clone(),
            root_identity.profile_id().clone(),
        )))
        .expect("root operation starts");
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(root_source.clone()),
            harness_revision_id: root_identity.revision_id().clone(),
            harness_snapshot_id: root_identity.snapshot_id().clone(),
            model_harness_profile: root_identity.profile_id().clone(),
            core_run_id: CoreRunId::new("fixture-root-multi-core-run").expect("core run ID"),
            epoch_resume_data: BTreeMap::new(),
        }))
        .expect("root epoch starts");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry::assistant(
                assistant_id.clone(),
                "",
                calls
                    .iter()
                    .map(|(call, _)| {
                        AssistantToolCall::new(
                            call.id.to_string(),
                            call.name.clone(),
                            JsonValue::parse(call.arguments.as_str()).expect("fixture tool args parse"),
                        )
                    })
                    .collect(),
            ),
        )
        .expect("assistant spawn calls commit");
    for (index, (call, _)) in calls.iter().enumerate() {
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
                RecordId::new(format!("fixture-root-multi-tool-record-{index}"))
                    .expect("tool record ID"),
                operation_id.clone(),
                epoch_id.clone(),
                assistant_id.clone(),
                index as u32,
                call.id.to_string(),
                "spawn_agent",
                JsonValue::parse(call.arguments.as_str()).expect("fixture tool args parse"),
                EntryId::new(format!("fixture-root-multi-tool-result-{index}"))
                    .expect("tool result ID"),
                ToolReplayPolicy::Never,
                Digest::from_bytes(format!("fixture spawn definition {index}").as_bytes()),
                root_identity.revision_id().clone(),
                format!("fixture-root-multi-key-{index}"),
            )))
            .expect("spawn tool intent commits");
    }
    let provenance = RunProvenance {
        session_id: Some("runtime-subagent-multi".into()),
        lane_id: Some(LaneId::main().to_string()),
        agent_id: None,
        operation_id: Some(operation_id.to_string()),
        epoch_id: Some(epoch_id.to_string()),
        source_leaf_id: Some(root_source.to_string()),
        core_run_id: Some("fixture-root-multi-core-run".into()),
        harness_snapshot_id: Some(root_identity.snapshot_id().to_string()),
        harness_revision_id: Some(root_identity.revision_id().to_string()),
        model_harness_profile_id: Some(root_identity.profile_id().to_string()),
        provider_surface_digest: None,
        experiment_id: None,
    };
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity,
        root_services,
        artifacts: store,
        rollover_budget: 1,
        subagents: Some(SubagentServices {
            policy,
            host,
            tasks: Arc::clone(&tasks) as Arc<dyn TaskRuntime>,
        }),
    })
    .expect("fixture supervisor creates");
    (runtime, tasks, provenance, calls)
}

fn fixture_apply_call(agent_id: &AgentId) -> ToolCall {
    let delta_id = WorkspaceDeltaId::derive(
        &WorkspaceLeaseId::derive(agent_id),
        "fixture-child-base",
        "fixture-child-result",
    );
    ToolCall {
        id: ToolCallId::new("fixture-root-replay-apply-call").expect("tool call ID"),
        name: "apply_agent_changes".into(),
        arguments: SerializedJson::new(
            JsonValue::object([("delta_id", JsonValue::String(delta_id.to_string()))])
                .to_json_string()
                .expect("fixture apply arguments encode"),
        ),
    }
}

fn spawn_stream(call_id: &str, context: AgentContextMode) -> ModelStream {
    spawn_batch(vec![spawn_call(
        call_id,
        "audit_session",
        "inspect the durable session",
        "child-model",
        None,
        context,
    )])
}

fn spawn_call(
    call_id: &str,
    task_name: &str,
    task: &str,
    model: &str,
    thinking: Option<&str>,
    context: AgentContextMode,
) -> AgentToolCall {
    let context = match context {
        AgentContextMode::Task => "task",
        AgentContextMode::Parent => "parent",
    };
    let mut fields = vec![
        ("task_name", JsonValue::String(task_name.into())),
        ("task", JsonValue::String(task.into())),
        ("model", JsonValue::String(model.into())),
        ("context", JsonValue::String(context.into())),
    ];
    if let Some(thinking) = thinking {
        fields.push(("thinking", JsonValue::String(thinking.into())));
    }
    AgentToolCall {
        id: ToolCallId::new(call_id).expect("fixture spawn call ID"),
        name: "spawn_agent".into(),
        arguments: SerializedJson::new(
            JsonValue::object(fields)
                .to_json_string()
                .expect("fixture spawn arguments encode"),
        ),
    }
}

fn spawn_batch(calls: Vec<AgentToolCall>) -> ModelStream {
    let mut events = calls
        .into_iter()
        .map(ModelStreamEvent::ToolCall)
        .collect::<Vec<_>>();
    events.push(ModelStreamEvent::End(StopReason::ToolUse));
    ModelStream { events }
}

fn completion_stream() -> ModelStream {
    ModelStream {
        events: vec![
            ModelStreamEvent::TextDelta("root continues without child output".into()),
            ModelStreamEvent::End(StopReason::Stop),
        ],
    }
}

#[test]
fn subagent_spawn_accepts_a_task_context_child_and_hands_it_to_the_task_runtime() {
    smol::block_on(async {
        let (runtime, requests, tasks) = build_subagent_runtime(
            "runtime-subagent-task-context",
            vec![spawn_stream("spawn-task-context", AgentContextMode::Task), completion_stream()],
        );
        runtime
            .run_root_prompt("delegate an isolated review")
            .await
            .expect("root operation settles after accepting the child");

        let requests = requests.lock().expect("fixture request mutex");
        assert_eq!(requests.len(), 1, "one workspace is prepared");
        let request = &requests[0];
        assert_eq!(request.context_mode, AgentContextMode::Task);
        assert_eq!(request.parent_source_leaf_id, None);
        assert!(
            request.workspace_source_leaf_id.is_some(),
            "workspace snapshots use the current parent leaf, not task semantic context"
        );
        assert_eq!(
            *tasks.accepted.lock().expect("fixture task count mutex"),
            1,
            "the child operation is handed to the explicit task runtime before spawn settles"
        );
        assert_eq!(
            tasks.owned_task_count(),
            0,
            "root settlement joins and removes an unobserved child task"
        );
        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("spawned child graph reduces");
        assert_eq!(graph.agents.len(), 1);
        let child = graph.agents.values().next().expect("child exists");
        assert!(child.operation_id.is_some(), "child operation is accepted");
        assert_eq!(
            child.state,
            AgentState::Interrupted,
            "root settlement cancels an unobserved child before finishing its own operation"
        );
        assert_eq!(child.spawned.workspace_lease_id, WorkspaceLeaseId::derive(&child.spawned.agent_id));
    });
}

#[test]
fn subagent_spawn_intent_is_durably_replayable_before_any_child_fact_exists() {
    smol::block_on(async {
        let (runtime, _requests, _tasks) = build_subagent_runtime(
            "runtime-subagent-intent-replay-policy",
            vec![
                spawn_stream("spawn-replay-policy", AgentContextMode::Task),
                completion_stream(),
            ],
        );
        runtime
            .run_root_prompt("delegate with crash-safe intent")
            .await
            .expect("fixture root settles");

        let snapshot = runtime.snapshot().expect("snapshot reads");
        let spawn = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::ToolStarted(started) if started.tool_name == "spawn_agent" => {
                    Some(started)
                }
                _ => None,
            })
            .expect("spawn intent is durable");
        assert_eq!(
            spawn.replay_policy_at_start,
            ToolReplayPolicy::Safe,
            "an intent-only crash must resume the deterministic spawn transaction"
        );
    });
}

#[test]
fn subagent_spawn_intent_only_resume_replays_exactly_one_child_transaction() {
    smol::block_on(async {
        let (
            runtime,
            tasks,
            requests,
            _reopens,
            _cleanup,
            _finalizations,
            _outcomes,
            _apply_requests,
            _call,
            _provenance,
            _request,
        ) = build_active_spawn_replay_fixture_with_options(false, false, false, None);

        runtime
            .resume()
            .await
            .expect("an intent-only spawn resumes the deterministic transaction");

        let snapshot = runtime.snapshot().expect("recovered snapshot reads");
        let graph = reduce_agent_graph(&snapshot).expect("recovered child graph reduces");
        assert_eq!(graph.agents.len(), 1, "replay derives one durable child");
        let child = graph.agents.values().next().expect("replayed child exists");
        assert!(
            child.operation_id.is_some(),
            "replay accepts the child operation before settling the root"
        );
        assert_eq!(
            requests.lock().expect("fixture request mutex").len(),
            1,
            "replay prepares one isolated workspace"
        );
        assert_eq!(
            *tasks.accepted.lock().expect("fixture task count mutex"),
            1,
            "replay hands one child operation to structured task ownership"
        );
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.body,
                        SessionEntry::ToolResult(result)
                            if result.tool_call_id == "fixture-root-replay-call"
                                && result.tool_name == "spawn_agent"
                    )
                })
                .count(),
            1,
            "the recovered spawn intent has exactly one durable tool result"
        );

        let recovered_sequence = snapshot.last_sequence();
        let repeated = runtime
            .resume()
            .await
            .expect_err("a settled operation has no recovery obligation");
        assert!(
            repeated.to_string().contains("no open operation"),
            "repeat recovery reports the settled boundary instead of replaying again"
        );
        assert_eq!(
            runtime.snapshot().expect("repeated snapshot reads").last_sequence(),
            recovered_sequence,
            "repeat recovery cannot duplicate the child transaction"
        );
    });
}

#[test]
fn subagent_spawn_replay_inherits_durable_parent_thinking_after_reopen() {
    smol::block_on(async {
        let (
            runtime,
            tasks,
            requests,
            _reopens,
            _cleanup,
            _finalizations,
            _outcomes,
            _apply_requests,
            _call,
            _provenance,
            _request,
        ) = build_active_spawn_replay_fixture_with_options(
            false,
            false,
            false,
            Some(crate::state::ThinkingLevel::High),
        );
        let session = runtime
            .clone_session_for_test()
            .expect("intent-only session clones for restart fixture");
        let (resolver, root_services, artifacts, subagents) = runtime
            .reopen_parts_for_test()
            .expect("reopen parts remain host-owned");
        tasks.lose_owned_tasks_for_restart();
        drop(runtime);

        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services: root_services.thinking_level(crate::state::ThinkingLevel::Off),
            lane_services: BTreeMap::new(),
            artifacts,
            rollover_budget: 1,
            subagents,
        })
        .expect("reopen hydrates the durable root configuration");
        reopened
            .resume()
            .await
            .expect("spawn recovery inherits the durable parent thinking");

        assert_eq!(
            requests.lock().expect("fixture request mutex")[0].thinking,
            crate::state::ThinkingLevel::High,
            "host preparation receives the reduced parent thinking, not the reopened service default"
        );
        let graph = reduce_agent_graph(&reopened.snapshot().expect("recovered snapshot reads"))
            .expect("recovered graph reduces");
        assert_eq!(
            graph
                .agents
                .values()
                .next()
                .expect("replayed child exists")
                .spawned
                .thinking,
            "high",
            "the replayed child retains the exact durable inherited identity"
        );
    });
}

#[test]
fn subagent_spawn_parent_context_uses_the_epoch_source_leaf_not_the_later_workspace_leaf() {
    smol::block_on(async {
        let (runtime, requests, _tasks) = build_subagent_runtime(
            "runtime-subagent-parent-context",
            vec![spawn_stream("spawn-parent-context", AgentContextMode::Parent), completion_stream()],
        );
        runtime
            .run_root_prompt("delegate with exact parent context")
            .await
            .expect("root operation settles after accepting the child");

        let request = requests
            .lock()
            .expect("fixture request mutex")
            .pop()
            .expect("one workspace request");
        assert_eq!(request.context_mode, AgentContextMode::Parent);
        assert!(request.parent_source_leaf_id.is_some());
        assert_ne!(
            request.parent_source_leaf_id, request.workspace_source_leaf_id,
            "parent semantic context is frozen to the epoch source, while the repository snapshot follows source order"
        );
        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("spawned child graph reduces");
        let child = graph.agents.values().next().expect("child exists");
        assert_eq!(child.spawned.base_leaf_id, request.parent_source_leaf_id);
    });
}

#[test]
fn subagent_spawn_rejects_invalid_names_and_disallowed_models_before_workspace_preparation() {
    smol::block_on(async {
        for (session_id, call) in [
            (
                "runtime-subagent-invalid-name",
                spawn_call(
                    "invalid-task-name",
                    "Not_valid",
                    "inspect the durable session",
                    "child-model",
                    None,
                    AgentContextMode::Task,
                ),
            ),
            (
                "runtime-subagent-disallowed-model",
                spawn_call(
                    "disallowed-model",
                    "audit_session",
                    "inspect the durable session",
                    "not-authorized",
                    None,
                    AgentContextMode::Task,
                ),
            ),
        ] {
            let (runtime, requests, tasks) =
                build_subagent_runtime(session_id, vec![spawn_batch(vec![call]), completion_stream()]);
            runtime
                .run_root_prompt("attempt a rejected delegation")
                .await
                .expect("recoverable spawn rejection leaves the root runnable");
            assert!(
                requests.lock().expect("fixture request mutex").is_empty(),
                "rejected spawn must not prepare a workspace"
            );
            assert_eq!(*tasks.accepted.lock().expect("fixture task count mutex"), 0);
            assert!(
                reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
                    .expect("graph reduces")
                    .agents
                    .is_empty(),
                "rejected spawn must not create a durable child"
            );
        }
    });
}

#[test]
fn subagent_spawn_rejects_a_root_collaboration_harness_prepared_for_a_child() {
    smol::block_on(async {
        let (runtime, requests, tasks) = build_subagent_runtime_with_child_surface(
            "runtime-subagent-root-child-surface",
            vec![spawn_stream("root-as-child", AgentContextMode::Task), completion_stream()],
            fixture_subagent_policy(),
            true,
        );
        runtime
            .run_root_prompt("attempt an invalid child harness")
            .await
            .expect("the root receives a recoverable child-harness rejection");
        assert_eq!(requests.lock().expect("fixture request mutex").len(), 1);
        assert_eq!(*tasks.accepted.lock().expect("fixture task count mutex"), 0);
        assert!(
            reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
                .expect("graph reduces")
                .agents
                .is_empty(),
            "a root collaboration surface can never become a durable child lane"
        );
    });
}

#[test]
fn subagent_spawn_inherits_or_explicitly_overrides_parent_thinking() {
    smol::block_on(async {
        let (defaulted, defaulted_requests, _) = build_subagent_runtime(
            "runtime-subagent-default-thinking",
            vec![spawn_stream("default-thinking", AgentContextMode::Task), completion_stream()],
        );
        defaulted
            .run_root_prompt("delegate with the host default thinking")
            .await
            .expect("root settles");
        let defaulted_snapshot = defaulted.snapshot().expect("snapshot reads");
        assert!(
            defaulted_snapshot.entries().iter().all(|entry| {
                entry.lane_id != LaneId::main()
                    || !matches!(entry.body, SessionEntry::ThinkingChanged(_))
            }),
            "the valid host default need not be materialized as a parent lane entry"
        );
        let defaulted_spawn = defaulted_snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::ToolStarted(started) if started.tool_name == "spawn_agent" => {
                    Some(started)
                }
                _ => None,
            })
            .expect("defaulted spawn intent is durable");
        assert!(
            defaulted_spawn.effective_args.get("thinking").is_none(),
            "omitted thinking remains an inheritance request in the durable tool arguments"
        );
        assert_eq!(
            defaulted_requests.lock().expect("fixture request mutex")[0].thinking,
            crate::state::ThinkingLevel::Off,
            "the host receives the resolved default before preparing child services"
        );
        assert_eq!(
            reduce_agent_graph(&defaulted_snapshot)
                .expect("defaulted graph reduces")
                .agents
                .values()
                .next()
                .expect("defaulted child exists")
                .spawned
                .thinking,
            "off",
            "the spawn fact is the first durable resolved default"
        );

        let (inherited, inherited_requests, _) = build_subagent_runtime(
            "runtime-subagent-inherited-thinking",
            vec![spawn_stream("inherit-thinking", AgentContextMode::Task), completion_stream()],
        );
        inherited
            .replace_thinking_level(crate::state::ThinkingLevel::High)
            .expect("idle parent thinking changes");
        inherited
            .run_root_prompt("delegate with inherited thinking")
            .await
            .expect("root settles");
        let inherited_graph = reduce_agent_graph(&inherited.snapshot().expect("snapshot reads"))
            .expect("graph reduces");
        assert_eq!(
            inherited_requests.lock().expect("fixture request mutex")[0].thinking,
            crate::state::ThinkingLevel::High,
            "the host receives the resolved inherited thinking level before it builds services"
        );
        assert_eq!(
            inherited_graph
                .agents
                .values()
                .next()
                .expect("child exists")
                .spawned
                .thinking,
            "high"
        );

        let (explicit, explicit_requests, _) = build_subagent_runtime(
            "runtime-subagent-explicit-thinking",
            vec![
                spawn_batch(vec![spawn_call(
                    "explicit-thinking",
                    "audit_session",
                    "inspect the durable session",
                    "child-model",
                    Some("low"),
                    AgentContextMode::Task,
                )]),
                completion_stream(),
            ],
        );
        explicit
            .replace_thinking_level(crate::state::ThinkingLevel::High)
            .expect("idle parent thinking changes");
        explicit
            .run_root_prompt("delegate with explicit thinking")
            .await
            .expect("root settles");
        let explicit_graph = reduce_agent_graph(&explicit.snapshot().expect("snapshot reads"))
            .expect("graph reduces");
        assert_eq!(
            explicit_requests.lock().expect("fixture request mutex")[0].thinking,
            crate::state::ThinkingLevel::Low,
            "explicit thinking reaches the host before lane registration"
        );
        assert_eq!(
            explicit_graph
                .agents
                .values()
                .next()
                .expect("child exists")
                .spawned
                .thinking,
            "low"
        );
    });
}

#[test]
fn subagent_spawn_rejects_duplicate_task_names_and_enforces_active_capacity() {
    smol::block_on(async {
        let (duplicate, duplicate_requests, duplicate_tasks) = build_subagent_runtime(
            "runtime-subagent-duplicate-task",
            vec![
                spawn_batch(vec![
                    spawn_call(
                        "duplicate-first",
                        "same_task",
                        "inspect first assignment",
                        "child-model",
                        None,
                        AgentContextMode::Task,
                    ),
                    spawn_call(
                        "duplicate-second",
                        "same_task",
                        "inspect second assignment",
                        "child-model",
                        None,
                        AgentContextMode::Task,
                    ),
                ]),
                completion_stream(),
            ],
        );
        duplicate
            .run_root_prompt("try two equal task names")
            .await
            .expect("root settles after recoverable duplicate rejection");
        assert_eq!(
            reduce_agent_graph(&duplicate.snapshot().expect("snapshot reads"))
                .expect("graph reduces")
                .agents
                .len(),
            1
        );
        assert_eq!(duplicate_requests.lock().expect("fixture request mutex").len(), 1);
        assert_eq!(*duplicate_tasks.accepted.lock().expect("fixture task count mutex"), 1);

        let mut limited = fixture_subagent_policy();
        limited.max_concurrent = NonZeroU32::new(1).expect("fixture limit");
        limited.max_total_per_operation = NonZeroU32::new(1).expect("fixture limit");
        let (capacity, capacity_requests, capacity_tasks) = build_subagent_runtime_with_policy(
            "runtime-subagent-active-capacity",
            vec![
                spawn_batch(vec![
                    spawn_call(
                        "capacity-first",
                        "first_task",
                        "inspect first assignment",
                        "child-model",
                        None,
                        AgentContextMode::Task,
                    ),
                    spawn_call(
                        "capacity-second",
                        "second_task",
                        "inspect second assignment",
                        "child-model",
                        None,
                        AgentContextMode::Task,
                    ),
                ]),
                completion_stream(),
            ],
            limited,
        );
        capacity
            .run_root_prompt("fill child capacity")
            .await
            .expect("root settles after recoverable capacity rejection");
        assert_eq!(
            reduce_agent_graph(&capacity.snapshot().expect("snapshot reads"))
                .expect("graph reduces")
                .agents
                .len(),
            1
        );
        assert_eq!(capacity_requests.lock().expect("fixture request mutex").len(), 1);
        assert_eq!(*capacity_tasks.accepted.lock().expect("fixture task count mutex"), 1);
    });
}

#[test]
fn subagent_spawn_keeps_source_order_and_accepts_distinct_models_from_one_provider() {
    smol::block_on(async {
        let mut policy = fixture_subagent_policy();
        policy.models.push(SubagentModel {
            descriptor: ModelDescriptor {
                provider: "fixture".into(),
                model: "child-model-b".into(),
                revision: Some("fixture-child-r2".into()),
            },
            display_name: "Fixture child B".into(),
            context_window: None,
        });
        let (runtime, requests, tasks) = build_subagent_runtime_with_policy(
            "runtime-subagent-source-order",
            vec![
                spawn_batch(vec![
                    spawn_call(
                        "source-first",
                        "first_task",
                        "inspect first assignment",
                        "child-model",
                        None,
                        AgentContextMode::Parent,
                    ),
                    spawn_call(
                        "source-second",
                        "second_task",
                        "inspect second assignment",
                        "child-model-b",
                        None,
                        AgentContextMode::Parent,
                    ),
                ]),
                completion_stream(),
            ],
            policy,
        );
        runtime
            .run_root_prompt("delegate two independent tasks")
            .await
            .expect("root settles");
        let requests = requests.lock().expect("fixture request mutex");
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].model.descriptor.model, "child-model");
        assert_eq!(requests[1].model.descriptor.model, "child-model-b");
        assert_eq!(
            requests[0].parent_source_leaf_id, requests[1].parent_source_leaf_id,
            "all calls from the assistant response share the epoch semantic source"
        );
        assert_ne!(
            requests[0].workspace_source_leaf_id, requests[1].workspace_source_leaf_id,
            "the second sequential spawn snapshots after the first durable tool result"
        );
        assert_eq!(*tasks.accepted.lock().expect("fixture task count mutex"), 2);
        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("graph reduces");
        assert_eq!(graph.agents.len(), 2);
        assert_eq!(
            graph
                .agents
                .values()
                .map(|node| node.spawned.model.model.as_str())
                .collect::<Vec<_>>(),
            ["child-model", "child-model-b"]
        );
    });
}

#[test]
fn subagent_spawn_uses_the_configured_alternate_provider_catalog() {
    smol::block_on(async {
        let mut policy = fixture_subagent_policy();
        policy.models[0].descriptor.provider = "alternate-fixture".into();
        policy.models[0].descriptor.model = "alternate-model".into();
        let (runtime, requests, _) = build_subagent_runtime_with_policy(
            "runtime-subagent-alternate-provider",
            vec![
                spawn_batch(vec![spawn_call(
                    "alternate-provider",
                    "alternate_task",
                    "inspect alternate provider assignment",
                    "alternate-model",
                    None,
                    AgentContextMode::Task,
                )]),
                completion_stream(),
            ],
            policy,
        );
        runtime
            .run_root_prompt("delegate through the alternate provider catalog")
            .await
            .expect("root settles");
        assert_eq!(
            requests.lock().expect("fixture request mutex")[0]
                .model
                .descriptor
                .provider,
            "alternate-fixture"
        );
    });
}

#[test]
fn subagent_spawn_replay_reuses_one_durable_child_and_reacquires_task_ownership() {
    smol::block_on(async {
        let (runtime, tasks, requests, reopen_count, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        tasks.reject_once();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let first = runtime
            .accept_subagent_spawn(&coordinator, call.clone(), provenance.clone(), request.clone())
            .await
            .expect_err("task-runtime refusal remains a recoverable durable prefix");
        assert!(first.to_string().contains("refused"));
        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("durable prefix reduces");
        assert_eq!(graph.agents.len(), 1, "first attempt committed one child only");
        assert_eq!(
            graph.agents.values().next().expect("child exists").state,
            AgentState::Running
        );
        assert_eq!(requests.lock().expect("fixture request mutex").len(), 1);

        let mut mismatch = request.clone();
        mismatch.task_name = "different_task".into();
        assert!(
            runtime
                .accept_subagent_spawn(&coordinator, call.clone(), provenance.clone(), mismatch)
                .await
                .is_err(),
            "a same-idempotency replay with mismatched durable task data is rejected"
        );

        let handle = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("exact replay reopens the child lease and retries task handoff");
        assert_eq!(handle.state, AgentState::Running);
        assert_eq!(requests.lock().expect("fixture request mutex").len(), 1);
        assert_eq!(*reopen_count.lock().expect("fixture reopen mutex"), 1);
        assert_eq!(
            *tasks.accepted.lock().expect("fixture task count mutex"),
            1,
            "only the successful replay task is supervisor-owned"
        );
        assert_eq!(
            reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
                .expect("graph reduces")
                .agents
                .len(),
            1,
            "replay cannot append a duplicate child graph node"
        );
    });
}

#[test]
fn subagent_reopen_registers_and_resumes_an_open_child_before_root_waits() {
    smol::block_on(async {
        let (runtime, tasks, _requests, reopen_count, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("child operation accepts");
        let child_epoch = runtime
            .start_subagent_epoch_for_test(&spawned.agent_id)
            .expect("fixture commits an open child epoch prefix");
        runtime
            .append_subagent_open_tool_prefix_for_test(&spawned.agent_id, &child_epoch)
            .expect("fixture commits an open child tool prefix");
        let child = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("child graph reduces")
            .agents
            .get(&spawned.agent_id)
            .cloned()
            .expect("child exists");
        let child_identity = HarnessIdentity::new(
            child.spawned.harness_revision_id.clone(),
            child.spawned.harness_snapshot_id.clone(),
            child.spawned.model_harness_profile_id.clone(),
        );
        assert_eq!(
            runtime
                .provenance_for_test(
                    &child.spawned.lane_id,
                    &spawned.operation_id,
                    &child_epoch,
                    &child_identity,
                )
                .expect("child provenance derives from durable graph")
                .agent_id,
            Some(spawned.agent_id.to_string()),
            "child traces carry only their exact durable AgentId"
        );
        let session = runtime
            .clone_session_for_test()
            .expect("durable session clones for restart fixture");
        let (resolver, root_services, artifacts, subagents) = runtime
            .reopen_parts_for_test()
            .expect("reopen parts remain host-owned");
        tasks.lose_owned_tasks_for_restart();
        drop(runtime);

        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            lane_services: BTreeMap::new(),
            artifacts,
            rollover_budget: 1,
            subagents,
        })
        .expect("reopen reconstructs root from durable identity");
        reopened
            .recover_subagents_for_test()
            .await
            .expect("open child is restored before root recovery continues");
        let recovered_coordinator = reopened
            .subagent_coordinator_for_test()
            .expect("reopened coordinator exists");
        assert!(
            recovered_coordinator.has_handle(&spawned.agent_id),
            "reopen owns a replacement structured child task"
        );
        assert_eq!(
            *reopen_count.lock().expect("fixture reopen mutex"),
            1,
            "the durable child lease is reattached exactly once"
        );
        assert_eq!(
            tasks.owned_task_count(),
            1,
            "the old volatile task is gone and one recovered task is registered"
        );
        assert_eq!(
            reopened
                .snapshot()
                .expect("recovered snapshot reads")
                .records()
                .iter()
                .filter(|stored| matches!(&stored.record, LaneRecord::EpochStarted(record) if record.id == child_epoch))
                .count(),
            1,
            "recovery registers a task for the existing epoch instead of starting a fresh epoch"
        );
    });
}

#[test]
fn subagent_reopen_reports_missing_live_workspace_with_a_typed_error() {
    smol::block_on(async {
        let (runtime, tasks, _requests, _reopen_count, cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("child operation accepts");
        let session = runtime
            .clone_session_for_test()
            .expect("durable session clones for restart fixture");
        let (resolver, root_services, artifacts, subagents) = runtime
            .reopen_parts_for_test()
            .expect("reopen parts remain host-owned");
        tasks.lose_owned_tasks_for_restart();
        *cleanup_fail.lock().expect("fixture host failure mutex") = true;
        drop(runtime);
        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            lane_services: BTreeMap::new(),
            artifacts,
            rollover_budget: 1,
            subagents,
        })
        .expect("reopen reconstructs the root before live child recovery");
        assert_eq!(
            reopened
                .recover_subagents_for_test()
                .await
                .expect_err("an open child cannot recover without host workspace authority"),
            crate::harness::HarnessError::SubagentRecovery {
                agent_id: spawned.agent_id,
                stage: crate::harness::SubagentRecoveryStage::ReopenWorkspace,
            },
            "missing live state crosses the provider-neutral boundary as a typed recovery error"
        );
    });
}

#[test]
fn subagent_terminal_recovery_cleans_idempotently_without_reopening_a_worktree() {
    smol::block_on(async {
        let (runtime, tasks, _requests, reopen_count, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
            .await
            .expect("child operation accepts");
        coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance,
                },
                &spawned.agent_id.to_string(),
            )
            .await
            .expect("child terminal report and initial cleanup are durable");
        let session = runtime
            .clone_session_for_test()
            .expect("durable terminal prefix clones for restart fixture");
        let (resolver, root_services, artifacts, subagents) = runtime
            .reopen_parts_for_test()
            .expect("reopen parts remain host-owned");
        tasks.lose_owned_tasks_for_restart();
        drop(runtime);
        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            lane_services: BTreeMap::new(),
            artifacts,
            rollover_budget: 1,
            subagents,
        })
        .expect("reopen reconstructs the root");
        reopened
            .recover_subagents_for_test()
            .await
            .expect("terminal recovery performs cleanup-only work");
        let recovered = reopened
            .subagent_coordinator_for_test()
            .expect("reopened coordinator exists");
        assert!(recovered.is_exposable(&spawned.agent_id));
        assert_eq!(
            *reopen_count.lock().expect("fixture reopen mutex"),
            0,
            "a terminal child cleanup never requests a removed workspace to reopen"
        );
    });
}

#[test]
fn subagent_reopen_finishes_a_durable_delta_prefix_without_losing_its_report() {
    smol::block_on(async {
        let (runtime, tasks, _requests, _reopen_count, _cleanup_fail, finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("child operation accepts");
        runtime
            .append_subagent_assistant_for_test(&spawned.agent_id, "recoverable delta report".into())
            .expect("fixture assistant report appends");
        finalizations
            .lock()
            .expect("fixture finalization mutex")
            .push_back(FixtureFinalization::Delta);
        runtime
            .persist_subagent_delta_without_terminal_for_test(&coordinator, &spawned.agent_id)
            .await
            .expect("durable delta prefix persists before terminal report");
        let prefix = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("delta prefix reduces");
        assert!(prefix.agents[&spawned.agent_id].workspace_delta.is_some());
        assert!(prefix.agents[&spawned.agent_id].terminal.is_none());
        let session = runtime
            .clone_session_for_test()
            .expect("durable delta prefix clones for restart fixture");
        let (resolver, root_services, artifacts, subagents) = runtime
            .reopen_parts_for_test()
            .expect("reopen parts remain host-owned");
        tasks.lose_owned_tasks_for_restart();
        drop(runtime);
        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            lane_services: BTreeMap::new(),
            artifacts,
            rollover_budget: 1,
            subagents,
        })
        .expect("reopen reconstructs root from durable identity");
        reopened
            .recover_subagents_for_test()
            .await
            .expect("recovery finalizes delta-only prefix before root can wait");
        let graph = reduce_agent_graph(&reopened.snapshot().expect("snapshot reads"))
            .expect("recovered delta graph reduces");
        let child = &graph.agents[&spawned.agent_id];
        assert!(child.terminal.is_some());
        assert!(child.workspace_delta.is_some(), "existing immutable delta is retained");
        assert_eq!(
            child.terminal.as_ref().expect("terminal report exists").report,
            PayloadRef::Inline(JsonValue::String("recoverable delta report".into())),
        );
        assert!(
            reopened
                .subagent_coordinator_for_test()
                .expect("reopened coordinator exists")
                .is_exposable(&spawned.agent_id),
            "terminal report becomes parent-visible only after recovery cleanup"
        );
    });
}

#[test]
fn subagent_terminalization_retains_inline_and_artifact_reports_with_nochange_or_delta() {
    smol::block_on(async {
        let (inline_runtime, _tasks, _requests, _reopen_count, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let inline_coordinator = inline_runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let inline_child = inline_runtime
            .accept_subagent_spawn(&inline_coordinator, call, provenance.clone(), request)
            .await
            .expect("inline child accepts");
        inline_runtime
            .append_subagent_assistant_for_test(&inline_child.agent_id, "short final report".into())
            .expect("fixture assistant report appends");
        inline_coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance,
                },
                &inline_child.agent_id.to_string(),
            )
            .await
            .expect("no-change child terminalizes");
        let inline_graph = reduce_agent_graph(&inline_runtime.snapshot().expect("snapshot reads"))
            .expect("inline terminal graph reduces");
        let inline = &inline_graph.agents[&inline_child.agent_id];
        assert_eq!(inline.state, AgentState::Interrupted);
        assert!(inline.workspace_delta.is_none(), "no-change completion has no synthetic delta");
        assert_eq!(
            inline.terminal.as_ref().expect("terminal report exists").report,
            PayloadRef::Inline(JsonValue::String("short final report".into())),
        );

        let (artifact_runtime, _tasks, _requests, _reopen_count, _cleanup_fail, finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let artifact_coordinator = artifact_runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let artifact_child = artifact_runtime
            .accept_subagent_spawn(&artifact_coordinator, call, provenance.clone(), request)
            .await
            .expect("artifact child accepts");
        let oversized = "x".repeat(32 * 1024 + 1);
        artifact_runtime
            .append_subagent_assistant_for_test(&artifact_child.agent_id, oversized.clone())
            .expect("fixture oversized assistant report appends");
        finalizations
            .lock()
            .expect("fixture finalization mutex")
            .push_back(FixtureFinalization::Delta);
        let wait_provenance = provenance.clone();
        artifact_coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance,
                },
                &artifact_child.agent_id.to_string(),
            )
            .await
            .expect("interrupted child finalizes its salvageable delta");
        let artifact_graph = reduce_agent_graph(&artifact_runtime.snapshot().expect("snapshot reads"))
            .expect("artifact terminal graph reduces");
        let artifact = &artifact_graph.agents[&artifact_child.agent_id];
        assert!(matches!(
            artifact.state,
            AgentState::DeltaReady {
                outcome: OperationOutcome::Aborted,
                ..
            }
        ));
        assert!(artifact.workspace_delta.is_some(), "interrupted changes remain salvageable");
        let PayloadRef::Artifact { artifact_id, byte_len, .. } = &artifact
            .terminal
            .as_ref()
            .expect("terminal report exists")
            .report
        else {
            panic!("oversized report is retained as an immutable artifact");
        };
        assert_eq!(*byte_len, oversized.len() as u64);
        let waited = artifact_coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: wait_provenance,
                },
                WaitAgentsRequest {
                    targets: vec![artifact_child.agent_id.to_string()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect("cleanup-ready artifact report is visible to the owning root operation");
        let report = &waited.completed[0].report;
        assert!(
            report.preview.len() <= 16 * 1024,
            "artifact reports expose only a bounded parent-facing preview"
        );
        assert_eq!(report.artifact_id.as_ref(), Some(artifact_id));
        assert!(!artifact_id.to_string().is_empty());
    });
}

#[test]
fn subagent_spawn_replay_completes_each_partial_lane_binding_prefix() {
    smol::block_on(async {
        for configured_entries in 0..=3 {
            let (runtime, tasks, _requests, _reopen_count, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
                build_active_spawn_replay_fixture();
            let coordinator = runtime
                .subagent_coordinator_for_test()
                .expect("fixture coordinator exists");
            let agent_id = runtime
                .persist_subagent_prefix_for_test(
                    &coordinator,
                    call.clone(),
                    provenance.clone(),
                    request.clone(),
                    configured_entries,
                    false,
                )
                .await
                .expect("deterministic partial prefix persists");
            let spawned = runtime
                .accept_subagent_spawn(&coordinator, call, provenance, request)
                .await
                .expect("normal spawn completes only the missing durable binding suffix");
            assert_eq!(spawned.agent_id, agent_id);
            let snapshot = runtime.snapshot().expect("snapshot reads");
            assert_eq!(
                snapshot
                    .lane_mutations()
                    .iter()
                    .filter(|stored| matches!(&stored.mutation, LaneMutation::Created { lane_id, .. } if lane_id == &agent_id.lane_id()))
                    .count(),
                1,
                "partial prefix {configured_entries} does not duplicate lane creation"
            );
            for kind in ["model", "thinking", "harness"] {
                assert_eq!(
                    snapshot
                        .entries()
                        .iter()
                        .filter(|entry| {
                            entry.lane_id == agent_id.lane_id()
                                && match kind {
                                    "model" => matches!(entry.body, SessionEntry::ModelChanged(_)),
                                    "thinking" => matches!(entry.body, SessionEntry::ThinkingChanged(_)),
                                    "harness" => matches!(entry.body, SessionEntry::HarnessRevisionChanged(_)),
                                    _ => false,
                                }
                        })
                        .count(),
                    1,
                    "partial prefix {configured_entries} has one durable {kind} binding"
                );
            }
            assert_eq!(
                reduce_agent_graph(&snapshot)
                    .expect("completed prefix reduces")
                    .agents[&agent_id]
                    .state,
                AgentState::Running,
            );
            assert_eq!(tasks.owned_task_count(), 1);
        }
    });
}

#[test]
fn subagent_spawn_replay_revalidates_a_pre_registered_partial_child_lane() {
    smol::block_on(async {
        let (
            runtime,
            tasks,
            _requests,
            _reopen_count,
            _cleanup_fail,
            _finalizations,
            _apply_outcomes,
            _apply_requests,
            call,
            provenance,
            request,
        ) = build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let agent_id = runtime
            .persist_subagent_prefix_for_test(
                &coordinator,
                call.clone(),
                provenance.clone(),
                request.clone(),
                3,
                false,
            )
            .await
            .expect("complete child configuration persists before its spawn fact");
        let session = runtime
            .clone_session_for_test()
            .expect("partial child session clones for restart fixture");
        let (resolver, root_services, artifacts, subagents) = runtime
            .reopen_parts_for_test()
            .expect("reopen parts remain host-owned");
        tasks.lose_owned_tasks_for_restart();
        drop(runtime);

        let policy = fixture_subagent_policy();
        let spawn_definition = root_subagent_tool_definitions(&policy)
            .expect("fixture root definitions resolve")
            .into_iter()
            .find(|definition| definition.name == "spawn_agent")
            .expect("fixture spawn definition exists");
        let mut forbidden_child_tools = ToolRegistry::default();
        forbidden_child_tools.insert(Arc::new(DefinitionOnlyTool(spawn_definition)));
        let child_services = RuntimeServices::new(
            Arc::new(QueuedProvider {
                streams: Mutex::new(VecDeque::new()),
            }),
            forbidden_child_tools,
        )
        .model(policy.models[0].descriptor.clone());
        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            lane_services: BTreeMap::from([(agent_id.lane_id(), child_services)]),
            artifacts,
            rollover_budget: 1,
            subagents,
        })
        .expect("a graph-unclaimed partial lane can be registered for deterministic replay");
        let reopened_coordinator = reopened
            .subagent_coordinator_for_test()
            .expect("reopened coordinator exists");

        let error = reopened
            .accept_subagent_spawn(&reopened_coordinator, call, provenance, request)
            .await
            .expect_err("graph binding revalidates the already registered child surface");
        assert!(
            error
                .to_string()
                .contains("subagent child harness cannot expose root collaboration tool spawn_agent"),
            "the root collaboration capability is rejected at the post-fact child boundary"
        );
        assert!(
            reduce_agent_graph(&reopened.snapshot().expect("replayed snapshot reads"))
                .expect("replayed graph reduces")
                .agents
                .contains_key(&agent_id),
            "the rejection occurs after the durable spawn fact makes the lane a child"
        );
        assert_eq!(
            tasks.owned_task_count(),
            0,
            "an invalid pre-registered child never reaches task handoff"
        );
    });
}

#[test]
fn subagent_spawn_fact_without_operation_replays_the_original_assignment() {
    smol::block_on(async {
        let (runtime, tasks, _requests, reopen_count, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let agent_id = runtime
            .persist_subagent_prefix_for_test(
                &coordinator,
                call.clone(),
                provenance.clone(),
                request.clone(),
                3,
                true,
            )
            .await
            .expect("spawn fact persists before child operation acceptance");
        assert_eq!(
            reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
                .expect("spawn-only prefix reduces")
                .agents[&agent_id]
                .state,
            AgentState::Spawned,
        );
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("identical root-tool replay accepts the original child operation");
        assert_eq!(spawned.agent_id, agent_id);
        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("accepted child graph reduces");
        let child = &graph.agents[&agent_id];
        assert_eq!(child.state, AgentState::Running);
        assert_eq!(
            child.operation_id.as_ref(),
            Some(&spawned.operation_id),
            "replay uses the child operation derived before the crash"
        );
        let post_replay = runtime.snapshot().expect("snapshot reads");
        let assignment = post_replay
            .entries()
            .iter()
            .find(|entry| entry.lane_id == agent_id.lane_id() && matches!(entry.body, SessionEntry::UserMessage(_)))
            .expect("original task assignment is appended once");
        assert_eq!(
            match &assignment.body {
                SessionEntry::UserMessage(entry) => &entry.content,
                _ => unreachable!("assignment filter selects a user entry"),
            },
            "inspect the replay prefix",
            "replay recovers the durable original assignment rather than a synthesized task"
        );
        assert_eq!(*reopen_count.lock().expect("fixture reopen mutex"), 1);
        assert_eq!(tasks.owned_task_count(), 1);
    });
}

#[test]
fn subagent_policy_timeout_fails_the_child_and_retains_salvageable_changes() {
    smol::block_on(async {
        let (runtime, tasks, _requests, _reopens, _cleanup_fail, finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("child operation accepts before its task starts");
        finalizations
            .lock()
            .expect("fixture finalization mutex")
            .push_back(FixtureFinalization::Delta);

        tasks.poll_one_owned_task();

        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("timed-out child graph reduces");
        let child = graph.agents.get(&spawned.agent_id).expect("child remains durable");
        assert_eq!(
            child.state,
            AgentState::DeltaReady {
                outcome: OperationOutcome::Failed {
                    code: "subagent_timeout".into(),
                },
                delta_id: child
                    .workspace_delta
                    .as_ref()
                    .expect("timeout delta is durable")
                    .delta_id
                    .clone(),
            },
            "policy expiry is distinguishable from an explicit interruption"
        );
        assert!(child.terminal.is_some(), "timeout retains a final report");
        assert_eq!(
            child
                .workspace_delta
                .as_ref()
                .expect("timeout preserves the salvageable delta")
                .changed_paths,
            vec!["src/fixture.rs"],
        );
        assert!(
            coordinator.is_exposable(&spawned.agent_id),
            "cleanup completes before timeout result becomes wait-visible"
        );
        assert_eq!(
            tasks.owned_task_count(),
            0,
            "the completed task is no longer executor-owned"
        );
        assert!(
            !runtime
                .lane_has_active_agent_for_test(&child.spawned.lane_id)
                .expect("child lane remains registered"),
            "forced timeout clears the live lane agent even though the drive future was dropped"
        );
        coordinator
            .reserve(
                AgentId::new("fixture-timeout-replacement").expect("replacement agent ID"),
                &OperationId::new("fixture-root-replay-operation").expect("root operation ID"),
                0,
                1,
            )
            .expect("a timed-out terminal child no longer consumes active capacity");
        runtime
            .settle_root_children_for_test(
                &OperationId::new("fixture-root-replay-operation").expect("root operation ID"),
            )
            .await
            .expect("root structured settlement externally joins the completed child");
        assert!(
            !coordinator.has_handle(&spawned.agent_id),
            "only the external root join/reap boundary drops the retained completed handle"
        );
    });
}

#[test]
fn completed_child_trace_header_carries_its_durable_agent_id() {
    smol::block_on(async {
        let (runtime, tasks, _requests, _reopen_count, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        tasks.hold_wait_timeouts();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance, request)
            .await
            .expect("child operation accepts");
        tasks.poll_one_owned_task();
        let snapshot = runtime.snapshot().expect("completed child snapshot reads");
        let trace = snapshot
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                SessionFact::TraceArtifact(trace) if trace.operation_id == spawned.operation_id => {
                    Some(trace.clone())
                }
                _ => None,
            })
            .expect("completed child persists exactly one trace artifact");
        let bytes = runtime
            .artifact_bytes_for_test(trace.artifact_id)
            .expect("trace artifact remains reachable");
        let events = tea_trace::decode_jsonl(
            std::str::from_utf8(&bytes).expect("trace artifact is UTF-8 JSONL"),
        )
        .expect("child trace artifact decodes");
        let tea_trace::TraceEvent::EpisodeHeader(header) = &events[0] else {
            panic!("trace begins with episode header");
        };
        assert_eq!(
            header
                .provenance
                .as_ref()
                .and_then(|provenance| provenance.agent_id.as_deref()),
            Some(spawned.agent_id.as_str()),
            "the child trace header receives the exact graph AgentId without assigning one to root"
        );
    });
}

#[test]
fn subagent_coordinator_rejects_a_total_budget_exhausted_by_durable_history() {
    let (runtime, _tasks, _requests, _reopens, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, _call, _provenance, _request) =
        build_active_spawn_replay_fixture();
    let coordinator = runtime
        .subagent_coordinator_for_test()
        .expect("fixture coordinator exists");
    let error = coordinator
        .reserve(
            tea_session::AgentId::new("fixture-total-budget-agent").expect("agent ID"),
            &OperationId::new("fixture-root-replay-operation").expect("operation ID"),
            0,
            4,
        )
        .expect_err("the durable total count reaches the configured four-child budget");
    assert!(error.to_string().contains("total-spawn limit"));
}

#[test]
fn subagent_wait_observes_only_cleanup_ready_results_and_interrupt_is_idempotent() {
    smol::block_on(async {
        let (runtime, tasks, _requests, _reopens, cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
            .await
            .expect("child operation accepts");
        *cleanup_fail.lock().expect("fixture cleanup failure mutex") = true;
        let interrupted = coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                &spawned.agent_id.to_string(),
            )
            .await;
        assert!(
            interrupted.is_err(),
            "a terminal fact with failed operational cleanup is not a completed child result"
        );
        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("terminal prefix reduces");
        assert!(
            graph.agents[&spawned.agent_id].terminal.is_some(),
            "the interrupted operation is durable before cleanup retries"
        );
        assert!(
            !coordinator.is_exposable(&spawned.agent_id),
            "failed cleanup does not expose a terminal report to wait_agent"
        );
        let blocked_wait = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec![spawned.agent_id.to_string()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect("timeout remains an ordinary wait result");
        assert!(blocked_wait.completed.is_empty());
        assert_eq!(blocked_wait.pending.len(), 1);
        assert!(blocked_wait.timed_out);

        *cleanup_fail.lock().expect("fixture cleanup failure mutex") = false;
        let first = coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                &spawned.agent_id.to_string(),
            )
            .await
            .expect("idempotent interruption retries cleanup and settles");
        assert_eq!(first.previous, AgentState::Interrupted);
        assert_eq!(first.resulting, AgentState::Interrupted);
        assert!(coordinator.is_exposable(&spawned.agent_id));
        assert_eq!(tasks.owned_task_count(), 0, "cancelled child task is not detached");

        let ready_wait = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec![spawned.agent_id.to_string()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect("already cleanup-ready child returns immediately");
        assert_eq!(ready_wait.completed.len(), 1);
        assert!(ready_wait.pending.is_empty());
        assert!(!ready_wait.timed_out);

        let repeated = coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance,
                },
                &spawned.agent_id.to_string(),
            )
            .await
            .expect("repeated interruption is an idempotent observation");
        assert_eq!(repeated.previous, AgentState::Interrupted);
        assert_eq!(repeated.resulting, AgentState::Interrupted);
    });
}

#[test]
fn subagent_wait_timeout_and_cancellation_drop_notifier_wakers() {
    smol::block_on(async {
        let (runtime, tasks, _requests, _reopens, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, call, provenance, request) =
            build_active_spawn_replay_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
            .await
            .expect("child operation accepts");
        let wait_request = WaitAgentsRequest {
            targets: vec![spawned.agent_id.to_string()],
            return_when: WaitReturnWhen::All,
            timeout: Duration::from_millis(100),
        };
        for _ in 0..3 {
            let result = coordinator
                .wait(
                    ToolContext {
                        cancellation: CancellationToken::new(),
                        provenance: provenance.clone(),
                    },
                    wait_request.clone(),
                )
                .await
                .expect("immediate fixture timeout returns a result");
            assert!(result.timed_out);
            assert_eq!(coordinator.activity_waiter_count_for_test(), 0);
        }

        tasks.hold_wait_timeouts();
        for _ in 0..3 {
            let cancellation = CancellationToken::new();
            let observed = coordinator.activity_generation();
            let mut timeout = coordinator.timeout(Duration::from_millis(100));
            let mut waiting = Box::pin(coordinator.wait_for_activity(
                observed,
                cancellation.clone(),
                &mut timeout,
            ));
            let waker = std::task::Waker::noop();
            let mut context = std::task::Context::from_waker(waker);
            assert!(matches!(
                waiting.as_mut().poll(&mut context),
                std::task::Poll::Pending
            ));
            assert_eq!(coordinator.activity_waiter_count_for_test(), 1);
            cancellation.cancel();
            assert!(matches!(
                waiting.as_mut().poll(&mut context),
                std::task::Poll::Ready(_)
            ));
            drop(waiting);
            assert_eq!(
                coordinator.activity_waiter_count_for_test(),
                0,
                "cancelled waits remove their retained activity waker"
            );
        }
    });
}

#[test]
fn subagent_wait_keeps_requested_order_while_list_sorts_by_task_name() {
    smol::block_on(async {
        let (runtime, tasks, provenance, calls) = build_active_two_spawn_fixture();
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let mut handles = Vec::new();
        for (call, request) in calls {
            handles.push(
                runtime
                    .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
                    .await
                    .expect("each source-ordered spawn accepts"),
            );
        }
        // Settle `a_task` first even though it is second in the later wait
        // request. `any` returns only the completed subset and leaves the
        // still-running requested child pending.
        coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                &handles[1].agent_id.to_string(),
            )
            .await
            .expect("first interruption settles child");
        let any = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec![handles[0].agent_id.to_string(), handles[1].agent_id.to_string()],
                    return_when: WaitReturnWhen::Any,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect("any returns as soon as one requested child is cleanup-ready");
        assert_eq!(any.completed.len(), 1);
        assert_eq!(any.completed[0].status.task_name, "a_task");
        assert_eq!(any.pending.len(), 1);
        assert_eq!(any.pending[0].task_name, "z_task");
        coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                &handles[0].agent_id.to_string(),
            )
            .await
            .expect("second interruption settles child");
        assert_eq!(tasks.owned_task_count(), 0);
        let listed = coordinator
            .list(&ToolContext {
                cancellation: CancellationToken::new(),
                provenance: provenance.clone(),
            })
            .expect("list observes current root children");
        assert_eq!(
            listed.iter().map(|status| status.task_name.as_str()).collect::<Vec<_>>(),
            ["a_task", "z_task"],
            "list_agents has its own deterministic task-name ordering"
        );
        let waited = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance,
                },
                WaitAgentsRequest {
                    targets: vec![handles[0].agent_id.to_string(), handles[1].agent_id.to_string()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect("cleanup-ready children return immediately");
        assert_eq!(
            waited
                .completed
                .iter()
                .map(|entry| entry.status.task_name.as_str())
                .collect::<Vec<_>>(),
            ["z_task", "a_task"],
            "wait_agent retains exact requested order despite inverse completion"
        );
    });
}

#[test]
fn apply_agent_changes_commits_a_proven_delta_and_replays_without_reapplying() {
    smol::block_on(async {
        let (
            runtime,
            _tasks,
            _requests,
            _reopens,
            _cleanup_fail,
            finalizations,
            apply_outcomes,
            apply_requests,
            call,
            provenance,
            request,
        ) = build_active_spawn_replay_fixture();
        finalizations
            .lock()
            .expect("fixture finalization mutex")
            .push_back(FixtureFinalization::Delta);
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
            .await
            .expect("child operation accepts");
        coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                &spawned.agent_id.to_string(),
            )
            .await
            .expect("interruption retains the configured workspace delta");
        let apply_call = fixture_apply_call(&spawned.agent_id);
        let delta_id = WorkspaceDeltaId::derive(
            &WorkspaceLeaseId::derive(&spawned.agent_id),
            "fixture-child-base",
            "fixture-child-result",
        );
        // This also models a physical already-applied patch after a crash:
        // the host proves the expected tree and core commits the missing fact.
        apply_outcomes
            .lock()
            .expect("fixture apply outcome mutex")
            .push_back(Ok(WorkspaceApplyOutcome::Applied {
                changed_paths: vec!["src/fixture.rs".into()],
            }));
        let first = coordinator
            .apply(
                apply_call.clone(),
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                delta_id.clone(),
            )
            .await
            .expect("proven host application is durable");
        assert_eq!(
            first,
            ApplyAgentChangesResult::Applied {
                delta_id: delta_id.clone(),
                changed_paths: vec!["src/fixture.rs".into()],
            }
        );
        assert_eq!(apply_requests.lock().expect("fixture apply request mutex").len(), 1);
        let graph = reduce_agent_graph(&runtime.snapshot().expect("snapshot reads"))
            .expect("applied graph reduces");
        assert!(graph
            .agents
            .get(&spawned.agent_id)
            .expect("spawned node exists")
            .applied
            .is_some());

        let replay = coordinator
            .apply(
                apply_call,
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance,
                },
                delta_id,
            )
            .await
            .expect("durably committed application replays as observation");
        assert_eq!(replay, first);
        assert_eq!(
            apply_requests.lock().expect("fixture apply request mutex").len(),
            1,
            "an existing WorkspaceDeltaAppliedFact never re-enters host mutation"
        );
    });
}

#[test]
fn apply_agent_changes_validates_host_outcomes_and_ignores_post_begin_cancellation() {
    smol::block_on(async {
        let (
            runtime,
            _tasks,
            _requests,
            _reopens,
            _cleanup_fail,
            finalizations,
            apply_outcomes,
            _apply_requests,
            call,
            provenance,
            request,
        ) = build_active_spawn_replay_fixture();
        finalizations
            .lock()
            .expect("fixture finalization mutex")
            .push_back(FixtureFinalization::Delta);
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
            .await
            .expect("child operation accepts");
        coordinator
            .interrupt(
                &ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                &spawned.agent_id.to_string(),
            )
            .await
            .expect("child settles with durable delta");
        let delta_id = WorkspaceDeltaId::derive(
            &WorkspaceLeaseId::derive(&spawned.agent_id),
            "fixture-child-base",
            "fixture-child-result",
        );
        let apply_call = fixture_apply_call(&spawned.agent_id);

        apply_outcomes
            .lock()
            .expect("fixture apply outcome mutex")
            .push_back(Ok(WorkspaceApplyOutcome::Conflict {
                conflicting_paths: vec!["/private/host-path".into()],
            }));
        assert!(
            coordinator
                .apply(
                    apply_call.clone(),
                    ToolContext {
                        cancellation: CancellationToken::new(),
                        provenance: provenance.clone(),
                    },
                    delta_id.clone(),
                )
                .await
                .is_err(),
            "host conflict paths cannot expose absolute or non-durable paths"
        );

        apply_outcomes
            .lock()
            .expect("fixture apply outcome mutex")
            .push_back(Ok(WorkspaceApplyOutcome::Indeterminate {
                diagnostic: "repository state requires explicit inspection".into(),
            }));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let result = coordinator
            .apply(
                apply_call,
                ToolContext {
                    cancellation: cancelled,
                    provenance,
                },
                delta_id,
            )
            .await
            .expect("cancellation after application begins cannot erase classification");
        assert!(matches!(result, ApplyAgentChangesResult::Indeterminate { .. }));
    });
}

#[test]
fn wait_targets_are_owner_scoped_and_timeout_rereads_the_boundary_snapshot() {
    smol::block_on(async {
        let (
            runtime,
            tasks,
            _requests,
            _reopens,
            cleanup_fail,
            _finalizations,
            _apply_outcomes,
            _apply_requests,
            call,
            provenance,
            request,
        ) = build_active_spawn_replay_fixture_with_foreign_reused_task(true);
        let coordinator = runtime
            .subagent_coordinator_for_test()
            .expect("fixture coordinator exists");
        let spawned = runtime
            .accept_subagent_spawn(&coordinator, call, provenance.clone(), request)
            .await
            .expect("current child accepts beside historical foreign child");
        let duplicate = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec![spawned.agent_id.to_string(), spawned.agent_id.to_string()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect_err("duplicate child target is not an ambiguous wait request");
        assert!(duplicate.to_string().contains("duplicate child"));

        let older = AgentId::derive(
            &SessionId::new("runtime-subagent-replay").expect("fixture session ID"),
            &LaneId::main(),
            &OperationId::new("fixture-older-root-operation").expect("older operation ID"),
            "fixture-older-root-spawn-key",
        );
        let foreign = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec![older.to_string()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect_err("foreign root-operation child is never selectable by direct ID");
        assert!(foreign.to_string().contains("not owned"));

        let by_reused_name = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec!["replay_task".into()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect("owner filter runs before task-name matching");
        assert_eq!(by_reused_name.pending[0].agent_id, spawned.agent_id);

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let cancellation = coordinator
            .wait(
                ToolContext {
                    cancellation: cancelled,
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec!["replay_task".into()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect_err("public wait propagates caller cancellation");
        assert!(cancellation.to_string().contains("cancelled"));

        *cleanup_fail.lock().expect("fixture cleanup failure mutex") = true;
        assert!(
            coordinator
                .interrupt(
                    &ToolContext {
                        cancellation: CancellationToken::new(),
                        provenance: provenance.clone(),
                    },
                    &spawned.agent_id.to_string(),
                )
                .await
                .is_err(),
            "terminal fact is retained but remains non-exposable while cleanup fails"
        );
        *cleanup_fail.lock().expect("fixture cleanup failure mutex") = false;
        let coordinator_for_timeout = Arc::clone(&coordinator);
        let agent_for_timeout = spawned.agent_id.clone();
        tasks.act_on_next_sleep(move || {
            coordinator_for_timeout.mark_exposable_and_notify(agent_for_timeout);
        });
        let boundary = coordinator
            .wait(
                ToolContext {
                    cancellation: CancellationToken::new(),
                    provenance: provenance.clone(),
                },
                WaitAgentsRequest {
                    targets: vec!["replay_task".into()],
                    return_when: WaitReturnWhen::All,
                    timeout: Duration::from_millis(100),
                },
            )
            .await
            .expect("timeout rereads the terminal activity boundary snapshot");
        assert!(boundary.timed_out);
        assert_eq!(boundary.completed[0].status.agent_id, spawned.agent_id);

    });
}

#[test]
fn root_abort_remains_sticky_while_ambiguous_apply_requires_reconciliation() {
    smol::block_on(async {
        let (runtime, _tasks, _requests, _reopens, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, _call, _provenance, _request) =
            build_active_spawn_replay_fixture_with_options(false, true, true, None);
        assert!(
            runtime.abort_root().expect("root abort checks durable operation"),
            "a durable accepted root operation accepts cancellation before its agent exists"
        );
        assert!(runtime.root_abort_requested_for_test().expect("root lane reads"));
        assert!(
            runtime.abort_root().expect("repeated root abort is idempotent"),
            "the same still-open operation retains its sticky cancellation request"
        );
        let resumed = runtime.resume().await;
        assert!(matches!(
            resumed,
            Err(HarnessError::RecoveryRequired {
                plan: tea_session::RecoveryPlan::SynthesizeInterruptedToolResult { .. },
            })
        ));
        let reduction = reduce_lane(
            runtime.snapshot().expect("snapshot reads"),
            LaneId::main(),
        )
        .expect("root reduction succeeds");
        assert!(
            reduction.lane_state.active_operation.is_some(),
            "ambiguous apply recovery must retain the open root operation: {reduction:?}"
        );
        assert!(
            runtime.root_abort_requested_for_test().expect("root lane reads"),
            "reconciliation cannot erase the pending durable root cancellation"
        );
    });
}

#[test]
fn root_abort_is_sticky_after_claim_before_operation_acceptance() {
    let (runtime, _requests, _tasks) = build_subagent_runtime(
        "runtime-root-preaccept-abort",
        vec![completion_stream()],
    );
    runtime
        .claim_root_before_acceptance_for_test()
        .expect("test owns the pre-accept claim");
    assert!(
        runtime.abort_root().expect("claimed root drive is cancellable"),
        "Ctrl+C between claim and OperationStarted becomes an install-time abort"
    );
    assert!(runtime.root_abort_requested_for_test().expect("root lane reads"));
}

#[test]
fn rejected_concurrent_root_prompt_cannot_clear_the_active_abort_request() {
    smol::block_on(async {
        let (runtime, _tasks, _requests, _reopens, _cleanup_fail, _finalizations, _apply_outcomes, _apply_requests, _call, _provenance, _request) =
            build_active_spawn_replay_fixture();
        assert!(runtime.abort_root().expect("accepted root operation is cancellable"));
        assert!(
            runtime.run_root_prompt("a concurrent prompt").await.is_err(),
            "the already-open root operation owns the sole lane claim"
        );
        assert!(
            runtime.root_abort_requested_for_test().expect("root lane reads"),
            "a rejected contender cannot erase the active operation's sticky abort"
        );
    });
}

#[test]
fn no_extension_runtime_persists_intents_trace_and_verifies() {
    smol::block_on(async {
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("runtime-record-call")
                                .expect("fixture tool call ID"),
                            name: "record".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("done".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ])),
        });
        let store = Arc::new(MemoryArtifactStore::default());
        let (runtime, identity) = build_runtime("runtime-no-extension", provider, store.clone());

        let operation = runtime
            .run_root_prompt("exercise durable core runtime")
            .await
            .expect("runtime operation settles");
        assert!(operation.is_completed());
        let snapshot = runtime.snapshot().expect("durable snapshot is readable");
        assert!(
            snapshot
                .records()
                .iter()
                .any(|stored| { matches!(stored.record, LaneRecord::ProviderRequestStarted(_)) })
        );
        assert!(
            snapshot
                .records()
                .iter()
                .any(|stored| matches!(stored.record, LaneRecord::ToolStarted(_)))
        );
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.body, SessionEntry::ToolResult(_)))
                .count(),
            1,
        );
        let trace = snapshot
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                SessionFact::TraceArtifact(trace) => Some(trace),
                _ => None,
            })
            .expect("completed epoch retains a trace artifact");
        assert_eq!(trace.operation_id, *operation.id());
        assert_eq!(trace.harness_revision_id, *identity.revision_id());
        assert_eq!(trace.harness_snapshot_id, *identity.snapshot_id());
        assert_eq!(trace.model_harness_profile, *identity.profile_id());
        let trace_bytes = store
            .get(trace.artifact_id)
            .expect("retained trace artifact remains reachable");
        assert!(
            std::str::from_utf8(&trace_bytes)
                .expect("trace is JSON Lines")
                .contains(r#""type":"episode_end""#)
        );
        runtime
            .verify_durable_state()
            .expect("runtime verifies its catalog and reachable artifacts");
    });
}

#[test]
fn jsonl_runtime_reopens_the_persisted_no_extension_catalog() {
    let directory = std::env::temp_dir().join(format!(
        "tea-core-runtime-reopen-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos(),
    ));
    let store = Arc::new(MemoryArtifactStore::default());
    let provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let (manager, identity, services) = fixture_manager(provider, store.clone());
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("runtime-jsonl-reopen").expect("fixture session ID"),
            "runtime-test-workspace",
            fixture_metadata(),
        ),
        DurabilityMode::Strict,
    )
    .expect("fixture JSONL session creates");
    append_initial_revision(&mut session, &identity);
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity: identity.clone(),
        root_services: services,
        artifacts: store.clone(),
        rollover_budget: 1,
        subagents: None,
    })
    .expect("supervisor persists its immutable catalog");
    let expected_sequence = runtime
        .snapshot()
        .expect("created runtime snapshot is readable")
        .last_sequence();
    drop(runtime);

    let reopened_session =
        JsonlSession::open(&directory, DurabilityMode::Strict).expect("JSONL session reopens");
    let reopened_provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let empty_repository =
        HarnessRepository::with_extension_engine(store.clone(), Arc::new(NoExtensions));
    let reopened_manager = Arc::new(HarnessResolver::new(empty_repository, Default::default()));
    let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
        session: reopened_session,
        resolver: reopened_manager,
        root_services: RuntimeServices::new(reopened_provider, ToolRegistry::default()),
        lane_services: BTreeMap::new(),
        artifacts: store,
        rollover_budget: 1,
        subagents: None,
    })
    .expect("supervisor restores the catalog from durable state");
    assert_eq!(
        reopened
            .snapshot()
            .expect("reopened snapshot is readable")
            .last_sequence(),
        expected_sequence,
    );
    reopened
        .verify_durable_state()
        .expect("reopened runtime verifies catalog and artifacts");
    assert_eq!(
        reopened
            .measure_prompt_layout(&ModelRequest::default())
            .continuity,
        crate::measurement::PromptContinuity::FirstRequest,
        "reopen starts a fresh volatile prompt-layout continuity ledger",
    );
    drop(reopened);
    std::fs::remove_dir_all(&directory).expect("fixture JSONL directory removes");
}

#[test]
fn live_runtime_joins_prompt_layout_across_fresh_operations() {
    smol::block_on(async {
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("first response".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("second response".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ])),
        });
        let store = Arc::new(MemoryArtifactStore::default());
        let (runtime, _) = build_runtime("runtime-prompt-layout", provider, store.clone());
        let first = runtime
            .run_root_prompt("first operation")
            .await
            .expect("first settles");
        let second = runtime
            .run_root_prompt("second operation")
            .await
            .expect("second settles");
        let snapshot = runtime.snapshot().expect("snapshot reads");
        let traces = snapshot
            .facts()
            .iter()
            .filter_map(|stored| match &stored.fact {
                SessionFact::TraceArtifact(trace) if trace.operation_id == *first.id() => Some((
                    false,
                    store.get(trace.artifact_id).expect("trace artifact reads"),
                )),
                SessionFact::TraceArtifact(trace) if trace.operation_id == *second.id() => Some((
                    true,
                    store.get(trace.artifact_id).expect("trace artifact reads"),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(traces.len(), 2);
        let second_trace = traces
            .iter()
            .find(|(second, _)| *second)
            .expect("second trace is present");
        let second_trace = std::str::from_utf8(&second_trace.1).expect("trace UTF-8");
        assert!(second_trace.contains("deterministic_common_prefix_bytes"));
        let first_trace = traces
            .iter()
            .find(|(second, _)| !*second)
            .expect("first trace is present");
        let first_trace = std::str::from_utf8(&first_trace.1).expect("trace UTF-8");
        assert!(first_trace.contains(r#""continuity":"first_request""#));
        assert!(first_trace.contains(r#""deterministic_common_prefix_bytes":null"#));
    });
}

#[test]
fn scripted_lanes_drive_concurrently_with_independent_ledgers_and_one_writer() {
    smol::block_on(async {
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("alpha complete".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("beta complete".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ])),
        });
        let store = Arc::new(MemoryArtifactStore::default());
        let (manager, identity, services) = fixture_manager(provider, store.clone());
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("runtime-two-lanes").expect("fixture session ID"),
            "runtime-test-workspace",
            fixture_metadata(),
        ))
        .expect("fixture session creates");
        append_initial_revision(&mut session, &identity);
        let base_leaf = EntryId::new("runtime-test-initial-revision").expect("fixture entry ID");
        let alpha = LaneId::new("agent-alpha").expect("fixture lane ID");
        let beta = LaneId::new("agent-beta").expect("fixture lane ID");
        for lane in [&alpha, &beta] {
            session
                .append_lane_mutation(LaneMutation::Created {
                    lane_id: lane.clone(),
                    base_leaf_id: Some(base_leaf.clone()),
                })
                .expect("lane topology commits");
            session
                .append_entry(
                    lane,
                    ProvisionedEntry {
                        id: EntryId::new(format!("{}-revision", lane.as_str()))
                            .expect("fixture revision entry ID"),
                        body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                            revision_id: identity.revision_id().clone(),
                            snapshot_id: identity.snapshot_id().clone(),
                            rollback_from: None,
                        }),
                    },
                )
                .expect("lane harness identity commits");
        }
        let supervisor = SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity,
            root_services: services.clone(),
            artifacts: store,
            rollover_budget: 1,
            subagents: None,
        })
        .expect("supervisor creates");
        supervisor
            .register_lane(alpha.clone(), services.clone())
            .expect("alpha services register");
        supervisor
            .register_lane(beta.clone(), services)
            .expect("beta services register");

        let (alpha_result, beta_result) = smol::future::zip(
            supervisor.run_lane_prompt(alpha.clone(), "alpha assignment"),
            supervisor.run_lane_prompt(beta.clone(), "beta assignment"),
        )
        .await;
        assert!(alpha_result.expect("alpha operation settles").is_completed());
        assert!(beta_result.expect("beta operation settles").is_completed());

        let snapshot = supervisor.snapshot().expect("serialized snapshot reads");
        assert!(reduce_lane(snapshot.clone(), alpha)
            .expect("alpha reduces")
            .lane_state
            .active_operation
            .is_none());
        assert!(reduce_lane(snapshot.clone(), beta)
            .expect("beta reduces")
            .lane_state
            .active_operation
            .is_none());
        assert_eq!(
            snapshot.last_sequence().0,
            snapshot.mutations().count() as u64,
            "one shared writer assigns one consecutive global sequence"
        );
    });
}
