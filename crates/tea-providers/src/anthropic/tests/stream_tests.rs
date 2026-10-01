//! Stream cases through the real transport, ported from upstream Pi
//! (`anthropic-sse-parsing.test.ts`, `provider-retry.test.ts`,
//! `anthropic-cache-write-1h-cost.test.ts`, and error-path behavior).

use super::fixture::{FixtureServer, Piece, Scripted, gate, sse};
use super::*;
use crate::scheduler::ModelStreamEvent;
use crate::state::{StopReason, Usage};
use std::time::Duration;

fn json(value: &str) -> String {
    value.to_owned()
}

fn message_start(model: &str, input: u64) -> (&'static str, String) {
    (
        "message_start",
        json(&format!(
            r#"{{"type":"message_start","message":{{"id":"msg_test","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
        )),
    )
}

fn block_start(index: u64, block: &str) -> (&'static str, String) {
    (
        "content_block_start",
        format!(r#"{{"type":"content_block_start","index":{index},"content_block":{block}}}"#),
    )
}

fn block_delta(index: u64, delta: &str) -> (&'static str, String) {
    (
        "content_block_delta",
        format!(r#"{{"type":"content_block_delta","index":{index},"delta":{delta}}}"#),
    )
}

fn block_stop(index: u64) -> (&'static str, String) {
    (
        "content_block_stop",
        format!(r#"{{"type":"content_block_stop","index":{index}}}"#),
    )
}

fn message_delta(stop: &str, usage: &str) -> (&'static str, String) {
    (
        "message_delta",
        format!(r#"{{"type":"message_delta","delta":{{"stop_reason":"{stop}"}},"usage":{usage}}}"#),
    )
}

fn message_stop() -> (&'static str, String) {
    ("message_stop", r#"{"type":"message_stop"}"#.into())
}

fn minimal_events() -> Vec<(&'static str, String)> {
    vec![
        message_start("claude-haiku-4-5", 12),
        block_start(0, r#"{"type":"text","text":""}"#),
        block_delta(0, r#"{"type":"text_delta","text":"Hello"}"#),
        block_stop(0),
        message_delta(
            "end_turn",
            r#"{"input_tokens":12,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}"#,
        ),
        message_stop(),
    ]
}

fn provider_for(server: &FixtureServer, model: &str) -> AnthropicProvider {
    provider_with(server, model, |config| config)
}

fn provider_with(
    server: &FixtureServer,
    model: &str,
    edit: impl FnOnce(AnthropicConfig) -> AnthropicConfig,
) -> AnthropicProvider {
    let config = AnthropicConfig::try_new("sk-ant-test-secret", model)
        .expect("config")
        .with_test_origin(server.origin.clone())
        .with_retry_policy(crate::RetryPolicy::new(
            2,
            Duration::from_millis(1),
            Duration::from_millis(2),
        ))
        .with_timeouts(Duration::from_secs(10), Duration::from_secs(10))
        .expect("timeouts");
    AnthropicProvider::new(edit(config))
}

fn collect(provider: &AnthropicProvider, request: ModelRequest) -> Vec<ModelStreamEvent> {
    let cancellation = CancellationToken::new();
    let mut stream =
        smol::block_on(provider.stream(request, cancellation.clone())).expect("stream starts");
    let mut events = Vec::new();
    while let Some(event) =
        smol::block_on(stream.next_event(cancellation.clone())).expect("event")
    {
        events.push(event);
    }
    events
}

fn without_observation(events: Vec<ModelStreamEvent>) -> Vec<ModelStreamEvent> {
    events
        .into_iter()
        .filter(|event| !matches!(event, ModelStreamEvent::RequestObservation(_)))
        .collect()
}

fn hello_request(model: &str) -> ModelRequest {
    request(model, vec![user(1, "Hello")], ThinkingLevel::Off)
}

#[test]
fn a_fragmented_text_response_streams_through_the_real_transport() {
    // Every byte arrives in its own HTTP chunk.
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&minimal_events()), 1)]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = collect(&provider, hello_request("claude-haiku-4-5"));
    assert!(matches!(
        &events[0],
        ModelStreamEvent::RequestObservation(observation)
            if observation.serialized_request_bytes.is_some()
    ));
    assert_eq!(
        without_observation(events),
        vec![
            ModelStreamEvent::TextDelta("Hello".into()),
            ModelStreamEvent::Usage(Usage {
                total_tokens: Some(17),
                input_tokens: Some(12),
                output_tokens: Some(5),
                reasoning_tokens: None,
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
                cost: None,
            }),
            ModelStreamEvent::End(StopReason::Stop),
        ]
    );
    let requests = server.join();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.target, "/v1/messages?beta=true");
    assert_eq!(sent.header("x-api-key"), Some("sk-ant-test-secret"));
    assert_eq!(sent.header("anthropic-version"), Some("2023-06-01"));
    assert!(sent.header("user-agent").is_some_and(|agent| agent.starts_with("tea/")));
    assert!(sent.header("authorization").is_none());
    let body = sent.json();
    assert_eq!(body.get("stream"), Some(&JsonValue::Bool(true)));
    assert_eq!(body.get("model").and_then(JsonValue::as_str), Some("claude-haiku-4-5"));
}

// "preserves content from content_block_start events" plus tool use and
// signatures, delivered in seven-byte fragments.
#[test]
fn block_start_content_signatures_and_tool_input_are_preserved() {
    let events = vec![
        message_start("claude-haiku-4-5", 12),
        block_start(0, r#"{"type":"thinking","thinking":"Initial thinking","signature":"initial signature"}"#),
        block_delta(0, r#"{"type":"thinking_delta","thinking":" plus delta"}"#),
        block_delta(0, r#"{"type":"signature_delta","signature":" plus delta"}"#),
        block_stop(0),
        block_start(1, r#"{"type":"text","text":"Initial text"}"#),
        block_delta(1, r#"{"type":"text_delta","text":" plus delta"}"#),
        block_stop(1),
        block_start(2, r#"{"type":"tool_use","id":"toolu_1","name":"read","input":{}}"#),
        block_delta(2, r#"{"type":"input_json_delta","partial_json":"{\"path\":"}"#),
        block_delta(2, r#"{"type":"input_json_delta","partial_json":"\"README.md\"}"}"#),
        block_stop(2),
        message_delta("tool_use", r#"{"output_tokens":9}"#),
        message_stop(),
    ];
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&events), 7)]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = without_observation(collect(&provider, hello_request("claude-haiku-4-5")));
    assert_eq!(events[0], ModelStreamEvent::ThinkingDelta("Initial thinking".into()));
    assert_eq!(events[1], ModelStreamEvent::ThinkingDelta(" plus delta".into()));
    assert!(matches!(
        &events[2],
        ModelStreamEvent::ThinkingSignature(item)
            if item.payload() == "initial signature plus delta" && item.provider() == "anthropic"
    ));
    assert_eq!(events[3], ModelStreamEvent::TextDelta("Initial text".into()));
    assert_eq!(events[4], ModelStreamEvent::TextDelta(" plus delta".into()));
    assert!(matches!(
        &events[5],
        ModelStreamEvent::ToolCall(call)
            if call.id.as_str() == "toolu_1" && call.name == "read"
                && call.arguments.as_str() == r#"{"path":"README.md"}"#
    ));
    assert!(matches!(&events[6], ModelStreamEvent::Usage(_)));
    assert_eq!(events[7], ModelStreamEvent::End(StopReason::ToolUse));
}

// "repairs malformed SSE JSON and malformed streamed tool JSON"
#[test]
fn malformed_tool_json_is_repaired() {
    let malformed = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#;
    let events = vec![
        message_start("claude-haiku-4-5", 12),
        block_start(0, r#"{"type":"tool_use","id":"toolu_test","name":"edit","input":{}}"#),
        ("content_block_delta", malformed.to_owned()),
        block_stop(0),
        message_delta("tool_use", r#"{"output_tokens":5}"#),
        message_stop(),
    ];
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&events), 64)]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = without_observation(collect(&provider, hello_request("claude-haiku-4-5")));
    let ModelStreamEvent::ToolCall(call) = &events[0] else {
        panic!("tool call, got {events:?}");
    };
    let arguments = JsonValue::parse(call.arguments.as_str()).expect("repaired JSON");
    assert_eq!(arguments.get("path").and_then(JsonValue::as_str), Some("A\\H"));
    assert_eq!(arguments.get("text").and_then(JsonValue::as_str), Some("col1\tcol2"));
    assert_eq!(events.last(), Some(&ModelStreamEvent::End(StopReason::ToolUse)));
}

// "preserves refusal stop details" and "preserves sensitive stop reasons"
#[test]
fn refusal_and_sensitive_stops_end_with_descriptive_errors() {
    let explanation = "This request triggered restrictions and was blocked.";
    let refusal = vec![
        message_start("claude-fable-5-1", 412),
        (
            "message_delta",
            format!(
                r#"{{"type":"message_delta","delta":{{"stop_reason":"refusal","stop_details":{{"type":"refusal","category":"cyber","explanation":"{explanation}"}}}},"usage":{{"output_tokens":0}}}}"#
            ),
        ),
        message_stop(),
    ];
    let sensitive = vec![
        message_start("claude-haiku-4-5", 12),
        message_delta("sensitive", r#"{"output_tokens":0}"#),
        message_stop(),
    ];
    let server = FixtureServer::start(vec![
        Scripted::sse(&sse(&refusal), 32),
        Scripted::sse(&sse(&sensitive), 32),
    ]);
    let fable = provider_for(&server, "claude-fable-5-1");
    let events = collect(&fable, hello_request("claude-fable-5-1"));
    assert_eq!(
        events.last(),
        Some(&ModelStreamEvent::Error {
            message: explanation.into()
        })
    );
    let haiku = provider_for(&server, "claude-haiku-4-5");
    let events = collect(&haiku, hello_request("claude-haiku-4-5"));
    assert_eq!(
        events.last(),
        Some(&ModelStreamEvent::Error {
            message: "Provider stopped with: sensitive".into()
        })
    );
}

// "treats message_delta without usage as a no-op" and "ignores unknown SSE
// events after message_stop"
#[test]
fn usage_less_deltas_and_trailing_unknown_events_are_tolerated() {
    let mut events = minimal_events();
    events[4] = (
        "message_delta",
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#.into(),
    );
    events.push(("done", "[DONE]".into()));
    events.push(("proxy.stats", "not json".into()));
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&events), 13)]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = without_observation(collect(&provider, hello_request("claude-haiku-4-5")));
    assert_eq!(events[0], ModelStreamEvent::TextDelta("Hello".into()));
    assert!(matches!(
        &events[1],
        ModelStreamEvent::Usage(Usage { input_tokens: Some(12), total_tokens: Some(12), .. })
    ));
    assert_eq!(events[2], ModelStreamEvent::End(StopReason::Stop));
}

// "fails safely when Anthropic falls back after output begins"
#[test]
fn a_model_fallback_after_output_fails_safely() {
    let events = vec![
        message_start("claude-opus-5-5", 1),
        block_start(0, r#"{"type":"text","text":"partial"}"#),
        block_stop(0),
        block_start(1, r#"{"type":"fallback","from":{"model":"claude-opus-5-5"},"to":{"model":"claude-opus-4-8"}}"#),
    ];
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&events), 32)]);
    let provider = provider_for(&server, "claude-opus-5-5");
    let events = collect(&provider, hello_request("claude-opus-5-5"));
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Error { message }) if message.contains("unsupported mid-output model fallback")
    ));
}

// "keeps signed thinking replayable when a proxy relabels the model" and "uses
// the serving model input transformations from the final stream event"
#[test]
fn a_relabeled_response_model_and_input_transformations_are_recorded() {
    let events = vec![
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"msg_x","model":"kimi-for-coding","usage":{"input_tokens":100,"output_tokens":0},"input_transformations":[{"type":"thinking_dropped","path":"messages.1.content.0","reason":"prefix_binding_mismatch"}]}}"#.into(),
        ),
        block_start(0, r#"{"type":"thinking","thinking":"reasoning","signature":"signature"}"#),
        block_stop(0),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20},"input_transformations":[{"type":"thinking_dropped","path":"messages.3.content.0","reason":"model_binding_mismatch"}]}"#.into(),
        ),
        message_stop(),
    ];
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&events), 32)]);
    let provider = provider_for(&server, "claude-opus-5-5");
    let events = without_observation(collect(&provider, hello_request("claude-opus-5-5")));
    assert!(matches!(
        &events[1],
        ModelStreamEvent::ThinkingSignature(item) if item.payload() == "signature"
    ));
    let turns = provider.turns();
    assert_eq!(turns[0].response_model.as_deref(), Some("kimi-for-coding"));
    let transformations = provider.last_input_transformations();
    assert_eq!(transformations.len(), 1);
    assert_eq!(
        transformations[0].get("path").and_then(JsonValue::as_str),
        Some("messages.3.content.0")
    );
    // A managed-effort model records the effort its response used.
    assert!(events.iter().any(|event| matches!(
        event,
        ModelStreamEvent::OpaqueProviderContext(item) if item.kind() == "effort" && item.payload() == "low"
    )));
}

#[test]
fn incomplete_streams_end_with_explicit_errors() {
    let unterminated = vec![
        message_start("claude-haiku-4-5", 1),
        block_start(0, r#"{"type":"text","text":"partial"}"#),
    ];
    let no_reason = vec![message_start("claude-haiku-4-5", 1), message_stop()];
    let server = FixtureServer::start(vec![
        Scripted::sse(&sse(&unterminated), 32),
        Scripted::sse(&sse(&no_reason), 32),
    ]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let first = collect(&provider, hello_request("claude-haiku-4-5"));
    assert!(matches!(
        first.last(),
        Some(ModelStreamEvent::Error { message }) if message == "Anthropic stream ended before message_stop"
    ));
    let second = collect(&provider, hello_request("claude-haiku-4-5"));
    assert!(matches!(
        second.last(),
        Some(ModelStreamEvent::Error { message }) if message == "Anthropic stream ended without a stop reason"
    ));
}

#[test]
fn a_mid_stream_error_event_ends_the_response_without_a_retry() {
    let events = vec![
        message_start("claude-haiku-4-5", 1),
        block_start(0, r#"{"type":"text","text":"partial"}"#),
        (
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#.into(),
        ),
    ];
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&events), 32)]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = without_observation(collect(&provider, hello_request("claude-haiku-4-5")));
    assert_eq!(events[0], ModelStreamEvent::TextDelta("partial".into()));
    assert!(matches!(
        &events[1],
        ModelStreamEvent::ProviderError(record)
            if record.error_type.as_deref() == Some("overloaded_error")
                && record.visible_stream_event == Some(true)
    ));
    assert_eq!(
        events[2],
        ModelStreamEvent::Error {
            message: "Anthropic stream error (overloaded_error): Overloaded".into()
        }
    );
    assert_eq!(server.join().len(), 1);
}

// provider-retry.test.ts: retryable statuses retry before any output.
#[test]
fn retryable_statuses_retry_before_output_and_do_not_duplicate_events() {
    let server = FixtureServer::start(vec![
        Scripted::error(
            529,
            &[("retry-after-ms", "1")],
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ),
        Scripted::dropped(),
        Scripted::sse(&sse(&minimal_events()), 5),
    ]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = without_observation(collect(&provider, hello_request("claude-haiku-4-5")));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelStreamEvent::TextDelta(_)))
            .count(),
        1
    );
    assert_eq!(events.last(), Some(&ModelStreamEvent::End(StopReason::Stop)));
    assert_eq!(server.join().len(), 3);
}

// cache-warmer.ts sends refreshes with maxRetries: 0.
#[test]
fn cache_maintenance_requests_are_never_retried() {
    let server = FixtureServer::start(vec![Scripted::error(
        529,
        &[("retry-after-ms", "1")],
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    )]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let mut request = hello_request("claude-haiku-4-5");
    request.purpose = crate::scheduler::RequestPurpose::CacheMaintenance;
    request.max_output_tokens = Some(1);
    let events = without_observation(collect(&provider, request));
    assert!(matches!(events.last(), Some(ModelStreamEvent::Error { .. })));
    let requests = server.join();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].json().get("max_tokens").and_then(tea_protocol::JsonValue::as_u64), Some(1));
}

// provider-retry.test.ts: a server-requested delay above the maximum fails.
#[test]
fn an_excessive_server_retry_delay_fails_immediately() {
    let server = FixtureServer::start(vec![Scripted::error(
        429,
        &[("retry-after", "120")],
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
    )]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = collect(&provider, hello_request("claude-haiku-4-5"));
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Error { message })
            if message.starts_with("Server requested 120s retry delay (max: 60s)")
                && message.contains("rate_limit_error")
    ));
    assert_eq!(server.join().len(), 1);
}

// provider-retry.test.ts: x-should-retry overrides the status classification.
#[test]
fn x_should_retry_overrides_status_classification() {
    let server = FixtureServer::start(vec![
        Scripted::error(500, &[("x-should-retry", "false")], r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#),
        Scripted::error(400, &[("x-should-retry", "true")], r#"{"type":"error","error":{"type":"invalid_request_error","message":"transient"}}"#),
        Scripted::sse(&sse(&minimal_events()), 64),
    ]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let first = collect(&provider, hello_request("claude-haiku-4-5"));
    assert!(matches!(first.last(), Some(ModelStreamEvent::Error { .. })));
    let second = collect(&provider, hello_request("claude-haiku-4-5"));
    assert_eq!(second.last(), Some(&ModelStreamEvent::End(StopReason::Stop)));
    assert_eq!(server.join().len(), 3);
}

#[test]
fn client_errors_are_typed_redacted_and_not_retried() {
    let server = FixtureServer::start(vec![Scripted::error(
        400,
        &[("request-id", "req_123")],
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad key sk-ant-test-secret"}}"#,
    )]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = collect(&provider, hello_request("claude-haiku-4-5"));
    let Some(ModelStreamEvent::ProviderError(record)) = events.iter().rev().nth(1) else {
        panic!("provider error, got {events:?}");
    };
    assert_eq!(record.status_code, Some(400));
    assert_eq!(record.error_type.as_deref(), Some("invalid_request_error"));
    assert_eq!(record.logical_request_id.as_deref(), Some("req_123"));
    assert_eq!(record.retryable, Some(false));
    assert!(!record.response_body.as_deref().unwrap_or_default().contains("sk-ant-test-secret"));
    assert!(matches!(
        events.last(),
        Some(ModelStreamEvent::Error { message })
            if message.starts_with("Anthropic API error 400 (invalid_request_error)")
    ));
    assert_eq!(server.join().len(), 1);
}

// overflow.ts: Anthropic's prompt-too-long and request_too_large errors.
#[test]
fn context_overflow_is_typed() {
    let server = FixtureServer::start(vec![
        Scripted::error(400, &[], r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213462 tokens > 200000 maximum"}}"#),
        Scripted::error(413, &[], r#"{"type":"error","error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}"#),
    ]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    for _ in 0..2 {
        let events = collect(&provider, hello_request("claude-haiku-4-5"));
        assert!(matches!(events.last(), Some(ModelStreamEvent::ContextOverflow { .. })), "{events:?}");
    }
}

#[test]
fn cancellation_while_the_body_is_held_open_settles_as_cancelled() {
    let (release, wait) = gate();
    let mut response = Scripted::sse(&sse(&minimal_events()[..3]), 1024);
    response.pieces.push(wait);
    response.pieces.push(Piece::Bytes(sse(&minimal_events()[3..])));
    let server = FixtureServer::start(vec![response]);
    let provider = provider_for(&server, "claude-haiku-4-5");
    let cancellation = CancellationToken::new();
    let mut stream = smol::block_on(provider.stream(hello_request("claude-haiku-4-5"), cancellation.clone()))
        .expect("stream starts");
    let mut seen = Vec::new();
    loop {
        let event = smol::block_on(stream.next_event(cancellation.clone()))
            .expect("event")
            .expect("open stream");
        let text = matches!(event, ModelStreamEvent::TextDelta(_));
        seen.push(event);
        if text {
            break;
        }
    }
    cancellation.cancel();
    assert_eq!(
        smol::block_on(stream.next_event(cancellation.clone())).expect("event"),
        Some(ModelStreamEvent::End(StopReason::Cancelled))
    );
    let _ = release.send(());
    drop(stream);
    let _ = server.join();
}

#[test]
fn usage_maps_to_full_prompt_input_with_cache_subsets_and_estimated_cost() {
    let events = vec![
        (
            "message_start",
            r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":100,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":1000000,"cache_creation":{"ephemeral_5m_input_tokens":600000,"ephemeral_1h_input_tokens":400000}}}}"#.into(),
        ),
        block_start(0, r#"{"type":"text","text":"Hi"}"#),
        block_stop(0),
        message_delta(
            "end_turn",
            r#"{"input_tokens":100,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":1000000,"output_tokens_details":{"thinking_tokens":2}}"#,
        ),
        message_stop(),
    ];
    let server = FixtureServer::start(vec![Scripted::sse(&sse(&events), 64)]);
    let provider = provider_with(&server, "claude-haiku-4-5", |config| {
        config.with_pricing(Some(crate::scheduler::ModelPricing {
            input: "5".into(),
            output: "25".into(),
            cache_read: "0.5".into(),
            cache_write: "6.25".into(),
        }))
    });
    let events = without_observation(collect(&provider, hello_request("claude-haiku-4-5")));
    assert_eq!(
        events[1],
        ModelStreamEvent::Usage(Usage {
            total_tokens: Some(1_000_105),
            input_tokens: Some(1_000_100),
            output_tokens: Some(5),
            reasoning_tokens: Some(2),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(1_000_000),
            cost: None,
        })
    );
    let turn = &provider.turns()[0];
    assert_eq!(turn.usage.cache_write_1h, 400_000);
    // 100 * 5 + 5 * 25 + 600k * 6.25 + 400k * 10, per million.
    assert_eq!(turn.estimated_cost.as_deref(), Some("7.750625"));
}

#[test]
fn a_mismatched_model_is_rejected_before_transport() {
    let server = FixtureServer::start(Vec::new());
    let provider = provider_for(&server, "claude-haiku-4-5");
    let events = collect(&provider, hello_request("claude-opus-5-5"));
    assert!(matches!(events.last(), Some(ModelStreamEvent::Error { .. })));
    assert!(server.requests().is_empty());
}
