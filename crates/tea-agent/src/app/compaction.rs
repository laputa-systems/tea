//! Provider-backed compaction policy for the repository-owned terminal host.
//!
//! The core deliberately does not choose a summary prompt or provider. This
//! host supplies both explicitly by reusing the selected provider for a
//! tool-execution-prohibited summarization request and retaining the exact
//! suffix selected by the core's automatic-compaction split.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use tea_core::compaction::{
    AutomaticCompactionRequest, CompactionContext, CompactionError, CompactionFuture,
    CompactionRequestLayout, CompactionRequestPort, CompactionResult, CompactionStrategy,
    Compactor, ProviderContext,
};
use tea_core::effect::CompactionProviderEffectOutcome;
use tea_core::scheduler::{
    CancellationToken, ModelProvider, ModelRequest, ModelStreamEvent, RequestPurpose,
};
use tea_core::state::{
    AgentMessage, ConfigurationUpdate, MessageId, ModelDescriptor, StopReason, ThinkingLevel,
    Usage,
};
use tea_core::transcript::{ConfigurationProjection, Transcript};

const SUMMARY_SYSTEM_PROMPT: &str = r#"You compact coding-agent conversation history.
Produce a concise structured summary that preserves everything needed to continue the work.
Use exactly these Markdown sections:

## Goal
[The user's active goal]

## Constraints & Preferences
- [Requirements and preferences]

## Progress
### Done
- [Completed work]

### In Progress
- [Current work]

### Blocked
- [Current blockers, or "None"]

## Key Decisions
- [Important decisions and rationale]

## Next Steps
1. [The next concrete actions]

## Critical Context
- [Exact paths, symbols, errors, and other details needed to continue]

Be concise, preserve exact file paths and identifiers, and do not omit unresolved work."#;

const SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const SUMMARY_SUFFIX: &str = "\n</summary>";
const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = r#"Update the existing compacted summary using the conversation above.
Preserve all durable facts needed to continue the work, including exact paths, symbols, errors,
constraints, unresolved work, and the next concrete actions. Return only the updated summary using
the same Markdown sections as the system instructions; do not call tools."#;
const CACHE_FRIENDLY_CONTEXT_SAFETY_MARGIN: u64 = 4_096;

/// One immutable provider/model compactor for a durable runtime-service bundle.
pub(super) struct ProviderCompactor {
    provider: Arc<dyn ModelProvider>,
    model: ModelDescriptor,
    tool_free_requests: bool,
    thinking_level: ThinkingLevel,
}

impl fmt::Debug for ProviderCompactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCompactor")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl ProviderCompactor {
    /// Bind one compactor to the exact provider/model descriptor selected by the host.
    pub(super) fn new(model: ModelDescriptor, provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            provider,
            tool_free_requests: model.provider == "codex",
            model,
            thinking_level: ThinkingLevel::Off,
        }
    }

    /// Pin a host-selected effort for every summary request in this compactor.
    #[cfg(feature = "live-verification")]
    pub(super) fn with_thinking_level(mut self, level: ThinkingLevel) -> Self {
        self.thinking_level = level;
        self
    }

    fn configured(
        &self,
        context: &CompactionContext,
    ) -> Result<(Arc<dyn ModelProvider>, ModelDescriptor), CompactionError> {
        if let Some(model) = &context.model {
            if model != &self.model {
                return Err(CompactionError::failed(
                    "compaction context model does not match the immutable configured provider",
                ));
            }
        }
        Ok((Arc::clone(&self.provider), self.model.clone()))
    }
}

impl Compactor for ProviderCompactor {
    fn strategy(&self) -> CompactionStrategy {
        CompactionStrategy::cache_replay_summary_v1(baseline_prompt_fingerprint())
    }

    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        self.compact_with_request_port(context, cancellation, None)
    }

    fn compact_with_requests<'a>(
        &'a self,
        context: CompactionContext,
        cancellation: CancellationToken,
        requests: &'a dyn CompactionRequestPort,
    ) -> CompactionFuture<'a> {
        self.compact_with_request_port(context, cancellation, Some(requests))
    }

    fn compact_automatic<'a>(
        &'a self,
        context: CompactionContext,
        request: AutomaticCompactionRequest,
        cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        self.compact_automatic_with_request_port(context, request, cancellation, None)
    }

    fn compact_automatic_with_requests<'a>(
        &'a self,
        context: CompactionContext,
        request: AutomaticCompactionRequest,
        cancellation: CancellationToken,
        requests: &'a dyn CompactionRequestPort,
    ) -> CompactionFuture<'a> {
        self.compact_automatic_with_request_port(context, request, cancellation, Some(requests))
    }
}

impl ProviderCompactor {
    fn compact_with_request_port<'a>(
        &'a self,
        context: CompactionContext,
        cancellation: CancellationToken,
        requests: Option<&'a dyn CompactionRequestPort>,
    ) -> CompactionFuture<'a> {
        let configured = self.configured(&context);
        let tool_free_requests = self.tool_free_requests;
        let thinking_level = self.thinking_level;
        Box::pin(async move {
            let (provider, model) = configured?;
            if context.messages.is_empty() {
                return Ok(CompactionResult::new(Vec::new()));
            }
            let prepared = prepare_summary_request(
                model,
                context.messages.clone(),
                None,
                tool_free_requests,
                context.session_id.clone(),
                thinking_level,
            )?;
            let layout = prepared.layout;
            let source_is_active_context_prefix = prepared.source_is_active_context_prefix;
            let (summary, usage, request_observation) =
                summarize(provider, prepared.request, cancellation, requests).await?;
            let replacement = vec![summary_message(&context.messages, summary)?];
            let result = match usage {
                Some(usage) => CompactionResult::new(replacement).with_usage(usage),
                None => CompactionResult::new(replacement),
            }
            .with_request_layout(layout, source_is_active_context_prefix);
            Ok(match request_observation {
                Some(observation) => result.with_request_observation(observation),
                None => result,
            })
        })
    }

    fn compact_automatic_with_request_port<'a>(
        &'a self,
        context: CompactionContext,
        request: AutomaticCompactionRequest,
        cancellation: CancellationToken,
        requests: Option<&'a dyn CompactionRequestPort>,
    ) -> CompactionFuture<'a> {
        let configured = self.configured(&context);
        let tool_free_requests = self.tool_free_requests;
        let thinking_level = self.thinking_level;
        Box::pin(async move {
            let (provider, model) = configured?;
            let source_context = context
                .provider_context
                .as_ref()
                .filter(|source| source_context_fits(source, &request));
            let mut messages_to_summarize = request.prefix_messages;
            messages_to_summarize.extend(request.split_turn_prefix);
            if messages_to_summarize.is_empty() {
                return Ok(CompactionResult::new(request.retained_messages));
            }
            let prepared = prepare_summary_request(
                model,
                messages_to_summarize.clone(),
                source_context,
                tool_free_requests,
                context.session_id.clone(),
                thinking_level,
            )?;
            let layout = prepared.layout;
            let source_is_active_context_prefix = prepared.source_is_active_context_prefix;
            let (summary, usage, request_observation) =
                summarize(provider, prepared.request, cancellation, requests).await?;
            let retained_messages = request.retained_messages;
            let mut all_messages = messages_to_summarize;
            all_messages.extend(retained_messages.iter().cloned());
            let mut replacement = vec![summary_message(&all_messages, summary)?];
            replacement.extend(retained_messages);
            let result = match usage {
                Some(usage) => CompactionResult::new(replacement).with_usage(usage),
                None => CompactionResult::new(replacement),
            }
            .with_request_layout(layout, source_is_active_context_prefix);
            Ok(match request_observation {
                Some(observation) => result.with_request_observation(observation),
                None => result,
            })
        })
    }
}

struct PreparedSummaryRequest {
    request: ModelRequest,
    layout: CompactionRequestLayout,
    source_is_active_context_prefix: Option<bool>,
}

fn summary_message(
    messages: &[AgentMessage],
    summary: String,
) -> Result<AgentMessage, CompactionError> {
    Ok(AgentMessage::User {
        id: next_message_id(messages),
        content: format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"),
    })
}

fn next_message_id(messages: &[AgentMessage]) -> MessageId {
    let used = messages
        .iter()
        .map(|message| message.id().0)
        .collect::<BTreeSet<_>>();
    let mut candidate = 1_u64;
    while used.contains(&candidate) {
        candidate = candidate.saturating_add(1);
    }
    MessageId(candidate)
}

fn prepare_summary_request(
    model: ModelDescriptor,
    messages: Vec<AgentMessage>,
    source_context: Option<&ProviderContext>,
    tool_free_requests: bool,
    session_id: Option<String>,
    thinking_level: ThinkingLevel,
) -> Result<PreparedSummaryRequest, CompactionError> {
    let (transcript, layout, source_is_active_context_prefix) = if let Some(source) =
        source_context
    {
        // Reuse the active request's exact typed transcript and append one
        // summary instruction, so the summary request extends the cached
        // conversation prefix. Tool execution is prohibited by the compactor
        // stream; retaining the declarations keeps the prompt-facing envelope
        // aligned with the ordinary request.
        let mut transcript = source.source.clone();
        let next_id = transcript
            .messages
            .iter()
            .map(|message| message.id().0)
            .max()
            .unwrap_or(0);
        if tool_free_requests {
            let tools_removed = transcript
                .tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect::<Vec<_>>();
            if !tools_removed.is_empty() {
                transcript.messages.push(AgentMessage::System {
                    id: MessageId(next_id.saturating_add(1)),
                    update: ConfigurationUpdate {
                        tools_removed,
                        ..ConfigurationUpdate::default()
                    },
                });
            }
        }
        transcript.messages.push(AgentMessage::User {
            id: MessageId(next_id.saturating_add(2)),
            content: UPDATE_SUMMARIZATION_INSTRUCTIONS.into(),
        });
        (
            transcript,
            CompactionRequestLayout::ExactReplay,
            Some(source.source_is_active_prefix()),
        )
    } else {
        // The standalone request has its own summary configuration; the
        // conversation's configuration messages are not part of what it
        // summarizes.
        let conversation = messages
            .into_iter()
            .filter(|message| !matches!(message, AgentMessage::System { .. }));
        (
            Transcript::standalone(SUMMARY_SYSTEM_PROMPT, Vec::new(), conversation),
            CompactionRequestLayout::StandaloneFallback,
            None,
        )
    };
    let session_id = source_context
        .and_then(|source| source.session_id.clone())
        .or(session_id);
    Ok(PreparedSummaryRequest {
        request: ModelRequest {
            purpose: RequestPurpose::Compaction,
            transcript,
            model: Some(model),
            selected_model: None,
            thinking_level,
            session_id,
            max_output_tokens: None,
        },
        layout,
        source_is_active_context_prefix,
    })
}

async fn summarize(
    provider: Arc<dyn ModelProvider>,
    request: ModelRequest,
    cancellation: CancellationToken,
    requests: Option<&dyn CompactionRequestPort>,
) -> Result<
    (
        String,
        Option<Usage>,
        Option<tea_core::scheduler::AdapterRequestObservation>,
    ),
    CompactionError,
> {
    let ticket = match requests {
        Some(requests) => Some(requests.begin_provider_request(request.clone()).await?),
        None => None,
    };
    let result = summarize_ungated(provider, request, cancellation.clone()).await;
    if let (Some(requests), Some(ticket)) = (requests, ticket) {
        let outcome = match &result {
            Ok((_, usage, request_observation)) => CompactionProviderEffectOutcome::Succeeded {
                usage: usage.clone(),
                request_observation: request_observation.clone(),
            },
            Err(_) if cancellation.is_cancelled() => CompactionProviderEffectOutcome::Cancelled,
            Err(error) => CompactionProviderEffectOutcome::Failed {
                message: error.to_string(),
            },
        };
        requests.settle_provider_request(ticket, outcome).await?;
    }
    result
}

async fn summarize_ungated(
    provider: Arc<dyn ModelProvider>,
    request: ModelRequest,
    cancellation: CancellationToken,
) -> Result<
    (
        String,
        Option<Usage>,
        Option<tea_core::scheduler::AdapterRequestObservation>,
    ),
    CompactionError,
> {
    let mut stream = provider
        .stream(request, cancellation.clone())
        .await
        .map_err(|error| CompactionError::failed(error.to_string()))?;
    let mut summary = String::new();
    let mut usage = None;
    let mut request_observation = None;
    loop {
        if cancellation.is_cancelled() {
            return Err(CompactionError::failed("compaction cancelled"));
        }
        let event = stream
            .next_event(cancellation.clone())
            .await
            .map_err(|error| CompactionError::failed(error.to_string()))?
            .ok_or_else(|| {
                CompactionError::failed("compaction provider closed without a terminal event")
            })?;
        match event {
            ModelStreamEvent::RequestObservation(observation) => {
                request_observation = Some(observation)
            }
            ModelStreamEvent::TextDelta(delta) => summary.push_str(&delta),
            // Exposed thinking is never part of the summary text.
            ModelStreamEvent::ThinkingDelta(_)
            | ModelStreamEvent::ThinkingSignature(_)
            | ModelStreamEvent::RedactedThinking(_) => {}
            ModelStreamEvent::Usage(reported) => usage = Some(reported),
            // A compaction summary is an ordinary, tool-free assistant turn.
            // Provider-private continuation state belongs only to the source
            // conversation, so never attach it to the synthetic summary.
            ModelStreamEvent::OpaqueProviderContext(_) => {}
            ModelStreamEvent::End(reason) => {
                if reason == StopReason::Error {
                    return Err(CompactionError::failed(
                        "compaction provider ended with an error",
                    ));
                }
                break;
            }
            ModelStreamEvent::Error { message }
            | ModelStreamEvent::ContextOverflow { message }
            | ModelStreamEvent::Aborted { message } => {
                return Err(CompactionError::failed(message));
            }
            ModelStreamEvent::ProviderError(_) => {}
            ModelStreamEvent::ToolCall(_) => {
                return Err(CompactionError::failed(
                    "compaction provider returned a tool call instead of a summary",
                ));
            }
        }
    }
    if summary.trim().is_empty() {
        return Err(CompactionError::failed(
            "compaction provider returned an empty summary",
        ));
    }
    Ok((summary, usage, request_observation))
}

fn stable_fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Fingerprint the complete checked-in baseline prompt surface without
/// reconstructing or logging a provider request. This is strategy metadata,
/// not a prompt-cache key.
fn baseline_prompt_fingerprint() -> u64 {
    let mut bytes = Vec::new();
    for component in [
        SUMMARY_SYSTEM_PROMPT,
        SUMMARY_PREFIX,
        SUMMARY_SUFFIX,
        UPDATE_SUMMARIZATION_INSTRUCTIONS,
    ] {
        bytes.extend_from_slice(component.as_bytes());
        bytes.push(0);
    }
    stable_fingerprint(&bytes)
}

fn source_context_fits(source: &ProviderContext, request: &AutomaticCompactionRequest) -> bool {
    if source.active.is_some() && !source.source_is_active_prefix() {
        return false;
    }
    let resolved = source.source.resolve(ConfigurationProjection::Collapsed);
    let tool_bytes = resolved
        .leading
        .tools
        .iter()
        .map(|tool| {
            tool.schema
                .to_json_string()
                .map_or(0, |schema| schema.len())
                .saturating_add(tool.name.len())
                .saturating_add(tool.description.len())
        })
        .sum::<usize>();
    let source_bytes = resolved
        .leading
        .system_prompt()
        .len()
        .saturating_add(
            source
                .source
                .layout_context(ConfigurationProjection::Collapsed)
                .len(),
        )
        .saturating_add(tool_bytes);
    let source_tokens = (source_bytes as u64).saturating_add(3) / 4;
    source_tokens
        .saturating_add(request.reserved_tokens)
        .saturating_add(CACHE_FRIENDLY_CONTEXT_SAFETY_MARGIN)
        <= request.context_budget_tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use tea_core::state::{PromptSection, SectionChange, SystemPrompt};
    use tea_core::tool::ToolDeclaration;

    fn model() -> ModelDescriptor {
        ModelDescriptor {
            provider: "codex".into(),
            model: "gpt-test".into(),
            revision: None,
        }
    }

    fn source() -> ProviderContext {
        let tool = ToolDeclaration::new(
            "read",
            "Read a file",
            tea_protocol::JsonValue::object([("type", tea_protocol::JsonValue::from("object"))]),
        );
        let source = Transcript::standalone(
            SystemPrompt::new(vec![PromptSection::new("base", "work")]).expect("prompt"),
            vec![tool],
            [tea_core::transcript::user_message("before")],
        );
        ProviderContext {
            active: Some(source.clone()),
            source,
            model: Some(model()),
            thinking_level: ThinkingLevel::Off,
            session_id: Some("session".into()),
        }
    }

    #[test]
    fn cache_replay_update_extends_the_active_transcript_with_a_user_instruction() {
        let source = source();
        let prepared = prepare_summary_request(
            model(),
            Vec::new(),
            Some(&source),
            false,
            None,
            ThinkingLevel::Off,
        )
        .expect("summary request prepares");
        assert_eq!(prepared.request.purpose, RequestPurpose::Compaction);
        assert_eq!(prepared.layout, CompactionRequestLayout::ExactReplay);
        assert_eq!(prepared.source_is_active_context_prefix, Some(true));
        let messages = &prepared.request.transcript.messages;
        assert!(messages.starts_with(&source.source.messages));
        assert!(matches!(
            messages.last(),
            Some(AgentMessage::User { content, .. }) if content == UPDATE_SUMMARIZATION_INSTRUCTIONS
        ));
        assert_eq!(prepared.request.tools().len(), 1);
    }

    #[test]
    fn tool_free_providers_withdraw_tools_before_the_summary_instruction() {
        let prepared = prepare_summary_request(
            model(),
            Vec::new(),
            Some(&source()),
            true,
            None,
            ThinkingLevel::Off,
        )
        .expect("summary request prepares");
        assert!(prepared.request.tools().is_empty());
        assert_eq!(prepared.request.system_prompt(), "work");
    }

    #[test]
    fn standalone_summary_uses_its_own_configuration() {
        let conversation = vec![
            AgentMessage::System {
                id: MessageId(1),
                update: ConfigurationUpdate {
                    sections: vec![SectionChange {
                        id: "base".into(),
                        content: Some("conversation prompt".into()),
                    }],
                    ..ConfigurationUpdate::default()
                },
            },
            tea_core::transcript::user_message("hello"),
        ];
        let prepared = prepare_summary_request(
            model(),
            conversation,
            None,
            false,
            None,
            ThinkingLevel::Off,
        )
        .expect("summary request prepares");
        assert_eq!(prepared.layout, CompactionRequestLayout::StandaloneFallback);
        assert_eq!(prepared.request.system_prompt(), SUMMARY_SYSTEM_PROMPT);
        assert!(prepared.request.tools().is_empty());
    }
}
