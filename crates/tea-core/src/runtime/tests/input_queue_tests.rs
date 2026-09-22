use super::*;
use crate::runtime::{
    ExtensionCommandAdmission, IdleAuthorization, IdleDriveOutcome, InputDisposition,
    InputOutcome, SessionEvent, SessionSupervisorReopenInput, TeaEvent,
};
use crate::harness::extension::{
    ExtensionCapabilityBindings, ExtensionCommandInput, ExtensionCommandResult,
    ExtensionDescriptor, ExtensionEngine, ExtensionError, ExtensionHostCommand,
    ExtensionHostCommandDescription, ExtensionIdleHook, ExtensionIdleInput, ExtensionIdleResult,
    ExtensionLimits, ExtensionMemoryCollector, ExtensionSourceTree, ExtensionStateUpdate,
    ResolvedExtension,
};
use crate::harness::{
    HarnessActor, HarnessResourceLimits, HarnessSeedBuilder, HarnessSeedExtension,
    HarnessSeedExtensionScope, ModelHarnessProfile,
};
use crate::hooks::HookSet;
use crate::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use crate::state::StopReason;
use crate::tool::ToolRegistry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use tea_session::{
    EntryId, LaneRecord, OperationOutcome, SessionFact, SessionHeader, SessionId,
};

#[derive(Clone)]
struct GateProvider {
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl ModelProvider for GateProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.started.store(true, std::sync::atomic::Ordering::Release);
        self.calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            while !release.load(std::sync::atomic::Ordering::Acquire)
                && !cancellation.is_cancelled()
            {
                smol::future::yield_now().await;
            }
            let stream = if cancellation.is_cancelled() {
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Aborted)],
                }
            } else {
                completion_stream()
            };
            Ok(Box::new(stream) as _)
        })
    }
}

#[derive(Clone)]
struct ControlCommand {
    description: ExtensionHostCommandDescription,
}

impl ExtensionHostCommand for ControlCommand {
    fn description(&self) -> &ExtensionHostCommandDescription {
        &self.description
    }

    fn invoke(
        &self,
        input: &ExtensionCommandInput,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        if input.arguments != "pause" {
            return Err(ExtensionError::new("fixture control only accepts pause"));
        }
        Ok(ExtensionCommandResult {
            notice: Some("paused".into()),
            state: Some(ExtensionStateUpdate {
                value: JsonValue::object([("mode", JsonValue::String("paused".into()))]),
            }),
            internal_input: None,
        })
    }
}

struct ControlIdleHook;

impl ExtensionIdleHook for ControlIdleHook {
    fn on_idle(&self, input: &ExtensionIdleInput) -> Result<ExtensionIdleResult, ExtensionError> {
        let paused = input.state.value.as_ref().is_some_and(|value| {
            value
                .get("mode")
                .and_then(JsonValue::as_str)
                .is_some_and(|mode| mode == "paused")
        });
        Ok(if paused {
            ExtensionIdleResult::default()
        } else {
            ExtensionIdleResult {
                state: None,
                internal_input: Some("fixture automatic goal continuation".into()),
            }
        })
    }
}

struct ControlExtensionEngine;

impl ExtensionEngine for ControlExtensionEngine {
    fn describe(
        &self,
        _source: &ExtensionSourceTree,
    ) -> Result<ExtensionDescriptor, ExtensionError> {
        Ok(ExtensionDescriptor {
            requested_capabilities: BTreeSet::new(),
            prompt_sections: Vec::new(),
            tools: Vec::new(),
            host_commands: vec![control_command_description()],
            lifecycle_hook_ids: Vec::new(),
            state_version: Some("control-v1".into()),
        })
    }

    fn resolve(
        &self,
        _source: &ExtensionSourceTree,
        _bindings: ExtensionCapabilityBindings,
        inner_hooks: Arc<dyn HookSet>,
        _extension_index: usize,
        _memory_collector: Arc<ExtensionMemoryCollector>,
    ) -> Result<ResolvedExtension, ExtensionError> {
        Ok(ResolvedExtension {
            hooks: inner_hooks,
            tools: ToolRegistry::default(),
            host_commands: vec![Arc::new(ControlCommand {
                description: control_command_description(),
            })],
            idle_hook: Some(Arc::new(ControlIdleHook)),
            context_policy: None,
            lifecycle: None,
        })
    }
}

fn control_command_description() -> ExtensionHostCommandDescription {
    ExtensionHostCommandDescription {
        name: "/control".into(),
        help: "pause fixture continuation".into(),
        allowed_while_active: true,
    }
}

fn build_control_runtime(
    session_id: &str,
    provider: Arc<dyn ModelProvider>,
) -> Arc<SessionSupervisor<MemorySession>> {
    let store = Arc::new(MemoryArtifactStore::default());
    let services = RuntimeServices::new(provider, ToolRegistry::default());
    let resource_limits = HarnessResourceLimits::default();
    let profile = ModelHarnessProfile::new(
        "fixture",
        "control-model",
        None,
        "control-prompt",
        "control-tools",
        "control-compaction",
        "control-projection",
    )
    .expect("control fixture profile is valid");
    let source = ExtensionSourceTree {
        extension_id: "control".into(),
        files: BTreeMap::from([
            ("manifest.json".into(), "{}".into()),
            ("init.luau".into(), "return {}".into()),
        ]),
        expected_capabilities: Some(BTreeSet::new()),
        limits: ExtensionLimits {
            max_source_bytes: resource_limits.source_bytes,
            max_memory_bytes: resource_limits.memory_bytes,
            max_interrupt_checks: resource_limits.instruction_checks as usize,
        },
    };
    let seeded = HarnessSeedBuilder::new(
        store.clone(),
        Arc::new(ControlExtensionEngine),
        Digest::from_bytes("control-fixture-host-profile"),
        "control fixture system prompt",
        profile,
        SelfExtensionMode::Off,
        resource_limits,
        services.runtime_policy_identities(),
    )
    .extensions(vec![HarnessSeedExtension {
        scope: HarnessSeedExtensionScope::Global,
        source,
    }])
    .seed(HarnessActor::Host, 1)
    .expect("control fixture immutable harness seeds");
    let identity = HarnessIdentity::new(
        seeded.revision.revision_id.clone(),
        seeded.snapshot.id.clone(),
        seeded.snapshot.spec.model_harness_profile.clone(),
    );
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new(session_id).expect("fixture session ID is valid"),
        "runtime-control-fixture-workspace",
        fixture_metadata(),
    ))
    .expect("control fixture session creates");
    append_initial_revision(&mut session, &identity);
    SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: Arc::new(HarnessResolver::new(seeded.repository, BTreeSet::new())),
        root_identity: identity,
        root_services: services,
        artifacts: store,
        rollover_budget: 1,
        subagents: None,
    })
    .expect("control fixture supervisor creates")
}

#[test]
fn accepted_inputs_preserve_atomic_withdrawal_and_own_terminal_handles() {
    let provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let store = Arc::new(MemoryArtifactStore::default());
    let (runtime, _) = build_runtime("runtime-input-withdraw", provider, store);

    let first = runtime
        .submit_input("first retained message")
        .expect("first input is durably accepted");
    let second = runtime
        .submit_input("second retained message")
        .expect("second input is durably accepted");
    assert_eq!(
        runtime
            .queued_inputs()
            .expect("queue projects")
            .iter()
            .map(|input| input.content())
            .collect::<Vec<_>>(),
        vec!["first retained message", "second retained message"],
    );

    let unknown = EntryId::new("input-not-accepted").expect("fixture ID is valid");
    assert!(
        runtime
            .withdraw_inputs(&[first.id().clone(), unknown])
            .is_err(),
        "one ineligible member rejects the entire atomic withdrawal"
    );
    assert_eq!(
        runtime
            .queued_inputs()
            .expect("failed withdrawal leaves queue intact")
            .iter()
            .map(|input| input.id().clone())
            .collect::<Vec<_>>(),
        vec![first.id().clone(), second.id().clone()],
    );

    let withdrawn = runtime
        .withdraw_inputs(&[second.id().clone(), first.id().clone()])
        .expect("both undispatched inputs withdraw together");
    assert_eq!(
        withdrawn
            .inputs()
            .iter()
            .map(|input| input.content())
            .collect::<Vec<_>>(),
        vec!["second retained message", "first retained message"],
        "the runtime returns exact payloads only after their grouped withdrawal commits",
    );
    assert!(
        runtime
            .queued_inputs()
            .expect("queue projects after withdrawal")
            .is_empty()
    );
    for input in [&first, &second] {
        assert_eq!(
            runtime
                .input_status(input.id())
                .expect("durable input status reads"),
            Some(InputDisposition::Withdrawn),
        );
        assert!(matches!(
            input.completion().try_result().map(|result| result.outcome().clone()),
            Some(InputOutcome::Withdrawn),
        ));
    }
}

#[test]
fn queued_input_changes_reach_a_subscriber_registered_before_nonexecuting_admission() {
    let provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let store = Arc::new(MemoryArtifactStore::default());
    let (runtime, _) = build_runtime("runtime-input-queue-observation", provider, store);
    let subscription = runtime
        .subscribe_events()
        .expect("subscription captures the initial queue snapshot");
    assert!(
        runtime
            .queued_inputs()
            .expect("queue projects before admission")
            .is_empty()
    );

    let accepted = runtime
        .submit_input("observe this queued input")
        .expect("input accepts without executing");
    let accepted_sequence = runtime
        .snapshot()
        .expect("accepted snapshot reads")
        .last_sequence();
    assert_ne!(subscription.snapshot.view.sequence, accepted_sequence);
    assert!(matches!(
        subscription.try_recv().expect("accepted input change is observed"),
        TeaEvent::Session(SessionEvent::InputQueueChanged { sequence, lane_id })
            if sequence == accepted_sequence && lane_id == LaneId::main()
    ));
    assert_eq!(
        runtime
            .queued_inputs()
            .expect("queue projects after admission")
            .iter()
            .map(|input| input.content())
            .collect::<Vec<_>>(),
        vec!["observe this queued input"],
        "the event asks consumers to refresh durable queued input projection"
    );

    runtime
        .withdraw_inputs(&[accepted.id().clone()])
        .expect("queued input withdraws without executing");
    let withdrawn_sequence = runtime
        .snapshot()
        .expect("withdrawn snapshot reads")
        .last_sequence();
    assert!(matches!(
        subscription.try_recv().expect("withdrawal change is observed"),
        TeaEvent::Session(SessionEvent::InputQueueChanged { sequence, lane_id })
            if sequence == withdrawn_sequence && lane_id == LaneId::main()
    ));
    assert!(
        runtime
            .queued_inputs()
            .expect("queue projects after withdrawal")
            .is_empty()
    );
    assert!(matches!(
        accepted.completion().try_result(),
        Some(completion) if matches!(completion.outcome(), InputOutcome::Withdrawn)
    ));
}

#[test]
fn combined_input_dispatch_records_ordered_membership_and_survives_reopen() {
    let provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::from([completion_stream()])),
    });
    let store = Arc::new(MemoryArtifactStore::default());
    let (runtime, _) = build_runtime("runtime-combined-input-dispatch", provider, store);
    let first = runtime
        .submit_input("combine first")
        .expect("first input accepts");
    let second = runtime
        .submit_input("combine second")
        .expect("second input accepts");

    let (operation_id, input_ids) = smol::block_on(async {
        match runtime
            .drive_next_input(IdleAuthorization::UserInputOnly)
            .await
            .expect("accepted batch drives through the fixed agent FSM")
        {
            IdleDriveOutcome::Inputs {
                operation,
                input_ids,
            } => (operation.id().clone(), input_ids),
            other => panic!("expected accepted input batch, got {other:?}"),
        }
    });
    assert_eq!(input_ids, vec![first.id().clone(), second.id().clone()]);
    let snapshot = runtime.snapshot().expect("session snapshot reads");
    let membership = snapshot
        .records()
        .iter()
        .find_map(|stored| match &stored.record {
            LaneRecord::OperationStarted(record) if record.id == operation_id => {
                Some(record.input_ids.clone())
            }
            _ => None,
        })
        .expect("batch operation persists explicit membership");
    assert_eq!(membership, input_ids);

    for input in [&first, &second] {
        assert!(matches!(
            input.completion().try_result().map(|result| result.outcome().clone()),
            Some(InputOutcome::Operation {
                operation_id: ref settled_operation,
                outcome: OperationOutcome::Completed,
            }) if settled_operation == &operation_id,
        ));
        assert_eq!(
            runtime
                .input_status(input.id())
                .expect("durable input status reads"),
            Some(InputDisposition::Settled {
                operation_id: operation_id.clone(),
                outcome: OperationOutcome::Completed,
            }),
        );
    }

    let session = runtime
        .clone_session_for_test()
        .expect("completed session clones for reopen");
    let (resolver, root_services, artifacts, subagents) = runtime
        .reopen_parts_for_test()
        .expect("host-owned reopen inputs remain available");
    drop(runtime);
    let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
        session,
        resolver,
        root_services,
        lane_services: BTreeMap::new(),
        artifacts,
        rollover_budget: 1,
        subagents,
    })
    .expect("completed durable session reopens without an event subscription");
    let recovered = reopened
        .input_completion_handle(first.id())
        .expect("durable completion handle reconstructs")
        .expect("accepted input remains known after reopen");
    assert!(matches!(
        recovered.try_result().map(|result| result.outcome().clone()),
        Some(InputOutcome::Operation {
            operation_id: ref settled_operation,
            outcome: OperationOutcome::Completed,
        }) if settled_operation == &operation_id,
    ));
    assert!(reopened
        .queued_inputs()
        .expect("reopened queue projects")
        .is_empty());
}

#[test]
fn direct_root_prompt_rejects_a_busy_claim_without_queuing_hidden_input() {
    let provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let store = Arc::new(MemoryArtifactStore::default());
    let (runtime, _) = build_runtime("runtime-direct-prompt-busy", provider, store);
    let before = runtime.snapshot().expect("snapshot before rejected prompt");
    runtime
        .claim_root_before_acceptance_for_test()
        .expect("fixture holds the root drive claim");

    assert!(smol::block_on(runtime.run_root_prompt("must not be accepted")).is_err());
    assert_eq!(
        runtime.snapshot().expect("snapshot after rejected prompt"),
        before,
        "a convenience caller receives no invisible queued input when its root drive is rejected",
    );
    assert!(runtime
        .queued_inputs()
        .expect("queue remains inspectable")
        .is_empty());
}

#[test]
fn close_preserves_accepted_queue_but_rejects_new_execution_authority() {
    let provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let store = Arc::new(MemoryArtifactStore::default());
    let (runtime, _) = build_runtime("runtime-close-input-queue", provider, store);
    let accepted = runtime
        .submit_input("keep this durable input")
        .expect("input accepts before close");

    smol::block_on(runtime.close()).expect("idle runtime closes without detached work");
    assert!(runtime.is_closed());
    assert_eq!(
        runtime
            .queued_inputs()
            .expect("closed runtime remains inspectable")
            .iter()
            .map(|input| input.id().clone())
            .collect::<Vec<_>>(),
        vec![accepted.id().clone()],
    );
    assert!(runtime.submit_input("must be rejected after close").is_err());
    assert!(smol::block_on(runtime.drive_next_input(IdleAuthorization::UserInputOnly)).is_err());
    assert!(accepted.completion().try_result().is_none());
}

#[test]
fn close_cancels_and_joins_its_owned_drive_without_settling_queued_input() {
    smol::block_on(async {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(GateProvider {
            started: Arc::clone(&started),
            release,
            calls,
        });
        let store = Arc::new(MemoryArtifactStore::default());
        let (runtime, _) = build_runtime("runtime-close-joins-drive", provider, store);
        let active_input = runtime
            .submit_input("cancel this active input")
            .expect("active fixture input accepts");
        let drive = {
            let runtime = Arc::clone(&runtime);
            smol::spawn(async move {
                runtime
                    .drive_next_input(IdleAuthorization::UserInputOnly)
                    .await
            })
        };
        while !started.load(std::sync::atomic::Ordering::Acquire) {
            smol::future::yield_now().await;
        }
        let queued_input = runtime
            .submit_input("keep this queued input")
            .expect("input remains admissible while a root drive is active");

        let close = {
            let runtime = Arc::clone(&runtime);
            smol::spawn(async move { runtime.close().await })
        };
        close
            .await
            .expect("close returns only after the owned drive claim releases");
        let _ = drive.await;
        assert!(!runtime.is_active());
        assert!(runtime.is_closed());
        assert!(matches!(
            active_input.completion().try_result().map(|result| result.outcome().clone()),
            Some(InputOutcome::Operation {
                outcome: OperationOutcome::Aborted,
                ..
            }),
        ));
        assert_eq!(
            runtime
                .input_status(queued_input.id())
                .expect("queued durable state reads"),
            Some(InputDisposition::Pending),
        );
        assert!(queued_input.completion().try_result().is_none());
    });
}

#[test]
fn queued_control_precedes_user_input_and_stops_headless_goal_continuation() {
    smol::block_on(async {
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(GateProvider {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
            calls: Arc::clone(&calls),
        });
        let runtime = build_control_runtime("runtime-control-precedence", provider);
        let first_input = runtime
            .submit_input("first model turn")
            .expect("first input accepts");
        let first_drive = {
            let runtime = Arc::clone(&runtime);
            smol::spawn(async move {
                runtime
                    .drive_next_input(IdleAuthorization::AllowExtensionContinuation)
                    .await
            })
        };
        while !started.load(std::sync::atomic::Ordering::Acquire) {
            smol::future::yield_now().await;
        }

        let control_id = match runtime
            .dispatch_extension_command("/control", "pause")
            .expect("active pause control is accepted durably")
        {
            ExtensionCommandAdmission::Queued { control_id } => control_id,
            other => panic!("active command must queue, got {other:?}"),
        };
        let queued_user = runtime
            .submit_input("user input queued behind pause")
            .expect("second input may queue while the first run is active");
        release.store(true, std::sync::atomic::Ordering::Release);

        let first_operation = match first_drive
            .await
            .expect("first root drive settles after provider release")
        {
            IdleDriveOutcome::Inputs {
                operation,
                input_ids,
            } => {
                assert_eq!(input_ids, vec![first_input.id().clone()]);
                operation.id().clone()
            }
            other => panic!("expected first user operation, got {other:?}"),
        };
        let second_operation = match runtime
            .drive_next_input(IdleAuthorization::AllowExtensionContinuation)
            .await
            .expect("next drive applies pause before dispatching queued user input")
        {
            IdleDriveOutcome::Inputs {
                operation,
                input_ids,
            } => {
                assert_eq!(input_ids, vec![queued_user.id().clone()]);
                operation.id().clone()
            }
            other => panic!("control must not leap ahead as a goal continuation: {other:?}"),
        };

        let snapshot = runtime.snapshot().expect("post-control snapshot reads");
        let applied_index = snapshot
            .records()
            .iter()
            .position(|stored| {
                matches!(
                    &stored.record,
                    LaneRecord::ExtensionControlApplied(record) if record.control_id == control_id
                )
            })
            .expect("queued control applies durably");
        let second_start_index = snapshot
            .records()
            .iter()
            .position(|stored| {
                matches!(
                    &stored.record,
                    LaneRecord::OperationStarted(record) if record.id == second_operation
                )
            })
            .expect("queued user operation starts durably");
        assert!(
            applied_index < second_start_index,
            "control application precedes the next accepted user operation"
        );
        assert!(snapshot.facts().iter().any(|stored| {
            matches!(
                &stored.fact,
                SessionFact::TurnCheckpoint(checkpoint) if checkpoint.operation_id == first_operation
            )
        }), "the final queued control closes the first turn checkpoint");
        let reduction = reduce_lane(snapshot, LaneId::main()).expect("root lane reduces");
        let state = reduction
            .extension_state
            .get("control")
            .expect("pause control persists its whole private state");
        assert_eq!(state.state_version, "control-v1");
        assert_eq!(
            state.value,
            JsonValue::object([("mode", JsonValue::String("paused".into()))]),
        );

        assert!(matches!(
            runtime
                .drive_next_input(IdleAuthorization::AllowExtensionContinuation)
                .await
                .expect("paused idle policy is a safe headless decision"),
            IdleDriveOutcome::Idle,
        ));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Acquire),
            2,
            "pause suppresses the automatic continuation instead of issuing a third model request",
        );
    });
}
