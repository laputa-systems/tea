//! Durable effect procedures for one core-owned compaction transaction.
//!
//! A compactor may use a provider, but its summary response never becomes an
//! assistant turn. The provider intent and exact request material are retained
//! before dispatch; only the post-validation replacement is allowed to append
//! a `CompactionEntry` and change future model context.

use super::{
    EpochRuntime, core_usage, durable_identifier, provider_request_digest, request_material,
};
use crate::compaction::{
    CompactionId, CompactionImplementation, CompactionOperation, CompactionReplacement,
};
use crate::effect::{CompactionProviderEffectOutcome, EffectGateError};
use crate::runtime::context::{
    compaction_replacement_digest, derive_default_snapshot_context,
    derive_snapshot_context_with_policies, encode_compaction_replacement,
};
use crate::runtime::ProviderLimits;
use tea_protocol::JsonValue;
use tea_session::{
    CompactionEntry, EntryId, LaneRecord, PayloadRef, ProviderRequestId,
    ProviderRequestSettledRecord, ProviderRequestStartedRecord, ProviderSettlementClassification,
    ProvisionedEntry, SessionCommit, SessionCommitItem, SessionEntry, SessionFact, SessionWriter,
    StepAttemptedRecord, StepId, StepKind,
};

/// Process-local evidence for the only physical provider request admitted by
/// one compaction operation.
pub(super) struct PendingCompactionProvider {
    request_id: ProviderRequestId,
    operation: CompactionId,
}

/// Durable identities allocated once a compaction dispatch is admitted.
pub(super) struct CompactionStep {
    result_entry_id: EntryId,
    settled_request_id: Option<ProviderRequestId>,
}

impl<S> EpochRuntime<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Persist a compactor provider intent and exact post-hook request before
    /// the terminal host calls its transport.
    pub(super) fn before_compaction_provider(
        &mut self,
        action_id: crate::effect::EffectId,
        operation: &CompactionOperation,
        request: &crate::scheduler::ModelRequest,
    ) -> Result<(), EffectGateError> {
        if self.compaction_steps.contains_key(&operation.id) {
            return Err(self.fault(
                "a compaction operation may admit exactly one provider request",
            ));
        }

        let snapshot = self.session_snapshot()?;
        let step_attempt = snapshot
            .records()
            .iter()
            .filter(|stored| {
                matches!(
                    &stored.record,
                    LaneRecord::StepAttempted(record)
                        if record.operation_id == self.operation_id
                            && record.epoch_id == self.epoch_id
                            && record.kind == StepKind::Compaction
                )
            })
            .count()
            .saturating_add(1) as u32;
        let operation_label = operation.id.to_string();
        let operation_attempt = operation.attempt.to_string();
        let step_id = StepId::new(durable_identifier(
            "step-compaction",
            [
                self.epoch_id.as_str(),
                operation_label.as_str(),
                operation_attempt.as_str(),
            ],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        let request_id = ProviderRequestId::new(durable_identifier(
            "provider-request",
            [step_id.as_str(), "1"],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        let result_entry_id = EntryId::new(durable_identifier(
            "entry-compaction",
            [step_id.as_str()],
        ))
        .map_err(|error| self.fault(error.to_string()))?;

        let material = request_material(request)?;
        let bytes = material
            .to_json_string()
            .map_err(|error| self.fault(error.to_string()))?;
        let retained = self
            .artifacts
            .put(bytes.as_bytes(), "application/vnd.tea.model-request+json")
            .map_err(|error| self.fault(error.to_string()))?;
        let retained_request = PayloadRef::Artifact {
            artifact_id: retained.artifact_id,
            byte_len: retained.byte_len,
            media_type: retained.media_type,
        };
        let operation_id = self.operation_id.clone();
        let epoch_id = self.epoch_id.clone();
        let profile_id = self.identity.profile_id.clone();
        let request_surface_digest = provider_request_digest(request);
        self.mutate(|session| {
            session.commit(SessionCommit::new(vec![
                SessionCommitItem::Record(LaneRecord::StepAttempted(StepAttemptedRecord {
                    id: step_id.clone(),
                    operation_id: operation_id.clone(),
                    epoch_id: epoch_id.clone(),
                    kind: StepKind::Compaction,
                    attempt: step_attempt,
                    result_entry_id: result_entry_id.clone(),
                    reason: Some(compaction_reason_label(operation).into()),
                })),
                SessionCommitItem::Record(LaneRecord::ProviderRequestStarted(
                    ProviderRequestStartedRecord {
                        request_id: request_id.clone(),
                        operation_id: operation_id.clone(),
                        epoch_id: epoch_id.clone(),
                        step_id,
                        physical_attempt: 1,
                        model_harness_profile: profile_id,
                        request_surface_digest,
                        idempotency_key: None,
                    },
                )),
                SessionCommitItem::Fact(SessionFact::ProviderRequestMaterial(
                    tea_session::ProviderRequestMaterialFact {
                        operation_id,
                        epoch_id,
                        request_id: request_id.clone(),
                        request: retained_request,
                    },
                )),
            ])?)?;
            Ok(())
        })?;

        self.compaction_steps.insert(
            operation.id,
            CompactionStep {
                result_entry_id,
                settled_request_id: None,
            },
        );
        if self
            .pending_compaction_providers
            .insert(
                action_id,
                PendingCompactionProvider {
                    request_id,
                    operation: operation.id,
                },
            )
            .is_some()
        {
            return Err(self.fault("compaction provider effect action ID was admitted twice"));
        }
        Ok(())
    }

    /// Persist terminal provider evidence before the compactor can return a
    /// replacement to core.
    pub(super) fn after_compaction_provider(
        &mut self,
        action_id: crate::effect::EffectId,
        outcome: CompactionProviderEffectOutcome,
    ) -> Result<(), EffectGateError> {
        let pending = self
            .pending_compaction_providers
            .remove(&action_id)
            .ok_or_else(|| self.fault("compaction provider settlement has no durable intent"))?;
        let operation_id = self.operation_id.clone();
        let (outcome, classification, usage, settled) = match outcome {
            CompactionProviderEffectOutcome::Succeeded {
                usage,
                request_observation: _,
            } => (
                JsonValue::object([
                    ("status", JsonValue::String("settled".into())),
                    ("kind", JsonValue::String("compaction".into())),
                ]),
                ProviderSettlementClassification::Completed,
                usage,
                true,
            ),
            CompactionProviderEffectOutcome::Cancelled => (
                JsonValue::object([
                    ("status", JsonValue::String("cancelled".into())),
                    ("kind", JsonValue::String("compaction".into())),
                ]),
                ProviderSettlementClassification::Interrupted,
                None,
                false,
            ),
            CompactionProviderEffectOutcome::Failed { message } => (
                JsonValue::object([
                    ("status", JsonValue::String("interrupted".into())),
                    ("kind", JsonValue::String("compaction".into())),
                    ("message", JsonValue::String(message)),
                ]),
                ProviderSettlementClassification::Interrupted,
                None,
                false,
            ),
        };
        self.mutate(|session| {
            let mut items = vec![SessionCommitItem::Record(LaneRecord::ProviderRequestSettled(
                ProviderRequestSettledRecord {
                    request_id: pending.request_id.clone(),
                    operation_id: operation_id.clone(),
                    outcome,
                    provider_error: None,
                    usage: usage.as_ref().map(core_usage),
                    response_artifact: None,
                    classification,
                },
            ))];
            if let Some(usage) = usage.as_ref() {
                items.push(SessionCommitItem::Record(LaneRecord::Usage(
                    tea_session::UsageRecord {
                        operation_id,
                        request_id: Some(pending.request_id.clone()),
                        usage: core_usage(usage),
                    },
                )));
            }
            session.commit(SessionCommit::new(items)?)?;
            Ok(())
        })?;
        if settled {
            let Some(step) = self.compaction_steps.get_mut(&pending.operation) else {
                return Err(self.fault("compaction provider settlement lost its durable step"));
            };
            step.settled_request_id = Some(pending.request_id);
        }
        Ok(())
    }

    /// Append exact replacement material only after it reproduces the live
    /// durable context and every admitted summary request has settled.
    pub(super) fn persist_compaction_replacement(
        &mut self,
        replacement: &CompactionReplacement,
    ) -> Result<(), EffectGateError> {
        crate::compaction::validate_messages(&replacement.source_messages)
            .map_err(|error| self.fault(error.to_string()))?;
        crate::compaction::validate_messages(&replacement.replacement_messages)
            .map_err(|error| self.fault(error.to_string()))?;
        if replacement.replacement_messages.is_empty() {
            return Err(self.fault(
                "durable compaction replacement must retain at least one canonical message",
            ));
        }

        let snapshot = self.session_snapshot()?;
        let source = self.current_compaction_source(&snapshot)?;
        let persisted_source = encode_compaction_replacement(&source.messages)
            .map_err(|error| self.fault(error.to_string()))?;
        let proposed_source = encode_compaction_replacement(&replacement.source_messages)
            .map_err(|error| self.fault(error.to_string()))?;
        if persisted_source != proposed_source {
            return Err(self.fault(
                "compaction source no longer matches the effective durable context",
            ));
        }

        let operation_label = replacement.operation.id.to_string();
        let operation_attempt = replacement.operation.attempt.to_string();
        let step_id = StepId::new(durable_identifier(
            "step-compaction",
            [
                self.epoch_id.as_str(),
                operation_label.as_str(),
                operation_attempt.as_str(),
            ],
        ))
        .map_err(|error| self.fault(error.to_string()))?;
        let generated_entry_id = EntryId::new(durable_identifier("entry-compaction", [step_id.as_str()]))
            .map_err(|error| self.fault(error.to_string()))?;
        let (result_entry_id, provider_request_id) = match self.compaction_steps.get(&replacement.operation.id) {
            Some(step) => (
                step.result_entry_id.clone(),
                Some(step.settled_request_id.clone().ok_or_else(|| {
                    self.fault(
                        "compaction replacement follows a provider request without a successful settlement",
                    )
                })?),
            ),
            None if replacement.operation.strategy.implementation
                == CompactionImplementation::ProviderSummarization => {
                return Err(self.fault(
                    "provider-backed compaction replacement has no admitted request intent and material",
                ));
            }
            None => (generated_entry_id, None),
        };
        let exact_replacement = encode_compaction_replacement(&replacement.replacement_messages)
            .map_err(|error| self.fault(error.to_string()))?;
        let replacement_digest = compaction_replacement_digest(&exact_replacement)
            .map_err(|error| self.fault(error.to_string()))?;
        let summary = compaction_summary(&replacement.replacement_messages)
            .ok_or_else(|| self.fault("compaction replacement has no searchable summary text"))?;
        let covered_from = source.included_entries.first().cloned();
        let covered_to = source.included_entries.last().cloned();
        if covered_from.is_some() != covered_to.is_some() {
            return Err(self.fault("compaction source has an incomplete coverage range"));
        }
        let lane = self.lane.clone();
        let entry = CompactionEntry {
            covered_from,
            covered_to,
            retained_tail_boundary: None,
            summary,
            strategy_id: replacement.operation.strategy.id.clone(),
            recovery_index_artifact: None,
            harness_revision_id: Some(self.identity.revision_id.clone()),
            replacement: PayloadRef::Inline(exact_replacement),
            replacement_digest,
            provider_request_id,
        };
        self.mutate(|session| {
            session.commit(SessionCommit::one(SessionCommitItem::Entry {
                lane_id: lane,
                entry: ProvisionedEntry {
                    id: result_entry_id,
                    body: SessionEntry::Compaction(entry),
                },
            }))?;
            Ok(())
        })?;
        Ok(())
    }

    fn current_compaction_source(
        &mut self,
        snapshot: &tea_session::SessionSnapshot,
    ) -> Result<crate::runtime::context::DerivedContext, EffectGateError> {
        let harness_snapshot = self.resolved_harness.harness_snapshot.clone();
        let context_policies = self.resolved_harness.context_policies.clone();
        let lane = self.lane.clone();
        match harness_snapshot {
            Some(harness) => {
                let limits = ProviderLimits::new(harness.spec.resource_limits.provider_surface_bytes)
                    .map_err(|error| self.fault(error.to_string()))?;
                derive_snapshot_context_with_policies(
                    snapshot,
                    lane,
                    &harness,
                    limits,
                    &context_policies,
                    None,
                )
                .map_err(|error| self.fault(error.to_string()))
            }
            None => derive_default_snapshot_context(snapshot, lane)
                .map_err(|error| self.fault(error.to_string())),
        }
    }
}

fn compaction_reason_label(operation: &CompactionOperation) -> &'static str {
    match operation.reason {
        crate::compaction::CompactionReason::UserRequest => "user_request",
        crate::compaction::CompactionReason::Threshold => "threshold",
        crate::compaction::CompactionReason::ProviderOverflow => "provider_overflow",
    }
}

fn compaction_summary(messages: &[crate::state::AgentMessage]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        crate::state::AgentMessage::User { content, .. }
        | crate::state::AgentMessage::Assistant { content, .. }
        | crate::state::AgentMessage::ToolResult { content, .. }
            if !content.trim().is_empty() =>
        {
            Some(content.clone())
        }
        _ => None,
    })
}
