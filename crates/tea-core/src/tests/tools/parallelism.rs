use super::super::*;

struct CancellingPendingParallelTool;

struct ExclusiveFixtureTool {
    calls: Arc<Mutex<u32>>,
    schema: tea_protocol::JsonValue,
}

struct NormalizingHook;

impl HookSet for NormalizingHook {
    fn before_tool_call(
        &self,
        _call: &ToolCall,
    ) -> Result<BeforeToolCall, crate::error::HookError> {
        Ok(BeforeToolCall::Normalize {
            arguments: SerializedJson::new(r#"{"text":"normalized by policy"}"#),
        })
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

    fn convert_to_llm(&self, _context: ContextEnvelope) -> Result<String, crate::error::HookError> {
        Ok("fixture context".into())
    }
}

impl AgentTool for ExclusiveFixtureTool {
    fn name(&self) -> &str {
        "exclusive"
    }

    fn description(&self) -> &str {
        "fixture transactional capability"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        &self.schema
    }

    fn requires_exclusive_batch(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        *self.calls.lock().expect("exclusive calls mutex") += 1;
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "must not execute in a mixed batch".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

impl AgentTool for CancellingPendingParallelTool {
    fn name(&self) -> &str {
        "cancel_pending"
    }

    fn description(&self) -> &str {
        "cancels its run and never settles by itself"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        static SCHEMA: std::sync::OnceLock<tea_protocol::JsonValue> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| tea_protocol::JsonValue::parse(r#"{"type":"object"}"#).unwrap())
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute<'a>(
        &'a self,
        _call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::poll_fn(move |_| {
            context.cancellation.cancel();
            Poll::Pending
        }))
    }
}

#[test]
fn cancellation_drops_a_parallel_tool_that_never_settles() {
    smol::block_on(async {
        let call_id = ToolCallId::new("cancel-pending").expect("fixture call ID");
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: call_id.clone(),
                        name: "cancel_pending".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Cancelled)],
            },
        ]));
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(CancellingPendingParallelTool))
            .build();
        let run = agent.start_prompt("cancel through the pending parallel capability")?;

        assert_eq!(run.drive().await, Err(CoreError::Cancelled));
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
        assert!(run.events().iter().any(|event| {
            matches!(
                &event.kind,
                AgentEventKind::ToolExecutionEnd { result, .. }
                    if result.failure.as_ref().is_some_and(|failure| {
                        failure.disposition() == crate::tool::ToolFailureDisposition::Cancelled
                    })
            )
        }));

        Ok::<(), CoreError>(())
    })
    .expect("parallel cancellation must settle the run");
}

#[test]
fn tool_turn_executes_then_continues_the_model_loop() {
    smol::block_on(async {
        let call_id = ToolCallId::new("call_openrouter_001").expect("non-empty provider ID");
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: call_id.clone(),
                        name: "echo".into(),
                        arguments: SerializedJson::new(r#"{"text":"hello"}"#),
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
        let executed = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(EchoTool {
                calls: Arc::clone(&executed),
                schema: tea_protocol::JsonValue::parse(
                    r#"{"type":"object","required":["text"]}"#,
                )
                .expect("test schema is valid JSON"),
            }))
            .build();
        let run = agent.start_prompt("echo hello")?;

        run.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        assert_eq!(
            executed.lock().expect("test tool mutex").as_slice(),
            [ToolCall {
                id: call_id.clone(),
                name: "echo".into(),
                arguments: SerializedJson::new(r#"{"text":"hello"}"#),
            }]
        );
        let events = run.events();
        assert!(matches!(events.first().map(|event| &event.kind), Some(AgentEventKind::AgentStart)));
        assert!(matches!(events.last().map(|event| &event.kind), Some(AgentEventKind::AgentEnd { .. })));
        let tool_start = events
            .iter()
            .position(|event| matches!(event.kind, AgentEventKind::ToolExecutionStart { .. }))
            .expect("tool starts");
        let tool_end = events
            .iter()
            .position(|event| matches!(event.kind, AgentEventKind::ToolExecutionEnd { .. }))
            .expect("tool ends");
        let tool_turn_end = events
            .iter()
            .position(|event| matches!(
                event.kind,
                AgentEventKind::TurnEnd {
                    reason: StopReason::ToolUse,
                    ..
                }
            ))
            .expect("tool-use turn ends");
        let second_turn_start = events
            .iter()
            .enumerate()
            .skip(tool_turn_end + 1)
            .find_map(|(index, event)| {
                matches!(event.kind, AgentEventKind::TurnStart { .. }).then_some(index)
            })
            .expect("follow-up turn starts");
        let assistant_update = events
            .iter()
            .enumerate()
            .skip(second_turn_start + 1)
            .find_map(|(index, event)| {
                matches!(event.kind, AgentEventKind::MessageUpdate { .. }).then_some(index)
            })
            .expect("follow-up assistant streams text");
        let stop_turn_end = events
            .iter()
            .enumerate()
            .skip(assistant_update + 1)
            .find_map(|(index, event)| {
                matches!(
                    event.kind,
                    AgentEventKind::TurnEnd {
                        reason: StopReason::Stop,
                        ..
                    }
                )
                .then_some(index)
            })
            .expect("follow-up turn stops");
        assert!(tool_start < tool_end);
        assert!(tool_end < tool_turn_end);
        assert!(tool_turn_end < second_turn_start);
        assert!(second_turn_start < assistant_update);
        assert!(assistant_update < stop_turn_end);

        let snapshot = agent.snapshot();
        assert_eq!(snapshot.phase, AgentPhase::Idle);
        assert_eq!(snapshot.messages.len(), 4);
        assert!(matches!(
            snapshot.messages[1],
            crate::state::AgentMessage::Assistant { ref tool_calls, .. }
                if tool_calls == &vec![AgentToolCall {
                    id: call_id.clone(),
                    name: "echo".into(),
                    arguments: SerializedJson::new(r#"{"text":"hello"}"#),
                }]
        ));
        assert!(matches!(
            snapshot.messages[2],
            crate::state::AgentMessage::ToolResult { ref tool_call_id, ref content, is_error: false, .. }
                if tool_call_id == &call_id && content == "echoed: hello"
        ));
        assert!(matches!(
            snapshot.messages[3],
            crate::state::AgentMessage::Assistant { ref content, ref tool_calls, .. }
                if content == "done" && tool_calls.is_empty()
        ));

        Ok::<(), CoreError>(())
    })
    .expect("tool call should continue to a final model turn");
}

#[test]
fn recovered_assistant_tool_calls_use_the_shared_scheduler_before_the_next_request() {
    smol::block_on(async {
        let call_id = ToolCallId::new("recovered-echo").expect("fixture call ID");
        let provider = Arc::new(ScriptedProvider::new([ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("continued after recovery".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }]));
        let executed = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(EchoTool {
                calls: Arc::clone(&executed),
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                    .expect("fixture schema"),
            }))
            .build();
        let recovered_calls = vec![AgentToolCall {
            id: call_id.clone(),
            name: "echo".into(),
            arguments: SerializedJson::new("{}"),
        }];
        agent.restore_pending_tool_calls(
            vec![
                AgentMessage::User {
                    id: MessageId(1),
                    content: "recover this tool call".into(),
                },
                AgentMessage::Assistant {
                    id: MessageId(2),
                    content: String::new(),
                    tool_calls: recovered_calls.clone(),
                    stop_reason: Some(StopReason::ToolUse),
                    error_message: None,
                },
            ],
            recovered_calls.clone(),
        )?;

        agent
            .start_recover_tool_calls(recovered_calls)?
            .drive()
            .await?;

        assert_eq!(
            executed.lock().expect("tool calls mutex").as_slice(),
            &[ToolCall {
                id: call_id,
                name: "echo".into(),
                arguments: SerializedJson::new("{}"),
            }]
        );
        assert_eq!(provider.requests().len(), 1);
        assert!(matches!(
            agent.snapshot().messages.last(),
            Some(AgentMessage::Assistant { content, .. }) if content == "continued after recovery"
        ));
        Ok::<(), CoreError>(())
    })
    .expect("recovered calls must share normal tool execution and continuation");
}

#[test]
fn recovery_resumes_only_the_missing_suffix_of_a_partially_committed_tool_batch() {
    smol::block_on(async {
        let first = ToolCallId::new("recovered-prefix-first").expect("fixture call ID");
        let second = ToolCallId::new("recovered-prefix-second").expect("fixture call ID");
        let provider = Arc::new(ScriptedProvider::new([ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("continued after partial recovery".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }]));
        let executed = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(EchoTool {
                calls: Arc::clone(&executed),
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                    .expect("fixture schema"),
            }))
            .build();
        let calls = vec![
            AgentToolCall {
                id: first.clone(),
                name: "echo".into(),
                arguments: SerializedJson::new("{}"),
            },
            AgentToolCall {
                id: second.clone(),
                name: "echo".into(),
                arguments: SerializedJson::new("{}"),
            },
        ];
        let missing = vec![calls[1].clone()];
        agent.restore_pending_tool_calls(
            vec![
                AgentMessage::User {
                    id: MessageId(1),
                    content: "recover a partially committed batch".into(),
                },
                AgentMessage::Assistant {
                    id: MessageId(2),
                    content: String::new(),
                    tool_calls: calls,
                    stop_reason: Some(StopReason::ToolUse),
                    error_message: None,
                },
                AgentMessage::ToolResult {
                    id: MessageId(3),
                    tool_call_id: first.clone(),
                    tool_name: "echo".into(),
                    content: "echoed: hello".into(),
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                },
            ],
            missing.clone(),
        )?;

        agent.start_recover_tool_calls(missing)?.drive().await?;

        assert_eq!(
            executed.lock().expect("tool calls mutex").as_slice(),
            &[ToolCall {
                id: second,
                name: "echo".into(),
                arguments: SerializedJson::new("{}"),
            }]
        );
        assert_eq!(provider.requests().len(), 1);
        assert_eq!(
            agent
                .snapshot()
                .messages
                .iter()
                .filter(|message| matches!(message, AgentMessage::ToolResult { .. }))
                .count(),
            2,
        );
        Ok::<(), CoreError>(())
    })
    .expect("partial durable prefixes must resume only their unresolved calls");
}

#[test]
fn after_tool_metadata_is_preserved_in_the_transcript() {
    smol::block_on(async {
        let call_id = ToolCallId::new("call_metadata").expect("non-empty provider ID");
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: call_id.clone(),
                        name: "echo".into(),
                        arguments: SerializedJson::new(r#"{"text":"hello"}"#),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
        ]));
        let agent = Agent::builder()
            .model_provider(provider)
            .hooks(Arc::new(MetadataAfterToolHook))
            .tool(Arc::new(EchoTool {
                calls: Arc::new(Mutex::new(Vec::new())),
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object","required":["text"]}"#)
                    .expect("test schema is valid JSON"),
            }))
            .build();
        let run = agent.start_prompt("attach metadata to echo")?;

        run.drive().await?;

        assert!(matches!(
            &agent.snapshot().messages[2],
            crate::state::AgentMessage::ToolResult {
                tool_call_id,
                details: Some(details),
                usage: Some(Usage {
                    input_tokens: Some(3),
                    output_tokens: Some(5),
                    reasoning_tokens: Some(2),
                    ..
                }),
                added_tool_names,
                ..
            } if tool_call_id == &call_id
                && details.as_str() == r#"{"source":"hook"}"#
                && added_tool_names == &["later-tool"]
        ));

        Ok::<(), CoreError>(())
    })
    .expect("after-tool result metadata should survive transcript insertion");
}

#[test]
fn invalid_tool_arguments_become_an_error_result_and_the_model_can_continue() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("call_invalid_arguments")
                            .expect("non-empty provider ID"),
                        name: "echo".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
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
        let executed = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(EchoTool {
                calls: Arc::clone(&executed),
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object","required":["text"]}"#)
                    .expect("test schema is valid JSON"),
            }))
            .build();
        let run = agent.start_prompt("send malformed echo arguments")?;

        run.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        assert!(executed.lock().expect("test tool mutex").is_empty());
        assert!(matches!(
            agent.snapshot().messages[2],
            crate::state::AgentMessage::ToolResult { is_error: true, .. }
        ));
        assert!(run.events().iter().any(|event| {
            matches!(
                event.kind,
                AgentEventKind::ToolExecutionEnd {
                    result: AgentToolResult { is_error: true, .. },
                    ..
                }
            )
        }));
        assert!(matches!(
            agent.snapshot().messages.last(),
            Some(crate::state::AgentMessage::Assistant { content, .. }) if content == "recovered"
        ));

        Ok::<(), CoreError>(())
    })
    .expect("schema failure should remain tool-scoped");
}

#[test]
fn parallel_tool_ends_follow_completion_order_while_results_keep_source_order() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("call_slow").expect("non-empty provider ID"),
                        name: "slow".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("call_fast").expect("non-empty provider ID"),
                        name: "fast".into(),
                        arguments: SerializedJson::new("{}"),
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
        let schema = tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
            .expect("test schema is valid JSON");
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(ParallelFixtureTool {
                name: "slow",
                execution_mode: ToolExecutionMode::Parallel,
                yield_once: true,
                update: Some("slow update"),
                schema: schema.clone(),
            }))
            .tool(Arc::new(ParallelFixtureTool {
                name: "fast",
                execution_mode: ToolExecutionMode::Parallel,
                yield_once: false,
                update: None,
                schema,
            }))
            .build();
        let run = agent.start_prompt("run two tools")?;

        run.drive().await?;

        let events = run.events();
        let ended = events
            .iter()
            .filter_map(|event| match &event.kind {
                AgentEventKind::ToolExecutionEnd { tool_call_id, .. } => {
                    Some(tool_call_id.as_str().to_owned())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ended, ["call_fast", "call_slow"]);
        let update_index = events
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    AgentEventKind::ToolExecutionUpdate {
                        tool_call_id,
                        update,
                        ..
                    } if tool_call_id.as_str() == "call_slow" && update.content == "slow update"
                )
            })
            .expect("slow tool update should be emitted");
        let fast_end_index = events
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    AgentEventKind::ToolExecutionEnd { tool_call_id, .. }
                        if tool_call_id.as_str() == "call_fast"
                )
            })
            .expect("fast tool end should be emitted");
        assert!(
            update_index < fast_end_index,
            "an update emitted before another completion must not wait for its own future"
        );
        let result_ids = agent
            .snapshot()
            .messages
            .into_iter()
            .filter_map(|message| match message {
                crate::state::AgentMessage::ToolResult { tool_call_id, .. } => {
                    Some(tool_call_id.as_str().to_owned())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(result_ids, ["call_slow", "call_fast"]);

        Ok::<(), CoreError>(())
    })
    .expect("parallel tools should preserve Pi completion and transcript order contracts");
}

#[test]
fn parallel_result_context_is_source_ordered_across_deterministic_completion_permutations() {
    smol::block_on(async {
        for yields in [[2, 0, 1], [1, 2, 0], [0, 1, 2]] {
            let provider = Arc::new(ScriptedProvider::new([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("call_a").expect("non-empty provider ID"),
                            name: "a".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("call_b").expect("non-empty provider ID"),
                            name: "b".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("call_c").expect("non-empty provider ID"),
                            name: "c".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ]));
            let schema = tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("test schema is valid JSON");
            let agent = Agent::builder()
                .model_provider(provider)
                .tool(Arc::new(VariableDelayTool {
                    name: "a",
                    yields: yields[0],
                    schema: schema.clone(),
                }))
                .tool(Arc::new(VariableDelayTool {
                    name: "b",
                    yields: yields[1],
                    schema: schema.clone(),
                }))
                .tool(Arc::new(VariableDelayTool {
                    name: "c",
                    yields: yields[2],
                    schema,
                }))
                .build();
            let run = agent.start_prompt("exercise completion permutation")?;
            run.drive().await?;

            let source_result_ids = agent
                .snapshot()
                .messages
                .into_iter()
                .filter_map(|message| match message {
                    crate::state::AgentMessage::ToolResult { tool_call_id, .. } => {
                        Some(tool_call_id.to_string())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(source_result_ids, ["call_a", "call_b", "call_c"]);

            let completed_ids = run
                .events()
                .into_iter()
                .filter_map(|event| match event.kind {
                    AgentEventKind::ToolExecutionEnd { tool_call_id, .. } => {
                        Some(tool_call_id.to_string())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(completed_ids.len(), 3);
            let mut sorted_completed = completed_ids;
            sorted_completed.sort();
            assert_eq!(sorted_completed, ["call_a", "call_b", "call_c"]);
        }

        Ok::<(), CoreError>(())
    })
    .expect("completion order may vary while model context remains source ordered");
}

#[test]
fn any_sequential_tool_serializes_a_mixed_tool_batch() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("call_serial").expect("non-empty provider ID"),
                        name: "serial".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("call_parallel").expect("non-empty provider ID"),
                        name: "parallel".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
        ]));
        let schema = tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
            .expect("test schema is valid JSON");
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(ParallelFixtureTool {
                name: "serial",
                execution_mode: ToolExecutionMode::Sequential,
                yield_once: true,
                update: None,
                schema: schema.clone(),
            }))
            .tool(Arc::new(ParallelFixtureTool {
                name: "parallel",
                execution_mode: ToolExecutionMode::Parallel,
                yield_once: false,
                update: None,
                schema,
            }))
            .build();
        let run = agent.start_prompt("run the mixed batch")?;

        run.drive().await?;

        let lifecycle = run
            .events()
            .into_iter()
            .filter_map(|event| match event.kind {
                AgentEventKind::ToolExecutionStart { tool_call_id, .. } => {
                    Some(("start", tool_call_id.to_string()))
                }
                AgentEventKind::ToolExecutionEnd { tool_call_id, .. } => {
                    Some(("end", tool_call_id.to_string()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle,
            vec![
                ("start", "call_serial".into()),
                ("end", "call_serial".into()),
                ("start", "call_parallel".into()),
                ("end", "call_parallel".into()),
            ]
        );

        Ok::<(), CoreError>(())
    })
    .expect("a sequential tool must serialize the whole Pi tool batch");
}

#[test]
fn exclusive_tool_rejects_the_entire_mixed_batch_before_any_effect_starts() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("ordinary-before-exclusive").expect("fixture call ID"),
                        name: "echo".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("exclusive-after-ordinary").expect("fixture call ID"),
                        name: "exclusive".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
        ]));
        let schema =
            tea_protocol::JsonValue::parse(r#"{"type":"object"}"#).expect("fixture schema");
        let ordinary_calls = Arc::new(Mutex::new(Vec::new()));
        let exclusive_calls = Arc::new(Mutex::new(0));
        let agent = Agent::builder()
            .model_provider(provider)
            .tool(Arc::new(EchoTool {
                calls: Arc::clone(&ordinary_calls),
                schema: schema.clone(),
            }))
            .tool(Arc::new(ExclusiveFixtureTool {
                calls: Arc::clone(&exclusive_calls),
                schema,
            }))
            .build();
        let run = agent.start_prompt("attempt an invalid mixed transactional batch")?;

        run.drive().await?;

        assert!(ordinary_calls
            .lock()
            .expect("ordinary calls mutex")
            .is_empty());
        assert_eq!(*exclusive_calls.lock().expect("exclusive calls mutex"), 0);
        assert!(!run
            .events()
            .iter()
            .any(|event| { matches!(event.kind, AgentEventKind::ToolExecutionStart { .. }) }));
        let results = agent
            .snapshot()
            .messages
            .into_iter()
            .filter_map(|message| match message {
                AgentMessage::ToolResult {
                    content, is_error, ..
                } => Some((content, is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(content, is_error)| {
            *is_error && content.contains("exclusive must be the only tool call")
        }));

        Ok::<(), CoreError>(())
    })
    .expect("mixed transactional batches must not start any sibling effect");
}

#[test]
fn before_tool_normalization_is_schema_validated_then_reaches_the_capability() {
    smol::block_on(async {
        let call_id = ToolCallId::new("normalize-before-execution").expect("fixture call ID");
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: call_id.clone(),
                        name: "echo".into(),
                        // This omits the required field. The hook must get a
                        // chance to normalize it before the canonical schema
                        // validation, and the capability must see only the
                        // validated transformed call.
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
        ]));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .model_provider(provider)
            .hooks(Arc::new(NormalizingHook))
            .tool(Arc::new(EchoTool {
                calls: Arc::clone(&calls),
                schema: tea_protocol::JsonValue::parse(
                    r#"{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}"#,
                )
                .expect("fixture schema"),
            }))
            .build();

        agent.start_prompt("normalize this call")?.drive().await?;

        assert_eq!(
            calls.lock().expect("normalized calls mutex").as_slice(),
            &[ToolCall {
                id: call_id,
                name: "echo".into(),
                arguments: SerializedJson::new(r#"{"text":"normalized by policy"}"#),
            }],
        );

        Ok::<(), CoreError>(())
    })
    .expect("policy normalization must be validated before a capability starts");
}

#[test]
fn parallel_completions_return_to_source_order_for_context() {
    let scheduler = Scheduler;
    let calls = (1..=3).map(|id| {
        (
            ToolCall {
                id: ToolCallId::new(format!("provider-call-{id}"))
                    .expect("non-empty test tool-call ID"),
                name: format!("tool-{id}"),
                arguments: SerializedJson::new("{}"),
            },
            ToolExecutionMode::Parallel,
        )
    });
    let batch = scheduler.plan_tool_batch(calls);
    let mut completions = crate::scheduler::CompletionSet::default();
    for id in [3, 1, 2] {
        batch
            .record_completion(
                &mut completions,
                AgentToolResult {
                    tool_call_id: ToolCallId::new(format!("provider-call-{id}"))
                        .expect("non-empty test tool-call ID"),
                    content: format!("{id}"),
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                },
            )
            .expect("planned call");
    }
    assert_eq!(
        completions
            .in_source_order(&batch)
            .into_iter()
            .map(|result| result.content)
            .collect::<Vec<_>>(),
        vec!["1", "2", "3"]
    );
}
