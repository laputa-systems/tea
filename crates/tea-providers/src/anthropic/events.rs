//! Reduction of Anthropic Messages stream events to Tea model-stream events.
//!
//! Port of the event loop in upstream Pi's `stream()`, `iterateAnthropicEvents`,
//! and `mapStopReason` (`packages/ai/src/api/anthropic-messages.ts`). Answer
//! text and thinking are forwarded as they arrive; a thinking block's
//! signature is accumulated and emitted when the block stops; redacted
//! thinking is opaque from its start; tool input JSON is accumulated and
//! emitted, repaired if necessary, when its block stops.

use super::PROVIDER_ID;
use super::payload::{EFFORT_CONTEXT_KIND, REDACTED_THINKING_CONTEXT_KIND, SIGNATURE_CONTEXT_KIND};
use super::sse::{ServerSentEvent, parse_json_with_repair};
use crate::json::JsonValue;
use crate::scheduler::ModelStreamEvent;
use crate::state::{
    AgentToolCall, OpaqueProviderContextItem, SerializedJson, StopReason, ToolCallId, Usage,
};
use std::collections::BTreeMap;

const MESSAGE_EVENTS: &[&str] = &[
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// Raw Anthropic usage counters, kept for exact cost estimation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnthropicUsage {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens, including thinking.
    pub output: u64,
    /// Cache-read input tokens.
    pub cache_read: u64,
    /// Cache-write input tokens of any lifetime.
    pub cache_write: u64,
    /// Subset of `cache_write` written with the one-hour lifetime.
    pub cache_write_1h: u64,
    /// Thinking tokens, a subset of `output`, when reported.
    pub reasoning: Option<u64>,
}

impl AnthropicUsage {
    /// Provider-neutral usage. Input is the full prompt (uncached plus cache
    /// reads and writes); no monetary value is reported by Anthropic.
    pub fn to_usage(self) -> Usage {
        let input = self.input + self.cache_read + self.cache_write;
        Usage {
            total_tokens: Some(input + self.output),
            input_tokens: Some(input),
            output_tokens: Some(self.output),
            reasoning_tokens: self.reasoning,
            cache_read_tokens: Some(self.cache_read),
            cache_write_tokens: Some(self.cache_write),
            cost: None,
        }
    }
}

/// A failure that ends the stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StreamFailure {
    pub(super) message: String,
    pub(super) error_type: Option<String>,
}

#[derive(Debug)]
enum Block {
    Text,
    Thinking { signature: String },
    Redacted,
    ToolUse {
        id: String,
        name: String,
        initial: JsonValue,
        partial: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Outcome {
    Stop(StopReason),
    Failed(String),
}

/// One response's reduction state.
#[derive(Debug, Default)]
pub(super) struct StreamReducer {
    blocks: BTreeMap<u64, Block>,
    emitted_content: bool,
    pub(super) usage: AnthropicUsage,
    outcome: Option<Outcome>,
    pub(super) raw_stop_reason: Option<String>,
    started: bool,
    stopped: bool,
    pub(super) response_model: Option<String>,
    pub(super) response_id: Option<String>,
    pub(super) input_transformations: Vec<JsonValue>,
    managed_effort: Option<&'static str>,
}

impl StreamReducer {
    pub(super) fn new(managed_effort: Option<&'static str>) -> Self {
        Self {
            managed_effort,
            ..Self::default()
        }
    }

    /// Reduce one server-sent event.
    pub(super) fn push(
        &mut self,
        sse: ServerSentEvent,
    ) -> Result<Vec<ModelStreamEvent>, StreamFailure> {
        let event_name = sse.event.as_deref().unwrap_or("");
        if event_name == "error" {
            return Err(stream_error(&sse.data));
        }
        if self.stopped || !MESSAGE_EVENTS.contains(&event_name) {
            return Ok(Vec::new());
        }
        let event = parse_json_with_repair(&sse.data).ok_or_else(|| StreamFailure {
            message: format!(
                "Could not parse Anthropic SSE event {event_name}: invalid JSON data"
            ),
            error_type: None,
        })?;
        let mut output = Vec::new();
        match event.get("type").and_then(JsonValue::as_str).unwrap_or(event_name) {
            "message_start" => {
                self.started = true;
                let message = event.get("message");
                self.response_id = message
                    .and_then(|message| message.get("id"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                self.response_model = message
                    .and_then(|message| message.get("model"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                self.record_transformations(message.and_then(|message| {
                    message.get("input_transformations")
                }));
                if let Some(usage) = message.and_then(|message| message.get("usage")) {
                    self.usage.input = count(usage, "input_tokens").unwrap_or(0);
                    self.usage.output = count(usage, "output_tokens").unwrap_or(0);
                    self.usage.cache_read = count(usage, "cache_read_input_tokens").unwrap_or(0);
                    self.usage.cache_write =
                        count(usage, "cache_creation_input_tokens").unwrap_or(0);
                    self.usage.cache_write_1h = usage
                        .get("cache_creation")
                        .and_then(|creation| count(creation, "ephemeral_1h_input_tokens"))
                        .unwrap_or(0);
                }
            }
            "content_block_start" => {
                let index = count(&event, "index").unwrap_or(0);
                let Some(block) = event.get("content_block") else {
                    return Ok(output);
                };
                match block.get("type").and_then(JsonValue::as_str) {
                    Some("fallback") => {
                        if self.emitted_content {
                            return Err(StreamFailure {
                                message: "Anthropic performed an unsupported mid-output model fallback"
                                    .into(),
                                error_type: None,
                            });
                        }
                    }
                    Some("text") => {
                        self.blocks.insert(index, Block::Text);
                        let text = string(block, "text");
                        if !text.is_empty() {
                            self.emitted_content = true;
                            output.push(ModelStreamEvent::TextDelta(text));
                        }
                    }
                    Some("thinking") => {
                        self.blocks.insert(
                            index,
                            Block::Thinking {
                                signature: string(block, "signature"),
                            },
                        );
                        let thinking = string(block, "thinking");
                        if !thinking.is_empty() {
                            self.emitted_content = true;
                            output.push(ModelStreamEvent::ThinkingDelta(thinking));
                        }
                    }
                    Some("redacted_thinking") => {
                        self.blocks.insert(index, Block::Redacted);
                        let data = string(block, "data");
                        if !data.is_empty() {
                            self.emitted_content = true;
                            output.push(ModelStreamEvent::RedactedThinking(opaque(
                                REDACTED_THINKING_CONTEXT_KIND,
                                data,
                            )?));
                        }
                    }
                    Some("tool_use") => {
                        self.blocks.insert(
                            index,
                            Block::ToolUse {
                                id: string(block, "id"),
                                name: string(block, "name"),
                                initial: block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or_else(|| JsonValue::object::<_, String>([])),
                                partial: String::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = count(&event, "index").unwrap_or(0);
                let Some(delta) = event.get("delta") else {
                    return Ok(output);
                };
                let block = self.blocks.get_mut(&index);
                match (delta.get("type").and_then(JsonValue::as_str), block) {
                    (Some("text_delta"), Some(Block::Text)) => {
                        let text = string(delta, "text");
                        if !text.is_empty() {
                            self.emitted_content = true;
                            output.push(ModelStreamEvent::TextDelta(text));
                        }
                    }
                    (Some("thinking_delta"), Some(Block::Thinking { .. })) => {
                        let thinking = string(delta, "thinking");
                        if !thinking.is_empty() {
                            self.emitted_content = true;
                            output.push(ModelStreamEvent::ThinkingDelta(thinking));
                        }
                    }
                    (Some("signature_delta"), Some(Block::Thinking { signature })) => {
                        signature.push_str(&string(delta, "signature"));
                    }
                    (Some("input_json_delta"), Some(Block::ToolUse { partial, .. })) => {
                        partial.push_str(&string(delta, "partial_json"));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = count(&event, "index").unwrap_or(0);
                match self.blocks.remove(&index) {
                    Some(Block::Thinking { signature }) if !signature.is_empty() => {
                        self.emitted_content = true;
                        output.push(ModelStreamEvent::ThinkingSignature(opaque(
                            SIGNATURE_CONTEXT_KIND,
                            signature,
                        )?));
                    }
                    Some(Block::ToolUse {
                        id,
                        name,
                        initial,
                        partial,
                    }) => {
                        let arguments = tool_arguments(&initial, &partial);
                        let id = ToolCallId::new(id).map_err(|_| StreamFailure {
                            message: "Anthropic tool use has no identifier".into(),
                            error_type: None,
                        })?;
                        self.emitted_content = true;
                        output.push(ModelStreamEvent::ToolCall(AgentToolCall {
                            id,
                            name,
                            arguments: SerializedJson::new(arguments),
                        }));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                self.record_transformations(event.get("input_transformations"));
                if let Some(delta) = event.get("delta")
                    && let Some(reason) = delta.get("stop_reason").and_then(JsonValue::as_str)
                {
                    self.raw_stop_reason = Some(reason.to_owned());
                    self.outcome = Some(map_stop_reason(reason, delta.get("stop_details")));
                }
                // Only present fields update usage, preserving message_start
                // input when a proxy omits it here.
                if let Some(usage) = event.get("usage") {
                    if let Some(value) = count(usage, "input_tokens") {
                        self.usage.input = value;
                    }
                    if let Some(value) = count(usage, "output_tokens") {
                        self.usage.output = value;
                    }
                    if let Some(value) = count(usage, "cache_read_input_tokens") {
                        self.usage.cache_read = value;
                    }
                    if let Some(value) = count(usage, "cache_creation_input_tokens") {
                        self.usage.cache_write = value;
                    }
                    if let Some(value) = usage
                        .get("cache_creation")
                        .and_then(|creation| count(creation, "ephemeral_1h_input_tokens"))
                    {
                        self.usage.cache_write_1h = value;
                    }
                    if let Some(value) = usage
                        .get("output_tokens_details")
                        .and_then(|details| count(details, "thinking_tokens"))
                    {
                        self.usage.reasoning = Some(value);
                    }
                }
            }
            "message_stop" => {
                self.stopped = true;
            }
            _ => {}
        }
        Ok(output)
    }

    /// Settle at end of body.
    pub(super) fn finish(&mut self) -> Result<Vec<ModelStreamEvent>, StreamFailure> {
        if self.started && !self.stopped {
            return Err(StreamFailure {
                message: "Anthropic stream ended before message_stop".into(),
                error_type: None,
            });
        }
        let outcome = self.outcome.clone().ok_or_else(|| StreamFailure {
            message: "Anthropic stream ended without a stop reason".into(),
            error_type: None,
        })?;
        let mut output = vec![ModelStreamEvent::Usage(self.usage.to_usage())];
        if let Some(effort) = self.managed_effort {
            output.push(ModelStreamEvent::OpaqueProviderContext(opaque(
                EFFORT_CONTEXT_KIND,
                effort.into(),
            )?));
        }
        output.push(match outcome {
            Outcome::Stop(reason) => ModelStreamEvent::End(reason),
            Outcome::Failed(message) => ModelStreamEvent::Error { message },
        });
        Ok(output)
    }

    fn record_transformations(&mut self, value: Option<&JsonValue>) {
        if let Some(transformations) = value.and_then(JsonValue::as_array) {
            self.input_transformations = transformations.to_vec();
        }
    }
}

/// Pi's `mapStopReason`.
fn map_stop_reason(reason: &str, details: Option<&JsonValue>) -> Outcome {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Outcome::Stop(StopReason::Stop),
        "max_tokens" => Outcome::Stop(StopReason::Length),
        "tool_use" => Outcome::Stop(StopReason::ToolUse),
        "refusal" => Outcome::Failed(
            details
                .and_then(|details| details.get("explanation"))
                .and_then(JsonValue::as_str)
                .filter(|explanation| !explanation.is_empty())
                .unwrap_or("The model refused to complete the request")
                .to_owned(),
        ),
        "sensitive" => Outcome::Failed("Provider stopped with: sensitive".into()),
        other => Outcome::Failed(format!("Unhandled stop reason: {other}")),
    }
}

/// Tool arguments: the streamed JSON when any arrived, else the start input.
/// Malformed streamed JSON is repaired as in Pi; an unrepairable value is
/// passed through so the core refuses it as invalid arguments.
fn tool_arguments(initial: &JsonValue, partial: &str) -> String {
    if partial.trim().is_empty() {
        return initial
            .to_json_string()
            .unwrap_or_else(|_| "{}".to_owned());
    }
    match parse_json_with_repair(partial) {
        Some(value) => value.to_json_string().unwrap_or_else(|_| partial.to_owned()),
        None => partial.to_owned(),
    }
}

/// A mid-stream `error` event. Its message is bounded diagnostic text.
pub(super) fn stream_error(data: &str) -> StreamFailure {
    let parsed = JsonValue::parse(data).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    let error_type = error
        .and_then(|error| error.get("type"))
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| bounded(data, 512));
    StreamFailure {
        message: match &error_type {
            Some(error_type) => format!("Anthropic stream error ({error_type}): {message}"),
            None => format!("Anthropic stream error: {message}"),
        },
        error_type,
    }
}

fn opaque(kind: &str, payload: String) -> Result<OpaqueProviderContextItem, StreamFailure> {
    OpaqueProviderContextItem::new(PROVIDER_ID, kind, None, payload).map_err(|error| {
        StreamFailure {
            message: format!("Anthropic replay material is invalid: {error}"),
            error_type: None,
        }
    })
}

fn count(value: &JsonValue, field: &str) -> Option<u64> {
    value.get(field).and_then(JsonValue::as_u64)
}

fn string(value: &JsonValue, field: &str) -> String {
    value
        .get(field)
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(super) fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
