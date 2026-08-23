//! Dependency-free integration matrices for public v1 contracts.
//!
//! These tests intentionally live outside `tea-core`'s unit-test module.  They exercise
//! only the public provider, tool, lifecycle, and profile seams, so a private implementation
//! helper cannot make a contract appear covered accidentally.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tea_core::agent::Agent;
use tea_core::coding::{PiDefaultCodingProfile, ProfileSpec};
use tea_core::event::AgentEventKind;
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentMessage, AgentPhase, AgentToolCall, RunPhase, StopReason, ToolCallId};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolDefinition, ToolExecutionMode,
    ToolFuture, ToolRegistry, ToolUpdateSink,
};

fn schema() -> tea_protocol::JsonValue {
    tea_protocol::JsonValue::parse(r#"{"type":"object"}"#).expect("fixture schema is valid JSON")
}

fn tool_call(id: &str, name: &str) -> AgentToolCall {
    AgentToolCall {
        id: ToolCallId::new(id).expect("fixture call IDs are non-empty"),
        name: name.to_owned(),
        arguments: tea_core::state::SerializedJson::new("{}"),
    }
}

fn result_for(call: ToolCall) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id,
        content: format!("completed {}", call.name),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    }
}

/// A finite provider is sufficient here: caller-owned providers are the public scheduler seam.
struct ScriptedProvider {
    streams: Mutex<VecDeque<Result<ModelStream, tea_core::error::SchedulerError>>>,
}

impl ScriptedProvider {
    fn new(
        streams: impl IntoIterator<Item = Result<ModelStream, tea_core::error::SchedulerError>>,
    ) -> Self {
        Self {
            streams: Mutex::new(streams.into_iter().collect()),
        }
    }
}

impl ModelProvider for ScriptedProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = self
            .streams
            .lock()
            .expect("provider stream mutex")
            .pop_front()
            .expect("fixture supplied enough model streams");
        Box::pin(std::future::ready(
            stream.map(|stream| Box::new(stream) as _),
        ))
    }
}

struct YieldingResult {
    remaining: u8,
    result: Option<Result<AgentToolResult, tea_core::error::ToolError>>,
}

impl Future for YieldingResult {
    type Output = Result<AgentToolResult, tea_core::error::ToolError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.remaining > 0 {
            self.remaining -= 1;
            context.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(
            self.result
                .take()
                .expect("fixture future polled after completion"),
        )
    }
}

/// Parallel tools whose deterministic yield count creates a completion-order permutation.
struct DelayedTool {
    name: &'static str,
    yields: u8,
    description: &'static str,
    schema: tea_protocol::JsonValue,
}

impl AgentTool for DelayedTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        &self.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(YieldingResult {
            remaining: self.yields,
            result: Some(Ok(result_for(call))),
        })
    }
}

fn tool_turn_stream() -> ModelStream {
    ModelStream {
        events: vec![
            ModelStreamEvent::ToolCall(tool_call("call_a", "a")),
            ModelStreamEvent::ToolCall(tool_call("call_b", "b")),
            ModelStreamEvent::ToolCall(tool_call("call_c", "c")),
            ModelStreamEvent::End(StopReason::ToolUse),
        ],
    }
}

#[test]
fn completion_permutation_matrix_preserves_source_order_and_emits_each_end_once() {
    smol::block_on(async {
        // Every permutation gives a distinct delay assignment while keeping the assistant
        // source order fixed. This is intentionally larger than a single happy-path fixture.
        for yields in [
            [0_u8, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let provider = Arc::new(ScriptedProvider::new([
                Ok(tool_turn_stream()),
                Ok(ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("done".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                }),
            ]));
            let shared_schema = schema();
            let agent = Agent::builder()
                .model_provider(provider)
                .tool(Arc::new(DelayedTool {
                    name: "a",
                    yields: yields[0],
                    description: "a",
                    schema: shared_schema.clone(),
                }))
                .tool(Arc::new(DelayedTool {
                    name: "b",
                    yields: yields[1],
                    description: "b",
                    schema: shared_schema.clone(),
                }))
                .tool(Arc::new(DelayedTool {
                    name: "c",
                    yields: yields[2],
                    description: "c",
                    schema: shared_schema,
                }))
                .build();
            let run = agent
                .start_prompt("exercise completion permutation matrix")
                .expect("run starts");
            run.drive().await.expect("tool matrix run succeeds");

            let source_result_ids = agent
                .snapshot()
                .messages
                .into_iter()
                .filter_map(|message| match message {
                    AgentMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(source_result_ids, ["call_a", "call_b", "call_c"]);

            let completion_ids = run
                .events()
                .into_iter()
                .filter_map(|event| match event.kind {
                    AgentEventKind::ToolExecutionEnd { tool_call_id, .. } => {
                        Some(tool_call_id.to_string())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            // The public contract exposes completion order as the order observed by the
            // caller, not as a promise about a particular executor's poll interleaving.  The
            // matrix varies all delay assignments and verifies the stronger invariant: every
            // call settles once, while only the durable context is source ordered.
            assert_eq!(completion_ids.len(), 3);
            assert_eq!(
                completion_ids
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                3,
                "each source tool must settle exactly once",
            );
            assert_eq!(run.snapshot().phase, RunPhase::Succeeded);
            assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
        }
    });
}

fn normal_stream(text: &str) -> Result<ModelStream, tea_core::error::SchedulerError> {
    Ok(ModelStream {
        events: vec![
            ModelStreamEvent::TextDelta(text.to_owned()),
            ModelStreamEvent::End(StopReason::Stop),
        ],
    })
}

#[test]
fn terminal_cleanup_matrix_allows_reuse_after_drop_success_and_model_error() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            normal_stream("first success"),
            Ok(ModelStream {
                events: vec![ModelStreamEvent::Error {
                    message: "fixture model failure".into(),
                }],
            }),
            normal_stream("recovered success"),
        ]));
        let agent = Agent::builder().model_provider(provider).build();

        // An un-driven handle must not leave ownership stuck in Running.
        let dropped = agent
            .start_prompt("drop before driving")
            .expect("run starts");
        let dropped_id = dropped.id();
        drop(dropped);
        assert_clean_idle(&agent);

        let first = agent
            .start_prompt("normal terminal run")
            .expect("reuse after drop");
        first.drive().await.expect("normal run succeeds");
        assert_clean_idle(&agent);

        let failed = agent
            .start_prompt("model failure run")
            .expect("failure run starts");
        let error = failed.drive().await.expect_err("model error is returned");
        assert_eq!(
            error,
            tea_core::error::CoreError::ModelError {
                message: "fixture model failure".into()
            }
        );
        assert_eq!(failed.snapshot().phase, RunPhase::Failed);
        assert_clean_idle(&agent);

        let recovered = agent
            .start_prompt("reuse after model failure")
            .expect("agent remains reusable");
        recovered.drive().await.expect("recovery run succeeds");
        assert!(recovered.id().0 > dropped_id.0);
        assert_clean_idle(&agent);
        assert_eq!(
            agent.snapshot().messages.len(),
            7,
            "drop adds its user message; three driven runs add two each"
        );
    });
}

fn assert_clean_idle(agent: &Agent) {
    let snapshot = agent.snapshot();
    assert_eq!(snapshot.phase, AgentPhase::Idle);
    assert!(snapshot.partial_response.is_none());
    assert!(!snapshot.is_streaming);
    assert!(snapshot.pending_tool_calls.is_empty());
}

#[test]
fn lifecycle_balance_matrix_covers_success_tool_turn_and_model_error() {
    smol::block_on(async {
        let cases = [
            (vec![normal_stream("text")], false),
            (
                vec![Ok(tool_turn_stream()), normal_stream("tool complete")],
                true,
            ),
            (
                vec![Ok(ModelStream {
                    events: vec![ModelStreamEvent::Error {
                        message: "bad model".into(),
                    }],
                })],
                false,
            ),
        ];
        for (streams, has_tools) in cases {
            let provider = Arc::new(ScriptedProvider::new(streams));
            let mut builder = Agent::builder().model_provider(provider);
            if has_tools {
                let shared_schema = schema();
                for (name, call_id) in [("a", "call_a"), ("b", "call_b"), ("c", "call_c")] {
                    let _ = call_id;
                    builder = builder.tool(Arc::new(DelayedTool {
                        name,
                        yields: 0,
                        description: name,
                        schema: shared_schema.clone(),
                    }));
                }
            }
            let agent = builder.build();
            let subscription = agent.subscribe_nonblocking(
                std::num::NonZeroUsize::new(128).expect("positive subscription capacity"),
            );
            let run = agent
                .start_prompt("balance lifecycle events")
                .expect("run starts");
            let _ = run.drive().await;
            assert_lifecycle_balance(&run.events());

            let mut retained = Vec::new();
            while let Ok(event) = subscription.try_recv() {
                retained.push(event);
            }
            assert_eq!(subscription.dropped_events(), 0);
            assert_eq!(retained, run.events());
            assert_clean_idle(&agent);
        }
    });
}

fn assert_lifecycle_balance(events: &[tea_core::event::AgentEvent]) {
    assert!(!events.is_empty());
    let run_id = events[0].run_id;
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.run_id, run_id);
        assert_eq!(event.sequence.0, (index + 1) as u64);
    }
    assert!(matches!(
        events.first().map(|event| &event.kind),
        Some(AgentEventKind::AgentStart)
    ));
    assert!(matches!(
        events.last().map(|event| &event.kind),
        Some(AgentEventKind::AgentEnd { .. })
    ));

    let mut turns = 0_u32;
    let mut messages = 0_u32;
    let mut tool_starts = BTreeMap::<ToolCallId, u32>::new();
    let mut tool_ends = BTreeMap::<ToolCallId, u32>::new();
    let mut agent_starts = 0_u32;
    let mut agent_ends = 0_u32;
    for event in events {
        match &event.kind {
            AgentEventKind::AgentStart => agent_starts += 1,
            AgentEventKind::AgentEnd { .. } => agent_ends += 1,
            AgentEventKind::TurnStart { .. } => turns += 1,
            AgentEventKind::TurnEnd { .. } => {
                assert!(turns > 0, "turn end cannot precede a turn start");
                turns -= 1;
            }
            AgentEventKind::MessageStart { .. } => messages += 1,
            AgentEventKind::MessageEnd { .. } => {
                assert!(messages > 0, "message end cannot precede a message start");
                messages -= 1;
            }
            AgentEventKind::ToolExecutionStart { tool_call_id, .. } => {
                *tool_starts.entry(tool_call_id.clone()).or_default() += 1;
            }
            AgentEventKind::ToolExecutionEnd { tool_call_id, .. } => {
                *tool_ends.entry(tool_call_id.clone()).or_default() += 1;
            }
            AgentEventKind::MessageUpdate { .. }
            | AgentEventKind::ToolExecutionUpdate { .. }
            | AgentEventKind::ModelTurnUsage { .. }
            | AgentEventKind::CompactionStart { .. }
            | AgentEventKind::CompactionResult { .. }
            | AgentEventKind::CompactionEnd { .. }
            | AgentEventKind::AutomaticCompactionStart { .. }
            | AgentEventKind::AutomaticCompactionEnd { .. }
            | AgentEventKind::ContextEstimate { .. }
            | AgentEventKind::ProviderRequestSkipped { .. }
            | AgentEventKind::ToolFailureObserved { .. }
            | AgentEventKind::CompactionLifecycle { .. }
            | AgentEventKind::ProviderRequestObserved { .. } => {}
        }
    }
    assert_eq!(agent_starts, 1);
    assert_eq!(agent_ends, 1);
    assert_eq!(turns, 0);
    assert_eq!(messages, 0);
    assert_eq!(tool_starts, tool_ends);
    assert!(tool_starts.values().all(|count| *count == 1));
}

struct NamedTool {
    name: &'static str,
    description: &'static str,
    schema: tea_protocol::JsonValue,
}

impl AgentTool for NamedTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
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
        Box::pin(std::future::ready(Ok(result_for(call))))
    }
}

#[test]
fn profile_composition_matrix_keeps_prompt_explicit_while_tools_replace_or_remove() {
    let alpha = Arc::new(NamedTool {
        name: "alpha",
        description: "original alpha",
        schema: schema(),
    });
    let beta = Arc::new(NamedTool {
        name: "beta",
        description: "original beta",
        schema: schema(),
    });
    let profile = PiDefaultCodingProfile::from_spec(ProfileSpec {
        system_prompt: "profile prompt".into(),
        tools: vec![
            ToolDefinition::from_tool(alpha.as_ref()),
            ToolDefinition::from_tool(beta.as_ref()),
        ],
        tool_guidance: vec!["profile guidance".into()],
    })
    .expect("fixture profile is valid");
    let mut registry = ToolRegistry::default();
    registry.insert(alpha);
    registry.insert(beta);
    profile
        .validate_registry(&registry)
        .expect("registry satisfies profile");

    let replacement = Arc::new(NamedTool {
        name: "alpha",
        description: "replacement alpha",
        schema: schema(),
    });
    let replaced = Agent::builder()
        .system_prompt(profile.system_prompt())
        .tools(registry.clone())
        .tool(replacement)
        .build();
    assert_eq!(
        replaced.snapshot().system_prompt,
        "profile prompt\nprofile guidance"
    );
    assert_eq!(
        replaced
            .tool_definitions()
            .into_iter()
            .map(|tool| (tool.name, tool.description))
            .collect::<Vec<_>>(),
        [
            ("alpha".into(), "replacement alpha".into()),
            ("beta".into(), "original beta".into())
        ]
    );

    let removed = Agent::builder()
        .system_prompt(profile.system_prompt())
        .tools(registry)
        .remove_tool("beta")
        .build();
    assert_eq!(
        removed.snapshot().system_prompt,
        "profile prompt\nprofile guidance"
    );
    assert_eq!(
        removed
            .tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        ["alpha"]
    );
}
