//! Small, synchronous HTTP/1.1 boundary shared by the opt-in provider adapters.
//!
//! Provider features use the local `h12tiny-client-sync` direct-origin transport. Its blocking
//! Rustls HTTP/1.1 reader runs on the provider's narrowly scoped worker thread, so the core
//! executor remains independent of HTTP, DNS, and TLS runtime details. Providers choose whether
//! to collect the body with [`send`] or expose it incrementally with [`stream`]; provider modules
//! retain ownership of status/error classification and response parsing.

#![allow(dead_code)] // provider features consume different request methods and response fields

use super::super::scheduler::CancellationToken;
use h12tiny_client_sync::{Client, ResponseBody};
use http::{header::HeaderName, HeaderValue, Method as HttpMethod, Request as HttpRequest, Uri};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

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

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Method {
    Get,
    Post,
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) method: Method,
    pub(crate) url: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    pub(crate) timeout: Duration,
    pub(crate) stall_timeout: Option<Duration>,
}

impl Request {
    pub(crate) fn get(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout,
            stall_timeout: None,
        }
    }

    pub(crate) fn post(
        url: impl Into<String>,
        body: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: body.into(),
            timeout,
            stall_timeout: None,
        }
    }

    pub(crate) fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    pub(crate) fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((key.into(), value.into()));
        self
    }

    pub(crate) fn with_stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall_timeout = Some(timeout);
        self
    }
}

#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status_code: u16,
    pub(crate) body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Timeout {
    RecvResponse,
    RecvBody,
}

#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) message: String,
    pub(crate) status_code: Option<u16>,
    pub(crate) body: Vec<u8>,
    timeout: Option<Timeout>,
}

/// Incremental items delivered by the provider-owned HTTP body worker.
#[derive(Debug)]
pub(crate) enum StreamEvent {
    /// Response headers arrived and establish the status for following body chunks.
    Response { status_code: u16 },
    /// A non-empty body chunk became available before the response settled.
    Chunk(Vec<u8>),
    /// The body reached EOF without a transport failure.
    End,
    /// The response could not be opened or read further.
    Failure(Failure),
}

#[derive(Debug, Default)]
struct StreamState {
    events: VecDeque<StreamEvent>,
    waker: Option<Waker>,
}

/// Caller-polled response body backed by one provider-owned HTTP worker.
#[derive(Debug)]
pub(crate) struct HttpStream {
    state: Arc<Mutex<StreamState>>,
}

impl HttpStream {
    /// Poll the next response item.
    pub(crate) fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<StreamEvent> {
        let mut state = self.state.lock().expect("HTTP stream state mutex poisoned");
        if let Some(event) = state.events.pop_front() {
            return Poll::Ready(event);
        }
        match &mut state.waker {
            Some(waker) if waker.will_wake(context.waker()) => {}
            slot => *slot = Some(context.waker().clone()),
        }
        Poll::Pending
    }
}

/// Begin a response-body stream without waiting for its first body chunk.
pub(crate) fn stream(request: Request, cancellation: &CancellationToken) -> HttpStream {
    let state = Arc::new(Mutex::new(StreamState::default()));
    let worker_state = Arc::clone(&state);
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::Builder::new()
        .name("tea-http-stream".into())
        .spawn(move || stream_worker(request, worker_cancellation, worker_state));
    if let Err(error) = worker {
        push_stream_event(
            &state,
            StreamEvent::Failure(Failure {
                message: format!("cannot start HTTP streaming worker: {error}"),
                status_code: None,
                body: Vec::new(),
                timeout: None,
            }),
        );
    }
    HttpStream { state }
}

fn push_stream_event(state: &Arc<Mutex<StreamState>>, event: StreamEvent) {
    let waker = {
        let mut state = state.lock().expect("HTTP stream state mutex poisoned");
        state.events.push_back(event);
        state.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// Build a direct-origin h12tiny client with the smallest useful feature set: HTTP/1.1 only.
///
/// The sync client supplies explicit Graviola-backed public roots and offers only `http/1.1`
/// through ALPN. It owns no idle pool or background driver.
fn client() -> Client {
    Client::builder().connect_timeout(CONNECT_TIMEOUT).build()
}

fn build_request(request: Request) -> Result<HttpRequest<Vec<u8>>, Failure> {
    let url = append_query(request.url, &request.query);
    let uri = url.parse::<Uri>().map_err(|error| Failure {
        message: format!("invalid HTTP request URI: {error}"),
        status_code: None,
        body: Vec::new(),
        timeout: None,
    })?;
    let method = match request.method {
        Method::Get => HttpMethod::GET,
        Method::Post => HttpMethod::POST,
    };
    let mut builder = HttpRequest::builder().method(method).uri(uri);
    for (key, value) in request.headers {
        let name = HeaderName::try_from(key).map_err(|error| Failure {
            message: format!("invalid HTTP request header name: {error}"),
            status_code: None,
            body: Vec::new(),
            timeout: None,
        })?;
        let value = HeaderValue::try_from(value).map_err(|error| Failure {
            message: format!("invalid HTTP request header value: {error}"),
            status_code: None,
            body: Vec::new(),
            timeout: None,
        })?;
        builder = builder.header(name, value);
    }
    builder.body(request.body).map_err(|error| Failure {
        message: format!("cannot build HTTP request: {error}"),
        status_code: None,
        body: Vec::new(),
        timeout: None,
    })
}

fn append_query(mut url: String, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return url;
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    url.push(separator);
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

struct OpenResponse {
    status_code: u16,
    body: ResponseBody,
    deadline: Instant,
    stall_timeout: Option<Duration>,
}

fn deadline(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now)
}

fn remaining(deadline: Instant, stall_timeout: Option<Duration>) -> Duration {
    let total = deadline.saturating_duration_since(Instant::now());
    stall_timeout.map_or(total, |stall| total.min(stall))
}

fn open(request: Request) -> Result<OpenResponse, Failure> {
    let timeout = request.timeout;
    let stall_timeout = request.stall_timeout;
    let deadline = deadline(timeout);
    let http_request = build_request(request)?;
    let response_timeout = stall_timeout.map_or(timeout, |stall| timeout.min(stall));
    let response = match client().request_with_timeout(http_request, Some(response_timeout)) {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            return Err(Failure {
                message: "HTTP request timed out while receiving response headers".into(),
                status_code: None,
                body: Vec::new(),
                timeout: Some(Timeout::RecvResponse),
            })
        }
        Err(error) => {
            return Err(Failure {
                message: format!("HTTP request failed: {error}"),
                status_code: None,
                body: Vec::new(),
                timeout: None,
            })
        }
    };
    Ok(OpenResponse {
        status_code: response.status().as_u16(),
        body: response.into_body(),
        deadline,
        stall_timeout,
    })
}

#[derive(Debug)]
enum BodyReadFailure {
    Timeout,
    Transport(String),
}

fn next_body_chunk(
    body: &mut ResponseBody,
    timeout: Duration,
) -> Result<Option<Vec<u8>>, BodyReadFailure> {
    if body.is_complete() {
        return Ok(None);
    }
    if timeout.is_zero() {
        return Err(BodyReadFailure::Timeout);
    }
    body.set_read_timeout(Some(timeout))
        .map_err(|error| BodyReadFailure::Transport(error.to_string()))?;
    let mut chunk = vec![0_u8; 8 * 1024];
    match body.read(&mut chunk) {
        Ok(0) => Ok(None),
        Ok(read) => {
            chunk.truncate(read);
            Ok(Some(chunk))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            Err(BodyReadFailure::Timeout)
        }
        Err(error) => Err(BodyReadFailure::Transport(error.to_string())),
    }
}

fn stream_worker(
    request: Request,
    cancellation: CancellationToken,
    state: Arc<Mutex<StreamState>>,
) {
    if cancellation.is_cancelled() {
        push_stream_event(
            &state,
            StreamEvent::Failure(cancelled_failure(None, Vec::new())),
        );
        return;
    }
    let response = match open(request) {
        Ok(response) => response,
        Err(failure) => {
            push_stream_event(&state, StreamEvent::Failure(failure));
            return;
        }
    };
    let status_code = response.status_code;
    push_stream_event(&state, StreamEvent::Response { status_code });
    let mut body = response.body;
    loop {
        if cancellation.is_cancelled() {
            push_stream_event(
                &state,
                StreamEvent::Failure(cancelled_failure(Some(status_code), Vec::new())),
            );
            return;
        }
        match next_body_chunk(
            &mut body,
            remaining(response.deadline, response.stall_timeout),
        ) {
            Ok(Some(chunk)) => push_stream_event(&state, StreamEvent::Chunk(chunk)),
            Ok(None) => {
                push_stream_event(&state, StreamEvent::End);
                return;
            }
            Err(BodyReadFailure::Timeout) => {
                push_stream_event(
                    &state,
                    StreamEvent::Failure(Failure {
                        message: "HTTP response body receive timed out".into(),
                        status_code: Some(status_code),
                        body: Vec::new(),
                        timeout: Some(Timeout::RecvBody),
                    }),
                );
                return;
            }
            Err(BodyReadFailure::Transport(error)) => {
                push_stream_event(
                    &state,
                    StreamEvent::Failure(Failure {
                        message: format!("HTTP response body read failed: {error}"),
                        status_code: Some(status_code),
                        body: Vec::new(),
                        timeout: None,
                    }),
                );
                return;
            }
        }
    }
}

fn cancelled_failure(status_code: Option<u16>, body: Vec<u8>) -> Failure {
    Failure {
        message: "HTTP request cancelled".into(),
        status_code,
        body,
        timeout: None,
    }
}

impl Failure {
    pub(crate) fn is_stall(&self) -> bool {
        matches!(
            self.timeout,
            Some(Timeout::RecvResponse | Timeout::RecvBody)
        )
    }
}

/// Execute one finite request with explicit timeout and cancellation checkpoints.
pub(crate) fn send(
    request: Request,
    cancellation: &CancellationToken,
) -> Result<Response, Failure> {
    if cancellation.is_cancelled() {
        return Err(cancelled_failure(None, Vec::new()));
    }

    let response = open(request)?;
    let status_code = response.status_code;
    let mut body = Vec::new();
    let mut incoming = response.body;
    loop {
        if cancellation.is_cancelled() {
            return Err(cancelled_failure(Some(status_code), body));
        }
        match next_body_chunk(
            &mut incoming,
            remaining(response.deadline, response.stall_timeout),
        ) {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(BodyReadFailure::Timeout) => {
                return Err(Failure {
                    message: "HTTP response body receive timed out".into(),
                    status_code: Some(status_code),
                    body,
                    timeout: Some(Timeout::RecvBody),
                })
            }
            Err(BodyReadFailure::Transport(error)) => {
                return Err(Failure {
                    message: format!("HTTP response body read failed: {error}"),
                    status_code: Some(status_code),
                    body,
                    timeout: None,
                })
            }
        }
    }
    if cancellation.is_cancelled() {
        return Err(cancelled_failure(Some(status_code), body));
    }
    Ok(Response { status_code, body })
}

#[cfg(test)]
mod tests {
    use super::{send, stream, Request, StreamEvent};
    use crate::scheduler::CancellationToken;
    use std::io::Write;
    use std::net::{Shutdown, TcpListener};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn streaming_transport_yields_a_body_chunk_before_the_response_settles() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let (first_chunk_sent, first_chunk_received) = mpsc::channel();
        let (finish_response, wait_for_finish) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("native client should connect");
            let mut request = [0_u8; 1024];
            let _ = std::io::Read::read(&mut socket, &mut request);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nfirst ",
                )
                .expect("mock response prefix should write");
            first_chunk_sent
                .send(())
                .expect("test receiver remains open");
            wait_for_finish
                .recv()
                .expect("test releases response settlement");
            socket
                .write_all(b"second")
                .expect("mock response suffix should write");
        });

        let cancellation = CancellationToken::new();
        let mut response = stream(
            Request::get(format!("http://{address}/stream"), Duration::from_secs(2)),
            &cancellation,
        );
        first_chunk_received
            .recv_timeout(Duration::from_secs(1))
            .expect("server sends the first response chunk");
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            StreamEvent::Response { status_code: 200 }
        ));
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            StreamEvent::Chunk(bytes) if bytes == b"first "
        ));

        finish_response
            .send(())
            .expect("server remains ready to finish");
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            StreamEvent::Chunk(bytes) if bytes == b"second"
        ));
        assert!(matches!(
            smol::block_on(std::future::poll_fn(|context| response.poll_next(context))),
            StreamEvent::End
        ));
        server.join().expect("mock server should finish");
    }

    #[test]
    fn preserves_non_success_status_and_response_body_for_provider_parsers() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("native client should connect");
            let mut request = [0_u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut request);
            stream
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad",
                )
                .expect("mock response should write");
            stream
                .shutdown(Shutdown::Write)
                .expect("mock response should close its write side");
        });

        let response = send(
            Request::get(format!("http://{address}/status"), Duration::from_secs(2)),
            &CancellationToken::new(),
        )
        .expect("HTTP status errors remain responses for provider parsers");
        assert_eq!(response.status_code, 429);
        assert_eq!(response.body, b"bad");
        server.join().expect("mock HTTP server should finish");
    }

    #[test]
    fn returns_partial_body_when_the_configured_receive_timeout_fires() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("native client should connect");
            let mut request = [0_u8; 1024];
            let _ = std::io::Read::read(&mut stream, &mut request);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
                .expect("mock response prefix should write");
            std::thread::sleep(Duration::from_millis(200));
        });

        let failure = send(
            Request::get(format!("http://{address}/stall"), Duration::from_secs(2))
                .with_stall_timeout(Duration::from_millis(50)),
            &CancellationToken::new(),
        )
        .expect_err("a stalled response should be classified as a transport failure");
        assert!(failure.is_stall());
        assert_eq!(failure.status_code, Some(200));
        assert_eq!(failure.body, b"abc");
        server.join().expect("mock HTTP server should finish");
    }
}
