//! Durable subagent facts and their pure graph reduction.
//!
//! The graph is deliberately a projection of the sealed session timeline. It
//! never stores mutable coordinator state, workspace paths, task handles, or
//! provider credentials. That keeps every recovery decision reproducible from
//! the session prefix plus host-owned workspace recovery ports.

use crate::{
    AgentId, Corruption, EntryId, HarnessRevisionId, HarnessSnapshotId, LaneId,
    LaneRecord, ModelChangedEntry, ModelHarnessProfileId, OperationId, OperationKind,
    OperationOutcome, PayloadRef, SessionEntry, SessionMutationRef, SessionSnapshot,
    SessionHeader, WorkspaceDeltaId, WorkspaceLeaseId, derive_subagent_operation_id,
};
use std::collections::{BTreeMap, BTreeSet};

const SUBAGENT_POLICY_SCHEMA_VERSION: u16 = 1;
const MAX_MODEL_IDENTIFIER_BYTES: usize = 512;
const MAX_DISPLAY_NAME_BYTES: usize = 256;
const MAX_TASK_NAME_BYTES: usize = 64;
const MAX_THINKING_BYTES: usize = 32;
const MAX_COMMIT_BYTES: usize = 200;
const MAX_CHANGED_PATH_BYTES: usize = 4_096;
const MAX_INLINE_REPORT_BYTES: usize = 32 * 1024;

/// The context branch a child receives at spawn time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentContextMode {
    /// Start with the stable child prompt and explicit assignment only.
    Task,
    /// Fork the parent semantic source leaf selected for the spawning tool.
    Parent,
}

/// One host-authorized child model retained in the immutable session policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentModelRecord {
    /// Provider family selected by the host.
    pub provider: String,
    /// Exact provider-local model identifier.
    pub model: String,
    /// Pinned provider revision when the host has one.
    pub revision: Option<String>,
    /// Stable display label selected by the host at session creation.
    pub display_name: String,
    /// Known context capacity when the host has one.
    pub context_window: Option<u64>,
}

impl SubagentModelRecord {
    fn validate(&self) -> Result<(), Corruption> {
        validate_bounded_nonempty("subagent model provider", &self.provider, MAX_MODEL_IDENTIFIER_BYTES)?;
        validate_bounded_nonempty("subagent model identifier", &self.model, MAX_MODEL_IDENTIFIER_BYTES)?;
        if let Some(revision) = &self.revision {
            validate_bounded_nonempty(
                "subagent model revision",
                revision,
                MAX_MODEL_IDENTIFIER_BYTES,
            )?;
        }
        validate_bounded_nonempty(
            "subagent model display name",
            &self.display_name,
            MAX_DISPLAY_NAME_BYTES,
        )?;
        if self.context_window == Some(0) {
            return Err(Corruption::new(
                "subagent model context window must be nonzero when known",
            ));
        }
        Ok(())
    }
}

/// Immutable policy that fixed the root collaboration tool surface at session
/// creation. Absence of this fact means the session has no subagent feature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentPolicyFact {
    /// Version of this policy payload, independent from the JSONL v1 envelope.
    pub schema_version: u16,
    /// One provider family and ordered closed model catalog.
    pub models: Vec<SubagentModelRecord>,
    /// Simultaneously active child-operation ceiling, excluding the root.
    pub max_concurrent: u32,
    /// Total children a root operation may create.
    pub max_total_per_operation: u32,
    /// Per-child host timeout in milliseconds.
    pub timeout_ms: u64,
    /// Exact root subagent-tool schema digest.
    pub tool_surface_digest: crate::Digest,
}

impl SubagentPolicyFact {
    /// Validate the immutable v1 child-model domain and operational limits.
    pub fn validate(&self) -> Result<(), Corruption> {
        if self.schema_version != SUBAGENT_POLICY_SCHEMA_VERSION {
            return Err(Corruption::new(format!(
                "unsupported subagent policy schema version {}",
                self.schema_version
            )));
        }
        if self.models.is_empty() {
            return Err(Corruption::new(
                "subagent policy must retain a nonempty model catalog",
            ));
        }
        let mut identities = BTreeSet::new();
        let mut model_ids = BTreeSet::new();
        let mut provider = None;
        for model in &self.models {
            model.validate()?;
            if !identities.insert((
                model.provider.clone(),
                model.model.clone(),
                model.revision.clone(),
            )) {
                return Err(Corruption::new(format!(
            "subagent policy repeats model {}/{}",
                    model.provider, model.model
                )));
            }
            // `spawn_agent` accepts the provider-local model ID, not a
            // revision selector. Retaining two revisions under that one
            // closed enum value would make durable replay choose one
            // arbitrarily even though each full descriptor is distinct.
            if !model_ids.insert(model.model.clone()) {
                return Err(Corruption::new(format!(
                    "subagent policy repeats model identifier {}",
                    model.model
                )));
            }
            match &provider {
                Some(expected) if expected != &model.provider => {
                    return Err(Corruption::new(
                        "subagent policy must retain exactly one provider family",
                    ));
                }
                None => provider = Some(model.provider.clone()),
                Some(_) => {}
            }
        }
        if !(1..=16).contains(&self.max_concurrent) {
            return Err(Corruption::new(
                "subagent policy max_concurrent must be within 1..=16",
            ));
        }
        if self.max_total_per_operation < self.max_concurrent
            || self.max_total_per_operation > 64
        {
            return Err(Corruption::new(
                "subagent policy max_total_per_operation must be within max_concurrent..=64",
            ));
        }
        if !(30_000..=7_200_000).contains(&self.timeout_ms) {
            return Err(Corruption::new(
                "subagent policy timeout_ms must be within 30000..=7200000",
            ));
        }
        Ok(())
    }
}

/// Durable graph linkage and immutable child configuration accepted by one
/// parent `spawn_agent` tool effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentSpawnedFact {
    /// Deterministic identity of this child.
    pub agent_id: AgentId,
    /// Lane and root operation that own the child.
    pub parent_lane_id: LaneId,
    pub parent_operation_id: OperationId,
    /// The unique durable child lane.
    pub lane_id: LaneId,
    /// Stable name unique within the owning root operation.
    pub task_name: String,
    /// Model selected from the persisted policy.
    pub model: SubagentModelRecord,
    /// Persisted child thinking setting.
    pub thinking: String,
    /// Whether the child starts clean or from the parent source leaf.
    pub context_mode: AgentContextMode,
    /// Parent semantic leaf retained by a parent-context child.
    pub base_leaf_id: Option<EntryId>,
    /// Host workspace lease identity derived from this agent.
    pub workspace_lease_id: WorkspaceLeaseId,
    /// Immutable child harness identity prepared before the child runs.
    pub harness_revision_id: HarnessRevisionId,
    pub harness_snapshot_id: HarnessSnapshotId,
    pub model_harness_profile_id: ModelHarnessProfileId,
    /// Provider tool-call identifier correlated with the parent tool intent.
    pub spawn_tool_call_id: String,
}

/// Immutable Git result retained before its operational worktree is removed.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceDeltaFact {
    /// Deterministic identity derived from the lease and Git result.
    pub delta_id: WorkspaceDeltaId,
    /// Child that owns the isolated result.
    pub agent_id: AgentId,
    /// Workspace lease from which the result was finalized.
    pub workspace_lease_id: WorkspaceLeaseId,
    /// Synthetic Git commits naming exact before and after trees.
    pub base_commit: String,
    pub result_commit: String,
    /// Deterministically ordered normalized repository-relative paths.
    pub changed_paths: Vec<String>,
    /// The binary patch object. Patches are always immutable artifacts.
    pub patch: PayloadRef,
}

/// Durable child terminal report and optional isolated workspace result.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentTaskFinishedFact {
    /// Child whose operation settled.
    pub agent_id: AgentId,
    /// Child operation that reached its terminal WAL record.
    pub operation_id: OperationId,
    /// Same terminal outcome as the child operation record.
    pub outcome: OperationOutcome,
    /// The child's settled final assistant entry, when one was materialized.
    pub final_entry_id: Option<EntryId>,
    /// Final report inline or in an immutable artifact.
    pub report: PayloadRef,
    /// Isolated workspace delta, when the child changed files.
    pub workspace_delta_id: Option<WorkspaceDeltaId>,
}

/// Proven application of one immutable child workspace delta to its parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceDeltaAppliedFact {
    /// Applied isolated child result.
    pub delta_id: WorkspaceDeltaId,
    /// Parent lane that received the committed change.
    pub target_lane_id: LaneId,
    /// Parent `apply_agent_changes` provider tool-call identifier.
    pub tool_call_id: String,
    /// Exact applied paths, retained independently for a concise result.
    pub changed_paths: Vec<String>,
}

/// Durable graph state derived solely from committed facts and operation WAL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentState {
    /// Workspace and lane are durable, but the child operation has not yet
    /// been accepted. This is a valid spawn-recovery prefix.
    Spawned,
    /// The child operation remains open.
    Running,
    /// The child operation is terminal but result finalization is incomplete.
    Finalizing { outcome: OperationOutcome },
    /// A terminal report exists with no workspace delta.
    Completed { outcome: OperationOutcome },
    /// A terminal report records an interrupted child operation.
    Interrupted,
    /// A terminal report records a failed child operation.
    Failed { code: String },
    /// A terminal report and durable delta exist.
    DeltaReady { outcome: OperationOutcome, delta_id: WorkspaceDeltaId },
    /// The parent durably committed application of the child delta.
    Applied { outcome: OperationOutcome, delta_id: WorkspaceDeltaId },
}

/// One child node in the reduced session-owned graph.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentGraphNode {
    /// Immutable spawn linkage and child configuration.
    pub spawned: AgentSpawnedFact,
    /// Child operation, once the spawn transaction reached acceptance.
    pub operation_id: Option<OperationId>,
    /// Durable child terminal result, once finalization completed.
    pub terminal: Option<AgentTaskFinishedFact>,
    /// Durable isolated workspace result, if one exists.
    pub workspace_delta: Option<WorkspaceDeltaFact>,
    /// Durable parent application, if one exists.
    pub applied: Option<WorkspaceDeltaAppliedFact>,
    /// Current derived state.
    pub state: AgentState,
}

/// Pure reduction of the optional subagent graph embedded in one session.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentGraphReduction {
    /// The immutable subagent policy, or `None` for a session without the
    /// optional feature.
    pub policy: Option<SubagentPolicyFact>,
    /// Nodes keyed by their stable child identity.
    pub agents: BTreeMap<AgentId, AgentGraphNode>,
}

#[derive(Clone)]
struct OperationInfo {
    lane_id: LaneId,
    kind: OperationKind,
    original_input: Vec<crate::ProvisionedEntry>,
    initial_harness_revision: HarnessRevisionId,
    model_harness_profile: ModelHarnessProfileId,
    started_seq: crate::Sequence,
    finished: Option<(crate::Sequence, OperationOutcome)>,
}

#[derive(Clone)]
struct ToolInfo {
    operation_id: OperationId,
    epoch_id: crate::EpochId,
    tool_name: String,
    idempotency_key: String,
    sequence: crate::Sequence,
}

/// Reduce and validate the durable child-agent graph.
///
/// This function accepts every intended crash prefix. In particular, a
/// spawned child may lack an accepted operation, a finished operation may
/// lack a delta, and a delta may lack its final report. It rejects only graph
/// facts that cannot become valid by appending the documented next records.
pub fn reduce_agent_graph(snapshot: &SessionSnapshot) -> Result<AgentGraphReduction, Corruption> {
    reduce_agent_graph_prefix(snapshot.header(), snapshot.mutations())
}

/// Borrowed prospective-append form used by the session writer. It preserves
/// the graph reducer's exact semantics without cloning a retained snapshot.
pub(crate) fn reduce_agent_graph_ref_with_append(
    snapshot: &SessionSnapshot,
    appended: &crate::StoredMutation,
) -> Result<AgentGraphReduction, Corruption> {
    reduce_agent_graph_prefix(
        snapshot.header(),
        snapshot
            .mutations()
            .chain(std::iter::once(appended.borrowed())),
    )
}

fn reduce_agent_graph_prefix<'a>(
    header: &SessionHeader,
    mutations: impl Iterator<Item = crate::StoredMutationRef<'a>>,
) -> Result<AgentGraphReduction, Corruption> {
    let mut lanes = BTreeMap::<LaneId, (Option<EntryId>, crate::Sequence)>::new();
    lanes.insert(header.initial_lane.clone(), (None, crate::Sequence(0)));
    let mut entries = BTreeMap::<EntryId, (LaneId, SessionEntry, crate::Sequence)>::new();
    let mut operations = BTreeMap::<OperationId, OperationInfo>::new();
    let mut epochs = BTreeMap::<crate::EpochId, (OperationId, Option<EntryId>, crate::Sequence)>::new();
    let mut tools = BTreeMap::<String, Vec<ToolInfo>>::new();
    let mut policy = None;
    let mut policy_sequence = None;
    let mut spawns = Vec::<(crate::Sequence, AgentSpawnedFact)>::new();
    let mut deltas = Vec::<(crate::Sequence, WorkspaceDeltaFact)>::new();
    let mut terminals = Vec::<(crate::Sequence, AgentTaskFinishedFact)>::new();
    let mut applied = Vec::<(crate::Sequence, WorkspaceDeltaAppliedFact)>::new();

    for mutation in mutations {
        match mutation.mutation {
            SessionMutationRef::Lane(stored) => {
                let crate::LaneMutation::Created {
                    lane_id,
                    base_leaf_id,
                } = &stored.mutation;
                lanes.insert(lane_id.clone(), (base_leaf_id.clone(), stored.seq));
            }
            SessionMutationRef::Entry(stored) => {
                entries.insert(
                    stored.header.id.clone(),
                    (
                        stored.lane_id.clone(),
                        stored.body.clone(),
                        stored.header.seq,
                    ),
                );
            }
            SessionMutationRef::Record(stored) => match &stored.record {
                LaneRecord::OperationStarted(record) => {
                    operations.insert(
                        record.id.clone(),
                        OperationInfo {
                            lane_id: record.lane_id.clone(),
                            kind: record.kind.clone(),
                            original_input: record.original_input.clone(),
                            initial_harness_revision: record.initial_harness_revision.clone(),
                            model_harness_profile: record.model_harness_profile.clone(),
                            started_seq: stored.seq,
                            finished: None,
                        },
                    );
                }
                LaneRecord::OperationFinished(record) => {
                    let operation = operations.get_mut(&record.operation_id).ok_or_else(|| {
                        Corruption::new(format!(
                            "agent graph operation finish refers to unknown operation {}",
                            record.operation_id
                        ))
                    })?;
                    operation.finished = Some((stored.seq, record.outcome.clone()));
                }
                LaneRecord::EpochStarted(record) => {
                    epochs.insert(
                        record.id.clone(),
                        (
                            record.operation_id.clone(),
                            record.source_leaf_id.clone(),
                            stored.seq,
                        ),
                    );
                }
                LaneRecord::ToolStarted(record) => {
                    tools
                        .entry(record.tool_call_id.clone())
                        .or_default()
                        .push(ToolInfo {
                            operation_id: record.operation_id.clone(),
                            epoch_id: record.epoch_id.clone(),
                            tool_name: record.tool_name.clone(),
                            idempotency_key: record.idempotency_key.clone(),
                            sequence: stored.seq,
                        });
                }
                _ => {}
            },
            SessionMutationRef::Fact(stored) => match &stored.fact {
                crate::SessionFact::SubagentPolicy(value) => {
                    if policy.is_some() {
                        return Err(Corruption::new(
                            "session contains more than one subagent policy fact",
                        ));
                    }
                    value.validate()?;
                    policy = Some(value.clone());
                    policy_sequence = Some(stored.seq);
                }
                crate::SessionFact::AgentSpawned(value) => {
                    spawns.push((stored.seq, value.clone()));
                }
                crate::SessionFact::WorkspaceDelta(value) => {
                    deltas.push((stored.seq, value.clone()));
                }
                crate::SessionFact::AgentTaskFinished(value) => {
                    terminals.push((stored.seq, value.clone()));
                }
                crate::SessionFact::WorkspaceDeltaApplied(value) => {
                    applied.push((stored.seq, value.clone()));
                }
                _ => {}
            },
        }
    }

    let spawn_sequences = spawns
        .iter()
        .map(|(sequence, spawn)| (spawn.agent_id.clone(), *sequence))
        .collect::<BTreeMap<_, _>>();
    if let (Some(policy_sequence), Some(first_root_harness_revision)) = (
        policy_sequence,
        entries
            .values()
            .filter_map(|(lane_id, entry, sequence)| {
                (lane_id == &header.initial_lane
                    && matches!(entry, SessionEntry::HarnessRevisionChanged(_)))
                .then_some(*sequence)
            })
            .min(),
    ) && policy_sequence >= first_root_harness_revision
    {
        return Err(Corruption::new(
            "subagent policy must be persisted before the initial root harness revision",
        ));
    }
    let mut nodes = BTreeMap::<AgentId, AgentGraphNode>::new();
    let mut agent_by_lane = BTreeMap::<LaneId, AgentId>::new();
    let mut task_names = BTreeSet::<(OperationId, String)>::new();
    for (sequence, spawn) in spawns {
        let policy = policy.as_ref().ok_or_else(|| {
            Corruption::new("agent spawn requires a prior persisted subagent policy")
        })?;
        if policy_sequence.is_none_or(|policy_sequence| policy_sequence >= sequence) {
            return Err(Corruption::new(
                "agent spawn must follow the persisted subagent policy",
            ));
        }
        validate_spawn(
            &spawn,
            sequence,
            &header.session_id,
            &lanes,
            &entries,
            &operations,
            &epochs,
            &tools,
            policy,
        )?;
        if nodes.contains_key(&spawn.agent_id) {
            return Err(Corruption::new(format!(
                "agent {} was spawned more than once",
                spawn.agent_id
            )));
        }
        if agent_by_lane
            .insert(spawn.lane_id.clone(), spawn.agent_id.clone())
            .is_some()
        {
            return Err(Corruption::new(format!(
                "lane {} is bound to more than one agent",
                spawn.lane_id
            )));
        }
        if !task_names.insert((spawn.parent_operation_id.clone(), spawn.task_name.clone())) {
            return Err(Corruption::new(format!(
                "root operation {} has duplicate child task name {:?}",
                spawn.parent_operation_id, spawn.task_name
            )));
        }
        nodes.insert(
            spawn.agent_id.clone(),
            AgentGraphNode {
                spawned: spawn,
                operation_id: None,
                terminal: None,
                workspace_delta: None,
                applied: None,
                state: AgentState::Spawned,
            },
        );
    }

    for (operation_id, operation) in &operations {
        let OperationKind::Subagent {
            agent_id,
            parent_operation_id,
        } = &operation.kind
        else {
            continue;
        };
        let node = nodes.get_mut(agent_id).ok_or_else(|| {
            Corruption::new(format!(
                "subagent operation {operation_id} has no preceding agent spawn fact"
            ))
        })?;
        if node.spawned.parent_operation_id != *parent_operation_id
            || node.spawned.lane_id != operation.lane_id
        {
            return Err(Corruption::new(format!(
                "subagent operation {operation_id} disagrees with agent {agent_id} linkage"
            )));
        }
        if operation.started_seq
            <= *spawn_sequences.get(agent_id).ok_or_else(|| {
                Corruption::new(format!("agent {agent_id} has no spawn sequence"))
            })?
        {
            return Err(Corruption::new(format!(
                "subagent operation {operation_id} precedes its agent spawn fact"
            )));
        }
        if node.operation_id.replace(operation_id.clone()).is_some() {
            return Err(Corruption::new(format!(
                "agent {agent_id} has more than one child operation"
            )));
        }
        if operation.original_input.len() != 1 {
            return Err(Corruption::new(format!(
                "subagent operation {operation_id} must accept exactly one assignment entry"
            )));
        }
        let SessionEntry::UserMessage(assignment) = &operation.original_input[0].body else {
            return Err(Corruption::new(format!(
                "subagent operation {operation_id} assignment must be a user message"
            )));
        };
        if derive_subagent_operation_id(agent_id, &assignment.content) != *operation_id {
            return Err(Corruption::new(format!(
                "subagent operation {operation_id} does not match its deterministic assignment identity"
            )));
        }
        validate_child_operation_identity(operation, &node.spawned)?;
    }

    let mut deltas_by_id = BTreeMap::<WorkspaceDeltaId, (crate::Sequence, WorkspaceDeltaFact)>::new();
    let mut delta_by_agent = BTreeMap::<AgentId, WorkspaceDeltaId>::new();
    for (sequence, delta) in deltas {
        validate_delta(&delta)?;
        let node = nodes.get(&delta.agent_id).ok_or_else(|| {
            Corruption::new(format!("workspace delta {} refers to unknown agent {}", delta.delta_id, delta.agent_id))
        })?;
        if node.spawned.workspace_lease_id != delta.workspace_lease_id {
            return Err(Corruption::new(format!(
                "workspace delta {} does not use agent {} lease",
                delta.delta_id, delta.agent_id
            )));
        }
        let operation_id = node.operation_id.as_ref().ok_or_else(|| {
            Corruption::new(format!(
                "workspace delta {} exists before agent {} operation acceptance",
                delta.delta_id, delta.agent_id
            ))
        })?;
        let operation = operations.get(operation_id).expect("agent operation was indexed");
        let Some((finished_sequence, _)) = &operation.finished else {
            return Err(Corruption::new(format!(
                "workspace delta {} precedes child operation completion",
                delta.delta_id
            )));
        };
        if *finished_sequence >= sequence {
            return Err(Corruption::new(format!(
                "workspace delta {} must follow child operation completion",
                delta.delta_id
            )));
        }
        if delta_by_agent.insert(delta.agent_id.clone(), delta.delta_id.clone()).is_some()
            || deltas_by_id.contains_key(&delta.delta_id)
        {
            return Err(Corruption::new(format!(
                "agent {} has more than one workspace delta",
                delta.agent_id
            )));
        }
        deltas_by_id.insert(delta.delta_id.clone(), (sequence, delta));
    }

    for (_, node) in &mut nodes {
        if let Some(delta_id) = delta_by_agent.get(&node.spawned.agent_id)
            && let Some((_, delta)) = deltas_by_id.get(delta_id)
        {
            node.workspace_delta = Some(delta.clone());
        }
    }

    let mut terminal_sequences = BTreeMap::<AgentId, crate::Sequence>::new();
    for (sequence, terminal) in terminals {
        validate_terminal(&terminal)?;
        let node = nodes.get_mut(&terminal.agent_id).ok_or_else(|| {
            Corruption::new(format!("agent terminal result refers to unknown agent {}", terminal.agent_id))
        })?;
        if node.operation_id.as_ref() != Some(&terminal.operation_id) {
            return Err(Corruption::new(format!(
                "agent terminal result {} does not name its child operation",
                terminal.agent_id
            )));
        }
        let operation = operations
            .get(&terminal.operation_id)
            .expect("agent operation was indexed");
        let Some((finished_sequence, outcome)) = &operation.finished else {
            return Err(Corruption::new(format!(
                "agent terminal result {} precedes child operation completion",
                terminal.agent_id
            )));
        };
        if *finished_sequence >= sequence || *outcome != terminal.outcome {
            return Err(Corruption::new(format!(
                "agent terminal result {} disagrees with child operation outcome",
                terminal.agent_id
            )));
        }
        if let Some(entry_id) = &terminal.final_entry_id {
            let Some((lane_id, entry, entry_sequence)) = entries.get(entry_id) else {
                return Err(Corruption::new(format!(
                    "agent terminal result {} refers to missing final entry {entry_id}",
                    terminal.agent_id
                )));
            };
            if *entry_sequence >= sequence
                || lane_id != &node.spawned.lane_id
                || !matches!(entry, SessionEntry::AssistantMessage(_))
            {
                return Err(Corruption::new(format!(
                    "agent terminal result {} final entry is not a child assistant entry",
                    terminal.agent_id
                )));
            }
            if let (
                SessionEntry::AssistantMessage(assistant),
                PayloadRef::Inline(crate::JsonValue::String(report)),
            ) = (entry, &terminal.report)
                && assistant.content != *report
            {
                return Err(Corruption::new(format!(
                    "agent terminal result {} inline report differs from its final assistant entry",
                    terminal.agent_id
                )));
            }
        }
        match (&terminal.workspace_delta_id, &node.workspace_delta) {
            (Some(delta_id), Some(delta)) if delta_id == &delta.delta_id => {
                if deltas_by_id
                    .get(delta_id)
                    .is_none_or(|(delta_sequence, _)| *delta_sequence >= sequence)
                {
                    return Err(Corruption::new(format!(
                        "agent terminal result {} must follow its workspace delta fact",
                        terminal.agent_id
                    )));
                }
            }
            (Some(delta_id), _) => {
                return Err(Corruption::new(format!(
                    "agent terminal result {} refers to unknown workspace delta {delta_id}",
                    terminal.agent_id
                )));
            }
            (None, Some(_)) => {
                return Err(Corruption::new(format!(
                    "agent terminal result {} omits its durable workspace delta",
                    terminal.agent_id
                )));
            }
            (None, None) => {}
        }
        if node.terminal.replace(terminal).is_some() {
            return Err(Corruption::new("agent has more than one terminal result fact"));
        }
        if terminal_sequences
            .insert(node.spawned.agent_id.clone(), sequence)
            .is_some()
        {
            return Err(Corruption::new("agent has more than one terminal result sequence"));
        }
    }

    for (sequence, applied_fact) in applied {
        validate_changed_paths(&applied_fact.changed_paths)?;
        validate_bounded_nonempty("applied delta tool call ID", &applied_fact.tool_call_id, MAX_MODEL_IDENTIFIER_BYTES)?;
        let Some((delta_sequence, delta)) = deltas_by_id.get(&applied_fact.delta_id) else {
            return Err(Corruption::new(format!(
                "applied workspace delta {} is unknown",
                applied_fact.delta_id
            )));
        };
        if *delta_sequence >= sequence {
            return Err(Corruption::new(format!(
                "applied workspace delta {} precedes its delta fact",
                applied_fact.delta_id
            )));
        }
        if applied_fact.changed_paths != delta.changed_paths {
            return Err(Corruption::new(format!(
                "applied workspace delta {} has different changed paths",
                applied_fact.delta_id
            )));
        }
        let node = nodes
            .get_mut(&delta.agent_id)
            .expect("workspace delta agent was validated");
        if node
            .terminal
            .as_ref()
            .is_none_or(|terminal| terminal.workspace_delta_id.as_ref() != Some(&delta.delta_id))
        {
            return Err(Corruption::new(format!(
                "applied workspace delta {} has no durable child terminal result",
                applied_fact.delta_id
            )));
        }
        if terminal_sequences
            .get(&delta.agent_id)
            .is_none_or(|terminal_sequence| *terminal_sequence >= sequence)
        {
            return Err(Corruption::new(format!(
                "applied workspace delta {} must follow the child terminal result",
                applied_fact.delta_id
            )));
        }
        if applied_fact.target_lane_id != node.spawned.parent_lane_id {
            return Err(Corruption::new(format!(
                "applied workspace delta {} targets a lane other than its parent",
                applied_fact.delta_id
            )));
        }
        let matches = tools
            .get(&applied_fact.tool_call_id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|tool| {
                tool.sequence < sequence
                    && tool.operation_id == node.spawned.parent_operation_id
                    && tool.tool_name == "apply_agent_changes"
            })
            .count();
        if matches != 1 {
            return Err(Corruption::new(format!(
                "applied workspace delta {} must correlate with exactly one parent apply_agent_changes tool intent",
                applied_fact.delta_id
            )));
        }
        if node.applied.replace(applied_fact).is_some() {
            return Err(Corruption::new("workspace delta was applied more than once"));
        }
    }

    for node in nodes.values_mut() {
        node.state = match (&node.operation_id, &node.terminal, &node.workspace_delta, &node.applied) {
            (None, _, _, _) => AgentState::Spawned,
            (Some(_), None, _, _) => {
                let operation = operations
                    .get(node.operation_id.as_ref().expect("state has operation"))
                    .expect("agent operation was indexed");
                match &operation.finished {
                    Some((_, outcome)) => AgentState::Finalizing {
                        outcome: outcome.clone(),
                    },
                    None => AgentState::Running,
                }
            }
            (Some(_), Some(terminal), Some(delta), Some(_)) => AgentState::Applied {
                outcome: terminal.outcome.clone(),
                delta_id: delta.delta_id.clone(),
            },
            (Some(_), Some(terminal), Some(delta), None) => AgentState::DeltaReady {
                outcome: terminal.outcome.clone(),
                delta_id: delta.delta_id.clone(),
            },
            (Some(_), Some(terminal), None, None) => match &terminal.outcome {
                OperationOutcome::Completed => AgentState::Completed {
                    outcome: OperationOutcome::Completed,
                },
                OperationOutcome::Aborted => AgentState::Interrupted,
                OperationOutcome::Failed { code } => AgentState::Failed { code: code.clone() },
            },
            (Some(_), Some(_), None, Some(_)) => {
                return Err(Corruption::new(
                    "agent applied state has no durable workspace delta",
                ));
            }
        };
    }

    Ok(AgentGraphReduction {
        policy,
        agents: nodes,
    })
}

fn validate_spawn(
    spawn: &AgentSpawnedFact,
    sequence: crate::Sequence,
    session_id: &crate::SessionId,
    lanes: &BTreeMap<LaneId, (Option<EntryId>, crate::Sequence)>,
    entries: &BTreeMap<EntryId, (LaneId, SessionEntry, crate::Sequence)>,
    operations: &BTreeMap<OperationId, OperationInfo>,
    epochs: &BTreeMap<crate::EpochId, (OperationId, Option<EntryId>, crate::Sequence)>,
    tools: &BTreeMap<String, Vec<ToolInfo>>,
    policy: &SubagentPolicyFact,
) -> Result<(), Corruption> {
    validate_task_name(&spawn.task_name)?;
    validate_bounded_nonempty("agent thinking", &spawn.thinking, MAX_THINKING_BYTES)?;
    validate_bounded_nonempty(
        "agent spawn tool call ID",
        &spawn.spawn_tool_call_id,
        MAX_MODEL_IDENTIFIER_BYTES,
    )?;
    spawn.model.validate()?;
    if !policy.models.iter().any(|model| model == &spawn.model) {
        return Err(Corruption::new(format!(
            "agent {} selected a model outside the persisted subagent policy",
            spawn.agent_id
        )));
    }
    let parent = operations.get(&spawn.parent_operation_id).ok_or_else(|| {
        Corruption::new(format!(
            "agent {} refers to unknown parent operation {}",
            spawn.agent_id, spawn.parent_operation_id
        ))
    })?;
    if parent.lane_id != spawn.parent_lane_id
        || parent.lane_id != LaneId::main()
        || matches!(parent.kind, OperationKind::Subagent { .. })
        || parent.finished.as_ref().is_some_and(|(finished, _)| *finished < sequence)
    {
        return Err(Corruption::new(format!(
            "agent {} parent operation is not a root-lane operation",
            spawn.agent_id
        )));
    }
    let spawn_tool = tools
        .get(&spawn.spawn_tool_call_id)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|tool| {
            tool.sequence < sequence
                && tool.operation_id == spawn.parent_operation_id
                && tool.tool_name == "spawn_agent"
        })
        .collect::<Vec<_>>();
    if spawn_tool.len() != 1 {
        return Err(Corruption::new(format!(
            "agent {} must correlate with exactly one parent spawn_agent tool intent",
            spawn.agent_id
        )));
    }
    if AgentId::derive(
        session_id,
        &spawn.parent_lane_id,
        &spawn.parent_operation_id,
        &spawn_tool[0].idempotency_key,
    ) != spawn.agent_id
    {
        return Err(Corruption::new(format!(
            "agent {} does not match its deterministic spawn identity",
            spawn.agent_id
        )));
    }
    if spawn.lane_id != spawn.agent_id.lane_id() {
        return Err(Corruption::new(format!(
            "agent {} does not own its deterministic child lane",
            spawn.agent_id
        )));
    }
    if spawn.workspace_lease_id != WorkspaceLeaseId::derive(&spawn.agent_id) {
        return Err(Corruption::new(format!(
            "agent {} does not own its deterministic workspace lease",
            spawn.agent_id
        )));
    }
    let Some((base_leaf_id, lane_sequence)) = lanes.get(&spawn.lane_id) else {
        return Err(Corruption::new(format!(
            "agent {} refers to a missing child lane {}",
            spawn.agent_id, spawn.lane_id
        )));
    };
    if *lane_sequence >= sequence || *base_leaf_id != spawn.base_leaf_id {
        return Err(Corruption::new(format!(
            "agent {} child lane does not match its durable branch base",
            spawn.agent_id
        )));
    }
    match (&spawn.context_mode, &spawn.base_leaf_id) {
        (AgentContextMode::Task, None) | (AgentContextMode::Parent, Some(_)) => {}
        (AgentContextMode::Task, Some(_)) => {
            return Err(Corruption::new("task-context agent must not inherit a base leaf"));
        }
        (AgentContextMode::Parent, None) => {
            return Err(Corruption::new("parent-context agent requires a base leaf"));
        }
    }
    let spawn_tool_epoch = epochs.get(&spawn_tool[0].epoch_id).ok_or_else(|| {
        Corruption::new(format!(
            "agent {} spawn tool intent refers to an unknown epoch",
            spawn.agent_id
        ))
    })?;
    if spawn_tool_epoch.0 != spawn.parent_operation_id || spawn_tool_epoch.2 >= sequence {
        return Err(Corruption::new(format!(
            "agent {} spawn tool epoch does not belong to its parent operation",
            spawn.agent_id
        )));
    }
    if matches!(spawn.context_mode, AgentContextMode::Parent)
        && spawn.base_leaf_id != spawn_tool_epoch.1
    {
        return Err(Corruption::new(
            "parent-context agent must fork the exact parent epoch source leaf",
        ));
    }
    let mut model = None::<(crate::Sequence, &ModelChangedEntry)>;
    let mut thinking = None::<(crate::Sequence, &str)>;
    let mut harness = None::<(crate::Sequence, &crate::HarnessRevisionChangedEntry)>;
    for (lane_id, entry, entry_sequence) in entries.values() {
        if lane_id != &spawn.lane_id || *entry_sequence >= sequence {
            continue;
        }
        match entry {
            SessionEntry::ModelChanged(value)
                if model.is_none_or(|(prior, _)| prior < *entry_sequence) =>
            {
                model = Some((*entry_sequence, value));
            }
            SessionEntry::ThinkingChanged(value)
                if thinking.is_none_or(|(prior, _)| prior < *entry_sequence) =>
            {
                thinking = Some((*entry_sequence, value.level.as_str()));
            }
            SessionEntry::HarnessRevisionChanged(value)
                if harness.is_none_or(|(prior, _)| prior < *entry_sequence) =>
            {
                harness = Some((*entry_sequence, value));
            }
            _ => {}
        }
    }
    if !matches!(model, Some((_, ModelChangedEntry { provider, model, revision })) if provider == &spawn.model.provider && model == &spawn.model.model && revision == &spawn.model.revision)
        || thinking.map(|(_, value)| value) != Some(spawn.thinking.as_str())
        || !matches!(harness, Some((_, value)) if value.revision_id == spawn.harness_revision_id && value.snapshot_id == spawn.harness_snapshot_id)
    {
        return Err(Corruption::new(format!(
            "agent {} child lane configuration does not match its spawn fact",
            spawn.agent_id
        )));
    }
    Ok(())
}

fn validate_child_operation_identity(
    operation: &OperationInfo,
    spawn: &AgentSpawnedFact,
) -> Result<(), Corruption> {
    if operation.lane_id != spawn.lane_id
        || operation.initial_harness_revision != spawn.harness_revision_id
        || operation.model_harness_profile != spawn.model_harness_profile_id
    {
        return Err(Corruption::new(
            "child operation identity does not match its agent spawn",
        ));
    }
    Ok(())
}

fn validate_delta(delta: &WorkspaceDeltaFact) -> Result<(), Corruption> {
    validate_bounded_nonempty("workspace delta base commit", &delta.base_commit, MAX_COMMIT_BYTES)?;
    validate_bounded_nonempty(
        "workspace delta result commit",
        &delta.result_commit,
        MAX_COMMIT_BYTES,
    )?;
    if WorkspaceDeltaId::derive(
        &delta.workspace_lease_id,
        &delta.base_commit,
        &delta.result_commit,
    ) != delta.delta_id
    {
        return Err(Corruption::new(format!(
            "workspace delta {} does not match its deterministic identity",
            delta.delta_id
        )));
    }
    if delta.changed_paths.is_empty() {
        return Err(Corruption::new(
            "workspace delta must retain at least one changed path",
        ));
    }
    validate_changed_paths(&delta.changed_paths)?;
    match &delta.patch {
        PayloadRef::Artifact {
            artifact_id: _,
            byte_len,
            media_type,
        } if *byte_len > 0 => {
            validate_bounded_nonempty("workspace delta patch media type", media_type, MAX_DISPLAY_NAME_BYTES)
        }
        PayloadRef::Artifact { .. } => Err(Corruption::new(
            "workspace delta patch artifact must have a nonzero byte length",
        )),
        PayloadRef::Inline(_) => Err(Corruption::new(
            "workspace delta patch must be retained as an immutable artifact",
        )),
    }
}

fn validate_terminal(terminal: &AgentTaskFinishedFact) -> Result<(), Corruption> {
    match &terminal.report {
        PayloadRef::Inline(value) => {
            let Some(report) = value.as_str() else {
                return Err(Corruption::new("inline agent report must be a UTF-8 string"));
            };
            if report.len() > MAX_INLINE_REPORT_BYTES {
                return Err(Corruption::new(
                    "inline agent report exceeds the 32768-byte retention limit",
                ));
            }
        }
        PayloadRef::Artifact { media_type, .. } => {
            validate_bounded_nonempty("agent report media type", media_type, MAX_DISPLAY_NAME_BYTES)?;
        }
    }
    Ok(())
}

fn validate_task_name(task_name: &str) -> Result<(), Corruption> {
    if task_name.is_empty() || task_name.len() > MAX_TASK_NAME_BYTES {
        return Err(Corruption::new(
            "agent task name must contain 1..=64 ASCII bytes",
        ));
    }
    let bytes = task_name.as_bytes();
    if !bytes[0].is_ascii_lowercase()
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return Err(Corruption::new(
            "agent task name must match ^[a-z][a-z0-9_]{0,63}$",
        ));
    }
    Ok(())
}

fn validate_changed_paths(paths: &[String]) -> Result<(), Corruption> {
    let mut previous: Option<&String> = None;
    for path in paths {
        if path.is_empty()
            || path.len() > MAX_CHANGED_PATH_BYTES
            || path.contains('\0')
            || path.starts_with('/')
            || path.starts_with('\\')
            || path.as_bytes().get(1) == Some(&b':')
            || path.contains('\\')
        {
            return Err(Corruption::new(
                "workspace delta path must be a bounded normalized repository-relative path",
            ));
        }
        if path.split('/').any(|segment| segment.is_empty() || segment == "." || segment == "..") {
            return Err(Corruption::new(
                "workspace delta path contains an empty, current, or parent component",
            ));
        }
        if previous.is_some_and(|previous| previous >= path) {
            return Err(Corruption::new(
                "workspace delta paths must be unique and deterministically ordered",
            ));
        }
        previous = Some(path);
    }
    Ok(())
}

fn validate_bounded_nonempty(name: &str, value: &str, maximum: usize) -> Result<(), Corruption> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(Corruption::new(format!(
            "{name} must be a nonempty bounded string without control characters"
        )));
    }
    Ok(())
}
