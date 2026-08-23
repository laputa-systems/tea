//! OpenAI-compatible context conversion for the concrete HTTP adapters.

use crate::error::HookError;
use crate::hooks::{AfterToolCall, AgentLoopTurnUpdate, BeforeToolCall, ContextEnvelope, HookSet};
use crate::json::JsonValue;
use crate::state::AgentMessage;

/// Convert the core transcript into an OpenAI Chat Completions message array.
///
/// OpenRouter and Command Code both consume this host-produced context shape. The hook remains
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
        let messages = context
            .messages
            .iter()
            .map(openai_message)
            .collect::<Result<Vec<_>, _>>()?;
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

fn openai_message(message: &AgentMessage) -> Result<JsonValue, HookError> {
    match message {
        AgentMessage::User { content, .. } => Ok(JsonValue::object([
            ("role", JsonValue::from("user")),
            ("content", JsonValue::from(content.clone())),
        ])),
        AgentMessage::Assistant {
            content,
            tool_calls,
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
            Ok(JsonValue::object([
                ("role", JsonValue::from("assistant")),
                (
                    "content",
                    if content.is_empty() {
                        JsonValue::Null
                    } else {
                        JsonValue::from(content.clone())
                    },
                ),
                ("tool_calls", JsonValue::Array(calls)),
            ]))
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
            Ok(JsonValue::object([
                ("role", JsonValue::from("tool")),
                ("tool_call_id", JsonValue::from(tool_call_id.as_str())),
                ("content", JsonValue::from(model_content)),
                ("is_error", JsonValue::from(*is_error)),
            ]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MessageId, SerializedJson, ToolCallId};
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
}
