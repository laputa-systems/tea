use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tea_core::event::AgentEventKind;
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolExecutionMode, ToolFuture, ToolUpdate,
    ToolUpdateSink,
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

#[derive(Debug, Default)]
struct Gate {
    ready: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn release(&self) {
        self.ready.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().expect("gate waker mutex").take() {
            waker.wake();
        }
    }
}

#[derive(Debug)]
struct UpdateThenGateTool {
    name: &'static str,
    gate: Arc<Gate>,
    update: Option<&'static str>,
    schema: tea_protocol::JsonValue,
}

#[derive(Debug)]
struct GatedToolFuture {
    call_id: ToolCallId,
    name: &'static str,
    gate: Arc<Gate>,
    updates: ToolUpdateSink,
    update: Option<&'static str>,
    emitted: bool,
}

impl Future for GatedToolFuture {
    type Output = Result<AgentToolResult, tea_core::error::ToolError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.emitted {
            if let Some(update) = self.update {
                self.updates.emit(ToolUpdate {
                    content: update.into(),
                    details: None,
                });
            }
            self.emitted = true;
        }
        if self.gate.ready.load(Ordering::Acquire) {
            return Poll::Ready(Ok(AgentToolResult {
                tool_call_id: self.call_id.clone(),
                content: self.name.into(),
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                terminate: false,
                is_error: false,
                failure: None,
            }));
        }
        *self.gate.waker.lock().expect("gate waker mutex") = Some(context.waker().clone());
        Poll::Pending
    }
}

impl AgentTool for UpdateThenGateTool {
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
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(GatedToolFuture {
            call_id: call.id,
            name: self.name,
            gate: Arc::clone(&self.gate),
            updates,
            update: self.update,
            emitted: false,
        })
    }
}

fn model_with_parallel_calls() -> ModelStream {
    ModelStream {
        events: vec![
            ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new("call_updates").expect("non-empty ID"),
                name: "updates".into(),
                arguments: SerializedJson::new("{}"),
            }),
            ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new("call_waiting").expect("non-empty ID"),
                name: "waiting".into(),
                arguments: SerializedJson::new("{}"),
            }),
            ModelStreamEvent::End(StopReason::ToolUse),
        ],
    }
}

#[test]
fn callback_update_is_emitted_while_parallel_tools_are_still_pending() {
    let updates_gate = Arc::new(Gate::default());
    let waiting_gate = Arc::new(Gate::default());
    let schema = tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
        .expect("fixture schema is valid JSON");
    let agent = Agent::builder()
        .model_provider(Arc::new(ScriptedProvider::new([
            model_with_parallel_calls(),
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
        ])))
        .tool(Arc::new(UpdateThenGateTool {
            name: "updates",
            gate: Arc::clone(&updates_gate),
            update: Some("started"),
            schema: schema.clone(),
        }))
        .tool(Arc::new(UpdateThenGateTool {
            name: "waiting",
            gate: Arc::clone(&waiting_gate),
            update: None,
            schema,
        }))
        .build();
    let run = agent
        .start_prompt("exercise callback timing")
        .expect("run starts");
    let mut drive = Box::pin(run.drive());
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);

    assert!(matches!(drive.as_mut().poll(&mut context), Poll::Pending));
    let events = run.events();
    let update_index = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                AgentEventKind::ToolExecutionUpdate {
                    tool_call_id,
                    update,
                    ..
                } if tool_call_id.as_str() == "call_updates" && update.content == "started"
            )
        })
        .expect("callback update should be emitted before either tool settles");
    assert!(!events
        .iter()
        .any(|event| { matches!(event.kind, AgentEventKind::ToolExecutionEnd { .. }) }));
    assert!(update_index > 0);

    updates_gate.release();
    waiting_gate.release();
    let mut completed = false;
    for _ in 0..8 {
        if matches!(drive.as_mut().poll(&mut context), Poll::Ready(Ok(()))) {
            completed = true;
            break;
        }
    }
    assert!(completed, "released tools should let the run settle");
}
