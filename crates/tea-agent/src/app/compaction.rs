//! Provider-backed compaction policy for the repository-owned terminal host.
//!
//! The core deliberately does not choose a summary prompt or provider. This
//! host supplies both explicitly by reusing the selected provider for a small,
//! tool-free summarization request and retaining the exact suffix selected by
//! the core's automatic-compaction split.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, RwLock};
use tea_core::compaction::{
    AutomaticCompactionRequest, CompactionContext, CompactionError, CompactionFuture,
    CompactionRequestLayout, CompactionResult, CompactionStrategy, Compactor, ProviderContext,
};
use tea_core::hooks::{ContextEnvelope, HookSet};
use tea_core::provider::openai::OpenAiContextHook;
use tea_core::scheduler::{CancellationToken, ModelProvider, ModelRequest, ModelStreamEvent};
use tea_core::state::{AgentMessage, MessageId, ModelDescriptor, StopReason, ThinkingLevel};
use tea_core::Usage;
use tea_protocol::JsonValue;

use super::workspace_ledger::WorkspaceLedger;

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
const STRUCTURED_CHECKPOINT_SYSTEM_PROMPT: &str = r#"You compact coding-agent conversation history into a durable checkpoint.
Return concise Markdown with exactly these sections, in this order:

## Goal
## Constraints and Preferences
## Current Checkpoint
## Decisions and Rationale
## Progress — Done
## Progress — In Progress
## Progress — Blocked
## Failed Attempts
## Verification
## Next Concrete Action
## Critical Context

State only facts supported by the supplied history. Keep exact paths, symbols, command/test
statuses, decisions, failures, and next action. Do not include a Workspace Ledger: the host adds
its independently observed ledger after your response. Do not call tools."#;
const INCREMENTAL_CHECKPOINT_SYSTEM_PROMPT: &str = r#"You update a tea coding-agent checkpoint.
Merge the previous checkpoint with only the newly discarded history. Latest supported facts win;
do not resurrect completed work, superseded constraints, or rejected alternatives. Preserve exact
paths, symbols, command/test status, failure reasons, and the next concrete action.

Return concise Markdown with exactly these sections, in this order:

## Goal
## Constraints and Preferences
## Current Checkpoint
## Decisions and Rationale
## Progress — Done
## Progress — In Progress
## Progress — Blocked
## Failed Attempts
## Verification
## Next Concrete Action
## Critical Context

Do not emit a checkpoint marker or Workspace Ledger; the host owns both. Do not call tools."#;
const CACHE_FRIENDLY_CONTEXT_SAFETY_MARGIN: u64 = 4_096;

/// Explicit host-owned provider compaction selection.
///
/// The baseline remains the default. Every other variant is an experiment that
/// must be selected by ID and is recorded in the core lifecycle descriptor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderCompactionStrategy {
    /// Preserve the original request layout, including prompt-facing tools.
    #[default]
    CacheReplaySummaryV0,
    /// Keep the exact source context but omit tools for provider compatibility.
    ToolFreeReplaySummaryV1,
    /// Build a marker-versioned standalone structured checkpoint.
    StructuredCheckpointV1,
    /// Merge a prior marker checkpoint with only newly discarded history.
    IncrementalCheckpointUpdateV1,
}

impl ProviderCompactionStrategy {
    /// Parse one explicit stable strategy identifier.
    pub fn from_id(id: &str) -> Result<Self, String> {
        match id {
            tea_core::CACHE_REPLAY_SUMMARY_V0 => Ok(Self::CacheReplaySummaryV0),
            tea_core::TOOL_FREE_REPLAY_SUMMARY_V1 => Ok(Self::ToolFreeReplaySummaryV1),
            tea_core::STRUCTURED_CHECKPOINT_V1 => Ok(Self::StructuredCheckpointV1),
            tea_core::INCREMENTAL_CHECKPOINT_UPDATE_V1 => Ok(Self::IncrementalCheckpointUpdateV1),
            _ => Err(format!(
                "unknown compaction strategy {id:?}; expected {}, {}, {}, or {}",
                tea_core::CACHE_REPLAY_SUMMARY_V0,
                tea_core::TOOL_FREE_REPLAY_SUMMARY_V1,
                tea_core::STRUCTURED_CHECKPOINT_V1,
                tea_core::INCREMENTAL_CHECKPOINT_UPDATE_V1,
            )),
        }
    }

    /// Stable strategy identifier suitable for a CLI flag or sanitized report.
    pub const fn id(self) -> &'static str {
        match self {
            Self::CacheReplaySummaryV0 => tea_core::CACHE_REPLAY_SUMMARY_V0,
            Self::ToolFreeReplaySummaryV1 => tea_core::TOOL_FREE_REPLAY_SUMMARY_V1,
            Self::StructuredCheckpointV1 => tea_core::STRUCTURED_CHECKPOINT_V1,
            Self::IncrementalCheckpointUpdateV1 => tea_core::INCREMENTAL_CHECKPOINT_UPDATE_V1,
        }
    }

    fn descriptor(self) -> CompactionStrategy {
        match self {
            Self::CacheReplaySummaryV0 => {
                CompactionStrategy::cache_replay_summary_v0(baseline_prompt_fingerprint())
            }
            Self::ToolFreeReplaySummaryV1 => {
                CompactionStrategy::tool_free_replay_summary_v1(baseline_prompt_fingerprint())
            }
            Self::StructuredCheckpointV1 => {
                CompactionStrategy::structured_checkpoint_v1(structured_prompt_fingerprint())
            }
            Self::IncrementalCheckpointUpdateV1 => {
                CompactionStrategy::incremental_checkpoint_update_v1(
                    incremental_prompt_fingerprint(),
                )
            }
        }
    }
}

/// A compactor whose provider/model pair follows the TUI's idle model selection.
pub(super) struct ProviderCompactor {
    provider: RwLock<Option<Arc<dyn ModelProvider>>>,
    model: RwLock<Option<ModelDescriptor>>,
    strategy: RwLock<ProviderCompactionStrategy>,
}

impl fmt::Debug for ProviderCompactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCompactor")
            .field(
                "model",
                &self.model.read().ok().and_then(|model| model.clone()),
            )
            .field(
                "strategy",
                &self.strategy.read().ok().map(|strategy| *strategy),
            )
            .finish_non_exhaustive()
    }
}

impl Default for ProviderCompactor {
    fn default() -> Self {
        Self {
            provider: RwLock::new(None),
            model: RwLock::new(None),
            strategy: RwLock::new(ProviderCompactionStrategy::default()),
        }
    }
}

impl ProviderCompactor {
    /// Construct a compactor with one explicit host strategy.
    pub(super) fn with_strategy(strategy: ProviderCompactionStrategy) -> Self {
        Self {
            strategy: RwLock::new(strategy),
            ..Self::default()
        }
    }

    /// Set the explicit provider/model used for future compaction requests.
    pub(super) fn configure(&self, model: ModelDescriptor, provider: Arc<dyn ModelProvider>) {
        *self
            .provider
            .write()
            .expect("TUI compactor provider lock poisoned") = Some(provider);
        *self
            .model
            .write()
            .expect("TUI compactor model lock poisoned") = Some(model);
    }

    fn selected_strategy(&self) -> ProviderCompactionStrategy {
        *self
            .strategy
            .read()
            .expect("TUI compactor strategy lock poisoned")
    }

    fn configured(
        &self,
        context: &CompactionContext,
    ) -> Result<(Arc<dyn ModelProvider>, ModelDescriptor), CompactionError> {
        let provider = self
            .provider
            .read()
            .expect("TUI compactor provider lock poisoned")
            .clone()
            .ok_or_else(|| CompactionError::failed("no selected provider for compaction"))?;
        let model = context
            .model
            .clone()
            .or_else(|| {
                self.model
                    .read()
                    .expect("TUI compactor model lock poisoned")
                    .clone()
            })
            .ok_or_else(|| CompactionError::failed("no selected model for compaction"))?;
        Ok((provider, model))
    }
}

impl Compactor for ProviderCompactor {
    fn strategy(&self) -> CompactionStrategy {
        self.selected_strategy().descriptor()
    }

    fn compact<'a>(
        &'a self,
        context: CompactionContext,
        cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        let configured = self.configured(&context);
        let strategy = self.selected_strategy();
        Box::pin(async move {
            let (provider, model) = configured?;
            if context.messages.is_empty() {
                return Ok(CompactionResult::new(Vec::new()));
            }
            let prepared =
                prepare_summary_request(strategy, model, context.messages.clone(), None)?;
            let layout = prepared.layout;
            let source_is_active_context_prefix = prepared.source_is_active_context_prefix;
            let envelope = prepared.envelope;
            let (summary, usage, request_observation) =
                summarize(provider, prepared.request, cancellation).await?;
            let replacement = vec![summary_message(&context.messages, summary, envelope)?];
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

    fn compact_automatic<'a>(
        &'a self,
        context: CompactionContext,
        request: AutomaticCompactionRequest,
        cancellation: CancellationToken,
    ) -> CompactionFuture<'a> {
        let configured = self.configured(&context);
        let strategy = self.selected_strategy();
        Box::pin(async move {
            let (provider, model) = configured?;
            let source_context = match strategy {
                ProviderCompactionStrategy::CacheReplaySummaryV0
                | ProviderCompactionStrategy::ToolFreeReplaySummaryV1 => context
                    .provider_context
                    .as_ref()
                    .filter(|source| source_context_fits(source, &request)),
                ProviderCompactionStrategy::StructuredCheckpointV1
                | ProviderCompactionStrategy::IncrementalCheckpointUpdateV1 => None,
            };
            let mut messages_to_summarize = request.prefix_messages;
            messages_to_summarize.extend(request.split_turn_prefix);
            if messages_to_summarize.is_empty() {
                return Ok(CompactionResult::new(request.retained_messages));
            }
            let prepared = prepare_summary_request(
                strategy,
                model,
                messages_to_summarize.clone(),
                source_context,
            )?;
            let layout = prepared.layout;
            let source_is_active_context_prefix = prepared.source_is_active_context_prefix;
            let envelope = prepared.envelope;
            let (summary, usage, request_observation) =
                summarize(provider, prepared.request, cancellation).await?;
            let retained_messages = request.retained_messages;
            let mut all_messages = messages_to_summarize;
            all_messages.extend(retained_messages.iter().cloned());
            let mut replacement = vec![summary_message(&all_messages, summary, envelope)?];
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
    envelope: CheckpointEnvelope,
}

enum CheckpointEnvelope {
    BaselineSummary,
    StructuredCheckpoint {
        generation: u32,
        ledger: WorkspaceLedger,
    },
}

fn summary_message(
    messages: &[AgentMessage],
    summary: String,
    envelope: CheckpointEnvelope,
) -> Result<AgentMessage, CompactionError> {
    let content = match envelope {
        CheckpointEnvelope::BaselineSummary => format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"),
        CheckpointEnvelope::StructuredCheckpoint { generation, ledger } => {
            let semantic = remove_model_ledger(summary);
            if semantic.trim().is_empty() {
                return Err(CompactionError::failed(
                    "structured checkpoint contained no model-authored semantic state",
                ));
            }
            format!(
                "<!-- tea-checkpoint:v1 generation={generation} -->\n{semantic}\n\n{}",
                ledger.render(),
            )
        }
    };
    Ok(AgentMessage::User {
        id: next_message_id(messages),
        content,
    })
}

fn next_message_id(messages: &[AgentMessage]) -> MessageId {
    let used = messages
        .iter()
        .map(|message| match message {
            AgentMessage::User { id, .. }
            | AgentMessage::Assistant { id, .. }
            | AgentMessage::ToolResult { id, .. } => id.0,
        })
        .collect::<BTreeSet<_>>();
    let mut candidate = 1_u64;
    while used.contains(&candidate) {
        candidate = candidate.saturating_add(1);
    }
    MessageId(candidate)
}

fn prepare_summary_request(
    strategy: ProviderCompactionStrategy,
    model: ModelDescriptor,
    messages: Vec<AgentMessage>,
    source_context: Option<&ProviderContext>,
) -> Result<PreparedSummaryRequest, CompactionError> {
    let (system_prompt, context, tools, layout, source_is_active_context_prefix, envelope) =
        match strategy {
            ProviderCompactionStrategy::CacheReplaySummaryV0
            | ProviderCompactionStrategy::ToolFreeReplaySummaryV1 => {
                if let Some(source) = source_context {
                    let tools = if strategy == ProviderCompactionStrategy::CacheReplaySummaryV0 {
                        // This is the frozen baseline surface. Tool execution is still prohibited by
                        // the compactor stream, but the prompt envelope remains byte-for-byte aligned
                        // with the ordinary request so the adapter can report the true domain match.
                        source.tools.clone()
                    } else {
                        // This is intentionally a separate compatibility candidate, used for models
                        // such as Laguna XS that may reject a tool-enabled summary request.
                        Vec::new()
                    };
                    (
                        source.system_prompt.clone(),
                        append_update_instruction(&source.context)?,
                        tools,
                        CompactionRequestLayout::ExactReplay,
                        Some(true),
                        CheckpointEnvelope::BaselineSummary,
                    )
                } else {
                    (
                        SUMMARY_SYSTEM_PROMPT.into(),
                        convert_messages(messages)?,
                        Vec::new(),
                        CompactionRequestLayout::StandaloneFallback,
                        None,
                        CheckpointEnvelope::BaselineSummary,
                    )
                }
            }
            ProviderCompactionStrategy::StructuredCheckpointV1 => {
                let ledger = WorkspaceLedger::from_messages(&messages);
                let generation = checkpoint_generation(&messages).saturating_add(1);
                (
                STRUCTURED_CHECKPOINT_SYSTEM_PROMPT.into(),
                append_instruction(
                    &convert_messages(messages)?,
                    &format!(
                        "Create the checkpoint now. The host-derived ledger below is authoritative; do not repeat it.\n\n{}",
                        ledger.render()
                    ),
                )?,
                Vec::new(),
                CompactionRequestLayout::StandaloneFallback,
                None,
                CheckpointEnvelope::StructuredCheckpoint { generation, ledger },
            )
            }
            ProviderCompactionStrategy::IncrementalCheckpointUpdateV1 => {
                if let Some((prior, prior_index, generation)) = latest_checkpoint(&messages) {
                    let delta = messages[prior_index.saturating_add(1)..].to_vec();
                    let ledger = WorkspaceLedger::from_messages(&delta);
                    let update = format!(
                    "Previous tea checkpoint:\n<checkpoint>\n{prior}\n</checkpoint>\n\nNewly discarded provider history:\n<history-json>\n{}\n</history-json>\n\nThe host-derived ledger delta is authoritative; do not repeat it.\n\n{}",
                    convert_messages(delta)?,
                    ledger.render(),
                );
                    (
                        INCREMENTAL_CHECKPOINT_SYSTEM_PROMPT.into(),
                        convert_messages(vec![AgentMessage::User {
                            id: next_message_id(&messages),
                            content: update,
                        }])?,
                        Vec::new(),
                        CompactionRequestLayout::IncrementalCheckpointUpdate,
                        None,
                        CheckpointEnvelope::StructuredCheckpoint {
                            generation: generation.saturating_add(1),
                            ledger,
                        },
                    )
                } else {
                    // The first generation has no delta to merge. The candidate visibly uses the
                    // bounded standalone layout and writes a v1 marker for the next compaction.
                    let ledger = WorkspaceLedger::from_messages(&messages);
                    (
                    STRUCTURED_CHECKPOINT_SYSTEM_PROMPT.into(),
                    append_instruction(
                        &convert_messages(messages)?,
                        &format!(
                            "Create the first checkpoint now. The host-derived ledger below is authoritative; do not repeat it.\n\n{}",
                            ledger.render()
                        ),
                    )?,
                    Vec::new(),
                    CompactionRequestLayout::StandaloneFallback,
                    None,
                    CheckpointEnvelope::StructuredCheckpoint {
                        generation: 1,
                        ledger,
                    },
                )
                }
            }
        };
    Ok(PreparedSummaryRequest {
        request: ModelRequest {
            system_prompt,
            context,
            tools,
            model: Some(model),
            thinking_level: ThinkingLevel::Off,
        },
        layout,
        source_is_active_context_prefix,
        envelope,
    })
}

fn convert_messages(messages: Vec<AgentMessage>) -> Result<String, CompactionError> {
    OpenAiContextHook
        .convert_to_llm(ContextEnvelope {
            version: 1,
            messages,
            host_messages: Vec::new(),
        })
        .map_err(|error| CompactionError::failed(error.to_string()))
}

async fn summarize(
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
            ModelStreamEvent::Usage(reported) => usage = Some(reported),
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

fn structured_prompt_fingerprint() -> u64 {
    stable_fingerprint(STRUCTURED_CHECKPOINT_SYSTEM_PROMPT.as_bytes())
}

fn incremental_prompt_fingerprint() -> u64 {
    stable_fingerprint(INCREMENTAL_CHECKPOINT_SYSTEM_PROMPT.as_bytes())
}

fn checkpoint_generation(messages: &[AgentMessage]) -> u32 {
    latest_checkpoint(messages)
        .map(|(_, _, generation)| generation)
        .unwrap_or(0)
}

fn latest_checkpoint(messages: &[AgentMessage]) -> Option<(&str, usize, u32)> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| match message {
            AgentMessage::User { content, .. } => parse_checkpoint_marker(content)
                .map(|generation| (content.as_str(), index, generation)),
            AgentMessage::Assistant { .. } | AgentMessage::ToolResult { .. } => None,
        })
}

fn parse_checkpoint_marker(content: &str) -> Option<u32> {
    const PREFIX: &str = "<!-- tea-checkpoint:v1 generation=";
    let first = content.lines().next()?;
    first
        .strip_prefix(PREFIX)?
        .strip_suffix(" -->")?
        .parse()
        .ok()
}

fn remove_model_ledger(summary: String) -> String {
    let mut filtered = Vec::new();
    let mut omitting = false;
    for line in summary.lines() {
        if line == "## Workspace Ledger" {
            omitting = true;
            continue;
        }
        if omitting && line.starts_with("## ") {
            omitting = false;
        }
        if !omitting {
            filtered.push(line);
        }
    }
    filtered.join("\n").trim().to_owned()
}

fn source_context_fits(source: &ProviderContext, request: &AutomaticCompactionRequest) -> bool {
    if let Some(active_context) = &source.active_context {
        if !is_exact_message_prefix(&source.context, active_context) {
            return false;
        }
    }
    let tool_bytes = source
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
    let source_bytes = source
        .system_prompt
        .len()
        .saturating_add(source.context.len())
        .saturating_add(tool_bytes);
    let source_tokens = (source_bytes as u64).saturating_add(3) / 4;
    source_tokens
        .saturating_add(request.reserved_tokens)
        .saturating_add(CACHE_FRIENDLY_CONTEXT_SAFETY_MARGIN)
        <= request.context_budget_tokens
}

fn is_exact_message_prefix(source: &str, active: &str) -> bool {
    let Ok(JsonValue::Array(source_messages)) = JsonValue::parse(source) else {
        return false;
    };
    let Ok(JsonValue::Array(active_messages)) = JsonValue::parse(active) else {
        return false;
    };
    active_messages.starts_with(&source_messages)
}

fn append_update_instruction(context: &str) -> Result<String, CompactionError> {
    append_instruction(context, UPDATE_SUMMARIZATION_INSTRUCTIONS)
}

/// Append exactly one host instruction to an already converted provider context.
///
/// This is shared by candidate preparation so the observed request is the same
/// JSON value passed to the provider; no hook or provider projection is rerun
/// for measurement.
fn append_instruction(context: &str, instruction: &str) -> Result<String, CompactionError> {
    let mut value = JsonValue::parse(context).map_err(|error| {
        CompactionError::failed(format!("active provider context is not JSON: {error}"))
    })?;
    let JsonValue::Array(messages) = &mut value else {
        return Err(CompactionError::failed(
            "active provider context is not a message array",
        ));
    };
    messages.push(JsonValue::object([
        ("role", JsonValue::from("user")),
        ("content", JsonValue::from(instruction)),
    ]));
    value
        .to_json_string()
        .map_err(|error| CompactionError::failed(error.to_string()))
}
