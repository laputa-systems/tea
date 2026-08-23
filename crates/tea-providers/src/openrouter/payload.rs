//! OpenRouter request payload encoding.

use super::config::OpenRouterConfig;
use crate::json::{JsonValue, json_value, to_bytes};
use crate::scheduler::ModelRequest;

pub(super) fn reasoning_effort(level: crate::state::ThinkingLevel) -> Option<&'static str> {
    match level {
        crate::state::ThinkingLevel::Off => Some("none"),
        crate::state::ThinkingLevel::Minimal => Some("minimal"),
        crate::state::ThinkingLevel::Low => Some("low"),
        crate::state::ThinkingLevel::Medium => Some("medium"),
        crate::state::ThinkingLevel::High => Some("high"),
        crate::state::ThinkingLevel::XHigh => Some("xhigh"),
        crate::state::ThinkingLevel::Max => Some("max"),
    }
}

pub(super) fn build_payload(
    config: &OpenRouterConfig,
    request: &ModelRequest,
) -> Result<Vec<u8>, String> {
    let messages = JsonValue::parse(&request.context)
        .map_err(|_| "OpenRouter received invalid converted context".to_owned())?;
    let messages = messages
        .as_array()
        .ok_or_else(|| "OpenRouter converted context must be an array".to_owned())?
        .to_owned();
    let mut chat_messages = Vec::with_capacity(messages.len() + 1);
    chat_messages.push(json_value!({
        "role": "system",
        "content": request.system_prompt.clone()
    }));
    chat_messages.extend(messages);
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let schema = tool.schema.clone();
            Ok(json_value!({
                "type": "function",
                "function": json_value!({
                    "name": tool.name.clone(),
                    "description": tool.description.clone(),
                    "parameters": schema,
                }),
            }))
        })
        .collect::<Result<Vec<_>, &str>>()?;
    let mut payload = json_value!({
        "model": config.model.clone(),
        "messages": chat_messages,
        "temperature": 0,
        "stream": true,
        "stream_options": json_value!({"include_usage": true}),
    });
    if let Some(max_tokens) = config.max_tokens {
        payload
            .as_object_mut()
            .expect("OpenRouter payload is an object")
            .insert("max_tokens".to_owned(), json_value!(max_tokens));
    }
    if let Some(effort) = reasoning_effort(request.thinking_level) {
        payload
            .as_object_mut()
            .expect("OpenRouter payload is an object")
            .insert("reasoning".to_owned(), json_value!({"effort": effort}));
    }
    if !tools.is_empty() {
        let object = payload
            .as_object_mut()
            .expect("OpenRouter payload is an object");
        object.insert("tools".to_owned(), JsonValue::Array(tools));
        // Require OpenRouter to select an endpoint that honors every supplied tool parameter;
        // otherwise an endpoint may silently ignore the tool schema and fall back to prose.
        object.insert(
            "provider".to_owned(),
            json_value!({"require_parameters": true}),
        );
    }
    to_bytes(&payload).map_err(|_| "cannot serialize OpenRouter request".to_owned())
}
