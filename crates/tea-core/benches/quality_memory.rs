//! Allocation and timing probe for the concrete Rust harness hot path.
//!
//! Run with the repository's pinned nightly:
//! `cargo +nightly-2026-07-24 bench -p tea-core --bench quality_memory`.
//! Rustybench reports allocation count, allocated bytes, and peak live bytes;
//! this is intentionally a diagnostic companion to the cross-process peak-RSS
//! values recorded by `python3 -m evals.quality fast`.

use rustybench::AllocProfiler;
use std::sync::Arc;
use tea_core::Agent;
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{ModelDescriptor, StopReason, ThinkingLevel};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

/// A provider-free response representative of one completed harness turn.
struct TextProvider;

impl ModelProvider for TextProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        Box::pin(std::future::ready(Ok(Box::new(ModelStream {
            events: vec![
                ModelStreamEvent::TextDelta("quality memory fixture".into()),
                ModelStreamEvent::End(StopReason::Stop),
            ],
        }) as _)))
    }
}

#[rustybench::bench]
fn one_text_turn() {
    let agent = Agent::builder()
        .system_prompt("quality allocation fixture")
        .model(ModelDescriptor {
            provider: "fixture".into(),
            model: "quality-memory".into(),
            revision: None,
        })
        .thinking_level(ThinkingLevel::Off)
        .model_provider(Arc::new(TextProvider))
        .build();
    let run = agent
        .start_prompt("measure one deterministic response")
        .expect("idle fixture agent");
    smol::block_on(run.drive()).expect("provider-free fixture settles");
}

fn main() {
    rustybench::main();
}
