//! Durable transcript-ordered configuration: persistence, reopen, discovery
//! replay, and settled-turn forks.

use super::*;
use crate::state::{AgentMessage, EffectiveConfiguration};
use crate::testing::{ScriptedProvider, ScriptedTurn};
use crate::tool::ToolExposure;

/// A trusted tool whose result asks discovery to load other tools.
struct DiscoveryTool {
    name: &'static str,
    exposure: ToolExposure,
    loads: Vec<String>,
}

impl AgentTool for DiscoveryTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "discovery fixture"
    }

    fn schema(&self) -> &JsonValue {
        static SCHEMA: std::sync::LazyLock<JsonValue> =
            std::sync::LazyLock::new(|| JsonValue::parse(r#"{"type":"object"}"#).unwrap());
        &SCHEMA
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: format!("{} ran", self.name),
            details: None,
            usage: None,
            added_tool_names: self.loads.clone(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

fn discovery_tools() -> ToolRegistry {
    let mut tools = ToolRegistry::default();
    tools.insert(Arc::new(RecordingTool));
    tools.insert(Arc::new(DiscoveryTool {
        name: "search",
        exposure: ToolExposure::Direct,
        loads: vec!["late".into()],
    }));
    tools.insert(Arc::new(DiscoveryTool {
        name: "late",
        exposure: ToolExposure::Deferred,
        loads: Vec::new(),
    }));
    tools
}

fn configuration_entries(snapshot: &tea_session::SessionSnapshot, lane: &LaneId) -> usize {
    let reduction = reduce_lane(snapshot.clone(), lane.clone()).expect("lane reduces");
    let mut cursor = reduction.lane_state.leaf_id;
    let mut count = 0;
    while let Some(id) = cursor {
        let entry = snapshot
            .entries()
            .iter()
            .find(|entry| entry.header.id == id)
            .expect("branch entry");
        if matches!(entry.body, SessionEntry::ConfigurationChanged(_)) {
            count += 1;
        }
        cursor = entry.header.parent_id.clone();
    }
    count
}

fn temporary_session_directory(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "tea-core-configuration-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
    ))
}

#[test]
fn durable_configuration_and_discovery_survive_reopen_and_forks() {
    let directory = temporary_session_directory("reopen");
    let store = Arc::new(MemoryArtifactStore::default());
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-search", "search", "{}")
            .end_tool_use(),
        ScriptedTurn::new().text("loaded").stop(),
    ]);
    let (manager, identity, _) = fixture_manager(Arc::new(provider.clone()), store.clone());
    let services = RuntimeServices::new(Arc::new(provider.clone()), discovery_tools());
    let mut session = JsonlSession::create(
        &directory,
        SessionHeader::new(
            SessionId::new("runtime-configuration").expect("session ID"),
            "runtime-test-workspace",
            fixture_metadata(),
        ),
        DurabilityMode::Strict,
    )
    .expect("session creates");
    append_initial_revision(&mut session, &identity);
    let runtime = SessionSupervisor::create(SessionSupervisorInput {
        session,
        resolver: manager,
        root_identity: identity,
        root_services: services,
        artifacts: store.clone(),
        rollover_budget: 1,
        subagents: None,
    })
    .expect("supervisor creates");
    smol::block_on(runtime.run_root_prompt("find the tool")).expect("first operation settles");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let names = |request: &ModelRequest| {
        request
            .tools()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
    };
    const BASE: [&str; 5] = [
        "record",
        "search",
        "tea_artifact_read",
        "tea_artifact_search",
        "tea_history_search",
    ];
    let loaded = BASE.iter().copied().chain(["late"]).collect::<Vec<_>>();
    assert_eq!(names(&requests[0]), BASE);
    assert_eq!(names(&requests[1]), loaded);
    let snapshot = runtime.snapshot().expect("snapshot");
    // Initial configuration plus the discovery update, both durable.
    assert_eq!(configuration_entries(&snapshot, &LaneId::main()), 2);
    let checkpoint = snapshot
        .facts()
        .iter()
        .find_map(|stored| match &stored.fact {
            tea_session::SessionFact::TurnCheckpoint(checkpoint) => {
                Some(checkpoint.checkpoint_id.clone())
            }
            _ => None,
        })
        .expect("settled turn checkpoint");
    drop(runtime);

    // Reopen: the derived context replays the persisted configuration, so the
    // next request declares the loaded tool without a new configuration entry.
    provider.push_turn(ScriptedTurn::new().text("still loaded").stop());
    let empty_repository =
        HarnessRepository::with_extension_engine(store.clone(), Arc::new(NoExtensions));
    let reopened = SessionSupervisor::reopen(SessionSupervisorReopenInput {
        session: JsonlSession::open(&directory, DurabilityMode::Strict).expect("reopens"),
        resolver: Arc::new(HarnessResolver::new(empty_repository, Default::default())),
        root_services: RuntimeServices::new(Arc::new(provider.clone()), discovery_tools()),
        lane_services: BTreeMap::new(),
        artifacts: store.clone(),
        rollover_budget: 1,
        subagents: None,
    })
    .expect("supervisor reopens");
    smol::block_on(reopened.run_root_prompt("again")).expect("reopened operation settles");
    let requests = provider.requests();
    assert_eq!(names(&requests[2]), loaded);
    let snapshot = reopened.snapshot().expect("snapshot");
    assert_eq!(configuration_entries(&snapshot, &LaneId::main()), 2);

    // A fork from the first settled turn inherits exactly the configuration
    // in force at that checkpoint.
    let fork_lane = LaneId::new("fork-configuration").expect("lane ID");
    reopened
        .fork_settled_turn(checkpoint, fork_lane.clone())
        .expect("fork commits");
    let snapshot = reopened.snapshot().expect("snapshot");
    let fork_context = crate::runtime::context::derive_default_snapshot_context(&snapshot, fork_lane)
        .expect("fork context derives");
    let configuration =
        EffectiveConfiguration::replay(&fork_context.messages).expect("fork configuration");
    assert_eq!(
        configuration
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        loaded
    );
    assert!(
        fork_context
            .messages
            .iter()
            .any(|message| matches!(message, AgentMessage::System { .. }))
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&directory);
}
