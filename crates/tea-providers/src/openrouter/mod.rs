//! OpenRouter Chat Completions provider adapter.
//!
//! This opt-in adapter sends a caller-provided OpenRouter API key through the native
//! rustls-backed HTTP boundary. It yields network-time SSE records through the caller-polled
//! model stream while the core remains independent of HTTP, credentials, and provider price
//! formats.

mod accounting;
mod config;
mod payload;
mod response;
mod transport;

use super::http::{HttpStream, Request, StreamEvent, stream};
use super::retry::{RetryableError, retry_with_backoff};
use crate::scheduler::{
    AdapterRequestObservation, CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture,
    ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use crate::state::{StopReason, Usage};
use accounting::{Accounting, add_usage};
pub use accounting::{OpenRouterCostReport, OpenRouterCostSource, OpenRouterCostTurn};
pub use config::{OpenRouterConfig, OpenRouterConfigError};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
#[cfg(test)]
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use payload::build_payload;
#[cfg(test)]
use response::exact_number_at_path;
use response::{
    StreamingSseDecoder, decimal_add, openrouter_context_overflow, openrouter_response_retryable,
    openrouter_status_retryable, parse_generation_cost, parse_partial_response, parse_response,
    response_body_prefix, unavailable_cost,
};
use transport::{COMPLETIONS_URL, GENERATION_URL, retryable_transport_error, run_http};

/// The private source of the most recent OpenRouter failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRouterErrorSource {
    /// The HTTP transport or its response boundary failed.
    Transport,
    /// OpenRouter returned a response that the adapter could not accept.
    Response,
    /// The adapter rejected a request before transport.
    Adapter,
}

impl OpenRouterErrorSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Response => "response",
            Self::Adapter => "adapter",
        }
    }
}

/// Bounded diagnostic for the most recent OpenRouter failure.
///
/// The agent-facing stream error remains a stable adapter message. Hosts that own a private
/// diagnostic sink can use this report to distinguish transport, HTTP, and response-shape
/// failures without retaining the API key or an unbounded provider body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenRouterErrorReport {
    /// Failure boundary that produced the report.
    pub source: OpenRouterErrorSource,
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

impl fmt::Display for OpenRouterErrorReport {
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

/// OpenRouter implementation of the generic [`ModelProvider`] port.
#[derive(Clone)]
pub struct OpenRouterProvider {
    config: OpenRouterConfig,
    accounting: Arc<Mutex<Accounting>>,
    last_error: Arc<Mutex<Option<OpenRouterErrorReport>>>,
}

/// One live OpenRouter SSE response. The HTTP worker owns blocking body reads while this source
/// remains caller-polled through the core's provider-neutral stream boundary.
struct OpenRouterEventStream {
    provider: OpenRouterProvider,
    response: Option<HttpStream>,
    decoder: Option<StreamingSseDecoder>,
    pending: VecDeque<ModelStreamEvent>,
    status_code: Option<u16>,
    error_body: Vec<u8>,
    payload_bytes: usize,
}

impl OpenRouterEventStream {
    fn start(
        provider: OpenRouterProvider,
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
                openrouter_request_observation(
                    &event_stream.provider.config,
                    &request,
                    payload.len(),
                ),
            ));
        event_stream.response = Some(stream(
            Request::post(
                event_stream.provider.config.completion_url(),
                payload,
                event_stream.provider.config.request_timeout,
            )
            .header(
                "Authorization",
                format!("Bearer {}", event_stream.provider.config.api_key),
            )
            .header("Content-Type", "application/json")
            .with_stall_timeout(event_stream.provider.config.stall_timeout),
            &cancellation,
        ));
        event_stream.decoder = Some(StreamingSseDecoder::new());
        event_stream
    }

    fn adapter_failure(&mut self, message: String) {
        self.provider.record_error(OpenRouterErrorReport {
            source: OpenRouterErrorSource::Adapter,
            message: message.clone(),
            status_code: None,
            retryable: false,
            attempt: 0,
            response_bytes: None,
            request_bytes: None,
            response_prefix: None,
        });
        self.pending.push_back(ModelStreamEvent::Error { message });
    }

    fn transport_failure(&mut self, message: String, status_code: Option<u16>) {
        self.response = None;
        self.decoder = None;
        self.provider.record_error(OpenRouterErrorReport {
            source: OpenRouterErrorSource::Transport,
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
        self.pending.push_back(ModelStreamEvent::Error { message });
    }

    fn response_failure(&mut self, message: String) {
        self.response = None;
        self.decoder = None;
        let context_overflow = openrouter_context_overflow(&self.error_body)
            || message == "OpenRouter context capacity exceeded";
        self.provider.record_error(OpenRouterErrorReport {
            source: OpenRouterErrorSource::Response,
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
        self.pending.push_back(if context_overflow {
            ModelStreamEvent::ContextOverflow { message }
        } else {
            ModelStreamEvent::Error { message }
        });
    }

    fn finish_sse(&mut self) {
        self.response = None;
        let Some(decoder) = self.decoder.take() else {
            self.response_failure("OpenRouter response stream was not initialized".into());
            return;
        };
        match decoder.finish(false) {
            Ok(completion) => {
                let mut usage = completion.usage;
                let cost = completion
                    .inline_cost
                    .unwrap_or_else(|| unavailable_cost(&usage, &self.provider.config.model));
                usage.cache_read_tokens = usage.cache_read_tokens.or(cost.cache_read_tokens);
                usage.cache_write_tokens = usage.cache_write_tokens.or(cost.cache_write_tokens);
                usage.cost = cost.total_usd_exact.clone();
                let mut events = completion.events;
                for event in &mut events {
                    if let ModelStreamEvent::Usage(event_usage) = event {
                        *event_usage = usage.clone();
                    }
                }
                if usage.is_reported() {
                    self.provider.record(usage, cost);
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
                            "OpenRouter response stream was not initialized".into(),
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
                        self.response_failure("OpenRouter rejected the request".into());
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
                                "OpenRouter HTTP transport failed{}: {}",
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

impl ModelEventStream for OpenRouterEventStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(std::future::poll_fn(move |context| {
            self.poll_next_event(context, cancellation.clone())
        }))
    }
}

/// Return content-safe facts from the same OpenRouter payload bytes handed to
/// transport. The payload itself is never retained in this observation.
fn openrouter_request_observation(
    config: &OpenRouterConfig,
    request: &ModelRequest,
    serialized_request_bytes: usize,
) -> AdapterRequestObservation {
    let mut components = BTreeMap::<String, u64>::new();
    components.insert(
        "adapter".into(),
        stable_fingerprint(b"openrouter-chat-completions/v1"),
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
            b"function-tools-require-parameters"
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

impl OpenRouterProvider {
    /// Construct an adapter from explicit caller-owned configuration.
    pub fn new(config: OpenRouterConfig) -> Self {
        Self {
            config,
            accounting: Arc::new(Mutex::new(Accounting::default())),
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Return the most recent adapter or provider failure observed by this provider.
    pub fn last_error_report(&self) -> Option<OpenRouterErrorReport> {
        self.last_error
            .lock()
            .expect("OpenRouter error mutex poisoned")
            .clone()
    }

    /// Return aggregate portable token usage across settled OpenRouter turns.
    pub fn usage_snapshot(&self) -> Usage {
        self.accounting
            .lock()
            .expect("OpenRouter accounting mutex poisoned")
            .usage
            .clone()
    }

    /// Return a redacted snapshot of provider-reported cost accounting.
    pub fn cost_report(&self) -> OpenRouterCostReport {
        let accounting = self
            .accounting
            .lock()
            .expect("OpenRouter accounting mutex poisoned");
        let reported = accounting
            .costs
            .iter()
            .filter(|turn| turn.total_usd_exact.is_some())
            .count();
        let total = accounting
            .costs
            .iter()
            .filter_map(|turn| turn.total_usd)
            .sum::<f64>();
        let inference = accounting
            .costs
            .iter()
            .filter_map(|turn| turn.upstream_inference_usd)
            .sum::<f64>();
        let exact_total = accounting
            .costs
            .iter()
            .filter_map(|turn| turn.total_usd_exact.as_deref())
            .fold(None, |sum, value| Some(decimal_add(sum.as_deref(), value)));
        let exact_inference = accounting
            .costs
            .iter()
            .filter_map(|turn| turn.upstream_inference_usd_exact.as_deref())
            .fold(None, |sum, value| Some(decimal_add(sum.as_deref(), value)));
        OpenRouterCostReport {
            reported_turn_count: reported,
            unavailable_turn_count: accounting.costs.len().saturating_sub(reported),
            complete: reported == accounting.costs.len(),
            reported_total_usd: if total == 0.0 { 0.0 } else { total },
            reported_total_usd_exact: exact_total,
            reported_upstream_inference_usd: if inference == 0.0 { 0.0 } else { inference },
            reported_upstream_inference_usd_exact: exact_inference,
            turns: accounting.costs.clone(),
        }
    }

    #[allow(dead_code)]
    fn response_stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelStream {
        if cancellation.is_cancelled() {
            return ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Cancelled)],
            };
        }
        match self.complete(request, &cancellation) {
            Ok((mut events, mut usage, cost)) => {
                usage.cache_read_tokens = usage.cache_read_tokens.or(cost.cache_read_tokens);
                usage.cache_write_tokens = usage.cache_write_tokens.or(cost.cache_write_tokens);
                usage.cost = cost.total_usd_exact.clone();
                self.record(usage.clone(), cost);
                if usage.is_reported() {
                    // v1 streams cannot deliver any event after `End`; usage is part of the
                    // provider response and must precede the terminal settlement event.
                    let terminal = events
                        .pop()
                        .expect("parsed OpenRouter response has terminal event");
                    events.push(ModelStreamEvent::Usage(usage));
                    events.push(terminal);
                }
                ModelStream { events }
            }
            Err(_message) if cancellation.is_cancelled() => ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Cancelled)],
            },
            Err(message) => ModelStream {
                events: vec![ModelStreamEvent::Error { message }],
            },
        }
    }

    fn record(&self, usage: Usage, mut cost: OpenRouterCostTurn) {
        let mut accounting = self
            .accounting
            .lock()
            .expect("OpenRouter accounting mutex poisoned");
        add_usage(&mut accounting.usage.input_tokens, usage.input_tokens);
        add_usage(&mut accounting.usage.output_tokens, usage.output_tokens);
        add_usage(
            &mut accounting.usage.reasoning_tokens,
            usage.reasoning_tokens,
        );
        add_usage(
            &mut accounting.usage.cache_read_tokens,
            usage.cache_read_tokens,
        );
        add_usage(
            &mut accounting.usage.cache_write_tokens,
            usage.cache_write_tokens,
        );
        if let Some(cost) = usage.cost.as_deref() {
            accounting.usage.cost = Some(match accounting.usage.cost.as_deref() {
                Some(previous) => decimal_add(Some(previous), cost),
                None => cost.to_owned(),
            });
        }
        cost.turn = accounting.costs.len() + 1;
        accounting.costs.push(cost);
    }

    fn validate_model(&self, request: &ModelRequest) -> Result<(), String> {
        self.config.validate().map_err(|error| error.to_string())?;
        let model = request
            .model
            .as_ref()
            .ok_or_else(|| "OpenRouter request omitted its exact model descriptor".to_owned())?;
        if model.provider != "openrouter" || model.model != self.config.model {
            return Err(format!(
                "OpenRouter configuration does not match requested model: expected openrouter/{}, got {}/{}",
                self.config.model, model.provider, model.model
            ));
        }
        Ok(())
    }

    #[allow(dead_code)]
    fn complete(
        &self,
        request: ModelRequest,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<ModelStreamEvent>, Usage, OpenRouterCostTurn), String> {
        self.clear_error();
        self.validate_model(&request).inspect_err(|message| {
            self.record_error(OpenRouterErrorReport {
                source: OpenRouterErrorSource::Adapter,
                message: message.clone(),
                status_code: None,
                retryable: false,
                attempt: 0,
                response_bytes: None,
                request_bytes: None,
                response_prefix: None,
            });
        })?;
        let payload = build_payload(&self.config, &request).inspect_err(|message| {
            self.record_error(OpenRouterErrorReport {
                source: OpenRouterErrorSource::Adapter,
                message: message.clone(),
                status_code: None,
                retryable: false,
                attempt: 0,
                response_bytes: None,
                request_bytes: None,
                response_prefix: None,
            });
        })?;
        let mut attempts: u32 = 0;
        let parsed = retry_with_backoff(self.config.retry_policy, cancellation, || {
            attempts = attempts.saturating_add(1);
            let request = Request::post(
                COMPLETIONS_URL,
                payload.clone(),
                self.config.request_timeout,
            )
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .with_stall_timeout(self.config.stall_timeout);
            let output = run_http(request, cancellation);
            let output = output.map_err(|message| {
                let retryable = !cancellation.is_cancelled() && retryable_transport_error(&message);
                self.record_error(OpenRouterErrorReport {
                    source: OpenRouterErrorSource::Transport,
                    message: message.clone(),
                    status_code: None,
                    retryable,
                    attempt: attempts,
                    response_bytes: None,
                    request_bytes: Some(payload.len()),
                    response_prefix: None,
                });
                RetryableError { retryable, message }
            })?;
            let retryable = openrouter_status_retryable(output.status_code)
                || openrouter_response_retryable(&output.body);
            let parse = if output.partial {
                parse_partial_response(&output.body)
            } else {
                parse_response(&output.body)
            };
            parse.map_err(|message| {
                let message = message.replace(&self.config.api_key, "[redacted]");
                self.record_error(OpenRouterErrorReport {
                    source: OpenRouterErrorSource::Response,
                    message: message.clone(),
                    status_code: output.status_code,
                    retryable,
                    attempt: attempts,
                    response_bytes: Some(output.body.len()),
                    request_bytes: Some(payload.len()),
                    response_prefix: Some(response_body_prefix(
                        &output.body,
                        Some(&self.config.api_key),
                    )),
                });
                RetryableError { retryable, message }
            })
        })?;
        if cancellation.is_cancelled() {
            return Err("OpenRouter HTTP transport cancelled".into());
        }
        self.clear_error();
        // The completion's own `usage.cost` is the immediate accounting source. Query the
        // generation endpoint only when that provider field is absent: this avoids adding a
        // retention-sensitive metadata round trip to ordinary model turns.
        let cost = parsed
            .inline_cost
            .or_else(|| {
                parsed.generation_id.as_deref().and_then(|generation_id| {
                    self.generation_cost(generation_id, &parsed.usage, cancellation)
                })
            })
            .unwrap_or_else(|| unavailable_cost(&parsed.usage, &self.config.model));
        Ok((parsed.events, parsed.usage, cost))
    }

    /// Fetch redacted accounting metadata after a completion only if chat usage omitted cost.
    #[allow(dead_code)]
    fn generation_cost(
        &self,
        generation_id: &str,
        usage: &Usage,
        cancellation: &CancellationToken,
    ) -> Option<OpenRouterCostTurn> {
        for attempt in 0..=self.config.retry_policy.max_retries() {
            if cancellation.is_cancelled() {
                return None;
            }
            let request = Request::get(GENERATION_URL, std::time::Duration::from_secs(15))
                .query("id", generation_id)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .header("Content-Type", "application/json")
                .with_stall_timeout(self.config.stall_timeout);
            if let Ok(output) = run_http(request, cancellation)
                && let Some(cost) = parse_generation_cost(&output.body, usage)
            {
                return Some(cost);
            }
            if attempt < self.config.retry_policy.max_retries()
                && !super::retry::wait_with_cancellation(
                    self.config.retry_policy.delay_before_retry(attempt),
                    cancellation,
                )
            {
                return None;
            }
        }
        None
    }

    fn record_error(&self, report: OpenRouterErrorReport) {
        *self
            .last_error
            .lock()
            .expect("OpenRouter error mutex poisoned") = Some(report);
    }

    fn clear_error(&self) {
        *self
            .last_error
            .lock()
            .expect("OpenRouter error mutex poisoned") = None;
    }
}

impl fmt::Debug for OpenRouterProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenRouterProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ModelProvider for OpenRouterProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = OpenRouterEventStream::start(self.clone(), request, cancellation);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::JsonValue;
    use crate::state::{AgentToolCall, ModelDescriptor, SerializedJson, ThinkingLevel, ToolCallId};
    use crate::tool::{ToolDefinition, ToolExecutionMode};
    use std::time::Duration;

    #[test]
    fn request_timeout_is_explicit_and_not_thirty_seconds() {
        let config = OpenRouterConfig::new("key", "model");
        assert_eq!(config.request_timeout(), Duration::from_secs(300));
        assert_eq!(config.stall_timeout(), Duration::from_secs(60));
        assert_eq!(
            config
                .with_request_timeout(Duration::from_secs(42))
                .request_timeout(),
            Duration::from_secs(42)
        );
        assert_eq!(
            OpenRouterConfig::new("key", "model")
                .with_stall_timeout(Duration::from_secs(7))
                .stall_timeout(),
            Duration::from_secs(7)
        );
        assert_eq!(
            OpenRouterConfig::new("key", "model")
                .with_stall_timeout(Duration::ZERO)
                .validate(),
            Err(OpenRouterConfigError::ZeroStallTimeout)
        );
    }

    #[test]
    fn parses_redacted_generation_cost_without_retaining_identifier() {
        let usage = Usage {
            input_tokens: Some(10),
            output_tokens: Some(3),
            reasoning_tokens: None,
            ..Usage::default()
        };
        let cost = parse_generation_cost(
            br#"{
                "data": {
                    "id": "gen_must_not_be_written_to_artifacts",
                    "total_cost": 0.0000123,
                    "upstream_inference_cost": 0.00001,
                    "model": "poolside/laguna-xs-2.1:free",
                    "provider_name": "Poolside",
                    "tokens_prompt": 12,
                    "tokens_completion": 4,
                    "tokens_cached": 2,
                    "tokens_reasoning": 1
                }
            }"#,
            &usage,
        )
        .expect("provider cost is parsed");
        assert_eq!(cost.source, OpenRouterCostSource::Generation);
        assert_eq!(cost.total_usd, Some(0.0000123));
        assert_eq!(cost.total_usd_exact.as_deref(), Some("0.0000123"));
        assert_eq!(
            cost.upstream_inference_usd_exact.as_deref(),
            Some("0.00001")
        );
        assert_eq!(cost.provider.as_deref(), Some("Poolside"));
        assert!(!format!("{cost:?}").contains("gen_must_not_be_written_to_artifacts"));
    }

    #[test]
    fn chat_usage_cost_is_preferred_without_generation_metadata() {
        let bytes = br#"{
                "id": "gen_example",
                "model": "poolside/laguna-xs-2.1:free",
                "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 2, "completion_tokens": 1, "cost": 0}
            }"#;
        assert_eq!(
            exact_number_at_path(bytes, &["usage", "cost"]).as_deref(),
            Some("0")
        );
        let parsed = parse_response(bytes).expect("chat response parses");
        let cost = parsed.inline_cost.expect("inline provider cost");
        assert_eq!(cost.source, OpenRouterCostSource::ChatUsage);
        assert_eq!(cost.total_usd, Some(0.0));
        assert_eq!(cost.total_usd_exact.as_deref(), Some("0"));
        assert_eq!(parsed.generation_id.as_deref(), Some("gen_example"));
    }

    #[test]
    fn parses_openrouter_sse_text_usage_and_terminal_event() {
        let bytes = br#": OPENROUTER PROCESSING

data: {"id":"gen_stream","model":"deepseek/deepseek-v4-flash-0731","choices":[{"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}

data: {"id":"gen_stream","model":"deepseek/deepseek-v4-flash-0731","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"cost":0.000001}}

data: [DONE]

"#;
        let parsed = parse_response(bytes).expect("SSE response parses");
        assert_eq!(parsed.generation_id.as_deref(), Some("gen_stream"));
        assert_eq!(
            parsed.events[0],
            ModelStreamEvent::TextDelta("hello".into())
        );
        assert_eq!(
            parsed.events[1],
            ModelStreamEvent::Usage(Usage {
                input_tokens: Some(2),
                output_tokens: Some(1),
                ..Usage::default()
            })
        );
        assert_eq!(parsed.events[2], ModelStreamEvent::End(StopReason::Stop));
        assert_eq!(
            parsed
                .inline_cost
                .as_ref()
                .and_then(|cost| cost.total_usd_exact.as_deref()),
            Some("0.000001")
        );
    }

    #[test]
    fn streaming_sse_decoder_exposes_each_delta_before_body_settlement() {
        let mut decoder = StreamingSseDecoder::new();
        let first = decoder
            .push(
                br#"data: {"id":"gen_stream","choices":[{"delta":{"content":"first "},"finish_reason":null}]}

"#,
            )
            .expect("first SSE record parses");
        assert_eq!(first, [ModelStreamEvent::TextDelta("first ".into())]);

        let second = decoder
            .push(
                br#"data: {"id":"gen_stream","choices":[{"delta":{"content":"second"},"finish_reason":null}]}

"#,
            )
            .expect("second SSE record parses");
        assert_eq!(second, [ModelStreamEvent::TextDelta("second".into())]);

        assert!(decoder
            .push(
                br#"data: {"id":"gen_stream","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2}}

data: [DONE]

"#,
            )
            .expect("terminal SSE records parse")
            .is_empty());
        let complete = decoder.finish(false).expect("SSE body settles");
        assert_eq!(
            complete.events,
            [
                ModelStreamEvent::Usage(Usage {
                    input_tokens: Some(2),
                    output_tokens: Some(2),
                    ..Usage::default()
                }),
                ModelStreamEvent::End(StopReason::Stop),
            ]
        );
    }

    #[test]
    fn live_event_stream_yields_a_delta_before_the_mock_http_body_settles() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let (first_delta_sent, first_delta_received) = mpsc::channel();
        let (settle_response, wait_for_settlement) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("provider should connect");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request);
            let first = br#"data: {"id":"streamed","choices":[{"delta":{"content":"first "},"finish_reason":null}]}

"#;
            let second = br#"data: {"id":"streamed","choices":[{"delta":{"content":"second"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2}}

data: [DONE]

"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        first.len() + second.len()
                    )
                    .as_bytes(),
                )
                .expect("response headers should write");
            socket
                .write_all(first)
                .expect("first SSE record should write");
            socket.flush().expect("first SSE record should flush");
            first_delta_sent
                .send(())
                .expect("test waits for the first SSE record");
            wait_for_settlement
                .recv()
                .expect("test releases the response body");
            socket
                .write_all(second)
                .expect("terminal SSE records should write");
        });

        let provider = OpenRouterProvider::new(
            OpenRouterConfig::try_new("test-key", "test-model")
                .expect("explicit test configuration")
                .with_test_completion_url(format!("http://{address}/")),
        );
        let cancellation = CancellationToken::new();
        let mut source = smol::block_on(provider.stream(
            ModelRequest {
                context: "[]".into(),
                model: Some(crate::state::ModelDescriptor {
                    provider: "openrouter".into(),
                    model: "test-model".into(),
                    revision: None,
                }),
                ..ModelRequest::default()
            },
            cancellation.clone(),
        ))
        .expect("OpenRouter should start an event source");

        assert!(matches!(
            smol::block_on(source.next_event(cancellation.clone()))
                .expect("request observation should arrive"),
            Some(ModelStreamEvent::RequestObservation(observation))
                if observation.serialized_request_bytes.is_some()
                    && observation.cache_domain_fingerprint.is_some()
        ));
        assert_eq!(
            smol::block_on(source.next_event(cancellation.clone()))
                .expect("first event should arrive"),
            Some(ModelStreamEvent::TextDelta("first ".into()))
        );
        first_delta_received
            .recv_timeout(Duration::from_secs(1))
            .expect("mock server should send the first SSE record");

        settle_response
            .send(())
            .expect("mock server should receive body release");
        assert_eq!(
            smol::block_on(source.next_event(cancellation.clone()))
                .expect("second event should arrive"),
            Some(ModelStreamEvent::TextDelta("second".into()))
        );
        assert!(matches!(
            smol::block_on(source.next_event(cancellation.clone())),
            Ok(Some(ModelStreamEvent::Usage(Usage {
                input_tokens: Some(2),
                output_tokens: Some(2),
                ..
            })))
        ));
        assert_eq!(
            smol::block_on(source.next_event(cancellation)).expect("terminal event should arrive"),
            Some(ModelStreamEvent::End(StopReason::Stop))
        );
        server.join().expect("mock HTTP server should finish");
    }

    #[test]
    fn parses_partial_sse_as_output_limit_only_when_explicitly_allowed() {
        let bytes = br#"data: {"id":"gen_partial","choices":[{"delta":{"content":"partial"},"finish_reason":null}]}

"#;
        assert!(parse_response(bytes).is_err());
        let parsed = parse_partial_response(bytes).expect("partial SSE response parses");
        assert_eq!(
            parsed.events[0],
            ModelStreamEvent::TextDelta("partial".into())
        );
        assert_eq!(parsed.events[1], ModelStreamEvent::End(StopReason::Length));
    }

    #[test]
    fn partial_sse_ignores_only_a_truncated_final_event() {
        let bytes = br#"data: {"id":"gen_partial","choices":[{"delta":{"content":"before-cut"},"finish_reason":null}]}

data: {"id":"gen_partial","choices":[{"delta":{"content":"after-cut"}"#;
        let parsed = parse_partial_response(bytes).expect("partial SSE response parses");
        assert_eq!(
            parsed.events[0],
            ModelStreamEvent::TextDelta("before-cut".into())
        );
        assert_eq!(parsed.events[1], ModelStreamEvent::End(StopReason::Length));
    }

    #[test]
    fn partial_sse_rejects_a_malformed_event_before_later_data() {
        let bytes = br#"data: {"id":"gen_partial","choices":[{"delta":{"content":"before"},"finish_reason":null}]}

data: {"id":"gen_partial","choices":[{"delta":{"content":"broken"}

data: {"id":"gen_partial","choices":[{"delta":{"content":"after"},"finish_reason":"stop"}]}
"#;
        assert!(matches!(
            parse_partial_response(bytes),
            Err(error) if error == "OpenRouter returned an invalid SSE event"
        ));
    }

    #[test]
    fn parses_openrouter_sse_tool_call_deltas() {
        let bytes = br#"data: {"id":"gen_tool","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"workspace_read","arguments":"{\"path\":"}}]},"finish_reason":null}]}

data: {"id":"gen_tool","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"LANG.md\"}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
        let parsed = parse_response(bytes).expect("tool-call SSE response parses");
        assert_eq!(
            parsed.events[0],
            ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new("call_1").unwrap(),
                name: "workspace_read".into(),
                arguments: SerializedJson::new("{\"path\":\"LANG.md\"}"),
            })
        );
        assert_eq!(parsed.events[1], ModelStreamEvent::End(StopReason::ToolUse));
    }

    #[test]
    fn builds_explicit_output_cap_and_openrouter_reasoning_wire() {
        let config = OpenRouterConfig::try_new("key", "openai/gpt-5.6-luna").unwrap();
        let payload = build_payload(
            &config.with_max_tokens(128_000),
            &ModelRequest {
                system_prompt: "system".into(),
                context: "[]".into(),
                model: Some(ModelDescriptor {
                    provider: "openrouter".into(),
                    model: "openai/gpt-5.6-luna".into(),
                    revision: None,
                }),
                thinking_level: ThinkingLevel::XHigh,
                ..ModelRequest::default()
            },
        )
        .unwrap();
        let payload = JsonValue::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
        assert_eq!(
            payload.get("max_tokens").and_then(JsonValue::as_u64),
            Some(128_000)
        );
        assert_eq!(
            payload
                .get("reasoning")
                .and_then(|value| value.get("effort"))
                .and_then(JsonValue::as_str),
            Some("xhigh")
        );
        assert_eq!(
            payload.get("stream").and_then(JsonValue::as_bool),
            Some(true)
        );
        assert_eq!(
            payload
                .get("stream_options")
                .and_then(|value| value.get("include_usage"))
                .and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[test]
    fn leaves_output_length_to_provider_when_no_cap_is_requested() {
        let config = OpenRouterConfig::try_new("key", "openai/gpt-5.6-luna").unwrap();
        let payload = build_payload(
            &config,
            &ModelRequest {
                context: "[]".into(),
                ..ModelRequest::default()
            },
        )
        .unwrap();
        let payload = JsonValue::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
        assert!(payload.get("max_tokens").is_none());
    }

    #[test]
    fn requires_tool_capable_openrouter_routing_when_tools_are_admitted() {
        let config = OpenRouterConfig::try_new("key", "deepseek/deepseek-v4-flash-0731").unwrap();
        let payload = super::payload::build_payload(
            &config,
            &ModelRequest {
                context: "[]".into(),
                tools: vec![ToolDefinition {
                    name: "work_complete".into(),
                    description: "finish the assignment".into(),
                    schema: JsonValue::Object(std::collections::BTreeMap::new()),
                    execution_mode: ToolExecutionMode::Sequential,
                    requires_exclusive_batch: false,
                    cancellation_settlement_mode:
                        crate::tool::CancellationSettlementMode::DropFuture,
                }],
                ..ModelRequest::default()
            },
        )
        .unwrap();
        let payload = JsonValue::parse(std::str::from_utf8(&payload).unwrap()).unwrap();
        assert_eq!(
            payload
                .get("provider")
                .and_then(|value| value.get("require_parameters"))
                .and_then(JsonValue::as_bool),
            Some(true)
        );
    }

    #[test]
    fn rejects_missing_or_mismatched_model_before_transport() {
        let provider = OpenRouterProvider::new(OpenRouterConfig::try_new("key", "model").unwrap());
        let cancellation = CancellationToken::new();
        let request = ModelRequest {
            context: "[]".into(),
            ..ModelRequest::default()
        };
        let stream = provider.response_stream(request, cancellation);
        assert!(
            matches!(stream.events.first(), Some(ModelStreamEvent::Error { message }) if message.contains("omitted its exact model"))
        );

        let stream = provider.response_stream(
            ModelRequest {
                context: "[]".into(),
                model: Some(ModelDescriptor {
                    provider: "openrouter".into(),
                    model: "other-model".into(),
                    revision: None,
                }),
                ..ModelRequest::default()
            },
            CancellationToken::new(),
        );
        assert!(
            matches!(stream.events.first(), Some(ModelStreamEvent::Error { message }) if message.contains("does not match requested model"))
        );
    }

    #[test]
    fn retries_only_transient_openrouter_response_statuses() {
        assert!(openrouter_response_retryable(
            br#"{"error":{"code":429,"message":"slow down"}}"#
        ));
        assert!(openrouter_response_retryable(
            br#"{"error":{"code":503,"message":"temporarily unavailable"}}"#
        ));
        assert!(!openrouter_response_retryable(
            br#"{"error":{"code":400,"message":"invalid request"}}"#
        ));
        assert!(!openrouter_response_retryable(
            br#"{"error":{"message":"invalid request"}}"#
        ));
        assert!(openrouter_status_retryable(Some(502)));
        assert!(!openrouter_status_retryable(Some(400)));
    }

    #[test]
    fn classifies_context_capacity_errors_for_automatic_recovery() {
        assert!(openrouter_context_overflow(
            br#"{"error":{"code":400,"message":"This model's maximum context length is 131072 tokens"}}"#
        ));
        // Poolside through OpenRouter omits the word `context` from its
        // overflow diagnostic. It must still produce the typed core event so
        // bounded automatic compaction can recover the interrupted request.
        assert!(openrouter_context_overflow(
            br#"{"error":{"code":400,"message":"Input length 32769 exceeds the maximum allowed input length of 32768 tokens."}}"#
        ));
        assert!(openrouter_context_overflow(
            br#"{"error":{"message":"prompt is too long"}}"#
        ));
        assert!(!openrouter_context_overflow(
            br#"{"error":{"code":400,"message":"invalid tool arguments"}}"#
        ));
        assert!(!openrouter_context_overflow(
            br#"{"error":{"code":400,"message":"context window is unavailable"}}"#
        ));
    }

    #[test]
    fn response_stream_marks_context_capacity_errors_as_typed_overflow() {
        let provider = OpenRouterProvider::new(OpenRouterConfig::new("key", "model"));
        let mut stream = OpenRouterEventStream {
            provider,
            response: None,
            decoder: None,
            pending: VecDeque::new(),
            status_code: Some(400),
            error_body: br#"{"error":{"message":"maximum context length exceeded"}}"#.to_vec(),
            payload_bytes: 0,
        };
        stream.response_failure("OpenRouter rejected the request".into());
        assert!(matches!(
            stream.pending.front(),
            Some(ModelStreamEvent::ContextOverflow { .. })
        ));
    }

    #[test]
    fn streaming_sse_context_errors_keep_the_typed_overflow_marker() {
        let mut decoder = StreamingSseDecoder::new();
        let error = decoder
            .push(
                br#"data: {"error":{"message":"maximum context length exceeded"}}

"#,
            )
            .expect_err("context error should stop the stream");
        assert_eq!(error, "OpenRouter context capacity exceeded");
    }

    #[test]
    fn non_json_response_keeps_stream_error_stable_and_report_body_bounded() {
        let error = match parse_response(
            br#"<html><body>upstream gateway failure with a very long diagnostic</body></html>"#,
        ) {
            Ok(_) => panic!("non-JSON response unexpectedly parsed"),
            Err(error) => error,
        };
        assert_eq!(error, "OpenRouter returned a non-JSON response");
        assert_eq!(
            response_body_prefix(
                br#"<html><body>upstream gateway failure with a very long diagnostic</body></html>"#,
                Some("failure"),
            ),
            "<html><body>upstream gateway [redacted] with a very long diagnostic</body></html>"
        );
    }

    #[test]
    fn preserves_decimal_costs_and_usage_without_float_aggregation() {
        let provider = OpenRouterProvider::new(OpenRouterConfig::new("key", "model"));
        provider.record(
            Usage {
                input_tokens: Some(2),
                output_tokens: Some(3),
                reasoning_tokens: None,
                ..Usage::default()
            },
            OpenRouterCostTurn {
                turn: 0,
                source: OpenRouterCostSource::ChatUsage,
                total_usd: Some(0.1),
                total_usd_exact: Some("0.100000000000000001".into()),
                upstream_inference_usd: None,
                upstream_inference_usd_exact: None,
                model: Some("model".into()),
                provider: None,
                input_tokens: Some(2),
                output_tokens: Some(3),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
        );
        provider.record(
            Usage::default(),
            OpenRouterCostTurn {
                turn: 0,
                source: OpenRouterCostSource::ChatUsage,
                total_usd: Some(0.2),
                total_usd_exact: Some("0.2".into()),
                upstream_inference_usd: None,
                upstream_inference_usd_exact: None,
                model: Some("model".into()),
                provider: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
        );
        let report = provider.cost_report();
        assert_eq!(
            report.reported_total_usd_exact.as_deref(),
            Some("0.300000000000000001")
        );
        assert_eq!(provider.usage_snapshot().input_tokens, Some(2));
        assert_eq!(provider.usage_snapshot().output_tokens, Some(3));
        assert_eq!(provider.usage_snapshot().reasoning_tokens, None);
    }

    #[test]
    fn cancellation_is_rejected_before_native_request() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = run_http(
            Request::get("http://127.0.0.1:1", std::time::Duration::from_secs(1)),
            &cancellation,
        );
        assert_eq!(result.unwrap_err(), "OpenRouter HTTP transport cancelled");
    }
}
