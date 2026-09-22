//! Application-level committed-state events and reconnect snapshots.
//!
//! Core events remain run-scoped. This module adds a deliberately separate
//! harness/session/artifact envelope for application consumers. It is not a
//! telemetry sink: callers decide whether a local UI wants full core events,
//! while durable-state events contain only IDs, sizes, and bounded
//! diagnostics.

use crate::event::{AgentEvent, AgentEventKind, EventSequence};
use crate::harness::{HarnessError, HarnessSurface};
use crate::state::{AgentMessage, MessageId, RunId, ToolCallId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use tea_session::{
    ArtifactId, EpochId, HarnessCandidateId, HarnessRevisionId, HarnessSnapshotId, LaneId,
    LaneMutation, ModelHarnessProfileId, OperationId, Sequence, SessionId, SessionSnapshot,
    reduce_lane,
};

/// Maximum number of queued semantic and preview updates per subscription.
pub const SUBSCRIBER_EVENT_BUFFER: usize = 256;

/// Maximum number of coalesced live previews represented in one snapshot.
pub const LIVE_PREVIEW_LIMIT: usize = 64;

/// Maximum per-run terminal preview fences retained while that run is active.
///
/// Source-order sequence validation rejects delayed lifecycle events before a
/// capped fence could be evicted. The fences only remove already-queued
/// previews and defend against an invalid newer preview for a settled subject.
pub const TERMINAL_PREVIEW_FENCE_LIMIT: usize = LIVE_PREVIEW_LIMIT;

/// Maximum completed-run fences retained by one live subscription.
///
/// The hub rejects a preview unless its run is currently registered by an
/// `AgentStart`, so this bounded subscriber-local defense cannot turn an old
/// delayed preview into live UI state after its entry is evicted.
pub const COMPLETED_RUN_FENCE_LIMIT: usize = LIVE_PREVIEW_LIMIT;

/// Maximum retained UTF-8 bytes in one assistant-preview update.
pub const MAX_PREVIEW_TEXT_BYTES: usize = 8 * 1024;

/// Maximum retained UTF-8 bytes in one tool-progress activity field.
pub const MAX_PREVIEW_ACTIVITY_BYTES: usize = 1024;

/// Maximum retained UTF-8 bytes in one tool-progress display name.
pub const MAX_PREVIEW_TOOL_NAME_BYTES: usize = 256;

/// One unambiguous process-local core attempt observed by the durable runtime.
///
/// Core [`RunId`] values restart when the supervisor builds a replacement
/// epoch agent. The durable operation and epoch identities prevent one old
/// attempt's preview from colliding with the next attempt's same local run ID.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ObservationRun {
    /// Durable lane that owns this core attempt.
    pub lane_id: LaneId,
    /// Caller-visible durable operation that owns this attempt.
    pub operation_id: OperationId,
    /// Immutable core epoch that owns this attempt.
    pub epoch_id: EpochId,
    /// Core-local execution identity within the epoch agent.
    pub run_id: RunId,
}

impl ObservationRun {
    /// Construct the complete observation identity for one core attempt.
    pub fn new(
        lane_id: LaneId,
        operation_id: OperationId,
        epoch_id: EpochId,
        run_id: RunId,
    ) -> Self {
        Self {
            lane_id,
            operation_id,
            epoch_id,
            run_id,
        }
    }
}

/// Individually coalescible live-preview subject within one core attempt.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PreviewTarget {
    /// One streaming assistant message.
    AssistantMessage {
        /// Core message identity.
        message_id: MessageId,
    },
    /// One executing tool call.
    ToolCall {
        /// Provider-assigned tool-call identity.
        tool_call_id: ToolCallId,
    },
}

/// Stable key used to coalesce and terminally fence one transient preview.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PreviewIdentity {
    /// Core attempt that owns this preview.
    pub run: ObservationRun,
    /// Independently settleable preview subject.
    pub target: PreviewTarget,
}

impl PreviewIdentity {
    /// Identify a streaming assistant message.
    pub fn assistant(run: ObservationRun, message_id: MessageId) -> Self {
        Self {
            run,
            target: PreviewTarget::AssistantMessage { message_id },
        }
    }

    /// Identify one tool-progress stream.
    pub fn tool(run: ObservationRun, tool_call_id: ToolCallId) -> Self {
        Self {
            run,
            target: PreviewTarget::ToolCall { tool_call_id },
        }
    }
}

/// A bounded, transient update that is never durable session truth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewEvent {
    /// Incremental text from one incomplete assistant message.
    AssistantText {
        /// Coalescing and terminal-fence key.
        identity: PreviewIdentity,
        /// Monotonic event sequence within `identity.run`.
        sequence: EventSequence,
        /// Bounded text accumulated since this subscriber's prior preview.
        text: String,
        /// Earlier bytes were discarded to preserve the preview bound.
        truncated: bool,
    },
    /// Latest transient presentation update from an executing tool.
    ToolProgress {
        /// Coalescing and terminal-fence key.
        identity: PreviewIdentity,
        /// Monotonic event sequence within `identity.run`.
        sequence: EventSequence,
        /// Stable tool capability name.
        tool_name: String,
        /// Latest bounded progress content.
        content: String,
        /// Latest bounded activity replacement, when the tool supplied one.
        activity: Option<String>,
        /// Content or activity bytes were discarded to preserve the preview bound.
        truncated: bool,
    },
}

impl PreviewEvent {
    /// Borrow the identity that coalesces this update.
    pub fn identity(&self) -> &PreviewIdentity {
        match self {
            Self::AssistantText { identity, .. } | Self::ToolProgress { identity, .. } => {
                identity
            }
        }
    }

    fn sequence(&self) -> EventSequence {
        match self {
            Self::AssistantText { sequence, .. } | Self::ToolProgress { sequence, .. } => {
                *sequence
            }
        }
    }

    fn coalesce(self, newer: Self) -> Self {
        match (self, newer) {
            (
                Self::AssistantText {
                    identity,
                    text,
                    truncated,
                    ..
                },
                Self::AssistantText {
                    sequence,
                    text: newer_text,
                    truncated: newer_truncated,
                    ..
                },
            ) => {
                let (text, overflowed) = retain_preview_text_suffix(&text, &newer_text);
                Self::AssistantText {
                    identity,
                    sequence,
                    text,
                    truncated: truncated || newer_truncated || overflowed,
                }
            }
            (
                Self::ToolProgress {
                    content,
                    activity,
                    truncated,
                    ..
                },
                Self::ToolProgress {
                    identity,
                    sequence,
                    tool_name,
                    content: newer_content,
                    activity: newer_activity,
                    truncated: newer_truncated,
                },
            ) => Self::ToolProgress {
                identity,
                sequence,
                tool_name,
                content: if newer_content.is_empty() {
                    content
                } else {
                    newer_content
                },
                activity: newer_activity.or(activity),
                truncated: truncated || newer_truncated,
            },
            (_, newer) => newer,
        }
    }
}

/// One application-level event envelope.
#[derive(Clone, Debug, PartialEq)]
pub enum TeaEvent {
    /// A core-owned run event for a local application consumer.
    Agent {
        /// Complete process-local attempt identity.
        run: ObservationRun,
        /// Original core event without any cross-lane projection.
        event: AgentEvent,
    },
    /// Coalescible, non-durable presentation state.
    Preview(PreviewEvent),
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
            Self::Agent { .. }
            | Self::Preview(_)
            | Self::Harness(_)
            | Self::Artifact(_) => None,
        }
    }

    fn preview(&self) -> Option<&PreviewEvent> {
        match self {
            Self::Preview(preview) => Some(preview),
            Self::Agent { .. } | Self::Session(_) | Self::Harness(_) | Self::Artifact(_) => None,
        }
    }

    /// Return every preview subject superseded by this semantic event.
    ///
    /// Consumers retain these identities after applying the semantic event so
    /// a late process-local preview cannot revive settled presentation.
    pub fn terminal_preview_identities(&self) -> Vec<PreviewIdentity> {
        let Self::Agent { run, event } = self else {
            return Vec::new();
        };
        match &event.kind {
            AgentEventKind::MessageEnd {
                message: AgentMessage::Assistant { id, .. },
            } => vec![PreviewIdentity::assistant(run.clone(), *id)],
            AgentEventKind::ToolExecutionEnd { tool_call_id, .. } => {
                vec![PreviewIdentity::tool(run.clone(), tool_call_id.clone())]
            }
            AgentEventKind::AgentEnd { .. } => Vec::new(),
            _ => Vec::new(),
        }
    }

    /// Return the core attempt that has completely ended, when this is its
    /// terminal lifecycle event.
    pub fn completed_observation_run(&self) -> Option<&ObservationRun> {
        match self {
            Self::Agent {
                run,
                event:
                    AgentEvent {
                        kind: AgentEventKind::AgentEnd { .. },
                        ..
                    },
            } => Some(run),
            Self::Agent { .. }
            | Self::Preview(_)
            | Self::Session(_)
            | Self::Harness(_)
            | Self::Artifact(_) => None,
        }
    }

    fn agent_sequence(&self) -> Option<(&ObservationRun, EventSequence)> {
        match self {
            Self::Agent { run, event } => Some((run, event.sequence)),
            Self::Preview(preview) => Some((&preview.identity().run, preview.sequence())),
            Self::Session(_) | Self::Harness(_) | Self::Artifact(_) => None,
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
    /// Bounded process-local previews observed at this same subscription frontier.
    ///
    /// These are explicitly transient. A reopened session reconstructs only
    /// `session`; it never treats this field as durable semantic state.
    pub previews: Vec<PreviewEvent>,
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
            previews: Vec::new(),
        })
    }

    fn with_previews(mut self, previews: impl IntoIterator<Item = PreviewEvent>) -> Self {
        self.previews = previews.into_iter().collect();
        self
    }
}

/// One atomic durable prefix and bounded live-preview view.
///
/// The contained session is suitable for rebuilding a terminal projection
/// after an explicit lag signal. It is captured once per subscription, never
/// per preview token.
#[derive(Clone, Debug, PartialEq)]
pub struct TeaObservationSnapshot {
    /// Authoritative committed session prefix at the subscription frontier.
    pub session: SessionSnapshot,
    /// Content-free lane summary plus explicitly transient previews.
    pub view: HarnessSnapshotView,
}

/// One live event subscription paired with its initial atomic observation view.
pub struct TeaEventSubscription {
    /// The single atomic view to apply before consuming live events.
    pub snapshot: TeaObservationSnapshot,
    subscriber: Arc<EventSubscriber>,
    notifications: Receiver<()>,
}

/// Blocking receive failure for a lossy observation subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeaEventRecvError {
    /// A semantic event could not fit in the bounded queue. Discard this
    /// subscription and obtain a fresh [`TeaObservationSnapshot`].
    Lagged,
    /// The local observation source closed.
    Disconnected,
}

impl fmt::Display for TeaEventRecvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lagged => formatter.write_str("tea event subscription lagged; resnapshot required"),
            Self::Disconnected => formatter.write_str("tea event subscription disconnected"),
        }
    }
}

impl std::error::Error for TeaEventRecvError {}

/// Nonblocking receive failure for a lossy observation subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TeaEventTryRecvError {
    /// No update is currently available.
    Empty,
    /// A semantic event could not fit in the bounded queue. Discard this
    /// subscription and obtain a fresh [`TeaObservationSnapshot`].
    Lagged,
    /// The local observation source closed.
    Disconnected,
}

impl TeaEventSubscription {
    /// Receive the next live event. Durable session events already represented
    /// in `snapshot` are suppressed; events are never replayed on reconnect.
    ///
    /// A [`TeaEventRecvError::Lagged`] result is terminal for this
    /// subscription. Reliable operation completion remains outside this
    /// observation channel.
    pub fn recv(&self) -> Result<TeaEvent, TeaEventRecvError> {
        loop {
            match self.take_next() {
                Ok(Some(event)) => return Ok(event),
                Ok(None) => match self.notifications.recv() {
                    Ok(()) => {}
                    Err(_) => {
                        return match self.take_next() {
                            Ok(Some(event)) => Ok(event),
                            Ok(None) => Err(TeaEventRecvError::Disconnected),
                            Err(error) => Err(error),
                        };
                    }
                },
                Err(error) => return Err(error),
            }
        }
    }

    /// Nonblocking form of [`Self::recv`].
    pub fn try_recv(&self) -> Result<TeaEvent, TeaEventTryRecvError> {
        match self.take_next() {
            Ok(Some(event)) => Ok(event),
            Ok(None) => Err(TeaEventTryRecvError::Empty),
            Err(TeaEventRecvError::Lagged) => Err(TeaEventTryRecvError::Lagged),
            Err(TeaEventRecvError::Disconnected) => Err(TeaEventTryRecvError::Disconnected),
        }
    }

    fn take_next(&self) -> Result<Option<TeaEvent>, TeaEventRecvError> {
        let mut state = self.subscriber.state.lock().map_err(|_| {
            TeaEventRecvError::Disconnected
        })?;
        if state.lagged {
            return Err(TeaEventRecvError::Lagged);
        }
        while let Some(event) = state.events.pop_front() {
            if event.session_sequence().is_none_or(|sequence| {
                sequence > self.snapshot.view.sequence
            }) {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

/// Process-local event fanout behind the durable supervisor.
pub(crate) struct EventHub {
    state: Mutex<EventHubState>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            state: Mutex::new(EventHubState::default()),
        }
    }
}

impl EventHub {
    /// Capture the durable prefix and register the subscriber under one shared
    /// publication lock. The closure must not publish events or hold a writer
    /// mutex that a publisher needs.
    pub(crate) fn subscribe_with_snapshot(
        &self,
        capture: impl FnOnce() -> Result<TeaObservationSnapshot, HarnessError>,
    ) -> Result<TeaEventSubscription, HarnessError> {
        let mut hub = self
            .state
            .lock()
            .map_err(|_| HarnessError::invalid_state("harness event hub mutex is poisoned"))?;
        let mut snapshot = capture()?;
        snapshot.view = snapshot
            .view
            .with_previews(hub.live_previews.iter().cloned());
        let (notifications, receiver) = mpsc::sync_channel(1);
        let subscriber = Arc::new(EventSubscriber::new(notifications));
        hub.subscribers.push(Arc::downgrade(&subscriber));
        Ok(TeaEventSubscription {
            snapshot,
            subscriber,
            notifications: receiver,
        })
    }

    /// Project and publish one core event without cloning an assistant's
    /// growing partial message snapshot for each token.
    pub(crate) fn publish_agent(&self, run: ObservationRun, event: &AgentEvent) {
        let event = match &event.kind {
            AgentEventKind::MessageUpdate {
                message_id,
                text_delta,
            } => {
                let (text, truncated) =
                    bounded_preview_text(text_delta, MAX_PREVIEW_TEXT_BYTES);
                TeaEvent::Preview(PreviewEvent::AssistantText {
                    identity: PreviewIdentity::assistant(run, *message_id),
                    sequence: event.sequence,
                    text,
                    truncated,
                })
            }
            AgentEventKind::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                update,
            } => {
                let (tool_name, tool_name_truncated) =
                    bounded_preview_text(tool_name, MAX_PREVIEW_TOOL_NAME_BYTES);
                let (content, content_truncated) =
                    bounded_preview_text(&update.content, MAX_PREVIEW_TEXT_BYTES);
                let (activity, activity_truncated) = match &update.activity {
                    Some(activity) => {
                        let (activity, truncated) =
                            bounded_preview_text(activity, MAX_PREVIEW_ACTIVITY_BYTES);
                        (Some(activity), truncated)
                    }
                    None => (None, false),
                };
                TeaEvent::Preview(PreviewEvent::ToolProgress {
                    identity: PreviewIdentity::tool(run, tool_call_id.clone()),
                    sequence: event.sequence,
                    tool_name,
                    content,
                    activity,
                    truncated: tool_name_truncated || content_truncated || activity_truncated,
                })
            }
            _ => TeaEvent::Agent {
                run,
                event: event.clone(),
            },
        };
        self.publish_normalized(event);
    }

    pub(crate) fn publish(&self, event: TeaEvent) {
        let Some(event) = (match event {
            TeaEvent::Agent { run, event } => project_owned_agent_event(run, event),
            TeaEvent::Preview(preview) => Some(TeaEvent::Preview(normalize_preview(preview))),
            event => Some(event),
        }) else {
            return;
        };
        self.publish_normalized(event);
    }

    fn publish_normalized(&self, event: TeaEvent) {
        let Ok(mut hub) = self.state.lock() else {
            // Event consumers are passive. A poisoned local fanout must not
            // retroactively turn a successful durable commit into a failed
            // operation or block subsequent recovery.
            return;
        };
        if !hub.accept(&event) {
            return;
        }
        hub.subscribers.retain(|subscriber| {
            let Some(subscriber) = subscriber.upgrade() else {
                return false;
            };
            subscriber.enqueue(event.clone());
            true
        });
    }
}

fn project_owned_agent_event(run: ObservationRun, event: AgentEvent) -> Option<TeaEvent> {
    match event {
        AgentEvent {
            sequence,
            kind:
                AgentEventKind::MessageUpdate {
                    message_id,
                    text_delta,
                },
            ..
        } => {
            let (text, truncated) = bounded_preview_text(&text_delta, MAX_PREVIEW_TEXT_BYTES);
            Some(TeaEvent::Preview(PreviewEvent::AssistantText {
                identity: PreviewIdentity::assistant(run, message_id),
                sequence,
                text,
                truncated,
            }))
        }
        AgentEvent {
            sequence,
            kind:
                AgentEventKind::ToolExecutionUpdate {
                    tool_call_id,
                    tool_name,
                    update,
                },
            ..
        } => {
            let (tool_name, tool_name_truncated) =
                bounded_preview_text(&tool_name, MAX_PREVIEW_TOOL_NAME_BYTES);
            let (content, content_truncated) =
                bounded_preview_text(&update.content, MAX_PREVIEW_TEXT_BYTES);
            let (activity, activity_truncated) = match update.activity {
                Some(activity) => {
                    let (activity, truncated) =
                        bounded_preview_text(&activity, MAX_PREVIEW_ACTIVITY_BYTES);
                    (Some(activity), truncated)
                }
                None => (None, false),
            };
            Some(TeaEvent::Preview(PreviewEvent::ToolProgress {
                identity: PreviewIdentity::tool(run, tool_call_id),
                sequence,
                tool_name,
                content,
                activity,
                truncated: tool_name_truncated || content_truncated || activity_truncated,
            }))
        }
        event => Some(TeaEvent::Agent { run, event }),
    }
}

#[derive(Default)]
struct EventHubState {
    subscribers: Vec<Weak<EventSubscriber>>,
    live_previews: VecDeque<PreviewEvent>,
    observed_sequences: BTreeMap<ObservationRun, EventSequence>,
    fenced_previews: VecDeque<PreviewIdentity>,
}

impl EventHubState {
    fn accept(&mut self, event: &TeaEvent) -> bool {
        if let Some((run, sequence)) = event.agent_sequence() {
            let starts_run = matches!(
                event,
                TeaEvent::Agent {
                    event: AgentEvent {
                        kind: AgentEventKind::AgentStart,
                        ..
                    },
                    ..
                }
            );
            let ends_run = matches!(
                event,
                TeaEvent::Agent {
                    event: AgentEvent {
                        kind: AgentEventKind::AgentEnd { .. },
                        ..
                    },
                    ..
                }
            );
            match self.observed_sequences.get_mut(run) {
                Some(previous) if sequence <= *previous => return false,
                Some(previous) => *previous = sequence,
                None if starts_run => {
                    self.observed_sequences.insert(run.clone(), sequence);
                }
                None => return false,
            }
            if ends_run {
                self.live_previews
                    .retain(|preview| &preview.identity().run != run);
                self.fenced_previews.retain(|identity| &identity.run != run);
                self.observed_sequences.remove(run);
            }
        }

        if let Some(preview) = event.preview() {
            if self.fenced_previews.contains(preview.identity()) {
                return false;
            }
            self.record_preview(preview.clone());
        }
        let terminal_fences = event.terminal_preview_identities();
        if !terminal_fences.is_empty() {
            self.live_previews.retain(|preview| {
                !terminal_fences
                    .iter()
                    .any(|identity| identity == preview.identity())
            });
            for identity in terminal_fences {
                record_terminal_preview_fence(&mut self.fenced_previews, identity);
            }
        }
        true
    }

    fn record_preview(&mut self, preview: PreviewEvent) {
        if let Some(index) = self
            .live_previews
            .iter()
            .position(|current| current.identity() == preview.identity())
        {
            let current = self
                .live_previews
                .remove(index)
                .expect("preview index is in bounds");
            self.live_previews.push_back(current.coalesce(preview));
            return;
        }
        if self.live_previews.len() == LIVE_PREVIEW_LIMIT {
            self.live_previews.pop_front();
        }
        self.live_previews.push_back(preview);
    }
}

struct EventSubscriber {
    state: Mutex<SubscriberState>,
    notifications: SyncSender<()>,
}

impl EventSubscriber {
    fn new(notifications: SyncSender<()>) -> Self {
        Self {
            state: Mutex::new(SubscriberState::default()),
            notifications,
        }
    }

    fn enqueue(&self, event: TeaEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.lagged {
            return;
        }
        let queued = match event.preview() {
            Some(preview) => state.enqueue_preview(preview.clone()),
            None => state.enqueue_semantic(event),
        };
        if queued {
            let _ = self.notifications.try_send(());
        }
    }
}

#[derive(Default)]
struct SubscriberState {
    events: VecDeque<TeaEvent>,
    fenced_previews: VecDeque<PreviewIdentity>,
    completed_runs: VecDeque<ObservationRun>,
    lagged: bool,
}

impl SubscriberState {
    fn enqueue_preview(&mut self, preview: PreviewEvent) -> bool {
        if self.fenced_previews.contains(preview.identity())
            || self.completed_runs.contains(&preview.identity().run)
        {
            return false;
        }
        let preview = if let Some(index) = self
            .events
            .iter()
            .position(|event| event.preview().is_some_and(|current| current.identity() == preview.identity()))
        {
            let current = self.events.remove(index).expect("preview index is in bounds");
            match current {
                TeaEvent::Preview(current) => current.coalesce(preview),
                TeaEvent::Agent { .. }
                | TeaEvent::Session(_)
                | TeaEvent::Harness(_)
                | TeaEvent::Artifact(_) => unreachable!("only previews match a preview identity"),
            }
        } else {
            preview
        };
        if self.events.len() == SUBSCRIBER_EVENT_BUFFER {
            // Preview loss is explicit in its transient contract and never
            // turns a reliable operation outcome into a channel failure.
            return false;
        }
        self.events.push_back(TeaEvent::Preview(preview));
        true
    }

    fn enqueue_semantic(&mut self, event: TeaEvent) -> bool {
        if let Some(run) = event.completed_observation_run() {
            self.events.retain(|queued| {
                !queued
                    .preview()
                    .is_some_and(|preview| &preview.identity().run == run)
            });
            self.fenced_previews.retain(|identity| &identity.run != run);
            record_completed_run_fence(&mut self.completed_runs, run.clone());
        }
        let terminal_fences = event.terminal_preview_identities();
        if !terminal_fences.is_empty() {
            self.events.retain(|queued| {
                !queued.preview().is_some_and(|preview| {
                    terminal_fences
                        .iter()
                        .any(|identity| identity == preview.identity())
                })
            });
            for identity in terminal_fences {
                record_terminal_preview_fence(&mut self.fenced_previews, identity);
            }
        }
        if self.events.len() == SUBSCRIBER_EVENT_BUFFER {
            self.events.clear();
            self.fenced_previews.clear();
            self.completed_runs.clear();
            self.lagged = true;
            return true;
        }
        self.events.push_back(event);
        true
    }
}

fn record_terminal_preview_fence(
    fences: &mut VecDeque<PreviewIdentity>,
    identity: PreviewIdentity,
) {
    if let Some(index) = fences.iter().position(|current| current == &identity) {
        let _ = fences.remove(index);
    }
    if fences.len() == TERMINAL_PREVIEW_FENCE_LIMIT {
        let _ = fences.pop_front();
    }
    fences.push_back(identity);
}

fn record_completed_run_fence(fences: &mut VecDeque<ObservationRun>, run: ObservationRun) {
    if let Some(index) = fences.iter().position(|current| current == &run) {
        let _ = fences.remove(index);
    }
    if fences.len() == COMPLETED_RUN_FENCE_LIMIT {
        let _ = fences.pop_front();
    }
    fences.push_back(run);
}

fn bounded_preview_text(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_owned(), false);
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (value[..end].to_owned(), true)
}

fn normalize_preview(preview: PreviewEvent) -> PreviewEvent {
    match preview {
        PreviewEvent::AssistantText {
            identity,
            sequence,
            text,
            truncated,
        } => {
            let (text, overflowed) = bounded_preview_text(&text, MAX_PREVIEW_TEXT_BYTES);
            PreviewEvent::AssistantText {
                identity,
                sequence,
                text,
                truncated: truncated || overflowed,
            }
        }
        PreviewEvent::ToolProgress {
            identity,
            sequence,
            tool_name,
            content,
            activity,
            truncated,
        } => {
            let (tool_name, tool_name_truncated) =
                bounded_preview_text(&tool_name, MAX_PREVIEW_TOOL_NAME_BYTES);
            let (content, content_truncated) =
                bounded_preview_text(&content, MAX_PREVIEW_TEXT_BYTES);
            let (activity, activity_truncated) = match activity {
                Some(activity) => {
                    let (activity, truncated) =
                        bounded_preview_text(&activity, MAX_PREVIEW_ACTIVITY_BYTES);
                    (Some(activity), truncated)
                }
                None => (None, false),
            };
            PreviewEvent::ToolProgress {
                identity,
                sequence,
                tool_name,
                content,
                activity,
                truncated: truncated || tool_name_truncated || content_truncated || activity_truncated,
            }
        }
    }
}

fn retain_preview_text_suffix(existing: &str, newer: &str) -> (String, bool) {
    let mut combined = String::with_capacity(existing.len().saturating_add(newer.len()));
    combined.push_str(existing);
    combined.push_str(newer);
    if combined.len() <= MAX_PREVIEW_TEXT_BYTES {
        return (combined, false);
    }
    let mut start = combined.len().saturating_sub(MAX_PREVIEW_TEXT_BYTES);
    while start < combined.len() && !combined.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    (combined[start..].to_owned(), true)
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use crate::event::{AgentEventKind, EventSequence};
    use crate::state::{AgentMessage, MessageId, RunId, TurnId};
    use tea_session::{MemorySession, Metadata, SessionHeader};

    fn run() -> ObservationRun {
        ObservationRun::new(
            LaneId::main(),
            OperationId::new("observation-operation").expect("valid fixture operation ID"),
            EpochId::new("observation-epoch").expect("valid fixture epoch ID"),
            RunId(1),
        )
    }

    fn snapshot() -> TeaObservationSnapshot {
        let session = MemorySession::create(SessionHeader::new(
            SessionId::new("observation-test").expect("valid fixture session ID"),
            "observation-workspace",
            Metadata::new(),
        ))
        .expect("fixture session creates")
        .snapshot()
        .expect("fixture session snapshots");
        let view = HarnessSnapshotView::from_session(&session).expect("snapshot view builds");
        TeaObservationSnapshot { session, view }
    }

    fn start_run(hub: &EventHub) {
        let run = run();
        hub.publish(TeaEvent::Agent {
            run,
            event: AgentEvent {
                run_id: RunId(1),
                sequence: EventSequence(0),
                kind: AgentEventKind::AgentStart,
            },
        });
    }

    fn assistant_message(id: u64, content: &str) -> AgentMessage {
        AgentMessage::Assistant {
            id: MessageId(id),
            content: content.into(),
            tool_calls: Vec::new(),
            stop_reason: None,
            error_message: None,
            opaque_context: Vec::new(),
        }
    }

    fn assistant_preview(sequence: u64, text: &str) -> TeaEvent {
        TeaEvent::Preview(PreviewEvent::AssistantText {
            identity: PreviewIdentity::assistant(
                run(),
                MessageId(1),
            ),
            sequence: EventSequence(sequence),
            text: text.into(),
            truncated: false,
        })
    }

    #[test]
    fn slow_semantic_consumer_receives_an_explicit_lag_signal() {
        let hub = EventHub::default();
        start_run(&hub);
        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");
        for sequence in 1..=SUBSCRIBER_EVENT_BUFFER.saturating_add(1) {
            hub.publish(TeaEvent::Agent {
                run: run(),
                event: AgentEvent {
                    run_id: RunId(1),
                    sequence: EventSequence(sequence as u64),
                    kind: AgentEventKind::TurnStart {
                        turn_id: TurnId(sequence as u64),
                    },
                },
            });
        }

        assert_eq!(subscription.try_recv(), Err(TeaEventTryRecvError::Lagged));
    }

    #[test]
    fn pending_assistant_previews_coalesce_without_growing_a_transcript_snapshot() {
        let hub = EventHub::default();
        start_run(&hub);
        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");
        hub.publish(assistant_preview(1, "first "));
        hub.publish(assistant_preview(2, "second"));

        assert_eq!(
            subscription.try_recv(),
            Ok(assistant_preview(2, "first second"))
        );
        assert_eq!(subscription.try_recv(), Err(TeaEventTryRecvError::Empty));
    }

    #[test]
    fn agent_message_delta_projects_to_a_bounded_preview_without_a_semantic_snapshot() {
        let hub = EventHub::default();
        start_run(&hub);
        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");
        hub.publish_agent(
            run(),
            &AgentEvent {
                run_id: RunId(1),
                sequence: EventSequence(1),
                kind: AgentEventKind::MessageUpdate {
                    message_id: MessageId(1),
                    text_delta: "one exact fragment".into(),
                },
            },
        );

        assert_eq!(
            subscription.try_recv(),
            Ok(assistant_preview(1, "one exact fragment"))
        );
    }

    #[test]
    fn subscription_snapshot_captures_the_bounded_live_preview_frontier() {
        let hub = EventHub::default();
        start_run(&hub);
        hub.publish(assistant_preview(1, "still streaming"));

        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");

        assert_eq!(
            subscription.snapshot.view.previews,
            vec![assistant_preview(1, "still streaming").preview().cloned().expect("preview")]
        );
        assert_eq!(subscription.try_recv(), Err(TeaEventTryRecvError::Empty));
    }

    #[test]
    fn session_events_at_or_before_the_snapshot_sequence_are_suppressed() {
        let hub = EventHub::default();
        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");
        hub.publish(TeaEvent::Session(SessionEvent::OperationAccepted {
            sequence: Sequence(0),
            lane_id: LaneId::main(),
            operation_id: OperationId::new("already-snapshotted")
                .expect("valid fixture operation ID"),
        }));

        assert_eq!(subscription.try_recv(), Err(TeaEventTryRecvError::Empty));
    }

    #[test]
    fn terminal_message_removes_its_pending_preview_and_fences_later_previews() {
        let hub = EventHub::default();
        start_run(&hub);
        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");
        hub.publish(assistant_preview(1, "partial"));
        hub.publish(TeaEvent::Agent {
            run: run(),
            event: AgentEvent {
                run_id: RunId(1),
                sequence: EventSequence(2),
                kind: AgentEventKind::MessageEnd {
                    message: assistant_message(1, "complete"),
                },
            },
        });
        assert!(matches!(
            subscription.try_recv(),
            Ok(TeaEvent::Agent {
                event: AgentEvent {
                    kind: AgentEventKind::MessageEnd { .. },
                    ..
                },
                ..
            })
        ));
        hub.publish(assistant_preview(3, "late"));
        assert_eq!(subscription.try_recv(), Err(TeaEventTryRecvError::Empty));
    }

    #[test]
    fn completed_run_fence_rejects_a_preview_after_agent_end() {
        let (notifications, _receiver) = mpsc::sync_channel(1);
        let subscriber = EventSubscriber::new(notifications);
        subscriber.enqueue(TeaEvent::Agent {
            run: run(),
            event: AgentEvent {
                run_id: RunId(1),
                sequence: EventSequence(1),
                kind: AgentEventKind::AgentEnd { messages: Vec::new() },
            },
        });
        subscriber.enqueue(assistant_preview(2, "late after agent end"));

        let state = subscriber
            .state
            .lock()
            .expect("subscriber state remains available");
        assert!(state.completed_runs.contains(&run()));
        assert!(state.events.iter().all(|event| event.preview().is_none()));
    }

    #[test]
    fn terminal_fences_and_completed_run_fences_are_bounded() {
        let mut terminal_fences = VecDeque::new();
        for id in 0..TERMINAL_PREVIEW_FENCE_LIMIT.saturating_add(1) {
            record_terminal_preview_fence(
                &mut terminal_fences,
                PreviewIdentity::assistant(run(), MessageId(id as u64)),
            );
        }
        assert_eq!(terminal_fences.len(), TERMINAL_PREVIEW_FENCE_LIMIT);

        let mut completed_runs = VecDeque::new();
        for id in 0..COMPLETED_RUN_FENCE_LIMIT.saturating_add(1) {
            let operation_id = OperationId::new(format!("completed-observation-{id}"))
                .expect("valid fixture operation ID");
            record_completed_run_fence(
                &mut completed_runs,
                ObservationRun::new(
                    LaneId::main(),
                    operation_id,
                    EpochId::new("observation-epoch").expect("valid fixture epoch ID"),
                    RunId(1),
                ),
            );
        }
        assert_eq!(completed_runs.len(), COMPLETED_RUN_FENCE_LIMIT);
    }

    #[test]
    fn preview_payloads_have_a_hard_retained_byte_limit() {
        let hub = EventHub::default();
        start_run(&hub);
        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");
        hub.publish(assistant_preview(
            1,
            &"x".repeat(MAX_PREVIEW_TEXT_BYTES.saturating_add(1)),
        ));

        match subscription.try_recv() {
            Ok(TeaEvent::Preview(PreviewEvent::AssistantText {
                text, truncated, ..
            })) => {
                assert!(truncated);
                assert!(text.len() <= MAX_PREVIEW_TEXT_BYTES);
            }
            other => panic!("expected bounded assistant preview, got {other:?}"),
        }
    }

    #[test]
    fn tool_preview_display_fields_are_bounded() {
        let hub = EventHub::default();
        start_run(&hub);
        let subscription = hub
            .subscribe_with_snapshot(|| Ok(snapshot()))
            .expect("subscription opens");
        hub.publish(TeaEvent::Preview(PreviewEvent::ToolProgress {
            identity: PreviewIdentity::tool(
                run(),
                ToolCallId::new("preview-tool-call").expect("valid fixture tool call ID"),
            ),
            sequence: EventSequence(1),
            tool_name: "t".repeat(MAX_PREVIEW_TOOL_NAME_BYTES.saturating_add(1)),
            content: "c".repeat(MAX_PREVIEW_TEXT_BYTES.saturating_add(1)),
            activity: Some("a".repeat(MAX_PREVIEW_ACTIVITY_BYTES.saturating_add(1))),
            truncated: false,
        }));

        match subscription.try_recv() {
            Ok(TeaEvent::Preview(PreviewEvent::ToolProgress {
                tool_name,
                content,
                activity,
                truncated,
                ..
            })) => {
                assert!(truncated);
                assert!(tool_name.len() <= MAX_PREVIEW_TOOL_NAME_BYTES);
                assert!(content.len() <= MAX_PREVIEW_TEXT_BYTES);
                assert!(activity.is_some_and(|activity| {
                    activity.len() <= MAX_PREVIEW_ACTIVITY_BYTES
                }));
            }
            other => panic!("expected bounded tool preview, got {other:?}"),
        }
    }
}
