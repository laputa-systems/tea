//! Local OpenAI-compatible Chat Completions provider.
//!
//! This adapter is intentionally transport-specific but server-agnostic: the caller supplies a
//! base URL and model, and the adapter sends one streaming `chat/completions` request through the
//! shared rustls-backed HTTP boundary. It does not discover a server, read credentials, inspect the home
//! directory, or select a model from the environment. oMLX is the first supported local server;
//! its Laguna XS 2.1 endpoint is represented by [`LocalConfig::laguna_xs_2_1`].

mod config;
mod payload;
mod response;

use super::http::{HttpStream, Request, StreamEvent, stream};
pub use config::{LocalConfig, LocalConfigError};
use payload::local_payload;
use response::{LocalSseComplete, LocalSseDecoder, parse_local_response};

use crate::scheduler::{
    AdapterRequestObservation, CancellationToken, ModelEventFuture, ModelEventStream, ModelFuture,
    ModelProvider, ModelRequest, ModelStreamEvent,
};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::task::{Context, Poll};

/// The model ID exposed by the documented 5-bit Laguna checkpoint.
pub const LAGUNA_XS_2_1_MODEL: &str = "Laguna-XS-2.1-5bit";

/// Default local OpenAI-compatible API root used by oMLX.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8000/v1";

/// A streaming local OpenAI-compatible provider.
pub struct LocalProvider {
    config: LocalConfig,
}

impl fmt::Debug for LocalProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalProvider")
            .field("config", &self.config)
            .finish()
    }
}

impl LocalProvider {
    /// Construct a provider from already validated explicit configuration.
    pub fn new(config: LocalConfig) -> Self {
        Self { config }
    }
}

/// One live local SSE response.  The HTTP worker owns blocking body reads while this source
/// remains caller-polled through the provider-neutral model stream boundary.
struct LocalEventStream {
    config: LocalConfig,
    response: Option<HttpStream>,
    decoder: Option<LocalSseDecoder>,
    pending: VecDeque<ModelStreamEvent>,
    status_code: Option<u16>,
    error_body: Vec<u8>,
}

impl LocalEventStream {
    fn start(config: LocalConfig, request: ModelRequest, cancellation: CancellationToken) -> Self {
        let mut event_stream = Self {
            config,
            response: None,
            decoder: None,
            pending: VecDeque::new(),
            status_code: None,
            error_body: Vec::new(),
        };
        if cancellation.is_cancelled() {
            event_stream
                .pending
                .push_back(ModelStreamEvent::End(crate::state::StopReason::Cancelled));
            return event_stream;
        }
        if let Err(message) = event_stream.validate_model(&request) {
            event_stream
                .pending
                .push_back(ModelStreamEvent::Error { message });
            return event_stream;
        }
        let payload = match local_payload(&event_stream.config, request) {
            Ok(payload) => payload,
            Err(message) => {
                event_stream
                    .pending
                    .push_back(ModelStreamEvent::Error { message });
                return event_stream;
            }
        };
        event_stream
            .pending
            .push_back(ModelStreamEvent::RequestObservation(
                local_request_observation(&event_stream.config, payload.len()),
            ));
        let endpoint = format!(
            "{}/chat/completions",
            event_stream.config.base_url.trim_end_matches('/')
        );
        event_stream.response = Some(stream(
            Request::post(
                endpoint,
                payload.into_bytes(),
                event_stream.config.request_timeout,
            )
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream"),
            &cancellation,
        ));
        event_stream.decoder = Some(LocalSseDecoder::new());
        event_stream
    }

    fn validate_model(&self, request: &ModelRequest) -> Result<(), String> {
        let model = request
            .model
            .as_ref()
            .ok_or_else(|| "local request omitted its model descriptor".to_owned())?;
        if model.provider != "local" || model.model != self.config.model {
            return Err(format!(
                "local provider received model {}/{} but serves local/{}",
                model.provider, model.model, self.config.model
            ));
        }
        Ok(())
    }

    fn response_failure(&mut self, message: String) {
        self.response = None;
        self.decoder = None;
        self.pending.push_back(ModelStreamEvent::Error { message });
    }

    fn finish_response(&mut self) {
        self.response = None;
        if self
            .status_code
            .is_some_and(|status| !(200..300).contains(&status))
        {
            let message = parse_local_response(
                &self.error_body,
                self.status_code.expect("non-success status is present"),
            )
            .err()
            .unwrap_or_else(|| {
                format!(
                    "local server returned HTTP {} without a completion",
                    self.status_code.expect("non-success status is present")
                )
            });
            self.response_failure(message);
            return;
        }
        let Some(decoder) = self.decoder.take() else {
            self.response_failure("local response stream was not initialized".to_owned());
            return;
        };
        match decoder.finish() {
            Ok(LocalSseComplete { events }) => self.pending.extend(events),
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
                return Poll::Ready(Ok(Some(ModelStreamEvent::End(
                    crate::state::StopReason::Cancelled,
                ))));
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
                            "local response stream was not initialized".to_owned(),
                        );
                        continue;
                    };
                    match decoder.push(&bytes) {
                        Ok(events) => self.pending.extend(events),
                        Err(message) => self.response_failure(message),
                    }
                }
                Poll::Ready(StreamEvent::End) => self.finish_response(),
                Poll::Ready(StreamEvent::Failure(failure)) => {
                    if cancellation.is_cancelled() || failure.message == "HTTP request cancelled" {
                        self.response = None;
                        self.decoder = None;
                        self.pending
                            .push_back(ModelStreamEvent::End(crate::state::StopReason::Cancelled));
                    } else {
                        self.response_failure(format!(
                            "local HTTP transport failed{}: {}",
                            failure
                                .status_code
                                .map(|status| format!(" with status {status}"))
                                .unwrap_or_default(),
                            failure.message
                        ));
                    }
                }
            }
        }
    }
}

fn local_request_observation(
    config: &LocalConfig,
    serialized_request_bytes: usize,
) -> AdapterRequestObservation {
    let mut components = BTreeMap::<String, u64>::new();
    components.insert(
        "adapter".into(),
        stable_fingerprint(b"openai-compatible-chat/v1"),
    );
    components.insert(
        "output_token_limit".into(),
        stable_fingerprint(config.max_tokens.to_string().as_bytes()),
    );
    components.insert(
        "reasoning_encoding".into(),
        stable_fingerprint(config.enable_thinking.to_string().as_bytes()),
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

impl ModelEventStream for LocalEventStream {
    fn next_event<'a>(&'a mut self, cancellation: CancellationToken) -> ModelEventFuture<'a> {
        Box::pin(std::future::poll_fn(move |context| {
            self.poll_next_event(context, cancellation.clone())
        }))
    }
}

impl ModelProvider for LocalProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = LocalEventStream::start(self.config.clone(), request, cancellation);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LAGUNA_XS_2_1_MODEL, LocalConfig, LocalProvider, local_payload, parse_local_response,
    };
    use crate::scheduler::{CancellationToken, ModelProvider, ModelRequest, ModelStreamEvent};
    use crate::state::{ModelDescriptor, ThinkingLevel, Usage};
    use crate::tool::ToolDefinition;
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::sync::mpsc;
    use tea_protocol::JsonValue;

    #[test]
    fn laguna_defaults_target_o_mlx_without_ambient_configuration() {
        let config = LocalConfig::laguna_xs_2_1("http://127.0.0.1:8000/v1");
        assert_eq!(config.model(), LAGUNA_XS_2_1_MODEL);
        assert!(config.validate().is_ok());
        assert!(format!("{config:?}").contains("enable_thinking: true"));
    }

    #[test]
    fn payload_uses_o_mlx_thinking_and_openai_tool_shapes() {
        let config = LocalConfig::laguna_xs_2_1("http://127.0.0.1:8000/v1");
        let payload = local_payload(
            &config,
            crate::scheduler::ModelRequest {
                system_prompt: "system".into(),
                context: "[{\"role\":\"user\",\"content\":\"hello\"}]".into(),
                tools: vec![ToolDefinition {
                    name: "write".into(),
                    description: "write a file".into(),
                    schema: JsonValue::object([("type", JsonValue::from("object"))]),
                    execution_mode: crate::tool::ToolExecutionMode::Parallel,
                    requires_exclusive_batch: false,
                    cancellation_settlement_mode:
                        crate::tool::CancellationSettlementMode::DropFuture,
                }],
                model: Some(ModelDescriptor {
                    provider: "local".into(),
                    model: LAGUNA_XS_2_1_MODEL.into(),
                    revision: None,
                }),
                thinking_level: ThinkingLevel::High,
            },
        )
        .expect("payload should serialize");
        assert!(payload.contains("chat_template_kwargs"));
        assert!(payload.contains("enable_thinking"));
        assert!(payload.contains("\"tools\""));
        assert!(payload.contains("\"write\""));
    }

    #[cfg(unix)]
    #[test]
    fn transport_posts_the_serialized_body_to_the_local_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock server address");
        let response = br#"data: {"choices":[{"delta":{"content":"READY"},"finish_reason":null}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":1}}

data: [DONE]

"#;
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server should accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let body_start = loop {
                let read = stream.read(&mut buffer).expect("mock request should read");
                assert!(read > 0, "mock client closed before headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..body_start]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("HTTP client should send a content length");
            while request.len() < body_start + content_length {
                let read = stream.read(&mut buffer).expect("mock body should read");
                assert!(read > 0, "mock client closed before body");
                request.extend_from_slice(&buffer[..read]);
            }
            let body = String::from_utf8_lossy(&request[body_start..body_start + content_length]);
            assert!(body.contains("\"model\":\"Laguna-XS-2.1-5bit\""));
            assert!(body.contains("\"enable_thinking\":true"));
            assert!(body.contains("\"stream\":true"));
            assert!(body.contains("\"include_usage\":true"));
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream
                .write_all(header.as_bytes())
                .expect("mock headers should write");
            stream.write_all(response).expect("mock body should write");
        });
        let config = LocalConfig::laguna_xs_2_1(format!("http://{address}/v1"))
            .with_request_timeout(std::time::Duration::from_secs(5));
        let provider = LocalProvider::new(config);
        let request = ModelRequest {
            system_prompt: "system".into(),
            context: "[{\"role\":\"user\",\"content\":\"hello\"}]".into(),
            tools: Vec::new(),
            model: Some(ModelDescriptor {
                provider: "local".into(),
                model: LAGUNA_XS_2_1_MODEL.into(),
                revision: None,
            }),
            thinking_level: ThinkingLevel::High,
        };
        let cancellation = CancellationToken::new();
        let mut source = smol::block_on(provider.stream(request, cancellation.clone()))
            .expect("mock local response should start");
        server.join().expect("mock server should finish");
        assert!(matches!(
            smol::block_on(source.next_event(cancellation.clone())),
            Ok(Some(ModelStreamEvent::RequestObservation(observation)))
                if observation.serialized_request_bytes.is_some()
                    && observation.cache_domain_fingerprint.is_some()
        ));
        assert!(
            matches!(smol::block_on(source.next_event(cancellation.clone())), Ok(Some(ModelStreamEvent::TextDelta(text))) if text == "READY")
        );
        assert_eq!(
            smol::block_on(source.next_event(cancellation.clone())),
            Ok(Some(ModelStreamEvent::Usage(Usage {
                input_tokens: Some(4),
                output_tokens: Some(1),
                ..Usage::default()
            })))
        );
        assert_eq!(
            smol::block_on(source.next_event(cancellation)),
            Ok(Some(ModelStreamEvent::End(crate::state::StopReason::Stop)))
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_local_stream_yields_a_delta_before_the_response_body_settles() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock HTTP server should bind");
        let address = listener.local_addr().expect("mock HTTP server address");
        let first = br#"data: {"choices":[{"delta":{"content":"first "},"finish_reason":null}]}

"#;
        let second = br#"data: {"choices":[{"delta":{"content":"second"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":2}}

data: [DONE]

"#;
        let (first_sent, first_received) = mpsc::channel();
        let (release, wait_for_release) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("provider should connect");
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request);
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
            first_sent
                .send(())
                .expect("test should observe first record");
            wait_for_release
                .recv()
                .expect("test should release the response body");
            socket
                .write_all(second)
                .expect("terminal SSE records should write");
        });
        let provider = LocalProvider::new(
            LocalConfig::laguna_xs_2_1(format!("http://{address}/v1"))
                .with_request_timeout(std::time::Duration::from_secs(5)),
        );
        let cancellation = CancellationToken::new();
        let mut source = smol::block_on(provider.stream(
            ModelRequest {
                context: "[]".into(),
                model: Some(ModelDescriptor {
                    provider: "local".into(),
                    model: LAGUNA_XS_2_1_MODEL.into(),
                    revision: None,
                }),
                ..ModelRequest::default()
            },
            cancellation.clone(),
        ))
        .expect("local provider should start an event source");
        assert!(matches!(
            smol::block_on(source.next_event(cancellation.clone())),
            Ok(Some(ModelStreamEvent::RequestObservation(observation)))
                if observation.serialized_request_bytes.is_some()
        ));
        assert_eq!(
            smol::block_on(source.next_event(cancellation.clone())),
            Ok(Some(ModelStreamEvent::TextDelta("first ".into())))
        );
        first_received
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("mock server should send the first record");
        release
            .send(())
            .expect("mock server should receive release");
        assert_eq!(
            smol::block_on(source.next_event(cancellation.clone())),
            Ok(Some(ModelStreamEvent::TextDelta("second".into())))
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
            smol::block_on(source.next_event(cancellation)),
            Ok(Some(ModelStreamEvent::End(crate::state::StopReason::Stop)))
        );
        server.join().expect("mock server should finish");
    }

    #[test]
    fn parses_o_mlx_tool_calls_and_usage() {
        let response = br#"{
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": null,
                    "reasoning_content": "think",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "write", "arguments": "{\"path\":\"a.py\"}"}
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 3}
            }
        }"#;
        let (events, usage) = parse_local_response(response, 200).expect("response should parse");
        assert!(matches!(events[0], ModelStreamEvent::ToolCall(_)));
        assert!(matches!(events.last(), Some(ModelStreamEvent::End(_))));
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(10),
                output_tokens: Some(20),
                cache_read_tokens: Some(3),
                ..Usage::default()
            }
        );
    }
}
