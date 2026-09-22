//! User-facing forks anchored only at durable settled-turn checkpoints.
//!
//! A fork never accepts an arbitrary entry ID. Its semantic history and
//! immutable harness configuration come from the checkpoint leaf, while its
//! extension state begins at the checkpoint's exact private-state snapshot.

use super::{
    LaneRuntime, SessionSupervisor, thinking_level_from_name, validate_reserved_host_tool_names,
    validate_runtime_model_selection,
};
use crate::harness::HarnessError;
use crate::runtime::RuntimeServices;
use std::sync::Arc;
use tea_session::{
    ForkedLaneFact, LaneId, LaneMutation, SessionCommit, SessionCommitItem, SessionFact,
    SessionSnapshot, SessionWriter, TurnCheckpointFact, TurnCheckpointId,
    preview_session_commit, reduce_lane,
};

/// A fresh user-facing lane created from one settled-turn checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettledTurnFork {
    checkpoint_id: TurnCheckpointId,
    lane_id: LaneId,
}

impl SettledTurnFork {
    /// The immutable settled-turn boundary selected by the caller.
    pub fn checkpoint_id(&self) -> &TurnCheckpointId {
        &self.checkpoint_id
    }

    /// The fresh independently executable lane identity.
    pub fn lane_id(&self) -> &LaneId {
        &self.lane_id
    }
}

impl<S> SessionSupervisor<S>
where
    S: SessionWriter + Send + 'static,
{
    /// Create one user-facing branch using the current root host services.
    ///
    /// The exact checkpoint configuration remains authoritative: the cloned
    /// services are validated against it before the branch becomes durable.
    /// Hosts that intentionally supply a distinct service bundle use
    /// [`Self::fork_settled_turn_with_services`].
    pub fn fork_settled_turn(
        &self,
        checkpoint_id: TurnCheckpointId,
        lane_id: LaneId,
    ) -> Result<SettledTurnFork, HarnessError> {
        let services = self.root_lane()?.runtime_services.clone();
        self.fork_settled_turn_with_services(checkpoint_id, lane_id, services)
    }

    /// Create one user-facing branch from an exact settled-turn checkpoint.
    ///
    /// This operation is deliberately inert: it commits only lane topology and
    /// the checkpoint binding, then installs fresh process-local services for
    /// the new lane. It does not enqueue input, retain a live handle, inherit
    /// effects or child ownership, or authorize a goal/continuation to run.
    pub fn fork_settled_turn_with_services(
        &self,
        checkpoint_id: TurnCheckpointId,
        lane_id: LaneId,
        services: RuntimeServices,
    ) -> Result<SettledTurnFork, HarnessError> {
        if lane_id == self.root_lane_id {
            return Err(HarnessError::invalid_state(
                "a settled-turn fork requires a fresh non-root lane ID",
            ));
        }

        // A root claim closes the narrow gap between the caller's idle check
        // and the durable fork commit. Other lanes may continue independently;
        // they cannot alter this root checkpoint's history or configuration.
        let root_lane = self.root_lane()?;
        let _claim = self.claim_lane_operation(Arc::clone(&root_lane))?;
        let snapshot = self.snapshot()?;
        let root_reduction = reduce_lane(snapshot.clone(), self.root_lane_id.clone())?;
        ensure_fork_source_is_settled(&root_reduction)?;
        let checkpoint = settled_root_checkpoint(&snapshot, &self.root_lane_id, &checkpoint_id)?;
        if snapshot
            .lane_mutations()
            .iter()
            .any(|stored| matches!(&stored.mutation, LaneMutation::Created { lane_id: existing, .. } if existing == &lane_id))
        {
            return Err(HarnessError::invalid_state(format!(
                "fork lane {lane_id} already exists"
            )));
        }

        // Verify immutable source objects before adding a durable reference to
        // their historical leaf. The preflight uses the same reducer as commit
        // and lets service/configuration errors fail before lane creation.
        let artifact_roots = self.manager.artifact_roots()?;
        tea_session::verify_session(&snapshot, self.artifacts.as_ref(), artifact_roots)?;
        let commit = settled_turn_fork_commit(&checkpoint, lane_id.clone())?;
        let projected = preview_session_commit(&snapshot, &commit, lane_id.clone(), 0)?;
        let services = services_for_fork_lane(services, &projected)?;
        let configuration = self.configuration_for_reduction_services(&services, &projected)?;
        validate_reserved_host_tool_names(&services, &configuration)?;
        let runtime_lane = Arc::new(LaneRuntime::new(lane_id.clone(), services));

        let mut lanes = self
            .lanes
            .lock()
            .map_err(|_| HarnessError::invalid_state("lane map mutex is poisoned"))?;
        if lanes.contains_key(&lane_id) {
            return Err(HarnessError::invalid_state(format!(
                "runtime lane {lane_id} is already registered"
            )));
        }
        {
            let mut session = self.session_lock()?;
            let current = session.snapshot()?;
            if current.last_digest() != snapshot.last_digest() {
                return Err(HarnessError::invalid_state(
                    "durable session changed while the fork was being validated; retry the fork",
                ));
            }
            session.commit(commit)?;
        }
        let previous = lanes.insert(lane_id.clone(), runtime_lane);
        debug_assert!(previous.is_none());
        Ok(SettledTurnFork {
            checkpoint_id,
            lane_id,
        })
    }
}

fn settled_root_checkpoint(
    snapshot: &SessionSnapshot,
    root_lane_id: &LaneId,
    checkpoint_id: &TurnCheckpointId,
) -> Result<TurnCheckpointFact, HarnessError> {
    let checkpoint = snapshot
        .facts()
        .iter()
        .find_map(|stored| match &stored.fact {
            SessionFact::TurnCheckpoint(checkpoint)
                if &checkpoint.checkpoint_id == checkpoint_id =>
            {
                Some(checkpoint.clone())
            }
            _ => None,
        })
        .ok_or_else(|| {
            HarnessError::invalid_state(format!(
                "unknown settled-turn checkpoint {checkpoint_id}"
            ))
        })?;
    if &checkpoint.lane_id != root_lane_id {
        return Err(HarnessError::invalid_state(format!(
            "checkpoint {checkpoint_id} is not a user-facing root-lane boundary"
        )));
    }
    Ok(checkpoint)
}

fn ensure_fork_source_is_settled(
    reduction: &tea_session::LaneReduction,
) -> Result<(), HarnessError> {
    if reduction.lane_state.active_operation.is_some() {
        return Err(HarnessError::invalid_state(
            "user-facing forks require an idle root lane",
        ));
    }
    if !reduction.input_reduction.pending_inputs.is_empty() {
        return Err(HarnessError::invalid_state(
            "user-facing forks require no pending accepted root inputs",
        ));
    }
    if !reduction.pending_extension_controls.is_empty() {
        return Err(HarnessError::invalid_state(
            "user-facing forks require no pending extension controls",
        ));
    }
    Ok(())
}

fn settled_turn_fork_commit(
    checkpoint: &TurnCheckpointFact,
    lane_id: LaneId,
) -> Result<SessionCommit, HarnessError> {
    SessionCommit::new(vec![
        SessionCommitItem::Lane(LaneMutation::Created {
            lane_id: lane_id.clone(),
            base_leaf_id: checkpoint.leaf_id.clone(),
        }),
        SessionCommitItem::Fact(SessionFact::ForkedLane(ForkedLaneFact {
            checkpoint_id: checkpoint.checkpoint_id.clone(),
            lane_id,
            base_leaf_id: checkpoint.leaf_id.clone(),
        })),
    ])
    .map_err(Into::into)
}

fn services_for_fork_lane(
    mut services: RuntimeServices,
    reduction: &tea_session::LaneReduction,
) -> Result<RuntimeServices, HarnessError> {
    validate_runtime_model_selection(&services, reduction)?;
    if let Some(level) = reduction.effective_configuration.thinking_level.as_deref() {
        services = services.thinking_level(thinking_level_from_name(level)?);
    }
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::{CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream};
    use crate::state::ModelDescriptor;
    use crate::tool::ToolRegistry;
    use std::collections::BTreeMap;
    use tea_protocol::JsonValue;
    use tea_session::{
        ExtensionStateValue, ExtensionStateValueSetFact, InputAcceptedRecord, InputSettledRecord,
        LaneRecord, OperationFinishedRecord, OperationId, OperationKind, OperationOutcome,
        OperationStartedRecord, ProvisionedEntry, SessionHeader, SessionId,
    };

    #[derive(Debug)]
    struct UnusedProvider;

    impl ModelProvider for UnusedProvider {
        fn stream<'a>(
            &'a self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> ModelFuture<'a> {
            Box::pin(std::future::ready(Ok(Box::new(ModelStream::default()) as _)))
        }
    }

    fn checkpoint_fixture() -> (tea_session::MemorySession, TurnCheckpointFact) {
        let lane = LaneId::main();
        let input = ProvisionedEntry::user(
            tea_session::EntryId::new("fork-input").expect("valid input ID"),
            "settled source turn",
        );
        let operation_id = OperationId::new("fork-operation").expect("valid operation ID");
        let checkpoint_id = TurnCheckpointId::new("fork-checkpoint").expect("valid checkpoint ID");
        let state = ExtensionStateValue {
            state_version: "todo.v1".into(),
            value: JsonValue::object([("open", JsonValue::from(1_u64))]),
        };
        let checkpoint = TurnCheckpointFact {
            checkpoint_id,
            lane_id: lane.clone(),
            operation_id: operation_id.clone(),
            leaf_id: Some(input.id.clone()),
            extension_state: BTreeMap::from([("todo".into(), state.clone())]),
        };
        let revision = tea_session::HarnessRevisionId::new("fork-revision")
            .expect("valid revision ID");
        let profile = tea_session::ModelHarnessProfileId::new("fork-profile")
            .expect("valid profile ID");
        let mut session = tea_session::MemorySession::create(SessionHeader::new(
            SessionId::new("fork-session").expect("valid session ID"),
            "workspace-test",
            BTreeMap::new(),
        ))
        .expect("memory session creates");
        session
            .commit(SessionCommit::one(SessionCommitItem::Record(LaneRecord::InputAccepted(
                InputAcceptedRecord {
                    lane_id: lane.clone(),
                    entry: input.clone(),
                },
            ))))
            .expect("input accepts");
        session
            .commit(
                SessionCommit::new(vec![
                    SessionCommitItem::Record(LaneRecord::OperationStarted(
                        OperationStartedRecord::new(
                            operation_id.clone(),
                            lane.clone(),
                            None,
                            OperationKind::Run,
                            vec![input.clone()],
                            revision,
                            profile,
                        )
                        .with_input_ids(vec![input.id.clone()]),
                    )),
                    SessionCommitItem::Entry {
                        lane_id: lane.clone(),
                        entry: input.clone(),
                    },
                ])
                .expect("operation starts and input materializes"),
            )
            .expect("operation dispatches");
        session
            .commit(
                SessionCommit::new(vec![
                    SessionCommitItem::Record(LaneRecord::OperationFinished(
                        OperationFinishedRecord {
                            operation_id,
                            outcome: OperationOutcome::Completed,
                        },
                    )),
                    SessionCommitItem::Record(LaneRecord::InputSettled(InputSettledRecord {
                        operation_id: checkpoint.operation_id.clone(),
                        input_id: input.id.clone(),
                        outcome: OperationOutcome::Completed,
                    })),
                    SessionCommitItem::Fact(SessionFact::ExtensionStateValueSet(
                        ExtensionStateValueSetFact {
                            lane_id: lane.clone(),
                            extension_id: "todo".into(),
                            state_version: state.state_version.clone(),
                            value: state.value.clone(),
                        },
                    )),
                    SessionCommitItem::Fact(SessionFact::TurnCheckpoint(checkpoint.clone())),
                ])
                .expect("settlement checkpoint commit"),
            )
            .expect("checkpoint settles");
        (session, checkpoint)
    }

    #[test]
    fn fork_commit_inherits_only_the_checkpoint_leaf_and_private_state() {
        let (mut session, checkpoint) = checkpoint_fixture();
        let fork_lane = LaneId::new("fork-target").expect("valid fork lane ID");
        let commit = settled_turn_fork_commit(&checkpoint, fork_lane.clone())
            .expect("fork commit builds from checkpoint only");
        let snapshot = session.snapshot().expect("snapshot succeeds");
        let projected = preview_session_commit(&snapshot, &commit, fork_lane.clone(), 0)
            .expect("valid checkpoint fork previews");
        assert_eq!(projected.lane_state.leaf_id, checkpoint.leaf_id);
        assert_eq!(projected.extension_state, checkpoint.extension_state);
        assert!(projected.lane_state.active_operation.is_none());
        assert!(projected.pending_extension_controls.is_empty());

        session.commit(commit).expect("fork commit persists");
        session
            .append_fact(SessionFact::ExtensionStateValueSet(
                ExtensionStateValueSetFact {
                    lane_id: LaneId::main(),
                    extension_id: "todo".into(),
                    state_version: "todo.v1".into(),
                    value: JsonValue::object([("open", JsonValue::from(99_u64))]),
                },
            ))
            .expect("source state later changes");
        assert_eq!(
            tea_session::extension_state_for_lane(
                &session.snapshot().expect("snapshot succeeds"),
                fork_lane,
            )
            .expect("fork state reduces"),
            checkpoint.extension_state,
            "later source-lane state cannot leak into the fork"
        );
    }

    #[test]
    fn fork_commit_cannot_choose_an_arbitrary_leaf() {
        let (_session, checkpoint) = checkpoint_fixture();
        let commit = settled_turn_fork_commit(
            &checkpoint,
            LaneId::new("fork-shape").expect("valid fork lane ID"),
        )
        .expect("fork commit builds");
        let [SessionCommitItem::Lane(LaneMutation::Created { base_leaf_id, .. }),
            SessionCommitItem::Fact(SessionFact::ForkedLane(fact))] = commit.items()
        else {
            panic!("fork has exactly one topology mutation and one checkpoint binding");
        };
        assert_eq!(base_leaf_id, &checkpoint.leaf_id);
        assert_eq!(fact.base_leaf_id, checkpoint.leaf_id);
        assert_eq!(fact.checkpoint_id, checkpoint.checkpoint_id);
    }

    #[test]
    fn fork_source_rejects_pending_accepted_input() {
        let (mut session, _checkpoint) = checkpoint_fixture();
        session
            .append_record(LaneRecord::InputAccepted(InputAcceptedRecord {
                lane_id: LaneId::main(),
                entry: ProvisionedEntry::user(
                    tea_session::EntryId::new("fork-pending-input")
                        .expect("valid pending input ID"),
                    "do not inherit this queue item",
                ),
            }))
            .expect("post-checkpoint input accepts");

        let reduction = reduce_lane(
            session.snapshot().expect("snapshot succeeds"),
            LaneId::main(),
        )
        .expect("source lane reduces");
        let error = ensure_fork_source_is_settled(&reduction)
            .expect_err("pending root input blocks a user-facing fork");

        assert!(
            error
                .to_string()
                .contains("no pending accepted root inputs")
        );
    }

    #[test]
    fn fork_services_must_match_the_checkpoint_model() {
        let (session, checkpoint) = checkpoint_fixture();
        let projected = preview_session_commit(
            &session.snapshot().expect("snapshot succeeds"),
            &settled_turn_fork_commit(
                &checkpoint,
                LaneId::new("fork-model").expect("valid fork lane ID"),
            )
            .expect("fork commit builds"),
            LaneId::new("fork-model").expect("valid fork lane ID"),
            0,
        )
        .expect("fork reduction previews");
        let mut projected = projected;
        projected.effective_configuration.model = Some(tea_session::ModelChangedEntry {
            provider: "fixture-provider".into(),
            model: "historical-model".into(),
            revision: Some("historical-revision".into()),
        });
        let mismatched = RuntimeServices::new(Arc::new(UnusedProvider), ToolRegistry::default())
            .model(ModelDescriptor {
                provider: "fixture-provider".into(),
                model: "current-model".into(),
                revision: Some("current-revision".into()),
            });

        let error = services_for_fork_lane(mismatched, &projected)
            .expect_err("fork must not execute historical configuration with another model");

        assert!(error.to_string().contains("durable model selection"));
    }
}
