use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tea_core::event::AgentEventKind;
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentMessage, AgentToolCall, SerializedJson, StopReason, ToolCallId};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};
use tea_core::Agent;

#[derive(Debug)]
struct ScriptedProvider {
    streams: Mutex<VecDeque<ModelStream>>,
}

impl ScriptedProvider {
    fn new(streams: impl IntoIterator<Item = ModelStream>) -> Self {
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
            .expect("fixture supplied too few model streams");
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[derive(Debug)]
struct TerminatingTool {
    schema: tea_protocol::JsonValue,
}

impl AgentTool for TerminatingTool {
    fn name(&self) -> &str {
        "finish"
    }

    fn description(&self) -> &str {
        "Finish the current tool batch."
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
            content: "finished".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: true,
            is_error: false,
            failure: None,
        })))
    }
}

fn agent_end_messages(agent_run: &tea_core::RunHandle) -> Vec<AgentMessage> {
    agent_run
        .events()
        .into_iter()
        .find_map(|event| match event.kind {
            AgentEventKind::AgentEnd { messages } => Some(messages),
            _ => None,
        })
        .expect("run emits one agent_end event")
}

#[test]
fn continuation_agent_end_contains_only_messages_created_by_continuation() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("call_finish").expect("non-empty tool-call ID"),
                        name: "finish".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("continued response".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(TerminatingTool {
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                    .expect("valid fixture schema"),
            }))
            .build();

        let first_run = agent.start_prompt("initial prompt").expect("start prompt");
        first_run.drive().await.expect("first run succeeds");
        assert!(matches!(
            agent.snapshot().messages.last(),
            Some(AgentMessage::ToolResult { .. })
        ));

        let first_messages = agent_end_messages(&first_run);
        assert_eq!(first_messages.len(), 3);
        assert!(
            matches!(first_messages[0], AgentMessage::User { ref content, .. } if content == "initial prompt")
        );

        let continuation = agent.start_continue().expect("start continuation");
        continuation.drive().await.expect("continuation succeeds");

        let continuation_messages = agent_end_messages(&continuation);
        assert_eq!(continuation_messages.len(), 1);
        assert!(matches!(
            &continuation_messages[0],
            AgentMessage::Assistant { content, .. } if content == "continued response"
        ));
        assert!(!continuation_messages.iter().any(|message| matches!(
            message,
            AgentMessage::User { content, .. } if content == "initial prompt"
        )));
    });
}
