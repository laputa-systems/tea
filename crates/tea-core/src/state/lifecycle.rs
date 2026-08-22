//! Agent and run lifecycle state and snapshots.

use super::*;
use std::collections::BTreeSet;
use std::fmt;

/// Why model generation stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// The provider produced a normal final response.
    Stop,
    /// The provider requested tool execution.
    ToolUse,
    /// The provider stopped because the output token limit was reached.
    Length,
    /// The provider aborted generation independently of host cancellation.
    Aborted,
    /// The host cancelled the run.
    Cancelled,
    /// The provider or host failed.
    Error,
}

impl StopReason {
    /// Compatibility spelling for the upstream `stop` outcome.
    #[allow(non_upper_case_globals)]
    pub const EndTurn: Self = Self::Stop;
}

/// The externally observable agent ownership phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentPhase {
    /// No run currently owns the agent.
    Idle,
    /// A run is processing model/tool work.
    Running(RunId),
    /// Cancellation was requested and settlement is pending.
    Cancelling(RunId),
}

/// The run lifecycle.  Terminal variants are immutable outcomes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunPhase {
    /// The run handle exists but has not emitted its first turn event.
    Created,
    /// The run is processing a turn.
    Running,
    /// The run has stopped accepting work and is settling observers.
    Settling,
    /// The run completed normally.
    Succeeded,
    /// The run completed with a runtime error.
    Failed,
    /// The run completed because cancellation won settlement.
    Cancelled,
}

impl RunPhase {
    /// Whether no further run transition is legal.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Mutable state owned by an [`Agent`](crate::Agent).
#[derive(Clone, Debug)]
pub struct AgentState {
    /// Static system instructions.
    pub system_prompt: String,
    /// Selected model identity.
    pub model: Option<ModelDescriptor>,
    /// Selected reasoning level.
    pub thinking_level: ThinkingLevel,
    /// Canonical conversation history.
    pub messages: Vec<AgentMessage>,
    /// Explicit host-only context retained beside the transcript.
    ///
    /// The core does not invent a UI-message type or send these values to a
    /// provider by default. A context hook may filter or convert them at the
    /// model boundary.
    pub host_messages: Vec<SerializedJson>,
    /// Current ownership phase.
    pub phase: AgentPhase,
    /// Partial assistant content while a model stream is active.
    pub partial_response: Option<String>,
    /// Whether a provider stream is currently open.
    pub is_streaming: bool,
    /// Tool calls awaiting preparation or execution.
    pub pending_tool_calls: BTreeSet<ToolCallId>,
    /// Last runtime error, retained for state inspection.
    pub last_error: Option<String>,
    /// Retained provider-reported model accounting.
    pub accounting: ModelAccountingSnapshot,
    /// Monotonic generation of canonical conversation history.
    ///
    /// A compactor owns only a snapshot. Its replacement may commit only if
    /// this generation still names the source it summarized.
    pub history_revision: u64,
    next_message_id: u64,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model: None,
            thinking_level: ThinkingLevel::Off,
            messages: Vec::new(),
            host_messages: Vec::new(),
            phase: AgentPhase::Idle,
            partial_response: None,
            is_streaming: false,
            pending_tool_calls: BTreeSet::new(),
            last_error: None,
            accounting: ModelAccountingSnapshot::default(),
            history_revision: 0,
            next_message_id: 1,
        }
    }
}

impl AgentState {
    /// Allocate the next message identifier.
    pub(crate) fn allocate_message_id(&mut self) -> MessageId {
        let id = MessageId(self.next_message_id);
        self.next_message_id = self.next_message_id.saturating_add(1);
        id
    }

    /// Replace retained history after the compaction transaction has validated it.
    ///
    /// The next generated ID advances beyond any caller-proposed replacement
    /// ID, so a later prompt cannot collide with a compactor-created summary.
    pub(crate) fn replace_messages(&mut self, messages: Vec<AgentMessage>) {
        let next_id = messages
            .iter()
            .map(message_id)
            .map(|id| id.0.saturating_add(1))
            .max()
            .unwrap_or(1);
        self.next_message_id = self.next_message_id.max(next_id);
        self.messages = messages;
        self.history_revision = self.history_revision.saturating_add(1);
    }

    /// Append one canonical message and advance the source generation.
    pub(crate) fn append_message(&mut self, message: AgentMessage) {
        self.messages.push(message);
        self.history_revision = self.history_revision.saturating_add(1);
    }

    /// Replace the most recent canonical message and advance the source generation.
    pub(crate) fn replace_last_message(&mut self, message: AgentMessage) {
        if let Some(last) = self.messages.last_mut() {
            *last = message;
            self.history_revision = self.history_revision.saturating_add(1);
        }
    }

    /// Remove a transient suffix and advance the source generation when it changed.
    pub(crate) fn truncate_messages(&mut self, length: usize) {
        if length < self.messages.len() {
            self.messages.truncate(length);
            self.history_revision = self.history_revision.saturating_add(1);
        }
    }

    /// Produce an owned inspection snapshot.
    pub(crate) fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            system_prompt: self.system_prompt.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            messages: self.messages.clone(),
            host_messages: self.host_messages.clone(),
            phase: self.phase,
            partial_response: self.partial_response.clone(),
            is_streaming: self.is_streaming,
            pending_tool_calls: self.pending_tool_calls.clone(),
            last_error: self.last_error.clone(),
            accounting: self.accounting.clone(),
            history_revision: self.history_revision,
        }
    }
}

fn message_id(message: &AgentMessage) -> MessageId {
    match message {
        AgentMessage::User { id, .. }
        | AgentMessage::Assistant { id, .. }
        | AgentMessage::ToolResult { id, .. } => *id,
    }
}

/// Owned, read-only view of agent state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSnapshot {
    /// Static system instructions.
    pub system_prompt: String,
    /// Selected model identity.
    pub model: Option<ModelDescriptor>,
    /// Selected reasoning level.
    pub thinking_level: ThinkingLevel,
    /// Canonical conversation history.
    pub messages: Vec<AgentMessage>,
    /// Explicit host-only context retained beside the transcript.
    pub host_messages: Vec<SerializedJson>,
    /// Current ownership phase.
    pub phase: AgentPhase,
    /// Partial assistant content, if streaming.
    pub partial_response: Option<String>,
    /// Whether a provider stream is open.
    pub is_streaming: bool,
    /// Pending tool calls.
    pub pending_tool_calls: BTreeSet<ToolCallId>,
    /// Last runtime error.
    pub last_error: Option<String>,
    /// Retained per-turn and aggregate provider-reported model accounting.
    pub accounting: ModelAccountingSnapshot,
    /// Monotonic canonical-history generation.
    pub history_revision: u64,
}

/// Mutable state retained by one run handle.
#[derive(Clone, Debug)]
pub struct RunState {
    /// Stable run identifier.
    pub id: RunId,
    /// Current lifecycle phase.
    pub phase: RunPhase,
    /// Current turn, if one has started.
    pub turn_id: Option<TurnId>,
    /// Terminal reason, if known.
    pub stop_reason: Option<StopReason>,
    /// Runtime error text, if failed.
    pub error: Option<String>,
    /// Number of events emitted for this run.
    pub event_count: u64,
    /// Lifecycle events emitted in source order.
    pub events: Vec<crate::event::AgentEvent>,
}

impl RunState {
    /// Create a run before its first lifecycle event.
    pub const fn created(id: RunId) -> Self {
        Self {
            id,
            phase: RunPhase::Created,
            turn_id: None,
            stop_reason: None,
            error: None,
            event_count: 0,
            events: Vec::new(),
        }
    }

    /// Produce an owned run snapshot.
    pub fn snapshot(&self) -> RunSnapshot {
        RunSnapshot {
            id: self.id,
            phase: self.phase,
            turn_id: self.turn_id,
            stop_reason: self.stop_reason,
            error: self.error.clone(),
            event_count: self.event_count,
        }
    }
}

/// Owned, read-only view of run state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunSnapshot {
    /// Stable run identifier.
    pub id: RunId,
    /// Current lifecycle phase.
    pub phase: RunPhase,
    /// Current turn identifier.
    pub turn_id: Option<TurnId>,
    /// Terminal reason.
    pub stop_reason: Option<StopReason>,
    /// Runtime error text.
    pub error: Option<String>,
    /// Number of emitted events.
    pub event_count: u64,
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run-{}", self.0)
    }
}
