//! Deterministic scripted model provider for tests.
//!
//! [`ScriptedProvider`] replays scripted turns through the real
//! [`ModelProvider`] port, so tests drive the actual agent loop, durable
//! supervisor, and hosts rather than a parallel fake. A turn is an ordered list
//! of [`ScriptStep`]s: stream events, named pauses, and cancellation waits.
//!
//! Coordination never relies on sleeps:
//!
//! - [`ScriptedProvider::wait_for_requests`] blocks a test thread until the
//!   runtime has dispatched a given number of requests;
//! - a [`ScriptStep::Pause`] holds the stream at a named [`Gate`] until the
//!   test releases it, or until the run is cancelled; and
//! - every dispatched [`ModelRequest`] is captured for inspection.
//!
//! The facility is compiled only with the `testing` feature.

use crate::scheduler::{
    CancellationToken, ModelCapabilities, ModelEventFuture, ModelEventStream, ModelFuture,
    ModelProvider, ModelRequest, ModelStreamEvent,
};
use crate::state::{
    AgentToolCall, ModelDescriptor, OpaqueProviderContextItem, SerializedJson, StopReason,
    ToolCallId, Usage,
};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// One step of a scripted provider turn.
#[derive(Clone, Debug)]
pub enum ScriptStep {
    /// Yield one stream event.
    Event(ModelStreamEvent),
    /// Hold the stream until the named gate is released. Cancellation of the
    /// run ends the turn with [`StopReason::Cancelled`].
    Pause(String),
    /// Hold the stream until the run is cancelled, then end it as cancelled.
    WaitForCancellation,
    /// End the stream with a provider error event.
    Fail(String),
}

/// One scripted provider response.
#[derive(Clone, Debug, Default)]
pub struct ScriptedTurn {
    /// Steps in stream order.
    pub steps: Vec<ScriptStep>,
}

impl ScriptedTurn {
    /// Start an empty turn.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one stream event.
    pub fn event(mut self, event: ModelStreamEvent) -> Self {
        self.steps.push(ScriptStep::Event(event));
        self
    }

    /// Append answer text.
    pub fn text(self, text: impl Into<String>) -> Self {
        self.event(ModelStreamEvent::TextDelta(text.into()))
    }

    /// Append provider-exposed thinking text.
    pub fn thinking(self, text: impl Into<String>) -> Self {
        self.event(ModelStreamEvent::ThinkingDelta(text.into()))
    }

    /// End the current thinking block with provider-private replay material.
    pub fn thinking_signature(self, provider: &str, signature: &str) -> Self {
        self.event(ModelStreamEvent::ThinkingSignature(
            OpaqueProviderContextItem::new(provider, "thinking_signature", None, signature)
                .expect("scripted signature is bounded"),
        ))
    }

    /// Append one complete tool call with JSON arguments.
    pub fn tool_call(self, id: &str, name: &str, arguments: &str) -> Self {
        self.event(ModelStreamEvent::ToolCall(AgentToolCall {
            id: ToolCallId::new(id).expect("scripted tool-call ID is nonempty"),
            name: name.into(),
            arguments: SerializedJson::new(arguments),
        }))
    }

    /// Append a usage report.
    pub fn usage(self, usage: Usage) -> Self {
        self.event(ModelStreamEvent::Usage(usage))
    }

    /// Hold at a named gate.
    pub fn pause(mut self, gate: impl Into<String>) -> Self {
        self.steps.push(ScriptStep::Pause(gate.into()));
        self
    }

    /// Hold until cancellation.
    pub fn wait_for_cancellation(mut self) -> Self {
        self.steps.push(ScriptStep::WaitForCancellation);
        self
    }

    /// End the stream with a provider error.
    pub fn fail(mut self, message: impl Into<String>) -> Self {
        self.steps.push(ScriptStep::Fail(message.into()));
        self
    }

    /// End normally.
    pub fn end(self, reason: StopReason) -> Self {
        self.event(ModelStreamEvent::End(reason))
    }

    /// End as a tool-use turn.
    pub fn end_tool_use(self) -> Self {
        self.end(StopReason::ToolUse)
    }

    /// End as a final answer.
    pub fn stop(self) -> Self {
        self.end(StopReason::Stop)
    }

    /// End with a provider error.
    pub fn error(self, message: impl Into<String>) -> Self {
        self.event(ModelStreamEvent::Error {
            message: message.into(),
        })
    }
}

/// A named pause point shared by a provider and a test.
#[derive(Default)]
pub struct Gate {
    state: Mutex<GateState>,
    reached: Condvar,
}

#[derive(Default)]
struct GateState {
    released: bool,
    arrivals: u64,
    wakers: Vec<Waker>,
}

impl Gate {
    /// Release every current and future waiter.
    pub fn release(&self) {
        let wakers = {
            let mut state = self.state.lock().expect("gate mutex poisoned");
            state.released = true;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
        self.reached.notify_all();
    }

    /// Whether the gate has been released.
    pub fn is_released(&self) -> bool {
        self.state.lock().expect("gate mutex poisoned").released
    }

    /// Block until a stream has reached this gate at least `count` times.
    pub fn wait_reached(&self, count: u64, timeout: Duration) -> bool {
        let state = self.state.lock().expect("gate mutex poisoned");
        let (state, _) = self
            .reached
            .wait_timeout_while(state, timeout, |state| state.arrivals < count)
            .expect("gate mutex poisoned");
        state.arrivals >= count
    }

    fn arrive(&self) {
        self.state.lock().expect("gate mutex poisoned").arrivals += 1;
        self.reached.notify_all();
    }
}

struct Shared {
    turns: Mutex<VecDeque<ScriptedTurn>>,
    requests: Mutex<Vec<ModelRequest>>,
    dispatched: Condvar,
    gates: Mutex<BTreeMap<String, Arc<Gate>>>,
    capabilities: ModelCapabilities,
}

/// A deterministic provider that replays scripted turns.
#[derive(Clone)]
pub struct ScriptedProvider {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ScriptedProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScriptedProvider")
            .field("requests", &self.request_count())
            .finish_non_exhaustive()
    }
}

impl ScriptedProvider {
    /// Replay `turns` in order, one per request.
    pub fn new(turns: impl IntoIterator<Item = ScriptedTurn>) -> Self {
        Self::with_capabilities(turns, ModelCapabilities::default())
    }

    /// Replay turns and declare explicit model capabilities.
    pub fn with_capabilities(
        turns: impl IntoIterator<Item = ScriptedTurn>,
        capabilities: ModelCapabilities,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                turns: Mutex::new(turns.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
                dispatched: Condvar::new(),
                gates: Mutex::new(BTreeMap::new()),
                capabilities,
            }),
        }
    }

    /// Append another turn.
    pub fn push_turn(&self, turn: ScriptedTurn) {
        self.shared
            .turns
            .lock()
            .expect("scripted turns mutex poisoned")
            .push_back(turn);
    }

    /// The gate with this name, created on first use.
    pub fn gate(&self, name: &str) -> Arc<Gate> {
        Arc::clone(
            self.shared
                .gates
                .lock()
                .expect("scripted gates mutex poisoned")
                .entry(name.to_owned())
                .or_default(),
        )
    }

    /// Every dispatched request, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.shared
            .requests
            .lock()
            .expect("scripted requests mutex poisoned")
            .clone()
    }

    /// Number of dispatched requests.
    pub fn request_count(&self) -> usize {
        self.shared
            .requests
            .lock()
            .expect("scripted requests mutex poisoned")
            .len()
    }

    /// Number of scripted turns not yet consumed.
    pub fn remaining_turns(&self) -> usize {
        self.shared
            .turns
            .lock()
            .expect("scripted turns mutex poisoned")
            .len()
    }

    /// Block until at least `count` requests were dispatched.
    pub fn wait_for_requests(&self, count: usize, timeout: Duration) -> bool {
        let requests = self
            .shared
            .requests
            .lock()
            .expect("scripted requests mutex poisoned");
        let (requests, _) = self
            .shared
            .dispatched
            .wait_timeout_while(requests, timeout, |requests| requests.len() < count)
            .expect("scripted requests mutex poisoned");
        requests.len() >= count
    }
}

impl ModelProvider for ScriptedProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let turn = self
            .shared
            .turns
            .lock()
            .expect("scripted turns mutex poisoned")
            .pop_front();
        self.shared
            .requests
            .lock()
            .expect("scripted requests mutex poisoned")
            .push(request);
        self.shared.dispatched.notify_all();
        let turn = turn.unwrap_or_else(|| {
            ScriptedTurn::new().error("scripted provider has no remaining turn")
        });
        let stream = ScriptedStream {
            steps: turn.steps.into(),
            provider: self.clone(),
            done: false,
        };
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }

    fn capabilities(&self, _model: Option<&ModelDescriptor>) -> ModelCapabilities {
        self.shared.capabilities.clone()
    }
}

struct ScriptedStream {
    steps: VecDeque<ScriptStep>,
    provider: ScriptedProvider,
    done: bool,
}

impl ModelEventStream for ScriptedStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(async move {
            loop {
                if self.done {
                    return Ok(None);
                }
                let Some(step) = self.steps.pop_front() else {
                    self.done = true;
                    return Ok(None);
                };
                match step {
                    ScriptStep::Event(event) => {
                        if matches!(
                            event,
                            ModelStreamEvent::End(_)
                                | ModelStreamEvent::Error { .. }
                                | ModelStreamEvent::Aborted { .. }
                                | ModelStreamEvent::ContextOverflow { .. }
                        ) {
                            self.done = true;
                        }
                        return Ok(Some(event));
                    }
                    ScriptStep::Fail(message) => {
                        self.done = true;
                        return Ok(Some(ModelStreamEvent::Error { message }));
                    }
                    ScriptStep::Pause(name) => {
                        let gate = self.provider.gate(&name);
                        gate.arrive();
                        match (WaitGate {
                            gate,
                            cancellation: cancellation.clone(),
                        })
                        .await
                        {
                            GateOutcome::Released => continue,
                            GateOutcome::Cancelled => {
                                self.done = true;
                                return Ok(Some(ModelStreamEvent::End(StopReason::Cancelled)));
                            }
                        }
                    }
                    ScriptStep::WaitForCancellation => {
                        cancellation.cancelled().await;
                        self.done = true;
                        return Ok(Some(ModelStreamEvent::End(StopReason::Cancelled)));
                    }
                }
            }
        })
    }
}

enum GateOutcome {
    Released,
    Cancelled,
}

struct WaitGate {
    gate: Arc<Gate>,
    cancellation: CancellationToken,
}

impl Future for WaitGate {
    type Output = GateOutcome;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.cancellation.is_cancelled() {
            return Poll::Ready(GateOutcome::Cancelled);
        }
        {
            let mut state = self.gate.state.lock().expect("gate mutex poisoned");
            if state.released {
                return Poll::Ready(GateOutcome::Released);
            }
            if !state.wakers.iter().any(|waker| waker.will_wake(context.waker())) {
                state.wakers.push(context.waker().clone());
            }
        }
        self.cancellation.register_waker(context.waker());
        if self.cancellation.is_cancelled() {
            Poll::Ready(GateOutcome::Cancelled)
        } else {
            Poll::Pending
        }
    }
}
