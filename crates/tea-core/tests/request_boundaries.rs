use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tea_core::Agent;
use tea_core::error::HookError;
use tea_core::hooks::{
    AfterToolCall, AgentLoopTurnUpdate, BeforeToolCall, ContextEnvelope, HookSet,
};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{
    AgentToolCall, ModelDescriptor, SerializedJson, StopReason, ThinkingLevel, ToolCallId,
};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};

#[derive(Debug)]
struct RecordingProvider {
    streams: Mutex<VecDeque<ModelStream>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl RecordingProvider {
    fn new(streams: impl IntoIterator<Item = ModelStream>) -> Self {
        Self {
            streams: Mutex::new(streams.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .expect("provider request mutex")
            .clone()
    }
}

impl ModelProvider for RecordingProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests
            .lock()
            .expect("provider request mutex")
            .push(request);
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
struct EchoTool {
    schema: tea_protocol::JsonValue,
}

impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo a value."
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
            content: "echoed".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

#[derive(Debug)]
struct RequestBoundaryHooks;

impl HookSet for RequestBoundaryHooks {
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

    fn transform_context(
        &self,
        mut context: ContextEnvelope,
    ) -> Result<ContextEnvelope, HookError> {
        context
            .host_messages
            .push(SerializedJson::new("transformed"));
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        let host_messages = context
            .host_messages
            .iter()
            .map(SerializedJson::as_str)
            .collect::<Vec<_>>()
            .join("|");
        Ok(format!("converted:{host_messages}"))
    }

    fn prepare_next_turn(
        &self,
        _context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, HookError> {
        Ok(AgentLoopTurnUpdate {
            context: Some(ContextEnvelope {
                version: 1,
                messages: Vec::new(),
                host_messages: vec![SerializedJson::new("replacement")],
            }),
            model: Some(ModelDescriptor {
                provider: "next-provider".into(),
                model: "next-model".into(),
                revision: Some("next-revision".into()),
            }),
            thinking_level: Some(ThinkingLevel::High),
        })
    }
}

#[test]
fn public_request_boundaries_reach_sequential_provider_requests() {
    smol::block_on(async {
        let call_id = ToolCallId::new("call_echo").expect("non-empty tool-call ID");
        let provider = Arc::new(RecordingProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: call_id,
                        name: "echo".into(),
                        arguments: SerializedJson::new(r#"{"value":"hello"}"#),
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
        ]));
        let agent = Agent::builder()
            .model(ModelDescriptor {
                provider: "initial-provider".into(),
                model: "initial-model".into(),
                revision: Some("initial-revision".into()),
            })
            .thinking_level(ThinkingLevel::Low)
            .host_message(SerializedJson::new("host-only"))
            .hooks(Arc::new(RequestBoundaryHooks))
            .model_provider(Arc::clone(&provider) as Arc<dyn ModelProvider>)
            .tool(Arc::new(EchoTool {
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                    .expect("valid fixture schema"),
            }))
            .build();

        agent
            .start_prompt("invoke echo")
            .expect("start prompt")
            .drive()
            .await
            .expect("scripted run succeeds");

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].context, "converted:host-only|transformed");
        assert_eq!(
            requests[0].model,
            Some(ModelDescriptor {
                provider: "initial-provider".into(),
                model: "initial-model".into(),
                revision: Some("initial-revision".into()),
            })
        );
        assert_eq!(requests[0].thinking_level, ThinkingLevel::Low);

        assert_eq!(requests[1].context, "converted:replacement|transformed");
        assert_eq!(
            requests[1].model,
            Some(ModelDescriptor {
                provider: "next-provider".into(),
                model: "next-model".into(),
                revision: Some("next-revision".into()),
            })
        );
        assert_eq!(requests[1].thinking_level, ThinkingLevel::High);
    });
}
