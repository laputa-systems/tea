//! OpenRouter HTTP transport and response-boundary classification.
//!
//! The finite transport helpers remain regression-fixture coverage for the retired buffering
//! path; the live adapter consumes the incremental `tea-http` stream.

#![allow(dead_code)]

use crate::transport_runtime::client as http_client;
use crate::scheduler::CancellationToken;
use tea_http::TransportRequest as Request;

pub(super) const COMPLETIONS_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub(super) const GENERATION_URL: &str = "https://openrouter.ai/api/v1/generation";

#[derive(Debug)]
pub(super) struct TransportResponse {
    pub(super) body: Vec<u8>,
    pub(super) status_code: Option<u16>,
    pub(super) partial: bool,
}

/// Execute one OpenRouter request and preserve the adapter's partial-response boundary.
///
/// The native client keeps credentials in request headers rather than argv, environment, or
/// temporary files. OpenRouter's configured stall timeout is applied to response headers and
/// body reads by the shared transport; a timed-out partial body is handed to the existing
/// partial SSE parser only when it contains meaningful provider data.
pub(super) fn run_http(
    request: Request,
    cancellation: &CancellationToken,
) -> Result<TransportResponse, String> {
    match http_client().send_blocking(request, cancellation) {
        Ok(response) => Ok(TransportResponse {
            body: response.body,
            status_code: Some(response.status_code),
            partial: false,
        }),
        Err(failure)
            if failure.message == "HTTP request cancelled" || cancellation.is_cancelled() =>
        {
            Err("OpenRouter HTTP transport cancelled".into())
        }
        Err(failure) if failure.is_stall() => {
            let bytes = failure.body.len();
            if response_bytes_meaningful(&failure.body) {
                Ok(TransportResponse {
                    body: failure.body,
                    status_code: failure.status_code,
                    partial: true,
                })
            } else {
                Err(format!(
                    "OpenRouter HTTP transport stalled after {bytes} response bytes without meaningful progress"
                ))
            }
        }
        Err(failure) if response_bytes_meaningful(&failure.body) => {
            if failure.status_code.is_none() {
                return Ok(TransportResponse {
                    body: failure.body,
                    status_code: None,
                    partial: true,
                });
            }
            Err(format!(
                "OpenRouter HTTP transport failed after {} response bytes: {}",
                failure.body.len(),
                failure.message
            ))
        }
        Err(failure) => Err(format!(
            "OpenRouter HTTP transport failed before a provider response: {}",
            failure.message
        )),
    }
}

/// Retry only failures that occurred before the provider emitted response bytes.
///
/// Once a completion has produced any body bytes, replaying the request can charge the
/// provider twice and repeats a potentially pathological generation. A zero-byte stall and
/// connection failure remain safe bounded retry cases.
pub(super) fn retryable_transport_error(message: &str) -> bool {
    if message.contains("before a provider response") {
        return true;
    }
    message
        .strip_prefix("OpenRouter HTTP transport stalled after ")
        .and_then(|rest| rest.split_once(" response bytes"))
        .and_then(|(bytes, _)| bytes.parse::<u64>().ok())
        == Some(0)
}

fn response_bytes_meaningful(bytes: &[u8]) -> bool {
    bytes.split(|byte| *byte == b'\n').any(|line| {
        let line = line.trim_ascii();
        !line.is_empty() && !line.starts_with(b":")
    })
}

#[cfg(test)]
mod tests {
    use super::{response_bytes_meaningful, retryable_transport_error};

    #[test]
    fn ignores_whitespace_and_sse_comments_as_progress() {
        assert!(!response_bytes_meaningful(
            b"   \n: OPENROUTER PROCESSING\n"
        ));
        assert!(response_bytes_meaningful(b"data: {\"id\":\"x\"}\n"));
    }

    #[test]
    fn retries_only_failures_before_response_bytes() {
        assert!(retryable_transport_error(
            "OpenRouter HTTP transport failed before a provider response: HTTP request failed"
        ));
        assert!(retryable_transport_error(
            "OpenRouter HTTP transport stalled after 0 response bytes without meaningful progress"
        ));
        assert!(!retryable_transport_error(
            "OpenRouter HTTP transport failed after 32768 response bytes: HTTP response body read failed"
        ));
        assert!(!retryable_transport_error(
            "OpenRouter HTTP transport stalled after 32768 response bytes without meaningful progress"
        ));
    }
}
