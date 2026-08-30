//! Redacted direct-Codex failure classification and durable diagnostics.

use super::wire::MAX_DIAGNOSTIC_RESPONSE_BYTES;
use crate::json::JsonValue;
use std::fmt;

const SENSITIVE_RESPONSE_FIELDS: &[&str] = &[
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "authorization",
    "code",
    "code_verifier",
    "client_secret",
    "account_id",
    "chatgpt_account_id",
    "encrypted_content",
];

/// Boundary at which a direct Codex attempt failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexErrorSource {
    /// The Tea HTTP transport did not complete an attempt.
    Transport,
    /// The direct Codex backend rejected or terminated an attempt.
    Response,
    /// Tea rejected a request before it reached the network.
    Adapter,
    /// Tea could not obtain a fresh explicit OAuth snapshot.
    Authentication,
}

impl CodexErrorSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Response => "response",
            Self::Adapter => "adapter",
            Self::Authentication => "authentication",
        }
    }
}

/// Bounded diagnostic retained with a durable provider error.
///
/// It intentionally contains only adapter-selected text. It never stores a
/// bearer token, refresh token, account ID, request body, or raw headers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexErrorReport {
    /// Failing protocol boundary.
    pub source: CodexErrorSource,
    /// Stable redacted explanation for an operator.
    pub message: String,
    /// HTTP status if headers were received.
    pub status_code: Option<u16>,
    /// Bounded backend error category, when a structured response supplied one.
    pub error_type: Option<String>,
    /// Bounded backend error code, when a structured response supplied one.
    pub error_code: Option<String>,
    /// Whether this failure was eligible for a pre-output retry.
    pub retryable: bool,
    /// One-based attempt count for this logical request.
    pub attempt: u32,
    /// Stable nonsecret request ID reused across retries of this logical turn.
    pub logical_request_id: Option<String>,
    /// Whether any externally visible stream event preceded the failure.
    pub visible_stream_event: bool,
    /// Whether this logical request already forced an OAuth refresh.
    pub auth_refresh_attempted: bool,
    /// Provider-supplied subscription reset timestamp, in Unix seconds.
    pub quota_reset_at_unix_seconds: Option<u64>,
    /// Number of backend body bytes observed before classification.
    pub response_bytes: Option<usize>,
    /// Byte count of the request JSON.
    pub request_bytes: Option<usize>,
    /// Sanitized bounded backend diagnostic prefix, if safe to retain.
    pub response_prefix: Option<String>,
}

impl CodexErrorReport {
    /// Convert this diagnostic into Tea's persistable provider-error record.
    pub fn as_session_error(&self) -> tea_session::ProviderErrorRecord {
        tea_session::ProviderErrorRecord {
            source: self.source.as_str().to_owned(),
            message: Some(self.message.clone()),
            status_code: self.status_code,
            attempt: Some(self.attempt),
            logical_request_id: self.logical_request_id.clone(),
            visible_stream_event: Some(self.visible_stream_event),
            auth_refresh_attempted: Some(self.auth_refresh_attempted),
            quota_reset_at_unix_seconds: self.quota_reset_at_unix_seconds,
            error_type: self.error_type.clone(),
            error_code: self.error_code.clone(),
            retryable: Some(self.retryable),
            response_bytes: self.response_bytes.map(|value| value as u64),
            request_bytes: self.request_bytes.map(|value| value as u64),
            response_body: self.response_prefix.clone(),
        }
    }
}

impl fmt::Display for CodexErrorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "source={} message={:?} retryable={} attempt={}",
            self.source.as_str(),
            self.message,
            self.retryable,
            self.attempt
        )?;
        if let Some(status) = self.status_code {
            write!(formatter, " status_code={status}")?;
        }
        if let Some(error_type) = &self.error_type {
            write!(formatter, " error_type={error_type}")?;
        }
        if let Some(error_code) = &self.error_code {
            write!(formatter, " error_code={error_code}")?;
        }
        if let Some(reset) = self.quota_reset_at_unix_seconds {
            write!(formatter, " quota_reset_at_unix_seconds={reset}")?;
        }
        if let Some(bytes) = self.response_bytes {
            write!(formatter, " response_bytes={bytes}")?;
        }
        Ok(())
    }
}

/// Extract only bounded safe labels from a structured trusted backend error.
/// Message prose remains in the separately redacted diagnostic prefix and is
/// never promoted into a model-facing error string.
pub(super) fn backend_error_fields(bytes: &[u8]) -> (Option<String>, Option<String>, Option<u64>) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return (None, None, None);
    };
    let Ok(value) = JsonValue::parse(text) else {
        return (None, None, None);
    };
    let error = value.get("error").unwrap_or(&value);
    let error_type = error
        .get("type")
        .or_else(|| error.get("error_type"))
        .and_then(JsonValue::as_str)
        .and_then(bounded_label);
    let error_code = error
        .get("code")
        .or_else(|| value.get("code"))
        .and_then(JsonValue::as_str)
        .and_then(bounded_label);
    let quota_reset_at_unix_seconds = error
        .get("resets_at")
        .and_then(JsonValue::as_u64)
        .filter(|value| (946_684_800..=4_102_444_800).contains(value));
    (error_type, error_code, quota_reset_at_unix_seconds)
}

fn bounded_label(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    .then(|| value.to_owned())
}

/// Make a trusted backend error prefix safe for durable diagnostics.
pub(super) fn response_body_prefix(bytes: &[u8], secrets: &[&str]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let prefix = &bytes[..bytes.len().min(MAX_DIAGNOSTIC_RESPONSE_BYTES)];
    let mut text = String::from_utf8_lossy(prefix).into_owned();
    // Preserve structured diagnostic shape where possible while ensuring that
    // common secret-bearing fields never enter a durable provider record.
    if let Ok(mut value) = JsonValue::parse(&text) {
        redact_json_secret_fields(&mut value);
        if let Ok(redacted) = value.to_json_string() {
            text = redacted;
        }
    }
    let mut secrets = secrets
        .iter()
        .copied()
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "[redacted]");
        }
    }
    let text = text
        .chars()
        .map(|character| {
            if character.is_control() && character != '\n' && character != '\t' {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let text = redact_bearer_values(&text);
    let text = redact_jwt_like_values(&text);
    (!text.trim().is_empty()).then_some(text)
}

fn redact_json_secret_fields(value: &mut JsonValue) {
    match value {
        JsonValue::Array(values) => {
            for value in values {
                redact_json_secret_fields(value);
            }
        }
        JsonValue::Object(values) => {
            for (key, value) in values {
                if SENSITIVE_RESPONSE_FIELDS
                    .iter()
                    .any(|sensitive| key.eq_ignore_ascii_case(sensitive))
                {
                    *value = JsonValue::String("[redacted]".into());
                } else {
                    redact_json_secret_fields(value);
                }
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
    }
}

fn redact_bearer_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("bearer ") {
        let start = cursor + offset;
        let token_start = start + "bearer ".len();
        result.push_str(&text[cursor..token_start]);
        let token_end = text[token_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                matches!(
                    character,
                    ' ' | '\t' | '\r' | '\n' | '"' | '\'' | ',' | '}' | ']'
                )
                .then_some(token_start + offset)
            })
            .unwrap_or(text.len());
        if token_end == token_start {
            cursor = token_start;
            continue;
        }
        result.push_str("[redacted]");
        cursor = token_end;
    }
    result.push_str(&text[cursor..]);
    result
}

fn redact_jwt_like_values(text: &str) -> String {
    fn token_character(character: char) -> bool {
        character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
    }

    fn looks_like_jwt(candidate: &str) -> bool {
        let segments = candidate.split('.').collect::<Vec<_>>();
        candidate.len() >= 20
            && segments.len() == 3
            && segments.iter().all(|segment| segment.len() >= 4)
    }

    let mut result = String::with_capacity(text.len());
    let mut candidate = String::new();
    for character in text.chars() {
        if token_character(character) {
            candidate.push(character);
            continue;
        }
        if looks_like_jwt(&candidate) {
            result.push_str("[redacted]");
        } else {
            result.push_str(&candidate);
        }
        candidate.clear();
        result.push(character);
    }
    if looks_like_jwt(&candidate) {
        result.push_str("[redacted]");
    } else {
        result.push_str(&candidate);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_prefix_redacts_structured_and_unstructured_credentials() {
        let response = br#"{"access_token":"access-secret","account_id":"acct-secret","detail":"Bearer known-secret","nested":{"code_verifier":"verifier-secret","encrypted_content":"encrypted-reasoning-state"},"jwt":"aaaaaa.bbbbbb.cccccc"}"#;
        let prefix = response_body_prefix(response, &["known-secret", "acct-secret"])
            .expect("nonempty diagnostic prefix");
        for secret in [
            "access-secret",
            "acct-secret",
            "known-secret",
            "verifier-secret",
            "encrypted-reasoning-state",
            "aaaaaa.bbbbbb.cccccc",
        ] {
            assert!(!prefix.contains(secret), "diagnostic retained {secret}");
        }
        assert!(prefix.contains("[redacted]"));
    }

    #[test]
    fn extracts_only_safe_structured_backend_error_labels() {
        assert_eq!(
            backend_error_fields(
                br#"{"error":{"type":"invalid_request","code":"usage_limit_reached"}}"#
            ),
            (
                Some("invalid_request".into()),
                Some("usage_limit_reached".into()),
                None,
            )
        );
        assert_eq!(
            backend_error_fields(br#"{"error":{"type":"not safe","code":"bad\ncode"}}"#),
            (None, None, None),
        );
        assert_eq!(
            backend_error_fields(br#"{"error":{"resets_at":1704069000}}"#),
            (None, None, Some(1_704_069_000)),
        );
    }
}
