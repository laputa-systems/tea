//! Opt-in provider coding-evaluation adapter for the Rust default profile.
//!
//! This binary is intentionally outside the library boundary: it is invoked only by the final
//! v1 evaluation controller through a caller-owned secret-injection boundary. It supplies a
//! concrete transport to exercise the otherwise provider-free core, while retaining the core's
//! explicit workspace, profile, and Smol-owned execution boundaries.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tea_core::event::AgentEventKind;
use tea_core::provider::commandcode::{
    CommandCodeConfig, CommandCodeHostContext, CommandCodeProvider,
};
use tea_core::provider::openai::OpenAiContextHook;
use tea_core::provider::openrouter::{OpenRouterConfig, OpenRouterCostReport, OpenRouterProvider};
use tea_core::scheduler::ModelProvider;
use tea_core::state::{AgentMessage, ModelDescriptor};
use tea_core::{Agent, DefaultCodingTools};
use tea_protocol::{JsonNumber, JsonValue};

const RESULT_SCHEMA: &str = "tea-coding-eval-result/v1";

/// Explicit command-line arguments supplied by `evals/controller.py`.
struct Args {
    provider: ProviderKind,
    model: String,
    task_json: PathBuf,
    workspace: PathBuf,
    capabilities_json: PathBuf,
    result_json: PathBuf,
    attempt_id: String,
    baseline_id: String,
    commandcode_date: Option<String>,
    commandcode_environment: Option<String>,
    commandcode_thread_id: Option<String>,
    commandcode_project_slug: Option<String>,
}

/// Explicit provider choice for this executable integration boundary.
#[derive(Clone, Copy)]
enum ProviderKind {
    OpenRouter,
    CommandCode,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut values = std::collections::BTreeMap::<String, String>::new();
        let mut arguments = env::args().skip(1);
        while let Some(flag) = arguments.next() {
            if !flag.starts_with("--") {
                return Err(format!("unexpected positional argument {flag:?}"));
            }
            let value = arguments
                .next()
                .ok_or_else(|| format!("missing value for {flag}"))?;
            if values.insert(flag, value).is_some() {
                return Err("evaluation adapter arguments must not repeat flags".into());
            }
        }
        let take = |flag: &str| {
            values
                .get(flag)
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or_else(|| format!("missing required argument {flag}"))
        };
        let provider = match values.get("--provider").map(String::as_str) {
            None | Some("openrouter") => ProviderKind::OpenRouter,
            Some("commandcode" | "command-code") => ProviderKind::CommandCode,
            Some(_) => return Err("--provider must be openrouter or commandcode".into()),
        };
        let commandcode_date = values.get("--commandcode-date").cloned();
        let commandcode_environment = values.get("--commandcode-environment").cloned();
        let commandcode_thread_id = values.get("--commandcode-thread-id").cloned();
        let commandcode_project_slug = values.get("--commandcode-project-slug").cloned();
        for flag in values.keys() {
            if !matches!(
                flag.as_str(),
                "--provider"
                    | "--model"
                    | "--task-json"
                    | "--workspace"
                    | "--capabilities-json"
                    | "--result-json"
                    | "--attempt-id"
                    | "--baseline-id"
                    | "--commandcode-date"
                    | "--commandcode-environment"
                    | "--commandcode-thread-id"
                    | "--commandcode-project-slug"
            ) {
                return Err(format!("unsupported evaluation adapter argument {flag}"));
            }
        }
        if matches!(provider, ProviderKind::CommandCode)
            && (commandcode_date.is_none()
                || commandcode_environment.is_none()
                || commandcode_thread_id.is_none()
                || commandcode_project_slug.is_none())
        {
            return Err(
                "Command Code requires --commandcode-date, --commandcode-environment, --commandcode-thread-id, and --commandcode-project-slug".into(),
            );
        }
        Ok(Self {
            provider,
            model: take("--model")?,
            task_json: PathBuf::from(take("--task-json")?),
            workspace: PathBuf::from(take("--workspace")?),
            capabilities_json: PathBuf::from(take("--capabilities-json")?),
            result_json: PathBuf::from(take("--result-json")?),
            attempt_id: take("--attempt-id")?,
            baseline_id: take("--baseline-id")?,
            commandcode_date,
            commandcode_environment,
            commandcode_thread_id,
            commandcode_project_slug,
        })
    }
}

fn event_name(event: &AgentEventKind) -> &'static str {
    match event {
        AgentEventKind::CompactionLifecycle { .. } => "compaction_lifecycle",
        AgentEventKind::ProviderRequestObserved { .. } => "provider_request_observed",
        AgentEventKind::CompactionStart { .. } => "compaction_start",
        AgentEventKind::CompactionResult { .. } => "compaction_result",
        AgentEventKind::CompactionEnd { .. } => "compaction_end",
        AgentEventKind::AutomaticCompactionStart { .. } => "automatic_compaction_start",
        AgentEventKind::AutomaticCompactionEnd { .. } => "automatic_compaction_end",
        AgentEventKind::ContextEstimate { .. } => "context_estimate",
        AgentEventKind::ProviderRequestSkipped { .. } => "provider_request_skipped",
        AgentEventKind::ToolFailureObserved { .. } => "tool_failure_observed",
        AgentEventKind::AgentStart => "agent_start",
        AgentEventKind::TurnStart { .. } => "turn_start",
        AgentEventKind::MessageStart { .. } => "message_start",
        AgentEventKind::MessageUpdate { .. } => "message_update",
        AgentEventKind::MessageEnd { .. } => "message_end",
        AgentEventKind::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEventKind::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEventKind::ToolExecutionEnd { .. } => "tool_execution_end",
        AgentEventKind::ModelTurnUsage { .. } => "model_turn_usage",
        AgentEventKind::TurnEnd { .. } => "turn_end",
        AgentEventKind::AgentEnd { .. } => "agent_end",
    }
}

fn openrouter_cost_json(report: &OpenRouterCostReport) -> JsonValue {
    let turns = report
        .turns
        .iter()
        .map(|turn| {
            JsonValue::object([
                ("turn", JsonValue::from(turn.turn as u64)),
                ("source", JsonValue::from(turn.source.as_str())),
                ("total_usd", optional_f64(turn.total_usd)),
                (
                    "upstream_inference_usd",
                    optional_f64(turn.upstream_inference_usd),
                ),
                ("model", optional_string(turn.model.as_ref())),
                ("provider", optional_string(turn.provider.as_ref())),
                ("input_tokens", optional_u64(turn.input_tokens)),
                ("output_tokens", optional_u64(turn.output_tokens)),
                ("cache_read_tokens", optional_u64(turn.cache_read_tokens)),
                ("cache_write_tokens", optional_u64(turn.cache_write_tokens)),
                ("reasoning_tokens", optional_u64(turn.reasoning_tokens)),
            ])
        })
        .collect::<Vec<_>>();
    JsonValue::object([
        ("schema_version", JsonValue::from("tea-eval-cost/v1")),
        ("currency", JsonValue::from("USD")),
        ("pricing", JsonValue::from("provider_reported")),
        (
            "reported_turn_count",
            JsonValue::from(report.reported_turn_count as u64),
        ),
        (
            "unavailable_turn_count",
            JsonValue::from(report.unavailable_turn_count as u64),
        ),
        ("complete", JsonValue::from(report.complete)),
        ("reported_total_usd", json_f64(report.reported_total_usd)),
        (
            "reported_upstream_inference_usd",
            json_f64(report.reported_upstream_inference_usd),
        ),
        ("turns", JsonValue::Array(turns)),
    ])
}

fn json_f64(value: f64) -> JsonValue {
    JsonValue::number(JsonNumber::Float(value)).expect("evaluation JSON numbers are finite")
}

fn optional_f64(value: Option<f64>) -> JsonValue {
    value.map(json_f64).unwrap_or(JsonValue::Null)
}

fn optional_u64(value: Option<u64>) -> JsonValue {
    value.map(JsonValue::from).unwrap_or(JsonValue::Null)
}

fn optional_string(value: Option<&String>) -> JsonValue {
    value
        .map(|value| JsonValue::from(value.clone()))
        .unwrap_or(JsonValue::Null)
}

/// A concrete opt-in provider plus only the accounting this evaluation host needs.
enum EvalProvider {
    OpenRouter(Arc<OpenRouterProvider>),
    CommandCode(Arc<CommandCodeProvider>),
}

impl EvalProvider {
    fn model_provider(&self) -> Arc<dyn ModelProvider> {
        match self {
            Self::OpenRouter(provider) => provider.clone() as Arc<dyn ModelProvider>,
            Self::CommandCode(provider) => provider.clone() as Arc<dyn ModelProvider>,
        }
    }

    fn usage_snapshot(&self) -> tea_core::state::Usage {
        match self {
            Self::OpenRouter(provider) => provider.usage_snapshot(),
            Self::CommandCode(provider) => provider.usage_snapshot(),
        }
    }

    fn provider_name(&self) -> &'static str {
        match self {
            Self::OpenRouter(_) => "openrouter",
            Self::CommandCode(_) => "command-code",
        }
    }

    fn cost_json(&self) -> Option<JsonValue> {
        match self {
            Self::OpenRouter(provider) => Some(openrouter_cost_json(&provider.cost_report())),
            // The Command Code gateway does not report price fields in its NDJSON contract.
            // Omitting this optional artifact field avoids manufacturing a local price estimate.
            Self::CommandCode(_) => None,
        }
    }

    /// Preserve actionable Command Code failure classification in the controller artifact while
    /// keeping its arbitrary remote message out of a broadly retained evaluation report.
    fn error_json(&self) -> Option<JsonValue> {
        let Self::CommandCode(provider) = self else {
            return None;
        };
        provider.last_error_report().map(|report| {
            JsonValue::object([
                ("source", JsonValue::from(report.source.as_str())),
                (
                    "status_code",
                    optional_u64(report.status_code.map(u64::from)),
                ),
                ("error_type", optional_string(report.error_type.as_ref())),
                ("error_code", optional_string(report.error_code.as_ref())),
                (
                    "retryable",
                    report
                        .retryable
                        .map(JsonValue::from)
                        .unwrap_or(JsonValue::Null),
                ),
            ])
        })
    }
}

fn terminal_status(result: &Result<(), tea_core::CoreError>) -> &'static str {
    match result {
        Ok(()) => "completed",
        Err(tea_core::CoreError::Cancelled) => "cancelled",
        Err(tea_core::CoreError::ModelAborted { .. }) => "aborted",
        Err(_) => "failed",
    }
}

/// A redacted failure class for the controller artifact. It is intentionally not the provider
/// message: evaluation reports must not retain arbitrary provider payloads or credentials.
fn terminal_code(result: &Result<(), tea_core::CoreError>) -> Option<&'static str> {
    match result {
        Ok(()) => None,
        Err(tea_core::CoreError::Cancelled) => Some("cancelled"),
        Err(tea_core::CoreError::ModelAborted { .. }) => Some("model_aborted"),
        Err(tea_core::CoreError::ModelError { .. }) => Some("model_error"),
        Err(tea_core::CoreError::ModelProvider { .. }) => Some("model_provider"),
        Err(tea_core::CoreError::UnsupportedModelStream { .. }) => Some("unsupported_model_stream"),
        Err(tea_core::CoreError::Hook(_)) => Some("hook"),
        Err(tea_core::CoreError::EffectGate(_)) => Some("effect_gate"),
        Err(tea_core::CoreError::MissingModelProvider) => Some("missing_model_provider"),
        Err(tea_core::CoreError::ActiveRun { .. }) => Some("active_run"),
        Err(tea_core::CoreError::InvalidTransition(_)) => Some("invalid_transition"),
        Err(tea_core::CoreError::RunFinished { .. }) => Some("run_finished"),
        Err(tea_core::CoreError::Compaction(_)) => Some("compaction"),
        Err(tea_core::CoreError::MissingCompactor) => Some("missing_compactor"),
        Err(tea_core::CoreError::AutomaticCompactionUnavailable { .. }) => {
            Some("automatic_compaction_unavailable")
        }
        Err(tea_core::CoreError::AutomaticCompaction { .. }) => Some("automatic_compaction"),
        Err(tea_core::CoreError::InvalidAutomaticCompactionPolicy { .. }) => {
            Some("invalid_automatic_compaction_policy")
        }
        Err(tea_core::CoreError::InvalidToolResultProjectionPolicy { .. }) => {
            Some("invalid_tool_result_projection_policy")
        }
        Err(tea_core::CoreError::ToolCircuitBreaker { .. }) => Some("tool_circuit_breaker"),
    }
}

fn final_text(agent: &Agent) -> String {
    agent
        .snapshot()
        .messages
        .into_iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .unwrap_or_default()
}

fn read_json(path: &PathBuf, label: &str) -> Result<JsonValue, String> {
    let bytes = fs::read(path).map_err(|_| format!("cannot read evaluation {label}"))?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| format!("evaluation {label} is not JSON"))?;
    JsonValue::parse(text).map_err(|_| format!("evaluation {label} is not JSON"))
}

fn main() -> Result<(), String> {
    let args = Args::parse()?;
    let task = read_json(&args.task_json, "task")?;
    let capabilities = read_json(&args.capabilities_json, "capabilities")?;
    if task.get("capabilities") != Some(&capabilities) {
        return Err("evaluation capability manifest does not match task".into());
    }
    let prompt = task
        .get("prompt")
        .and_then(JsonValue::as_str)
        .filter(|prompt| !prompt.is_empty())
        .ok_or_else(|| "evaluation task has no prompt".to_owned())?;
    let default_tools = DefaultCodingTools::new(&args.workspace)
        .map_err(|error| format!("cannot construct explicit workspace tools: {error}"))?;
    let provider = match args.provider {
        ProviderKind::OpenRouter => {
            let api_key = env::var("OPENROUTER_API_KEY").map_err(|_| {
                "OPENROUTER_API_KEY must be supplied by the caller's secret injector".to_owned()
            })?;
            EvalProvider::OpenRouter(Arc::new(OpenRouterProvider::new(OpenRouterConfig::new(
                api_key,
                args.model.clone(),
            ))))
        }
        ProviderKind::CommandCode => {
            let api_key = env::var("COMMANDCODE_API_KEY").map_err(|_| {
                "COMMANDCODE_API_KEY must be supplied by the caller's secret injector".to_owned()
            })?;
            let host = CommandCodeHostContext::new(
                args.workspace.to_string_lossy(),
                args.commandcode_date
                    .as_deref()
                    .expect("validated Command Code date"),
                args.commandcode_environment
                    .as_deref()
                    .expect("validated Command Code environment"),
            )
            .map_err(|error| format!("invalid Command Code host context: {error}"))?;
            let config = CommandCodeConfig::new(api_key, args.model.clone(), host)
                .map_err(|error| format!("invalid Command Code configuration: {error}"))?
                .with_thread_id(
                    args.commandcode_thread_id
                        .as_deref()
                        .expect("validated Command Code thread ID"),
                )
                .and_then(|config| {
                    config.with_project_slug(
                        args.commandcode_project_slug
                            .as_deref()
                            .expect("validated Command Code project slug"),
                    )
                })
                .map_err(|error| format!("invalid Command Code configuration: {error}"))?;
            EvalProvider::CommandCode(Arc::new(CommandCodeProvider::new(config)))
        }
    };
    let agent = Agent::builder()
        .model(ModelDescriptor {
            provider: provider.provider_name().into(),
            model: args.model,
            revision: None,
        })
        .hooks(Arc::new(OpenAiContextHook))
        .model_provider(provider.model_provider())
        .pinned_default_coding_profile(default_tools)
        .map_err(|error| format!("cannot apply pinned default profile: {error}"))?
        .build();
    let run = agent
        .start_prompt(prompt)
        .map_err(|error| format!("cannot start evaluation run: {error}"))?;
    let result = smol::block_on(run.drive());
    let events = run.events();
    let totals = provider.usage_snapshot();
    let trace = events
        .iter()
        .map(|event| {
            JsonValue::object([
                ("seq", JsonValue::from(event.sequence.0)),
                ("type", JsonValue::from(event_name(&event.kind))),
            ])
        })
        .collect::<Vec<_>>();
    let turns = events
        .iter()
        .filter(|event| matches!(event.kind, AgentEventKind::TurnStart { .. }))
        .count();
    let tool_calls = events
        .iter()
        .filter(|event| matches!(event.kind, AgentEventKind::ToolExecutionStart { .. }))
        .count();
    let mut output = JsonValue::object([
        ("schema_version", JsonValue::from(RESULT_SCHEMA)),
        ("attempt_id", JsonValue::from(args.attempt_id)),
        ("baseline_id", JsonValue::from(args.baseline_id)),
        (
            "terminal",
            JsonValue::object([
                ("status", JsonValue::from(terminal_status(&result))),
                (
                    "code",
                    terminal_code(&result)
                        .map(JsonValue::from)
                        .unwrap_or(JsonValue::Null),
                ),
            ]),
        ),
        ("final_text", JsonValue::from(final_text(&agent))),
        ("turns", JsonValue::from(turns as u64)),
        ("tool_calls", JsonValue::from(tool_calls as u64)),
        (
            "usage",
            JsonValue::object([
                ("input", JsonValue::from(totals.input_tokens.unwrap_or(0))),
                ("output", JsonValue::from(totals.output_tokens.unwrap_or(0))),
                ("cache_read", JsonValue::from(0_u64)),
                ("cache_write", JsonValue::from(0_u64)),
            ]),
        ),
        ("trace", JsonValue::Array(trace)),
    ]);
    if let Some(cost) = provider.cost_json() {
        output
            .as_object_mut()
            .expect("evaluation output is an object")
            .insert("cost".to_owned(), cost);
    }
    if let Some(error) = provider.error_json() {
        output
            .as_object_mut()
            .expect("evaluation output is an object")
            .insert("provider_error".to_owned(), error);
    }
    let encoded = output
        .to_json_string()
        .map(String::into_bytes)
        .map_err(|_| "cannot encode evaluation result".to_owned())?;
    fs::write(&args.result_json, encoded)
        .map_err(|_| "cannot write evaluation result".to_owned())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::terminal_code;

    #[test]
    fn terminal_code_redacts_effect_gate_failures() {
        let error = tea_core::CoreError::EffectGate(tea_core::EffectGateError::new(
            "host durability detail must not enter the evaluation report",
        ));
        assert_eq!(terminal_code(&Err(error)), Some("effect_gate"));
    }
}
