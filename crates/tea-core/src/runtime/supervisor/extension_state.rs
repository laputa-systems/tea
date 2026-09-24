//! Bounded extension-owned state projected from durable session facts.
//!
//! This module is deliberately narrower than generic plugin memory. An
//! extension has exactly one private JSON value per lane, and the state value
//! is only writable through a version-pinned immutable extension contract.

use super::{LaneRuntime, SessionSupervisor, durable_identifier, portable_extension_label};
use crate::harness::{HarnessError, ResolvedHarness};
use std::collections::BTreeMap;
use tea_core::harness::extension::{
    ExtensionError, ExtensionStateGeneration, ExtensionStateStore, ExtensionStateUpdate,
    ExtensionStateView, MAX_EXTENSION_STATE_VALUE_BYTES,
};
use tea_session::{
    EpochStartedRecord, ExtensionStateValue, ExtensionStateValueSetFact, HarnessRevisionId, LaneId,
    OperationId, SessionCommit, SessionCommitItem, SessionFact, SessionSnapshot, SessionWriter,
    TurnCheckpointFact, TurnCheckpointId, extension_state_for_lane, reduce_lane,
};

/// Project the exact private state value owned by `extension_id` on `lane_id`.
///
/// `tea-session` owns fork inheritance: this query starts a fork at its
/// checkpoint snapshot and never observes later source-lane writes.
pub(super) fn extension_state_view(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
    extension_id: &str,
) -> Result<ExtensionStateView, HarnessError> {
    validate_extension_state_identity(extension_id, "namespace")?;
    let state = extension_state_for_lane(snapshot, lane_id.clone())?;
    Ok(ExtensionStateView {
        value: state.get(extension_id).map(|value| value.value.clone()),
    })
}

/// Build the only durable mutation that can replace extension-owned state.
///
/// The caller supplies a state contract selected from an immutable resolved
/// harness. The returned item is pure data, so it can be grouped atomically
/// with control settlement without invoking extension code under a writer lock.
pub(super) fn extension_state_commit_item(
    lane_id: LaneId,
    extension_id: &str,
    state_version: &str,
    update: ExtensionStateUpdate,
) -> Result<SessionCommitItem, HarnessError> {
    validate_extension_state_identity(extension_id, "namespace")?;
    validate_extension_state_identity(state_version, "version")?;
    let encoded = update
        .value
        .to_json_string()
        .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
    if encoded.len() > MAX_EXTENSION_STATE_VALUE_BYTES {
        return Err(HarnessError::invalid_state(format!(
            "extension state value exceeds the {MAX_EXTENSION_STATE_VALUE_BYTES}-byte limit"
        )));
    }
    // Commit the canonical decode of the encoded value, not the producer's
    // representation. `JsonValue` equality distinguishes numeric forms (Luau
    // yields `Signed(1)`, the codec decodes `Unsigned(1)`), so this keeps the
    // live reduction identical to the state a reopen reconstructs.
    let value = tea_protocol::JsonValue::parse(&encoded).map_err(|_| {
        HarnessError::invalid_state("extension state value does not decode canonically")
    })?;
    Ok(SessionCommitItem::Fact(
        SessionFact::ExtensionStateValueSet(ExtensionStateValueSetFact {
            lane_id,
            extension_id: extension_id.to_owned(),
            state_version: state_version.to_owned(),
            value,
        }),
    ))
}

/// Build the exact settled-turn boundary fact that may later anchor a fork.
///
/// The caller appends this item immediately after `OperationFinished` and its
/// input-settlement records in one commit. The session reducer validates that
/// placement and rejects any unresolved work before accepting the checkpoint.
pub(super) fn turn_checkpoint_item(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
    operation_id: &OperationId,
) -> Result<SessionCommitItem, HarnessError> {
    let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
    turn_checkpoint_item_with_state(snapshot, lane_id, operation_id, reduction.extension_state)
}

/// Build a settled-turn checkpoint after an atomically preceding state update.
///
/// `extension_state` must be the complete prospective namespace map, not a
/// delta. The session reducer independently proves it equals the state facts
/// preceding this checkpoint in the same semantic commit.
pub(super) fn turn_checkpoint_item_with_state(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
    operation_id: &OperationId,
    extension_state: BTreeMap<String, ExtensionStateValue>,
) -> Result<SessionCommitItem, HarnessError> {
    let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
    if reduction.lane_state.active_operation.as_ref() != Some(operation_id) {
        return Err(HarnessError::invalid_state(format!(
            "turn checkpoint operation {operation_id} is not active on lane {lane_id}"
        )));
    }
    checkpoint_item(
        snapshot,
        lane_id,
        operation_id,
        reduction.lane_state.leaf_id,
        extension_state,
    )
}

/// Build a checkpoint in the immediate semantic follow-on to a completed turn.
///
/// Queued extension controls settle after `OperationFinished`. Their final
/// state replacement, control application, and checkpoint share this follow-on
/// commit; the reducer verifies that no intervening mutation broke the
/// settled-turn boundary.
pub(super) fn settled_turn_checkpoint_item_with_state(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
    operation_id: &OperationId,
    extension_state: BTreeMap<String, ExtensionStateValue>,
) -> Result<SessionCommitItem, HarnessError> {
    let reduction = reduce_lane(snapshot.clone(), lane_id.clone())?;
    if reduction.lane_state.active_operation.is_some() {
        return Err(HarnessError::invalid_state(format!(
            "settled-turn checkpoint operation {operation_id} cannot follow an active lane {lane_id}"
        )));
    }
    let Some((latest_operation, outcome, _, _)) = super::terminal_operation(snapshot, lane_id)
    else {
        return Err(HarnessError::invalid_state(format!(
            "settled-turn checkpoint operation {operation_id} has no terminal record on lane {lane_id}"
        )));
    };
    if latest_operation != *operation_id || outcome != tea_session::OperationOutcome::Completed {
        return Err(HarnessError::invalid_state(format!(
            "settled-turn checkpoint operation {operation_id} is not the latest completed operation on lane {lane_id}"
        )));
    }
    checkpoint_item(
        snapshot,
        lane_id,
        operation_id,
        reduction.lane_state.leaf_id,
        extension_state,
    )
}

fn checkpoint_item(
    snapshot: &SessionSnapshot,
    lane_id: &LaneId,
    operation_id: &OperationId,
    leaf_id: Option<tea_session::EntryId>,
    extension_state: BTreeMap<String, ExtensionStateValue>,
) -> Result<SessionCommitItem, HarnessError> {
    let checkpoint_id = TurnCheckpointId::new(durable_identifier(
        "turn-checkpoint",
        [
            snapshot.header().session_id.as_str(),
            lane_id.as_str(),
            operation_id.as_str(),
        ],
    ))
    .map_err(|error| HarnessError::invalid_state(error.to_string()))?;
    Ok(SessionCommitItem::Fact(SessionFact::TurnCheckpoint(
        TurnCheckpointFact {
            checkpoint_id,
            lane_id: lane_id.clone(),
            operation_id: operation_id.clone(),
            leaf_id,
            extension_state,
        },
    )))
}

impl<S> SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Persist a callback result only when its observed whole state is still
    /// current on the same immutable harness revision.
    pub(super) fn append_extension_state_update_if_state_matches(
        &self,
        lane: &LaneRuntime,
        expected_revision: &HarnessRevisionId,
        extension_id: &str,
        state_version: &str,
        observed_state: Option<&ExtensionStateValue>,
        update: ExtensionStateUpdate,
    ) -> Result<(), HarnessError> {
        self.commit_extension_state_update_for_revision(
            lane,
            expected_revision,
            extension_id,
            state_version,
            observed_state,
            update,
        )
    }

    fn commit_extension_state_update_for_revision(
        &self,
        lane: &LaneRuntime,
        expected_revision: &HarnessRevisionId,
        extension_id: &str,
        state_version: &str,
        observed_state: Option<&ExtensionStateValue>,
        update: ExtensionStateUpdate,
    ) -> Result<(), HarnessError> {
        let item =
            extension_state_commit_item(lane.lane_id.clone(), extension_id, state_version, update)?;
        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        let reduction = reduce_lane(snapshot, lane.lane_id.clone())?;
        if reduction.lane_state.active_harness_revision.as_ref() != Some(expected_revision) {
            return Err(HarnessError::invalid_state(format!(
                "extension state update for {extension_id} was selected under closed harness revision {expected_revision}"
            )));
        }
        if let Some(existing) = reduction.extension_state.get(extension_id)
            && existing.state_version != state_version
        {
            return Err(HarnessError::invalid_state(format!(
                "extension state update for {extension_id} changes retained state version from {} to {state_version}",
                existing.state_version,
            )));
        }
        if reduction.extension_state.get(extension_id) != observed_state {
            return Err(HarnessError::invalid_state(format!(
                "extension state for {extension_id} changed while its callback was evaluating"
            )));
        }
        session.commit(SessionCommit::one(item))?;
        Ok(())
    }

    fn append_extension_state_update_for_generation(
        &self,
        generation: &ExtensionStateGeneration,
        extension_id: &str,
        update: ExtensionStateUpdate,
    ) -> Result<(), HarnessError> {
        let lane = self.lane(generation.lane_id())?;
        let snapshot = self.snapshot()?;
        let started = validate_active_extension_state_generation(&snapshot, generation)?;
        let configuration = self.configuration_for_epoch_started(&lane, &snapshot, started)?;
        let state_version = required_state_version(&configuration, extension_id)?;
        let item =
            extension_state_commit_item(lane.lane_id.clone(), extension_id, state_version, update)?;

        let mut session = self.session_lock()?;
        let snapshot = session.snapshot()?;
        validate_active_extension_state_generation(&snapshot, generation)?;
        let reduction = reduce_lane(snapshot, lane.lane_id.clone())?;
        if let Some(existing) = reduction.extension_state.get(extension_id)
            && existing.state_version != state_version
        {
            return Err(HarnessError::invalid_state(format!(
                "extension state update for {extension_id} changes retained state version from {} to {state_version}",
                existing.state_version,
            )));
        }
        session.commit(SessionCommit::one(item))?;
        Ok(())
    }
}

impl<S> ExtensionStateStore for SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    fn read_extension_state(
        &self,
        generation: &ExtensionStateGeneration,
        extension_id: &str,
    ) -> Result<ExtensionStateView, ExtensionError> {
        let snapshot = self
            .snapshot()
            .map_err(|error| ExtensionError::new(error.to_string()))?;
        validate_active_extension_state_generation(&snapshot, generation)
            .map_err(|error| ExtensionError::new(error.to_string()))?;
        extension_state_view(&snapshot, generation.lane_id(), extension_id)
            .map_err(|error| ExtensionError::new(error.to_string()))
    }

    fn replace_extension_state(
        &self,
        generation: &ExtensionStateGeneration,
        extension_id: &str,
        update: ExtensionStateUpdate,
    ) -> Result<(), ExtensionError> {
        self.append_extension_state_update_for_generation(generation, extension_id, update)
            .map_err(|error| ExtensionError::new(error.to_string()))
    }
}

fn required_state_version<'a>(
    configuration: &'a ResolvedHarness,
    extension_id: &str,
) -> Result<&'a str, HarnessError> {
    configuration
        .extension_state_version(extension_id)
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "extension {extension_id} has no immutable state_version contract"
            ))
        })
}

/// Verify that a state capability still belongs to the one epoch that minted
/// it. This is repeated under the session writer before replacement, so a
/// late callback cannot write merely because a later operation selected the
/// same harness revision.
fn validate_active_extension_state_generation<'a>(
    snapshot: &'a SessionSnapshot,
    generation: &ExtensionStateGeneration,
) -> Result<&'a EpochStartedRecord, HarnessError> {
    let reduction = reduce_lane(snapshot.clone(), generation.lane_id().clone())?;
    if reduction.lane_state.active_operation.as_ref() != Some(generation.operation_id()) {
        return Err(HarnessError::invalid_state(format!(
            "extension state generation operation {} is no longer active on lane {}",
            generation.operation_id(),
            generation.lane_id(),
        )));
    }
    if reduction.lane_state.active_harness_revision.as_ref()
        != Some(generation.harness_revision_id())
    {
        return Err(HarnessError::invalid_state(format!(
            "extension state generation revision {} is no longer active on lane {}",
            generation.harness_revision_id(),
            generation.lane_id(),
        )));
    }
    if super::open_epoch(snapshot, generation.operation_id()).as_ref()
        != Some(generation.epoch_id())
    {
        return Err(HarnessError::invalid_state(format!(
            "extension state generation epoch {} is no longer open for operation {}",
            generation.epoch_id(),
            generation.operation_id(),
        )));
    }
    let started = snapshot
        .records()
        .iter()
        .find_map(|stored| match &stored.record {
            tea_session::LaneRecord::EpochStarted(record)
                if record.id == *generation.epoch_id() =>
            {
                Some(record)
            }
            _ => None,
        })
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "extension state generation epoch {} has no durable start record",
                generation.epoch_id(),
            ))
        })?;
    if started.operation_id != *generation.operation_id()
        || started.harness_revision_id != *generation.harness_revision_id()
    {
        return Err(HarnessError::invalid_state(format!(
            "extension state generation does not match durable epoch {}",
            generation.epoch_id(),
        )));
    }
    Ok(started)
}

fn validate_extension_state_identity(value: &str, role: &str) -> Result<(), HarnessError> {
    if portable_extension_label(value) {
        Ok(())
    } else {
        Err(HarnessError::invalid_state(format!(
            "extension state {role} must use a portable non-empty label"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tea_protocol::JsonValue;
    use tea_session::{
        CoreRunId, EpochFinishReason, EpochFinishedRecord, EpochId, EpochStartedRecord,
        HarnessRevisionChangedEntry, HarnessSnapshotId, MemorySession, ModelHarnessProfileId,
        OperationFinishedRecord, OperationId, OperationKind, OperationOutcome,
        OperationStartedRecord, ProvisionedEntry, SessionEntry, SessionHeader, SessionId,
    };

    #[test]
    fn state_commit_item_is_one_version_pinned_whole_value() {
        let item = extension_state_commit_item(
            LaneId::main(),
            "todo",
            "todo.v1",
            ExtensionStateUpdate {
                value: JsonValue::object([("open", JsonValue::from(2_u64))]),
            },
        )
        .expect("bounded state commit item");

        let SessionCommitItem::Fact(SessionFact::ExtensionStateValueSet(fact)) = item else {
            panic!("state update must persist as its dedicated fact");
        };
        assert_eq!(fact.lane_id, LaneId::main());
        assert_eq!(fact.extension_id, "todo");
        assert_eq!(fact.state_version, "todo.v1");
        assert_eq!(
            fact.value,
            JsonValue::object([("open", JsonValue::from(2_u64))])
        );
    }

    #[test]
    fn state_commit_item_rejects_values_above_the_public_bound() {
        let error = extension_state_commit_item(
            LaneId::main(),
            "todo",
            "todo.v1",
            ExtensionStateUpdate {
                value: JsonValue::String("x".repeat(MAX_EXTENSION_STATE_VALUE_BYTES)),
            },
        )
        .expect_err("encoded JSON quotes place this value above the bound");

        assert!(error.to_string().contains("extension state value exceeds"));
    }

    #[test]
    fn state_commit_item_commits_the_value_reopen_will_decode() {
        let produced = JsonValue::object([
            (
                "next_id",
                JsonValue::Number(tea_protocol::JsonNumber::Signed(2)),
            ),
            (
                "offset",
                JsonValue::Number(tea_protocol::JsonNumber::Signed(-1)),
            ),
        ]);
        let reopened = JsonValue::parse(&produced.to_json_string().expect("value encodes"))
            .expect("encoded value decodes");
        assert_ne!(
            produced, reopened,
            "the codec normalizes non-negative integers"
        );

        let SessionCommitItem::Fact(SessionFact::ExtensionStateValueSet(fact)) =
            extension_state_commit_item(
                LaneId::main(),
                "todo",
                "todo.v1",
                ExtensionStateUpdate { value: produced },
            )
            .expect("bounded state value commits")
        else {
            panic!("state update commits one extension state fact");
        };

        assert_eq!(fact.value, reopened);
    }

    #[test]
    fn closed_epoch_generation_is_rejected_after_same_revision_starts_another_operation() {
        let lane = LaneId::main();
        let revision =
            HarnessRevisionId::new("state-generation-revision").expect("fixture revision ID");
        let snapshot_id =
            HarnessSnapshotId::new("state-generation-snapshot").expect("fixture snapshot ID");
        let profile =
            ModelHarnessProfileId::new("state-generation-profile").expect("fixture profile ID");
        let first_operation =
            OperationId::new("state-generation-first-operation").expect("fixture operation ID");
        let first_epoch = EpochId::new("state-generation-first-epoch").expect("fixture epoch ID");
        let second_operation =
            OperationId::new("state-generation-second-operation").expect("fixture operation ID");
        let second_epoch = EpochId::new("state-generation-second-epoch").expect("fixture epoch ID");
        let first_generation = ExtensionStateGeneration::new(
            lane.clone(),
            first_operation.clone(),
            first_epoch.clone(),
            revision.clone(),
        );
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("state-generation-session").expect("fixture session ID"),
            "workspace-test",
            BTreeMap::new(),
        ))
        .expect("memory session creates");
        let revision_entry =
            tea_session::EntryId::new("state-generation-revision-entry").expect("fixture entry ID");
        session
            .append_entry(
                &lane,
                ProvisionedEntry {
                    id: revision_entry.clone(),
                    body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                        revision_id: revision.clone(),
                        snapshot_id: snapshot_id.clone(),
                        rollback_from: None,
                    }),
                },
            )
            .expect("initial harness revision commits");
        start_generation_operation(
            &mut session,
            &lane,
            &first_operation,
            &first_epoch,
            &revision,
            &snapshot_id,
            &profile,
            &revision_entry,
        );
        validate_active_extension_state_generation(
            &session.snapshot().expect("snapshot succeeds"),
            &first_generation,
        )
        .expect("first generation is active");

        session
            .commit(
                SessionCommit::new(vec![
                    SessionCommitItem::Record(tea_session::LaneRecord::EpochFinished(
                        EpochFinishedRecord {
                            epoch_id: first_epoch,
                            operation_id: first_operation,
                            reason: EpochFinishReason::Settled,
                        },
                    )),
                    SessionCommitItem::Record(tea_session::LaneRecord::OperationFinished(
                        OperationFinishedRecord {
                            operation_id: first_generation.operation_id().clone(),
                            outcome: OperationOutcome::Completed,
                        },
                    )),
                ])
                .expect("first operation settles"),
            )
            .expect("first settlement commits");
        start_generation_operation(
            &mut session,
            &lane,
            &second_operation,
            &second_epoch,
            &revision,
            &snapshot_id,
            &profile,
            &revision_entry,
        );

        let snapshot = session.snapshot().expect("snapshot succeeds");
        assert_eq!(
            reduce_lane(snapshot.clone(), lane)
                .expect("second operation reduces")
                .lane_state
                .active_harness_revision,
            Some(revision),
            "the stale write must be rejected even when the revision is unchanged"
        );
        let error = validate_active_extension_state_generation(&snapshot, &first_generation)
            .expect_err("a closed epoch generation cannot access state");
        assert!(error.to_string().contains("no longer active"));
    }

    fn start_generation_operation(
        session: &mut MemorySession,
        lane: &LaneId,
        operation_id: &OperationId,
        epoch_id: &EpochId,
        revision: &HarnessRevisionId,
        snapshot_id: &HarnessSnapshotId,
        profile: &ModelHarnessProfileId,
        source_leaf: &tea_session::EntryId,
    ) {
        session
            .commit(
                SessionCommit::new(vec![
                    SessionCommitItem::Record(tea_session::LaneRecord::OperationStarted(
                        OperationStartedRecord::new(
                            operation_id.clone(),
                            lane.clone(),
                            Some(source_leaf.clone()),
                            OperationKind::Run,
                            Vec::new(),
                            revision.clone(),
                            profile.clone(),
                        ),
                    )),
                    SessionCommitItem::Record(tea_session::LaneRecord::EpochStarted(
                        EpochStartedRecord {
                            id: epoch_id.clone(),
                            operation_id: operation_id.clone(),
                            epoch_index: 0,
                            source_leaf_id: Some(source_leaf.clone()),
                            harness_revision_id: revision.clone(),
                            harness_snapshot_id: snapshot_id.clone(),
                            model_harness_profile: profile.clone(),
                            core_run_id: CoreRunId::new(format!(
                                "state-generation-core-run-{}",
                                epoch_id.as_str(),
                            ))
                            .expect("fixture core run ID"),
                            epoch_resume_data: BTreeMap::new(),
                        },
                    )),
                ])
                .expect("generation operation commit builds"),
            )
            .expect("generation operation starts");
    }
}
