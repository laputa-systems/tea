use std::collections::BTreeMap;
use tea_core::event::{AgentEvent, AgentEventKind};
use tea_core::hooks::ContextEnvelope;
use tea_core::scheduler::ModelRequest;
use tea_core::state::{AgentMessage, StopReason, ThinkingLevel};
use tea_protocol::JsonValue;

pub(super) fn normalize_request(request: &ModelRequest) -> JsonValue {
    JsonValue::object([
        ("context", JsonValue::from(request.context.clone())),
        (
            "model",
            request
                .model
                .as_ref()
                .map(|model| {
                    JsonValue::object([
                        ("provider", JsonValue::from(model.provider.clone())),
                        ("id", JsonValue::from(model.model.clone())),
                    ])
                })
                .unwrap_or(JsonValue::Null),
        ),
        (
            "thinking_level",
            JsonValue::from(thinking_level_name(request.thinking_level)),
        ),
    ])
}

/// Normalize the request at the shared logical boundary before a provider
/// adapter serializes it. The fixture contract captures this same semantic
/// surface, so request fingerprints retain ordering and schemas without
/// conflating a transport's wire format with core parity.
pub(super) fn normalize_quality_request(
    request: &ModelRequest,
    context: &ContextEnvelope,
) -> Result<JsonValue, String> {
    Ok(JsonValue::object([
        (
            "system_prompt",
            JsonValue::from(request.system_prompt.clone()),
        ),
        (
            "messages",
            JsonValue::Array(
                context
                    .messages
                    .iter()
                    .map(normalize_message)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        ),
        (
            "host_messages",
            JsonValue::Array(
                context
                    .host_messages
                    .iter()
                    .map(|message| JsonValue::from(message.as_str().to_owned()))
                    .collect(),
            ),
        ),
        (
            "tools",
            JsonValue::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        JsonValue::object([
                            ("name", JsonValue::from(tool.name.clone())),
                            ("description", JsonValue::from(tool.description.clone())),
                            ("parameters", tool.schema.clone()),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "model",
            request
                .model
                .as_ref()
                .map(|model| {
                    JsonValue::object([
                        ("provider", JsonValue::from(model.provider.clone())),
                        ("id", JsonValue::from(model.model.clone())),
                    ])
                })
                .unwrap_or(JsonValue::Null),
        ),
        (
            "thinking_level",
            JsonValue::from(thinking_level_name(request.thinking_level)),
        ),
    ]))
}

pub(super) fn normalize_event(
    sequence: usize,
    event: &AgentEvent,
    turn_offset: u64,
) -> Result<JsonValue, String> {
    let (kind, data) = match &event.kind {
        AgentEventKind::CompactionLifecycle { .. } => ("compaction_lifecycle", empty_object()),
        AgentEventKind::ProviderRequestObserved { .. } => {
            ("provider_request_observed", empty_object())
        }
        AgentEventKind::PromptLayoutObserved { .. } => {
            ("prompt_layout_observed", empty_object())
        }
        AgentEventKind::CompactionStart { .. } => ("compaction_start", empty_object()),
        AgentEventKind::CompactionResult { .. } => ("compaction_result", empty_object()),
        AgentEventKind::CompactionEnd { .. } => ("compaction_end", empty_object()),
        AgentEventKind::AutomaticCompactionStart { .. } => {
            ("automatic_compaction_start", empty_object())
        }
        AgentEventKind::AutomaticCompactionEnd { .. } => {
            ("automatic_compaction_end", empty_object())
        }
        AgentEventKind::ContextEstimate { .. } => ("context_estimate", empty_object()),
        AgentEventKind::ProviderRequestSkipped { .. } => {
            ("provider_request_skipped", empty_object())
        }
        AgentEventKind::ToolFailureObserved { .. } => ("tool_failure_observed", empty_object()),
        AgentEventKind::AgentStart => ("agent_start", empty_object()),
        AgentEventKind::AgentEnd { .. } => ("agent_end", empty_object()),
        AgentEventKind::TurnStart { turn_id } => (
            "turn_start",
            JsonValue::object([(
                "turn",
                JsonValue::from(turn_offset.saturating_add(turn_id.0.saturating_sub(1))),
            )]),
        ),
        AgentEventKind::TurnEnd { reason, .. } => (
            "turn_end",
            JsonValue::object([("stop_reason", JsonValue::from(stop_reason_name(*reason)))]),
        ),
        AgentEventKind::ModelTurnUsage { .. } => ("model_turn_usage", empty_object()),
        AgentEventKind::MessageStart { message } => (
            "message_start",
            JsonValue::object([("role", JsonValue::from(message_role_name(message)))]),
        ),
        AgentEventKind::MessageEnd { message } => (
            "message_end",
            JsonValue::object([("role", JsonValue::from(message_role_name(message)))]),
        ),
        AgentEventKind::MessageUpdate {
            message,
            text_delta,
        } => (
            "message_update",
            JsonValue::object([
                ("role", JsonValue::from(message_role_name(message))),
                (
                    "delta",
                    JsonValue::from(
                        text_delta
                            .as_deref()
                            .unwrap_or_else(|| message_text(message)),
                    ),
                ),
            ]),
        ),
        AgentEventKind::ToolExecutionStart {
            tool_call_id,
            tool_name,
            ..
        } => (
            "tool_execution_start",
            JsonValue::object([
                ("tool_call_id", JsonValue::from(tool_call_id.as_str())),
                ("tool_name", JsonValue::from(tool_name.clone())),
            ]),
        ),
        AgentEventKind::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
        } => (
            "tool_execution_end",
            JsonValue::object([
                ("tool_call_id", JsonValue::from(tool_call_id.as_str())),
                ("tool_name", JsonValue::from(tool_name.clone())),
                ("is_error", JsonValue::from(result.is_error)),
            ]),
        ),
        AgentEventKind::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            update,
        } => (
            "tool_execution_update",
            JsonValue::object([
                ("tool_call_id", JsonValue::from(tool_call_id.as_str())),
                ("tool_name", JsonValue::from(tool_name.clone())),
                ("content", JsonValue::from(update.content.clone())),
            ]),
        ),
    };
    Ok(JsonValue::object([
        ("seq", JsonValue::from(sequence as u64)),
        ("type", JsonValue::from(kind)),
        ("data", data),
    ]))
}

pub(super) fn normalize_message(message: &AgentMessage) -> Result<JsonValue, String> {
    let content = match message {
        AgentMessage::User { content, .. } | AgentMessage::ToolResult { content, .. } => {
            vec![text_content(content)]
        }
        AgentMessage::Assistant {
            content,
            tool_calls,
            ..
        } => {
            let mut parts = Vec::new();
            if !content.is_empty() {
                parts.push(text_content(content));
            }
            for tool_call in tool_calls {
                parts.push(JsonValue::object([
                    ("type", JsonValue::from("tool_call")),
                    ("id", JsonValue::from(tool_call.id.as_str())),
                    ("name", JsonValue::from(tool_call.name.clone())),
                    (
                        "arguments",
                        JsonValue::parse(tool_call.arguments.as_str())
                            .map_err(|error| error.to_string())?,
                    ),
                ]));
            }
            parts
        }
    };
    Ok(JsonValue::object([
        ("role", JsonValue::from(message_role_name(message))),
        ("content", JsonValue::Array(content)),
    ]))
}

fn text_content(text: &str) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::from("text")),
        ("text", JsonValue::from(text)),
    ])
}

fn message_role_name(message: &AgentMessage) -> &'static str {
    match message {
        AgentMessage::User { .. } => "user",
        AgentMessage::Assistant { .. } => "assistant",
        AgentMessage::ToolResult { .. } => "tool_result",
    }
}

fn message_text(message: &AgentMessage) -> &str {
    match message {
        AgentMessage::User { content, .. }
        | AgentMessage::Assistant { content, .. }
        | AgentMessage::ToolResult { content, .. } => content,
    }
}

pub(super) fn thinking_level_name(value: ThinkingLevel) -> &'static str {
    match value {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

pub(super) fn stop_reason_name(value: StopReason) -> &'static str {
    match value {
        StopReason::Stop => "stop",
        StopReason::ToolUse => "tool_call",
        StopReason::Length => "length",
        StopReason::Aborted => "aborted",
        StopReason::Cancelled => "cancelled",
        StopReason::Error => "error",
    }
}

fn empty_object() -> JsonValue {
    JsonValue::Object(BTreeMap::new())
}
