use std::sync::{Arc, Mutex};
use tea_core::compaction::{
    CompactionContext, CompactionError, CompactionFuture, CompactionLifecycleRecord,
    CompactionResult, CompactionTerminalOutcome, Compactor,
};
use tea_core::event::{AgentEventKind, CompactionOutcome};
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::StopReason;
use tea_core::error::CoreError;
use tea_core::Agent;

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

struct KeepFirstMessage;

impl Compactor for KeepFirstMessage {
    fn compact<'a>(
        &'a self,
        mut context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        context.messages.truncate(1);
        Box::pin(std::future::ready(Ok(CompactionResult::new(
            context.messages,
        ))))
    }
}

struct DuplicateMessage;

impl Compactor for DuplicateMessage {
    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        let message = context.messages[0].clone();
        Box::pin(std::future::ready(Ok(CompactionResult::new(vec![
            message.clone(),
            message,
        ]))))
    }
}

struct FailingCompactor;

struct TimedOutCompactor;

impl Compactor for FailingCompactor {
    fn compact<'a>(
        &'a self,
        _context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        Box::pin(std::future::ready(Err(CompactionError::failed(
            "fixture compactor failed",
        ))))
    }
}

impl Compactor for TimedOutCompactor {
    fn compact<'a>(
        &'a self,
        _context: CompactionContext,
        _cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        Box::pin(std::future::ready(Err(CompactionError::timed_out(
            "fixture deadline elapsed",
        ))))
    }
}

fn provider_with_answers(answers: &[&str]) -> Arc<FixtureProvider> {
    Arc::new(FixtureProvider {
        streams: Mutex::new(
            answers
                .iter()
                .map(|answer| ModelStream {
                    events: vec![
                        ModelStreamEvent::TextDelta((*answer).into()),
                        ModelStreamEvent::End(StopReason::Stop),
                    ],
                })
                .collect(),
        ),
    })
}

#[test]
fn compaction_replaces_context_emits_its_grammar_and_allows_reuse() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(provider_with_answers(&["first answer", "second answer"]))
            .compactor(Arc::new(KeepFirstMessage))
            .build();
        agent
            .start_prompt("first prompt")
            .expect("first run starts")
            .drive()
            .await
            .expect("first run succeeds");

        let compaction = agent.start_compaction().expect("compaction starts");
        compaction.drive().await.expect("compaction succeeds");

        let snapshot = agent.snapshot();
        assert_eq!(snapshot.messages.len(), 1);
        assert!(compaction.events().iter().any(|event| {
            matches!(
                event.kind,
                AgentEventKind::CompactionStart {
                    source_message_count: 2
                }
            )
        }));
        assert!(compaction.events().iter().any(|event| {
            matches!(
                event.kind,
                AgentEventKind::CompactionResult {
                    retained_message_count: 1,
                    ..
                }
            )
        }));
        assert!(matches!(
            compaction.events().last().map(|event| &event.kind),
            Some(AgentEventKind::CompactionEnd {
                outcome: CompactionOutcome::Succeeded {
                    retained_message_count: 1
                }
            })
        ));
        let lifecycle: Vec<_> = compaction
            .events()
            .into_iter()
            .filter_map(|event| match event.kind {
                AgentEventKind::CompactionLifecycle { record } => Some(record),
                _ => None,
            })
            .collect();
        assert_eq!(lifecycle.len(), 6);
        let ids: Vec<_> = lifecycle
            .iter()
            .map(|record| match record {
                CompactionLifecycleRecord::Started { operation } => operation.id,
                CompactionLifecycleRecord::SourceSelected { id, .. }
                | CompactionLifecycleRecord::RequestPrepared { id, .. }
                | CompactionLifecycleRecord::ProviderUsageObserved { id, .. }
                | CompactionLifecycleRecord::ReplacementProposed { id, .. }
                | CompactionLifecycleRecord::Terminal { id, .. } => *id,
            })
            .collect();
        assert!(ids.iter().all(|id| *id == ids[0]));
        assert!(matches!(
            lifecycle.last(),
            Some(CompactionLifecycleRecord::Terminal {
                outcome: CompactionTerminalOutcome::Committed,
                ..
            })
        ));

        agent
            .start_prompt("second prompt")
            .expect("compacted agent is idle and reusable")
            .drive()
            .await
            .expect("second run succeeds");
    });
}

#[test]
fn invalid_replacement_and_compactor_failure_preserve_history() {
    smol::block_on(async {
        let invalid_agent = Agent::builder()
            .model_provider(provider_with_answers(&["answer"]))
            .compactor(Arc::new(DuplicateMessage))
            .build();
        invalid_agent
            .start_prompt("prompt")
            .expect("run starts")
            .drive()
            .await
            .expect("run succeeds");
        let original = invalid_agent.snapshot().messages;
        let error = invalid_agent
            .start_compaction()
            .expect("compaction reserves idle agent")
            .drive()
            .await
            .expect_err("duplicate message IDs are invalid");
        assert!(matches!(error, CoreError::Compaction(_)));
        assert_eq!(invalid_agent.snapshot().messages, original);

        let failing_agent = Agent::builder()
            .model_provider(provider_with_answers(&["answer"]))
            .compactor(Arc::new(FailingCompactor))
            .build();
        failing_agent
            .start_prompt("prompt")
            .expect("run starts")
            .drive()
            .await
            .expect("run succeeds");
        let original = failing_agent.snapshot().messages;
        let error = failing_agent
            .start_compaction()
            .expect("compaction reserves idle agent")
            .drive()
            .await
            .expect_err("compactor failure is surfaced");
        assert!(matches!(error, CoreError::Compaction(_)));
        assert_eq!(failing_agent.snapshot().messages, original);
    });
}

#[test]
fn compaction_rejects_an_active_run_and_cancellation_preserves_history() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(provider_with_answers(&["answer"]))
            .compactor(Arc::new(KeepFirstMessage))
            .build();
        let active = agent.start_prompt("active").expect("run starts");
        assert!(matches!(
            agent.start_compaction(),
            Err(CoreError::ActiveRun { .. })
        ));
        active.abort().expect("created run aborts");

        let original = agent.snapshot().messages;
        let cancellable = agent.start_compaction().expect("compaction starts");
        agent.abort();
        assert!(matches!(
            cancellable.drive().await,
            Err(CoreError::Cancelled)
        ));
        assert!(matches!(
            cancellable.events().last().map(|event| &event.kind),
            Some(AgentEventKind::CompactionEnd {
                outcome: CompactionOutcome::Cancelled
            })
        ));
        assert_eq!(agent.snapshot().messages, original);
    });
}

#[test]
fn typed_compactor_timeout_preserves_history_and_lifecycle() {
    smol::block_on(async {
        let agent = Agent::builder()
            .model_provider(provider_with_answers(&["answer"]))
            .compactor(Arc::new(TimedOutCompactor))
            .build();
        agent.start_prompt("prompt")?.drive().await?;
        let original = agent.snapshot().messages;
        let compaction = agent.start_compaction()?;
        assert!(matches!(
            compaction.drive().await,
            Err(CoreError::Compaction(CompactionError::TimedOut { .. }))
        ));
        assert_eq!(agent.snapshot().messages, original);
        assert!(compaction.events().iter().any(|event| matches!(
            event.kind,
            AgentEventKind::CompactionLifecycle {
                record: CompactionLifecycleRecord::Terminal {
                    outcome: CompactionTerminalOutcome::TimedOut,
                    ..
                }
            }
        )));
        Ok::<(), CoreError>(())
    })
    .expect("typed timeout remains transactional");
}
