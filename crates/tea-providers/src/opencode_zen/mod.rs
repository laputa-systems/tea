//! OpenCode Zen Responses API provider adapter.
//!
//! This opt-in adapter sends a caller-provided OpenCode Zen API key through the native
//! rustls-backed HTTP boundary. It yields network-time SSE records through the caller-polled
//! model stream while the core remains independent of HTTP, credentials, and provider price
//! formats.

mod config;
mod payload;
mod response;

use crate::scheduler::{
    AdapterRequestObservation, CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture,
    ModelProvider, ModelRequest, ModelStreamEvent,
};
use crate::state::{StopReason, Usage};
use crate::transport_runtime::client as http_client;
pub use config::{OpencodeZenConfig, OpencodeZenConfigError};
use payload::build_payload;
use response::{OpencodeZenSseDecoder, opencode_zen_context_overflow, response_body_prefix};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tea_http::{
    TransportRequest as Request, TransportStream as HttpStream, TransportStreamEvent as StreamEvent,
};

pub(crate) const RESPONSES_URL: &str = "https://opencode.ai/zen/v1/responses";

/// The private source of the most recent OpenCode Zen failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpencodeZenErrorSource {
    /// The HTTP transport or its response boundary failed.
    Transport,
    /// OpenCode Zen returned a response that the adapter could not accept.
    Response,
    /// The adapter rejected a request before transport.
    Adapter,
}

impl OpencodeZenErrorSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Response => "response",
            Self::Adapter => "adapter",
        }
    }
}

/// Bounded diagnostic for the most recent OpenCode Zen failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpencodeZenErrorReport {
    /// Failure boundary that produced the report.
    pub source: OpencodeZenErrorSource,
    /// Stable local adapter message.
    pub message: String,
    /// HTTP status observed in the captured response headers.
    pub status_code: Option<u16>,
    /// Whether the adapter classified the failure as retryable.
    pub retryable: bool,
    /// One-based attempt number within the current completion request.
    pub attempt: u32,
    /// Total response body bytes captured before parsing.
    pub response_bytes: Option<usize>,
    /// Total request payload bytes sent to the provider.
    pub request_bytes: Option<usize>,
    /// Bounded, redacted response body prefix for trusted diagnostics.
    pub response_prefix: Option<String>,
}

impl OpencodeZenErrorReport {
    /// Convert this bounded diagnostic to the session's persistable provider-error shape.
    pub fn as_session_error(&self) -> tea_session::ProviderErrorRecord {
        tea_session::ProviderErrorRecord {
            source: self.source.as_str().to_owned(),
            message: Some(self.message.clone()),
            status_code: self.status_code,
            attempt: Some(self.attempt),
            error_type: None,
            error_code: None,
            retryable: Some(self.retryable),
            response_bytes: self.response_bytes.map(|v| v as u64),
            request_bytes: self.request_bytes.map(|v| v as u64),
            response_body: self.response_prefix.clone(),
        }
    }
}

impl fmt::Display for OpencodeZenErrorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "source={} message={:?} retryable={} attempt={}",
            self.source.as_str(),
            self.message,
            self.retryable,
            self.attempt
        )?;
        if let Some(status_code) = self.status_code {
            write!(formatter, " status_code={status_code}")?;
        }
        if let Some(response_bytes) = self.response_bytes {
            write!(formatter, " response_bytes={response_bytes}")?;
        }
        if let Some(request_bytes) = self.request_bytes {
            write!(formatter, " request_bytes={request_bytes}")?;
        }
        if let Some(response_prefix) = &self.response_prefix {
            write!(formatter, " response_prefix={response_prefix:?}")?;
        }
        Ok(())
    }
}

/// OpenCode Zen implementation of the generic [`ModelProvider`] port.
#[derive(Clone)]
pub struct OpencodeZenProvider {
    config: OpencodeZenConfig,
    accounting: Arc<Mutex<Usage>>,
    last_error: Arc<Mutex<Option<OpencodeZenErrorReport>>>,
}

/// One live OpenCode Zen SSE response.
struct OpencodeZenEventStream {
    provider: OpencodeZenProvider,
    response: Option<HttpStream>,
    decoder: Option<OpencodeZenSseDecoder>,
    pending: VecDeque<ModelStreamEvent>,
    status_code: Option<u16>,
    error_body: Vec<u8>,
    payload_bytes: usize,
}

impl OpencodeZenEventStream {
    fn start(
        provider: OpencodeZenProvider,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Self {
        provider.clear_error();
        let mut event_stream = Self {
            provider,
            response: None,
            decoder: None,
            pending: VecDeque::new(),
            status_code: None,
            error_body: Vec::new(),
            payload_bytes: 0,
        };
        if cancellation.is_cancelled() {
            event_stream
                .pending
                .push_back(ModelStreamEvent::End(StopReason::Cancelled));
            return event_stream;
        }
        if let Err(message) = event_stream.provider.validate_model(&request) {
            event_stream.adapter_failure(message);
            return event_stream;
        }
        let payload = match build_payload(&event_stream.provider.config, &request) {
            Ok(payload) => payload,
            Err(message) => {
                event_stream.adapter_failure(message);
                return event_stream;
            }
        };
        event_stream.payload_bytes = payload.len();
        event_stream
            .pending
            .push_back(ModelStreamEvent::RequestObservation(
                opencode_zen_request_observation(
                    &event_stream.provider.config,
                    &request,
                    payload.len(),
                ),
            ));
        // Headers mirror real opencode client (packages/opencode/src/session/llm/request.ts:187-204
        // and packages/opencode/src/provider/provider.ts BUNDLED_PROVIDERS for opencode).
        // The edge strips x-opencode-* for legacy providers but retains for new inference (console./inf.)
        // so we send minimal correlation headers without leaking workspace paths.
        event_stream.response = Some(
            http_client().stream(
                Request::post(
                    event_stream.provider.config.responses_url(),
                    payload,
                    event_stream.provider.config.request_timeout,
                )
                .header(
                    "Authorization",
                    format!("Bearer {}", event_stream.provider.config.api_key),
                )
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream")
                .header("User-Agent", "tea/1.0 opencode-zen")
                .header("x-opencode-client", "tea")
                .with_stall_timeout(event_stream.provider.config.stall_timeout),
                cancellation.clone(),
            ),
        );
        event_stream.decoder = Some(OpencodeZenSseDecoder::new());
        event_stream
    }

    fn adapter_failure(&mut self, message: String) {
        self.provider.record_error(OpencodeZenErrorReport {
            source: OpencodeZenErrorSource::Adapter,
            message: message.clone(),
            status_code: None,
            retryable: false,
            attempt: 0,
            response_bytes: None,
            request_bytes: None,
            response_prefix: None,
        });
        self.pending.push_back(ModelStreamEvent::ProviderError(
            self.provider
                .last_error_report()
                .expect("recorded adapter error")
                .as_session_error(),
        ));
        self.pending.push_back(ModelStreamEvent::Error { message });
    }

    fn transport_failure(&mut self, message: String, status_code: Option<u16>) {
        self.response = None;
        self.decoder = None;
        self.provider.record_error(OpencodeZenErrorReport {
            source: OpencodeZenErrorSource::Transport,
            message: message.clone(),
            status_code,
            retryable: false,
            attempt: 1,
            response_bytes: (!self.error_body.is_empty()).then_some(self.error_body.len()),
            request_bytes: Some(self.payload_bytes),
            response_prefix: (!self.error_body.is_empty()).then(|| {
                response_body_prefix(&self.error_body, Some(&self.provider.config.api_key))
            }),
        });
        self.pending.push_back(ModelStreamEvent::ProviderError(
            self.provider
                .last_error_report()
                .expect("recorded transport error")
                .as_session_error(),
        ));
        self.pending.push_back(ModelStreamEvent::Error { message });
    }

    fn response_failure(&mut self, message: String) {
        self.response = None;
        self.decoder = None;
        let context_overflow = opencode_zen_context_overflow(&self.error_body)
            || message == "OpenCode Zen context capacity exceeded";
        self.provider.record_error(OpencodeZenErrorReport {
            source: OpencodeZenErrorSource::Response,
            message: message.clone(),
            status_code: self.status_code,
            retryable: false,
            attempt: 1,
            response_bytes: Some(self.error_body.len()),
            request_bytes: Some(self.payload_bytes),
            response_prefix: Some(response_body_prefix(
                &self.error_body,
                Some(&self.provider.config.api_key),
            )),
        });
        self.pending.push_back(ModelStreamEvent::ProviderError(
            self.provider
                .last_error_report()
                .expect("recorded response error")
                .as_session_error(),
        ));
        self.pending.push_back(if context_overflow {
            ModelStreamEvent::ContextOverflow { message }
        } else {
            ModelStreamEvent::Error { message }
        });
    }

    fn finish_sse(&mut self) {
        self.response = None;
        let Some(decoder) = self.decoder.take() else {
            self.response_failure("OpenCode Zen response stream was not initialized".into());
            return;
        };
        match decoder.finish(false) {
            Ok(completion) => {
                let usage = completion.usage.clone();
                let events = completion.events;
                if usage.is_reported() {
                    // Record usage before emitting events to keep accounting consistent
                    self.provider.record_usage(usage.clone());
                    // Ensure usage event includes cost if present (free model cost is 0)
                }
                self.pending.extend(events);
            }
            Err(message) => self.response_failure(message),
        }
    }

    fn poll_next_event(
        &mut self,
        context: &mut Context<'_>,
        cancellation: CancellationToken,
    ) -> Poll<Result<Option<ModelStreamEvent>, crate::error::SchedulerError>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Poll::Ready(Ok(Some(event)));
            }
            if cancellation.is_cancelled() {
                self.response = None;
                self.decoder = None;
                return Poll::Ready(Ok(Some(ModelStreamEvent::End(StopReason::Cancelled))));
            }
            let Some(response) = self.response.as_mut() else {
                return Poll::Ready(Ok(None));
            };
            match response.poll_next(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(StreamEvent::Response { status_code }) => {
                    self.status_code = Some(status_code);
                }
                Poll::Ready(StreamEvent::Chunk(bytes)) => {
                    if self
                        .status_code
                        .is_some_and(|status| !(200..300).contains(&status))
                    {
                        self.error_body.extend_from_slice(&bytes);
                        continue;
                    }
                    let Some(decoder) = self.decoder.as_mut() else {
                        self.response_failure(
                            "OpenCode Zen response stream was not initialized".into(),
                        );
                        continue;
                    };
                    match decoder.push(&bytes) {
                        Ok(events) => self.pending.extend(events),
                        Err(message) => self.response_failure(message),
                    }
                }
                Poll::Ready(StreamEvent::End) => {
                    if self
                        .status_code
                        .is_some_and(|status| !(200..300).contains(&status))
                    {
                        self.response_failure("OpenCode Zen rejected the request".into());
                    } else {
                        self.finish_sse();
                    }
                }
                Poll::Ready(StreamEvent::Failure(failure)) => {
                    if cancellation.is_cancelled() || failure.message == "HTTP request cancelled" {
                        self.response = None;
                        self.decoder = None;
                        self.pending
                            .push_back(ModelStreamEvent::End(StopReason::Cancelled));
                    } else {
                        self.transport_failure(
                            format!(
                                "OpenCode Zen HTTP transport failed{}: {}",
                                failure
                                    .status_code
                                    .map(|status| format!(" with status {status}"))
                                    .unwrap_or_default(),
                                failure.message
                            ),
                            failure.status_code,
                        );
                    }
                }
            }
        }
    }
}

impl ModelEventStream for OpencodeZenEventStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(std::future::poll_fn(move |context| {
            self.poll_next_event(context, cancellation.clone())
        }))
    }
}

fn opencode_zen_request_observation(
    config: &OpencodeZenConfig,
    request: &ModelRequest,
    serialized_request_bytes: usize,
) -> AdapterRequestObservation {
    let mut components = BTreeMap::<String, u64>::new();
    components.insert(
        "adapter".into(),
        stable_fingerprint(b"opencode-zen-responses/v1"),
    );
    components.insert(
        "output_token_limit".into(),
        stable_fingerprint(format!("{:?}", config.max_tokens).as_bytes()),
    );
    components.insert(
        "reasoning_encoding".into(),
        stable_fingerprint(
            payload::reasoning_effort(request.thinking_level)
                .unwrap_or("omitted")
                .as_bytes(),
        ),
    );
    components.insert(
        "tool_transport".into(),
        stable_fingerprint(if request.tools.is_empty() {
            b"no-tools"
        } else {
            b"function-tools"
        }),
    );
    let mut domain_bytes = Vec::new();
    for (name, fingerprint) in &components {
        domain_bytes.extend_from_slice(name.as_bytes());
        domain_bytes.push(0);
        domain_bytes.extend_from_slice(&fingerprint.to_le_bytes());
    }
    AdapterRequestObservation {
        deterministic_common_prefix_bytes: None,
        deterministic_common_prefix_tokens_estimate: None,
        serialized_request_bytes: Some(serialized_request_bytes),
        cache_domain_fingerprint: Some(stable_fingerprint(&domain_bytes)),
        cache_domain_components: components,
        provider_request_id: None,
    }
}

fn stable_fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

impl OpencodeZenProvider {
    /// Construct an adapter from explicit caller-owned configuration.
    pub fn new(config: OpencodeZenConfig) -> Self {
        Self {
            config,
            accounting: Arc::new(Mutex::new(Usage::default())),
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Return the most recent adapter or provider failure observed by this provider.
    pub fn last_error_report(&self) -> Option<OpencodeZenErrorReport> {
        self.last_error
            .lock()
            .expect("OpencodeZen error mutex poisoned")
            .clone()
    }

    /// Return aggregate portable token usage across settled turns.
    pub fn usage_snapshot(&self) -> Usage {
        self.accounting
            .lock()
            .expect("OpencodeZen accounting mutex poisoned")
            .clone()
    }

    fn record_usage(&self, usage: Usage) {
        let mut accounting = self
            .accounting
            .lock()
            .expect("OpencodeZen accounting mutex poisoned");
        // Merge usage fields saturating
        if let Some(v) = usage.input_tokens {
            accounting.input_tokens = Some(accounting.input_tokens.unwrap_or(0).saturating_add(v));
        }
        if let Some(v) = usage.output_tokens {
            accounting.output_tokens =
                Some(accounting.output_tokens.unwrap_or(0).saturating_add(v));
        }
        if let Some(v) = usage.reasoning_tokens {
            accounting.reasoning_tokens =
                Some(accounting.reasoning_tokens.unwrap_or(0).saturating_add(v));
        }
        if let Some(v) = usage.cache_read_tokens {
            accounting.cache_read_tokens =
                Some(accounting.cache_read_tokens.unwrap_or(0).saturating_add(v));
        }
        if let Some(v) = usage.cache_write_tokens {
            accounting.cache_write_tokens =
                Some(accounting.cache_write_tokens.unwrap_or(0).saturating_add(v));
        }
        // Cost handling: if usage.cost is Some, replace/accumulate? For free model cost is "0"
        // Keep last cost for snapshot? For simplicity not accumulating decimal string.
    }

    fn validate_model(&self, request: &ModelRequest) -> Result<(), String> {
        self.config.validate().map_err(|e| e.to_string())?;
        let model = request
            .model
            .as_ref()
            .ok_or_else(|| "OpenCode Zen request omitted its exact model descriptor".to_owned())?;
        if model.provider != "opencode-zen" || model.model != self.config.model {
            return Err(format!(
                "OpenCode Zen configuration does not match requested model: expected opencode-zen/{}, got {}/{}",
                self.config.model, model.provider, model.model
            ));
        }
        Ok(())
    }

    fn record_error(&self, report: OpencodeZenErrorReport) {
        *self
            .last_error
            .lock()
            .expect("OpencodeZen error mutex poisoned") = Some(report);
    }

    fn clear_error(&self) {
        *self
            .last_error
            .lock()
            .expect("OpencodeZen error mutex poisoned") = None;
    }
}

impl fmt::Debug for OpencodeZenProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpencodeZenProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ModelProvider for OpencodeZenProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = OpencodeZenEventStream::start(self.clone(), request, cancellation);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::JsonValue;
    use crate::state::{ModelDescriptor, ThinkingLevel};
    
    use std::time::Duration;

    #[test]
    fn request_timeout_is_explicit() {
        let config = OpencodeZenConfig::new("key", "model");
        assert_eq!(config.request_timeout(), Duration::from_secs(300));
        assert_eq!(config.stall_timeout(), Duration::from_secs(60));
        assert_eq!(
            config
                .clone()
                .with_request_timeout(Duration::from_secs(42))
                .request_timeout(),
            Duration::from_secs(42)
        );
        assert_eq!(
            OpencodeZenConfig::new("key", "model")
                .with_stall_timeout(Duration::from_secs(7))
                .stall_timeout(),
            Duration::from_secs(7)
        );
        assert_eq!(
            OpencodeZenConfig::new("key", "model")
                .with_stall_timeout(Duration::ZERO)
                .validate(),
            Err(OpencodeZenConfigError::ZeroStallTimeout)
        );
    }

    #[test]
    fn builds_responses_payload_with_tools_and_reasoning() {
        let config = OpencodeZenConfig::try_new("key", "muse-spark-1.2-contributor-free").unwrap();
        let payload = build_payload(
            &config.with_max_tokens(4096),
            &ModelRequest {
                system_prompt: "system".into(),
                context: "[]".into(),
                model: Some(ModelDescriptor {
                    provider: "opencode-zen".into(),
                    model: "muse-spark-1.2-contributor-free".into(),
                    revision: None,
                }),
                thinking_level: ThinkingLevel::High,
                ..ModelRequest::default()
            },
        )
        .unwrap();
        let payload = JsonValue::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
        assert_eq!(
            payload.get("model").and_then(JsonValue::as_str),
            Some("muse-spark-1.2-contributor-free")
        );
        assert_eq!(
            payload.get("stream").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            payload
                .get("reasoning")
                .and_then(|v| v.get("effort"))
                .and_then(JsonValue::as_str),
            Some("high")
        );
        // input should contain system prompt
        let input = payload.get("input").and_then(JsonValue::as_array).unwrap();
        assert!(
            input
                .iter()
                .any(|m| m.get("role").and_then(JsonValue::as_str) == Some("system"))
        );
    }

    #[test]
    fn leaves_output_length_to_provider_when_no_cap() {
        let config = OpencodeZenConfig::try_new("key", "muse-spark-1.2-contributor-free").unwrap();
        let payload = build_payload(
            &config,
            &ModelRequest {
                context: "[]".into(),
                ..ModelRequest::default()
            },
        )
        .unwrap();
        let payload = JsonValue::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
        assert!(payload.get("max_output_tokens").is_none());
    }

    #[test]
    fn parses_responses_sse_text_and_tool_call() {
        let mut decoder = OpencodeZenSseDecoder::new();
        let chunk1 = b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        let events = decoder.push(chunk1).expect("first delta parses");
        assert_eq!(events, vec![ModelStreamEvent::TextDelta("hello".into())]);

        let chunk2 = b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_123\",\"type\":\"function_call\",\"name\":\"read\",\"call_id\":\"call_123\",\"arguments\":\"\"}}\n\n";
        let events = decoder.push(chunk2).expect("tool added");
        assert!(events.is_empty()); // buffered until finish

        let chunk3 = b"event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_123\",\"delta\":\"{\\\"path\\\":\"}\n\n";
        let events = decoder.push(chunk3).unwrap();
        assert!(events.is_empty());

        let chunk_done = b"event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_123\",\"type\":\"function_call\",\"name\":\"read\",\"call_id\":\"call_123\",\"arguments\":\"{\\\"path\\\":\\\"ZERO.md\\\"}\"}}\n\n";
        let events = decoder.push(chunk_done).expect("done");
        // tool call is now buffered until finish to ensure correct stop reason
        assert!(events.is_empty());

        let chunk_completed = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_123\",\"model\":\"muse-spark-1.2-contributor-free\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n";
        let events = decoder.push(chunk_completed).unwrap();
        assert!(events.is_empty()); // usage and tool deferred to finish
        let complete = decoder.finish(false).expect("finish");
        assert!(complete
            .events
            .iter()
            .any(|e| matches!(e, ModelStreamEvent::ToolCall(call) if call.name == "read" && call.id.as_str() == "call_123")));
        assert!(
            complete
                .events
                .iter()
                .any(|e| matches!(e, ModelStreamEvent::Usage(u) if u.input_tokens==Some(10)))
        );
        assert!(
            complete
                .events
                .iter()
                .any(|e| matches!(e, ModelStreamEvent::End(_)))
        );
        // Ensure tool call precedes usage and end
        let tool_pos = complete
            .events
            .iter()
            .position(|e| matches!(e, ModelStreamEvent::ToolCall(_)))
            .unwrap();
        let usage_pos = complete
            .events
            .iter()
            .position(|e| matches!(e, ModelStreamEvent::Usage(_)))
            .unwrap();
        let end_pos = complete
            .events
            .iter()
            .position(|e| matches!(e, ModelStreamEvent::End(_)))
            .unwrap();
        assert!(tool_pos < usage_pos && usage_pos < end_pos);
    }
}
