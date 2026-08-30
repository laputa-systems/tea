//! Incremental, bounded Codex Responses Server-Sent Events reduction.

use super::wire::{MAX_SSE_RECORD_BYTES, PROVIDER_ID};
use crate::json::JsonValue;
use crate::scheduler::ModelStreamEvent;
use crate::state::{
    AgentToolCall, OpaqueProviderContextItem, SerializedJson, StopReason, ToolCallId, Usage,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, Debug, Default)]
struct PendingTool {
    output_index: Option<u64>,
    item_id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
    emitted: bool,
}

/// Stateful byte-stream reducer for the direct Codex Responses SSE protocol.
pub(super) struct CodexSseDecoder {
    buffered: Vec<u8>,
    tools: BTreeMap<String, PendingTool>,
    output_index_keys: BTreeMap<u64, String>,
    emitted_text_by_part: BTreeMap<String, String>,
    captured_reasoning: BTreeSet<(Option<String>, String)>,
    emitted_tool_call_ids: BTreeSet<String>,
    saw_tool_call: bool,
    terminal: Option<StopReason>,
}

impl CodexSseDecoder {
    /// Start a fresh bounded incremental decoder.
    pub(super) fn new() -> Self {
        Self {
            buffered: Vec::new(),
            tools: BTreeMap::new(),
            output_index_keys: BTreeMap::new(),
            emitted_text_by_part: BTreeMap::new(),
            captured_reasoning: BTreeSet::new(),
            emitted_tool_call_ids: BTreeSet::new(),
            saw_tool_call: false,
            terminal: None,
        }
    }

    /// Reduce arbitrary body chunks without waiting for a whole response.
    pub(super) fn push(
        &mut self,
        mut bytes: &[u8],
    ) -> Result<Vec<ModelStreamEvent>, CodexStreamError> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        while !bytes.is_empty() {
            if let Some((record_bytes, consumed)) =
                cross_chunk_record_boundary(&self.buffered, bytes)
            {
                if record_bytes > MAX_SSE_RECORD_BYTES {
                    return Err(CodexStreamError::OversizedRecord);
                }
                self.buffered.extend_from_slice(&bytes[..consumed]);
                bytes = &bytes[consumed..];
                self.drain_complete_records(&mut events)?;
                continue;
            }
            if let Some((position, length)) = find_record_boundary(bytes) {
                if self.buffered.len().saturating_add(position) > MAX_SSE_RECORD_BYTES {
                    return Err(CodexStreamError::OversizedRecord);
                }
                self.buffered.extend_from_slice(&bytes[..position + length]);
                bytes = &bytes[position + length..];
                self.drain_complete_records(&mut events)?;
                continue;
            }
            // At most three bytes can be a partial CRLF delimiter. Reject
            // before appending a giant transport chunk so malformed peers
            // cannot make this parser allocate beyond its record bound.
            if bytes.len()
                > MAX_SSE_RECORD_BYTES
                    .saturating_add(3)
                    .saturating_sub(self.buffered.len())
            {
                return Err(CodexStreamError::OversizedRecord);
            }
            self.buffered.extend_from_slice(bytes);
            break;
        }
        Ok(events)
    }

    fn drain_complete_records(
        &mut self,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        while let Some((position, length)) = find_record_boundary(&self.buffered) {
            let record = self.buffered.drain(..position + length).collect::<Vec<_>>();
            let record = &record[..record.len().saturating_sub(length)];
            if record.len() > MAX_SSE_RECORD_BYTES {
                return Err(CodexStreamError::OversizedRecord);
            }
            self.reduce_record(record, events)?;
        }
        Ok(())
    }

    /// Finish a body stream, accepting one final record without a blank-line
    /// terminator but rejecting EOF before a protocol terminal event.
    pub(super) fn finish(mut self) -> Result<Vec<ModelStreamEvent>, CodexStreamError> {
        let mut events = Vec::new();
        if !self.buffered.is_empty() {
            if self.buffered.len() > MAX_SSE_RECORD_BYTES {
                return Err(CodexStreamError::OversizedRecord);
            }
            let record = std::mem::take(&mut self.buffered);
            self.reduce_record(&record, &mut events)?;
        }
        if self.terminal.is_none() {
            return Err(CodexStreamError::PrematureEof);
        }
        Ok(events)
    }

    fn reduce_record(
        &mut self,
        record: &[u8],
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        if record.is_empty() {
            return Ok(());
        }
        let text = std::str::from_utf8(record).map_err(|_| CodexStreamError::InvalidUtf8)?;
        let mut event_name = None;
        let mut data_lines = Vec::new();
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(value) = line.strip_prefix("event:") {
                event_name = Some(value.trim_start().to_owned());
            } else if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.strip_prefix(' ').unwrap_or(value));
            }
        }
        if data_lines.is_empty() {
            return Ok(());
        }
        let data = data_lines.join("\n");
        if data.trim() == "[DONE]" {
            return Ok(());
        }
        if self.terminal.is_some() {
            // Backends commonly leave keepalive/DONE records after completion;
            // those are harmless, but a second JSON event would violate the
            // one-terminal model stream invariant.
            return Err(CodexStreamError::EventAfterTerminal);
        }
        let value = JsonValue::parse(&data).map_err(|_| CodexStreamError::InvalidJson)?;
        let event_type = value
            .get("type")
            .and_then(JsonValue::as_str)
            .or(event_name.as_deref())
            .unwrap_or_default();
        self.reduce_event(event_type, &value, events)
    }

    fn reduce_event(
        &mut self,
        event_type: &str,
        value: &JsonValue,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        match event_type {
            "response.created" | "response.in_progress" | "response.content_part.added" => {}
            "response.output_item.added" => {
                if let Some(item) = value.get("item") {
                    self.observe_output_item(item, false, events)?;
                }
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    self.observe_output_item(item, true, events)?;
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(JsonValue::as_str)
                    && !delta.is_empty()
                {
                    self.emit_text_delta(self.text_key_from_event(value), delta, events);
                }
            }
            "response.output_text.done" => {
                if let Some(text) = value
                    .get("text")
                    .or_else(|| value.get("output_text"))
                    .and_then(JsonValue::as_str)
                {
                    self.emit_final_text(self.text_key_from_event(value), text, events);
                }
            }
            "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.delta" => {
                // Summary text is intentionally not assistant output, and raw
                // reasoning is never surfaced from this adapter.
            }
            "response.function_call_arguments.delta" => {
                let key = self.tool_key_from_event(value);
                let tool = self.tools.entry(key).or_default();
                merge_event_tool_fields(tool, value);
                if let Some(delta) = value.get("delta").and_then(JsonValue::as_str) {
                    tool.arguments.push_str(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let key = self.tool_key_from_event(value);
                let tool = self.tools.entry(key.clone()).or_default();
                merge_event_tool_fields(tool, value);
                if let Some(arguments) = value.get("arguments").and_then(JsonValue::as_str) {
                    tool.arguments = arguments.to_owned();
                }
                self.emit_tool(&key, events)?;
            }
            "response.completed" | "response.done" => {
                self.observe_terminal_response(value, events, None)?;
            }
            "response.incomplete" => {
                self.observe_incomplete_response(value, events)?;
            }
            "response.failed" | "error" => {
                self.terminal = Some(StopReason::Error);
                events.push(ModelStreamEvent::Error {
                    message: "Codex Responses request failed".into(),
                });
            }
            _ => {
                // Forward-compatible events are intentionally ignored. The
                // JSON type, not an optional SSE event line, drives this switch.
            }
        }
        Ok(())
    }

    fn observe_terminal_response(
        &mut self,
        value: &JsonValue,
        events: &mut Vec<ModelStreamEvent>,
        forced_reason: Option<StopReason>,
    ) -> Result<(), CodexStreamError> {
        self.observe_response_output_and_usage(value, events)?;
        let reason = forced_reason.unwrap_or({
            if self.saw_tool_call {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            }
        });
        self.terminal = Some(reason);
        events.push(ModelStreamEvent::End(reason));
        Ok(())
    }

    fn observe_incomplete_response(
        &mut self,
        value: &JsonValue,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        self.observe_response_output_and_usage(value, events)?;
        let response = value.get("response").unwrap_or(value);
        let incomplete_reason = response
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(JsonValue::as_str);
        match incomplete_reason {
            Some("max_output_tokens" | "max_tokens" | "length") => {
                self.terminal = Some(StopReason::Length);
                events.push(ModelStreamEvent::End(StopReason::Length));
            }
            Some("cancelled" | "canceled") => {
                self.terminal = Some(StopReason::Cancelled);
                events.push(ModelStreamEvent::End(StopReason::Cancelled));
            }
            Some("content_filter") => {
                self.terminal = Some(StopReason::Error);
                events.push(ModelStreamEvent::Error {
                    message: "Codex response was incomplete due to a provider content filter"
                        .into(),
                });
            }
            _ => {
                self.terminal = Some(StopReason::Error);
                events.push(ModelStreamEvent::Error {
                    message: "Codex response was incomplete".into(),
                });
            }
        }
        Ok(())
    }

    fn observe_response_output_and_usage(
        &mut self,
        value: &JsonValue,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        let response = value.get("response").unwrap_or(value);
        if let Some(output) = response.get("output").and_then(JsonValue::as_array) {
            for item in output {
                self.observe_output_item(item, true, events)?;
            }
        }
        if let Some(usage) = response.get("usage").or_else(|| value.get("usage")) {
            let usage = parse_usage(usage);
            if usage.is_reported() {
                events.push(ModelStreamEvent::Usage(usage));
            }
        }
        Ok(())
    }

    fn observe_output_item(
        &mut self,
        item: &JsonValue,
        completed: bool,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        match item.get("type").and_then(JsonValue::as_str) {
            Some("function_call") => {
                let key = self.tool_key_from_item(item);
                let tool = self.tools.entry(key.clone()).or_default();
                merge_item_tool_fields(tool, item);
                if completed {
                    self.emit_tool(&key, events)?;
                }
            }
            Some("reasoning") if completed => self.capture_reasoning(item, events)?,
            Some("message") if completed => self.capture_final_message_text(item, events),
            _ => {}
        }
        Ok(())
    }

    fn text_key_from_event(&self, event: &JsonValue) -> String {
        let item = event
            .get("item_id")
            .or_else(|| event.get("id"))
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .map(|value| format!("item:{value}"))
            .or_else(|| {
                event
                    .get("output_index")
                    .and_then(JsonValue::as_u64)
                    .map(|index| format!("output:{index}"))
            })
            .unwrap_or_else(|| "unidentified".into());
        let content_index = event
            .get("content_index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        format!("{item}:content:{content_index}")
    }

    fn text_key_from_item(&self, item: &JsonValue, content_index: usize) -> String {
        let item = item
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .map(|value| format!("item:{value}"))
            .or_else(|| {
                item.get("output_index")
                    .and_then(JsonValue::as_u64)
                    .map(|index| format!("output:{index}"))
            })
            .unwrap_or_else(|| "unidentified".into());
        format!("{item}:content:{content_index}")
    }

    fn emit_text_delta(&mut self, key: String, delta: &str, events: &mut Vec<ModelStreamEvent>) {
        self.emitted_text_by_part
            .entry(key)
            .or_default()
            .push_str(delta);
        events.push(ModelStreamEvent::TextDelta(delta.to_owned()));
    }

    fn emit_final_text(&mut self, key: String, text: &str, events: &mut Vec<ModelStreamEvent>) {
        if text.is_empty() {
            return;
        }
        let emitted = self.emitted_text_by_part.entry(key).or_default();
        if text == emitted || emitted.starts_with(text) {
            return;
        }
        if let Some(suffix) = text.strip_prefix(emitted.as_str()) {
            if !suffix.is_empty() {
                events.push(ModelStreamEvent::TextDelta(suffix.to_owned()));
            }
            *emitted = text.to_owned();
            return;
        }
        // If the server did not expose deltas for this distinct Responses
        // content part, the completed output remains the only authoritative
        // visible text. A mismatched same-part final value is not replayed,
        // because duplicating already rendered content is worse than treating
        // a protocol inconsistency as stream-only output.
        if emitted.is_empty() {
            *emitted = text.to_owned();
            events.push(ModelStreamEvent::TextDelta(text.to_owned()));
        }
    }

    fn capture_final_message_text(&mut self, item: &JsonValue, events: &mut Vec<ModelStreamEvent>) {
        let Some(content) = item.get("content").and_then(JsonValue::as_array) else {
            return;
        };
        for (content_index, part) in content.iter().enumerate() {
            if part.get("type").and_then(JsonValue::as_str) != Some("output_text") {
                continue;
            }
            if let Some(text) = part.get("text").and_then(JsonValue::as_str) {
                self.emit_final_text(self.text_key_from_item(item, content_index), text, events);
            }
        }
    }

    fn tool_key_from_item(&mut self, item: &JsonValue) -> String {
        let key = item
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                item.get("output_index")
                    .and_then(JsonValue::as_u64)
                    .map(|index| format!("output-{index}"))
            })
            .unwrap_or_else(|| format!("unidentified-{}", self.tools.len()));
        if let Some(index) = item.get("output_index").and_then(JsonValue::as_u64) {
            self.output_index_keys.insert(index, key.clone());
        }
        key
    }

    fn tool_key_from_event(&mut self, event: &JsonValue) -> String {
        if let Some(id) = event
            .get("item_id")
            .or_else(|| event.get("id"))
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
        {
            return id.to_owned();
        }
        if let Some(index) = event.get("output_index").and_then(JsonValue::as_u64) {
            if let Some(key) = self.output_index_keys.get(&index) {
                return key.clone();
            }
            let key = format!("output-{index}");
            self.output_index_keys.insert(index, key.clone());
            return key;
        }
        format!("unidentified-{}", self.tools.len())
    }

    fn emit_tool(
        &mut self,
        key: &str,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        let tool = self
            .tools
            .get_mut(key)
            .ok_or(CodexStreamError::MalformedToolCall)?;
        if tool.emitted {
            return Ok(());
        }
        let call_id = tool
            .call_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or(CodexStreamError::MalformedToolCall)?;
        let name = tool
            .name
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or(CodexStreamError::MalformedToolCall)?;
        JsonValue::parse(&tool.arguments).map_err(|_| CodexStreamError::InvalidToolArguments)?;
        if !self.emitted_tool_call_ids.insert(call_id.to_owned()) {
            return Err(CodexStreamError::DuplicateToolCallId);
        }
        let id =
            ToolCallId::new(call_id.to_owned()).map_err(|_| CodexStreamError::MalformedToolCall)?;
        tool.emitted = true;
        self.saw_tool_call = true;
        events.push(ModelStreamEvent::ToolCall(AgentToolCall {
            id,
            name: name.to_owned(),
            arguments: SerializedJson::new(tool.arguments.clone()),
        }));
        Ok(())
    }

    fn capture_reasoning(
        &mut self,
        item: &JsonValue,
        events: &mut Vec<ModelStreamEvent>,
    ) -> Result<(), CodexStreamError> {
        let Some(payload) = item.get("encrypted_content").and_then(JsonValue::as_str) else {
            return Ok(());
        };
        let item_id = item
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let summary = match item.get("summary") {
            Some(summary) if summary.as_array().is_some() => summary.clone(),
            Some(_) => return Err(CodexStreamError::MalformedReasoningItem),
            None => JsonValue::Array(Vec::new()),
        };
        let mut fields = vec![
            ("type", JsonValue::String("reasoning".into())),
            ("summary", summary),
            ("encrypted_content", JsonValue::String(payload.to_owned())),
        ];
        if let Some(id) = item_id.as_deref() {
            fields.push(("id", JsonValue::String(id.to_owned())));
        }
        let stored_payload = JsonValue::object(fields)
            .to_json_string()
            .map_err(|_| CodexStreamError::MalformedReasoningItem)?;
        let dedupe = (item_id.clone(), stored_payload.clone());
        if !self.captured_reasoning.insert(dedupe) {
            return Ok(());
        }
        let item =
            OpaqueProviderContextItem::new(PROVIDER_ID, "reasoning", item_id, stored_payload)
                .map_err(|_| CodexStreamError::MalformedReasoningItem)?;
        events.push(ModelStreamEvent::OpaqueProviderContext(item));
        Ok(())
    }
}

fn merge_item_tool_fields(tool: &mut PendingTool, item: &JsonValue) {
    if let Some(index) = item.get("output_index").and_then(JsonValue::as_u64) {
        tool.output_index = Some(index);
    }
    if let Some(id) = item
        .get("id")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
    {
        tool.item_id = Some(id.to_owned());
    }
    if let Some(call_id) = item
        .get("call_id")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
    {
        tool.call_id = Some(call_id.to_owned());
    }
    if let Some(name) = item
        .get("name")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
    {
        tool.name = Some(name.to_owned());
    }
    if let Some(arguments) = item.get("arguments").and_then(JsonValue::as_str)
        && (!arguments.is_empty() || tool.arguments.is_empty()) {
            tool.arguments = arguments.to_owned();
        }
}

fn merge_event_tool_fields(tool: &mut PendingTool, event: &JsonValue) {
    if let Some(index) = event.get("output_index").and_then(JsonValue::as_u64) {
        tool.output_index = Some(index);
    }
    if let Some(id) = event
        .get("item_id")
        .or_else(|| event.get("id"))
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
    {
        tool.item_id = Some(id.to_owned());
    }
    if let Some(call_id) = event
        .get("call_id")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
    {
        tool.call_id = Some(call_id.to_owned());
    }
    if let Some(name) = event
        .get("name")
        .and_then(JsonValue::as_str)
        .filter(|value| !value.is_empty())
    {
        tool.name = Some(name.to_owned());
    }
}

fn parse_usage(value: &JsonValue) -> Usage {
    let input_tokens = value.get("input_tokens").and_then(JsonValue::as_u64);
    let output_tokens = value.get("output_tokens").and_then(JsonValue::as_u64);
    let cache_read_tokens = value
        .get("input_tokens_details")
        .and_then(|value| value.get("cached_tokens"))
        .and_then(JsonValue::as_u64);
    let reasoning_tokens = value
        .get("output_tokens_details")
        .and_then(|value| value.get("reasoning_tokens"))
        .and_then(JsonValue::as_u64);
    Usage {
        total_tokens: value.get("total_tokens").and_then(JsonValue::as_u64),
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cache_read_tokens,
        cache_write_tokens: None,
        cost: None,
    }
}

fn find_record_boundary(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'\n' && bytes[index + 1] == b'\n' {
            return Some((index, 2));
        }
        if index + 3 < bytes.len()
            && bytes[index] == b'\r'
            && bytes[index + 1] == b'\n'
            && bytes[index + 2] == b'\r'
            && bytes[index + 3] == b'\n'
        {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

/// Locate an SSE blank-line delimiter split between the retained partial
/// record and the next transport chunk without first joining the chunks.
fn cross_chunk_record_boundary(buffered: &[u8], incoming: &[u8]) -> Option<(usize, usize)> {
    if buffered.ends_with(b"\n") && incoming.starts_with(b"\n") {
        return Some((buffered.len().saturating_sub(1), 1));
    }
    const CRLF_DELIMITER: &[u8] = b"\r\n\r\n";
    for split in 1..CRLF_DELIMITER.len() {
        if buffered.ends_with(&CRLF_DELIMITER[..split])
            && incoming.starts_with(&CRLF_DELIMITER[split..])
        {
            return Some((
                buffered.len().saturating_sub(split),
                CRLF_DELIMITER.len() - split,
            ));
        }
    }
    None
}

/// Bounded direct-Responses stream protocol failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CodexStreamError {
    /// A complete SSE record was not UTF-8.
    InvalidUtf8,
    /// A complete `data:` payload was not JSON.
    InvalidJson,
    /// A single record exceeded the bounded parser buffer.
    OversizedRecord,
    /// The body ended before a terminal Responses event.
    PrematureEof,
    /// JSON arrived after a terminal event.
    EventAfterTerminal,
    /// A function-call item omitted an identity or name.
    MalformedToolCall,
    /// Function-call arguments were not valid JSON at completion.
    InvalidToolArguments,
    /// Two completed calls used the same provider call ID.
    DuplicateToolCallId,
    /// Encrypted reasoning continuity fields were invalid or too large.
    MalformedReasoningItem,
}

impl fmt::Display for CodexStreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidUtf8 => "Codex SSE stream contained invalid UTF-8",
            Self::InvalidJson => "Codex SSE stream contained invalid JSON",
            Self::OversizedRecord => "Codex SSE record exceeded the configured limit",
            Self::PrematureEof => "Codex SSE stream ended before a terminal response event",
            Self::EventAfterTerminal => "Codex SSE stream contained an event after completion",
            Self::MalformedToolCall => "Codex SSE stream contained an incomplete function call",
            Self::InvalidToolArguments => {
                "Codex SSE stream contained invalid function-call JSON arguments"
            }
            Self::DuplicateToolCallId => "Codex SSE stream repeated a function-call ID",
            Self::MalformedReasoningItem => {
                "Codex SSE stream contained invalid encrypted reasoning state"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CodexStreamError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_with_chunks(
        bytes: &[u8],
        chunks: impl IntoIterator<Item = usize>,
    ) -> Vec<ModelStreamEvent> {
        let mut decoder = CodexSseDecoder::new();
        let mut events = Vec::new();
        let mut offset = 0;
        for width in chunks {
            let end = (offset + width).min(bytes.len());
            if offset < end {
                events.extend(decoder.push(&bytes[offset..end]).unwrap());
            }
            offset = end;
        }
        if offset < bytes.len() {
            events.extend(decoder.push(&bytes[offset..]).unwrap());
        }
        events.extend(decoder.finish().unwrap());
        events
    }

    #[test]
    fn parses_text_reasoning_usage_and_terminal_at_any_byte_boundary() {
        let fixture = concat!(
            "event: response.output_text.delta\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hé\"}\r\n\r\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"cipher\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":1},\"output_tokens\":2,\"output_tokens_details\":{\"reasoning_tokens\":1}}}}\n\n"
        )
        .as_bytes();
        for split in 0..=fixture.len() {
            let events = collect_with_chunks(fixture, [split, fixture.len().saturating_sub(split)]);
            assert!(
                events.iter().any(
                    |event| matches!(event, ModelStreamEvent::TextDelta(value) if value == "hé")
                )
            );
            assert!(events.iter().any(|event| {
                matches!(event, ModelStreamEvent::OpaqueProviderContext(item)
                    if JsonValue::parse(item.payload())
                        .ok()
                        .and_then(|payload| payload.get("encrypted_content").and_then(JsonValue::as_str).map(str::to_owned))
                        .as_deref()
                        == Some("cipher"))
            }));
            assert!(events.iter().any(|event| matches!(event, ModelStreamEvent::Usage(usage) if usage.cache_read_tokens == Some(1))));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ModelStreamEvent::End(_)))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn reconstructs_interleaved_parallel_calls_exactly_once() {
        let fixture = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"read\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\",\"call_id\":\"call_b\",\"name\":\"find\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_a\",\"delta\":\"a\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_b\",\"delta\":\"b\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_b\",\"arguments\":\"{\\\"query\\\":\\\"tea\\\"}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_a\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
        );
        let events = collect_with_chunks(fixture.as_bytes(), [1; 1]);
        let calls = events
            .iter()
            .filter_map(|event| match event {
                ModelStreamEvent::ToolCall(call) => Some((call.id.as_str(), call.name.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(calls, vec![("call_b", "find"), ("call_a", "read")]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelStreamEvent::End(StopReason::ToolUse)))
        );
    }

    #[test]
    fn completed_message_text_fills_a_missing_suffix_without_duplicate_deltas() {
        let final_message = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"content_index\":0,\"delta\":\"hello \"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello world\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        let events = collect_with_chunks(final_message.as_bytes(), [2, 5, 1, 3, 8]);
        let text = events
            .iter()
            .filter_map(|event| match event {
                ModelStreamEvent::TextDelta(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "hello world");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ModelStreamEvent::End(_)))
                .count(),
            1,
        );
    }

    #[test]
    fn handles_comments_multiline_data_done_and_forward_compatible_events() {
        let fixture = concat!(
            ": keepalive\r\n\r\n",
            "event: ignored\r\n",
            "data: {\"type\":\"future.event\",\r\n",
            "data: \"value\":true}\r\n\r\n",
            "data: [DONE]\r\n\r\n",
            "event: response.done\r\n",
            "data: {\"type\":\"response.done\",\"response\":{}}\r\n\r\n",
        );
        let events = collect_with_chunks(fixture.as_bytes(), [1; 1]);
        assert_eq!(events, vec![ModelStreamEvent::End(StopReason::Stop)]);
    }

    #[test]
    fn rejects_invalid_utf8_and_oversized_records() {
        let mut invalid = CodexSseDecoder::new();
        assert_eq!(
            invalid.push(b"data: \xff\n\n"),
            Err(CodexStreamError::InvalidUtf8)
        );

        let mut oversized = CodexSseDecoder::new();
        let bytes = vec![b'x'; super::MAX_SSE_RECORD_BYTES + 64 * 1024];
        assert_eq!(
            oversized.push(&bytes),
            Err(CodexStreamError::OversizedRecord)
        );
        assert!(oversized.buffered.len() <= super::MAX_SSE_RECORD_BYTES + 3);
    }

    #[test]
    fn rejects_eof_without_terminal_and_invalid_json_arguments() {
        let mut decoder = CodexSseDecoder::new();
        decoder
            .push(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n")
            .unwrap();
        assert_eq!(decoder.finish(), Err(CodexStreamError::PrematureEof));
    }

    #[test]
    fn distinguishes_output_length_cancellation_and_provider_incomplete_failures() {
        let output_length = collect_with_chunks(
            br#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}

"#,
            [1; 1],
        );
        assert_eq!(
            output_length,
            vec![ModelStreamEvent::End(StopReason::Length)]
        );

        let cancelled = collect_with_chunks(
            br#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"cancelled"}}}

"#,
            [1; 1],
        );
        assert_eq!(
            cancelled,
            vec![ModelStreamEvent::End(StopReason::Cancelled)]
        );

        let filtered = collect_with_chunks(
            br#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}

"#,
            [1; 1],
        );
        assert!(matches!(
            filtered.as_slice(),
            [ModelStreamEvent::Error { message }]
                if message.contains("content filter")
        ));
    }
}
