//! Transcript-ordered configuration, deferred discovery, exposure, and
//! provider-exposed thinking through the real agent loop.

use std::sync::Arc;
use tea_core::Agent;
use tea_core::agent::AgentConfiguration;
use tea_core::compaction::{CompactionContext, CompactionFuture, CompactionResult, Compactor};
use tea_core::error::HookError;
use tea_core::event::{AgentEventKind, MessageDelta};
use tea_core::hooks::{AfterToolCall, BeforeToolCall, ContextEnvelope, HookSet, NoHooks};
use tea_core::measurement::{PromptContinuity, PromptLayoutLedger, RequestLayout};
use tea_core::scheduler::{
    CancellationToken, ConfigurationUpdateSupport, ModelCapabilities, ModelProvider,
};
use tea_core::state::{
    AgentMessage, AssistantContent, ConfigurationUpdate, EffectiveConfiguration, MessageId,
    PromptSection, SystemPrompt, ToolCallId,
};
use tea_core::testing::{ScriptedProvider, ScriptedTurn};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolDeclaration, ToolExposure, ToolFuture,
    ToolRegistry, ToolUpdateSink,
};
use tea_core::transcript::ConfigurationProjection;
use tea_protocol::JsonValue;

/// A tool with a configurable exposure whose result may request discovery.
struct FixtureTool {
    name: &'static str,
    exposure: ToolExposure,
    schema: JsonValue,
    loads: Vec<String>,
}

impl FixtureTool {
    fn new(name: &'static str, exposure: ToolExposure) -> Arc<Self> {
        Arc::new(Self {
            name,
            exposure,
            schema: JsonValue::object([("type", JsonValue::from("object"))]),
            loads: Vec::new(),
        })
    }

    fn loading(name: &'static str, loads: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            loads: loads.iter().map(|name| (*name).to_owned()).collect(),
            ..Arc::try_unwrap(Self::new(name, ToolExposure::Direct)).ok().expect("fresh")
        })
    }
}

impl AgentTool for FixtureTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "fixture tool"
    }

    fn schema(&self) -> &JsonValue {
        &self.schema
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

fn prompt(sections: &[(&str, &str)]) -> SystemPrompt {
    SystemPrompt::new(
        sections
            .iter()
            .map(|(id, content)| PromptSection::new(*id, *content))
            .collect(),
    )
    .expect("valid prompt")
}

fn system_messages(messages: &[AgentMessage]) -> Vec<ConfigurationUpdate> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::System { update, .. } => Some(update.clone()),
            _ => None,
        })
        .collect()
}

fn names(tools: &[ToolDeclaration]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

#[test]
fn the_first_request_declares_direct_tools_and_withholds_deferred_ones() {
    let provider = ScriptedProvider::new([ScriptedTurn::new().text("hello").stop()]);
    let agent = Agent::builder()
        .system_prompt(prompt(&[("base", "You are tea."), ("rules", "Be brief.")]))
        .tool(FixtureTool::new("read", ToolExposure::Direct))
        .tool(FixtureTool::new("late", ToolExposure::Deferred))
        .tool(FixtureTool::new("inner", ToolExposure::Composition))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("hi").expect("run starts").drive()).expect("run settles");

    let request = &provider.requests()[0];
    assert_eq!(request.system_prompt(), "You are tea.\n\nBe brief.");
    assert_eq!(names(&request.tools()), ["read"]);
    let messages = agent.snapshot().messages;
    assert!(matches!(messages[0], AgentMessage::User { .. }));
    assert!(matches!(messages[1], AgentMessage::System { .. }));
    // The initial configuration leads the in-place projection even though it
    // was recorded after the first prompt.
    let resolved = request.transcript.resolve(ConfigurationProjection::InPlace);
    assert_eq!(names(&resolved.leading.tools), ["read"]);
    assert!(
        resolved
            .messages
            .iter()
            .all(|message| !matches!(message, AgentMessage::System { .. }))
    );
}

#[test]
fn a_discovery_result_loads_only_authorized_deferred_tools_for_the_next_request() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-search", "search", "{}")
            .end_tool_use(),
        ScriptedTurn::new()
            .tool_call("call-late", "late", "{}")
            .end_tool_use(),
        ScriptedTurn::new().text("done").stop(),
    ]);
    let agent = Agent::builder()
        .system_prompt("base")
        .tool(FixtureTool::loading(
            "search",
            &["late", "ghost", "search", "inner"],
        ))
        .tool(FixtureTool::new("late", ToolExposure::Deferred))
        .tool(FixtureTool::new("inner", ToolExposure::Composition))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("find it").expect("run starts").drive())
        .expect("run settles");

    let requests = provider.requests();
    assert_eq!(names(&requests[0].tools()), ["search"]);
    // Discovery exposed the already-authorized deferred tool; an unknown name,
    // an already-direct tool, and a composition-only tool changed nothing.
    assert_eq!(names(&requests[1].tools()), ["search", "late"]);
    assert_eq!(names(&requests[2].tools()), ["search", "late"]);

    let messages = agent.snapshot().messages;
    let updates = system_messages(&messages);
    assert_eq!(updates.len(), 2);
    assert_eq!(names(&updates[1].tools_added), ["late"]);
    assert!(updates[1].tools_removed.is_empty());
    // The update follows the discovery result in conversation order.
    let result_index = messages
        .iter()
        .position(|message| matches!(message, AgentMessage::ToolResult { tool_name, .. } if tool_name == "search"))
        .expect("search result");
    assert!(matches!(messages[result_index + 1], AgentMessage::System { .. }));
    // The loaded tool really executed.
    assert!(messages.iter().any(|message| matches!(
        message,
        AgentMessage::ToolResult { tool_name, content, is_error: false, .. }
            if tool_name == "late" && content == "late ran"
    )));
}

#[test]
fn direct_calls_to_unloaded_deferred_or_composition_tools_are_refused() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new()
            .tool_call("call-late", "late", "{}")
            .tool_call("call-inner", "inner", "{}")
            .end_tool_use(),
        ScriptedTurn::new().text("ok").stop(),
    ]);
    let agent = Agent::builder()
        .tool(FixtureTool::new("late", ToolExposure::Deferred))
        .tool(FixtureTool::new("inner", ToolExposure::Composition))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("call them").expect("run starts").drive())
        .expect("run settles");
    let results = agent
        .snapshot()
        .messages
        .into_iter()
        .filter_map(|message| match message {
            AgentMessage::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } => Some((tool_name, content, is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|(_, content, is_error)| *is_error && !content.contains("ran")));
    assert!(results[0].1.contains("not loaded"));
    assert!(results[1].1.contains("composition script"));
}

#[test]
fn a_later_configuration_is_a_delta_and_stays_append_only_in_place() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new().text("one").stop(),
        ScriptedTurn::new().text("two").stop(),
    ]);
    let agent = Agent::builder()
        .system_prompt(prompt(&[("base", "base"), ("plugin:a", "old a")]))
        .tool(FixtureTool::new("read", ToolExposure::Direct))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("first").expect("run starts").drive())
        .expect("first run settles");
    let mut tools = ToolRegistry::default();
    tools.insert(FixtureTool::new("read", ToolExposure::Direct));
    tools.insert(FixtureTool::new("edit", ToolExposure::Direct));
    agent
        .replace_configuration(AgentConfiguration::new(
            prompt(&[("base", "base"), ("plugin:a", "new a")]),
            tools,
            Arc::new(NoHooks),
        ))
        .expect("idle replacement");
    smol::block_on(agent.start_prompt("second").expect("run starts").drive())
        .expect("second run settles");

    let messages = agent.snapshot().messages;
    let updates = system_messages(&messages);
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[1].sections.len(), 1);
    assert_eq!(updates[1].sections[0].id, "plugin:a");
    assert_eq!(names(&updates[1].tools_added), ["edit"]);

    let requests = provider.requests();
    assert_eq!(requests[1].system_prompt(), "base\n\nnew a");
    // In place, the second request only appends: its leading configuration
    // and earlier conversation bytes are unchanged.
    let in_place = PromptLayoutLedger::default();
    let _ = in_place.observe(&RequestLayout::new(
        &requests[0],
        ConfigurationProjection::InPlace,
    ));
    assert_eq!(
        in_place
            .observe(&RequestLayout::new(
                &requests[1],
                ConfigurationProjection::InPlace
            ))
            .continuity,
        PromptContinuity::ExactExtension
    );
    // Collapsed, the leading prompt changes and the ledger says so.
    let collapsed = PromptLayoutLedger::default();
    let _ = collapsed.observe(&RequestLayout::new(
        &requests[0],
        ConfigurationProjection::Collapsed,
    ));
    assert_eq!(
        collapsed
            .observe(&RequestLayout::new(
                &requests[1],
                ConfigurationProjection::Collapsed
            ))
            .continuity,
        PromptContinuity::DomainChanged
    );
}

#[test]
fn an_in_place_provider_capability_selects_the_measured_projection() {
    let capabilities = ModelCapabilities {
        configuration_updates: ConfigurationUpdateSupport {
            system_messages: true,
            tool_changes: true,
        },
        ..ModelCapabilities::default()
    };
    let provider = ScriptedProvider::with_capabilities(
        [
            ScriptedTurn::new()
                .tool_call("call-search", "search", "{}")
                .end_tool_use(),
            ScriptedTurn::new().text("done").stop(),
        ],
        capabilities,
    );
    let agent = Agent::builder()
        .system_prompt("base")
        .tool(FixtureTool::loading("search", &["late"]))
        .tool(FixtureTool::new("late", ToolExposure::Deferred))
        .model_provider(Arc::new(provider.clone()))
        .build();
    let run = agent.start_prompt("go").expect("run starts");
    smol::block_on(run.drive()).expect("run settles");
    let continuities = run
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::PromptLayoutObserved { measurement, .. } => {
                Some(measurement.continuity)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    // Loading a tool mid-run is an in-place append for this transport.
    assert_eq!(
        continuities,
        [
            PromptContinuity::FirstRequest,
            PromptContinuity::ExactExtension
        ]
    );
}

struct InjectingHooks;

impl HookSet for InjectingHooks {
    fn before_tool_call(&self, _call: &ToolCall) -> Result<BeforeToolCall, HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        Ok(AfterToolCall::default())
    }

    /// Drop the canonical configuration and inject one declaring a tool the
    /// run cannot execute.
    fn transform_context(
        &self,
        mut context: ContextEnvelope,
    ) -> Result<ContextEnvelope, HookError> {
        context
            .messages
            .retain(|message| !matches!(message, AgentMessage::System { .. }));
        context.messages.insert(
            0,
            AgentMessage::System {
                id: MessageId(900),
                update: ConfigurationUpdate {
                    tools_added: vec![ToolDeclaration::new(
                        "secret",
                        "not authorized",
                        JsonValue::object([("type", JsonValue::from("object"))]),
                    )],
                    ..ConfigurationUpdate::default()
                },
            },
        );
        Ok(context)
    }
}

#[test]
fn context_hooks_cannot_replace_or_widen_the_canonical_configuration() {
    let provider = ScriptedProvider::new([ScriptedTurn::new().text("ok").stop()]);
    let agent = Agent::builder()
        .system_prompt("base")
        .hooks(Arc::new(InjectingHooks))
        .tool(FixtureTool::new("read", ToolExposure::Direct))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("go").expect("run starts").drive()).expect("run settles");
    let request = &provider.requests()[0];
    assert_eq!(names(&request.tools()), ["read"]);
    assert_eq!(request.system_prompt(), "base");
}

/// Keep only the newest message and a summary, dropping configuration.
struct DroppingCompactor;

impl Compactor for DroppingCompactor {
    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        let last = context.messages.last().cloned().expect("source history");
        let next = context
            .messages
            .iter()
            .map(|message| message.id().0)
            .max()
            .unwrap_or(0)
            + 1;
        Box::pin(std::future::ready(Ok(CompactionResult::new(vec![
            AgentMessage::User {
                id: MessageId(next),
                content: "summary".into(),
            },
            last,
        ]))))
    }
}

#[test]
fn compaction_keeps_the_configuration_in_force_without_applying_later_changes() {
    let provider = ScriptedProvider::new([
        ScriptedTurn::new().text("one").stop(),
        ScriptedTurn::new().text("two").stop(),
    ]);
    let agent = Agent::builder()
        .system_prompt(prompt(&[("base", "base")]))
        .tool(FixtureTool::new("read", ToolExposure::Direct))
        .compactor(Arc::new(DroppingCompactor))
        .model_provider(Arc::new(provider.clone()))
        .build();
    smol::block_on(agent.start_prompt("first").expect("run starts").drive())
        .expect("first run settles");
    let before = EffectiveConfiguration::replay(&agent.snapshot().messages).expect("configured");
    smol::block_on(agent.start_compaction().expect("compaction starts").drive())
        .expect("compaction commits");

    let messages = agent.snapshot().messages;
    // The summary replaced the configuration message, so the replacement
    // gained one leading configuration equal to the one in force.
    assert!(matches!(messages[0], AgentMessage::System { .. }));
    let after = EffectiveConfiguration::replay(&messages).expect("configured");
    assert!(after.same_content(&before));
    tea_core::Agent::validate_messages(&messages).expect("valid replacement");

    smol::block_on(agent.start_prompt("second").expect("run starts").drive())
        .expect("second run settles");
    let request = provider.requests().pop().expect("second request");
    assert_eq!(names(&request.tools()), ["read"]);
    assert_eq!(request.system_prompt(), "base");
    // No redundant configuration was appended after compaction.
    assert_eq!(system_messages(&agent.snapshot().messages).len(), 1);
}

#[test]
fn thinking_streams_as_distinct_ordered_blocks_with_private_signatures() {
    let provider = ScriptedProvider::new([ScriptedTurn::new()
        .thinking("plan ")
        .thinking("carefully")
        .thinking_signature("fixture", "sig-1")
        .text("answer")
        .thinking("second thought")
        .text(" done")
        .stop()]);
    let agent = Agent::builder()
        .model_provider(Arc::new(provider.clone()))
        .build();
    let run = agent.start_prompt("think").expect("run starts");
    smol::block_on(run.drive()).expect("run settles");

    let Some(AgentMessage::Assistant { content, .. }) = agent.snapshot().messages.last().cloned()
    else {
        panic!("assistant reply");
    };
    assert_eq!(content.len(), 4);
    assert!(matches!(
        &content[0],
        AssistantContent::Thinking { text, signature: Some(signature) }
            if text == "plan carefully" && signature.payload() == "sig-1"
    ));
    assert_eq!(content[1], AssistantContent::text("answer"));
    assert_eq!(content[2], AssistantContent::thinking("second thought"));
    assert_eq!(content[3], AssistantContent::text(" done"));
    assert_eq!(tea_core::state::assistant_text(&content), "answer done");

    let deltas = run
        .events()
        .iter()
        .filter_map(|event| match &event.kind {
            AgentEventKind::MessageUpdate { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        deltas,
        [
            MessageDelta::Thinking("plan ".into()),
            MessageDelta::Thinking("carefully".into()),
            MessageDelta::Text("answer".into()),
            MessageDelta::Thinking("second thought".into()),
            MessageDelta::Text(" done".into()),
        ]
    );
    // The signature never appears in a debug rendering.
    assert!(!format!("{content:?}").contains("sig-1"));
}

#[test]
fn cancelling_during_thinking_never_turns_it_into_answer_text() {
    let provider = ScriptedProvider::new([ScriptedTurn::new()
        .thinking("private reasoning")
        .wait_for_cancellation()]);
    let agent = Agent::builder()
        .model_provider(Arc::new(provider.clone()))
        .build();
    let run = Arc::new(agent.start_prompt("think").expect("run starts"));
    let executor = smol::Executor::new();
    let driving = Arc::clone(&run);
    let task = executor.spawn(async move { driving.drive().await });
    while executor.try_tick() {}
    // The live preview holds no thinking as answer text.
    assert_eq!(agent.snapshot().partial_response, None);
    agent.abort();
    let result = smol::block_on(executor.run(task));
    assert!(result.is_err());
    for message in agent.snapshot().messages {
        if let AgentMessage::Assistant { content, .. } = message {
            assert!(!tea_core::state::assistant_text(&content).contains("private reasoning"));
        }
    }
}

#[test]
fn replayed_configuration_matches_the_tool_identity_of_a_result() {
    // A tool-result identity unrelated to configuration still pairs with its
    // call when configuration messages surround the batch.
    let call = ToolCallId::new("call-1").expect("id");
    let messages = vec![
        AgentMessage::User {
            id: MessageId(1),
            content: "go".into(),
        },
        AgentMessage::System {
            id: MessageId(2),
            update: ConfigurationUpdate::default(),
        },
        AgentMessage::Assistant {
            id: MessageId(3),
            content: Vec::new(),
            tool_calls: vec![tea_core::state::AgentToolCall {
                id: call.clone(),
                name: "read".into(),
                arguments: tea_core::state::SerializedJson::new("{}"),
            }],
            stop_reason: Some(tea_core::state::StopReason::ToolUse),
            error_message: None,
            opaque_context: Vec::new(),
            origin: None,
        },
        AgentMessage::ToolResult {
            id: MessageId(4),
            tool_call_id: call,
            tool_name: "read".into(),
            content: "ok".into(),
            details: None,
            usage: Box::new(None),
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        },
    ];
    Agent::validate_messages(&messages).expect("configuration is transparent to pairing");
    let _ = Agent::builder()
        .model_provider(Arc::new(ScriptedProvider::new([])) as Arc<dyn ModelProvider>)
        .build()
        .restore_messages(messages)
        .expect("restores");
}
