//! Policy VM loading and hook evaluation.

use super::parsing::{
    parse_after_tool_output, parse_context_projection, parse_decision, parse_declaration,
    parse_resume_state, policy_result_fields, runtime_error,
};
use super::types::{PolicyRuntime, PolicyTool};
use super::{
    LuaPolicy, PolicyAfterToolOutput, PolicyContextInput, PolicyContextProjectionPatch,
    PolicyError, PolicyLimits,
};
use crate::bundle::{Bundle, BUNDLE_ABI_VERSION};
use crate::bundle_runtime::BundleRuntime;
use mlua::{Lua, LuaOptions, StdLib, Table, Value, VmState};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
        let declaration = parse_declaration(&declaration, BUNDLE_ABI_VERSION)?;

        Ok(Self {
            runtime: Mutex::new(PolicyRuntime {
                lua,
                before_tool_call: declaration.before_tool_call,
                after_tool_call: declaration.after_tool_call,
                context_projection: declaration.context_projection,
                resume_hooks: declaration.resume_hooks,
                interrupt_budget,
                max_interrupt_checks: limits.max_interrupt_checks,
            }),
            prompt_sections: declaration.prompt_sections,
            tools: declaration.tools,
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
                interrupt_budget,
                max_interrupt_checks: limits.max_interrupt_checks,
            }),
            prompt_sections: declaration.prompt_sections,
            tools: declaration.tools,
        })
    }

    /// Return deterministic named prompt sections declared by the v1 bundle.
    pub fn prompt_sections(&self) -> &[super::PolicyPromptSection] {
        &self.prompt_sections
    }

    /// Return the ordered, authority-free tool declarations.
    pub fn tools(&self) -> &[PolicyTool] {
        &self.tools
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

    /// Evaluate a v1 post-tool projection. The result table deliberately
    /// excludes usage and host failure metadata, and the parser accepts only
    /// model-visible replacements.
    pub fn after_tool_call(
        &self,
        call: &ToolCall,
        result: &AgentToolResult,
    ) -> Result<AfterToolCall, PolicyError> {
        Ok(self.after_tool_output(call, result)?.projection)
    }

    /// Evaluate the complete v1 post-tool output. The projection remains
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

    /// Evaluate the optional v1 context policy using only bounded,
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

    /// Evaluate every v1 `before_operation` lifecycle callback before
    /// the harness commits its operation-start record. Keys are bundle-local
    /// stable IDs; the harness namespaces them with the immutable plugin ID
    /// before persistence.
    pub fn before_operation_resume_data(&self) -> Result<BTreeMap<String, JsonValue>, PolicyError> {
        self.lifecycle_resume_data(ResumeDataPhase::Operation)
    }

    /// Evaluate every v1 `before_epoch` lifecycle callback before the
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
