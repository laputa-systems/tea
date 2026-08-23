//! Command Code NDJSON gateway provider adapter.
//!
//! The adapter accepts all authority explicitly: its API key, the gateway model, and the host
//! context the gateway includes in each request. It never discovers a working directory, date,
//! operating system, environment variable, or local Command Code credential file. Hosts convert
//! their transcript to the standard Chat Completions message array before it reaches this adapter.

mod config;
mod payload;
mod response;

use super::http::{Request, send};
use super::retry::{RetryableError, retry_with_backoff, wait_with_cancellation};
use crate::json::{JsonValue, json_value, to_bytes};
use crate::scheduler::{
    CancellationToken, ModelFuture, ModelProvider, ModelRequest, ModelStream, ModelStreamEvent,
};
use crate::state::{StopReason, Usage};
use config::project_slug_from_working_directory;
pub use config::{
    CommandCodeConfig, CommandCodeConfigError, CommandCodeErrorReport, CommandCodeErrorSource,
    CommandCodeHostContext, CommandCodePermissionMode,
};
use std::fmt;
use std::sync::Mutex;

// Command Code's installed 1.24.0 client sends this exact gateway version. Keep this wire
// value here, rather than inheriting a host-process version, so embeddings remain reproducible.
const CLIENT_VERSION: &str = "1.24.0";

use payload::{commandcode_messages, reasoning_effort};
use response::{
    ParsedCommandCodeResponse, add_usage, is_retryable_response_error, parse_ndjson_response,
};

const API_URL: &str = "https://api.commandcode.ai/alpha/generate";

/// Command Code implementation of the generic [`ModelProvider`] port.
///
/// This adapter deliberately returns a finite stream after collecting the gateway's NDJSON
/// response through the shared rustls-backed HTTP boundary. Its parser preserves the gateway event grammar and rejects a
/// missing terminal event; it does not make an executor, transport, or credential-discovery
/// mechanism a default core dependency.
pub struct CommandCodeProvider {
    config: CommandCodeConfig,
    usage: Mutex<Usage>,
    last_error: Mutex<Option<CommandCodeErrorReport>>,
}

impl CommandCodeProvider {
    /// Construct an adapter from explicit caller-owned configuration.
    pub fn new(config: CommandCodeConfig) -> Self {
        Self {
            config,
            usage: Mutex::new(Usage::default()),
            last_error: Mutex::new(None),
        }
    }

    /// Return aggregate portable token usage across settled Command Code turns.
    pub fn usage_snapshot(&self) -> Usage {
        self.usage
            .lock()
            .expect("Command Code usage mutex poisoned")
            .clone()
    }

    /// Return the most recent adapter or gateway failure observed by this provider.
    ///
    /// This is intentionally separate from the agent-facing stream error. See
    /// [`CommandCodeErrorReport`] for the privacy boundary and host logging requirements.
    pub fn last_error_report(&self) -> Option<CommandCodeErrorReport> {
        self.last_error
            .lock()
            .expect("Command Code error mutex poisoned")
            .clone()
    }

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
            Ok(mut response) => {
                if let Some(report) = response.error.take() {
                    self.record_error(report);
                }
                let usage = response.usage;
                self.record_usage(&usage);
                if usage.is_reported() {
                    let terminal = response
                        .events
                        .pop()
                        .expect("parsed Command Code response has terminal event");
                    response.events.push(ModelStreamEvent::Usage(usage));
                    response.events.push(terminal);
                }
                ModelStream {
                    events: response.events,
                }
            }
            Err(_message) if cancellation.is_cancelled() => ModelStream {
                events: vec![ModelStreamEvent::End(StopReason::Cancelled)],
            },
            Err(message) => {
                self.record_error(CommandCodeErrorReport {
                    source: CommandCodeErrorSource::Adapter,
                    message: message.clone(),
                    status_code: None,
                    error_type: None,
                    error_code: None,
                    retryable: None,
                });
                ModelStream {
                    events: vec![ModelStreamEvent::Error { message }],
                }
            }
        }
    }

    fn record_error(&self, report: CommandCodeErrorReport) {
        *self
            .last_error
            .lock()
            .expect("Command Code error mutex poisoned") = Some(report);
    }

    fn record_usage(&self, usage: &Usage) {
        let mut totals = self
            .usage
            .lock()
            .expect("Command Code usage mutex poisoned");
        add_usage(&mut totals.input_tokens, usage.input_tokens);
        add_usage(&mut totals.output_tokens, usage.output_tokens);
        add_usage(&mut totals.reasoning_tokens, usage.reasoning_tokens);
        add_usage(&mut totals.cache_read_tokens, usage.cache_read_tokens);
        add_usage(&mut totals.cache_write_tokens, usage.cache_write_tokens);
    }

    fn complete(
        &self,
        request: ModelRequest,
        cancellation: &CancellationToken,
    ) -> Result<ParsedCommandCodeResponse, String> {
        self.validate_model(&request)?;
        let payload = self.build_payload(&request)?;
        let payload =
            to_bytes(&payload).map_err(|_| "cannot serialize Command Code request".to_owned())?;
        let mut retry_index = 0;
        loop {
            let response = self.run_request(&payload, cancellation)?;
            let parsed = match parse_ndjson_response(&response, &self.config.api_key) {
                Ok(parsed) => parsed,
                Err(message)
                    if is_retryable_response_error(&message)
                        && retry_index < self.config.retry_policy.max_retries() =>
                {
                    if !wait_with_cancellation(
                        self.config.retry_policy.delay_before_retry(retry_index),
                        cancellation,
                    ) {
                        return Err("Command Code request cancelled".into());
                    }
                    retry_index += 1;
                    continue;
                }
                Err(message) => return Err(message),
            };
            let retryable = parsed
                .error
                .as_ref()
                .and_then(|report| report.retryable)
                .unwrap_or(false);
            if !retryable || retry_index >= self.config.retry_policy.max_retries() {
                return Ok(parsed);
            }
            if !wait_with_cancellation(
                self.config.retry_policy.delay_before_retry(retry_index),
                cancellation,
            ) {
                return Err("Command Code request cancelled".into());
            }
            retry_index += 1;
        }
    }

    fn validate_model(&self, request: &ModelRequest) -> Result<(), String> {
        let Some(model) = &request.model else {
            return Ok(());
        };
        if model.provider == "command-code" && model.model == self.config.model {
            return Ok(());
        }
        Err("Command Code configuration does not match the requested model".into())
    }

    fn build_payload(&self, request: &ModelRequest) -> Result<JsonValue, String> {
        let messages = commandcode_messages(&request.context)?;
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                let schema = tool.schema.clone();
                Ok(json_value!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": schema,
                }))
            })
            .collect::<Result<Vec<_>, &str>>()?;
        let mut params = json_value!({
            "model": self.config.model,
            "messages": messages,
            "tools": tools,
            "system": request.system_prompt,
            "max_tokens": self.config.max_tokens,
            "stream": true,
        });
        if let Some(temperature) = self.config.temperature {
            params
                .as_object_mut()
                .expect("Command Code params are an object")
                .insert("temperature".to_owned(), json_value!(temperature));
        }
        if let Some(reasoning) = reasoning_effort(request.thinking_level) {
            params
                .as_object_mut()
                .expect("Command Code params are an object")
                .insert(
                    "reasoning_effort".to_owned(),
                    JsonValue::String(reasoning.into()),
                );
        }
        let mut payload = json_value!({
            "config": json_value!({
                "workingDir": self.config.host.working_directory,
                "date": self.config.host.date,
                "environment": self.config.host.environment,
                "structure": json_value!([]),
                "isGitRepo": false,
                "currentBranch": "",
                "mainBranch": "",
                // This adapter does not discover a repository. Match Command Code's current
                // non-repository shape instead of claiming an unverified clean worktree.
                "gitStatus": "",
                "recentCommits": json_value!([]),
            }),
            "memory": JsonValue::Null,
            "taste": JsonValue::Null,
            "skills": JsonValue::Null,
            "permissionMode": self.config.permission_mode.as_str(),
            "threadId": self.config.thread_id,
            "mode": self.config.mode,
            "params": params,
        });
        // The upstream JSON serializer omits `undefined` thread IDs. Sending a JSON null is a
        // distinct gateway input and is rejected, so preserve that wire-level distinction.
        if self.config.thread_id.is_none() {
            payload
                .as_object_mut()
                .expect("Command Code payload is an object")
                .remove("threadId");
        }
        Ok(payload)
    }

    fn run_request(
        &self,
        payload: &[u8],
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, String> {
        retry_with_backoff(self.config.retry_policy, cancellation, || {
            let mut request = Request::post(
                API_URL,
                payload.to_vec(),
                std::time::Duration::from_secs(60),
            );
            for header in self.request_headers() {
                let (name, value) = header
                    .split_once(": ")
                    .expect("Command Code headers use `name: value` spelling");
                request = request.header(name, value);
            }
            send(request, cancellation)
                .map(|response| response.body)
                .map_err(|failure| RetryableError {
                    retryable: !cancellation.is_cancelled(),
                    message: if cancellation.is_cancelled() {
                        "Command Code request cancelled".to_owned()
                    } else {
                        format!(
                            "Command Code HTTP transport failed before a provider response: {}",
                            failure.message
                        )
                    },
                })
        })
    }

    fn request_headers(&self) -> Vec<String> {
        let mut headers = vec![
            "Accept: application/x-ndjson".into(),
            "Content-Type: application/json".into(),
            format!("Authorization: Bearer {}", self.config.api_key),
            // Current Command Code CLI normalizes its production telemetry environment to this
            // exact wire value. It is gateway client metadata, not the host operating system.
            "X-CLI-Environment: production".into(),
            format!("X-Command-Code-Version: {CLIENT_VERSION}"),
            "User-Agent: cli".into(),
            format!("X-Project-Slug: {}", self.project_slug()),
            format!("X-Taste-Learning: {}", self.config.taste_learning_enabled),
            // The official direct-key client sends this explicit non-OAuth value.
            "X-Co-Flag: false".into(),
        ];
        // The official client uses the same generated UUID for a fresh headless thread and
        // session. Library callers own that identifier; never synthesize one from ambient state.
        if let Some(thread_id) = &self.config.thread_id {
            headers.push(format!("X-Session-Id: {thread_id}"));
        }
        if self.config.zero_data_retention {
            headers.push("X-Cmd-Zdr: 1".into());
        }
        headers
    }

    fn project_slug(&self) -> &str {
        self.config.project_slug.as_deref().unwrap_or_else(|| {
            project_slug_from_working_directory(&self.config.host.working_directory)
        })
    }
}

impl fmt::Debug for CommandCodeProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandCodeProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ModelProvider for CommandCodeProvider {
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> ModelFuture<'a> {
        let stream = self.response_stream(request, cancellation);
        Box::pin(std::future::ready(Ok(Box::new(stream) as _)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AgentToolCall, ModelDescriptor, SerializedJson, ThinkingLevel, ToolCallId};
    use crate::tool::{ToolDefinition, ToolExecutionMode};
    use tea_protocol::JsonValue;

    fn config() -> CommandCodeConfig {
        let host = CommandCodeHostContext::new("/sandbox/project", "2026-08-14", "darwin")
            .expect("explicit host context");
        CommandCodeConfig::new("test-key", "deepseek/deepseek-v4-flash", host)
            .expect("explicit provider config")
            .with_permission_mode(CommandCodePermissionMode::AutoAccept)
            .with_thread_id("b51a3243-2dd9-4c81-b659-a039645b7d4e")
            .expect("thread id")
            .with_temperature(0.25)
            .expect("temperature")
            .with_zero_data_retention(true)
    }

    #[test]
    fn serializes_gateway_payload_from_explicit_host_context() {
        let request = ModelRequest {
            system_prompt: "Be concise".into(),
            context: r#"[
                {"role":"user","content":"inspect the tree"},
                {"role":"assistant","content":null,"tool_calls":[{
                    "id":"call-1","type":"function",
                    "function":{"name":"read","arguments":"{\"path\":\"README.md\"}"}
                }]},
                {"role":"tool","tool_call_id":"call-1","content":"contents","is_error":true,"details":"{\"raw\":\"details\"}"}
            ]"#
            .into(),
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read a file".into(),
                schema: JsonValue::parse(
                    r#"{"type":"object","properties":{"path":{"type":"string"}}}"#,
                )
                .expect("tool schema"),
                execution_mode: ToolExecutionMode::Parallel,
            }],
            model: Some(ModelDescriptor {
                provider: "command-code".into(),
                model: "deepseek/deepseek-v4-flash".into(),
                revision: None,
            }),
            thinking_level: ThinkingLevel::High,
        };

        let payload = CommandCodeProvider::new(config())
            .build_payload(&request)
            .expect("payload");
        assert_eq!(
            field(field(&payload, "config"), "workingDir").as_str(),
            Some("/sandbox/project")
        );
        assert_eq!(
            field(field(&payload, "config"), "date").as_str(),
            Some("2026-08-14")
        );
        assert_eq!(
            field(field(&payload, "config"), "environment").as_str(),
            Some("darwin")
        );
        assert_eq!(
            field(&payload, "permissionMode").as_str(),
            Some("auto-accept")
        );
        assert_eq!(
            field(&payload, "threadId").as_str(),
            Some("b51a3243-2dd9-4c81-b659-a039645b7d4e")
        );
        assert_eq!(
            field(field(&payload, "config"), "gitStatus").as_str(),
            Some("")
        );
        assert_eq!(
            field(field(&payload, "params"), "reasoning_effort").as_str(),
            Some("high")
        );
        assert_eq!(
            field(
                array_item(field(field(&payload, "params"), "tools"), 0),
                "input_schema"
            )
            .get("type")
            .and_then(JsonValue::as_str),
            Some("object")
        );
        assert_eq!(
            field(
                array_item(field(field(&payload, "params"), "messages"), 1),
                "content"
            )
            .as_array()
            .and_then(|content| content.first())
            .and_then(|content| content.get("type"))
            .and_then(JsonValue::as_str),
            Some("tool-call")
        );
        assert_eq!(
            field(
                array_item(field(field(&payload, "params"), "messages"), 2),
                "content"
            )
            .as_array()
            .and_then(|content| content.first())
            .and_then(|content| content.get("toolName"))
            .and_then(JsonValue::as_str),
            Some("read")
        );
        assert_eq!(
            field(
                array_item(field(field(&payload, "params"), "messages"), 2),
                "content"
            )
            .as_array()
            .and_then(|content| content.first())
            .and_then(|content| content.get("isError"))
            .and_then(JsonValue::as_bool),
            Some(true)
        );
        assert!(
            field(
                array_item(field(field(&payload, "params"), "messages"), 2),
                "content"
            )
            .as_array()
            .and_then(|content| content.first())
            .and_then(|content| content.get("output"))
            .and_then(|output| output.get("value"))
            .and_then(JsonValue::as_str)
            .is_some_and(|content| content.contains("[tool details (serialized JSON):"))
        );
    }

    #[test]
    fn maps_generic_reasoning_levels_to_command_code_vocabulary() {
        assert_eq!(reasoning_effort(ThinkingLevel::Off), None);
        assert_eq!(reasoning_effort(ThinkingLevel::Minimal), Some("low"));
        assert_eq!(reasoning_effort(ThinkingLevel::Low), Some("low"));
        assert_eq!(reasoning_effort(ThinkingLevel::Max), Some("max"));
    }

    fn field<'a>(value: &'a JsonValue, name: &str) -> &'a JsonValue {
        value
            .get(name)
            .unwrap_or_else(|| panic!("missing JSON field {name}"))
    }

    fn array_item(value: &JsonValue, index: usize) -> &JsonValue {
        value
            .as_array()
            .and_then(|values| values.get(index))
            .unwrap_or_else(|| panic!("missing JSON array item {index}"))
    }

    #[test]
    fn translates_ndjson_text_tool_usage_and_finish() {
        let parsed = parse_ndjson_response(
            br#"{"type":"text-delta","text":"hi"}
{"type":"reasoning-delta","text":"thinking"}
{"type":"tool-call","toolCallId":"call-1","toolName":"read","input":{"path":"README.md"}}
{"type":"finish","finishReason":"tool-calls","totalUsage":{"inputTokens":12,"outputTokens":4,"reasoningTokens":2}}
{"type":"provider-metadata","provider":"command-code"}"#,
            "test-key",
        )
        .expect("NDJSON response parses");
        assert_eq!(parsed.events.len(), 3);
        assert_eq!(parsed.events[0], ModelStreamEvent::TextDelta("hi".into()));
        assert_eq!(
            parsed.events[1],
            ModelStreamEvent::ToolCall(AgentToolCall {
                id: ToolCallId::new("call-1").expect("call id"),
                name: "read".into(),
                arguments: SerializedJson::new(r#"{"path":"README.md"}"#),
            })
        );
        assert_eq!(parsed.events[2], ModelStreamEvent::End(StopReason::ToolUse));
        assert_eq!(
            parsed.usage,
            Usage {
                input_tokens: Some(12),
                output_tokens: Some(4),
                reasoning_tokens: Some(2),
                ..Usage::default()
            }
        );
    }

    #[test]
    fn remote_error_body_is_not_exposed_to_the_agent() {
        let parsed = parse_ndjson_response(
            br#"{"type":"error","error":{"message":"key test-key leaked remotely"}}"#,
            "test-key",
        )
        .expect("error is a terminal provider event");
        assert_eq!(parsed.usage, Usage::default());
        assert_eq!(
            parsed.events,
            vec![ModelStreamEvent::Error {
                message: "Command Code provider returned an error".into(),
            }]
        );
        assert_eq!(
            parsed.error,
            Some(CommandCodeErrorReport {
                source: CommandCodeErrorSource::Gateway,
                message: "key [redacted] leaked remotely".into(),
                status_code: None,
                error_type: None,
                error_code: None,
                retryable: Some(true),
            })
        );
    }

    #[test]
    fn http_error_envelope_without_event_type_preserves_gateway_diagnostics() {
        let parsed = parse_ndjson_response(
            br#"{"success":false,"error":{"code":"UNAUTHORIZED","status":401,"message":"Invalid token"}}"#,
            "test-key",
        )
        .expect("HTTP error envelope is a terminal provider event");
        assert_eq!(
            parsed.error,
            Some(CommandCodeErrorReport {
                source: CommandCodeErrorSource::Gateway,
                message: "Invalid token".into(),
                status_code: Some(401),
                error_type: None,
                error_code: Some("UNAUTHORIZED".into()),
                retryable: Some(false),
            })
        );
        assert_eq!(
            parsed.events,
            vec![ModelStreamEvent::Error {
                message: "Command Code provider returned an error".into(),
            }]
        );
    }

    #[test]
    fn remote_error_report_preserves_gateway_classification_and_retry_advice() {
        let parsed = parse_ndjson_response(
            br#"{"type":"error","error":{"message":"429 {\"error\":{\"type\":\"rate_limit\",\"message\":\"slow down\"}}","statusCode":500,"isRetryable":false,"code":"upstream_failed"}}"#,
            "test-key",
        )
        .expect("error is a terminal provider event");
        assert_eq!(
            parsed.error,
            Some(CommandCodeErrorReport {
                source: CommandCodeErrorSource::Gateway,
                message: "slow down".into(),
                status_code: Some(429),
                error_type: Some("rate_limit".into()),
                error_code: Some("upstream_failed".into()),
                // Match Command Code's 1.24.0 rule: a reported status takes precedence over
                // an explicit false retry hint, and 429 is retryable.
                retryable: Some(true),
            })
        );
    }

    #[test]
    fn rejects_content_after_finish_but_accepts_only_known_metadata() {
        let error = parse_ndjson_response(
            br#"{"type":"finish","finishReason":"stop"}
{"type":"text-delta","text":"late content"}"#,
            "test-key",
        )
        .expect_err("content after finish is not valid Command Code stream grammar");
        assert_eq!(
            error,
            "Command Code response contained events after its terminal event"
        );
    }

    #[test]
    fn classifies_incomplete_gateway_responses_as_retryable() {
        assert!(is_retryable_response_error(
            "Command Code NDJSON event did not contain type"
        ));
        assert!(is_retryable_response_error(
            "Command Code stream ended without a terminal event"
        ));
        assert!(!is_retryable_response_error(
            "Command Code tool call omitted its identifier"
        ));
        assert!(!is_retryable_response_error(
            "Command Code received invalid converted context"
        ));
    }

    #[test]
    fn configuration_rejects_ambient_placeholders_and_redacts_the_key() {
        assert_eq!(
            CommandCodeHostContext::new("", "2026-08-14", "darwin"),
            Err(CommandCodeConfigError::EmptyField("working directory"))
        );
        let debug = format!("{:?}", config());
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("test-key"));
    }

    #[test]
    fn gateway_headers_match_the_upstream_command_code_contract() {
        let headers = CommandCodeProvider::new(config()).request_headers();
        assert!(headers.contains(&"Accept: application/x-ndjson".into()));
        assert!(headers.contains(&"Authorization: Bearer test-key".into()));
        assert!(headers.contains(&"X-CLI-Environment: production".into()));
        assert!(headers.contains(&"X-Command-Code-Version: 1.24.0".into()));
        assert!(headers.contains(&"X-Project-Slug: project".into()));
        assert!(headers.contains(&"X-Taste-Learning: true".into()));
        assert!(headers.contains(&"X-Co-Flag: false".into()));
        assert!(headers.contains(&"X-Session-Id: b51a3243-2dd9-4c81-b659-a039645b7d4e".into()));
        assert!(headers.contains(&"User-Agent: cli".into()));
        assert!(headers.contains(&"X-Cmd-Zdr: 1".into()));
    }

    #[test]
    fn rejects_a_request_for_a_different_provider_or_model() {
        let request = ModelRequest {
            model: Some(ModelDescriptor {
                provider: "openrouter".into(),
                model: "different-model".into(),
                revision: None,
            }),
            ..ModelRequest::default()
        };
        let error = CommandCodeProvider::new(config())
            .validate_model(&request)
            .expect_err("provider mismatch is explicit");
        assert_eq!(
            error,
            "Command Code configuration does not match the requested model"
        );
    }

    #[test]
    fn omits_an_unset_thread_id_instead_of_sending_json_null() {
        let host = CommandCodeHostContext::new("/sandbox/project", "2026-08-14", "darwin")
            .expect("explicit host context");
        let provider = CommandCodeProvider::new(
            CommandCodeConfig::new("test-key", "deepseek/deepseek-v4-flash", host)
                .expect("provider config"),
        );
        let payload = provider
            .build_payload(&ModelRequest {
                context: "[]".into(),
                ..ModelRequest::default()
            })
            .expect("payload");
        assert!(payload.get("threadId").is_none());
    }

    #[test]
    fn rejects_non_uuid_thread_ids_instead_of_sending_a_different_wire_shape() {
        let host = CommandCodeHostContext::new("/sandbox/project", "2026-08-14", "darwin")
            .expect("explicit host context");
        let error = CommandCodeConfig::new("test-key", "deepseek/deepseek-v4-flash", host)
            .expect("provider config")
            .with_thread_id("thread-7")
            .expect_err("the current Command Code client omits non-UUID thread IDs");
        assert_eq!(error, CommandCodeConfigError::InvalidThreadId);
    }

    #[test]
    fn caller_can_override_the_project_slug_and_disable_taste_learning() {
        let headers = CommandCodeProvider::new(
            config()
                .with_project_slug("virtual-project")
                .expect("project slug")
                .with_taste_learning_enabled(false),
        )
        .request_headers();
        assert!(headers.contains(&"X-Project-Slug: virtual-project".into()));
        assert!(headers.contains(&"X-Taste-Learning: false".into()));
    }
}
