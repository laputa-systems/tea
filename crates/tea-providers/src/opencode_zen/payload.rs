//! OpenCode Zen Responses API payload encoding.

use super::config::OpencodeZenConfig;
use crate::json::{JsonValue, json_value, to_bytes};
use crate::scheduler::ModelRequest;

pub(super) fn reasoning_effort(level: crate::state::ThinkingLevel) -> Option<&'static str> {
    match level {
        crate::state::ThinkingLevel::Off => None,
        crate::state::ThinkingLevel::Minimal => Some("low"),
        crate::state::ThinkingLevel::Low => Some("low"),
        crate::state::ThinkingLevel::Medium => Some("medium"),
        crate::state::ThinkingLevel::High => Some("high"),
        crate::state::ThinkingLevel::XHigh => Some("high"),
        crate::state::ThinkingLevel::Max => Some("high"),
    }
}

pub(super) fn build_payload(
    config: &OpencodeZenConfig,
    request: &ModelRequest,
) -> Result<Vec<u8>, String> {
    // Context is OpenAiContextHook output: JSON array of Chat messages.
    let messages = JsonValue::parse(&request.context)
        .map_err(|_| "OpenCode Zen received invalid converted context".to_owned())?;
    let messages = messages
        .as_array()
        .ok_or_else(|| "OpenCode Zen converted context must be an array".to_owned())?
        .to_owned();

    // Build Responses `input` array from Chat messages + system_prompt
    let mut input: Vec<JsonValue> = Vec::with_capacity(messages.len() + 1);

    if !request.system_prompt.trim().is_empty() {
        input.push(json_value!({
            "role": "system",
            "content": request.system_prompt.clone()
        }));
    }

    for msg in messages {
        let role = msg.get("role").and_then(JsonValue::as_str).unwrap_or("");
        match role {
            "system" | "developer" => {
                if let Some(content) = msg.get("content").and_then(JsonValue::as_str) {
                    if !content.is_empty() {
                        input.push(json_value!({
                            "role": "system",
                            "content": content.to_owned()
                        }));
                    }
                } else if let Some(content) = msg.get("content") {
                    // If content is not simple string, keep as is for system
                    input.push(json_value!({
                        "role": "system",
                        "content": content.clone()
                    }));
                }
            }
            "user" => {
                // OpenAI Responses accepts user content as string or array of input_text parts.
                // Keep original shape: if content is string, preserve string; if array, map.
                if let Some(content) = msg.get("content") {
                    if let Some(s) = content.as_str() {
                        input.push(json_value!({
                            "role": "user",
                            "content": s.to_owned()
                        }));
                    } else {
                        // For complex user content (array with text/image parts), preserve but
                        // try to normalize to input_text where possible.
                        // The gateway handles translation; keep original array.
                        input.push(json_value!({
                            "role": "user",
                            "content": content.clone()
                        }));
                    }
                }
            }
            "assistant" => {
                // Assistant messages may contain content and tool_calls
                let content = msg.get("content").and_then(JsonValue::as_str).unwrap_or("");
                let has_text = !content.is_empty();
                // For Responses, assistant text is separate message item, but gateway likely accepts Chat style too.
                // To be maximally compatible, emit Chat-style assistant plus function_call items separately.
                // However Responses spec expects assistant messages as {role:"assistant", content:[{type:"output_text", text:""}]}
                // We'll emit both: one assistant message for text, and separate function_call items for each tool call.
                if has_text {
                    input.push(json_value!({
                        "role": "assistant",
                        "content": content.to_owned()
                    }));
                }
                if let Some(calls) = msg.get("tool_calls").and_then(JsonValue::as_array) {
                    for call in calls {
                        let id = call
                            .get("id")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("call_unknown");
                        let func = call.get("function").and_then(JsonValue::as_object);
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(JsonValue::as_str)
                            .unwrap_or("unknown");
                        let args = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(JsonValue::as_str)
                            .unwrap_or("{}");
                        // Responses function_call item
                        input.push(json_value!({
                            "type": "function_call",
                            "call_id": id.to_owned(),
                            "name": name.to_owned(),
                            "arguments": args.to_owned()
                        }));
                    }
                } else if !has_text {
                    // Empty assistant placeholder: ensure at least an empty assistant to keep turn order
                    // If only tool_calls existed, we already emitted them above; if neither, emit empty.
                    if msg.get("tool_calls").is_none() {
                        input.push(json_value!({
                            "role": "assistant",
                            "content": ""
                        }));
                    }
                }
            }
            "tool" => {
                // Chat tool result -> Responses function_call_output
                let call_id = msg
                    .get("tool_call_id")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("unknown");
                let content = msg.get("content").and_then(JsonValue::as_str).unwrap_or("");
                let output = content.to_owned();
                input.push(json_value!({
                    "type": "function_call_output",
                    "call_id": call_id.to_owned(),
                    "output": output
                }));
            }
            _ => {
                // Unknown role: pass through as is to avoid dropping context
                input.push(msg);
            }
        }
    }

    let tools = request
        .tools
        .iter()
        .map(|tool| {
            let schema = tool.schema.clone();
            // Real opencode client forces strict:false for OpenAI-family to avoid structured-output validation
            // failures with MCP schemas that violate OpenAI's strict rules (see opencode/src/session/llm/request.ts:152-158).
            Ok(json_value!({
                "type": "function",
                "name": tool.name.clone(),
                "description": tool.description.clone(),
                "parameters": schema,
                "strict": false
            }))
        })
        .collect::<Result<Vec<_>, &str>>()?;

    // Temperature is intentionally omitted for opencode-zen/muse-spark: the real
    // OpenCode transform leaves temperature undefined for Meta/muse-spark (see
    // provider/transform.ts:527-544), letting the model default (1.0) apply.
    // Forcing 0 would degrade reasoning quality and mismatches cost telemetry;
    // openrouter's forced 0 is specific to its chat completions contract.
    let mut payload = json_value!({
        "model": config.model.clone(),
        "input": input,
        "stream": true,
    });

    if let Some(max_tokens) = config.max_tokens {
        payload
            .as_object_mut()
            .expect("payload is object")
            .insert("max_output_tokens".to_owned(), json_value!(max_tokens));
    }
    if let Some(effort) = reasoning_effort(request.thinking_level) {
        payload
            .as_object_mut()
            .expect("payload is object")
            .insert("reasoning".to_owned(), json_value!({"effort": effort}));
    }
    if !tools.is_empty() {
        payload
            .as_object_mut()
            .expect("payload is object")
            .insert("tools".to_owned(), JsonValue::Array(tools));
    }

    to_bytes(&payload).map_err(|_| "cannot serialize OpenCode Zen request".to_owned())
}
