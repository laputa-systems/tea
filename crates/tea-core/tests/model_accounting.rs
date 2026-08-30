use std::sync::{Arc, Mutex};
use tea_core::Agent;
use tea_core::event::AgentEventKind;
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{ModelDescriptor, StopReason, Usage};

struct FixtureProvider {
    streams: Mutex<Vec<ModelStream>>,
}

impl ModelProvider for FixtureProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = self
            .streams
            .lock()
            .expect("fixture provider mutex poisoned")
            .remove(0);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

fn usage(input: Option<u64>, output: Option<u64>, cost: Option<&str>) -> Usage {
    Usage {
        total_tokens: input.zip(output).map(|(input, output)| input + output),
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost: cost.map(str::to_owned),
    }
}

#[test]
fn usage_is_reduced_into_a_settled_turn_event_and_snapshot() {
    let model = ModelDescriptor {
        provider: "fixture".into(),
        model: "accounting-model".into(),
        revision: Some("2026-08-14".into()),
    };
    let provider = Arc::new(FixtureProvider {
        streams: Mutex::new(vec![ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("answer".into()),
                ModelStreamEvent::Usage(Usage {
                    total_tokens: Some(4),
                    input_tokens: Some(4),
                    output_tokens: Some(0),
                    reasoning_tokens: Some(2),
                    cache_read_tokens: Some(3),
                    cache_write_tokens: Some(0),
                    cost: Some("0.000000000000000001".into()),
                }),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }]),
    });
    let agent = Agent::builder()
        .model(model.clone())
        .model_provider(provider)
        .build();
    let run = agent.start_prompt("prompt").expect("run starts");
    smol::block_on(run.drive()).expect("run succeeds");

    let accounting = &agent.snapshot().accounting;
    assert_eq!(accounting.turns.len(), 1);
    assert_eq!(accounting.turns[0].run_id, run.id());
    assert_eq!(accounting.turns[0].turn_id.0, 1);
    assert_eq!(accounting.turns[0].model, Some(model));
    assert_eq!(accounting.turns[0].usage.output_tokens, Some(0));
    assert_eq!(accounting.aggregate.cache_write_tokens, Some(0));
    assert_eq!(
        accounting.aggregate.cost.as_deref(),
        Some("0.000000000000000001")
    );
    assert!(run.events().iter().any(|event| {
        matches!(
            &event.kind,
            AgentEventKind::ModelTurnUsage { accounting }
                if accounting.usage.input_tokens == Some(4)
                    && accounting.usage.cost.as_deref() == Some("0.000000000000000001")
        )
    }));
}

#[test]
fn aggregate_preserves_unknown_fields_and_adds_exact_costs() {
    let provider = Arc::new(FixtureProvider {
        streams: Mutex::new(vec![
            ModelStream {
                events: vec![
                    ModelStreamEvent::Usage(usage(Some(1), Some(2), Some("0.1"))),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
            ModelStream {
                events: vec![
                    ModelStreamEvent::Usage(usage(Some(0), None, Some("0.200000000000000001"))),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
            },
        ]),
    });
    let agent = Agent::builder().model_provider(provider).build();
    let first = agent.start_prompt("first").expect("first run starts");
    smol::block_on(first.drive()).expect("first run succeeds");
    let second = agent.start_prompt("second").expect("second run starts");
    smol::block_on(second.drive()).expect("second run succeeds");

    let accounting = agent.snapshot().accounting;
    assert_eq!(accounting.turns.len(), 2);
    assert_eq!(accounting.aggregate.input_tokens, Some(1));
    assert_eq!(accounting.aggregate.output_tokens, Some(2));
    assert_eq!(accounting.aggregate.reasoning_tokens, None);
    assert_eq!(accounting.aggregate.cache_read_tokens, None);
    assert_eq!(
        accounting.aggregate.cost.as_deref(),
        Some("0.300000000000000001")
    );
}
