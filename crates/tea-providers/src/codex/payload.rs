//! Deterministic Codex Responses request serialization.

use super::config::CodexConfig;
use crate::json::{JsonValue, json_value, to_bytes};
use crate::scheduler::ModelRequest;
use crate::state::ThinkingLevel;
use std::collections::BTreeSet;

/// Convert Tea's typed thinking selection to the pinned Codex effort spelling.
pub(super) const fn reasoning_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
        ThinkingLevel::XHigh | ThinkingLevel::Max => Some("xhigh"),
    }
}

/// Build one exact direct Codex Responses payload.
pub(super) fn build_payload(
    config: &CodexConfig,
    request: &ModelRequest,
    session_id: &str,
) -> Result<Vec<u8>, String> {
    let input = JsonValue::parse(&request.context)
        .map_err(|_| "Codex received invalid converted Responses context".to_owned())?;
    let input = input
        .as_array()
        .ok_or_else(|| "Codex converted Responses context must be an array".to_owned())?
        .to_vec();
    if session_id.is_empty() || session_id.chars().any(char::is_control) {
        return Err("Codex session identity is invalid".into());
    }

    let mut payload = JsonValue::object([
        ("model", JsonValue::String(config.model.clone())),
        ("store", JsonValue::Bool(false)),
        ("stream", JsonValue::Bool(true)),
        ("input", JsonValue::Array(input)),
        ("tool_choice", JsonValue::String("auto".into())),
        ("parallel_tool_calls", JsonValue::Bool(true)),
        ("include", json_value!(["reasoning.encrypted_content"])),
        (
            "text",
            JsonValue::object([(
                "verbosity",
                JsonValue::String(config.text_verbosity.as_wire().into()),
            )]),
        ),
        ("prompt_cache_key", JsonValue::String(session_id.to_owned())),
    ]);
    let object = payload
        .as_object_mut()
        .expect("Codex payload is always an object");
    if !request.system_prompt.trim().is_empty() {
        object.insert(
            "instructions".into(),
            JsonValue::String(request.system_prompt.clone()),
        );
    }
    if let Some(effort) = reasoning_effort(request.thinking_level) {
        object.insert(
            "reasoning".into(),
            JsonValue::object([
                ("effort", JsonValue::String(effort.into())),
                ("summary", JsonValue::String("auto".into())),
            ]),
        );
    }
    if !request.tools.is_empty() {
        let mut names = BTreeSet::new();
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                if tool.name.trim().is_empty() || !names.insert(tool.name.clone()) {
                    return Err("Codex tool names must be nonempty and unique".to_owned());
                }
                Ok(JsonValue::object([
                    ("type", JsonValue::String("function".into())),
                    ("name", JsonValue::String(tool.name.clone())),
                    ("description", JsonValue::String(tool.description.clone())),
                    ("parameters", tool.schema.clone()),
                    // The pinned Codex wire implementation uses the native
                    // non-strict representation rather than forcing schemas
                    // into a stricter contract than Tea's tool boundary.
                    ("strict", JsonValue::Null),
                ]))
            })
            .collect::<Result<Vec<_>, _>>()?;
        object.insert("tools".into(), JsonValue::Array(tools));
    }
    to_bytes(&payload).map_err(|_| "cannot serialize Codex Responses request".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::auth::CodexAuthManager;
    use crate::codex::credentials::InMemoryCredentialStore;
    use crate::state::{ModelDescriptor, ThinkingLevel};
    use std::sync::Arc;

    fn config() -> CodexConfig {
        CodexConfig::new(
            Arc::new(CodexAuthManager::with_system_clock(Arc::new(
                InMemoryCredentialStore::new(),
            ))),
            "gpt-5-codex",
        )
    }

    #[test]
    fn request_uses_responses_input_and_never_chat_completions_fields() {
        let payload = build_payload(
            &config(),
            &ModelRequest {
                system_prompt: "system".into(),
                context: r#"[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]"#.into(),
                model: Some(ModelDescriptor {
                    provider: "codex".into(),
                    model: "gpt-5-codex".into(),
                    revision: None,
                }),
                thinking_level: ThinkingLevel::High,
                session_id: Some("session_1".into()),
                ..ModelRequest::default()
            },
            "session_1",
        )
        .unwrap();
        let payload = JsonValue::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
        assert_eq!(
            payload.get("store").and_then(JsonValue::as_bool),
            Some(false)
        );
        assert_eq!(
            payload.get("stream").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert!(payload.get("input").is_some());
        assert!(payload.get("messages").is_none());
        assert!(payload.get("max_tokens").is_none());
        assert_eq!(
            payload
                .get("reasoning")
                .and_then(|value| value.get("effort"))
                .and_then(JsonValue::as_str),
            Some("high")
        );
        assert_eq!(
            payload.get("prompt_cache_key").and_then(JsonValue::as_str),
            Some("session_1")
        );
        assert_eq!(
            payload.get("tool_choice").and_then(JsonValue::as_str),
            Some("auto")
        );
    }

    #[test]
    fn reasoning_levels_are_typed_and_clamped() {
        assert_eq!(reasoning_effort(ThinkingLevel::Off), None);
        assert_eq!(reasoning_effort(ThinkingLevel::Minimal), Some("minimal"));
        assert_eq!(reasoning_effort(ThinkingLevel::XHigh), Some("xhigh"));
        assert_eq!(reasoning_effort(ThinkingLevel::Max), Some("xhigh"));
    }

    #[test]
    fn function_tools_pin_the_native_non_strict_wire_shape() {
        let payload = build_payload(
            &config(),
            &ModelRequest {
                context: r#"[]"#.into(),
                tools: vec![crate::tool::ToolDefinition {
                    name: "read".into(),
                    description: "Read a file".into(),
                    schema: JsonValue::object([("type", JsonValue::String("object".into()))]),
                    execution_mode: crate::tool::ToolExecutionMode::Sequential,
                    requires_exclusive_batch: false,
                    cancellation_settlement_mode:
                        crate::tool::CancellationSettlementMode::DropFuture,
                }],
                ..ModelRequest::default()
            },
            "session_1",
        )
        .unwrap();
        let payload = JsonValue::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
        let tool = payload
            .get("tools")
            .and_then(JsonValue::as_array)
            .and_then(|tools| tools.first())
            .expect("one serialized function tool");
        assert_eq!(tool.get("strict"), Some(&JsonValue::Null));
    }
}
