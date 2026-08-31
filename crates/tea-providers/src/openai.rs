//! OpenAI-compatible context conversion for the concrete HTTP adapters.

use crate::error::HookError;
use crate::hooks::{AfterToolCall, AgentLoopTurnUpdate, BeforeToolCall, ContextEnvelope, HookSet};
use crate::json::JsonValue;
use crate::state::AgentMessage;

// `OpenAiContextHook` is compiled for multiple OpenAI-compatible providers,
// including builds that omit OpenRouter. Keep its private continuation labels
// here rather than coupling the generic converter to an optional module.
const OPENROUTER_CONTEXT_PROVIDER: &str = "openrouter";
const OPENROUTER_REASONING_DETAILS_CONTEXT_KIND: &str = "reasoning_details";

/// Convert the core transcript into an OpenAI Chat Completions message array.
///
/// OpenRouter and other OpenAI-compatible adapters consume this host-produced context shape. The
/// hook remains
/// explicit because the core's default [`crate::hooks::NoHooks`] representation is diagnostic
/// Rust text, not a provider wire format.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiContextHook;

impl HookSet for OpenAiContextHook {
    fn before_tool_call(&self, _call: &crate::tool::ToolCall) -> Result<BeforeToolCall, HookError> {
        Ok(BeforeToolCall::Allow)
    }

    fn after_tool_call(
        &self,
        _call: &crate::tool::ToolCall,
        _result: &crate::tool::AgentToolResult,
    ) -> Result<AfterToolCall, HookError> {
        Ok(AfterToolCall::default())
    }

    fn transform_context(&self, context: ContextEnvelope) -> Result<ContextEnvelope, HookError> {
        Ok(context)
    }

    fn convert_to_llm(&self, context: ContextEnvelope) -> Result<String, HookError> {
        let mut messages = context
            .messages
            .iter()
            // OpenAI-compatible providers reject an assistant history entry
            // with neither visible content nor a tool call. Reasoning details
            // alone are private continuation state, not visible content.
            .filter(|message| openai_message_has_visible_content_or_tool_call(message))
            .map(openai_message)
            .collect::<Result<Vec<_>, _>>()?;
        // Host messages are deliberately not `AgentMessage::User` values.
        // A durable extension continuation uses this explicit provider context
        // so the model can distinguish internal steering from user-authored
        // conversation history.
        messages.extend(context.host_messages.into_iter().map(|message| {
            JsonValue::object([
                ("role", JsonValue::from("developer")),
                ("content", JsonValue::from(message.as_str().to_owned())),
            ])
        }));
        JsonValue::Array(messages)
            .to_json_string()
            .map_err(|error| HookError::new("convert_to_llm", error.to_string()))
    }

    fn should_stop_after_turn(&self, _context: &ContextEnvelope) -> Result<bool, HookError> {
        Ok(false)
    }

    fn prepare_next_turn(
        &self,
        _context: ContextEnvelope,
    ) -> Result<AgentLoopTurnUpdate, HookError> {
        Ok(AgentLoopTurnUpdate::default())
    }
}

fn openai_message_has_visible_content_or_tool_call(message: &AgentMessage) -> bool {
    match message {
        AgentMessage::Assistant {
            content, tool_calls, ..
        } => !content.trim().is_empty() || !tool_calls.is_empty(),
        _ => true,
    }
}

fn openai_message(message: &AgentMessage) -> Result<JsonValue, HookError> {
    match message {
        AgentMessage::User { content, .. } => Ok(JsonValue::object([
            ("role", JsonValue::from("user")),
            ("content", JsonValue::from(content.clone())),
        ])),
        AgentMessage::Assistant {
            content,
            tool_calls,
            opaque_context,
            ..
        } => {
            let calls = tool_calls
                .iter()
                .map(|call| {
                    JsonValue::object([
                        ("id", JsonValue::from(call.id.as_str())),
                        ("type", JsonValue::from("function")),
                        (
                            "function",
                            JsonValue::object([
                                ("name", JsonValue::from(call.name.clone())),
                                ("arguments", JsonValue::from(call.arguments.as_str())),
                            ]),
                        ),
                    ])
                })
                .collect::<Vec<_>>();
            let mut fields = vec![
                ("role", JsonValue::from("assistant")),
                (
                    "content",
                    if content.is_empty() {
                        JsonValue::Null
                    } else {
                        JsonValue::from(content.clone())
                    },
                ),
            ];
            // Pi only emits tool_calls when the assistant actually called a tool.
            // Omitting an empty array keeps the OpenAI-compatible wire shape and
            // avoids spending context tokens on a field with no semantic value.
            if !calls.is_empty() {
                fields.push(("tool_calls", JsonValue::Array(calls)));
            }
            if let Some(details) = openrouter_reasoning_details(opaque_context) {
                fields.push(("reasoning_details", details));
            }
            Ok(JsonValue::object(fields))
        }
        AgentMessage::ToolResult {
            tool_call_id,
            content,
            details,
            is_error,
            ..
        } => {
            let mut model_content = content.clone();
            if let Some(details) = details {
                model_content.push_str("\n[tool details (serialized JSON): ");
                model_content.push_str(&crate::tool::truncate_middle(
                    details.as_str(),
                    crate::tool::ToolResultProjectionPolicy::default().max_details_bytes,
                ));
                model_content.push(']');
            }
            let mut fields = vec![
                ("role", JsonValue::from("tool")),
                ("tool_call_id", JsonValue::from(tool_call_id.as_str())),
                ("content", JsonValue::from(model_content)),
            ];
            // OpenAI-compatible providers do not need a success marker, and
            // Pi omits it from successful tool results. Keep the explicit
            // error bit only when it carries meaning not already implied by
            // the role/content shape.
            if *is_error {
                fields.push(("is_error", JsonValue::Bool(true)));
            }
            Ok(JsonValue::object(fields))
        }
    }
}

/// Recover the OpenRouter continuation data that was captured by the adapter
/// beside an assistant message. It stays invisible to transcript renderers and
/// tools, but OpenRouter requires it to continue a reasoning/tool sequence.
fn openrouter_reasoning_details(
    opaque_context: &[crate::state::OpaqueProviderContextItem],
) -> Option<JsonValue> {
    // Match Pi's continuation recovery: each persisted signature is an
    // all-or-nothing JSON array, and the first valid one wins. This keeps a
    // stale or corrupt provider-private record from changing the next wire
    // request or turning a recoverable transcript into a hook error.
    opaque_context.iter().find_map(|item| {
        if item.provider() != OPENROUTER_CONTEXT_PROVIDER
            || item.kind() != OPENROUTER_REASONING_DETAILS_CONTEXT_KIND
        {
            return None;
        }
        let parsed = JsonValue::parse(item.payload()).ok()?;
        let entries = parsed.as_array()?;
        (!entries.is_empty() && entries.iter().all(valid_openai_reasoning_detail))
            .then(|| JsonValue::Array(entries.to_vec()))
    })
}

fn valid_openai_reasoning_detail(detail: &JsonValue) -> bool {
    let Some(object) = detail.as_object() else {
        return false;
    };
    if !optional_nullable_string(object, "id")
        || !optional_string(object, "format")
        || !optional_number(object, "index")
    {
        return false;
    }
    match object.get("type").and_then(JsonValue::as_str) {
        Some("reasoning.summary") => object.get("summary").and_then(JsonValue::as_str).is_some(),
        Some("reasoning.encrypted") => object.get("data").and_then(JsonValue::as_str).is_some(),
        Some("reasoning.text") => {
            object.get("text").and_then(JsonValue::as_str).is_some()
                && optional_nullable_string(object, "signature")
        }
        _ => false,
    }
}

fn optional_nullable_string(
    object: &std::collections::BTreeMap<String, JsonValue>,
    name: &str,
) -> bool {
    match object.get(name) {
        None | Some(JsonValue::Null) => true,
        Some(value) => value.as_str().is_some(),
    }
}

fn optional_string(object: &std::collections::BTreeMap<String, JsonValue>, name: &str) -> bool {
    object.get(name).is_none_or(|value| value.as_str().is_some())
}

fn optional_number(object: &std::collections::BTreeMap<String, JsonValue>, name: &str) -> bool {
    object.get(name).is_none_or(|value| value.as_f64().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AgentToolCall, MessageId, OpaqueProviderContextItem, SerializedJson, ToolCallId,
    };
    use crate::tool::{FailureSignature, ToolFailure};

    #[test]
    fn tool_projection_keeps_error_state_and_marks_unsupported_details() {
        let message = AgentMessage::ToolResult {
            id: MessageId(1),
            tool_call_id: ToolCallId::new("call-1").expect("fixture call ID"),
            tool_name: "fixture".into(),
            content: "error output".into(),
            details: Some(SerializedJson::new(r#"{"detail":"raw"}"#)),
            usage: Box::new(None),
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: true,
            failure: Some(ToolFailure::fatal(
                FailureSignature::new("fixture:dead").expect("signature"),
            )),
        };
        let projected = openai_message(&message).expect("projection");
        assert_eq!(
            projected.get("is_error").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert!(
            projected
                .get("content")
                .and_then(JsonValue::as_str)
                .is_some_and(|content| content.contains("[tool details (serialized JSON):"))
        );
    }

    #[test]
    fn assistant_projection_omits_empty_tool_calls_like_pi() {
        let message = AgentMessage::Assistant {
            id: MessageId(1),
            content: "finished".into(),
            tool_calls: Vec::new(),
            stop_reason: Some(crate::state::StopReason::Stop),
            error_message: None,
            opaque_context: Vec::new(),
        };
        let projected = openai_message(&message).expect("projection");
        assert!(projected.get("tool_calls").is_none());
    }

    #[test]
    fn assistant_projection_replays_openrouter_reasoning_details() {
        let details = r#"[{"type":"reasoning.text","text":"inspect the router","format":"unknown","index":0}]"#;
        let message = AgentMessage::Assistant {
            id: MessageId(1),
            content: String::new(),
            tool_calls: vec![AgentToolCall {
                id: ToolCallId::new("call-1").expect("fixture call ID"),
                name: "read".into(),
                arguments: SerializedJson::new(r#"{"path":"lib/router/index.js"}"#),
            }],
            stop_reason: Some(crate::state::StopReason::ToolUse),
            error_message: None,
            opaque_context: vec![
                OpaqueProviderContextItem::new(
                    "openrouter",
                    "reasoning_details",
                    None,
                    details,
                )
                .expect("bounded OpenRouter reasoning details"),
            ],
        };

        let projected = openai_message(&message).expect("projection");
        assert_eq!(
            projected.get("reasoning_details"),
            Some(&JsonValue::parse(details).expect("details JSON")),
        );
    }

    #[test]
    fn assistant_projection_uses_the_first_valid_openrouter_reasoning_record() {
        let invalid = r#"[{"type":"reasoning.text","text":"wrong","format":null,"index":0}]"#;
        let valid = r#"[{"type":"reasoning.text","text":"right","format":"unknown","index":0}]"#;
        let message = AgentMessage::Assistant {
            id: MessageId(1),
            content: String::new(),
            tool_calls: vec![AgentToolCall {
                id: ToolCallId::new("call-1").expect("fixture call ID"),
                name: "read".into(),
                arguments: SerializedJson::new(r#"{"path":"lib/router/index.js"}"#),
            }],
            stop_reason: Some(crate::state::StopReason::ToolUse),
            error_message: None,
            opaque_context: vec![
                OpaqueProviderContextItem::new(
                    "openrouter",
                    "reasoning_details",
                    None,
                    invalid,
                )
                .expect("bounded invalid fixture"),
                OpaqueProviderContextItem::new(
                    "openrouter",
                    "reasoning_details",
                    None,
                    valid,
                )
                .expect("bounded valid fixture"),
            ],
        };

        let projected = openai_message(&message).expect("projection");
        assert_eq!(
            projected.get("reasoning_details"),
            Some(&JsonValue::parse(valid).expect("valid details JSON")),
        );
    }

    #[test]
    fn context_omits_empty_assistant_messages_even_with_private_reasoning() {
        let details = r#"[{"type":"reasoning.text","text":"aborted","format":"unknown","index":0}]"#;
        let context = ContextEnvelope {
            version: 1,
            messages: vec![AgentMessage::Assistant {
                id: MessageId(1),
                content: String::new(),
                tool_calls: Vec::new(),
                stop_reason: Some(crate::state::StopReason::Stop),
                error_message: None,
                opaque_context: vec![
                    OpaqueProviderContextItem::new(
                        "openrouter",
                        "reasoning_details",
                        None,
                        details,
                    )
                    .expect("bounded OpenRouter reasoning details"),
                ],
            }],
            host_messages: Vec::new(),
        };

        let encoded = OpenAiContextHook
            .convert_to_llm(context)
            .expect("context conversion");
        assert_eq!(
            JsonValue::parse(&encoded).expect("encoded context"),
            JsonValue::Array(Vec::new()),
        );
    }

    #[test]
    fn successful_tool_projection_omits_redundant_error_flag() {
        let message = AgentMessage::ToolResult {
            id: MessageId(1),
            tool_call_id: ToolCallId::new("call-1").expect("fixture call ID"),
            tool_name: "fixture".into(),
            content: "ok".into(),
            details: None,
            usage: Box::new(None),
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: false,
            failure: None,
        };
        let projected = openai_message(&message).expect("projection");
        assert!(projected.get("is_error").is_none());
    }

    #[test]
    fn host_only_context_uses_a_developer_message_not_a_user_message() {
        let context = ContextEnvelope {
            version: 1,
            messages: Vec::new(),
            host_messages: vec![SerializedJson::new("continue the extension")],
        };
        let encoded = OpenAiContextHook
            .convert_to_llm(context)
            .expect("host-only context converts");
        let messages = JsonValue::parse(&encoded).expect("encoded context is JSON");
        let message = messages
            .as_array()
            .and_then(|messages| messages.first())
            .expect("one developer message");
        assert_eq!(
            message.get("role").and_then(JsonValue::as_str),
            Some("developer")
        );
        assert_eq!(
            message.get("content").and_then(JsonValue::as_str),
            Some("continue the extension"),
        );
    }
}
