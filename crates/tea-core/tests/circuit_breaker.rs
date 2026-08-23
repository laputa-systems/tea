use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId};
use tea_core::tool::{
    AgentTool, FailureSignature, ToolCall, ToolContext, ToolExecutionMode, ToolFailure,
    ToolFailureCircuitBreaker, ToolFuture, ToolUpdateSink, AgentToolResult,
};
use tea_core::{Agent, AgentEventKind, AgentMessage, CoreError};

struct Provider {
    streams: Mutex<Vec<ModelStream>>,
    requests: Mutex<usize>,
}

impl Provider {
    fn new(streams: Vec<ModelStream>) -> Self {
        Self {
            streams: Mutex::new(streams),
            requests: Mutex::new(0),
        }
    }
}

impl ModelProvider for Provider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        *self.requests.lock().expect("request mutex poisoned") += 1;
        let stream = self
            .streams
            .lock()
            .expect("stream mutex poisoned")
            .remove(0);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

struct FailureTool {
    mode: ToolExecutionMode,
    calls: Mutex<Vec<String>>,
    failure: Option<ToolFailure>,
}

impl AgentTool for FailureTool {
    fn name(&self) -> &str {
        "capability"
    }

    fn description(&self) -> &str {
        "fixture capability"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        static SCHEMA: std::sync::OnceLock<tea_protocol::JsonValue> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            tea_protocol::JsonValue::parse(r#"{"type":"object","required":["text"]}"#).unwrap()
        })
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        self.calls
            .lock()
            .expect("call mutex poisoned")
            .push(call.id.to_string());
        let failure = self.failure.clone();
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "capability is unavailable".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: failure.is_some(),
            failure,
        })))
    }
}

fn call(id: &str, arguments: &str) -> AgentToolCall {
    AgentToolCall {
        id: ToolCallId::new(id).expect("fixture tool ID"),
        name: "capability".into(),
        arguments: SerializedJson::new(arguments),
    }
}

#[test]
fn fatal_result_ends_a_sequential_batch_without_another_provider_request() {
    smol::block_on(async {
        let provider = Arc::new(Provider::new(vec![ModelStream {
            events: vec![
                ModelStreamEvent::ToolCall(call("fatal-first", r#"{"text":"first"}"#)),
                ModelStreamEvent::ToolCall(call("fatal-later", r#"{"text":"later"}"#)),
                ModelStreamEvent::End(StopReason::ToolUse),
            ],
        }]));
        let tool = Arc::new(FailureTool {
            mode: ToolExecutionMode::Sequential,
            calls: Mutex::new(Vec::new()),
            failure: Some(
                ToolFailure::fatal(
                    FailureSignature::new("bridge-process:dead").expect("stable signature"),
                )
                .with_recovery_guidance("The capability process exited; do not retry it."),
            ),
        });
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(tool.clone())
            .build();

        let run = agent.start_prompt("start")?;
        let error = run
            .drive()
            .await
            .expect_err("fatal capability must stop the run");
        assert!(matches!(error, CoreError::ToolCircuitBreaker { .. }));
        assert_eq!(
            *provider.requests.lock().expect("request mutex poisoned"),
            1
        );
        assert_eq!(
            tool.calls.lock().expect("call mutex poisoned").as_slice(),
            ["fatal-first"]
        );
        assert_eq!(
            agent
                .snapshot()
                .messages
                .iter()
                .filter(|message| matches!(message, AgentMessage::ToolResult { .. }))
                .count(),
            2,
            "the skipped call has a result so compaction can retain valid pairs"
        );
        assert!(run.events().iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::ToolFailureObserved {
                terminal: true,
                consecutive_count: 1,
                ..
            }
        )));
        Ok::<(), CoreError>(())
    })
    .expect("fatal result leaves the agent settled");
}

#[test]
fn repeated_identical_retryable_failures_trip_the_configured_threshold() {
    smol::block_on(async {
        let provider = Arc::new(Provider::new(vec![
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(call("retry-one", r#"{"text":"one"}"#)),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(call("retry-two", r#"{"text":"two"}"#)),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
        ]));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(FailureTool {
                mode: ToolExecutionMode::Parallel,
                calls: Mutex::new(Vec::new()),
                failure: Some(ToolFailure::retryable(
                    FailureSignature::new("mcp:bridge-unreachable").expect("stable signature"),
                )),
            }))
            .tool_failure_circuit_breaker(ToolFailureCircuitBreaker {
                max_consecutive_retryable_failures: Some(NonZeroU32::new(2).unwrap()),
            })
            .build();

        let run = agent.start_prompt("start")?;
        assert!(matches!(
            run.drive().await,
            Err(CoreError::ToolCircuitBreaker { .. })
        ));
        assert_eq!(
            *provider.requests.lock().expect("request mutex poisoned"),
            2
        );
        assert!(run.events().iter().any(|event| matches!(
            &event.kind,
            AgentEventKind::ToolFailureObserved {
                signature: Some(signature),
                consecutive_count: 2,
                terminal: true,
                ..
            } if signature == "mcp:bridge-unreachable"
        )));
        Ok::<(), CoreError>(())
    })
    .expect("retry circuit breaker settles the run");
}

#[test]
fn invalid_arguments_remain_recoverable_with_a_circuit_breaker_enabled() {
    smol::block_on(async {
        let provider = Arc::new(Provider::new(vec![
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(call("bad-arguments", "{}")),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("recovered".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(FailureTool {
                mode: ToolExecutionMode::Parallel,
                calls: Mutex::new(Vec::new()),
                failure: None,
            }))
            .tool_failure_circuit_breaker(ToolFailureCircuitBreaker {
                max_consecutive_retryable_failures: Some(NonZeroU32::new(1).unwrap()),
            })
            .build();

        agent.start_prompt("start")?.drive().await?;
        assert_eq!(
            *provider.requests.lock().expect("request mutex poisoned"),
            2
        );
        Ok::<(), CoreError>(())
    })
    .expect("invalid arguments are recoverable");
}
