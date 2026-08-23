use super::*;

/// Records the exact relationship between the injected boundary and the
/// caller-owned effect implementation.  The test deliberately uses the real
/// run loop instead of a separate scheduler so a gate cannot accidentally
/// become a post-hoc observer.
#[derive(Debug)]
struct OrderingGate {
    trace: Arc<Mutex<Vec<String>>>,
}

impl OrderingGate {
    fn record(&self, prefix: &str, action: &EffectAction) {
        let label = match action.subject() {
            EffectSubject::ProviderRequest { .. } => "provider",
            EffectSubject::ToolExecution { .. } => "tool",
            _ => return,
        };
        self.trace
            .lock()
            .expect("effect order trace mutex")
            .push(format!("{prefix}-{label}"));
    }
}

impl EffectGate for OrderingGate {
    fn before<'a>(&'a self, action: EffectAction) -> EffectFuture<'a> {
        self.record("before", &action);
        Box::pin(std::future::ready(Ok(())))
    }

    fn after<'a>(&'a self, action: EffectAction, _outcome: EffectOutcome) -> EffectFuture<'a> {
        self.record("after", &action);
        Box::pin(std::future::ready(Ok(())))
    }
}

#[derive(Debug)]
struct OrderedProvider {
    trace: Arc<Mutex<Vec<String>>>,
    streams: Mutex<VecDeque<ModelStream>>,
}

impl ModelProvider for OrderedProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.trace
            .lock()
            .expect("effect order trace mutex")
            .push("provider".into());
        let stream = self
            .streams
            .lock()
            .expect("ordered provider stream mutex")
            .pop_front()
            .expect("fixture has exactly two provider responses");
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[derive(Debug)]
struct OrderedTool {
    trace: Arc<Mutex<Vec<String>>>,
    schema: tea_protocol::JsonValue,
}

impl AgentTool for OrderedTool {
    fn name(&self) -> &str {
        "ordered"
    }

    fn description(&self) -> &str {
        "records when the real capability starts"
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
        self.trace
            .lock()
            .expect("effect order trace mutex")
            .push("tool".into());
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "settled".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

#[test]
fn effect_gate_commits_intent_before_provider_and_tool_effects() {
    smol::block_on(async {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(OrderedProvider {
            trace: Arc::clone(&trace),
            streams: Mutex::new(VecDeque::from([
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("ordered-call").expect("fixture call ID"),
                            name: "ordered".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ])),
        });
        let agent = Agent::builder()
            .model_provider(provider)
            .effect_gate(Arc::new(OrderingGate {
                trace: Arc::clone(&trace),
            }))
            .tool(Arc::new(OrderedTool {
                trace: Arc::clone(&trace),
                schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                    .expect("fixture schema"),
            }))
            .build();

        agent.start_prompt("exercise durable boundary")?.drive().await?;

        assert_eq!(
            *trace.lock().expect("effect order trace mutex"),
            vec![
                "before-provider",
                "provider",
                "after-provider",
                "before-tool",
                "tool",
                "after-tool",
                "before-provider",
                "provider",
                "after-provider",
            ]
        );

        Ok::<(), CoreError>(())
    })
    .expect("real effects must remain inside the injected boundary");
}

#[test]
fn manual_effect_gate_parks_the_real_run_before_each_provider_and_tool_boundary() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(ManualEffectGate::default());
    let provider = Arc::new(OrderedProvider {
        trace: Arc::clone(&trace),
        streams: Mutex::new(VecDeque::from([
            ModelStream {
                events: vec![
                    ModelStreamEvent::ToolCall(AgentToolCall {
                        id: ToolCallId::new("manual-call").expect("fixture call ID"),
                        name: "ordered".into(),
                        arguments: SerializedJson::new("{}"),
                    }),
                    ModelStreamEvent::End(StopReason::ToolUse),
                ],
            },
            ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Stop)],
            },
        ])),
    });
    let agent = Agent::builder()
        .model_provider(provider)
        .effect_gate(gate.clone())
        .tool(Arc::new(OrderedTool {
            trace: Arc::clone(&trace),
            schema: tea_protocol::JsonValue::parse(r#"{"type":"object"}"#)
                .expect("fixture schema"),
        }))
        .build();
    let run = Arc::new(agent.start_prompt("drive one effect at a time").expect("run starts"));
    let executor = smol::Executor::new();
    let driving_run = Arc::clone(&run);
    let drive = executor.spawn(async move { driving_run.drive().await });

    assert!(executor.try_tick());
    let first = gate.peek_action().expect("first hook action is parked");
    assert_eq!(gate.peek_action(), Some(first.clone()));
    assert!(trace.lock().expect("effect order trace mutex").is_empty());

    let mut saw_provider_before = false;
    let mut saw_provider_after = false;
    let mut saw_tool_before = false;
    let mut saw_tool_after = false;
    let mut provider_before_count = 0;
    let mut tool_before_count = 0;
    loop {
        while executor.try_tick() {}
        let Some(action) = gate.peek_action() else {
            break;
        };
        match (action.phase, action.action.subject()) {
            (EffectPhase::Before, EffectSubject::ProviderRequest { .. }) => {
                saw_provider_before = true;
                provider_before_count += 1;
                assert!(
                    trace
                        .lock()
                        .expect("effect order trace mutex")
                        .iter()
                        .filter(|item| item.as_str() == "provider")
                        .count()
                        < provider_before_count,
                    "provider dispatch must not occur while its before action is parked"
                );
            }
            (EffectPhase::After, EffectSubject::ProviderRequest { .. }) => {
                saw_provider_after = true;
                assert!(
                    trace
                        .lock()
                        .expect("effect order trace mutex")
                        .iter()
                        .any(|item| item == "provider"),
                    "provider settlement is parked only after dispatch"
                );
            }
            (EffectPhase::Before, EffectSubject::ToolExecution { .. }) => {
                saw_tool_before = true;
                tool_before_count += 1;
                assert!(
                    trace
                        .lock()
                        .expect("effect order trace mutex")
                        .iter()
                        .filter(|item| item.as_str() == "tool")
                        .count()
                        < tool_before_count,
                    "tool execution must not occur while its before action is parked"
                );
            }
            (EffectPhase::After, EffectSubject::ToolExecution { .. }) => {
                saw_tool_after = true;
                assert!(
                    trace
                        .lock()
                        .expect("effect order trace mutex")
                        .iter()
                        .any(|item| item == "tool"),
                    "tool settlement is parked only after execution"
                );
            }
            _ => {}
        }
        smol::block_on(gate.execute_action(action.id)).expect("exact pending action releases");
    }

    assert_eq!(smol::block_on(drive), Ok(()));
    assert!(saw_provider_before && saw_provider_after);
    assert!(saw_tool_before && saw_tool_after);
}
