//! Operation orchestration that binds core effects to the session WAL.

use crate::artifact::{
    projection_content, retain_direct_recovery_result_with_projection,
    retain_tool_result_with_projection, RetainedToolResult,
};
use crate::artifact_tools::{stable_artifact_tools, STABLE_ARTIFACT_TOOL_NAMES};
use crate::harness_tool::{stable_harness_tools, STABLE_HARNESS_TOOL_NAME};
use crate::context::derive_snapshot_context_with_policies;
use crate::events::EventHub;
use crate::{
    ArtifactEvent, CoreEpochTemplate, HarnessActor, HarnessError, HarnessEvent, HarnessManager,
    ProviderLimits,
    ResolvedHarnessConfiguration, HarnessSnapshotView, SessionEvent, TeaEvent,
    TeaEventSubscription,
};
use std::convert::Infallible;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tea_core::effect::{
    DurableWriteRequest, EffectAction, EffectCompletion, EffectFuture, EffectGate,
    EffectGateError, EffectOutcome, EffectSubject, HookEffectOutcome, HookInvocation,
    ProviderEffectOutcome, ProviderResponse, RunProvenance, ToolEffectOutcome,
};
use tea_core::trace::TraceObserver;
use tea_core::{Agent, AgentEvent, EventObserver, ObserverFuture};
use tea_core::state::{
    AgentMessage, AgentToolCall, MessageId, SerializedJson, StopReason, ThinkingLevel, ToolCallId,
};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolFailureDisposition, ToolRegistry,
};
use tea_luau::{
    PolicyMemoryCollector, PolicyMemoryRetention, PolicyMemoryVisibility,
};
use tea_session::{
    reduce_lane, ArtifactStore, CanonicalHashWriter, CoreRunId, Digest, EntryId,
    EpochFinishReason, EpochFinishedRecord, EpochId, EpochStartedRecord, HarnessRevisionId,
    HarnessSnapshotId, LaneId, LaneRecord, MemoryRetention, MemoryVisibility, ModelHarnessProfileId, OperationFinishedRecord,
    OperationId, OperationKind, OperationOutcome, OperationStartedRecord,
    ProviderRequestId, ProviderRequestSettledRecord, ProviderRequestStartedRecord,
    ProviderSettlementClassification, ProvisionedEntry, RecoveryPlan, SessionEntry, SessionFact,
    SessionSnapshot, SessionWriter, StepAttemptedRecord, StepId,
    StepKind, ToolReplayPolicy, ToolResultEntry, ToolSchemaDeviationFact, ToolStartedRecord,
    Usage, PayloadRef, PluginMemoryEntry, SchemaFieldMismatch, TraceArtifactFact,
};
use tea_protocol::JsonValue;
use tea_trace::{JsonLinesSink, RedactingSink, Redactor, TraceEvent, TraceSink};

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

/// A single-lane durable Tea supervisor.
///
/// The first complete vertical slice deliberately exposes only `main`.  The
/// underlying session and all effect records remain lane-keyed, so a future
/// UI can add lanes without migrating the durable format.
pub struct DurableHarness<S> {
    session: Arc<Mutex<S>>,
    /// Every epoch resolves its executable template from this semantic branch
    /// manager and its committed immutable revision.
    manager: Arc<HarnessManager>,
    artifacts: Arc<dyn ArtifactStore>,
    rollover_budget: u32,
    active: AtomicBool,
    /// The currently driven core epoch, exposed only through narrow host
    /// controls such as cancellation and explicit queueing. It is never a
    /// source of durable state: loss or replacement across recovery is
    /// expected, and every durable fact still flows through `EpochRuntime`.
    active_agent: Mutex<Option<Agent>>,
    /// Process-local live-event fanout. It never owns durable state.
    events: Arc<EventHub>,
    /// Serializes snapshot registration with post-commit publication so a UI
    /// receives one atomic view and then only events beyond that view.
    publication: Mutex<()>,
}

impl<S> std::fmt::Debug for DurableHarness<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableHarness")
            .field("lane", &LaneId::main())
            .field("rollover_budget", &self.rollover_budget)
            .field("active", &self.active.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl<S> DurableHarness<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Construct a supervisor whose future epochs are resolved from immutable
    /// harness lineage.  The caller must seed the initial
    /// `HarnessRevisionChanged` entry before construction; the manager rejects
    /// a branch that has no durable active revision rather than relying on an
    /// in-memory pointer.
    pub fn new_with_artifact_store(
        mut session: S,
        manager: Arc<HarnessManager>,
        initial_identity: HarnessIdentity,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Self, HarnessError> {
        let snapshot = session.snapshot()?;
        let persisted_mode = snapshot
            .header()
            .metadata
            .get(crate::SELF_EXTENSION_MODE_METADATA_KEY)
            .and_then(JsonValue::as_str)
            .and_then(crate::SelfExtensionMode::parse)
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "managed harness session metadata must contain {} as off, author, or adaptive",
                    crate::SELF_EXTENSION_MODE_METADATA_KEY,
                ))
            })?;
        if persisted_mode != manager.self_extension_mode_value() {
            return Err(HarnessError::invalid_state(format!(
                "managed harness session mode {} does not match manager mode {}",
                persisted_mode.as_str(),
                manager.self_extension_mode_value().as_str(),
            )));
        }
        let reduction = reduce_lane(snapshot.clone(), LaneId::main())?;
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
        let resolved = manager.resolve_revision(initial_identity.revision_id())?;
        if resolved.identity != initial_identity {
            return Err(HarnessError::invalid_state(
                "managed harness initial identity does not match its immutable revision",
            ));
        }
        validate_reserved_host_tool_names(&resolved.template)?;
        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            manager,
            artifacts,
            rollover_budget: 1,
            active: AtomicBool::new(false),
            active_agent: Mutex::new(None),
            events: Arc::new(EventHub::default()),
            publication: Mutex::new(()),
        })
    }

    /// Reopen a managed harness from its committed semantic branch and
    /// immutable catalog.
    ///
    /// The caller supplies the same trusted base template, capability catalog,
    /// ceiling, and session mode used to create the original manager. This
    /// method deliberately derives the active revision, snapshot, and model
    /// profile from durable state instead of accepting a host-provided
    /// identity that could drift from the restored branch.
    pub fn reopen_with_artifact_store(
        session: S,
        manager: Arc<HarnessManager>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Self, HarnessError> {
        let snapshot = session.snapshot()?;
        let catalog = latest_harness_catalog(&snapshot).ok_or_else(|| {
            HarnessError::invalid_state(
                "managed harness reopen requires a committed immutable harness catalog",
            )
        })?;
        manager.restore_catalog(catalog, Arc::clone(&artifacts))?;
        let reduction = reduce_lane(snapshot, LaneId::main())?;
        let revision_id = reduction
            .lane_state
            .active_harness_revision
            .ok_or_else(|| {
                HarnessError::invalid_state(
                    "managed harness reopen requires a committed active harness revision",
                )
            })?;
        let revision = manager.revision(&revision_id)?;
        let harness_snapshot = manager.snapshot(&revision.snapshot_id)?;
        let identity = HarnessIdentity::new(
            revision.revision_id,
            harness_snapshot.id,
            harness_snapshot.spec.model_harness_profile,
        );
        Self::new_with_artifact_store(session, manager, identity, artifacts)
    }

    /// Set the maximum number of automatic immutable harness rollovers one
    /// durable user operation may perform.  Zero disables automatic
    /// activation; the default permits exactly one successful self-extension
    /// without allowing an edit–reload loop.
    pub fn rollover_budget(mut self, maximum_rollovers: u32) -> Self {
        self.rollover_budget = maximum_rollovers;
        self
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
        self.active.load(Ordering::Acquire)
    }

    /// Request cancellation of the live core epoch, if it has reached the
    /// executable state. Cancellation remains asynchronous: the epoch's
    /// durable effect gate and terminal operation record decide settlement.
    pub fn abort(&self) -> Result<bool, HarnessError> {
        let agent = self
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
        self.active_agent()?.enqueue_steering(content).map_err(Into::into)
    }

    /// Queue a follow-up prompt for the current durable core epoch.
    pub fn enqueue_follow_up(&self, content: impl Into<String>) -> Result<u64, HarnessError> {
        self.active_agent()?.enqueue_follow_up(content).map_err(Into::into)
    }

    /// Return a read-only core snapshot for the current epoch, when one is
    /// live. A terminal host may project it locally, but it must not persist
    /// this process-local snapshot as a session replacement.
    pub fn active_agent_snapshot(&self) -> Result<Option<tea_core::AgentSnapshot>, HarnessError> {
        let agent = self
            .active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?
            .clone();
        Ok(agent.map(|agent| agent.snapshot()))
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
        if self.active.load(Ordering::Acquire) {
            return Err(HarnessError::invalid_state(
                "artifact collection requires an idle durable harness",
            ));
        }
        let reduction = reduce_lane(self.snapshot()?, LaneId::main())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "artifact collection requires no open durable operation",
            ));
        }
        Ok(())
    }

    fn active_agent(&self) -> Result<Agent, HarnessError> {
        self.active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?
            .clone()
            .ok_or_else(|| HarnessError::invalid_state("no executable core epoch is active"))
    }

    fn install_active_agent(&self, agent: Agent) -> Result<(), HarnessError> {
        let mut slot = self
            .active_agent
            .lock()
            .map_err(|_| HarnessError::invalid_state("active core epoch mutex is poisoned"))?;
        if slot.is_some() {
            return Err(HarnessError::invalid_state(
                "cannot install a second active core epoch",
            ));
        }
        *slot = Some(agent);
        Ok(())
    }

    fn clear_active_agent(&self) {
        if let Ok(mut slot) = self.active_agent.lock() {
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
    pub async fn run_prompt(&self, input: impl Into<String>) -> Result<DurableOperation, HarnessError> {
        self.run_prompt_with_authoring_authorization(input, false).await
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
        self.run_prompt_with_authoring_authorization(input, true).await
    }

    async fn run_prompt_with_authoring_authorization(
        &self,
        input: impl Into<String>,
        authoring_authorized: bool,
    ) -> Result<DurableOperation, HarnessError> {
        let _claim = self.claim_operation()?;
        let operation = self.accept_prompt(input.into(), authoring_authorized)?;
        self.drive_fresh_epoch(operation).await
    }

    /// Recover the one durable operation currently open on `main`.
    ///
    /// Recovery is derived exclusively from the session reducer. The harness
    /// never guesses whether an unrecorded provider request happened: that
    /// ambiguity is returned as [`HarnessError::RecoveryRequired`] until a
    /// host-specific reconciliation policy is supplied.
    pub async fn resume(&self) -> Result<DurableOperation, HarnessError> {
        let _claim = self.claim_operation()?;
        // Rehydrate only process-local policy state before inspecting the
        // next durable obligation. This performs no session mutation, so a
        // crash before the next consumer commits can safely invoke the same
        // idempotent callback again on the next recovery attempt.
        let (_, _, snapshot) = self.active_recovery()?;
        self.rebuild_lifecycle_state(&snapshot)?;
        loop {
            let (operation_id, plan, snapshot) = self.active_recovery()?;
            match plan {
                RecoveryPlan::AppendAcceptedInput { entries, .. } => {
                    let sequence = {
                        let mut session = self.session_lock()?;
                        let mut sequence = None;
                        for entry in entries {
                            sequence = Some(session.append_entry(&LaneId::main(), entry)?.header.seq);
                        }
                        sequence
                    };
                    if let Some(sequence) = sequence {
                        self.publish_event(TeaEvent::Session(SessionEvent::OperationAccepted {
                            sequence,
                            lane_id: LaneId::main(),
                            operation_id: operation_id.clone(),
                        }))?;
                    }
                }
                RecoveryPlan::SynthesizeInterruptedToolResult { result_entry_id } => {
                    self.append_interrupted_tool_result(&snapshot, &result_entry_id)?;
                }
                RecoveryPlan::ReplayToolIfStillSafe { tool } => {
                    if !self.replay_is_still_safe(&tool) {
                        self.append_interrupted_tool_result(&snapshot, &tool.result_entry_id)?;
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
                    self.activate_pending_harness(&operation_id, &request)?;
                    return self.drive_fresh_epoch(operation_id).await;
                }
                plan @ RecoveryPlan::ReconcileProviderRequest { .. } => {
                    return Err(HarnessError::RecoveryRequired { plan });
                }
                RecoveryPlan::StartEpoch { .. } => {
                    return self.drive_fresh_epoch(operation_id).await;
                }
                RecoveryPlan::ResumeOperation { .. } => {
                    let epoch_id = open_epoch(&snapshot, &operation_id).ok_or_else(|| {
                        HarnessError::invalid_state(
                            "ordinary operation recovery has no open durable epoch",
                        )
                    })?;
                    return self.drive_epoch(operation_id, epoch_id, None).await;
                }
            }
        }
    }

    fn claim_operation(&self) -> Result<OperationClaim<'_>, HarnessError> {
        self.active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| HarnessError::invalid_state("durable harness already has an active drive"))?;
        Ok(OperationClaim {
            active: &self.active,
        })
    }

    fn active_recovery(&self) -> Result<(OperationId, RecoveryPlan, SessionSnapshot), HarnessError> {
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), LaneId::main())?;
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
        input: String,
        authoring_authorized: bool,
    ) -> Result<OperationId, HarnessError> {
        let lane = LaneId::main();
        let (operation_id, sequence) = {
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let reduction = reduce_lane(snapshot.clone(), lane.clone())?;
            if reduction.lane_state.active_operation.is_some() {
                return Err(HarnessError::RecoveryRequired {
                    plan: reduction.recovery_plan.ok_or_else(|| {
                        HarnessError::invalid_state("main lane has an open operation without a recovery plan")
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
                    crate::AUTHORING_AUTHORIZATION_METADATA_KEY.into(),
                    JsonValue::Bool(true),
                );
            }
            let configuration = self.configuration_for_reduction(&reduction)?;
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

    async fn drive_fresh_epoch(
        &self,
        operation_id: OperationId,
    ) -> Result<DurableOperation, HarnessError> {
        let epoch = self.start_epoch(&operation_id)?;
        self.drive_epoch(operation_id, epoch, None).await
    }

    async fn drive_epoch(
        &self,
        operation_id: OperationId,
        epoch_id: EpochId,
        recovery: Option<RecoveryToolDrive>,
    ) -> Result<DurableOperation, HarnessError> {
        let configuration = self.epoch_configuration(&epoch_id)?;
        let messages = self.core_messages(&configuration, recovery.as_ref())?;
        let provider_surface_digest = configuration
            .harness_snapshot
            .as_ref()
            .map(|snapshot| snapshot.fingerprints.provider_surface_digest.to_hex());
        let provenance = self.provenance(
            &operation_id,
            &epoch_id,
            &configuration.identity,
            provider_surface_digest,
        )?;
        let host_tools = self.host_tools_for_configuration(&configuration, &operation_id)?;
        let tool_definition_digests = all_tool_definition_digests(
            &configuration.template,
            &host_tools,
        )?;
        let tool_definition_schemas = all_tool_definition_schemas(
            &configuration.template,
            &host_tools,
        )?;
        let replay_safe_host_tools = replay_safe_host_tools(&host_tools);
        let recovery_assistant_entry = recovery
            .as_ref()
            .map(|recovery| recovery.assistant_entry_id.clone());
        let replay_tool_starts = recovery
            .as_ref()
            .map(|recovery| recovery.replay_tool_starts.clone())
            .unwrap_or_default();
        let runtime = Arc::new(Mutex::new(EpochRuntime::new(
            Arc::clone(&self.session),
            Arc::clone(&self.artifacts),
            Arc::clone(&self.events),
            operation_id.clone(),
            epoch_id.clone(),
            configuration.identity.clone(),
            configuration.template.clone(),
            Arc::clone(&configuration.memory_collector),
            tool_definition_digests,
            tool_definition_schemas,
            replay_safe_host_tools,
            recovery_assistant_entry,
            replay_tool_starts,
        )));
        let gate: Arc<dyn EffectGate> = Arc::new(DurableEffectGate { runtime });
        let agent = configuration
            .template
            .build_agent_with_tools(gate, provenance.clone(), host_tools)?;
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
        }));
        let run = match recovery {
            Some(recovery) => {
                agent.restore_pending_tool_calls(messages, recovery.tool_calls.clone())?;
                agent.start_recover_tool_calls(recovery.tool_calls)?
            }
            None => {
                agent.restore_messages(messages)?;
                agent.start_continue()?
            }
        };
        self.install_active_agent(agent)?;
        let drive_result = run.drive().await;
        self.clear_active_agent();
        let trace_events = trace_capture.events()?;
        self.persist_trace_artifact(
            &operation_id,
            &epoch_id,
            &configuration.identity,
            &provenance,
            trace_events,
        )?;
        match drive_result {
            Ok(()) => {
                let reduction = reduce_lane(self.snapshot()?, LaneId::main())?;
                if let Some(pending) = reduction.pending_harness_activation {
                    self.finish_epoch(
                        &operation_id,
                        &epoch_id,
                        EpochFinishReason::ActivationPending,
                    )?;
                    let revision = self.activate_pending_harness(&operation_id, &pending.request)?;
                    self.publish_event(TeaEvent::Harness(HarnessEvent::RolloverStarted {
                        lane_id: LaneId::main(),
                        operation_id: operation_id.clone(),
                        from_epoch: epoch_id,
                        to_revision_id: revision.revision_id.clone(),
                    }))?;
                    let next_epoch = self.start_epoch(&operation_id)?;
                    self.publish_event(TeaEvent::Harness(HarnessEvent::RolloverCompleted {
                        lane_id: LaneId::main(),
                        operation_id: operation_id.clone(),
                        epoch_id: next_epoch.clone(),
                        revision_id: revision.revision_id,
                    }))?;
                    Box::pin(self.drive_epoch(operation_id, next_epoch, None)).await
                } else {
                    self.finish_operation(&operation_id, &epoch_id, OperationOutcome::Completed)
                }
            }
            Err(error @ tea_core::CoreError::EffectGate(_)) => Err(HarnessError::Core(error)),
            Err(error) => {
                let outcome = if matches!(error, tea_core::CoreError::Cancelled) {
                    OperationOutcome::Aborted
                } else {
                    OperationOutcome::Failed {
                        code: core_failure_code(&error).into(),
                    }
                };
                self.finish_operation(&operation_id, &epoch_id, outcome)?;
                Err(HarnessError::Core(error))
            }
        }
    }

    /// Assemble only Rust-owned stable tools for one immutable epoch. The
    /// control tool receives a frozen epoch identity and durable operation;
    /// it may stage a candidate but cannot activate it in place.
    fn host_tools_for_configuration(
        &self,
        configuration: &ResolvedHarnessConfiguration,
        operation_id: &OperationId,
    ) -> Result<ToolRegistry, HarnessError> {
        let artifact_tools = stable_artifact_tools(
            Arc::clone(&self.session),
            Arc::clone(&self.artifacts),
            configuration.template.artifact_policy_config().clone(),
        )?;
        let mut tools = ToolRegistry::default();
        if configuration.self_extension_mode.exposes_control_tool() {
            merge_tool_registries(
                &mut tools,
                stable_harness_tools(
                    Arc::clone(&self.session),
                    Arc::clone(&self.artifacts),
                    Arc::clone(&self.manager),
                    configuration.identity.clone(),
                    operation_id.clone(),
                    self.rollover_budget,
                    Arc::clone(&self.events),
                ),
            )?;
        }
        merge_tool_registries(&mut tools, artifact_tools)?;
        Ok(tools)
    }

    fn start_epoch(&self, operation_id: &OperationId) -> Result<EpochId, HarnessError> {
        let lane = LaneId::main();
        let (epoch_id, sequence, revision_id, snapshot_id, profile_id) = {
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let reduction = reduce_lane(snapshot.clone(), lane.clone())?;
            if reduction.lane_state.active_operation.as_ref() != Some(operation_id) {
                return Err(HarnessError::invalid_state(format!(
                    "operation {operation_id} is not active on main"
                )));
            }
            if let Some(plan) = &reduction.recovery_plan {
                if !matches!(&plan, RecoveryPlan::StartEpoch { operation_id: plan_operation } if plan_operation == operation_id)
                {
                    return Err(HarnessError::RecoveryRequired { plan: plan.clone() });
                }
            }
            let configuration = self.configuration_for_reduction(&reduction)?;
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
            let core_run_id = CoreRunId::new(durable_identifier(
                "core-run",
                [epoch_id.as_str(), "v1"],
            ))
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
        operation_id: &OperationId,
        epoch_id: &EpochId,
        outcome: OperationOutcome,
    ) -> Result<DurableOperation, HarnessError> {
        self.finish_epoch(
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
            let stored = session.append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                operation_id: operation_id.clone(),
                outcome: outcome.clone(),
            }))?;
            let snapshot = session.snapshot()?;
            (stored.seq, reduce_lane(snapshot, LaneId::main())?)
        };
        if reduction.lane_state.active_operation.is_some() || reduction.recovery_plan.is_some() {
            return Err(HarnessError::invalid_state(
                "terminal operation did not reduce to an idle main lane",
            ));
        }
        self.publish_event(TeaEvent::Session(SessionEvent::OperationFinished {
            sequence,
            lane_id: LaneId::main(),
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
        operation_id: &OperationId,
        epoch_id: &EpochId,
        reason: EpochFinishReason,
    ) -> Result<(), HarnessError> {
        self.session_lock()?.append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
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
        operation_id: &OperationId,
        request: &tea_session::HarnessActivationRequestedRecord,
    ) -> Result<crate::HarnessRevisionV1, HarnessError> {
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
                    }) if finished_operation == operation_id
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
        let reduction = reduce_lane(self.snapshot()?, LaneId::main())?;
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
                &LaneId::main(),
                ProvisionedEntry {
                    id: request.revision_entry_id.clone(),
                    body: SessionEntry::HarnessRevisionChanged(tea_session::HarnessRevisionChangedEntry {
                        revision_id: revision.revision_id.clone(),
                        snapshot_id: revision.snapshot_id.clone(),
                        rollback_from: matches!(revision.reason, crate::HarnessRevisionReason::Rollback)
                            .then(|| request.parent_revision_id.clone()),
                    }),
                },
            )?;
        }
        let post_activation = reduce_lane(self.snapshot()?, LaneId::main())?;
        if post_activation.pending_harness_activation.is_some()
            || post_activation.effective_configuration.harness_revision.as_ref()
                == Some(&request.parent_revision_id)
        {
            return Err(HarnessError::invalid_state(
                "harness activation entry did not advance the reduced branch revision",
            ));
        }
        self.publish_event(TeaEvent::Harness(HarnessEvent::ActivationScheduled {
            lane_id: LaneId::main(),
            operation_id: operation_id.clone(),
            candidate_id: candidate.candidate_id,
            target_revision_id: revision.revision_id.clone(),
        }))?;
        let provider_surface_changed = candidate
            .draft
            .changed_surfaces
            .contains(&crate::HarnessSurface::SystemPrompt)
            || candidate
                .draft
                .changed_surfaces
                .contains(&crate::HarnessSurface::ToolDefinitions);
        self.publish_event(TeaEvent::Harness(HarnessEvent::SnapshotActivated {
            lane_id: LaneId::main(),
            operation_id: operation_id.clone(),
            previous_revision_id: request.parent_revision_id.clone(),
            revision_id: revision.revision_id.clone(),
            snapshot_id: revision.snapshot_id.clone(),
            provider_surface_changed,
            changed_surfaces: candidate.draft.changed_surfaces.clone(),
        }))?;
        if matches!(revision.reason, crate::HarnessRevisionReason::Rollback) {
            self.publish_event(TeaEvent::Harness(HarnessEvent::RolledBack {
                lane_id: LaneId::main(),
                from_revision_id: request.parent_revision_id.clone(),
                to_revision_id: revision.revision_id.clone(),
            }))?;
        }
        Ok(revision)
    }

    fn provenance(
        &self,
        operation_id: &OperationId,
        epoch_id: &EpochId,
        identity: &HarnessIdentity,
        provider_surface_digest: Option<String>,
    ) -> Result<RunProvenance, HarnessError> {
        let snapshot = self.session_lock()?.snapshot()?;
        let session_id = snapshot.header().session_id.to_string();
        let core_run_id = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(record)
                    if &record.id == epoch_id && &record.operation_id == operation_id =>
                {
                    Some(record.core_run_id.to_string())
                }
                _ => None,
            })
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "epoch {epoch_id} has no durable core-run identity",
                ))
            })?;
        Ok(RunProvenance {
            session_id: Some(session_id),
            lane_id: Some(LaneId::main().to_string()),
            operation_id: Some(operation_id.to_string()),
            epoch_id: Some(epoch_id.to_string()),
            core_run_id: Some(core_run_id),
            harness_snapshot_id: Some(identity.snapshot_id.to_string()),
            harness_revision_id: Some(identity.revision_id.to_string()),
            model_harness_profile_id: Some(identity.profile_id.to_string()),
            provider_surface_digest,
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
        let existing = snapshot.facts().iter().find_map(|stored| match &stored.fact {
            SessionFact::TraceArtifact(existing)
                if existing.operation_id == fact.operation_id
                    && existing.epoch_id == fact.epoch_id
                    && existing.core_run_id == fact.core_run_id =>
            {
                Some(existing)
            }
            SessionFact::HarnessCatalog(_)
            | SessionFact::ToolSchemaDeviation(_)
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
        configuration: &ResolvedHarnessConfiguration,
        recovery: Option<&RecoveryToolDrive>,
    ) -> Result<Vec<AgentMessage>, HarnessError> {
        let snapshot = self.snapshot()?;
        if let Some(harness_snapshot) = &configuration.harness_snapshot {
            let limits = ProviderLimits::new(
                harness_snapshot.spec.resource_limits.provider_surface_bytes,
            )?;
            return Ok(derive_snapshot_context_with_policies(
                &snapshot,
                LaneId::main(),
                harness_snapshot,
                limits,
                &configuration.context_policies,
                recovery.map(|recovery| {
                    (
                        &recovery.assistant_entry_id,
                        recovery.tool_calls.as_slice(),
                    )
                }),
            )?
            .messages);
        }
        derive_core_messages(&snapshot, &LaneId::main())
    }

    /// Invoke only the currently relevant snapshot's process-local resume
    /// callbacks. The resolved registry converts each persisted global stable
    /// hook ID back into that plugin's local ID and never hands a policy data
    /// owned by another registration.
    fn rebuild_lifecycle_state(&self, snapshot: &SessionSnapshot) -> Result<(), HarnessError> {
        let reduction = reduce_lane(snapshot.clone(), LaneId::main())?;
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
            snapshot.records().iter().find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(record) if record.id == epoch_id => Some(record.clone()),
                _ => None,
            })
        });
        let configuration = match &epoch {
            Some(epoch) => {
                let configuration = self.configuration_for_revision(&epoch.harness_revision_id)?;
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
            None => self.configuration_for_revision(&operation.initial_harness_revision)?,
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
    fn replay_is_still_safe(&self, tool: &ToolStartedRecord) -> bool {
        self.configuration_for_revision(&tool.harness_revision_id)
            .is_ok_and(|configuration| {
                self.host_tools_for_configuration(&configuration, &tool.operation_id)
                    .and_then(|host_tools| {
                        let definitions = all_tool_definition_digests(
                            &configuration.template,
                            &host_tools,
                        )?;
                        let current = definitions.get(&tool.tool_name).ok_or_else(|| {
                            HarnessError::invalid_state(format!(
                                "recovery tool {} is no longer declared",
                                tool.tool_name,
                            ))
                        })?;
                        Ok(
                            (configuration.template.is_replay_safe(&tool.tool_name)
                                || replay_safe_host_tools(&host_tools)
                                    .contains(&tool.tool_name))
                                && tool.harness_revision_id == *configuration.identity.revision_id()
                                && current == &tool.tool_definition_digest,
                        )
                    })
                    .unwrap_or(false)
            })
    }

    fn append_interrupted_tool_result(
        &self,
        snapshot: &SessionSnapshot,
        result_entry_id: &EntryId,
    ) -> Result<(), HarnessError> {
        let started = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::ToolStarted(started) if &started.result_entry_id == result_entry_id => {
                    Some(started.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                HarnessError::invalid_state(
                    "recovery requested an interrupted tool result with no durable intent",
                )
            })?;
        let tool_call_id = ToolCallId::new(started.tool_call_id.clone()).map_err(|error| {
            HarnessError::invalid_state(format!("stored tool invocation has invalid call ID: {error}"))
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
        let configuration = self.configuration_for_revision(&started.harness_revision_id)?;
        let retained = retain_tool_result_with_projection(
            self.artifacts.as_ref(),
            configuration.template.artifact_policy_config(),
            &result,
            &result,
        )
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let entry = tool_result_entry(&result, &result, &started.tool_name, retained)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        self.session_lock()?.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: result_entry_id.clone(),
                body: SessionEntry::ToolResult(entry),
            },
        )?;
        Ok(())
    }

    fn configuration_for_reduction(
        &self,
        reduction: &tea_session::LaneReduction,
    ) -> Result<ResolvedHarnessConfiguration, HarnessError> {
        let revision_id = reduction
            .lane_state
            .active_harness_revision
            .as_ref()
            .ok_or_else(|| {
                HarnessError::invalid_state(
                    "durable lane has no committed active harness revision",
                )
            })?;
        self.manager.resolve_revision(revision_id)
    }

    fn configuration_for_revision(
        &self,
        revision_id: &HarnessRevisionId,
    ) -> Result<ResolvedHarnessConfiguration, HarnessError> {
        self.manager.resolve_revision(revision_id)
    }

    fn epoch_configuration(
        &self,
        epoch_id: &EpochId,
    ) -> Result<ResolvedHarnessConfiguration, HarnessError> {
        let snapshot = self.snapshot()?;
        let started = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(record) if &record.id == epoch_id => Some(record),
                _ => None,
            })
            .ok_or_else(|| HarnessError::invalid_state(format!(
                "epoch {epoch_id} has no durable start record",
            )))?;
        let configuration = self.configuration_for_revision(&started.harness_revision_id)?;
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

impl DurableHarness<tea_session::JsonlSession> {
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

fn validate_reserved_host_tool_names(template: &CoreEpochTemplate) -> Result<(), HarnessError> {
    for name in STABLE_ARTIFACT_TOOL_NAMES {
        if template.tools().get(name).is_some() {
            return Err(HarnessError::invalid_state(format!(
                "harness templates cannot replace the reserved host tool {name}"
            )));
        }
    }
    if template.tools().get(STABLE_HARNESS_TOOL_NAME).is_some() {
        return Err(HarnessError::invalid_state(format!(
            "harness templates cannot replace the reserved host tool {STABLE_HARNESS_TOOL_NAME}"
        )));
    }
    Ok(())
}

fn latest_harness_catalog(
    snapshot: &SessionSnapshot,
) -> Option<&tea_session::HarnessCatalogFact> {
    snapshot.facts().iter().rev().find_map(|stored| match &stored.fact {
        SessionFact::HarnessCatalog(catalog) => Some(catalog),
        SessionFact::ToolSchemaDeviation(_)
        | SessionFact::TraceArtifact(_)
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
        .filter(|name| tools.get(name).is_some())
        .map(str::to_owned)
        .collect()
}

struct OperationClaim<'a> {
    active: &'a AtomicBool,
}

/// Bridges core's awaited run-local observer boundary into the bounded
/// application fanout. This intentionally has no session writer: core events
/// are observational, while durable state continues to flow through the
/// supervisor's explicit commit procedures.
struct HarnessAgentEventObserver {
    events: Arc<EventHub>,
}

/// An in-memory staging sink for one core run's trace. The supervisor writes
/// its redacted JSON Lines artifact and durable fact only after core has
/// emitted its terminal event, before it closes the owning epoch.
#[derive(Clone, Default)]
struct TraceCaptureSink {
    events: Arc<Mutex<Vec<TraceEvent>>>,
}

impl TraceCaptureSink {
    fn events(&self) -> Result<Vec<TraceEvent>, HarnessError> {
        self.events
            .lock()
            .map(|events| events.clone())
            .map_err(|_| HarnessError::invalid_state("trace capture mutex is poisoned"))
    }
}

impl TraceSink for TraceCaptureSink {
    type Error = Infallible;

    fn append(&mut self, event: TraceEvent) -> Result<(), Self::Error> {
        self.events
            .lock()
            .expect("trace capture mutex is not reentered by a trace observer")
            .push(event);
        Ok(())
    }
}

/// Redacts every content-bearing trace field before it reaches durable
/// storage. Provenance, identities, cache evidence, and lifecycle labels stay
/// intact, while prompts, model output, tool arguments/results, and terminal
/// diagnostics never enter the immutable trace artifact.
struct DurableTraceRedactor;

impl Redactor for DurableTraceRedactor {
    fn redact(&self, event: TraceEvent) -> TraceEvent {
        match event {
            TraceEvent::Turn(mut turn) => {
                if !turn.input.is_empty() {
                    turn.input = "[redacted]".into();
                }
                if turn.output.is_some() {
                    turn.output = Some("[redacted]".into());
                }
                TraceEvent::Turn(turn)
            }
            TraceEvent::Tool(mut tool) => {
                if !tool.input.is_empty() {
                    tool.input = "[redacted]".into();
                }
                if tool.output.is_some() {
                    tool.output = Some("[redacted]".into());
                }
                if tool.error.is_some() {
                    tool.error = Some("[redacted]".into());
                }
                TraceEvent::Tool(tool)
            }
            TraceEvent::EpisodeEnd(mut end) => {
                if end.error.is_some() {
                    end.error = Some("[redacted]".into());
                }
                TraceEvent::EpisodeEnd(end)
            }
            event @ (TraceEvent::EpisodeHeader(_) | TraceEvent::Compaction(_)) => event,
        }
    }
}

impl EventObserver for HarnessAgentEventObserver {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: tea_core::scheduler::CancellationToken,
    ) -> ObserverFuture<'a> {
        self.events.publish(TeaEvent::Agent(event.clone()));
        Box::pin(std::future::ready(Ok(())))
    }
}

impl Drop for OperationClaim<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
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
    template: CoreEpochTemplate,
    memory_collector: Arc<PolicyMemoryCollector>,
    /// Exact declarations for both template capabilities and stable Rust host
    /// tools participating in this epoch.
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
    pending_providers: BTreeMap<tea_core::EffectId, PendingProvider>,
    pending_tools: BTreeMap<tea_core::EffectId, PendingTool>,
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

impl<S> EpochRuntime<S>
where
    S: SessionWriter + Send + 'static,
{
    fn new(
        session: Arc<Mutex<S>>,
        artifacts: Arc<dyn ArtifactStore>,
        events: Arc<EventHub>,
        operation_id: OperationId,
        epoch_id: EpochId,
        identity: HarnessIdentity,
        template: CoreEpochTemplate,
        memory_collector: Arc<PolicyMemoryCollector>,
        tool_definition_digests: BTreeMap<String, Digest>,
        tool_definition_schemas: BTreeMap<String, JsonValue>,
        replay_safe_host_tools: BTreeSet<String>,
        last_assistant_entry: Option<EntryId>,
        replay_tool_starts: BTreeMap<(EntryId, u32), ToolStartedRecord>,
    ) -> Self {
        Self {
            session,
            artifacts,
            events,
            lane: LaneId::main(),
            operation_id,
            epoch_id,
            identity,
            template,
            memory_collector,
            tool_definition_digests,
            tool_definition_schemas,
            replay_safe_host_tools,
            replay_tool_starts,
            pending_providers: BTreeMap::new(),
            pending_tools: BTreeMap::new(),
            started_tool_indices: BTreeMap::new(),
            last_assistant_entry,
            fault: None,
        }
    }

    fn before(&mut self, action: EffectAction) -> Result<(), EffectGateError> {
        self.ensure_healthy()?;
        match action.subject() {
            EffectSubject::DurableWrite { write } => self.before_durable_write(write),
            EffectSubject::ProviderRequest { request } => self.before_provider(action.id(), request),
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
                self.after_tool(action.id(), call, outcome)
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

    fn before_durable_write(
        &mut self,
        write: &DurableWriteRequest,
    ) -> Result<(), EffectGateError> {
        match write {
            DurableWriteRequest::ToolResult { call, result } => {
                // Immediate schema/policy failures have no ToolExecution
                // settlement and therefore materialize here. Executed tools
                // were already persisted by `after_tool` before the core can
                // emit their end event; this call verifies that exact durable
                // entry instead of writing a second result.
                if result
                    .failure
                    .as_ref()
                    .is_some_and(|failure| {
                        failure.disposition() == ToolFailureDisposition::InvalidArguments
                    })
                {
                    self.persist_tool_schema_deviation(call)?;
                }
                self.persist_tool_result(call, result, result)
            }
        }
    }

    fn before_provider(
        &mut self,
        action_id: tea_core::EffectId,
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
        let result_entry_id = EntryId::new(durable_identifier(
            "entry-assistant",
            [step_id.as_str()],
        ))
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
            session.append_record(LaneRecord::ProviderRequestStarted(ProviderRequestStartedRecord {
                request_id: request_id.clone(),
                operation_id,
                epoch_id,
                step_id,
                physical_attempt: 1,
                model_harness_profile: profile_id,
                request_surface_digest,
                idempotency_key: None,
            }))?;
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
        action_id: tea_core::EffectId,
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

    fn before_tool(
        &mut self,
        action_id: tea_core::EffectId,
        call: &ToolCall,
    ) -> Result<(), EffectGateError> {
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
        let effective_args = JsonValue::parse(call.arguments.as_str())
            .map_err(|error| self.fault(format!("tool arguments cannot enter durable WAL: {error}")))?;
        let definition_digest = self
            .tool_definition_digests
            .get(&call.name)
            .cloned()
            .ok_or_else(|| self.fault(format!(
                "durable tool intent names unknown executable tool {}",
                call.name,
            )))?;
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
        let replay = if self.template.is_replay_safe(&call.name)
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
            .position(|candidate| {
                candidate.id == call.id.as_str() && candidate.name == call.name
            })
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
        let retained = if is_direct_recovery_tool(&call.name) {
            retain_direct_recovery_result_with_projection(
                self.template.artifact_policy_config(),
                raw_result,
                model_result,
            )
        } else {
            retain_tool_result_with_projection(
                self.artifacts.as_ref(),
                self.template.artifact_policy_config(),
                raw_result,
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
            self.events.publish(TeaEvent::Artifact(ArtifactEvent::Retained {
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
        if self.template.artifact_policy_config().redact_before_persist {
            return Err(self.fault(
                "schema-deviation raw argument capture requires an installed host redactor when the artifact policy requires redaction",
            ));
        }
        let assistant_entry_id = self
            .last_assistant_entry
            .clone()
            .ok_or_else(|| self.fault("schema-deviation evidence has no durable assistant entry"))?;
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
                Ok(arguments) => crate::inspect_tool_schema_deviation(
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
            self.events.publish(TeaEvent::Artifact(ArtifactEvent::Retained {
                artifact_id: descriptor.artifact_id,
                byte_len: descriptor.byte_len,
                policy_id: self.template.artifact_policy_config().policy_id.clone(),
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
            .map_err(|error| self.fault(format!("cannot consume post-tool memory proposals: {error}")))?;
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
                    collected.plugin_id.as_str(),
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
        action_id: tea_core::EffectId,
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
        action_id: tea_core::EffectId,
        hook: &HookInvocation,
        phase: &str,
        outcome: Option<&HookEffectOutcome>,
    ) -> Result<(), EffectGateError> {
        let mut fields = vec![
            ("operation_id", JsonValue::String(self.operation_id.to_string())),
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

pub(crate) fn durable_identifier<'a>(kind: &str, values: impl IntoIterator<Item = &'a str>) -> String {
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
    writer.discriminant("thinking_level", thinking_discriminant(request.thinking_level));
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
    template: &CoreEpochTemplate,
    host_tools: &ToolRegistry,
) -> Result<BTreeMap<String, Digest>, HarnessError> {
    let mut digests = BTreeMap::new();
    for registry in [template.tools(), host_tools] {
        for name in registry.names() {
            let tool = registry
                .get(name)
                .expect("registered executable tool remains present");
            let digest = tool_definition_digest(tool.as_ref())?;
            if digests.insert(name.to_owned(), digest).is_some() {
                return Err(HarnessError::invalid_state(format!(
                    "immutable template and stable host tools both declare {name}",
                )));
            }
        }
    }
    Ok(digests)
}

fn all_tool_definition_schemas(
    template: &CoreEpochTemplate,
    host_tools: &ToolRegistry,
) -> Result<BTreeMap<String, JsonValue>, HarnessError> {
    let mut schemas = BTreeMap::new();
    for registry in [template.tools(), host_tools] {
        for name in registry.names() {
            let tool = registry
                .get(name)
                .expect("registered executable tool remains present");
            if schemas.insert(name.to_owned(), tool.schema().clone()).is_some() {
                return Err(HarnessError::invalid_state(format!(
                    "immutable template and stable host tools both declare {name}",
                )));
            }
        }
    }
    Ok(schemas)
}

fn tool_definition_digest(tool: &dyn AgentTool) -> Result<Digest, HarnessError> {
    let mut writer = CanonicalHashWriter::new("tea-tool-definition-v1", 1, 1);
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
    Ok(writer.finish())
}

fn assistant_entry(response: &ProviderResponse) -> Result<tea_session::AssistantMessageEntry, EffectGateError> {
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
        usage: raw_result.usage.as_ref().map(core_usage).unwrap_or_default(),
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
    collected: tea_luau::CollectedPolicyMemoryProposal,
) -> Result<PluginMemoryEntry, EffectGateError> {
    let proposal = collected.proposal;
    if !portable_memory_label(&collected.plugin_id) || !portable_memory_label(&proposal.kind) {
        return Err(EffectGateError::new(
            "post-tool memory proposal has an invalid plugin ID or kind",
        ));
    }
    let encoded = proposal
        .content
        .to_json_string()
        .map_err(|error| EffectGateError::new(format!("cannot encode post-tool memory: {error}")))?;
    if encoded.len() > 16 * 1024 {
        return Err(EffectGateError::new(
            "post-tool memory proposal exceeds the 16384 byte inline limit",
        ));
    }
    if proposal.provenance.len() > 32
        || proposal.provenance.iter().any(|value| {
            value.is_empty()
                || value.len() > 200
                || value.chars().any(char::is_control)
        })
    {
        return Err(EffectGateError::new(
            "post-tool memory proposal has invalid provenance values",
        ));
    }
    Ok(PluginMemoryEntry {
        plugin_id: collected.plugin_id,
        kind: proposal.kind,
        content: PayloadRef::Inline(proposal.content),
        provenance: proposal.provenance,
        visibility: match proposal.visibility {
            PolicyMemoryVisibility::ModelVisible => MemoryVisibility::ModelVisible,
            PolicyMemoryVisibility::ExternalOnly => MemoryVisibility::ExternalOnly,
        },
        retention: match proposal.retention {
            PolicyMemoryRetention::Session => MemoryRetention::Session,
            PolicyMemoryRetention::Checkpoint => MemoryRetention::Checkpoint,
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
        return Err("durable tool-result identity was materialized with another semantic type".into());
    };
    if stored.tool_call_id != model_result.tool_call_id.as_str()
        || stored.tool_name != tool_name
        || stored.is_error != model_result.is_error
        || stored.terminate != model_result.terminate
    {
        return Err(
            "durable tool-result identity was materialized with a different tool settlement"
                .into(),
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
        ("context_overflow", JsonValue::Bool(response.context_overflow)),
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

fn core_failure_code(error: &tea_core::CoreError) -> &'static str {
    match error {
        tea_core::CoreError::Cancelled => "cancelled",
        tea_core::CoreError::ModelError { .. } => "model_error",
        tea_core::CoreError::ModelAborted { .. } => "model_aborted",
        tea_core::CoreError::ModelProvider { .. } => "provider_error",
        tea_core::CoreError::ToolCircuitBreaker { .. } => "tool_circuit_breaker",
        tea_core::CoreError::Hook(_) => "hook_error",
        tea_core::CoreError::EffectGate(_) => "effect_gate_error",
        _ => "core_error",
    }
}

fn derive_core_messages(
    snapshot: &SessionSnapshot,
    lane: &LaneId,
) -> Result<Vec<AgentMessage>, HarnessError> {
    let mut messages = Vec::new();
    for entry in snapshot.entries().iter().filter(|entry| &entry.lane_id == lane) {
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
                    tool_call_id: ToolCallId::new(result.tool_call_id.clone()).map_err(|error| {
                        HarnessError::invalid_state(format!(
                            "durable tool-result call ID is invalid: {error}"
                        ))
                    })?,
                    tool_name: result.tool_name.clone(),
                    content,
                    details: details.map(SerializedJson::new),
                    usage: Some(tea_core::state::Usage {
                        input_tokens: result.usage.input_tokens,
                        output_tokens: result.usage.output_tokens,
                        reasoning_tokens: result.usage.reasoning_tokens,
                        cache_read_tokens: result.usage.cache_read_tokens,
                        cache_write_tokens: result.usage.cache_write_tokens,
                        cost: result.usage.cost.clone(),
                    }),
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
                )))
            }
            SessionEntry::PluginMemory(_) | SessionEntry::Custom(_) => {}
        }
    }
    Ok(messages)
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
    snapshot.records().iter().rev().find_map(|stored| match &stored.record {
        LaneRecord::EpochStarted(record)
            if &record.operation_id == operation_id && !finished.contains(&record.id) =>
        {
            Some(record.id.clone())
        }
        _ => None,
    })
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
    let SessionEntry::AssistantMessage(assistant) = &snapshot.entries()[assistant_index].body else {
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
        let result = trailing_results.iter().find(|result| {
            result.tool_call_id == call.id && result.tool_name == call.name
        });
        match (first_missing, result) {
            (None, Some(_)) => {}
            (None, None) => first_missing = Some(index),
            (Some(_), None) => {}
            (Some(_), Some(_)) => {
                return Err(HarnessError::invalid_state(
                    "durable recovered tool results are not a source-order prefix",
                ))
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
                arguments: SerializedJson::new(call.arguments.to_json_string().map_err(|error| {
                    HarnessError::invalid_state(format!(
                        "recovery assistant arguments cannot encode: {error}"
                    ))
                })?),
            })
        })
        .collect()
}
