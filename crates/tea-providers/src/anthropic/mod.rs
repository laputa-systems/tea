//! Native Anthropic Messages API-key adapter.
//!
//! This adapter ports upstream Pi's `anthropic-messages` implementation and the
//! parts of the Anthropic SDK its wire behavior relies on: the
//! `POST /v1/messages?beta=true` request with `x-api-key`,
//! `anthropic-version`, and `anthropic-beta` headers; SSE event iteration;
//! error-body parsing; and the SDK retry classification (`x-should-retry`,
//! 408/409/429/5xx, `retry-after-ms`, `retry-after`). Retries happen only
//! before any model-visible event escapes, with a cancellable wait.
//!
//! Credentials are caller-owned. The adapter never reads the environment or a
//! credential file and identifies itself honestly as Tea. OAuth, workload
//! identity federation, first-party client impersonation, images, strict tool
//! schemas, and server-side model fallbacks are deliberately not ported. See
//! `docs/anthropic-provider.md` for the upstream-to-Tea mapping.

mod catalog;
mod config;
mod events;
mod payload;
mod sse;

#[cfg(test)]
mod tests;

pub use catalog::{
    AnthropicCompat, AnthropicModel, LONG_CACHE_TTL_SECONDS, ListedPricing, MODELS,
    SHORT_CACHE_TTL_SECONDS, model,
};
pub use config::{
    API_ORIGIN, API_VERSION, AnthropicConfig, AnthropicConfigError, AnthropicRequestCapture,
    CacheRetention, CapturedRequest, ThinkingDisplay,
};
pub use events::AnthropicUsage;

use crate::scheduler::{
    AdapterRequestObservation, CancellationToken, CancellationWait, ConfigurationUpdateSupport,
    ModelCapabilities, ModelEventFuture, ModelEventStream, ModelFuture, ModelPricing,
    ModelProvider, ModelRequest, ModelStreamEvent, PromptCacheCapability,
};
use crate::state::{ModelDescriptor, StopReason};
use crate::transport_runtime::client as http_client;
use events::{StreamFailure, StreamReducer, bounded};
use sse::SseDecoder;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tea_http::{
    TransportRequest as Request, TransportStream as HttpStream, TransportStreamEvent as StreamEvent,
};

/// Stable provider identity.
pub const PROVIDER_ID: &str = "anthropic";

/// Bounded diagnostic of the most recent failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicErrorReport {
    /// `adapter`, `transport`, or `response`.
    pub source: &'static str,
    /// Stable local message.
    pub message: String,
    /// HTTP status, when observed.
    pub status_code: Option<u16>,
    /// Anthropic error type, when reported.
    pub error_type: Option<String>,
    /// Anthropic `request-id`, when returned.
    pub request_id: Option<String>,
    /// Whether the failure was classified retryable.
    pub retryable: bool,
    /// One-based attempt.
    pub attempt: u32,
    /// Whether model-visible output had escaped.
    pub visible_stream_event: bool,
    /// Request payload bytes.
    pub request_bytes: Option<usize>,
    /// Bounded, redacted response prefix.
    pub response_prefix: Option<String>,
}

impl AnthropicErrorReport {
    /// The session's persistable provider-error shape.
    pub fn as_session_error(&self) -> tea_session::ProviderErrorRecord {
        tea_session::ProviderErrorRecord {
            source: self.source.to_owned(),
            message: Some(self.message.clone()),
            status_code: self.status_code,
            attempt: Some(self.attempt),
            logical_request_id: self.request_id.clone(),
            visible_stream_event: Some(self.visible_stream_event),
            auth_refresh_attempted: None,
            quota_reset_at_unix_seconds: None,
            error_type: self.error_type.clone(),
            error_code: None,
            retryable: Some(self.retryable),
            response_bytes: self.response_prefix.as_ref().map(|prefix| prefix.len() as u64),
            request_bytes: self.request_bytes.map(|bytes| bytes as u64),
            response_body: self.response_prefix.clone(),
        }
    }
}

/// One settled response's accounting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicTurn {
    /// Raw counters.
    pub usage: AnthropicUsage,
    /// Model reported by the response, when it differed from the request.
    pub response_model: Option<String>,
    /// Estimated cost from configured prices; `None` when prices are unknown.
    /// This is an estimate, never a provider-reported charge.
    pub estimated_cost: Option<String>,
}

#[derive(Default)]
struct Shared {
    turns: Vec<AnthropicTurn>,
    last_error: Option<AnthropicErrorReport>,
    last_input_transformations: Vec<tea_protocol::JsonValue>,
}

/// Anthropic implementation of the [`ModelProvider`] port.
#[derive(Clone)]
pub struct AnthropicProvider {
    config: Arc<AnthropicConfig>,
    shared: Arc<Mutex<Shared>>,
}

impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Construct from explicit configuration.
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            config: Arc::new(config),
            shared: Arc::new(Mutex::new(Shared::default())),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    /// The most recent failure.
    pub fn last_error_report(&self) -> Option<AnthropicErrorReport> {
        self.shared().last_error.clone()
    }

    /// Settled responses in order.
    pub fn turns(&self) -> Vec<AnthropicTurn> {
        self.shared().turns.clone()
    }

    /// Input transformations the server reported for the latest response.
    pub fn last_input_transformations(&self) -> Vec<tea_protocol::JsonValue> {
        self.shared().last_input_transformations.clone()
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().expect("Anthropic shared state poisoned")
    }

    fn record_error(&self, report: AnthropicErrorReport) {
        self.shared().last_error = Some(report);
    }

    fn validate_model(&self, request: &ModelRequest) -> Result<(), String> {
        match &request.model {
            Some(model) if model.provider == PROVIDER_ID && model.model == self.config.model => {
                Ok(())
            }
            Some(model) => Err(format!(
                "Anthropic adapter configured for {} cannot serve {}/{}",
                self.config.model, model.provider, model.model
            )),
            None => Err("Anthropic request has no model descriptor".into()),
        }
    }
}

/// Exact estimated cost of one response in decimal dollars (Pi's
/// `calculateCost`): one-hour cache writes cost twice the input price.
pub fn estimate_cost(usage: AnthropicUsage, pricing: &ModelPricing) -> Option<String> {
    let short_write = usage.cache_write.saturating_sub(usage.cache_write_1h);
    let mut total = Decimal::zero();
    total = total.add(&Decimal::parse(&pricing.input)?.mul_tokens(usage.input));
    total = total.add(&Decimal::parse(&pricing.output)?.mul_tokens(usage.output));
    total = total.add(&Decimal::parse(&pricing.cache_read)?.mul_tokens(usage.cache_read));
    total = total.add(&Decimal::parse(&pricing.cache_write)?.mul_tokens(short_write));
    total = total.add(
        &Decimal::parse(&pricing.input)?
            .mul_tokens(usage.cache_write_1h)
            .mul_small(2),
    );
    Some(total.per_million().to_string())
}

/// Minimal exact non-negative decimal used for price estimates.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Decimal {
    digits: u128,
    scale: u32,
}

impl Decimal {
    fn zero() -> Self {
        Self {
            digits: 0,
            scale: 0,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        if whole.is_empty() && fraction.is_empty()
            || !whole.chars().all(|c| c.is_ascii_digit())
            || !fraction.chars().all(|c| c.is_ascii_digit())
            || fraction.len() > 12
        {
            return None;
        }
        let digits = format!("{whole}{fraction}").parse::<u128>().ok()?;
        Some(Self {
            digits,
            scale: fraction.len() as u32,
        })
    }

    fn rescale(&self, scale: u32) -> u128 {
        self.digits * 10u128.pow(scale - self.scale)
    }

    fn add(&self, other: &Self) -> Self {
        let scale = self.scale.max(other.scale);
        Self {
            digits: self.rescale(scale) + other.rescale(scale),
            scale,
        }
    }

    fn mul_tokens(&self, tokens: u64) -> Self {
        Self {
            digits: self.digits * u128::from(tokens),
            scale: self.scale,
        }
    }

    fn mul_small(&self, factor: u128) -> Self {
        Self {
            digits: self.digits * factor,
            scale: self.scale,
        }
    }

    fn per_million(&self) -> Self {
        Self {
            digits: self.digits,
            scale: self.scale + 6,
        }
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.digits.to_string();
        let scale = self.scale as usize;
        let (whole, fraction) = if text.len() > scale {
            (text[..text.len() - scale].to_owned(), text[text.len() - scale..].to_owned())
        } else {
            ("0".to_owned(), format!("{}{text}", "0".repeat(scale - text.len())))
        };
        let fraction = fraction.trim_end_matches('0');
        if fraction.is_empty() {
            formatter.write_str(&whole)
        } else {
            write!(formatter, "{whole}.{fraction}")
        }
    }
}

impl ModelProvider for AnthropicProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = AnthropicEventStream::start(self.clone(), request, cancellation);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }

    fn capabilities(&self, _model: Option<&ModelDescriptor>) -> ModelCapabilities {
        let compat = self.config.compat;
        let ttl_seconds = match self.config.cache_retention {
            CacheRetention::None => None,
            CacheRetention::Short => Some(SHORT_CACHE_TTL_SECONDS),
            CacheRetention::Long if compat.supports_long_cache_retention => {
                Some(LONG_CACHE_TTL_SECONDS)
            }
            CacheRetention::Long => Some(SHORT_CACHE_TTL_SECONDS),
        };
        ModelCapabilities {
            configuration_updates: ConfigurationUpdateSupport {
                system_messages: compat.mid_conversation_system_messages,
                tool_changes: compat.mid_conversation_tool_changes,
            },
            exposes_thinking: compat.reasoning,
            prompt_cache: ttl_seconds.map(|ttl_seconds| PromptCacheCapability {
                ttl_seconds,
                minimal_output_replay: payload::minimal_output_replay_is_safe(compat),
            }),
            pricing: self.config.pricing.clone(),
            context_window: self.config.context_window,
        }
    }
}

/// One live response with bounded retry before visible output.
struct AnthropicEventStream {
    provider: AnthropicProvider,
    body: Vec<u8>,
    beta_header: Option<String>,
    managed_effort: Option<&'static str>,
    response: Option<HttpStream>,
    status_code: Option<u16>,
    headers: Vec<(String, String)>,
    headers_received: bool,
    error_body: Vec<u8>,
    decoder: SseDecoder,
    reducer: StreamReducer,
    pending: VecDeque<ModelStreamEvent>,
    attempt: u32,
    visible: bool,
    terminal: bool,
    retry_timer: Option<Pin<Box<smol::Timer>>>,
    retry_cancellation: Option<Pin<Box<CancellationWait>>>,
}

impl AnthropicEventStream {
    fn start(
        provider: AnthropicProvider,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Self {
        let mut stream = Self {
            provider,
            body: Vec::new(),
            beta_header: None,
            managed_effort: None,
            response: None,
            status_code: None,
            headers: Vec::new(),
            headers_received: false,
            error_body: Vec::new(),
            decoder: SseDecoder::new(),
            reducer: StreamReducer::new(None),
            pending: VecDeque::new(),
            attempt: 0,
            visible: false,
            terminal: false,
            retry_timer: None,
            retry_cancellation: None,
        };
        if cancellation.is_cancelled() {
            stream
                .pending
                .push_back(ModelStreamEvent::End(StopReason::Cancelled));
            return stream;
        }
        if let Err(message) = stream.provider.validate_model(&request) {
            stream.fail("adapter", message, None, false);
            return stream;
        }
        let built = match payload::build_request(&stream.provider.config, &request) {
            Ok(built) => built,
            Err(message) => {
                stream.fail("adapter", message, None, false);
                return stream;
            }
        };
        let body = match built.body.to_json_string() {
            Ok(body) => body.into_bytes(),
            Err(error) => {
                stream.fail(
                    "adapter",
                    format!("cannot encode Anthropic request: {error}"),
                    None,
                    false,
                );
                return stream;
            }
        };
        stream.beta_header = (!built.betas.is_empty()).then(|| built.betas.join(","));
        stream.managed_effort = built.managed_effort;
        if let Some(capture) = &stream.provider.config.request_capture {
            capture.observe(&body, stream.beta_header.as_deref());
        }
        stream
            .pending
            .push_back(ModelStreamEvent::RequestObservation(request_observation(
                &stream.provider.config,
                &built,
                body.len(),
            )));
        stream.body = body;
        stream.start_attempt(&cancellation);
        stream
    }

    fn start_attempt(&mut self, cancellation: &CancellationToken) {
        if cancellation.is_cancelled() {
            self.pending
                .push_back(ModelStreamEvent::End(StopReason::Cancelled));
            return;
        }
        self.attempt += 1;
        self.status_code = None;
        self.headers.clear();
        self.headers_received = false;
        self.error_body.clear();
        self.decoder = SseDecoder::new();
        self.reducer = StreamReducer::new(self.managed_effort);
        let config = &self.provider.config;
        let mut request = Request::post(
            config.messages_url(),
            self.body.clone(),
            config.request_timeout,
        )
        .query("beta", "true")
        .header("x-api-key", config.api_key.clone())
        .header("anthropic-version", API_VERSION)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("user-agent", config.user_agent.clone())
        .with_stall_timeout(config.stall_timeout);
        if let Some(beta) = &self.beta_header {
            request = request.header("anthropic-beta", beta.clone());
        }
        self.response = Some(http_client().stream(request, cancellation.clone()));
    }

    fn request_id(&self) -> Option<String> {
        header(&self.headers, "request-id").map(str::to_owned)
    }

    fn fail(
        &mut self,
        source: &'static str,
        message: String,
        error_type: Option<String>,
        retryable: bool,
    ) {
        self.response = None;
        let overflow = is_context_overflow(&message) || is_context_overflow_type(&error_type);
        let report = AnthropicErrorReport {
            source,
            message: message.clone(),
            status_code: self.status_code,
            error_type,
            request_id: self.request_id(),
            retryable,
            attempt: self.attempt,
            visible_stream_event: self.visible,
            request_bytes: (!self.body.is_empty()).then_some(self.body.len()),
            response_prefix: (!self.error_body.is_empty()).then(|| {
                redact(
                    &bounded(&String::from_utf8_lossy(&self.error_body), 2_048),
                    &self.provider.config.api_key,
                )
            }),
        };
        let record = report.as_session_error();
        self.provider.record_error(report);
        self.pending.push_back(ModelStreamEvent::ProviderError(record));
        self.pending.push_back(if overflow {
            ModelStreamEvent::ContextOverflow { message }
        } else {
            ModelStreamEvent::Error { message }
        });
    }

    /// Queue a cancellable retry. Returns false when retrying is not allowed.
    fn queue_retry(&mut self, server_delay: Option<Duration>) -> Result<bool, String> {
        if self.visible || self.attempt > self.provider.config.retry_policy.max_retries() {
            return Ok(false);
        }
        let policy = self.provider.config.retry_policy;
        let delay = match server_delay {
            Some(delay) => {
                let maximum = self.provider.config.maximum_retry_delay;
                if !maximum.is_zero() && delay > maximum {
                    return Err(format!(
                        "Server requested {}s retry delay (max: {}s)",
                        delay.as_secs_f64().ceil() as u64,
                        maximum.as_secs_f64().ceil() as u64
                    ));
                }
                delay
            }
            None => policy.delay_before_retry(self.attempt.saturating_sub(1)),
        };
        self.response = None;
        self.retry_timer = Some(Box::pin(smol::Timer::after(delay)));
        self.retry_cancellation = None;
        Ok(true)
    }

    fn handle_status_failure(&mut self) {
        let parsed = parse_error_body(&self.error_body);
        let status = self.status_code.unwrap_or(0);
        let retryable = status_retryable(status, &self.headers);
        if retryable {
            match self.queue_retry(retry_delay(&self.headers)) {
                Ok(true) => return,
                Ok(false) => {}
                Err(message) => {
                    let message = format!("{message}. {}", describe_status(status, &parsed));
                    self.fail("response", message, parsed.error_type, true);
                    return;
                }
            }
        }
        let message = describe_status(status, &parsed);
        self.fail("response", message, parsed.error_type, retryable);
    }

    fn handle_transport_failure(&mut self, failure: tea_http::TransportError) {
        self.status_code = self.status_code.or(failure.status_code);
        self.error_body.extend_from_slice(&failure.body);
        if !self.visible
            && !self.headers_received
            && let Ok(true) = self.queue_retry(None)
        {
            return;
        }
        self.fail(
            "transport",
            format!("Anthropic HTTP transport failed: {}", failure.message),
            None,
            !self.headers_received,
        );
    }

    fn reduce(&mut self, result: Result<Vec<ModelStreamEvent>, StreamFailure>) {
        match result {
            Ok(events) => {
                for event in events {
                    if !matches!(
                        event,
                        ModelStreamEvent::RequestObservation(_)
                            | ModelStreamEvent::ProviderError(_)
                    ) {
                        self.visible = true;
                    }
                    if let ModelStreamEvent::Usage(_) = &event {
                        self.settle_accounting();
                    }
                    self.pending.push_back(event);
                }
            }
            Err(failure) => {
                // A failure after output began is never retried: replaying an
                // ambiguous request is the embedding's decision.
                self.fail("response", failure.message, failure.error_type, false);
            }
        }
    }

    fn settle_accounting(&mut self) {
        let usage = self.reducer.usage;
        let estimated_cost = self
            .provider
            .config
            .pricing
            .as_ref()
            .and_then(|pricing| estimate_cost(usage, pricing));
        let mut shared = self.provider.shared();
        shared.turns.push(AnthropicTurn {
            usage,
            response_model: self
                .reducer
                .response_model
                .clone()
                .filter(|model| model != &self.provider.config.model),
            estimated_cost,
        });
        shared.last_input_transformations = self.reducer.input_transformations.clone();
    }

    fn poll_next_event(
        &mut self,
        context: &mut Context<'_>,
        cancellation: CancellationToken,
    ) -> Poll<Result<Option<ModelStreamEvent>, crate::error::SchedulerError>> {
        loop {
            if self.terminal {
                return Poll::Ready(Ok(None));
            }
            if let Some(event) = self.pending.pop_front() {
                if matches!(
                    event,
                    ModelStreamEvent::End(_)
                        | ModelStreamEvent::Error { .. }
                        | ModelStreamEvent::ContextOverflow { .. }
                        | ModelStreamEvent::Aborted { .. }
                ) {
                    self.terminal = true;
                    self.response = None;
                    self.retry_timer = None;
                    self.retry_cancellation = None;
                }
                return Poll::Ready(Ok(Some(event)));
            }
            if cancellation.is_cancelled() {
                self.terminal = true;
                self.response = None;
                self.retry_timer = None;
                return Poll::Ready(Ok(Some(ModelStreamEvent::End(StopReason::Cancelled))));
            }
            if let Some(timer) = self.retry_timer.as_mut() {
                if self.retry_cancellation.is_none() {
                    self.retry_cancellation = Some(Box::pin(cancellation.cancelled()));
                }
                if let Some(wait) = self.retry_cancellation.as_mut()
                    && wait.as_mut().poll(context).is_ready()
                {
                    self.retry_timer = None;
                    self.retry_cancellation = None;
                    continue;
                }
                match timer.as_mut().poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(_) => {
                        self.retry_timer = None;
                        self.retry_cancellation = None;
                        self.start_attempt(&cancellation);
                        continue;
                    }
                }
            }
            let Some(response) = self.response.as_mut() else {
                return Poll::Ready(Ok(None));
            };
            match response.poll_next(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(StreamEvent::Response {
                    status_code,
                    headers,
                }) => {
                    self.headers_received = true;
                    self.status_code = Some(status_code);
                    self.headers = headers;
                }
                Poll::Ready(StreamEvent::Chunk(bytes)) => {
                    if self
                        .status_code
                        .is_some_and(|status| !(200..300).contains(&status))
                    {
                        self.error_body.extend_from_slice(&bytes);
                        continue;
                    }
                    for event in self.decoder.push(&bytes) {
                        let result = self.reducer.push(event);
                        self.reduce(result);
                        if self.response.is_none() {
                            break;
                        }
                    }
                }
                Poll::Ready(StreamEvent::End) => {
                    if self
                        .status_code
                        .is_some_and(|status| !(200..300).contains(&status))
                    {
                        self.handle_status_failure();
                        continue;
                    }
                    self.response = None;
                    for event in self.decoder.finish() {
                        let result = self.reducer.push(event);
                        self.reduce(result);
                    }
                    if !self
                        .pending
                        .iter()
                        .any(|event| matches!(event, ModelStreamEvent::Error { .. }))
                    {
                        let result = self.reducer.finish();
                        self.reduce(result);
                    }
                }
                Poll::Ready(StreamEvent::Failure(failure)) => {
                    if cancellation.is_cancelled() || failure.message == "HTTP request cancelled" {
                        self.response = None;
                        self.pending
                            .push_back(ModelStreamEvent::End(StopReason::Cancelled));
                    } else {
                        self.handle_transport_failure(failure);
                    }
                }
            }
        }
    }
}

impl ModelEventStream for AnthropicEventStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(std::future::poll_fn(move |context| {
            self.poll_next_event(context, cancellation.clone())
        }))
    }
}

/// Content-safe facts about the exact body sent.
fn request_observation(
    config: &AnthropicConfig,
    built: &payload::BuiltRequest,
    serialized_request_bytes: usize,
) -> AdapterRequestObservation {
    let mut components = BTreeMap::<String, u64>::new();
    components.insert(
        "adapter".into(),
        stable_fingerprint(b"anthropic-messages/v1"),
    );
    components.insert(
        "beta_features".into(),
        stable_fingerprint(built.betas.join(",").as_bytes()),
    );
    components.insert(
        "cache_retention".into(),
        stable_fingerprint(format!("{:?}", config.cache_retention).as_bytes()),
    );
    for field in ["thinking", "output_config", "tools", "system"] {
        if let Some(value) = built.body.get(field) {
            components.insert(
                field.into(),
                stable_fingerprint(value.to_json_string().unwrap_or_default().as_bytes()),
            );
        }
    }
    let mut domain = Vec::new();
    for (name, fingerprint) in &components {
        domain.extend_from_slice(name.as_bytes());
        domain.push(0);
        domain.extend_from_slice(&fingerprint.to_le_bytes());
    }
    AdapterRequestObservation {
        deterministic_common_prefix_bytes: None,
        deterministic_common_prefix_tokens_estimate: None,
        serialized_request_bytes: Some(serialized_request_bytes),
        cache_domain_fingerprint: Some(stable_fingerprint(&domain)),
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

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

/// SDK retry classification: `x-should-retry` wins, then 408/409/429/5xx.
fn status_retryable(status: u16, headers: &[(String, String)]) -> bool {
    match header(headers, "x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    matches!(status, 408 | 409 | 429) || status >= 500
}

/// Server-requested retry delay (`retry-after-ms`, then `retry-after` seconds).
fn retry_delay(headers: &[(String, String)]) -> Option<Duration> {
    if let Some(milliseconds) =
        header(headers, "retry-after-ms").and_then(|value| value.parse::<f64>().ok())
        && milliseconds.is_finite()
        && milliseconds >= 0.0
    {
        return Some(Duration::from_secs_f64(milliseconds / 1_000.0));
    }
    header(headers, "retry-after")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(Duration::from_secs_f64)
}

#[derive(Debug, Default)]
struct ParsedError {
    error_type: Option<String>,
    message: Option<String>,
}

fn parse_error_body(body: &[u8]) -> ParsedError {
    let parsed = tea_protocol::JsonValue::parse(&String::from_utf8_lossy(body)).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    ParsedError {
        error_type: error
            .and_then(|error| error.get("type"))
            .and_then(tea_protocol::JsonValue::as_str)
            .map(str::to_owned),
        message: error
            .and_then(|error| error.get("message"))
            .and_then(tea_protocol::JsonValue::as_str)
            .map(|message| bounded(message, 1_024)),
    }
}

fn describe_status(status: u16, parsed: &ParsedError) -> String {
    match (&parsed.error_type, &parsed.message) {
        (Some(error_type), Some(message)) => {
            format!("Anthropic API error {status} ({error_type}): {message}")
        }
        (None, Some(message)) => format!("Anthropic API error {status}: {message}"),
        (Some(error_type), None) => format!("Anthropic API error {status} ({error_type})"),
        (None, None) => format!("Anthropic API error {status}"),
    }
}

/// Pi's Anthropic overflow patterns.
fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("request_too_large")
        || lower.contains("input length and `max_tokens` exceed context limit")
}

fn is_context_overflow_type(error_type: &Option<String>) -> bool {
    error_type.as_deref() == Some("request_too_large")
}

fn redact(text: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        text.to_owned()
    } else {
        text.replace(api_key, "[redacted]")
    }
}
