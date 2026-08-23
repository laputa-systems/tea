use super::super::*;
use crate::agent::AgentConfiguration;
use crate::tool::ToolRegistry;

struct ConfigurationTool {
    name: &'static str,
}

impl AgentTool for ConfigurationTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "configuration reload fixture"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        static SCHEMA: std::sync::OnceLock<tea_protocol::JsonValue> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| tea_protocol::JsonValue::parse(r#"{"type":"object"}"#).unwrap())
    }

    fn execute<'a>(
        &'a self,
        _call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Err(
            crate::error::ToolError::Execution {
                tool: self.name.into(),
                message: "configuration fixture is not executable".into(),
            },
        )))
    }
}

struct MarkerHooks {
    marker: &'static str,
}

impl HookSet for MarkerHooks {
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

    fn convert_to_llm(&self, _context: ContextEnvelope) -> Result<String, crate::error::HookError> {
        Ok(self.marker.into())
    }
}

#[test]
fn idle_configuration_replacement_is_atomic_and_preserves_agent_state() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
        ]));
        let model = ModelDescriptor {
            provider: "fixture".into(),
            model: "fixed-model".into(),
            revision: None,
        };
        let agent = Agent::builder()
            .system_prompt("old prompt")
            .model(model.clone())
            .model_provider(provider.clone())
            .tool(Arc::new(ConfigurationTool { name: "old_tool" }))
            .hooks(Arc::new(MarkerHooks { marker: "old hook" }))
            .build();

        agent.start_prompt("first")?.drive().await?;
        agent.enqueue_steering("keep steering")?;
        agent.enqueue_follow_up("keep follow-up")?;
        let retained_messages = agent.snapshot().messages;
        let queue_lengths = (
            agent.queue_snapshot().steering.len(),
            agent.queue_snapshot().follow_up.len(),
        );

        let mut tools = ToolRegistry::default();
        tools.insert(Arc::new(ConfigurationTool { name: "new_tool" }));
        agent.replace_configuration(AgentConfiguration::new(
            "new prompt",
            tools,
            Arc::new(MarkerHooks { marker: "new hook" }),
        ))?;

        let snapshot = agent.snapshot();
        assert_eq!(snapshot.system_prompt, "new prompt");
        assert_eq!(snapshot.model, Some(model));
        assert_eq!(snapshot.messages, retained_messages);
        assert_eq!(
            (
                agent.queue_snapshot().steering.len(),
                agent.queue_snapshot().follow_up.len()
            ),
            queue_lengths
        );
        assert_eq!(agent.tool_definitions()[0].name, "new_tool");

        // Drain the preserved queues so the second request remains a single-turn
        // check of the newly installed prompt, tool registry, and hook set.
        agent.clear_all_queues();
        agent.start_prompt("second")?.drive().await?;
        let requests = provider.requests();
        assert_eq!(requests[0].system_prompt, "old prompt");
        assert_eq!(requests[0].tools[0].name, "old_tool");
        assert_eq!(requests[0].context, "old hook");
        assert_eq!(requests[1].system_prompt, "new prompt");
        assert_eq!(requests[1].tools[0].name, "new_tool");
        assert_eq!(requests[1].context, "new hook");

        Ok::<(), CoreError>(())
    })
    .expect("idle configuration replacement must preserve state and affect the next run");
}

#[test]
fn configuration_replacement_is_rejected_for_an_active_run() {
    let agent = Agent::builder().build();
    let active = agent.start_prompt("active").expect("run starts");
    let error = agent
        .replace_configuration(AgentConfiguration::new(
            "replacement",
            ToolRegistry::default(),
            Arc::new(crate::hooks::NoHooks),
        ))
        .expect_err("active run owns its configuration snapshot");
    assert!(matches!(error, CoreError::ActiveRun { .. }));
    assert_eq!(agent.snapshot().system_prompt, "");
    active.abort().expect("created run aborts cleanly");
}

#[test]
fn idle_provider_replacement_preserves_history_and_changes_the_next_request() {
    smol::block_on(async {
        let first_provider = Arc::new(ScriptedProvider::new([ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("first response".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }]));
        let second_provider = Arc::new(ScriptedProvider::new([ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("second response".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }]));
        let initial_model = ModelDescriptor {
            provider: "fixture".into(),
            model: "initial".into(),
            revision: None,
        };
        let replacement_model = ModelDescriptor {
            provider: "fixture".into(),
            model: "replacement".into(),
            revision: Some("pinned".into()),
        };
        let agent = Agent::builder()
            .model(initial_model)
            .model_provider(first_provider)
            .build();

        agent.start_prompt("first")?.drive().await?;
        let retained_messages = agent.snapshot().messages;

        agent.replace_model_provider(replacement_model.clone(), second_provider.clone())?;
        assert_eq!(agent.snapshot().messages, retained_messages);
        assert_eq!(agent.snapshot().model, Some(replacement_model.clone()));

        agent.start_prompt("second")?.drive().await?;
        assert_eq!(second_provider.requests().len(), 1);
        assert_eq!(second_provider.requests()[0].model, Some(replacement_model));

        Ok::<(), CoreError>(())
    })
    .expect("idle replacement must retain the linear transcript and select the new provider");
}

#[test]
fn provider_replacement_is_rejected_while_a_run_is_owned() {
    let agent = Agent::builder()
        .model_provider(Arc::new(TextOnlyProvider))
        .build();
    let active = agent.start_prompt("active").expect("run starts");
    let error = agent
        .replace_model_provider(
            ModelDescriptor {
                provider: "fixture".into(),
                model: "replacement".into(),
                revision: None,
            },
            Arc::new(TextOnlyProvider),
        )
        .expect_err("an active run owns its model/provider pair");
    assert!(matches!(error, CoreError::ActiveRun { .. }));
    active.abort().expect("created run aborts cleanly");
    assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
}

#[test]
fn restoring_messages_is_idle_only_and_advances_message_ids() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .build();
        agent
            .restore_messages(vec![AgentMessage::User {
                id: MessageId(40),
                content: "restored".into(),
            }])
            .expect("valid linear messages restore while idle");
        agent
            .replace_thinking_level(ThinkingLevel::High)
            .expect("thinking level replacement is idle-only");
        assert_eq!(agent.snapshot().thinking_level, ThinkingLevel::High);
        assert_eq!(agent.snapshot().messages.len(), 1);

        agent.start_prompt("next")?.drive().await?;
        let snapshot = agent.snapshot();
        let next_id = snapshot
            .messages
            .iter()
            .map(|message| match message {
                AgentMessage::User { id, .. }
                | AgentMessage::Assistant { id, .. }
                | AgentMessage::ToolResult { id, .. } => id.0,
            })
            .max()
            .expect("restored and new messages remain present");
        assert!(next_id > 40);

        let active = agent.start_prompt("active")?;
        let error = agent
            .restore_messages(Vec::new())
            .expect_err("active run owns the canonical transcript");
        assert!(matches!(error, CoreError::ActiveRun { .. }));
        active.abort().expect("created run aborts cleanly");
        Ok::<(), CoreError>(())
    })
    .expect("restoring a session must preserve idle-only ownership");
}

#[test]
fn agent_allows_one_run_and_drop_settles_cancellation() {
    let agent = Agent::builder().build();
    let run = agent.start_prompt("first").expect("first run should start");
    assert!(matches!(agent.snapshot().phase, AgentPhase::Running(_)));

    let second = agent.start_prompt("second");
    assert!(matches!(second, Err(CoreError::ActiveRun { .. })));

    drop(run);
    assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
    assert!(!agent.snapshot().is_streaming);
    assert!(agent.snapshot().pending_tool_calls.is_empty());
}

#[test]
fn generated_run_message_and_event_ids_are_monotonic_after_cancellation() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .build();
        let cancelled = agent.start_prompt("cancel before driving")?;
        assert_eq!(cancelled.id(), crate::state::RunId(1));
        agent.abort();
        assert_eq!(
            cancelled.snapshot().phase,
            crate::state::RunPhase::Cancelled
        );

        let completed = agent.start_prompt("assign stable identifiers")?;
        assert_eq!(completed.id(), crate::state::RunId(2));
        completed.drive().await?;

        let events = completed.events();
        assert!(events.iter().all(|event| event.run_id == completed.id()));
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence.0)
                .collect::<Vec<_>>(),
            (1..=events.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, AgentEventKind::AgentEnd { .. }))
                .count(),
            1
        );
        let snapshot = agent.snapshot();
        let ids = snapshot
            .messages
            .iter()
            .map(|message| match message {
                crate::state::AgentMessage::User { id, .. }
                | crate::state::AgentMessage::Assistant { id, .. }
                | crate::state::AgentMessage::ToolResult { id, .. } => id.0,
            })
            .collect::<Vec<_>>();
        // The cancelled prompt is retained as message 1; the next prompt and
        // its response receive fresh IDs rather than reusing that durable
        // transcript entry.
        assert_eq!(ids, vec![1, 2, 3]);

        Ok::<(), CoreError>(())
    })
    .expect("v1 correlation identifiers must not be reused after cancellation");
}

#[test]
fn deterministic_scale_creates_and_settles_one_thousand_isolated_agents() {
    smol::block_on(async {
        for index in 0..1_000 {
            let agent = Agent::builder()
                .model_provider(Arc::new(TextOnlyProvider))
                .build();
            let run = agent.start_prompt(format!("scale fixture {index}"))?;
            run.drive().await?;

            let snapshot = agent.snapshot();
            assert_eq!(snapshot.phase, AgentPhase::Idle);
            assert!(!snapshot.is_streaming);
            assert!(snapshot.pending_tool_calls.is_empty());
            assert_eq!(snapshot.messages.len(), 2);
        }
        Ok::<(), CoreError>(())
    })
    .expect("independent agents must settle without hidden shared runtime state");
}

#[test]
fn concurrent_run_claims_admit_exactly_one_owner_and_leave_no_stale_active_run() {
    for _ in 0..100 {
        let agent = Agent::builder().build();
        let start_barrier = Arc::new(std::sync::Barrier::new(3));
        let claim_barrier = Arc::new(std::sync::Barrier::new(3));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for prompt in ["left", "right"] {
                let agent = agent.clone();
                let start_barrier = Arc::clone(&start_barrier);
                let claim_barrier = Arc::clone(&claim_barrier);
                let outcomes = Arc::clone(&outcomes);
                scope.spawn(move || {
                    start_barrier.wait();
                    let run = agent.start_prompt(prompt);
                    let outcome = match &run {
                        Ok(_) => "owner",
                        Err(CoreError::ActiveRun { .. }) => "busy",
                        Err(error) => panic!("unexpected concurrent start error: {error}"),
                    };
                    outcomes.lock().expect("test outcomes mutex").push(outcome);
                    // Keep the winning handle alive until both competing
                    // claims have completed, rather than letting a fast drop
                    // create a new legal idle boundary for the other call.
                    claim_barrier.wait();
                    drop(run);
                });
            }
            start_barrier.wait();
            claim_barrier.wait();
        });
        let outcomes = outcomes.lock().expect("test outcomes mutex");
        assert_eq!(
            outcomes
                .iter()
                .filter(|&&outcome| outcome == "owner")
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|&&outcome| outcome == "busy")
                .count(),
            1
        );
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
    }
}

#[test]
fn tool_call_identifier_preserves_provider_text_and_rejects_empty_values() {
    let id = ToolCallId::new("provider-tool-call_42").expect("non-empty provider ID");

    assert_eq!(id.as_str(), "provider-tool-call_42");
    assert!(ToolCallId::new("").is_err());
}
