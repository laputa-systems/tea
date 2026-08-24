//! The owned records that make up a v1 trace.
//!
//! A trace is deliberately smaller than a session or UI event log.  It has one
//! header, zero or more turn and tool records, and one terminal record.  The
//! order in which [`TraceEvent`] values are handed to a sink is the trajectory
//! order; no tree identifiers or session metadata are implied here.

use std::collections::BTreeMap;

/// Version of the compact trace schema described by this crate.
pub const TRACE_SCHEMA_VERSION: u16 = 1;

/// The stable, host-assigned number of a model turn within an episode.
pub type TurnIndex = u32;

/// One content-free stage in a compaction operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum CompactionStage {
    /// Identity and immutable policy were allocated.
    #[default]
    Started,
    /// The canonical source and retained suffix were selected.
    SourceSelected,
    /// A compactor request was prepared.
    RequestPrepared,
    /// Provider usage or its serialized request observation arrived.
    ProviderUsageObserved,
    /// A replacement was checked before commit.
    ReplacementProposed,
    /// The compaction reached one terminal outcome.
    Terminal,
    /// The first normal provider request after a committed replacement.
    PostCompactionRequestObserved,
}

/// Content-free, append-only observability record for one compaction.
///
/// This type deliberately contains identifiers, sizes, fingerprints,
/// counters, and classified outcomes only. It never carries a checkpoint,
/// prompt, tool argument, tool result, or serialized request body.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Compaction {
    /// Stable join key shared by all stages of one operation.
    pub compaction_id: String,
    /// Which lifecycle stage this record captures.
    pub stage: CompactionStage,
    /// Manual or automatic trigger, when allocated.
    pub trigger: Option<String>,
    /// Concrete threshold, overflow, or user-request reason, when allocated.
    pub reason: Option<String>,
    /// Agent-loop phase, when allocated.
    pub phase: Option<String>,
    /// Versioned strategy identity, when allocated.
    pub strategy_id: Option<String>,
    /// Strategy schema version, when allocated.
    pub strategy_schema_version: Option<u32>,
    /// Model-visible request layout, when known.
    pub request_layout: Option<String>,
    /// Fingerprint of the model-facing strategy instruction, when supplied.
    pub prompt_fingerprint: Option<u64>,
    /// Canonical history generation selected for the source.
    pub source_history_revision: Option<u64>,
    /// Attempt count within the operation kind.
    pub attempt: Option<u32>,
    /// Automatic-operation ordinal in its run.
    pub automatic_ordinal: Option<u32>,
    /// Overflow-retry ordinal in its run.
    pub overflow_retry_ordinal: Option<u32>,
    /// Whether this operation will retry an interrupted provider request.
    pub retry_provider_request: Option<bool>,
    /// Canonical source message count.
    pub source_message_count: Option<usize>,
    /// Canonical source byte count.
    pub source_message_bytes: Option<usize>,
    /// Exact retained suffix message count.
    pub retained_message_count: Option<usize>,
    /// Exact retained suffix byte count.
    pub retained_suffix_bytes: Option<usize>,
    /// Selected source tool-result byte count.
    pub tool_result_bytes: Option<usize>,
    /// Prepared compactor-context bytes, when known.
    pub compactor_context_bytes: Option<usize>,
    /// Prepared compactor tool count, when known.
    pub compactor_tool_count: Option<usize>,
    /// Whether compactor tool execution was prohibited.
    pub tools_execution_prohibited: Option<bool>,
    /// Whether the selected source was an exact active-context prefix.
    pub source_is_active_context_prefix: Option<bool>,
    /// Proposed replacement message count.
    pub replacement_message_count: Option<usize>,
    /// Proposed replacement byte count.
    pub replacement_bytes: Option<usize>,
    /// Estimated context tokens after replacement, when a policy supplied a budget.
    pub estimated_context_tokens_after: Option<u64>,
    /// Working context headroom after replacement, when a policy supplied a budget.
    pub headroom_tokens: Option<u64>,
    /// Whether structural validation passed.
    pub structural_validation_passed: Option<bool>,
    /// Whether the retained suffix matched exactly.
    pub retained_suffix_exact: Option<bool>,
    /// Whether source generation still matched during proposal validation.
    pub source_generation_matches: Option<bool>,
    /// Provider-reported input tokens, when available.
    pub provider_input_tokens: Option<u64>,
    /// Provider-reported output tokens, when available.
    pub provider_output_tokens: Option<u64>,
    /// Provider-reported cache-read input tokens, when available.
    pub provider_cache_read_tokens: Option<u64>,
    /// Provider-reported cache-write input tokens, when available.
    pub provider_cache_write_tokens: Option<u64>,
    /// Exact adapter-serialized request bytes, when available.
    pub serialized_request_bytes: Option<usize>,
    /// Adapter-defined cache-domain fingerprint, when available.
    pub cache_domain_fingerprint: Option<u64>,
    /// One classified terminal outcome.
    pub terminal_outcome: Option<String>,
    /// Model turn joined to the first normal request after a committed replacement.
    pub post_compaction_turn_index: Option<TurnIndex>,
}

impl Compaction {
    /// Creates a content-free record for `compaction_id` and `stage`.
    pub fn new(compaction_id: impl Into<String>, stage: CompactionStage) -> Self {
        Self {
            compaction_id: compaction_id.into(),
            stage,
            ..Self::default()
        }
    }
}

/// The first record in an episode.
///
/// Metadata is intentionally a deterministic string map.  Hosts that need a
/// richer representation can encode it before crossing this dependency-free
/// boundary.  Secrets must be removed before this record is sent to a sink.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EpisodeHeader {
    /// Host-assigned identifier for the episode.
    pub episode_id: String,
    /// Optional host metadata used to identify a run or dataset partition.
    pub metadata: BTreeMap<String, String>,
    /// Optional wall-clock time supplied by the host, in milliseconds since
    /// the Unix epoch.  The trace crate does not read a clock.
    pub started_at_ms: Option<u64>,
    /// Durable attribution for the exact harness/core run, when a host owns
    /// one. These are identifiers only; no workspace path, prompt, provider
    /// request body, or credential can cross this telemetry boundary.
    pub provenance: Option<TraceProvenance>,
}

impl EpisodeHeader {
    /// Creates a header with no ambient metadata or timestamp.
    pub fn new(episode_id: impl Into<String>) -> Self {
        Self {
            episode_id: episode_id.into(),
            ..Self::default()
        }
    }

    /// Adds one deterministic metadata field to this header.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Attaches a host-provided wall-clock start time.
    pub fn with_started_at_ms(mut self, started_at_ms: u64) -> Self {
        self.started_at_ms = Some(started_at_ms);
        self
    }

    /// Attach content-free durable run attribution.
    pub fn with_provenance(mut self, provenance: TraceProvenance) -> Self {
        self.provenance = Some(provenance);
        self
    }
}

/// Content-free durable identity for one traced execution.
///
/// This type deliberately uses strings rather than depending on the session
/// crate's ID newtypes: `tea-trace` remains an optional passive observer with
/// no ownership of session storage or harness state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TraceProvenance {
    pub session_id: Option<String>,
    pub lane_id: Option<String>,
    /// Durable child-agent identity when the traced lane belongs to one. This
    /// remains optional because the main lane has no child-agent identity.
    pub agent_id: Option<String>,
    pub operation_id: Option<String>,
    pub epoch_id: Option<String>,
    pub core_run_id: Option<String>,
    pub harness_snapshot_id: Option<String>,
    pub harness_revision_id: Option<String>,
    pub model_harness_profile_id: Option<String>,
    pub experiment_id: Option<String>,
}

/// Content-free cache evidence observed at an adapter request boundary.
///
/// Deterministic prefix similarity and provider cache usage remain distinct:
/// missing provider metrics stay unknown rather than being represented as
/// zeroes or inferred from a local fingerprint.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CacheEvidence {
    /// Core classification of continuity with the preceding request.
    pub continuity: Option<String>,
    /// Stable fingerprint of complete core-owned prompt/tool/model/thinking domain.
    pub cache_domain_fingerprint: Option<u64>,
    /// Named core-owned components that changed from the preceding request.
    pub changed_cache_domain_components: Vec<String>,
    /// Converted provider-context bytes in the current logical request.
    pub context_bytes: Option<u64>,
    /// Shared converted-context bytes with the preceding request.
    pub common_context_prefix_bytes: Option<u64>,
    /// Shared converted-context ratio, in millionths of predecessor bytes.
    pub common_context_prefix_ratio_millionths: Option<u32>,
    /// Whether same-domain context changed before its prior end.
    pub context_projection_changed: Option<bool>,
    /// Stable fingerprint of the converted provider context.
    pub context_fingerprint: Option<u64>,
    /// Stable fingerprint of the system prompt.
    pub system_prompt_fingerprint: Option<u64>,
    /// Stable fingerprint of the complete ordered tool definitions.
    pub tool_definition_fingerprint: Option<u64>,
    /// Stable fingerprint of exposed tool name order.
    pub tool_order_fingerprint: Option<u64>,
    /// Stable fingerprint of selected model identity.
    pub model_fingerprint: Option<u64>,
    /// Stable fingerprint of provider-neutral thinking configuration.
    pub thinking_fingerprint: Option<u64>,
    pub deterministic_common_prefix_bytes: Option<u64>,
    pub deterministic_common_prefix_tokens_estimate: Option<u64>,
    pub provider_cache_read_tokens: Option<u64>,
    pub provider_cache_write_tokens: Option<u64>,
    /// Exact adapter-serialized request bytes, when the adapter observed them.
    pub serialized_request_bytes: Option<u64>,
    /// Adapter-defined cache-domain fingerprint, when safely exposed.
    pub adapter_cache_domain_fingerprint: Option<u64>,
    /// Adapter-defined cache-relevant envelope component fingerprints.
    pub adapter_cache_domain_components: BTreeMap<String, u64>,
    pub provider_surface_digest: Option<String>,
}

/// One model request/response turn in an episode.
///
/// A missing response represents a turn that did not produce a completed
/// assistant response.  It is preferable to inventing a response merely to
/// make an incomplete run look successful.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    /// Zero-based turn number within the episode.
    pub index: TurnIndex,
    /// Redacted model input or a compact host-defined representation of it.
    pub input: String,
    /// Redacted assistant output, when one was produced.
    pub output: Option<String>,
    /// Host/model stop reason, if one is available.
    pub stop_reason: Option<String>,
    /// Per-request cache-domain evidence, when a host observed it.
    pub cache_evidence: Option<CacheEvidence>,
}

/// Domain spelling for [`Turn`] when the caller wants to emphasize that the
/// record represents a model turn.
pub type ModelTurn = Turn;

impl Turn {
    /// Creates a turn with an input and no response yet.
    pub fn new(index: TurnIndex, input: impl Into<String>) -> Self {
        Self {
            index,
            input: input.into(),
            ..Self::default()
        }
    }

    /// Sets the assistant output for this turn.
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = Some(output.into());
        self
    }

    /// Sets the host/model stop reason for this turn.
    pub fn with_stop_reason(mut self, stop_reason: impl Into<String>) -> Self {
        self.stop_reason = Some(stop_reason.into());
        self
    }

    /// Attach content-free request cache evidence.
    pub fn with_cache_evidence(mut self, cache_evidence: CacheEvidence) -> Self {
        self.cache_evidence = Some(cache_evidence);
        self
    }
}

/// One tool request and its eventual result.
///
/// V1 keeps request and result together so a compact linear sink can write one
/// record per execution.  A failed tool is represented by [`Tool::error`],
/// rather than by a sink error: tool failure is part of the trajectory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tool {
    /// Turn that requested the tool.
    pub turn_index: TurnIndex,
    /// Stable identifier for the request, supplied by the runtime.
    pub call_id: String,
    /// Tool name as exposed to the model.
    pub name: String,
    /// Redacted tool arguments.
    pub input: String,
    /// Redacted tool result, when execution completed successfully.
    pub output: Option<String>,
    /// Redacted tool failure, when execution failed.
    pub error: Option<String>,
}

/// Domain spelling for [`Tool`] when the caller wants to emphasize execution.
pub type ToolExecution = Tool;

impl Tool {
    /// Creates a pending tool record.
    pub fn new(
        turn_index: TurnIndex,
        call_id: impl Into<String>,
        name: impl Into<String>,
        input: impl Into<String>,
    ) -> Self {
        Self {
            turn_index,
            call_id: call_id.into(),
            name: name.into(),
            input: input.into(),
            ..Self::default()
        }
    }

    /// Marks this tool record as successful.
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = Some(output.into());
        self.error = None;
        self
    }

    /// Marks this tool record as failed.
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self.output = None;
        self
    }

    /// Reports whether this record contains a tool failure.
    pub fn is_failure(&self) -> bool {
        self.error.is_some()
    }
}

/// Why an episode stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum EndReason {
    /// The agent completed normally.
    #[default]
    Completed,
    /// The host cancelled the episode.
    Cancelled,
    /// The runtime or model failed.
    Failed,
    /// The host stopped the episode for a reason outside the runtime.
    Aborted,
    /// A host-defined reason that is still part of the terminal record.
    Other(String),
}

/// The final record in an episode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EpisodeEnd {
    /// Why the episode ended.
    pub reason: EndReason,
    /// Optional redacted diagnostic associated with a failed or aborted run.
    pub error: Option<String>,
    /// Optional host-provided wall-clock end time in milliseconds since the
    /// Unix epoch.  The trace crate does not read a clock.
    pub finished_at_ms: Option<u64>,
}

impl EpisodeEnd {
    /// Creates a terminal record for a normal completion.
    pub fn completed() -> Self {
        Self::default()
    }

    /// Creates a terminal record for host cancellation.
    pub fn cancelled() -> Self {
        Self {
            reason: EndReason::Cancelled,
            ..Self::default()
        }
    }

    /// Creates a terminal record for a runtime or model failure.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            reason: EndReason::Failed,
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// Attaches a host-provided wall-clock end time.
    pub fn with_finished_at_ms(mut self, finished_at_ms: u64) -> Self {
        self.finished_at_ms = Some(finished_at_ms);
        self
    }
}

/// One append-only record in a v1 episode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceEvent {
    /// Must be the first record in an episode.
    EpisodeHeader(EpisodeHeader),
    /// A model request/response turn.
    Turn(Turn),
    /// A tool request and result.
    Tool(Tool),
    /// A content-free compaction lifecycle or post-compaction request record.
    Compaction(Compaction),
    /// Must be the final record in an episode.
    EpisodeEnd(EpisodeEnd),
}

impl TraceEvent {
    /// Creates the episode-header event.
    pub fn episode_header(header: EpisodeHeader) -> Self {
        Self::EpisodeHeader(header)
    }

    /// Creates a model-turn event.
    pub fn model_turn(turn: Turn) -> Self {
        Self::Turn(turn)
    }

    /// Creates a tool-execution event.
    pub fn tool_execution(tool: Tool) -> Self {
        Self::Tool(tool)
    }

    /// Creates a content-free compaction record.
    pub fn compaction(compaction: Compaction) -> Self {
        Self::Compaction(compaction)
    }

    /// Creates the episode-end event.
    pub fn episode_end(end: EpisodeEnd) -> Self {
        Self::EpisodeEnd(end)
    }

    /// Returns the stable kind of this event without exposing its payload.
    pub const fn kind(&self) -> TraceEventKind {
        match self {
            Self::EpisodeHeader(_) => TraceEventKind::EpisodeHeader,
            Self::Turn(_) => TraceEventKind::Turn,
            Self::Tool(_) => TraceEventKind::Tool,
            Self::Compaction(_) => TraceEventKind::Compaction,
            Self::EpisodeEnd(_) => TraceEventKind::EpisodeEnd,
        }
    }

    /// Whether this event closes its episode.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::EpisodeEnd(_))
    }
}

impl From<EpisodeHeader> for TraceEvent {
    fn from(value: EpisodeHeader) -> Self {
        Self::EpisodeHeader(value)
    }
}

impl From<Turn> for TraceEvent {
    fn from(value: Turn) -> Self {
        Self::Turn(value)
    }
}

impl From<Tool> for TraceEvent {
    fn from(value: Tool) -> Self {
        Self::Tool(value)
    }
}

impl From<Compaction> for TraceEvent {
    fn from(value: Compaction) -> Self {
        Self::Compaction(value)
    }
}

impl From<EpisodeEnd> for TraceEvent {
    fn from(value: EpisodeEnd) -> Self {
        Self::EpisodeEnd(value)
    }
}

/// The finite set of event kinds in the v1 contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraceEventKind {
    /// Episode header.
    EpisodeHeader,
    /// Model turn.
    Turn,
    /// Tool execution.
    Tool,
    /// Content-free compaction lifecycle record.
    Compaction,
    /// Episode end.
    EpisodeEnd,
}
