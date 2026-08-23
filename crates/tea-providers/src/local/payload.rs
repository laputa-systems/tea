//! Local OpenAI-compatible request payload encoding.

use super::config::LocalConfig;
use crate::json::{JsonValue, json_value};
use crate::scheduler::ModelRequest;
use crate::state::ThinkingLevel;

pub(super) fn local_payload(config: &LocalConfig, request: ModelRequest) -> Result<String, String> {
    let context = JsonValue::parse(request.context.trim())
        .map_err(|_| "local context was not valid JSON".to_owned())?;
    let JsonValue::Array(mut messages) = context else {
        return Err("local context was not a JSON message array".to_owned());
    };
    messages.insert(
        0,
        JsonValue::object([
            ("role", JsonValue::from("system")),
            ("content", JsonValue::from(request.system_prompt)),
        ]),
    );
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            JsonValue::object([
                ("type", JsonValue::from("function")),
                (
                    "function",
                    JsonValue::object([
                        ("name", JsonValue::from(tool.name.clone())),
                        ("description", JsonValue::from(tool.description.clone())),
                        ("parameters", tool.schema.clone()),
                    ]),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let body = json_value!({
        "model": config.model.clone(),
        "messages": JsonValue::Array(messages),
        "temperature": config.temperature,
        "top_p": config.top_p,
        "min_p": config.min_p,
        "max_tokens": config.max_tokens,
        // Local OpenAI-compatible servers expose incremental responses as SSE.  The
        // provider owns decoding those records, so the request must opt into that wire mode.
        "stream": true,
        "stream_options": JsonValue::object([("include_usage", JsonValue::from(true))]),
        "chat_template_kwargs": JsonValue::object([(
            "enable_thinking",
            JsonValue::from(config.enable_thinking && request.thinking_level != ThinkingLevel::Off),
        )]),
        "tools": JsonValue::Array(tools)
    });
    body.to_json_string()
        .map_err(|error| format!("could not serialize local request: {error}"))
}
