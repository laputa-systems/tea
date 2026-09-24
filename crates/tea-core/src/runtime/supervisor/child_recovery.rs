//! Explicit reconciliation for interrupted root-owned child assignments.
//!
//! Reopening a supervisor reconstructs durable graph state only. This module
//! runs later, after the root recovery gate authorizes an explicit
//! continuation. It reconciles a retained child workspace into a truthful
//! terminal result, but never restores its provider drive, task handle, or
//! checkpointed epoch.

use super::{
    FinalizeSubagentRequest, SessionSupervisor, SubagentCoordinator, WorkspaceFinalization,
    child_operation_outcome, open_epoch, retain_tool_result_with_projection, subagent_entry_id,
    subagent_host_stage_error, tool_result_entry,
};
use crate::harness::{HarnessError, SubagentRecoveryStage};
use crate::runtime::subagents::{
    ApplyAgentChangesResult, SpawnedAgentHandle, apply_result_value, parse_apply_delta_id_value,
    spawn_result_value,
};
use crate::state::ToolCallId;
use crate::tool::AgentToolResult;
use std::sync::Arc;
use tea_protocol::JsonValue;
use tea_session::{
    AgentGraphNode, EpochFinishReason, EpochFinishedRecord, LaneRecord, OperationFinishedRecord,
    OperationId, OperationOutcome, PayloadRef, ProvisionedEntry, RecoveryPlan, SessionCommit,
    SessionCommitItem, SessionEntry, SessionSnapshot, SessionWriter, ToolStartedRecord,
    reduce_agent_graph, reduce_lane,
};

const INTERRUPTED_CHILD_REPORT: &str = "Child execution was interrupted before it reached a settled report. Tea did not resume the prior assignment.";

/// An ordinary tool result reconstructed from a stronger, already committed
/// child fact. The host effect itself is never replayed here.
struct CommittedChildToolOutcome {
    started: ToolStartedRecord,
    result: AgentToolResult,
}

impl<S> SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Restore ordinary root tool-result entries when a stronger child fact
    /// already proves the exact effect outcome.
    ///
    /// This handles only a fully accepted `spawn_agent` assignment and an
    /// `apply_agent_changes` intent with an exact `WorkspaceDeltaApplied`
    /// fact. In particular, a prepared-only child, conflict, rollback,
    /// indeterminate host outcome, or any mismatched fact remains unresolved
    /// for explicit host reconciliation. This method contacts no child host,
    /// creates no task, and never reapplies a workspace delta.
    pub(crate) fn reconcile_committed_child_tool_outcomes(
        &self,
        lane_id: &tea_session::LaneId,
    ) -> Result<(), HarnessError> {
        if lane_id != &self.root_lane_id {
            return Ok(());
        }
        let lane = self.lane(lane_id)?;
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let outcomes = self.committed_child_tool_outcomes(&snapshot, lane_id)?;
        if outcomes.is_empty() {
            return Ok(());
        }

        let mut items = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            let configuration =
                self.configuration_for_revision(&lane, &outcome.started.harness_revision_id)?;
            let retained = retain_tool_result_with_projection(
                self.artifacts.as_ref(),
                configuration.artifact_policy_config(),
                &outcome.result,
                &outcome.result,
            )
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let entry = tool_result_entry(
                &outcome.result,
                &outcome.result,
                &outcome.started.tool_name,
                retained,
            )
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            items.push(SessionCommitItem::Entry {
                lane_id: lane_id.clone(),
                entry: ProvisionedEntry {
                    id: outcome.started.result_entry_id,
                    body: SessionEntry::ToolResult(entry),
                },
            });
        }
        session.commit(SessionCommit::new(items)?)?;
        Ok(())
    }

    fn committed_child_tool_outcomes(
        &self,
        snapshot: &SessionSnapshot,
        lane_id: &tea_session::LaneId,
    ) -> Result<Vec<CommittedChildToolOutcome>, HarnessError> {
        let graph = reduce_agent_graph(snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let lane_operations = snapshot
            .records()
            .iter()
            .filter_map(|stored| match &stored.record {
                LaneRecord::OperationStarted(operation) if &operation.lane_id == lane_id => {
                    Some(&operation.id)
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut outcomes = Vec::new();
        for stored in snapshot.records() {
            let LaneRecord::ToolStarted(started) = &stored.record else {
                continue;
            };
            if !lane_operations.contains(&started.operation_id)
                || snapshot
                    .entries()
                    .iter()
                    .any(|entry| entry.header.id == started.result_entry_id)
            {
                continue;
            }
            let value = match started.tool_name.as_str() {
                "spawn_agent" => self.committed_spawn_result(snapshot, lane_id, started, &graph),
                "apply_agent_changes" => self.committed_apply_result(lane_id, started, &graph),
                _ => None,
            };
            let Some(value) = value else {
                continue;
            };
            outcomes.push(CommittedChildToolOutcome {
                started: started.clone(),
                result: committed_child_tool_result(started, value)?,
            });
        }
        Ok(outcomes)
    }

    fn committed_spawn_result(
        &self,
        snapshot: &SessionSnapshot,
        lane_id: &tea_session::LaneId,
        started: &ToolStartedRecord,
        graph: &tea_session::AgentGraphReduction,
    ) -> Option<JsonValue> {
        let agent_id = tea_session::AgentId::derive(
            &snapshot.header().session_id,
            lane_id,
            &started.operation_id,
            &started.idempotency_key,
        );
        let node = graph.agents.get(&agent_id)?;
        let operation_id = node.operation_id.clone()?;
        (node.spawned.parent_lane_id == *lane_id
            && node.spawned.parent_operation_id == started.operation_id
            && node.spawned.spawn_tool_call_id == started.tool_call_id)
            .then(|| {
                spawn_result_value(&SpawnedAgentHandle {
                    agent_id,
                    operation_id,
                    task_name: node.spawned.task_name.clone(),
                    state: node.state.clone(),
                })
            })
    }

    fn committed_apply_result(
        &self,
        lane_id: &tea_session::LaneId,
        started: &ToolStartedRecord,
        graph: &tea_session::AgentGraphReduction,
    ) -> Option<JsonValue> {
        let delta_id = parse_apply_delta_id_value(&started.effective_args).ok()?;
        let node = graph.agents.values().find(|node| {
            node.spawned.parent_lane_id == *lane_id
                && node.spawned.parent_operation_id == started.operation_id
                && node
                    .workspace_delta
                    .as_ref()
                    .is_some_and(|delta| delta.delta_id == delta_id)
        })?;
        let delta = node.workspace_delta.as_ref()?;
        let applied = node.applied.as_ref()?;
        (applied.delta_id == delta_id
            && applied.target_lane_id == *lane_id
            && applied.tool_call_id == started.tool_call_id
            && applied.changed_paths == delta.changed_paths)
            .then(|| {
                apply_result_value(ApplyAgentChangesResult::Applied {
                    delta_id: applied.delta_id.clone(),
                    changed_paths: applied.changed_paths.clone(),
                })
            })
    }

    /// Reconcile children of the currently open root operation after the
    /// caller explicitly elects to continue that root operation.
    ///
    /// This is deliberately separate from [`SessionSupervisor::reopen`]. A
    /// terminal child remains queryable from its graph fact alone. An open
    /// child is classified as interrupted, its isolated workspace is finalized
    /// when host authority is available, and no prior child operation is ever
    /// scheduled again.
    pub(crate) async fn reconcile_interrupted_subagents_before_root_continue(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
    ) -> Result<(), HarnessError> {
        let snapshot = self.snapshot()?;
        let root = reduce_lane(snapshot.clone(), self.root_lane_id.clone())?;
        let Some(root_operation_id) = root.lane_state.active_operation else {
            return Ok(());
        };
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let nodes = graph
            .agents
            .values()
            .filter(|node| {
                node.spawned.parent_lane_id == self.root_lane_id
                    && node.spawned.parent_operation_id == root_operation_id
            })
            .cloned()
            .collect::<Vec<_>>();
        self.ensure_owned_child_effects_reconciled(&snapshot, &nodes)?;
        self.interrupt_owned_child_provider_attempts(&snapshot, &nodes)?;
        for node in nodes {
            self.reconcile_interrupted_subagent(coordinator, node)
                .await?;
        }
        Ok(())
    }

    /// A child is never resumed, so an indeterminate child tool effect cannot
    /// be silently classified by child finalization. The parent must
    /// explicitly reconcile the durable effect first; only then may this path
    /// retain workspace evidence and clean the lease.
    fn ensure_owned_child_effects_reconciled(
        &self,
        snapshot: &tea_session::SessionSnapshot,
        nodes: &[AgentGraphNode],
    ) -> Result<(), HarnessError> {
        for node in nodes {
            let Some(operation_id) = node.operation_id.as_ref() else {
                continue;
            };
            for stored in snapshot.records() {
                match &stored.record {
                    LaneRecord::ToolStarted(tool)
                        if &tool.operation_id == operation_id
                            && !snapshot
                                .entries()
                                .iter()
                                .any(|entry| entry.header.id == tool.result_entry_id) =>
                    {
                        return Err(HarnessError::RecoveryRequired {
                            plan: RecoveryPlan::ReconcileToolEffect {
                                result_entry_id: tool.result_entry_id.clone(),
                            },
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Provider interruption has no external workspace effect to replay. On
    /// explicit continuation, retain its unknown outcome as an interrupted
    /// attempt before aborting the child; this starts no replacement request
    /// and records no synthetic usage.
    fn interrupt_owned_child_provider_attempts(
        &self,
        snapshot: &tea_session::SessionSnapshot,
        nodes: &[AgentGraphNode],
    ) -> Result<(), HarnessError> {
        for node in nodes {
            if node.terminal.is_some() {
                continue;
            }
            let Some(operation_id) = node.operation_id.as_ref() else {
                continue;
            };
            for stored in snapshot.records() {
                let LaneRecord::ProviderRequestStarted(request) = &stored.record else {
                    continue;
                };
                if &request.operation_id != operation_id
                    || snapshot.records().iter().any(|settled| {
                        matches!(
                            &settled.record,
                            LaneRecord::ProviderRequestSettled(record)
                                if record.request_id == request.request_id
                        )
                    })
                {
                    continue;
                }
                self.interrupt_provider_attempt(operation_id, request.request_id.clone())?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn reconcile_interrupted_subagents_for_test(
        self: &Arc<Self>,
    ) -> Result<(), HarnessError> {
        match self.subagent_coordinator()? {
            Some(coordinator) => {
                self.reconcile_interrupted_subagents_before_root_continue(&coordinator)
                    .await
            }
            None => Ok(()),
        }
    }

    async fn reconcile_interrupted_subagent(
        self: &Arc<Self>,
        coordinator: &Arc<SubagentCoordinator<S>>,
        node: AgentGraphNode,
    ) -> Result<(), HarnessError> {
        if node.terminal.is_some() {
            coordinator.restore_terminal_visibility(node.spawned.agent_id);
            return Ok(());
        }
        let Some(operation_id) = node.operation_id.clone() else {
            // The graph records a prepared spawn, but no child operation was
            // accepted. It remains inspectable as `Spawned`; accepting the old
            // assignment now would be checkpoint resurrection.
            return Ok(());
        };

        let prepared = self.reopen_subagent_prepared(coordinator, &node).await?;
        self.validate_reopened_subagent(&node, &prepared)?;

        let snapshot = self.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let current = graph
            .agents
            .get(&node.spawned.agent_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state("interrupted child disappeared from graph")
            })?;
        if current.terminal.is_some() {
            coordinator.restore_terminal_visibility(current.spawned.agent_id);
            return Ok(());
        }

        let (outcome, final_entry_id, report) =
            match child_operation_outcome(&snapshot, &operation_id) {
                Some(outcome) => {
                    let (final_entry_id, report) =
                        self.subagent_report_payload(&snapshot, &current)?;
                    (outcome, final_entry_id, report)
                }
                None => {
                    let final_entry_id =
                        self.append_interrupted_child_report(&current, &operation_id)?;
                    self.finish_recovered_child_operation(&current, &operation_id)?;
                    (
                        OperationOutcome::Aborted,
                        Some(final_entry_id),
                        PayloadRef::Inline(JsonValue::String(INTERRUPTED_CHILD_REPORT.into())),
                    )
                }
            };

        let snapshot = self.snapshot()?;
        let graph = reduce_agent_graph(&snapshot)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let current = graph
            .agents
            .get(&node.spawned.agent_id)
            .cloned()
            .ok_or_else(|| {
                HarnessError::invalid_state("interrupted child disappeared from graph")
            })?;
        let delta = if current.workspace_delta.is_none() {
            match coordinator
                .services()
                .host
                .finalize(FinalizeSubagentRequest {
                    agent_id: current.spawned.agent_id.clone(),
                    workspace: prepared.workspace.clone(),
                })
                .await
                .map_err(|error| {
                    subagent_host_stage_error(
                        true,
                        &current.spawned.agent_id,
                        SubagentRecoveryStage::FinalizeWorkspace,
                        error,
                    )
                })? {
                WorkspaceFinalization::NoChanges => None,
                WorkspaceFinalization::Delta(delta) => {
                    Some(self.workspace_delta_fact(&current, delta)?)
                }
            }
        } else {
            None
        };
        self.append_subagent_terminal(
            &current,
            operation_id,
            outcome,
            final_entry_id,
            report,
            delta,
        )?;
        coordinator
            .services()
            .host
            .cleanup(prepared.workspace)
            .await
            .map_err(|error| {
                subagent_host_stage_error(
                    true,
                    &current.spawned.agent_id,
                    SubagentRecoveryStage::CleanupWorkspace,
                    error,
                )
            })?;
        coordinator.mark_exposable_and_notify(current.spawned.agent_id);
        Ok(())
    }

    fn append_interrupted_child_report(
        &self,
        node: &AgentGraphNode,
        operation_id: &OperationId,
    ) -> Result<tea_session::EntryId, HarnessError> {
        let entry_id = subagent_entry_id(&node.spawned.agent_id, "interrupted-report")?;
        let entry =
            ProvisionedEntry::assistant(entry_id.clone(), INTERRUPTED_CHILD_REPORT, Vec::new());
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), node.spawned.lane_id.clone())?;
        if reduction.lane_state.active_operation.as_ref() != Some(operation_id) {
            return Err(HarnessError::invalid_state(
                "interrupted child report has no matching active operation",
            ));
        }
        if let Some(existing) = snapshot
            .entries()
            .iter()
            .find(|existing| existing.header.id == entry_id)
        {
            if existing.lane_id == node.spawned.lane_id && existing.body == entry.body {
                return Ok(entry_id);
            }
            return Err(HarnessError::invalid_state(
                "interrupted child report conflicts with an existing durable entry",
            ));
        }
        session.append_entry(&node.spawned.lane_id, entry)?;
        Ok(entry_id)
    }

    fn finish_recovered_child_operation(
        &self,
        node: &AgentGraphNode,
        operation_id: &OperationId,
    ) -> Result<(), HarnessError> {
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let reduction = reduce_lane(snapshot.clone(), node.spawned.lane_id.clone())?;
        match reduction.lane_state.active_operation {
            Some(active) if active == *operation_id => {}
            Some(_) => {
                return Err(HarnessError::invalid_state(
                    "interrupted child lane owns a different active operation",
                ));
            }
            None if child_operation_outcome(&snapshot, operation_id).is_some() => return Ok(()),
            None => {
                return Err(HarnessError::invalid_state(
                    "interrupted child operation is absent from its durable lane",
                ));
            }
        }
        let mut items = Vec::new();
        if let Some(epoch_id) = open_epoch(&snapshot, operation_id) {
            items.push(SessionCommitItem::Record(LaneRecord::EpochFinished(
                EpochFinishedRecord {
                    epoch_id,
                    operation_id: operation_id.clone(),
                    reason: EpochFinishReason::Interrupted,
                },
            )));
        }
        items.push(SessionCommitItem::Record(LaneRecord::OperationFinished(
            OperationFinishedRecord {
                operation_id: operation_id.clone(),
                outcome: OperationOutcome::Aborted,
            },
        )));
        session.commit(SessionCommit::new(items)?)?;
        Ok(())
    }
}

fn committed_child_tool_result(
    started: &ToolStartedRecord,
    value: JsonValue,
) -> Result<AgentToolResult, HarnessError> {
    Ok(AgentToolResult {
        tool_call_id: ToolCallId::new(started.tool_call_id.clone())
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
        content: value
            .to_json_string()
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?,
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    })
}
