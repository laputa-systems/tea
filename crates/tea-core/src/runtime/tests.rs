use super::{HarnessIdentity, RuntimeServices, SessionRuntime};
use crate::harness::extension::NoExtensions;
use crate::harness::{
    HarnessActor, HarnessRepository, HarnessResolver, HarnessResourceLimits, HarnessSnapshotSpec,
    PromptSectionDescriptor, SelfExtensionMode, ToolPresentationDescriptor,
    SELF_EXTENSION_MODE_METADATA_KEY,
};
use crate::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use crate::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId};
use crate::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolRegistry, ToolUpdateSink,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tea_protocol::JsonValue;
use tea_session::{
    ArtifactStore, Digest, EntryId, HarnessRevisionChangedEntry, LaneId, LaneRecord,
    DurabilityMode, JsonlSession, MemoryArtifactStore, MemorySession, ModelHarnessProfileId,
    ProvisionedEntry, SessionEntry, SessionFact, SessionHeader, SessionId, SessionWriter,
};

#[derive(Debug)]
struct QueuedProvider {
    streams: Mutex<VecDeque<ModelStream>>,
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
            .expect("fixture provider queue mutex")
            .pop_front()
            .expect("fixture provider has a response stream");
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[derive(Debug)]
struct RecordingTool;

impl AgentTool for RecordingTool {
    fn name(&self) -> &str {
        "record"
    }

    fn description(&self) -> &str {
        "records one durable tool intent"
    }

    fn schema(&self) -> &JsonValue {
        static SCHEMA: std::sync::LazyLock<JsonValue> =
            std::sync::LazyLock::new(|| JsonValue::parse(r#"{"type":"object"}"#).unwrap());
        &SCHEMA
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
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

fn snapshot_spec() -> HarnessSnapshotSpec {
    HarnessSnapshotSpec {
        base_profile_digest: Digest::from_bytes("runtime-test-host-profile"),
        base_system_prompt: "Use the durable runtime fixture.".into(),
        model_harness_profile: ModelHarnessProfileId::new("runtime-test-profile")
            .expect("fixture profile ID"),
        self_extension_addendum: None,
        ordered_global_plugins: Vec::new(),
        ordered_session_plugins: Vec::new(),
        prompt_sections: vec![PromptSectionDescriptor {
            id: "runtime-test".into(),
            content: "Keep durable evidence ordered.".into(),
        }],
        plugin_prompt_sections: Vec::new(),
        tool_presentations: vec![ToolPresentationDescriptor {
            name: "record".into(),
            description: "records one durable tool intent".into(),
            schema: JsonValue::parse(r#"{"type":"object"}"#).expect("fixture schema"),
            execution_mode: "parallel".into(),
        }],
        plugin_tool_presentations: Vec::new(),
        hook_bundle_digest: Digest::from_bytes("runtime-test-hooks"),
        capability_bindings: Vec::new(),
        resource_limits: HarnessResourceLimits::default(),
        compaction_policy_digest: Digest::from_bytes("runtime-test-compaction"),
        tool_projection_digest: Digest::from_bytes("runtime-test-projection"),
        failure_policy_digest: Digest::from_bytes("runtime-test-failure-policy"),
    }
}

fn fixture_manager(
    provider: Arc<dyn ModelProvider>,
    store: Arc<MemoryArtifactStore>,
) -> (Arc<HarnessResolver>, HarnessIdentity) {
    let mut repository = HarnessRepository::with_extension_engine(store.clone(), Arc::new(NoExtensions));
    let snapshot = repository
        .stage_snapshot(snapshot_spec())
        .expect("no-extension snapshot stages");
    let revision = repository
        .seed_revision(snapshot.id.clone(), HarnessActor::Host, 1)
        .expect("initial fixture revision stages");
    let identity = HarnessIdentity::new(
        revision.revision_id.clone(),
        snapshot.id.clone(),
        snapshot.spec.model_harness_profile.clone(),
    );
    let mut tools = ToolRegistry::default();
    tools.insert(Arc::new(RecordingTool));
    (
        Arc::new(HarnessResolver::new(
        repository,
        RuntimeServices::new(provider, tools),
        Default::default(),
        )),
        identity,
    )
}

fn fixture_metadata() -> tea_session::Metadata {
    [(
        SELF_EXTENSION_MODE_METADATA_KEY.into(),
        SelfExtensionMode::Off.metadata_value(),
    )]
    .into_iter()
    .collect()
}

fn append_initial_revision<S: SessionWriter>(
    session: &mut S,
    identity: &HarnessIdentity,
) {
    session
        .append_entry(
            &LaneId::main(),
            ProvisionedEntry {
                id: EntryId::new("runtime-test-initial-revision")
                    .expect("fixture revision entry ID"),
                body: SessionEntry::HarnessRevisionChanged(HarnessRevisionChangedEntry {
                    revision_id: identity.revision_id().clone(),
                    snapshot_id: identity.snapshot_id().clone(),
                    rollback_from: None,
                }),
            },
        )
        .expect("initial revision entry commits");
}

fn build_runtime(
    session_id: &str,
    provider: Arc<dyn ModelProvider>,
    store: Arc<MemoryArtifactStore>,
) -> (SessionRuntime<MemorySession>, HarnessIdentity) {
    let (manager, identity) = fixture_manager(provider, store.clone());
    let mut session = MemorySession::create(
        SessionHeader::new(
            SessionId::new(session_id).expect("fixture session ID"),
            "runtime-test-workspace",
            fixture_metadata(),
        ),
    )
    .expect("fixture session creates");
    append_initial_revision(&mut session, &identity);
    (
        SessionRuntime::new_with_artifact_store(session, manager, identity.clone(), store)
            .expect("runtime creates from immutable no-extension lineage"),
        identity,
    )
}

#[test]
fn no_extension_runtime_persists_intents_trace_and_verifies() {
    smol::block_on(async {
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("runtime-record-call")
                                .expect("fixture tool call ID"),
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
        });
        let store = Arc::new(MemoryArtifactStore::default());
        let (runtime, identity) = build_runtime("runtime-no-extension", provider, store.clone());

        let operation = runtime
            .run_prompt("exercise durable core runtime")
            .await
            .expect("runtime operation settles");
        assert!(operation.is_completed());
        let snapshot = runtime.snapshot().expect("durable snapshot is readable");
        assert!(snapshot.records().iter().any(|stored| {
            matches!(stored.record, LaneRecord::ProviderRequestStarted(_))
        }));
        assert!(snapshot
            .records()
            .iter()
            .any(|stored| matches!(stored.record, LaneRecord::ToolStarted(_))));
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(entry.body, SessionEntry::ToolResult(_)))
                .count(),
            1,
        );
        let trace = snapshot
            .facts()
            .iter()
            .find_map(|stored| match &stored.fact {
                SessionFact::TraceArtifact(trace) => Some(trace),
                _ => None,
            })
            .expect("completed epoch retains a trace artifact");
        assert_eq!(trace.operation_id, *operation.id());
        assert_eq!(trace.harness_revision_id, *identity.revision_id());
        assert_eq!(trace.harness_snapshot_id, *identity.snapshot_id());
        assert_eq!(trace.model_harness_profile, *identity.profile_id());
        let trace_bytes = store
            .get(trace.artifact_id)
            .expect("retained trace artifact remains reachable");
        assert!(std::str::from_utf8(&trace_bytes)
            .expect("trace is JSON Lines")
            .contains(r#""type":"episode_end""#));
        runtime
            .verify_durable_state()
            .expect("runtime verifies its catalog and reachable artifacts");

    });
}

#[test]
fn jsonl_runtime_reopens_the_persisted_no_extension_catalog() {
    let directory = std::env::temp_dir().join(format!(
        "tea-core-runtime-reopen-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos(),
    ));
    let store = Arc::new(MemoryArtifactStore::default());
    let provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let (manager, identity) = fixture_manager(provider, store.clone());
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("runtime-jsonl-reopen").expect("fixture session ID"),
            "runtime-test-workspace",
            fixture_metadata(),
        ),
        DurabilityMode::Strict,
    )
    .expect("fixture JSONL session creates");
    append_initial_revision(&mut session, &identity);
    let runtime = SessionRuntime::new_with_artifact_store(session, manager, identity, store.clone())
        .expect("runtime persists its immutable catalog");
    let expected_sequence = runtime
        .snapshot()
        .expect("created runtime snapshot is readable")
        .last_sequence();
    drop(runtime);

    let reopened_session =
        JsonlSession::open(&directory, DurabilityMode::Strict).expect("JSONL session reopens");
    let reopened_provider = Arc::new(QueuedProvider {
        streams: Mutex::new(VecDeque::new()),
    });
    let empty_repository = HarnessRepository::with_extension_engine(store.clone(), Arc::new(NoExtensions));
    let reopened_manager = Arc::new(HarnessResolver::new(
        empty_repository,
        RuntimeServices::new(reopened_provider, ToolRegistry::default()),
        Default::default(),
    ));
    let reopened = SessionRuntime::reopen_with_artifact_store(
        reopened_session,
        reopened_manager,
        store,
    )
    .expect("runtime restores the catalog from durable state");
    assert_eq!(
        reopened
            .snapshot()
            .expect("reopened snapshot is readable")
            .last_sequence(),
        expected_sequence,
    );
    reopened
        .verify_durable_state()
        .expect("reopened runtime verifies catalog and artifacts");
    assert_eq!(
        reopened
            .measure_prompt_layout(&ModelRequest::default())
            .continuity,
        crate::measurement::PromptContinuity::FirstRequest,
        "reopen starts a fresh volatile prompt-layout continuity ledger",
    );
    drop(reopened);
    std::fs::remove_dir_all(&directory).expect("fixture JSONL directory removes");
}

#[test]
fn live_runtime_joins_prompt_layout_across_fresh_operations() {
    smol::block_on(async {
        let provider = Arc::new(QueuedProvider {
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("first response".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("second response".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ])),
        });
        let store = Arc::new(MemoryArtifactStore::default());
        let (runtime, _) = build_runtime("runtime-prompt-layout", provider, store.clone());
        let first = runtime.run_prompt("first operation").await.expect("first settles");
        let second = runtime
            .run_prompt("second operation")
            .await
            .expect("second settles");
        let snapshot = runtime.snapshot().expect("snapshot reads");
        let traces = snapshot
            .facts()
            .iter()
            .filter_map(|stored| match &stored.fact {
                SessionFact::TraceArtifact(trace) if trace.operation_id == *first.id() => {
                    Some((false, store.get(trace.artifact_id).expect("trace artifact reads")))
                }
                SessionFact::TraceArtifact(trace) if trace.operation_id == *second.id() => {
                    Some((true, store.get(trace.artifact_id).expect("trace artifact reads")))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(traces.len(), 2);
        let second_trace = traces
            .iter()
            .find(|(second, _)| *second)
            .expect("second trace is present");
        let second_trace = std::str::from_utf8(&second_trace.1).expect("trace UTF-8");
        assert!(second_trace.contains("deterministic_common_prefix_bytes"));
        let first_trace = traces
            .iter()
            .find(|(second, _)| !*second)
            .expect("first trace is present");
        let first_trace = std::str::from_utf8(&first_trace.1).expect("trace UTF-8");
        assert!(first_trace.contains(r#""continuity":"first_request""#));
        assert!(first_trace.contains(r#""deterministic_common_prefix_bytes":null"#));
    });
}
