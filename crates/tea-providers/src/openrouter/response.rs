//! OpenRouter response parsing and exact decimal accounting.
//!
//! The finite parser below is retained for recorded-response regression fixtures while the
//! production adapter uses `StreamingSseDecoder` directly.

#![allow(dead_code)]

use super::accounting::{OpenRouterCostSource, OpenRouterCostTurn};
use crate::json::{JsonValue, from_bytes};
use crate::scheduler::ModelStreamEvent;
use crate::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId, Usage};
use std::collections::VecDeque;

pub(super) struct ParsedResponse {
    pub(super) events: Vec<ModelStreamEvent>,
    pub(super) usage: Usage,
    pub(super) generation_id: Option<String>,
    pub(super) inline_cost: Option<OpenRouterCostTurn>,
}

fn finite_nonnegative(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value >= 0.0)
}

fn number(value: &JsonValue, key: &str) -> Option<f64> {
    finite_nonnegative(value.get(key).and_then(JsonValue::as_f64))
}

/// Extract one provider number without routing it through `f64`.
///
/// `JsonValue` intentionally models JSON floating-point numbers as `f64`, which is a useful
/// generic protocol boundary but cannot preserve a billing decimal's source spelling. This
/// narrow path scanner runs only after the response has passed normal JSON parsing and is used
/// solely for the redacted cost fields.
pub(super) fn exact_number_at_path(input: &[u8], path: &[&str]) -> Option<String> {
    let mut cursor = RawJsonCursor { input, position: 0 };
    let value = cursor.value_at_path(path)?;
    if !valid_nonnegative_json_number(&value) {
        return None;
    }
    Some(value)
}

fn valid_nonnegative_json_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes[0] == b'-' {
        return false;
    }
    let mut index = 0;
    if bytes[index] == b'0' {
        index += 1;
    } else if bytes[index].is_ascii_digit() {
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
    } else {
        return false;
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction_start {
            return false;
        }
    }
    if bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b'e' | b'E'))
    {
        index += 1;
        if bytes
            .get(index)
            .is_some_and(|byte| matches!(byte, b'+' | b'-'))
        {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

struct RawJsonCursor<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> RawJsonCursor<'a> {
    fn value_at_path(&mut self, path: &[&str]) -> Option<String> {
        self.skip_space();
        if path.is_empty() {
            return self.number();
        }
        if self.input.get(self.position) != Some(&b'{') {
            return None;
        }
        self.position += 1;
        self.skip_space();
        if self.input.get(self.position) == Some(&b'}') {
            return None;
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            self.skip_space();
            if self.input.get(self.position) != Some(&b':') {
                return None;
            }
            self.position += 1;
            self.skip_space();
            if key == path[0] {
                return self.value_at_path(&path[1..]);
            }
            self.skip_value()?;
            self.skip_space();
            match self.input.get(self.position) {
                Some(b',') => self.position += 1,
                Some(b'}') | None => return None,
                _ => return None,
            }
        }
    }

    fn skip_value(&mut self) -> Option<()> {
        self.skip_space();
        match self.input.get(self.position).copied()? {
            b'{' => {
                self.position += 1;
                self.skip_space();
                if self.input.get(self.position) == Some(&b'}') {
                    self.position += 1;
                    return Some(());
                }
                loop {
                    self.skip_space();
                    self.string()?;
                    self.skip_space();
                    if self.input.get(self.position) != Some(&b':') {
                        return None;
                    }
                    self.position += 1;
                    self.skip_value()?;
                    self.skip_space();
                    match self.input.get(self.position) {
                        Some(b',') => self.position += 1,
                        Some(b'}') => {
                            self.position += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.position += 1;
                self.skip_space();
                if self.input.get(self.position) == Some(&b']') {
                    self.position += 1;
                    return Some(());
                }
                loop {
                    self.skip_value()?;
                    self.skip_space();
                    match self.input.get(self.position) {
                        Some(b',') => self.position += 1,
                        Some(b']') => {
                            self.position += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => {
                self.string()?;
                Some(())
            }
            b'-' | b'0'..=b'9' => {
                self.number()?;
                Some(())
            }
            _ if self.input[self.position..].starts_with(b"true") => {
                self.position += 4;
                Some(())
            }
            _ if self.input[self.position..].starts_with(b"false") => {
                self.position += 5;
                Some(())
            }
            _ if self.input[self.position..].starts_with(b"null") => {
                self.position += 4;
                Some(())
            }
            _ => None,
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.input.get(self.position) != Some(&b'"') {
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut escaped = false;
        while let Some(byte) = self.input.get(self.position).copied() {
            self.position += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                return std::str::from_utf8(&self.input[start..self.position - 1])
                    .ok()
                    .map(ToOwned::to_owned);
            }
        }
        None
    }

    fn number(&mut self) -> Option<String> {
        let start = self.position;
        while let Some(byte) = self.input.get(self.position).copied() {
            if byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.position += 1;
            } else {
                break;
            }
        }
        if start == self.position {
            return None;
        }
        std::str::from_utf8(&self.input[start..self.position])
            .ok()
            .map(ToOwned::to_owned)
    }

    fn skip_space(&mut self) {
        while self
            .input
            .get(self.position)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.position += 1;
        }
    }
}

pub(super) fn decimal_add(lhs: Option<&str>, rhs: &str) -> String {
    let Some(lhs) = lhs else {
        return decimal_normalize(rhs);
    };
    let (left_digits, left_scale) = decimal_parts(lhs);
    let (right_digits, right_scale) = decimal_parts(rhs);
    let scale = left_scale.max(right_scale);
    let mut left = left_digits;
    let mut right = right_digits;
    left.extend(std::iter::repeat_n('0', scale - left_scale));
    right.extend(std::iter::repeat_n('0', scale - right_scale));
    let mut output = String::new();
    let mut carry = 0u8;
    for (left, right) in left.bytes().rev().zip(right.bytes().rev()) {
        let sum = left - b'0' + right - b'0' + carry;
        output.push(char::from(b'0' + sum % 10));
        carry = sum / 10;
    }
    if carry != 0 {
        output.push(char::from(b'0' + carry));
    }
    let mut output: String = output.chars().rev().collect();
    if scale != 0 {
        if output.len() <= scale {
            let zeros = "0".repeat(scale + 1 - output.len());
            output = format!("{zeros}{output}");
        }
        let position = output.len() - scale;
        output.insert(position, '.');
    }
    decimal_normalize(&output)
}

fn decimal_parts(value: &str) -> (String, usize) {
    let (coefficient, exponent) = value
        .split_once(['e', 'E'])
        .map(|(coefficient, exponent)| (coefficient, exponent.parse::<i64>().unwrap_or(0)))
        .unwrap_or((value, 0));
    let (whole, fraction) = coefficient.split_once('.').unwrap_or((coefficient, ""));
    let mut digits = String::with_capacity(whole.len() + fraction.len());
    digits.push_str(whole.trim_start_matches('+'));
    digits.push_str(fraction);
    let scale = (fraction.len() as i64 - exponent).max(0) as usize;
    let mut digits = digits.trim_start_matches('0').to_owned();
    if digits.is_empty() {
        digits.push('0');
    }
    (digits, scale)
}

fn decimal_normalize(value: &str) -> String {
    let (digits, scale) = decimal_parts(value);
    if scale == 0 {
        return digits;
    }
    let mut output = if digits.len() <= scale {
        format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
    } else {
        let position = digits.len() - scale;
        format!("{}.{}", &digits[..position], &digits[position..])
    };
    while output.ends_with('0') {
        output.pop();
    }
    if output.ends_with('.') {
        output.pop();
    }
    output
}

pub(super) fn unavailable_cost(usage: &Usage, model: &str) -> OpenRouterCostTurn {
    OpenRouterCostTurn {
        turn: 0,
        source: OpenRouterCostSource::Unavailable,
        total_usd: None,
        total_usd_exact: None,
        upstream_inference_usd: None,
        upstream_inference_usd_exact: None,
        model: Some(model.to_owned()),
        provider: None,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: usage.reasoning_tokens,
    }
}

pub(super) fn parse_response(bytes: &[u8]) -> Result<ParsedResponse, String> {
    parse_response_inner(bytes, false)
}

pub(super) fn parse_partial_response(bytes: &[u8]) -> Result<ParsedResponse, String> {
    parse_response_inner(bytes, true)
}

fn parse_response_inner(bytes: &[u8], allow_partial_sse: bool) -> Result<ParsedResponse, String> {
    let trimmed = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|start| &bytes[start..])
        .unwrap_or_default();
    if trimmed.starts_with(b"data:") || trimmed.starts_with(b":") {
        return parse_sse_response(bytes, allow_partial_sse);
    }
    let response =
        from_bytes(bytes).map_err(|_| "OpenRouter returned a non-JSON response".to_owned())?;
    if response.get("error").is_some() {
        return Err("OpenRouter rejected the request".into());
    }
    let choice = response
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "OpenRouter response did not contain a completion choice".to_owned())?;
    let message = choice
        .get("message")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| "OpenRouter completion choice did not contain a message".to_owned())?;
    let mut events = Vec::new();
    if let Some(content) = message.get("content").and_then(JsonValue::as_str)
        && !content.is_empty()
    {
        events.push(ModelStreamEvent::TextDelta(content.to_owned()));
    }
    let mut has_tool_calls = false;
    if let Some(calls) = message.get("tool_calls").and_then(JsonValue::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let id = call
                .get("id")
                .and_then(JsonValue::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("openrouter-call-{index}"));
            let function = call
                .get("function")
                .and_then(JsonValue::as_object)
                .ok_or_else(|| "OpenRouter tool call did not contain a function".to_owned())?;
            let name = function
                .get("name")
                .and_then(JsonValue::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "OpenRouter tool call did not contain a name".to_owned())?;
            let arguments = function
                .get("arguments")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    "OpenRouter tool call did not contain serialized arguments".to_owned()
                })?;
            events.push(ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new(id)
                    .map_err(|_| "OpenRouter tool call omitted its identifier".to_owned())?,
                name: name.to_owned(),
                arguments: SerializedJson::new(arguments),
            }));
            has_tool_calls = true;
        }
    }
    let stop_reason = match choice.get("finish_reason").and_then(JsonValue::as_str) {
        Some("tool_calls" | "tool_call") if has_tool_calls => StopReason::ToolUse,
        Some("length") => StopReason::Length,
        _ if has_tool_calls => StopReason::ToolUse,
        _ => StopReason::Stop,
    };
    events.push(ModelStreamEvent::End(stop_reason));
    let usage = response.get("usage").cloned().unwrap_or(JsonValue::Null);
    let parsed_usage = parse_usage(&usage);
    let total_usd_exact = exact_number_at_path(bytes, &["usage", "cost"]);
    let inline_cost = total_usd_exact.map(|total_usd_exact| OpenRouterCostTurn {
        turn: 0,
        source: OpenRouterCostSource::ChatUsage,
        total_usd: number(&usage, "cost"),
        total_usd_exact: Some(total_usd_exact),
        upstream_inference_usd: None,
        upstream_inference_usd_exact: None,
        model: response
            .get("model")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        provider: None,
        input_tokens: parsed_usage.input_tokens,
        output_tokens: parsed_usage.output_tokens,
        cache_read_tokens: parsed_usage.cache_read_tokens,
        cache_write_tokens: parsed_usage.cache_write_tokens,
        reasoning_tokens: parsed_usage.reasoning_tokens,
    });
    Ok(ParsedResponse {
        events,
        usage: parsed_usage,
        generation_id: response
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        inline_cost,
    })
}

fn parse_usage(usage: &JsonValue) -> Usage {
    let token = |name: &str| usage.get(name).and_then(JsonValue::as_u64);
    let reasoning = usage
        .get("completion_tokens_details")
        .and_then(JsonValue::as_object)
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(JsonValue::as_u64);
    Usage {
        total_tokens: token("total_tokens"),
        input_tokens: token("prompt_tokens"),
        output_tokens: token("completion_tokens"),
        reasoning_tokens: reasoning,
        cache_read_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(JsonValue::as_u64),
        cache_write_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cache_write_tokens"))
            .and_then(JsonValue::as_u64),
        cost: None,
    }
}

#[derive(Default)]
struct StreamingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn parse_sse_response(bytes: &[u8], allow_partial: bool) -> Result<ParsedResponse, String> {
    let mut decoder = StreamingSseDecoder::new();
    let mut events = decoder.push(bytes)?;
    let complete = decoder.finish(allow_partial)?;
    events.extend(complete.events);
    Ok(ParsedResponse {
        events,
        usage: complete.usage,
        generation_id: complete.generation_id,
        inline_cost: complete.inline_cost,
    })
}

/// Stateful OpenRouter SSE parser shared by recorded responses and the native body stream.
///
/// Text deltas are emitted as their SSE records arrive. Tool-call fragments remain internal
/// until their terminal record so the core sees one complete typed call in provider order.
pub(super) struct StreamingSseDecoder {
    buffered: Vec<u8>,
    tool_calls: Vec<Option<StreamingToolCall>>,
    finish_reason: Option<String>,
    usage: JsonValue,
    usage_bytes: Option<Vec<u8>>,
    generation_id: Option<String>,
    model: Option<String>,
    saw_data: bool,
    saw_done: bool,
}

pub(super) struct StreamingSseComplete {
    pub(super) events: Vec<ModelStreamEvent>,
    pub(super) usage: Usage,
    pub(super) generation_id: Option<String>,
    pub(super) inline_cost: Option<OpenRouterCostTurn>,
}

impl StreamingSseDecoder {
    pub(super) fn new() -> Self {
        Self {
            buffered: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: JsonValue::Null,
            usage_bytes: None,
            generation_id: None,
            model: None,
            saw_data: false,
            saw_done: false,
        }
    }

    /// Reduce complete SSE records from one arbitrary HTTP body chunk.
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelStreamEvent>, String> {
        self.buffered.extend_from_slice(bytes);
        let mut events = VecDeque::new();
        while let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffered.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            self.process_line(&line, &mut events)?;
        }
        Ok(events.into())
    }

    /// Finish a closed body. Partial capture is accepted only for the existing stall boundary.
    pub(super) fn finish(mut self, allow_partial: bool) -> Result<StreamingSseComplete, String> {
        let mut events = VecDeque::new();
        if !self.buffered.is_empty() {
            let line = std::mem::take(&mut self.buffered);
            if let Err(error) = self.process_line(&line, &mut events)
                && !allow_partial
            {
                return Err(error);
            }
        }
        if !self.saw_data || (!allow_partial && !self.saw_done && self.finish_reason.is_none()) {
            return Err("OpenRouter SSE response ended before completion".to_owned());
        }
        if allow_partial && !self.saw_done && self.finish_reason.is_none() {
            self.finish_reason = Some("length".to_owned());
        }
        let mut has_tool_calls = false;
        for (index, call) in self.tool_calls.into_iter().flatten().enumerate() {
            let id = call
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("openrouter-call-{index}"));
            let name = call
                .name
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "OpenRouter tool call did not contain a name".to_owned())?;
            events.push_back(ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new(id)
                    .map_err(|_| "OpenRouter tool call omitted its identifier".to_owned())?,
                name,
                arguments: SerializedJson::new(&call.arguments),
            }));
            has_tool_calls = true;
        }
        let usage = parse_usage(&self.usage);
        let inline_cost = self.usage_bytes.as_deref().and_then(|usage_bytes| {
            let total_usd_exact = exact_number_at_path(usage_bytes, &["usage", "cost"])?;
            Some(OpenRouterCostTurn {
                turn: 0,
                source: OpenRouterCostSource::ChatUsage,
                total_usd: number(&self.usage, "cost"),
                total_usd_exact: Some(total_usd_exact),
                upstream_inference_usd: None,
                upstream_inference_usd_exact: None,
                model: self.model,
                provider: None,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            })
        });
        if usage.is_reported() || inline_cost.is_some() {
            events.push_back(ModelStreamEvent::Usage(usage.clone()));
        }
        let stop_reason = match self.finish_reason.as_deref() {
            Some("length") => StopReason::Length,
            Some("tool_calls" | "tool_call") if has_tool_calls => StopReason::ToolUse,
            _ if has_tool_calls => StopReason::ToolUse,
            _ => StopReason::Stop,
        };
        events.push_back(ModelStreamEvent::End(stop_reason));
        Ok(StreamingSseComplete {
            events: events.into(),
            usage,
            generation_id: self.generation_id,
            inline_cost,
        })
    }

    fn process_line(
        &mut self,
        line: &[u8],
        events: &mut VecDeque<ModelStreamEvent>,
    ) -> Result<(), String> {
        let line = std::str::from_utf8(line)
            .map_err(|_| "OpenRouter returned a non-UTF-8 SSE response".to_owned())?
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
        let chunk = from_bytes(data.as_bytes())
            .map_err(|_| "OpenRouter returned an invalid SSE event".to_owned())?;
        if chunk.get("error").is_some() {
            return Err(if openrouter_context_overflow(data.as_bytes()) {
                "OpenRouter context capacity exceeded".into()
            } else {
                "OpenRouter rejected the request".into()
            });
        }
        if self.generation_id.is_none() {
            self.generation_id = chunk
                .get("id")
                .and_then(JsonValue::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
        }
        if self.model.is_none() {
            self.model = chunk
                .get("model")
                .and_then(JsonValue::as_str)
                .filter(|model| !model.is_empty())
                .map(str::to_owned);
        }
        if let Some(usage) = chunk.get("usage") {
            self.usage = usage.clone();
            self.usage_bytes = Some(data.as_bytes().to_owned());
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
        let Some(delta) = choice.get("delta").and_then(JsonValue::as_object) else {
            return Ok(());
        };
        if let Some(content) = delta.get("content").and_then(JsonValue::as_str)
            && !content.is_empty()
        {
            events.push_back(ModelStreamEvent::TextDelta(content.to_owned()));
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

/// Classify an OpenRouter JSON error without exposing its remote diagnostic to the agent.
/// OpenRouter places the HTTP status in the error object's numeric `code` field for the common
/// rate-limit and transient-service failures.
pub(super) fn openrouter_response_retryable(bytes: &[u8]) -> bool {
    let Some(error) = from_bytes(bytes)
        .ok()
        .and_then(|response| response.get("error").cloned())
        .and_then(|error| error.as_object().cloned())
    else {
        return false;
    };
    let status = error
        .get("code")
        .and_then(JsonValue::as_u64)
        .or_else(|| error.get("status").and_then(JsonValue::as_u64));
    matches!(status, Some(429) | Some(500..=599))
}

/// Identify the context-capacity failures OpenRouter returns in its JSON error envelope.
///
/// This classification belongs to the provider adapter: the generic core only reacts to the
/// typed `ContextOverflow` event and never guesses from a remote diagnostic string.
pub(super) fn openrouter_context_overflow(bytes: &[u8]) -> bool {
    let Some(message) = from_bytes(bytes)
        .ok()
        .and_then(|response| response.get("error").cloned())
        .and_then(|error| {
            error
                .get("message")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        })
    else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    let overflow = message.contains("maximum")
        || message.contains("exceed")
        || message.contains("too long")
        || message.contains("over limit")
        || message.contains("limit reached");
    // Poolside through OpenRouter reports a precise input-bound diagnostic
    // without mentioning "context". Keep this adapter-specific wording here
    // instead of teaching the provider-neutral core to inspect remote errors.
    let poolside_input_limit = message.contains("input length")
        && message.contains("maximum allowed input length")
        && message.contains("token");
    (overflow
        && message.contains("context")
        && (message.contains("length")
            || message.contains("limit")
            || message.contains("window")
            || message.contains("token")
            || message.contains("capacity")))
        || poolside_input_limit
        || message.contains("too many tokens")
        || message.contains("prompt is too long")
}

pub(super) fn openrouter_status_retryable(status: Option<u16>) -> bool {
    matches!(status, Some(429) | Some(500..=599))
}

pub(super) fn response_body_prefix(bytes: &[u8], secret: Option<&str>) -> String {
    const MAX_BYTES: usize = 512;
    let bounded = &bytes[..bytes.len().min(MAX_BYTES)];
    let mut value = String::from_utf8_lossy(bounded).into_owned();
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        value = value.replace(secret, "[redacted]");
    }
    let value = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let value = value.trim();
    if value.is_empty() {
        "<empty>".to_owned()
    } else {
        value.to_owned()
    }
}

pub(super) fn parse_generation_cost(
    bytes: &[u8],
    fallback_usage: &Usage,
) -> Option<OpenRouterCostTurn> {
    let response = from_bytes(bytes).ok()?;
    let data = response.get("data")?;
    let total_usd_exact = exact_number_at_path(bytes, &["data", "total_cost"])?;
    Some(OpenRouterCostTurn {
        turn: 0,
        source: OpenRouterCostSource::Generation,
        total_usd: number(data, "total_cost"),
        total_usd_exact: Some(total_usd_exact),
        upstream_inference_usd: number(data, "upstream_inference_cost"),
        upstream_inference_usd_exact: exact_number_at_path(
            bytes,
            &["data", "upstream_inference_cost"],
        ),
        model: data
            .get("model")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        provider: data
            .get("provider_name")
            .and_then(JsonValue::as_str)
            .map(str::to_owned),
        input_tokens: data
            .get("tokens_prompt")
            .and_then(JsonValue::as_u64)
            .or(fallback_usage.input_tokens),
        output_tokens: data
            .get("tokens_completion")
            .and_then(JsonValue::as_u64)
            .or(fallback_usage.output_tokens),
        cache_read_tokens: data.get("tokens_cached").and_then(JsonValue::as_u64),
        cache_write_tokens: data.get("tokens_cache_write").and_then(JsonValue::as_u64),
        reasoning_tokens: data
            .get("tokens_reasoning")
            .and_then(JsonValue::as_u64)
            .or(fallback_usage.reasoning_tokens),
    })
}
