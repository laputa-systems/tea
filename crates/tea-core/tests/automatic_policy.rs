use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};
use tea_core::compaction::{
    AutomaticCompactionPolicy, AutomaticCompactionReason, AutomaticCompactionRequest,
    CompactionContext, CompactionError, CompactionFuture, CompactionRejection, CompactionResult,
    CompactionTerminalOutcome, Compactor, ContextBudgetSource, OverflowRecovery,
};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId, Usage};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink,
};
use tea_core::{Agent, AgentMessage, CoreError};

#[derive(Default)]
struct RecordingProvider {
    streams: Mutex<Vec<ModelStream>>,
    requests: Mutex<Vec<ModelRequest>>,
    order: Arc<Mutex<Vec<&'static str>>>,
}

impl RecordingProvider {
    fn new(streams: Vec<ModelStream>, order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            streams: Mutex::new(streams),
            requests: Mutex::new(Vec::new()),
            order,
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .expect("request mutex poisoned")
            .clone()
    }
}

impl ModelProvider for RecordingProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.order
            .lock()
            .expect("order mutex poisoned")
            .push("provider");
        self.requests
            .lock()
            .expect("request mutex poisoned")
            .push(request);
        let stream = self
            .streams
            .lock()
            .expect("stream mutex poisoned")
            .remove(0);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

struct OutputTool {
    outputs: Mutex<Vec<String>>,
}

impl AgentTool for OutputTool {
    fn name(&self) -> &str {
        "fixture"
    }

    fn description(&self) -> &str {
        "returns fixed fixture output"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        static SCHEMA: std::sync::OnceLock<tea_protocol::JsonValue> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| tea_protocol::JsonValue::parse(r#"{"type":"object"}"#).unwrap())
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        let content = self
            .outputs
            .lock()
            .expect("output mutex poisoned")
            .remove(0);
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content,
            details: Some(SerializedJson::new(r#"{"raw":"details"}"#)),
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

enum CompactorMode {
    Reduce,
    Fail,
}

struct RecordingCompactor {
    calls: Mutex<Vec<AutomaticCompactionRequest>>,
    order: Arc<Mutex<Vec<&'static str>>>,
    mode: CompactorMode,
}

struct CancellingCompactor;

enum RejectionMode {
    NonShrinking,
    EmptyCheckpoint,
    InsufficientHeadroom,
}

struct RejectionCompactor {
    mode: RejectionMode,
}

impl Compactor for CancellingCompactor {
    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        Box::pin(std::future::ready(Ok(CompactionResult::new(
            context.messages,
        ))))
    }

    fn compact_automatic<'a>(
        &'a self,
        context: CompactionContext,
        _request: AutomaticCompactionRequest,
        cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        cancellation.cancel();
        Box::pin(std::future::ready(Ok(CompactionResult::new(
            context.messages,
        ))))
    }
}

impl Compactor for RejectionCompactor {
    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        Box::pin(std::future::ready(Ok(CompactionResult::new(
            context.messages,
        ))))
    }

    fn compact_automatic<'a>(
        &'a self,
        context: CompactionContext,
        _request: AutomaticCompactionRequest,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        let messages = match self.mode {
            RejectionMode::NonShrinking
            | RejectionMode::EmptyCheckpoint
            | RejectionMode::InsufficientHeadroom => context
                .messages
                .first()
                .cloned()
                .map(|message| match message {
                    AgentMessage::User { id, .. } => AgentMessage::User {
                        id,
                        content: match self.mode {
                            RejectionMode::NonShrinking => "x".repeat(1_000),
                            RejectionMode::EmptyCheckpoint => "   ".into(),
                            RejectionMode::InsufficientHeadroom => "C".into(),
                        },
                    },
                    message => message,
                })
                .into_iter()
                .collect(),
        };
        Box::pin(std::future::ready(Ok(CompactionResult::new(messages))))
    }
}

impl RecordingCompactor {
    fn reduce(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            order,
            mode: CompactorMode::Reduce,
        }
    }

    fn fail(order: Arc<Mutex<Vec<&'static str>>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            order,
            mode: CompactorMode::Fail,
        }
    }
}

impl Compactor for RecordingCompactor {
    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        Box::pin(std::future::ready(Ok(CompactionResult::new(
            context.messages,
        ))))
    }

    fn compact_automatic<'a>(
        &'a self,
        context: CompactionContext,
        request: AutomaticCompactionRequest,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        self.order
            .lock()
            .expect("order mutex poisoned")
            .push("compactor");
        self.calls
            .lock()
            .expect("compactor mutex poisoned")
            .push(request.clone());
        match self.mode {
            CompactorMode::Fail => Box::pin(std::future::ready(Err(CompactionError::failed(
                "fixture compactor failed",
            )))),
            CompactorMode::Reduce => {
                let mut messages = Vec::new();
                if let Some(first) = context.messages.first() {
                    let checkpoint = match first {
                        AgentMessage::User { id, .. } => AgentMessage::User {
                            id: *id,
                            content: "C".into(),
                        },
                        message => message.clone(),
                    };
                    messages.push(checkpoint);
                }
                messages.extend(request.retained_messages);
                Box::pin(std::future::ready(Ok(CompactionResult::new(messages))))
            }
        }
    }
}

fn policy(
    context_tokens: u64,
    recent_tokens: u64,
    overflow_recovery: OverflowRecovery,
) -> AutomaticCompactionPolicy {
    AutomaticCompactionPolicy {
        enabled: true,
        context_budget: ContextBudgetSource::ContextBudget(
            NonZeroU64::new(context_tokens).expect("non-zero fixture budget"),
        ),
        reserved_tokens: 0,
        minimum_headroom_tokens: 1,
        recent_tokens,
        overflow_recovery,
        max_compactions_per_run: 2,
        max_overflow_retries_per_run: 1,
    }
}

fn tool_call(id: &str) -> AgentToolCall {
    AgentToolCall {
        id: ToolCallId::new(id).expect("fixture tool call ID"),
        name: "fixture".into(),
        arguments: SerializedJson::new("{}"),
    }
}

#[test]
fn idle_agent_can_replace_automatic_policy_without_rebuilding_history() {
    let agent = Agent::builder().build();
    let configured = policy(262_144, 20_000, OverflowRecovery::CompactAndRetry);
    agent
        .replace_automatic_compaction(configured.clone())
        .expect("idle policy replacement succeeds");
    assert_eq!(agent.automatic_compaction(), configured);
}

#[test]
fn threshold_compacts_once_before_the_next_provider_request() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![
                ModelStream {
                    events: vec![
                        ModelStreamEvent::Usage(Usage {
                            input_tokens: Some(100),
                            ..Usage::default()
                        }),
                        ModelStreamEvent::ToolCall(tool_call("call-threshold")),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("done".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ],
            Arc::clone(&order),
        ));
        let compactor = Arc::new(RecordingCompactor::reduce(Arc::clone(&order)));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(OutputTool {
                outputs: Mutex::new(vec!["x".repeat(300)]),
            }))
            .compactor(compactor.clone())
            .automatic_compaction(policy(90, 0, OverflowRecovery::Disabled))?
            .build();

        agent.start_prompt("start")?.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        assert_eq!(
            order.lock().expect("order mutex poisoned").as_slice(),
            ["provider", "compactor", "provider"]
        );
        let calls = compactor.calls.lock().expect("compactor mutex poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].reason, AutomaticCompactionReason::Threshold);
        assert!(!calls[0].retry_provider_request);
        assert!(agent.snapshot().messages.iter().all(|message| !matches!(
            message,
            AgentMessage::ToolResult { content, .. } if content.len() == 300
        )));
        Ok::<(), CoreError>(())
    })
    .expect("threshold compaction succeeds");
}

#[test]
fn zero_usage_does_not_replace_the_last_valid_context_checkpoint() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![
                ModelStream {
                    events: vec![
                        ModelStreamEvent::Usage(Usage {
                            input_tokens: Some(800),
                            ..Usage::default()
                        }),
                        ModelStreamEvent::ToolCall(tool_call("call-valid-usage")),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::Usage(Usage {
                            input_tokens: Some(0),
                            ..Usage::default()
                        }),
                        ModelStreamEvent::ToolCall(tool_call("call-zero-usage")),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("done".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ],
            Arc::clone(&order),
        ));
        let compactor = Arc::new(RecordingCompactor::reduce(Arc::clone(&order)));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(OutputTool {
                outputs: Mutex::new(vec!["small".into(), "x".repeat(300)]),
            }))
            .compactor(compactor.clone())
            .automatic_compaction(policy(850, 0, OverflowRecovery::Disabled))?
            .build();

        agent.start_prompt("start")?.drive().await?;

        assert_eq!(provider.requests().len(), 3);
        assert_eq!(
            compactor
                .calls
                .lock()
                .expect("compactor mutex poisoned")
                .len(),
            1
        );
        Ok::<(), CoreError>(())
    })
    .expect("zero usage retains the prior checkpoint");
}

#[test]
fn overflow_compacts_and_retries_the_same_continuation_once() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![
                ModelStream {
                    events: vec![ModelStreamEvent::ContextOverflow {
                        message: "context capacity exceeded".into(),
                    }],
                },
                ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta("recovered".into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                },
            ],
            Arc::clone(&order),
        ));
        let compactor = Arc::new(RecordingCompactor::reduce(Arc::clone(&order)));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .compactor(compactor.clone())
            .automatic_compaction(policy(10_000, 0, OverflowRecovery::CompactAndRetry))?
            .build();

        agent.start_prompt("start")?.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        let calls = compactor.calls.lock().expect("compactor mutex poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].reason, AutomaticCompactionReason::Overflow);
        assert!(calls[0].retry_provider_request);
        assert!(agent.snapshot().messages.iter().all(|message| !matches!(
            message,
            AgentMessage::Assistant { error_message: Some(message), .. }
                if message == "context capacity exceeded"
        )));
        Ok::<(), CoreError>(())
    })
    .expect("one overflow retry succeeds");
}

#[test]
fn a_second_overflow_does_not_retry_the_same_continuation_again() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![
                ModelStream {
                    events: vec![ModelStreamEvent::ContextOverflow {
                        message: "first context capacity exceeded".into(),
                    }],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::ContextOverflow {
                        message: "second context capacity exceeded".into(),
                    }],
                },
            ],
            Arc::clone(&order),
        ));
        let compactor = Arc::new(RecordingCompactor::reduce(Arc::clone(&order)));
        let mut configured = policy(10_000, 0, OverflowRecovery::CompactAndRetry);
        configured.max_overflow_retries_per_run = 2;
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .compactor(compactor.clone())
            .automatic_compaction(configured)?
            .build();

        assert!(matches!(
            agent.start_prompt("start")?.drive().await,
            Err(CoreError::ModelError { message }) if message == "second context capacity exceeded"
        ));
        assert_eq!(provider.requests().len(), 2);
        assert_eq!(
            compactor
                .calls
                .lock()
                .expect("compactor mutex poisoned")
                .len(),
            1
        );
        Ok::<(), CoreError>(())
    })
    .expect("a logical continuation is retried at most once");
}

#[test]
fn overflow_error_keeps_the_last_valid_usage_checkpoint_for_compaction() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![
                ModelStream {
                    events: vec![
                        ModelStreamEvent::Usage(Usage {
                            input_tokens: Some(800),
                            ..Usage::default()
                        }),
                        ModelStreamEvent::ToolCall(tool_call("call-before-overflow")),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::ContextOverflow {
                        message: "context capacity exceeded".into(),
                    }],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ],
            Arc::clone(&order),
        ));
        let compactor = Arc::new(RecordingCompactor::reduce(Arc::clone(&order)));
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(OutputTool {
                outputs: Mutex::new(vec!["tool output after valid usage".into()]),
            }))
            .compactor(compactor.clone())
            .automatic_compaction(policy(1_000, 0, OverflowRecovery::CompactAndRetry))?
            .build();

        agent.start_prompt("start")?.drive().await?;
        let request = compactor.calls.lock().expect("compactor mutex poisoned")[0].clone();
        assert_eq!(request.reason, AutomaticCompactionReason::Overflow);
        assert!(request
            .estimated_tokens_before
            .is_some_and(|tokens| tokens > 800));
        Ok::<(), CoreError>(())
    })
    .expect("overflow must not reset valid usage accounting");
}

#[test]
fn failed_automatic_compaction_does_not_mutate_the_pre_transaction_transcript() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![ModelStream {
                events: vec![
                    ModelStreamEvent::Usage(Usage {
                        input_tokens: Some(100),
                        ..Usage::default()
                    }),
                    ModelStreamEvent::ToolCall(tool_call("call-failure")),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            }],
            Arc::clone(&order),
        ));
        let compactor = Arc::new(RecordingCompactor::fail(Arc::clone(&order)));
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(OutputTool {
                outputs: Mutex::new(vec!["raw tool output".into()]),
            }))
            .compactor(compactor)
            .automatic_compaction(policy(90, 0, OverflowRecovery::Disabled))?
            .build();

        let error = agent
            .start_prompt("start")?
            .drive()
            .await
            .expect_err("failed compaction stops at the configured boundary");
        assert!(matches!(error, CoreError::AutomaticCompaction { .. }));
        let messages = agent.snapshot().messages;
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[2], AgentMessage::ToolResult { ref content, .. } if content == "raw tool output"));
        Ok::<(), CoreError>(())
    })
    .expect("automatic transaction failure is non-mutating");
}

#[test]
fn automatic_request_exposes_a_split_turn_prefix_without_cutting_tool_pairs() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![
                ModelStream {
                    events: vec![
                        ModelStreamEvent::Usage(Usage {
                            input_tokens: Some(1_000),
                            ..Usage::default()
                        }),
                        ModelStreamEvent::ToolCall(tool_call("call-split")),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ],
            Arc::clone(&order),
        ));
        let compactor = Arc::new(RecordingCompactor::reduce(Arc::clone(&order)));
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(OutputTool {
                outputs: Mutex::new(vec!["tool output".into()]),
            }))
            .compactor(compactor.clone())
            .automatic_compaction(policy(900, 12, OverflowRecovery::Disabled))?
            .build();

        agent.start_prompt("start")?.drive().await?;
        let request = compactor.calls.lock().expect("compactor mutex poisoned")[0].clone();
        assert!(matches!(
            request.retained_messages.first(),
            Some(AgentMessage::Assistant { .. })
        ));
        assert!(matches!(
            request.retained_messages.last(),
            Some(AgentMessage::ToolResult { .. })
        ));
        assert!(matches!(
            request.split_turn_prefix.as_slice(),
            [AgentMessage::User { .. }]
        ));
        Ok::<(), CoreError>(())
    })
    .expect("split-turn request remains valid");
}

#[test]
fn cancelled_automatic_compaction_leaves_the_pre_transaction_transcript_unchanged() {
    smol::block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            vec![ModelStream {
                events: vec![
                    ModelStreamEvent::Usage(Usage {
                        input_tokens: Some(100),
                        ..Usage::default()
                    }),
                    ModelStreamEvent::ToolCall(tool_call("call-cancel-compact")),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            }],
            Arc::clone(&order),
        ));
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(OutputTool {
                outputs: Mutex::new(vec!["raw tool output".into()]),
            }))
            .compactor(Arc::new(CancellingCompactor))
            .automatic_compaction(policy(90, 0, OverflowRecovery::Disabled))?
            .build();

        let run = agent.start_prompt("start")?;
        assert_eq!(run.drive().await, Err(CoreError::Cancelled));
        let messages = agent.snapshot().messages;
        assert_eq!(messages.len(), 3);
        assert!(matches!(messages[2], AgentMessage::ToolResult { ref content, .. } if content == "raw tool output"));
        assert!(matches!(
            run.events().iter().find_map(|event| match &event.kind {
                tea_core::AgentEventKind::AutomaticCompactionEnd { outcome, .. } => Some(outcome),
                _ => None,
            }),
            Some(tea_core::AutomaticCompactionOutcome::Cancelled)
        ));
        Ok::<(), CoreError>(())
    })
    .expect("cancelled compaction is transactional");
}

#[test]
fn automatic_rejection_gates_are_typed_and_non_mutating() {
    smol::block_on(async {
        let cases = [
            (
                RejectionMode::NonShrinking,
                policy(90, 0, OverflowRecovery::Disabled),
                CompactionRejection::NonShrinkingReplacement,
            ),
            (
                RejectionMode::EmptyCheckpoint,
                policy(90, 0, OverflowRecovery::Disabled),
                CompactionRejection::EmptyCheckpoint,
            ),
            (
                RejectionMode::InsufficientHeadroom,
                AutomaticCompactionPolicy {
                    minimum_headroom_tokens: 99,
                    ..policy(100, 0, OverflowRecovery::Disabled)
                },
                CompactionRejection::InsufficientHeadroom,
            ),
        ];
        for (mode, configured_policy, expected) in cases {
            let order = Arc::new(Mutex::new(Vec::new()));
            let provider = Arc::new(RecordingProvider::new(
                vec![ModelStream {
                    events: vec![
                        ModelStreamEvent::Usage(Usage {
                            input_tokens: Some(100),
                            ..Usage::default()
                        }),
                        ModelStreamEvent::ToolCall(tool_call("gate-rejection")),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                }],
                Arc::clone(&order),
            ));
            let agent = Agent::builder()
                .model_provider(provider)
                .tool(Arc::new(OutputTool {
                    outputs: Mutex::new(vec!["x".repeat(300)]),
                }))
                .compactor(Arc::new(RejectionCompactor { mode }))
                .automatic_compaction(configured_policy)?
                .build();
            let run = agent.start_prompt("start")?;
            assert!(matches!(
                run.drive().await,
                Err(CoreError::AutomaticCompaction { .. })
            ));
            assert!(run.events().iter().any(|event| matches!(
                event.kind,
                tea_core::AgentEventKind::CompactionLifecycle {
                    record: tea_core::CompactionLifecycleRecord::Terminal {
                        outcome: CompactionTerminalOutcome::Rejected(rejection),
                        ..
                    }
                } if rejection == expected
            )));
            assert!(matches!(agent.snapshot().messages.last(), Some(AgentMessage::ToolResult { content, .. }) if content.len() == 300));
        }
        Ok::<(), CoreError>(())
    })
    .expect("rejection gates preserve the transaction source");
}
