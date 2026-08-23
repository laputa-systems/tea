use crate::{
    EffectiveLaneConfiguration, EntryId, EpochId, HarnessRevisionChangedEntry, LaneId, LaneRecord,
    LaneState, LaneStatus, OperationId, PendingHarnessActivation, PendingQueues, PendingWrite,
    ProvisionedEntry, Sequence, SessionEntry, SessionSnapshot, StepId, StepKind, StoredEntry,
    StoredMutation, ToolReplayPolicy, ToolStartedRecord, Usage,
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
    fn new(message: impl Into<String>) -> Self {
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
    /// A `Never` effect intent has no durable result; append an explicit
    /// ambiguity result rather than claiming the effect failed or replaying it.
    SynthesizeInterruptedToolResult {
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
    /// Queue items accepted minus cancellation facts.
    pub pending_queues: PendingQueues,
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
    epoch_ids: Vec<EpochId>,
    open_epochs: BTreeSet<EpochId>,
    entry_ids: Vec<EntryId>,
}

/// Reduce one complete session snapshot without reading clocks, files, hooks,
/// providers, tools, or mutable live state.
pub fn reduce_lane(input: SessionSnapshot, lane: LaneId) -> Result<LaneReduction, Corruption> {
    let mut lane_leaves = BTreeMap::<LaneId, Option<EntryId>>::new();
    lane_leaves.insert(input.header().initial_lane.clone(), None);

    let mut entries = BTreeMap::<EntryId, StoredEntry>::new();
    let mut operations = BTreeMap::<OperationId, OperationState>::new();
    let mut active_operations = BTreeMap::<LaneId, OperationId>::new();
    let mut epochs = BTreeMap::<EpochId, OperationId>::new();
    let mut step_attempts = BTreeMap::<(OperationId, EpochId, StepKind), (StepId, u32)>::new();
    let mut provisioned_entries = BTreeMap::<EntryId, ProvisionedEntry>::new();
    let mut tool_starts = Vec::new();
    let mut provider_starts = BTreeMap::new();
    let mut provider_settled = BTreeSet::new();
    let mut queues = PendingQueues::default();
    let mut deferred_writes = Vec::new();
    let mut activation_requests = Vec::new();
    let mut usage_by_lane = BTreeMap::<LaneId, Usage>::new();
    let mut expected_sequence = 1_u64;

    for mutation in input.mutations() {
        let sequence = mutation.sequence();
        if sequence != Sequence(expected_sequence) {
            return Err(Corruption::new(format!(
                "session sequence must be consecutive: expected {}, found {}",
                expected_sequence, sequence.0
            )));
        }
        expected_sequence = expected_sequence.saturating_add(1);

        match mutation {
            StoredMutation::Lane(stored) => match &stored.mutation {
                crate::LaneMutation::Created {
                    lane_id,
                    base_leaf_id,
                } => {
                    if lane_leaves.contains_key(lane_id) {
                        return Err(Corruption::new(format!("duplicate lane ID {lane_id}")));
                    }
                    if let Some(base_leaf_id) = base_leaf_id
                        && !entries.contains_key(base_leaf_id) {
                            return Err(Corruption::new(format!(
                                "lane {lane_id} refers to missing base entry {base_leaf_id}"
                            )));
                        }
                    lane_leaves.insert(lane_id.clone(), base_leaf_id.clone());
                }
            },
            StoredMutation::Entry(stored) => {
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
                    && provisioned.body != stored.body {
                        return Err(Corruption::new(format!(
                            "provisioned entry {} materialized with different content",
                            stored.header.id
                        )));
                    }
                if let Some(parent_id) = &stored.header.parent_id
                    && !entries.contains_key(parent_id) {
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
            StoredMutation::Record(stored) => match &stored.record {
                LaneRecord::OperationStarted(record) => {
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
                    operations.insert(
                        record.id.clone(),
                        OperationState {
                            lane_id: record.lane_id.clone(),
                            original_input: record.original_input.clone(),
                            finished: false,
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
                    match active_operations.remove(&operation.lane_id) {
                        Some(current) if current == record.operation_id => {}
                        _ => {
                            return Err(Corruption::new(format!(
                                "operation {} was not active on its lane at finish",
                                record.operation_id
                            )));
                        }
                    }
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
                        || !provider_settled.insert(record.request_id.clone())
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
                LaneRecord::QueueEnqueued(record) => {
                    let _ = open_operation(&operations, &record.operation_id)?;
                    let items = queues.items.entry(record.operation_id.clone()).or_default();
                    if !items.insert(record.queue_item_id.clone()) {
                        return Err(Corruption::new(format!(
                            "queue item {} was accepted twice",
                            record.queue_item_id
                        )));
                    }
                }
                LaneRecord::QueueCancelled(record) => {
                    let _ = open_operation(&operations, &record.operation_id)?;
                    let Some(items) = queues.items.get_mut(&record.operation_id) else {
                        return Err(Corruption::new(format!(
                            "queue item {} was cancelled before acceptance",
                            record.queue_item_id
                        )));
                    };
                    if !items.remove(&record.queue_item_id) {
                        return Err(Corruption::new(format!(
                            "queue item {} was cancelled more than once or is unknown",
                            record.queue_item_id
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
            StoredMutation::Fact(_) => {}
        }
    }

    if input.last_sequence().0.saturating_add(1) != expected_sequence {
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
    let pending_harness_activation =
        unresolved_activation(&activation_requests, &entries, active_operation.as_ref())?;
    let recovery_plan = derive_recovery_plan(
        &entries,
        &operations,
        &tool_starts,
        &provider_starts,
        &provider_settled,
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
        pending_queues: queues,
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

fn derive_recovery_plan(
    entries: &BTreeMap<EntryId, StoredEntry>,
    operations: &BTreeMap<OperationId, OperationState>,
    tool_starts: &[(Sequence, ToolStartedRecord)],
    provider_starts: &BTreeMap<crate::ProviderRequestId, crate::ProviderRequestStartedRecord>,
    provider_settled: &BTreeSet<crate::ProviderRequestId>,
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
                ToolReplayPolicy::Never => RecoveryPlan::SynthesizeInterruptedToolResult {
                    result_entry_id: tool.result_entry_id.clone(),
                },
                ToolReplayPolicy::Safe => {
                    RecoveryPlan::ReplayToolIfStillSafe { tool: tool.clone() }
                }
            }));
        }
    }

    for request in provider_starts.values() {
        if &request.operation_id == operation_id && !provider_settled.contains(&request.request_id)
        {
            return Ok(Some(RecoveryPlan::ReconcileProviderRequest {
                request_id: request.request_id.clone(),
            }));
        }
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
