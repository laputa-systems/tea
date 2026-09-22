//! Read-only interruption reports and explicit host reconciliation.

use super::*;

#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryReport {
    pub sequence: tea_session::Sequence,
    pub lanes: Vec<LaneRecoveryReport>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaneRecoveryReport {
    pub lane_id: LaneId,
    pub operation_id: Option<OperationId>,
    pub next: Option<RecoveryPlan>,
    pub effects: Vec<InterruptedEffect>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InterruptedEffect {
    ToolNotInvoked {
        assistant_entry_id: EntryId,
        tool_call_id: String,
        tool_name: String,
    },
    ToolOutcomeUnknown {
        result_entry_id: EntryId,
        tool_call_id: String,
        tool_name: String,
        replay_policy: ToolReplayPolicy,
    },
    ProviderOutcomeUnknown {
        request_id: ProviderRequestId,
    },
    ProviderNotInvoked {
        request_id: ProviderRequestId,
    },
}

pub(super) fn committed_run_outcome(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
    epoch_id: &EpochId,
) -> Result<Option<OperationOutcome>, HarnessError> {
    let epoch_sequence = snapshot.records().iter().find_map(|stored| match &stored.record {
        LaneRecord::EpochStarted(epoch) if &epoch.id == epoch_id => Some(stored.seq),
        _ => None,
    }).ok_or_else(|| HarnessError::invalid_state("recovery epoch has no committed start"))?;
    let messages = snapshot.entries().iter().filter(|entry|
        &entry.lane_id == lane_id && entry.header.seq >= epoch_sequence
        && matches!(&entry.body, SessionEntry::UserMessage(_) | SessionEntry::AssistantMessage(_) | SessionEntry::ToolResult(_))).collect::<Vec<_>>();
    if matches!(messages.last().map(|entry| &entry.body), Some(SessionEntry::ToolResult(_))) {
        let Some(assistant_index) = messages.iter().rposition(|entry| matches!(&entry.body, SessionEntry::AssistantMessage(_))) else {
            return Ok(None);
        };
        let SessionEntry::AssistantMessage(assistant) = &messages[assistant_index].body else { unreachable!() };
        let all_terminated = !assistant.tool_calls.is_empty() && assistant.tool_calls.iter().all(|call|
            messages[assistant_index + 1..].iter().any(|entry| matches!(&entry.body,
                SessionEntry::ToolResult(result) if result.tool_call_id == call.id && result.tool_name == call.name && result.terminate)));
        return Ok(all_terminated.then_some(OperationOutcome::Completed));
    }
    let Some(SessionEntry::AssistantMessage(assistant)) = messages.last().map(|entry| &entry.body) else {
        return Ok(None);
    };
    if !assistant.tool_calls.is_empty() {
        return Ok(None);
    }
    Ok(match assistant.stop_reason.as_deref().map(parse_stop_reason).transpose()? {
        Some(StopReason::Stop) => Some(OperationOutcome::Completed),
        Some(StopReason::Cancelled) => Some(OperationOutcome::Aborted),
        Some(StopReason::Error) => Some(OperationOutcome::Failed { code: "model_error".into() }),
        Some(StopReason::Aborted) => Some(OperationOutcome::Failed { code: "model_aborted".into() }),
        _ => None,
    })
}

pub fn inspect_recovery(snapshot: &SessionSnapshot) -> Result<RecoveryReport, HarnessError> {
    let mut lane_ids = BTreeSet::from([snapshot.header().initial_lane.clone()]);
    for mutation in snapshot.lane_mutations() {
        let LaneMutation::Created { lane_id, .. } = &mutation.mutation;
        lane_ids.insert(lane_id.clone());
    }
    let mut lanes = Vec::new();
    for lane_id in lane_ids {
        let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
        let operations = snapshot.records().iter().filter_map(|stored| match &stored.record {
            LaneRecord::OperationStarted(operation) if operation.lane_id == lane_id => Some(operation.id.clone()),
            _ => None,
        }).collect::<BTreeSet<_>>();
        let mut effects = Vec::new();
        for stored in snapshot.records() {
            match &stored.record {
                LaneRecord::ToolStarted(tool) if operations.contains(&tool.operation_id)
                    && !snapshot.entries().iter().any(|entry| entry.header.id == tool.result_entry_id) => {
                    effects.push(InterruptedEffect::ToolOutcomeUnknown {
                        result_entry_id: tool.result_entry_id.clone(),
                        tool_call_id: tool.tool_call_id.clone(),
                        tool_name: tool.tool_name.clone(),
                        replay_policy: tool.replay_policy_at_start,
                    });
                }
                LaneRecord::ProviderRequestStarted(request) if operations.contains(&request.operation_id)
                    && !snapshot.records().iter().any(|stored| matches!(&stored.record,
                        LaneRecord::ProviderRequestSettled(settled) if settled.request_id == request.request_id)) => {
                    let admitted = snapshot.facts().iter().any(|stored| matches!(&stored.fact,
                        SessionFact::ProviderRequestMaterial(material) if material.request_id == request.request_id));
                    effects.push(if admitted {
                        InterruptedEffect::ProviderOutcomeUnknown { request_id: request.request_id.clone() }
                    } else {
                        InterruptedEffect::ProviderNotInvoked { request_id: request.request_id.clone() }
                    });
                }
                _ => {}
            }
        }
        for (entry_index, entry) in snapshot.entries().iter().enumerate().filter(|(_, entry)| entry.lane_id == lane_id) {
            let SessionEntry::AssistantMessage(assistant) = &entry.body else { continue };
            for (index, call) in assistant.tool_calls.iter().enumerate() {
                let invoked = snapshot.records().iter().any(|stored| matches!(&stored.record,
                    LaneRecord::ToolStarted(tool) if tool.assistant_entry_id == entry.header.id && tool.tool_index as usize == index));
                let settled = snapshot.entries()[entry_index + 1..].iter()
                    .filter(|entry| entry.lane_id == lane_id)
                    .take_while(|entry| !matches!(&entry.body, SessionEntry::AssistantMessage(_) | SessionEntry::UserMessage(_)))
                    .any(|entry| matches!(&entry.body, SessionEntry::ToolResult(result) if result.tool_call_id == call.id && result.tool_name == call.name));
                if !invoked && !settled {
                    effects.push(InterruptedEffect::ToolNotInvoked {
                        assistant_entry_id: entry.header.id.clone(), tool_call_id: call.id.clone(), tool_name: call.name.clone(),
                    });
                }
            }
        }
        if reduction.lane_state.active_operation.is_some() || !effects.is_empty() {
            lanes.push(LaneRecoveryReport {
                lane_id, operation_id: reduction.lane_state.active_operation,
                next: reduction.recovery_plan, effects,
            });
        }
    }
    Ok(RecoveryReport { sequence: snapshot.last_sequence(), lanes })
}

impl<S> SessionSupervisor<S>
where S: SessionWriter + Send + 'static,
{
    pub fn recovery_report(&self) -> Result<RecoveryReport, HarnessError> {
        inspect_recovery(&self.snapshot()?)
    }

    pub(super) fn ensure_recovery_permitted(&self, lane_id: &LaneId) -> Result<(), HarnessError> {
        let snapshot = self.snapshot()?;
        let lane = self.lane(lane_id)?;
        let operations = snapshot.records().iter().filter_map(|stored| match &stored.record {
            LaneRecord::OperationStarted(operation) if &operation.lane_id == lane_id => Some(operation.id.clone()),
            _ => None,
        }).collect::<BTreeSet<_>>();
        for stored in snapshot.records() {
            let LaneRecord::ToolStarted(tool) = &stored.record else { continue };
            if !operations.contains(&tool.operation_id)
                || snapshot.entries().iter().any(|entry| entry.header.id == tool.result_entry_id) {
                continue;
            }
            if tool.replay_policy_at_start == ToolReplayPolicy::Never
                || !self.replay_is_still_safe(&lane, tool) {
                return Err(HarnessError::RecoveryRequired {
                    plan: RecoveryPlan::ReconcileToolEffect { result_entry_id: tool.result_entry_id.clone() },
                });
            }
        }
        Ok(())
    }

    pub fn reconcile_tool_result(
        &self,
        lane_id: LaneId,
        result_entry_id: EntryId,
        result: AgentToolResult,
        evidence: String,
    ) -> Result<(), HarnessError> {
        if evidence.trim().is_empty() || evidence.len() > 16 * 1024 {
            return Err(HarnessError::invalid_state("tool reconciliation requires non-empty evidence within 16384 bytes"));
        }
        let lane = self.lane(&lane_id)?;
        let _claim = self.claim_lane_operation(lane.clone())?;
        let snapshot = self.snapshot()?;
        let started = recovery_tool_start(&snapshot, &result_entry_id)?;
        let owns_operation = snapshot.records().iter().any(|stored| matches!(&stored.record,
            LaneRecord::OperationStarted(operation) if operation.id == started.operation_id && operation.lane_id == lane_id));
        if !owns_operation || result.tool_call_id.as_str() != started.tool_call_id {
            return Err(HarnessError::invalid_state("tool reconciliation does not match its owning lane and invocation"));
        }
        if snapshot.entries().iter().any(|entry| entry.header.id == result_entry_id) {
            return Err(HarnessError::invalid_state("a committed tool outcome cannot be reconciled again"));
        }
        let configuration = self.configuration_for_revision(&lane, &started.harness_revision_id)?;
        let mut retained_result = result;
        retained_result.details = Some(SerializedJson::new(JsonValue::object([
            ("reconciliation_evidence", JsonValue::String(evidence)),
            ("host_result_details", retained_result.details.as_ref().map(|details| JsonValue::String(details.as_str().into())).unwrap_or(JsonValue::Null)),
        ]).to_json_string().map_err(|error| HarnessError::invalid_state(error.to_string()))?));
        let retained = retain_tool_result_with_projection(self.artifacts.as_ref(),
            configuration.artifact_policy_config(), &retained_result, &retained_result)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let entry = tool_result_entry(&retained_result, &retained_result, &started.tool_name, retained)
            .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
        let mut items = vec![SessionCommitItem::Entry {
            lane_id: lane_id.clone(),
            entry: ProvisionedEntry { id: result_entry_id, body: SessionEntry::ToolResult(entry) },
        }];
        let operation_closed = snapshot.records().iter().any(|stored| matches!(&stored.record,
            LaneRecord::OperationFinished(finished) if finished.operation_id == started.operation_id));
        if operation_closed {
            items.extend(self.uninvoked_sibling_results(&snapshot, &lane_id, started, &configuration)?);
        }
        self.session_lock()?.commit(SessionCommit::new(items)?)?;
        Ok(())
    }

    fn uninvoked_sibling_results(
        &self,
        snapshot: &SessionSnapshot,
        lane_id: &LaneId,
        started: &ToolStartedRecord,
        configuration: &ResolvedHarness,
    ) -> Result<Vec<SessionCommitItem>, HarnessError> {
        let assistant_index = snapshot.entries().iter().position(|entry| entry.header.id == started.assistant_entry_id)
            .ok_or_else(|| HarnessError::invalid_state("reconciliation source assistant is missing"))?;
        let SessionEntry::AssistantMessage(assistant) = &snapshot.entries()[assistant_index].body else {
            return Err(HarnessError::invalid_state("reconciliation source is not an assistant"));
        };
        let mut items = Vec::new();
        for (index, call) in assistant.tool_calls.iter().enumerate() {
            let admitted = snapshot.records().iter().any(|stored| matches!(&stored.record,
                LaneRecord::ToolStarted(tool) if tool.assistant_entry_id == started.assistant_entry_id && tool.tool_index as usize == index));
            let settled = snapshot.entries()[assistant_index + 1..].iter()
                .filter(|entry| &entry.lane_id == lane_id)
                .take_while(|entry| !matches!(&entry.body, SessionEntry::AssistantMessage(_) | SessionEntry::UserMessage(_)))
                .any(|entry| matches!(&entry.body, SessionEntry::ToolResult(result) if result.tool_call_id == call.id && result.tool_name == call.name));
            if admitted || settled {
                continue;
            }
            let result = AgentToolResult {
                tool_call_id: ToolCallId::new(call.id.clone()).map_err(|error| HarnessError::invalid_state(error.to_string()))?,
                content: "Tool was not invoked before its owning operation ended.".into(),
                details: Some(SerializedJson::new(r#"{"invoked":false,"outcome_known":true}"#)),
                usage: None, added_tool_names: Vec::new(), terminate: false, is_error: true,
                failure: Some(crate::tool::ToolFailure::cancelled()),
            };
            let retained = retain_tool_result_with_projection(self.artifacts.as_ref(), configuration.artifact_policy_config(), &result, &result)
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let entry = tool_result_entry(&result, &result, &call.name, retained)
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            let result_id = EntryId::new(durable_identifier("entry-tool-result", [started.assistant_entry_id.as_str(), &index.to_string()]))
                .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
            items.push(SessionCommitItem::Entry {
                lane_id: lane_id.clone(), entry: ProvisionedEntry { id: result_id, body: SessionEntry::ToolResult(entry) },
            });
        }
        Ok(items)
    }

    pub(super) fn interrupt_provider_attempt(
        &self,
        operation_id: &OperationId,
        request_id: ProviderRequestId,
    ) -> Result<(), HarnessError> {
        let admitted = self.snapshot()?.facts().iter().any(|stored| matches!(&stored.fact,
            SessionFact::ProviderRequestMaterial(material) if material.request_id == request_id));
        self.session_lock()?.append_record(LaneRecord::ProviderRequestSettled(ProviderRequestSettledRecord {
            request_id, operation_id: operation_id.clone(),
            outcome: JsonValue::object([
                ("status", JsonValue::String(if admitted { "interrupted" } else { "not_invoked" }.into())),
                ("outcome_known", JsonValue::Bool(!admitted)),
                ("continuation", JsonValue::String("explicit_new_attempt".into())),
            ]),
            provider_error: None, usage: None, response_artifact: None,
            classification: ProviderSettlementClassification::Interrupted,
        }))?;
        Ok(())
    }
}
