//! Safe, host-local response fixtures for interactive terminal exploration.
//!
//! This is intentionally not a `tea-core` provider adapter: it has no
//! transport, credentials, workspace authority, or provider registry entry.
//! It uses the normal Luau coding builtins with no-effect operation adapters, so
//! every mock operation is consequence-free without a model-facing Rust tool.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::agent::AgentConfiguration;
use tea_core::coding::{
    CodingOperations, CommandEnvironment, CommandOutput, CommandTermination, EditTransaction,
    EditTransactionOutcome, EntryMetadata, OperationError, OperationFuture, SearchResult,
    SearchTruncation,
};
use tea_core::scheduler::{
    CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture, ModelProvider,
    ModelRequest, ModelStreamEvent,
};
use tea_core::state::{AgentToolCall, ModelDescriptor, SerializedJson, StopReason, ToolCallId};
use tea_core::tool::{ToolRegistry, ToolUpdateSink};
use tea_providers::{openai::OpenAiContextHook, ConfiguredProvider};

pub(super) const PROVIDER_ID: &str = "mock";
pub(super) const DEFAULT_MODEL_ID: &str = "mock";
pub(super) const CONTEXT_WINDOW: u64 = 16_384;

const COMPACTION_SUMMARY: &str = r#"## Goal
Continue exploring the safe mock terminal session.

## Constraints & Preferences
- The mock provider's coding operations have no workspace or process side effects.

## Progress
### Done
- The mock provider is configured with a 16k context window.

### In Progress
- Continue the current terminal interaction.

### Blocked
- None

## Key Decisions
- Mock compaction returns a deterministic structured summary without tools.

## Next Steps
1. Continue with the next submitted prompt.

## Critical Context
- The mock coding capabilities report safe fixture results and never change files."#;

/// Build the isolated configuration used only by the mock provider.
pub(super) fn configuration() -> AgentConfiguration {
    AgentConfiguration::new(
        "You are a safe terminal mock. Produce concise Markdown or code samples. Coding tools use safe mock capabilities and never change workspace files or start processes.",
        ToolRegistry::default(),
        Arc::new(OpenAiContextHook),
    )
}

/// Return the no-effect host port used only by the mock terminal profile.
pub(super) fn coding_operations() -> Arc<dyn CodingOperations> {
    Arc::new(MockCodingOperations)
}

/// Resolve one host-local mock provider without involving the core registry.
pub(super) fn configured_provider(model: &str) -> ConfiguredProvider {
    ConfiguredProvider {
        descriptor: ModelDescriptor {
            provider: PROVIDER_ID.into(),
            model: model.into(),
            revision: None,
        },
        provider: Arc::new(MockProvider::default()),
    }
}

#[derive(Debug, Default)]
struct MockProvider {
    sequence: AtomicUsize,
    awaiting_tool_follow_up: AtomicBool,
}

impl ModelProvider for MockProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let events = if cancellation.is_cancelled() {
            vec![ModelStreamEvent::End(StopReason::Cancelled)]
        } else if super::compaction::is_compaction_request(&request) {
            vec![
                ModelStreamEvent::TextDelta(COMPACTION_SUMMARY.into()),
                ModelStreamEvent::End(StopReason::Stop),
            ]
        } else if self.awaiting_tool_follow_up.swap(false, Ordering::AcqRel) {
            vec![
                ModelStreamEvent::TextDelta(
                    "The mock tool completed successfully. No files were changed.".into(),
                ),
                ModelStreamEvent::End(StopReason::Stop),
            ]
        } else {
            match self.response_index() {
                0 => vec![
                    ModelStreamEvent::TextDelta(
                        "## Mock response\n\nThis is a safe response fixture for exploring the terminal UI.".into(),
                    ),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
                1 => vec![
                    ModelStreamEvent::TextDelta(
                        "```rust\nfn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n```".into(),
                    ),
                    ModelStreamEvent::End(StopReason::Stop),
                ],
                _ => {
                    self.awaiting_tool_follow_up.store(true, Ordering::Release);
                    let call_id = self.sequence.fetch_add(1, Ordering::Relaxed);
                    vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new(format!("mock-find-{call_id}"))
                                .expect("mock tool call ID is non-empty"),
                            name: "find".into(),
                            arguments: SerializedJson::new(r#"{"pattern":"*.rs","limit":1}"#),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ]
                }
            }
        };
        Box::pin(std::future::ready(Ok(Box::new(MockStream {
            events,
            initial_delay: Some(self.thinking_delay()),
            cancellation_terminal_emitted: false,
        }) as _)))
    }
}

impl MockProvider {
    fn response_index(&self) -> usize {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos() as usize)
            .unwrap_or_default();
        sequence.wrapping_mul(1_103_515_245).wrapping_add(nanos) % 3
    }

    fn thinking_delay(&self) -> Duration {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos() as usize)
            .unwrap_or_default();
        Duration::from_secs(((sequence ^ nanos) % 10 + 1) as u64)
    }
}

/// Finite mock events with one cancellable, visible thinking delay per request.
struct MockStream {
    events: Vec<ModelStreamEvent>,
    initial_delay: Option<Duration>,
    /// A finite model stream may expose its terminal cancellation exactly once.
    /// Core polls once more after a terminal event to verify that the source
    /// has closed, so re-emitting cancellation would violate that boundary.
    cancellation_terminal_emitted: bool,
}

impl ModelEventStream for MockStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        let delay = self.initial_delay.take();
        Box::pin(async move {
            if let Some(delay) = delay {
                let cancellation_wait = cancellation.clone();
                smol::future::or(
                    async move {
                        smol::Timer::after(delay).await;
                    },
                    async move { cancellation_wait.cancelled().await },
                )
                .await;
            }
            if cancellation.is_cancelled() {
                self.events.clear();
                return Ok((!self.cancellation_terminal_emitted).then(|| {
                    self.cancellation_terminal_emitted = true;
                    ModelStreamEvent::End(StopReason::Cancelled)
                }));
            }
            Ok((!self.events.is_empty()).then(|| self.events.remove(0)))
        })
    }
}

/// Test-only operation port behind the production coding capability contract.
/// It permits read-only fixture inspection, but mutation commits and process
/// calls report deterministic success without publishing or spawning anything.
#[derive(Clone, Debug, Default)]
struct MockCodingOperations;

impl CodingOperations for MockCodingOperations {
    fn read_file<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, Vec<u8>> {
        let path = path.to_path_buf();
        Box::pin(
            async move { fs::read(path).map_err(|error| OperationError::new(error.to_string())) },
        )
    }

    fn metadata<'a>(&'a self, path: &'a Path) -> OperationFuture<'a, EntryMetadata> {
        let path = path.to_path_buf();
        Box::pin(async move {
            let metadata =
                fs::metadata(path).map_err(|error| OperationError::new(error.to_string()))?;
            Ok(EntryMetadata {
                is_directory: metadata.is_dir(),
                is_regular_file: metadata.is_file(),
            })
        })
    }

    fn commit_edit_transaction<'a>(
        &'a self,
        _transaction: &'a EditTransaction,
        cancellation: CancellationToken,
    ) -> OperationFuture<'a, EditTransactionOutcome> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(OperationError::new("cancelled"))
            } else {
                Ok(EditTransactionOutcome::Committed)
            }
        })
    }

    fn find_files<'a>(
        &'a self,
        _root: &'a Path,
        _pattern: &'a str,
        _max_results: usize,
        _max_output_bytes: usize,
        cancellation: CancellationToken,
    ) -> OperationFuture<'a, SearchResult> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(OperationError::new("cancelled"))
            } else {
                Ok(SearchResult {
                    matches: Vec::new(),
                    truncation: SearchTruncation::Complete,
                })
            }
        })
    }

    fn execute_command<'a>(
        &'a self,
        _command: &'a str,
        _cwd: &'a Path,
        _timeout: Duration,
        _environment: &'a CommandEnvironment,
        cancellation: CancellationToken,
        _updates: ToolUpdateSink,
    ) -> OperationFuture<'a, CommandOutput> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                Err(OperationError::new("cancelled"))
            } else {
                Ok(CommandOutput {
                    termination: CommandTermination::Exited { code: 0 },
                    stdout: b"Mock command preview: no process started.".to_vec(),
                    stderr: Vec::new(),
                })
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_configuration_leaves_model_facing_tools_to_the_luau_coding_builtins() {
        let configuration = configuration();
        assert_eq!(configuration.tools.names().next(), None);
    }

    #[test]
    fn cancelled_mock_stream_emits_one_terminal_event_then_closes() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut stream = MockStream {
            events: vec![ModelStreamEvent::TextDelta("unreachable".into())],
            initial_delay: None,
            cancellation_terminal_emitted: false,
        };

        assert!(matches!(
            smol::block_on(stream.next_event(cancellation.clone()))
                .expect("mock cancellation polling succeeds"),
            Some(ModelStreamEvent::End(StopReason::Cancelled))
        ));
        assert!(
            smol::block_on(stream.next_event(cancellation))
                .expect("mock cancellation close polling succeeds")
                .is_none(),
            "a finite stream must close after its terminal cancellation event"
        );
    }
}
