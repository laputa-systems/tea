//! Generic pooled byte-stream transport for provider adapters.
//!
//! Unlike the route-scoped JSON capability, this layer accepts an explicit
//! absolute URI. It owns direct-origin HTTP, TLS, pooling, cancellation, and
//! deadline enforcement; callers retain only their provider wire format and
//! response parsing.

use crate::client::TransportErrorCode;
use bytes::{Buf, Bytes};
use futures_util::future::{self, Either};
use h12tiny_client::{Client as H12Client, ErrorKind, RequestOptions};
use h12tiny_core::runtime::BoxExecutor;
use http::{HeaderValue, Method, Request, Uri, header::HeaderName};
use http_body::{Body, Frame, SizeHint};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};
use tea_core::scheduler::CancellationToken;

const MAX_EXPOSED_RESPONSE_HEADERS: usize = 64;
const MAX_EXPOSED_RESPONSE_HEADER_BYTES: usize = 8_192;

fn debug_headers(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(name, value)| {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization"
                    | "cookie"
                    | "proxy-authorization"
                    | "chatgpt-account-id"
                    | "set-cookie"
            ) {
                (name.as_str(), "[redacted]")
            } else {
                (name.as_str(), value.as_str())
            }
        })
        .collect()
}

const QUERY_ENCODED: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'$')
    .add(b'%')
    .add(b'&')
    .add(b'\'')
    .add(b'+')
    .add(b',')
    .add(b'/')
    .add(b':')
    .add(b';')
    .add(b'<')
    .add(b'=')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// A generic direct-origin byte request. Providers use this to express their
/// own HTTP wire format without constructing a transport client themselves.
#[derive(Clone)]
pub struct TransportRequest {
    method: Method,
    url: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
    stall_timeout: Option<Duration>,
}

impl std::fmt::Debug for TransportRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers = debug_headers(&self.headers);
        formatter
            .debug_struct("TransportRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("query", &self.query)
            .field("headers", &headers)
            .field("body_bytes", &self.body.len())
            .field("timeout", &self.timeout)
            .field("stall_timeout", &self.stall_timeout)
            .finish()
    }
}

impl TransportRequest {
    /// Build a GET request with an explicit total deadline.
    pub fn get(url: impl Into<String>, timeout: Duration) -> Self {
        Self::new(Method::GET, url, Vec::new(), timeout)
    }

    /// Build a POST request with an explicit total deadline.
    pub fn post(url: impl Into<String>, body: impl Into<Vec<u8>>, timeout: Duration) -> Self {
        Self::new(Method::POST, url, body.into(), timeout)
    }

    fn new(method: Method, url: impl Into<String>, body: Vec<u8>, timeout: Duration) -> Self {
        Self {
            method,
            url: url.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body,
            timeout,
            stall_timeout: None,
        }
    }

    /// Append one percent-encoded query pair.
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Add one request header. Header syntax is validated before I/O starts.
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    /// Limit how long the transport may wait for any one response phase.
    /// The total deadline still remains in force.
    pub fn with_stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall_timeout = Some(timeout);
        self
    }
}

/// A complete generic byte response. HTTP statuses remain responses so the
/// caller's adapter can apply its own protocol-specific classification.
pub struct TransportResponse {
    /// The received HTTP status.
    pub status_code: u16,
    /// Bounded textual response headers for adapter-level protocol handling.
    ///
    /// Header values that are non-textual or exceed the transport bound are
    /// omitted. Callers must treat this as a protocol convenience rather than
    /// a complete raw-header representation.
    pub headers: Vec<(String, String)>,
    /// The complete response body.
    pub body: Vec<u8>,
}

impl std::fmt::Debug for TransportResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportResponse")
            .field("status_code", &self.status_code)
            .field("headers", &debug_headers(&self.headers))
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// A transport failure that preserves status and partial bytes when available.
pub struct TransportError {
    /// Stable error class independent of h12tiny internals.
    pub code: TransportErrorCode,
    /// Concise transport diagnostic.
    pub message: String,
    /// Status received before a later body failure, when any.
    pub status_code: Option<u16>,
    /// Partial bytes received before the failure, when any.
    pub body: Vec<u8>,
    stalled: bool,
}

impl std::fmt::Debug for TransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("status_code", &self.status_code)
            .field("body_bytes", &self.body.len())
            .field("stalled", &self.stalled)
            .finish()
    }
}

impl TransportError {
    /// Whether this failure is a response-header or response-body idle timeout.
    pub const fn is_stall(&self) -> bool {
        self.stalled
    }

    fn new(
        code: TransportErrorCode,
        message: impl Into<String>,
        status_code: Option<u16>,
        stalled: bool,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            status_code,
            body: Vec::new(),
            stalled,
        }
    }

    fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }
}

/// Incremental items emitted by [`TransportStream`].
pub enum TransportStreamEvent {
    /// Response headers arrived.
    Response {
        /// Received HTTP status.
        status_code: u16,
        /// Bounded textual response headers.
        headers: Vec<(String, String)>,
    },
    /// A non-empty response body chunk.
    Chunk(Vec<u8>),
    /// The body reached EOF.
    End,
    /// The request could not be opened or read further.
    Failure(TransportError),
}

impl std::fmt::Debug for TransportStreamEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response {
                status_code,
                headers,
            } => formatter
                .debug_struct("TransportStreamEvent::Response")
                .field("status_code", status_code)
                .field("headers", &debug_headers(headers))
                .finish(),
            Self::Chunk(bytes) => formatter
                .debug_tuple("TransportStreamEvent::Chunk")
                .field(&format_args!("{} bytes", bytes.len()))
                .finish(),
            Self::End => formatter.write_str("TransportStreamEvent::End"),
            Self::Failure(error) => formatter
                .debug_tuple("TransportStreamEvent::Failure")
                .field(error)
                .finish(),
        }
    }
}

#[derive(Debug, Default)]
struct StreamState {
    events: VecDeque<TransportStreamEvent>,
    waker: Option<Waker>,
}

#[derive(Debug, Default)]
struct StreamShared {
    state: Mutex<StreamState>,
    arrived: Condvar,
}

/// Caller-polled stream backed by the shared asynchronous Tea HTTP transport.
#[derive(Debug)]
pub struct TransportStream {
    shared: Arc<StreamShared>,
}

impl TransportStream {
    /// Poll one event without blocking the caller's executor.
    pub fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<TransportStreamEvent> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("HTTP stream state mutex poisoned");
        if let Some(event) = state.events.pop_front() {
            return Poll::Ready(event);
        }
        match &mut state.waker {
            Some(waker) if waker.will_wake(context.waker()) => {}
            slot => *slot = Some(context.waker().clone()),
        }
        Poll::Pending
    }

    /// Wait for one event for a legacy finite-response adapter. The HTTP I/O
    /// still runs in the caller-provided asynchronous executor; this method
    /// only waits on the event bridge and checks cancellation promptly.
    pub fn next_blocking(&mut self, cancellation: &CancellationToken) -> TransportStreamEvent {
        loop {
            if cancellation.is_cancelled() {
                return TransportStreamEvent::Failure(TransportError::new(
                    TransportErrorCode::Cancelled,
                    "HTTP request cancelled",
                    None,
                    false,
                ));
            }
            let state = self
                .shared
                .state
                .lock()
                .expect("HTTP stream state mutex poisoned");
            let (mut state, _) = self
                .shared
                .arrived
                .wait_timeout(state, Duration::from_millis(10))
                .expect("HTTP stream state mutex poisoned");
            if let Some(event) = state.events.pop_front() {
                return event;
            }
        }
    }
}

/// One reusable direct-origin client for generic provider byte traffic.
///
/// Construction requires a caller-owned executor. The transport never creates
/// or drives an async runtime; it uses that executor both for h12tiny's pooled
/// connection drivers and for stream pumps.
#[derive(Clone)]
pub struct TransportClient {
    inner: Arc<H12Client<RequestBody>>,
    executor: BoxExecutor,
}

impl TransportClient {
    /// Create a pooled generic transport.
    pub fn new(executor: BoxExecutor) -> Self {
        let mut builder = H12Client::builder(executor.clone());
        builder.pool_max_connections_per_host(4);
        Self {
            inner: Arc::new(builder.build()),
            executor,
        }
    }

    /// Start a response body stream immediately.
    pub fn stream(
        &self,
        request: TransportRequest,
        cancellation: CancellationToken,
    ) -> TransportStream {
        let shared = Arc::new(StreamShared::default());
        let client = self.clone();
        let worker_shared = Arc::clone(&shared);
        self.executor.execute(async move {
            client
                .stream_worker(request, cancellation, worker_shared)
                .await;
        });
        TransportStream { shared }
    }

    /// Collect one finite response through the same streaming transport.
    pub fn send_blocking(
        &self,
        request: TransportRequest,
        cancellation: &CancellationToken,
    ) -> Result<TransportResponse, TransportError> {
        let mut stream = self.stream(request, cancellation.clone());
        let mut status_code = None;
        let mut headers = Vec::new();
        let mut body = Vec::new();
        loop {
            match stream.next_blocking(cancellation) {
                TransportStreamEvent::Response {
                    status_code: status,
                    headers: response_headers,
                } => {
                    status_code = Some(status);
                    headers = response_headers;
                }
                TransportStreamEvent::Chunk(chunk) => body.extend_from_slice(&chunk),
                TransportStreamEvent::End => {
                    return Ok(TransportResponse {
                        status_code: status_code.expect("HTTP stream ended after response headers"),
                        headers,
                        body,
                    });
                }
                TransportStreamEvent::Failure(error) => return Err(error.with_body(body)),
            }
        }
    }

    async fn stream_worker(
        &self,
        request: TransportRequest,
        cancellation: CancellationToken,
        shared: Arc<StreamShared>,
    ) {
        if cancellation.is_cancelled() {
            push(
                &shared,
                TransportStreamEvent::Failure(TransportError::new(
                    TransportErrorCode::Cancelled,
                    "HTTP request cancelled",
                    None,
                    false,
                )),
            );
            return;
        }
        let deadline = Instant::now()
            .checked_add(request.timeout)
            .unwrap_or_else(Instant::now);
        let response = match self.open(request.clone(), &cancellation, deadline).await {
            Ok(response) => response,
            Err(error) => {
                push(&shared, TransportStreamEvent::Failure(error));
                return;
            }
        };
        let (status_code, headers, mut body) = response;
        push(
            &shared,
            TransportStreamEvent::Response {
                status_code,
                headers,
            },
        );
        loop {
            let now = Instant::now();
            let overall = match deadline
                .checked_duration_since(now)
                .filter(|duration| !duration.is_zero())
            {
                Some(remaining) => remaining,
                None => {
                    push(
                        &shared,
                        TransportStreamEvent::Failure(TransportError::new(
                            TransportErrorCode::Timeout,
                            "HTTP response body receive timed out",
                            Some(status_code),
                            false,
                        )),
                    );
                    return;
                }
            };
            let step = request
                .stall_timeout
                .map_or(overall, |stall| stall.min(overall));
            let stalled = request.stall_timeout.is_some_and(|stall| stall <= overall);
            let frame = match run_until(
                future::poll_fn(|context| Pin::new(&mut body).poll_frame(context)),
                &cancellation,
                Instant::now()
                    .checked_add(step)
                    .unwrap_or_else(Instant::now),
                stalled,
                Some(status_code),
            )
            .await
            {
                Ok(frame) => frame,
                Err(error) => {
                    push(&shared, TransportStreamEvent::Failure(error));
                    return;
                }
            };
            let Some(frame) = frame else {
                push(&shared, TransportStreamEvent::End);
                return;
            };
            match frame {
                Ok(frame) => {
                    if let Ok(mut data) = frame.into_data() {
                        let mut chunk = Vec::with_capacity(data.remaining());
                        while data.has_remaining() {
                            let bytes = data.chunk();
                            chunk.extend_from_slice(bytes);
                            let length = bytes.len();
                            data.advance(length);
                        }
                        if !chunk.is_empty() {
                            push(&shared, TransportStreamEvent::Chunk(chunk));
                        }
                    }
                }
                Err(_) => {
                    push(
                        &shared,
                        TransportStreamEvent::Failure(TransportError::new(
                            TransportErrorCode::Read,
                            "HTTP response body read failed",
                            Some(status_code),
                            false,
                        )),
                    );
                    return;
                }
            }
        }
    }

    async fn open(
        &self,
        request: TransportRequest,
        cancellation: &CancellationToken,
        deadline: Instant,
    ) -> Result<(u16, Vec<(String, String)>, impl Body<Data = Bytes> + Unpin), TransportError> {
        let request = build_request(request)?;
        let timeout = remaining(deadline).ok_or_else(|| {
            TransportError::new(
                TransportErrorCode::Timeout,
                "HTTP request timed out",
                None,
                false,
            )
        })?;
        let options = RequestOptions::new()
            .with_dns_timeout(timeout)
            .with_connect_timeout(timeout)
            .with_tls_timeout(timeout)
            .with_headers_timeout(timeout);
        let response = run_until(
            self.inner.request_with_options(request, options),
            cancellation,
            deadline,
            false,
            None,
        )
        .await?
        .map_err(TransportError::from_h12)?;
        let (parts, body) = response.into_parts();
        Ok((
            parts.status.as_u16(),
            bounded_response_headers(&parts.headers),
            body,
        ))
    }
}

fn bounded_response_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            if name.as_str().len() + value.len() > MAX_EXPOSED_RESPONSE_HEADER_BYTES {
                return None;
            }
            Some((name.as_str().to_owned(), value.to_owned()))
        })
        .take(MAX_EXPOSED_RESPONSE_HEADERS)
        .collect()
}

fn push(shared: &Arc<StreamShared>, event: TransportStreamEvent) {
    let waker = {
        let mut state = shared
            .state
            .lock()
            .expect("HTTP stream state mutex poisoned");
        state.events.push_back(event);
        state.waker.take()
    };
    shared.arrived.notify_all();
    if let Some(waker) = waker {
        waker.wake();
    }
}

fn build_request(request: TransportRequest) -> Result<Request<RequestBody>, TransportError> {
    let uri = append_query(request.url, &request.query)
        .parse::<Uri>()
        .map_err(|error| {
            TransportError::new(
                TransportErrorCode::InvalidResponse,
                format!("invalid HTTP request URI: {error}"),
                None,
                false,
            )
        })?;
    let mut builder = Request::builder().method(request.method).uri(uri);
    for (key, value) in request.headers {
        let name = HeaderName::try_from(key).map_err(|error| {
            TransportError::new(
                TransportErrorCode::Write,
                format!("invalid HTTP request header name: {error}"),
                None,
                false,
            )
        })?;
        let value = HeaderValue::try_from(value).map_err(|error| {
            TransportError::new(
                TransportErrorCode::Write,
                format!("invalid HTTP request header value: {error}"),
                None,
                false,
            )
        })?;
        builder = builder.header(name, value);
    }
    builder
        .body(RequestBody::new(request.body))
        .map_err(|error| {
            TransportError::new(
                TransportErrorCode::Write,
                format!("cannot build HTTP request: {error}"),
                None,
                false,
            )
        })
}

fn append_query(mut url: String, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return url;
    }
    url.push(if url.contains('?') { '&' } else { '?' });
    for (index, (key, value)) in query.iter().enumerate() {
        if index != 0 {
            url.push('&');
        }
        url.push_str(&utf8_percent_encode(key, QUERY_ENCODED).to_string());
        url.push('=');
        url.push_str(&utf8_percent_encode(value, QUERY_ENCODED).to_string());
    }
    url
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

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
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

async fn run_until<T>(
    operation: impl Future<Output = T>,
    cancellation: &CancellationToken,
    deadline: Instant,
    stalled: bool,
    status_code: Option<u16>,
) -> Result<T, TransportError> {
    let timed = future::select(Box::pin(operation), Box::pin(async_io::Timer::at(deadline)));
    match future::select(Box::pin(timed), Box::pin(cancellation.cancelled())).await {
        Either::Left((Either::Left((value, _)), _)) => Ok(value),
        Either::Left((Either::Right(_), _)) => Err(TransportError::new(
            TransportErrorCode::Timeout,
            if stalled {
                "HTTP response receive stalled"
            } else {
                "HTTP request timed out"
            },
            status_code,
            stalled,
        )),
        Either::Right(_) => Err(TransportError::new(
            TransportErrorCode::Cancelled,
            "HTTP request cancelled",
            status_code,
            false,
        )),
    }
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
}

impl TransportError {
    fn from_h12(error: h12tiny_client::Error) -> Self {
        let code = match error.kind() {
            ErrorKind::DnsTimeout => TransportErrorCode::Dns,
            ErrorKind::ConnectTimeout | ErrorKind::Connect => TransportErrorCode::Connect,
            ErrorKind::TlsTimeout | ErrorKind::Tls | ErrorKind::Alpn | ErrorKind::Handshake => {
                TransportErrorCode::Tls
            }
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
        Self::new(code, error.to_string(), None, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_executor;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    fn client() -> TransportClient {
        TransportClient::new(background_executor(|future| {
            smol::spawn(future).detach();
        }))
    }

    #[test]
    fn request_debug_redacts_secret_headers_and_body() {
        let request = TransportRequest::post(
            "https://example.invalid",
            b"refresh_token=private-token".to_vec(),
            Duration::from_secs(1),
        )
        .header("Authorization", "Bearer private-token")
        .header("ChatGPT-Account-ID", "acct_private")
        .header("originator", "tea");
        let debug = format!("{request:?}");
        assert!(!debug.contains("private-token"));
        assert!(!debug.contains("refresh_token"));
        assert!(!debug.contains("acct_private"));
        assert!(debug.contains("[redacted]"));
        assert!(debug.contains("originator"));
    }

    #[test]
    fn response_and_transport_debug_do_not_expose_body_or_cookie_values() {
        let response = TransportResponse {
            status_code: 200,
            headers: vec![("Set-Cookie".into(), "session=private-cookie".into())],
            body: b"access_token=private-token".to_vec(),
        };
        let failure =
            TransportError::new(TransportErrorCode::Read, "read failed", Some(502), false)
                .with_body(b"refresh_token=private-token".to_vec());
        let stream = TransportStreamEvent::Chunk(b"id_token=private-token".to_vec());
        let debug = format!("{response:?} {failure:?} {stream:?}");
        for secret in [
            "private-cookie",
            "private-token",
            "access_token",
            "refresh_token",
            "id_token",
        ] {
            assert!(!debug.contains(secret), "transport debug leaked {secret}");
        }
        assert!(debug.contains("body_bytes"));
    }

    #[test]
    fn streams_before_settlement() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let (first_chunk_sent, first_chunk_received) = mpsc::channel();
        let (finish_response, wait_for_finish) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("client should connect");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).expect("request should read");
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nfirst ",
                )
                .expect("response prefix should write");
            first_chunk_sent.send(()).expect("receiver remains open");
            wait_for_finish.recv().expect("test releases response");
            socket
                .write_all(b"second")
                .expect("response suffix should write");
        });

        let cancellation = CancellationToken::new();
        let mut response = client().stream(
            TransportRequest::get(format!("http://{address}/stream"), Duration::from_secs(2)),
            cancellation.clone(),
        );
        first_chunk_received
            .recv_timeout(Duration::from_secs(1))
            .expect("server sends the first response chunk");
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            TransportStreamEvent::Response {
                status_code: 200,
                ..
            }
        ));
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            TransportStreamEvent::Chunk(bytes) if bytes == b"first "
        ));
        finish_response.send(()).expect("server remains ready");
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            TransportStreamEvent::Chunk(bytes) if bytes == b"second"
        ));
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            TransportStreamEvent::End
        ));
        server.join().expect("mock server should finish");
    }

    fn read_headers(socket: &mut std::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).expect("request should read");
            assert_ne!(read, 0, "client should not close before its request");
            request.extend_from_slice(&buffer[..read]);
        }
        request
    }

    #[test]
    fn finite_responses_reuse_one_keep_alive_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let connections = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&connections);
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("client should connect");
            observed.fetch_add(1, Ordering::SeqCst);
            for path in ["/first", "/second"] {
                let request = read_headers(&mut socket);
                assert!(
                    request.starts_with(format!("GET {path} HTTP/1.1\r\n").as_bytes()),
                    "request should use the expected path"
                );
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                    )
                    .expect("response should write");
                socket.flush().expect("response should flush");
            }
        });

        let transport = client();
        for path in ["first", "second"] {
            let response = transport
                .send_blocking(
                    TransportRequest::get(
                        format!("http://{address}/{path}"),
                        Duration::from_secs(2),
                    ),
                    &CancellationToken::new(),
                )
                .expect("finite response should settle");
            assert_eq!(response.status_code, 200);
            assert_eq!(response.body, b"ok");
        }
        server.join().expect("mock server should finish");
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn finite_responses_preserve_status_body_and_stall_partial_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("client should connect");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).expect("request should read");
            socket
                .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad")
                .expect("response should write");
            socket
                .shutdown(Shutdown::Write)
                .expect("response should close");
        });
        let cancellation = CancellationToken::new();
        let response = client()
            .send_blocking(
                TransportRequest::get(format!("http://{address}/status"), Duration::from_secs(2)),
                &cancellation,
            )
            .expect("HTTP statuses remain provider responses");
        assert_eq!(response.status_code, 429);
        assert_eq!(response.body, b"bad");
        server.join().expect("mock server should finish");

        let listener = TcpListener::bind("127.0.0.1:0").expect("stall server should bind");
        let address = listener.local_addr().expect("stall server address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("client should connect");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).expect("request should read");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
                .expect("response prefix should write");
            std::thread::sleep(Duration::from_millis(200));
        });
        let error = client()
            .send_blocking(
                TransportRequest::get(format!("http://{address}/stall"), Duration::from_secs(2))
                    .with_stall_timeout(Duration::from_millis(50)),
                &CancellationToken::new(),
            )
            .expect_err("stalled body should fail");
        assert!(error.is_stall());
        assert_eq!(error.status_code, Some(200));
        assert_eq!(error.body, b"abc");
        server.join().expect("stall server should finish");
    }
}
