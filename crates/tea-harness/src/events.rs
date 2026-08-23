//! Application-level committed-state events and reconnect snapshots.
//!
//! Core events remain run-scoped. This module adds a deliberately separate
//! harness/session/artifact envelope for application consumers. It is not a
//! telemetry sink: callers decide whether a local UI wants full core events,
//! while durable-state events contain only IDs, sizes, and bounded
//! diagnostics.

use crate::{HarnessError, HarnessSurface};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use tea_core::AgentEvent;
use tea_session::{
    ArtifactId, HarnessCandidateId, HarnessRevisionId, HarnessSnapshotId, LaneId, LaneMutation,
    ModelHarnessProfileId, OperationId, Sequence, SessionId, SessionSnapshot, reduce_lane,
};

const SUBSCRIBER_BUFFER: usize = 256;

/// One application-level event envelope.
#[derive(Clone, Debug, PartialEq)]
pub enum TeaEvent {
    /// A core-owned run event for a local application consumer.
    Agent(AgentEvent),
    /// A committed durable session fact.
    Session(SessionEvent),
    /// A committed or validated immutable harness transition.
    Harness(HarnessEvent),
    /// Content-free immutable artifact lifecycle data.
    Artifact(ArtifactEvent),
}

impl TeaEvent {
    fn session_sequence(&self) -> Option<Sequence> {
        match self {
            Self::Session(event) => Some(event.sequence()),
            Self::Agent(_) | Self::Harness(_) | Self::Artifact(_) => None,
        }
    }
}

/// Durable session state change observed only after its commit succeeds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    /// A caller-visible operation and its original user entry are durable.
    OperationAccepted {
        /// Global sequence of the user-entry commit that completed acceptance.
        sequence: Sequence,
        /// Owning lane.
        lane_id: LaneId,
        /// Durable operation identity.
        operation_id: OperationId,
    },
    /// One immutable core epoch started under a pinned revision/snapshot.
    EpochStarted {
        /// Global sequence of the epoch-start record.
        sequence: Sequence,
        /// Owning lane.
        lane_id: LaneId,
        /// Durable operation identity.
        operation_id: OperationId,
        /// Immutable revision identity.
        revision_id: HarnessRevisionId,
        /// Immutable snapshot identity.
        snapshot_id: HarnessSnapshotId,
        /// Model-harness profile identity.
        profile_id: ModelHarnessProfileId,
    },
    /// One operation reached its unique durable terminal outcome.
    OperationFinished {
        /// Global sequence of the terminal record.
        sequence: Sequence,
        /// Owning lane.
        lane_id: LaneId,
        /// Durable operation identity.
        operation_id: OperationId,
        /// Bounded terminal state spelling.
        outcome: String,
    },
}

impl SessionEvent {
    fn sequence(&self) -> Sequence {
        match self {
            Self::OperationAccepted { sequence, .. }
            | Self::EpochStarted { sequence, .. }
            | Self::OperationFinished { sequence, .. } => *sequence,
        }
    }
}

/// Candidate-validation stage that produced a harness rejection event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationStage {
    /// Immutable source/tree/snapshot validation.
    Static,
    /// Frozen session capability ceiling validation.
    Capability,
    /// Candidate activation/lineage validation.
    Activation,
    /// Host/operator evaluation or promotion validation.
    Evaluation,
}

/// Stable diagnostic category for UI grouping without exposing source/output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticCode(String);

impl DiagnosticCode {
    /// Construct a portable bounded diagnostic category.
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 120
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(HarnessError::invalid_state(
                "diagnostic code must use [A-Za-z0-9._-] and be at most 120 bytes",
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the stable category spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Application-level harness transition event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessEvent {
    /// An immutable candidate entered retained lineage.
    CandidateStaged {
        /// Owning lane.
        lane_id: LaneId,
        /// Candidate identity.
        candidate_id: HarnessCandidateId,
        /// Candidate parent revision.
        parent_revision_id: HarnessRevisionId,
        /// Candidate snapshot.
        snapshot_id: HarnessSnapshotId,
        /// Exact changed immutable source paths.
        changed_paths: Vec<tea_session::NormalizedPath>,
    },
    /// A candidate or activation was rejected without mutating the active revision.
    CandidateRejected {
        /// Owning lane.
        lane_id: LaneId,
        /// Candidate when staging reached an addressable object.
        candidate_id: Option<HarnessCandidateId>,
        /// Active revision left unchanged.
        active_revision_id: HarnessRevisionId,
        /// Validation boundary.
        stage: ValidationStage,
        /// Stable category.
        code: DiagnosticCode,
        /// Bounded diagnostic with no source or provider payload.
        diagnostic: String,
    },
    /// Activation became a durable operation obligation.
    ActivationScheduled {
        /// Owning lane.
        lane_id: LaneId,
        /// Operation that will roll over.
        operation_id: OperationId,
        /// Candidate identity.
        candidate_id: HarnessCandidateId,
        /// Child revision selected by the candidate.
        target_revision_id: HarnessRevisionId,
    },
    /// A semantic branch transition activated an immutable snapshot.
    SnapshotActivated {
        /// Owning lane.
        lane_id: LaneId,
        /// Operation that crossed the safe boundary.
        operation_id: OperationId,
        /// Previous immutable revision.
        previous_revision_id: HarnessRevisionId,
        /// New immutable revision.
        revision_id: HarnessRevisionId,
        /// New immutable snapshot.
        snapshot_id: HarnessSnapshotId,
        /// Whether prompt/tool provider surface changed.
        provider_surface_changed: bool,
        /// Exact affected durable surfaces.
        changed_surfaces: BTreeSet<HarnessSurface>,
    },
    /// A durable core rollover started after prior epoch settlement.
    RolloverStarted {
        /// Owning lane.
        lane_id: LaneId,
        /// Operation crossing the boundary.
        operation_id: OperationId,
        /// Previous epoch identity.
        from_epoch: tea_session::EpochId,
        /// Target immutable revision.
        to_revision_id: HarnessRevisionId,
    },
    /// A replacement core epoch started after activation.
    RolloverCompleted {
        /// Owning lane.
        lane_id: LaneId,
        /// Operation crossing the boundary.
        operation_id: OperationId,
        /// New epoch identity.
        epoch_id: tea_session::EpochId,
        /// Revision used by the new epoch.
        revision_id: HarnessRevisionId,
    },
    /// A revision transition selected an earlier immutable snapshot.
    RolledBack {
        /// Owning lane.
        lane_id: LaneId,
        /// Revision left behind.
        from_revision_id: HarnessRevisionId,
        /// Existing immutable revision selected again.
        to_revision_id: HarnessRevisionId,
    },
}

/// Content-free immutable artifact lifecycle event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactEvent {
    /// Exact immutable artifact bytes became available for recovery.
    Retained {
        /// Content-addressed identity.
        artifact_id: ArtifactId,
        /// Exact byte length.
        byte_len: u64,
        /// Stable retention/projection policy identity.
        policy_id: tea_session::ArtifactPolicyId,
    },
    /// A reviewed GC pass removed one unreachable object.
    Collected {
        /// Content-addressed identity.
        artifact_id: ArtifactId,
        /// Exact byte length before deletion.
        byte_len: u64,
    },
}

/// Content-free lane state supplied with a reconnect snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaneSnapshotView {
    /// Lane identity.
    pub lane_id: LaneId,
    /// Current semantic branch leaf.
    pub leaf_id: Option<tea_session::EntryId>,
    /// Open operation when the lane is busy.
    pub active_operation: Option<OperationId>,
    /// Current branch-pinned harness revision.
    pub active_harness_revision: Option<HarnessRevisionId>,
    /// Bounded lane status spelling.
    pub status: String,
}

/// One atomic application reconnect view, followed by live events only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessSnapshotView {
    /// Durable session identity.
    pub session_id: SessionId,
    /// Last committed global session sequence represented by this view.
    pub sequence: Sequence,
    /// Every lane known by the durable snapshot.
    pub lanes: Vec<LaneSnapshotView>,
}

impl HarnessSnapshotView {
    /// Build a view from exactly one already-atomic session snapshot.
    pub fn from_session(snapshot: &SessionSnapshot) -> Result<Self, HarnessError> {
        let mut lane_ids = BTreeSet::new();
        lane_ids.insert(snapshot.header().initial_lane.clone());
        for mutation in snapshot.lane_mutations() {
            let LaneMutation::Created { lane_id, .. } = &mutation.mutation;
            lane_ids.insert(lane_id.clone());
        }
        let mut lanes = Vec::with_capacity(lane_ids.len());
        for lane_id in lane_ids {
            let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
            lanes.push(LaneSnapshotView {
                lane_id,
                leaf_id: reduction.lane_state.leaf_id,
                active_operation: reduction.lane_state.active_operation,
                active_harness_revision: reduction.lane_state.active_harness_revision,
                status: format!("{:?}", reduction.lane_state.status).to_ascii_lowercase(),
            });
        }
        Ok(Self {
            session_id: snapshot.header().session_id.clone(),
            sequence: snapshot.last_sequence(),
            lanes,
        })
    }
}

/// One live event subscription paired with its initial reconnect view.
pub struct TeaEventSubscription {
    /// The single atomic view to apply before consuming live events.
    pub snapshot: HarnessSnapshotView,
    receiver: Receiver<TeaEvent>,
}

impl TeaEventSubscription {
    /// Receive the next live event. Durable session events already represented
    /// in `snapshot` are suppressed; events are never replayed on reconnect.
    pub fn recv(&self) -> Result<TeaEvent, mpsc::RecvError> {
        loop {
            let event = self.receiver.recv()?;
            if event
                .session_sequence()
                .is_none_or(|sequence| sequence > self.snapshot.sequence)
            {
                return Ok(event);
            }
        }
    }

    /// Nonblocking form of [`Self::recv`].
    pub fn try_recv(&self) -> Result<TeaEvent, TryRecvError> {
        loop {
            let event = self.receiver.try_recv()?;
            if event
                .session_sequence()
                .is_none_or(|sequence| sequence > self.snapshot.sequence)
            {
                return Ok(event);
            }
        }
    }
}

/// Process-local event fanout behind the durable supervisor.
#[derive(Default)]
pub(crate) struct EventHub {
    subscribers: Mutex<Vec<SyncSender<TeaEvent>>>,
}

impl EventHub {
    pub(crate) fn subscribe(
        &self,
        snapshot: HarnessSnapshotView,
    ) -> Result<TeaEventSubscription, HarnessError> {
        let (sender, receiver) = mpsc::sync_channel(SUBSCRIBER_BUFFER);
        self.subscribers
            .lock()
            .map_err(|_| HarnessError::invalid_state("harness event subscriber mutex is poisoned"))?
            .push(sender);
        Ok(TeaEventSubscription { snapshot, receiver })
    }

    pub(crate) fn publish(&self, event: TeaEvent) {
        let Ok(mut subscribers) = self.subscribers.lock() else {
            // Event consumers are passive. A poisoned local fanout must not
            // retroactively turn a successful durable commit into a failed
            // operation or block subsequent recovery.
            return;
        };
        subscribers.retain(|sender| match sender.try_send(event.clone()) {
            Ok(()) => true,
            // A slow connection must reconnect for a fresh atomic view;
            // silently accumulating unbounded event content is neither a
            // durable queue nor a safe telemetry substitute.
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        });
    }
}
