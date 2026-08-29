//! Shared pooled client, bounded JSON exchange, retry, and batch execution.

use crate::route::{HttpMethod, Route, RouteError};
use bytes::{Buf, Bytes};
use futures_util::future::{self, Either};
use h12tiny_client::{Client as H12Client, ErrorKind, RequestOptions};
use h12tiny_core::runtime::{BoxExecutor, BoxSendFuture, FnExecutor};
use http::header::{CONTENT_TYPE, RETRY_AFTER};
use http::{Request, StatusCode};
use http_body::{Body, Frame, SizeHint};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
use tea_core::scheduler::CancellationToken;
use tea_protocol::JsonValue;

const MAX_BATCH_REQUESTS: usize = 16;
const MAX_DIAGNOSTIC_BYTES: usize = 240;

/// A host-selected bounded retry policy. `max_retries` is in addition to the
/// first attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_retries: u32,
    initial_delay: Duration,
    max_delay: Duration,
}

impl RetryPolicy {
    /// Create a bounded exponential retry policy.
    pub fn new(max_retries: u32, initial_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_retries,
            initial_delay,
            max_delay: max_delay.max(initial_delay),
        }
    }

    /// The conservative policy selected for the Firecrawl route.
    pub const fn firecrawl() -> Self {
        Self {
            max_retries: 1,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(2),
        }
    }

    /// Number of retries after the initial attempt.
    pub const fn max_retries(self) -> u32 {
        self.max_retries
    }

    /// Base retry delay.
    pub const fn initial_delay(self) -> Duration {
        self.initial_delay
    }

    /// Maximum retry and `Retry-After` delay used by this route.
    pub const fn max_delay(self) -> Duration {
        self.max_delay
    }

    fn delay_before_retry(self, retry_index: u32) -> Duration {
        let mut delay = self.initial_delay;
        for _ in 0..retry_index {
            delay = delay.checked_mul(2).unwrap_or(self.max_delay);
            if delay >= self.max_delay {
                return self.max_delay;
            }
        }
        delay.min(self.max_delay)
    }
}

/// An authorized JSON request. It deliberately carries a route name and path,
/// never an arbitrary user-provided origin or URL.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    /// Host-configured route name.
    pub route: String,
    /// Extension-visible method.
    pub method: HttpMethod,
    /// Route-allowed path.
    pub path: String,
    /// JSON request body.
    pub json: JsonValue,
}

/// Structured completion presented to the extension policy.
#[derive(Clone, Debug, PartialEq)]
pub enum HttpOutcome {
    /// A complete HTTP response. Non-success status codes stay structured so
    /// the provider policy, rather than Rust transport, can interpret them.
    Response {
        /// HTTP status code.
        status: u16,
        /// Total requests attempted, including the first.
        attempts: u32,
        /// Small allowlisted response headers useful to generic policy.
        headers: BTreeMap<String, String>,
        /// Parsed JSON response body.
        json: JsonValue,
    },
    /// A bounded transport or response-decoding failure.
    TransportError {
        /// Stable failure classification.
        code: TransportErrorCode,
        /// Total requests attempted, including the first.
        attempts: u32,
        /// Host-safe concise diagnostic.
        message: String,
    },
}

/// Stable transport classes suitable for provider policy without exposing
/// h12tiny implementation details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportErrorCode {
    /// The owning Tea operation was cancelled.
    Cancelled,
    /// A route-wide deadline elapsed.
    Timeout,
    /// DNS failed or exceeded its deadline.
    Dns,
    /// TCP connection establishment failed.
    Connect,
    /// TLS validation, negotiation, or ALPN failed.
    Tls,
    /// Sending the request failed.
    Write,
    /// Reading or framing the response failed.
    Read,
    /// The bounded response collector rejected an oversized body.
    BodyTooLarge,
    /// A response advertised as JSON was malformed or not UTF-8.
    InvalidResponse,
}

impl TransportErrorCode {
    /// Stable extension-facing code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Write => "write",
            Self::Read => "read",
            Self::BodyTooLarge => "body_too_large",
            Self::InvalidResponse => "invalid_response",
        }
    }
}

/// Construct the h12tiny executor adapter from an embedding-owned task
/// submission closure. `tea-http` never creates or drives an async runtime.
pub fn background_executor<F>(submit: F) -> BoxExecutor
where
    F: Fn(BoxSendFuture) + Clone + Send + Sync + 'static,
{
    BoxExecutor::new(FnExecutor::new(submit))
}

/// One shared h12tiny client and host-defined route policies.
#[derive(Clone)]
pub struct Client {
    inner: Arc<H12Client<RequestBody>>,
    routes: Arc<BTreeMap<String, Arc<RouteRuntime>>>,
}

impl Client {
    /// Create a pooled client whose connection work is submitted by the
    /// embedding host. Duplicate route names are rejected.
    pub fn new(
        executor: BoxExecutor,
        routes: impl IntoIterator<Item = Route>,
    ) -> Result<Self, ClientError> {
        let mut configured = BTreeMap::new();
        for route in routes {
            let name = route.name().to_owned();
            if configured
                .insert(name.clone(), Arc::new(RouteRuntime::new(route)))
                .is_some()
            {
                return Err(ClientError::DuplicateRoute(name));
            }
        }
        if configured.is_empty() {
            return Err(ClientError::NoRoutes);
        }
        let mut builder = H12Client::builder(executor);
        // Route schedulers impose a narrower in-flight limit. A finite pool
        // bound also prevents an accidental route addition from multiplying
        // idle H1 connections without changing the host policy deliberately.
        builder.pool_max_connections_per_host(4);
        Ok(Self {
            inner: Arc::new(builder.build()),
            routes: Arc::new(configured),
        })
    }

    /// Return the stable semantics for every host-configured route.
    pub fn route_identities(&self) -> Vec<String> {
        self.routes
            .values()
            .map(|route| route.route.semantic_identity())
            .collect()
    }

    /// Execute one route-authorized JSON request.
    pub async fn request(
        &self,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> Result<HttpOutcome, ClientError> {
        let route = self.authorize(&request)?;
        Ok(self.execute(route, request, cancellation).await)
    }

    /// Execute bounded independent requests concurrently. The returned vector
    /// preserves input order even when route work completes out of order.
    pub async fn request_many(
        &self,
        requests: Vec<HttpRequest>,
        cancellation: CancellationToken,
    ) -> Result<Vec<HttpOutcome>, ClientError> {
        if requests.is_empty() || requests.len() > MAX_BATCH_REQUESTS {
            return Err(ClientError::InvalidBatchCount {
                maximum: MAX_BATCH_REQUESTS,
                actual: requests.len(),
            });
        }
        let authorized = requests
            .iter()
            .map(|request| self.authorize(request))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(future::join_all(
            requests
                .into_iter()
                .zip(authorized)
                .map(|(request, route)| self.execute(route, request, cancellation.clone())),
        )
        .await
        .into_iter()
        .collect::<Vec<_>>())
    }

    fn authorize(&self, request: &HttpRequest) -> Result<Arc<RouteRuntime>, ClientError> {
        let route = self
            .routes
            .get(&request.route)
            .cloned()
            .ok_or_else(|| ClientError::UnknownRoute(request.route.clone()))?;
        route
            .route
            .authorize(request.method, &request.path)
            .map_err(ClientError::Route)?;
        let encoded = request
            .json
            .to_json_string()
            .map_err(|error| ClientError::InvalidJson(error.to_string()))?;
        if encoded.len() > route.route.max_request_bytes() {
            return Err(ClientError::RequestTooLarge {
                maximum: route.route.max_request_bytes(),
                actual: encoded.len(),
            });
        }
        Ok(route)
    }

    async fn execute(
        &self,
        route: Arc<RouteRuntime>,
        request: HttpRequest,
        cancellation: CancellationToken,
    ) -> HttpOutcome {
        let deadline = Instant::now() + route.route.timeout();
        let mut attempts = 0;
        let mut retry_index = 0;
        loop {
            if cancellation.is_cancelled() {
                return transport(TransportErrorCode::Cancelled, attempts, "request cancelled");
            }
            if Instant::now() >= deadline {
                return transport(TransportErrorCode::Timeout, attempts, "request timed out");
            }
            let permit = match route.acquire(&cancellation, deadline).await {
                Ok(permit) => permit,
                Err(code) => return transport(code, attempts, code_message(code)),
            };
            attempts += 1;
            let attempt = self.execute_once(&route, &request, &cancellation, deadline).await;
            drop(permit);
            match attempt {
                Ok(response) => {
                    let retry_after = response
                        .headers
                        .get("retry-after")
                        .and_then(|value| retry_after(value, route.route.retry().max_delay()));
                    if response.status == StatusCode::TOO_MANY_REQUESTS.as_u16() {
                        if let Some(delay) = retry_after {
                            route.set_cooldown(delay);
                        }
                    }
                    if retryable_status(response.status) && retry_index < route.route.retry().max_retries() {
                        let delay = retry_after.unwrap_or_else(|| route.route.retry().delay_before_retry(retry_index));
                        retry_index += 1;
                        if let Err(code) = wait(delay, &cancellation, deadline).await {
                            return transport(code, attempts, code_message(code));
                        }
                        continue;
                    }
                    return HttpOutcome::Response {
                        status: response.status,
                        attempts,
                        headers: response.headers,
                        json: response.json,
                    };
                }
                Err(error) if retryable_transport(error.code) && retry_index < route.route.retry().max_retries() => {
                    let delay = route.route.retry().delay_before_retry(retry_index);
                    retry_index += 1;
                    if let Err(code) = wait(delay, &cancellation, deadline).await {
                        return transport(code, attempts, code_message(code));
                    }
                }
                Err(error) => return transport(error.code, attempts, &error.message),
            }
        }
    }

    async fn execute_once(
        &self,
        route: &RouteRuntime,
        request: &HttpRequest,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<AttemptResponse, AttemptError> {
        let body = request
            .json
            .to_json_string()
            .map_err(|_| AttemptError::new(TransportErrorCode::InvalidResponse, "cannot encode JSON request"))?
            .into_bytes();
        let http_request = Request::builder()
            .method(request.method.as_str())
            .uri(route.route.origin().uri(&request.path))
            .header(CONTENT_TYPE, "application/json")
            .body(RequestBody::new(body))
            .map_err(|error| AttemptError::new(TransportErrorCode::Write, &format!("cannot build request: {error}")))?;
        let options = RequestOptions::new()
            .with_dns_timeout(remaining(deadline)?)
            .with_connect_timeout(remaining(deadline)?)
            .with_tls_timeout(remaining(deadline)?)
            .with_headers_timeout(remaining(deadline)?);
        let response = run_until(
            self.inner.request_with_options(http_request, options),
            cancellation,
            deadline,
        )
        .await?
        .map_err(AttemptError::from_h12)?;
        let (parts, body) = response.into_parts();
        let headers = visible_headers(&parts.headers);
        let bytes = collect_body(body, route.route.max_response_bytes(), cancellation, deadline).await?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| AttemptError::new(TransportErrorCode::InvalidResponse, "response body was not valid UTF-8"))?;
        let json = JsonValue::parse(text)
            .map_err(|_| AttemptError::new(TransportErrorCode::InvalidResponse, "response body was not valid JSON"))?;
        Ok(AttemptResponse {
            status: parts.status.as_u16(),
            headers,
            json,
        })
    }
}

#[derive(Debug)]
struct RouteRuntime {
    route: Route,
    state: Mutex<RouteState>,
}

impl RouteRuntime {
    fn new(route: Route) -> Self {
        Self {
            route,
            state: Mutex::new(RouteState::default()),
        }
    }

    async fn acquire(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<RoutePermit, TransportErrorCode> {
        loop {
            if cancellation.is_cancelled() {
                return Err(TransportErrorCode::Cancelled);
            }
            let now = Instant::now();
            let wait_for = {
                let mut state = self.state.lock().expect("route rate state mutex poisoned");
                if state.in_flight < self.route.rate().max_in_flight.get()
                    && state.cooldown_until.is_none_or(|until| until <= now)
                    && state.next_start.is_none_or(|start| start <= now)
                {
                    state.in_flight += 1;
                    state.next_start = self.route.rate().minimum_interval.map(|interval| now + interval);
                    return Ok(RoutePermit { route: Arc::clone(self) });
                }
                state
                    .cooldown_until
                    .filter(|until| *until > now)
                    .or_else(|| state.next_start.filter(|start| *start > now))
                    .map(|until| until.saturating_duration_since(now))
            };
            wait_for_state(self, wait_for, cancellation, deadline).await?;
        }
    }

    fn set_cooldown(&self, delay: Duration) {
        let until = Instant::now() + delay;
        let waiters = {
            let mut state = self.state.lock().expect("route rate state mutex poisoned");
            if state.cooldown_until.is_none_or(|existing| existing < until) {
                state.cooldown_until = Some(until);
            }
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

#[derive(Default, Debug)]
struct RouteState {
    in_flight: usize,
    cooldown_until: Option<Instant>,
    next_start: Option<Instant>,
    waiters: Vec<Waker>,
}

struct RoutePermit {
    route: Arc<RouteRuntime>,
}

impl Drop for RoutePermit {
    fn drop(&mut self) {
        let waiters = {
            let mut state = self.route.state.lock().expect("route rate state mutex poisoned");
            state.in_flight = state.in_flight.saturating_sub(1);
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

async fn wait_for_state(
    route: &Arc<RouteRuntime>,
    delay: Option<Duration>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), TransportErrorCode> {
    let route = Arc::clone(route);
    let waiting = async move {
        let notified = StateNotification { route };
        match delay {
            Some(delay) => {
                let _ = future::select(Box::pin(notified), Box::pin(async_io::Timer::after(delay))).await;
            }
            None => notified.await,
        }
    };
    let timed = future::select(Box::pin(waiting), Box::pin(async_io::Timer::at(deadline)));
    match future::select(Box::pin(timed), Box::pin(cancellation.cancelled())).await {
        Either::Left((Either::Left(_), _)) => Ok(()),
        Either::Left((Either::Right(_), _)) => Err(TransportErrorCode::Timeout),
        Either::Right(_) => Err(TransportErrorCode::Cancelled),
    }
}

struct StateNotification {
    route: Arc<RouteRuntime>,
}

impl Future for StateNotification {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.route.state.lock().expect("route rate state mutex poisoned");
        if !state.waiters.iter().any(|waiter| waiter.will_wake(context.waker())) {
            state.waiters.push(context.waker().clone());
        }
        Poll::Pending
    }
}

struct RequestBody(Option<Bytes>);

impl RequestBody {
    fn new(body: Vec<u8>) -> Self {
        Self(Some(Bytes::from(body)))
    }
}

impl Body for RequestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(self.0.take().map(|bytes| Ok(Frame::data(bytes))))
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        hint.set_exact(self.0.as_ref().map_or(0, |body| body.len() as u64));
        hint
    }
}

struct AttemptResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    json: JsonValue,
}

struct AttemptError {
    code: TransportErrorCode,
    message: String,
}

impl AttemptError {
    fn new(code: TransportErrorCode, message: &str) -> Self {
        Self {
            code,
            message: bounded(message),
        }
    }

}

async fn collect_body<B>(
    mut body: B,
    maximum: usize,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>, AttemptError>
where
    B: Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let mut bytes = Vec::new();
    loop {
        let frame = run_until(
            future::poll_fn(|context| Pin::new(&mut body).poll_frame(context)),
            cancellation,
            deadline,
        )
        .await?;
        let Some(frame) = frame else {
            return Ok(bytes);
        };
        let frame = frame.map_err(|error| AttemptError::new(TransportErrorCode::Read, &format!("response body read failed: {error}")))?;
        if let Ok(mut data) = frame.into_data() {
            if data.remaining() > maximum.saturating_sub(bytes.len()) {
                return Err(AttemptError::new(TransportErrorCode::BodyTooLarge, "response body exceeded route limit"));
            }
            while data.has_remaining() {
                let chunk = data.chunk();
                bytes.extend_from_slice(chunk);
                let length = chunk.len();
                data.advance(length);
            }
        }
    }
}

async fn run_until<T>(
    operation: impl Future<Output = T>,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<T, AttemptError> {
    let timed = future::select(Box::pin(operation), Box::pin(async_io::Timer::at(deadline)));
    match future::select(Box::pin(timed), Box::pin(cancellation.cancelled())).await {
        Either::Left((Either::Left((value, _)), _)) => Ok(value),
        Either::Left((Either::Right(_), _)) => Err(AttemptError::new(TransportErrorCode::Timeout, "request timed out")),
        Either::Right(_) => Err(AttemptError::new(TransportErrorCode::Cancelled, "request cancelled")),
    }
}

async fn wait(
    delay: Duration,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), TransportErrorCode> {
    if delay.is_zero() {
        return Ok(());
    }
    run_until(async_io::Timer::after(delay), cancellation, deadline)
        .await
        .map(|_| ())
        .map_err(|error| error.code)
}

fn remaining(deadline: Instant) -> Result<Duration, AttemptError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| AttemptError::new(TransportErrorCode::Timeout, "request timed out"))
}

fn visible_headers(headers: &http::HeaderMap) -> BTreeMap<String, String> {
    [RETRY_AFTER, CONTENT_TYPE]
        .into_iter()
        .filter_map(|name| {
            headers
                .get(&name)
                .and_then(|value| value.to_str().ok())
                .map(|value| (name.as_str().to_ascii_lowercase(), bounded(value)))
        })
        .collect()
}

fn retry_after(value: &str, maximum: Duration) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .map(|delay| delay.min(maximum))
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn retryable_transport(code: TransportErrorCode) -> bool {
    matches!(
        code,
        TransportErrorCode::Timeout
            | TransportErrorCode::Dns
            | TransportErrorCode::Connect
            | TransportErrorCode::Tls
            | TransportErrorCode::Write
            | TransportErrorCode::Read
    )
}

fn transport(code: TransportErrorCode, attempts: u32, message: &str) -> HttpOutcome {
    HttpOutcome::TransportError {
        code,
        attempts,
        message: bounded(message),
    }
}

fn bounded(message: &str) -> String {
    let mut output = String::new();
    for character in message.chars() {
        if output.len() + character.len_utf8() > MAX_DIAGNOSTIC_BYTES {
            output.push_str("…");
            break;
        }
        output.push(character);
    }
    output
}

fn code_message(code: TransportErrorCode) -> &'static str {
    match code {
        TransportErrorCode::Cancelled => "request cancelled",
        TransportErrorCode::Timeout => "request timed out",
        TransportErrorCode::Dns => "DNS resolution failed",
        TransportErrorCode::Connect => "connection establishment failed",
        TransportErrorCode::Tls => "TLS negotiation failed",
        TransportErrorCode::Write => "request write failed",
        TransportErrorCode::Read => "response read failed",
        TransportErrorCode::BodyTooLarge => "response body exceeded route limit",
        TransportErrorCode::InvalidResponse => "response was invalid",
    }
}

/// A host-side request validation or client-configuration error. These remain
/// capability errors rather than becoming provider-visible HTTP outcomes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientError {
    /// No route was configured.
    NoRoutes,
    /// Two host routes used one capability-visible name.
    DuplicateRoute(String),
    /// The extension selected no configured route.
    UnknownRoute(String),
    /// The host route rejected this method/path pair.
    Route(RouteError),
    /// JSON could not be serialized safely.
    InvalidJson(String),
    /// Request JSON exceeds the host-defined transport limit.
    RequestTooLarge { maximum: usize, actual: usize },
    /// The batch size is outside the generic hard bound.
    InvalidBatchCount { maximum: usize, actual: usize },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRoutes => formatter.write_str("HTTP client requires at least one route"),
            Self::DuplicateRoute(route) => write!(formatter, "duplicate HTTP route {route:?}"),
            Self::UnknownRoute(route) => write!(formatter, "unknown HTTP route {route:?}"),
            Self::Route(error) => error.fmt(formatter),
            Self::InvalidJson(error) => write!(formatter, "invalid JSON transport value: {error}"),
            Self::RequestTooLarge { maximum, actual } => write!(formatter, "request body exceeds {maximum} byte route limit ({actual} bytes)"),
            Self::InvalidBatchCount { maximum, actual } => write!(formatter, "request_many accepts 1..={maximum} requests, received {actual}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl AttemptError {
    fn from_h12(error: h12tiny_client::Error) -> Self {
        let code = match error.kind() {
            ErrorKind::DnsTimeout => TransportErrorCode::Dns,
            ErrorKind::ConnectTimeout | ErrorKind::Connect => TransportErrorCode::Connect,
            ErrorKind::TlsTimeout | ErrorKind::Tls | ErrorKind::Alpn | ErrorKind::Handshake => TransportErrorCode::Tls,
            ErrorKind::SendRequest => TransportErrorCode::Write,
            ErrorKind::Canceled => TransportErrorCode::Read,
            ErrorKind::HeadersTimeout => TransportErrorCode::Timeout,
            ErrorKind::UnsupportedScheme
            | ErrorKind::UnsupportedMethod
            | ErrorKind::UnsupportedVersion
            | ErrorKind::AbsoluteUriRequired
            | ErrorKind::ProtocolUnavailable => TransportErrorCode::InvalidResponse,
            _ => TransportErrorCode::Read,
        };
        Self::new(code, &error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::route::{Origin, RatePolicy};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn route() -> Route {
        Route::new(
            "fixture",
            Origin::new("http", "127.0.0.1:8080").expect("origin is valid"),
            Duration::from_secs(1),
            128,
            128,
            RetryPolicy::new(0, Duration::ZERO, Duration::ZERO),
            RatePolicy::new(NonZeroUsize::new(1).expect("nonzero"), None),
        )
        .expect("route is valid")
        .allow(HttpMethod::Post, "/ok")
        .expect("path is valid")
    }

    #[test]
    fn route_authority_rejects_an_unknown_route_and_unlisted_path_before_io() {
        let client = Client::new(background_executor(|future| drop(future)), [route()])
            .expect("client configures");
        let cancellation = CancellationToken::new();
        let unknown = smol_block(client.request(
            HttpRequest {
                route: "other".into(),
                method: HttpMethod::Post,
                path: "/ok".into(),
                json: JsonValue::Null,
            },
            cancellation.clone(),
        ));
        assert!(matches!(unknown, Err(ClientError::UnknownRoute(_))));
        let forbidden = smol_block(client.request(
            HttpRequest {
                route: "fixture".into(),
                method: HttpMethod::Post,
                path: "/other".into(),
                json: JsonValue::Null,
            },
            cancellation,
        ));
        assert!(matches!(forbidden, Err(ClientError::Route(RouteError::ForbiddenRequest { .. }))));
    }

    #[test]
    fn retry_policy_is_bounded_and_capped() {
        let policy = RetryPolicy::new(3, Duration::from_millis(250), Duration::from_secs(1));
        assert_eq!(policy.delay_before_retry(0), Duration::from_millis(250));
        assert_eq!(policy.delay_before_retry(1), Duration::from_millis(500));
        assert_eq!(policy.delay_before_retry(2), Duration::from_secs(1));
        assert_eq!(policy.delay_before_retry(8), Duration::from_secs(1));
        assert_eq!(retry_after("3600", Duration::from_secs(2)), Some(Duration::from_secs(2)));
    }

    fn smol_block<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn client_for(origin: Origin, response_bytes: usize, retries: RetryPolicy) -> Client {
        let route = Route::new(
            "fixture",
            origin,
            Duration::from_secs(2),
            256,
            response_bytes,
            retries,
            RatePolicy::new(NonZeroUsize::new(4).expect("nonzero"), None),
        )
        .expect("route is valid")
        .allow(HttpMethod::Post, "/ok")
        .expect("path is valid");
        Client::new(
            background_executor(|future| {
                smol::spawn(future).detach();
            }),
            [route],
        )
        .expect("client configures")
    }

    fn request() -> HttpRequest {
        HttpRequest {
            route: "fixture".into(),
            method: HttpMethod::Post,
            path: "/ok".into(),
            json: JsonValue::object([("request", JsonValue::String("value".into()))]),
        }
    }

    fn read_request(stream: &mut TcpStream) {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let count = stream.read(&mut buffer).expect("fixture request reads");
            assert_ne!(count, 0, "client must not close before a request");
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = std::str::from_utf8(&bytes[..header_end]).expect("headers are ASCII");
        assert!(headers.starts_with("POST /ok HTTP/1.1\r\n"));
        assert!(headers.to_ascii_lowercase().contains("content-type: application/json"));
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .expect("request has a fixed JSON content length");
        while bytes.len() < header_end + content_length {
            let count = stream.read(&mut buffer).expect("fixture request body reads");
            assert_ne!(count, 0, "client must send the complete JSON body");
            bytes.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(
            std::str::from_utf8(&bytes[header_end..header_end + content_length])
                .expect("request JSON is UTF-8"),
            r#"{"request":"value"}"#,
        );
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
        let reason = match status {
            200 => "OK",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Fixture",
        };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len(),
        )
        .expect("fixture response writes");
        stream.flush().expect("fixture response flushes");
    }

    #[test]
    fn pooled_client_reuses_one_keep_alive_connection_after_complete_body_consumption() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture binds");
        let address = listener.local_addr().expect("fixture has address");
        let connections = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&connections);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("fixture accepts one connection");
            observed.fetch_add(1, Ordering::SeqCst);
            for _ in 0..2 {
                read_request(&mut stream);
                write_response(&mut stream, 200, r#"{"ok":true}"#);
            }
        });
        let client = client_for(
            Origin::new("http", address.to_string()).expect("origin is valid"),
            1024,
            RetryPolicy::new(0, Duration::ZERO, Duration::ZERO),
        );
        let first = smol::block_on(client.request(request(), CancellationToken::new()))
            .expect("first request is authorized");
        let second = smol::block_on(client.request(request(), CancellationToken::new()))
            .expect("second request is authorized");
        assert!(matches!(first, HttpOutcome::Response { status: 200, .. }));
        assert!(matches!(second, HttpOutcome::Response { status: 200, .. }));
        server.join().expect("fixture server settles");
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retryable_status_retries_once_and_preserves_attempt_count() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture binds");
        let address = listener.local_addr().expect("fixture has address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("fixture accepts connection");
            read_request(&mut stream);
            write_response(&mut stream, 503, r#"{"error":"temporary"}"#);
            read_request(&mut stream);
            write_response(&mut stream, 200, r#"{"ok":true}"#);
        });
        let client = client_for(
            Origin::new("http", address.to_string()).expect("origin is valid"),
            1024,
            RetryPolicy::new(1, Duration::ZERO, Duration::ZERO),
        );
        let outcome = smol::block_on(client.request(request(), CancellationToken::new()))
            .expect("request is authorized");
        assert!(matches!(
            outcome,
            HttpOutcome::Response {
                status: 200,
                attempts: 2,
                ..
            }
        ));
        server.join().expect("fixture server settles");
    }

    #[test]
    fn request_many_overlaps_local_requests_and_preserves_input_order() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fixture binds");
        listener.set_nonblocking(true).expect("fixture listener is nonblocking");
        let address = listener.local_addr().expect("fixture has address");
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let active_server = Arc::clone(&active);
        let maximum_server = Arc::clone(&maximum);
        let server = thread::spawn(move || {
            let mut workers = Vec::new();
            while workers.len() < 3 {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // On Darwin, accepted sockets inherit the listener's
                        // nonblocking mode. Each worker reads synchronously.
                        stream
                            .set_nonblocking(false)
                            .expect("fixture worker stream is blocking");
                        let active = Arc::clone(&active_server);
                        let maximum = Arc::clone(&maximum_server);
                        workers.push(thread::spawn(move || {
                            read_request(&mut stream);
                            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                            maximum.fetch_max(current, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(75));
                            active.fetch_sub(1, Ordering::SeqCst);
                            write_response(&mut stream, 200, r#"{"ok":true}"#);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("fixture accept fails: {error}"),
                }
            }
            for worker in workers {
                worker.join().expect("fixture worker settles");
            }
        });
        let client = client_for(
            Origin::new("http", address.to_string()).expect("origin is valid"),
            1024,
            RetryPolicy::new(0, Duration::ZERO, Duration::ZERO),
        );
        let outcomes = smol::block_on(client.request_many(
            vec![request(), request(), request()],
            CancellationToken::new(),
        ))
        .expect("batch is authorized");
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes.iter().all(|outcome| matches!(outcome, HttpOutcome::Response { status: 200, .. })));
        server.join().expect("fixture server settles");
        assert!(maximum.load(Ordering::SeqCst) >= 2, "batch requests overlapped");
    }
}
