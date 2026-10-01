//! Incremental Server-Sent Events decoder for the Anthropic stream.
//!
//! Port of upstream Pi's `iterateSseMessages`/`decodeSseLine`
//! (`packages/ai/src/api/anthropic-messages.ts`): CR, LF, and CRLF line
//! endings; comment lines; `event` and multi-line `data` fields; one optional
//! space after the field colon; and a final flush of a trailing record without
//! its blank-line terminator. Bytes may arrive split anywhere, including inside
//! a UTF-8 sequence or between CR and LF.

/// One complete server-sent event.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ServerSentEvent {
    /// The `event` field, when present.
    pub(crate) event: Option<String>,
    /// The `data` field lines joined with `\n`.
    pub(crate) data: String,
}

/// Incremental decoder state.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
    has_fields: bool,
}

impl SseDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed bytes and return every event completed by them.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<ServerSentEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            let Some(index) = self.buffer.iter().position(|byte| *byte == b'\r' || *byte == b'\n')
            else {
                break;
            };
            // A CR at the very end may be the first half of a CRLF pair.
            if self.buffer[index] == b'\r' && index + 1 == self.buffer.len() {
                break;
            }
            let mut next = index + 1;
            if self.buffer[index] == b'\r' && self.buffer.get(next) == Some(&b'\n') {
                next += 1;
            }
            let line = self.buffer[..index].to_vec();
            self.buffer.drain(..next);
            if let Some(event) = self.line(&String::from_utf8_lossy(&line)) {
                events.push(event);
            }
        }
        events
    }

    /// Flush a trailing record at end of input.
    pub(crate) fn finish(&mut self) -> Vec<ServerSentEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(event) = self.line(&String::from_utf8_lossy(&line)) {
                events.push(event);
            }
        }
        if let Some(event) = self.flush() {
            events.push(event);
        }
        events
    }

    fn line(&mut self, line: &str) -> Option<ServerSentEvent> {
        if line.is_empty() {
            return self.flush();
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.find(':') {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (line, ""),
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => {
                self.event = Some(value.to_owned());
                self.has_fields = true;
            }
            "data" => {
                self.data.push(value.to_owned());
                self.has_fields = true;
            }
            _ => {}
        }
        None
    }

    fn flush(&mut self) -> Option<ServerSentEvent> {
        if !self.has_fields || (self.event.is_none() && self.data.is_empty()) {
            self.has_fields = false;
            return None;
        }
        self.has_fields = false;
        Some(ServerSentEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data).join("\n"),
        })
    }
}

/// Repair malformed JSON string literals, as upstream Pi's `repairJson`:
/// raw control characters inside strings are escaped and a backslash before an
/// invalid escape character is doubled.
pub(crate) fn repair_json(json: &str) -> String {
    const VALID_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let characters = json.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if !in_string {
            repaired.push(character);
            if character == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            repaired.push(character);
            in_string = false;
            index += 1;
            continue;
        }
        if character == '\\' {
            let Some(next) = characters.get(index + 1).copied() else {
                repaired.push_str("\\\\");
                index += 1;
                continue;
            };
            if next == 'u' {
                let digits = characters
                    .get(index + 2..index + 6)
                    .map(|digits| digits.iter().collect::<String>());
                if let Some(digits) = digits
                    && digits.len() == 4
                    && digits.chars().all(|digit| digit.is_ascii_hexdigit())
                {
                    repaired.push_str("\\u");
                    repaired.push_str(&digits);
                    index += 6;
                    continue;
                }
            }
            if VALID_ESCAPES.contains(&next) {
                repaired.push('\\');
                repaired.push(next);
                index += 2;
                continue;
            }
            repaired.push_str("\\\\");
            index += 1;
            continue;
        }
        if (character as u32) <= 0x1f {
            match character {
                '\u{8}' => repaired.push_str("\\b"),
                '\u{c}' => repaired.push_str("\\f"),
                '\n' => repaired.push_str("\\n"),
                '\r' => repaired.push_str("\\r"),
                '\t' => repaired.push_str("\\t"),
                other => repaired.push_str(&format!("\\u{:04x}", other as u32)),
            }
        } else {
            repaired.push(character);
        }
        index += 1;
    }
    repaired
}

/// Parse JSON, retrying once after [`repair_json`] changed the text.
pub(crate) fn parse_json_with_repair(json: &str) -> Option<tea_protocol::JsonValue> {
    match tea_protocol::JsonValue::parse(json) {
        Ok(value) => Some(value),
        Err(_) => {
            let repaired = repair_json(json);
            (repaired != json)
                .then(|| tea_protocol::JsonValue::parse(&repaired).ok())
                .flatten()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> Vec<ServerSentEvent> {
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(decoder.push(chunk));
        }
        events.extend(decoder.finish());
        events
    }

    #[test]
    fn decodes_events_split_at_every_byte_and_line_ending() {
        let body = b"event: message_start\r\ndata: {\"a\":1}\r\n\r\n: comment\nevent: ping\ndata: x\ndata: y\n\nevent: tail\ndata: z";
        let whole = decode_all(&[body]);
        let single_bytes = body.iter().map(std::slice::from_ref).collect::<Vec<_>>();
        assert_eq!(decode_all(&single_bytes), whole);
        assert_eq!(
            whole,
            vec![
                ServerSentEvent {
                    event: Some("message_start".into()),
                    data: "{\"a\":1}".into(),
                },
                ServerSentEvent {
                    event: Some("ping".into()),
                    data: "x\ny".into(),
                },
                ServerSentEvent {
                    event: Some("tail".into()),
                    data: "z".into(),
                },
            ]
        );
    }

    #[test]
    fn splits_inside_a_multibyte_character_without_corruption() {
        let body = "event: message_delta\ndata: {\"t\":\"é🙈\"}\n\n".as_bytes();
        for split in 1..body.len() {
            let events = decode_all(&[&body[..split], &body[split..]]);
            assert_eq!(events.len(), 1, "split {split}");
            assert_eq!(events[0].data, "{\"t\":\"é🙈\"}");
        }
    }

    #[test]
    fn a_lone_carriage_return_terminates_a_line() {
        let events = decode_all(&[b"event: a\rdata: b\r\r"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "b");
    }

    #[test]
    fn repairs_control_characters_and_invalid_escapes_like_pi() {
        let malformed = "{\"path\":\"A\\H\",\"text\":\"col1\tcol2\"}";
        let parsed = parse_json_with_repair(malformed).expect("repairable JSON");
        assert_eq!(
            parsed.get("path").and_then(tea_protocol::JsonValue::as_str),
            Some("A\\H")
        );
        assert_eq!(
            parsed.get("text").and_then(tea_protocol::JsonValue::as_str),
            Some("col1\tcol2")
        );
        assert_eq!(repair_json("{\"ok\":\"\\u00e9\"}"), "{\"ok\":\"\\u00e9\"}");
        assert!(parse_json_with_repair("{not json").is_none());
    }
}
