//! Caller-polled direct Codex Responses adapter.

use super::auth::CodexAuthSnapshot;
use super::config::CodexConfig;
use super::error::{
    CodexErrorReport, CodexErrorSource, backend_error_fields, response_body_prefix,
};
use super::payload::build_payload;
use super::stream::CodexSseDecoder;
use super::wire::{
    CODEX_WIRE_COMPAT_VERSION, HEADER_ACCEPT, HEADER_ACCOUNT_ID, HEADER_AUTHORIZATION,
    HEADER_CLIENT_REQUEST_ID, HEADER_CONTENT_TYPE, HEADER_OPENAI_BETA, HEADER_ORIGINATOR,
    HEADER_SESSION_ID, HEADER_USER_AGENT, HEADER_VERSION, ORIGINATOR, PROVIDER_ID, RESPONSES_BETA,
    tea_user_agent,
};
use crate::scheduler::{
    AdapterRequestObservation, CancellationToken, CancellationWait, ModelEventFuture,
    ModelEventStream, ModelFuture, ModelProvider, ModelRequest, ModelStreamEvent,
};
use crate::state::{StopReason, Usage};
use crate::transport_runtime::client as http_client;
use base64::Engine as _;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_http::{
    TransportRequest as Request, TransportStream as HttpStream, TransportStreamEvent as StreamEvent,
};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Native direct ChatGPT-subscription Codex provider.
#[derive(Clone)]
pub struct CodexProvider {
    config: CodexConfig,
    accounting: Arc<Mutex<Usage>>,
    last_error: Arc<Mutex<Option<CodexErrorReport>>>,
    fallback_session_id: Arc<Mutex<Option<String>>>,
}

impl CodexProvider {
    /// Construct a provider from one explicit model and shared auth manager.
    pub fn new(config: CodexConfig) -> Self {
        Self {
            config,
            accounting: Arc::new(Mutex::new(Usage::default())),
            last_error: Arc::new(Mutex::new(None)),
            fallback_session_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Return the latest bounded failure observed by this provider instance.
    pub fn last_error_report(&self) -> Option<CodexErrorReport> {
        self.last_error
            .lock()
            .expect("Codex error mutex poisoned")
            .clone()
    }

    /// Return accumulated provider-reported usage. Cost remains `None` for
    /// ChatGPT-subscription requests because the backend does not report a
    /// portable monetary amount.
    pub fn usage_snapshot(&self) -> Usage {
        self.accounting
            .lock()
            .expect("Codex usage mutex poisoned")
            .clone()
    }

    fn clear_error(&self) {
        *self.last_error.lock().expect("Codex error mutex poisoned") = None;
    }

    fn record_error(&self, report: CodexErrorReport) {
        *self.last_error.lock().expect("Codex error mutex poisoned") = Some(report);
    }

    fn record_usage(&self, usage: Usage) {
        self.accounting
            .lock()
            .expect("Codex usage mutex poisoned")
            .accumulate(usage);
    }

    fn validate_model(&self, request: &ModelRequest) -> Result<(), String> {
        self.config.validate().map_err(|error| error.to_string())?;
        let model = request
            .model
            .as_ref()
            .ok_or_else(|| "Codex request omitted its exact model descriptor".to_owned())?;
        if model.provider != PROVIDER_ID || model.model != self.config.model() {
            return Err(format!(
                "Codex configuration does not match requested model: expected codex/{}, got {}/{}",
                self.config.model(),
                model.provider,
                model.model
            ));
        }
        Ok(())
    }

    fn session_id(&self, request: &ModelRequest) -> Result<String, String> {
        if let Some(session_id) = request.session_id.as_deref() {
            return validate_identifier("session identity", session_id).map(str::to_owned);
        }
        let mut slot = self
            .fallback_session_id
            .lock()
            .map_err(|_| "Codex session identity lock is poisoned".to_owned())?;
        if slot.is_none() {
            *slot = Some(new_identifier("tea-session")?);
        }
        Ok(slot
            .clone()
            .expect("assigned Codex fallback session identity"))
    }
}

impl fmt::Debug for CodexProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CodexProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// One live direct-Codex SSE logical request.
struct CodexEventStream {
    provider: CodexProvider,
    request: ModelRequest,
    payload: Vec<u8>,
    session_id: String,
    request_id: String,
    response: Option<HttpStream>,
    decoder: Option<CodexSseDecoder>,
    pending: VecDeque<ModelStreamEvent>,
    status_code: Option<u16>,
    response_headers: Vec<(String, String)>,
    response_headers_received: bool,
    error_body: Vec<u8>,
    // Values used only to scrub an untrusted backend echo before it reaches a
    // durable provider diagnostic. This stream never implements Debug.
    diagnostic_redactions: Vec<String>,
    response_body_bytes: usize,
    attempt: u32,
    forced_refresh_used: bool,
    request_observed: bool,
    visible_stream_event: bool,
    /// Core verifies that a model source closes after its terminal event. Keep
    /// this boundary in the adapter so cancellation cannot re-emit `End` on
    /// every later poll of the same already-cancelled token.
    terminal_event_emitted: bool,
    retry_timer: Option<Pin<Box<smol::Timer>>>,
    retry_cancellation: Option<Pin<Box<CancellationWait>>>,
    retry_force_refresh: bool,
}

impl CodexEventStream {
    fn start(
        provider: CodexProvider,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> Self {
        provider.clear_error();
        let mut stream = Self {
            provider,
            request,
            payload: Vec::new(),
            session_id: String::new(),
            request_id: String::new(),
            response: None,
            decoder: None,
            pending: VecDeque::new(),
            status_code: None,
            response_headers: Vec::new(),
            response_headers_received: false,
            error_body: Vec::new(),
            diagnostic_redactions: Vec::new(),
            response_body_bytes: 0,
            attempt: 0,
            forced_refresh_used: false,
            request_observed: false,
            visible_stream_event: false,
            terminal_event_emitted: false,
            retry_timer: None,
            retry_cancellation: None,
            retry_force_refresh: false,
        };
        if cancellation.is_cancelled() {
            stream
                .pending
                .push_back(ModelStreamEvent::End(StopReason::Cancelled));
            return stream;
        }
        if let Err(message) = stream.provider.validate_model(&stream.request) {
            stream.adapter_failure(message);
            return stream;
        }
        stream.session_id = match stream.provider.session_id(&stream.request) {
            Ok(value) => value,
            Err(message) => {
                stream.adapter_failure(message);
                return stream;
            }
        };
        stream.request_id = match new_identifier("tea-request") {
            Ok(value) => value,
            Err(message) => {
                stream.adapter_failure(message);
                return stream;
            }
        };
        stream.payload =
            match build_payload(&stream.provider.config, &stream.request, &stream.session_id) {
                Ok(payload) => payload,
                Err(message) => {
                    stream.adapter_failure(message);
                    return stream;
                }
            };
        if let Err(message) = stream.start_attempt(false, &cancellation) {
            stream.authentication_failure(message);
        }
        stream
    }

    fn start_attempt(
        &mut self,
        force_refresh: bool,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        if cancellation.is_cancelled() {
            self.pending
                .push_back(ModelStreamEvent::End(StopReason::Cancelled));
            return Ok(());
        }
        let snapshot = if force_refresh {
            self.provider.config.auth.force_refresh(cancellation)
        } else {
            self.provider.config.auth.snapshot(cancellation)
        }
        .map_err(|error| error.to_string())?;
        self.diagnostic_redactions = vec![
            snapshot.access_token.expose().to_owned(),
            snapshot.account_id.clone(),
        ];
        self.attempt = self.attempt.saturating_add(1);
        self.status_code = None;
        self.response_headers.clear();
        self.response_headers_received = false;
        self.error_body.clear();
        self.response_body_bytes = 0;
        let headers = request_headers(&snapshot, &self.session_id, &self.request_id)?;
        #[cfg(any(test, feature = "provider-codex-test-support"))]
        if let Some(capture) = &self.provider.config.request_capture {
            capture.observe(self.payload.clone(), redact_headers(&headers));
        }
        let mut request = Request::post(
            self.provider.config.responses_url(),
            self.payload.clone(),
            self.provider.config.request_timeout,
        )
        .with_stall_timeout(self.provider.config.stall_timeout);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        self.response = Some(http_client().stream(request, cancellation.clone()));
        self.decoder = Some(CodexSseDecoder::new());
        Ok(())
    }

    fn adapter_failure(&mut self, message: String) {
        self.record_terminal_failure(CodexErrorReport {
            source: CodexErrorSource::Adapter,
            message: message.clone(),
            status_code: None,
            error_type: None,
            error_code: None,
            retryable: false,
            attempt: self.attempt,
            logical_request_id: (!self.request_id.is_empty()).then(|| self.request_id.clone()),
            visible_stream_event: self.visible_stream_event,
            auth_refresh_attempted: self.forced_refresh_used,
            quota_reset_at_unix_seconds: None,
            response_bytes: None,
            request_bytes: (!self.payload.is_empty()).then_some(self.payload.len()),
            response_prefix: None,
        });
        self.queue_last_error();
        self.pending.push_back(ModelStreamEvent::Error { message });
    }

    fn authentication_failure(&mut self, message: String) {
        self.record_terminal_failure(CodexErrorReport {
            source: CodexErrorSource::Authentication,
            message: format!("Codex authentication failed: {message}"),
            status_code: None,
            error_type: None,
            error_code: None,
            retryable: false,
            attempt: self.attempt,
            logical_request_id: (!self.request_id.is_empty()).then(|| self.request_id.clone()),
            visible_stream_event: self.visible_stream_event,
            auth_refresh_attempted: self.forced_refresh_used,
            quota_reset_at_unix_seconds: None,
            response_bytes: None,
            request_bytes: (!self.payload.is_empty()).then_some(self.payload.len()),
            response_prefix: None,
        });
        self.queue_last_error();
        self.pending.push_back(ModelStreamEvent::Error {
            message: "Codex authentication failed; run `tea auth login codex` if login is required"
                .into(),
        });
    }

    fn record_terminal_failure(&self, report: CodexErrorReport) {
        self.provider.record_error(report);
    }

    fn queue_last_error(&mut self) {
        if let Some(report) = self.provider.last_error_report() {
            self.pending
                .push_back(ModelStreamEvent::ProviderError(report.as_session_error()));
        }
    }

    fn terminal_response_failure(&mut self, message: String, context_overflow: bool) {
        self.response = None;
        self.decoder = None;
        let (error_type, error_code, body_reset_at) = backend_error_fields(&self.error_body);
        let report = CodexErrorReport {
            source: CodexErrorSource::Response,
            message: message.clone(),
            status_code: self.status_code,
            error_type,
            error_code,
            retryable: false,
            attempt: self.attempt,
            logical_request_id: (!self.request_id.is_empty()).then(|| self.request_id.clone()),
            visible_stream_event: self.visible_stream_event,
            auth_refresh_attempted: self.forced_refresh_used,
            quota_reset_at_unix_seconds: body_reset_at
                .or_else(|| quota_reset_at(&self.response_headers)),
            response_bytes: Some(self.response_body_bytes),
            request_bytes: Some(self.payload.len()),
            response_prefix: self.diagnostic_response_prefix(),
        };
        self.record_terminal_failure(report);
        self.queue_last_error();
        self.pending.push_back(if context_overflow {
            ModelStreamEvent::ContextOverflow { message }
        } else {
            ModelStreamEvent::Error { message }
        });
    }

    fn terminal_transport_failure(&mut self, message: String) {
        self.response = None;
        self.decoder = None;
        let (error_type, error_code, body_reset_at) = backend_error_fields(&self.error_body);
        let report = CodexErrorReport {
            source: CodexErrorSource::Transport,
            message: message.clone(),
            status_code: self.status_code,
            error_type,
            error_code,
            retryable: false,
            attempt: self.attempt,
            logical_request_id: (!self.request_id.is_empty()).then(|| self.request_id.clone()),
            visible_stream_event: self.visible_stream_event,
            auth_refresh_attempted: self.forced_refresh_used,
            quota_reset_at_unix_seconds: body_reset_at
                .or_else(|| quota_reset_at(&self.response_headers)),
            response_bytes: Some(self.response_body_bytes),
            request_bytes: Some(self.payload.len()),
            response_prefix: self.diagnostic_response_prefix(),
        };
        self.record_terminal_failure(report);
        self.queue_last_error();
        self.pending.push_back(ModelStreamEvent::Error { message });
    }

    fn queue_retry(&mut self, retry_after: Option<Duration>, force_refresh: bool) -> bool {
        if self.visible_stream_event
            || self.attempt > self.provider.config.retry_policy.max_retries()
        {
            return false;
        }
        let retry_index = self.attempt.saturating_sub(1);
        let backoff = self
            .provider
            .config
            .retry_policy
            .delay_before_retry(retry_index);
        let delay = retry_after
            .unwrap_or(backoff)
            .min(self.provider.config.retry_policy.max_delay());
        self.response = None;
        self.decoder = None;
        self.retry_force_refresh = force_refresh;
        self.retry_timer = Some(Box::pin(smol::Timer::after(delay)));
        self.retry_cancellation = None;
        true
    }

    fn handle_status_failure(&mut self, cancellation: &CancellationToken) {
        let status = self.status_code.unwrap_or_default();
        if status == 401 && !self.forced_refresh_used && !self.visible_stream_event {
            self.forced_refresh_used = true;
            self.response = None;
            self.decoder = None;
            if let Err(message) = self.start_attempt(true, cancellation) {
                self.authentication_failure(message);
            }
            return;
        }
        let retryable = is_retryable_status(status, &self.error_body);
        if retryable && self.queue_retry(retry_after(&self.response_headers), false) {
            return;
        }
        let context_overflow = response_indicates_context_overflow(&self.error_body);
        self.terminal_response_failure(
            status_message(status, &self.error_body, self.provider.config.model()),
            context_overflow,
        );
    }

    fn handle_transport_failure(
        &mut self,
        failure: tea_http::TransportError,
        cancellation: &CancellationToken,
    ) {
        if cancellation.is_cancelled() {
            self.response = None;
            self.decoder = None;
            self.pending
                .push_back(ModelStreamEvent::End(StopReason::Cancelled));
            return;
        }
        self.status_code = self.status_code.or(failure.status_code);
        self.append_error_body(&failure.body);
        // A transport failure can arrive after the HTTP status line but before
        // a response body completes. The status is already authoritative for
        // safe pre-output recovery: a 401 still gets its sole forced refresh,
        // and retryable gateway/rate-limit statuses remain safe to replay.
        // Never apply this path after a model-visible event.
        if !self.visible_stream_event {
            if self.status_code == Some(401) && !self.forced_refresh_used {
                self.forced_refresh_used = true;
                self.response = None;
                self.decoder = None;
                if let Err(message) = self.start_attempt(true, cancellation) {
                    self.authentication_failure(message);
                }
                return;
            }
            if self
                .status_code
                .is_some_and(|status| is_retryable_status(status, &self.error_body))
                && self.queue_retry(retry_after(&self.response_headers), false)
            {
                return;
            }
        }
        if !self.response_headers_received
            && !self.visible_stream_event
            && self.queue_retry(None, false)
        {
            return;
        }
        self.terminal_transport_failure(format!(
            "Codex HTTP transport failed{}: {}",
            self.status_code
                .map(|status| format!(" with status {status}"))
                .unwrap_or_default(),
            failure.message
        ));
    }

    fn append_error_body(&mut self, bytes: &[u8]) {
        self.response_body_bytes = self.response_body_bytes.saturating_add(bytes.len());
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(self.error_body.len());
        self.error_body
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }

    fn diagnostic_response_prefix(&self) -> Option<String> {
        let secrets = self
            .diagnostic_redactions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        response_body_prefix(&self.error_body, &secrets)
    }

    fn observe_request(&mut self) {
        if self.request_observed {
            return;
        }
        self.request_observed = true;
        self.pending.push_back(ModelStreamEvent::RequestObservation(
            codex_request_observation(&self.request, self.payload.len(), &self.session_id),
        ));
    }

    fn handle_decoded_events(
        &mut self,
        events: Vec<ModelStreamEvent>,
        cancellation: &CancellationToken,
    ) {
        for event in events {
            if cancellation.is_cancelled() {
                return;
            }
            if matches!(
                event,
                ModelStreamEvent::TextDelta(_)
                    | ModelStreamEvent::ToolCall(_)
                    | ModelStreamEvent::OpaqueProviderContext(_)
                    | ModelStreamEvent::Usage(_)
            ) {
                self.visible_stream_event = true;
            }
            if let ModelStreamEvent::Usage(usage) = &event {
                self.provider.record_usage(usage.clone());
            }
            if let ModelStreamEvent::Error { message } = &event {
                self.terminal_response_failure(message.clone(), false);
                return;
            }
            self.pending.push_back(event);
        }
    }

    fn poll_next_event(
        &mut self,
        context: &mut Context<'_>,
        cancellation: CancellationToken,
    ) -> Poll<Result<Option<ModelStreamEvent>, crate::error::SchedulerError>> {
        loop {
            if self.terminal_event_emitted {
                return Poll::Ready(Ok(None));
            }
            if cancellation.is_cancelled() {
                self.response = None;
                self.decoder = None;
                self.retry_timer = None;
                self.retry_cancellation = None;
                self.pending.clear();
                self.terminal_event_emitted = true;
                return Poll::Ready(Ok(Some(ModelStreamEvent::End(StopReason::Cancelled))));
            }
            if let Some(event) = self.pending.pop_front() {
                if matches!(
                    event,
                    ModelStreamEvent::End(_)
                        | ModelStreamEvent::Error { .. }
                        | ModelStreamEvent::ContextOverflow { .. }
                        | ModelStreamEvent::Aborted { .. }
                ) {
                    self.terminal_event_emitted = true;
                    self.response = None;
                    self.decoder = None;
                    self.retry_timer = None;
                    self.retry_cancellation = None;
                }
                return Poll::Ready(Ok(Some(event)));
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
                        let force_refresh = self.retry_force_refresh;
                        self.retry_force_refresh = false;
                        if let Err(message) = self.start_attempt(force_refresh, &cancellation) {
                            self.authentication_failure(message);
                        }
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
                    self.response_headers_received = true;
                    self.status_code = Some(status_code);
                    self.response_headers = headers;
                    if cancellation.is_cancelled() {
                        continue;
                    }
                    if (200..300).contains(&status_code) {
                        self.observe_request();
                    }
                }
                Poll::Ready(StreamEvent::Chunk(bytes)) => {
                    if self
                        .status_code
                        .is_some_and(|status| !(200..300).contains(&status))
                    {
                        self.append_error_body(&bytes);
                        continue;
                    }
                    let Some(decoder) = self.decoder.as_mut() else {
                        self.terminal_response_failure(
                            "Codex response stream was not initialized".into(),
                            false,
                        );
                        continue;
                    };
                    match decoder.push(&bytes) {
                        Ok(events) => self.handle_decoded_events(events, &cancellation),
                        Err(error) => self.terminal_response_failure(error.to_string(), false),
                    }
                }
                Poll::Ready(StreamEvent::End) => {
                    if self
                        .status_code
                        .is_some_and(|status| !(200..300).contains(&status))
                    {
                        self.handle_status_failure(&cancellation);
                        continue;
                    }
                    self.response = None;
                    let Some(decoder) = self.decoder.take() else {
                        self.terminal_response_failure(
                            "Codex response stream was not initialized".into(),
                            false,
                        );
                        continue;
                    };
                    match decoder.finish() {
                        Ok(events) => self.handle_decoded_events(events, &cancellation),
                        Err(error) => self.terminal_response_failure(error.to_string(), false),
                    }
                }
                Poll::Ready(StreamEvent::Failure(failure)) => {
                    self.handle_transport_failure(failure, &cancellation);
                }
            }
        }
    }
}

impl ModelEventStream for CodexEventStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(std::future::poll_fn(move |context| {
            self.poll_next_event(context, cancellation.clone())
        }))
    }
}

impl ModelProvider for CodexProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = CodexEventStream::start(self.clone(), request, cancellation);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

fn request_headers(
    snapshot: &CodexAuthSnapshot,
    session_id: &str,
    request_id: &str,
) -> Result<Vec<(String, String)>, String> {
    validate_identifier("account identity", &snapshot.account_id)?;
    validate_identifier("session identity", session_id)?;
    validate_identifier("request identity", request_id)?;
    let headers: Vec<(String, String)> = vec![
        (
            HEADER_AUTHORIZATION.into(),
            format!("Bearer {}", snapshot.access_token.expose()),
        ),
        (HEADER_ACCOUNT_ID.into(), snapshot.account_id.clone()),
        (HEADER_ORIGINATOR.into(), ORIGINATOR.into()),
        (HEADER_VERSION.into(), CODEX_WIRE_COMPAT_VERSION.into()),
        (HEADER_USER_AGENT.into(), tea_user_agent()),
        (HEADER_OPENAI_BETA.into(), RESPONSES_BETA.into()),
        (HEADER_ACCEPT.into(), "text/event-stream".into()),
        (HEADER_CONTENT_TYPE.into(), "application/json".into()),
        (HEADER_SESSION_ID.into(), session_id.into()),
        (HEADER_CLIENT_REQUEST_ID.into(), request_id.into()),
    ];
    for (name, value) in &headers {
        if name.is_empty() || value.is_empty() || value.chars().any(char::is_control) {
            return Err("Codex request header is invalid".into());
        }
    }
    Ok(headers)
}

#[cfg(any(test, feature = "provider-codex-test-support"))]
fn redact_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            if name.eq_ignore_ascii_case(HEADER_AUTHORIZATION)
                || name.eq_ignore_ascii_case(HEADER_ACCOUNT_ID)
            {
                (name.clone(), "[redacted]".into())
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

fn validate_identifier<'a>(name: &str, value: &'a str) -> Result<&'a str, String> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(format!("Codex {name} is invalid"));
    }
    Ok(value)
}

fn new_identifier(prefix: &str) -> Result<String, String> {
    let mut bytes = [0_u8; 18];
    getrandom::fill(&mut bytes).map_err(|_| "Codex secure request randomness failed".to_owned())?;
    Ok(format!(
        "{prefix}-{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    retry_after_at(headers, SystemTime::now())
}

fn retry_after_at(headers: &[(String, String)], now: SystemTime) -> Option<Duration> {
    if let Some(milliseconds) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after-ms"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_millis(milliseconds));
    }
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.trim())?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let target = parse_imf_fixdate(value)?;
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    Some(Duration::from_secs(target.saturating_sub(now)))
}

/// Parse the IMF-fixdate form accepted by HTTP `Retry-After`, without adding
/// a general date dependency solely for this diagnostic retry boundary.
fn parse_imf_fixdate(value: &str) -> Option<u64> {
    let (weekday, rest) = value.split_once(", ")?;
    if !matches!(
        weekday,
        "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat" | "Sun"
    ) {
        return None;
    }
    let mut fields = rest.split_ascii_whitespace();
    let day = fixed_decimal(fields.next()?, 2)?;
    let month = match fields.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year = fixed_decimal(fields.next()?, 4)?;
    let time = fields.next()?;
    if fields.next()? != "GMT" || fields.next().is_some() {
        return None;
    }
    let mut time = time.split(':');
    let hour = fixed_decimal(time.next()?, 2)?;
    let minute = fixed_decimal(time.next()?, 2)?;
    let second = fixed_decimal(time.next()?, 2)?;
    if time.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let maximum_day = days_in_month(year, month)?;
    if day == 0 || day > maximum_day {
        return None;
    }
    unix_seconds(year, month, day, hour, minute, second)
}

fn fixed_decimal(value: &str, width: usize) -> Option<u32> {
    (value.len() == width && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse::<u32>().ok())
        .flatten()
}

fn days_in_month(year: u32, month: u32) -> Option<u32> {
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => 29,
        2 => 28,
        _ => return None,
    })
}

fn unix_seconds(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<u64> {
    // Howard Hinnant's civil-date conversion, anchored at the Unix epoch.
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    let days = u64::try_from(days_since_epoch).ok()?;
    days.checked_mul(86_400)?
        .checked_add(u64::from(hour) * 3_600)?
        .checked_add(u64::from(minute) * 60)?
        .checked_add(u64::from(second))
}

/// Extract the bounded Unix-second quota reset hints emitted by the direct
/// Codex backend. These are diagnostic-only metadata, never retry timers.
fn quota_reset_at(headers: &[(String, String)]) -> Option<u64> {
    [
        "x-codex-primary-reset-at",
        "x-codex-secondary-primary-reset-at",
        "x-codex-secondary-reset-at",
    ]
    .iter()
    .find_map(|header| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(header))
            .and_then(|(_, value)| value.trim().parse::<u64>().ok())
    })
    .filter(|value| (946_684_800..=4_102_444_800).contains(value))
}

fn is_retryable_status(status: u16, body: &[u8]) -> bool {
    match status {
        500 | 502 | 503 | 504 => true,
        429 => !response_indicates_quota(body),
        _ => false,
    }
}

fn response_indicates_quota(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    [
        "quota",
        "usage_limit",
        "usage_limit_reached",
        "usage_not_included",
        "insufficient_quota",
        "rate_limit_exceeded",
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance exhausted",
        "out of budget",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn response_indicates_context_overflow(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    [
        "context_length",
        "context window",
        "maximum context",
        "too many tokens",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn status_message(status: u16, body: &[u8], model: &str) -> String {
    match status {
        401 => "Codex authorization was rejected after a fresh token attempt; run `tea auth login codex`".into(),
        404 => format!(
            "Codex backend did not expose the selected model through Tea's honest originator (codex/{}, wire compatibility {}); availability can depend on account rollout",
            model, CODEX_WIRE_COMPAT_VERSION
        ),
        429 if response_indicates_quota(body) => {
            "Codex subscription quota is currently unavailable; retry after capacity is restored".into()
        }
        429 => "Codex rate limit persisted after bounded retries".into(),
        400..=499 => format!("Codex backend rejected the request with HTTP {status}"),
        500..=599 => format!("Codex backend remained unavailable after bounded retries (HTTP {status})"),
        _ => format!("Codex backend returned HTTP {status}"),
    }
}

fn codex_request_observation(
    request: &ModelRequest,
    serialized_request_bytes: usize,
    session_id: &str,
) -> AdapterRequestObservation {
    let mut components: BTreeMap<String, u64> = BTreeMap::new();
    components.insert(
        "adapter".into(),
        stable_fingerprint(b"codex-responses-sse/v1"),
    );
    components.insert(
        "wire_compatibility".into(),
        stable_fingerprint(CODEX_WIRE_COMPAT_VERSION.as_bytes()),
    );
    components.insert(
        "session_identity".into(),
        stable_fingerprint(session_id.as_bytes()),
    );
    components.insert(
        "reasoning_effort".into(),
        stable_fingerprint(format!("{:?}", request.thinking_level).as_bytes()),
    );
    components.insert(
        "tool_transport".into(),
        stable_fingerprint(if request.tools.is_empty() {
            b"no-tools"
        } else {
            b"function-tools"
        }),
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::CodexRequestCapture;
    use crate::codex::auth::{Clock, CodexAuthManager};
    use crate::codex::credentials::{
        CodexCredential, FileCredentialStore, InMemoryCredentialStore, SecretString,
    };
    use crate::codex::oauth::{CodexOAuthClient, OAuthError, OAuthHttpClient, OsRandomSource};
    use crate::hooks::{ContextEnvelope, HookSet};
    use crate::json::JsonValue;
    use crate::scheduler::{ModelEventStream, ModelProvider};
    use crate::state::{AgentMessage, MessageId, ModelDescriptor, ThinkingLevel};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn drain_request(socket: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4_096];
        let header_end = loop {
            let read = socket
                .read(&mut buffer)
                .expect("mock server should read request");
            assert_ne!(
                read, 0,
                "provider must not close before sending the request"
            );
            request.extend_from_slice(&buffer[..read]);
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let content_length = std::str::from_utf8(&request[..header_end])
            .expect("mock request headers are text")
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .expect("Codex request must include content length");
        while request.len() < header_end + content_length {
            let read = socket
                .read(&mut buffer)
                .expect("mock server should read body");
            assert_ne!(read, 0, "provider must not close before sending the body");
            request.extend_from_slice(&buffer[..read]);
        }
        request
    }

    fn serve_responses(responses: Vec<(u16, Vec<u8>)>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock Codex origin should bind");
        let address = listener.local_addr().expect("mock Codex origin address");
        let server = std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().expect("provider should connect");
                let _ = drain_request(&mut socket);
                let status_text = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    429 => "Too Many Requests",
                    500 => "Internal Server Error",
                    _ => "Test Status",
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len(),
                        )
                        .as_bytes(),
                    )
                    .expect("mock response headers should write");
                socket
                    .write_all(&body)
                    .expect("mock response body should write");
            }
        });
        (format!("http://{address}/responses"), server)
    }

    fn serve_stalled_status_then_response(
        status: u16,
        response: Vec<u8>,
    ) -> (String, std::thread::JoinHandle<()>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock Codex origin should bind");
        let address = listener.local_addr().expect("mock Codex origin address");
        let retried = Arc::new(AtomicUsize::new(0));
        let observed_retry = Arc::clone(&retried);
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("provider should connect");
            let _ = drain_request(&mut socket);
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .expect("stalled response headers should write");
            socket
                .flush()
                .expect("stalled response headers should flush");
            std::thread::sleep(Duration::from_millis(100));
            drop(socket);

            listener
                .set_nonblocking(true)
                .expect("mock listener should become nonblocking");
            let deadline = Instant::now() + Duration::from_millis(500);
            loop {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        observed_retry.fetch_add(1, Ordering::SeqCst);
                        let _ = drain_request(&mut socket);
                        socket
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    response.len(),
                                )
                                .as_bytes(),
                            )
                            .expect("retry response headers should write");
                        socket
                            .write_all(&response)
                            .expect("retry response body should write");
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("retry listener failed: {error}"),
                }
            }
        });
        (format!("http://{address}/responses"), server, retried)
    }

    fn request(context: String, session_id: &str) -> ModelRequest {
        ModelRequest {
            context,
            model: Some(ModelDescriptor {
                provider: "codex".into(),
                model: "gpt-test".into(),
                revision: None,
            }),
            thinking_level: ThinkingLevel::High,
            session_id: Some(session_id.into()),
            ..ModelRequest::default()
        }
    }

    fn collect_until_terminal(
        source: &mut dyn ModelEventStream,
        cancellation: &CancellationToken,
    ) -> Vec<ModelStreamEvent> {
        let mut events = Vec::new();
        loop {
            let event = smol::block_on(source.next_event(cancellation.clone()))
                .expect("Codex event stream should not reject polling")
                .expect("Codex response should retain a terminal event");
            let terminal = matches!(
                event,
                ModelStreamEvent::End(_)
                    | ModelStreamEvent::Error { .. }
                    | ModelStreamEvent::ContextOverflow { .. }
                    | ModelStreamEvent::Aborted { .. }
            );
            events.push(event);
            if terminal {
                return events;
            }
        }
    }

    fn sse(events: &str) -> Vec<u8> {
        events.as_bytes().to_vec()
    }

    fn provider() -> CodexProvider {
        let credential = CodexCredential::new(
            SecretString::new("header.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xMjM0In19.signature").unwrap(),
            SecretString::new("refresh").unwrap(),
            4_102_444_000_000,
            "acct_1234",
            1,
        )
        .unwrap();
        let auth = Arc::new(CodexAuthManager::with_system_clock(Arc::new(
            InMemoryCredentialStore::with_credential(credential),
        )));
        CodexProvider::new(CodexConfig::new(auth, "gpt-test"))
    }

    #[test]
    fn request_headers_are_honest_and_redacted_in_capture() {
        let snapshot = CodexAuthSnapshot {
            access_token: SecretString::new("access").unwrap(),
            account_id: "acct_1234".into(),
        };
        let headers = request_headers(&snapshot, "session", "request").unwrap();
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "originator" && value == "tea")
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "version" && value == CODEX_WIRE_COMPAT_VERSION)
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "OpenAI-Beta" && value == RESPONSES_BETA)
        );
        let captured = redact_headers(&headers);
        assert!(!format!("{captured:?}").contains("access"));
        assert!(!format!("{captured:?}").contains("acct_1234"));
    }

    #[test]
    fn request_headers_reject_all_control_characters_before_network_io() {
        let snapshot = CodexAuthSnapshot {
            access_token: SecretString::new("access").expect("safe access fixture"),
            account_id: "acct\tunsafe".into(),
        };
        assert!(request_headers(&snapshot, "session", "request").is_err());

        let snapshot = CodexAuthSnapshot {
            access_token: SecretString::new("access").expect("safe access fixture"),
            account_id: "acct_1234".into(),
        };
        assert!(request_headers(&snapshot, "session\u{0000}unsafe", "request").is_err());
    }

    #[test]
    fn cancellation_wins_over_a_queued_unexposed_stream_event() {
        let start_cancellation = CancellationToken::new();
        start_cancellation.cancel();
        let mut stream =
            CodexEventStream::start(provider(), ModelRequest::default(), start_cancellation);
        stream.pending.clear();
        stream
            .pending
            .push_back(ModelStreamEvent::TextDelta("not yet exposed".into()));
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let event = smol::block_on(stream.next_event(cancellation))
            .expect("cancellation must not reject polling");
        assert!(matches!(
            event,
            Some(ModelStreamEvent::End(StopReason::Cancelled))
        ));
    }

    #[test]
    fn cancellation_terminal_event_is_emitted_once_then_the_stream_closes() {
        let start_cancellation = CancellationToken::new();
        start_cancellation.cancel();
        let mut stream = CodexEventStream::start(
            provider(),
            ModelRequest::default(),
            start_cancellation.clone(),
        );

        assert!(matches!(
            smol::block_on(stream.next_event(start_cancellation.clone()))
                .expect("first cancellation poll succeeds"),
            Some(ModelStreamEvent::End(StopReason::Cancelled))
        ));
        assert!(
            smol::block_on(stream.next_event(start_cancellation))
                .expect("post-terminal cancellation poll succeeds")
                .is_none(),
            "a model stream must close after its terminal cancellation event"
        );
    }

    #[test]
    fn model_validation_requires_exact_codex_descriptor() {
        let provider = provider();
        let request = ModelRequest {
            model: Some(ModelDescriptor {
                provider: "codex".into(),
                model: "gpt-test".into(),
                revision: None,
            }),
            thinking_level: ThinkingLevel::Off,
            ..ModelRequest::default()
        };
        assert!(provider.validate_model(&request).is_ok());
    }

    #[test]
    fn loopback_transport_replays_encrypted_reasoning_once_on_the_next_turn() {
        let first = sse(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"first answer\"}\n\n\
             data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"encrypted-state\"}}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}\n\n",
        );
        let second = sse(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"second answer\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":1,\"total_tokens\":5}}}\n\n",
        );
        let (endpoint, server) = serve_responses(vec![(200, first), (200, second)]);
        let capture = CodexRequestCapture::default();
        let auth = Arc::new(CodexAuthManager::with_system_clock(Arc::new(
            InMemoryCredentialStore::with_credential(
                CodexCredential::new(
                    SecretString::new("access-token").unwrap(),
                    SecretString::new("refresh-token").unwrap(),
                    4_102_444_000_000,
                    "acct_12345678",
                    1,
                )
                .unwrap(),
            ),
        )));
        let provider = CodexProvider::new(
            CodexConfig::new(auth, "gpt-test")
                .with_test_responses_url(endpoint)
                .with_request_capture(capture.clone()),
        );
        let cancellation = CancellationToken::new();
        let initial_context = crate::codex::CodexContextHook
            .convert_to_llm(ContextEnvelope {
                version: 1,
                messages: vec![AgentMessage::User {
                    id: MessageId(1),
                    content: "first prompt".into(),
                }],
                host_messages: Vec::new(),
            })
            .expect("first Codex context should convert");
        let mut first_source = smol::block_on(provider.stream(
            request(initial_context, "durable-session"),
            cancellation.clone(),
        ))
        .expect("first response source should start");
        let first_events = collect_until_terminal(first_source.as_mut(), &cancellation);
        assert!(first_events.iter().any(|event| {
            matches!(event, ModelStreamEvent::TextDelta(text) if text == "first answer")
        }));
        assert!(first_events.iter().any(|event| {
            matches!(
                event,
                ModelStreamEvent::Usage(Usage {
                    total_tokens: Some(3),
                    cost: None,
                    ..
                })
            )
        }));
        let opaque = first_events
            .iter()
            .find_map(|event| match event {
                ModelStreamEvent::OpaqueProviderContext(item) => Some(item.clone()),
                _ => None,
            })
            .expect("first response should preserve encrypted reasoning");
        let next_context = crate::codex::CodexContextHook
            .convert_to_llm(ContextEnvelope {
                version: 1,
                messages: vec![
                    AgentMessage::User {
                        id: MessageId(1),
                        content: "first prompt".into(),
                    },
                    AgentMessage::Assistant {
                        id: MessageId(2),
                        content: "first answer".into(),
                        tool_calls: Vec::new(),
                        stop_reason: Some(StopReason::Stop),
                        error_message: None,
                        opaque_context: vec![opaque],
                    },
                ],
                host_messages: Vec::new(),
            })
            .expect("second Codex context should convert");
        let mut second_source = smol::block_on(provider.stream(
            request(next_context, "durable-session"),
            cancellation.clone(),
        ))
        .expect("second response source should start");
        let second_events = collect_until_terminal(second_source.as_mut(), &cancellation);
        assert!(second_events.iter().any(|event| {
            matches!(event, ModelStreamEvent::TextDelta(text) if text == "second answer")
        }));
        server.join().expect("mock Codex server should finish");

        let captures = capture.requests();
        assert_eq!(captures.len(), 2);
        for captured in &captures {
            assert_eq!(captured.headers.len(), 10);
            assert_eq!(
                captured.headers[0],
                (HEADER_AUTHORIZATION.into(), "[redacted]".into())
            );
            assert_eq!(
                captured.headers[1],
                (HEADER_ACCOUNT_ID.into(), "[redacted]".into())
            );
            assert_eq!(
                captured.headers[2],
                (HEADER_ORIGINATOR.into(), "tea".into())
            );
            assert_eq!(
                captured.headers[3],
                (HEADER_VERSION.into(), CODEX_WIRE_COMPAT_VERSION.into())
            );
            assert_eq!(
                captured.headers[4],
                (HEADER_USER_AGENT.into(), tea_user_agent())
            );
            assert_eq!(
                captured.headers[5],
                (HEADER_OPENAI_BETA.into(), RESPONSES_BETA.into())
            );
            assert_eq!(
                captured.headers[6],
                (HEADER_ACCEPT.into(), "text/event-stream".into())
            );
            assert_eq!(
                captured.headers[7],
                (HEADER_CONTENT_TYPE.into(), "application/json".into())
            );
            assert_eq!(
                captured.headers[8],
                (HEADER_SESSION_ID.into(), "durable-session".into())
            );
            assert!(
                captured.headers[9]
                    .0
                    .eq_ignore_ascii_case(HEADER_CLIENT_REQUEST_ID)
            );
            assert!(captured.headers[9].1.starts_with("tea-request-"));
        }
        let second_payload = JsonValue::parse(
            std::str::from_utf8(&captures[1].payload).expect("captured JSON is UTF-8"),
        )
        .expect("captured payload is JSON");
        let input = second_payload
            .get("input")
            .and_then(JsonValue::as_array)
            .expect("second payload has Responses input");
        let replayed_reasoning = input
            .iter()
            .filter(|item| {
                item.get("type").and_then(JsonValue::as_str) == Some("reasoning")
                    && item.get("encrypted_content").and_then(JsonValue::as_str)
                        == Some("encrypted-state")
            })
            .count();
        assert_eq!(replayed_reasoning, 1);
        let reasoning_index = input
            .iter()
            .position(|item| item.get("type").and_then(JsonValue::as_str) == Some("reasoning"))
            .expect("replayed reasoning item");
        let assistant_index = input
            .iter()
            .position(|item| item.get("role").and_then(JsonValue::as_str) == Some("assistant"))
            .expect("visible assistant item");
        assert!(reasoning_index < assistant_index);
    }

    #[test]
    fn retryable_pre_stream_failure_reuses_the_logical_request_identity() {
        let completed = sse(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"recovered\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        let (endpoint, server) = serve_responses(vec![(500, Vec::new()), (200, completed)]);
        let capture = CodexRequestCapture::default();
        let auth = Arc::new(CodexAuthManager::with_system_clock(Arc::new(
            InMemoryCredentialStore::with_credential(
                CodexCredential::new(
                    SecretString::new("access-token").unwrap(),
                    SecretString::new("refresh-token").unwrap(),
                    4_102_444_000_000,
                    "acct_12345678",
                    1,
                )
                .unwrap(),
            ),
        )));
        let provider = CodexProvider::new(
            CodexConfig::new(auth, "gpt-test")
                .with_test_responses_url(endpoint)
                .with_request_capture(capture.clone())
                .with_retry_policy(crate::retry::RetryPolicy::new(
                    1,
                    Duration::ZERO,
                    Duration::ZERO,
                )),
        );
        let cancellation = CancellationToken::new();
        let mut source = smol::block_on(provider.stream(
            request("[]".into(), "durable-session"),
            cancellation.clone(),
        ))
        .expect("response source should start");
        let events = collect_until_terminal(source.as_mut(), &cancellation);
        assert!(events.iter().any(|event| {
            matches!(event, ModelStreamEvent::TextDelta(text) if text == "recovered")
        }));
        server.join().expect("retry mock server should finish");
        let captures = capture.requests();
        assert_eq!(captures.len(), 2);
        assert_eq!(captures[0].payload, captures[1].payload);
        assert_eq!(captures[0].headers[8], captures[1].headers[8]);
        assert_eq!(captures[0].headers[9], captures[1].headers[9]);
    }

    #[test]
    fn quota_exhaustion_is_terminal_and_retry_headers_are_bounded() {
        assert!(!is_retryable_status(
            429,
            br#"{"error":{"code":"usage_limit_reached"}}"#,
        ));
        assert!(is_retryable_status(
            429,
            br#"{"error":{"code":"temporary"}}"#
        ));
        assert_eq!(
            retry_after(&[("retry-after-ms".into(), "17".into())]),
            Some(Duration::from_millis(17)),
        );
        assert_eq!(
            retry_after(&[("retry-after".into(), "2".into())]),
            Some(Duration::from_secs(2)),
        );
        assert_eq!(
            quota_reset_at(&[("x-codex-primary-reset-at".into(), "1704069000".into(),)]),
            Some(1_704_069_000),
        );
        assert_eq!(
            quota_reset_at(&[("x-codex-primary-reset-at".into(), "0".into(),)]),
            None,
        );
    }

    #[test]
    fn retry_after_accepts_a_valid_http_date() {
        let target = parse_imf_fixdate("Fri, 01 Jan 2027 00:00:05 GMT")
            .expect("fixture HTTP date should parse");
        let now = std::time::UNIX_EPOCH + Duration::from_secs(target.saturating_sub(5));
        assert_eq!(
            retry_after_at(
                &[("retry-after".into(), "Fri, 01 Jan 2027 00:00:05 GMT".into(),)],
                now,
            ),
            Some(Duration::from_secs(5)),
        );
    }

    #[derive(Clone)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now_unix_ms(&self) -> Result<u64, crate::codex::AuthError> {
            Ok(10_000)
        }
    }

    struct RefreshTransport {
        calls: AtomicUsize,
    }

    impl OAuthHttpClient for RefreshTransport {
        fn send(
            &self,
            _request: tea_http::TransportRequest,
            _cancellation: &CancellationToken,
        ) -> Result<tea_http::TransportResponse, OAuthError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(tea_http::TransportResponse {
                status_code: 200,
                headers: Vec::new(),
                body: br#"{"access_token":"new-access","expires_in":3600}"#.to_vec(),
            })
        }
    }

    #[test]
    fn one_pre_stream_401_forces_one_refresh_then_replays_the_logical_request() {
        let completed = sse(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fresh\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        let (endpoint, server) = serve_responses(vec![(401, Vec::new()), (200, completed)]);
        let refresh_transport = Arc::new(RefreshTransport {
            calls: AtomicUsize::new(0),
        });
        let auth = Arc::new(CodexAuthManager::new(
            Arc::new(InMemoryCredentialStore::with_credential(
                CodexCredential::new(
                    SecretString::new("old-access").unwrap(),
                    SecretString::new("old-refresh").unwrap(),
                    4_102_444_000_000,
                    "acct_12345678",
                    1,
                )
                .unwrap(),
            )),
            CodexOAuthClient::new(refresh_transport.clone(), Arc::new(OsRandomSource)),
            Arc::new(FixedClock),
        ));
        let capture = CodexRequestCapture::default();
        let provider = CodexProvider::new(
            CodexConfig::new(auth, "gpt-test")
                .with_test_responses_url(endpoint)
                .with_request_capture(capture.clone())
                .with_retry_policy(crate::retry::RetryPolicy::new(
                    0,
                    Duration::ZERO,
                    Duration::ZERO,
                )),
        );
        let cancellation = CancellationToken::new();
        let mut source = smol::block_on(provider.stream(
            request("[]".into(), "durable-session"),
            cancellation.clone(),
        ))
        .expect("response source should start");
        let events = collect_until_terminal(source.as_mut(), &cancellation);
        assert!(events.iter().any(|event| {
            matches!(event, ModelStreamEvent::TextDelta(text) if text == "fresh")
        }));
        server.join().expect("401 mock server should finish");
        assert_eq!(refresh_transport.calls.load(Ordering::SeqCst), 1);
        let captures = capture.requests();
        assert_eq!(captures.len(), 2);
        assert_eq!(captures[0].headers[9], captures[1].headers[9]);
    }

    #[test]
    fn stalled_pre_output_401_still_forces_one_refresh_and_replays() {
        let completed = sse(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fresh after stall\"}\n\n\
             data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        let (endpoint, server, retried) = serve_stalled_status_then_response(401, completed);
        let refresh_transport = Arc::new(RefreshTransport {
            calls: AtomicUsize::new(0),
        });
        let auth = Arc::new(CodexAuthManager::new(
            Arc::new(InMemoryCredentialStore::with_credential(
                CodexCredential::new(
                    SecretString::new("old-access").unwrap(),
                    SecretString::new("old-refresh").unwrap(),
                    4_102_444_000_000,
                    "acct_12345678",
                    1,
                )
                .unwrap(),
            )),
            CodexOAuthClient::new(refresh_transport.clone(), Arc::new(OsRandomSource)),
            Arc::new(FixedClock),
        ));
        let provider = CodexProvider::new(
            CodexConfig::new(auth, "gpt-test")
                .with_test_responses_url(endpoint)
                .with_request_timeout(Duration::from_secs(2))
                .with_stall_timeout(Duration::from_millis(20))
                .with_retry_policy(crate::retry::RetryPolicy::new(
                    0,
                    Duration::ZERO,
                    Duration::ZERO,
                )),
        );
        let cancellation = CancellationToken::new();
        let mut source = smol::block_on(provider.stream(
            request("[]".into(), "durable-session"),
            cancellation.clone(),
        ))
        .expect("response source should start");
        let events = collect_until_terminal(source.as_mut(), &cancellation);
        server.join().expect("401 stall mock server should finish");

        assert_eq!(retried.load(Ordering::SeqCst), 1);
        assert_eq!(refresh_transport.calls.load(Ordering::SeqCst), 1);
        assert!(events.iter().any(|event| {
            matches!(event, ModelStreamEvent::TextDelta(text) if text == "fresh after stall")
        }));
    }

    /// Deliberately excluded from ordinary tests: it reaches the fixed direct
    /// ChatGPT backend only after the operator opts in and supplies a separate
    /// Tea-owned credential record created by `tea auth login codex`.
    #[test]
    #[ignore = "requires TEA_CODEX_LIVE_SMOKE=1, TEA_CODEX_CREDENTIAL_PATH, and TEA_CODEX_LIVE_MODEL"]
    fn live_chatgpt_subscription_smoke() {
        if std::env::var("TEA_CODEX_LIVE_SMOKE").as_deref() != Ok("1") {
            eprintln!("Codex live smoke is not opted in; set TEA_CODEX_LIVE_SMOKE=1 to run it");
            return;
        }
        let credential_path = std::env::var_os("TEA_CODEX_CREDENTIAL_PATH")
            .map(std::path::PathBuf::from)
            .expect("live Codex smoke requires an explicit Tea credential path");
        let is_tea_owned_path = credential_path.is_absolute()
            && credential_path.file_name() == Some(std::ffi::OsStr::new("codex.json"))
            && credential_path
                .parent()
                .and_then(std::path::Path::file_name)
                == Some(std::ffi::OsStr::new("auth"));
        assert!(
            is_tea_owned_path,
            "live Codex smoke requires an absolute Tea auth/codex.json credential path"
        );
        let is_codex_client_path = credential_path.file_name()
            == Some(std::ffi::OsStr::new("auth.json"))
            && credential_path
                .parent()
                .and_then(std::path::Path::file_name)
                == Some(std::ffi::OsStr::new(".codex"));
        assert!(
            !is_codex_client_path,
            "live Codex smoke refuses the independent Codex client credential path"
        );
        let model = std::env::var("TEA_CODEX_LIVE_MODEL")
            .expect("live Codex smoke requires an explicit current model name");
        assert!(
            !model.trim().is_empty(),
            "live Codex model name must not be empty"
        );

        let auth = Arc::new(CodexAuthManager::with_system_clock(Arc::new(
            FileCredentialStore::new(credential_path),
        )));
        let provider = CodexProvider::new(
            CodexConfig::try_new(auth, model.clone())
                .expect("explicit live Codex configuration should be valid"),
        );
        let context = crate::codex::CodexContextHook
            .convert_to_llm(ContextEnvelope {
                version: 1,
                messages: vec![AgentMessage::User {
                    id: MessageId(1),
                    content: "Reply with exactly: tea codex smoke ok".into(),
                }],
                host_messages: Vec::new(),
            })
            .expect("live smoke context should convert to Responses input");
        let session_id = new_identifier("tea-live-smoke")
            .expect("live smoke session identity should use secure randomness");
        let cancellation = CancellationToken::new();
        let mut source = smol::block_on(provider.stream(
            ModelRequest {
                context,
                model: Some(ModelDescriptor {
                    provider: "codex".into(),
                    model,
                    revision: None,
                }),
                thinking_level: ThinkingLevel::Off,
                session_id: Some(session_id),
                ..ModelRequest::default()
            },
            cancellation.clone(),
        ))
        .expect("live Codex stream should start");
        let events = collect_until_terminal(source.as_mut(), &cancellation);
        assert!(
            events.iter().any(
                |event| matches!(event, ModelStreamEvent::TextDelta(text) if !text.is_empty())
            ),
            "live Codex smoke expected at least one assistant text delta"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ModelStreamEvent::End(_))),
            "live Codex smoke expected a terminal model event"
        );
    }
}
