//! Nested tool calls made by trusted composition tools such as codemode.
//!
//! A composition tool never invokes another capability directly. It submits a
//! request through [`crate::tool::ToolComposition::call`]; the run that owns
//! the composition tool services the request with the same preparation path
//! as a model-issued call (registry lookup, JSON and schema validation,
//! before/after hooks, the effect gate, cancellation) and returns the result
//! to the composition tool instead of appending it to the transcript.
//!
//! The service is polled beside the composition tool's future. When that
//! future settles, requests that never started fail without effects, and
//! started calls are driven to settlement before the composition result is
//! returned, so no nested call becomes detached work.

use super::{AgentInner, PreparedToolCall, RunHandle, ToolStep, error_tool_result, next_tool_step};
use crate::state::{SerializedJson, ToolCallId};
use crate::tool::{
    AgentToolResult, NestedCallFuture, NestedCallState, ToolCall, ToolExecutionMode, ToolFuture,
};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Completion slot of one nested call.
#[derive(Default)]
pub(crate) struct NestedSlot {
    state: Mutex<(Option<AgentToolResult>, Option<Waker>)>,
}

impl NestedSlot {
    fn complete(&self, result: AgentToolResult) {
        let waker = {
            let mut state = self.state.lock().expect("nested slot mutex poisoned");
            state.0 = Some(result);
            state.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub(crate) fn poll_result(&self, context: &mut Context<'_>) -> Poll<AgentToolResult> {
        let mut state = self.state.lock().expect("nested slot mutex poisoned");
        match state.0.take() {
            Some(result) => Poll::Ready(result),
            None => {
                state.1 = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }
}

struct NestedRequest {
    call: ToolCall,
    slot: Arc<NestedSlot>,
}

#[derive(Default)]
struct NestedState {
    next: u64,
    queue: VecDeque<NestedRequest>,
    closed: bool,
    service: Option<Waker>,
}

/// The request channel shared by a composition tool and its owning run.
#[derive(Clone)]
pub(crate) struct NestedCalls {
    parent: ToolCallId,
    state: Arc<Mutex<NestedState>>,
}

impl NestedCalls {
    pub(crate) fn new(parent: ToolCallId) -> Self {
        Self {
            parent,
            state: Arc::new(Mutex::new(NestedState::default())),
        }
    }

    pub(crate) fn submit(&self, name: &str, arguments: SerializedJson) -> NestedCallFuture {
        let mut state = self.state.lock().expect("nested call mutex poisoned");
        state.next += 1;
        let id = ToolCallId::new(format!("{}.{}", self.parent, state.next))
            .expect("nested call identifiers are non-empty");
        let call = ToolCall {
            id,
            name: name.to_owned(),
            arguments,
        };
        if state.closed {
            return NestedCallFuture {
                state: NestedCallState::Ready(Some(error_tool_result(
                    &call,
                    "The composition tool already settled; this call never started.",
                ))),
            };
        }
        let slot = Arc::new(NestedSlot::default());
        state.queue.push_back(NestedRequest {
            call,
            slot: Arc::clone(&slot),
        });
        if let Some(waker) = state.service.take() {
            waker.wake();
        }
        NestedCallFuture {
            state: NestedCallState::Pending(slot),
        }
    }

    fn take(&self, context: &mut Context<'_>) -> Vec<NestedRequest> {
        let mut state = self.state.lock().expect("nested call mutex poisoned");
        state.service = Some(context.waker().clone());
        state.queue.drain(..).collect()
    }

    fn close(&self) -> Vec<NestedRequest> {
        let mut state = self.state.lock().expect("nested call mutex poisoned");
        state.closed = true;
        state.queue.drain(..).collect()
    }
}

type RunningCall<'a> = (bool, Pin<Box<dyn Future<Output = ()> + Send + 'a>>);

impl RunHandle {
    /// Drive a composition tool's future while servicing its nested calls.
    ///
    /// Calls to sequential tools run alone; parallel tools may overlap. After
    /// the composition tool settles, queued calls fail without starting and
    /// started calls run to settlement before the result is returned.
    pub(super) fn with_nested_calls<'a>(
        &'a self,
        agent: &'a AgentInner,
        calls: NestedCalls,
        tool_future: ToolFuture<'a>,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let mut tool_future = tool_future;
            let mut output = None;
            let mut waiting = VecDeque::<NestedRequest>::new();
            let mut running = Vec::<RunningCall<'a>>::new();
            std::future::poll_fn(move |context| {
                if output.is_none()
                    && let Poll::Ready(result) = tool_future.as_mut().poll(context)
                {
                    output = Some(result);
                    for request in calls.close().into_iter().chain(waiting.drain(..)) {
                        request.slot.complete(error_tool_result(
                            &request.call,
                            "The composition tool settled before this call started.",
                        ));
                    }
                }
                if output.is_none() {
                    waiting.extend(calls.take(context));
                }
                loop {
                    while let Some(front) = waiting.front() {
                        let sequential = self.nested_is_sequential(&front.call.name);
                        if running.iter().any(|(busy, _)| *busy)
                            || (sequential && !running.is_empty())
                        {
                            break;
                        }
                        let request = waiting.pop_front().expect("front exists");
                        let parent = calls.parent.clone();
                        running.push((
                            sequential,
                            Box::pin(async move {
                                let result =
                                    self.execute_nested_call(agent, &parent, request.call).await;
                                request.slot.complete(result);
                            }),
                        ));
                    }
                    let before = running.len();
                    running.retain_mut(|(_, future)| future.as_mut().poll(context).is_pending());
                    if running.len() == before || waiting.is_empty() {
                        break;
                    }
                }
                match output.take() {
                    Some(result) if running.is_empty() => Poll::Ready(result),
                    Some(result) => {
                        output = Some(result);
                        Poll::Pending
                    }
                    None => Poll::Pending,
                }
            })
            .await
        })
    }

    fn nested_is_sequential(&self, name: &str) -> bool {
        self.configuration
            .tools
            .get(name)
            .is_some_and(|tool| tool.execution_mode() == ToolExecutionMode::Sequential)
    }

    /// Execute one nested call through the ordinary preparation path. Its
    /// result goes back to the composition tool, never to the transcript.
    async fn execute_nested_call(
        &self,
        agent: &AgentInner,
        parent: &ToolCallId,
        mut call: ToolCall,
    ) -> AgentToolResult {
        let prepared = match self
            .prepare_tool_call_for(agent, &mut call, Some(parent))
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => return error_tool_result(&call, error.to_string()),
        };
        let mut result = match prepared {
            PreparedToolCall::Immediate { result, .. } => *result,
            PreparedToolCall::Execute { tool, effect } => {
                let updates = super::PendingToolUpdates::default();
                let mut future =
                    self.start_tool_future(agent, &tool, call.clone(), updates.clone());
                let mode = tool.cancellation_settlement_mode();
                // Nested progress stays inside the composition tool; only the
                // composition tool's own updates reach the host.
                let execution = loop {
                    match next_tool_step(
                        &mut future,
                        &updates,
                        &call.id,
                        &self.cancellation,
                        false,
                        mode,
                    )
                    .await
                    {
                        ToolStep::Updates(_) => {}
                        ToolStep::Completed { result, .. } => break *result,
                    }
                };
                drop(future);
                match self
                    .finalize_executed_tool(agent, &call, *effect, execution)
                    .await
                {
                    Ok((result, _terminate)) => result,
                    Err(error) => error_tool_result(&call, error.to_string()),
                }
            }
        };
        super::tool_execution::normalize_result_failure(&mut result);
        // Discovery is the model's decision; a nested result cannot change
        // what the next request declares.
        result.added_tool_names.clear();
        result
    }
}
