//! Generic lane-operation helpers live with the supervisor implementation.
//!
//! Keeping this module explicit makes the one-lane-at-a-time claim a lane
//! invariant instead of a property accidentally attached to the root host.

use super::extension_state::{
    extension_state_commit_item, extension_state_view, settled_turn_checkpoint_item_with_state,
};
use super::{ExtensionContinuation, LaneRuntime, SessionSupervisor, durable_identifier};
use crate::harness::HarnessError;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use tea_core::harness::extension::ExtensionCommandInput;
use tea_session::{
    EntryId, ExtensionControlAppliedRecord, ExtensionStateValue, InputAcceptedRecord,
    InputSettledRecord, InputStatus, InputWithdrawnRecord, LaneRecord, OperationId, OperationKind,
    OperationOutcome, OperationStartedRecord, PendingExtensionControl, ProvisionedEntry,
    SessionCommit, SessionCommitItem, SessionEntry, SessionFact, SessionWriter, reduce_lane,
};

/// The durable terminal result for one accepted user input.
///
/// The result becomes visible only after the input's owning operation and its
/// membership settlement have committed. It is deliberately separate from
/// the lossy observation subscription: a terminal UI update can be dropped
/// without losing an embedding's completion signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputCompletion {
    input_id: EntryId,
    outcome: InputOutcome,
}

/// The terminal disposition of one accepted input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputOutcome {
    /// The input was atomically returned to local composition before dispatch.
    Withdrawn,
    /// The input's owning durable operation settled.
    Operation {
        /// Durable operation that dispatched the input.
        operation_id: OperationId,
        /// Exact terminal classification shared with that operation.
        outcome: OperationOutcome,
    },
}

impl InputCompletion {
    pub(crate) fn new(
        input_id: EntryId,
        operation_id: OperationId,
        outcome: OperationOutcome,
    ) -> Self {
        Self {
            input_id,
            outcome: InputOutcome::Operation {
                operation_id,
                outcome,
            },
        }
    }

    pub(crate) fn withdrawn(input_id: EntryId) -> Self {
        Self {
            input_id,
            outcome: InputOutcome::Withdrawn,
        }
    }

    /// Return the accepted input that settled.
    pub fn input_id(&self) -> &EntryId {
        &self.input_id
    }

    /// Return the durable operation that dispatched this input, when any.
    pub fn operation_id(&self) -> Option<&OperationId> {
        match &self.outcome {
            InputOutcome::Withdrawn => None,
            InputOutcome::Operation { operation_id, .. } => Some(operation_id),
        }
    }

    /// Return the committed terminal input disposition.
    pub fn outcome(&self) -> &InputOutcome {
        &self.outcome
    }
}

/// Queryable local completion endpoint for one accepted input.
///
/// Handles are process-local waiters over a durable result. A handle created
/// before a process crash does not survive it, but callers can query the
/// supervisor's durable input state after reopen instead of inferring an
/// outcome from event delivery or global idleness.
#[derive(Clone, Debug)]
pub struct InputCompletionHandle {
    input_id: EntryId,
    cell: Arc<InputCompletionCell>,
}

impl InputCompletionHandle {
    /// Return the input identity whose result this handle observes.
    pub fn input_id(&self) -> &EntryId {
        &self.input_id
    }

    /// Return a completed result without waiting.
    pub fn try_result(&self) -> Option<InputCompletion> {
        self.cell
            .state
            .lock()
            .expect("input completion state mutex is poisoned")
            .completion
            .clone()
    }

    /// Return a future that resolves when this one input settles.
    pub fn wait(&self) -> InputCompletionFuture {
        InputCompletionFuture {
            cell: Arc::clone(&self.cell),
            waiter_id: self.cell.next_waiter_id.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// One caller-polled wait for an [`InputCompletionHandle`].
#[derive(Debug)]
pub struct InputCompletionFuture {
    cell: Arc<InputCompletionCell>,
    waiter_id: u64,
}

impl Future for InputCompletionFuture {
    type Output = InputCompletion;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self
            .cell
            .state
            .lock()
            .expect("input completion state mutex is poisoned");
        if let Some(completion) = &state.completion {
            return Poll::Ready(completion.clone());
        }
        match state.waiters.get_mut(&self.waiter_id) {
            Some(waiter) if waiter.will_wake(context.waker()) => {}
            Some(waiter) => *waiter = context.waker().clone(),
            None => {
                state
                    .waiters
                    .insert(self.waiter_id, context.waker().clone());
            }
        }
        Poll::Pending
    }
}

impl Drop for InputCompletionFuture {
    fn drop(&mut self) {
        let Ok(mut state) = self.cell.state.lock() else {
            return;
        };
        if state.completion.is_none() {
            state.waiters.remove(&self.waiter_id);
        }
    }
}

#[derive(Debug, Default)]
struct InputCompletionState {
    completion: Option<InputCompletion>,
    waiters: BTreeMap<u64, Waker>,
}

#[derive(Debug, Default)]
struct InputCompletionCell {
    state: Mutex<InputCompletionState>,
    next_waiter_id: AtomicU64,
}

/// Process-local fan-in for independently durable input settlements.
///
/// It contains no authoritative session state. The supervisor inserts a
/// result only after the matching `InputSettled` durable commit succeeds.
#[derive(Debug, Default)]
pub(crate) struct InputCompletionRegistry {
    cells: Mutex<BTreeMap<EntryId, Weak<InputCompletionCell>>>,
}

impl InputCompletionRegistry {
    pub(crate) fn register(&self, input_id: EntryId) -> InputCompletionHandle {
        let cell = self.cell_for(&input_id);
        InputCompletionHandle { input_id, cell }
    }

    /// Deliver a result to any process-local handles that still exist.
    ///
    /// A durable completion is already authoritative when this runs. Conflicts
    /// and misbehaving wakers therefore cannot turn that committed result into
    /// an operation error. A caller that needs to diagnose a stale or
    /// conflicting local handle can re-read the durable input disposition.
    pub(crate) fn settle(&self, completion: InputCompletion) {
        let cell = self.cell_for(&completion.input_id);
        let waiters = {
            let mut state = cell
                .state
                .lock()
                .expect("input completion state mutex is poisoned");
            match &state.completion {
                Some(existing) if existing == &completion => return,
                Some(_) => return,
                None => {
                    state.completion = Some(completion);
                    std::mem::take(&mut state.waiters)
                }
            }
        };
        for waiter in waiters.into_values() {
            // `Wake` is supplied by an embedding executor. It must not be
            // allowed to unwind across a completed durable transaction.
            let _ = catch_unwind(AssertUnwindSafe(|| waiter.wake()));
        }
    }

    fn cell_for(&self, input_id: &EntryId) -> Arc<InputCompletionCell> {
        let mut cells = self
            .cells
            .lock()
            .expect("input completion registry mutex is poisoned");
        cells.retain(|_, cell| cell.strong_count() > 0);
        if let Some(cell) = cells.get(input_id).and_then(Weak::upgrade) {
            return cell;
        }
        let cell = Arc::new(InputCompletionCell::default());
        cells.insert(input_id.clone(), Arc::downgrade(&cell));
        cell
    }
}

/// One user input that the runtime has durably accepted.
///
/// Acceptance does not start model work. The runtime retains the payload and
/// queue position until a host explicitly drives it, withdraws it before
/// dispatch, or a dispatched operation reaches a durable terminal outcome.
#[derive(Clone, Debug)]
pub struct AcceptedInput {
    input_id: EntryId,
    completion: InputCompletionHandle,
}

impl AcceptedInput {
    pub(crate) fn new(input_id: EntryId, completion: InputCompletionHandle) -> Self {
        Self {
            input_id,
            completion,
        }
    }

    /// Return the stable durable identity of the accepted input.
    pub fn id(&self) -> &EntryId {
        &self.input_id
    }

    /// Return this input's completion endpoint.
    pub fn completion(&self) -> &InputCompletionHandle {
        &self.completion
    }
}

/// A presentation-safe view of one input still waiting for dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedInput {
    input_id: EntryId,
    content: String,
}

impl QueuedInput {
    pub(crate) fn new(input_id: EntryId, content: String) -> Self {
        Self { input_id, content }
    }

    /// Return the accepted input identity.
    pub fn id(&self) -> &EntryId {
        &self.input_id
    }

    /// Return the exact user text retained for later dispatch.
    pub fn content(&self) -> &str {
        &self.content
    }
}

/// Current durable lifecycle state for one accepted input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputDisposition {
    /// The input remains queued and eligible for explicit dispatch.
    Pending,
    /// The input was atomically removed before it could dispatch.
    Withdrawn,
    /// One live durable operation owns the input.
    Dispatched {
        /// Owning operation.
        operation_id: OperationId,
    },
    /// The operation completed and the input settlement committed.
    Settled {
        /// Owning operation.
        operation_id: OperationId,
        /// Exact shared terminal classification.
        outcome: OperationOutcome,
    },
}

/// Inputs atomically returned from the runtime queue to local composition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WithdrawnInputs {
    inputs: Vec<QueuedInput>,
}

impl WithdrawnInputs {
    pub(crate) fn new(inputs: Vec<QueuedInput>) -> Self {
        Self { inputs }
    }

    /// Return inputs in the caller-requested order.
    pub fn inputs(&self) -> &[QueuedInput] {
        &self.inputs
    }

    /// Consume the ordered withdrawn inputs.
    pub fn into_inputs(self) -> Vec<QueuedInput> {
        self.inputs
    }
}

/// Explicit authority to evaluate extension idle continuation policy.
///
/// This is a caller-supplied live decision, never a persisted `active` bit.
/// Reopening a session therefore cannot autonomously continue a goal: the
/// host must make a fresh explicit call with this authorization.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IdleAuthorization {
    /// Drain only explicitly accepted user input.
    #[default]
    UserInputOnly,
    /// After controls and user input, permit one extension idle continuation.
    AllowExtensionContinuation,
}

impl IdleAuthorization {
    /// Return whether automatic extension continuation is explicitly allowed.
    pub fn allows_extension_continuation(self) -> bool {
        matches!(self, Self::AllowExtensionContinuation)
    }
}

/// Result from one host-driven idle advancement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdleDriveOutcome {
    /// No accepted input or authorized extension continuation was available.
    Idle,
    /// One batch of accepted inputs settled through one durable operation.
    Inputs {
        /// Terminal durable operation.
        operation: super::DurableOperation,
        /// Accepted input membership in dispatch order.
        input_ids: Vec<EntryId>,
    },
    /// One extension-owned internal continuation settled through an operation.
    ExtensionContinuation {
        /// Terminal durable operation.
        operation: super::DurableOperation,
    },
}

/// Whether an extension command was applied immediately or durably queued.
#[derive(Clone, Debug, PartialEq)]
pub enum ExtensionCommandAdmission {
    /// The command ran while the lane was idle and committed its result.
    Applied(super::ExtensionCommandDispatch),
    /// The command is committed and will run after its target operation settles.
    Queued {
        /// Stable runtime control identity.
        control_id: String,
    },
}

impl<S> SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Accept one user input into the root lane's durable queue.
    ///
    /// This commits only admission. A caller must explicitly drive the queue
    /// through [`Self::drive_next_input`] before provider or tool work begins.
    pub fn submit_input(&self, content: impl Into<String>) -> Result<AcceptedInput, HarnessError> {
        self.submit_input_with_authoring_authorization(content.into(), false)
    }

    pub(super) fn submit_input_with_authoring_authorization(
        &self,
        content: String,
        authoring_authorized: bool,
    ) -> Result<AcceptedInput, HarnessError> {
        let lane = self.root_lane()?;
        let (input_id, completion, sequence) = {
            let _gate = self.operation_gate_lock()?;
            self.ensure_open()?;
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let sequence = snapshot.next_sequence().0.to_string();
            let input_id = EntryId::new(durable_identifier(
                "accepted-input",
                [
                    snapshot.header().session_id.as_str(),
                    sequence.as_str(),
                    content.as_str(),
                ],
            ))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let mut entry = ProvisionedEntry::user(input_id.clone(), content);
            if authoring_authorized {
                let SessionEntry::UserMessage(user) = &mut entry.body else {
                    unreachable!("user entry constructor must produce a user message");
                };
                user.metadata.insert(
                    crate::harness::AUTHORING_AUTHORIZATION_METADATA_KEY.into(),
                    tea_protocol::JsonValue::Bool(true),
                );
            }
            // Install the local endpoint before the durable admission. A
            // concurrent drive may settle immediately after this commit, so a
            // post-commit status query would make successful admission appear
            // to fail if that query itself could not run.
            let completion = self.input_completions.register(input_id.clone());
            let stored = session.commit(SessionCommit::one(SessionCommitItem::Record(
                LaneRecord::InputAccepted(InputAcceptedRecord {
                    lane_id: lane.lane_id.clone(),
                    entry,
                }),
            )))?;
            (input_id, completion, stored.seq)
        };
        // Observation is lossy and never changes the outcome of the committed
        // admission. Consumers refresh `queued_inputs` from durable state.
        let _ = self.publish_event(tea_core::runtime::TeaEvent::Session(
            tea_core::runtime::SessionEvent::InputQueueChanged {
                sequence,
                lane_id: lane.lane_id.clone(),
            },
        ));
        Ok(AcceptedInput::new(input_id, completion))
    }

    /// Return root-lane inputs that are still eligible for explicit dispatch.
    pub fn queued_inputs(&self) -> Result<Vec<QueuedInput>, HarnessError> {
        let lane = self.root_lane()?;
        let reduction = reduce_lane(self.snapshot()?, lane.lane_id.clone())?;
        queued_inputs_from_reduction(&reduction.input_reduction.pending_inputs)
    }

    /// Return the authoritative durable disposition for one accepted input.
    pub fn input_status(
        &self,
        input_id: &EntryId,
    ) -> Result<Option<InputDisposition>, HarnessError> {
        let lane = self.root_lane()?;
        let reduction = reduce_lane(self.snapshot()?, lane.lane_id.clone())?;
        Ok(reduction
            .input_reduction
            .input_states
            .get(input_id)
            .map(|state| input_disposition(&state.status)))
    }

    /// Query a durable input completion after a restart or without retaining
    /// the process-local completion handle.
    pub fn input_completion(
        &self,
        input_id: &EntryId,
    ) -> Result<Option<InputCompletion>, HarnessError> {
        let Some(status) = self.input_status(input_id)? else {
            return Ok(None);
        };
        Ok(match status {
            InputDisposition::Pending | InputDisposition::Dispatched { .. } => None,
            InputDisposition::Withdrawn => Some(InputCompletion::withdrawn(input_id.clone())),
            InputDisposition::Settled {
                operation_id,
                outcome,
            } => Some(InputCompletion::new(
                input_id.clone(),
                operation_id,
                outcome,
            )),
        })
    }

    /// Reconstruct a process-local completion endpoint for an accepted input.
    ///
    /// This is useful after a reopen or when the original admission handle was
    /// dropped. The durable status is checked only after installing the local
    /// cell, so a concurrent terminal commit cannot be missed between a status
    /// read and waiter registration.
    pub fn input_completion_handle(
        &self,
        input_id: &EntryId,
    ) -> Result<Option<InputCompletionHandle>, HarnessError> {
        let handle = self.input_completions.register(input_id.clone());
        let Some(status) = self.input_status(input_id)? else {
            return Ok(None);
        };
        if let Some(completion) = completion_from_disposition(input_id.clone(), &status) {
            self.input_completions.settle(completion);
        }
        Ok(Some(handle))
    }

    /// Atomically remove a projection slot's undispatched inputs and return
    /// their exact retained payloads in caller order.
    ///
    /// The operation fails without writing anything when an ID is unknown,
    /// duplicated, withdrawn, or already dispatched. This lets a terminal put
    /// a combined next-message slot back into local composition without
    /// erasing accepted history or racing a started operation.
    pub fn withdraw_inputs(&self, input_ids: &[EntryId]) -> Result<WithdrawnInputs, HarnessError> {
        if input_ids.is_empty() {
            return Err(HarnessError::invalid_state(
                "withdrawing inputs requires at least one accepted input ID",
            ));
        }
        let lane = self.root_lane()?;
        let (withdrawn, sequence) = {
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let reduction = reduce_lane(snapshot, lane.lane_id.clone())?;
            let mut seen = BTreeSet::new();
            let mut inputs = Vec::with_capacity(input_ids.len());
            for input_id in input_ids {
                if !seen.insert(input_id.clone()) {
                    return Err(HarnessError::invalid_state(format!(
                        "input {input_id} was requested more than once for withdrawal",
                    )));
                }
                let state = reduction
                    .input_reduction
                    .input_states
                    .get(input_id)
                    .ok_or_else(|| {
                        HarnessError::invalid_state(format!("accepted input {input_id} is unknown"))
                    })?;
                if !matches!(state.status, InputStatus::Pending) {
                    return Err(HarnessError::invalid_state(format!(
                        "accepted input {input_id} is no longer eligible for withdrawal",
                    )));
                }
                inputs.push(queued_input_from_accepted(&state.accepted)?);
            }
            let records = input_ids
                .iter()
                .cloned()
                .map(|input_id| {
                    SessionCommitItem::Record(LaneRecord::InputWithdrawn(InputWithdrawnRecord {
                        lane_id: lane.lane_id.clone(),
                        input_id,
                    }))
                })
                .collect();
            let stored = session.commit(SessionCommit::new(records)?)?;
            (inputs, stored.seq)
        };
        // Publish before waking local completion waiters so callback code
        // cannot observe a withdrawal before the subscription has a durable
        // queue-change signal to consume.
        let _ = self.publish_event(tea_core::runtime::TeaEvent::Session(
            tea_core::runtime::SessionEvent::InputQueueChanged {
                sequence,
                lane_id: lane.lane_id.clone(),
            },
        ));
        for input in &withdrawn {
            self.input_completions
                .settle(InputCompletion::withdrawn(input.input_id.clone()));
        }
        Ok(WithdrawnInputs::new(withdrawn))
    }

    /// Drive one root-lane queue decision under fresh host authorization.
    ///
    /// Input admission never invokes this method implicitly. If multiple
    /// inputs are pending, they become one operation with ordered durable
    /// membership and are all settled together. Extension-control and goal
    /// continuation processing is layered before the input decision by the
    /// runtime's idle-control path.
    pub async fn drive_next_input(
        &self,
        authorization: IdleAuthorization,
    ) -> Result<IdleDriveOutcome, HarnessError> {
        let lane = self.root_lane()?;
        let _claim = self.claim_lane_operation(Arc::clone(&lane))?;
        self.drive_next_input_claimed(&lane, authorization).await
    }

    /// Advance the root queue while the caller already owns its local drive
    /// claim.
    ///
    /// The convenience root-prompt path claims before it accepts input so a
    /// concurrent drive or blocked recovery cannot leave an unreported input
    /// behind. Public callers should use [`Self::drive_next_input`], which
    /// obtains this claim itself.
    pub(super) async fn drive_next_input_claimed(
        &self,
        lane: &Arc<LaneRuntime>,
        authorization: IdleAuthorization,
    ) -> Result<IdleDriveOutcome, HarnessError> {
        if lane.lane_id != self.root_lane_id {
            return Err(HarnessError::invalid_state(
                "accepted-input queue dispatch is available only on the root lane",
            ));
        }
        let reduction = reduce_lane(self.snapshot()?, lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::RecoveryRequired {
                plan: reduction.recovery_plan.ok_or_else(|| {
                    HarnessError::invalid_state(
                        "root lane has an open operation without a recovery plan",
                    )
                })?,
            });
        }
        // Explicit controls are durable work accepted against a prior live
        // operation. Apply every settled control before deciding whether a
        // queued user batch or an automatic extension continuation may run.
        self.apply_pending_extension_controls(lane)?;
        let reduction = reduce_lane(self.snapshot()?, lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "extension control application unexpectedly opened a root operation",
            ));
        }
        if !reduction.input_reduction.pending_inputs.is_empty() {
            let (operation_id, input_ids) = self.accept_pending_inputs(lane)?;
            // A user batch supersedes any process-local continuation produced
            // by a prior goal control or idle hook. Failed/cancelled user
            // operations therefore cannot accidentally self-resume old work.
            self.replace_pending_extension_continuation(lane, None);
            let operation = self.drive_fresh_epoch(lane, operation_id).await?;
            return Ok(IdleDriveOutcome::Inputs {
                operation,
                input_ids,
            });
        }
        if authorization.allows_extension_continuation() {
            if self.pending_extension_continuation(lane)?.is_none()
                && let Some(continuation) = self.evaluate_idle_extensions_claimed(lane)?
            {
                self.replace_pending_extension_continuation(lane, Some(continuation));
            }
            if let Some(continuation) = self.pending_extension_continuation(lane)? {
                let operation_id = self.accept_extension_continuation(
                    lane,
                    continuation.extension_id,
                    continuation.input,
                )?;
                self.replace_pending_extension_continuation(lane, None);
                let operation = self.drive_fresh_epoch(lane, operation_id).await?;
                return Ok(IdleDriveOutcome::ExtensionContinuation { operation });
            }
        }
        Ok(IdleDriveOutcome::Idle)
    }

    /// Publish local input completions after their matching durable terminal
    /// batch has committed successfully.
    ///
    /// `finish_operation` owns the preceding commit. Keeping publication as
    /// a separate post-commit step means a slow or dropped waiter cannot make
    /// a completed operation appear to have failed.
    pub(crate) fn publish_input_completions_after_commit(
        &self,
        operation_id: &OperationId,
        input_ids: &[EntryId],
        outcome: &OperationOutcome,
    ) -> Result<(), HarnessError> {
        for input_id in input_ids {
            self.input_completions.settle(InputCompletion::new(
                input_id.clone(),
                operation_id.clone(),
                outcome.clone(),
            ));
        }
        Ok(())
    }

    /// Return the exact input membership recorded for one operation.
    pub(crate) fn input_ids_for_operation(
        &self,
        operation_id: &OperationId,
    ) -> Result<Vec<EntryId>, HarnessError> {
        operation_input_ids(&self.snapshot()?, operation_id)
    }

    pub(super) fn apply_pending_extension_controls(
        &self,
        lane: &LaneRuntime,
    ) -> Result<(), HarnessError> {
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot, lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "extension controls require an idle durable lane",
            ));
        }
        for control in reduction.pending_extension_controls {
            self.apply_pending_extension_control(lane, control)?;
        }
        Ok(())
    }

    fn apply_pending_extension_control(
        &self,
        lane: &LaneRuntime,
        pending: PendingExtensionControl,
    ) -> Result<(), HarnessError> {
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::invalid_state(
                "extension controls require an idle durable lane",
            ));
        }
        if reduction.lane_state.active_harness_revision.as_ref()
            != Some(&pending.control.harness_revision_id)
        {
            return Err(HarnessError::invalid_state(format!(
                "extension control {} was accepted for closed harness revision {}",
                pending.control.control_id, pending.control.harness_revision_id,
            )));
        }
        let current = reduction
            .pending_extension_controls
            .iter()
            .find(|candidate| candidate.control.control_id == pending.control.control_id)
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "extension control {} is no longer pending",
                    pending.control.control_id,
                ))
            })?;
        if current != &pending {
            return Err(HarnessError::invalid_state(format!(
                "extension control {} changed while waiting for its idle boundary",
                pending.control.control_id,
            )));
        }

        let configuration =
            self.configuration_for_revision(lane, &pending.control.harness_revision_id)?;
        if configuration.identity.revision_id() != &pending.control.harness_revision_id {
            return Err(HarnessError::invalid_state(format!(
                "extension control {} did not resolve to its pinned harness revision",
                pending.control.control_id,
            )));
        }
        let selected = configuration
            .host_commands()
            .iter()
            .find(|command| {
                command.extension_id == pending.control.extension_id
                    && command.command.description().name == pending.control.command_name
            })
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state(format!(
                    "extension control {} no longer resolves to command {} in extension {} at revision {}",
                    pending.control.control_id,
                    pending.control.command_name,
                    pending.control.extension_id,
                    pending.control.harness_revision_id,
                ))
            })?;
        let state_version = selected.state_version.clone().ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "extension control {} targets extension {} without an immutable state_version contract",
                pending.control.control_id,
                pending.control.extension_id,
            ))
        })?;
        ensure_retained_state_version(&reduction, &pending.control.extension_id, &state_version)?;
        let observed_state = reduction
            .extension_state
            .get(&pending.control.extension_id)
            .cloned();
        let arguments = match &pending.control.arguments {
            tea_protocol::JsonValue::String(arguments) => arguments.clone(),
            _ => {
                return Err(HarnessError::invalid_state(format!(
                    "extension control {} has non-string command arguments",
                    pending.control.control_id,
                )));
            }
        };
        let state = extension_state_view(&snapshot, &lane.lane_id, &pending.control.extension_id)?;
        // The sandbox callback runs after every durable/state snapshot lock has
        // been released. Its result is only adopted by the exact atomic
        // state-replacement plus control-applied commit below.
        let result = selected
            .command
            .invoke(&ExtensionCommandInput { arguments, state })
            .map_err(super::extension_error)?;
        let state_update = result.state.clone();
        let state_item = state_update
            .clone()
            .map(|update| {
                extension_state_commit_item(
                    lane.lane_id.clone(),
                    &pending.control.extension_id,
                    &state_version,
                    update,
                )
            })
            .transpose()?;
        let continuation = result
            .internal_input
            .clone()
            .map(|input| ExtensionContinuation {
                extension_id: pending.control.extension_id.clone(),
                input,
            });

        {
            let mut session = self.session_lock()?;
            let current_snapshot = session.snapshot()?;
            let current_reduction = reduce_lane(current_snapshot.clone(), lane.lane_id.clone())?;
            if current_reduction.lane_state.active_operation.is_some() {
                return Err(HarnessError::invalid_state(
                    "extension control became active before it could apply; retry the idle drive",
                ));
            }
            if current_reduction
                .lane_state
                .active_harness_revision
                .as_ref()
                != Some(&pending.control.harness_revision_id)
            {
                return Err(HarnessError::invalid_state(format!(
                    "extension control {} cannot write through closed harness revision {}",
                    pending.control.control_id, pending.control.harness_revision_id,
                )));
            }
            let Some(current) = current_reduction
                .pending_extension_controls
                .iter()
                .find(|candidate| candidate.control.control_id == pending.control.control_id)
            else {
                return Err(HarnessError::invalid_state(format!(
                    "extension control {} was applied concurrently; retry the idle drive",
                    pending.control.control_id,
                )));
            };
            if current != &pending {
                return Err(HarnessError::invalid_state(format!(
                    "extension control {} changed before it could apply",
                    pending.control.control_id,
                )));
            }
            ensure_retained_state_version(
                &current_reduction,
                &pending.control.extension_id,
                &state_version,
            )?;
            if current_reduction
                .extension_state
                .get(&pending.control.extension_id)
                != observed_state.as_ref()
            {
                return Err(HarnessError::invalid_state(format!(
                    "extension state for {} changed while control {} was evaluating; retry the idle drive",
                    pending.control.extension_id, pending.control.control_id,
                )));
            }
            let mut items = Vec::with_capacity(3);
            if let Some(state_item) = state_item {
                items.push(state_item);
            }
            items.push(SessionCommitItem::Record(
                LaneRecord::ExtensionControlApplied(ExtensionControlAppliedRecord {
                    control_id: pending.control.control_id.clone(),
                }),
            ));
            if checkpoint_after_final_extension_control(
                &current_snapshot,
                lane,
                &current_reduction,
                &pending,
            ) {
                let mut prospective_state = current_reduction.extension_state.clone();
                if let Some(update) = &state_update {
                    prospective_state.insert(
                        pending.control.extension_id.clone(),
                        ExtensionStateValue {
                            state_version: state_version.clone(),
                            value: update.value.clone(),
                        },
                    );
                }
                items.push(settled_turn_checkpoint_item_with_state(
                    &current_snapshot,
                    &lane.lane_id,
                    &pending.control.operation_id,
                    prospective_state,
                )?);
            }
            session.commit(SessionCommit::new(items)?)?;
        }
        // This authority is process-local by design. A restart can inspect the
        // resulting extension state, but cannot infer permission to start an
        // automatic goal operation from a persisted control alone.
        self.replace_pending_extension_continuation(lane, continuation);
        Ok(())
    }

    fn pending_extension_continuation(
        &self,
        lane: &LaneRuntime,
    ) -> Result<Option<ExtensionContinuation>, HarnessError> {
        lane.pending_extension_continuation
            .lock()
            .map(|continuation| continuation.clone())
            .map_err(|_| HarnessError::invalid_state("extension continuation mutex is poisoned"))
    }

    pub(super) fn replace_pending_extension_continuation(
        &self,
        lane: &LaneRuntime,
        continuation: Option<ExtensionContinuation>,
    ) {
        if let Ok(mut slot) = lane.pending_extension_continuation.lock() {
            *slot = continuation;
        }
    }

    fn accept_pending_inputs(
        &self,
        lane: &LaneRuntime,
    ) -> Result<(OperationId, Vec<EntryId>), HarnessError> {
        let snapshot = self.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), lane.lane_id.clone())?;
        if reduction.lane_state.active_operation.is_some() {
            return Err(HarnessError::RecoveryRequired {
                plan: reduction.recovery_plan.ok_or_else(|| {
                    HarnessError::invalid_state(
                        "root lane has an open operation without a recovery plan",
                    )
                })?,
            });
        }
        let accepted = reduction.input_reduction.pending_inputs.clone();
        if accepted.is_empty() {
            return Err(HarnessError::invalid_state(
                "no accepted root input is available for dispatch",
            ));
        }
        let input_ids = accepted
            .iter()
            .map(|input| input.entry.id.clone())
            .collect::<Vec<_>>();
        let sequence = snapshot.last_sequence().0.to_string();
        let mut identity_values = vec![snapshot.header().session_id.as_str(), sequence.as_str()];
        identity_values.extend(input_ids.iter().map(EntryId::as_str));
        let operation_id = OperationId::new(durable_identifier("operation", identity_values))
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let original_input = accepted
            .iter()
            .map(|input| input.entry.clone())
            .collect::<Vec<_>>();
        let expected_configuration = reduction.effective_configuration.clone();
        let configuration = self.configuration_for_reduction(lane, &reduction)?;
        let mut operation = OperationStartedRecord::new(
            operation_id.clone(),
            lane.lane_id.clone(),
            reduction.lane_state.leaf_id.clone(),
            OperationKind::Run,
            original_input.clone(),
            configuration.identity.revision_id.clone(),
            configuration.identity.profile_id.clone(),
        )
        .with_input_ids(input_ids.clone());
        // Lifecycle code is an extension callback and must never execute
        // while the session writer is locked. The commit below verifies that
        // the leaf, queue membership, and immutable configuration it observed
        // are still current before admitting the operation.
        operation.operation_resume_data = configuration.lifecycle.before_operation()?;

        // An accepted input is intentionally allowed while recovery is
        // blocked, but dispatch is not: a new user prompt must not bypass an
        // unresolved non-replayable effect from this lane's history.
        self.ensure_recovery_permitted(&lane.lane_id)?;
        let sequence = {
            let mut session = self.session_lock()?;
            let snapshot = session.snapshot()?;
            let reduction = reduce_lane(snapshot.clone(), lane.lane_id.clone())?;
            if reduction.lane_state.active_operation.is_some() {
                return Err(HarnessError::RecoveryRequired {
                    plan: reduction.recovery_plan.ok_or_else(|| {
                        HarnessError::invalid_state(
                            "root lane has an open operation without a recovery plan",
                        )
                    })?,
                });
            }
            let current_inputs = reduction.input_reduction.pending_inputs.clone();
            let current_input_ids = current_inputs
                .iter()
                .map(|input| input.entry.id.clone())
                .collect::<Vec<_>>();
            let current_original_input = current_inputs
                .iter()
                .map(|input| input.entry.clone())
                .collect::<Vec<_>>();
            if reduction.lane_state.leaf_id != operation.source_leaf_id
                || current_input_ids != input_ids
                || current_original_input != original_input
            {
                return Err(HarnessError::invalid_state(
                    "accepted input queue changed while dispatch policy was evaluated; retry the drive",
                ));
            }
            if reduction.effective_configuration != expected_configuration {
                return Err(HarnessError::invalid_state(
                    "root effective configuration changed while input dispatch policy was evaluated; retry the drive",
                ));
            }
            if reduction.lane_state.active_harness_revision.as_ref()
                != Some(&operation.initial_harness_revision)
            {
                return Err(HarnessError::invalid_state(
                    "root harness revision changed while input dispatch policy was evaluated; retry the drive",
                ));
            }
            let mut items = Vec::with_capacity(original_input.len().saturating_add(1));
            items.push(SessionCommitItem::Record(LaneRecord::OperationStarted(
                operation,
            )));
            items.extend(
                original_input
                    .into_iter()
                    .map(|entry| SessionCommitItem::Entry {
                        lane_id: lane.lane_id.clone(),
                        entry,
                    }),
            );
            let stored = session.commit(SessionCommit::new(items)?)?;
            stored.seq
        };
        self.publish_event(tea_core::runtime::TeaEvent::Session(
            tea_core::runtime::SessionEvent::OperationAccepted {
                sequence,
                lane_id: lane.lane_id.clone(),
                operation_id: operation_id.clone(),
            },
        ))?;
        Ok((operation_id, input_ids))
    }
}

fn queued_inputs_from_reduction(
    accepted: &[tea_session::AcceptedInput],
) -> Result<Vec<QueuedInput>, HarnessError> {
    accepted.iter().map(queued_input_from_accepted).collect()
}

fn queued_input_from_accepted(
    accepted: &tea_session::AcceptedInput,
) -> Result<QueuedInput, HarnessError> {
    let SessionEntry::UserMessage(user) = &accepted.entry.body else {
        return Err(HarnessError::invalid_state(format!(
            "accepted input {} is not a user message",
            accepted.entry.id
        )));
    };
    Ok(QueuedInput::new(
        accepted.entry.id.clone(),
        user.content.clone(),
    ))
}

fn input_disposition(status: &InputStatus) -> InputDisposition {
    match status {
        InputStatus::Pending => InputDisposition::Pending,
        InputStatus::Withdrawn => InputDisposition::Withdrawn,
        InputStatus::Dispatched { operation_id } => InputDisposition::Dispatched {
            operation_id: operation_id.clone(),
        },
        InputStatus::Settled {
            operation_id,
            outcome,
        } => InputDisposition::Settled {
            operation_id: operation_id.clone(),
            outcome: outcome.clone(),
        },
    }
}

fn completion_from_disposition(
    input_id: EntryId,
    disposition: &InputDisposition,
) -> Option<InputCompletion> {
    match disposition {
        InputDisposition::Pending | InputDisposition::Dispatched { .. } => None,
        InputDisposition::Withdrawn => Some(InputCompletion::withdrawn(input_id)),
        InputDisposition::Settled {
            operation_id,
            outcome,
        } => Some(InputCompletion::new(
            input_id,
            operation_id.clone(),
            outcome.clone(),
        )),
    }
}

pub(super) fn ensure_retained_state_version(
    reduction: &tea_session::LaneReduction,
    extension_id: &str,
    expected_version: &str,
) -> Result<(), HarnessError> {
    let Some(state) = reduction.extension_state.get(extension_id) else {
        return Ok(());
    };
    if state.state_version == expected_version {
        return Ok(());
    }
    Err(HarnessError::invalid_state(format!(
        "extension {extension_id} retains state version {} but its pinned command contract requires {expected_version}",
        state.state_version,
    )))
}

/// Return whether applying `pending` closes the last control gap before a
/// completed root turn can receive its fork checkpoint.
///
/// `finish_operation` intentionally leaves that checkpoint absent while a
/// control remains pending. The final control must therefore group its state
/// replacement, durable application record, and exact prospective namespace
/// map with the checkpoint. Children retain their own terminal evidence but
/// never create root-user fork anchors.
fn checkpoint_after_final_extension_control(
    snapshot: &tea_session::SessionSnapshot,
    lane: &LaneRuntime,
    reduction: &tea_session::LaneReduction,
    pending: &PendingExtensionControl,
) -> bool {
    if snapshot.facts().iter().any(|stored| {
        matches!(
            &stored.fact,
            SessionFact::AgentSpawned(agent) if agent.lane_id == lane.lane_id
        )
    }) {
        return false;
    }
    if snapshot.facts().iter().any(|stored| {
        matches!(
            &stored.fact,
            SessionFact::TurnCheckpoint(checkpoint)
                if checkpoint.operation_id == pending.control.operation_id
        )
    }) {
        return false;
    }
    if reduction
        .pending_extension_controls
        .iter()
        .any(|candidate| {
            candidate.control.operation_id == pending.control.operation_id
                && candidate.control.control_id != pending.control.control_id
        })
    {
        return false;
    }
    matches!(
        super::terminal_operation(snapshot, &lane.lane_id),
        Some((operation_id, OperationOutcome::Completed, _, _))
            if operation_id == pending.control.operation_id
    )
}

pub(crate) fn operation_input_ids(
    snapshot: &tea_session::SessionSnapshot,
    operation_id: &OperationId,
) -> Result<Vec<EntryId>, HarnessError> {
    snapshot
        .records()
        .iter()
        .find_map(|stored| match &stored.record {
            LaneRecord::OperationStarted(record) if &record.id == operation_id => {
                Some(record.input_ids.clone())
            }
            _ => None,
        })
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "operation {operation_id} has no durable operation-start record",
            ))
        })
}

pub(crate) fn input_settlement_records(
    operation_id: &OperationId,
    input_ids: &[EntryId],
    outcome: &OperationOutcome,
) -> Vec<LaneRecord> {
    input_ids
        .iter()
        .cloned()
        .map(|input_id| {
            LaneRecord::InputSettled(InputSettledRecord {
                operation_id: operation_id.clone(),
                input_id,
                outcome: outcome.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    struct CountWake(AtomicUsize);

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct PanicWake;

    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("fixture executor wake panics")
        }
    }

    fn poll_once<T>(future: Pin<&mut impl Future<Output = T>>, waker: &Waker) -> Poll<T> {
        let mut context = Context::from_waker(waker);
        future.poll(&mut context)
    }

    #[test]
    fn input_completion_wakes_its_own_waiter_without_an_event_subscription() {
        let registry = InputCompletionRegistry::default();
        let input_id = EntryId::new("input-completion").expect("input ID is valid");
        let handle = registry.register(input_id.clone());
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wake));
        let mut wait = Box::pin(handle.wait());

        assert!(matches!(poll_once(wait.as_mut(), &waker), Poll::Pending));
        registry.settle(InputCompletion::new(
            input_id,
            OperationId::new("operation-completion").expect("operation ID is valid"),
            OperationOutcome::Completed,
        ));

        assert_eq!(wake.0.load(Ordering::Acquire), 1);
        let Poll::Ready(result) = poll_once(wait.as_mut(), &waker) else {
            panic!("the input-specific completion future should resolve");
        };
        assert_eq!(
            result.outcome(),
            &InputOutcome::Operation {
                operation_id: OperationId::new("operation-completion")
                    .expect("operation ID is valid"),
                outcome: OperationOutcome::Completed,
            }
        );
        assert!(handle.try_result().is_some());
    }

    #[test]
    fn input_completion_keeps_the_first_terminal_result_when_delivery_repeats() {
        let registry = InputCompletionRegistry::default();
        let input_id = EntryId::new("input-conflict").expect("input ID is valid");
        let handle = registry.register(input_id.clone());
        let first = InputCompletion::new(
            input_id.clone(),
            OperationId::new("operation-first").expect("operation ID is valid"),
            OperationOutcome::Completed,
        );
        registry.settle(first.clone());
        registry.settle(first);
        registry.settle(InputCompletion::new(
            input_id,
            OperationId::new("operation-second").expect("operation ID is valid"),
            OperationOutcome::Aborted,
        ));

        assert_eq!(
            handle
                .try_result()
                .expect("first completion remains available")
                .outcome(),
            &InputOutcome::Operation {
                operation_id: OperationId::new("operation-first").expect("operation ID is valid"),
                outcome: OperationOutcome::Completed,
            }
        );
    }

    #[test]
    fn input_completion_keeps_committed_result_when_a_waker_panics() {
        let registry = InputCompletionRegistry::default();
        let input_id = EntryId::new("input-panic-waker").expect("input ID is valid");
        let handle = registry.register(input_id.clone());
        let waker = Waker::from(Arc::new(PanicWake));
        let mut wait = Box::pin(handle.wait());
        assert!(matches!(poll_once(wait.as_mut(), &waker), Poll::Pending));

        let completion = InputCompletion::new(
            input_id,
            OperationId::new("operation-panic-waker").expect("operation ID is valid"),
            OperationOutcome::Completed,
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.settle(completion.clone());
            }))
            .is_ok()
        );
        assert_eq!(handle.try_result(), Some(completion));
    }
}
