//! Evidence that live provider streams are reduced before their terminal event.
//!
//! The finite [`ModelStream`] fixture adapter is intentionally convenient, but it must not
//! accidentally define the production contract. These tests use a source that pauses between a
//! text delta and `EndTurn`, exactly as an HTTP or native-model adapter would.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tea_core::Agent;
use tea_core::event::AgentEventKind;
use tea_core::scheduler::{
    CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture, ModelProvider,
    ModelRequest, ModelStreamEvent,
};
use tea_core::state::{RunPhase, StopReason};

#[derive(Debug, Default)]
struct Gate {
    released: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn release(&self) {
        self.released.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().expect("gate waker mutex poisoned").take() {
            waker.wake();
        }
    }
}

struct GateEnd {
    gate: Arc<Gate>,
}

impl Future for GateEnd {
    type Output = Result<Option<ModelStreamEvent>, tea_core::error::SchedulerError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.gate.released.load(Ordering::Acquire) {
            return Poll::Ready(Ok(Some(ModelStreamEvent::End(StopReason::Stop))));
        }
        let mut waker = self.gate.waker.lock().expect("gate waker mutex poisoned");
        if self.gate.released.load(Ordering::Acquire) {
            Poll::Ready(Ok(Some(ModelStreamEvent::End(StopReason::Stop))))
        } else {
            *waker = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

struct GatedStream {
    phase: u8,
    gate: Arc<Gate>,
}

impl ModelEventStream for GatedStream {
    fn next_event<'a>(&'a mut self, _cancellation: CancellationToken) -> ModelEventFuture<'a> {
        match self.phase {
            0 => {
                self.phase = 1;
                Box::pin(std::future::ready(Ok(Some(ModelStreamEvent::TextDelta(
                    "first delta".into(),
                )))))
            }
            1 => {
                self.phase = 2;
                Box::pin(GateEnd {
                    gate: Arc::clone(&self.gate),
                })
            }
            _ => Box::pin(std::future::ready(Ok(None))),
        }
    }
}

struct GatedProvider {
    source: Mutex<Option<GatedStream>>,
}

impl GatedProvider {
    fn new(gate: Arc<Gate>) -> Self {
        Self {
            source: Mutex::new(Some(GatedStream { phase: 0, gate })),
        }
    }
}

impl ModelProvider for GatedProvider {
    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let source = self
            .source
            .lock()
            .expect("provider source mutex poisoned")
            .take()
            .expect("test starts one model request");
        Box::pin(std::future::ready(Ok(Box::new(source) as _)))
    }
}

#[test]
fn delta_is_visible_while_the_provider_stream_is_still_open() {
    let gate = Arc::new(Gate::default());
    let provider = Arc::new(GatedProvider::new(Arc::clone(&gate)));
    let agent = Agent::builder()
        .model_provider(provider as Arc<dyn ModelProvider>)
        .build();
    let run = Arc::new(agent.start_prompt("stream one delta").unwrap());
    let executor = smol::Executor::new();
    let driving_run = Arc::clone(&run);
    let drive = executor.spawn(async move { driving_run.drive().await });

    assert!(executor.try_tick());
    let snapshot = agent.snapshot();
    assert_eq!(snapshot.partial_response.as_deref(), Some("first delta"));
    assert!(snapshot.is_streaming);
    assert_eq!(run.snapshot().phase, RunPhase::Running);
    assert!(run.events().iter().any(|event| {
        matches!(
            &event.kind,
            AgentEventKind::MessageUpdate {
                text_delta: Some(delta),
                ..
            } if delta == "first delta"
        )
    }));

    gate.release();
    assert!(executor.try_tick());
    assert_eq!(smol::block_on(drive), Ok(()));
    assert_eq!(run.snapshot().phase, RunPhase::Succeeded);
    assert!(!agent.snapshot().is_streaming);
    assert_eq!(agent.snapshot().partial_response, None);
}

#[test]
fn cancellation_waiter_is_woken_without_runtime_specific_primitives() {
    let cancellation = CancellationToken::new();
    let (sent, received) = std::sync::mpsc::channel();
    let waiter = cancellation.clone();
    let executor = smol::Executor::new();
    let task = executor.spawn(async move {
        waiter.cancelled().await;
        sent.send(()).expect("test receiver remains open");
    });

    assert!(executor.try_tick());
    assert!(received.try_recv().is_err());
    cancellation.cancel();
    assert!(executor.try_tick());
    smol::block_on(task);
    received
        .try_recv()
        .expect("cancellation wakes the registered waiter");
}
