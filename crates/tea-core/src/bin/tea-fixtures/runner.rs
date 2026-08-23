use std::collections::VecDeque;
use std::env;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use tea_core::event::{AgentEvent, AgentEventKind, EventObserver, ObserverFuture};
use tea_core::hooks::{
    AfterToolCall, AgentLoopTurnUpdate, BeforeToolCall, ContextEnvelope, HookFuture, HookSet,
    Replacement,
};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentMessage, AgentPhase, ModelDescriptor, SerializedJson, ToolCallId};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};
use tea_core::error::CoreError;
use tea_core::Agent;
use tea_protocol::JsonValue;

use super::fixture::{
    Fixture, FixtureAction, FixtureActiveQueueArrival, FixtureAfterToolReplace,
    FixtureBeforeToolPolicy, FixtureContextHooks, FixtureModelStream, FixtureToolResponse,
};
use super::normalize::{
    normalize_event, normalize_message, normalize_quality_request, normalize_request,
    stop_reason_name, thinking_level_name,
};

#[derive(Debug)]
struct FixtureHooks {
    before_tool_policy: Option<FixtureBeforeToolPolicy>,
    after_tool_replace: Option<FixtureAfterToolReplace>,
    context_hooks: Option<FixtureContextHooks>,
    should_stop_after_turn: bool,
    /// The quality adapter enables this explicitly to retain the logical
    /// pre-conversion request envelope. Normal parity output remains byte-for-
    /// byte stable unless `TEA_QUALITY_CAPTURE=1` is set.
    request_contexts: Option<Arc<Mutex<Vec<ContextEnvelope>>>>,
}

/// A deterministic, explicitly held `agent_end` observer used to prove that terminal
/// settlement waits for listeners. It has no timers or background executor authority.
#[derive(Debug, Default)]
struct FixtureObserverGate {
    reached: AtomicBool,
    released: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl FixtureObserverGate {
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        if let Some(waker) = self
            .waker
            .lock()
            .expect("fixture observer gate mutex poisoned")
            .take()
        {
            waker.wake();
        }
    }
}

impl EventObserver for FixtureObserverGate {
    fn observe<'a>(
        &'a self,
        event: &'a AgentEvent,
        _cancellation: CancellationToken,
    ) -> ObserverFuture<'a> {
        let hold_agent_end = matches!(event.kind, AgentEventKind::AgentEnd { .. });
        Box::pin(std::future::poll_fn(move |context| {
            if !hold_agent_end {
                return Poll::Ready(Ok(()));
            }
            self.reached.store(true, Ordering::Release);
            if self.released.load(Ordering::Acquire) {
                Poll::Ready(Ok(()))
            } else {
                *self
                    .waker
                    .lock()
                    .expect("fixture observer gate mutex poisoned") = Some(context.waker().clone());
                Poll::Pending
            }
        }))
    }
}

impl HookSet for FixtureHooks {
    fn before_tool_call(
        &self,
        call: &ToolCall,
    ) -> Result<BeforeToolCall, tea_core::error::HookError> {
        match &self.before_tool_policy {
            Some(rule) if rule.tool_name == call.name && rule.terminate => {
                Ok(BeforeToolCall::Terminate {
                    reason: rule.reason.clone(),
                })
            }
            Some(rule) if rule.tool_name == call.name => Ok(BeforeToolCall::Block {
                reason: rule.reason.clone(),
            }),
            _ => Ok(BeforeToolCall::Allow),
        }
    }

    fn before_tool_call_async<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ContextEnvelope,
        cancellation: CancellationToken,
    ) -> HookFuture<'a, BeforeToolCall> {
        let Some(rule) = self
            .before_tool_policy
            .as_ref()
            .filter(|rule| rule.tool_name == call.name && rule.yield_once)
        else {
            return Box::pin(std::future::ready(self.before_tool_call(call)));
        };
        let cancel_after_yield = rule.cancel_after_yield;
        Box::pin(async move {
            yield_to_another_tool().await;
            if cancel_after_yield {
                cancellation.cancel();
                Ok(BeforeToolCall::Allow)
            } else {
                self.before_tool_call(call)
            }
        })
    }

    fn after_tool_call(
        &self,
        call: &ToolCall,
        _result: &AgentToolResult,
    ) -> Result<AfterToolCall, tea_core::error::HookError> {
        match &self.after_tool_replace {
            Some(rule) if rule.tool_name == call.name => Ok(AfterToolCall {
                content: Replacement::Replace(rule.content.clone()),
                is_error: Replacement::Replace(rule.is_error),
                terminate: rule.terminate,
                ..AfterToolCall::default()
            }),
            _ => Ok(AfterToolCall::default()),
        }
    }

    fn transform_context(
        &self,
        mut context: ContextEnvelope,
    ) -> Result<ContextEnvelope, tea_core::error::HookError> {
        if let Some(policy) = &self.context_hooks {
            context.host_messages.push(SerializedJson::new(
                policy.transform_append_host_message.clone(),
            ));
        }
        Ok(context)
    }

    fn convert_to_llm(
        &self,
        context: ContextEnvelope,
    ) -> Result<String, tea_core::error::HookError> {
        if let Some(request_contexts) = &self.request_contexts {
            request_contexts
                .lock()
                .expect("fixture quality request-context mutex poisoned")
                .push(context.clone());
        }
        if let Some(policy) = &self.context_hooks {
            let host_messages = context
                .host_messages
                .iter()
                .map(SerializedJson::as_str)
                .collect::<Vec<_>>()
                .join("|");
            return Ok(format!("{}{}", policy.convert_prefix, host_messages));
        }
        Ok(context
            .messages
            .into_iter()
            .map(|message| format!("{message:?}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn prepare_next_turn(
        &self,
        mut context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, tea_core::error::HookError> {
        let Some(policy) = &self.context_hooks else {
            return Ok(AgentLoopTurnUpdate::default());
        };
        context.host_messages = policy
            .next_host_messages
            .iter()
            .cloned()
            .map(SerializedJson::new)
            .collect();
        Ok(AgentLoopTurnUpdate {
            context: Some(context),
            model: Some(policy.next_model.clone()),
            thinking_level: Some(policy.next_thinking_level),
        })
    }

    fn should_stop_after_turn(
        &self,
        _context: &ContextEnvelope,
    ) -> Result<bool, tea_core::error::HookError> {
        Ok(self.should_stop_after_turn)
    }
}

#[derive(Debug)]
struct FixtureProvider {
    streams: Mutex<VecDeque<FixtureModelStream>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for FixtureProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests
            .lock()
            .expect("fixture model request mutex poisoned")
            .push(request);
        let cancelled_before_request = cancellation.is_cancelled();
        let scripted = self
            .streams
            .lock()
            .expect("fixture model stream mutex poisoned")
            .pop_front()
            .ok_or_else(|| tea_core::error::SchedulerError::UnknownToolCall {
                tool_call_id: ToolCallId::new("fixture-exhausted-model-script")
                    .expect("fixed fixture ID is non-empty"),
            });
        Box::pin(std::future::ready(scripted.map(move |script| {
            if cancelled_before_request {
                return Box::new(ModelStream {
                    events: vec![ModelStreamEvent::Aborted {
                        message: "Operation aborted".into(),
                    }],
                }) as _;
            }
            if script.cancel_after_text_delta {
                cancellation.cancel();
            }
            Box::new(script.stream) as _
        })))
    }
}

#[derive(Debug)]
struct FixtureTool {
    name: String,
    description: String,
    execution_mode: tea_core::tool::ToolExecutionMode,
    schema: JsonValue,
    responses: Mutex<Vec<FixtureToolResponse>>,
    active_queue_target: Arc<Mutex<Option<Agent>>>,
}

impl AgentTool for FixtureTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> &JsonValue {
        &self.schema
    }

    fn execution_mode(&self) -> tea_core::tool::ToolExecutionMode {
        self.execution_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let call_name = call.name.clone();
        let response = {
            let mut responses = self
                .responses
                .lock()
                .expect("fixture tool response mutex poisoned");
            responses
                .iter()
                .position(|response| response.arguments == call.arguments)
                .map(|index| responses.remove(index))
        };
        let yield_once = response
            .as_ref()
            .is_some_and(|response| response.yield_once);
        let enqueue_during_execution = response
            .as_ref()
            .and_then(|response| response.enqueue_during_execution.clone());
        if let Some(response) = &response {
            for content in &response.updates {
                updates.emit(tea_core::tool::ToolUpdate {
                    content: content.clone(),
                    details: None,
                });
                if response.cancel_after_update {
                    context.cancellation.cancel();
                }
            }
        }
        let result = match response {
            Some(response) => Ok(AgentToolResult {
                tool_call_id: call.id,
                content: response.content,
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                terminate: response.terminate,
                is_error: response.is_error,
                failure: None,
            }),
            None => Err(tea_core::error::ToolError::Execution {
                tool: call.name,
                message: "fixture has no matching host tool response".into(),
            }),
        };
        if yield_once || enqueue_during_execution.is_some() {
            let active_queue_target = Arc::clone(&self.active_queue_target);
            let tool_name = call_name;
            Box::pin(async move {
                if yield_once {
                    yield_to_another_tool().await;
                }
                if let Some(arrival) = enqueue_during_execution {
                    let agent = active_queue_target
                        .lock()
                        .expect("fixture active-queue target mutex poisoned")
                        .clone()
                        .ok_or_else(|| tea_core::error::ToolError::Execution {
                            tool: tool_name.clone(),
                            message: "fixture queued a message before the agent was ready".into(),
                        })?;
                    match arrival {
                        FixtureActiveQueueArrival::Steer(text) => agent.enqueue_steering(text),
                        FixtureActiveQueueArrival::FollowUp(text) => agent.enqueue_follow_up(text),
                    }
                    .map_err(|error| {
                        tea_core::error::ToolError::Execution {
                            tool: tool_name,
                            message: error.to_string(),
                        }
                    })?;
                }
                result
            })
        } else {
            Box::pin(std::future::ready(result))
        }
    }
}

pub(super) async fn run_fixture(fixture: Fixture) -> Result<JsonValue, String> {
    let Fixture {
        id,
        system_prompt,
        provider,
        model,
        thinking_level,
        steering_mode,
        follow_up_mode,
        actions,
        before_tool_policy,
        after_tool_replace,
        context_hooks,
        should_stop_after_turn,
        hold_agent_end_observer,
        tools,
        streams,
        last_usage,
        last_stop_reason: _last_stop_reason,
    } = fixture;
    let tool_names = tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    let model_provider = Arc::new(FixtureProvider {
        streams: Mutex::new(streams.into()),
        requests: Arc::new(Mutex::new(Vec::new())),
    });
    let quality_capture_requests = env::var("TEA_QUALITY_CAPTURE").ok().as_deref() == Some("1");
    let quality_request_contexts =
        quality_capture_requests.then(|| Arc::new(Mutex::new(Vec::new())));
    let active_queue_target = Arc::new(Mutex::new(None));
    let observer_gate = hold_agent_end_observer.then(|| Arc::new(FixtureObserverGate::default()));
    if observer_gate.is_some()
        && actions
            .iter()
            .filter(|action| matches!(action, FixtureAction::Prompt(_) | FixtureAction::Continue))
            .count()
            != 1
    {
        return Err("host.observer.hold_agent_end requires exactly one run-starting action".into());
    }
    let mut builder = Agent::builder()
        .system_prompt(system_prompt.clone())
        .model(ModelDescriptor {
            provider: provider.clone(),
            model: model.clone(),
            revision: None,
        })
        .thinking_level(thinking_level)
        .steering_mode(steering_mode)
        .follow_up_mode(follow_up_mode)
        .model_provider(Arc::clone(&model_provider) as Arc<dyn ModelProvider>);
    if before_tool_policy.is_some()
        || after_tool_replace.is_some()
        || context_hooks.is_some()
        || should_stop_after_turn
        || quality_capture_requests
    {
        builder = builder.hooks(Arc::new(FixtureHooks {
            before_tool_policy,
            after_tool_replace,
            context_hooks: context_hooks.clone(),
            should_stop_after_turn,
            request_contexts: quality_request_contexts.clone(),
        }));
    }
    if let Some(context_hooks) = &context_hooks {
        for message in &context_hooks.host_messages {
            builder = builder.host_message(SerializedJson::new(message.clone()));
        }
    }
    if let Some(observer_gate) = &observer_gate {
        builder = builder.observer(Arc::clone(observer_gate) as Arc<dyn EventObserver>);
    }
    for tool in tools {
        builder = builder.tool(Arc::new(FixtureTool {
            name: tool.name,
            description: tool.description,
            execution_mode: tool.execution_mode,
            schema: JsonValue::parse(tool.parameters.as_str())
                .map_err(|error| error.to_string())?,
            responses: Mutex::new(tool.responses),
            active_queue_target: Arc::clone(&active_queue_target),
        }));
    }
    let agent = builder.build();
    *active_queue_target
        .lock()
        .expect("fixture active-queue target mutex poisoned") = Some(agent.clone());
    let mut events = Vec::new();
    let mut event_sequence = 0;
    let mut turn_offset = 0;
    let mut outcome = "completed";
    let mut observer_active_before_release = None;
    for action in actions {
        let run = match action {
            FixtureAction::Steer(input) => {
                agent.enqueue_steering(input).map_err(core_error)?;
                None
            }
            FixtureAction::FollowUp(input) => {
                agent.enqueue_follow_up(input).map_err(core_error)?;
                None
            }
            FixtureAction::Prompt(input) => Some(agent.start_prompt(input).map_err(core_error)?),
            FixtureAction::Continue => Some(agent.start_continue().map_err(core_error)?),
        };
        if let Some(run) = run {
            // Pi represents a completed error/aborted assistant response as
            // terminal lifecycle events, not an adapter process failure. The
            // Rust library preserves its typed error for direct callers; this
            // closed parity adapter normalizes that API distinction only after
            // confirming the run settled with the equivalent terminal reason.
            let drive_result = if let Some(observer_gate) = &observer_gate {
                let mut driving = Box::pin(run.drive());
                std::future::poll_fn(|context| match driving.as_mut().poll(context) {
                    Poll::Ready(_) => Poll::Ready(Err(
                        "run settled before its held agent_end observer was released".to_owned(),
                    )),
                    Poll::Pending if observer_gate.reached.load(Ordering::Acquire) => {
                        Poll::Ready(Ok(()))
                    }
                    Poll::Pending => {
                        context.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await?;
                let active = !matches!(agent.snapshot().phase, AgentPhase::Idle);
                if !active {
                    return Err(
                        "agent became idle before its held agent_end observer was released".into(),
                    );
                }
                observer_active_before_release = Some(active);
                observer_gate.release();
                driving.await
            } else {
                run.drive().await
            };
            match drive_result {
                Ok(()) => outcome = "completed",
                Err(CoreError::Cancelled) => outcome = "cancelled",
                Err(CoreError::ModelError { .. } | CoreError::ModelAborted { .. }) => {
                    // Provider/model failures are terminal assistant responses
                    // in this adapter and do not mean the host cancelled the run.
                    outcome = "completed";
                }
                Err(error) => return Err(core_error(error)),
            }
            let run_events = run.events();
            let turns_in_run = run_events
                .iter()
                .filter(|event| matches!(event.kind, AgentEventKind::TurnStart { .. }))
                .count() as u64;
            for event in &run_events {
                // Provider-request observation is an internal bridge for
                // durable/runtime consumers. Declarative fixtures retain the
                // established agent lifecycle grammar; their separate request
                // capture owns provider request assertions.
                if matches!(event.kind, AgentEventKind::ProviderRequestObserved { .. }) {
                    continue;
                }
                events.push(normalize_event(event_sequence, event, turn_offset)?);
                event_sequence = event_sequence.saturating_add(1);
            }
            turn_offset = turn_offset.saturating_add(turns_in_run);
        }
    }
    let snapshot = agent.snapshot();
    if snapshot.phase != AgentPhase::Idle
        || snapshot.is_streaming
        || !snapshot.pending_tool_calls.is_empty()
    {
        return Err("Rust agent did not settle the fixture run".into());
    }
    let actual_stop_reason = snapshot
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Assistant { stop_reason, .. } => *stop_reason,
            AgentMessage::User { .. } | AgentMessage::ToolResult { .. } => None,
        })
        .ok_or_else(|| "Rust agent did not retain a terminal assistant response".to_owned())?;
    if !model_provider
        .streams
        .lock()
        .expect("fixture model stream mutex poisoned")
        .is_empty()
    {
        return Err("model_script contains unused turns".into());
    }

    events.push(JsonValue::object([
        ("seq", JsonValue::from(events.len() as u64)),
        ("type", JsonValue::from("agent_settled")),
        (
            "data",
            JsonValue::object([("outcome", JsonValue::from(outcome))]),
        ),
    ]));

    let mut result_fields = vec![
        ("format_version", JsonValue::from(1_u64)),
        ("kind", JsonValue::from("canonical_parity_result")),
        ("fixture_id", JsonValue::from(id)),
        ("outcome", JsonValue::from(outcome)),
        ("settled", JsonValue::from(true)),
        (
            "state",
            JsonValue::object([
                ("system_prompt", JsonValue::from(snapshot.system_prompt)),
                (
                    "model",
                    JsonValue::object([
                        ("provider", JsonValue::from(provider)),
                        ("id", JsonValue::from(model)),
                    ]),
                ),
                (
                    "thinking_level",
                    JsonValue::from(thinking_level_name(snapshot.thinking_level)),
                ),
                (
                    "tool_names",
                    JsonValue::Array(tool_names.into_iter().map(JsonValue::from).collect()),
                ),
                ("pending_tool_calls", JsonValue::Array(Vec::new())),
            ]),
        ),
        ("events", JsonValue::Array(events)),
        (
            "messages",
            JsonValue::Array(
                snapshot
                    .messages
                    .iter()
                    .map(normalize_message)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        ),
        (
            "last_response",
            JsonValue::object([
                ("api", JsonValue::from("fixture")),
                (
                    "stop_reason",
                    JsonValue::from(stop_reason_name(actual_stop_reason)),
                ),
            ]),
        ),
        (
            "usage",
            JsonValue::object([
                ("input", JsonValue::from(last_usage.input)),
                ("output", JsonValue::from(last_usage.output)),
                ("cache_read", JsonValue::from(last_usage.cache_read)),
                ("cache_write", JsonValue::from(last_usage.cache_write)),
                ("total_tokens", JsonValue::from(last_usage.total_tokens)),
            ]),
        ),
        ("error", JsonValue::Null),
    ];
    if context_hooks.is_some() {
        let requests = model_provider
            .requests
            .lock()
            .expect("fixture model request mutex poisoned");
        result_fields.push((
            "request_trace",
            JsonValue::Array(requests.iter().map(normalize_request).collect()),
        ));
    } else if quality_capture_requests {
        let requests = model_provider
            .requests
            .lock()
            .expect("fixture model request mutex poisoned");
        let contexts = quality_request_contexts
            .as_ref()
            .expect("quality request capture must allocate contexts")
            .lock()
            .expect("fixture quality request-context mutex poisoned");
        if requests.len() != contexts.len() {
            return Err("quality request capture count does not match model request count".into());
        }
        result_fields.push((
            "request_trace",
            JsonValue::Array(
                requests
                    .iter()
                    .zip(contexts.iter())
                    .map(|(request, context)| normalize_quality_request(request, context))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        ));
    }
    if observer_gate.is_some() {
        result_fields.push((
            "observer_settlement",
            JsonValue::object([
                ("agent_end_observed", JsonValue::from(true)),
                (
                    "active_before_release",
                    JsonValue::from(observer_active_before_release == Some(true)),
                ),
                ("idle_after_release", JsonValue::from(true)),
            ]),
        ));
    }
    Ok(JsonValue::object(result_fields))
}

async fn yield_to_another_tool() {
    let mut yielded = false;
    std::future::poll_fn(|context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

fn core_error(error: CoreError) -> String {
    error.to_string()
}
