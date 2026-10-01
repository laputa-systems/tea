//! Typed Tea-transcript conversion for Codex Responses `input` items.

use crate::json::JsonValue;
use crate::scheduler::ModelRequest;
use crate::state::AgentMessage;
use crate::transcript::ConfigurationProjection;

/// Convert a typed request transcript to Codex Responses input items.
///
/// System instructions deliberately remain outside the input: the provider
/// sends the current Tea system prompt in top-level `instructions`, so a
/// configuration change is collapsed into that field. Codex-scoped encrypted
/// reasoning is replayed beside the assistant turn that produced it; visible
/// reasoning summaries are presentation only and are never replayed as text.
pub fn responses_input(request: &ModelRequest) -> Result<Vec<JsonValue>, String> {
    let transcript = request.transcript.prepared_for(request.model.as_ref());
    let resolved = transcript.resolve(ConfigurationProjection::Collapsed);
    let mut input = Vec::new();
    for message in &resolved.messages {
        append_message(&mut input, message);
    }
    // Host additions are explicitly developer context, never a user-authored
    // message. This mirrors the OpenAI projection's semantic boundary.
    for note in &resolved.host_notes {
        input.push(message_item("developer", note));
    }
    Ok(input)
}

fn append_message(input: &mut Vec<JsonValue>, message: &AgentMessage) {
    match message {
        AgentMessage::User { content, .. } => input.push(message_item("user", content)),
        AgentMessage::Assistant {
            content,
            tool_calls,
            opaque_context,
            ..
        } => {
            for item in opaque_context {
                if item.provider() != super::wire::PROVIDER_ID || item.kind() != "reasoning" {
                    continue;
                }
                if let Some(reasoning) = replay_reasoning_item(item) {
                    input.push(reasoning);
                }
            }
            let content = crate::state::assistant_text(content);
            if !content.is_empty() || tool_calls.is_empty() {
                input.push(message_item("assistant", &content));
            }
            for call in tool_calls {
                input.push(JsonValue::object([
                    ("type", JsonValue::String("function_call".into())),
                    ("call_id", JsonValue::String(call.id.as_str().to_owned())),
                    ("name", JsonValue::String(call.name.clone())),
                    (
                        "arguments",
                        JsonValue::String(call.arguments.as_str().to_owned()),
                    ),
                ]));
            }
        }
        AgentMessage::ToolResult {
            tool_call_id,
            content,
            ..
        } => input.push(JsonValue::object([
            ("type", JsonValue::String("function_call_output".into())),
            (
                "call_id",
                JsonValue::String(tool_call_id.as_str().to_owned()),
            ),
            ("output", JsonValue::String(content.clone())),
        ])),
        AgentMessage::System { .. } => {}
    }
}

/// Reconstruct the minimal upstream reasoning-item shape from Tea's opaque
/// continuation record. New records preserve the server's summary field so
/// the input is structurally accepted by Responses; legacy records that held
/// only encrypted content remain replayable with an empty summary.
fn replay_reasoning_item(item: &crate::state::OpaqueProviderContextItem) -> Option<JsonValue> {
    let parsed = JsonValue::parse(item.payload()).ok();
    let (encrypted_content, summary, parsed_id) =
        match parsed.as_ref().and_then(JsonValue::as_object) {
            Some(object) => {
                if object.get("type").and_then(JsonValue::as_str) != Some("reasoning") {
                    return None;
                }
                let encrypted_content = object
                    .get("encrypted_content")
                    .and_then(JsonValue::as_str)
                    .filter(|value| !value.is_empty())?
                    .to_owned();
                let summary = match object.get("summary") {
                    Some(summary) if summary.as_array().is_some() => summary.clone(),
                    Some(_) => return None,
                    None => JsonValue::Array(Vec::new()),
                };
                let parsed_id = object
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                    .map(str::to_owned);
                (encrypted_content, summary, parsed_id)
            }
            None => (
                item.payload().to_owned(),
                JsonValue::Array(Vec::new()),
                None,
            ),
        };
    let mut fields = vec![
        ("type", JsonValue::String("reasoning".into())),
        ("summary", summary),
        ("encrypted_content", JsonValue::String(encrypted_content)),
    ];
    if let Some(id) = item.item_id().map(str::to_owned).or(parsed_id) {
        fields.push(("id", JsonValue::String(id)));
    }
    Some(JsonValue::object(fields))
}

fn message_item(role: &str, text: &str) -> JsonValue {
    let content_type = match role {
        "assistant" => "output_text",
        _ => "input_text",
    };
    JsonValue::object([
        ("type", JsonValue::String("message".into())),
        ("role", JsonValue::String(role.into())),
        (
            "content",
            JsonValue::Array(vec![JsonValue::object([
                ("type", JsonValue::String(content_type.into())),
                ("text", JsonValue::String(text.into())),
            ])]),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AgentToolCall, MessageId, OpaqueProviderContextItem, SerializedJson, ToolCallId,
    };

    #[test]
    fn keeps_encrypted_reasoning_private_but_replayable() {
        let request = ModelRequest {
            transcript: crate::transcript::Transcript::new(vec![AgentMessage::Assistant {
                id: MessageId(1),
                content: crate::state::text_content("visible"),
                tool_calls: vec![AgentToolCall {
                    id: ToolCallId::new("call_1").unwrap(),
                    name: "read".into(),
                    arguments: SerializedJson::new(r#"{"path":"README.md"}"#),
                }],
                stop_reason: None,
                error_message: None,
                opaque_context: vec![OpaqueProviderContextItem::new(
                    "codex",
                    "reasoning",
                    Some("rs_1".into()),
                    r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"provider-private summary"}],"encrypted_content":"opaque-encrypted-state"}"#,
                )
                .unwrap()],
                origin: None,
            }]),
            ..ModelRequest::default()
        };

        let items = responses_input(&request).unwrap();
        assert_eq!(
            items[0].get("type").and_then(JsonValue::as_str),
            Some("reasoning")
        );
        assert_eq!(
            items[0]
                .get("encrypted_content")
                .and_then(JsonValue::as_str),
            Some("opaque-encrypted-state")
        );
        assert_eq!(
            items[0]
                .get("summary")
                .and_then(JsonValue::as_array)
                .and_then(|summary| summary.first())
                .and_then(|entry| entry.get("text"))
                .and_then(JsonValue::as_str),
            Some("provider-private summary")
        );
        assert_eq!(
            items[1].get("role").and_then(JsonValue::as_str),
            Some("assistant")
        );
        assert_eq!(
            items[2].get("type").and_then(JsonValue::as_str),
            Some("function_call")
        );
    }
}
