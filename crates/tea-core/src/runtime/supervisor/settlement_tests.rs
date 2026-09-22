use super::*;
use crate::harness::extension::{
    CollectedExtensionMemoryProposal, ExtensionMemoryProposal, ExtensionMemoryRetention,
    ExtensionMemoryVisibility,
};
use crate::harness::ResolvedHarness;
use crate::hooks::NoHooks;
use crate::state::{SerializedJson, ToolCallId};
use tea_session::{
    AssistantMessageEntry, AssistantToolCall, MemoryArtifactStore, MemorySession, Metadata,
    ModelHarnessProfileId, SessionHeader, SessionId, SessionWriter,
};

fn fixture_resolved_harness(
    identity: HarnessIdentity,
    memory_collector: Arc<ExtensionMemoryCollector>,
) -> ResolvedHarness {
    ResolvedHarness {
        identity,
        system_prompt: String::new(),
        extension_tools: ToolRegistry::default(),
        host_commands: Vec::new(),
        idle_hooks: Vec::new(),
        extension_state_versions: BTreeMap::new(),
        hooks: Arc::new(NoHooks),
        automatic_compaction: Default::default(),
        tool_result_projection: Default::default(),
        tool_failure_circuit_breaker: Default::default(),
        replay_safe_tools: BTreeSet::new(),
        artifact_policy: Default::default(),
        self_extension_mode: SelfExtensionMode::Off,
        lifecycle: Default::default(),
        memory_collector,
        harness_snapshot: None,
        context_policies: Default::default(),
    }
}

fn fixture_tool_result(call_id: ToolCallId) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call_id,
        content: "completed".into(),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    }
}

#[test]
fn after_tool_commits_memory_with_its_tool_result() {
    let lane = LaneId::main();
    let operation_id = OperationId::new("settlement-operation").expect("fixture operation ID");
    let epoch_id = EpochId::new("settlement-epoch").expect("fixture epoch ID");
    let identity = HarnessIdentity::new(
        HarnessRevisionId::new("settlement-revision").expect("fixture revision ID"),
        HarnessSnapshotId::new("settlement-snapshot").expect("fixture snapshot ID"),
        ModelHarnessProfileId::new("settlement-profile").expect("fixture profile ID"),
    );
    let assistant_entry_id = EntryId::new("settlement-assistant").expect("fixture entry ID");
    let call_id = ToolCallId::new("settlement-call").expect("fixture call ID");
    let call = ToolCall {
        id: call_id.clone(),
        name: "fixture_tool".into(),
        arguments: SerializedJson::new("{}"),
    };
    let result_entry_id = EntryId::new(durable_identifier(
        "entry-tool-result",
        [assistant_entry_id.as_str(), "0"],
    ))
    .expect("fixture result entry ID");

    let mut session = MemorySession::create(SessionHeader::new(
        SessionId::new("settlement-session").expect("fixture session ID"),
        "settlement-workspace",
        Metadata::new(),
    ))
    .expect("fixture session creates");
    session
        .append_entry(
            &lane,
            ProvisionedEntry {
                id: assistant_entry_id.clone(),
                body: SessionEntry::AssistantMessage(AssistantMessageEntry {
                    content: String::new(),
                    tool_calls: vec![AssistantToolCall::new(
                        call.id.to_string(),
                        call.name.clone(),
                        JsonValue::parse("{}").expect("fixture arguments"),
                    )],
                    stop_reason: None,
                    error_message: None,
                    opaque_context: Vec::new(),
                    metadata: Metadata::new(),
                }),
            },
        )
        .expect("fixture assistant entry commits");

    let session = Arc::new(Mutex::new(session));
    let collector = Arc::new(ExtensionMemoryCollector::default());
    collector
        .record(
            call.id.as_str(),
            0,
            CollectedExtensionMemoryProposal {
                extension_id: "fixture.extension".into(),
                proposal: ExtensionMemoryProposal {
                    kind: "fixture.memory".into(),
                    content: JsonValue::String("remember this settlement".into()),
                    provenance: vec!["fixture".into()],
                    visibility: ExtensionMemoryVisibility::ExternalOnly,
                    retention: ExtensionMemoryRetention::Session,
                },
            },
        )
        .expect("fixture proposal records");
    let artifacts = Arc::new(MemoryArtifactStore::default());
    let mut runtime = EpochRuntime::new(EpochRuntimeInit {
        session: Arc::clone(&session),
        artifacts,
        events: Arc::new(EventHub::default()),
        lane: lane.clone(),
        operation_id,
        epoch_id,
        identity: identity.clone(),
        resolved_harness: fixture_resolved_harness(identity, Arc::clone(&collector)),
        memory_collector: collector,
        tool_definition_digests: BTreeMap::new(),
        tool_definition_schemas: BTreeMap::new(),
        replay_safe_host_tools: BTreeSet::new(),
        last_assistant_entry: Some(assistant_entry_id),
        replay_tool_starts: BTreeMap::new(),
    });
    runtime.pending_tools.insert(
        EffectId(1),
        PendingTool {
            result_entry_id: result_entry_id.clone(),
            tool_name: call.name.clone(),
            tool_call_id: call.id.to_string(),
        },
    );
    let result = fixture_tool_result(call.id.clone());

    runtime
        .after_tool(
            EffectId(1),
            &call,
            ToolEffectOutcome {
                raw_result: result.clone(),
                result,
            },
        )
        .expect("tool settlement commits");

    let snapshot = session
        .lock()
        .expect("fixture session lock")
        .snapshot()
        .expect("fixture snapshot reads");
    let tool_result = snapshot
        .entries()
        .iter()
        .find(|entry| entry.header.id == result_entry_id)
        .expect("tool result is durable");
    let memory = snapshot
        .entries()
        .iter()
        .find(|entry| matches!(entry.body, SessionEntry::PluginMemory(_)))
        .expect("post-tool memory is durable");

    assert_eq!(tool_result.header.seq, memory.header.seq);
}
