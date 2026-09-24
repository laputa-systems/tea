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
use tea_core::state::{AgentMessage, MAX_PARTIAL_RESPONSE_BYTES, RunPhase, StopReason};

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
    deltas: Vec<String>,
    next_delta: usize,
    emitted_end: bool,
    gate: Arc<Gate>,
}

impl ModelEventStream for GatedStream {
    fn next_event<'a>(&'a mut self, _cancellation: CancellationToken) -> ModelEventFuture<'a> {
        if let Some(delta) = self.deltas.get(self.next_delta).cloned() {
            self.next_delta = self.next_delta.saturating_add(1);
            return Box::pin(std::future::ready(Ok(Some(ModelStreamEvent::TextDelta(
                delta,
            )))));
        }
        if !self.emitted_end {
            self.emitted_end = true;
            return Box::pin(GateEnd {
                gate: Arc::clone(&self.gate),
            });
        }
        Box::pin(std::future::ready(Ok(None)))
    }
}

struct GatedProvider {
    source: Mutex<Option<GatedStream>>,
}

impl GatedProvider {
    fn new(gate: Arc<Gate>) -> Self {
        Self::with_deltas(gate, ["first delta".into()])
    }

    fn with_deltas(gate: Arc<Gate>, deltas: impl IntoIterator<Item = String>) -> Self {
        Self {
            source: Mutex::new(Some(GatedStream {
                deltas: deltas.into_iter().collect(),
                next_delta: 0,
                emitted_end: false,
                gate,
            })),
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
    let message_start_id = run
        .events()
        .iter()
        .find_map(|event| match &event.kind {
            AgentEventKind::MessageStart {
                message: AgentMessage::Assistant { id, .. },
            } => Some(*id),
            _ => None,
        })
        .expect("streaming assistant message starts");
    assert!(run.events().iter().any(|event| {
        matches!(
            &event.kind,
            AgentEventKind::MessageUpdate {
                message_id,
                text_delta: delta,
            } if *message_id == message_start_id && delta == "first delta"
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
fn message_updates_are_deltas_while_final_assistant_content_remains_complete() {
    let gate = Arc::new(Gate::default());
    let deltas = ["one ".to_owned(), "two ".to_owned(), "three".to_owned()];
    let provider = Arc::new(GatedProvider::with_deltas(
        Arc::clone(&gate),
        deltas.clone(),
    ));
    let agent = Agent::builder()
        .model_provider(provider as Arc<dyn ModelProvider>)
        .build();
    let run = Arc::new(agent.start_prompt("stream several deltas").unwrap());
    let executor = smol::Executor::new();
    let driving_run = Arc::clone(&run);
    let drive = executor.spawn(async move { driving_run.drive().await });

    assert!(executor.try_tick());
    let snapshot = agent.snapshot();
    let message_id = match snapshot.messages.last() {
        Some(AgentMessage::Assistant { id, content, .. }) => {
            assert!(
                content.is_empty(),
                "live assistant content stays out of snapshots"
            );
            *id
        }
        other => panic!("expected live assistant placeholder, got {other:?}"),
    };
    assert_eq!(snapshot.partial_response.as_deref(), Some("one two three"));
    let updates = run
        .events()
        .into_iter()
        .filter_map(|event| match event.kind {
            AgentEventKind::MessageUpdate {
                message_id: update_id,
                text_delta: delta,
            } => Some((update_id, delta)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        updates,
        vec![
            (message_id, "one ".into()),
            (message_id, "two ".into()),
            (message_id, "three".into()),
        ]
    );

    gate.release();
    assert!(executor.try_tick());
    assert_eq!(smol::block_on(drive), Ok(()));
    assert!(matches!(
        agent.snapshot().messages.last(),
        Some(AgentMessage::Assistant { content, .. }) if content == "one two three"
    ));
}

#[test]
fn live_partial_response_is_bounded_without_truncating_the_final_message() {
    let gate = Arc::new(Gate::default());
    let response = "x".repeat(MAX_PARTIAL_RESPONSE_BYTES.saturating_add(17));
    let provider = Arc::new(GatedProvider::with_deltas(
        Arc::clone(&gate),
        [response.clone()],
    ));
    let agent = Agent::builder()
        .model_provider(provider as Arc<dyn ModelProvider>)
        .build();
    let run = Arc::new(agent.start_prompt("stream a long delta").unwrap());
    let executor = smol::Executor::new();
    let driving_run = Arc::clone(&run);
    let drive = executor.spawn(async move { driving_run.drive().await });

    assert!(executor.try_tick());
    let snapshot = agent.snapshot();
    assert_eq!(
        snapshot.partial_response.as_deref(),
        Some(&response[response.len() - MAX_PARTIAL_RESPONSE_BYTES..])
    );
    assert!(
        snapshot
            .partial_response
            .as_ref()
            .is_some_and(|partial| partial.len() <= MAX_PARTIAL_RESPONSE_BYTES)
    );

    gate.release();
    assert!(executor.try_tick());
    assert_eq!(smol::block_on(drive), Ok(()));
    assert!(matches!(
        agent.snapshot().messages.last(),
        Some(AgentMessage::Assistant { content, .. }) if content == &response
    ));
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
