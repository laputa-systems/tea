//! Stable model-facing collaboration surfaces for optional child lanes.
//!
//! These definitions are immutable harness input, not live capabilities. The
//! coordinator installs executable implementations only after the durable
//! policy, graph linkage, and task ownership contracts have been accepted.
//! Keeping the surface here lets a host seed the exact root and child harness
//! bytes before any `spawn_agent` call can occur.

use super::{
    ApplyAgentChangesResult, InterruptAgentResult, SpawnAgentRequest, SubagentCoordinator, SubagentPolicy,
    SubagentPolicyError, SubagentStatus, WaitAgentsRequest, WaitAgentsResult, WaitReturnWhen,
    WaitedSubagent,
};
use crate::harness::ToolPresentationDescriptor;
use crate::tool::{
    AgentTool, AgentToolResult, CancellationSettlementMode, ToolCall, ToolContext, ToolDefinition,
    ToolExecutionMode, ToolFailure, ToolFuture, ToolRegistry, ToolUpdateSink,
};
use std::sync::Arc;
use std::time::Duration;
use tea_protocol::{JsonNumber, JsonValue};
use tea_session::{CanonicalHashWriter, Digest, WorkspaceDeltaId};
use tea_session::{AgentContextMode, AgentState, SessionWriter};

/// Stable root-only instruction appended when child services are enabled.
pub const ROOT_SUBAGENT_INSTRUCTION_SUFFIX: &str = concat!(
    "You may delegate independent work to isolated subagents.\n\n",
    "Use spawn_agent with a complete assignment and a model selected from the\n",
    "host-authorized catalog. Subagents do not share your writable working tree.\n",
    "Their changes are returned as durable deltas and remain invisible here until\n",
    "you explicitly call apply_agent_changes.\n\n",
    "Use wait_agent to retrieve reports. Child output is never inserted into your\n",
    "context automatically. Delegate only work that can proceed independently; keep\n",
    "small or sequential work in the current agent."
);

/// Stable instruction appended to every child harness.
pub const CHILD_SUBAGENT_INSTRUCTION_SUFFIX: &str = concat!(
    "You are a Tea subagent executing one bounded assignment.\n\n",
    "Work only on the assigned task. Your workspace is an isolated snapshot. Your\n",
    "edits are not visible to the parent until the parent explicitly applies them.\n\n",
    "Inspect and test your work normally. Use your final response as a concise\n",
    "report covering what you found, what you changed, validation performed, and\n",
    "remaining risks.\n\n",
    "You cannot spawn additional agents."
);

const THINKING_LEVELS: [&str; 7] = [
    "off", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// Names reserved exclusively for the immutable root collaboration surface.
/// Child harnesses reject both a presentation and an executable capability
/// with any of these names in V1.
pub(crate) const ROOT_SUBAGENT_TOOL_NAMES: [&str; 5] = [
    "spawn_agent",
    "wait_agent",
    "list_agents",
    "interrupt_agent",
    "apply_agent_changes",
];

/// Return the five root-only collaboration definitions in their durable order.
///
/// The ordered policy catalog supplies the `spawn_agent.model` enum verbatim.
/// A child receives no definitions from this function; its harness contains
/// only its independently selected coding and recovery tools.
pub fn root_subagent_tool_definitions(
    policy: &SubagentPolicy,
) -> Result<Vec<ToolDefinition>, SubagentPolicyError> {
    policy.validate()?;
    let models = policy
        .models
        .iter()
        .map(|model| JsonValue::String(model.descriptor.model.clone()))
        .collect::<Vec<_>>();
    Ok(vec![
        ToolDefinition {
            name: "spawn_agent".into(),
            description: "Start one isolated child assignment without waiting for its completion. The selected model must be in the host-authorized catalog.".into(),
            schema: spawn_agent_schema(models),
            execution_mode: ToolExecutionMode::Sequential,
            requires_exclusive_batch: false,
            cancellation_settlement_mode: CancellationSettlementMode::AwaitFuture,
        },
        ToolDefinition {
            name: "wait_agent".into(),
            description: "Wait for one or more child assignments owned by the current root operation and return only requested durable reports.".into(),
            schema: wait_agent_schema(),
            execution_mode: ToolExecutionMode::Sequential,
            requires_exclusive_batch: false,
            cancellation_settlement_mode: CancellationSettlementMode::DropFuture,
        },
        ToolDefinition {
            name: "list_agents".into(),
            description: "List child assignments owned by the current root operation without returning reports, intermediate output, patches, or paths.".into(),
            schema: empty_object_schema(),
            execution_mode: ToolExecutionMode::Parallel,
            requires_exclusive_batch: false,
            cancellation_settlement_mode: CancellationSettlementMode::DropFuture,
        },
        ToolDefinition {
            name: "interrupt_agent".into(),
            description: "Interrupt one child assignment owned by the current root operation and wait for durable cancellation settlement.".into(),
            schema: interrupt_agent_schema(),
            execution_mode: ToolExecutionMode::Sequential,
            requires_exclusive_batch: false,
            cancellation_settlement_mode: CancellationSettlementMode::AwaitFuture,
        },
        ToolDefinition {
            name: "apply_agent_changes".into(),
            description: "Preflight and apply exactly one durable child workspace delta to the root workspace without changing the user index.".into(),
            schema: apply_agent_changes_schema(),
            execution_mode: ToolExecutionMode::Sequential,
            requires_exclusive_batch: true,
            cancellation_settlement_mode: CancellationSettlementMode::AwaitFuture,
        },
    ])
}

/// Return root collaboration presentations suitable for immutable harness seeding.
pub fn root_subagent_tool_presentations(
    policy: &SubagentPolicy,
) -> Result<Vec<ToolPresentationDescriptor>, SubagentPolicyError> {
    Ok(root_subagent_tool_definitions(policy)?
        .into_iter()
        .map(presentation_from_definition)
        .collect())
}

/// Compute the exact ordered collaboration surface digest persisted with an
/// enabled subagent policy.
pub fn root_subagent_tool_surface_digest(
    policy: &SubagentPolicy,
) -> Result<Digest, SubagentPolicyError> {
    let definitions = root_subagent_tool_definitions(policy)?;
    let mut writer = CanonicalHashWriter::new("tea-subagent-tool-surface-v1", 1, 1);
    writer.u64("tool_count", definitions.len() as u64);
    for (index, definition) in definitions.iter().enumerate() {
        writer.u64("tool_index", index as u64);
        writer.string("tool_name", &definition.name);
        writer.string("tool_description", &definition.description);
        writer.string(
            "tool_schema",
            &definition
                .schema
                .to_json_string()
                .expect("fixed subagent tool schemas are encodable"),
        );
        writer.string(
            "tool_execution_mode",
            execution_mode_name(definition.execution_mode),
        );
        writer.boolean(
            "tool_requires_exclusive_batch",
            definition.requires_exclusive_batch,
        );
        writer.string(
            "tool_cancellation_settlement_mode",
            cancellation_mode_name(definition.cancellation_settlement_mode),
        );
    }
    Ok(writer.finish())
}

/// Append the enabled root surface while preserving feature-disabled bytes.
///
/// With `None`, this leaves both values exactly unchanged and returns no
/// digest. With a policy, it appends the fixed suffix and five definitions in
/// the documented order; callers then persist the returned digest with the
/// durable policy before activating the resulting harness revision.
pub fn append_root_subagent_surface(
    system_prompt: &mut String,
    tools: &mut Vec<ToolDefinition>,
    policy: Option<&SubagentPolicy>,
) -> Result<Option<Digest>, SubagentPolicyError> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    let definitions = root_subagent_tool_definitions(policy)?;
    let digest = root_subagent_tool_surface_digest(policy)?;
    append_suffix(system_prompt, ROOT_SUBAGENT_INSTRUCTION_SUFFIX);
    tools.extend(definitions);
    Ok(Some(digest))
}

/// Append the fixed child instruction suffix to a child system prompt.
///
/// This function deliberately accepts no policy and returns no collaboration
/// definitions: V1 children cannot spawn, wait for, list, interrupt, or apply
/// work owned by other agents.
pub fn append_child_subagent_instruction_suffix(system_prompt: &mut String) {
    append_suffix(system_prompt, CHILD_SUBAGENT_INSTRUCTION_SUFFIX);
}

/// Return the child collaboration definitions for V1.
///
/// The empty result is intentional and separately exposed so a child-harness
/// builder does not accidentally inherit the root's dynamic surface.
pub fn child_subagent_tool_definitions() -> Vec<ToolDefinition> {
    Vec::new()
}

/// Construct the live root registry for the fixed collaboration surface.
///
/// Install the live root collaboration tools only after the optional
/// coordinator has explicit host authority. The immutable presentations are
/// seeded before session creation; these implementations perform the durable
/// operation-specific work at effect time.
pub(crate) fn root_subagent_runtime_tools<S>(
    coordinator: Arc<SubagentCoordinator<S>>,
) -> Result<ToolRegistry, SubagentPolicyError>
where
    S: SessionWriter + Send + 'static,
{
    let mut tools = ToolRegistry::default();
    for definition in root_subagent_tool_definitions(&coordinator.services().policy)? {
        match definition.name.as_str() {
            "spawn_agent" => tools.insert(Arc::new(SpawnAgentTool {
                coordinator: Arc::clone(&coordinator),
                definition,
            })),
            "wait_agent" => tools.insert(Arc::new(WaitAgentTool {
                coordinator: Arc::clone(&coordinator),
                definition,
            })),
            "list_agents" => tools.insert(Arc::new(ListAgentsTool {
                coordinator: Arc::clone(&coordinator),
                definition,
            })),
            "interrupt_agent" => tools.insert(Arc::new(InterruptAgentTool {
                coordinator: Arc::clone(&coordinator),
                definition,
            })),
            "apply_agent_changes" => tools.insert(Arc::new(ApplyAgentChangesTool {
                coordinator: Arc::clone(&coordinator),
                definition,
            })),
            _ => unreachable!("fixed subagent definitions have five known names"),
        };
    }
    Ok(tools)
}

struct WaitAgentTool<S> {
    coordinator: Arc<SubagentCoordinator<S>>,
    definition: ToolDefinition,
}

impl<S> AgentTool for WaitAgentTool<S>
where
    S: SessionWriter + Send + 'static,
{
    fn name(&self) -> &str { &self.definition.name }
    fn description(&self) -> &str { &self.definition.description }
    fn schema(&self) -> &JsonValue { &self.definition.schema }
    fn execution_mode(&self) -> ToolExecutionMode { self.definition.execution_mode }
    fn requires_exclusive_batch(&self) -> bool { self.definition.requires_exclusive_batch }
    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        self.definition.cancellation_settlement_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let request = match parse_wait_request(&call) {
                Ok(request) => request,
                Err(message) => return Ok(recoverable_error(call, message)),
            };
            match self.coordinator.wait(context, request).await {
                Ok(result) => Ok(json_result(call, wait_result_value(result))),
                Err(error) => Ok(recoverable_error(call, error.to_string())),
            }
        })
    }
}

struct ListAgentsTool<S> {
    coordinator: Arc<SubagentCoordinator<S>>,
    definition: ToolDefinition,
}

impl<S> AgentTool for ListAgentsTool<S>
where
    S: SessionWriter + Send + 'static,
{
    fn name(&self) -> &str { &self.definition.name }
    fn description(&self) -> &str { &self.definition.description }
    fn schema(&self) -> &JsonValue { &self.definition.schema }
    fn execution_mode(&self) -> ToolExecutionMode { self.definition.execution_mode }
    fn requires_exclusive_batch(&self) -> bool { self.definition.requires_exclusive_batch }
    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        self.definition.cancellation_settlement_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            if let Err(message) = parse_empty_object(&call, "list_agents") {
                return Ok(recoverable_error(call, message));
            }
            match self.coordinator.list(&context) {
                Ok(statuses) => Ok(json_result(
                    call,
                    JsonValue::object([(
                        "agents",
                        JsonValue::Array(statuses.iter().map(list_status_value).collect()),
                    )]),
                )),
                Err(error) => Ok(recoverable_error(call, error.to_string())),
            }
        })
    }
}

struct InterruptAgentTool<S> {
    coordinator: Arc<SubagentCoordinator<S>>,
    definition: ToolDefinition,
}

impl<S> AgentTool for InterruptAgentTool<S>
where
    S: SessionWriter + Send + 'static,
{
    fn name(&self) -> &str { &self.definition.name }
    fn description(&self) -> &str { &self.definition.description }
    fn schema(&self) -> &JsonValue { &self.definition.schema }
    fn execution_mode(&self) -> ToolExecutionMode { self.definition.execution_mode }
    fn requires_exclusive_batch(&self) -> bool { self.definition.requires_exclusive_batch }
    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        self.definition.cancellation_settlement_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let target = match parse_interrupt_target(&call) {
                Ok(target) => target,
                Err(message) => return Ok(recoverable_error(call, message)),
            };
            match self.coordinator.interrupt(&context, &target).await {
                Ok(result) => Ok(json_result(call, interrupt_result_value(result))),
                Err(error) => Ok(recoverable_error(call, error.to_string())),
            }
        })
    }
}

struct SpawnAgentTool<S> {
    coordinator: Arc<SubagentCoordinator<S>>,
    definition: ToolDefinition,
}

impl<S> AgentTool for SpawnAgentTool<S>
where
    S: SessionWriter + Send + 'static,
{
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn schema(&self) -> &JsonValue {
        &self.definition.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.definition.execution_mode
    }

    fn requires_exclusive_batch(&self) -> bool {
        self.definition.requires_exclusive_batch
    }

    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        self.definition.cancellation_settlement_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let request = match parse_spawn_request(&call) {
                Ok(request) => request,
                Err(message) => return Ok(recoverable_error(call, message)),
            };
            match self.coordinator.spawn(call.clone(), context, request).await {
                Ok(handle) => Ok(AgentToolResult {
                    tool_call_id: call.id,
                    content: JsonValue::object([
                        ("agent_id", JsonValue::String(handle.agent_id.to_string())),
                        ("task_id", JsonValue::String(handle.operation_id.to_string())),
                        ("task_name", JsonValue::String(handle.task_name)),
                        ("state", JsonValue::String(agent_state_name(&handle.state).into())),
                    ])
                    .to_json_string()
                    .expect("fixed spawn result is JSON encodable"),
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                    is_error: false,
                    failure: None,
                }),
                Err(error) => Ok(recoverable_error(call, error.to_string())),
            }
        })
    }
}

struct ApplyAgentChangesTool<S> {
    coordinator: Arc<SubagentCoordinator<S>>,
    definition: ToolDefinition,
}

impl<S> AgentTool for ApplyAgentChangesTool<S>
where
    S: SessionWriter + Send + 'static,
{
    fn name(&self) -> &str {
        &self.definition.name
    }

    fn description(&self) -> &str {
        &self.definition.description
    }

    fn schema(&self) -> &JsonValue {
        &self.definition.schema
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.definition.execution_mode
    }

    fn requires_exclusive_batch(&self) -> bool {
        self.definition.requires_exclusive_batch
    }

    fn cancellation_settlement_mode(&self) -> CancellationSettlementMode {
        self.definition.cancellation_settlement_mode
    }

    fn execute<'a>(
        &'a self,
        call: ToolCall,
        context: ToolContext,
        _updates: ToolUpdateSink,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let delta_id = match parse_apply_delta_id(&call) {
                Ok(delta_id) => delta_id,
                Err(message) => return Ok(recoverable_error(call, message)),
            };
            match self.coordinator.apply(call.clone(), context, delta_id).await {
                Ok(result) => Ok(json_result(call, apply_result_value(result))),
                Err(error) => Ok(recoverable_error(call, error.to_string())),
            }
        })
    }
}

fn parse_spawn_request(call: &ToolCall) -> Result<SpawnAgentRequest, String> {
    let value = JsonValue::parse(call.arguments.as_str())
        .map_err(|_| "spawn_agent arguments must be valid JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "spawn_agent arguments must be a JSON object".to_owned())?;
    if object.keys().any(|key| {
        !matches!(key.as_str(), "task_name" | "task" | "model" | "thinking" | "context")
    }) {
        return Err("spawn_agent arguments contain an unknown field".into());
    }
    let required_string = |name: &str| {
        object
            .get(name)
            .and_then(JsonValue::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("spawn_agent requires string field {name}"))
    };
    let task_name = required_string("task_name")?;
    let task = required_string("task")?;
    let model = required_string("model")?;
    let thinking = match object.get("thinking") {
        Some(value) => Some(parse_thinking_level(
            value
                .as_str()
                .ok_or_else(|| "spawn_agent thinking must be a string".to_owned())?,
        )?),
        None => None,
    };
    let context_mode = match object.get("context") {
        None => AgentContextMode::Task,
        Some(value) => match value
            .as_str()
            .ok_or_else(|| "spawn_agent context must be a string".to_owned())?
        {
            "task" => AgentContextMode::Task,
            "parent" => AgentContextMode::Parent,
            _ => return Err("spawn_agent context must be task or parent".into()),
        },
    };
    validate_spawn_text(&task_name, &task)?;
    Ok(SpawnAgentRequest {
        task_name,
        task,
        model,
        thinking,
        context_mode,
    })
}

fn parse_wait_request(call: &ToolCall) -> Result<WaitAgentsRequest, String> {
    let value = JsonValue::parse(call.arguments.as_str())
        .map_err(|_| "wait_agent arguments must be valid JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "wait_agent arguments must be a JSON object".to_owned())?;
    if object.keys().any(|key| !matches!(key.as_str(), "targets" | "return_when" | "timeout_ms")) {
        return Err("wait_agent arguments contain an unknown field".into());
    }
    let targets = object
        .get("targets")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "wait_agent requires an array targets field".to_owned())?;
    if targets.is_empty() || targets.len() > 16 {
        return Err("wait_agent targets must contain between one and sixteen items".into());
    }
    let targets = targets
        .iter()
        .map(|value| {
            let target = value
                .as_str()
                .ok_or_else(|| "wait_agent targets must be strings".to_owned())?;
            if target.trim().is_empty() || target.len() > 256 {
                return Err("wait_agent targets must be bounded non-empty strings".into());
            }
            Ok(target.to_owned())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let return_when = match object.get("return_when") {
        None => WaitReturnWhen::All,
        Some(value) => match value
            .as_str()
            .ok_or_else(|| "wait_agent return_when must be a string".to_owned())?
        {
            "all" => WaitReturnWhen::All,
            "any" => WaitReturnWhen::Any,
            _ => return Err("wait_agent return_when must be any or all".into()),
        },
    };
    let timeout_ms = match object.get("timeout_ms") {
        None => 300_000,
        Some(value) => value
            .as_u64()
            .filter(|value| (100..=600_000).contains(value))
            .ok_or_else(|| {
                "wait_agent timeout_ms must be an integer within 100..=600000".to_owned()
            })?,
    };
    Ok(WaitAgentsRequest {
        targets,
        return_when,
        timeout: Duration::from_millis(timeout_ms),
    })
}

fn parse_empty_object(call: &ToolCall, tool_name: &str) -> Result<(), String> {
    let value = JsonValue::parse(call.arguments.as_str())
        .map_err(|_| format!("{tool_name} arguments must be valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| format!("{tool_name} arguments must be a JSON object"))?;
    if object.is_empty() {
        Ok(())
    } else {
        Err(format!("{tool_name} accepts no arguments"))
    }
}

fn parse_interrupt_target(call: &ToolCall) -> Result<String, String> {
    let value = JsonValue::parse(call.arguments.as_str())
        .map_err(|_| "interrupt_agent arguments must be valid JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "interrupt_agent arguments must be a JSON object".to_owned())?;
    if object.keys().any(|key| key != "target") {
        return Err("interrupt_agent arguments contain an unknown field".into());
    }
    let target = object
        .get("target")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "interrupt_agent requires string field target".to_owned())?;
    if target.trim().is_empty() || target.len() > 256 {
        return Err("interrupt_agent target must be a bounded non-empty string".into());
    }
    Ok(target.to_owned())
}

fn parse_apply_delta_id(call: &ToolCall) -> Result<WorkspaceDeltaId, String> {
    let value = JsonValue::parse(call.arguments.as_str())
        .map_err(|_| "apply_agent_changes arguments must be valid JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "apply_agent_changes arguments must be a JSON object".to_owned())?;
    if object.keys().any(|key| key != "delta_id") {
        return Err("apply_agent_changes arguments contain an unknown field".into());
    }
    let delta_id = object
        .get("delta_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "apply_agent_changes requires string field delta_id".to_owned())?;
    WorkspaceDeltaId::new(delta_id.to_owned())
        .map_err(|_| "apply_agent_changes delta_id must be a valid durable delta ID".to_owned())
}

fn validate_spawn_text(task_name: &str, task: &str) -> Result<(), String> {
    let valid_task_name = task_name.len() <= 64
        && task_name
            .bytes()
            .enumerate()
            .all(|(index, byte)| match index {
                0 => byte.is_ascii_lowercase(),
                _ => byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_',
            });
    if !valid_task_name {
        return Err("spawn_agent task_name must match ^[a-z][a-z0-9_]{0,63}$".into());
    }
    if task.is_empty() || task != task.trim() || task.len() > 64 * 1024 {
        return Err("spawn_agent task must be trimmed non-empty UTF-8 within 65536 bytes".into());
    }
    Ok(())
}

fn parse_thinking_level(value: &str) -> Result<crate::state::ThinkingLevel, String> {
    match value {
        "off" => Ok(crate::state::ThinkingLevel::Off),
        "minimal" => Ok(crate::state::ThinkingLevel::Minimal),
        "low" => Ok(crate::state::ThinkingLevel::Low),
        "medium" => Ok(crate::state::ThinkingLevel::Medium),
        "high" => Ok(crate::state::ThinkingLevel::High),
        "xhigh" => Ok(crate::state::ThinkingLevel::XHigh),
        "max" => Ok(crate::state::ThinkingLevel::Max),
        _ => Err("spawn_agent thinking must be one documented enum value".into()),
    }
}

fn recoverable_error(call: ToolCall, message: impl Into<String>) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id,
        content: message.into(),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: true,
        failure: Some(ToolFailure::recoverable()),
    }
}

fn json_result(call: ToolCall, value: JsonValue) -> AgentToolResult {
    AgentToolResult {
        tool_call_id: call.id,
        content: value
            .to_json_string()
            .expect("fixed subagent coordinator result is JSON encodable"),
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: false,
        is_error: false,
        failure: None,
    }
}

fn wait_result_value(result: WaitAgentsResult) -> JsonValue {
    JsonValue::object([
        (
            "completed",
            JsonValue::Array(result.completed.iter().map(waited_status_value).collect()),
        ),
        (
            "pending",
            JsonValue::Array(result.pending.iter().map(wait_pending_status_value).collect()),
        ),
        ("timed_out", JsonValue::Bool(result.timed_out)),
    ])
}

fn apply_result_value(result: ApplyAgentChangesResult) -> JsonValue {
    match result {
        ApplyAgentChangesResult::Applied {
            delta_id,
            changed_paths,
        } => JsonValue::object([
            ("delta_id", JsonValue::String(delta_id.to_string())),
            ("state", JsonValue::String("applied".into())),
            (
                "changed_paths",
                JsonValue::Array(changed_paths.into_iter().map(JsonValue::String).collect()),
            ),
        ]),
        ApplyAgentChangesResult::Conflict {
            delta_id,
            conflicting_paths,
            patch_artifact,
        } => JsonValue::object([
            ("delta_id", JsonValue::String(delta_id.to_string())),
            ("state", JsonValue::String("conflict".into())),
            (
                "conflicting_paths",
                JsonValue::Array(conflicting_paths.into_iter().map(JsonValue::String).collect()),
            ),
            ("patch_artifact", JsonValue::String(patch_artifact.to_string())),
        ]),
        ApplyAgentChangesResult::RolledBack {
            delta_id,
            diagnostic,
        } => JsonValue::object([
            ("delta_id", JsonValue::String(delta_id.to_string())),
            ("state", JsonValue::String("rolled_back".into())),
            ("diagnostic", JsonValue::String(diagnostic)),
        ]),
        ApplyAgentChangesResult::Indeterminate {
            delta_id,
            diagnostic,
        } => JsonValue::object([
            ("delta_id", JsonValue::String(delta_id.to_string())),
            ("state", JsonValue::String("indeterminate".into())),
            ("diagnostic", JsonValue::String(diagnostic)),
        ]),
    }
}

fn waited_status_value(waited: &WaitedSubagent) -> JsonValue {
    let mut fields = status_fields(&waited.status);
    fields.push((
        "report",
        JsonValue::object([
            ("preview", JsonValue::String(waited.report.preview.clone())),
            (
                "artifact",
                waited
                    .report
                    .artifact_id
                    .as_ref()
                    .map(|id| JsonValue::String(id.to_string()))
                    .unwrap_or(JsonValue::Null),
            ),
        ]),
    ));
    fields.push(("changes", wait_changes_value(&waited.status)));
    JsonValue::object(fields)
}

fn wait_pending_status_value(status: &SubagentStatus) -> JsonValue {
    let mut fields = status_fields(status);
    fields.push(("changes", wait_changes_value(status)));
    JsonValue::object(fields)
}

fn list_status_value(status: &SubagentStatus) -> JsonValue {
    let mut fields = status_fields(status);
    fields.push((
        "workspace_delta_id",
        status
            .workspace_change
            .as_ref()
            .map(|change| JsonValue::String(change.delta_id.to_string()))
            .unwrap_or(JsonValue::Null),
    ));
    fields.push((
        "changed_path_count",
        unsigned(
            status
                .workspace_change
                .as_ref()
                .map(|change| change.changed_paths.len() as u64)
                .unwrap_or_default(),
        ),
    ));
    JsonValue::object(fields)
}

fn status_fields(status: &SubagentStatus) -> Vec<(&'static str, JsonValue)> {
    vec![
        ("agent_id", JsonValue::String(status.agent_id.to_string())),
        ("task_id", JsonValue::String(status.operation_id.to_string())),
        ("task_name", JsonValue::String(status.task_name.clone())),
        ("model", JsonValue::String(model_label(status))),
        ("thinking", JsonValue::String(status.thinking.clone())),
        ("state", JsonValue::String(agent_state_name(&status.state).into())),
        (
            "context",
            JsonValue::String(match status.context_mode {
                AgentContextMode::Task => "task",
                AgentContextMode::Parent => "parent",
            }
            .into()),
        ),
        ("usage", usage_value(&status.usage)),
    ]
}

fn model_label(status: &SubagentStatus) -> String {
    let mut model = format!("{}/{}", status.model.provider, status.model.model);
    if let Some(revision) = &status.model.revision {
        model.push('@');
        model.push_str(revision);
    }
    model
}

fn wait_changes_value(status: &SubagentStatus) -> JsonValue {
    status.workspace_change.as_ref().map(|change| {
        JsonValue::object([
            ("delta_id", JsonValue::String(change.delta_id.to_string())),
            (
                "changed_paths",
                JsonValue::Array(
                    change
                        .changed_paths
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            ),
            (
                "patch_artifact",
                JsonValue::String(change.patch_artifact.to_string()),
            ),
        ])
    }).unwrap_or(JsonValue::Null)
}

fn usage_value(usage: &tea_session::Usage) -> JsonValue {
    JsonValue::object([
        (
            "input_tokens",
            usage.input_tokens.map(unsigned).unwrap_or(JsonValue::Null),
        ),
        (
            "output_tokens",
            usage.output_tokens.map(unsigned).unwrap_or(JsonValue::Null),
        ),
        (
            "cache_read_tokens",
            usage.cache_read_tokens.map(unsigned).unwrap_or(JsonValue::Null),
        ),
        (
            "cache_write_tokens",
            usage.cache_write_tokens.map(unsigned).unwrap_or(JsonValue::Null),
        ),
    ])
}

fn interrupt_result_value(result: InterruptAgentResult) -> JsonValue {
    JsonValue::object([
        ("agent_id", JsonValue::String(result.agent_id.to_string())),
        (
            "previous_state",
            JsonValue::String(agent_state_name(&result.previous).into()),
        ),
        (
            "state",
            JsonValue::String(agent_state_name(&result.resulting).into()),
        ),
    ])
}

fn agent_state_name(state: &AgentState) -> &'static str {
    match state {
        AgentState::Spawned => "spawned",
        AgentState::Running => "running",
        AgentState::Finalizing { .. } => "finalizing",
        AgentState::Completed { .. } => "completed",
        AgentState::Interrupted => "interrupted",
        AgentState::Failed { .. } => "failed",
        AgentState::DeltaReady { .. } => "delta_ready",
        AgentState::Applied { .. } => "applied",
    }
}

fn append_suffix(target: &mut String, suffix: &str) {
    if !target.is_empty() {
        target.push_str("\n\n");
    }
    target.push_str(suffix);
}

fn presentation_from_definition(definition: ToolDefinition) -> ToolPresentationDescriptor {
    ToolPresentationDescriptor {
        name: definition.name,
        description: definition.description,
        schema: definition.schema,
        execution_mode: execution_mode_name(definition.execution_mode).into(),
        requires_exclusive_batch: definition.requires_exclusive_batch,
        cancellation_settlement_mode: cancellation_mode_name(
            definition.cancellation_settlement_mode,
        )
        .into(),
    }
}

fn execution_mode_name(mode: ToolExecutionMode) -> &'static str {
    match mode {
        ToolExecutionMode::Sequential => "sequential",
        ToolExecutionMode::Parallel => "parallel",
    }
}

fn cancellation_mode_name(mode: CancellationSettlementMode) -> &'static str {
    match mode {
        CancellationSettlementMode::DropFuture => "drop_future",
        CancellationSettlementMode::AwaitFuture => "await_future",
    }
}

fn spawn_agent_schema(models: Vec<JsonValue>) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::String("object".into())),
        (
            "properties",
            JsonValue::object([
                (
                    "task_name",
                    JsonValue::object([
                        ("type", JsonValue::String("string".into())),
                        (
                            "description",
                            JsonValue::String(
                                "Stable unique name for this child within the current root operation."
                                    .into(),
                            ),
                        ),
                        ("minLength", unsigned(1)),
                        ("maxLength", unsigned(64)),
                    ]),
                ),
                (
                    "task",
                    JsonValue::object([
                        ("type", JsonValue::String("string".into())),
                        (
                            "description",
                            JsonValue::String(
                                "Complete, self-contained assignment for the child.".into(),
                            ),
                        ),
                        ("minLength", unsigned(1)),
                        ("maxLength", unsigned(65_536)),
                    ]),
                ),
                (
                    "model",
                    JsonValue::object([
                        ("type", JsonValue::String("string".into())),
                        ("enum", JsonValue::Array(models)),
                        (
                            "description",
                            JsonValue::String("Model selected for this child.".into()),
                        ),
                    ]),
                ),
                (
                    "thinking",
                    JsonValue::object([
                        ("type", JsonValue::String("string".into())),
                        (
                            "enum",
                            JsonValue::Array(
                                THINKING_LEVELS
                                    .into_iter()
                                    .map(|value| JsonValue::String(value.into()))
                                    .collect(),
                            ),
                        ),
                        (
                            "description",
                            JsonValue::String("Optional child reasoning level.".into()),
                        ),
                    ]),
                ),
                (
                    "context",
                    JsonValue::object([
                        ("type", JsonValue::String("string".into())),
                        (
                            "enum",
                            JsonValue::Array(vec![
                                JsonValue::String("task".into()),
                                JsonValue::String("parent".into()),
                            ]),
                        ),
                        (
                            "description",
                            JsonValue::String(
                                "task uses only the assignment; parent forks the exact parent source context."
                                    .into(),
                            ),
                        ),
                    ]),
                ),
            ]),
        ),
        (
            "required",
            JsonValue::Array(vec![
                JsonValue::String("task_name".into()),
                JsonValue::String("task".into()),
                JsonValue::String("model".into()),
            ]),
        ),
        ("additionalProperties", JsonValue::Bool(false)),
    ])
}

fn wait_agent_schema() -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::String("object".into())),
        (
            "properties",
            JsonValue::object([
                (
                    "targets",
                    JsonValue::object([
                        ("type", JsonValue::String("array".into())),
                        (
                            "items",
                            JsonValue::object([("type", JsonValue::String("string".into()))]),
                        ),
                        ("minItems", unsigned(1)),
                        ("maxItems", unsigned(16)),
                        (
                            "description",
                            JsonValue::String(
                                "Agent IDs or task names returned by spawn_agent.".into(),
                            ),
                        ),
                    ]),
                ),
                (
                    "return_when",
                    JsonValue::object([
                        ("type", JsonValue::String("string".into())),
                        (
                            "enum",
                            JsonValue::Array(vec![
                                JsonValue::String("any".into()),
                                JsonValue::String("all".into()),
                            ]),
                        ),
                    ]),
                ),
                (
                    "timeout_ms",
                    JsonValue::object([
                        ("type", JsonValue::String("integer".into())),
                        ("minimum", unsigned(100)),
                        ("maximum", unsigned(600_000)),
                    ]),
                ),
            ]),
        ),
        (
            "required",
            JsonValue::Array(vec![JsonValue::String("targets".into())]),
        ),
        ("additionalProperties", JsonValue::Bool(false)),
    ])
}

fn interrupt_agent_schema() -> JsonValue {
    single_required_string_schema(
        "target",
        "Agent ID or task name owned by the current root operation.",
    )
}

fn apply_agent_changes_schema() -> JsonValue {
    single_required_string_schema("delta_id", "Durable child workspace delta ID to apply.")
}

fn single_required_string_schema(name: &str, description: &str) -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::String("object".into())),
        (
            "properties",
            JsonValue::object([(
                name,
                JsonValue::object([
                    ("type", JsonValue::String("string".into())),
                    ("description", JsonValue::String(description.into())),
                ]),
            )]),
        ),
        (
            "required",
            JsonValue::Array(vec![JsonValue::String(name.into())]),
        ),
        ("additionalProperties", JsonValue::Bool(false)),
    ])
}

fn empty_object_schema() -> JsonValue {
    JsonValue::object([
        ("type", JsonValue::String("object".into())),
        ("properties", JsonValue::Object(Default::default())),
        ("additionalProperties", JsonValue::Bool(false)),
    ])
}

fn unsigned(value: u64) -> JsonValue {
    JsonValue::Number(JsonNumber::Unsigned(value))
}

#[cfg(test)]
mod parsing_tests {
    use super::*;
    use crate::state::ToolCallId;
    use crate::state::SerializedJson;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(format!("{name}-parse-test")).expect("fixture call ID"),
            name: name.into(),
            arguments: SerializedJson::new(arguments),
        }
    }

    #[test]
    fn optional_enum_fields_reject_wrong_json_types_instead_of_using_defaults() {
        assert!(parse_spawn_request(&call(
            "spawn_agent",
            r#"{"task_name":"audit","task":"inspect","model":"child","context":7}"#,
        ))
        .is_err());
        assert!(parse_wait_request(&call(
            "wait_agent",
            r#"{"targets":["audit"],"return_when":false}"#,
        ))
        .is_err());
    }

    #[test]
    fn spawn_assignment_must_already_be_trimmed() {
        assert!(parse_spawn_request(&call(
            "spawn_agent",
            r#"{"task_name":"audit","task":" inspect ","model":"child"}"#,
        ))
        .is_err());
    }
}
