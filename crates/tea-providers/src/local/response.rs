//! Local OpenAI-compatible response parsing.

use crate::json::{JsonValue, from_bytes};
use crate::scheduler::ModelStreamEvent;
use crate::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId, Usage};
use std::collections::BTreeMap;

/// The completed portion of one local OpenAI-compatible SSE response.
pub(super) struct LocalSseComplete {
    pub(super) events: Vec<ModelStreamEvent>,
}

#[derive(Default)]
struct StreamingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Incremental decoder for OpenAI-compatible Chat Completions SSE records.
///
/// Text is emitted as each `delta.content` record arrives. Tool calls are assembled from their
/// indexed fragments and exposed only after the response has settled, matching the core event
/// contract's complete-call boundary.
pub(super) struct LocalSseDecoder {
    buffered: Vec<u8>,
    tool_calls: Vec<Option<StreamingToolCall>>,
    finish_reason: Option<String>,
    usage: JsonValue,
    saw_data: bool,
    saw_done: bool,
}

impl LocalSseDecoder {
    pub(super) fn new() -> Self {
        Self {
            buffered: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: JsonValue::Null,
            saw_data: false,
            saw_done: false,
        }
    }

    /// Reduce complete SSE records from an arbitrary HTTP body chunk.
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelStreamEvent>, String> {
        self.buffered.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffered.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            self.process_line(&line, &mut events)?;
        }
        Ok(events)
    }

    /// Finish a response body and append usage, tool calls, and the terminal event.
    pub(super) fn finish(mut self) -> Result<LocalSseComplete, String> {
        let mut events = Vec::new();
        if !self.buffered.is_empty() {
            let line = std::mem::take(&mut self.buffered);
            self.process_line(&line, &mut events)?;
        }
        if !self.saw_data || (!self.saw_done && self.finish_reason.is_none()) {
            return Err("local SSE response ended before completion".to_owned());
        }
        let has_tool_calls = self.tool_calls.iter().any(Option::is_some);
        for (index, call) in self.tool_calls.into_iter().flatten().enumerate() {
            let id = call
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("local-call-{index}"));
            let name = call
                .name
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "local tool call did not contain a name".to_owned())?;
            events.push(ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new(id).map_err(|error| error.to_string())?,
                name,
                arguments: SerializedJson::new(call.arguments),
            }));
        }
        let usage = parse_stream_usage(Some(&self.usage));
        // Preserve the finite local adapter's accounting contract: one usage update precedes
        // every terminal event, even when the server omitted token counts.
        events.push(ModelStreamEvent::Usage(usage));
        let stop_reason = match self.finish_reason.as_deref() {
            Some("length") => StopReason::Length,
            Some("tool_calls" | "tool_call") if has_tool_calls => StopReason::ToolUse,
            _ if has_tool_calls => StopReason::ToolUse,
            _ => StopReason::Stop,
        };
        events.push(ModelStreamEvent::End(stop_reason));
        Ok(LocalSseComplete { events })
    }

    fn process_line(
        &mut self,
        line: &[u8],
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), String> {
        let line = std::str::from_utf8(line)
            .map_err(|_| "local server returned a non-UTF-8 SSE response".to_owned())?
            .trim();
        if line.is_empty() || line.starts_with(':') {
            return Ok(());
        }
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            return Ok(());
        };
        if data == "[DONE]" {
            self.saw_done = true;
            return Ok(());
        }
        self.saw_data = true;
        let chunk = crate::json::from_bytes(data.as_bytes())
            .map_err(|_| "local server returned an invalid SSE event".to_owned())?;
        if let Some(error) = chunk.get("error") {
            return Err(format!(
                "local server rejected the request: {}",
                error_message(error)
            ));
        }
        if let Some(usage) = chunk.get("usage")
            && !matches!(usage, JsonValue::Null)
        {
            self.usage = usage.clone();
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(JsonValue::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(JsonValue::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }
        let delta = choice
            .get("delta")
            .and_then(JsonValue::as_object)
            .or_else(|| choice.get("message").and_then(JsonValue::as_object));
        let Some(delta) = delta else {
            return Ok(());
        };
        if let Some(content) = delta.get("content").and_then(JsonValue::as_str)
            && !content.is_empty()
        {
            events.push(ModelStreamEvent::TextDelta(content.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(JsonValue::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(JsonValue::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .unwrap_or(position);
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(None);
                }
                let entry = self.tool_calls[index].get_or_insert_with(StreamingToolCall::default);
                if let Some(id) = call.get("id").and_then(JsonValue::as_str) {
                    entry.id = Some(id.to_owned());
                }
                if let Some(function) = call.get("function").and_then(JsonValue::as_object) {
                    if let Some(name) = function.get("name").and_then(JsonValue::as_str) {
                        entry.name = Some(name.to_owned());
                    }
                    if let Some(arguments) = function.get("arguments").and_then(JsonValue::as_str) {
                        entry.arguments.push_str(arguments);
                    }
                }
            }
        }
        Ok(())
    }
}

fn parse_stream_usage(value: Option<&JsonValue>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    Usage {
        input_tokens: value.get("prompt_tokens").and_then(JsonValue::as_u64),
        output_tokens: value.get("completion_tokens").and_then(JsonValue::as_u64),
        cache_read_tokens: value
            .get("prompt_tokens_details")
            .and_then(JsonValue::as_object)
            .and_then(|details| details.get("cached_tokens"))
            .and_then(JsonValue::as_u64),
        ..Usage::default()
    }
}

pub(super) fn parse_local_response(
    bytes: &[u8],
    http_status: u16,
) -> Result<(Vec<ModelStreamEvent>, Usage), String> {
    let response = from_bytes(bytes)?;
    if let Some(error) = response.get("error") {
        return Err(format!(
            "local server rejected the request with HTTP {http_status}: {}",
            error_message(error)
        ));
    }
    if !(200..300).contains(&http_status) {
        return Err(format!(
            "local server returned HTTP {http_status} without a completion"
        ));
    }
    let choice = array_field(&response, "choices")?
        .first()
        .ok_or_else(|| "local response did not contain a completion choice".to_owned())?;
    let message = object_field(choice, "message")?;
    let mut events = Vec::new();
    if let Some(content) = optional_string(message.get("content"))?
        && !content.is_empty()
    {
        events.push(ModelStreamEvent::TextDelta(content.to_owned()));
    }
    let mut has_tool_calls = false;
    if let Some(calls) = optional_array(message.get("tool_calls"))? {
        for (index, call) in calls.iter().enumerate() {
            let call_object = as_object(call, "local tool call")?;
            let id = optional_string(call_object.get("id"))?
                .filter(|id| !id.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("local-call-{index}"));
            let function = object_field(call, "function")?;
            let name = required_string(function.get("name"), "local tool call name")?;
            let arguments =
                required_string(function.get("arguments"), "local serialized tool arguments")?;
            events.push(ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new(id).map_err(|error| error.to_string())?,
                name: name.to_owned(),
                arguments: SerializedJson::new(arguments),
            }));
            has_tool_calls = true;
        }
    }
    let finish_reason = optional_string(as_object(choice, "local choice")?.get("finish_reason"))?;
    let stop_reason = match finish_reason {
        Some("tool_calls" | "tool_call") if has_tool_calls => StopReason::ToolUse,
        Some("length") => StopReason::Length,
        _ if has_tool_calls => StopReason::ToolUse,
        _ => StopReason::Stop,
    };
    events.push(ModelStreamEvent::End(stop_reason));
    Ok((events, parse_usage(response.get("usage"))?))
}

fn parse_usage(value: Option<&JsonValue>) -> Result<Usage, String> {
    let Some(value) = value else {
        return Ok(Usage::default());
    };
    let cache_read_tokens = match value.get("prompt_tokens_details") {
        None | Some(JsonValue::Null) => None,
        Some(details) => number_field(details, "cached_tokens")?,
    };
    Ok(Usage {
        input_tokens: number_field(value, "prompt_tokens")?,
        output_tokens: number_field(value, "completion_tokens")?,
        cache_read_tokens,
        ..Usage::default()
    })
}
fn as_object<'a>(
    value: &'a JsonValue,
    description: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, String> {
    match value {
        JsonValue::Object(value) => Ok(value),
        _ => Err(format!("{description} was not a JSON object")),
    }
}

fn object_field<'a>(
    value: &'a JsonValue,
    name: &str,
) -> Result<&'a BTreeMap<String, JsonValue>, String> {
    as_object(value, "local JSON value")?
        .get(name)
        .ok_or_else(|| format!("local response omitted {name:?}"))
        .and_then(|value| as_object(value, name))
}

fn array_field<'a>(value: &'a JsonValue, name: &str) -> Result<&'a [JsonValue], String> {
    match as_object(value, "local JSON value")?.get(name) {
        Some(JsonValue::Array(value)) => Ok(value),
        Some(_) => Err(format!("local response field {name:?} was not an array")),
        None => Err(format!("local response omitted {name:?}")),
    }
}

fn optional_array(value: Option<&JsonValue>) -> Result<Option<&[JsonValue]>, String> {
    match value {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Array(value)) => Ok(Some(value)),
        Some(_) => Err("local tool_calls was not an array".to_owned()),
    }
}

fn optional_string(value: Option<&JsonValue>) -> Result<Option<&str>, String> {
    match value {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(value)) => Ok(Some(value)),
        Some(_) => Err("local response field was not a string".to_owned()),
    }
}

fn required_string<'a>(value: Option<&'a JsonValue>, description: &str) -> Result<&'a str, String> {
    optional_string(value)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{description} was missing or empty"))
}

fn number_field(value: &JsonValue, name: &str) -> Result<Option<u64>, String> {
    let object = as_object(value, "local usage")?;
    match object.get(name) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Number(tea_protocol::JsonNumber::Unsigned(value))) => Ok(Some(*value)),
        Some(JsonValue::Number(tea_protocol::JsonNumber::Signed(value))) if *value >= 0 => {
            Ok(Some(*value as u64))
        }
        Some(_) => Err(format!(
            "local usage field {name:?} was not a non-negative integer"
        )),
    }
}

fn error_message(error: &JsonValue) -> String {
    error
        .get("message")
        .and_then(|value| match value {
            JsonValue::String(value) => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "local server rejected the request".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{LocalSseDecoder, ModelStreamEvent};
    use crate::state::{StopReason, Usage};

    #[test]
    fn local_sse_decoder_releases_text_before_terminal_records() {
        let mut decoder = LocalSseDecoder::new();
        assert_eq!(
            decoder
                .push(
                    br#"data: {"choices":[{"delta":{"content":"first "},"finish_reason":null}]}

"#,
                )
                .expect("first local SSE record parses"),
            [ModelStreamEvent::TextDelta("first ".into())]
        );
        assert_eq!(
            decoder
                .push(
                    br#"data: {"choices":[{"delta":{"content":"second"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}

data: [DONE]

"#,
                )
                .expect("terminal local SSE records parse"),
            [ModelStreamEvent::TextDelta("second".into())]
        );
        let complete = decoder.finish().expect("local SSE body settles");
        assert_eq!(
            complete.events,
            [
                ModelStreamEvent::Usage(Usage {
                    input_tokens: Some(2),
                    output_tokens: Some(1),
                    ..Usage::default()
                }),
                ModelStreamEvent::End(StopReason::Stop),
            ]
        );
    }

    #[test]
    fn local_sse_decoder_assembles_indexed_tool_fragments() {
        let mut decoder = LocalSseDecoder::new();
        decoder
            .push(
                br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"edit","arguments":"{\"path\":"}}]},"finish_reason":null}]}

"#,
            )
            .expect("first tool fragment parses");
        decoder
            .push(
                br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.py\"}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#,
            )
            .expect("terminal tool fragment parses");
        let complete = decoder.finish().expect("tool response settles");
        assert!(matches!(
            complete.events.first(),
            Some(ModelStreamEvent::ToolCall(call))
                if call.id.as_str() == "call_1"
                    && call.name == "edit"
                    && call.arguments.as_str() == "{\"path\":\"a.py\"}"
        ));
        assert_eq!(
            complete.events.last(),
            Some(&ModelStreamEvent::End(StopReason::ToolUse))
        );
    }
}
