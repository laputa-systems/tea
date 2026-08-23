//! Tool preparation, execution, update delivery, and result insertion for one run.

use super::{
    apply_after_tool_call, error_tool_result, error_tool_result_from_error, next_parallel_step,
    next_tool_step, ParallelToolStep, PendingToolExecution, PendingToolUpdate, PendingToolUpdates,
    PreparedToolCall, PreparedToolExecution, RunHandle, TerminalToolFailure, ToolBatchOutcome,
    ToolStep,
};
use crate::agent::AgentInner;
use crate::effect::{
    DurableWriteRequest, EffectCompletion, EffectOutcome, EffectSubject, HookInvocation,
    ToolEffectOutcome,
};
use crate::error::CoreError;
use crate::event::AgentEventKind;
use crate::hooks::BeforeToolCall;
use crate::schema_validation::validate_tool_arguments;
use crate::state::{AgentMessage, AgentToolCall};
use crate::tool::{AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolUpdateSink};
use std::sync::Arc;
use std::task::Poll;

impl RunHandle {
    pub(super) async fn execute_tool_calls(
        &self,
        agent: &AgentInner,
        tool_calls: &[AgentToolCall],
    ) -> Result<ToolBatchOutcome, CoreError> {
        if tool_calls.len() > 1 {
            let exclusive = tool_calls
                .iter()
                .filter_map(|assistant_call| {
                    self.configuration
                        .tools
                        .get(&assistant_call.name)
                        .filter(|tool| tool.requires_exclusive_batch())
                        .map(|tool| tool.name().to_owned())
                })
                .collect::<Vec<_>>();
            if !exclusive.is_empty() {
                return self
                    .reject_exclusive_tool_batch(agent, tool_calls, &exclusive)
                    .await;
            }
        }
        let has_sequential_tool = tool_calls.iter().any(|assistant_call| {
            self.configuration
                .tools
                .get(&assistant_call.name)
                .is_some_and(|tool| {
                    tool.execution_mode() == crate::tool::ToolExecutionMode::Sequential
                })
        });
        if has_sequential_tool {
            self.execute_tool_calls_sequential(agent, tool_calls).await
        } else {
            self.execute_tool_calls_parallel(agent, tool_calls).await
        }
    }

    /// Close every call in an invalid mixed batch without beginning any
    /// external capability.  A transactional host tool cannot safely be
    /// ordered beside arbitrary sibling effects, and rejecting just that tool
    /// would still permit a partial batch to escape.
    async fn reject_exclusive_tool_batch(
        &self,
        agent: &AgentInner,
        tool_calls: &[AgentToolCall],
        exclusive: &[String],
    ) -> Result<ToolBatchOutcome, CoreError> {
        let names = exclusive.join(", ");
        let message = format!(
            "{names} must be the only tool call in an assistant batch. No calls in this batch were executed; retry the transactional request alone."
        );
        for assistant_call in tool_calls {
            let call = ToolCall {
                id: assistant_call.id.clone(),
                name: assistant_call.name.clone(),
                arguments: assistant_call.arguments.clone(),
            };
            self.append_tool_result_message(
                agent,
                call.clone(),
                error_tool_result(&call, &message),
            )
            .await?;
        }
        Ok(ToolBatchOutcome {
            all_terminate: false,
            terminal_failure: None,
        })
    }

    async fn execute_tool_calls_sequential(
        &self,
        agent: &AgentInner,
        tool_calls: &[AgentToolCall],
    ) -> Result<ToolBatchOutcome, CoreError> {
        let mut all_terminate = true;
        for (source_index, assistant_call) in tool_calls.iter().enumerate() {
            let mut call = ToolCall {
                id: assistant_call.id.clone(),
                name: assistant_call.name.clone(),
                arguments: assistant_call.arguments.clone(),
            };
            {
                let mut state = agent.state.lock().expect("agent state mutex poisoned");
                state.pending_tool_calls.insert(call.id.clone());
            }
            self.emit(
                agent,
                AgentEventKind::ToolExecutionStart {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            )
            .await?;

            let (mut result, terminate) = self.execute_one_tool_call(agent, &mut call).await?;
            normalize_result_failure(&mut result);
            self.emit_tool_execution_end(agent, &call, &result).await?;
            self.append_tool_result_message(agent, call.clone(), result.clone())
                .await?;
            if let Some(terminal_failure) = self.observe_tool_failure(agent, &call, &result).await?
            {
                self.append_skipped_sequential_calls(
                    agent,
                    &tool_calls[source_index.saturating_add(1)..],
                    &terminal_failure.message,
                )
                .await?;
                return Ok(ToolBatchOutcome {
                    all_terminate: false,
                    terminal_failure: Some(terminal_failure),
                });
            }
            all_terminate &= terminate;
        }
        Ok(ToolBatchOutcome {
            all_terminate,
            terminal_failure: None,
        })
    }

    async fn execute_tool_calls_parallel(
        &self,
        agent: &AgentInner,
        tool_calls: &[AgentToolCall],
    ) -> Result<ToolBatchOutcome, CoreError> {
        let mut prepared = Vec::with_capacity(tool_calls.len());
        let updates = PendingToolUpdates::default();
        let mut completions = (0..tool_calls.len())
            .map(|_| None::<(AgentToolResult, bool)>)
            .collect::<Vec<_>>();

        // Pi announces each call and prepares it in source order before it starts the parallel
        // batch. Immediate preparation failures therefore end before later calls are announced;
        // successful result messages remain deferred until every batch completion is known.
        for (source_index, assistant_call) in tool_calls.iter().enumerate() {
            let mut call = ToolCall {
                id: assistant_call.id.clone(),
                name: assistant_call.name.clone(),
                arguments: assistant_call.arguments.clone(),
            };
            {
                let mut state = agent.state.lock().expect("agent state mutex poisoned");
                state.pending_tool_calls.insert(call.id.clone());
            }
            self.emit(
                agent,
                AgentEventKind::ToolExecutionStart {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            )
            .await?;
            let mut preparation = self.prepare_tool_call(agent, &mut call).await?;
            if let PreparedToolCall::Immediate { result, terminate } = &mut preparation {
                normalize_result_failure(result);
                self.emit_tool_execution_end(agent, &call, result).await?;
                completions[source_index] = Some((result.clone(), *terminate));
            }
            prepared.push(PreparedToolExecution {
                source_index,
                call,
                preparation,
            });
        }

        // `pending` borrows the tool Arcs retained in `prepared`; it is declared after the
        // prepared vector so futures are dropped before their referenced capabilities.
        let mut pending = Vec::new();
        for prepared_call in &prepared {
            if let PreparedToolCall::Execute { tool, effect } = &prepared_call.preparation {
                let future =
                    self.start_tool_future(tool, prepared_call.call.clone(), updates.clone());
                pending.push(PendingToolExecution {
                    source_index: prepared_call.source_index,
                    call: prepared_call.call.clone(),
                    effect: effect.clone(),
                    future,
                });
            }
        }
        let mut allowed_after_cancellation = std::collections::BTreeSet::new();
        while !pending.is_empty() {
            match next_parallel_step(
                &mut pending,
                &updates,
                &self.cancellation,
                &mut allowed_after_cancellation,
            )
            .await
            {
                ParallelToolStep::Cancelled {
                    updates: pending_updates,
                } => {
                    self.emit_tool_updates(agent, pending_updates).await?;
                    // Drop every pending future immediately. A tool that has
                    // not implemented cancellation must not keep the caller's
                    // run busy after the shared scope is cancelled.
                    for pending_call in std::mem::take(&mut pending) {
                        updates.close(&pending_call.call.id);
                        let mut result = error_tool_result(&pending_call.call, "Operation aborted");
                        result.failure = Some(crate::tool::ToolFailure::cancelled());
                        self.settle_effect(
                            pending_call.effect,
                            EffectOutcome::ToolExecution(ToolEffectOutcome {
                                raw_result: result.clone(),
                                result: result.clone(),
                            }),
                        )
                        .await?;
                        self.emit_tool_execution_end(agent, &pending_call.call, &result)
                            .await?;
                        completions[pending_call.source_index] = Some((result, false));
                    }
                }
                ParallelToolStep::Updates(pending_updates) => {
                    let update_call_ids = pending_updates
                        .iter()
                        .map(|(tool_call_id, _, _)| tool_call_id.clone())
                        .collect::<std::collections::BTreeSet<_>>();
                    self.emit_tool_updates(agent, pending_updates).await?;
                    if self.cancellation.is_cancelled() {
                        // Pi's parallel scheduler gives the sibling calls a
                        // terminal result before it settles the call whose
                        // update requested cancellation. Keeping that call
                        // pending retains its normal completion semantics.
                        // A sibling is polled once only: an already-ready
                        // result is safe to preserve, while a pending future
                        // is dropped and turned into a cancellation result so
                        // a cancellation-unaware tool cannot hold the run.
                        let mut still_running = Vec::new();
                        for mut pending_call in std::mem::take(&mut pending) {
                            if update_call_ids.contains(&pending_call.call.id) {
                                still_running.push(pending_call);
                                continue;
                            }
                            let execution = std::future::poll_fn(|context| {
                                match pending_call.future.as_mut().poll(context) {
                                    Poll::Ready(result) => Poll::Ready(Some(result)),
                                    Poll::Pending => Poll::Ready(None),
                                }
                            })
                            .await;
                            updates.close(&pending_call.call.id);
                            self.flush_tool_updates(agent, &updates).await?;
                            let (result, terminate) = match execution {
                                Some(result) => {
                                    self.finalize_executed_tool(
                                        agent,
                                        &pending_call.call,
                                        pending_call.effect,
                                        result,
                                    )
                                    .await?
                                }
                                None => {
                                    let mut result =
                                        error_tool_result(&pending_call.call, "Operation aborted");
                                    result.failure = Some(crate::tool::ToolFailure::cancelled());
                                    self.settle_effect(
                                        pending_call.effect,
                                        EffectOutcome::ToolExecution(ToolEffectOutcome {
                                            raw_result: result.clone(),
                                            result: result.clone(),
                                        }),
                                    )
                                    .await?;
                                    (result, false)
                                }
                            };
                            self.emit_tool_execution_end(agent, &pending_call.call, &result)
                                .await?;
                            completions[pending_call.source_index] = Some((result, terminate));
                        }
                        pending = still_running;
                        allowed_after_cancellation.extend(
                            pending
                                .iter()
                                .filter(|pending_call| {
                                    update_call_ids.contains(&pending_call.call.id)
                                })
                                .map(|pending_call| pending_call.call.id.clone()),
                        );
                    }
                }
                ParallelToolStep::Completed {
                    completed,
                    updates: pending_updates,
                } => {
                    self.emit_tool_updates(agent, pending_updates).await?;
                    let (result, terminate) = self
                        .finalize_executed_tool(
                            agent,
                            &completed.call,
                            completed.effect,
                            completed.result,
                        )
                        .await?;
                    // Another parallel tool may have delivered an update while
                    // the completion hook was awaited. Deliver it before this
                    // tool's terminal event.
                    self.flush_tool_updates(agent, &updates).await?;
                    self.emit_tool_execution_end(agent, &completed.call, &result)
                        .await?;
                    completions[completed.source_index] = Some((result, terminate));
                }
            }
        }
        drop(pending);

        let mut all_terminate = true;
        let mut terminal_failure = None;
        for prepared_call in prepared {
            let (mut result, terminate) = completions[prepared_call.source_index]
                .take()
                .expect("each prepared tool call must have exactly one completion");
            normalize_result_failure(&mut result);
            self.append_tool_result_message(agent, prepared_call.call.clone(), result.clone())
                .await?;
            if let Some(observed) = self
                .observe_tool_failure(agent, &prepared_call.call, &result)
                .await?
            {
                terminal_failure.get_or_insert(observed);
            }
            all_terminate &= terminate;
        }
        Ok(ToolBatchOutcome {
            all_terminate,
            terminal_failure,
        })
    }

    async fn execute_one_tool_call(
        &self,
        agent: &AgentInner,
        call: &mut ToolCall,
    ) -> Result<(AgentToolResult, bool), CoreError> {
        match self.prepare_tool_call(agent, call).await? {
            PreparedToolCall::Immediate { result, terminate } => Ok((result, terminate)),
            PreparedToolCall::Execute { tool, effect } => {
                let updates = PendingToolUpdates::default();
                let future = self.start_tool_future(&tool, call.clone(), updates.clone());
                let mut future = future;
                // A tool can synchronously emit an update that cancels the
                // run before returning its future. Preserve the update and
                // allow one poll for its already-created completion.
                let mut allow_one_poll_after_cancellation =
                    self.cancellation.is_cancelled() && updates.has_updates();
                let execution = loop {
                    let allow = std::mem::take(&mut allow_one_poll_after_cancellation);
                    match next_tool_step(&mut future, &updates, &call.id, &self.cancellation, allow)
                        .await
                    {
                        ToolStep::Updates(updates) => {
                            self.emit_tool_updates(agent, updates).await?;
                            // Preserve Pi's established update-cancellation
                            // ordering: a tool that became ready while
                            // producing the cancelling update may still
                            // report that completion. A still-pending future
                            // is dropped on that one poll instead.
                            allow_one_poll_after_cancellation = self.cancellation.is_cancelled();
                        }
                        ToolStep::Completed { result, updates } => {
                            self.emit_tool_updates(agent, updates).await?;
                            break result;
                        }
                    }
                };
                self.flush_tool_updates(agent, &updates).await?;
                self.finalize_executed_tool(agent, call, effect, execution)
                    .await
            }
        }
    }

    async fn prepare_tool_call(
        &self,
        agent: &AgentInner,
        call: &mut ToolCall,
    ) -> Result<PreparedToolCall, CoreError> {
        let Some(tool) = self.configuration.tools.get(&call.name).cloned() else {
            return Ok(PreparedToolCall::Immediate {
                result: error_tool_result(call, format!("Tool {} not found", call.name)),
                terminate: false,
            });
        };
        if let Err(error) = tea_protocol::JsonValue::parse(call.arguments.as_str()) {
            return Ok(PreparedToolCall::Immediate {
                result: error_tool_result(
                    call,
                    format!(
                        "Tool {} received invalid JSON arguments: {error}",
                        call.name
                    ),
                ),
                terminate: false,
            });
        }
        let context = self.current_context(agent)?;
        let before_tool_effect = self
            .begin_effect(EffectSubject::HookInvocation {
                hook: HookInvocation::BeforeTool {
                    tool_call_id: call.id.to_string(),
                    tool_name: call.name.clone(),
                },
            })
            .await?;
        let before = self
            .configuration
            .hooks
            .before_tool_call_async(call, context, self.cancellation.clone())
            .await;
        self.settle_hook(before_tool_effect, &before).await?;
        match before {
            Ok(BeforeToolCall::Allow) => {}
            Ok(BeforeToolCall::Normalize { arguments }) => {
                call.arguments = arguments;
            }
            Ok(BeforeToolCall::Block { reason }) => {
                return Ok(PreparedToolCall::Immediate {
                    result: error_tool_result(call, reason),
                    terminate: false,
                });
            }
            Ok(BeforeToolCall::Terminate { reason }) => {
                return Ok(PreparedToolCall::Immediate {
                    result: error_tool_result(call, reason),
                    terminate: true,
                });
            }
            Err(error) => {
                return Ok(PreparedToolCall::Immediate {
                    result: error_tool_result(call, error.message),
                    terminate: false,
                });
            }
        }
        if let Err(error) = validate_tool_arguments(&call.name, tool.schema(), &call.arguments) {
            return Ok(PreparedToolCall::Immediate {
                result: error_tool_result_from_error(call, error),
                terminate: false,
            });
        }
        if self.cancellation.is_cancelled() {
            let mut result = error_tool_result(call, "Operation aborted");
            result.failure = Some(crate::tool::ToolFailure::cancelled());
            return Ok(PreparedToolCall::Immediate {
                result,
                // Pi records this tool failure and gives the provider the
                // already-aborted signal on the next turn. It is not a policy
                // termination hint.
                terminate: false,
            });
        }
        let effect = self
            .begin_effect(EffectSubject::ToolExecution { call: call.clone() })
            .await?;
        Ok(PreparedToolCall::Execute { tool, effect })
    }

    fn start_tool_future<'a>(
        &self,
        tool: &'a Arc<dyn AgentTool>,
        call: ToolCall,
        updates: PendingToolUpdates,
    ) -> ToolFuture<'a> {
        let update_call_id = call.id.clone();
        let update_tool_name = call.name.clone();
        let update_sink = ToolUpdateSink::new({
            let updates = updates.clone();
            move |update| updates.push((update_call_id.clone(), update_tool_name.clone(), update))
        });
        tool.execute(
            call,
            ToolContext {
                cancellation: self.cancellation.clone(),
                metadata: None,
            },
            update_sink,
        )
    }

    async fn finalize_executed_tool(
        &self,
        agent: &AgentInner,
        call: &ToolCall,
        effect: super::EffectTicket,
        execution: Result<AgentToolResult, crate::error::ToolError>,
    ) -> Result<(AgentToolResult, bool), CoreError> {
        let raw_result = match execution {
            Ok(result) if result.tool_call_id == call.id => result,
            Ok(result) => error_tool_result(
                call,
                format!(
                    "Tool {} returned mismatched tool-call ID {}",
                    call.name, result.tool_call_id
                ),
            ),
            Err(error) => error_tool_result_from_error(call, error),
        };
        let mut result = raw_result.clone();
        let context = self.current_context(agent)?;
        let after_tool_effect = self
            .begin_effect(EffectSubject::HookInvocation {
                hook: HookInvocation::AfterTool {
                    tool_call_id: call.id.to_string(),
                    tool_name: call.name.clone(),
                },
            })
            .await?;
        let after = self
            .configuration
            .hooks
            .after_tool_call_async(call, &raw_result, context, self.cancellation.clone())
            .await;
        self.settle_hook(after_tool_effect, &after).await?;
        let terminate = match after {
            Ok(after) => {
                apply_after_tool_call(&mut result, after);
                result.terminate
            }
            Err(error) => {
                result = error_tool_result(call, error.message);
                false
            }
        };
        self.settle_effect(
            effect,
            EffectOutcome::ToolExecution(ToolEffectOutcome {
                raw_result,
                result: result.clone(),
            }),
        )
        .await?;
        Ok((result, terminate))
    }

    async fn append_skipped_sequential_calls(
        &self,
        agent: &AgentInner,
        calls: &[AgentToolCall],
        terminal_message: &str,
    ) -> Result<(), CoreError> {
        let bounded = crate::tool::truncate_middle(terminal_message, 512);
        for assistant_call in calls {
            let call = ToolCall {
                id: assistant_call.id.clone(),
                name: assistant_call.name.clone(),
                arguments: assistant_call.arguments.clone(),
            };
            let mut result = error_tool_result(
                &call,
                format!("Tool call was not executed after terminal capability failure: {bounded}"),
            );
            result.failure = Some(crate::tool::ToolFailure::recoverable());
            // The capability never ran, so no execution start/end event is
            // emitted. The canonical result still closes the assistant call.
            self.append_tool_result_message(agent, call, result).await?;
        }
        Ok(())
    }

    async fn observe_tool_failure(
        &self,
        agent: &AgentInner,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<Option<TerminalToolFailure>, CoreError> {
        if self.cancellation.is_cancelled() {
            return Ok(None);
        }
        if !result.is_error {
            self.policy
                .lock()
                .expect("run policy mutex poisoned")
                .failure_streak = None;
            return Ok(None);
        }
        let failure = result
            .failure
            .as_ref()
            .cloned()
            .unwrap_or_else(crate::tool::ToolFailure::recoverable);
        let signature = failure.signature().cloned();
        let observe = matches!(
            failure.disposition(),
            crate::tool::ToolFailureDisposition::Retryable
                | crate::tool::ToolFailureDisposition::Fatal
        ) || agent
            .tool_failure_circuit_breaker
            .max_consecutive_retryable_failures
            .is_some();
        let (consecutive_count, terminal) = {
            let mut policy_state = self.policy.lock().expect("run policy mutex poisoned");
            match (failure.disposition(), signature.clone()) {
                (crate::tool::ToolFailureDisposition::Retryable, Some(signature)) => {
                    let count = match &mut policy_state.failure_streak {
                        Some((previous, count)) if previous == &signature => {
                            *count = count.saturating_add(1);
                            *count
                        }
                        slot => {
                            *slot = Some((signature, 1));
                            1
                        }
                    };
                    let tripped = agent
                        .tool_failure_circuit_breaker
                        .max_consecutive_retryable_failures
                        .is_some_and(|limit| count >= limit.get());
                    (count, tripped)
                }
                (crate::tool::ToolFailureDisposition::Fatal, Some(signature)) => {
                    policy_state.failure_streak = Some((signature, 1));
                    (1, true)
                }
                _ => {
                    // An uncorrelated ordinary failure cannot be a consecutive
                    // retry against the same dead capability.
                    policy_state.failure_streak = None;
                    (0, false)
                }
            }
        };
        if !observe {
            return Ok(terminal.then_some(TerminalToolFailure {
                message: crate::tool::truncate_middle(
                    failure
                        .recovery_guidance()
                        .unwrap_or(result.content.as_str()),
                    1024,
                ),
            }));
        }
        let message = crate::tool::truncate_middle(
            failure
                .recovery_guidance()
                .unwrap_or(result.content.as_str()),
            1024,
        );
        self.emit(
            agent,
            AgentEventKind::ToolFailureObserved {
                tool_call_id: call.id.clone(),
                disposition: failure.disposition(),
                signature: signature.map(|signature| signature.as_str().to_owned()),
                consecutive_count,
                terminal,
                message: message.clone(),
            },
        )
        .await?;
        Ok(terminal.then_some(TerminalToolFailure { message }))
    }

    /// Flush any callbacks that raced with an awaited hook or lifecycle
    /// observer before the terminal event for the current tool is emitted.
    async fn flush_tool_updates(
        &self,
        agent: &AgentInner,
        updates: &PendingToolUpdates,
    ) -> Result<(), CoreError> {
        while let Some(updates) = updates.take() {
            self.emit_tool_updates(agent, updates).await?;
        }
        Ok(())
    }

    async fn emit_tool_updates(
        &self,
        agent: &AgentInner,
        updates: Vec<PendingToolUpdate>,
    ) -> Result<(), CoreError> {
        for (tool_call_id, tool_name, update) in updates {
            self.emit(
                agent,
                AgentEventKind::ToolExecutionUpdate {
                    tool_call_id,
                    tool_name,
                    update,
                },
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn emit_tool_execution_end(
        &self,
        agent: &AgentInner,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<(), CoreError> {
        {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            state.pending_tool_calls.remove(&call.id);
        }
        self.emit(
            agent,
            AgentEventKind::ToolExecutionEnd {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                result: result.clone(),
            },
        )
        .await?;
        Ok(())
    }

    pub(super) async fn append_tool_result_message(
        &self,
        agent: &AgentInner,
        call: ToolCall,
        result: AgentToolResult,
    ) -> Result<(), CoreError> {
        // A durable host receives the complete post-policy result before the
        // in-memory message exists. This also covers schema-invalid and
        // blocked calls, which deliberately have no ToolStarted intent but
        // still require an ordinary durable semantic result.
        let durable_write = self
            .begin_effect(EffectSubject::DurableWrite {
                write: DurableWriteRequest::ToolResult {
                    call: call.clone(),
                    result: result.clone(),
                },
            })
            .await?;
        self.settle_effect(
            durable_write,
            EffectOutcome::DurableWrite(EffectCompletion::Succeeded),
        )
        .await?;
        let message = {
            let mut state = agent.state.lock().expect("agent state mutex poisoned");
            let message = AgentMessage::ToolResult {
                id: state.allocate_message_id(),
                tool_call_id: call.id,
                tool_name: call.name,
                content: result.content,
                details: result.details,
                usage: result.usage,
                added_tool_names: result.added_tool_names,
                terminate: result.terminate,
                is_error: result.is_error,
                failure: result.failure,
            };
            state.append_message(message.clone());
            message
        };
        self.emit(
            agent,
            AgentEventKind::MessageStart {
                message: message.clone(),
            },
        )
        .await?;
        self.emit(agent, AgentEventKind::MessageEnd { message })
            .await?;
        Ok(())
    }
}

fn normalize_result_failure(result: &mut AgentToolResult) {
    if result.is_error && result.failure.is_none() {
        result.failure = Some(crate::tool::ToolFailure::recoverable());
    }
    if !result.is_error {
        result.failure = None;
    }
}
