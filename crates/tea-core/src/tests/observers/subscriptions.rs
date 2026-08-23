use super::super::*;

#[test]
fn trace_observer_does_not_change_observable_agent_behavior() {
    smol::block_on(async {
        let untraced = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .build();
        let untraced_run = untraced.start_prompt("trace identity")?;
        untraced_run.drive().await?;

        let json_trace = Arc::new(crate::trace::TraceObserver::new(
            "trace-jsonl",
            tea_trace::JsonLinesSink::new(Vec::new()),
        ));
        let json_traced = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .observer(json_trace.clone())
            .build();
        let json_run = json_traced.start_prompt("trace identity")?;
        json_run.drive().await?;

        let cbor_trace = Arc::new(crate::trace::TraceObserver::new(
            "trace-cbor",
            tea_trace::CborSink::new(Vec::new()),
        ));
        let cbor_traced = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .observer(cbor_trace.clone())
            .build();
        let cbor_run = cbor_traced.start_prompt("trace identity")?;
        cbor_run.drive().await?;

        for (events, snapshot) in [
            (json_run.events(), json_traced.snapshot()),
            (cbor_run.events(), cbor_traced.snapshot()),
        ] {
            assert_eq!(events, untraced_run.events());
            assert_eq!(snapshot, untraced.snapshot());
        }
        json_trace.with_sink(|sink| {
            let text = std::str::from_utf8(sink.inner()).expect("trace JSONL is UTF-8");
            assert!(text.contains(r#""type":"episode_header""#));
            assert!(text.contains(r#""type":"episode_end""#));
        });
        cbor_trace.with_sink(|sink| assert!(!sink.inner().is_empty()));

        Ok::<(), CoreError>(())
    })
    .expect("tracing must be observational only");
}

#[test]
fn awaited_observer_receives_each_reduced_event_in_source_order() {
    smol::block_on(async {
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .observer(Arc::new(RecordingObserver {
                events: Arc::clone(&observed),
            }))
            .build();
        let run = agent.start_prompt("observe the lifecycle")?;

        run.drive().await?;

        let emitted = run.events();
        let observed = observed.lock().expect("test observer mutex").clone();
        assert_eq!(observed, emitted);
        assert!(matches!(
            observed.last().map(|event| &event.kind),
            Some(AgentEventKind::AgentEnd { .. })
        ));

        Ok::<(), CoreError>(())
    })
    .expect("observer should settle with the run");
}

#[test]
fn runtime_subscription_is_reentrant_and_drop_unsubscribes_for_future_events() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("first run".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("second run".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer_agent = Arc::new(Mutex::new(None));
        let subscriptions = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent::builder()
            .model_provider(provider)
            .observer(Arc::new(SubscribeOnAgentStartObserver {
                agent: Arc::clone(&observer_agent),
                observed: Arc::clone(&observed),
                subscriptions: Arc::clone(&subscriptions),
                subscribed: AtomicBool::new(false),
            }))
            .build();
        *observer_agent.lock().expect("test agent mutex") = Some(agent.clone());

        let first = agent.start_prompt("first")?;
        first.drive().await?;
        let first_events = first.events();
        let observed_after_first = observed.lock().expect("test observer mutex").clone();
        assert_eq!(observed_after_first, first_events[1..]);
        assert!(matches!(
            observed_after_first.first().map(|event| &event.kind),
            Some(AgentEventKind::TurnStart { .. })
        ));

        subscriptions
            .lock()
            .expect("test subscription mutex")
            .clear();
        let second = agent.start_prompt("second")?;
        second.drive().await?;
        assert_eq!(
            observed.lock().expect("test observer mutex").as_slice(),
            &first_events[1..]
        );

        Ok::<(), CoreError>(())
    })
    .expect("runtime subscription must be safe from observer callbacks and unsubscribe on drop");
}

#[test]
fn nonblocking_subscription_is_ordered_lossy_and_never_delays_settlement() {
    smol::block_on(async {
        let full_capacity_agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .build();
        let ordered = full_capacity_agent
            .subscribe_nonblocking(std::num::NonZeroUsize::new(32).expect("nonzero capacity"));
        let ordered_run = full_capacity_agent.start_prompt("ordered events")?;
        ordered_run.drive().await?;
        let mut delivered = Vec::new();
        while let Ok(event) = ordered.try_recv() {
            delivered.push(event);
        }
        assert_eq!(delivered, ordered_run.events());
        assert_eq!(ordered.dropped_events(), 0);

        let constrained_agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .build();
        let constrained = constrained_agent
            .subscribe_nonblocking(std::num::NonZeroUsize::new(1).expect("nonzero capacity"));
        let constrained_run = constrained_agent.start_prompt("lossy events")?;
        constrained_run.drive().await?;
        assert_eq!(constrained_agent.snapshot().phase, AgentPhase::Idle);
        assert!(matches!(
            constrained.try_recv().map(|event| event.kind),
            Ok(AgentEventKind::AgentStart)
        ));
        assert_eq!(
            constrained.dropped_events(),
            constrained_run.events().len() as u64 - 1
        );

        Ok::<(), CoreError>(())
    })
    .expect("nonblocking event delivery must not participate in run settlement");
}

#[test]
fn lossless_subscription_is_ordered_without_capacity_drops() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .build();
        let subscription = agent.subscribe_lossless();
        let run = agent.start_prompt("lossless ordered events")?;

        run.drive().await?;

        let mut delivered = Vec::new();
        while let Ok(event) = subscription.try_recv() {
            delivered.push(event);
        }
        assert_eq!(delivered, run.events());
        assert!(matches!(
            subscription.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        Ok::<(), CoreError>(())
    })
    .expect("lossless event delivery must preserve source order");
}

#[test]
fn lossless_subscription_retains_all_events_under_volume() {
    smol::block_on(async {
        let run_count = 256;
        let provider = Arc::new(ScriptedProvider::new((0..run_count).map(|index| {
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta(format!("lossless event volume {index}")),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            }
        })));
        let agent = Agent::builder().model_provider(provider).build();
        let subscription = agent.subscribe_lossless();
        let mut emitted = Vec::new();

        for index in 0..run_count {
            let run = agent
                .start_prompt(format!("lossless volume run {index}"))
                .expect("volume run starts");
            run.drive().await?;
            emitted.extend(run.events());
        }
        assert!(emitted.len() > 1_000, "volume must exceed a small queue");

        let mut delivered = Vec::new();
        while let Ok(event) = subscription.try_recv() {
            delivered.push(event);
        }
        assert_eq!(delivered, emitted);

        Ok::<(), CoreError>(())
    })
    .expect("lossless event delivery must not silently drop under volume");
}

#[test]
fn dropping_lossless_subscription_unsubscribes_cleanly() {
    smol::block_on(async {
        let provider = Arc::new(ScriptedProvider::new([
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("before drop".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::TextDelta("after drop".into()),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]));
        let agent = Agent::builder().model_provider(provider).build();
        let subscription = agent.subscribe_lossless();
        let first = agent.start_prompt("before drop")?;
        first.drive().await?;
        drop(subscription);

        let second = agent.start_prompt("after drop")?;
        second.drive().await?;
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);

        Ok::<(), CoreError>(())
    })
    .expect("dropping a lossless subscription must not poison future runs");
}

#[test]
fn observer_failure_has_one_terminal_settlement_and_leaves_the_agent_reusable() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(Arc::new(TextOnlyProvider))
            .observer(Arc::new(FailingObserver))
            .build();
        let failed = agent.start_prompt("fail an observer")?;

        assert_eq!(
            failed.drive().await,
            Err(CoreError::Hook(crate::error::HookError::new(
                "observer",
                "fixture observer failure",
            )))
        );
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);
        assert_eq!(
            failed
                .events()
                .iter()
                .filter(|event| matches!(event.kind, AgentEventKind::AgentEnd { .. }))
                .count(),
            1
        );

        // The same explicit observer still fails future runs, but neither the
        // active-run ownership nor terminal event grammar are poisoned.
        let reused = agent.start_prompt("reuse after observer failure")?;
        assert_eq!(
            reused.drive().await,
            Err(CoreError::Hook(crate::error::HookError::new(
                "observer",
                "fixture observer failure",
            )))
        );
        assert_eq!(agent.snapshot().phase, AgentPhase::Idle);

        Ok::<(), CoreError>(())
    })
    .expect("observer failure must settle exactly once and preserve ownership invariants");
}
