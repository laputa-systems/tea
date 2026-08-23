use std::collections::VecDeque;
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};
use tea_core::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookSet, Replacement};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{SerializedJson, StopReason, ToolCallId};
use tea_luau::tool_handler::{
    CapabilityError, CapabilityFuture, CapabilityRequest, CapabilityResponse, HandlerLimits,
    LuauCapability,
};
use tea_session::{
    ArtifactPolicyId, AssistantMessageEntry, AssistantToolCall, CoreRunId, Digest, EntryId,
    CompactionEntry, EpochFinishReason, EpochFinishedRecord, EpochId, EpochStartedRecord,
    HarnessActivationRequestedRecord, HarnessRevisionChangedEntry, LaneId,
    LaneRecord, MemoryRetention, MemorySession, MemoryVisibility, OperationId, OperationKind, OperationStartedRecord, PayloadRef,
    PluginMemoryEntry,
    ProvisionedEntry, RecordId,
    ArtifactPolicy, ArtifactStore, MemoryArtifactStore, SessionEntry, SessionFact, SessionHeader, SessionId,
    SessionWriter, StableHookId, ToolReplayPolicy, ToolResultEntry, ToolStartedRecord, Usage,
};

use crate::{
    CandidateHypothesis, HarnessActor, HarnessCandidateDraft, HarnessRepository,
    HarnessLineageError, HarnessResourceLimits, HarnessSurface, HarnessTreeLimits, PluginBundleRef,
    PromptSectionDescriptor, ToolPresentationDescriptor, CoreEpochTemplate, DurableHarness,
    HarnessApplyRequest, HarnessFilePatch, HarnessIdentity, HarnessManager, HarnessSnapshotSpec,
    CapabilityBindingRef, PluginCapabilityBinding, PluginCapabilityCatalog, ContextProjectionPatch,
    ProviderLimits, derive_model_context, derive_model_context_with_patch,
    ArtifactEvent, ModelHarnessProfile, inspect_tool_schema_deviation, SessionEvent, TeaEvent,
};

#[derive(Debug)]
struct RecordingProvider {
    trace: Arc<Mutex<Vec<String>>>,
    streams: Mutex<VecDeque<ModelStream>>,
    harness: Arc<Mutex<Option<Arc<DurableHarness<MemorySession>>>>>,
    saw_durable_intent: Arc<Mutex<Vec<bool>>>,
}

impl ModelProvider for RecordingProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let durable_intent = self
            .harness
            .lock()
            .expect("harness probe mutex")
            .as_ref()
            .expect("harness is installed before the run")
            .snapshot()
            .expect("durable snapshot")
            .records()
            .iter()
            .any(|record| matches!(record.record, LaneRecord::ProviderRequestStarted(_)));
        self.saw_durable_intent
            .lock()
            .expect("provider intent probe mutex")
            .push(durable_intent);
        self.trace
            .lock()
            .expect("trace mutex")
            .push("provider".into());
        let stream = self
            .streams
            .lock()
            .expect("stream mutex")
            .pop_front()
            .expect("fixture response");
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[derive(Debug)]
struct RecordingTool {
    trace: Arc<Mutex<Vec<String>>>,
    schema: tea_protocol::JsonValue,
    harness: Arc<Mutex<Option<Arc<DurableHarness<MemorySession>>>>>,
    saw_durable_intent: Arc<Mutex<Vec<bool>>>,
}

#[derive(Debug)]
struct EchoPluginCapability {
    calls: Arc<Mutex<Vec<(String, tea_protocol::JsonValue)>>>,
}

impl LuauCapability for EchoPluginCapability {
    fn invoke(
        &self,
        request: CapabilityRequest,
        _cancellation: CancellationToken,
    ) -> CapabilityFuture {
        if request.method != "echo" {
            return Box::pin(std::future::ready(Err(CapabilityError::MethodDenied {
                capability: request.capability,
                method: request.method,
            })));
        }
        self.calls
            .lock()
            .expect("plugin capability calls")
            .push((request.tool_name, request.arguments));
        Box::pin(std::future::ready(Ok(CapabilityResponse {
            value: tea_protocol::JsonValue::String("host capability response".into()),
        })))
    }
}

impl AgentTool for RecordingTool {
    fn name(&self) -> &str {
        "record"
    }

    fn description(&self) -> &str {
        "records an effectful fixture call"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let durable_intent = self
            .harness
            .lock()
            .expect("harness probe mutex")
            .as_ref()
            .expect("harness is installed before the run")
            .snapshot()
            .expect("durable snapshot")
            .records()
            .iter()
            .any(|record| matches!(record.record, LaneRecord::ToolStarted(_)));
        self.saw_durable_intent
            .lock()
            .expect("tool intent probe mutex")
            .push(durable_intent);
        self.trace.lock().expect("trace mutex").push("tool".into());
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "recorded".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

#[test]
fn durable_harness_records_provider_and_tool_intents_before_real_effects() {
    smol::block_on(async {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let harness_probe = Arc::new(Mutex::new(None));
        let provider_intents = Arc::new(Mutex::new(Vec::new()));
        let tool_intents = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider {
            trace: Arc::clone(&trace),
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("durable-tool-call")
                                .expect("fixture call ID"),
                            name: "record".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("done".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ])),
            harness: Arc::clone(&harness_probe),
            saw_durable_intent: Arc::clone(&provider_intents),
        });
        let mut tools = tea_core::tool::ToolRegistry::default();
        tools.insert(Arc::new(RecordingTool {
            trace: Arc::clone(&trace),
            schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("fixture schema"),
            harness: Arc::clone(&harness_probe),
            saw_durable_intent: Arc::clone(&tool_intents),
        }));
        let template = CoreEpochTemplate::new(provider, tools);
        let store = Arc::new(MemoryArtifactStore::default());
        let (harness, _) = managed_harness("harness-session", template, store);
        let harness = Arc::new(harness);
        *harness_probe.lock().expect("harness probe mutex") = Some(Arc::clone(&harness));

        let operation = harness.run_prompt("make the durable call").await?;
        let snapshot = harness.snapshot()?;

        assert!(operation.is_completed());
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.body, SessionEntry::AssistantMessage(_)))
                .count(),
            2
        );
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.body, SessionEntry::ToolResult(_)))
                .count(),
            1
        );
        assert!(snapshot.records().iter().any(|record| {
            matches!(record.record, LaneRecord::ProviderRequestStarted(_))
        }));
        assert!(snapshot
            .records()
            .iter()
            .any(|record| matches!(record.record, LaneRecord::ToolStarted(_))));
        assert_eq!(
            *trace.lock().expect("trace mutex"),
            vec!["provider", "tool", "provider"]
        );
        assert_eq!(*provider_intents.lock().expect("provider intent probe mutex"), vec![true, true]);
        assert_eq!(*tool_intents.lock().expect("tool intent probe mutex"), vec![true]);

        Ok::<(), crate::HarnessError>(())
    })
    .expect("durable supervisor must settle a provider/tool operation");
}

#[test]
fn completed_epoch_retains_a_redacted_trace_with_exact_durable_provenance() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let (harness, identity) = managed_harness(
            "trace-session",
            CoreEpochTemplate::new(
                Arc::new(QueuedProvider {
                    streams: Mutex::new(VecDeque::from([ModelStream {
                        events: vec![
                            ModelStreamEvent::TextDelta("assistant secret".into()),
                            ModelStreamEvent::End(StopReason::Stop),
                        ],
                    }])),
                }),
                tea_core::tool::ToolRegistry::default(),
            ),
            store.clone(),
        );

        let operation = harness.run_prompt("user secret").await?;
        assert!(operation.is_completed());
        let snapshot = harness.snapshot()?;
        let trace = snapshot
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                SessionFact::TraceArtifact(trace) => Some(trace),
                SessionFact::HarnessCatalog(_)
                | SessionFact::ToolSchemaDeviation(_)
                | SessionFact::Custom { .. } => None,
            })
            .expect("completed durable epoch must retain a trace fact");
        assert_eq!(trace.schema_version, tea_trace::TRACE_SCHEMA_VERSION);
        assert_eq!(trace.operation_id, *operation.id());
        assert_eq!(trace.epoch_id.to_string().starts_with("epoch-"), true);
        assert_eq!(trace.harness_revision_id, identity.revision_id().clone());
        assert_eq!(trace.harness_snapshot_id, identity.snapshot_id().clone());
        assert_eq!(trace.model_harness_profile, identity.profile_id().clone());

        let text = String::from_utf8(store.get(trace.artifact_id)?)
            .expect("trace JSON Lines is UTF-8");
        assert!(text.contains(r#""type":"episode_header""#));
        assert!(text.contains(&format!(r#""operation_id":"{}""#, operation.id())));
        assert!(text.contains(&format!(r#""epoch_id":"{}""#, trace.epoch_id)));
        assert!(text.contains(&format!(r#""core_run_id":"{}""#, trace.core_run_id)));
        assert!(text.contains(r#""output":"[redacted]""#));
        assert!(!text.contains("user secret"));
        assert!(!text.contains("assistant secret"));
        assert!(text.contains(r#""type":"episode_end""#));
        assert_eq!(trace.byte_len, text.len() as u64);
        harness.verify_durable_state()?;

        Ok::<(), crate::HarnessError>(())
    })
    .expect("trace evidence must commit before the durable epoch closes");
}

#[test]
fn application_events_follow_an_atomic_snapshot_and_never_replay_on_reconnect() {
    smol::block_on(async {
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("settled".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            }])),
        });
        let store = Arc::new(MemoryArtifactStore::default());
        let (harness, _) = managed_harness(
            "event-session",
            CoreEpochTemplate::new(provider, tea_core::tool::ToolRegistry::default()),
            store,
        );

        let subscription = harness.subscribe_events()?;

        let operation = harness.run_prompt("record application events").await?;
        let mut events = Vec::new();
        loop {
            match subscription.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => panic!("live event subscription disconnected"),
            }
        }

        let session_events = events
            .iter()
            .filter(|event| matches!(event, TeaEvent::Session(_)))
            .collect::<Vec<_>>();
        assert!(events.iter().any(|event| matches!(event, TeaEvent::Agent(_))));
        assert_eq!(session_events.len(), 3);
        let TeaEvent::Session(SessionEvent::OperationAccepted { operation_id, .. }) = session_events[0] else {
            panic!("first durable application event must accept the operation");
        };
        assert_eq!(operation_id, operation.id());
        let TeaEvent::Session(SessionEvent::EpochStarted { operation_id, .. }) = session_events[1] else {
            panic!("second durable application event must start the epoch");
        };
        assert_eq!(operation_id, operation.id());
        let TeaEvent::Session(SessionEvent::OperationFinished { operation_id, outcome, .. }) = session_events[2] else {
            panic!("last durable application event must finish the operation");
        };
        assert_eq!(operation_id, operation.id());
        assert_eq!(outcome, "completed");

        let reconnect = harness.subscribe_events()?;
        assert!(reconnect.snapshot.sequence > subscription.snapshot.sequence);
        assert!(matches!(reconnect.try_recv(), Err(TryRecvError::Empty)));

        Ok::<(), crate::HarnessError>(())
    })
    .expect("application event stream must be post-commit and reconnect-safe");
}

#[test]
fn reviewed_idle_artifact_collection_emits_content_free_lifecycle_events() {
    let store = Arc::new(MemoryArtifactStore::default());
    let artifact = store
        .put(b"unreachable immutable bytes", "text/plain")
        .expect("fixture artifact writes");
    let (harness, _) = managed_harness(
        "artifact-gc-event-session",
        CoreEpochTemplate::new(
            Arc::new(QueuedProvider {
                streams: Mutex::new(VecDeque::new()),
            }),
            tea_core::tool::ToolRegistry::default(),
        ),
        store.clone(),
    );
    let subscription = harness
        .subscribe_events()
        .expect("fixture event subscription creates");
    let quota = tea_session::ArtifactQuota::default();
    let plan = harness
        .plan_artifact_gc(quota)
        .expect("unreferenced artifact receives a reviewed plan");
    assert_eq!(plan.unreferenced.len(), 1);
    let report = harness
        .apply_artifact_gc(&plan, quota)
        .expect("exact reviewed artifact plan applies");
    assert_eq!(report.removed.len(), 1);
    assert!(matches!(
        subscription.try_recv(),
        Ok(TeaEvent::Artifact(ArtifactEvent::Collected {
            artifact_id,
            byte_len,
        })) if artifact_id == artifact.artifact_id && byte_len == artifact.byte_len
    ));
}

#[derive(Debug)]
struct StopProvider {
    calls: Arc<Mutex<u32>>,
}

#[derive(Debug)]
struct QueuedProvider {
    streams: Mutex<VecDeque<ModelStream>>,
}

#[derive(Debug)]
struct PromptCapturingProvider {
    streams: Mutex<VecDeque<ModelStream>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for QueuedProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = self
            .streams
            .lock()
            .expect("provider streams mutex")
            .pop_front()
            .expect("fixture response");
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

impl ModelProvider for PromptCapturingProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests
            .lock()
            .expect("captured request mutex")
            .push(request);
        let stream = self
            .streams
            .lock()
            .expect("provider streams mutex")
            .pop_front()
            .expect("fixture response");
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

impl ModelProvider for StopProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        *self.calls.lock().expect("provider call mutex") += 1;
        Box::pin(std::future::ready(Ok(Box::new(ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("recovered".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }) as _)))
    }
}

#[derive(Debug)]
struct CountingTool {
    calls: Arc<Mutex<u32>>,
    schema: tea_protocol::JsonValue,
}

#[derive(Debug)]
struct LargeTool {
    schema: tea_protocol::JsonValue,
}

#[derive(Debug)]
struct RawEvidenceTool {
    schema: tea_protocol::JsonValue,
}

#[derive(Debug)]
struct RedactingProjectionHook;

impl HookSet for RedactingProjectionHook {
    fn before_tool_call(
        &self,
        _call: &ToolCall,
    ) -> Result<BeforeToolCall, tea_core::error::HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, tea_core::error::HookError> {
        Ok(AfterToolCall {
            content: Replacement::Replace("redacted model projection".into()),
            ..AfterToolCall::default()
        })
    }

    fn transform_context(
        &self,
        context: ContextEnvelope,
    ) -> Result<ContextEnvelope, tea_core::error::HookError> {
        Ok(context)
    }

    fn convert_to_llm(
        &self,
        context: ContextEnvelope,
    ) -> Result<String, tea_core::error::HookError> {
        Ok(format!("{:#?}", context.messages))
    }
}

impl AgentTool for LargeTool {
    fn name(&self) -> &str {
        "large"
    }

    fn description(&self) -> &str {
        "returns a fixture result that must spill to immutable storage"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: format!("{}{}{}", "α".repeat(100), "needle", "β".repeat(100)),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

impl AgentTool for RawEvidenceTool {
    fn name(&self) -> &str {
        "raw_evidence"
    }

    fn description(&self) -> &str {
        "returns exact evidence that a policy may redact only from model context"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "raw evidence: token=secret-fixture".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

impl AgentTool for CountingTool {
    fn name(&self) -> &str {
        "record"
    }

    fn description(&self) -> &str {
        "counts executions so recovery can prove it did not replay one"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        &self.schema
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        *self.calls.lock().expect("tool call mutex") += 1;
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "unexpected replay".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

#[test]
fn recovery_never_replays_an_ambiguous_non_replayable_tool() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let provider_calls = Arc::new(Mutex::new(0));
        let tool_calls = Arc::new(Mutex::new(0));
        let mut tools = tea_core::tool::ToolRegistry::default();
        tools.insert(Arc::new(CountingTool {
            calls: Arc::clone(&tool_calls),
            schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("fixture tool schema"),
        }));
        let (mut session, manager, identity) = staged_managed_session(
            "crash-recovery-session",
            CoreEpochTemplate::new(
                Arc::new(StopProvider {
                    calls: Arc::clone(&provider_calls),
                }),
                tools,
            ),
            store.clone(),
        );
        let operation_id = OperationId::new("operation-crash-fixture").expect("fixture operation ID");
        let input_entry_id = EntryId::new("entry-crash-input").expect("fixture input entry ID");
        let assistant_entry_id =
            EntryId::new("entry-crash-assistant").expect("fixture assistant entry ID");
        let result_entry_id = EntryId::new("entry-crash-result").expect("fixture result entry ID");
        let epoch_id = EpochId::new("epoch-crash-fixture").expect("fixture epoch ID");
        let input = ProvisionedEntry::user(input_entry_id.clone(), "recover this operation");
        let source_leaf_id = EntryId::new("managed-fixture-initial-revision")
            .expect("fixture initial revision entry ID");
        session.append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            operation_id.clone(),
            LaneId::main(),
            Some(source_leaf_id),
            OperationKind::Run,
            vec![input.clone()],
            identity.revision_id().clone(),
            identity.profile_id().clone(),
        )))?;
        session.append_entry(&LaneId::main(), input)?;
        session.append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(input_entry_id),
            harness_revision_id: identity.revision_id().clone(),
            harness_snapshot_id: identity.snapshot_id().clone(),
            model_harness_profile: identity.profile_id().clone(),
            core_run_id: CoreRunId::new("core-run-crash-fixture").expect("fixture core run ID"),
            epoch_resume_data: Default::default(),
        }))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry::assistant(
                assistant_entry_id.clone(),
                "",
                vec![AssistantToolCall::new(
                    "crash-tool-call",
                    "record",
                    tea_protocol::JsonValue::parse("{}").expect("fixture tool arguments"),
                )],
            ),
        )?;
        session.append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new("tool-start-crash-fixture").expect("fixture record ID"),
            operation_id.clone(),
            epoch_id,
            assistant_entry_id,
            0,
            "crash-tool-call",
            "record",
            tea_protocol::JsonValue::parse("{}").expect("fixture tool intent arguments"),
            result_entry_id.clone(),
            ToolReplayPolicy::Never,
            Digest::from_bytes("record-definition"),
            identity.revision_id().clone(),
            "tool-invocation-crash-fixture",
        )))?;

        let harness = DurableHarness::new_with_artifact_store(
            session,
            manager,
            identity,
            store,
        )?;

        let operation = harness.resume().await?;
        let snapshot = harness.snapshot()?;
        let interrupted_result = snapshot
            .entries()
            .iter()
            .find(|entry| entry.header.id == result_entry_id)
            .expect("recovery must materialize the provisioned result");
        let SessionEntry::ToolResult(interrupted_result) = &interrupted_result.body else {
            panic!("recovery result must be a tool result entry");
        };
        let tea_session::PayloadRef::Inline(full_result) = &interrupted_result.full_result else {
            panic!("small synthesized result must remain inline");
        };

        assert!(operation.is_completed());
        assert_eq!(*tool_calls.lock().expect("tool call mutex"), 0);
        assert_eq!(*provider_calls.lock().expect("provider call mutex"), 1);
        assert!(full_result
            .get("content")
            .and_then(tea_protocol::JsonValue::as_str)
            .expect("synthesized content")
            .contains("cannot prove"));
        assert_eq!(
            snapshot
                .records()
                .iter()
                .filter(|record| matches!(record.record, LaneRecord::ToolStarted(_)))
                .count(),
            1,
        );
        assert!(snapshot
            .records()
            .iter()
            .any(|record| matches!(record.record, LaneRecord::OperationFinished(_))));

        Ok::<(), crate::HarnessError>(())
    })
    .expect("non-replayable tool recovery must settle without tool replay");
}

#[test]
fn recovery_executes_only_the_unresolved_suffix_after_a_committed_tool_result_prefix() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let provider_calls = Arc::new(Mutex::new(0));
        let tool_calls = Arc::new(Mutex::new(0));
        let mut tools = tea_core::tool::ToolRegistry::default();
        tools.insert(Arc::new(CountingTool {
            calls: Arc::clone(&tool_calls),
            schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("fixture tool schema"),
        }));
        let (mut session, manager, identity) = staged_managed_session(
            "partial-recovery-session",
            CoreEpochTemplate::new(
                Arc::new(StopProvider {
                    calls: Arc::clone(&provider_calls),
                }),
                tools,
            ),
            store.clone(),
        );
        let operation_id = OperationId::new("operation-partial-fixture")
            .expect("fixture operation ID");
        let input_entry_id = EntryId::new("entry-partial-input").expect("fixture input entry ID");
        let assistant_entry_id =
            EntryId::new("entry-partial-assistant").expect("fixture assistant entry ID");
        let first_result_id =
            EntryId::new("entry-partial-first-result").expect("fixture first result ID");
        let epoch_id = EpochId::new("epoch-partial-fixture").expect("fixture epoch ID");
        let input = ProvisionedEntry::user(input_entry_id.clone(), "resume the second tool");
        let source_leaf_id = EntryId::new("managed-fixture-initial-revision")
            .expect("fixture initial revision entry ID");
        session.append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            operation_id.clone(),
            LaneId::main(),
            Some(source_leaf_id),
            OperationKind::Run,
            vec![input.clone()],
            identity.revision_id().clone(),
            identity.profile_id().clone(),
        )))?;
        session.append_entry(&LaneId::main(), input)?;
        session.append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(input_entry_id),
            harness_revision_id: identity.revision_id().clone(),
            harness_snapshot_id: identity.snapshot_id().clone(),
            model_harness_profile: identity.profile_id().clone(),
            core_run_id: CoreRunId::new("core-run-partial-fixture").expect("fixture core run ID"),
            epoch_resume_data: Default::default(),
        }))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: assistant_entry_id.clone(),
                body: SessionEntry::AssistantMessage(AssistantMessageEntry {
                    content: String::new(),
                    tool_calls: vec![
                    AssistantToolCall::new(
                        "partial-first-call",
                        "record",
                        tea_protocol::JsonValue::parse("{}").expect("fixture arguments"),
                    ),
                    AssistantToolCall::new(
                        "partial-second-call",
                        "record",
                        tea_protocol::JsonValue::parse("{}").expect("fixture arguments"),
                    ),
                    ],
                    stop_reason: Some("tool_use".into()),
                    error_message: None,
                    metadata: Default::default(),
                }),
            },
        )?;
        session.append_record(LaneRecord::ToolStarted(ToolStartedRecord::new(
            RecordId::new("tool-start-partial-first").expect("fixture record ID"),
            operation_id.clone(),
            epoch_id,
            assistant_entry_id,
            0,
            "partial-first-call",
            "record",
            tea_protocol::JsonValue::parse("{}").expect("fixture arguments"),
            first_result_id.clone(),
            ToolReplayPolicy::Never,
            Digest::from_bytes("record-definition"),
            identity.revision_id().clone(),
            "tool-invocation-partial-first",
        )))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: first_result_id,
                body: SessionEntry::ToolResult(ToolResultEntry {
                    tool_call_id: "partial-first-call".into(),
                    tool_name: "record".into(),
                    full_result: PayloadRef::Inline(tea_protocol::JsonValue::object([
                        ("content", tea_protocol::JsonValue::String("first result".into())),
                        ("details", tea_protocol::JsonValue::Null),
                        ("failure", tea_protocol::JsonValue::Null),
                    ])),
                    model_projection: tea_protocol::JsonValue::object([(
                        "content",
                        tea_protocol::JsonValue::String("first result".into()),
                    )]),
                    is_error: false,
                    terminate: false,
                    usage: Usage::default(),
                    projection_strategy_id: "fixture-inline".into(),
                    artifact_policy_id: ArtifactPolicyId::new("fixture-inline")
                        .expect("fixture artifact policy ID"),
                }),
            },
        )?;

        let harness = DurableHarness::new_with_artifact_store(
            session,
            manager,
            identity,
            store,
        )?;

        let operation = harness.resume().await?;
        let snapshot = harness.snapshot()?;

        assert!(operation.is_completed());
        assert_eq!(*tool_calls.lock().expect("tool call mutex"), 1);
        assert_eq!(*provider_calls.lock().expect("provider call mutex"), 1);
        assert_eq!(
            snapshot
                .records()
                .iter()
                .filter(|record| matches!(record.record, LaneRecord::ToolStarted(_)))
                .count(),
            2,
        );
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.body, SessionEntry::ToolResult(_)))
                .count(),
            2,
        );

        Ok::<(), crate::HarnessError>(())
    })
    .expect("partial result prefixes must not replay their committed tools");
}

#[test]
fn large_tool_results_are_retained_before_a_utf8_safe_locator_projection_is_exposed() {
    let store = MemoryArtifactStore::default();
    let mut policy = ArtifactPolicy::default();
    policy.maximum_inline_bytes = 32;
    let result = AgentToolResult {
        tool_call_id: ToolCallId::new("artifact-projection-call").expect("fixture call ID"),
        content: format!("{}{}{}", "α".repeat(3_000), "needle", "β".repeat(2_000)),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    };

    let retained = crate::retain_tool_result_with_projection(&store, &policy, &result, &result)
        .expect("large result must persist before projection");
    let tea_session::PayloadRef::Artifact {
        artifact_id,
        byte_len,
        media_type,
    } = retained.full_result
    else {
        panic!("large canonical result must spill to an immutable artifact");
    };
    let full = store.get(artifact_id).expect("complete artifact is readable");
    let projection = retained
        .model_projection
        .get("content")
        .and_then(tea_protocol::JsonValue::as_str)
        .expect("projection content");

    assert_eq!(byte_len, full.len() as u64);
    assert_eq!(media_type, "application/vnd.tea.tool-result+json");
    assert!(String::from_utf8(full).expect("canonical artifact is UTF-8").contains("needle"));
    assert!(projection.starts_with("[full tool result: tea-artifact://blake3/"));
    assert!(projection.contains("preview omits bytes"));
    assert!(projection.contains('α'));
    assert!(projection.contains('β'));
}

#[test]
fn durable_harness_persists_a_large_tool_result_before_continuing_with_its_projection() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let mut policy = ArtifactPolicy::default();
        policy.maximum_inline_bytes = 32;
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("large-tool-call").expect("fixture call ID"),
                            name: "large".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
        });
        let mut tools = tea_core::tool::ToolRegistry::default();
        tools.insert(Arc::new(LargeTool {
            schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("fixture tool schema"),
        }));
        let template = CoreEpochTemplate::new(provider, tools).artifact_policy(policy)?;
        let (harness, _) = managed_harness(
            "artifact-harness-session",
            template,
            store.clone(),
        );

        harness.run_prompt("make a large result").await?;
        let snapshot = harness.snapshot()?;
        let result = snapshot
            .entries()
            .iter()
            .find_map(|entry| match &entry.body {
                SessionEntry::ToolResult(result) if result.tool_name == "large" => Some(result),
                _ => None,
            })
            .expect("durable tool result");
        let tea_session::PayloadRef::Artifact { artifact_id, .. } = result.full_result else {
            panic!("large result must be retained through the configured artifact store");
        };
        let projection = result
            .model_projection
            .get("content")
            .and_then(tea_protocol::JsonValue::as_str)
            .expect("model projection content");

        assert!(store
            .get(artifact_id)
            .expect("complete retained result")
            .windows(b"needle".len())
            .any(|window| window == b"needle"));
        assert!(projection.starts_with("[full tool result: tea-artifact://blake3/"));
        Ok::<(), crate::HarnessError>(())
    })
    .expect("the durable harness must retain the complete result before model continuation");
}

#[test]
fn durable_after_tool_projection_keeps_raw_evidence_outside_model_context() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("raw-evidence-call").expect("fixture call ID"),
                            name: "raw_evidence".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
        });
        let mut tools = tea_core::tool::ToolRegistry::default();
        tools.insert(Arc::new(RawEvidenceTool {
            schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("fixture schema"),
        }));
        let template = CoreEpochTemplate::new(provider, tools).hooks(Arc::new(RedactingProjectionHook));
        let (harness, _) = managed_harness(
            "raw-projection-session",
            template,
            store,
        );

        harness.run_prompt("redact this tool result for the model").await?;
        let snapshot = harness.snapshot()?;
        let result = snapshot
            .entries()
            .iter()
            .find_map(|entry| match &entry.body {
                SessionEntry::ToolResult(result) if result.tool_name == "raw_evidence" => Some(result),
                _ => None,
            })
            .expect("durable tool result");
        let tea_session::PayloadRef::Inline(full) = &result.full_result else {
            panic!("small raw evidence must remain inline for this fixture");
        };

        assert_eq!(
            full.get("content").and_then(tea_protocol::JsonValue::as_str),
            Some("raw evidence: token=secret-fixture"),
        );
        assert_eq!(
            result
                .model_projection
                .get("content")
                .and_then(tea_protocol::JsonValue::as_str),
            Some("redacted model projection"),
        );
        Ok::<(), crate::HarnessError>(())
    })
    .expect("post-tool policy may change only the model projection, never raw durable evidence");
}

#[test]
fn stable_artifact_tools_return_bounded_pages_without_creating_a_second_locator() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let descriptor = store
            .put(b"first line\nneedle\nlast line", "text/plain")
            .expect("fixture artifact persists");
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("artifact-tools-session").expect("fixture session ID"),
            "fixture-workspace",
            Default::default(),
        ))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry::user(
                EntryId::new("artifact-tools-history").expect("fixture history entry ID"),
                "needle remains searchable in durable history",
            ),
        )?;
        let mut policy = ArtifactPolicy::default();
        policy.maximum_inline_bytes = 1;
        policy.maximum_page_bytes = 64;
        let tools = crate::artifact_tools::stable_artifact_tools(
            Arc::new(Mutex::new(session)),
            store,
            policy,
        )?;
        let call = ToolCall {
            id: ToolCallId::new("artifact-read-call").expect("fixture call ID"),
            name: "tea_artifact_read".into(),
            arguments: SerializedJson::new(format!(
                r#"{{"artifact_id":"{}","maximum_bytes":64}}"#,
                descriptor.artifact_id.to_hex()
            )),
        };
        let result = tools
            .get("tea_artifact_read")
            .expect("stable artifact reader")
            .execute(call, ToolContext {
                cancellation: CancellationToken::new(),
                metadata: None,
            }, ToolUpdateSink::disabled())
            .await
            .expect("artifact page reads");
        assert!(result.content.contains("needle"));
        assert!(!result.content.contains("tea-artifact://"));

        let history_call = ToolCall {
            id: ToolCallId::new("history-search-call").expect("fixture call ID"),
            name: "tea_history_search".into(),
            arguments: SerializedJson::new(r#"{"text":"needle","maximum_results":1}"#),
        };
        let history = tools
            .get("tea_history_search")
            .expect("stable history reader")
            .execute(history_call, ToolContext {
                cancellation: CancellationToken::new(),
                metadata: None,
            }, ToolUpdateSink::disabled())
            .await
            .expect("history search reads");
        assert!(history.content.contains("artifact-tools-history"));

        Ok::<(), crate::HarnessError>(())
    })
    .expect("stable recovery tools must expose direct bounded durable data");
}

#[test]
fn immutable_harness_lineage_stages_candidates_without_mutating_the_active_revision() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store);
    let tree = repository
        .stage_tree(
            [
                (
                    tea_session::NormalizedPath::new("plugins/session.verify/manifest.json")
                        .expect("fixture manifest path"),
                    br#"{"schema_version":1,"abi_version":1,"id":"session.verify","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":[]}"#.to_vec(),
                    "application/json".into(),
                ),
                (
                    tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                        .expect("fixture source path"),
                    b"return { prompt_sections = {} }".to_vec(),
                    "text/plain".into(),
                ),
            ],
            &HarnessTreeLimits::default(),
        )
        .expect("source tree stages immutably");
    let plugin = PluginBundleRef {
        plugin_id: "session.verify".into(),
        tree_id: tree.id.clone(),
        requested_capabilities: Default::default(),
    };
    let parent = repository
        .stage_snapshot(lineage_snapshot_spec(plugin.clone(), "trusted prefix"))
        .expect("parent snapshot");
    let parent_revision = repository
        .seed_revision(parent.id.clone(), HarnessActor::Host, 10)
        .expect("initial pinned revision");
    let hook_only = repository
        .stage_snapshot(HarnessSnapshotSpec {
            hook_bundle_digest: Digest::from_bytes("different hook bundle"),
            ..lineage_snapshot_spec(plugin, "trusted prefix")
        })
        .expect("hook-only snapshot");
    assert_eq!(
        parent.fingerprints.provider_surface_digest,
        hook_only.fingerprints.provider_surface_digest,
        "hook-only edits must not claim a provider surface change",
    );
    assert_ne!(parent.id, hook_only.id);

    let candidate = repository
        .stage_candidate(HarnessCandidateDraft {
            parent_revision_id: parent_revision.revision_id.clone(),
            proposed_snapshot_id: hook_only.id.clone(),
            actor: HarnessActor::Model,
            operation_id: None,
            tool_invocation_id: Some("tool-invocation-fixture".into()),
            hypothesis: CandidateHypothesis {
                targeted_evidence: "a policy hook needs a narrower guard".into(),
                expected_effect: "the hook blocks the unsafe call".into(),
                regression_risk: "unrelated calls could be blocked".into(),
            },
            changed_paths: vec![
                tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                    .expect("fixture changed path"),
            ],
            registry_operations: Vec::new(),
            changed_surfaces: [HarnessSurface::Hooks].into_iter().collect(),
            targeted_failures: vec!["failure-signature-fixture".into()],
            evidence: vec!["evidence-fixture".into()],
            expected_effects: vec!["block unsafe call".into()],
            regression_risks: vec!["overblock".into()],
            capability_ceiling: Default::default(),
        })
        .expect("candidate stages without activation");
    assert!(candidate.validation.accepted);
    assert_eq!(
        repository
            .revision(&parent_revision.revision_id)
            .expect("parent remains in lineage")
            .snapshot_id,
        parent.id,
    );

    let activated = repository
        .activate_candidate(&candidate.candidate_id, HarnessActor::Host, 20)
        .expect("only an accepted candidate can activate");
    assert_eq!(activated.snapshot_id, hook_only.id);
    assert_eq!(activated.parent_revision_ids, vec![parent_revision.revision_id.clone()]);
    let rollback = repository
        .rollback(
            &activated.revision_id,
            &parent_revision.revision_id,
            HarnessActor::Operator,
            30,
        )
        .expect("rollback is a new immutable revision");
    assert_eq!(rollback.snapshot_id, parent.id);
    assert_ne!(rollback.revision_id, parent_revision.revision_id);
}

#[test]
fn snapshot_validation_rejects_an_undeclared_v1_plugin_module_before_staging() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store);
    let tree = repository
        .stage_tree(
            [
                (
                    tea_session::NormalizedPath::new("plugins/session.verify/manifest.json")
                        .expect("fixture manifest path"),
                    br#"{"schema_version":1,"abi_version":1,"id":"session.verify","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":[]}"#.to_vec(),
                    "application/json".into(),
                ),
                (
                    tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                        .expect("fixture source path"),
                    b"local hidden = require('./hidden.luau') return { prompt_sections = {} }"
                        .to_vec(),
                    "text/plain".into(),
                ),
                (
                    tea_session::NormalizedPath::new("plugins/session.verify/hidden.luau")
                        .expect("fixture hidden source path"),
                    b"return 'not declared by the manifest'".to_vec(),
                    "text/plain".into(),
                ),
            ],
            &HarnessTreeLimits::default(),
        )
        .expect("source tree itself is immutable and well-formed");
    let result = repository.stage_snapshot(lineage_snapshot_spec(
        PluginBundleRef {
            plugin_id: "session.verify".into(),
            tree_id: tree.id,
            requested_capabilities: Default::default(),
        },
        "trusted prefix",
    ));

    assert!(matches!(
        result,
        Err(HarnessLineageError::Invalid { message }) if message.contains("undeclared module")
    ));
}

#[test]
fn capability_expansion_candidate_is_retained_for_manual_review_but_never_activates() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store);
    let tree = repository
        .stage_tree(
            [
                (
                    tea_session::NormalizedPath::new("plugins/session.expansion/manifest.json")
                        .expect("fixture manifest path"),
                    br#"{"schema_version":1,"abi_version":1,"id":"session.expansion","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":["fixture.new_authority"]}"#.to_vec(),
                    "application/json".into(),
                ),
                (
                    tea_session::NormalizedPath::new("plugins/session.expansion/main.luau")
                        .expect("fixture source path"),
                    b"return { prompt_sections = {} }".to_vec(),
                    "text/plain".into(),
                ),
            ],
            &HarnessTreeLimits::default(),
        )
        .expect("closed expansion plugin tree stages");
        let expansion_plugin = PluginBundleRef {
            plugin_id: "session.expansion".into(),
            tree_id: tree.id,
            requested_capabilities: ["fixture.new_authority".into()].into_iter().collect(),
        };
        let mut base_spec = lineage_snapshot_spec(expansion_plugin.clone(), "trusted prefix");
        base_spec.ordered_session_plugins.clear();
        base_spec.plugin_prompt_sections.clear();
        base_spec.plugin_tool_presentations.clear();
        let parent = repository
            .stage_snapshot(base_spec)
            .expect("capability-neutral parent snapshot stages");
        let parent_revision = repository
            .seed_revision(parent.id.clone(), HarnessActor::Host, 1)
            .expect("capability-neutral parent can activate");
        let requested = repository
            .stage_snapshot(lineage_snapshot_spec(expansion_plugin, "trusted prefix"))
            .expect("unbound candidate source remains inspectable in immutable lineage");
        let candidate = repository
            .stage_candidate(HarnessCandidateDraft {
                parent_revision_id: parent_revision.revision_id,
                proposed_snapshot_id: requested.id,
                actor: HarnessActor::Model,
                operation_id: None,
                tool_invocation_id: Some("capability-expansion-fixture".into()),
                hypothesis: CandidateHypothesis {
                    targeted_evidence: "a model requested a capability beyond the frozen session ceiling".into(),
                    expected_effect: "manual review can inspect the exact closed source".into(),
                    regression_risk: "automatic activation could grant new authority".into(),
                },
                changed_paths: vec![
                    tea_session::NormalizedPath::new("plugins/session.expansion/main.luau")
                        .expect("fixture path"),
                ],
                registry_operations: vec![crate::RegistryOperation::Add {
                    plugin_id: "session.expansion".into(),
                }],
                changed_surfaces: [HarnessSurface::CapabilityBindings]
                    .into_iter()
                    .collect(),
                targeted_failures: vec!["authority-expansion".into()],
                evidence: Vec::new(),
                expected_effects: vec!["manual-review".into()],
                regression_risks: vec!["ambient-authority".into()],
                capability_ceiling: Default::default(),
            })
            .expect("rejected candidate itself remains durable lineage data");
        assert!(!candidate.validation.accepted);
        assert!(candidate
            .validation
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("outside its frozen ceiling")));
        assert!(candidate
            .validation
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("without an immutable host binding")));
        assert!(matches!(
            repository.activate_candidate(&candidate.candidate_id, HarnessActor::Host, 2),
            Err(HarnessLineageError::InvalidActivation { .. })
        ));
}

#[test]
fn source_derived_v1_prompt_sections_are_snapshotted_into_the_provider_surface() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store);
    let first_tree = repository
        .stage_tree(
            v1_prompt_plugin_sources("Run the narrowest relevant validator."),
            &HarnessTreeLimits::default(),
        )
        .expect("first prompt plugin stages");
    let first = repository
        .stage_snapshot(lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: first_tree.id,
                requested_capabilities: Default::default(),
            },
            "trusted prefix",
        ))
        .expect("first snapshot stages");
    assert_eq!(
        first.spec.plugin_prompt_sections,
        vec![PromptSectionDescriptor {
            id: "session.verify.verification".into(),
            content: "Run the narrowest relevant validator.".into(),
        }],
    );

    let second_tree = repository
        .stage_tree(
            v1_prompt_plugin_sources("Inspect the targeted failure before finalizing."),
            &HarnessTreeLimits::default(),
        )
        .expect("second prompt plugin stages");
    let second = repository
        .stage_snapshot(lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: second_tree.id,
                requested_capabilities: Default::default(),
            },
            "trusted prefix",
        ))
        .expect("second snapshot stages");

    assert_ne!(first.id, second.id);
    assert_ne!(
        first.fingerprints.provider_surface_digest,
        second.fingerprints.provider_surface_digest,
        "a source-owned prompt change must invalidate the exact provider surface",
    );
}

#[test]
fn manager_apply_stages_a_closed_source_patch_as_an_inactive_candidate() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store);
    let tree = repository
        .stage_tree(
            v1_prompt_plugin_sources("Use the original check."),
            &HarnessTreeLimits::default(),
        )
        .expect("initial prompt plugin stages");
    let parent = repository
        .stage_snapshot(lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: tree.id,
                requested_capabilities: Default::default(),
            },
            "trusted prefix",
        ))
        .expect("parent snapshot stages");
    let parent_revision = repository
        .seed_revision(parent.id.clone(), HarnessActor::Host, 1)
        .expect("parent revision stages");
    let manager = HarnessManager::new(
        repository,
        CoreEpochTemplate::new(
            Arc::new(StopProvider {
                calls: Arc::new(Mutex::new(0)),
            }),
            tea_core::tool::ToolRegistry::default(),
        ),
        Default::default(),
    );

    let candidate = manager
        .apply(HarnessApplyRequest {
            base_revision_id: parent_revision.revision_id.clone(),
            hypothesis: CandidateHypothesis {
                targeted_evidence: "the original check missed a regression".into(),
                expected_effect: "the next prompt names the narrower check".into(),
                regression_risk: "the policy could over-constrain routine work".into(),
            },
            files: vec![HarnessFilePatch::Upsert {
                path: tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                    .expect("fixture source path"),
                content: "return { prompt_sections = { { id = 'verification', content = 'Run the targeted check.' } } }".into(),
            }],
            registry_operations: Vec::new(),
            operation_id: None,
            tool_invocation_id: "apply-prompt-fixture".into(),
        })
        .expect("closed source patch stages as a candidate");
    let proposed = manager
        .snapshot(&candidate.draft.proposed_snapshot_id)
        .expect("candidate snapshot remains addressable");

    assert!(candidate.validation.accepted);
    assert_ne!(candidate.draft.proposed_snapshot_id, parent.id);
    assert!(candidate
        .draft
        .changed_surfaces
        .contains(&HarnessSurface::SystemPrompt));
    assert_eq!(
        proposed.spec.plugin_prompt_sections[0].content,
        "Run the targeted check.",
    );
    assert_eq!(
        manager
            .revision(&parent_revision.revision_id)
            .expect("parent revision remains immutable")
            .snapshot_id,
        parent.id,
    );
}

#[test]
fn harness_catalog_rebuilds_exact_immutable_lineage_from_artifacts() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store.clone());
    let tree = repository
        .stage_tree(
            v1_prompt_plugin_sources("Run the original narrow check."),
            &HarnessTreeLimits::default(),
        )
        .expect("initial closed plugin tree stages");
    let parent = repository
        .stage_snapshot(lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: tree.id,
                requested_capabilities: Default::default(),
            },
            "trusted prefix",
        ))
        .expect("parent snapshot stages");
    let parent_revision = repository
        .seed_revision(parent.id.clone(), HarnessActor::Host, 1)
        .expect("initial revision stages");
    let candidate_snapshot = repository
        .stage_snapshot(HarnessSnapshotSpec {
            hook_bundle_digest: Digest::from_bytes("candidate hook bundle"),
            ..parent.spec.clone()
        })
        .expect("candidate snapshot stages");
    let candidate = repository
        .stage_candidate(HarnessCandidateDraft {
            parent_revision_id: parent_revision.revision_id.clone(),
            proposed_snapshot_id: candidate_snapshot.id,
            actor: HarnessActor::Model,
            operation_id: Some(
                OperationId::new("catalog-operation").expect("fixture operation ID"),
            ),
            tool_invocation_id: Some("catalog-tool-call".into()),
            hypothesis: CandidateHypothesis {
                targeted_evidence: "a durable catalog must survive restart".into(),
                expected_effect: "the candidate remains addressable after rebuild".into(),
                regression_risk: "catalog data could diverge from source blobs".into(),
            },
            changed_paths: vec![
                tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                    .expect("fixture source path"),
            ],
            registry_operations: Vec::new(),
            changed_surfaces: [HarnessSurface::Hooks].into_iter().collect(),
            targeted_failures: vec!["catalog-restart".into()],
            evidence: vec!["artifact:fixture".into()],
            expected_effects: vec!["rebuild lineage".into()],
            regression_risks: vec!["corrupt catalog".into()],
            capability_ceiling: Default::default(),
        })
        .expect("candidate stages");

    let catalog = repository
        .catalog_json()
        .expect("catalog encodes deterministically");
    let restored = HarnessRepository::from_catalog_json(store, &catalog)
        .expect("catalog restores only after all immutable objects validate");

    assert_eq!(
        restored
            .revision(&parent_revision.revision_id)
            .expect("initial revision restores"),
        &parent_revision,
    );
    assert_eq!(
        restored
            .candidate(&candidate.candidate_id)
            .expect("candidate restores"),
        &candidate,
    );
    assert_eq!(
        restored
            .snapshot(&parent.id)
            .expect("snapshot restores")
            .fingerprints,
        parent.fingerprints,
    );
}

#[test]
fn managed_harness_rebuilds_its_catalog_before_resolving_a_reopened_revision() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store.clone());
    let tree = repository
        .stage_tree(
            v1_prompt_plugin_sources("Use the pinned source after restart."),
            &HarnessTreeLimits::default(),
        )
        .expect("initial plugin source stages");
    let parent = repository
        .stage_snapshot(lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: tree.id,
                requested_capabilities: Default::default(),
            },
            "trusted prefix",
        ))
        .expect("parent snapshot stages");
    let parent_revision = repository
        .seed_revision(parent.id.clone(), HarnessActor::Host, 1)
        .expect("initial revision stages");
    let changed = repository
        .stage_snapshot(HarnessSnapshotSpec {
            hook_bundle_digest: Digest::from_bytes("reopened hook bundle"),
            ..parent.spec.clone()
        })
        .expect("candidate snapshot stages");
    let candidate = repository
        .stage_candidate(HarnessCandidateDraft {
            parent_revision_id: parent_revision.revision_id.clone(),
            proposed_snapshot_id: changed.id,
            actor: HarnessActor::Model,
            operation_id: None,
            tool_invocation_id: Some("reopened-catalog-call".into()),
            hypothesis: CandidateHypothesis {
                targeted_evidence: "restart must retain rejected lineage too".into(),
                expected_effect: "the immutable candidate is listed after reopen".into(),
                regression_risk: "source recovery could silently use a worktree".into(),
            },
            changed_paths: vec![
                tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                    .expect("fixture source path"),
            ],
            registry_operations: Vec::new(),
            changed_surfaces: [HarnessSurface::Hooks].into_iter().collect(),
            targeted_failures: vec!["restart".into()],
            evidence: Vec::new(),
            expected_effects: vec!["rebuild catalog".into()],
            regression_risks: vec!["bad source pointer".into()],
            capability_ceiling: Default::default(),
        })
        .expect("candidate stages before simulated process exit");
    let base_template = CoreEpochTemplate::new(
        Arc::new(StopProvider {
            calls: Arc::new(Mutex::new(0)),
        }),
        tea_core::tool::ToolRegistry::default(),
    );
    let original_manager = HarnessManager::new(repository, base_template.clone(), Default::default());
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("catalog-reopen-session").expect("fixture session ID"),
        "fixture-workspace",
        managed_session_metadata(crate::SelfExtensionMode::Off),
    ))
    .expect("session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("catalog-reopen-initial-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: parent_revision.revision_id.clone(),
                    snapshot_id: parent.id.clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("initial semantic revision persists");
    original_manager
        .persist_catalog(&mut session, store.as_ref())
        .expect("catalog commits before process exit");

    let reopened_manager = Arc::new(HarnessManager::new(
        HarnessRepository::new(store.clone()),
        base_template,
        Default::default(),
    ));
    let _harness = DurableHarness::reopen_with_artifact_store(
        session,
        Arc::clone(&reopened_manager),
        store,
    )
    .expect("reopened managed harness rebuilds catalog before resolving the active revision");

    assert_eq!(
        reopened_manager
            .candidate(&candidate.candidate_id)
            .expect("candidate survives catalog rebuild"),
        candidate,
    );
    assert_eq!(
        reopened_manager
            .revision(&parent_revision.revision_id)
            .expect("active revision survives catalog rebuild")
            .snapshot_id,
        parent.id,
    );
}

#[test]
fn managed_harness_rolls_the_same_operation_into_a_committed_candidate_revision() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let mut repository = HarnessRepository::new(store.clone());
        let tree = repository
            .stage_tree(
                [
                    (
                        tea_session::NormalizedPath::new("plugins/session.verify/manifest.json")
                            .expect("fixture manifest path"),
                        br#"{"schema_version":1,"abi_version":1,"id":"session.verify","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":[]}"#.to_vec(),
                        "application/json".into(),
                    ),
                    (
                        tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                            .expect("fixture source path"),
                        b"return { prompt_sections = {} }".to_vec(),
                        "text/plain".into(),
                    ),
                ],
                &HarnessTreeLimits::default(),
            )
            .expect("initial closed plugin tree stages");
        let plugin = PluginBundleRef {
            plugin_id: "session.verify".into(),
            tree_id: tree.id,
            requested_capabilities: Default::default(),
        };
        let parent = repository
            .stage_snapshot(lineage_snapshot_spec(plugin.clone(), "trusted prefix"))
            .expect("parent snapshot stages");
        let parent_revision = repository
            .seed_revision(parent.id.clone(), HarnessActor::Host, 10)
            .expect("initial revision stages");
        let proposed = repository
            .stage_snapshot(lineage_snapshot_spec(plugin, "trusted prefix after candidate"))
            .expect("candidate snapshot stages");
        let manager = Arc::new(HarnessManager::new(
            repository,
            CoreEpochTemplate::new(
                Arc::new(StopProvider {
                    calls: Arc::new(Mutex::new(0)),
                }),
                tea_core::tool::ToolRegistry::default(),
            ),
            Default::default(),
        ));
        let candidate = manager.stage_candidate(HarnessCandidateDraft {
            parent_revision_id: parent_revision.revision_id.clone(),
            proposed_snapshot_id: proposed.id.clone(),
            actor: HarnessActor::Model,
            operation_id: None,
            tool_invocation_id: Some("managed-rollover-tool".into()),
            hypothesis: CandidateHypothesis {
                targeted_evidence: "a stable instruction must be clarified".into(),
                expected_effect: "the next epoch uses the clarified prefix".into(),
                regression_risk: "the prompt could become noisier".into(),
            },
            changed_paths: Vec::new(),
            registry_operations: Vec::new(),
            changed_surfaces: [HarnessSurface::SystemPrompt].into_iter().collect(),
            targeted_failures: vec!["prompt-fixture".into()],
            evidence: vec!["trace-fixture".into()],
            expected_effects: vec!["clarify prompt".into()],
            regression_risks: vec!["noise".into()],
            capability_ceiling: Default::default(),
        })?;
        assert!(candidate.validation.accepted);

        let operation_id = OperationId::new("managed-rollover-operation")
            .expect("fixture operation ID");
        let user_entry_id = EntryId::new("managed-rollover-user").expect("fixture user ID");
        let old_epoch_id = EpochId::new("managed-rollover-old-epoch").expect("fixture epoch ID");
        let activation_entry_id = EntryId::new("managed-rollover-activation-entry")
            .expect("fixture activation entry ID");
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("managed-rollover-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Off),
        ))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("managed-rollover-initial-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: parent_revision.revision_id.clone(),
                    snapshot_id: parent.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let user = ProvisionedEntry::user(user_entry_id.clone(), "continue through activation");
        session.append_record(LaneRecord::OperationStarted(OperationStartedRecord::new(
            operation_id.clone(),
            LaneId::main(),
            session.snapshot()?.entries().last().map(|entry| entry.header.id.clone()),
            OperationKind::Run,
            vec![user.clone()],
            parent_revision.revision_id.clone(),
            parent.spec.model_harness_profile.clone(),
        )))?;
        session.append_entry(&LaneId::main(), user)?;
        session.append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: old_epoch_id.clone(),
            operation_id: operation_id.clone(),
            epoch_index: 0,
            source_leaf_id: Some(user_entry_id),
            harness_revision_id: parent_revision.revision_id.clone(),
            harness_snapshot_id: parent.id.clone(),
            model_harness_profile: parent.spec.model_harness_profile.clone(),
            core_run_id: CoreRunId::new("managed-rollover-old-core-run")
                .expect("fixture core run ID"),
            epoch_resume_data: Default::default(),
        }))?;
        session.append_record(LaneRecord::HarnessActivationRequested(
            HarnessActivationRequestedRecord {
                operation_id: operation_id.clone(),
                candidate_id: candidate.candidate_id.clone(),
                parent_revision_id: parent_revision.revision_id.clone(),
                proposed_snapshot_id: proposed.id.clone(),
                revision_entry_id: activation_entry_id.clone(),
            },
        ))?;
        session.append_record(LaneRecord::EpochFinished(EpochFinishedRecord {
            epoch_id: old_epoch_id,
            operation_id: operation_id.clone(),
            reason: EpochFinishReason::ActivationPending,
        }))?;

        let harness = DurableHarness::new_with_artifact_store(
            session,
            Arc::clone(&manager),
            HarnessIdentity::new(
                parent_revision.revision_id.clone(),
                parent.id.clone(),
                parent.spec.model_harness_profile.clone(),
            ),
            store,
        )?;
        let operation = harness.resume().await?;
        let snapshot = harness.snapshot()?;

        assert!(operation.is_completed());
        let activation = snapshot
            .entries()
            .iter()
            .find(|entry| entry.header.id == activation_entry_id)
            .expect("activation must materialize the provisioned semantic entry");
        let SessionEntry::HarnessRevisionChanged(activation) = &activation.body else {
            panic!("activation must be a semantic harness-revision transition");
        };
        assert_eq!(activation.snapshot_id, proposed.id);
        assert_ne!(activation.revision_id, parent_revision.revision_id);
        assert_eq!(
            snapshot
                .records()
                .iter()
                .filter(|record| matches!(record.record, LaneRecord::EpochStarted(_)))
                .count(),
            2,
        );
        assert_eq!(
            snapshot
                .records()
                .iter()
                .filter(|record| matches!(record.record, LaneRecord::OperationFinished(_)))
                .count(),
            1,
        );
        Ok::<(), crate::HarnessError>(())
    })
    .expect("a pending candidate must roll the same durable operation into a new epoch");
}

#[test]
fn managed_harness_apply_tool_commits_a_candidate_then_continues_under_its_new_snapshot() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let mut repository = HarnessRepository::new(store.clone());
        let tree = repository
            .stage_tree(
                v1_prompt_plugin_sources("Use the original check."),
                &HarnessTreeLimits::default(),
            )
            .expect("initial closed plugin tree stages");
        let mut parent_spec = lineage_snapshot_spec(
                PluginBundleRef {
                    plugin_id: "session.verify".into(),
                    tree_id: tree.id,
                    requested_capabilities: Default::default(),
                },
                "trusted prefix",
            );
        parent_spec.self_extension_addendum = Some(crate::SELF_EXTENSION_V1_CONCISE.into());
        let parent = repository
            .stage_snapshot(parent_spec)
            .expect("parent snapshot stages");
        let parent_revision = repository
            .seed_revision(parent.id.clone(), HarnessActor::Host, 1)
            .expect("initial revision stages");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(PromptCapturingProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("managed-apply-call")
                                .expect("fixture control-tool call ID"),
                            name: "tea_harness".into(),
                            arguments: SerializedJson::new(format!(
                                r#"{{"operation":"apply","base_revision":"{}","hypothesis":{{"failure_signature":"the original check missed a regression","expected_effect":"the prompt names the targeted check","regression_risk":"the policy could over-constrain routine work"}},"files":[{{"operation":"upsert","path":"plugins/session.verify/main.luau","content":"return {{ prompt_sections = {{ {{ id = 'verification', content = 'Run the targeted check.' }} }} }}"}}],"registry_operations":[]}}"#,
                                parent_revision.revision_id,
                            )),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("continued under the activated snapshot".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ])),
            requests: Arc::clone(&requests),
        });
        let manager = Arc::new(HarnessManager::new(
            repository,
            CoreEpochTemplate::new(provider, tea_core::tool::ToolRegistry::default()),
            Default::default(),
        ).self_extension_mode(crate::SelfExtensionMode::Adaptive));
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("managed-apply-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Adaptive),
        ))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("managed-apply-initial-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: parent_revision.revision_id.clone(),
                    snapshot_id: parent.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let harness = DurableHarness::new_with_artifact_store(
            session,
            Arc::clone(&manager),
            HarnessIdentity::new(
                parent_revision.revision_id.clone(),
                parent.id.clone(),
                parent.spec.model_harness_profile.clone(),
            ),
            store,
        )?;
        let application_events = harness.subscribe_events()?;

        let operation = harness.run_prompt("repair the reusable verification behavior").await?;
        let snapshot = harness.snapshot()?;
        let captured = requests.lock().expect("captured request mutex").clone();
        let mut emitted = Vec::new();
        loop {
            match application_events.try_recv() {
                Ok(event) => emitted.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => panic!("fixture event subscription disconnected"),
            }
        }

        assert!(operation.is_completed());
        assert_eq!(captured.len(), 2);
        assert!(captured[0].system_prompt.contains("Use the original check."));
        let base_index = captured[0]
            .system_prompt
            .find("trusted prefix")
            .expect("trusted base prefix");
        let addendum_index = captured[0]
            .system_prompt
            .find(crate::SELF_EXTENSION_V1_CONCISE)
            .expect("stable self-extension addendum");
        let plugin_index = captured[0]
            .system_prompt
            .find("Use the original check.")
            .expect("dynamic plugin prompt section");
        assert!(base_index < addendum_index && addendum_index < plugin_index);
        assert!(captured[0].tools.iter().any(|tool| tool.name == "tea_harness"));
        assert!(captured[1].system_prompt.contains("Run the targeted check."));
        assert_eq!(
            snapshot
                .records()
                .iter()
                .filter(|record| matches!(record.record, LaneRecord::EpochStarted(_)))
                .count(),
            2,
        );
        assert_eq!(
            snapshot
                .records()
                .iter()
                .filter(|record| matches!(record.record, LaneRecord::HarnessActivationRequested(_)))
                .count(),
            1,
        );
        let control_result = snapshot
            .entries()
            .iter()
            .find_map(|entry| match &entry.body {
                SessionEntry::ToolResult(result) if result.tool_name == "tea_harness" => Some(result),
                _ => None,
            })
            .expect("control tool must commit its durable result");
        assert!(control_result.terminate);
        assert!(control_result
            .model_projection
            .to_json_string()
            .expect("canonical tool projection")
            .contains("activation_scheduled"));
        assert!(snapshot.entries().iter().any(|entry| {
            matches!(
                &entry.body,
                SessionEntry::HarnessRevisionChanged(changed)
                    if changed.snapshot_id != parent.id
            )
        }));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TeaEvent::Harness(crate::HarnessEvent::CandidateStaged { .. })
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TeaEvent::Harness(crate::HarnessEvent::SnapshotActivated {
                provider_surface_changed: true,
                ..
            })
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TeaEvent::Harness(crate::HarnessEvent::RolloverStarted { .. })
        )));
        assert!(emitted.iter().any(|event| matches!(
            event,
            TeaEvent::Harness(crate::HarnessEvent::RolloverCompleted { .. })
        )));

        Ok::<(), crate::HarnessError>(())
    })
    .expect("the stable control tool must roll the same operation into its committed snapshot");
}

#[test]
fn noop_harness_candidate_is_retained_and_rejected_without_an_activation_obligation() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let mut repository = HarnessRepository::new(store.clone());
        let tree = repository
            .stage_tree(
                v1_prompt_plugin_sources("Use the original check."),
                &HarnessTreeLimits::default(),
            )
            .expect("initial closed plugin tree stages");
        let mut parent_spec = lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: tree.id,
                requested_capabilities: Default::default(),
            },
            "trusted prefix",
        );
        parent_spec.self_extension_addendum = Some(crate::SELF_EXTENSION_V1_CONCISE.into());
        parent_spec.hook_bundle_digest = crate::manager::session_plugin_hook_digest(&parent_spec);
        let parent = repository
            .stage_snapshot(parent_spec)
            .expect("parent snapshot stages");
        let parent_revision = repository
            .seed_revision(parent.id.clone(), HarnessActor::Host, 1)
            .expect("initial revision stages");
        let no_op_source = String::from_utf8(
            v1_prompt_plugin_sources("Use the original check.")[1].1.clone(),
        )
        .expect("fixture source is UTF-8");
        let apply_arguments = tea_protocol::JsonValue::object([
            ("operation", tea_protocol::JsonValue::String("apply".into())),
            (
                "base_revision",
                tea_protocol::JsonValue::String(parent_revision.revision_id.to_string()),
            ),
            (
                "hypothesis",
                tea_protocol::JsonValue::object([
                    (
                        "failure_signature",
                        tea_protocol::JsonValue::String(
                            "the original check needs confirmation".into(),
                        ),
                    ),
                    (
                        "expected_effect",
                        tea_protocol::JsonValue::String(
                            "retain the original exact check".into(),
                        ),
                    ),
                    (
                        "regression_risk",
                        tea_protocol::JsonValue::String(
                            "the proposal may be redundant".into(),
                        ),
                    ),
                ]),
            ),
            (
                "files",
                tea_protocol::JsonValue::Array(vec![tea_protocol::JsonValue::object([
                    (
                        "operation",
                        tea_protocol::JsonValue::String("upsert".into()),
                    ),
                    (
                        "path",
                        tea_protocol::JsonValue::String(
                            "plugins/session.verify/main.luau".into(),
                        ),
                    ),
                    ("content", tea_protocol::JsonValue::String(no_op_source)),
                ])]),
            ),
            ("registry_operations", tea_protocol::JsonValue::Array(Vec::new())),
        ])
        .to_json_string()
        .expect("canonical control request JSON");
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("managed-noop-call")
                                .expect("fixture control-tool call ID"),
                            name: "tea_harness".into(),
                            arguments: SerializedJson::new(apply_arguments),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
        });
        let manager = Arc::new(
            HarnessManager::new(
                repository,
                CoreEpochTemplate::new(provider, tea_core::tool::ToolRegistry::default()),
                Default::default(),
            )
            .self_extension_mode(crate::SelfExtensionMode::Adaptive),
        );
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("managed-noop-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Adaptive),
        ))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("managed-noop-initial-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: parent_revision.revision_id.clone(),
                    snapshot_id: parent.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let harness = DurableHarness::new_with_artifact_store(
            session,
            Arc::clone(&manager),
            HarnessIdentity::new(
                parent_revision.revision_id.clone(),
                parent.id.clone(),
                parent.spec.model_harness_profile.clone(),
            ),
            store,
        )?;
        let events = harness.subscribe_events()?;

        let operation = harness.run_prompt("propose the same source bytes").await?;
        let snapshot = harness.snapshot()?;
        let candidates = manager.candidates()?;
        let mut emitted = Vec::new();
        loop {
            match events.try_recv() {
                Ok(event) => emitted.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => panic!("fixture event subscription disconnected"),
            }
        }

        assert!(operation.is_completed());
        assert!(
            candidates.last().is_some_and(|candidate| candidate.validation.is_noop),
            "the exact same immutable source must stage as a no-op: {candidates:#?}",
        );
        assert!(!snapshot.records().iter().any(|stored| {
            matches!(stored.record, LaneRecord::HarnessActivationRequested(_))
        }));
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.body, SessionEntry::HarnessRevisionChanged(_)))
                .count(),
            1,
            "a rejected no-op must not create a semantic revision transition",
        );
        assert!(snapshot.entries().iter().any(|entry| {
            matches!(&entry.body, SessionEntry::ToolResult(result) if result.tool_name == "tea_harness" && result.is_error && !result.terminate)
        }));
        let staged = emitted.iter().find_map(|event| match event {
            TeaEvent::Harness(crate::HarnessEvent::CandidateStaged { candidate_id, .. }) => {
                Some(candidate_id.clone())
            }
            _ => None,
        });
        assert!(staged.is_some(), "the rejected candidate remains retained lineage");
        assert!(emitted.iter().any(|event| matches!(
            event,
            TeaEvent::Harness(crate::HarnessEvent::CandidateRejected {
                candidate_id: Some(candidate_id),
                stage: crate::ValidationStage::Static,
                code,
                ..
            }) if Some(candidate_id) == staged.as_ref() && code.as_str() == "candidate.noop"
        )));

        Ok::<(), crate::HarnessError>(())
    })
    .expect("a no-op candidate must remain inspectable but never schedule activation");
}

#[test]
fn off_mode_has_no_authoring_prompt_or_control_tool_overhead() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let mut repository = HarnessRepository::new(store.clone());
        let tree = repository
            .stage_tree(
                v1_prompt_plugin_sources("Use the ordinary verification check."),
                &HarnessTreeLimits::default(),
            )
            .expect("fixture plugin tree stages");
        let snapshot = repository
            .stage_snapshot(lineage_snapshot_spec(
                PluginBundleRef {
                    plugin_id: "session.verify".into(),
                    tree_id: tree.id,
                    requested_capabilities: Default::default(),
                },
                "trusted prefix",
            ))
            .expect("off-mode snapshot stages");
        let revision = repository
            .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
            .expect("initial revision stages");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(PromptCapturingProvider {
            streams: Mutex::new(VecDeque::from([ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            }])),
            requests: Arc::clone(&requests),
        });
        let manager = Arc::new(HarnessManager::new(
            repository,
            CoreEpochTemplate::new(provider, tea_core::tool::ToolRegistry::default()),
            Default::default(),
        ));
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("off-mode-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Off),
        ))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("off-mode-initial-revision").expect("fixture entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.revision_id.clone(),
                    snapshot_id: snapshot.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let harness = DurableHarness::new_with_artifact_store(
            session,
            manager,
            HarnessIdentity::new(
                revision.revision_id,
                snapshot.id,
                snapshot.spec.model_harness_profile.clone(),
            ),
            store,
        )?;

        harness.run_prompt("complete an ordinary task").await?;
        let request = requests
            .lock()
            .expect("captured request mutex")
            .first()
            .expect("one provider request")
            .clone();

        assert!(!request.system_prompt.contains(crate::SELF_EXTENSION_V1_CONCISE));
        assert!(!request.tools.iter().any(|tool| tool.name == "tea_harness"));
        assert!(request.tools.iter().any(|tool| tool.name == "tea_artifact_read"));
        Ok::<(), crate::HarnessError>(())
    })
    .expect("off mode must not add self-extension prompt or tool surface");
}

#[test]
fn managed_harness_rollback_tool_uses_the_same_durable_rollover_protocol() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let mut repository = HarnessRepository::new(store.clone());
        let original_tree = repository
            .stage_tree(
                v1_prompt_plugin_sources("Use the original check."),
                &HarnessTreeLimits::default(),
            )
            .expect("original plugin tree stages");
        let mut original_spec = lineage_snapshot_spec(
                PluginBundleRef {
                    plugin_id: "session.verify".into(),
                    tree_id: original_tree.id,
                    requested_capabilities: Default::default(),
                },
                "trusted prefix",
            );
        original_spec.self_extension_addendum = Some(crate::SELF_EXTENSION_V1_CONCISE.into());
        let original = repository
            .stage_snapshot(original_spec)
            .expect("original snapshot stages");
        let original_revision = repository
            .seed_revision(original.id.clone(), HarnessActor::Host, 1)
            .expect("initial revision stages");
        let changed_tree = repository
            .stage_tree(
                v1_prompt_plugin_sources("Use the changed check."),
                &HarnessTreeLimits::default(),
            )
            .expect("changed plugin tree stages");
        let mut changed_spec = lineage_snapshot_spec(
                PluginBundleRef {
                    plugin_id: "session.verify".into(),
                    tree_id: changed_tree.id,
                    requested_capabilities: Default::default(),
                },
                "trusted prefix",
            );
        changed_spec.self_extension_addendum = Some(crate::SELF_EXTENSION_V1_CONCISE.into());
        let changed = repository
            .stage_snapshot(changed_spec)
            .expect("changed snapshot stages");
        let staged = repository
            .stage_candidate(HarnessCandidateDraft {
                parent_revision_id: original_revision.revision_id.clone(),
                proposed_snapshot_id: changed.id.clone(),
                actor: HarnessActor::Model,
                operation_id: None,
                tool_invocation_id: Some("preexisting-change".into()),
                hypothesis: CandidateHypothesis {
                    targeted_evidence: "the original behavior needs a changed check".into(),
                    expected_effect: "the changed check is visible in the next epoch".into(),
                    regression_risk: "the changed check may be harmful".into(),
                },
                changed_paths: vec![
                    tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                        .expect("fixture source path"),
                ],
                registry_operations: Vec::new(),
                changed_surfaces: [HarnessSurface::SystemPrompt].into_iter().collect(),
                targeted_failures: vec!["changed-check".into()],
                evidence: Vec::new(),
                expected_effects: vec!["changed check".into()],
                regression_risks: vec!["harmful change".into()],
                capability_ceiling: Default::default(),
            })
            .expect("changed candidate stages");
        let changed_revision = repository
            .activate_candidate(&staged.candidate_id, HarnessActor::Host, 2)
            .expect("changed candidate activates before rollback fixture");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(PromptCapturingProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("managed-rollback-call")
                                .expect("fixture control-tool call ID"),
                            name: "tea_harness".into(),
                            arguments: SerializedJson::new(format!(
                                r#"{{"operation":"rollback","base_revision":"{}","target_revision":"{}","hypothesis":{{"failure_signature":"the changed check regressed routine work","expected_effect":"restore the original check","regression_risk":"the original check may miss the new failure"}}}}"#,
                                changed_revision.revision_id,
                                original_revision.revision_id,
                            )),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
            requests: Arc::clone(&requests),
        });
        let manager = Arc::new(HarnessManager::new(
            repository,
            CoreEpochTemplate::new(provider, tea_core::tool::ToolRegistry::default()),
            Default::default(),
        ).self_extension_mode(crate::SelfExtensionMode::Adaptive));
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("managed-rollback-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Adaptive),
        ))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("managed-rollback-current-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: changed_revision.revision_id.clone(),
                    snapshot_id: changed.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let harness = DurableHarness::new_with_artifact_store(
            session,
            manager,
            HarnessIdentity::new(
                changed_revision.revision_id.clone(),
                changed.id.clone(),
                changed.spec.model_harness_profile.clone(),
            ),
            store,
        )?;

        let operation = harness.run_prompt("roll back the harmful reusable behavior").await?;
        let snapshot = harness.snapshot()?;
        let captured = requests.lock().expect("captured request mutex").clone();

        assert!(operation.is_completed());
        assert_eq!(captured.len(), 2);
        assert!(captured[0].system_prompt.contains("Use the changed check."));
        assert!(captured[1].system_prompt.contains("Use the original check."));
        let rollback = snapshot
            .entries()
            .iter()
            .rev()
            .find_map(|entry| match &entry.body {
                SessionEntry::HarnessRevisionChanged(change)
                    if change.snapshot_id == original.id
                        && change.revision_id != original_revision.revision_id =>
                {
                    Some(change)
                }
                _ => None,
            })
            .expect("rollback must materialize a fresh child revision");
        assert_eq!(rollback.rollback_from, Some(changed_revision.revision_id));

        Ok::<(), crate::HarnessError>(())
    })
    .expect("rollback must be a recoverable immutable revision transition");
}

#[test]
fn resolved_v1_plugin_hooks_and_capability_bound_handler_are_executable() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let capability_calls = Arc::new(Mutex::new(Vec::new()));
        let host_binding = PluginCapabilityBinding::new(
            "session.echo",
            "fixture.echo",
            "v1",
            Digest::from_bytes("fixture echo host implementation"),
            HandlerLimits::default(),
            Arc::new(EchoPluginCapability {
                calls: Arc::clone(&capability_calls),
            }),
        )
        .expect("fixture binding is valid");
        let mut catalog = PluginCapabilityCatalog::new();
        catalog
            .insert(host_binding.clone())
            .expect("one binding is accepted");

        let mut repository = HarnessRepository::new(store.clone());
        let tree = repository
            .stage_tree(
                [
                    (
                        tea_session::NormalizedPath::new("plugins/session.echo/manifest.json")
                            .expect("fixture manifest path"),
                        br#"{"schema_version":1,"abi_version":1,"id":"session.echo","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":["fixture.echo"]}"#.to_vec(),
                        "application/json".into(),
                    ),
                    (
                        tea_session::NormalizedPath::new("plugins/session.echo/main.luau")
                            .expect("fixture source path"),
                        br#"
                            return {
                                prompt_sections = {
                                    { id = "echo", content = "Use plugin_echo for the bounded echo capability." },
                                },
                                before_tool = function(call)
                                    if call.name == "plugin_echo" then
                                        return { action = "normalize", arguments_json = "{\"normalized\":true}" }
                                    end
                                    return "allow"
                                end,
                                after_tool = function(call, result)
                                    if call.name == "plugin_echo" then
                                        return {
                                            content = "projected: " .. result.content,
                                            recovery_hint = "raw capability result is retained durably",
                                            memory = {
                                                kind = "capability_result",
                                                content_json = "{\"source\":\"plugin_echo\"}",
                                                provenance = { "tool:plugin_echo" },
                                                visibility = "external_only",
                                                retention = "session",
                                            },
                                        }
                                    end
                                    return "keep"
                                end,
                                context_projection = function(context)
                                    assert(context.entries[1].content == nil)
                                    return {
                                        annotations = {
                                            {
                                                id = "plugin_context",
                                                content = "Use the source-pinned context policy.",
                                            },
                                        },
                                    }
                                end,
                                tools = {
                                    {
                                        name = "plugin_echo",
                                        description = "Echo through a host-bound fixture capability.",
                                        capability = "fixture.echo",
                                        execution_mode = "sequential",
                                        schema_json = "{\"type\":\"object\",\"required\":[\"normalized\"]}",
                                        handler_source = [=[
                                            return function(call)
                                                local response = coroutine.yield({
                                                    kind = "capability",
                                                    capability = "fixture.echo",
                                                    method = "echo",
                                                    arguments_json = call.arguments_json,
                                                })
                                                return {
                                                    content = response,
                                                    details_json = "{\"handler\":true}",
                                                }
                                            end
                                        ]=],
                                    },
                                },
                            }
                        "#
                        .to_vec(),
                        "text/plain".into(),
                    ),
                ],
                &HarnessTreeLimits::default(),
            )
            .expect("closed v1 plugin tree stages");
        let plugin = PluginBundleRef {
            plugin_id: "session.echo".into(),
            tree_id: tree.id,
            requested_capabilities: ["fixture.echo".into()].into_iter().collect(),
        };
        let mut spec = lineage_snapshot_spec(plugin, "trusted base");
        spec.capability_bindings = vec![CapabilityBindingRef {
            plugin_id: "session.echo".into(),
            capability: "fixture.echo".into(),
            capability_version: "v1".into(),
            binding_digest: host_binding.binding_digest(),
        }];
        let snapshot = repository
            .stage_snapshot(spec)
            .expect("source and binding reference stage together");
        let revision = repository
            .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
            .expect("initial revision stages");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(PromptCapturingProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("plugin-echo-call")
                                .expect("fixture call ID"),
                            name: "plugin_echo".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
            requests: Arc::clone(&requests),
        });
        let manager = Arc::new(
            HarnessManager::new(
                repository,
                CoreEpochTemplate::new(provider, tea_core::tool::ToolRegistry::default()),
                ["fixture.echo".into()].into_iter().collect(),
            )
            .capability_catalog(catalog),
        );
        let mut session = MemorySession::create(SessionHeader::new(
            SessionId::new("plugin-handler-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Off),
        ))?;
        session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("plugin-handler-initial-revision")
                    .expect("fixture entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.revision_id.clone(),
                    snapshot_id: snapshot.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let harness = DurableHarness::new_with_artifact_store(
            session,
            manager,
            HarnessIdentity::new(
                revision.revision_id,
                snapshot.id,
                snapshot.spec.model_harness_profile.clone(),
            ),
            store,
        )?;

        let operation = harness.run_prompt("exercise the pinned plugin handler").await?;
        let durable = harness.snapshot()?;
        let requests = requests.lock().expect("captured provider requests").clone();

        assert!(operation.is_completed());
        assert!(requests[0]
            .tools
            .iter()
            .any(|tool| tool.name == "plugin_echo"));
        assert!(requests[0]
            .system_prompt
            .contains("Use plugin_echo for the bounded echo capability."));
        assert!(requests[0]
            .context
            .contains("Use the source-pinned context policy."));
        assert_eq!(
            *capability_calls.lock().expect("plugin capability calls"),
            vec![(
                "plugin_echo".into(),
                tea_protocol::JsonValue::parse(r#"{"normalized":true}"#)
                    .expect("normalized call JSON"),
            )]
        );
        let tool_result = durable
            .entries()
            .iter()
            .find_map(|entry| match &entry.body {
                SessionEntry::ToolResult(result) if result.tool_name == "plugin_echo" => Some(result),
                _ => None,
            })
            .expect("plugin tool result is durable");
        let tool_result_sequence = durable
            .entries()
            .iter()
            .find(|entry| matches!(&entry.body, SessionEntry::ToolResult(result) if result.tool_name == "plugin_echo"))
            .expect("plugin tool result envelope")
            .header
            .seq;
        assert!(tool_result
            .model_projection
            .to_json_string()
            .expect("projection JSON")
            .contains("projected: host capability response"));
        match &tool_result.full_result {
            PayloadRef::Inline(value) => assert!(
                value
                    .to_json_string()
                    .expect("raw result JSON")
                    .contains("host capability response")
            ),
            PayloadRef::Artifact { .. } => panic!("small fixture raw result should remain inline"),
        }
        let memory_entry = durable
            .entries()
            .iter()
            .find(|entry| match &entry.body {
                SessionEntry::PluginMemory(memory) if memory.kind == "capability_result" => {
                    true
                }
                _ => false,
            })
            .expect("post-tool policy proposal becomes a Rust-owned semantic memory entry");
        assert!(
            memory_entry.header.seq > tool_result_sequence,
            "raw tool evidence must commit before a post-tool memory proposal"
        );
        let SessionEntry::PluginMemory(memory) = &memory_entry.body else {
            unreachable!("entry filter requires plugin memory")
        };
        assert_eq!(memory.plugin_id, "session.echo");
        assert_eq!(memory.visibility, tea_session::MemoryVisibility::ExternalOnly);
        assert_eq!(memory.retention, tea_session::MemoryRetention::Session);
        assert_eq!(
            memory.content,
            PayloadRef::Inline(
                tea_protocol::JsonValue::parse(r#"{"source":"plugin_echo"}"#)
                    .expect("memory content JSON"),
            )
        );
        Ok::<(), crate::HarnessError>(())
    })
    .expect("a selected v1 plugin must execute only through its exact host binding");
}

#[test]
fn v1_lifecycle_state_commits_before_boundaries_and_rehydrates_on_resume() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let mut repository = HarnessRepository::new(store.clone());
        let tree = repository
            .stage_tree(
                [
                    (
                        tea_session::NormalizedPath::new(
                            "plugins/session.lifecycle/manifest.json",
                        )
                        .expect("fixture manifest path"),
                        br#"{"schema_version":1,"abi_version":1,"id":"session.lifecycle","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":[]}"#.to_vec(),
                        "application/json".into(),
                    ),
                    (
                        tea_session::NormalizedPath::new(
                            "plugins/session.lifecycle/main.luau",
                        )
                        .expect("fixture source path"),
                        br#"
                            return {
                                prompt_sections = {},
                                resume_hooks = {
                                    {
                                        id = "first",
                                        before_operation = function()
                                            return '{"owner":"first"}'
                                        end,
                                        before_epoch = function()
                                            return '{"epoch":"first"}'
                                        end,
                                        before_resume = function(state)
                                            assert(state.operation.owner == "first")
                                            assert(state.operation.private == nil)
                                            assert(state.epoch.epoch == "first")
                                        end,
                                    },
                                    {
                                        id = "second",
                                        before_operation = function()
                                            return '{"owner":"second","private":"second-only"}'
                                        end,
                                        before_epoch = function()
                                            return '{"epoch":"second"}'
                                        end,
                                        before_resume = function(state)
                                            assert(state.operation.owner == "second")
                                            assert(state.operation.private == "second-only")
                                            assert(state.epoch.epoch == "second")
                                        end,
                                    },
                                },
                            }
                        "#
                        .to_vec(),
                        "text/plain".into(),
                    ),
                ],
                &HarnessTreeLimits::default(),
            )
            .expect("closed lifecycle plugin tree stages");
        let plugin = PluginBundleRef {
            plugin_id: "session.lifecycle".into(),
            tree_id: tree.id,
            requested_capabilities: Default::default(),
        };
        let snapshot = repository
            .stage_snapshot(lineage_snapshot_spec(plugin, "trusted lifecycle base"))
            .expect("lifecycle source stages into a snapshot");
        let revision = repository
            .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
            .expect("initial lifecycle revision stages");
        let provider = Arc::new(PromptCapturingProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
            requests: Arc::new(Mutex::new(Vec::new())),
        });
        let manager = Arc::new(HarnessManager::new(
            repository,
            CoreEpochTemplate::new(provider, tea_core::tool::ToolRegistry::default()),
            Default::default(),
        ));
        let identity = HarnessIdentity::new(
            revision.revision_id.clone(),
            snapshot.id.clone(),
            snapshot.spec.model_harness_profile.clone(),
        );

        let mut first_session = MemorySession::create(SessionHeader::new(
            SessionId::new("lifecycle-first-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Off),
        ))?;
        first_session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("lifecycle-first-revision").expect("fixture entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.revision_id.clone(),
                    snapshot_id: snapshot.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let first = DurableHarness::new_with_artifact_store(
            first_session,
            Arc::clone(&manager),
            identity.clone(),
            store.clone(),
        )?;
        let completed = first.run_prompt("persist lifecycle state").await?;
        assert!(completed.is_completed());
        let committed = first.snapshot()?;
        let operation_state = committed
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::OperationStarted(record) => Some(record.operation_resume_data.clone()),
                _ => None,
            })
            .expect("operation start has lifecycle state");
        let epoch_state = committed
            .records()
            .iter()
            .find_map(|stored| match &stored.record {
                LaneRecord::EpochStarted(record) => Some(record.epoch_resume_data.clone()),
                _ => None,
            })
            .expect("epoch start has lifecycle state");
        assert_eq!(
            operation_state.get(
                &StableHookId::new("session.lifecycle.first").expect("stable hook ID"),
            ),
            Some(&tea_protocol::JsonValue::parse(r#"{"owner":"first"}"#).unwrap())
        );
        assert_eq!(
            operation_state.get(
                &StableHookId::new("session.lifecycle.second").expect("stable hook ID"),
            ),
            Some(
                &tea_protocol::JsonValue::parse(
                    r#"{"owner":"second","private":"second-only"}"#,
                )
                .unwrap(),
            )
        );
        assert_eq!(
            epoch_state.get(
                &StableHookId::new("session.lifecycle.first").expect("stable hook ID"),
            ),
            Some(&tea_protocol::JsonValue::parse(r#"{"epoch":"first"}"#).unwrap())
        );

        // Reconstruct an open durable prefix from those exact committed maps.
        // `resume` must invoke the source-pinned callbacks before it continues
        // the epoch, and the Lua assertions prove no hook receives its sibling
        // registration's state.
        let mut reopened_session = MemorySession::create(SessionHeader::new(
            SessionId::new("lifecycle-reopened-session").expect("fixture session ID"),
            "fixture-workspace",
            managed_session_metadata(crate::SelfExtensionMode::Off),
        ))?;
        let revision_entry = EntryId::new("lifecycle-reopened-revision").expect("entry ID");
        reopened_session.append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: revision_entry.clone(),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.revision_id.clone(),
                    snapshot_id: snapshot.id.clone(),
                    rollback_from: None,
                }),
            },
        )?;
        let operation_id = OperationId::new("lifecycle-reopened-operation").expect("operation ID");
        let input = ProvisionedEntry::user(
            EntryId::new("lifecycle-reopened-user").expect("entry ID"),
            "resume the durable lifecycle fixture",
        );
        let mut operation = OperationStartedRecord::new(
            operation_id.clone(),
            LaneId::main(),
            Some(revision_entry),
            OperationKind::Run,
            vec![input.clone()],
            revision.revision_id.clone(),
            snapshot.spec.model_harness_profile.clone(),
        );
        operation.operation_resume_data = operation_state;
        reopened_session.append_record(LaneRecord::OperationStarted(operation))?;
        reopened_session.append_entry(&LaneId::main(), input.clone())?;
        let epoch_id = EpochId::new("lifecycle-reopened-epoch").expect("epoch ID");
        reopened_session.append_record(LaneRecord::EpochStarted(EpochStartedRecord {
            id: epoch_id,
            operation_id,
            epoch_index: 0,
            source_leaf_id: Some(input.id),
            harness_revision_id: revision.revision_id.clone(),
            harness_snapshot_id: snapshot.id.clone(),
            model_harness_profile: snapshot.spec.model_harness_profile.clone(),
            core_run_id: CoreRunId::new("lifecycle-reopened-core-run").expect("core run ID"),
            epoch_resume_data: epoch_state,
        }))?;
        let reopened = DurableHarness::new_with_artifact_store(
            reopened_session,
            manager,
            identity,
            store,
        )?;
        let resumed = reopened.resume().await?;
        assert!(resumed.is_completed());

        Ok::<(), crate::HarnessError>(())
    })
    .expect("v1 lifecycle state must survive restart without crossing plugin hook boundaries");
}

#[test]
fn model_profiles_have_content_identities_and_record_schema_deviation_evidence() {
    let profile = ModelHarnessProfile::new(
        "openai-compatible",
        "fixture-model",
        Some("returned-revision-a".into()),
        "base-v1",
        "tools-v1",
        "bounded-summary-v1",
        "recoverable-tool-v1",
    )
    .expect("profile is valid");
    profile.verify_identity().expect("profile identity is content-derived");
    let changed_profile = ModelHarnessProfile::new(
        "openai-compatible",
        "fixture-model-2",
        Some("returned-revision-a".into()),
        "base-v1",
        "tools-v1",
        "bounded-summary-v1",
        "recoverable-tool-v1",
    )
    .expect("changed profile is valid");
    assert_ne!(profile.profile_id, changed_profile.profile_id);

    let schema = tea_protocol::JsonValue::parse(
        r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"},"limit":{"type":"number"}},"additionalProperties":false}"#,
    )
    .expect("schema");
    let deviation = inspect_tool_schema_deviation(
        profile.profile_id.clone(),
        "read_file",
        &schema,
        &tea_protocol::JsonValue::parse(r#"{"path":7,"unexpected":true}"#)
            .expect("arguments"),
        tea_session::ArtifactId::from_bytes("raw tool arguments"),
    )
    .expect("deviation inspection")
    .expect("closed schema deviations are recorded");
    assert_eq!(deviation.unknown_fields, vec!["unexpected"]);
    assert_eq!(deviation.missing_fields, Vec::<String>::new());
    assert_eq!(deviation.type_mismatches.len(), 1);
    assert_eq!(deviation.type_mismatches[0].field, "path");
}

#[test]
fn rejected_tool_arguments_retain_schema_evidence_before_the_invalid_result() {
    smol::block_on(async {
        let store = Arc::new(MemoryArtifactStore::default());
        let executions = Arc::new(Mutex::new(0));
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(tea_core::AgentToolCall {
                            id: ToolCallId::new("schema-deviation-call")
                                .expect("fixture call ID"),
                            name: "record".into(),
                            arguments: SerializedJson::new(
                                r#"{"path":7,"unexpected":true}"#,
                            ),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
        });
        let mut tools = tea_core::tool::ToolRegistry::default();
        tools.insert(Arc::new(CountingTool {
            calls: Arc::clone(&executions),
            schema: tea_protocol::JsonValue::parse(
                r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"}},"additionalProperties":false}"#,
            )
            .expect("closed schema"),
        }));
        let (harness, identity) = managed_harness(
            "schema-deviation-session",
            CoreEpochTemplate::new(provider, tools),
            store.clone(),
        );

        let operation = harness.run_prompt("make the malformed call").await?;
        let snapshot = harness.snapshot()?;
        let deviation = snapshot
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                tea_session::SessionFact::ToolSchemaDeviation(deviation) => Some(deviation),
                _ => None,
            })
            .expect("invalid arguments retain one durable schema-deviation fact");

        assert!(operation.is_completed());
        assert_eq!(*executions.lock().expect("execution count"), 0);
        assert_eq!(deviation.operation_id, operation.id().clone());
        assert_eq!(deviation.tool_call_id, "schema-deviation-call");
        assert_eq!(deviation.tool_name, "record");
        assert_eq!(deviation.model_harness_profile, identity.profile_id().clone());
        assert!(deviation.arguments_valid_json);
        assert_eq!(deviation.unknown_fields, vec!["unexpected"]);
        assert_eq!(deviation.missing_fields, Vec::<String>::new());
        assert_eq!(deviation.type_mismatches.len(), 1);
        assert_eq!(deviation.type_mismatches[0].field, "path");
        let PayloadRef::Artifact {
            artifact_id,
            byte_len,
            ..
        } = &deviation.raw_arguments
        else {
            panic!("rejected raw arguments must remain in an immutable artifact");
        };
        assert_eq!(
            store.get(*artifact_id).expect("retained raw arguments"),
            br#"{"path":7,"unexpected":true}"#,
        );
        assert_eq!(*byte_len, br#"{"path":7,"unexpected":true}"#.len() as u64);
        assert!(!snapshot.records().iter().any(|stored| {
            matches!(stored.record, LaneRecord::ToolStarted(_))
        }));
        assert!(snapshot.entries().iter().any(|entry| {
            matches!(&entry.body, SessionEntry::ToolResult(result) if result.tool_call_id == "schema-deviation-call" && result.is_error)
        }));
        harness.verify_durable_state()?;

        Ok::<(), crate::HarnessError>(())
    })
    .expect("invalid tool arguments must be durable evidence without an external effect");
}

#[test]
fn context_derivation_walks_the_branch_applies_compaction_and_preserves_protected_pairs() {
    let store = Arc::new(MemoryArtifactStore::default());
    let mut repository = HarnessRepository::new(store);
    let tree = repository
        .stage_tree(
            v1_prompt_plugin_sources("Use durable context fixtures."),
            &HarnessTreeLimits::default(),
        )
        .expect("context fixture plugin tree");
    let harness = repository
        .stage_snapshot(lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: tree.id,
                requested_capabilities: Default::default(),
            },
            "trusted context prefix",
        ))
        .expect("context fixture snapshot");
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("context-derivation-session").expect("session ID"),
        "fixture-workspace",
        Default::default(),
    ))
    .expect("context session");
    let lane = LaneId::main();
    let user_id = EntryId::new("context-root-user").expect("entry ID");
    let assistant_id = EntryId::new("context-compact-assistant").expect("entry ID");
    let result_id = EntryId::new("context-compact-result").expect("entry ID");
    let memory_id = EntryId::new("context-plugin-memory").expect("entry ID");

    session
        .append_entry(&lane, ProvisionedEntry::user(user_id.clone(), "solve the durable task"))
        .expect("root user persists");
    session
        .append_entry(
            &lane,
            ProvisionedEntry::assistant(
                assistant_id.clone(),
                "I will inspect the retained output.",
                vec![AssistantToolCall::new(
                    "context-tool-call",
                    "inspect",
                    tea_protocol::JsonValue::parse("{}").expect("tool arguments"),
                )],
            ),
        )
        .expect("assistant persists");
    session
        .append_entry(
            &lane,
            ProvisionedEntry {
                id: result_id.clone(),
                body: SessionEntry::ToolResult(ToolResultEntry {
                    tool_call_id: "context-tool-call".into(),
                    tool_name: "inspect".into(),
                    full_result: PayloadRef::Inline(
                        tea_protocol::JsonValue::parse(r#"{"content":"raw retained output"}"#)
                            .expect("raw result"),
                    ),
                    model_projection: tea_protocol::JsonValue::parse(
                        r#"{"content":"bounded projected output","details":null}"#,
                    )
                    .expect("projected result"),
                    is_error: false,
                    terminate: false,
                    usage: Usage::default(),
                    projection_strategy_id: "fixture-projection".into(),
                    artifact_policy_id: ArtifactPolicyId::new("fixture-artifact-policy")
                        .expect("artifact policy ID"),
                }),
            },
        )
        .expect("tool result persists");
    session
        .append_entry(
            &lane,
            ProvisionedEntry {
                id: EntryId::new("context-compaction").expect("entry ID"),
                body: SessionEntry::Compaction(CompactionEntry {
                    covered_from: Some(assistant_id.clone()),
                    covered_to: Some(result_id.clone()),
                    retained_tail_boundary: Some(memory_id.clone()),
                    summary: "The inspect call found bounded durable evidence. [history:context]"
                        .into(),
                    strategy_id: "fixture-compaction".into(),
                    recovery_index_artifact: None,
                    harness_revision_id: None,
                }),
            },
        )
        .expect("compaction persists");
    session
        .append_entry(
            &lane,
            ProvisionedEntry {
                id: memory_id.clone(),
                body: SessionEntry::PluginMemory(PluginMemoryEntry {
                    plugin_id: "session.verify".into(),
                    kind: "fact".into(),
                    content: PayloadRef::Inline(
                        tea_protocol::JsonValue::parse(r#"{"verified":true}"#)
                            .expect("memory content"),
                    ),
                    provenance: vec!["tool:inspect".into()],
                    visibility: MemoryVisibility::ModelVisible,
                    retention: MemoryRetention::Session,
                }),
            },
        )
        .expect("memory persists");

    let before_tail = derive_model_context(
        &session,
        lane.clone(),
        &harness,
        ProviderLimits::new(64 * 1024).expect("provider limit"),
    )
    .expect("context derives from compacted branch");
    assert!(before_tail.included_entries.contains(&user_id));
    assert!(!before_tail.included_entries.contains(&memory_id));
    assert!(!before_tail.included_entries.contains(&assistant_id));
    assert!(!before_tail.included_entries.contains(&result_id));
    assert!(before_tail
        .serialized_context
        .contains("Compaction summary"));

    let with_selected_memory = derive_model_context_with_patch(
        &session,
        lane.clone(),
        &harness,
        ProviderLimits::new(64 * 1024).expect("provider limit"),
        &ContextProjectionPatch {
            selected_memory: vec![memory_id.clone()],
            ..ContextProjectionPatch::default()
        },
    )
    .expect("typed memory is selected explicitly");
    assert!(with_selected_memory.included_entries.contains(&memory_id));

    let tail_id = EntryId::new("context-tail-user").expect("entry ID");
    session
        .append_entry(&lane, ProvisionedEntry::user(tail_id.clone(), "continue from the summary"))
        .expect("tail persists");
    let after_tail = derive_model_context(
        &session,
        lane.clone(),
        &harness,
        ProviderLimits::new(64 * 1024).expect("provider limit"),
    )
    .expect("context derives after append");
    assert!(after_tail.serialized_context.starts_with(&before_tail.serialized_context));
    assert!(after_tail.included_entries.contains(&tail_id));

    let root_removal = derive_model_context_with_patch(
        &session,
        lane.clone(),
        &harness,
        ProviderLimits::new(64 * 1024).expect("provider limit"),
        &ContextProjectionPatch {
            retain_entries: vec![tail_id.clone()],
            ..ContextProjectionPatch::default()
        },
    )
    .expect_err("original user task is protected");
    assert!(root_removal
        .to_string()
        .contains("original user task"));

    let split_pair = derive_model_context_with_patch(
        &session,
        lane,
        &harness,
        ProviderLimits::new(64 * 1024).expect("provider limit"),
        &ContextProjectionPatch {
            retain_entries: vec![user_id, assistant_id],
            ..ContextProjectionPatch::default()
        },
    )
    .expect_err("tool-call/result pair is protected");
    assert!(split_pair.to_string().contains("separates tool call"));
}

fn lineage_snapshot_spec(plugin: PluginBundleRef, prompt: &str) -> HarnessSnapshotSpec {
    HarnessSnapshotSpec {
        base_profile_digest: Digest::from_bytes("trusted base profile"),
        base_system_prompt: prompt.into(),
        model_harness_profile: tea_session::ModelHarnessProfileId::new("lineage-profile")
            .expect("fixture profile ID"),
        self_extension_addendum: None,
        ordered_global_plugins: Vec::new(),
        ordered_session_plugins: vec![plugin],
        prompt_sections: vec![PromptSectionDescriptor {
            id: "verify".into(),
            content: "verify narrow changes".into(),
        }],
        plugin_prompt_sections: Vec::new(),
        tool_presentations: vec![ToolPresentationDescriptor {
            name: "verify".into(),
            description: "verify a selected target".into(),
            schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("fixture schema"),
            execution_mode: "parallel".into(),
        }],
        plugin_tool_presentations: Vec::new(),
        hook_bundle_digest: Digest::from_bytes("hook bundle"),
        capability_bindings: Vec::new(),
        resource_limits: HarnessResourceLimits::default(),
        compaction_policy_digest: Digest::from_bytes("compaction"),
        tool_projection_digest: Digest::from_bytes("projection"),
        failure_policy_digest: Digest::from_bytes("failure"),
    }
}

fn managed_session_metadata(mode: crate::SelfExtensionMode) -> tea_session::Metadata {
    [(
        crate::SELF_EXTENSION_MODE_METADATA_KEY.into(),
        mode.metadata_value(),
    )]
    .into_iter()
    .collect()
}

fn staged_managed_session(
    session_id: &str,
    template: CoreEpochTemplate,
    store: Arc<MemoryArtifactStore>,
) -> (MemorySession, Arc<HarnessManager>, HarnessIdentity) {
    let mut repository = HarnessRepository::new(store.clone());
    let tree = repository
        .stage_tree(
            v1_prompt_plugin_sources("Use durable harness test fixtures."),
            &HarnessTreeLimits::default(),
        )
        .expect("fixture source tree stages");
    let snapshot = repository
        .stage_snapshot(lineage_snapshot_spec(
            PluginBundleRef {
                plugin_id: "session.verify".into(),
                tree_id: tree.id,
                requested_capabilities: Default::default(),
            },
            "Use durable harness test fixtures.",
        ))
        .expect("fixture snapshot stages");
    let revision = repository
        .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
        .expect("fixture revision stages");
    let identity = HarnessIdentity::new(
        revision.revision_id.clone(),
        snapshot.id.clone(),
        snapshot.spec.model_harness_profile.clone(),
    );
    let manager = Arc::new(HarnessManager::new(
        repository,
        template,
        Default::default(),
    ));
    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new(session_id).expect("fixture session ID"),
        "fixture-workspace",
        managed_session_metadata(crate::SelfExtensionMode::Off),
    ))
    .expect("fixture session creates");
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("managed-fixture-initial-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: revision.revision_id,
                    snapshot_id: snapshot.id,
                    rollback_from: None,
                }),
            },
        )
        .expect("fixture initial revision commits");
    (session, manager, identity)
}

fn managed_harness(
    session_id: &str,
    template: CoreEpochTemplate,
    store: Arc<MemoryArtifactStore>,
) -> (DurableHarness<MemorySession>, HarnessIdentity) {
    let (session, manager, identity) = staged_managed_session(session_id, template, store.clone());
    let harness = DurableHarness::new_with_artifact_store(
        session,
        manager,
        identity.clone(),
        store,
    )
    .expect("fixture durable harness creates");
    (harness, identity)
}

fn v1_prompt_plugin_sources(
    content: &str,
) -> [(tea_session::NormalizedPath, Vec<u8>, String); 2] {
    [
        (
            tea_session::NormalizedPath::new("plugins/session.verify/manifest.json")
                .expect("fixture manifest path"),
            br#"{"schema_version":1,"abi_version":1,"id":"session.verify","entrypoint":"main.luau","modules":["main.luau"],"requested_capabilities":[]}"#.to_vec(),
            "application/json".into(),
        ),
        (
            tea_session::NormalizedPath::new("plugins/session.verify/main.luau")
                .expect("fixture source path"),
            format!(
                "return {{ prompt_sections = {{ {{ id = 'verification', content = {:?} }} }} }}",
                content
            )
            .into_bytes(),
            "text/plain".into(),
        ),
    ]
}
