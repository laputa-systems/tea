//! Policy VM loading and hook evaluation.

use super::parsing::{
    parse_after_tool_output, parse_context_projection, parse_decision, parse_declaration,
    parse_extension_result, parse_idle_result, parse_resume_state, parse_route,
    policy_result_fields, runtime_error,
};
use super::types::{PolicyHostCommand, PolicyRuntime, PolicyTool};
use super::{
    LuaPolicy, PolicyAfterToolOutput, PolicyContextInput, PolicyContextProjectionPatch,
    PolicyError, PolicyLimits,
};
use crate::bundle::{Bundle, BUNDLE_ABI_V3_VERSION};
use crate::bundle_runtime::BundleRuntime;
use mlua::{Lua, LuaOptions, StdLib, Table, Value, VmState};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tea_core::harness::extension::{
    ExtensionCommandInput, ExtensionCommandResult, ExtensionIdleInput, ExtensionIdleResult,
    ExtensionOperationOutcome,
};
use tea_core::hooks::{AfterToolCall, BeforeToolCall};
use tea_core::tool::{AgentToolResult, ToolCall};
use tea_protocol::{JsonNumber, JsonValue};

const POLICY_CHUNK_NAME: &str = "tea-policy.luau";

#[derive(Clone, Copy)]
enum ResumeDataPhase {
    Operation,
    Epoch,
}

impl ResumeDataPhase {
    const fn label(self) -> &'static str {
        match self {
            Self::Operation => "before_operation",
            Self::Epoch => "before_epoch",
        }
    }
}

impl LuaPolicy {
    /// Load a policy with the conservative default VM limits.
    pub fn load(source: &str) -> Result<Self, PolicyError> {
        Self::load_with_limits(source, PolicyLimits::default())
    }

    /// Load a policy with host-selected, finite resource limits.
    pub fn load_with_limits(source: &str, limits: PolicyLimits) -> Result<Self, PolicyError> {
        validate_limits(limits)?;
        if source.len() > limits.max_source_bytes {
            return Err(PolicyError::SourceTooLarge {
                actual: source.len(),
                limit: limits.max_source_bytes,
            });
        }

        let lua = Lua::new_with(
            StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH,
            LuaOptions::new(),
        )
        .map_err(runtime_error)?;
        lua.set_memory_limit(limits.max_memory_bytes)
            .map_err(runtime_error)?;
        lua.enable_jit(true);
        // Luau makes global tables read-only and isolates script globals. This is
        // in addition to omitting ambient I/O, OS, package, and debug libraries.
        lua.sandbox(true).map_err(runtime_error)?;

        let interrupt_budget = Arc::new(AtomicUsize::new(limits.max_interrupt_checks));
        let interrupt_counter = Arc::clone(&interrupt_budget);
        lua.set_interrupt(move |_| {
            if interrupt_counter
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_err()
            {
                return Err(mlua::Error::RuntimeError(
                    "Luau policy interrupt budget exhausted".to_owned(),
                ));
            }
            Ok(VmState::Continue)
        });

        let declaration: Table = lua
            .load(source)
            .set_name(POLICY_CHUNK_NAME)
            .eval()
            .map_err(runtime_error)?;
        let declaration = parse_declaration(&declaration, BUNDLE_ABI_V3_VERSION)?;

        Ok(Self {
            runtime: Mutex::new(PolicyRuntime {
                lua,
                before_tool_call: declaration.before_tool_call,
                after_tool_call: declaration.after_tool_call,
                context_projection: declaration.context_projection,
                resume_hooks: declaration.resume_hooks,
                host_commands: declaration.host_command_handlers,
                on_idle: declaration.on_idle,
                virtual_routes: declaration.virtual_routes,
                interrupt_budget,
                max_interrupt_checks: limits.max_interrupt_checks,
            }),
            prompt_sections: declaration.prompt_sections,
            tools: declaration.tools,
            host_commands: declaration.host_commands,
            state_version: declaration.state_version,
            virtual_models: declaration.virtual_models,
        })
    }

    /// Load a closed multi-module policy bundle with the default VM limits.
    ///
    /// The bundle entrypoint must return the same declaration table accepted
    /// by [`Self::load`]. Its `require` function can resolve only explicit
    /// bundle-local `./` and `../` imports; it cannot load virtual modules,
    /// host files, packages, or network resources.
    pub fn load_bundle(bundle: Bundle) -> Result<Self, PolicyError> {
        Self::load_bundle_with_limits(bundle, PolicyLimits::default())
    }

    /// Load a closed multi-module policy bundle with host-selected limits.
    ///
    /// `max_source_bytes` applies to the aggregate UTF-8 bytes of every
    /// bundle module, not only the entrypoint. This prevents dormant modules
    /// from evading the source-size boundary.
    pub fn load_bundle_with_limits(
        bundle: Bundle,
        limits: PolicyLimits,
    ) -> Result<Self, PolicyError> {
        validate_limits(limits)?;
        let source_bytes = bundle.modules().values().try_fold(0usize, |total, source| {
            total.checked_add(source.len()).ok_or(())
        });
        let source_bytes = source_bytes.unwrap_or(usize::MAX);
        if source_bytes > limits.max_source_bytes {
            return Err(PolicyError::SourceTooLarge {
                actual: source_bytes,
                limit: limits.max_source_bytes,
            });
        }

        let lua = Lua::new_with(
            StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::UTF8 | StdLib::MATH,
            LuaOptions::new(),
        )
        .map_err(runtime_error)?;
        lua.set_memory_limit(limits.max_memory_bytes)
            .map_err(runtime_error)?;
        lua.enable_jit(true);

        let bundle_runtime = BundleRuntime::new(bundle);
        bundle_runtime
            .install(&lua)
            .map_err(|error| PolicyError::Runtime {
                message: error.to_string(),
            })?;
        // Luau makes global tables read-only and isolates script globals. This is
        // in addition to omitting ambient I/O, OS, package, and debug libraries.
        lua.sandbox(true).map_err(runtime_error)?;

        let interrupt_budget = Arc::new(AtomicUsize::new(limits.max_interrupt_checks));
        let interrupt_counter = Arc::clone(&interrupt_budget);
        lua.set_interrupt(move |_| {
            if interrupt_counter
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_err()
            {
                return Err(mlua::Error::RuntimeError(
                    "Luau policy interrupt budget exhausted".to_owned(),
                ));
            }
            Ok(VmState::Continue)
        });

        let declaration =
            match bundle_runtime
                .eval_entrypoint(&lua)
                .map_err(|error| PolicyError::Runtime {
                    message: error.to_string(),
                })? {
                Value::Table(declaration) => declaration,
                _ => {
                    return Err(PolicyError::Contract {
                        message: "bundle entrypoint must return a policy declaration table"
                            .to_owned(),
                    });
                }
            };
        let abi_version = bundle_runtime.bundle().manifest().abi_version();
        let declaration = parse_declaration(&declaration, abi_version)?;

        Ok(Self {
            runtime: Mutex::new(PolicyRuntime {
                lua,
                before_tool_call: declaration.before_tool_call,
                after_tool_call: declaration.after_tool_call,
                context_projection: declaration.context_projection,
                resume_hooks: declaration.resume_hooks,
                host_commands: declaration.host_command_handlers,
                on_idle: declaration.on_idle,
                virtual_routes: declaration.virtual_routes,
                interrupt_budget,
                max_interrupt_checks: limits.max_interrupt_checks,
            }),
            prompt_sections: declaration.prompt_sections,
            tools: declaration.tools,
            host_commands: declaration.host_commands,
            state_version: declaration.state_version,
            virtual_models: declaration.virtual_models,
        })
    }

    /// Return deterministic named prompt sections declared by the v3 bundle.
    pub fn prompt_sections(&self) -> &[super::PolicyPromptSection] {
        &self.prompt_sections
    }

    /// Return the ordered, authority-free tool declarations.
    pub fn tools(&self) -> &[PolicyTool] {
        &self.tools
    }

    /// Return immutable host-local command metadata.
    pub fn host_commands(&self) -> &[PolicyHostCommand] {
        &self.host_commands
    }

    /// Return the immutable whole-state contract declared by this bundle.
    pub fn state_version(&self) -> Option<&str> {
        self.state_version.as_deref()
    }

    /// Evaluate one constrained terminal command inside the policy VM.
    pub fn execute_host_command(
        &self,
        name: &str,
        input: &ExtensionCommandInput,
    ) -> Result<ExtensionCommandResult, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        let handler = runtime
            .host_commands
            .iter()
            .find(|handler| handler.name == name)
            .ok_or_else(|| PolicyError::Contract {
                message: format!("unknown declared extension command {name:?}"),
            })?;
        reset_interrupt_budget(&runtime);
        let input = extension_command_input_table(&runtime.lua, input)?;
        let output = handler
            .function
            .call::<Value>(input)
            .map_err(runtime_error)?;
        parse_extension_result(output)
    }

    /// Declared virtual models.
    pub fn virtual_models(&self) -> &[super::PolicyVirtualModel] {
        &self.virtual_models
    }

    /// Evaluate one declared virtual model's deterministic route function.
    ///
    /// The function receives a bounded summary of the request: `reason`,
    /// `selected`, `thinking_level`, `previous`, `targets`, `state`,
    /// `last_user_text` (at most 4096 bytes), `tool_results` since the last
    /// user message, and `previous_turn_tool_results` for the turn before it
    /// (each `{ name = ..., is_error = ... }`, at most 64).
    pub fn route_virtual_model(
        &self,
        id: &str,
        request: &tea_core::routing::RouteRequest<'_>,
    ) -> Result<tea_core::routing::Route, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        let route = runtime
            .virtual_routes
            .iter()
            .find(|(candidate, _)| candidate == id)
            .map(|(_, function)| function.clone())
            .ok_or_else(|| PolicyError::Contract {
                message: format!("unknown declared virtual model {id:?}"),
            })?;
        reset_interrupt_budget(&runtime);
        let input = route_request_table(&runtime.lua, request)?;
        let output = route.call::<Value>(input).map_err(runtime_error)?;
        let parsed = parse_route(output)?;
        let target =
            tea_core::routing::parse_descriptor(&parsed.target).ok_or_else(|| PolicyError::Contract {
                message: format!(
                    "virtual model route target {:?} must be provider/model",
                    parsed.target
                ),
            })?;
        let thinking_level = parsed
            .thinking_level
            .map(|name| {
                thinking_level_from_name(&name).ok_or_else(|| PolicyError::Contract {
                    message: format!("virtual model route thinking_level {name:?} is unknown"),
                })
            })
            .transpose()?;
        Ok(tea_core::routing::Route {
            target,
            thinking_level,
            state: parsed.state.map(|update| update.value),
        })
    }

    /// Return whether the policy contributes an idle continuation callback.
    pub fn has_idle_hook(&self) -> Result<bool, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        Ok(runtime.on_idle.is_some())
    }

    /// Evaluate the bounded post-operation continuation callback.
    pub fn on_idle(&self, input: &ExtensionIdleInput) -> Result<ExtensionIdleResult, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        let Some(handler) = runtime.on_idle.as_ref() else {
            return Ok(ExtensionIdleResult::default());
        };
        reset_interrupt_budget(&runtime);
        let input = extension_idle_input_table(&runtime.lua, input)?;
        let output = handler.call::<Value>(input).map_err(runtime_error)?;
        parse_idle_result(output)
    }

    /// Return whether this policy contributes metadata-only context behavior.
    ///
    /// Embeddings use this to reject unsupported hosted policy surfaces
    /// without treating an absent callback as an executable no-op port.
    pub fn has_context_projection(&self) -> Result<bool, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        Ok(runtime.context_projection.is_some())
    }

    /// Return whether this policy contributes durable lifecycle behavior.
    pub fn has_resume_hooks(&self) -> Result<bool, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        Ok(!runtime.resume_hooks.is_empty())
    }

    /// Evaluate the optional pre-tool decision without granting the policy an effect.
    pub fn before_tool_call(&self, call: &ToolCall) -> Result<BeforeToolCall, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        let Some(function) = runtime.before_tool_call.as_ref() else {
            return Ok(BeforeToolCall::Allow);
        };
        reset_interrupt_budget(&runtime);
        let call_table = policy_call_table(&runtime.lua, call)?;
        let decision = function.call::<Value>(call_table).map_err(runtime_error)?;
        parse_decision(decision)
    }

    /// Evaluate a v3 post-tool projection. The result table deliberately
    /// excludes usage and host failure metadata, and the parser accepts only
    /// model-visible replacements.
    pub fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<AfterToolCall, PolicyError> {
        Ok(self.after_tool_output(call, result)?.projection)
    }

    /// Evaluate the complete v3 post-tool output. The projection remains
    /// separate from the optional typed memory proposal so a caller cannot
    /// treat memory as a transcript mutation or let it alter raw evidence.
    pub fn after_tool_output(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<PolicyAfterToolOutput, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        let Some(function) = runtime.after_tool_call.as_ref() else {
            return Ok(PolicyAfterToolOutput {
                projection: AfterToolCall::default(),
                memory: None,
            });
        };
        reset_interrupt_budget(&runtime);
        let call_table = policy_call_table(&runtime.lua, call)?;
        let result_table = runtime.lua.create_table().map_err(runtime_error)?;
        for (name, value) in policy_result_fields(result)? {
            result_table
                .set(
                    name,
                    json_to_lua(&runtime.lua, &value).map_err(runtime_error)?,
                )
                .map_err(runtime_error)?;
        }
        let projection = function
            .call::<Value>((call_table, result_table))
            .map_err(runtime_error)?;
        parse_after_tool_output(projection)
    }

    /// Evaluate the optional v3 context policy using only bounded,
    /// metadata-only branch descriptors. The returned IDs remain opaque until
    /// the Rust harness maps and validates them against its immutable tree.
    pub fn context_projection(
        &self,
        input: &PolicyContextInput,
    ) -> Result<PolicyContextProjectionPatch, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        let Some(function) = runtime.context_projection.as_ref() else {
            return Ok(PolicyContextProjectionPatch::default());
        };
        reset_interrupt_budget(&runtime);
        let entries = runtime.lua.create_table().map_err(runtime_error)?;
        for (index, entry) in input.entries.iter().enumerate() {
            let value = runtime.lua.create_table().map_err(runtime_error)?;
            value.set("id", entry.id.as_str()).map_err(runtime_error)?;
            value
                .set("kind", entry.kind.as_str())
                .map_err(runtime_error)?;
            value
                .set("model_visible", entry.model_visible)
                .map_err(runtime_error)?;
            value
                .set("protected", entry.protected)
                .map_err(runtime_error)?;
            entries.set(index + 1, value).map_err(runtime_error)?;
        }
        let input_table = runtime.lua.create_table().map_err(runtime_error)?;
        input_table.set("entries", entries).map_err(runtime_error)?;
        let output = function.call::<Value>(input_table).map_err(runtime_error)?;
        parse_context_projection(output)
    }

    /// Evaluate every v3 `before_operation` lifecycle callback before
    /// the harness commits its operation-start record. Keys are bundle-local
    /// stable IDs; the harness namespaces them with the immutable plugin ID
    /// before persistence.
    pub fn before_operation_resume_data(&self) -> Result<BTreeMap<String, JsonValue>, PolicyError> {
        self.lifecycle_resume_data(ResumeDataPhase::Operation)
    }

    /// Evaluate every v3 `before_epoch` lifecycle callback before the
    /// harness commits its epoch-start record.
    pub fn before_epoch_resume_data(&self) -> Result<BTreeMap<String, JsonValue>, PolicyError> {
        self.lifecycle_resume_data(ResumeDataPhase::Epoch)
    }

    /// Return the deterministic bundle-local registration IDs used by this
    /// policy's lifecycle callbacks. Hosts namespace these IDs with immutable
    /// plugin identity before looking up persisted state on recovery.
    pub fn resume_hook_ids(&self) -> Result<Vec<String>, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        Ok(runtime
            .resume_hooks
            .iter()
            .map(|hook| hook.id.clone())
            .collect())
    }

    /// Rebuild process-local policy state from this policy's own durable
    /// lifecycle values. A callback receives a table with only `operation`
    /// and `epoch` values registered under its own local ID; it cannot name
    /// or inspect another registration's state.
    ///
    /// Resume callbacks must return `nil`. Their effects are deliberately
    /// limited to reconstructing VM-local closures: they receive no capability
    /// bindings, session writer, evaluator handle, or activation authority.
    /// A crash before the next durable consumer commits may invoke them again,
    /// so their source contract is explicitly idempotent.
    pub fn before_resume(
        &self,
        operation_data: &BTreeMap<String, JsonValue>,
        epoch_data: &BTreeMap<String, JsonValue>,
    ) -> Result<(), PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        for hook in &runtime.resume_hooks {
            let Some(function) = hook.before_resume.as_ref() else {
                continue;
            };
            reset_interrupt_budget(&runtime);
            let state = runtime.lua.create_table().map_err(runtime_error)?;
            if let Some(operation) = operation_data.get(&hook.id) {
                state
                    .set(
                        "operation",
                        json_to_lua(&runtime.lua, operation).map_err(runtime_error)?,
                    )
                    .map_err(runtime_error)?;
            }
            if let Some(epoch) = epoch_data.get(&hook.id) {
                state
                    .set(
                        "epoch",
                        json_to_lua(&runtime.lua, epoch).map_err(runtime_error)?,
                    )
                    .map_err(runtime_error)?;
            }
            let value = function.call::<Value>(state).map_err(runtime_error)?;
            if !matches!(value, Value::Nil) {
                return Err(PolicyError::Contract {
                    message: format!(
                        "before_resume hook {:?} must return nil; it may rebuild only process-local state",
                        hook.id,
                    ),
                });
            }
        }
        Ok(())
    }

    fn lifecycle_resume_data(
        &self,
        phase: ResumeDataPhase,
    ) -> Result<BTreeMap<String, JsonValue>, PolicyError> {
        let runtime = self.runtime.lock().map_err(|_| PolicyError::Runtime {
            message: "policy VM lock was poisoned".to_owned(),
        })?;
        let mut state = BTreeMap::new();
        for hook in &runtime.resume_hooks {
            let function = match phase {
                ResumeDataPhase::Operation => hook.before_operation.as_ref(),
                ResumeDataPhase::Epoch => hook.before_epoch.as_ref(),
            };
            let Some(function) = function else {
                continue;
            };
            reset_interrupt_budget(&runtime);
            let value = function.call::<Value>(()).map_err(runtime_error)?;
            if let Some(value) = parse_resume_state(value, phase.label())? {
                state.insert(hook.id.clone(), value);
            }
        }
        Ok(state)
    }
}

fn reset_interrupt_budget(runtime: &PolicyRuntime) {
    runtime
        .interrupt_budget
        .store(runtime.max_interrupt_checks, Ordering::Relaxed);
}

fn policy_call_table(lua: &Lua, call: &ToolCall) -> Result<Table, PolicyError> {
    let call_table = lua.create_table().map_err(runtime_error)?;
    call_table
        .set("id", call.id.as_str())
        .map_err(runtime_error)?;
    call_table
        .set("name", call.name.as_str())
        .map_err(runtime_error)?;
    call_table
        .set("arguments_json", call.arguments.as_str())
        .map_err(runtime_error)?;
    Ok(call_table)
}

fn extension_command_input_table(
    lua: &Lua,
    input: &ExtensionCommandInput,
) -> Result<Table, PolicyError> {
    let table = lua.create_table().map_err(runtime_error)?;
    table
        .set("arguments", input.arguments.as_str())
        .map_err(runtime_error)?;
    table
        .set("state", extension_state_value(lua, &input.state.value)?)
        .map_err(runtime_error)?;
    Ok(table)
}

fn extension_idle_input_table(lua: &Lua, input: &ExtensionIdleInput) -> Result<Table, PolicyError> {
    let table = lua.create_table().map_err(runtime_error)?;
    table
        .set("operation_id", input.operation_id.as_str())
        .map_err(runtime_error)?;
    table
        .set(
            "outcome",
            match &input.outcome {
                ExtensionOperationOutcome::Completed => "completed",
                ExtensionOperationOutcome::Aborted => "aborted",
                ExtensionOperationOutcome::Failed { .. } => "failed",
            },
        )
        .map_err(runtime_error)?;
    if let ExtensionOperationOutcome::Failed { code } = &input.outcome {
        table
            .set("failure_code", code.as_str())
            .map_err(runtime_error)?;
    }
    table
        .set("elapsed_active_seconds", input.elapsed_active_seconds)
        .map_err(runtime_error)?;
    table
        .set("state", extension_state_value(lua, &input.state.value)?)
        .map_err(runtime_error)?;
    let usage = lua.create_table().map_err(runtime_error)?;
    set_optional_u64(&usage, "input_tokens", input.usage.input_tokens)?;
    set_optional_u64(&usage, "output_tokens", input.usage.output_tokens)?;
    set_optional_u64(&usage, "reasoning_tokens", input.usage.reasoning_tokens)?;
    set_optional_u64(&usage, "cache_read_tokens", input.usage.cache_read_tokens)?;
    set_optional_u64(&usage, "cache_write_tokens", input.usage.cache_write_tokens)?;
    table.set("usage", usage).map_err(runtime_error)?;
    Ok(table)
}

fn thinking_level_from_name(name: &str) -> Option<tea_core::state::ThinkingLevel> {
    use tea_core::state::ThinkingLevel;
    Some(match name {
        "off" => ThinkingLevel::Off,
        "minimal" => ThinkingLevel::Minimal,
        "low" => ThinkingLevel::Low,
        "medium" => ThinkingLevel::Medium,
        "high" => ThinkingLevel::High,
        "xhigh" => ThinkingLevel::XHigh,
        "max" => ThinkingLevel::Max,
        _ => return None,
    })
}

fn thinking_level_name(level: tea_core::state::ThinkingLevel) -> &'static str {
    use tea_core::state::ThinkingLevel;
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

/// Bounded, provider-neutral summary of one routing request.
fn route_request_table(
    lua: &Lua,
    request: &tea_core::routing::RouteRequest<'_>,
) -> Result<Table, PolicyError> {
    use tea_core::state::AgentMessage;
    const USER_TEXT_LIMIT: usize = 4096;
    const TOOL_RESULT_LIMIT: usize = 64;
    let table = lua.create_table().map_err(runtime_error)?;
    table
        .set("reason", request.reason.label())
        .map_err(runtime_error)?;
    table
        .set(
            "selected",
            tea_core::routing::format_descriptor(request.selected),
        )
        .map_err(runtime_error)?;
    table
        .set("thinking_level", thinking_level_name(request.thinking_level))
        .map_err(runtime_error)?;
    if let Some(previous) = request.previous {
        table
            .set("previous", tea_core::routing::format_descriptor(previous))
            .map_err(runtime_error)?;
    }
    let targets = lua.create_table().map_err(runtime_error)?;
    for (index, target) in request.targets.iter().enumerate() {
        targets
            .set(index + 1, tea_core::routing::format_descriptor(target))
            .map_err(runtime_error)?;
    }
    table.set("targets", targets).map_err(runtime_error)?;
    table
        .set("state", extension_state_value(lua, &request.state.cloned())?)
        .map_err(runtime_error)?;
    let last_user = request
        .messages
        .iter()
        .rposition(|message| matches!(message, AgentMessage::User { .. }));
    if let Some(index) = last_user {
        if let AgentMessage::User { content, .. } = &request.messages[index] {
            let mut text = content.clone();
            if text.len() > USER_TEXT_LIMIT {
                let mut end = USER_TEXT_LIMIT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            table.set("last_user_text", text).map_err(runtime_error)?;
        }
    }
    let since = last_user.map_or(0, |index| index + 1);
    table
        .set(
            "tool_results",
            tool_results_table(lua, &request.messages[since..], TOOL_RESULT_LIMIT)?,
        )
        .map_err(runtime_error)?;
    // The turn before the latest user message, so a sticky router can act on
    // what happened there at the next user turn.
    let previous_user = last_user.and_then(|index| {
        request.messages[..index]
            .iter()
            .rposition(|message| matches!(message, AgentMessage::User { .. }))
    });
    let previous_turn = match (previous_user, last_user) {
        (Some(start), Some(end)) => &request.messages[start + 1..end],
        _ => &[],
    };
    table
        .set(
            "previous_turn_tool_results",
            tool_results_table(lua, previous_turn, TOOL_RESULT_LIMIT)?,
        )
        .map_err(runtime_error)?;
    Ok(table)
}

fn tool_results_table(
    lua: &Lua,
    messages: &[tea_core::state::AgentMessage],
    limit: usize,
) -> Result<Table, PolicyError> {
    let results = lua.create_table().map_err(runtime_error)?;
    for (index, (name, is_error)) in messages
        .iter()
        .filter_map(|message| match message {
            tea_core::state::AgentMessage::ToolResult {
                tool_name, is_error, ..
            } => Some((tool_name.as_str(), *is_error)),
            _ => None,
        })
        .take(limit)
        .enumerate()
    {
        let entry = lua.create_table().map_err(runtime_error)?;
        entry.set("name", name).map_err(runtime_error)?;
        entry.set("is_error", is_error).map_err(runtime_error)?;
        results.set(index + 1, entry).map_err(runtime_error)?;
    }
    Ok(results)
}

fn extension_state_value(lua: &Lua, value: &Option<JsonValue>) -> Result<Value, PolicyError> {
    value
        .as_ref()
        .map(|value| json_to_lua(lua, value).map_err(runtime_error))
        .transpose()
        .map(|value| value.unwrap_or(Value::Nil))
}

fn set_optional_u64(table: &Table, name: &str, value: Option<u64>) -> Result<(), PolicyError> {
    if let Some(value) = value {
        table.set(name, value).map_err(runtime_error)?;
    }
    Ok(())
}

fn json_to_lua(lua: &Lua, value: &JsonValue) -> mlua::Result<Value> {
    match value {
        JsonValue::Null => Ok(Value::Nil),
        JsonValue::Bool(value) => Ok(Value::Boolean(*value)),
        JsonValue::Number(JsonNumber::Signed(value)) => Ok(Value::Integer(*value)),
        JsonValue::Number(JsonNumber::Unsigned(value)) => {
            if *value <= i64::MAX as u64 {
                Ok(Value::Integer(*value as i64))
            } else {
                Ok(Value::Number(*value as f64))
            }
        }
        JsonValue::Number(JsonNumber::Float(value)) => Ok(Value::Number(*value)),
        JsonValue::String(value) => Ok(Value::String(lua.create_string(value)?)),
        JsonValue::Array(values) => {
            let table = lua.create_table()?;
            for (index, value) in values.iter().enumerate() {
                table.set(index + 1, json_to_lua(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
        JsonValue::Object(values) => {
            let table = lua.create_table()?;
            for (key, value) in values {
                table.set(key.as_str(), json_to_lua(lua, value)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

fn validate_limits(limits: PolicyLimits) -> Result<(), PolicyError> {
    for (field, value) in [
        ("max_source_bytes", limits.max_source_bytes),
        ("max_memory_bytes", limits.max_memory_bytes),
        ("max_interrupt_checks", limits.max_interrupt_checks),
    ] {
        if value == 0 {
            return Err(PolicyError::InvalidLimit { field });
        }
    }
    Ok(())
}
