//! Anthropic Messages request construction.
//!
//! Port of upstream Pi's `buildParams`, `convertMessages`, `convertTools`,
//! `insertThinkingLevelMessages`, `getBetaFeatures`, `getCacheControl`, and the
//! `streamSimple` reasoning mapping (`packages/ai/src/api/anthropic-messages.ts`
//! and `simple-options.ts`). Tea derives everything from the typed transcript;
//! OAuth identity, image blocks, strict tool schemas, server-side fallbacks,
//! session-affinity headers, and tool-choice overrides are deliberately not
//! ported.

use super::PROVIDER_ID;
use super::catalog::AnthropicCompat;
use super::config::{AnthropicConfig, CacheRetention};
use crate::json::JsonValue;
use crate::scheduler::ModelRequest;
use crate::state::{AgentMessage, AssistantContent, ThinkingLevel};
use crate::tool::ToolDeclaration;
use crate::transcript::{ConfigurationProjection, Transcript};
use std::collections::BTreeMap;

pub(super) const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
pub(super) const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
pub(super) const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str =
    "mid-conversation-output-config-2026-07-01";
pub(super) const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
pub(super) const MID_CONVERSATION_TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";

/// Opaque item kind recording the effort a managed-effort response used.
pub(super) const EFFORT_CONTEXT_KIND: &str = "effort";
/// Opaque item kind of a thinking signature.
pub(super) const SIGNATURE_CONTEXT_KIND: &str = "thinking_signature";
/// Opaque item kind of a redacted thinking block.
pub(super) const REDACTED_THINKING_CONTEXT_KIND: &str = "redacted_thinking";

/// Stable deferred tool declared whenever native tool changes are in use, so
/// the hidden deferred-tool scaffolding is in the cached prefix from the first
/// request. It is never activated.
const DEFERRED_TOOL_PLACEHOLDER_NAME: &str = "__tea_deferred_placeholder__";

const CONTEXT_SAFETY_TOKENS: u64 = 4_096;
const MIN_ANSWER_TOKENS: u32 = 1_024;

/// One built request.
#[derive(Clone, Debug)]
pub(super) struct BuiltRequest {
    pub(super) body: JsonValue,
    pub(super) betas: Vec<&'static str>,
    /// Effort recorded with the response of a managed-effort model.
    pub(super) managed_effort: Option<&'static str>,
}

/// Map a Tea level to a native adaptive effort (Pi's `mapThinkingLevelToEffort`).
pub(super) fn effort_for(level: ThinkingLevel, compat: AnthropicCompat) -> &'static str {
    match level {
        // Off is not offered by models that cannot disable thinking; the
        // least-thinking effort is the honest approximation.
        ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh if compat.native_xhigh => "xhigh",
        ThinkingLevel::Max if compat.native_max => "max",
        ThinkingLevel::XHigh | ThinkingLevel::Max => "high",
    }
}

/// Thinking budget for budget-based models (Pi's `DEFAULT_THINKING_BUDGETS`).
pub(super) fn thinking_budget(level: ThinkingLevel) -> u32 {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal => 1_024,
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 8_192,
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => 16_384,
    }
}

/// Whether a request can be replayed with a one-token output cap without
/// changing its cache key: budget-based thinking derives its budget from the
/// output cap (Pi's `isReplayable`).
pub(super) fn minimal_output_replay_is_safe(compat: AnthropicCompat) -> bool {
    !compat.reasoning || compat.force_adaptive_thinking
}

/// Normalize a tool-call identity to Anthropic's `^[a-zA-Z0-9_-]{1,64}$`.
pub(super) fn normalize_tool_call_id(id: &str) -> String {
    let normalized = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    if normalized.is_empty() {
        "_".into()
    } else {
        normalized
    }
}

fn cache_control(config: &AnthropicConfig) -> Option<JsonValue> {
    match config.cache_retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(JsonValue::object([(
            "type",
            JsonValue::from("ephemeral"),
        )])),
        CacheRetention::Long if config.compat.supports_long_cache_retention => {
            Some(JsonValue::object([
                ("type", JsonValue::from("ephemeral")),
                ("ttl", JsonValue::from("1h")),
            ]))
        }
        CacheRetention::Long => Some(JsonValue::object([(
            "type",
            JsonValue::from("ephemeral"),
        )])),
    }
}

/// Build one request body and its beta features.
pub(super) fn build_request(
    config: &AnthropicConfig,
    request: &ModelRequest,
) -> Result<BuiltRequest, String> {
    let compat = config.compat;
    let transcript = request.transcript.prepared_for(request.model.as_ref());
    let in_place = compat.mid_conversation_system_messages;
    let resolved = transcript.resolve(if in_place {
        ConfigurationProjection::InPlace
    } else {
        ConfigurationProjection::Collapsed
    });
    let initial_tools = resolved.leading.tools.clone();
    let native_tool_changes = in_place
        && compat.mid_conversation_tool_changes
        && !initial_tools.is_empty()
        && !transcript.has_tool_redefinitions();
    let cache_control = cache_control(config);
    let managed = compat.mid_conversation_effort;

    let converted = convert_messages(
        &resolved.messages,
        &resolved.host_notes,
        cache_control.as_ref(),
        compat,
        native_tool_changes,
        in_place,
    )?;

    let thinking_enabled = compat.reasoning && request.thinking_level != ThinkingLevel::Off;
    let active_effort = managed.then(|| effort_for(request.thinking_level, compat));
    let messages = match active_effort {
        Some(effort) => insert_effort_markers(converted, effort),
        None => converted.messages,
    };

    let mut max_tokens = request.max_output_tokens.unwrap_or(config.max_output_tokens);
    let mut budget = None;
    if compat.reasoning && !compat.force_adaptive_thinking && thinking_enabled {
        // Pi's adjustMaxTokensForThinking with an explicit per-request cap:
        // the answer keeps its cap and the budget is added beneath the model
        // ceiling.
        let wanted = thinking_budget(request.thinking_level);
        max_tokens = match request.max_output_tokens {
            Some(cap) => cap.saturating_add(wanted).min(config.max_output_tokens),
            None => config.max_output_tokens,
        };
        let mut thinking = wanted;
        if max_tokens <= thinking {
            thinking = thinking.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
        }
        budget = Some(thinking);
    }
    max_tokens = clamp_to_context(config, &request.transcript, max_tokens);
    if let Some(thinking) = budget.as_mut() {
        *thinking = (*thinking).min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
    }

    let mut body = BTreeMap::<String, JsonValue>::new();
    body.insert("model".into(), JsonValue::from(config.model.clone()));
    body.insert("messages".into(), JsonValue::Array(messages));
    body.insert("max_tokens".into(), JsonValue::from(u64::from(max_tokens)));
    body.insert("stream".into(), JsonValue::Bool(true));

    let system_text = resolved.leading.system_prompt();
    if !system_text.is_empty() {
        let mut block = vec![
            ("type", JsonValue::from("text")),
            ("text", JsonValue::from(system_text)),
        ];
        if let Some(cache_control) = &cache_control {
            block.push(("cache_control", cache_control.clone()));
        }
        body.insert(
            "system".into(),
            JsonValue::Array(vec![JsonValue::object(block)]),
        );
    }

    if let Some(temperature) = config.temperature
        && !thinking_enabled
        && !managed
        && compat.supports_temperature
    {
        body.insert(
            "temperature".into(),
            JsonValue::number(tea_protocol::JsonNumber::Float(temperature))
                .map_err(|error| error.to_string())?,
        );
    }

    let tool_cache_control = compat
        .cache_control_on_tools
        .then(|| cache_control.clone())
        .flatten();
    let current_tools = transcript.tools();
    if native_tool_changes {
        // Initial tools stay active with the cache breakpoint on the last one.
        // Every later declaration is deferred and surfaced by its in-place
        // `tool_addition`; removed tools stay declared and are withdrawn by
        // `tool_removal`, so the request-level list only grows.
        let initial_names = initial_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        let later = transcript
            .declared_tools()
            .into_iter()
            .filter(|tool| !initial_names.contains(&tool.name.as_str()))
            .collect::<Vec<_>>();
        let mut tools = convert_tools(&initial_tools, compat, tool_cache_control.as_ref());
        tools.push(JsonValue::object([
            ("name", JsonValue::from(DEFERRED_TOOL_PLACEHOLDER_NAME)),
            (
                "description",
                JsonValue::from("Reserved placeholder. Never available. Never call this."),
            ),
            (
                "input_schema",
                JsonValue::object([
                    ("type", JsonValue::from("object")),
                    ("properties", JsonValue::object::<_, String>([])),
                    ("required", JsonValue::Array(Vec::new())),
                ]),
            ),
            ("defer_loading", JsonValue::Bool(true)),
        ]));
        for mut tool in convert_tools(&later, compat, None) {
            if let Some(object) = tool.as_object_mut() {
                object.insert("defer_loading".into(), JsonValue::Bool(true));
            }
            tools.push(tool);
        }
        body.insert("tools".into(), JsonValue::Array(tools));
    } else if !current_tools.is_empty() {
        body.insert(
            "tools".into(),
            JsonValue::Array(convert_tools(
                &current_tools,
                compat,
                tool_cache_control.as_ref(),
            )),
        );
    }

    let display = config.thinking_display.as_wire();
    if managed {
        // Managed-effort models always use adaptive thinking, so a historical
        // prefix mismatch drops the block instead of failing the request.
        body.insert(
            "thinking".into(),
            JsonValue::object([
                ("type", JsonValue::from("adaptive")),
                ("display", JsonValue::from(display)),
                (
                    "block_binding",
                    JsonValue::object([(
                        "prefix_mismatch_behavior",
                        JsonValue::from("drop_block"),
                    )]),
                ),
            ]),
        );
        body.insert(
            "output_config".into(),
            JsonValue::object([("effort", JsonValue::from("high"))]),
        );
    } else if compat.reasoning {
        if thinking_enabled {
            if compat.force_adaptive_thinking {
                body.insert(
                    "thinking".into(),
                    JsonValue::object([
                        ("type", JsonValue::from("adaptive")),
                        ("display", JsonValue::from(display)),
                    ]),
                );
                body.insert(
                    "output_config".into(),
                    JsonValue::object([(
                        "effort",
                        JsonValue::from(effort_for(request.thinking_level, compat)),
                    )]),
                );
            } else {
                body.insert(
                    "thinking".into(),
                    JsonValue::object([
                        ("type", JsonValue::from("enabled")),
                        (
                            "budget_tokens",
                            JsonValue::from(u64::from(budget.unwrap_or(1_024))),
                        ),
                        ("display", JsonValue::from(display)),
                    ]),
                );
            }
        } else if compat.thinking_can_be_disabled {
            body.insert(
                "thinking".into(),
                JsonValue::object([("type", JsonValue::from("disabled"))]),
            );
        }
    }

    let mut betas = Vec::new();
    if !current_tools.is_empty() && !compat.eager_tool_input_streaming {
        betas.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if compat.reasoning && thinking_enabled && !compat.force_adaptive_thinking {
        betas.push(INTERLEAVED_THINKING_BETA);
    }
    if managed {
        betas.push(MID_CONVERSATION_OUTPUT_CONFIG_BETA);
        betas.push(THINKING_BINDING_CONTROLS_BETA);
    }
    if native_tool_changes {
        betas.push(MID_CONVERSATION_TOOL_CHANGES_BETA);
    }

    Ok(BuiltRequest {
        body: JsonValue::Object(body),
        betas,
        managed_effort: active_effort,
    })
}

/// Pi's `clampMaxTokensToContext`: keep room for the estimated input.
fn clamp_to_context(config: &AnthropicConfig, transcript: &Transcript, max_tokens: u32) -> u32 {
    let Some(window) = config.context_window else {
        return max_tokens.max(1);
    };
    let resolved = transcript.resolve(ConfigurationProjection::Collapsed);
    let tool_bytes = resolved
        .leading
        .tools
        .iter()
        .map(|tool| {
            tool.name.len()
                + tool.description.len()
                + tool.schema.to_json_string().map_or(0, |schema| schema.len())
        })
        .sum::<usize>();
    let characters = resolved.leading.system_prompt().chars().count()
        + transcript
            .layout_context(ConfigurationProjection::Collapsed)
            .chars()
            .count()
        + tool_bytes;
    let estimate = (characters as u64).div_ceil(4);
    let available = window
        .saturating_sub(estimate)
        .saturating_sub(CONTEXT_SAFETY_TOKENS)
        .max(1);
    u64::from(max_tokens).min(available).max(1) as u32
}

struct ConvertedMessages {
    messages: Vec<JsonValue>,
    /// Effort recorded for the assistant at a message index.
    assistant_efforts: BTreeMap<usize, String>,
}

fn convert_messages(
    messages: &[AgentMessage],
    host_notes: &[String],
    cache_control: Option<&JsonValue>,
    compat: AnthropicCompat,
    native_tool_changes: bool,
    in_place: bool,
) -> Result<ConvertedMessages, String> {
    let mut params = Vec::<JsonValue>::new();
    let mut assistant_efforts = BTreeMap::new();
    // Later system messages are held and emitted directly before the next
    // assistant message (or at the end). Anthropic requires tool results to
    // immediately follow their tool use, so an update placed before a user
    // message lands after it on the wire, as in Pi.
    let mut pending_system = Vec::<JsonValue>::new();
    let mut tool_ids = BTreeMap::<String, String>::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            AgentMessage::System { update, .. } => {
                let mut blocks = Vec::new();
                let text = update.render_update_text();
                if !text.is_empty() {
                    blocks.push(text_block(&text));
                }
                if native_tool_changes {
                    for name in &update.tools_removed {
                        blocks.push(tool_reference_block("tool_removal", name));
                    }
                    for tool in &update.tools_added {
                        blocks.push(tool_reference_block("tool_addition", &tool.name));
                    }
                }
                if !blocks.is_empty() {
                    pending_system.push(JsonValue::object([
                        ("role", JsonValue::from("system")),
                        ("content", JsonValue::Array(blocks)),
                    ]));
                }
            }
            AgentMessage::User { content, .. } => {
                if !content.trim().is_empty() {
                    params.push(JsonValue::object([
                        ("role", JsonValue::from("user")),
                        ("content", JsonValue::from(content.clone())),
                    ]));
                }
            }
            AgentMessage::Assistant {
                content,
                tool_calls,
                opaque_context,
                ..
            } => {
                params.append(&mut pending_system);
                let mut blocks = Vec::new();
                for block in content {
                    match block {
                        AssistantContent::Text { text } => {
                            if !text.trim().is_empty() {
                                blocks.push(text_block(text));
                            }
                        }
                        AssistantContent::Thinking { text, signature } => {
                            let signature = signature
                                .as_ref()
                                .filter(|item| {
                                    item.provider() == PROVIDER_ID
                                        && item.kind() == SIGNATURE_CONTEXT_KIND
                                        && !item.payload().trim().is_empty()
                                })
                                .map(|item| item.payload().to_owned());
                            match signature {
                                Some(signature) => blocks.push(JsonValue::object([
                                    ("type", JsonValue::from("thinking")),
                                    ("thinking", JsonValue::from(text.clone())),
                                    ("signature", JsonValue::from(signature)),
                                ])),
                                // Unsigned thinking (for example from an
                                // aborted stream) cannot be replayed as
                                // thinking. Pi converts it to answer text;
                                // Tea drops it rather than flatten reasoning
                                // into prose, unless the model accepts an
                                // empty signature.
                                None if compat.allow_empty_signature
                                    && !text.trim().is_empty() =>
                                {
                                    blocks.push(JsonValue::object([
                                        ("type", JsonValue::from("thinking")),
                                        ("thinking", JsonValue::from(text.clone())),
                                        ("signature", JsonValue::from("")),
                                    ]))
                                }
                                None => {}
                            }
                        }
                        AssistantContent::RedactedThinking { data } => {
                            if data.provider() == PROVIDER_ID
                                && data.kind() == REDACTED_THINKING_CONTEXT_KIND
                            {
                                blocks.push(JsonValue::object([
                                    ("type", JsonValue::from("redacted_thinking")),
                                    ("data", JsonValue::from(data.payload().to_owned())),
                                ]));
                            }
                        }
                    }
                }
                for call in tool_calls {
                    let id = normalize_tool_call_id(call.id.as_str());
                    tool_ids.insert(call.id.as_str().to_owned(), id.clone());
                    let input = JsonValue::parse(call.arguments.as_str())
                        .ok()
                        .filter(JsonValue::is_object)
                        .unwrap_or_else(|| JsonValue::object::<_, String>([]));
                    blocks.push(JsonValue::object([
                        ("type", JsonValue::from("tool_use")),
                        ("id", JsonValue::from(id)),
                        ("name", JsonValue::from(call.name.clone())),
                        ("input", input),
                    ]));
                }
                if !blocks.is_empty() {
                    if compat.mid_conversation_effort
                        && let Some(effort) = opaque_context.iter().find(|item| {
                            item.provider() == PROVIDER_ID && item.kind() == EFFORT_CONTEXT_KIND
                        })
                    {
                        assistant_efforts.insert(params.len(), effort.payload().to_owned());
                    }
                    params.push(JsonValue::object([
                        ("role", JsonValue::from("assistant")),
                        ("content", JsonValue::Array(blocks)),
                    ]));
                }
            }
            AgentMessage::ToolResult { .. } => {
                // Consecutive results form one user message.
                let mut results = Vec::new();
                while let Some(AgentMessage::ToolResult {
                    tool_call_id,
                    content,
                    details,
                    is_error,
                    ..
                }) = messages.get(index)
                {
                    let id = tool_ids
                        .get(tool_call_id.as_str())
                        .cloned()
                        .unwrap_or_else(|| normalize_tool_call_id(tool_call_id.as_str()));
                    let mut text = content.clone();
                    if let Some(details) = details {
                        text.push_str("\n[tool details (serialized JSON): ");
                        text.push_str(&crate::tool::truncate_middle(
                            details.as_str(),
                            crate::tool::ToolResultProjectionPolicy::default().max_details_bytes,
                        ));
                        text.push(']');
                    }
                    results.push(JsonValue::object([
                        ("type", JsonValue::from("tool_result")),
                        ("tool_use_id", JsonValue::from(id)),
                        ("content", JsonValue::from(text)),
                        ("is_error", JsonValue::Bool(*is_error)),
                    ]));
                    index += 1;
                }
                params.push(JsonValue::object([
                    ("role", JsonValue::from("user")),
                    ("content", JsonValue::Array(results)),
                ]));
                continue;
            }
        }
        index += 1;
    }
    params.append(&mut pending_system);

    // Host-only notes follow the conversation. A transport with in-place
    // system messages carries them as system content; otherwise they are the
    // final user message.
    for note in host_notes {
        params.push(JsonValue::object([
            (
                "role",
                JsonValue::from(if in_place { "system" } else { "user" }),
            ),
            ("content", JsonValue::Array(vec![text_block(note)])),
        ]));
    }

    // Cache the conversation through the last user or system message.
    if let Some(cache_control) = cache_control
        && let Some(last) = params.last_mut()
        && matches!(
            last.get("role").and_then(JsonValue::as_str),
            Some("user" | "system")
        )
        && let Some(object) = last.as_object_mut()
    {
        match object.get_mut("content") {
            Some(JsonValue::String(text)) => {
                let text = std::mem::take(text);
                object.insert(
                    "content".into(),
                    JsonValue::Array(vec![JsonValue::object([
                        ("type", JsonValue::from("text")),
                        ("text", JsonValue::from(text)),
                        ("cache_control", cache_control.clone()),
                    ])]),
                );
            }
            Some(JsonValue::Array(blocks)) => {
                if let Some(block) = blocks.last_mut()
                    && matches!(
                        block.get("type").and_then(JsonValue::as_str),
                        Some("text" | "tool_result" | "tool_addition" | "tool_removal")
                    )
                    && let Some(block) = block.as_object_mut()
                {
                    block.insert("cache_control".into(), cache_control.clone());
                }
            }
            _ => {}
        }
    }

    Ok(ConvertedMessages {
        messages: params,
        assistant_efforts,
    })
}

/// Pi's `insertThinkingLevelMessages`: a marker before each historical
/// assistant with its recorded effort, then the active effort last.
fn insert_effort_markers(converted: ConvertedMessages, active: &str) -> Vec<JsonValue> {
    let marker = |effort: &str| {
        JsonValue::object([
            ("role", JsonValue::from("system")),
            ("content", JsonValue::Array(Vec::new())),
            (
                "output_config",
                JsonValue::object([("effort", JsonValue::from(effort.to_owned()))]),
            ),
        ])
    };
    let mut messages = Vec::with_capacity(converted.messages.len() + 1);
    for (index, message) in converted.messages.into_iter().enumerate() {
        if let Some(effort) = converted.assistant_efforts.get(&index) {
            messages.push(marker(effort));
        }
        messages.push(message);
    }
    messages.push(marker(active));
    messages
}

fn text_block(text: &str) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::from("text")),
        ("text", JsonValue::from(text.to_owned())),
    ])
}

fn tool_reference_block(kind: &str, name: &str) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::from(kind.to_owned())),
        (
            "tool",
            JsonValue::object([
                ("type", JsonValue::from("tool_reference")),
                ("name", JsonValue::from(name.to_owned())),
            ]),
        ),
    ])
}

/// Pi's `convertTools` without strict-schema support: the input schema keeps
/// only `type`, `properties`, and `required`.
fn convert_tools(
    tools: &[ToolDeclaration],
    compat: AnthropicCompat,
    cache_control: Option<&JsonValue>,
) -> Vec<JsonValue> {
    let count = tools.len();
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let mut fields = vec![
                ("name", JsonValue::from(tool.name.clone())),
                ("description", JsonValue::from(tool.description.clone())),
            ];
            if compat.eager_tool_input_streaming {
                fields.push(("eager_input_streaming", JsonValue::Bool(true)));
            }
            fields.push((
                "input_schema",
                JsonValue::object([
                    ("type", JsonValue::from("object")),
                    (
                        "properties",
                        tool.schema
                            .get("properties")
                            .cloned()
                            .unwrap_or_else(|| JsonValue::object::<_, String>([])),
                    ),
                    (
                        "required",
                        tool.schema
                            .get("required")
                            .cloned()
                            .unwrap_or_else(|| JsonValue::Array(Vec::new())),
                    ),
                ]),
            ));
            if let Some(cache_control) = cache_control
                && index + 1 == count
            {
                fields.push(("cache_control", cache_control.clone()));
            }
            JsonValue::object(fields)
        })
        .collect()
}
