use super::*;

struct InterruptedMutation;

impl AgentTool for InterruptedMutation {
    fn name(&self) -> &str {
        "record"
    }
    fn description(&self) -> &str {
        "records one durable tool intent"
    }
    fn schema(&self) -> &JsonValue {
        RecordingTool.schema()
    }
    fn execute<'a>(
        &'a self,
        _call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            context.cancellation.cancel();
            std::future::pending().await
        })
    }
}

#[test]
fn cancelling_an_admitted_mutation_settles_the_run_but_requires_reconciliation() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("cancelled-mutation").unwrap(),
                            name: "record".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Error)],
                },
            ])),
        });
        let mut tools = ToolRegistry::default();
        tools.insert(Arc::new(InterruptedMutation));
        let services = RuntimeServices::new(provider, tools);
        let mut repository =
            HarnessRepository::with_extension_engine(store.clone(), Arc::new(NoExtensions));
        let snapshot = repository
            .stage_snapshot(snapshot_spec(services.runtime_policy_identities()))
            .unwrap();
        let revision = repository
            .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
            .unwrap();
        let identity = HarnessIdentity::new(
            revision.revision_id,
            snapshot.id,
            snapshot.spec.model_harness_profile,
        );
        let manager = Arc::new(HarnessResolver::new(repository, Default::default()));
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("cancelled-mutation-session").unwrap(),
            "synthetic-workspace",
            fixture_metadata(),
        ))
        .unwrap();
        append_initial_revision(&mut session, &identity);
        let runtime = SessionSupervisor::create(SessionSupervisorInput {
            session,
            resolver: manager,
            root_identity: identity,
            root_services: services,
            artifacts: store,
            rollover_budget: 1,
            subagents: None,
        })
        .unwrap();
        assert!(
            runtime
                .run_root_prompt("mutate synthetic data")
                .await
                .is_err()
        );
        assert!(!runtime.is_active());
        let snapshot = runtime.snapshot().unwrap();
        assert!(
            snapshot.entries().iter().all(
                |entry| !matches!(&entry.body, SessionEntry::ToolResult(result)
            if result.tool_call_id == "cancelled-mutation")
            ),
            "{:?}",
            snapshot.entries()
        );
        assert!(
            snapshot
                .records()
                .iter()
                .any(|stored| matches!(&stored.record,
            LaneRecord::OperationFinished(record) if record.outcome == OperationOutcome::Aborted))
        );
        assert!(matches!(
            runtime.run_root_prompt("try another action").await,
            Err(HarnessError::RecoveryRequired { .. })
        ));
    });
}
use std::sync::atomic::{AtomicUsize, Ordering};
use tea_session::{
    ProviderRequestId, ProviderRequestStartedRecord, StepAttemptedRecord, StepId, StepKind,
};

struct RecoveryProvider {
    calls: Arc<AtomicUsize>,
}

impl ModelProvider for RecoveryProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(Box::new(completion_stream()) as _)))
    }
}

fn interrupted_runtime(tool: bool) -> (Arc<SessionSupervisor<MemorySession>>, Arc<AtomicUsize>) {
    interrupted_runtime_with_calls(tool, 1)
}

fn interrupted_runtime_with_calls(
    tool: bool,
    tool_count: usize,
) -> (Arc<SessionSupervisor<MemorySession>>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(MemoryArtifactStore::default());
    let (manager, identity, services) = fixture_manager(
        Arc::new(RecoveryProvider {
            calls: calls.clone(),
        }),
        store.clone(),
    );
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("recovery-contract").unwrap(),
        "synthetic-workspace",
        fixture_metadata(),
    ))
    .unwrap();
    append_initial_revision(&mut session, &identity);
    let operation_id = OperationId::new("interrupted-operation").unwrap();
    let epoch_id = EpochId::new("interrupted-epoch").unwrap();
    let input = ProvisionedEntry::user(
        EntryId::new("accepted-user").unwrap(),
        "complete the synthetic task",
    );
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            operation_id.clone(),
            LaneId::main(),
            Some(EntryId::new("runtime-test-initial-revision").unwrap()),
            OperationKind::Run,
            vec![input.clone()],
            identity.revision_id().clone(),
            identity.profile_id().clone(),
        )))
        .unwrap();
    session.append_entry(&LaneId::main(), input).unwrap();
    session
        .append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(EntryId::new("accepted-user").unwrap()),
            harness_revision_id: identity.revision_id().clone(),
            harness_snapshot_id: identity.snapshot_id().clone(),
            model_harness_profile: identity.profile_id().clone(),
            core_run_id: CoreRunId::new("interrupted-core-run").unwrap(),
            epoch_resume_data: BTreeMap::new(),
        }))
        .unwrap();
    if tool {
        let assistant_id = EntryId::new("committed-tool-decision").unwrap();
        let tool_calls = (0..tool_count)
            .map(|index| {
                AssistantToolCall::new(
                    if index == 0 {
                        "indeterminate-call".into()
                    } else {
                        format!("uninvoked-call-{index}")
                    },
                    "record",
                    JsonValue::parse("{}").unwrap(),
                )
            })
            .collect();
        let mut assistant = ProvisionedEntry::assistant(assistant_id.clone(), "", tool_calls);
        if let SessionEntry::AssistantMessage(message) = &mut assistant.body {
            message.stop_reason = Some("tool_use".into());
        }
        session.append_entry(&LaneId::main(), assistant).unwrap();
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
                RecordId::new("indeterminate-intent").unwrap(),
                operation_id,
                epoch_id,
                assistant_id,
                0,
                "indeterminate-call",
                "record",
                JsonValue::parse("{}").unwrap(),
                EntryId::new("indeterminate-result").unwrap(),
                ToolReplayPolicy::Never,
                super::super::supervisor::tool_definition_digest(&RecordingTool).unwrap(),
                identity.revision_id().clone(),
                "indeterminate-key",
            )))
            .unwrap();
    } else {
        let step_id = StepId::new("interrupted-step").unwrap();
        session
            .append_record(LaneRecord::StepAttempted(StepAttemptedRecord {
                id: step_id.clone(),
                operation_id: operation_id.clone(),
                epoch_id: epoch_id.clone(),
                kind: StepKind::Assistant,
                attempt: 1,
                result_entry_id: EntryId::new("uncommitted-answer").unwrap(),
                reason: None,
            }))
            .unwrap();
        let request_id = ProviderRequestId::new("interrupted-provider-request").unwrap();
        session
            .commit(
                tea_session::SessionCommit::new(vec![
                    tea_session::SessionCommitItem::Record(LaneRecord::ProviderRequestStarted(
                        ProviderRequestStartedRecord {
                            request_id: request_id.clone(),
                            operation_id: operation_id.clone(),
                            epoch_id: epoch_id.clone(),
                            step_id,
                            physical_attempt: 1,
                            model_harness_profile: identity.profile_id().clone(),
                            request_surface_digest: Digest::from_bytes("synthetic request"),
                            idempotency_key: None,
                        },
                    )),
                    tea_session::SessionCommitItem::Fact(SessionFact::ProviderRequestMaterial(
                        tea_session::ProviderRequestMaterialFact {
                            operation_id,
                            epoch_id,
                            request_id,
                            request: PayloadRef::Inline(JsonValue::object([(
                                "prompt",
                                JsonValue::String("complete the synthetic task".into()),
                            )])),
                        },
                    )),
                ])
                .unwrap(),
            )
            .unwrap();
    }
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity: identity,
        root_services: services,
        artifacts: store,
        rollover_budget: 1,
        subagents: None,
    })
    .unwrap();
    (runtime, calls)
}

#[test]
fn generic_indeterminate_tool_blocks_continue_and_new_prompt_without_mutation() {
    smol::block_on(async {
        let (runtime, calls) = interrupted_runtime(true);
        let before = runtime.snapshot().unwrap();
        assert!(matches!(
            runtime.resume().await,
            Err(HarnessError::RecoveryRequired { .. })
        ));
        assert!(matches!(
            runtime.run_root_prompt("try again").await,
            Err(HarnessError::RecoveryRequired { .. })
        ));
        assert_eq!(runtime.snapshot().unwrap(), before);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn interrupted_provider_requires_explicit_continue_and_retains_unknown_usage() {
    smol::block_on(async {
        let (runtime, calls) = interrupted_runtime(false);
        let session = runtime.clone_session_for_test().unwrap();
        let (resolver, root_services, artifacts, subagents) =
            runtime.reopen_parts_for_test().unwrap();
        let before = session.snapshot().unwrap();
        drop(runtime);
        let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            artifacts,
            subagents,
            lane_services: BTreeMap::new(),
            rollover_budget: 1,
        })
        .unwrap();
        assert_eq!(reopened.snapshot().unwrap(), before);
        for _ in 0..8 {
            assert!(!reopened.is_active());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
        assert!(reopened.resume().await.unwrap().is_completed());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let snapshot = reopened.snapshot().unwrap();
        let interrupted = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::ProviderRequestSettled(record)
                    if record.request_id.as_str() == "interrupted-provider-request" =>
                {
                    Some(record)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            interrupted.classification,
            tea_session::ProviderSettlementClassification::Interrupted
        );
        assert_eq!(interrupted.usage, None);
        assert!(
            snapshot
                .entries()
                .iter()
                .all(|entry| entry.header.id.as_str() != "uncommitted-answer")
        );
        assert!(reopened.resume().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

fn reopen_after_committed_assistant(
    stop_reason: &str,
) -> (Arc<SessionSupervisor<MemorySession>>, Arc<AtomicUsize>) {
    let (runtime, calls) = interrupted_runtime(false);
    let mut session = runtime.clone_session_for_test().unwrap();
    let (resolver, root_services, artifacts, subagents) = runtime.reopen_parts_for_test().unwrap();
    let mut answer = ProvisionedEntry::assistant(
        EntryId::new("uncommitted-answer").unwrap(),
        "retained answer",
        Vec::new(),
    );
    if let SessionEntry::AssistantMessage(message) = &mut answer.body {
        message.stop_reason = Some(stop_reason.into());
    }
    session
        .commit(
            tea_session::SessionCommit::new(vec![
                tea_session::SessionCommitItem::Record(LaneRecord::ProviderRequestSettled(
                    tea_session::ProviderRequestSettledRecord {
                        request_id: ProviderRequestId::new("interrupted-provider-request").unwrap(),
                        operation_id: OperationId::new("interrupted-operation").unwrap(),
                        outcome: JsonValue::object([(
                            "stop_reason",
                            JsonValue::String(stop_reason.into()),
                        )]),
                        provider_error: None,
                        usage: None,
                        response_artifact: None,
                        classification: tea_session::ProviderSettlementClassification::Completed,
                    },
                )),
                tea_session::SessionCommitItem::Entry {
                    lane_id: LaneId::main(),
                    entry: answer,
                },
            ])
            .unwrap(),
        )
        .unwrap();
    drop(runtime);
    (
        SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            artifacts,
            subagents,
            lane_services: BTreeMap::new(),
            rollover_budget: 1,
        })
        .unwrap(),
        calls,
    )
}

#[test]
fn committed_final_assistant_settles_without_repeating_inference() {
    smol::block_on(async {
        let (runtime, calls) = reopen_after_committed_assistant("stop");
        assert!(runtime.resume().await.unwrap().is_completed());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(runtime.resume().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn committed_length_assistant_continues_with_a_fresh_attempt() {
    smol::block_on(async {
        let (runtime, calls) = reopen_after_committed_assistant("length");
        assert!(runtime.resume().await.unwrap().is_completed());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime
                .snapshot()
                .unwrap()
                .entries()
                .iter()
                .filter(|entry| matches!(&entry.body, SessionEntry::AssistantMessage(_)))
                .count(),
            2
        );
    });
}

#[test]
fn parallel_safe_tool_recovery_reuses_every_admitted_intent() {
    smol::block_on(async {
        let (runtime, calls) = interrupted_runtime(false);
        let mut session = runtime.clone_session_for_test().unwrap();
        let (resolver, root_services, artifacts, subagents) =
            runtime.reopen_parts_for_test().unwrap();
        let snapshot = session.snapshot().unwrap();
        let operation_id = OperationId::new("interrupted-operation").unwrap();
        let epoch_id = EpochId::new("interrupted-epoch").unwrap();
        let revision_id = snapshot
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(epoch) => Some(epoch.harness_revision_id.clone()),
                _ => None,
            })
            .unwrap();
        let assistant_id = EntryId::new("uncommitted-answer").unwrap();
        let mut answer = ProvisionedEntry::assistant(
            assistant_id.clone(),
            "",
            (0..2)
                .map(|index| {
                    AssistantToolCall::new(
                        format!("safe-call-{index}"),
                        "record",
                        JsonValue::object([] as [(&str, JsonValue); 0]),
                    )
                })
                .collect(),
        );
        if let SessionEntry::AssistantMessage(message) = &mut answer.body {
            message.stop_reason = Some("tool_use".into());
        }
        session
            .append_record(LaneRecord::ProviderRequestSettled(
                tea_session::ProviderRequestSettledRecord {
                    request_id: ProviderRequestId::new("interrupted-provider-request").unwrap(),
                    operation_id: operation_id.clone(),
                    outcome: JsonValue::Null,
                    provider_error: None,
                    usage: None,
                    response_artifact: None,
                    classification: tea_session::ProviderSettlementClassification::Completed,
                },
            ))
            .unwrap();
        session.append_entry(&LaneId::main(), answer).unwrap();
        for index in 0..2 {
            let ordinal = index.to_string();
            let identifier = |kind| {
                super::super::supervisor::durable_identifier(
                    kind,
                    [assistant_id.as_str(), ordinal.as_str()],
                )
            };
            session
                .append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
                    RecordId::new(identifier("record-tool-start")).unwrap(),
                    operation_id.clone(),
                    epoch_id.clone(),
                    assistant_id.clone(),
                    index,
                    format!("safe-call-{index}"),
                    "record",
                    JsonValue::parse("{}").unwrap(),
                    EntryId::new(identifier("entry-tool-result")).unwrap(),
                    ToolReplayPolicy::Safe,
                    super::super::supervisor::tool_definition_digest(&RecordingTool).unwrap(),
                    revision_id.clone(),
                    identifier("tool-invocation"),
                )))
                .unwrap();
        }
        drop(runtime);
        let runtime = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services: root_services.replay_safe_tool("record"),
            artifacts,
            subagents,
            lane_services: BTreeMap::new(),
            rollover_budget: 1,
        })
        .unwrap();
        assert!(runtime.resume().await.unwrap().is_completed());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(
            snapshot
                .records()
                .iter()
                .filter(|stored| matches!(&stored.record, LaneRecord::ToolStarted(_)))
                .count(),
            2
        );
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(&entry.body, SessionEntry::ToolResult(_)))
                .count(),
            2
        );
    });
}

#[test]
fn reconciling_a_cancelled_mutation_closes_uninvoked_siblings_without_execution() {
    smol::block_on(async {
        let (runtime, calls) = interrupted_runtime_with_calls(true, 2);
        let mut session = runtime.clone_session_for_test().unwrap();
        let (resolver, root_services, artifacts, subagents) =
            runtime.reopen_parts_for_test().unwrap();
        let operation_id = OperationId::new("interrupted-operation").unwrap();
        session
            .commit(
                tea_session::SessionCommit::new(vec![
                    tea_session::SessionCommitItem::Record(LaneRecord::EpochFinished(
                        EpochFinishedRecord {
                            operation_id: operation_id.clone(),
                            epoch_id: EpochId::new("interrupted-epoch").unwrap(),
                            reason: EpochFinishReason::Interrupted,
                        },
                    )),
                    tea_session::SessionCommitItem::Record(LaneRecord::OperationFinished(
                        OperationFinishedRecord {
                            operation_id,
                            outcome: OperationOutcome::Aborted,
                        },
                    )),
                ])
                .unwrap(),
            )
            .unwrap();
        drop(runtime);
        let runtime = SessionSupervisor::reopen(SessionSupervisorReopenInput {
            session,
            resolver,
            root_services,
            artifacts,
            subagents,
            lane_services: BTreeMap::new(),
            rollover_budget: 1,
        })
        .unwrap();
        runtime
            .reconcile_tool_result(
                LaneId::main(),
                EntryId::new("indeterminate-result").unwrap(),
                AgentToolResult {
                    tool_call_id: ToolCallId::new("indeterminate-call").unwrap(),
                    content: "Synthetic marker confirms the mutation completed.".into(),
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                },
                "Host checked the synthetic marker.".into(),
            )
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(runtime.recovery_report().unwrap().lanes.is_empty());
        assert!(
            runtime
                .run_root_prompt("begin a separate task")
                .await
                .unwrap()
                .is_completed()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn committed_terminating_tool_result_settles_without_another_provider_request() {
    smol::block_on(async {
        let (runtime, calls) = interrupted_runtime(true);
        runtime
            .reconcile_tool_result(
                LaneId::main(),
                EntryId::new("indeterminate-result").unwrap(),
                AgentToolResult {
                    tool_call_id: ToolCallId::new("indeterminate-call").unwrap(),
                    content: "The synthetic terminating tool completed.".into(),
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: true,
                    is_error: false,
                    failure: None,
                },
                "Host verified the terminal result.".into(),
            )
            .unwrap();
        assert!(runtime.resume().await.unwrap().is_completed());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn closing_the_supervisor_cancels_and_joins_a_live_fork_lane() {
    smol::block_on(async {
        let (runtime, _) = interrupted_runtime(false);
        runtime.resume().await.unwrap();
        let checkpoint = runtime
            .snapshot()
            .unwrap()
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                SessionFact::TurnCheckpoint(checkpoint) => Some(checkpoint.checkpoint_id.clone()),
                _ => None,
            })
            .unwrap();
        let started = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::default();
        tools.insert(Arc::new(RecordingTool));
        let services = RuntimeServices::new(
            Arc::new(BlockingProvider {
                started: started.clone(),
            }),
            tools,
        );
        let fork = runtime
            .fork_settled_turn_with_services(
                checkpoint,
                LaneId::new("close-fork").unwrap(),
                services,
            )
            .unwrap();
        let mut drive =
            Box::pin(runtime.run_lane_prompt(fork.lane_id().clone(), "hold until close"));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(drive.as_mut().poll(&mut context).is_pending());
        assert_eq!(started.load(Ordering::SeqCst), 1);
        let mut closing = Box::pin(runtime.close());
        assert!(
            closing.as_mut().poll(&mut context).is_pending(),
            "close cannot finish before the fork drive joins"
        );
        assert!(drive.await.is_err());
        closing.await.unwrap();
        let reduction = reduce_lane(runtime.snapshot().unwrap(), fork.lane_id().clone()).unwrap();
        assert!(reduction.lane_state.active_operation.is_none());
    });
}
