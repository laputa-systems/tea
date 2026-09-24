use crate::{
    EffectiveLaneConfiguration, EntryId, EpochId, HarnessRevisionChangedEntry, InputReduction,
    InputState, InputStatus, LaneId, LaneRecord, LaneState, LaneStatus, OperationId,
    PendingExtensionControl, PendingHarnessActivation, PendingWrite, ProvisionedEntry, Sequence,
    SessionEntry, SessionFact, SessionMutationRef, SessionSnapshot, StepId, StepKind, StoredCommit,
    StoredEntry, StoredMutation, StoredMutationRef, ToolReplayPolicy, ToolStartedRecord, Usage,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Validation failure in an append-only session prefix.
///
/// A corruption is never repaired by reduction. Hosts must fault the harness
/// and require an explicit reopen after storage repair.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Corruption {
    message: String,
}

impl Corruption {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Stable bounded diagnostic text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Corruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Corruption {}

/// Pure recovery work derived from one durable prefix.
#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryPlan {
    /// An accepted operation's exact provisioned input was not fully appended.
    AppendAcceptedInput {
        /// Owning operation.
        operation_id: OperationId,
        /// Still-unmaterialized provisioned entries in original order.
        entries: Vec<ProvisionedEntry>,
    },
    /// A committed effect intent has no durable outcome. It is indeterminate
    /// until an explicit host reconciliation classifies it; recovery never
    /// fabricates a failure result or replays it merely because the process
    /// lost its reply.
    ReconcileToolEffect {
        /// Provisioned result identity.
        result_entry_id: EntryId,
    },
    /// A persisted `Safe` intent needs a host comparison with the currently
    /// resolved declaration before it can replay.
    ReplayToolIfStillSafe {
        /// Original durable intent.
        tool: ToolStartedRecord,
    },
    /// The assistant entry exists but no effect intent/result does; normal
    /// schema and `before_tool` preparation must run again.
    ResumeAssistantToolPath {
        /// Assistant message whose tool calls need normal processing.
        assistant_entry_id: EntryId,
    },
    /// A physical request was dispatched but has no durable settlement.
    ReconcileProviderRequest {
        /// Provider request identity.
        request_id: crate::ProviderRequestId,
    },
    /// A provider request intent has no retained request material, so storage
    /// proves it was not admitted to the invocation boundary.
    ProviderRequestNotAdmitted {
        /// Provider request identity whose invocation never became eligible.
        request_id: crate::ProviderRequestId,
    },
    /// A validated activation request is ready only after the old epoch settled.
    ActivateHarness {
        /// Source activation request record.
        request: crate::HarnessActivationRequestedRecord,
    },
    /// No epoch is running under the accepted operation's currently derived revision.
    StartEpoch {
        /// Owning operation.
        operation_id: OperationId,
    },
    /// Resume ordinary operation scheduling without a pending external-effect ambiguity.
    ResumeOperation {
        /// Owning operation.
        operation_id: OperationId,
    },
}

/// Complete output of the pure lane reducer.
#[derive(Clone, Debug, PartialEq)]
pub struct LaneReduction {
    /// Authoritative reduced state of the requested lane.
    pub lane_state: LaneState,
    /// Configuration contributions resolved from the branch parent chain.
    pub effective_configuration: EffectiveLaneConfiguration,
    /// Exactly one next recovery action, when an operation remains open.
    pub recovery_plan: Option<RecoveryPlan>,
    /// Accepted input lifecycle and deterministic pending dispatch order.
    pub input_reduction: InputReduction,
    /// Extension controls waiting for their target operation's idle boundary.
    pub pending_extension_controls: Vec<PendingExtensionControl>,
    /// Exact version-pinned extension state effective on this lane.
    pub extension_state: BTreeMap<String, crate::ExtensionStateValue>,
    /// Deferred entries not yet materialized into semantic history.
    pub pending_writes: Vec<PendingWrite>,
    /// At most one unresolved activation request for the lane.
    pub pending_harness_activation: Option<PendingHarnessActivation>,
    /// Independently accumulated provider/tool usage facts.
    pub usage_totals: Usage,
}

#[derive(Clone)]
struct OperationState {
    lane_id: LaneId,
    original_input: Vec<ProvisionedEntry>,
    finished: bool,
    outcome: Option<crate::OperationOutcome>,
    epoch_ids: Vec<EpochId>,
    open_epochs: BTreeSet<EpochId>,
    entry_ids: Vec<EntryId>,
}

/// Reduce one complete session snapshot without reading clocks, files, hooks,
/// providers, tools, or mutable live state.
pub fn reduce_lane(input: SessionSnapshot, lane: LaneId) -> Result<LaneReduction, Corruption> {
    reduce_lane_ref(&input, lane)
}

/// Reduce only the exact version-pinned extension state effective on one
/// lane. The result includes checkpoint inheritance and therefore never reads
/// later parent-lane writes through a fork.
pub fn extension_state_for_lane(
    input: &SessionSnapshot,
    lane: LaneId,
) -> Result<BTreeMap<String, crate::ExtensionStateValue>, Corruption> {
    Ok(reduce_lane_ref(input, lane)?.extension_state)
}

/// Borrowed form used while a store validates a candidate before commit.
///
/// The reducer never consumes or mutates its input. Keeping this form crate
/// private avoids making snapshot ownership part of the public contract while
/// preventing an append validation from cloning the complete history merely to
/// call the same pure reducer.
pub(crate) fn reduce_lane_ref(
    input: &SessionSnapshot,
    lane: LaneId,
) -> Result<LaneReduction, Corruption> {
    reduce_lane_prefix(
        input.header(),
        input.last_sequence(),
        input.mutations(),
        lane,
    )
}

/// Borrowed prospective-commit form used by stores before the one durable
/// append. Every item in `appended` shares a commit sequence but reducer order
/// remains the caller's semantic item order.
pub(crate) fn reduce_lane_ref_with_commit(
    input: &SessionSnapshot,
    appended: &StoredCommit,
    lane: LaneId,
) -> Result<LaneReduction, Corruption> {
    reduce_lane_prefix(
        input.header(),
        appended.seq,
        input
            .mutations()
            .chain(appended.items.iter().map(StoredMutation::borrowed)),
        lane,
    )
}

fn reduce_lane_prefix<'a>(
    header: &crate::SessionHeader,
    last_sequence: Sequence,
    mutations: impl Iterator<Item = StoredMutationRef<'a>>,
    lane: LaneId,
) -> Result<LaneReduction, Corruption> {
    let mut lane_leaves = BTreeMap::<LaneId, Option<EntryId>>::new();
    lane_leaves.insert(header.initial_lane.clone(), None);

    let mut entries = BTreeMap::<EntryId, StoredEntry>::new();
    let mut operations = BTreeMap::<OperationId, OperationState>::new();
    let mut active_operations = BTreeMap::<LaneId, OperationId>::new();
    let mut epochs = BTreeMap::<EpochId, OperationId>::new();
    let mut step_attempts = BTreeMap::<(OperationId, EpochId, StepKind), (StepId, u32)>::new();
    let mut steps = BTreeMap::<StepId, (OperationId, EpochId, StepKind)>::new();
    let mut provisioned_entries = BTreeMap::<EntryId, ProvisionedEntry>::new();
    let mut tool_starts = Vec::new();
    let mut provider_starts =
        BTreeMap::<crate::ProviderRequestId, crate::ProviderRequestStartedRecord>::new();
    let mut provider_settled =
        BTreeMap::<crate::ProviderRequestId, crate::ProviderSettlementClassification>::new();
    let mut provider_material = BTreeSet::new();
    let mut compaction_provider_requests = BTreeSet::new();
    let mut input_states = BTreeMap::<EntryId, InputState>::new();
    let mut input_order = Vec::<EntryId>::new();
    let mut extension_controls = BTreeMap::<String, PendingExtensionControl>::new();
    let mut applied_extension_controls = BTreeSet::<String>::new();
    let mut extension_state =
        BTreeMap::<LaneId, BTreeMap<String, crate::ExtensionStateValue>>::new();
    extension_state.insert(header.initial_lane.clone(), BTreeMap::new());
    let mut checkpoints = BTreeMap::<crate::TurnCheckpointId, crate::TurnCheckpointFact>::new();
    let mut forked_lanes = BTreeSet::<LaneId>::new();
    let mut fresh_lanes = BTreeSet::<LaneId>::new();
    let mut child_spawns = BTreeMap::<crate::AgentId, (OperationId, Sequence)>::new();
    let mut child_terminal = BTreeSet::<crate::AgentId>::new();
    let mut checkpoint_ready_for = None::<OperationId>;
    let mut deferred_writes = Vec::new();
    let mut activation_requests = Vec::new();
    let mut usage_by_lane = BTreeMap::<LaneId, Usage>::new();
    let mut observed_sequence = Sequence(0);
    let mut observed_item_index = 0_u64;

    for mutation in mutations {
        let item_index = observed_item_index;
        observed_item_index = observed_item_index.saturating_add(1);
        let sequence = mutation.sequence();
        if sequence != observed_sequence
            && sequence != Sequence(observed_sequence.0.saturating_add(1))
        {
            return Err(Corruption::new(format!(
                "session commit sequence must be consecutive: expected {} or {}, found {}",
                observed_sequence.0,
                observed_sequence.0.saturating_add(1),
                sequence.0
            )));
        }
        observed_sequence = sequence;

        let preserves_checkpoint_readiness = match &mutation.mutation {
            SessionMutationRef::Record(stored) => match &stored.record {
                LaneRecord::InputSettled(record) => {
                    checkpoint_ready_for.as_ref() == Some(&record.operation_id)
                }
                LaneRecord::ExtensionControlApplied(record) => {
                    checkpoint_ready_for.as_ref().is_some_and(|operation_id| {
                        extension_controls
                            .get(&record.control_id)
                            .is_some_and(|control| control.control.operation_id == *operation_id)
                    })
                }
                _ => false,
            },
            SessionMutationRef::Fact(stored) => match &stored.fact {
                SessionFact::TurnCheckpoint(_) => true,
                SessionFact::ExtensionStateValueSet(fact) => {
                    checkpoint_ready_for.as_ref().is_some_and(|operation_id| {
                        operations
                            .get(operation_id)
                            .is_some_and(|operation| operation.lane_id == fact.lane_id)
                    })
                }
                _ => false,
            },
            SessionMutationRef::Entry(_) | SessionMutationRef::Lane(_) => false,
        };
        if !preserves_checkpoint_readiness {
            checkpoint_ready_for = None;
        }

        match mutation.mutation {
            SessionMutationRef::Lane(stored) => match &stored.mutation {
                crate::LaneMutation::Created {
                    lane_id,
                    base_leaf_id,
                } => {
                    if lane_leaves.contains_key(lane_id) {
                        return Err(Corruption::new(format!("duplicate lane ID {lane_id}")));
                    }
                    if let Some(base_leaf_id) = base_leaf_id
                        && !entries.contains_key(base_leaf_id)
                    {
                        return Err(Corruption::new(format!(
                            "lane {lane_id} refers to missing base entry {base_leaf_id}"
                        )));
                    }
                    lane_leaves.insert(lane_id.clone(), base_leaf_id.clone());
                    extension_state.insert(lane_id.clone(), BTreeMap::new());
                    fresh_lanes.insert(lane_id.clone());
                }
            },
            SessionMutationRef::Entry(stored) => {
                fresh_lanes.remove(&stored.lane_id);
                if let SessionEntry::Compaction(compaction) = &stored.body {
                    validate_compaction_replacement(compaction)?;
                    if let Some(request_id) = &compaction.provider_request_id {
                        let is_completed_compaction_request =
                            provider_starts.get(request_id).is_some_and(|request| {
                                steps.get(&request.step_id).is_some_and(
                                    |(operation_id, epoch_id, kind)| {
                                        operation_id == &request.operation_id
                                            && epoch_id == &request.epoch_id
                                            && *kind == StepKind::Compaction
                                    },
                                )
                            });
                        if !is_completed_compaction_request
                            || !provider_material.contains(request_id)
                            || !matches!(
                                provider_settled.get(request_id),
                                Some(crate::ProviderSettlementClassification::Completed)
                            )
                            || !compaction_provider_requests.insert(request_id.clone())
                        {
                            return Err(Corruption::new(format!(
                                "compaction entry refers to provider request {request_id} without one completed compaction request, material, and unique replacement"
                            )));
                        }
                    }
                }
                let Some(current_leaf) = lane_leaves.get(&stored.lane_id).cloned() else {
                    return Err(Corruption::new(format!(
                        "entry {} targets unknown lane {}",
                        stored.header.id, stored.lane_id
                    )));
                };
                if stored.header.parent_id != current_leaf {
                    return Err(Corruption::new(format!(
                        "entry {} has stale or invalid parent on lane {}",
                        stored.header.id, stored.lane_id
                    )));
                }
                if entries.contains_key(&stored.header.id) {
                    return Err(Corruption::new(format!(
                        "duplicate entry ID {}",
                        stored.header.id
                    )));
                }
                if let Some(provisioned) = provisioned_entries.get(&stored.header.id)
                    && provisioned.body != stored.body
                {
                    return Err(Corruption::new(format!(
                        "provisioned entry {} materialized with different content",
                        stored.header.id
                    )));
                }
                if let Some(parent_id) = &stored.header.parent_id
                    && !entries.contains_key(parent_id)
                {
                    return Err(Corruption::new(format!(
                        "entry {} refers to missing parent {parent_id}",
                        stored.header.id
                    )));
                }
                if let Some(operation_id) = active_operations.get(&stored.lane_id) {
                    operations
                        .get_mut(operation_id)
                        .expect("active operation was inserted")
                        .entry_ids
                        .push(stored.header.id.clone());
                }
                lane_leaves.insert(stored.lane_id.clone(), Some(stored.header.id.clone()));
                entries.insert(stored.header.id.clone(), stored.clone());
            }
            SessionMutationRef::Record(stored) => match &stored.record {
                LaneRecord::OperationStarted(record) => {
                    fresh_lanes.remove(&record.lane_id);
                    if operations.contains_key(&record.id) {
                        return Err(Corruption::new(format!(
                            "duplicate operation ID {}",
                            record.id
                        )));
                    }
                    let Some(current_leaf) = lane_leaves.get(&record.lane_id) else {
                        return Err(Corruption::new(format!(
                            "operation {} targets unknown lane {}",
                            record.id, record.lane_id
                        )));
                    };
                    if &record.source_leaf_id != current_leaf {
                        return Err(Corruption::new(format!(
                            "operation {} accepted against stale lane leaf",
                            record.id
                        )));
                    }
                    if active_operations
                        .insert(record.lane_id.clone(), record.id.clone())
                        .is_some()
                    {
                        return Err(Corruption::new(format!(
                            "lane {} has more than one open operation",
                            record.lane_id
                        )));
                    }
                    for provisioned in &record.original_input {
                        if entries.contains_key(&provisioned.id) {
                            return Err(Corruption::new(format!(
                                "operation {} provisions an already materialized entry {}",
                                record.id, provisioned.id
                            )));
                        }
                        match provisioned_entries.get(&provisioned.id) {
                            Some(existing) if existing != provisioned => {
                                return Err(Corruption::new(format!(
                                    "provisioned entry {} has conflicting content",
                                    provisioned.id
                                )));
                            }
                            Some(_) => {
                                return Err(Corruption::new(format!(
                                    "provisioned entry {} was accepted more than once",
                                    provisioned.id
                                )));
                            }
                            None => {
                                provisioned_entries
                                    .insert(provisioned.id.clone(), provisioned.clone());
                            }
                        }
                    }
                    if !record.input_ids.is_empty() {
                        if record.input_ids.len() != record.original_input.len() {
                            return Err(Corruption::new(format!(
                                "operation {} input membership and original input lengths differ",
                                record.id
                            )));
                        }
                        let mut seen_input_ids = BTreeSet::new();
                        for (input_id, provisioned) in
                            record.input_ids.iter().zip(&record.original_input)
                        {
                            if input_id != &provisioned.id
                                || !seen_input_ids.insert(input_id.clone())
                            {
                                return Err(Corruption::new(format!(
                                    "operation {} has duplicate or mismatched input membership",
                                    record.id
                                )));
                            }
                            let Some(input) = input_states.get_mut(input_id) else {
                                return Err(Corruption::new(format!(
                                    "operation {} dispatches unaccepted input {input_id}",
                                    record.id
                                )));
                            };
                            if input.accepted.lane_id != record.lane_id
                                || input.accepted.entry != *provisioned
                                || input.status != InputStatus::Pending
                            {
                                return Err(Corruption::new(format!(
                                    "operation {} dispatches unavailable input {input_id}",
                                    record.id
                                )));
                            }
                            input.status = InputStatus::Dispatched {
                                operation_id: record.id.clone(),
                            };
                        }
                    }
                    operations.insert(
                        record.id.clone(),
                        OperationState {
                            lane_id: record.lane_id.clone(),
                            original_input: record.original_input.clone(),
                            finished: false,
                            outcome: None,
                            epoch_ids: Vec::new(),
                            open_epochs: BTreeSet::new(),
                            entry_ids: Vec::new(),
                        },
                    );
                }
                LaneRecord::OperationFinished(record) => {
                    let operation = open_operation_mut(&mut operations, &record.operation_id)?;
                    if !operation.open_epochs.is_empty() {
                        return Err(Corruption::new(format!(
                            "operation {} finished with an open epoch",
                            record.operation_id
                        )));
                    }
                    operation.finished = true;
                    operation.outcome = Some(record.outcome.clone());
                    match active_operations.remove(&operation.lane_id) {
                        Some(current) if current == record.operation_id => {}
                        _ => {
                            return Err(Corruption::new(format!(
                                "operation {} was not active on its lane at finish",
                                record.operation_id
                            )));
                        }
                    }
                    checkpoint_ready_for = Some(record.operation_id.clone());
                }
                LaneRecord::AbortRequested(record) => {
                    let _ = open_operation(&operations, &record.operation_id)?;
                }
                LaneRecord::EpochStarted(record) => {
                    let operation = open_operation_mut(&mut operations, &record.operation_id)?;
                    if !operation.open_epochs.is_empty() {
                        return Err(Corruption::new(format!(
                            "operation {} has more than one open epoch",
                            record.operation_id
                        )));
                    }
                    if record.epoch_index != operation.epoch_ids.len() as u32 {
                        return Err(Corruption::new(format!(
                            "epoch {} has non-consecutive index {}",
                            record.id, record.epoch_index
                        )));
                    }
                    if epochs
                        .insert(record.id.clone(), record.operation_id.clone())
                        .is_some()
                    {
                        return Err(Corruption::new(format!("duplicate epoch ID {}", record.id)));
                    }
                    operation.epoch_ids.push(record.id.clone());
                    operation.open_epochs.insert(record.id.clone());
                }
                LaneRecord::EpochFinished(record) => {
                    let operation = open_operation_mut(&mut operations, &record.operation_id)?;
                    if epochs.get(&record.epoch_id) != Some(&record.operation_id)
                        || !operation.open_epochs.remove(&record.epoch_id)
                    {
                        return Err(Corruption::new(format!(
                            "epoch {} is not open for operation {}",
                            record.epoch_id, record.operation_id
                        )));
                    }
                }
                LaneRecord::StepAttempted(record) => {
                    let operation = open_operation(&operations, &record.operation_id)?;
                    if epochs.get(&record.epoch_id) != Some(&record.operation_id) {
                        return Err(Corruption::new(format!(
                            "step {} refers to invalid epoch {}",
                            record.id, record.epoch_id
                        )));
                    }
                    if !operation.open_epochs.contains(&record.epoch_id) {
                        return Err(Corruption::new(format!(
                            "step {} was recorded after its epoch settled",
                            record.id
                        )));
                    }
                    if steps
                        .insert(
                            record.id.clone(),
                            (
                                record.operation_id.clone(),
                                record.epoch_id.clone(),
                                record.kind,
                            ),
                        )
                        .is_some()
                    {
                        return Err(Corruption::new(format!(
                            "step ID {} was attempted more than once",
                            record.id
                        )));
                    }
                    let key = (
                        record.operation_id.clone(),
                        record.epoch_id.clone(),
                        record.kind,
                    );
                    match step_attempts.get(&key) {
                        None if record.attempt == 1 => {
                            step_attempts.insert(key, (record.id.clone(), record.attempt));
                        }
                        Some((previous_id, previous_attempt))
                            if record.attempt == previous_attempt.saturating_add(1)
                                && &record.id != previous_id =>
                        {
                            step_attempts.insert(key, (record.id.clone(), record.attempt));
                        }
                        Some((previous_id, _)) if &record.id == previous_id => {
                            return Err(Corruption::new(format!(
                                "step ID {} was attempted more than once",
                                record.id
                            )));
                        }
                        _ => {
                            return Err(Corruption::new(format!(
                                "step {} has non-consecutive attempt {}",
                                record.id, record.attempt
                            )));
                        }
                    }
                }
                LaneRecord::ProviderRequestStarted(record) => {
                    let operation = open_operation(&operations, &record.operation_id)?;
                    if epochs.get(&record.epoch_id) != Some(&record.operation_id)
                        || !operation.open_epochs.contains(&record.epoch_id)
                    {
                        return Err(Corruption::new(format!(
                            "provider request {} refers to a closed or invalid epoch",
                            record.request_id
                        )));
                    }
                    if !steps
                        .get(&record.step_id)
                        .is_some_and(|(operation_id, epoch_id, _)| {
                            operation_id == &record.operation_id && epoch_id == &record.epoch_id
                        })
                    {
                        return Err(Corruption::new(format!(
                            "provider request {} does not name its owning step {}",
                            record.request_id, record.step_id
                        )));
                    }
                    if provider_starts
                        .insert(record.request_id.clone(), record.clone())
                        .is_some()
                    {
                        return Err(Corruption::new(format!(
                            "duplicate provider request ID {}",
                            record.request_id
                        )));
                    }
                }
                LaneRecord::ProviderRequestSettled(record) => {
                    let _ = open_operation(&operations, &record.operation_id)?;
                    let Some(start) = provider_starts.get(&record.request_id) else {
                        return Err(Corruption::new(format!(
                            "provider settlement {} has no request intent",
                            record.request_id
                        )));
                    };
                    if start.operation_id != record.operation_id
                        || provider_settled
                            .insert(record.request_id.clone(), record.classification.clone())
                            .is_some()
                    {
                        return Err(Corruption::new(format!(
                            "invalid duplicate or cross-operation provider settlement {}",
                            record.request_id
                        )));
                    }
                }
                LaneRecord::ToolStarted(record) => {
                    let operation = open_operation(&operations, &record.operation_id)?;
                    if epochs.get(&record.epoch_id) != Some(&record.operation_id)
                        || !operation.open_epochs.contains(&record.epoch_id)
                    {
                        return Err(Corruption::new(format!(
                            "tool intent {} refers to a closed or invalid epoch",
                            record.record_id
                        )));
                    }
                    validate_tool_started(record, &entries, &tool_starts)?;
                    tool_starts.push((stored.seq, record.clone()));
                }
                LaneRecord::InputAccepted(record) => {
                    fresh_lanes.remove(&record.lane_id);
                    if !lane_leaves.contains_key(&record.lane_id) {
                        return Err(Corruption::new(format!(
                            "accepted input {} targets unknown lane {}",
                            record.entry.id, record.lane_id
                        )));
                    }
                    if !matches!(&record.entry.body, SessionEntry::UserMessage(_)) {
                        return Err(Corruption::new(format!(
                            "accepted input {} is not a user message",
                            record.entry.id
                        )));
                    }
                    if entries.contains_key(&record.entry.id)
                        || input_states.contains_key(&record.entry.id)
                    {
                        return Err(Corruption::new(format!(
                            "input {} was accepted more than once or already materialized",
                            record.entry.id
                        )));
                    }
                    input_order.push(record.entry.id.clone());
                    input_states.insert(
                        record.entry.id.clone(),
                        InputState {
                            accepted: crate::AcceptedInput {
                                lane_id: record.lane_id.clone(),
                                entry: record.entry.clone(),
                                accepted_sequence: stored.seq,
                            },
                            status: InputStatus::Pending,
                        },
                    );
                }
                LaneRecord::InputWithdrawn(record) => {
                    let Some(input) = input_states.get_mut(&record.input_id) else {
                        return Err(Corruption::new(format!(
                            "input {} was withdrawn before acceptance",
                            record.input_id
                        )));
                    };
                    if input.accepted.lane_id != record.lane_id
                        || input.status != InputStatus::Pending
                    {
                        return Err(Corruption::new(format!(
                            "input {} is not pending on lane {}",
                            record.input_id, record.lane_id
                        )));
                    }
                    input.status = InputStatus::Withdrawn;
                }
                LaneRecord::InputSettled(record) => {
                    let Some(operation) = operations.get(&record.operation_id) else {
                        return Err(Corruption::new(format!(
                            "input {} settles unknown operation {}",
                            record.input_id, record.operation_id
                        )));
                    };
                    if !operation.finished || operation.outcome.as_ref() != Some(&record.outcome) {
                        return Err(Corruption::new(format!(
                            "input {} settles before or contrary to operation {}",
                            record.input_id, record.operation_id
                        )));
                    }
                    let Some(input) = input_states.get_mut(&record.input_id) else {
                        return Err(Corruption::new(format!(
                            "input {} settled without acceptance",
                            record.input_id
                        )));
                    };
                    if input.status
                        != (InputStatus::Dispatched {
                            operation_id: record.operation_id.clone(),
                        })
                    {
                        return Err(Corruption::new(format!(
                            "input {} is not dispatched by operation {}",
                            record.input_id, record.operation_id
                        )));
                    }
                    input.status = InputStatus::Settled {
                        operation_id: record.operation_id.clone(),
                        outcome: record.outcome.clone(),
                    };
                }
                LaneRecord::ExtensionControlEnqueued(record) => {
                    let _ = open_operation(&operations, &record.operation_id)?;
                    validate_extension_control(record)?;
                    if extension_controls
                        .insert(
                            record.control_id.clone(),
                            PendingExtensionControl {
                                accepted_sequence: stored.seq,
                                accepted_item_index: item_index,
                                control: record.clone(),
                            },
                        )
                        .is_some()
                    {
                        return Err(Corruption::new(format!(
                            "extension control {} was accepted more than once",
                            record.control_id
                        )));
                    }
                }
                LaneRecord::ExtensionControlApplied(record) => {
                    let Some(control) = extension_controls.remove(&record.control_id) else {
                        return Err(Corruption::new(format!(
                            "extension control {} was applied before acceptance or twice",
                            record.control_id
                        )));
                    };
                    let Some(operation) = operations.get(&control.control.operation_id) else {
                        return Err(Corruption::new(format!(
                            "extension control {} targets unknown operation",
                            record.control_id
                        )));
                    };
                    if !operation.finished
                        || !applied_extension_controls.insert(record.control_id.clone())
                    {
                        return Err(Corruption::new(format!(
                            "extension control {} was applied before its operation settled",
                            record.control_id
                        )));
                    }
                }
                LaneRecord::WriteDeferred(record) => {
                    let _ = open_operation(&operations, &record.operation_id)?;
                    deferred_writes.push(crate::PendingWrite {
                        operation_id: record.operation_id.clone(),
                        entry: record.entry.clone(),
                    });
                }
                LaneRecord::HarnessActivationRequested(record) => {
                    let _ = open_operation(&operations, &record.operation_id)?;
                    activation_requests.push(record.clone());
                }
                LaneRecord::Usage(record) => {
                    let operation = open_operation(&operations, &record.operation_id)?;
                    usage_by_lane
                        .entry(operation.lane_id.clone())
                        .or_default()
                        .saturating_add_assign(&record.usage);
                }
            },
            SessionMutationRef::Fact(stored) => match &stored.fact {
                SessionFact::ProviderRequestMaterial(fact) => {
                    let Some(start) = provider_starts.get(&fact.request_id) else {
                        return Err(Corruption::new(format!(
                            "provider request material {} has no request intent",
                            fact.request_id
                        )));
                    };
                    if start.operation_id != fact.operation_id
                        || start.epoch_id != fact.epoch_id
                        || provider_settled.contains_key(&fact.request_id)
                        || !provider_material.insert(fact.request_id.clone())
                    {
                        return Err(Corruption::new(format!(
                            "provider request material {} is duplicate, late, or cross-owned",
                            fact.request_id
                        )));
                    }
                }
                SessionFact::ExtensionStateValueSet(fact) => {
                    fresh_lanes.remove(&fact.lane_id);
                    validate_extension_state_value(fact)?;
                    let Some(state) = extension_state.get_mut(&fact.lane_id) else {
                        return Err(Corruption::new(format!(
                            "extension state targets unknown lane {}",
                            fact.lane_id
                        )));
                    };
                    state.insert(
                        fact.extension_id.clone(),
                        crate::ExtensionStateValue {
                            state_version: fact.state_version.clone(),
                            value: fact.value.clone(),
                        },
                    );
                }
                SessionFact::TurnCheckpoint(fact) => {
                    if checkpoints.contains_key(&fact.checkpoint_id) {
                        return Err(Corruption::new(format!(
                            "turn checkpoint {} was recorded more than once",
                            fact.checkpoint_id
                        )));
                    }
                    if checkpoint_ready_for.as_ref() != Some(&fact.operation_id) {
                        return Err(Corruption::new(format!(
                            "turn checkpoint {} does not immediately follow settlement of operation {}",
                            fact.checkpoint_id, fact.operation_id
                        )));
                    }
                    let Some(operation) = operations.get(&fact.operation_id) else {
                        return Err(Corruption::new(format!(
                            "turn checkpoint {} refers to unknown operation {}",
                            fact.checkpoint_id, fact.operation_id
                        )));
                    };
                    if !operation.finished || operation.lane_id != fact.lane_id {
                        return Err(Corruption::new(format!(
                            "turn checkpoint {} does not name a settled operation on its lane",
                            fact.checkpoint_id
                        )));
                    }
                    if lane_leaves.get(&fact.lane_id).cloned().flatten() != fact.leaf_id {
                        return Err(Corruption::new(format!(
                            "turn checkpoint {} does not capture the current lane leaf",
                            fact.checkpoint_id
                        )));
                    }
                    if tool_starts.iter().any(|(_, tool)| {
                        tool.operation_id == fact.operation_id
                            && !entries.contains_key(&tool.result_entry_id)
                    }) || provider_starts.values().any(|request| {
                        request.operation_id == fact.operation_id
                            && !provider_settled.contains_key(&request.request_id)
                    }) || extension_controls
                        .values()
                        .any(|control| control.control.operation_id == fact.operation_id)
                        || child_spawns.iter().any(|(agent_id, (operation_id, _))| {
                            operation_id == &fact.operation_id && !child_terminal.contains(agent_id)
                        })
                    {
                        return Err(Corruption::new(format!(
                            "turn checkpoint {} has unresolved work owned by operation {}",
                            fact.checkpoint_id, fact.operation_id
                        )));
                    }
                    if extension_state.get(&fact.lane_id) != Some(&fact.extension_state) {
                        return Err(Corruption::new(format!(
                            "turn checkpoint {} does not capture the exact extension state",
                            fact.checkpoint_id
                        )));
                    }
                    checkpoints.insert(fact.checkpoint_id.clone(), fact.clone());
                    checkpoint_ready_for = None;
                }
                SessionFact::ForkedLane(fact) => {
                    let Some(checkpoint) = checkpoints.get(&fact.checkpoint_id) else {
                        return Err(Corruption::new(format!(
                            "forked lane {} refers to unknown checkpoint {}",
                            fact.lane_id, fact.checkpoint_id
                        )));
                    };
                    if !fresh_lanes.remove(&fact.lane_id)
                        || !forked_lanes.insert(fact.lane_id.clone())
                        || lane_leaves.get(&fact.lane_id).cloned().flatten() != fact.base_leaf_id
                        || fact.base_leaf_id != checkpoint.leaf_id
                        || operations
                            .values()
                            .any(|operation| operation.lane_id == fact.lane_id)
                    {
                        return Err(Corruption::new(format!(
                            "forked lane {} is not a fresh exact checkpoint branch",
                            fact.lane_id
                        )));
                    }
                    extension_state
                        .insert(fact.lane_id.clone(), checkpoint.extension_state.clone());
                }
                SessionFact::AgentSpawned(fact) => {
                    fresh_lanes.remove(&fact.lane_id);
                    if child_spawns
                        .insert(
                            fact.agent_id.clone(),
                            (fact.parent_operation_id.clone(), stored.seq),
                        )
                        .is_some()
                    {
                        return Err(Corruption::new(format!(
                            "agent {} was spawned more than once",
                            fact.agent_id
                        )));
                    }
                }
                SessionFact::AgentTaskFinished(fact) => {
                    child_terminal.insert(fact.agent_id.clone());
                }
                SessionFact::SubagentPolicy(_)
                | SessionFact::WorkspaceDelta(_)
                | SessionFact::WorkspaceDeltaApplied(_)
                | SessionFact::HarnessCatalog(_)
                | SessionFact::ToolSchemaDeviation(_)
                | SessionFact::TraceArtifact(_)
                | SessionFact::Custom { .. } => {}
            },
        }
    }

    if last_sequence != observed_sequence {
        return Err(Corruption::new(
            "snapshot last sequence disagrees with mutation timeline",
        ));
    }
    if !lane_leaves.contains_key(&lane) {
        return Err(Corruption::new(format!("unknown lane {lane}")));
    }

    validate_tool_results(&entries, &tool_starts)?;
    let effective_configuration =
        derive_configuration(&entries, lane_leaves.get(&lane).cloned().flatten())?;
    let active_operation = active_operations.get(&lane).cloned();
    let pending_writes = deferred_writes
        .into_iter()
        .filter(|pending| !entries.contains_key(&pending.entry.id))
        .collect::<Vec<_>>();
    let input_states = input_states
        .into_iter()
        .filter(|(_, input)| input.accepted.lane_id == lane)
        .collect::<BTreeMap<_, _>>();
    let pending_inputs = input_order
        .into_iter()
        .filter_map(|input_id| input_states.get(&input_id).cloned())
        .filter_map(|input| (input.status == InputStatus::Pending).then_some(input.accepted))
        .collect();
    let mut pending_extension_controls = extension_controls
        .into_values()
        .filter(|control| {
            operations
                .get(&control.control.operation_id)
                .is_some_and(|operation| operation.lane_id == lane)
        })
        .collect::<Vec<_>>();
    pending_extension_controls
        .sort_by_key(|control| (control.accepted_sequence, control.accepted_item_index));
    let pending_harness_activation =
        unresolved_activation(&activation_requests, &entries, active_operation.as_ref())?;
    let recovery_plan = derive_recovery_plan(
        &entries,
        &operations,
        &tool_starts,
        &provider_starts,
        &provider_settled,
        &provider_material,
        &activation_requests,
        active_operation.as_ref(),
    )?;
    let lane_state = LaneState {
        lane_id: lane.clone(),
        leaf_id: lane_leaves.get(&lane).cloned().flatten(),
        status: if active_operation.is_some() {
            LaneStatus::Running
        } else {
            LaneStatus::Idle
        },
        active_operation,
        active_harness_revision: effective_configuration.harness_revision.clone(),
    };

    Ok(LaneReduction {
        lane_state,
        effective_configuration,
        recovery_plan,
        input_reduction: InputReduction {
            pending_inputs,
            input_states,
        },
        pending_extension_controls,
        extension_state: extension_state.remove(&lane).unwrap_or_default(),
        pending_writes,
        pending_harness_activation,
        usage_totals: usage_by_lane.remove(&lane).unwrap_or_default(),
    })
}

fn open_operation<'a>(
    operations: &'a BTreeMap<OperationId, OperationState>,
    id: &OperationId,
) -> Result<&'a OperationState, Corruption> {
    let Some(operation) = operations.get(id) else {
        return Err(Corruption::new(format!(
            "record refers to unknown operation {id}"
        )));
    };
    if operation.finished {
        return Err(Corruption::new(format!(
            "record follows terminal operation {id}"
        )));
    }
    Ok(operation)
}

fn open_operation_mut<'a>(
    operations: &'a mut BTreeMap<OperationId, OperationState>,
    id: &OperationId,
) -> Result<&'a mut OperationState, Corruption> {
    let Some(operation) = operations.get_mut(id) else {
        return Err(Corruption::new(format!(
            "record refers to unknown operation {id}"
        )));
    };
    if operation.finished {
        return Err(Corruption::new(format!(
            "record follows terminal operation {id}"
        )));
    }
    Ok(operation)
}

fn validate_tool_started(
    record: &ToolStartedRecord,
    entries: &BTreeMap<EntryId, StoredEntry>,
    prior: &[(Sequence, ToolStartedRecord)],
) -> Result<(), Corruption> {
    if record.tool_call_id.is_empty()
        || record.tool_name.is_empty()
        || record.idempotency_key.is_empty()
    {
        return Err(Corruption::new(format!(
            "tool intent {} has an empty durable identity field",
            record.record_id
        )));
    }
    let Some(assistant) = entries.get(&record.assistant_entry_id) else {
        return Err(Corruption::new(format!(
            "tool intent {} refers to missing assistant entry {}",
            record.record_id, record.assistant_entry_id
        )));
    };
    let SessionEntry::AssistantMessage(assistant) = &assistant.body else {
        return Err(Corruption::new(format!(
            "tool intent {} source entry is not an assistant message",
            record.record_id
        )));
    };
    let Some(call) = assistant.tool_calls.get(record.tool_index as usize) else {
        return Err(Corruption::new(format!(
            "tool intent {} source ordinal {} is absent",
            record.record_id, record.tool_index
        )));
    };
    if call.id != record.tool_call_id || call.name != record.tool_name {
        return Err(Corruption::new(format!(
            "tool intent {} does not match its durable assistant call position",
            record.record_id
        )));
    }
    if prior.iter().any(|(_, prior)| {
        prior.assistant_entry_id == record.assistant_entry_id
            && prior.tool_index == record.tool_index
    }) {
        return Err(Corruption::new(format!(
            "tool invocation {}:{} was started more than once",
            record.assistant_entry_id, record.tool_index
        )));
    }
    if prior
        .iter()
        .any(|(_, prior)| prior.result_entry_id == record.result_entry_id)
    {
        return Err(Corruption::new(format!(
            "provisioned tool result entry {} is reused",
            record.result_entry_id
        )));
    }
    Ok(())
}

fn validate_tool_results(
    entries: &BTreeMap<EntryId, StoredEntry>,
    starts: &[(Sequence, ToolStartedRecord)],
) -> Result<(), Corruption> {
    for (_, start) in starts {
        if let Some(entry) = entries.get(&start.result_entry_id) {
            let SessionEntry::ToolResult(result) = &entry.body else {
                return Err(Corruption::new(format!(
                    "provisioned tool result entry {} materialized with a non-tool-result body",
                    start.result_entry_id
                )));
            };
            if result.tool_call_id != start.tool_call_id || result.tool_name != start.tool_name {
                return Err(Corruption::new(format!(
                    "tool result {} disagrees with its durable effect intent",
                    start.result_entry_id
                )));
            }
        }
    }
    for (entry_id, entry) in entries {
        let SessionEntry::ToolResult(result) = &entry.body else {
            continue;
        };
        let matching_start = starts
            .iter()
            .find(|(_, started)| started.result_entry_id == *entry_id)
            .map(|(_, started)| started);
        if let Some(start) = matching_start {
            if result.tool_call_id != start.tool_call_id || result.tool_name != start.tool_name {
                return Err(Corruption::new(format!(
                    "tool result {entry_id} disagrees with its durable effect intent"
                )));
            }
            continue;
        }
        if !any_assistant_call_matches(entries, &result.tool_call_id, &result.tool_name) {
            return Err(Corruption::new(format!(
                "tool result {entry_id} has no matching assistant tool call"
            )));
        }
    }
    Ok(())
}

fn any_assistant_call_matches(
    entries: &BTreeMap<EntryId, StoredEntry>,
    call_id: &str,
    tool_name: &str,
) -> bool {
    entries.values().any(|entry| match &entry.body {
        SessionEntry::AssistantMessage(assistant) => assistant
            .tool_calls
            .iter()
            .any(|call| call.id == call_id && call.name == tool_name),
        _ => false,
    })
}

fn validate_extension_state_value(
    fact: &crate::ExtensionStateValueSetFact,
) -> Result<(), Corruption> {
    validate_bounded_extension_identifier("extension state namespace", &fact.extension_id)?;
    validate_bounded_extension_identifier("extension state version", &fact.state_version)?;
    let bytes = fact
        .value
        .to_json_string()
        .map_err(|error| {
            Corruption::new(format!(
                "extension state cannot encode canonically: {error}"
            ))
        })?
        .len();
    if bytes > 64 * 1024 {
        return Err(Corruption::new(format!(
            "extension state value exceeds the 65536-byte limit ({bytes})"
        )));
    }
    Ok(())
}

fn validate_compaction_replacement(entry: &crate::CompactionEntry) -> Result<(), Corruption> {
    let digest = match &entry.replacement {
        crate::PayloadRef::Inline(value) => {
            if matches!(value, crate::JsonValue::Null) {
                return Err(Corruption::new("compaction replacement must not be null"));
            }
            let canonical = value.to_json_string().map_err(|error| {
                Corruption::new(format!(
                    "compaction replacement cannot encode canonically: {error}"
                ))
            })?;
            crate::Digest::from_bytes(canonical)
        }
        crate::PayloadRef::Artifact { artifact_id, .. } => artifact_id.digest(),
    };
    if digest != entry.replacement_digest {
        return Err(Corruption::new(
            "compaction replacement digest does not match its exact payload",
        ));
    }
    Ok(())
}

fn validate_extension_control(
    record: &crate::ExtensionControlEnqueuedRecord,
) -> Result<(), Corruption> {
    validate_bounded_extension_identifier("extension control ID", &record.control_id)?;
    validate_bounded_extension_identifier("extension control extension ID", &record.extension_id)?;
    validate_bounded_extension_identifier("extension control command name", &record.command_name)?;
    let bytes = record
        .arguments
        .to_json_string()
        .map_err(|error| {
            Corruption::new(format!(
                "extension control cannot encode canonically: {error}"
            ))
        })?
        .len();
    if bytes > 64 * 1024 {
        return Err(Corruption::new(format!(
            "extension control arguments exceed the 65536-byte limit ({bytes})"
        )));
    }
    Ok(())
}

fn validate_bounded_extension_identifier(label: &str, value: &str) -> Result<(), Corruption> {
    if value.is_empty() || value.len() > 200 || value.chars().any(char::is_control) {
        return Err(Corruption::new(format!(
            "{label} must be nonempty, non-control text within 200 bytes"
        )));
    }
    Ok(())
}

fn derive_configuration(
    entries: &BTreeMap<EntryId, StoredEntry>,
    leaf: Option<EntryId>,
) -> Result<EffectiveLaneConfiguration, Corruption> {
    let mut reverse_chain = Vec::new();
    let mut cursor = leaf;
    while let Some(entry_id) = cursor {
        let Some(entry) = entries.get(&entry_id) else {
            return Err(Corruption::new(format!(
                "lane leaf refers to missing entry {entry_id}"
            )));
        };
        reverse_chain.push(entry.clone());
        cursor = entry.header.parent_id.clone();
    }
    reverse_chain.reverse();
    let mut configuration = EffectiveLaneConfiguration::default();
    for entry in reverse_chain {
        match entry.body {
            SessionEntry::ModelChanged(model) => configuration.model = Some(model),
            SessionEntry::ThinkingChanged(thinking) => {
                configuration.thinking_level = Some(thinking.level)
            }
            SessionEntry::ToolActivationChanged(tools) => {
                configuration.active_tool_names = tools.active_tool_names
            }
            SessionEntry::HarnessRevisionChanged(revision) => {
                configuration.harness_revision = Some(revision.revision_id)
            }
            _ => {}
        }
    }
    Ok(configuration)
}

fn unresolved_activation(
    requests: &[crate::HarnessActivationRequestedRecord],
    entries: &BTreeMap<EntryId, StoredEntry>,
    active_operation: Option<&OperationId>,
) -> Result<Option<PendingHarnessActivation>, Corruption> {
    let Some(operation_id) = active_operation else {
        return Ok(None);
    };
    let mut pending = None;
    for request in requests
        .iter()
        .filter(|request| &request.operation_id == operation_id)
    {
        match entries.get(&request.revision_entry_id) {
            None => {
                if pending.is_some() {
                    return Err(Corruption::new(
                        "operation has more than one unresolved harness activation",
                    ));
                }
                pending = Some(PendingHarnessActivation {
                    request: request.clone(),
                });
            }
            Some(StoredEntry {
                body:
                    SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                        revision_id,
                        snapshot_id,
                        ..
                    }),
                ..
            }) if revision_id == &request.parent_revision_id
                || snapshot_id != &request.proposed_snapshot_id =>
            {
                return Err(Corruption::new(
                    "activation entry must name a new revision and the requested snapshot",
                ));
            }
            Some(StoredEntry {
                body: SessionEntry::HarnessRevisionChanged(_),
                ..
            }) => {}
            Some(_) => {
                return Err(Corruption::new(
                    "activation provisioned entry materialized with a different semantic type",
                ));
            }
        }
    }
    Ok(pending)
}

#[allow(clippy::too_many_arguments)]
fn derive_recovery_plan(
    entries: &BTreeMap<EntryId, StoredEntry>,
    operations: &BTreeMap<OperationId, OperationState>,
    tool_starts: &[(Sequence, ToolStartedRecord)],
    provider_starts: &BTreeMap<crate::ProviderRequestId, crate::ProviderRequestStartedRecord>,
    provider_settled: &BTreeMap<crate::ProviderRequestId, crate::ProviderSettlementClassification>,
    provider_material: &BTreeSet<crate::ProviderRequestId>,
    activation_requests: &[crate::HarnessActivationRequestedRecord],
    active_operation: Option<&OperationId>,
) -> Result<Option<RecoveryPlan>, Corruption> {
    let Some(operation_id) = active_operation else {
        return Ok(None);
    };
    let operation = operations
        .get(operation_id)
        .expect("active operation was inserted");
    let missing_input = operation
        .original_input
        .iter()
        .filter(|entry| !entries.contains_key(&entry.id))
        .cloned()
        .collect::<Vec<_>>();
    if !missing_input.is_empty() {
        return Ok(Some(RecoveryPlan::AppendAcceptedInput {
            operation_id: operation_id.clone(),
            entries: missing_input,
        }));
    }

    for (_, tool) in tool_starts
        .iter()
        .filter(|(_, tool)| &tool.operation_id == operation_id)
    {
        if !entries.contains_key(&tool.result_entry_id) {
            return Ok(Some(match tool.replay_policy_at_start {
                ToolReplayPolicy::Never => RecoveryPlan::ReconcileToolEffect {
                    result_entry_id: tool.result_entry_id.clone(),
                },
                ToolReplayPolicy::Safe => {
                    RecoveryPlan::ReplayToolIfStillSafe { tool: tool.clone() }
                }
            }));
        }
    }

    for request in provider_starts.values() {
        if &request.operation_id != operation_id
            || provider_settled.contains_key(&request.request_id)
        {
            continue;
        }
        if !provider_material.contains(&request.request_id) {
            return Ok(Some(RecoveryPlan::ProviderRequestNotAdmitted {
                request_id: request.request_id.clone(),
            }));
        }
        return Ok(Some(RecoveryPlan::ReconcileProviderRequest {
            request_id: request.request_id.clone(),
        }));
    }

    for request in activation_requests
        .iter()
        .filter(|request| &request.operation_id == operation_id)
    {
        if !entries.contains_key(&request.revision_entry_id) && operation.open_epochs.is_empty() {
            return Ok(Some(RecoveryPlan::ActivateHarness {
                request: request.clone(),
            }));
        }
    }

    for entry_id in operation.entry_ids.iter().rev() {
        let Some(entry) = entries.get(entry_id) else {
            continue;
        };
        let SessionEntry::AssistantMessage(assistant) = &entry.body else {
            continue;
        };
        if assistant.tool_calls.iter().any(|call| {
            !entries.values().any(|entry| {
                matches!(
                    &entry.body,
                    SessionEntry::ToolResult(result)
                        if result.tool_call_id == call.id && result.tool_name == call.name
                )
            })
        }) {
            return Ok(Some(RecoveryPlan::ResumeAssistantToolPath {
                assistant_entry_id: entry_id.clone(),
            }));
        }
    }

    if operation.epoch_ids.is_empty() || operation.open_epochs.is_empty() {
        return Ok(Some(RecoveryPlan::StartEpoch {
            operation_id: operation_id.clone(),
        }));
    }
    Ok(Some(RecoveryPlan::ResumeOperation {
        operation_id: operation_id.clone(),
    }))
}
