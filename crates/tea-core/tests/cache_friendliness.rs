//! Deterministic cacheability baseline for adjacent core model requests.

use std::sync::{Arc, Mutex};
use tea_core::Agent;
use tea_core::measurement::measure_prompt_cacheability;
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{ModelDescriptor, StopReason, ThinkingLevel};

#[derive(Clone, Default)]
struct RecordingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for RecordingProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests
            .lock()
            .expect("recording provider mutex poisoned")
            .push(request);
        Box::pin(std::future::ready(Ok(Box::new(ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("fixture response".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }) as _)))
    }
}

#[test]
fn adjacent_text_turns_keep_the_prior_context_prefix() {
    let provider = RecordingProvider::default();
    let agent = Agent::builder()
        .system_prompt("stable system prompt")
        .model(ModelDescriptor {
            provider: "fixture".into(),
            model: "cache-baseline".into(),
            revision: None,
        })
        .thinking_level(ThinkingLevel::Off)
        .model_provider(Arc::new(provider.clone()))
        .build();

    for prompt in ["first turn", "second turn", "third turn"] {
        smol::block_on(
            agent
                .start_prompt(prompt)
                .expect("idle fixture agent")
                .drive(),
        )
        .expect("fixture run settles");
    }

    let requests = provider
        .requests
        .lock()
        .expect("recording provider mutex poisoned")
        .clone();
    assert_eq!(requests.len(), 3);
    let measurements = requests
        .windows(2)
        .map(|pair| measure_prompt_cacheability(Some(&pair[0]), &pair[1]))
        .collect::<Vec<_>>();
    assert!(
        measurements
            .iter()
            .all(|measurement| !measurement.cache_domain_changed)
    );
    assert!(
        measurements
            .iter()
            .all(|measurement| measurement.common_context_prefix_bytes > 0)
    );
    eprintln!(
        "cache baseline: requests={}, context_bytes={:?}, common_prefix_bytes={:?}, ratios_ppm={:?}",
        requests.len(),
        requests
            .iter()
            .map(|request| request.context.len())
            .collect::<Vec<_>>(),
        measurements
            .iter()
            .map(|measurement| measurement.common_context_prefix_bytes)
            .collect::<Vec<_>>(),
        measurements
            .iter()
            .map(|measurement| measurement.common_context_prefix_ratio_millionths)
            .collect::<Vec<_>>(),
    );
}
