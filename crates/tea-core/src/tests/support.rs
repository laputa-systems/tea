use crate::agent::Agent;
use crate::error::CoreError;
use crate::event::{AgentEvent, AgentEventKind, EventObserver, ObserverFuture};
use crate::hooks::{
    AfterToolCall, AgentLoopTurnUpdate, BeforeToolCall, ContextEnvelope, HookFuture, HookSet,
    Replacement,
};
use crate::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use crate::state::{ModelDescriptor, SerializedJson, StopReason, ThinkingLevel, ToolCallId, Usage};
use crate::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolExecutionMode, ToolFuture,
    ToolUpdateSink,
};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

#[derive(Debug)]
pub(super) struct TextOnlyProvider;

impl ModelProvider for TextOnlyProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        if cancellation.is_cancelled() {
            return Box::pin(std::future::ready(Ok(Box::new(ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Aborted)],
            }) as _)));
        }
        Box::pin(std::future::ready(Ok(Box::new(ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("fixture capture succeeded.".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }) as _)))
    }
}

#[derive(Debug)]
pub(super) struct FailingProvider;

impl ModelProvider for FailingProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        Box::pin(std::future::ready(Err(
            crate::error::SchedulerError::UnknownToolCall {
                tool_call_id: ToolCallId::new("provider-call-99")
                    .expect("non-empty test tool-call ID"),
            },
        )))
    }
}

#[derive(Debug)]
pub(super) struct ScriptedProvider {
    streams: Mutex<VecDeque<ModelStream>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedProvider {
    pub(super) fn new(streams: impl IntoIterator<Item = ModelStream>) -> Self {
        Self {
            streams: Mutex::new(streams.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("test request mutex").clone()
    }
}

impl ModelProvider for ScriptedProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests
            .lock()
            .expect("test request mutex")
            .push(request);
        let stream = self
            .streams
            .lock()
            .expect("test stream mutex")
            .pop_front()
            .ok_or_else(|| crate::error::SchedulerError::UnknownToolCall {
                tool_call_id: ToolCallId::new("unexpected-model-request")
                    .expect("test tool-call ID is non-empty"),
            });
        Box::pin(std::future::ready(
            stream.map(|stream| Box::new(stream) as _),
        ))
    }
}

#[derive(Debug)]
pub(super) struct EchoTool {
    pub(super) calls: Arc<Mutex<Vec<ToolCall>>>,
    pub(super) schema: tea_protocol::JsonValue,
}

impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes its JSON input."
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
        self.calls
            .lock()
            .expect("test tool mutex")
            .push(call.clone());
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "echoed: hello".into(),
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
pub(super) struct YieldOnceToolFuture {
    result: Option<Result<AgentToolResult, crate::error::ToolError>>,
    yielded: bool,
}

#[derive(Debug)]
pub(super) struct YieldCountToolFuture {
    result: Option<Result<AgentToolResult, crate::error::ToolError>>,
    remaining_yields: u8,
}

impl Future for YieldCountToolFuture {
    type Output = Result<AgentToolResult, crate::error::ToolError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.remaining_yields > 0 {
            self.remaining_yields -= 1;
            context.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(
            self.result
                .take()
                .expect("tool future polled after completion"),
        )
    }
}

impl Future for YieldOnceToolFuture {
    type Output = Result<AgentToolResult, crate::error::ToolError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.yielded {
            self.yielded = true;
            context.waker().wake_by_ref();
            return Poll::Pending;
        }
        Poll::Ready(
            self.result
                .take()
                .expect("tool future polled after completion"),
        )
    }
}

#[derive(Debug)]
pub(super) struct ParallelFixtureTool {
    pub(super) name: &'static str,
    pub(super) execution_mode: ToolExecutionMode,
    pub(super) yield_once: bool,
    pub(super) update: Option<&'static str>,
    pub(super) schema: tea_protocol::JsonValue,
}

impl AgentTool for ParallelFixtureTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.name
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        &self.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.execution_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        if let Some(update) = self.update {
            updates.emit(crate::tool::ToolUpdate {
                content: update.into(),
                details: None,
            });
        }
        let result = AgentToolResult {
            tool_call_id: call.id,
            content: self.name.into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        };
        if self.yield_once {
            Box::pin(YieldOnceToolFuture {
                result: Some(Ok(result)),
                yielded: false,
            })
        } else {
            Box::pin(std::future::ready(Ok(result)))
        }
    }
}

#[derive(Debug)]
pub(super) struct VariableDelayTool {
    pub(super) name: &'static str,
    pub(super) yields: u8,
    pub(super) schema: tea_protocol::JsonValue,
}

impl AgentTool for VariableDelayTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.name
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
        Box::pin(YieldCountToolFuture {
            result: Some(Ok(AgentToolResult {
                tool_call_id: call.id,
                content: self.name.into(),
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: false,
                failure: None,
            })),
            remaining_yields: self.yields,
        })
    }
}

#[derive(Debug)]
pub(super) struct RecordingObserver {
    pub(super) events: Arc<std::sync::Mutex<Vec<AgentEvent>>>,
}

#[derive(Debug)]
pub(super) struct ReplacementContextHooks;

impl HookSet for ReplacementContextHooks {
    fn before_tool_call(
        &self,
        _call: &ToolCall,
    ) -> Result<BeforeToolCall, crate::error::HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, crate::error::HookError> {
        Ok(AfterToolCall::default())
    }

    fn transform_context(
        &self,
        context: ContextEnvelope,
    ) -> Result<ContextEnvelope, crate::error::HookError> {
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, crate::error::HookError> {
        Ok(context
            .host_messages
            .last()
            .map(|message| message.as_str().to_owned())
            .unwrap_or_else(|| "state-context".into()))
    }

    fn prepare_next_turn(
        &self,
        mut context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, crate::error::HookError> {
        context
            .host_messages
            .push(crate::state::SerializedJson::new("replacement-context"));
        Ok(AgentLoopTurnUpdate {
            context: Some(context),
            model: Some(ModelDescriptor {
                provider: "replacement-provider".into(),
                model: "replacement-model".into(),
                revision: Some("replacement-revision".into()),
            }),
            thinking_level: Some(ThinkingLevel::High),
        })
    }
}

impl EventObserver for RecordingObserver {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: CancellationToken,
    ) -> ObserverFuture<'a> {
        self.events
            .lock()
            .expect("test observer mutex")
            .push(event.clone());
        Box::pin(std::future::ready(Ok(())))
    }
}

#[derive(Debug)]
pub(super) struct AbortOnAgentStartObserver {
    pub(super) agent: Arc<Mutex<Option<Agent>>>,
}

#[derive(Debug)]
pub(super) struct SubscribeOnAgentStartObserver {
    pub(super) agent: Arc<Mutex<Option<Agent>>>,
    pub(super) observed: Arc<Mutex<Vec<AgentEvent>>>,
    pub(super) subscriptions: Arc<Mutex<Vec<crate::agent::ObserverSubscription>>>,
    pub(super) subscribed: AtomicBool,
}

#[derive(Debug)]
pub(super) struct FailingObserver;

impl EventObserver for FailingObserver {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: CancellationToken,
    ) -> ObserverFuture<'a> {
        let result = if matches!(event.kind, AgentEventKind::AgentStart) {
            Err(CoreError::Hook(crate::error::HookError::new(
                "observer",
                "fixture observer failure",
            )))
        } else {
            Ok(())
        };
        Box::pin(std::future::ready(result))
    }
}

impl EventObserver for SubscribeOnAgentStartObserver {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: CancellationToken,
    ) -> ObserverFuture<'a> {
        if matches!(event.kind, AgentEventKind::AgentStart)
            && !self.subscribed.swap(true, Ordering::SeqCst)
        {
            let agent = self
                .agent
                .lock()
                .expect("test agent mutex")
                .clone()
                .expect("agent is installed before the run starts");
            let subscription = agent.subscribe(Arc::new(RecordingObserver {
                events: Arc::clone(&self.observed),
            }));
            self.subscriptions
                .lock()
                .expect("test subscription mutex")
                .push(subscription);
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

impl EventObserver for AbortOnAgentStartObserver {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: CancellationToken,
    ) -> ObserverFuture<'a> {
        if matches!(event.kind, AgentEventKind::AgentStart)
            && let Some(agent) = self.agent.lock().expect("test agent mutex").clone()
        {
            agent.abort();
        }
        Box::pin(std::future::ready(Ok(())))
    }
}

#[derive(Debug)]
pub(super) struct AbortDuringBeforeToolHook {
    pub(super) agent: Arc<Mutex<Option<Agent>>>,
}

impl HookSet for AbortDuringBeforeToolHook {
    fn before_tool_call(
        &self,
        _call: &ToolCall,
    ) -> Result<BeforeToolCall, crate::error::HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, crate::error::HookError> {
        Ok(AfterToolCall::default())
    }

    fn transform_context(
        &self,
        context: ContextEnvelope,
    ) -> Result<ContextEnvelope, crate::error::HookError> {
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, crate::error::HookError> {
        Ok(context
            .messages
            .into_iter()
            .map(|message| format!("{message:?}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn before_tool_call_async<'a>(
        &'a self,
        _call: &'a ToolCall,
        _context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, BeforeToolCall> {
        if let Some(agent) = self.agent.lock().expect("test agent mutex").clone() {
            agent.abort();
        }
        assert!(cancellation.is_cancelled());
        Box::pin(std::future::ready(Ok(BeforeToolCall::Allow)))
    }
}

#[derive(Debug)]
pub(super) struct MetadataAfterToolHook;

impl HookSet for MetadataAfterToolHook {
    fn before_tool_call(
        &self,
        _call: &ToolCall,
    ) -> Result<BeforeToolCall, crate::error::HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, crate::error::HookError> {
        Ok(AfterToolCall {
            details: Replacement::Replace(Some(SerializedJson::new(r#"{"source":"hook"}"#))),
            usage: Replacement::Replace(Usage {
                input_tokens: Some(3),
                output_tokens: Some(5),
                reasoning_tokens: Some(2),
                ..Usage::default()
            }),
            added_tool_names: Replacement::Replace(vec!["later-tool".into()]),
            ..AfterToolCall::default()
        })
    }

    fn transform_context(
        &self,
        context: ContextEnvelope,
    ) -> Result<ContextEnvelope, crate::error::HookError> {
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, crate::error::HookError> {
        Ok(context
            .messages
            .into_iter()
            .map(|message| format!("{message:?}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}
