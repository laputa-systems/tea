//! Safe, host-local response fixtures for interactive terminal exploration.
//!
//! This is intentionally not a `tea-core` provider adapter: it has no transport,
//! credentials, workspace authority, or provider registry entry. Its only tool is
//! a no-op `edit` capability, so every mock operation is consequence-free.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::scheduler::{
    CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture, ModelProvider,
    ModelRequest, ModelStreamEvent,
};
use tea_core::state::{AgentToolCall, ModelDescriptor, SerializedJson, StopReason, ToolCallId};
use tea_core::tool::{
    AgentTool, AgentToolResult, ToolCall, ToolContext, ToolFuture, ToolRegistry, ToolUpdate,
    ToolUpdateSink,
};
use tea_core::agent::AgentConfiguration;
use tea_protocol::JsonValue;
use tea_providers::{openai::OpenAiContextHook, ConfiguredProvider};

pub(super) const PROVIDER_ID: &str = "mock";
pub(super) const DEFAULT_MODEL_ID: &str = "mock";
pub(super) const CONTEXT_WINDOW: u64 = 16_384;

const COMPACTION_SUMMARY: &str = r#"## Goal
Continue exploring the safe mock terminal session.

## Constraints & Preferences
- The mock provider and its edit preview must have no workspace side effects.

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
- The mock `edit` capability reports a preview and never changes files."#;

/// Build the isolated configuration used only by the mock provider.
pub(super) fn configuration() -> AgentConfiguration {
    let mut tools = ToolRegistry::default();
    tools.insert(Arc::new(MockEditTool));
    AgentConfiguration::new(
        "You are a safe terminal mock. Produce concise Markdown, code samples, or an edit preview. The edit tool never changes files.",
        tools,
        Arc::new(OpenAiContextHook),
    )
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
    awaiting_edit_follow_up: AtomicBool,
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
        } else if self.awaiting_edit_follow_up.swap(false, Ordering::AcqRel) {
            vec![
                ModelStreamEvent::TextDelta(
                    "The mock edit completed successfully. No files were changed.".into(),
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
                    self.awaiting_edit_follow_up.store(true, Ordering::Release);
                    let call_id = self.sequence.fetch_add(1, Ordering::Relaxed);
                    vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new(format!("mock-edit-{call_id}"))
                                .expect("mock tool call ID is non-empty"),
                            name: "edit".into(),
                            arguments: SerializedJson::new(
                                r#"{"path":"demo.md","edits":[{"oldText":"before","newText":"after"}]}"#,
                            ),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ]
                }
            }
        };
        Box::pin(std::future::ready(Ok(Box::new(MockStream {
            events,
            initial_delay: Some(self.thinking_delay()),
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
}

impl ModelEventStream for MockStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        let delay = self.initial_delay.take();
        Box::pin(async move {
            if let Some(delay) = delay {
                let cancellation_wait = cancellation.clone();
                smol::future::or(
                    // The race result is intentionally ignored; keep both branches `()`.
                    async move {
                        smol::Timer::after(delay).await;
                    },
                    async move { cancellation_wait.cancelled().await },
                )
                .await;
            }
            if cancellation.is_cancelled() {
                return Ok(Some(ModelStreamEvent::End(StopReason::Cancelled)));
            }
            Ok((!self.events.is_empty()).then(|| self.events.remove(0)))
        })
    }
}

#[derive(Debug)]
struct MockEditTool;

impl AgentTool for MockEditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Preview an edit without reading or changing any workspace file."
    }

    fn schema(&self) -> &JsonValue {
        static_schema()
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        updates.emit(ToolUpdate {
            content: "Mock edit preview: no files changed.".into(),
            details: None,
        });
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "Mock edit completed. No files were changed.".into(),
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        })))
    }
}

fn static_schema() -> &'static JsonValue {
    static SCHEMA: OnceLock<JsonValue> = OnceLock::new();
    SCHEMA.get_or_init(|| JsonValue::object([("type", JsonValue::String("object".into()))]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_configuration_exposes_only_the_no_op_edit_capability() {
        let configuration = configuration();
        assert_eq!(configuration.tools.names().collect::<Vec<_>>(), ["edit"]);
    }
}
