use crate::{
    AgentId, AgentSpawnedFact, AgentTaskFinishedFact, ArtifactId, ArtifactPolicyId, CoreRunId,
    Digest, EntryId, EpochId, HarnessCandidateId, HarnessRevisionId, HarnessSnapshotId, LaneId,
    ModelHarnessProfileId, OperationId, ProviderRequestId, RecordId, Sequence, SessionId,
    StableHookId, StepId, SubagentPolicyFact, WorkspaceDeltaAppliedFact, WorkspaceDeltaFact,
};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};
use tea_protocol::JsonValue;

#[cfg(test)]
thread_local! {
    static SESSION_SNAPSHOT_CLONE_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_session_snapshot_clone_count() -> usize {
    SESSION_SNAPSHOT_CLONE_COUNT.with(|count| count.replace(0))
}

/// Stable JSON metadata attached to a durable value.
///
/// Object ordering is deterministic because `JsonValue` itself uses
/// `BTreeMap`; metadata never carries mutable supervisor pointers.
pub type Metadata = BTreeMap<String, JsonValue>;

/// The sole on-disk session format supported by Tea.
pub const SESSION_FORMAT_VERSION: u16 = 1;

/// Header of a JSONL v1 session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionHeader {
    /// The fixed kind discriminator for this format.
    pub kind: String,
    /// Current on-disk session format version.
    pub version: u16,
    /// Durable session identity.
    pub session_id: SessionId,
    /// Time at which the session directory was initialized.
    pub created_at_ms: u64,
    /// Host-selected workspace identity, not a source of filesystem authority.
    pub workspace: String,
    /// Host-defined immutable session metadata.
    pub metadata: Metadata,
    /// The `main` root lane seeded when this multi-lane session is created.
    /// Additional child lanes are appended through durable topology mutations
    /// in that same session.
    pub initial_lane: LaneId,
    /// Integrity identity of this canonical v1 header. The JSONL wire codec
    /// seals it before a header can be persisted or reduced.
    pub digest: Digest,
}

impl SessionHeader {
    /// Construct a v1 session header with the required main lane.
    pub fn new(session_id: SessionId, workspace: impl Into<String>, metadata: Metadata) -> Self {
        Self::new_at(session_id, workspace, metadata, system_time_ms())
    }

    /// Construct a v1 session header at an explicit durable creation time.
    ///
    /// Hosts that need reproducible fixtures should supply a clock value here
    /// and pass the same clock to the session store for commit timestamps.
    pub fn new_at(
        session_id: SessionId,
        workspace: impl Into<String>,
        metadata: Metadata,
        created_at_ms: u64,
    ) -> Self {
        Self {
            kind: "session".into(),
            version: SESSION_FORMAT_VERSION,
            session_id,
            created_at_ms,
            workspace: workspace.into(),
            metadata,
            initial_lane: LaneId::main(),
            digest: Digest::zero(),
        }
    }
}

/// A versioned immutable semantic entry appended to a lane branch.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEntry {
    /// Human or external caller input.
    UserMessage(UserMessageEntry),
    /// One settled model response and its provider tool-call requests.
    AssistantMessage(AssistantMessageEntry),
    /// A durable tool result paired to an assistant tool call.
    ToolResult(ToolResultEntry),
    /// An explicit model-context compaction checkpoint.
    Compaction(CompactionEntry),
    /// A semantic summary attached to a branch.
    BranchSummary(BranchSummaryEntry),
    /// A model descriptor change.
    ModelChanged(ModelChangedEntry),
    /// A thinking-level change.
    ThinkingChanged(ThinkingChangedEntry),
    /// A change in visible or enabled tool names.
    ToolActivationChanged(ToolActivationChangedEntry),
    /// A durable branch-level harness revision transition.
    HarnessRevisionChanged(HarnessRevisionChangedEntry),
    /// Typed plugin-owned memory, validated and persisted by Rust.
    PluginMemory(PluginMemoryEntry),
    /// A trusted host-registered semantic entry unknown to this crate.
    Custom(CustomEntry),
}

impl SessionEntry {
    /// Whether this entry contributes model-visible context by default.
    pub fn is_model_visible(&self) -> bool {
        match self {
            Self::UserMessage(_) | Self::AssistantMessage(_) | Self::ToolResult(_) => true,
            Self::Compaction(_) | Self::BranchSummary(_) => true,
            Self::ModelChanged(_)
            | Self::ThinkingChanged(_)
            | Self::ToolActivationChanged(_)
            | Self::HarnessRevisionChanged(_) => false,
            Self::PluginMemory(entry) => entry.visibility == MemoryVisibility::ModelVisible,
            Self::Custom(entry) => entry.model_visible,
        }
    }

    /// Return immutable artifact objects pinned by this entry.
    pub fn artifact_references(&self) -> Vec<ArtifactId> {
        let mut references = Vec::new();
        match self {
            Self::ToolResult(entry) => entry
                .full_result
                .artifact_id()
                .into_iter()
                .for_each(|id| references.push(id)),
            Self::Compaction(entry) => entry
                .recovery_index_artifact
                .into_iter()
                .for_each(|id| references.push(id)),
            Self::PluginMemory(entry) => entry
                .content
                .artifact_id()
                .into_iter()
                .for_each(|id| references.push(id)),
            Self::Custom(entry) => entry
                .payload
                .artifact_id()
                .into_iter()
                .for_each(|id| references.push(id)),
            Self::UserMessage(_)
            | Self::AssistantMessage(_)
            | Self::BranchSummary(_)
            | Self::ModelChanged(_)
            | Self::ThinkingChanged(_)
            | Self::ToolActivationChanged(_)
            | Self::HarnessRevisionChanged(_) => {}
        }
        references
    }
}

/// Header allocated inside the storage commit for a semantic entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryHeader {
    /// Caller-provisioned immutable entry identity.
    pub id: EntryId,
    /// Storage-derived previous lane leaf.
    pub parent_id: Option<EntryId>,
    /// Storage-assigned, session-global commit sequence.
    pub seq: Sequence,
    /// Storage-assigned commit time.
    pub timestamp_ms: u64,
}

/// One persisted semantic entry together with the lane that advanced to it.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEntry {
    /// Lane whose leaf moved to this entry.
    pub lane_id: LaneId,
    /// Immutable entry header.
    pub header: EntryHeader,
    /// Versioned semantic body.
    pub body: SessionEntry,
}

/// An entry ID and immutable body provisioned before the storage commit.
#[derive(Clone, Debug, PartialEq)]
pub struct ProvisionedEntry {
    /// Caller-provisioned identity, retained across recovery.
    pub id: EntryId,
    /// Immutable semantic content.
    pub body: SessionEntry,
}

impl ProvisionedEntry {
    /// Construct user input with empty host metadata.
    pub fn user(id: EntryId, content: impl Into<String>) -> Self {
        Self {
            id,
            body: SessionEntry::UserMessage(UserMessageEntry {
                content: content.into(),
                metadata: Metadata::new(),
            }),
        }
    }

    /// Construct an assistant entry from text and source-order tool calls.
    pub fn assistant(
        id: EntryId,
        content: impl Into<String>,
        tool_calls: Vec<AssistantToolCall>,
    ) -> Self {
        Self {
            id,
            body: SessionEntry::AssistantMessage(AssistantMessageEntry {
                content: content.into(),
                tool_calls,
                stop_reason: None,
                error_message: None,
                metadata: Metadata::new(),
            }),
        }
    }
}

/// User message semantics.
#[derive(Clone, Debug, PartialEq)]
pub struct UserMessageEntry {
    /// Exact user-visible text.
    pub content: String,
    /// Host-owned stable metadata.
    pub metadata: Metadata,
}

/// Assistant message semantics.
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantMessageEntry {
    /// Settled assistant text.
    pub content: String,
    /// Provider tool calls in their original source order.
    pub tool_calls: Vec<AssistantToolCall>,
    /// Provider stop reason when retained.
    pub stop_reason: Option<String>,
    /// Provider-facing error text when the assistant turn settled as an error.
    pub error_message: Option<String>,
    /// Host-owned stable metadata.
    pub metadata: Metadata,
}

/// One provider-supplied assistant tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantToolCall {
    /// Provider call identity retained exactly for tool-result correlation.
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Validated or original raw arguments; durable recovery validates again.
    pub arguments: JsonValue,
}

impl AssistantToolCall {
    /// Construct one source-order assistant tool call.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: JsonValue) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

/// Complete retained payload or a reference to exact immutable artifact bytes.
#[derive(Clone, Debug, PartialEq)]
pub enum PayloadRef {
    /// Small payload retained directly in the session record.
    Inline(JsonValue),
    /// Exact immutable bytes retained in the content-addressed object store.
    Artifact {
        /// BLAKE3 identity of exact retained bytes.
        artifact_id: ArtifactId,
        /// Exact retained byte length.
        byte_len: u64,
        /// Stable content type for host readers.
        media_type: String,
    },
}

impl PayloadRef {
    /// Return the object identity when this payload lives outside JSONL.
    pub fn artifact_id(&self) -> Option<ArtifactId> {
        match self {
            Self::Inline(_) => None,
            Self::Artifact { artifact_id, .. } => Some(*artifact_id),
        }
    }
}

/// A paired durable tool outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResultEntry {
    /// Provider-supplied tool-call identity.
    pub tool_call_id: String,
    /// Registered tool name at execution time.
    pub tool_name: String,
    /// Complete redacted result retained before projection.
    pub full_result: PayloadRef,
    /// Bounded model-visible result, including any recovery locator.
    pub model_projection: JsonValue,
    /// Whether the tool reported an error.
    pub is_error: bool,
    /// Whether the run must terminate after this result.
    pub terminate: bool,
    /// Nested-model or tool-reported usage, if available.
    pub usage: Usage,
    /// Registered projection strategy identity.
    pub projection_strategy_id: String,
    /// Artifact policy that governed retention/redaction.
    pub artifact_policy_id: ArtifactPolicyId,
}

/// A durable compaction checkpoint. It changes projected context, not history.
#[derive(Clone, Debug, PartialEq)]
pub struct CompactionEntry {
    /// First covered semantic entry, when the source range is nonempty.
    pub covered_from: Option<EntryId>,
    /// Last covered semantic entry, when the source range is nonempty.
    pub covered_to: Option<EntryId>,
    /// First retained tail entry after compaction.
    pub retained_tail_boundary: Option<EntryId>,
    /// Model-visible summary with a stable history-search marker.
    pub summary: String,
    /// Registered compaction strategy identity.
    pub strategy_id: String,
    /// Optional searchable recovery index.
    pub recovery_index_artifact: Option<ArtifactId>,
    /// Harness revision that produced the checkpoint.
    pub harness_revision_id: Option<HarnessRevisionId>,
}

/// A semantic branch summary.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchSummaryEntry {
    /// Summary text.
    pub summary: String,
    /// Optional stable location of the summarized prefix.
    pub covered_to: Option<EntryId>,
}

/// Model selection semantic state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChangedEntry {
    /// Provider family selected by the host.
    pub provider: String,
    /// Requested model identifier.
    pub model: String,
    /// Returned or pinned model revision, if known.
    pub revision: Option<String>,
}

/// Thinking-level semantic state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThinkingChangedEntry {
    /// Stable provider-neutral thinking setting.
    pub level: String,
}

/// Tool activation semantic state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolActivationChangedEntry {
    /// Deterministically ordered active tool names.
    pub active_tool_names: Vec<String>,
}

/// Immutable branch-level harness revision transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessRevisionChangedEntry {
    /// New immutable revision identity.
    pub revision_id: HarnessRevisionId,
    /// Immutable snapshot selected by that revision.
    pub snapshot_id: HarnessSnapshotId,
    /// Whether this is an ordinary rollback transition.
    pub rollback_from: Option<HarnessRevisionId>,
}

/// Visibility class for typed plugin memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryVisibility {
    /// Context policy may serialize this memory to the model.
    ModelVisible,
    /// The memory remains external/queryable only.
    ExternalOnly,
}

/// Retention class for typed plugin memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryRetention {
    /// Keep until ordinary session export/GC roots no longer retain it.
    Session,
    /// Keep only while a host-defined checkpoint retains it.
    Checkpoint,
}

/// Rust-validated plugin memory semantics.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginMemoryEntry {
    /// Plugin identity within the exact snapshotted registry.
    pub plugin_id: String,
    /// Plugin-defined typed memory kind.
    pub kind: String,
    /// Exact retained data or immutable payload object.
    pub content: PayloadRef,
    /// Evidence references supplied by the policy and validated by the host.
    pub provenance: Vec<String>,
    /// Model visibility policy.
    pub visibility: MemoryVisibility,
    /// Retention policy.
    pub retention: MemoryRetention,
}

/// Trusted-host extension point for semantic types not known to this crate.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomEntry {
    /// Namespaced type name, including version where required by the host.
    pub type_name: String,
    /// Exact opaque payload retained by the host.
    pub payload: PayloadRef,
    /// Whether the host registered this type as model-visible.
    pub model_visible: bool,
}

/// Usage remains independent by category; an absent value is unknown, not zero.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    /// Input-token usage.
    pub input_tokens: Option<u64>,
    /// Output-token usage.
    pub output_tokens: Option<u64>,
    /// Reasoning-token usage.
    pub reasoning_tokens: Option<u64>,
    /// Provider cache-read usage.
    pub cache_read_tokens: Option<u64>,
    /// Provider cache-write usage.
    pub cache_write_tokens: Option<u64>,
    /// Exact provider-reported cost text.
    pub cost: Option<String>,
}

impl Usage {
    /// Accumulate known categories without converting unknown measurements to zero. Exact
    /// reported costs are added as decimal text.
    pub fn saturating_add_assign(&mut self, other: &Self) {
        self.input_tokens = add_optional(self.input_tokens, other.input_tokens);
        self.output_tokens = add_optional(self.output_tokens, other.output_tokens);
        self.reasoning_tokens = add_optional(self.reasoning_tokens, other.reasoning_tokens);
        self.cache_read_tokens = add_optional(self.cache_read_tokens, other.cache_read_tokens);
        self.cache_write_tokens = add_optional(self.cache_write_tokens, other.cache_write_tokens);
        if let Some(cost) = other.cost.as_deref() {
            self.cost = Some(match self.cost.as_deref() {
                Some(previous) => decimal_add(previous, cost),
                None => cost.to_owned(),
            });
        }
    }
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn decimal_add(lhs: &str, rhs: &str) -> String {
    let (left_digits, left_scale) = decimal_parts(lhs);
    let (right_digits, right_scale) = decimal_parts(rhs);
    let scale = left_scale.max(right_scale);
    let mut left = left_digits;
    let mut right = right_digits;
    left.extend(std::iter::repeat_n('0', scale - left_scale));
    right.extend(std::iter::repeat_n('0', scale - right_scale));
    let mut output = String::new();
    let mut carry = 0u8;
    let mut left = left.bytes().rev();
    let mut right = right.bytes().rev();
    loop {
        let left = left.next();
        let right = right.next();
        if left.is_none() && right.is_none() {
            break;
        }
        let sum = left.unwrap_or(b'0') - b'0' + right.unwrap_or(b'0') - b'0' + carry;
        output.push(char::from(b'0' + sum % 10));
        carry = sum / 10;
    }
    if carry != 0 {
        output.push(char::from(b'0' + carry));
    }
    let mut output: String = output.chars().rev().collect();
    if scale != 0 {
        if output.len() <= scale {
            let zeros = "0".repeat(scale + 1 - output.len());
            output = format!("{zeros}{output}");
        }
        output.insert(output.len() - scale, '.');
    }
    decimal_normalize(&output)
}

fn decimal_parts(value: &str) -> (String, usize) {
    let (coefficient, exponent) = value
        .split_once(['e', 'E'])
        .map(|(coefficient, exponent)| (coefficient, exponent.parse::<i64>().unwrap_or(0)))
        .unwrap_or((value, 0));
    let (whole, fraction) = coefficient.split_once('.').unwrap_or((coefficient, ""));
    let mut digits = String::with_capacity(whole.len() + fraction.len());
    digits.push_str(whole.trim_start_matches('+'));
    digits.push_str(fraction);
    let scale = (fraction.len() as i64 - exponent).max(0) as usize;
    let mut digits = digits.trim_start_matches('0').to_owned();
    if digits.is_empty() {
        digits.push('0');
    }
    (digits, scale)
}

fn decimal_normalize(value: &str) -> String {
    let (digits, scale) = decimal_parts(value);
    if scale == 0 {
        return digits;
    }
    let mut output = if digits.len() <= scale {
        format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
    } else {
        let position = digits.len() - scale;
        format!("{}.{}", &digits[..position], &digits[position..])
    };
    while output.ends_with('0') {
        output.pop();
    }
    if output.ends_with('.') {
        output.pop();
    }
    output
}

/// One operation record in the lane-owned write-ahead log.
#[derive(Clone, Debug, PartialEq)]
pub enum LaneRecord {
    /// Caller-visible operation acceptance.
    OperationStarted(OperationStartedRecord),
    /// One and only one terminal operation outcome.
    OperationFinished(OperationFinishedRecord),
    /// Durable abort request.
    AbortRequested(AbortRequestedRecord),
    /// Immutable core-run epoch start.
    EpochStarted(EpochStartedRecord),
    /// Immutable core-run epoch finish.
    EpochFinished(EpochFinishedRecord),
    /// Retryable model/compaction step intent.
    StepAttempted(StepAttemptedRecord),
    /// Physical provider-request intent.
    ProviderRequestStarted(ProviderRequestStartedRecord),
    /// Physical provider-request settlement.
    ProviderRequestSettled(ProviderRequestSettledRecord),
    /// Tool-effect intent preceding execution.
    ToolStarted(ToolStartedRecord),
    /// Queue acceptance.
    QueueEnqueued(QueueEnqueuedRecord),
    /// Queue cancellation.
    QueueCancelled(QueueCancelledRecord),
    /// Deferred semantic write.
    WriteDeferred(WriteDeferredRecord),
    /// Candidate activation obligation.
    HarnessActivationRequested(HarnessActivationRequestedRecord),
    /// Usage fact independent from semantic result materialization.
    Usage(UsageRecord),
}

impl LaneRecord {
    /// Construct an operation-start record variant.
    pub fn operation_started(record: OperationStartedRecord) -> Self {
        Self::OperationStarted(record)
    }

    /// Construct a tool-effect intent variant.
    pub fn tool_started(record: ToolStartedRecord) -> Self {
        Self::ToolStarted(record)
    }

    /// Return the operation identity when the record belongs to one.
    pub fn operation_id(&self) -> Option<&OperationId> {
        match self {
            Self::OperationStarted(record) => Some(&record.id),
            Self::OperationFinished(record) => Some(&record.operation_id),
            Self::AbortRequested(record) => Some(&record.operation_id),
            Self::EpochStarted(record) => Some(&record.operation_id),
            Self::EpochFinished(record) => Some(&record.operation_id),
            Self::StepAttempted(record) => Some(&record.operation_id),
            Self::ProviderRequestStarted(record) => Some(&record.operation_id),
            Self::ProviderRequestSettled(record) => Some(&record.operation_id),
            Self::ToolStarted(record) => Some(&record.operation_id),
            Self::QueueEnqueued(record) => Some(&record.operation_id),
            Self::QueueCancelled(record) => Some(&record.operation_id),
            Self::WriteDeferred(record) => Some(&record.operation_id),
            Self::HarnessActivationRequested(record) => Some(&record.operation_id),
            Self::Usage(record) => Some(&record.operation_id),
        }
    }
}

/// A record together with its storage-assigned envelope fields.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredRecord {
    /// Global commit sequence.
    pub seq: Sequence,
    /// Storage-assigned commit time.
    pub timestamp_ms: u64,
    /// Immutable WAL fact.
    pub record: LaneRecord,
}

/// The durable kind of one user-visible operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationKind {
    /// One user task that may span epochs.
    Run,
    /// One child task owned by exactly one root operation.
    Subagent {
        /// Stable child identity that owns this lane-local operation.
        agent_id: AgentId,
        /// Root operation that created and must settle this child.
        parent_operation_id: OperationId,
    },
    /// An explicitly named host-defined operation kind.
    Other(String),
}

/// Durable accepted-operation record.
#[derive(Clone, Debug, PartialEq)]
pub struct OperationStartedRecord {
    /// Durable operation identity.
    pub id: OperationId,
    /// Lane that owns this operation.
    pub lane_id: LaneId,
    /// Lane leaf observed when the operation was accepted.
    pub source_leaf_id: Option<EntryId>,
    /// Operation category.
    pub kind: OperationKind,
    /// Exact provisioned semantic input to append after acceptance.
    pub original_input: Vec<ProvisionedEntry>,
    /// Harness revision captured before the first effect begins.
    pub initial_harness_revision: HarnessRevisionId,
    /// Immutable model-harness profile selected by the host.
    pub model_harness_profile: ModelHarnessProfileId,
    /// Stable hook-ID keyed resume state.
    pub operation_resume_data: BTreeMap<StableHookId, JsonValue>,
}

impl OperationStartedRecord {
    /// Construct an operation start with empty durable resume state.
    pub fn new(
        id: OperationId,
        lane_id: LaneId,
        source_leaf_id: Option<EntryId>,
        kind: OperationKind,
        original_input: Vec<ProvisionedEntry>,
        initial_harness_revision: HarnessRevisionId,
        model_harness_profile: ModelHarnessProfileId,
    ) -> Self {
        Self {
            id,
            lane_id,
            source_leaf_id,
            kind,
            original_input,
            initial_harness_revision,
            model_harness_profile,
            operation_resume_data: BTreeMap::new(),
        }
    }
}

/// Terminal durable operation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationOutcome {
    /// The operation settled normally.
    Completed,
    /// The operation was cancelled by a durable abort request.
    Aborted,
    /// The host stopped with a bounded failure classification.
    Failed { code: String },
}

/// Durable terminal operation record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationFinishedRecord {
    /// Finished operation identity.
    pub operation_id: OperationId,
    /// Unique terminal outcome.
    pub outcome: OperationOutcome,
}

/// Durable abort obligation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbortRequestedRecord {
    /// Target operation.
    pub operation_id: OperationId,
    /// Bounded caller-visible reason.
    pub reason: Option<String>,
}

/// Core-run epoch start.
#[derive(Clone, Debug, PartialEq)]
pub struct EpochStartedRecord {
    /// Epoch identity.
    pub id: EpochId,
    /// Owning durable operation.
    pub operation_id: OperationId,
    /// Zero-based immutable epoch number within the operation.
    pub epoch_index: u32,
    /// Source branch leaf used to derive this run's context.
    pub source_leaf_id: Option<EntryId>,
    /// Exact immutable revision used by this epoch.
    pub harness_revision_id: HarnessRevisionId,
    /// Exact immutable snapshot used by this epoch.
    pub harness_snapshot_id: HarnessSnapshotId,
    /// Exact profile used by this epoch.
    pub model_harness_profile: ModelHarnessProfileId,
    /// Core run identity recorded for trace correlation.
    pub core_run_id: CoreRunId,
    /// Stable hook-ID keyed resume state.
    pub epoch_resume_data: BTreeMap<StableHookId, JsonValue>,
}

/// Core-run epoch terminal result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochFinishedRecord {
    /// Finished epoch.
    pub epoch_id: EpochId,
    /// Owning operation.
    pub operation_id: OperationId,
    /// Whether the epoch settled before a rollover.
    pub reason: EpochFinishReason,
}

/// Epoch settlement reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EpochFinishReason {
    /// Normal agent settlement.
    Settled,
    /// Settled specifically to permit a durable harness activation.
    ActivationPending,
    /// Abort or terminal failure.
    Interrupted,
}

/// A retryable assistant or compaction attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StepAttemptedRecord {
    /// Step identity.
    pub id: StepId,
    /// Owning operation.
    pub operation_id: OperationId,
    /// Owning epoch.
    pub epoch_id: EpochId,
    /// Step category.
    pub kind: StepKind,
    /// Durable attempt count.
    pub attempt: u32,
    /// Provisioned semantic result identity.
    pub result_entry_id: EntryId,
    /// Optional cause for a retry.
    pub reason: Option<String>,
}

/// Retryable step class.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StepKind {
    /// One assistant/provider turn.
    Assistant,
    /// One transactional compaction attempt.
    Compaction,
}

/// Provider request intent, written before network dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRequestStartedRecord {
    /// Physical request identity.
    pub request_id: ProviderRequestId,
    /// Owning operation.
    pub operation_id: OperationId,
    /// Owning epoch.
    pub epoch_id: EpochId,
    /// Retryable logical step.
    pub step_id: StepId,
    /// Physical attempt number.
    pub physical_attempt: u32,
    /// Profile used to compose this request.
    pub model_harness_profile: ModelHarnessProfileId,
    /// Exact model-visible request surface digest.
    pub request_surface_digest: Digest,
    /// Provider idempotency key where supported.
    pub idempotency_key: Option<String>,
}

/// Provider settlement classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderSettlementClassification {
    /// A settled response is available.
    Completed,
    /// The transport may be retried according to host policy.
    Retryable,
    /// The response was discarded after accounting (for example overflow).
    Discarded,
    /// Recovery must reconcile ambiguous request state with the provider.
    Interrupted,
}

/// Provider request terminal fact.
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRequestSettledRecord {
    /// Settled physical request identity.
    pub request_id: ProviderRequestId,
    /// Owning operation, retained to make malformed cross-operation records detectable.
    pub operation_id: OperationId,
    /// Outcome encoded by the host/provider boundary.
    pub outcome: JsonValue,
    /// Structured provider failure diagnostics, when the request did not complete normally.
    ///
    /// This is deliberately separate from `outcome`: the outcome remains a provider-defined
    /// semantic projection while this record carries bounded, typed transport evidence.
    pub provider_error: Option<ProviderErrorRecord>,
    /// Usage reported even when the semantic result never materializes.
    pub usage: Option<Usage>,
    /// Redacted raw response object when retained.
    pub response_artifact: Option<ArtifactId>,
    /// Settlement classification.
    pub classification: ProviderSettlementClassification,
}

/// Bounded, redacted evidence for a provider request failure.
///
/// Providers must redact credentials and cap response text before constructing this value.
/// The session layer does not retain arbitrary remote bodies implicitly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderErrorRecord {
    /// Failure boundary (`adapter`, `transport`, or `response`).
    pub source: String,
    /// Stable local diagnostic, when available.
    pub message: Option<String>,
    /// HTTP status observed from the provider, when available.
    pub status_code: Option<u16>,
    /// One-based transport attempt, or zero for an adapter-side rejection.
    pub attempt: Option<u32>,
    /// Provider error type, when reported.
    pub error_type: Option<String>,
    /// Provider error code, when reported.
    pub error_code: Option<String>,
    /// Whether retry policy classified this failure as retryable.
    pub retryable: Option<bool>,
    /// Number of response bytes captured before parsing.
    pub response_bytes: Option<u64>,
    /// Number of request payload bytes sent to the provider.
    pub request_bytes: Option<u64>,
    /// Redacted bounded response body prefix.
    pub response_body: Option<String>,
}

/// Whether a durable tool intent may be replayed after a crash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolReplayPolicy {
    /// The effect is ambiguous after a crash and must never be replayed.
    Never,
    /// The host has explicitly proven the effect replay-safe.
    Safe,
}

/// Tool-effect intent, committed after policy/argument validation and before effect execution.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolStartedRecord {
    /// Record identity for direct lookup and external trace attribution.
    pub record_id: RecordId,
    /// Owning operation.
    pub operation_id: OperationId,
    /// Owning epoch.
    pub epoch_id: EpochId,
    /// Assistant entry whose source-order call this settles.
    pub assistant_entry_id: EntryId,
    /// Source-order tool ordinal inside that assistant entry.
    pub tool_index: u32,
    /// Provider call identity at the validated source position.
    pub tool_call_id: String,
    /// Tool name at the validated source position.
    pub tool_name: String,
    /// Final schema-valid arguments after `before_tool` policy.
    ///
    /// Optional tool defaults are not necessarily expanded here. For example,
    /// an omitted `spawn_agent.thinking` remains a durable inheritance request.
    pub effective_args: JsonValue,
    /// Provisioned tool-result entry identity.
    pub result_entry_id: EntryId,
    /// Maximum host-authorized replay policy at effect start.
    pub replay_policy_at_start: ToolReplayPolicy,
    /// Exact definition identity used when policy was evaluated.
    pub tool_definition_digest: Digest,
    /// Exact harness revision at effect start.
    pub harness_revision_id: HarnessRevisionId,
    /// Durable per-invocation idempotency key.
    pub idempotency_key: String,
}

impl ToolStartedRecord {
    /// Construct a complete tool-effect intent.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        record_id: RecordId,
        operation_id: OperationId,
        epoch_id: EpochId,
        assistant_entry_id: EntryId,
        tool_index: u32,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        effective_args: JsonValue,
        result_entry_id: EntryId,
        replay_policy_at_start: ToolReplayPolicy,
        tool_definition_digest: Digest,
        harness_revision_id: HarnessRevisionId,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            record_id,
            operation_id,
            epoch_id,
            assistant_entry_id,
            tool_index,
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            effective_args,
            result_entry_id,
            replay_policy_at_start,
            tool_definition_digest,
            harness_revision_id,
            idempotency_key: idempotency_key.into(),
        }
    }
}

/// Queue acceptance fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueEnqueuedRecord {
    /// Owning operation.
    pub operation_id: OperationId,
    /// Host-provisioned queue item identity.
    pub queue_item_id: String,
}

/// Queue cancellation fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueCancelledRecord {
    /// Owning operation.
    pub operation_id: OperationId,
    /// Existing accepted queue item identity.
    pub queue_item_id: String,
}

/// Deferred semantic write fact.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteDeferredRecord {
    /// Owning operation.
    pub operation_id: OperationId,
    /// Provisioned entry to append at the documented checkpoint.
    pub entry: ProvisionedEntry,
}

/// Durable immutable harness activation obligation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessActivationRequestedRecord {
    /// Owning operation.
    pub operation_id: OperationId,
    /// Candidate that was structurally validated before scheduling.
    pub candidate_id: HarnessCandidateId,
    /// Revision active when the request was made.
    pub parent_revision_id: HarnessRevisionId,
    /// Proposed immutable snapshot.
    pub proposed_snapshot_id: HarnessSnapshotId,
    /// Provisioned semantic entry to use for the resulting transition.
    pub revision_entry_id: EntryId,
}

/// Durable usage fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageRecord {
    /// Owning operation.
    pub operation_id: OperationId,
    /// Associated physical request, when applicable.
    pub request_id: Option<ProviderRequestId>,
    /// Measured usage.
    pub usage: Usage,
}

/// Explicit lane topology mutation. Main is seeded by the header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaneMutation {
    /// Create a future lane rooted at an existing immutable branch entry.
    Created {
        /// New lane identity.
        lane_id: LaneId,
        /// Shared branch root, if any.
        base_leaf_id: Option<EntryId>,
    },
}

/// A lane mutation together with its storage envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredLaneMutation {
    /// Global commit sequence.
    pub seq: Sequence,
    /// Storage-assigned commit time.
    pub timestamp_ms: u64,
    /// Immutable topology fact.
    pub mutation: LaneMutation,
}

/// Session-wide fact that is not semantic context or an operation fact.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionFact {
    /// Immutable closed child-model policy for this optional session feature.
    SubagentPolicy(SubagentPolicyFact),
    /// Durable parent-to-child graph linkage and child configuration.
    AgentSpawned(AgentSpawnedFact),
    /// Immutable isolated child workspace result and binary patch artifact.
    WorkspaceDelta(WorkspaceDeltaFact),
    /// Durable child terminal report after its operation completed.
    AgentTaskFinished(AgentTaskFinishedFact),
    /// Proven parent application of one immutable child workspace delta.
    WorkspaceDeltaApplied(WorkspaceDeltaAppliedFact),
    /// Immutable harness-catalog manifest retained outside model context.
    ///
    /// The catalog itself is an exact content-addressed JSON object in the
    /// session artifact store. Keeping only its descriptor in JSONL lets a
    /// reopened durable harness reconstruct trees, snapshots, revisions, and
    /// rejected candidates without treating a mutable worktree as authority.
    HarnessCatalog(HarnessCatalogFact),
    /// Content-free structured evidence for a model-emitted tool-call schema
    /// rejection. The exact arguments live only in the referenced immutable
    /// artifact and never enter model context or telemetry.
    ToolSchemaDeviation(ToolSchemaDeviationFact),
    /// One immutable, redacted core-run trace retained outside model context.
    TraceArtifact(TraceArtifactFact),
    /// A durable host-defined fact with no model projection.
    Custom {
        type_name: String,
        payload: JsonValue,
    },
}

/// One committed immutable harness-catalog manifest reference.
///
/// Catalog objects are append-only snapshots of the harness repository. A
/// later fact supersedes an earlier catalog for reconstruction, while every
/// earlier object remains reachable for verification and export.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessCatalogFact {
    /// Version of the catalog JSON schema.
    pub schema_version: u16,
    /// Content-addressed manifest bytes.
    pub artifact_id: ArtifactId,
    /// Exact manifest byte length, checked during restore.
    pub byte_len: u64,
}

/// One field whose supplied JSON kind did not match a tool's declared schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaFieldMismatch {
    /// JSON-pointer-like location relative to the tool argument object.
    pub field: String,
    /// Declared JSON kind.
    pub expected: String,
    /// Supplied JSON kind.
    pub actual: String,
}

/// Durable evidence for an invalid model tool-call argument object.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSchemaDeviationFact {
    /// Operation that received the invalid call.
    pub operation_id: OperationId,
    /// Immutable epoch/profile that exposed the tool schema.
    pub epoch_id: EpochId,
    /// Persisted assistant message that contained the call.
    pub assistant_entry_id: EntryId,
    /// Provider-supplied tool-call identity.
    pub tool_call_id: String,
    /// Registered tool name checked by the core.
    pub tool_name: String,
    /// Exact immutable model-harness profile selected for the epoch.
    pub model_harness_profile: ModelHarnessProfileId,
    /// Whether the raw argument bytes decoded as protocol JSON.
    pub arguments_valid_json: bool,
    /// Closed-schema field names not declared by the tool.
    pub unknown_fields: Vec<String>,
    /// Required fields omitted by the call.
    pub missing_fields: Vec<String>,
    /// Visible supplied/declared JSON-kind mismatches.
    pub type_mismatches: Vec<SchemaFieldMismatch>,
    /// Exact model-emitted arguments retained outside model context.
    pub raw_arguments: PayloadRef,
}

/// Durable descriptor for one redacted trace captured from a completed core
/// run. The session WAL remains authoritative for recovery; this fact pins a
/// queryable evidence artifact without putting trace content into model
/// context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraceArtifactFact {
    /// Trace wire-schema version.
    pub schema_version: u16,
    /// Operation that owned the traced core run.
    pub operation_id: OperationId,
    /// Epoch that allocated the durable core-run identity.
    pub epoch_id: EpochId,
    /// Durable core-run identity selected before execution began.
    pub core_run_id: CoreRunId,
    /// Immutable harness revision used by the run.
    pub harness_revision_id: HarnessRevisionId,
    /// Immutable harness snapshot used by the run.
    pub harness_snapshot_id: HarnessSnapshotId,
    /// Immutable model-harness profile used by the run.
    pub model_harness_profile: ModelHarnessProfileId,
    /// Exact redacted JSON Lines trace bytes.
    pub artifact_id: ArtifactId,
    /// Exact trace artifact byte length.
    pub byte_len: u64,
    /// Stable trace artifact media type.
    pub media_type: String,
}

impl SessionFact {
    /// Return immutable artifact objects pinned by this session-wide fact.
    pub fn artifact_references(&self) -> Vec<ArtifactId> {
        match self {
            Self::SubagentPolicy(_) | Self::AgentSpawned(_) | Self::WorkspaceDeltaApplied(_) => {
                Vec::new()
            }
            Self::WorkspaceDelta(fact) => fact.patch.artifact_id().into_iter().collect(),
            Self::AgentTaskFinished(fact) => fact.report.artifact_id().into_iter().collect(),
            Self::HarnessCatalog(fact) => vec![fact.artifact_id],
            Self::ToolSchemaDeviation(fact) => {
                fact.raw_arguments.artifact_id().into_iter().collect()
            }
            Self::TraceArtifact(fact) => vec![fact.artifact_id],
            Self::Custom { .. } => Vec::new(),
        }
    }
}

/// A session fact together with its storage envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredFact {
    /// Global commit sequence.
    pub seq: Sequence,
    /// Storage-assigned commit time.
    pub timestamp_ms: u64,
    /// Immutable fact body.
    pub fact: SessionFact,
}

/// The narrow semantic mutation taxonomy interpreted by the pure reducer.
///
/// Sequence, timestamp, and integrity fields live only in `StoredMutation`'s
/// durable envelope. The payload types retain convenient materialized values
/// for existing domain consumers, but the JSONL wire codec never serializes
/// those copies independently.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionMutation {
    /// A semantic branch append.
    Entry(StoredEntry),
    /// An operation WAL fact.
    Record(StoredRecord),
    /// A lane-topology fact.
    Lane(StoredLaneMutation),
    /// A session-wide non-semantic fact.
    Fact(StoredFact),
}

/// One committed JSONL v1 mutation and its integrity envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredMutation {
    /// Session-global commit sequence, allocated by the store.
    pub seq: Sequence,
    /// Store-owned wall-clock sample captured once for this commit.
    pub timestamp_ms: u64,
    /// Digest of the preceding committed prefix.
    pub prev_digest: Digest,
    /// Domain-separated digest of this committed record.
    pub digest: Digest,
    /// The one semantic mutation represented by this line.
    pub mutation: SessionMutation,
}

impl StoredMutation {
    /// Return the session-global commit sequence.
    pub fn sequence(&self) -> Sequence {
        self.seq
    }

    /// Borrow this wire-boundary mutation without cloning its semantic payload.
    ///
    /// The pure reducer uses this while validating one prospective append so
    /// a store need not clone the complete retained session merely to check a
    /// transition before its single durable write.
    pub(crate) fn borrowed(&self) -> StoredMutationRef<'_> {
        let mutation = match &self.mutation {
            SessionMutation::Entry(entry) => SessionMutationRef::Entry(entry),
            SessionMutation::Record(record) => SessionMutationRef::Record(record),
            SessionMutation::Lane(lane) => SessionMutationRef::Lane(lane),
            SessionMutation::Fact(fact) => SessionMutationRef::Fact(fact),
        };
        StoredMutationRef {
            seq: self.seq,
            timestamp_ms: self.timestamp_ms,
            prev_digest: self.prev_digest,
            digest: self.digest,
            mutation,
        }
    }
}

/// Borrowed semantic payload for one mutation in a replayed snapshot.
///
/// `SessionSnapshot` owns each payload once in its typed entry, record, lane,
/// or fact view. Ordered replay borrows that same payload through this enum so
/// retaining an ordered envelope index does not duplicate potentially large
/// message, tool-result, or custom-entry bytes in memory.
#[derive(Clone, Copy, Debug)]
pub enum SessionMutationRef<'a> {
    /// A semantic branch append.
    Entry(&'a StoredEntry),
    /// An operation WAL fact.
    Record(&'a StoredRecord),
    /// A lane-topology fact.
    Lane(&'a StoredLaneMutation),
    /// A session-wide non-semantic fact.
    Fact(&'a StoredFact),
}

/// Borrowed envelope and semantic payload in exact durable commit order.
///
/// This is the replay view of a `StoredMutation`; stores still construct and
/// persist owned `StoredMutation` values at the wire boundary.
#[derive(Clone, Copy, Debug)]
pub struct StoredMutationRef<'a> {
    /// Session-global sequence allocated by the store.
    pub seq: Sequence,
    /// Store-owned timestamp retained by the committed envelope.
    pub timestamp_ms: u64,
    /// Digest naming the preceding committed prefix.
    pub prev_digest: Digest,
    /// Digest naming this committed prefix.
    pub digest: Digest,
    /// Borrowed semantic payload retained by the typed snapshot view.
    pub mutation: SessionMutationRef<'a>,
}

impl StoredMutationRef<'_> {
    /// Return the session-global commit sequence.
    pub fn sequence(&self) -> Sequence {
        self.seq
    }
}

#[derive(Clone, Debug, PartialEq)]
enum MutationLocation {
    Entry(usize),
    Record(usize),
    Lane(usize),
    Fact(usize),
}

#[derive(Clone, Debug, PartialEq)]
struct StoredMutationEnvelope {
    seq: Sequence,
    timestamp_ms: u64,
    prev_digest: Digest,
    digest: Digest,
    location: MutationLocation,
}

/// Fully replayed durable session state. It is a snapshot, not a mutable live pointer.
#[derive(Debug, PartialEq)]
pub struct SessionSnapshot {
    header: SessionHeader,
    entries: Vec<StoredEntry>,
    records: Vec<StoredRecord>,
    lane_mutations: Vec<StoredLaneMutation>,
    facts: Vec<StoredFact>,
    timeline: Vec<StoredMutationEnvelope>,
    last_sequence: Sequence,
    last_digest: Digest,
    main_harness_revision: Option<HarnessRevisionId>,
}

impl Clone for SessionSnapshot {
    fn clone(&self) -> Self {
        #[cfg(test)]
        SESSION_SNAPSHOT_CLONE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
        Self {
            header: self.header.clone(),
            entries: self.entries.clone(),
            records: self.records.clone(),
            lane_mutations: self.lane_mutations.clone(),
            facts: self.facts.clone(),
            timeline: self.timeline.clone(),
            last_sequence: self.last_sequence,
            last_digest: self.last_digest,
            main_harness_revision: self.main_harness_revision.clone(),
        }
    }
}

impl SessionSnapshot {
    /// Create an empty snapshot seeded with the header's initial lane.
    pub(crate) fn empty(header: SessionHeader) -> Self {
        let last_digest = header.digest;
        Self {
            header,
            entries: Vec::new(),
            records: Vec::new(),
            lane_mutations: Vec::new(),
            facts: Vec::new(),
            timeline: Vec::new(),
            last_sequence: Sequence(0),
            last_digest,
            main_harness_revision: None,
        }
    }

    /// Borrow immutable session header data.
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Borrow entries in global durable commit order.
    pub fn entries(&self) -> &[StoredEntry] {
        &self.entries
    }

    /// Borrow operation records in global durable commit order.
    pub fn records(&self) -> &[StoredRecord] {
        &self.records
    }

    /// Borrow lane mutations in global durable commit order.
    pub fn lane_mutations(&self) -> &[StoredLaneMutation] {
        &self.lane_mutations
    }

    /// Borrow global facts in global durable commit order.
    pub fn facts(&self) -> &[StoredFact] {
        &self.facts
    }

    /// Iterate over every mutation in exact session-global commit order.
    ///
    /// The returned envelopes borrow the matching typed view rather than
    /// retaining another owned copy of its semantic payload.
    pub fn mutations(&self) -> impl ExactSizeIterator<Item = StoredMutationRef<'_>> + '_ {
        self.timeline
            .iter()
            .map(|envelope| self.borrow_mutation(envelope))
    }

    /// Return the next sequence storage will allocate inside a successful commit.
    pub fn next_sequence(&self) -> Sequence {
        Sequence(self.last_sequence.0.saturating_add(1))
    }

    /// Return the last successfully committed sequence.
    pub fn last_sequence(&self) -> Sequence {
        self.last_sequence
    }

    /// Return the digest identifying the currently committed log prefix.
    pub fn last_digest(&self) -> Digest {
        self.last_digest
    }

    /// Return the revision last selected for the initial lane, if one exists.
    ///
    /// This is a derived index over committed entries. It exists so disposable
    /// session caches can be refreshed without replaying the complete log.
    pub fn active_main_harness_revision(&self) -> Option<&HarnessRevisionId> {
        self.main_harness_revision.as_ref()
    }

    pub(crate) fn push_mutation(&mut self, stored: StoredMutation) {
        self.last_sequence = stored.seq;
        self.last_digest = stored.digest;
        let location = match stored.mutation {
            SessionMutation::Entry(entry) => {
                if entry.lane_id == LaneId::main()
                    && let SessionEntry::HarnessRevisionChanged(revision) = &entry.body
                {
                    self.main_harness_revision = Some(revision.revision_id.clone());
                }
                self.entries.push(entry);
                MutationLocation::Entry(self.entries.len().saturating_sub(1))
            }
            SessionMutation::Record(record) => {
                self.records.push(record);
                MutationLocation::Record(self.records.len().saturating_sub(1))
            }
            SessionMutation::Lane(mutation) => {
                self.lane_mutations.push(mutation);
                MutationLocation::Lane(self.lane_mutations.len().saturating_sub(1))
            }
            SessionMutation::Fact(fact) => {
                self.facts.push(fact);
                MutationLocation::Fact(self.facts.len().saturating_sub(1))
            }
        };
        self.timeline.push(StoredMutationEnvelope {
            seq: stored.seq,
            timestamp_ms: stored.timestamp_ms,
            prev_digest: stored.prev_digest,
            digest: stored.digest,
            location,
        });
    }

    fn borrow_mutation(&self, envelope: &StoredMutationEnvelope) -> StoredMutationRef<'_> {
        let mutation = match envelope.location {
            MutationLocation::Entry(index) => SessionMutationRef::Entry(&self.entries[index]),
            MutationLocation::Record(index) => SessionMutationRef::Record(&self.records[index]),
            MutationLocation::Lane(index) => SessionMutationRef::Lane(&self.lane_mutations[index]),
            MutationLocation::Fact(index) => SessionMutationRef::Fact(&self.facts[index]),
        };
        StoredMutationRef {
            seq: envelope.seq,
            timestamp_ms: envelope.timestamp_ms,
            prev_digest: envelope.prev_digest,
            digest: envelope.digest,
            mutation,
        }
    }
}

/// Derived state for a lane. Pending state is reduced, never independently authoritative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaneState {
    /// Stable lane identity.
    pub lane_id: LaneId,
    /// Current semantic branch leaf.
    pub leaf_id: Option<EntryId>,
    /// Runtime-independent lane status.
    pub status: LaneStatus,
    /// At most one durable open operation.
    pub active_operation: Option<OperationId>,
    /// The active revision derived from semantic history.
    pub active_harness_revision: Option<HarnessRevisionId>,
}

/// Reduced lane status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaneStatus {
    /// No accepted operation remains open.
    Idle,
    /// One accepted operation remains open.
    Running,
    /// A caller may only reopen after repair; no new effects may start.
    Faulted { reason: String },
}

/// Reduced model/tool/harness configuration contributions.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct EffectiveLaneConfiguration {
    /// Most recent selected model descriptor.
    pub model: Option<ModelChangedEntry>,
    /// Most recent selected thinking level.
    pub thinking_level: Option<String>,
    /// Most recent deterministic active tool names.
    pub active_tool_names: Vec<String>,
    /// Branch-derived active harness revision.
    pub harness_revision: Option<HarnessRevisionId>,
}

/// Reduced accepted queue state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PendingQueues {
    /// Accepted but not cancelled queue item IDs by operation.
    pub items: BTreeMap<OperationId, BTreeSet<String>>,
}

/// A deferred semantic append not yet materialized.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingWrite {
    /// Owning operation.
    pub operation_id: OperationId,
    /// Provisioned semantic append.
    pub entry: ProvisionedEntry,
}

/// An activation obligation not yet represented by a branch semantic revision entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingHarnessActivation {
    /// Source durable record.
    pub request: HarnessActivationRequestedRecord,
}

fn system_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
