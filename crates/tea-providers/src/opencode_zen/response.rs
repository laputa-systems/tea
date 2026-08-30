//! OpenCode Zen Responses SSE parsing.

use crate::json::{JsonValue, from_bytes};
use crate::scheduler::ModelStreamEvent;
use crate::state::{AgentToolCall, SerializedJson, StopReason, ToolCallId, Usage};
use std::collections::HashMap;

#[derive(Default, Debug, Clone)]
struct PendingTool {
    id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

pub(super) struct OpencodeZenSseDecoder {
    buffered: Vec<u8>,
    tool_calls: HashMap<String, PendingTool>, // keyed by item_id (fc_...)
    pending_tool_order: Vec<String>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    generation_id: Option<String>,
    model: Option<String>,
    saw_data: bool,
    saw_done: bool,
}

pub(super) struct OpencodeZenComplete {
    pub(super) events: Vec<ModelStreamEvent>,
    pub(super) usage: Usage,
    #[allow(dead_code)]
    pub(super) generation_id: Option<String>,
}

impl OpencodeZenSseDecoder {
    pub(super) fn new() -> Self {
        Self {
            buffered: Vec::new(),
            tool_calls: HashMap::new(),
            pending_tool_order: Vec::new(),
            finish_reason: None,
            usage: None,
            generation_id: None,
            model: None,
            saw_data: false,
            saw_done: false,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<ModelStreamEvent>, String> {
        self.buffered.extend_from_slice(bytes);
        let mut events = Vec::new();
        // Process complete SSE records delimited by \n\n or \r\n\r\n
        loop {
            let delim = self.find_delim();
            let Some((pos, len)) = delim else { break };
            let record = self.buffered.drain(..pos + len).collect::<Vec<_>>();
            // record includes delim; strip it
            let record = &record[..record.len() - len];
            if record.is_empty() {
                continue;
            }
            let text = std::str::from_utf8(record)
                .map_err(|_| "OpenCode Zen returned non-UTF8 SSE".to_owned())?;
            // Each record may be : comment, event: ..., data: ...
            let mut event_type: Option<String> = None;
            let mut data_payload: Option<String> = None;
            for line in text.split('\n') {
                let line = line.trim_end_matches('\r');
                if line.is_empty() || line.starts_with(':') {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("event:") {
                    event_type = Some(rest.trim().to_owned());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    let trimmed = rest.trim();
                    // data may be [DONE] sentinel
                    data_payload = Some(trimmed.to_owned());
                }
            }
            let Some(data) = data_payload else {
                continue;
            };
            if data == "[DONE]" {
                self.saw_done = true;
                continue;
            }
            if data.is_empty() {
                continue;
            }
            self.saw_data = true;
            // Try parse as JSON; if fails, it may be ping or cost event
            let json: JsonValue = match from_bytes(data.as_bytes()) {
                Ok(v) => v,
                Err(_) => {
                    // Might be non-JSON data like cost ping: ignore
                    continue;
                }
            };
            if json.get("error").is_some() {
                // Provider error envelope
                // Check for context overflow classification later; for now treat as error
                return Err("OpenCode Zen rejected the request".into());
            }
            // Handle by event type
            match event_type.as_deref() {
                Some("response.output_text.delta") => {
                    if let Some(delta) = json.get("delta").and_then(JsonValue::as_str) {
                        if !delta.is_empty() {
                            events.push(ModelStreamEvent::TextDelta(delta.to_owned()));
                        }
                    } else if let Some(delta) = json.get("text").and_then(JsonValue::as_str)
                        && !delta.is_empty()
                    {
                        events.push(ModelStreamEvent::TextDelta(delta.to_owned()));
                    }
                }
                Some("response.output_item.added") => {
                    if let Some(item) = json.get("item").and_then(JsonValue::as_object) {
                        let typ = item.get("type").and_then(JsonValue::as_str).unwrap_or("");
                        if typ == "function_call" {
                            let item_id = item
                                .get("id")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let name = item
                                .get("name")
                                .and_then(JsonValue::as_str)
                                .map(|s| s.to_owned());
                            let call_id = item
                                .get("call_id")
                                .and_then(JsonValue::as_str)
                                .map(|s| s.to_owned());
                            let args = item
                                .get("arguments")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("")
                                .to_owned();
                            if !item_id.is_empty() {
                                let entry = self.tool_calls.entry(item_id.clone()).or_default();
                                entry.id = Some(item_id.clone());
                                if name.is_some() {
                                    entry.name = name;
                                }
                                if call_id.is_some() {
                                    entry.call_id = call_id;
                                }
                                entry.arguments = args;
                                if !self.pending_tool_order.contains(&item_id) {
                                    self.pending_tool_order.push(item_id);
                                }
                            }
                        } else if typ == "reasoning" {
                            // ignore encrypted reasoning
                        }
                    }
                }
                Some("response.function_call_arguments.delta") => {
                    if let Some(delta) = json.get("delta").and_then(JsonValue::as_str) {
                        if let Some(item_id) = json.get("item_id").and_then(JsonValue::as_str) {
                            if let Some(entry) = self.tool_calls.get_mut(item_id) {
                                entry.arguments.push_str(delta);
                            } else {
                                // Might be without prior added? create entry
                                let entry = self.tool_calls.entry(item_id.to_owned()).or_default();
                                entry.id = Some(item_id.to_owned());
                                entry.arguments.push_str(delta);
                                if !self.pending_tool_order.contains(&item_id.to_owned()) {
                                    self.pending_tool_order.push(item_id.to_owned());
                                }
                            }
                        }
                    } else if let Some(delta) =
                        json.get("arguments_delta").and_then(JsonValue::as_str)
                    {
                        // fallback alternative field name
                        if let Some(item_id) = json.get("item_id").and_then(JsonValue::as_str)
                            && let Some(entry) = self.tool_calls.get_mut(item_id)
                        {
                            entry.arguments.push_str(delta);
                        }
                    }
                }
                Some("response.function_call_arguments.done") => {
                    if let Some(args) = json.get("arguments").and_then(JsonValue::as_str)
                        && let Some(item_id) = json.get("item_id").and_then(JsonValue::as_str)
                        && let Some(entry) = self.tool_calls.get_mut(item_id)
                    {
                        entry.arguments = args.to_owned();
                    }
                    if let Some(name) = json.get("name").and_then(JsonValue::as_str)
                        && let Some(item_id) = json.get("item_id").and_then(JsonValue::as_str)
                        && let Some(entry) = self.tool_calls.get_mut(item_id)
                        && entry.name.is_none()
                    {
                        entry.name = Some(name.to_owned());
                    }
                }
                Some("response.output_item.done") => {
                    if let Some(item) = json.get("item").and_then(JsonValue::as_object) {
                        let typ = item.get("type").and_then(JsonValue::as_str).unwrap_or("");
                        if typ == "function_call" {
                            let item_id = item
                                .get("id")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let name = item
                                .get("name")
                                .and_then(JsonValue::as_str)
                                .map(|s| s.to_owned());
                            let call_id = item
                                .get("call_id")
                                .and_then(JsonValue::as_str)
                                .map(|s| s.to_owned());
                            let args = item
                                .get("arguments")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("")
                                .to_owned();
                            if !item_id.is_empty() {
                                let entry = self.tool_calls.entry(item_id.clone()).or_default();
                                entry.id = Some(item_id.clone());
                                if name.is_some() {
                                    entry.name = name;
                                }
                                if call_id.is_some() {
                                    entry.call_id = call_id;
                                }
                                if !args.is_empty() {
                                    entry.arguments = args;
                                }
                                if !self.pending_tool_order.contains(&item_id) {
                                    self.pending_tool_order.push(item_id.clone());
                                }
                            }
                            // Defer emission until finish to ensure correct stop reason
                        }
                    }
                }
                Some("response.completed") => {
                    // Extract generation id, model, usage, finish reason
                    self.saw_done = true;
                    if let Some(resp) = json.get("response").and_then(JsonValue::as_object) {
                        if let Some(id) = resp.get("id").and_then(JsonValue::as_str) {
                            self.generation_id = Some(id.to_owned());
                        }
                        if let Some(m) = resp.get("model").and_then(JsonValue::as_str) {
                            self.model = Some(m.to_owned());
                        }
                        // stop_reason mapping? Responses uses status completed; we infer from output last item
                        // But check resp.status or stop_reason
                        if let Some(reason) = resp.get("status").and_then(JsonValue::as_str) {
                            // status completed => stop
                            self.finish_reason = Some("stop".to_owned());
                            if reason != "completed" {
                                self.finish_reason = Some(reason.to_owned());
                            }
                        }
                        if let Some(usage) = resp.get("usage") {
                            self.usage = Some(parse_usage(usage));
                        }
                    } else if let Some(usage) = json.get("usage") {
                        self.usage = Some(parse_usage(usage));
                    }
                    // Also check for usage at top-level response.usage alternative
                }
                Some("ping") => {
                    // cost ping: {"type":"ping","cost":"0"} ignore
                }
                Some(_) => {
                    // Unknown event: try to extract text delta fallback?
                    // Check if json has delta field generically
                    if let Some(delta) = json.get("delta").and_then(JsonValue::as_str)
                        && !delta.is_empty()
                    {
                        // Might be text delta without correct event? emit
                        // But only if not already handled
                    }
                }
                None => {
                    // No event type: might be data with choices.delta style? fallback
                    // Check if JSON looks like chat completions delta
                    if let Some(choices) = json.get("choices").and_then(JsonValue::as_array)
                        && let Some(choice) = choices.first()
                    {
                        if let Some(delta) = choice.get("delta").and_then(JsonValue::as_object) {
                            if let Some(content) = delta.get("content").and_then(JsonValue::as_str)
                                && !content.is_empty()
                            {
                                events.push(ModelStreamEvent::TextDelta(content.to_owned()));
                            }
                            // tool_calls handling for chat completions fallback
                            if let Some(calls) =
                                delta.get("tool_calls").and_then(JsonValue::as_array)
                            {
                                for _call in calls {
                                    // This is fragment, need to assemble similar to OpenRouter but we don't have index handling here.
                                    // For simplicity ignore chat completions tool calls for zen (responses format is primary).
                                    let _ = _call;
                                }
                            }
                        }
                        if let Some(reason) =
                            choice.get("finish_reason").and_then(JsonValue::as_str)
                        {
                            self.finish_reason = Some(reason.to_owned());
                        }
                        if let Some(usage) = json.get("usage") {
                            self.usage = Some(parse_usage(usage));
                        }
                    }
                }
            }
        }
        Ok(events)
    }

    pub(super) fn finish(mut self, allow_partial: bool) -> Result<OpencodeZenComplete, String> {
        // Handle buffered remainder as final line if any
        if !self.buffered.is_empty() {
            let line = std::mem::take(&mut self.buffered);
            let text = std::str::from_utf8(&line)
                .map_err(|_| "OpenCode Zen returned non-UTF8 SSE".to_owned())?;
            // Try to process as one more record if it contains data:
            if text.contains("data:") {
                // Simulate push for remainder
                let mut tmp = Vec::new();
                tmp.extend_from_slice(&line);
                tmp.extend_from_slice(b"\n\n");
                let mut fake = OpencodeZenSseDecoder {
                    buffered: tmp,
                    tool_calls: self.tool_calls.clone(),
                    pending_tool_order: self.pending_tool_order.clone(),
                    finish_reason: self.finish_reason.clone(),
                    usage: self.usage.clone(),
                    generation_id: self.generation_id.clone(),
                    model: self.model.clone(),
                    saw_data: self.saw_data,
                    saw_done: self.saw_done,
                };
                if let Ok(ev) = fake.push(b"") {
                    // ignore events? This remainder likely truncated
                    if !allow_partial && !ev.is_empty() {
                        // If we got events from truncated remainder and not allowed partial, error
                    }
                }
                if !allow_partial
                    && !text.trim().ends_with("[DONE]")
                    && !self.saw_done
                    && self.finish_reason.is_none()
                {
                    // For partial handling, allow
                }
            }
        }
        if !self.saw_data
            && !allow_partial
            && !self.saw_done
            && self.finish_reason.is_none()
            && self.tool_calls.is_empty()
        {
            return Err("OpenCode Zen SSE response ended before completion".to_owned());
        }
        if allow_partial && !self.saw_done && self.finish_reason.is_none() {
            self.finish_reason = Some("length".to_owned());
        }
        let mut events = Vec::new();
        // Emit any pending tool calls that weren't emitted via done events (fallback for non-streaming or missed done)
        for item_id in self.pending_tool_order.clone() {
            if let Some(entry) = self.tool_calls.get(&item_id) {
                if let (Some(name), Some(call_id)) = (entry.name.clone(), entry.call_id.clone()) {
                    let call = AgentToolCall {
                        id: ToolCallId::new(call_id).unwrap_or_else(|_| {
                            ToolCallId::new(format!("call_{item_id}")).expect("fallback")
                        }),
                        name,
                        arguments: SerializedJson::new(entry.arguments.clone()),
                    };
                    events.push(ModelStreamEvent::ToolCall(call));
                } else if let Some(name) = entry.name.clone() {
                    // Fallback call id
                    let call = AgentToolCall {
                        id: ToolCallId::new(format!("call_{item_id}")).expect("fallback"),
                        name,
                        arguments: SerializedJson::new(entry.arguments.clone()),
                    };
                    events.push(ModelStreamEvent::ToolCall(call));
                }
            }
        }
        let usage = self.usage.unwrap_or_default();
        if usage.is_reported() {
            events.push(ModelStreamEvent::Usage(usage.clone()));
        }
        let has_tools = !events
            .iter()
            .filter(|e| matches!(e, ModelStreamEvent::ToolCall(_)))
            .collect::<Vec<_>>()
            .is_empty()
            || self.tool_calls.values().any(|t| t.name.is_some());
        let stop_reason = match self.finish_reason.as_deref() {
            Some("length") | Some("max_output_tokens") => StopReason::Length,
            Some("tool_calls") | Some("tool_call") if has_tools => StopReason::ToolUse,
            _ if has_tools => StopReason::ToolUse,
            _ => StopReason::Stop,
        };
        events.push(ModelStreamEvent::End(stop_reason));
        Ok(OpencodeZenComplete {
            events,
            usage,
            generation_id: self.generation_id,
        })
    }

    fn find_delim(&self) -> Option<(usize, usize)> {
        // Look for \n\n, \r\n\r\n, \n\r\n, etc. Simplify to \n\n
        // We treat any double newline (with optional \r) as delim.
        // Search for pattern "\n\n" or "\r\n\r\n" or "\r\r"
        let bytes = &self.buffered;
        for i in 0..bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i + 1] == b'\n' {
                return Some((i, 2));
            }
            if i + 3 < bytes.len()
                && bytes[i] == b'\r'
                && bytes[i + 1] == b'\n'
                && bytes[i + 2] == b'\r'
                && bytes[i + 3] == b'\n'
            {
                return Some((i, 4));
            }
            if i + 1 < bytes.len() && bytes[i] == b'\r' && bytes[i + 1] == b'\r' {
                return Some((i, 2));
            }
        }
        None
    }
}

fn parse_usage(usage: &JsonValue) -> Usage {
    // Responses usage shape: {input_tokens, output_tokens, total_tokens, input_tokens_details:{cached_tokens}, output_tokens_details:{reasoning_tokens}}
    let input = usage
        .get("input_tokens")
        .and_then(JsonValue::as_u64)
        .or_else(|| usage.get("prompt_tokens").and_then(JsonValue::as_u64));
    let output = usage
        .get("output_tokens")
        .and_then(JsonValue::as_u64)
        .or_else(|| usage.get("completion_tokens").and_then(JsonValue::as_u64));
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(JsonValue::as_u64)
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(JsonValue::as_u64)
        });
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(JsonValue::as_u64)
        .or_else(|| {
            usage
                .get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens"))
                .and_then(JsonValue::as_u64)
        });

    Usage {
        total_tokens: usage.get("total_tokens").and_then(JsonValue::as_u64),
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: reasoning,
        cache_read_tokens: cached,
        cache_write_tokens: None,
        cost: usage
            .get("cost")
            .and_then(JsonValue::as_str)
            .map(|s| s.to_owned()),
        ..Usage::default()
    }
}

pub(super) fn opencode_zen_context_overflow(bytes: &[u8]) -> bool {
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
    let m = message.to_ascii_lowercase();
    let overflow = m.contains("maximum")
        || m.contains("exceed")
        || m.contains("too long")
        || m.contains("over limit")
        || m.contains("limit reached");
    let poolside = m.contains("input length")
        && m.contains("maximum allowed input length")
        && m.contains("token");
    (overflow
        && m.contains("context")
        && (m.contains("length")
            || m.contains("limit")
            || m.contains("window")
            || m.contains("token")
            || m.contains("capacity")))
        || poolside
        || m.contains("too many tokens")
        || m.contains("prompt is too long")
}

#[allow(dead_code)]
pub(super) fn opencode_zen_status_retryable(status: Option<u16>) -> bool {
    matches!(status, Some(429) | Some(500..=599))
}

#[allow(dead_code)]
pub(super) fn opencode_zen_response_retryable(bytes: &[u8]) -> bool {
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

pub(super) fn response_body_prefix(bytes: &[u8], secret: Option<&str>) -> String {
    const MAX_BYTES: usize = 512;
    let bounded = &bytes[..bytes.len().min(MAX_BYTES)];
    let mut value = String::from_utf8_lossy(bounded).into_owned();
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        value = value.replace(secret, "[redacted]");
    }
    let value = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let v = value.trim();
    if v.is_empty() {
        "<empty>".to_owned()
    } else {
        v.to_owned()
    }
}
