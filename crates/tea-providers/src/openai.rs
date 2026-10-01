//! OpenAI-compatible Chat Completions projection for the concrete HTTP adapters.
//!
//! Adapters derive their wire messages from the typed request transcript. Chat
//! Completions has no native mid-conversation configuration update in the
//! adapters Tea ships, so configuration is collapsed: the current prompt is the
//! leading system message and the current tools are the request tools. A
//! configuration change therefore rewrites the request prefix, which the core
//! prompt-layout ledger reports as a domain change.

use crate::json::JsonValue;
use crate::scheduler::ModelRequest;
use crate::state::AgentMessage;
use crate::transcript::ConfigurationProjection;

// The projection is compiled for multiple OpenAI-compatible providers,
// including builds that omit OpenRouter. Keep its private continuation labels
// here rather than coupling the generic converter to an optional module.
const OPENROUTER_CONTEXT_PROVIDER: &str = "openrouter";
const OPENROUTER_REASONING_DETAILS_CONTEXT_KIND: &str = "reasoning_details";

/// Project a typed request into a Chat Completions message array.
///
/// The leading system message carries the current prompt. Replay follows the
/// shared transcript rules ([`crate::transcript::Transcript::prepared_for`]):
/// incomplete assistant turns are dropped and provider-exposed thinking is not
/// replayed as text. Host notes become trailing developer messages so the model
/// can distinguish internal steering from user-authored input.
pub fn chat_messages(request: &ModelRequest) -> Result<Vec<JsonValue>, String> {
    let mut messages = Vec::new();
    let system_prompt = request.system_prompt();
    if !system_prompt.is_empty() {
        messages.push(JsonValue::object([
            ("role", JsonValue::from("system")),
            ("content", JsonValue::from(system_prompt)),
        ]));
    }
    messages.extend(chat_conversation(request)?);
    Ok(messages)
}

/// Project the conversation after the leading system message.
pub fn chat_conversation(request: &ModelRequest) -> Result<Vec<JsonValue>, String> {
    let transcript = request.transcript.prepared_for(request.model.as_ref());
    let resolved = transcript.resolve(ConfigurationProjection::Collapsed);
    let mut messages = Vec::with_capacity(resolved.messages.len());
    for message in resolved
        .messages
        .iter()
        // OpenAI-compatible providers reject an assistant history entry
        // with neither visible content nor a tool call. Reasoning details
        // alone are private continuation state, not visible content.
        .filter(|message| openai_message_has_visible_content_or_tool_call(message))
    {
        if let Some(message) = openai_message(message)? {
            messages.push(message);
        }
    }
    messages.extend(resolved.host_notes.iter().map(|note| {
        JsonValue::object([
            ("role", JsonValue::from("developer")),
            ("content", JsonValue::from(note.clone())),
        ])
    }));
    Ok(messages)
}

/// Project the current tool declarations as Chat Completions function tools.
pub fn chat_tools(request: &ModelRequest) -> Vec<JsonValue> {
    request
        .tools()
        .into_iter()
        .map(|tool| {
            JsonValue::object([
                ("type", JsonValue::from("function")),
                (
                    "function",
                    JsonValue::object([
                        ("name", JsonValue::from(tool.name)),
                        ("description", JsonValue::from(tool.description)),
                        ("parameters", tool.schema),
                    ]),
                ),
            ])
        })
        .collect()
}

fn openai_message_has_visible_content_or_tool_call(message: &AgentMessage) -> bool {
    match message {
        AgentMessage::Assistant {
            content,
            tool_calls,
            ..
        } => !crate::state::assistant_text(content).trim().is_empty() || !tool_calls.is_empty(),
        _ => true,
    }
}

fn openai_message(message: &AgentMessage) -> Result<Option<JsonValue>, String> {
    Ok(Some(match message {
        AgentMessage::User { content, .. } => JsonValue::object([
            ("role", JsonValue::from("user")),
            ("content", JsonValue::from(content.clone())),
        ]),
        AgentMessage::Assistant {
            content,
            tool_calls,
            opaque_context,
            ..
        } => {
            let content = crate::state::assistant_text(content);
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
            JsonValue::object(fields)
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
            JsonValue::object(fields)
        }
        // Configuration is collapsed into the leading message before projection.
        AgentMessage::System { .. } => return Ok(None),
    }))
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
    object
        .get(name)
        .is_none_or(|value| value.as_str().is_some())
}

fn optional_number(object: &std::collections::BTreeMap<String, JsonValue>, name: &str) -> bool {
    object
        .get(name)
        .is_none_or(|value| value.as_f64().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AgentToolCall, AssistantContent, MessageId, ModelDescriptor, OpaqueProviderContextItem,
        SerializedJson, ToolCallId,
    };
    use crate::tool::{FailureSignature, ToolDeclaration, ToolFailure};
    use crate::transcript::Transcript;

    fn assistant(
        content: Vec<AssistantContent>,
        tool_calls: Vec<AgentToolCall>,
        opaque_context: Vec<OpaqueProviderContextItem>,
    ) -> AgentMessage {
        AgentMessage::Assistant {
            id: MessageId(1),
            content,
            tool_calls,
            stop_reason: Some(crate::state::StopReason::Stop),
            error_message: None,
            opaque_context,
            origin: None,
        }
    }

    fn request(messages: Vec<AgentMessage>) -> ModelRequest {
        ModelRequest {
            transcript: Transcript::new(messages),
            ..ModelRequest::default()
        }
    }

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
        let projected = openai_message(&message)
            .expect("projection")
            .expect("message");
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
        let message = assistant(
            crate::state::text_content("finished"),
            Vec::new(),
            Vec::new(),
        );
        let projected = openai_message(&message)
            .expect("projection")
            .expect("message");
        assert!(projected.get("tool_calls").is_none());
    }

    fn read_call() -> AgentToolCall {
        AgentToolCall {
            id: ToolCallId::new("call-1").expect("fixture call ID"),
            name: "read".into(),
            arguments: SerializedJson::new(r#"{"path":"lib/router/index.js"}"#),
        }
    }

    #[test]
    fn assistant_projection_replays_openrouter_reasoning_details() {
        let details = r#"[{"type":"reasoning.text","text":"inspect the router","format":"unknown","index":0}]"#;
        let message = assistant(
            Vec::new(),
            vec![read_call()],
            vec![
                OpaqueProviderContextItem::new("openrouter", "reasoning_details", None, details)
                    .expect("bounded OpenRouter reasoning details"),
            ],
        );
        let projected = openai_message(&message)
            .expect("projection")
            .expect("message");
        assert_eq!(
            projected.get("reasoning_details"),
            Some(&JsonValue::parse(details).expect("details JSON")),
        );
    }

    #[test]
    fn assistant_projection_uses_the_first_valid_openrouter_reasoning_record() {
        let invalid = r#"[{"type":"reasoning.text","text":"wrong","format":null,"index":0}]"#;
        let valid = r#"[{"type":"reasoning.text","text":"right","format":"unknown","index":0}]"#;
        let message = assistant(
            Vec::new(),
            vec![read_call()],
            vec![
                OpaqueProviderContextItem::new("openrouter", "reasoning_details", None, invalid)
                    .expect("bounded invalid fixture"),
                OpaqueProviderContextItem::new("openrouter", "reasoning_details", None, valid)
                    .expect("bounded valid fixture"),
            ],
        );
        let projected = openai_message(&message)
            .expect("projection")
            .expect("message");
        assert_eq!(
            projected.get("reasoning_details"),
            Some(&JsonValue::parse(valid).expect("valid details JSON")),
        );
    }

    #[test]
    fn context_omits_empty_assistant_messages_even_with_private_reasoning() {
        let details =
            r#"[{"type":"reasoning.text","text":"aborted","format":"unknown","index":0}]"#;
        let messages = chat_messages(&request(vec![assistant(
            Vec::new(),
            Vec::new(),
            vec![
                OpaqueProviderContextItem::new("openrouter", "reasoning_details", None, details)
                    .expect("bounded OpenRouter reasoning details"),
            ],
        )]))
        .expect("context conversion");
        assert!(messages.is_empty());
    }

    #[test]
    fn thinking_is_never_projected_as_answer_text() {
        let origin = ModelDescriptor {
            provider: "openrouter".into(),
            model: "m".into(),
            revision: None,
        };
        let mut message = assistant(
            vec![
                AssistantContent::thinking("private plan"),
                AssistantContent::text("answer"),
            ],
            Vec::new(),
            Vec::new(),
        );
        if let AgentMessage::Assistant { origin: slot, .. } = &mut message {
            *slot = Some(origin.clone());
        }
        let mut request = request(vec![message]);
        request.model = Some(origin);
        let messages = chat_messages(&request).expect("projection");
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].get("content").and_then(JsonValue::as_str),
            Some("answer")
        );
        let encoded = JsonValue::Array(messages).to_json_string().expect("JSON");
        assert!(!encoded.contains("private plan"));
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
        let projected = openai_message(&message)
            .expect("projection")
            .expect("message");
        assert!(projected.get("is_error").is_none());
    }

    #[test]
    fn host_only_context_uses_a_developer_message_not_a_user_message() {
        let mut request = request(Vec::new());
        request.transcript.host_notes = vec!["continue the extension".into()];
        let messages = chat_messages(&request).expect("host-only context converts");
        let message = messages.first().expect("one developer message");
        assert_eq!(
            message.get("role").and_then(JsonValue::as_str),
            Some("developer")
        );
        assert_eq!(
            message.get("content").and_then(JsonValue::as_str),
            Some("continue the extension"),
        );
    }

    #[test]
    fn configuration_is_collapsed_into_the_leading_system_message_and_tools() {
        let tool = |name: &str| {
            ToolDeclaration::new(
                name,
                format!("{name} tool"),
                JsonValue::object([("type", JsonValue::from("object"))]),
            )
        };
        let request = ModelRequest {
            transcript: Transcript::new(vec![
                AgentMessage::System {
                    id: MessageId(1),
                    update: crate::state::ConfigurationUpdate {
                        sections: vec![crate::state::SectionChange {
                            id: "base".into(),
                            content: Some("base prompt".into()),
                        }],
                        tools_added: vec![tool("first")],
                        tools_removed: Vec::new(),
                    },
                },
                AgentMessage::User {
                    id: MessageId(2),
                    content: "hello".into(),
                },
                AgentMessage::System {
                    id: MessageId(3),
                    update: crate::state::ConfigurationUpdate {
                        sections: vec![crate::state::SectionChange {
                            id: "late".into(),
                            content: Some("late section".into()),
                        }],
                        tools_added: vec![tool("second")],
                        tools_removed: vec!["first".into()],
                    },
                },
            ]),
            ..ModelRequest::default()
        };
        let messages = chat_messages(&request).expect("projection");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.get("role").and_then(JsonValue::as_str))
                .collect::<Vec<_>>(),
            [Some("system"), Some("user")]
        );
        assert_eq!(
            messages[0].get("content").and_then(JsonValue::as_str),
            Some("base prompt\n\nlate section")
        );
        let tools = chat_tools(&request);
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0]
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(JsonValue::as_str),
            Some("second")
        );
    }
}
