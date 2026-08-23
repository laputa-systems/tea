//! Command Code request/message encoding.

use crate::json::{JsonValue, json_value};
use crate::state::ThinkingLevel;
use std::collections::BTreeMap;
pub(super) fn reasoning_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        // Command Code's gateway rejects `off` and `minimal`; omitting the field is its
        // provider-native disabled/default form, while the smallest generic budget maps to low.
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("low"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::XHigh => Some("xhigh"),
        ThinkingLevel::Max => Some("max"),
    }
}
pub(super) fn commandcode_messages(context: &str) -> Result<Vec<JsonValue>, String> {
    let messages = JsonValue::parse(context)
        .map_err(|_| "Command Code received invalid converted context".to_owned())?;
    let messages = messages
        .as_array()
        .ok_or_else(|| "Command Code converted context must be an array".to_owned())?
        .to_owned();
    let mut tool_names = BTreeMap::<String, String>::new();
    messages
        .iter()
        .map(|message| commandcode_message(message, &mut tool_names))
        .collect()
}

fn commandcode_message(
    message: &JsonValue,
    tool_names: &mut BTreeMap<String, String>,
) -> Result<JsonValue, String> {
    let object = message
        .as_object()
        .ok_or_else(|| "Command Code context message must be an object".to_owned())?;
    let role = string_field(object, "role", "Command Code context message")?;
    match role {
        "user" => Ok(json_value!({
            "role": "user",
            "content": json_value!([json_value!({
                "type": "text",
                "text": string_or_null(object, "content")?,
            })]),
        })),
        "assistant" => {
            let mut content = Vec::new();
            if let Some(text) = optional_string_or_null(object, "content")?
                && !text.is_empty()
            {
                content.push(json_value!({"type": "text", "text": text}));
            }
            if let Some(calls) = object.get("tool_calls").and_then(JsonValue::as_array) {
                for call in calls {
                    let call = call.as_object().ok_or_else(|| {
                        "Command Code assistant tool call must be an object".to_owned()
                    })?;
                    let id = string_field(call, "id", "Command Code assistant tool call")?;
                    let function = call
                        .get("function")
                        .and_then(JsonValue::as_object)
                        .ok_or_else(|| {
                            "Command Code assistant tool call did not contain a function".to_owned()
                        })?;
                    let name = string_field(function, "name", "Command Code tool function")?;
                    let arguments =
                        string_field(function, "arguments", "Command Code tool function")?;
                    let input = JsonValue::parse(arguments).map_err(|_| {
                        "Command Code tool call arguments must be serialized JSON".to_owned()
                    })?;
                    if !input.is_object() {
                        return Err("Command Code tool call arguments must be a JSON object".into());
                    }
                    tool_names.insert(id.to_owned(), name.to_owned());
                    content.push(json_value!({
                        "type": "tool-call",
                        "toolCallId": id,
                        "toolName": name,
                        "input": input,
                    }));
                }
            }
            Ok(json_value!({"role": "assistant", "content": content}))
        }
        "tool" => {
            let id = string_field(object, "tool_call_id", "Command Code tool result")?;
            let name = tool_names.get(id).ok_or_else(|| {
                "Command Code tool result has no preceding assistant tool-call name".to_owned()
            })?;
            let mut content = string_or_null(object, "content")?;
            if let Some(details) = optional_string_or_null(object, "details")? {
                content.push_str("\n[tool details (serialized JSON): ");
                content.push_str(&crate::tool::truncate_middle(
                    details,
                    crate::tool::ToolResultProjectionPolicy::default().max_details_bytes,
                ));
                content.push(']');
            }
            Ok(json_value!({
                "role": "tool",
                "content": json_value!([json_value!({
                    "type": "tool-result",
                    "toolCallId": id,
                    "toolName": name,
                    "output": json_value!({
                        "type": "text",
                        "value": content,
                    }),
                    "isError": object
                        .get("is_error")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false),
                })]),
            }))
        }
        _ => Err("Command Code context role is unsupported".into()),
    }
}

pub(super) fn string_field<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    key: &str,
    subject: &str,
) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{subject} did not contain {key}"))
}

fn optional_string_or_null<'a>(
    object: &'a BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!(
            "Command Code context {key} must be a string or null"
        )),
    }
}

fn string_or_null(object: &BTreeMap<String, JsonValue>, key: &str) -> Result<String, String> {
    Ok(optional_string_or_null(object, key)?
        .unwrap_or_default()
        .to_owned())
}
