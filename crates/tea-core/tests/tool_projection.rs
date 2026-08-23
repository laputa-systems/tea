use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tea_core::Agent;
use tea_core::error::CoreError;
use tea_core::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use tea_core::state::{AgentMessage, AgentToolCall, SerializedJson, StopReason, ToolCallId};
use tea_core::tool::{
    AgentTool, AgentToolResult, FailureSignature, ToolCall, ToolContext, ToolFailure, ToolFuture,
    ToolResultProjectionPolicy, ToolUpdateSink, project_tool_result_as_text, truncate_middle,
};

#[test]
fn projection_marks_error_details_truncation_and_repeated_payloads_deterministically() {
    let policy = ToolResultProjectionPolicy {
        max_content_bytes: 24,
        max_details_bytes: 24,
        max_total_bytes: 512,
        deduplicate_repeated_errors: true,
    };
    let details = SerializedJson::new("{\"detail\":\"abcdefghijklmnopqrstuvwxyz\"}");
    let failure = ToolFailure::fatal(FailureSignature::new("process:dead").unwrap())
        .with_recovery_guidance("Choose a different available capability.");
    let mut seen = BTreeMap::new();
    let first = project_tool_result_as_text(
        "prefix-abcdefghijklmnopqrstuvwxyz-suffix",
        Some(&details),
        true,
        Some(&failure),
        &policy,
        &mut seen,
    );
    assert!(first.content.contains("[tool error status: fatal]"));
    assert!(first.content.contains("[tool details (serialized JSON):"));
    assert!(first.content.contains("… [truncated] …"));
    let second = project_tool_result_as_text(
        "prefix-abcdefghijklmnopqrstuvwxyz-suffix",
        Some(&details),
        true,
        Some(&failure),
        &policy,
        &mut seen,
    );
    assert_eq!(
        second.content,
        "[repeated tool error omitted; see the earlier matching result]"
    );
    let truncated = truncate_middle("prefix-abcdefghijklmnopqrstuvwxyz-suffix", 24);
    assert!(truncated.starts_with("pr"));
    assert!(truncated.ends_with("ix"));
    assert!(truncated.contains("… [truncated] …"));
}

#[test]
fn default_projection_preserves_standard_large_tool_results() {
    let content = "x".repeat(50 * 1024);
    let mut seen = BTreeMap::new();
    let projected = project_tool_result_as_text(
        &content,
        None,
        false,
        None,
        &ToolResultProjectionPolicy::default(),
        &mut seen,
    );

    assert_eq!(projected.content, content);
    assert!(!projected.content.contains("[truncated]"));
}

#[test]
fn default_projection_preserves_long_diagnostic_details() {
    let details = SerializedJson::new(format!("{{\"trace\":\"{}\"}}", "x".repeat(32 * 1024)));
    let mut seen = BTreeMap::new();
    let projected = project_tool_result_as_text(
        "",
        Some(&details),
        false,
        None,
        &ToolResultProjectionPolicy::default(),
        &mut seen,
    );

    assert!(projected.content.contains(&details.as_str()[..32 * 1024]));
    assert!(!projected.content.contains("[truncated]"));
}

struct CaptureProvider {
    streams: Mutex<Vec<ModelStream>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ModelProvider for CaptureProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        self.requests.lock().unwrap().push(request);
        let stream = self.streams.lock().unwrap().remove(0);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

struct DetailedTool;

impl AgentTool for DetailedTool {
    fn name(&self) -> &str {
        "detailed"
    }

    fn description(&self) -> &str {
        "returns raw details"
    }

    fn schema(&self) -> &tea_protocol::JsonValue {
        static SCHEMA: std::sync::OnceLock<tea_protocol::JsonValue> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| tea_protocol::JsonValue::parse(r#"{"type":"object"}"#).unwrap())
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        _context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(std::future::ready(Ok(AgentToolResult {
            tool_call_id: call.id,
            content: "prefix-abcdefghijklmnopqrstuvwxyz-suffix".into(),
            details: Some(SerializedJson::new(r#"{"raw":"unbounded host detail"}"#)),
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
            is_error: true,
            failure: Some(
                ToolFailure::retryable(FailureSignature::new("transport:temporary").unwrap())
                    .with_recovery_guidance("Retry after host readiness returns."),
            ),
        })))
    }
}

#[test]
fn canonical_tool_data_stays_raw_while_next_model_context_is_curated() {
    smol::block_on(async {
        let provider = Arc::new(CaptureProvider {
            streams: Mutex::new(vec![
                ModelStream {
                    events: vec![
                        ModelStreamEvent::ToolCall(AgentToolCall {
                            id: ToolCallId::new("detail-call").unwrap(),
                            name: "detailed".into(),
                            arguments: SerializedJson::new("{}"),
                        }),
                        ModelStreamEvent::End(StopReason::ToolUse),
                    ],
                },
                ModelStream {
                    events: vec![ModelStreamEvent::End(StopReason::Stop)],
                },
            ]),
            requests: Mutex::new(Vec::new()),
        });
        let policy = ToolResultProjectionPolicy {
            max_content_bytes: 24,
            max_details_bytes: 24,
            max_total_bytes: 128,
            deduplicate_repeated_errors: true,
        };
        let agent = Agent::builder()
            .model_provider(provider.clone())
            .tool(Arc::new(DetailedTool))
            .tool_result_projection(policy)?
            .build();
        agent.start_prompt("start")?.drive().await?;

        let snapshot = agent.snapshot();
        assert!(matches!(
            &snapshot.messages[2],
            AgentMessage::ToolResult {
                content,
                details: Some(details),
                failure: Some(_),
                ..
            } if content == "prefix-abcdefghijklmnopqrstuvwxyz-suffix"
                && details.as_str() == r#"{"raw":"unbounded host detail"}"#
        ));
        let requests = provider.requests.lock().unwrap();
        assert!(
            requests[1]
                .context
                .contains("[tool error status: retryable]")
        );
        assert!(requests[1].context.contains("… [truncated] …"));
        assert!(!requests[1].context.contains("unbounded host detail"));
        Ok::<(), CoreError>(())
    })
    .expect("projection does not mutate canonical state");
}
