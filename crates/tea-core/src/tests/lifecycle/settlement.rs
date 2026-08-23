use super::super::*;

#[test]
fn caller_driven_text_run_emits_lifecycle_and_settles() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .build();
        let run = agent.start_prompt("Return exactly: fixture capture succeeded.")?;

        run.drive().await?;

        let events = run.events();
        assert!(matches!(
            events.first().map(|event| &event.kind),
            Some(AgentEventKind::AgentStart)
        ));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(AgentEventKind::AgentEnd { .. })
        ));
        let user_start = events
            .iter()
            .position(|event| {
                matches!(
                    event.kind,
                    AgentEventKind::MessageStart {
                        message: crate::state::AgentMessage::User { .. }
                    }
                )
            })
            .expect("user message starts");
        let user_end = events
            .iter()
            .position(|event| {
                matches!(
                    event.kind,
                    AgentEventKind::MessageEnd {
                        message: crate::state::AgentMessage::User { .. }
                    }
                )
            })
            .expect("user message ends");
        let assistant_start = events
            .iter()
            .position(|event| {
                matches!(
                    event.kind,
                    AgentEventKind::MessageStart {
                        message: crate::state::AgentMessage::Assistant { .. }
                    }
                )
            })
            .expect("assistant message starts");
        let assistant_update = events
            .iter()
            .position(|event| {
                matches!(
                    event.kind,
                    AgentEventKind::MessageUpdate {
                        message: crate::state::AgentMessage::Assistant { .. },
                        ..
                    }
                )
            })
            .expect("assistant message updates");
        let assistant_end = events
            .iter()
            .position(|event| {
                matches!(
                    event.kind,
                    AgentEventKind::MessageEnd {
                        message: crate::state::AgentMessage::Assistant { .. }
                    }
                )
            })
            .expect("assistant message ends");
        let turn_end = events
            .iter()
            .position(|event| matches!(event.kind, AgentEventKind::TurnEnd { .. }))
            .expect("turn ends");
        assert!(user_start < user_end);
        assert!(user_end < assistant_start);
        assert!(assistant_start < assistant_update);
        assert!(assistant_update < assistant_end);
        assert!(assistant_end < turn_end);

        let snapshot = agent.snapshot();
        assert_eq!(snapshot.phase, AgentPhase::Idle);
        assert!(!snapshot.is_streaming);
        assert_eq!(snapshot.messages.len(), 2);
        assert!(matches!(
            snapshot.messages[1],
            crate::state::AgentMessage::Assistant { .. }
        ));

        Ok::<(), CoreError>(())
    })
    .expect("text run should settle");
}

#[test]
fn provider_failure_settles_the_agent_before_returning_the_error() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(Arc::new(FailingProvider))
            .build();
        let run = agent.start_prompt("trigger provider failure")?;

        let error = run.drive().await.expect_err("provider must fail");

        assert!(matches!(error, CoreError::ModelProvider { .. }));
        let events = run.events();
        assert!(matches!(
            events.first().map(|event| &event.kind),
            Some(AgentEventKind::AgentStart)
        ));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(AgentEventKind::AgentEnd { .. })
        ));
        let assistant_start = events
            .iter()
            .position(|event| {
                matches!(
                    event.kind,
                    AgentEventKind::MessageStart {
                        message: crate::state::AgentMessage::Assistant { .. }
                    }
                )
            })
            .expect("failed assistant message starts");
        let assistant_end = events
            .iter()
            .position(|event| {
                matches!(
                    event.kind,
                    AgentEventKind::MessageEnd {
                        message: crate::state::AgentMessage::Assistant { .. }
                    }
                )
            })
            .expect("failed assistant message ends");
        let turn_end = events
            .iter()
            .position(|event| matches!(event.kind, AgentEventKind::TurnEnd { .. }))
            .expect("failed turn ends");
        assert!(assistant_start < assistant_end);
        assert!(assistant_end < turn_end);
        let snapshot = agent.snapshot();
        assert_eq!(snapshot.phase, AgentPhase::Idle);
        assert!(!snapshot.is_streaming);
        assert!(snapshot.pending_tool_calls.is_empty());
        assert!(snapshot.last_error.is_some());
        assert!(matches!(
            snapshot.messages.last(),
            Some(crate::state::AgentMessage::Assistant { content, .. }) if content.is_empty()
        ));

        Ok::<(), CoreError>(())
    })
    .expect("failure settlement should not leave an active agent");
}

#[test]
fn explicit_model_error_preserves_the_assistant_failure_without_synthesizing_transport_error() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("partial".into()),
                ModelStreamEvent::Error {
                    message: "model refused the request".into(),
                },
            ],
        }]));
        let agent = Agent::builder().model_provider(provider).build();
        let run = agent.start_prompt("trigger a model error")?;

        let error = run
            .drive()
            .await
            .expect_err("model error must fail the run");
        assert_eq!(
            error,
            CoreError::ModelError {
                message: "model refused the request".into()
            }
        );
        assert_eq!(run.snapshot().phase, crate::state::RunPhase::Failed);
        assert_eq!(run.snapshot().stop_reason, Some(StopReason::Error));
        assert!(matches!(
            agent.snapshot().messages.last(),
            Some(crate::state::AgentMessage::Assistant {
                content,
                stop_reason: Some(StopReason::Error),
                error_message: Some(error_message),
                ..
            }) if content == "partial" && error_message == "model refused the request"
        ));
        assert!(matches!(
            run.events().last().map(|event| &event.kind),
            Some(AgentEventKind::AgentEnd { .. })
        ));
        assert_eq!(
            agent.snapshot().last_error.as_deref(),
            Some("model refused the request")
        );

        Ok::<(), CoreError>(())
    })
    .expect("explicit model error should settle without a duplicate failure message");
}

#[test]
fn explicit_model_abort_preserves_the_provider_diagnostic_without_host_cancellation() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([ModelStream {
            events: vec![ModelStreamEvent::Aborted {
                message: "provider stopped the response".into(),
            }],
        }]));
        let agent = Agent::builder().model_provider(provider).build();
        let run = agent.start_prompt("trigger an independent provider abort")?;

        assert_eq!(
            run.drive().await,
            Err(CoreError::ModelAborted {
                message: "provider stopped the response".into(),
            })
        );
        assert_eq!(run.snapshot().phase, crate::state::RunPhase::Failed);
        assert!(matches!(
            agent.snapshot().messages.last(),
            Some(crate::state::AgentMessage::Assistant {
                stop_reason: Some(StopReason::Aborted),
                error_message: Some(message),
                ..
            }) if message == "provider stopped the response"
        ));
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);

        Ok::<(), CoreError>(())
    })
    .expect("a provider abort must remain distinct from caller cancellation");
}

#[test]
fn length_stop_refuses_truncated_tool_calls_and_allows_a_recovery_turn() {
    smol::block_on(async {
        let call_id = ToolCallId::new("call_truncated").expect("non-empty provider ID");
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("partial tool request".into()),
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: call_id.clone(),
                        name: "echo".into(),
                        arguments: SerializedJson::new(r#"{"text":"hello"}"#),
                    }),
                    ModelStreamEvent::End(StopReason::Length),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("recovered after truncation".into()),
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
        let run = agent.start_prompt("call echo, but truncate the response")?;

        run.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        assert!(executed.lock().expect("test tool mutex").is_empty());
        assert!(matches!(
            agent.snapshot().messages[1],
            crate::state::AgentMessage::Assistant {
                stop_reason: Some(StopReason::Length),
                ref tool_calls,
                ..
            } if tool_calls.len() == 1 && tool_calls[0].id == call_id
        ));
        assert!(matches!(
            agent.snapshot().messages[2],
            crate::state::AgentMessage::ToolResult {
                ref tool_call_id,
                is_error: true,
                ref content,
                ..
            } if tool_call_id == &call_id && content.contains("output token limit")
        ));
        assert!(run.events().iter().any(|event| {
            matches!(
                event.kind,
                AgentEventKind::TurnEnd {
                    reason: StopReason::Length,
                    ..
                }
            )
        }));
        assert!(matches!(
            agent.snapshot().messages.last(),
            Some(crate::state::AgentMessage::Assistant { content, stop_reason: Some(StopReason::Stop), .. })
                if content == "recovered after truncation"
        ));

        Ok::<(), CoreError>(())
    })
    .expect("length stop should produce an error tool result and continue");
}

#[test]
fn cancellation_settles_terminal_events_before_wait_for_idle() {
    smol::block_on(async {
        let observer_agent = Arc::new(Mutex::new(None));
        let agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .observer(Arc::new(AbortOnAgentStartObserver {
                agent: Arc::clone(&observer_agent),
            }))
            .build();
        *observer_agent.lock().expect("test agent mutex") = Some(agent.clone());
        let run = agent.start_prompt("cancel through an awaited observer")?;

        let error = run
            .drive()
            .await
            .expect_err("observer requested cancellation");

        assert_eq!(error, CoreError::Cancelled);
        assert_eq!(run.snapshot().phase, crate::state::RunPhase::Cancelled);
        let events = run.events();
        assert!(matches!(events[0].kind, AgentEventKind::AgentStart));
        assert!(matches!(
            events[events.len() - 2].kind,
            AgentEventKind::TurnEnd {
                reason: StopReason::Aborted,
                ..
            }
        ));
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(AgentEventKind::AgentEnd { .. })
        ));
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);

        agent.wait_for_idle().await;

        Ok::<(), CoreError>(())
    })
    .expect("cancelled run must settle before wait_for_idle resolves");
}

#[test]
fn cancellation_during_an_async_before_hook_preserves_tool_result_then_allows_reuse() {
    smol::block_on(async {
        let call_id = ToolCallId::new("call_cancelled_before_tool").expect("non-empty ID");
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
                events: vec![ModelStreamEvent::End(StopReason::Aborted)],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("reused normally".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let executed = Arc::new(Mutex::new(Vec::new()));
        let hook_agent = Arc::new(Mutex::new(None));
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .hooks(Arc::new(AbortDuringBeforeToolHook {
                agent: Arc::clone(&hook_agent),
            }))
            .tool(Arc::new(EchoTool {
                calls: Arc::clone(&executed),
                schema: tea_protocol::JsonValue::parse(
                    r#"{"type":"object","required":["text"]}"#,
                )
                .expect("test schema is valid JSON"),
            }))
            .build();
        *hook_agent.lock().expect("test agent mutex") = Some(agent.clone());

        let cancelled = agent.start_prompt("cancel at the policy boundary")?;
        assert_eq!(cancelled.drive().await, Err(CoreError::Cancelled));
        assert!(executed.lock().expect("test tool mutex").is_empty());
        assert_eq!(
            cancelled.snapshot().phase,
            crate::state::RunPhase::Cancelled
        );
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
        assert!(agent.snapshot().pending_tool_calls.is_empty());
        let cancellation_events = cancelled.events();
        assert!(cancellation_events.iter().any(|event| {
            matches!(
                &event.kind,
                AgentEventKind::ToolExecutionEnd { result, .. }
                    if result.is_error && result.content == "Operation aborted"
            )
        }));
        assert!(matches!(
            cancellation_events.last().map(|event| &event.kind),
            Some(AgentEventKind::AgentEnd { .. })
        ));

        let reused = agent.start_prompt("reuse after cancellation")?;
        reused.drive().await?;
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
        assert!(matches!(
            agent.snapshot().messages.last(),
            Some(crate::state::AgentMessage::Assistant { content, .. }) if content == "reused normally"
        ));
        assert_eq!(provider.requests().len(), 3);

        Ok::<(), CoreError>(())
    })
    .expect("the agent should be reusable after a cancellation-aware hook");
}

#[test]
fn explicit_abort_settles_the_run_handle() {
    let agent = Agent::builder().build();
    let run = agent.start_prompt("cancel me").expect("run should start");
    agent.abort();
    assert_eq!(run.snapshot().phase, crate::state::RunPhase::Cancelled);
    assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
}

#[test]
fn reset_is_idle_only_and_clears_retained_state_and_queues() {
    let agent = Agent::builder()
        .host_message(SerializedJson::new("retained host value"))
        .build();
    agent
        .enqueue_steering("queued steering")
        .expect("idle queueing is explicit and allowed");
    agent
        .enqueue_follow_up("queued follow-up")
        .expect("idle queueing is explicit and allowed");
    let run = agent
        .start_prompt("retained until reset")
        .expect("run should start");

    assert!(matches!(agent.reset(), Err(CoreError::ActiveRun { .. })));
    agent.abort();
    assert_eq!(run.snapshot().phase, crate::state::RunPhase::Cancelled);
    agent.reset().expect("idle reset should succeed");

    let snapshot = agent.snapshot();
    assert!(snapshot.messages.is_empty());
    assert!(snapshot.host_messages.is_empty());
    assert!(snapshot.last_error.is_none());
    assert!(!snapshot.is_streaming);
    assert!(snapshot.pending_tool_calls.is_empty());
    assert!(!agent.has_queued_messages());
}
