//! Command Code NDJSON response parsing and usage accounting.

use super::config::{CommandCodeErrorReport, CommandCodeErrorSource};
use super::payload::string_field;
use crate::json::JsonValue;
use crate::scheduler::ModelStreamEvent;
use crate::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId, Usage};
use std::collections::BTreeMap;
#[derive(Debug)]
pub(super) struct ParsedCommandCodeResponse {
    pub(super) events: Vec<ModelStreamEvent>,
    pub(super) usage: Usage,
    pub(super) error: Option<CommandCodeErrorReport>,
}

pub(super) fn parse_ndjson_response(
    bytes: &[u8],
    api_key: &str,
) -> Result<ParsedCommandCodeResponse, String> {
    let response = std::str::from_utf8(bytes)
        .map_err(|_| "Command Code returned a non-UTF-8 NDJSON response".to_owned())?;
    let mut events = Vec::new();
    let mut usage = Usage::default();
    let mut terminal = false;
    let mut error = None;
    for line in response.lines().filter(|line| !line.trim().is_empty()) {
        let event = JsonValue::parse(line)
            .map_err(|_| "Command Code returned invalid NDJSON".to_owned())?;
        let event = event
            .as_object()
            .ok_or_else(|| "Command Code NDJSON event must be an object".to_owned())?;
        let event_type = match event.get("type").and_then(JsonValue::as_str) {
            Some(event_type) if !event_type.is_empty() => event_type,
            // The gateway also uses a JSON error envelope for HTTP-level failures. It is
            // returned on the same endpoint, but has no NDJSON event `type`; preserve its
            // structured diagnostics instead of reducing it to a malformed-stream error.
            None if event.contains_key("error") => {
                error = Some(parse_gateway_error(event, api_key));
                events.push(ModelStreamEvent::Error {
                    message: "Command Code provider returned an error".into(),
                });
                terminal = true;
                continue;
            }
            _ => return Err("Command Code NDJSON event did not contain type".to_owned()),
        };
        if terminal {
            // Command Code 1.24.0 emits this non-content metadata envelope after `finish`.
            // It is not a second terminal event and carries no core stream state.
            if event_type == "provider-metadata" {
                continue;
            }
            return Err("Command Code response contained events after its terminal event".into());
        }
        match event_type {
            "text-delta" => {
                let text = string_field(event, "text", "Command Code text delta")?;
                if !text.is_empty() {
                    events.push(ModelStreamEvent::TextDelta(text.to_owned()));
                }
            }
            "reasoning-start" | "reasoning-delta" => {}
            "tool-call" => {
                let id = string_field(event, "toolCallId", "Command Code tool call")?;
                let name = string_field(event, "toolName", "Command Code tool call")?;
                let input = event
                    .get("input")
                    .or_else(|| event.get("args"))
                    .filter(|value| value.is_object())
                    .ok_or_else(|| {
                        "Command Code tool call did not contain object input".to_owned()
                    })?;
                let arguments = input
                    .to_json_string()
                    .map_err(|_| "Command Code tool call input cannot be serialized".to_owned())?;
                events.push(ModelStreamEvent::ToolCall(AgentToolCall {
                    id: ToolCallId::new(id)
                        .map_err(|_| "Command Code tool call omitted its identifier".to_owned())?,
                    name: name.to_owned(),
                    arguments: SerializedJson::new(arguments),
                }));
            }
            "finish" => {
                usage = parse_usage(event.get("totalUsage"));
                events.push(ModelStreamEvent::End(finish_reason(
                    event.get("finishReason"),
                )));
                terminal = true;
            }
            "error" => {
                error = Some(parse_gateway_error(event, api_key));
                events.push(ModelStreamEvent::Error {
                    message: "Command Code provider returned an error".into(),
                });
                terminal = true;
            }
            "abort" => {
                events.push(ModelStreamEvent::Aborted {
                    message: "Command Code stream aborted".into(),
                });
                terminal = true;
            }
            _ => {}
        }
    }
    if terminal {
        Ok(ParsedCommandCodeResponse {
            events,
            usage,
            error,
        })
    } else {
        Err("Command Code stream ended without a terminal event".into())
    }
}

/// A provider response that is structurally incomplete before any stream event reaches the core.
/// These failures commonly represent a truncated or non-NDJSON gateway error body and are safe
/// to replay; request validation failures happen before this boundary and are not retried.
pub(super) fn is_retryable_response_error(message: &str) -> bool {
    matches!(
        message,
        "Command Code returned a non-UTF-8 NDJSON"
            | "Command Code returned invalid NDJSON"
            | "Command Code NDJSON event did not contain type"
            | "Command Code NDJSON event must be an object"
            | "Command Code stream ended without a terminal event"
            | "Command Code response contained events after its terminal event"
    )
}

/// Parse the error conventions used by Command Code 1.24.0 without turning remote text into an
/// agent-visible error. In addition to object fields, the client recognizes a message shaped as
/// `<status> {\"error\": {\"type\": ..., \"message\": ...}}`; preserve that useful diagnostic
/// structure for trusted hosts.
fn parse_gateway_error(
    event: &BTreeMap<String, JsonValue>,
    api_key: &str,
) -> CommandCodeErrorReport {
    let (mut message, direct_status, direct_retryable, mut error_type, error_code) =
        match event.get("error") {
            Some(JsonValue::String(message)) if !message.is_empty() => {
                (message.to_owned(), None, None, None, None)
            }
            Some(JsonValue::Object(error)) => (
                error
                    .get("message")
                    .and_then(JsonValue::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("Command Code gateway emitted an error without a message")
                    .to_owned(),
                status_code(error.get("statusCode")).or_else(|| status_code(error.get("status"))),
                error.get("isRetryable").and_then(JsonValue::as_bool),
                diagnostic_text(error.get("type"), api_key),
                diagnostic_text(error.get("code"), api_key),
            ),
            _ => (
                "Command Code gateway emitted an error without diagnostic details".into(),
                None,
                None,
                None,
                None,
            ),
        };
    if let Some(embedded) = parse_embedded_error(&message) {
        message = embedded.message;
        error_type = embedded.error_type.or(error_type);
        let status_code = embedded.status_code.or(direct_status);
        let retryable = is_retryable(status_code, direct_retryable, &message);
        return CommandCodeErrorReport {
            source: CommandCodeErrorSource::Gateway,
            message: redact_diagnostic_text(&message, api_key),
            status_code,
            error_type: error_type.map(|value| redact_diagnostic_text(&value, api_key)),
            error_code,
            retryable: Some(retryable),
        };
    }
    let retryable = is_retryable(direct_status, direct_retryable, &message);
    CommandCodeErrorReport {
        source: CommandCodeErrorSource::Gateway,
        message: redact_diagnostic_text(&message, api_key),
        status_code: direct_status,
        error_type,
        error_code,
        retryable: Some(retryable),
    }
}

struct EmbeddedGatewayError {
    message: String,
    status_code: Option<u16>,
    error_type: Option<String>,
}

fn parse_embedded_error(message: &str) -> Option<EmbeddedGatewayError> {
    let json_start = message.find('{')?;
    let payload = JsonValue::parse(&message[json_start..]).ok()?;
    let error = payload.get("error")?.as_object()?;
    let embedded_message = error.get("message")?.as_str()?.trim();
    if embedded_message.is_empty() {
        return None;
    }
    let prefix = message[..json_start].trim();
    Some(EmbeddedGatewayError {
        message: embedded_message.to_owned(),
        status_code: prefix.parse::<u16>().ok(),
        error_type: error
            .get("type")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    })
}

fn status_code(value: Option<&JsonValue>) -> Option<u16> {
    value
        .and_then(JsonValue::as_u64)
        .and_then(|value| u16::try_from(value).ok())
}

fn diagnostic_text(value: Option<&JsonValue>, api_key: &str) -> Option<String> {
    value
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| redact_diagnostic_text(value, api_key))
}

fn is_retryable(status_code: Option<u16>, reported: Option<bool>, message: &str) -> bool {
    if reported == Some(true) {
        return true;
    }
    if let Some(status_code) = status_code {
        return status_code == 429 || (500..=599).contains(&status_code);
    }
    reported != Some(false) && !has_terminal_marker(message)
}

fn has_terminal_marker(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "premium_credits_exhausted",
        "model_not_in_plan",
        "insufficient credits",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn redact_diagnostic_text(value: &str, api_key: &str) -> String {
    const MAX_DIAGNOSTIC_CHARS: usize = 4_096;
    let value = if api_key.is_empty() {
        value.to_owned()
    } else {
        value.replace(api_key, "[redacted]")
    };
    if value.chars().count() <= MAX_DIAGNOSTIC_CHARS {
        return value;
    }
    let truncated = value.chars().take(MAX_DIAGNOSTIC_CHARS).collect::<String>();
    format!("{truncated}… [truncated]")
}

fn finish_reason(value: Option<&JsonValue>) -> StopReason {
    match value.and_then(JsonValue::as_str) {
        Some("tool-calls") => StopReason::ToolUse,
        Some("length") => StopReason::Length,
        _ => StopReason::Stop,
    }
}

fn parse_usage(value: Option<&JsonValue>) -> Usage {
    let Some(value) = value.and_then(JsonValue::as_object) else {
        return Usage::default();
    };
    let input_tokens = value.get("inputTokens").and_then(JsonValue::as_u64);
    let output_tokens = value.get("outputTokens").and_then(JsonValue::as_u64);
    let reasoning_tokens = value
        .get("reasoningTokens")
        .and_then(JsonValue::as_u64)
        .or_else(|| value.get("reasoning_tokens").and_then(JsonValue::as_u64));
    Usage {
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cache_read_tokens: None,
        cache_write_tokens: None,
        cost: None,
    }
}

pub(super) fn add_usage(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}
