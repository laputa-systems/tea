use super::super::*;

#[test]
fn prepared_next_turn_context_survives_a_tool_continuation_without_queued_input() {
    smol::block_on(async {
        let call_id = ToolCallId::new("call_context_replacement").expect("non-empty provider ID");
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: call_id,
                        name: "echo".into(),
                        arguments: SerializedJson::new(r#"{"text":"hello"}"#),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("continued with replacement context".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let agent = Agent::builder()
            .model(ModelDescriptor {
                provider: "initial-provider".into(),
                model: "initial-model".into(),
                revision: None,
            })
            .thinking_level(ThinkingLevel::Low)
            .model_provider(provider.clone())
            .hooks(Arc::new(ReplacementContextHooks))
            .tool(Arc::new(EchoTool {
                calls: Arc::new(Mutex::new(Vec::new())),
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object","required":["text"]}"#)
                    .expect("test schema is valid JSON"),
            }))
            .build();
        let run = agent.start_prompt("echo with replaced continuation context")?;

        run.drive().await?;

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].model.as_ref().map(|model| model.model.as_str()),
            Some("initial-model")
        );
        assert_eq!(requests[0].thinking_level, ThinkingLevel::Low);
        assert_eq!(
            requests[1].transcript.host_notes.last().map(String::as_str),
            Some("replacement-context")
        );
        assert_eq!(
            requests[1].model.as_ref().map(|model| model.model.as_str()),
            Some("replacement-model")
        );
        assert_eq!(requests[1].thinking_level, ThinkingLevel::High);

        Ok::<(), CoreError>(())
    })
    .expect("prepared context should reach the next model request");
}

#[test]
fn host_only_context_is_explicit_and_reaches_only_the_converter_hook() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("converted host context".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }]));
        let agent = Agent::builder()
            .host_message(SerializedJson::new("host-only-value"))
            .hooks(Arc::new(ReplacementContextHooks))
            .model_provider(provider.clone())
            .build();
        let run = agent.start_prompt("use host context")?;

        run.drive().await?;

        assert_eq!(
            provider.requests()[0].transcript.host_notes,
            ["host-only-value"]
        );
        assert_eq!(
            agent.snapshot().host_messages,
            [SerializedJson::new("host-only-value")]
        );

        Ok::<(), CoreError>(())
    })
    .expect("host-only context should remain an explicit hook capability");
}

#[test]
fn steering_queue_drains_one_at_a_time_before_initial_and_later_turns() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("first response".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("second response".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let agent = Agent::builder().model_provider(provider.clone()).build();
        agent.enqueue_steering("first steering")?;
        agent.enqueue_steering("second steering")?;
        let run = agent.start_prompt("initial prompt")?;

        run.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        let messages = agent.snapshot().messages;
        let user_contents = messages
            .iter()
            .filter_map(|message| match message {
                crate::state::AgentMessage::User { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            user_contents,
            ["initial prompt", "first steering", "second steering"]
        );
        let turn_starts = run
            .events()
            .into_iter()
            .filter(|event| matches!(event.kind, AgentEventKind::TurnStart { .. }))
            .count();
        assert_eq!(turn_starts, 2);
        assert!(!agent.has_queued_messages());

        Ok::<(), CoreError>(())
    })
    .expect("one-at-a-time steering should produce one additional model turn");
}

#[test]
fn follow_up_queue_extends_a_run_only_at_the_idle_boundary() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("first response".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("follow-up response".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let agent = Agent::builder().model_provider(provider.clone()).build();
        agent.enqueue_follow_up("follow-up input")?;
        let run = agent.start_prompt("initial prompt")?;

        run.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        assert!(matches!(
            agent.snapshot().messages[2],
            crate::state::AgentMessage::User { ref content, .. } if content == "follow-up input"
        ));
        assert!(matches!(
            agent.snapshot().messages[3],
            crate::state::AgentMessage::Assistant { ref content, .. } if crate::state::assistant_text(content) == "follow-up response"
        ));
        assert!(!agent.has_queued_messages());

        Ok::<(), CoreError>(())
    })
    .expect("follow-up should be injected only after a normal stopping turn");
}

#[test]
fn continue_requires_a_non_assistant_tail_unless_queue_input_is_available() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("first response".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("continued response".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let agent = Agent::builder().model_provider(provider.clone()).build();
        let first = agent.start_prompt("initial prompt")?;
        first.drive().await?;

        assert!(matches!(
            agent.start_continue(),
            Err(CoreError::InvalidTransition(error)) if error.from == "assistant-tail"
        ));
        agent.enqueue_steering("queued continuation")?;
        let continued = agent.start_continue()?;
        continued.drive().await?;

        assert_eq!(provider.requests().len(), 2);
        assert!(matches!(
            agent.snapshot().messages[2],
            crate::state::AgentMessage::User { ref content, .. } if content == "queued continuation"
        ));
        let event_kinds = continued
            .events()
            .into_iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        assert!(matches!(event_kinds[0], AgentEventKind::AgentStart));
        assert!(matches!(event_kinds[1], AgentEventKind::TurnStart { .. }));
        assert!(matches!(
            event_kinds[2],
            AgentEventKind::MessageStart { .. }
        ));
        assert!(matches!(event_kinds[3], AgentEventKind::MessageEnd { .. }));
        assert!(matches!(
            event_kinds.last(),
            Some(AgentEventKind::AgentEnd { messages })
                if messages.len() == 2
                    && matches!(messages[0], crate::state::AgentMessage::User { ref content, .. } if content == "queued continuation")
                    && matches!(messages[1], crate::state::AgentMessage::Assistant { ref content, .. } if crate::state::assistant_text(content) == "continued response")
        ));

        Ok::<(), CoreError>(())
    })
    .expect("queued continuation input should recover from an assistant transcript tail");
}

#[test]
fn queue_modes_can_change_without_reconstructing_the_agent() {
    let agent = Agent::builder().build();
    assert_eq!(agent.steering_mode(), crate::queue::QueueMode::OneAtATime);
    assert_eq!(agent.follow_up_mode(), crate::queue::QueueMode::OneAtATime);

    agent.set_steering_mode(crate::queue::QueueMode::All);
    agent.set_follow_up_mode(crate::queue::QueueMode::All);

    assert_eq!(agent.steering_mode(), crate::queue::QueueMode::All);
    assert_eq!(agent.follow_up_mode(), crate::queue::QueueMode::All);
}

#[test]
fn queue_snapshot_exposes_core_owned_prompt_order_without_mutation() {
    let agent = Agent::builder().build();
    agent
        .enqueue_steering("current turn")
        .expect("steering queues");
    agent
        .enqueue_follow_up("next turn")
        .expect("follow-up queues");

    let snapshot = agent.queue_snapshot();
    assert_eq!(snapshot.steering.snapshot()[0].content, "current turn");
    assert_eq!(snapshot.follow_up.snapshot()[0].content, "next turn");
    snapshot.steering.clone().clear();
    assert!(agent.has_queued_messages());
}

#[test]
fn queue_mode_preserves_insertion_order() {
    let mut queue = crate::queue::SteeringQueue::default();
    assert_eq!(queue.push("a"), 1);
    assert_eq!(queue.push("b"), 2);
    assert_eq!(queue.drain(crate::queue::QueueMode::OneAtATime).len(), 1);
    assert_eq!(
        queue
            .drain(crate::queue::QueueMode::All)
            .into_iter()
            .map(|item| item.content)
            .collect::<Vec<_>>(),
        vec!["b"]
    );
}
