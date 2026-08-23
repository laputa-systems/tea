//! Injected boundaries around externally observable execution effects.
//!
//! `tea-core` deliberately does not own a session log, an artifact store, or
//! an executor.  A host that needs durable execution installs an
//! [`EffectGate`] and records an intent before allowing an external effect to
//! begin, then records settlement before the run advances past it.  The core
//! keeps the gate at the mechanism boundary; it does not interpret host
//! provenance or persist a particular format.

use crate::scheduler::ModelRequest;
use crate::state::{AgentToolCall, RunId, StopReason, Usage};
use crate::tool::{AgentToolResult, ToolCall};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// The externally observable category of an injected effect boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectKind {
    /// A host is committing an authoritative durable mutation.
    DurableWrite,
    /// A physical provider request may cross a process boundary.
    ProviderRequest,
    /// A registered tool capability may begin an external effect.
    ToolExecution,
    /// A host policy hook may invoke an external capability.
    HookInvocation,
    /// A host-owned timer may wait or schedule work.
    Timer,
    /// A host is writing immutable artifact bytes.
    ArtifactWrite,
    /// A host is activating an immutable harness revision.
    HarnessActivation,
}

/// Whether a manual action parks immediately before or after its effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectPhase {
    /// The effect has not begun.  Releasing this action permits it to start.
    Before,
    /// The effect has settled and downstream work is still blocked.
    After,
}

/// A process-local occurrence identity assigned by one core run.
///
/// This is useful for correlating the paired before/after gate calls.  Durable
/// recovery must instead use host-supplied operation and invocation IDs; a
/// core run ID is intentionally not a cross-process replay identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EffectId(pub u64);

/// Opaque host provenance attached to every action in one core run.
///
/// The core uses strings rather than session crate IDs so it remains free of
/// persistence ownership.  A durable host supplies canonical identifiers and
/// treats the fields as attribution, not filesystem authority.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunProvenance {
    /// Durable session identity, when the embedding has one.
    pub session_id: Option<String>,
    /// Durable lane identity, when the embedding has one.
    pub lane_id: Option<String>,
    /// Durable operation identity, when the run belongs to an operation.
    pub operation_id: Option<String>,
    /// Durable epoch identity, when the run belongs to an epoch.
    pub epoch_id: Option<String>,
    /// Durable core-run identity allocated with the epoch before this run
    /// starts. Unlike the process-local `RunId`, this value remains stable
    /// across telemetry sinks and recovery inspection.
    pub core_run_id: Option<String>,
    /// Immutable harness snapshot identity used by this run.
    pub harness_snapshot_id: Option<String>,
    /// Immutable harness revision identity used by this run.
    pub harness_revision_id: Option<String>,
    /// Immutable model-harness profile identity used by this run.
    pub model_harness_profile_id: Option<String>,
    /// Exact provider-visible prompt/tool surface identity, when the host
    /// owns one. This is deliberately distinct from the complete harness
    /// snapshot because hook-only changes must not be reported as cache
    /// invalidations.
    pub provider_surface_digest: Option<String>,
    /// Frozen experiment identity when this run is part of a trusted external
    /// evaluation campaign. Session-local agents never manufacture this.
    pub experiment_id: Option<String>,
}

impl RunProvenance {
    /// Return whether no host attribution was supplied.
    pub fn is_empty(&self) -> bool {
        self.session_id.is_none()
            && self.lane_id.is_none()
            && self.operation_id.is_none()
            && self.epoch_id.is_none()
            && self.core_run_id.is_none()
            && self.harness_snapshot_id.is_none()
            && self.harness_revision_id.is_none()
            && self.model_harness_profile_id.is_none()
            && self.provider_surface_digest.is_none()
            && self.experiment_id.is_none()
    }
}

/// The specific work described by an [`EffectAction`].
///
/// The variants intentionally carry the values a durable host needs to write
/// a correctly ordered intent or settlement.  Gates are not telemetry sinks:
/// hosts must not expose these values to broad logs or metrics.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectSubject {
    /// A host-owned durable mutation carrying its exact semantic payload.
    DurableWrite {
        /// Immutable value that must commit before core observes it.
        write: DurableWriteRequest,
    },
    /// One exact core-composed provider request.
    ProviderRequest {
        /// The request immediately before adapter dispatch.
        request: ModelRequest,
    },
    /// One schema-valid, policy-allowed tool call immediately before execution.
    ToolExecution {
        /// The effective tool call passed to the capability.
        call: ToolCall,
    },
    /// One explicit host-policy hook call.
    HookInvocation {
        /// Stable hook category.
        hook: HookInvocation,
    },
    /// A named host timer.
    Timer {
        /// Stable timer category selected by the host.
        timer: String,
    },
    /// One immutable host artifact write.
    ArtifactWrite {
        /// Stable artifact class selected by the host.
        artifact_kind: String,
    },
    /// One immutable harness revision activation.
    HarnessActivation {
        /// Target immutable revision identity selected by the host.
        target_revision: String,
    },
}

/// A semantic mutation that core asks its host to commit durably.
///
/// The core deliberately owns only the in-memory message transition. A
/// durable host interprets this request in its own session format and must
/// commit it before the core message becomes observable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableWriteRequest {
    /// One post-policy tool result about to join the canonical transcript.
    ToolResult {
        /// Validated call that identifies the source-order assistant position.
        call: ToolCall,
        /// Complete post-policy result to retain and project.
        result: AgentToolResult,
    },
}

impl EffectSubject {
    /// Return the only valid category for this subject.
    pub const fn kind(&self) -> EffectKind {
        match self {
            Self::DurableWrite { .. } => EffectKind::DurableWrite,
            Self::ProviderRequest { .. } => EffectKind::ProviderRequest,
            Self::ToolExecution { .. } => EffectKind::ToolExecution,
            Self::HookInvocation { .. } => EffectKind::HookInvocation,
            Self::Timer { .. } => EffectKind::Timer,
            Self::ArtifactWrite { .. } => EffectKind::ArtifactWrite,
            Self::HarnessActivation { .. } => EffectKind::HarnessActivation,
        }
    }
}

/// Stable core hook categories that may cross a gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookInvocation {
    /// Policy before one tool execution.
    BeforeTool {
        /// Provider-originated call identity.
        tool_call_id: String,
        /// Registered tool name.
        tool_name: String,
    },
    /// Policy after one tool execution.
    AfterTool {
        /// Provider-originated call identity.
        tool_call_id: String,
        /// Registered tool name.
        tool_name: String,
    },
    /// Context transformation before provider conversion.
    TransformContext,
    /// Context conversion into a provider-facing envelope.
    ConvertToLlm,
    /// Request-scoped next-turn policy.
    PrepareNextTurn,
    /// Final post-turn stop policy.
    ShouldStopAfterTurn,
}

/// One core-generated action presented to an [`EffectGate`].
#[derive(Clone, Debug, PartialEq)]
pub struct EffectAction {
    id: EffectId,
    run_id: RunId,
    provenance: RunProvenance,
    subject: EffectSubject,
}

impl EffectAction {
    /// Construct one action from core-owned and host-supplied identities.
    pub fn new(
        id: EffectId,
        run_id: RunId,
        provenance: RunProvenance,
        subject: EffectSubject,
    ) -> Self {
        Self {
            id,
            run_id,
            provenance,
            subject,
        }
    }

    /// Return this run-local correlation identity.
    pub const fn id(&self) -> EffectId {
        self.id
    }

    /// Return the core run that owns this action.
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Borrow host-supplied durable attribution.
    pub fn provenance(&self) -> &RunProvenance {
        &self.provenance
    }

    /// Return the action's category.
    pub const fn kind(&self) -> EffectKind {
        self.subject.kind()
    }

    /// Borrow the exact subject to be gated.
    pub fn subject(&self) -> &EffectSubject {
        &self.subject
    }
}

/// Completed provider response details passed to a host before core settlement.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderResponse {
    /// Terminal provider stop reason.
    pub stop_reason: StopReason,
    /// Complete assistant text accumulated by this response.
    pub assistant_text: String,
    /// Provider tool calls in their original source order.
    pub tool_calls: Vec<AgentToolCall>,
    /// Redacted terminal provider diagnostic, when supplied.
    pub error_message: Option<String>,
    /// Aggregated provider usage, including discarded responses.
    pub usage: Option<Usage>,
    /// Whether the adapter explicitly classified the response as context overflow.
    pub context_overflow: bool,
}

/// Provider effect settlement seen by a gate.
#[derive(Clone, Debug, PartialEq)]
pub enum ProviderEffectOutcome {
    /// The response reached a terminal stream event.
    Settled(ProviderResponse),
    /// Dispatch or stream processing failed before a terminal response.
    Failed {
        /// Bounded provider diagnostic.
        message: String,
    },
}

/// Tool effect settlement seen by a gate after `after_tool` policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolEffectOutcome {
    /// Exact result returned by the executed capability before a policy can
    /// alter its model-facing projection.
    pub raw_result: AgentToolResult,
    /// Final model-facing result that downstream core work observes.
    ///
    /// A durable host retains `raw_result` before exposing this projection, so
    /// a policy cannot erase already-completed external evidence.
    pub result: AgentToolResult,
}

/// Settlement of a host-policy hook invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookEffectOutcome {
    /// The hook completed and returned a policy value.
    Succeeded,
    /// The hook returned a bounded error.
    Failed {
        /// Bounded hook diagnostic.
        message: String,
    },
}

/// Generic outcome for host effects that do not have a core-specific result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectCompletion {
    /// The effect completed normally.
    Succeeded,
    /// The effect was cancelled.
    Cancelled,
    /// The effect failed with a bounded diagnostic.
    Failed {
        /// Bounded failure diagnostic.
        message: String,
    },
}

/// The result made durable by the after-effect gate before the run can advance.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectOutcome {
    /// Result of a host durable mutation.
    DurableWrite(EffectCompletion),
    /// Result of one physical provider request.
    ProviderRequest(ProviderEffectOutcome),
    /// Result of one tool capability execution.
    ToolExecution(ToolEffectOutcome),
    /// Result of one host hook invocation.
    HookInvocation(HookEffectOutcome),
    /// Result of a host timer.
    Timer(EffectCompletion),
    /// Result of an immutable artifact write.
    ArtifactWrite(EffectCompletion),
    /// Result of a harness activation.
    HarnessActivation(EffectCompletion),
}

/// A failure from the host-owned durable/effect boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectGateError {
    message: String,
}

impl EffectGateError {
    /// Construct a bounded host-facing gate failure.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Borrow the host-provided diagnostic.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for EffectGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EffectGateError {}

/// Caller-polled gate future.
pub type EffectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), EffectGateError>> + Send + 'a>>;

/// Host-owned barrier around every core effect.
///
/// `before` must durably accept the intent before resolving successfully.
/// `after` must durably settle the outcome before resolving successfully.  A
/// gate error is terminal for the current run: the core never begins a later
/// provider or tool effect after a failed gate call.
pub trait EffectGate: Send + Sync {
    /// Return the drive mode represented by this gate.
    fn drive_mode(&self) -> DriveMode {
        DriveMode::Automatic
    }

    /// Gate an intent immediately before an external effect begins.
    fn before<'a>(&'a self, action: EffectAction) -> EffectFuture<'a>;

    /// Gate settlement immediately before downstream core work observes it.
    fn after<'a>(&'a self, action: EffectAction, outcome: EffectOutcome) -> EffectFuture<'a>;
}

/// Whether an embedding releases effects automatically or one at a time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DriveMode {
    /// The gate resolves as its host durability work completes.
    #[default]
    Automatic,
    /// The gate parks each boundary until an exact action is released.
    Manual,
}

/// A no-op automatic gate for embeddings that do not need durable supervision.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopEffectGate;

impl EffectGate for NoopEffectGate {
    fn before<'a>(&'a self, _action: EffectAction) -> EffectFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn after<'a>(&'a self, _action: EffectAction, _outcome: EffectOutcome) -> EffectFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Identity of one pending action in a [`ManualEffectGate`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ActionId(pub u64);

/// One stable action returned by [`ManualEffectGate::peek_action`].
#[derive(Clone, Debug, PartialEq)]
pub struct PendingAction {
    /// Exact action identity required to release it.
    pub id: ActionId,
    /// Whether this parks before or after the external effect.
    pub phase: EffectPhase,
    /// Core and host facts for the action.
    pub action: EffectAction,
    /// Settlement data for an after-effect boundary only.
    pub outcome: Option<EffectOutcome>,
}

/// Result of releasing one manual action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionOutcome {
    /// The exact pending action was released once.
    Released,
}

/// A rejected manual gate operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManualGateError {
    /// The gate was closed while work was parked.
    Closed,
    /// No action with the requested identity is currently pending.
    UnknownAction {
        /// Caller-supplied action identity.
        expected: ActionId,
        /// Current pending identity, when one exists.
        actual: Option<ActionId>,
    },
    /// The pending action has already been released and is awaiting poll completion.
    AlreadyReleased {
        /// Repeated action identity.
        id: ActionId,
    },
}

impl fmt::Display for ManualGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("manual effect gate is closed"),
            Self::UnknownAction { expected, actual } => {
                write!(formatter, "manual action {expected:?} is not pending (current: {actual:?})")
            }
            Self::AlreadyReleased { id } => {
                write!(formatter, "manual action {id:?} has already been released")
            }
        }
    }
}

impl std::error::Error for ManualGateError {}

/// A production-compatible gate that parks actual effect calls for crash tests.
///
/// It owns no scheduler.  The normal core future awaits this gate in exactly
/// the same place as an automatic durability implementation; a test drives
/// that future, inspects [`Self::peek_action`], then releases one matching
/// action with [`Self::execute_action`].
#[derive(Clone, Default)]
pub struct ManualEffectGate {
    state: Arc<Mutex<ManualGateState>>,
}

#[derive(Default)]
struct ManualGateState {
    next_action_id: u64,
    pending: Option<ParkedAction>,
    closed: bool,
    waiting_for_slot: Vec<Waker>,
}

struct ParkedAction {
    pending: PendingAction,
    released: bool,
    waker: Option<Waker>,
}

impl fmt::Debug for ManualEffectGate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManualEffectGate")
            .field("pending", &self.peek_action())
            .finish()
    }
}

impl ManualEffectGate {
    /// Return the currently parked action without releasing or changing it.
    pub fn peek_action(&self) -> Option<PendingAction> {
        self.state
            .lock()
            .expect("manual effect gate mutex poisoned")
            .pending
            .as_ref()
            .filter(|pending| !pending.released)
            .map(|pending| pending.pending.clone())
    }

    /// Release exactly the action identified by a prior [`Self::peek_action`].
    ///
    /// This is async to fit a manual test loop alongside the caller-owned run
    /// future.  It performs no effect itself; it only wakes the production
    /// procedure that is already parked at the gate.
    pub async fn execute_action(
        &self,
        expected: ActionId,
    ) -> Result<ActionOutcome, ManualGateError> {
        self.release(expected)
    }

    /// Synchronous form of [`Self::execute_action`] for non-async harnesses.
    pub fn release(&self, expected: ActionId) -> Result<ActionOutcome, ManualGateError> {
        let waker = {
            let mut state = self.state.lock().expect("manual effect gate mutex poisoned");
            if state.closed {
                return Err(ManualGateError::Closed);
            }
            let Some(pending) = state.pending.as_mut() else {
                return Err(ManualGateError::UnknownAction {
                    expected,
                    actual: None,
                });
            };
            if pending.pending.id != expected {
                return Err(ManualGateError::UnknownAction {
                    expected,
                    actual: Some(pending.pending.id),
                });
            }
            if pending.released {
                return Err(ManualGateError::AlreadyReleased { id: expected });
            }
            pending.released = true;
            pending.waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(ActionOutcome::Released)
    }

    /// Fail the currently parked operation and reject future releases.
    pub fn close(&self) {
        let (waker, waiters) = {
            let mut state = self.state.lock().expect("manual effect gate mutex poisoned");
            state.closed = true;
            let waker = state.pending.as_ref().and_then(|pending| pending.waker.clone());
            let waiters = std::mem::take(&mut state.waiting_for_slot);
            (waker, waiters)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        for waiter in waiters {
            waiter.wake();
        }
    }

    fn wait(&self, phase: EffectPhase, action: EffectAction, outcome: Option<EffectOutcome>) -> ManualGateWait {
        ManualGateWait {
            state: Arc::clone(&self.state),
            phase,
            action,
            outcome,
            action_id: None,
        }
    }
}

impl EffectGate for ManualEffectGate {
    fn drive_mode(&self) -> DriveMode {
        DriveMode::Manual
    }

    fn before<'a>(&'a self, action: EffectAction) -> EffectFuture<'a> {
        Box::pin(self.wait(EffectPhase::Before, action, None))
    }

    fn after<'a>(&'a self, action: EffectAction, outcome: EffectOutcome) -> EffectFuture<'a> {
        Box::pin(self.wait(EffectPhase::After, action, Some(outcome)))
    }
}

struct ManualGateWait {
    state: Arc<Mutex<ManualGateState>>,
    phase: EffectPhase,
    action: EffectAction,
    outcome: Option<EffectOutcome>,
    action_id: Option<ActionId>,
}

impl Future for ManualGateWait {
    type Output = Result<(), EffectGateError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().expect("manual effect gate mutex poisoned");
        if state.closed {
            return Poll::Ready(Err(EffectGateError::new("manual effect gate is closed")));
        }

        if let Some(action_id) = self.action_id {
            let Some(pending) = state.pending.as_mut() else {
                return Poll::Ready(Err(EffectGateError::new(
                    "manual effect action disappeared before release",
                )));
            };
            if pending.pending.id != action_id {
                state.waiting_for_slot.push(context.waker().clone());
                return Poll::Pending;
            }
            if !pending.released {
                pending.waker = Some(context.waker().clone());
                return Poll::Pending;
            }
            state.pending = None;
            let waiters = std::mem::take(&mut state.waiting_for_slot);
            drop(state);
            for waiter in waiters {
                waiter.wake();
            }
            return Poll::Ready(Ok(()));
        }

        if state.pending.is_some() {
            state.waiting_for_slot.push(context.waker().clone());
            return Poll::Pending;
        }
        state.next_action_id = state.next_action_id.saturating_add(1);
        let action_id = ActionId(state.next_action_id);
        state.pending = Some(ParkedAction {
            pending: PendingAction {
                id: action_id,
                phase: self.phase,
                action: self.action.clone(),
                outcome: self.outcome.clone(),
            },
            released: false,
            waker: Some(context.waker().clone()),
        });
        drop(state);
        self.action_id = Some(action_id);
        Poll::Pending
    }
}
